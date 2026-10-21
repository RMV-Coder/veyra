//! Remotes: configuration, fetch and clone (through gitoxide's protocol implementation).
//!
//! Push is not implemented yet: gitoxide does not provide it, and Veyra will build it on
//! `gix-protocol` in Phase 2 (see docs/ROADMAP.md).

use std::path::Path;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, anyhow, bail};
use gix::ObjectId;
use gix::bstr::ByteSlice;

use crate::Repo;

/// How a fetch or clone treats history depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// Keep the shallow boundary where it is (a full repository stays full).
    Unchanged,
    /// Have only the last `n` commits of each fetched branch (`--depth n`).
    Last(u32),
    /// Fetch the rest of the history (`--unshallow`).
    Unshallow,
}

impl Depth {
    fn shallow(self) -> Result<gix::remote::fetch::Shallow> {
        use gix::remote::fetch::Shallow;
        Ok(match self {
            Depth::Unchanged => Shallow::NoChange,
            Depth::Last(n) => {
                Shallow::DepthAtRemote(std::num::NonZeroU32::new(n).context("--depth must be at least 1")?)
            }
            // Git's --unshallow is a deepen by the largest possible amount.
            Depth::Unshallow => Shallow::Deepen(i32::MAX as u32),
        })
    }
}

/// A remote ref: `(name, id, peeled id when it's an annotated tag)`; no id when unborn.
pub type RemoteRef = (String, Option<ObjectId>, Option<ObjectId>);
use crate::config;

/// A configured remote.
#[derive(Debug, Clone)]
pub struct RemoteInfo {
    pub name: String,
    pub fetch_url: Option<String>,
    pub push_url: Option<String>,
}

/// Summary of a fetch.
#[derive(Debug, Clone, Default)]
pub struct FetchSummary {
    pub remote: String,
    pub updated_refs: Vec<(String, String)>,
}

static NEVER_INTERRUPT: AtomicBool = AtomicBool::new(false);

impl Repo {
    /// All configured remotes.
    pub fn remotes(&self) -> Result<Vec<RemoteInfo>> {
        let mut out = Vec::new();
        for name in self.git.remote_names() {
            let name = name.to_str_lossy().into_owned();
            let fetch_url = self.config_get(&format!("remote.{name}.url"));
            let push_url = self.config_get(&format!("remote.{name}.pushurl")).or_else(|| fetch_url.clone());
            out.push(RemoteInfo { name, fetch_url, push_url });
        }
        Ok(out)
    }

    /// `git remote add <name> <url>` with the default fetch refspec.
    pub fn remote_add(&self, name: &str, url: &str) -> Result<()> {
        if name.is_empty() || name.contains(['/', ' ', '\\']) || name.starts_with('-') {
            bail!("'{name}' is not a valid remote name");
        }
        if self.remotes()?.iter().any(|r| r.name == name) {
            bail!("remote '{name}' already exists");
        }
        let path = self.config_path(config::Scope::Local)?;
        config::set_in_file(&path, &format!("remote.{name}.url"), url)?;
        config::set_in_file(&path, &format!("remote.{name}.fetch"), &format!("+refs/heads/*:refs/remotes/{name}/*"))?;
        Ok(())
    }

    /// `git remote remove <name>`: drop the config section and its remote-tracking refs.
    pub fn remote_remove(&self, name: &str) -> Result<()> {
        let path = self.config_path(config::Scope::Local)?;
        if !config::remove_section_in_file(&path, "remote", Some(name))? {
            bail!("no such remote: '{name}'");
        }
        let prefix = format!("refs/remotes/{name}/");
        let doomed: Vec<(String, Option<gix::ObjectId>)> =
            self.all_refs()?.into_keys().filter(|r| r.starts_with(&prefix)).map(|r| (r, None)).collect();
        self.set_refs(&doomed, &format!("remote: remove {name}"))?;
        Ok(())
    }

    /// Fetch from `remote` (or the default remote) using its configured refspecs.
    pub fn fetch(&self, remote: Option<&str>) -> Result<FetchSummary> {
        self.fetch_with(remote, Depth::Unchanged)
    }

