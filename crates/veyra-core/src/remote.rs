//! Remotes: configuration, fetch and clone (through gitoxide's protocol implementation).
//!
//! Push is not implemented yet: gitoxide does not provide it, and Veyra will build it on
//! `gix-protocol` in Phase 2 (see docs/ROADMAP.md).

use std::path::Path;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, anyhow, bail};
use gix::bstr::ByteSlice;

use crate::Repo;
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

    /// Clone `url` into `path` without checking anything out (no index, empty worktree).
    pub fn clone_from_no_checkout(url: &str, path: &Path) -> Result<Repo> {
        if path.exists() && std::fs::read_dir(path).map(|mut d| d.next().is_some()).unwrap_or(false) {
            bail!("destination path '{}' already exists and is not an empty directory", path.display());
        }
        std::fs::create_dir_all(path).with_context(|| format!("could not create {}", path.display()))?;
        let mut prepare = gix::prepare_clone(url, path).with_context(|| format!("could not prepare clone of {url}"))?;
        let (repo, _) = prepare
            .fetch_only(gix::progress::Discard, &NEVER_INTERRUPT)
            .with_context(|| format!("could not fetch {url}"))?;
        Ok(Repo::from_gix(repo))
    }

    /// Clone `url` into `path` and check out the default branch.
    pub fn clone_from(url: &str, path: &Path) -> Result<Repo> {
        if path.exists() && std::fs::read_dir(path).map(|mut d| d.next().is_some()).unwrap_or(false) {
            bail!("destination path '{}' already exists and is not an empty directory", path.display());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;
        }
        let mut prepare = gix::prepare_clone(url, path).with_context(|| format!("could not prepare clone of {url}"))?;
        let (mut checkout, _) = prepare
            .fetch_then_checkout(gix::progress::Discard, &NEVER_INTERRUPT)
            .with_context(|| format!("could not fetch {url}"))?;
        let (repo, _) = checkout
            .main_worktree(gix::progress::Discard, &NEVER_INTERRUPT)
            .context("could not check out the working tree")?;
        Ok(Repo::from_gix(repo))
    }
}