    /// Fetch, optionally changing the shallow boundary. In a partial clone the promisor remote
    /// is fetched with its object filter, so the repository stays partial.
    pub fn fetch_with(&self, remote: Option<&str>, depth: Depth) -> Result<FetchSummary> {
        if depth == Depth::Unchanged
            && let Some(promisor) = self.promisor_remote()
            && remote.is_none_or(|r| r == promisor)
        {
            let updated_refs = self.fetch_partial(&promisor)?;
            return Ok(FetchSummary { remote: promisor, updated_refs });
        }
        let remote = match remote {
            Some(name) => self.git.find_remote(name).map_err(|e| anyhow!("no such remote '{name}': {e}"))?,
            None => self
                .git
                .find_default_remote(gix::remote::Direction::Fetch)
                .ok_or_else(|| anyhow!("no remote configured\n  hint: veyra remote add origin <url>"))??,
        };
        let name =
            remote.name().map(|n| n.as_bstr().to_str_lossy().into_owned()).unwrap_or_else(|| "(anonymous)".into());
        let connection = remote
            .connect(gix::remote::Direction::Fetch)
            .with_context(|| format!("could not connect to remote '{name}'"))?;
        let outcome = connection
            .prepare_fetch(gix::progress::Discard, Default::default())
            .with_context(|| format!("could not negotiate with '{name}'"))?
            .with_shallow(depth.shallow()?)
            .receive(gix::progress::Discard, &NEVER_INTERRUPT)
            .with_context(|| format!("fetch from '{name}' failed"))?;
        let mut summary = FetchSummary { remote: name, updated_refs: Vec::new() };
        let update_refs = match &outcome.status {
            gix::remote::fetch::Status::Change { update_refs, .. } => Some(update_refs),
            gix::remote::fetch::Status::NoPackReceived { update_refs, .. } => Some(update_refs),
        };
        if let Some(update_refs) = update_refs {
            for (update, edit) in update_refs.updates.iter().zip(&update_refs.edits) {
                let mode = format!("{}", update.mode);
                summary.updated_refs.push((edit.name.as_bstr().to_str_lossy().into_owned(), mode));
            }
        }
        Ok(summary)
    }

    /// The refs a remote advertises (`git ls-remote`): `(name, id, peeled id of a tag)`.
    /// `remote` is a configured remote's name or a URL.
    pub fn remote_refs(&self, remote: &str) -> Result<Vec<RemoteRef>> {
        let handle = match self.git.find_remote(remote) {
            Ok(r) => r,
            Err(_) => {
                self.git.remote_at(remote).map_err(|e| anyhow!("'{remote}' is neither a remote nor a URL: {e}"))?
            }
        };
        let connection = handle
            .connect(gix::remote::Direction::Fetch)
            .with_context(|| format!("could not connect to '{remote}'"))?;
        let options =
            gix::remote::ref_map::Options { prefix_from_spec_as_filter_on_remote: false, ..Default::default() };
        let (map, _) = connection
            .ref_map(gix::progress::Discard, options)
            .with_context(|| format!("could not list the refs of '{remote}'"))?;
        use gix::protocol::handshake::Ref;
        let mut out: Vec<RemoteRef> = map
            .remote_refs
            .iter()
            .map(|r| match r {
                Ref::Peeled { full_ref_name, tag, object } => (full_ref_name.to_string(), Some(*tag), Some(*object)),
                Ref::Direct { full_ref_name, object } => (full_ref_name.to_string(), Some(*object), None),
                Ref::Symbolic { full_ref_name, tag, object, .. } => {
                    (full_ref_name.to_string(), Some(tag.unwrap_or(*object)), tag.map(|_| *object))
                }
                Ref::Unborn { full_ref_name, .. } => (full_ref_name.to_string(), None, None),
            })
            .collect();
        out.sort_by(|a, b| (a.0 != "HEAD", &a.0).cmp(&(b.0 != "HEAD", &b.0)));
        Ok(out)
    }

    /// Clone `url` into `path` without checking anything out (no index, empty worktree).
    pub fn clone_from_no_checkout(url: &str, path: &Path) -> Result<Repo> {
        Self::clone_from_no_checkout_with(url, path, Depth::Unchanged)
    }

    /// Like [`Repo::clone_from_no_checkout`], optionally shallow.
    pub fn clone_from_no_checkout_with(url: &str, path: &Path, depth: Depth) -> Result<Repo> {
        if path.exists() && std::fs::read_dir(path).map(|mut d| d.next().is_some()).unwrap_or(false) {
            bail!("destination path '{}' already exists and is not an empty directory", path.display());
        }
        std::fs::create_dir_all(path).with_context(|| format!("could not create {}", path.display()))?;
        let mut prepare = gix::prepare_clone(url, path)
            .with_context(|| format!("could not prepare clone of {url}"))?
            .with_shallow(depth.shallow()?);
        let (repo, _) = prepare
            .fetch_only(gix::progress::Discard, &NEVER_INTERRUPT)
            .with_context(|| format!("could not fetch {url}"))?;
        Ok(Repo::from_gix(repo))
    }

    /// Clone `url` into `path` and check out the default branch.
    pub fn clone_from(url: &str, path: &Path) -> Result<Repo> {
        Self::clone_from_with(url, path, Depth::Unchanged)
    }

    /// Like [`Repo::clone_from`], optionally shallow (`--depth`).
    pub fn clone_from_with(url: &str, path: &Path, depth: Depth) -> Result<Repo> {
        if path.exists() && std::fs::read_dir(path).map(|mut d| d.next().is_some()).unwrap_or(false) {
            bail!("destination path '{}' already exists and is not an empty directory", path.display());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;
        }
        let mut prepare = gix::prepare_clone(url, path)
            .with_context(|| format!("could not prepare clone of {url}"))?
            .with_shallow(depth.shallow()?);
        let (mut checkout, _) = prepare
            .fetch_then_checkout(gix::progress::Discard, &NEVER_INTERRUPT)
            .with_context(|| format!("could not fetch {url}"))?;
        let (repo, _) = checkout
            .main_worktree(gix::progress::Discard, &NEVER_INTERRUPT)
            .context("could not check out the working tree")?;
        Ok(Repo::from_gix(repo))
    }
}
