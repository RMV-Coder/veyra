//! Partial clones (`--filter=blob:none` and friends).
//!
//! gitoxide fetches whole histories only and can't fetch a missing object on demand, so Veyra
//! drives the fetch protocol itself here (on gitoxide's transport and handshake): it asks for
//! exactly the objects it wants, optionally with an object filter or a depth, and stores the pack.
//!
//! The repository is configured exactly like Git's partial clones (`remote.<name>.promisor`,
//! `remote.<name>.partialclonefilter`, `core.repositoryformatversion = 1`), so Git works in it
//! too. Objects the filter left out are fetched from the promisor remote when Veyra needs them:
//! in batches before checkouts, merges and diffs, and one at a time on any other blob read.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, anyhow, bail};
use gix::ObjectId;
use gix::bstr::ByteSlice;
use gix::objs::tree::EntryKind;
use gix::protocol::fetch::{Arguments, Negotiate, negotiate};

use crate::Repo;
use crate::flat::FlatTree;

static NEVER: AtomicBool = AtomicBool::new(false);

/// A negotiation that wants exactly `wants`, advertises `haves` once, and is done.
struct Exact<'a> {
    wants: &'a [ObjectId],
    haves: &'a [ObjectId],
    filter: Option<&'a str>,
    filter_unsupported: bool,
}

impl Negotiate for Exact<'_> {
    fn mark_complete_and_common_ref(&mut self) -> gix::Result<negotiate::Action> {
        Ok(negotiate::Action::MustNegotiate { remote_ref_target_known: Vec::new() })
    }

    fn add_wants(&mut self, arguments: &mut Arguments, _known: &[bool]) -> bool {
        for w in self.wants {
            arguments.want(w);
        }
        if let Some(f) = self.filter {
            if arguments.can_use_filter() {
                arguments.filter(f);
            } else {
                self.filter_unsupported = true;
            }
        }
        !self.wants.is_empty()
    }

    fn one_round(
        &mut self,
        _state: &mut negotiate::one_round::State,
        arguments: &mut Arguments,
        _previous: Option<&gix::protocol::fetch::Response>,
    ) -> gix::Result<(negotiate::Round, bool)> {
        for h in self.haves {
            arguments.have(h);
        }
        let round = negotiate::Round {
            haves_sent: self.haves.len(),
            in_vain: 0,
            haves_to_send: 0,
            previous_response_had_at_least_one_in_common: false,
        };
        Ok((round, true))
    }
}

/// How to fetch.
#[derive(Debug, Clone, Default)]
pub struct FetchRequest {
    pub wants: Vec<ObjectId>,
    /// Objects the remote may assume we have (our ref tips), so it sends only what's new.
    pub haves: Vec<ObjectId>,
    /// An object filter, e.g. `blob:none`, `blob:limit=1m`, `tree:0`.
    pub filter: Option<String>,
    /// Keep at most this many commits of history (shallow).
    pub depth: Option<u32>,
}

impl Repo {
    /// The name of the remote that promises the objects this repository lacks, if it's a partial clone.
    pub fn promisor_remote(&self) -> Option<String> {
        if let Some(name) = self.config_get("extensions.partialClone").filter(|n| !n.is_empty()) {
            return Some(name);
        }
        self.git
            .remote_names()
            .into_iter()
            .map(|n| n.to_str_lossy().into_owned())
            .find(|name| self.config_bool(&format!("remote.{name}.promisor")).unwrap_or(false))
    }

    /// Is this a partial clone?
    pub fn is_partial(&self) -> bool {
        self.promisor_remote().is_some()
    }

    fn remote_url(&self, name: &str) -> Result<gix::Url> {
        let remote = self.git.find_remote(name).map_err(|e| anyhow!("no such remote '{name}': {e}"))?;
        Ok(remote.url(gix::remote::Direction::Fetch).ok_or_else(|| anyhow!("remote '{name}' has no URL"))?.to_owned())
    }

    /// Run one fetch against `url` and store the pack it returns. Returns `false` when the
    /// server couldn't apply the filter (it then sent everything requested, unfiltered).
    pub fn fetch_exact(&self, url: &gix::Url, request: &FetchRequest) -> Result<bool> {
        use gix::protocol::transport::client::blocking_io::connect;
        if request.wants.is_empty() {
            return Ok(true);
        }
        let display = url.to_bstring().to_string();
        let ssh =
            if url.scheme == gix::url::Scheme::Ssh { self.git.ssh_connect_options()? } else { Default::default() };
        let mut transport = connect::connect(
            url.clone(),
            connect::Options { version: gix::protocol::transport::Protocol::V2, ssh, trace: false },
        )
        .with_context(|| format!("could not connect to {display}"))?;
        let (mut cascade, _, prompt) = self.git.config_snapshot().credential_helpers(url.clone())?;
        let authenticate = move |action| cascade.invoke(action, prompt.clone());
        let mut handshake = gix::protocol::handshake(
            &mut transport,
            gix::protocol::transport::Service::UploadPack,
            authenticate,
            Vec::new(),
            &mut gix::progress::Discard,
        )
        .with_context(|| format!("could not talk to {display} (git-upload-pack)"))?;

        let mut negotiator = Exact {
            wants: &request.wants,
            haves: &request.haves,
            filter: request.filter.as_deref(),
            filter_unsupported: false,
        };
        let shallow = match request.depth {
            Some(d) => gix::protocol::fetch::Shallow::DepthAtRemote(
                std::num::NonZeroU32::new(d).context("--depth must be at least 1")?,
            ),
            None => gix::protocol::fetch::Shallow::NoChange,
        };
        let pack_dir = self.common_dir().join("objects").join("pack");
        std::fs::create_dir_all(&pack_dir)?;
        let objects = self.git.objects.clone().into_arc()?;
        let promisor = request.filter.is_some() || self.is_partial();
        let result = gix::protocol::fetch(
            &mut negotiator,
            |reader, progress, interrupt| -> gix::Result<bool> {
                let outcome = gix::odb::pack::Bundle::write_to_directory(
                    reader,
                    Some(&pack_dir),
                    progress,
                    interrupt,
                    Some(objects),
                    self.object_hash(),
                    Default::default(),
                )
                .map_err(|e| {
                    gix::Error::from_error(std::io::Error::other(format!("could not store the received pack: {e}")))
                })?;
                if let Some(keep) = &outcome.keep_path {
                    let _ = std::fs::remove_file(keep);
                }
                // Like Git, mark packs from a promisor remote: objects they reference may be
                // missing on purpose (`git fsck` and `git gc` rely on this marker).
                if promisor && let Some(data) = &outcome.data_path {
                    let _ = std::fs::write(data.with_extension("promisor"), "");
                }
                Ok(true)
            },
            gix::progress::Discard,
            &NEVER,
            gix::protocol::fetch::Context {
                handshake: &mut handshake,
                transport: &mut transport,
                user_agent: ("agent", Some(format!("veyra/{}", env!("CARGO_PKG_VERSION")))),
                trace_packetlines: false,
            },
            gix::protocol::fetch::Options {
                shallow_file: self.git.shallow_file()?,
                shallow: &shallow,
                tags: gix::remote::fetch::Tags::None,
                reject_shallow_remote: false,
            },
        );
        result.map_err(|e| anyhow!("fetch from {display} failed: {e}"))?;
        Ok(!negotiator.filter_unsupported)
    }

    /// Fetch `ids` from the promisor remote if any of them are missing. A no-op in repositories
    /// that aren't partial clones.
    pub fn ensure_objects(&self, ids: impl IntoIterator<Item = ObjectId>) -> Result<usize> {
        let Some(remote) = self.promisor_remote() else { return Ok(0) };
        let missing: BTreeSet<ObjectId> =
            ids.into_iter().filter(|id| !id.is_null() && !self.git.has_object(*id)).collect();
        if missing.is_empty() {
            return Ok(0);
        }
        let url = self.remote_url(&remote)?;
        let wants: Vec<ObjectId> = missing.iter().copied().collect();
        self.fetch_exact(&url, &FetchRequest { wants, ..Default::default() })
            .with_context(|| format!("could not fetch {} missing object(s) from '{remote}'", missing.len()))?;
        Ok(missing.len())
    }

    /// Make sure `tree` and every tree below it are present: a `tree:<depth>` filter leaves them
    /// on the promisor remote. Missing trees are fetched one level at a time, each batch with
    /// `blob:none` (as Git's lazy fetch does), which also brings the trees below them. Returns
    /// how many were fetched.
    pub fn ensure_trees(&self, tree: ObjectId) -> Result<usize> {
        let Some(remote) = self.promisor_remote() else { return Ok(0) };
        let mut fetched = 0;
        let mut seen = HashSet::new();
        let mut level = vec![tree];
        while !level.is_empty() {
            let missing: Vec<ObjectId> = level.iter().copied().filter(|id| !self.git.has_object(*id)).collect();
            if !missing.is_empty() {
                let url = self.remote_url(&remote)?;
                let request =
                    FetchRequest { wants: missing.clone(), filter: Some("blob:none".into()), ..Default::default() };
                self.fetch_exact(&url, &request)
                    .with_context(|| format!("could not fetch {} missing tree(s) from '{remote}'", missing.len()))?;
                fetched += missing.len();
            }
            let mut next = Vec::new();
            for id in level {
                if !seen.insert(id) {
                    continue;
                }
                let tree = self.git.find_tree(id).with_context(|| format!("could not read tree {id}"))?;
                for entry in tree.iter() {
                    let entry = entry?;
                    if entry.mode().is_tree() {
                        next.push(entry.oid().to_owned());
                    }
                }
            }
            level = next;
        }
        Ok(fetched)
    }

    /// Make sure the blobs of `tree` (only under `paths`, if given) are present.
    pub fn ensure_tree_blobs(&self, flat: &FlatTree, sparse: Option<&crate::sparse::Sparse>) -> Result<usize> {
        if !self.is_partial() {
            return Ok(0);
        }
        let ids = flat
            .iter()
            .filter(|(p, e)| {
                matches!(e.kind, EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link)
                    && sparse.is_none_or(|s| s.includes(p.as_ref()))
            })
            .map(|(_, e)| e.id);
        self.ensure_objects(ids)
    }

    /// Before a three-way merge, fetch the blobs that differ between the trees.
    pub(crate) fn ensure_merge_blobs(&self, base: ObjectId, ours: ObjectId, theirs: ObjectId) -> Result<()> {
        if !self.is_partial() {
            return Ok(());
        }
        let (b, o, t) = (self.flat_from_tree(base)?, self.flat_from_tree(ours)?, self.flat_from_tree(theirs)?);
        let mut ids = HashSet::new();
        for side in [&o, &t] {
            for change in crate::flat::diff(&b, side) {
                ids.extend(change.old.map(|e| e.id));
                ids.extend(change.new.map(|e| e.id));
                // The other side's version is needed for a content merge too.
                ids.extend(o.get(&change.path).map(|e| e.id));
                ids.extend(t.get(&change.path).map(|e| e.id));
            }
        }
        self.ensure_objects(ids)?;
        Ok(())
    }

    /// `fetch` in a partial clone: new commits and trees only (the filter applies), refs updated
    /// like a normal fetch. Returns the updated refs.
    pub fn fetch_partial(&self, remote: &str) -> Result<Vec<(String, String)>> {
        let url = self.remote_url(remote)?;
        let filter = self.config_get(&format!("remote.{remote}.partialclonefilter"));
        let refs = self.remote_refs(remote)?;
        let haves: Vec<ObjectId> = self.all_refs()?.into_values().collect();
        let mut wants = Vec::new();
        let mut updates = Vec::new();
        for (name, id, _) in &refs {
            let Some(id) = id else { continue };
            let local = if let Some(b) = name.strip_prefix("refs/heads/") {
                format!("refs/remotes/{remote}/{b}")
            } else if name.starts_with("refs/tags/") {
                name.clone()
            } else {
                continue;
            };
            if self.ref_target(&local)? != Some(*id) {
                if !self.git.has_object(*id) {
                    wants.push(*id);
                }
                updates.push((local, *id));
            }
        }
        wants.sort();
        wants.dedup();
        self.fetch_exact(&url, &FetchRequest { wants, haves, filter, depth: None })?;
        let mut out = Vec::new();
        for (local, id) in updates {
            let mode = if self.ref_target(&local)?.is_some() { "updated" } else { "new" };
            self.set_ref(&local, id, None, &format!("fetch: {mode}"))?;
            out.push((local, mode.to_owned()));
        }
        Ok(out)
    }

    /// Clone with an object filter (and optionally a depth), the way `git clone --filter` does:
    /// commits and trees now, blobs only for what gets checked out (within `sparse`, if given).
    pub fn clone_partial(
        url: &str,
        path: &Path,
        filter: &str,
        depth: Option<u32>,
        scope: Option<crate::sparse::ScopeRequest>,
    ) -> Result<(Repo, Option<Vec<String>>)> {
        if path.exists() && std::fs::read_dir(path).map(|mut d| d.next().is_some()).unwrap_or(false) {
            bail!("destination path '{}' already exists and is not an empty directory", path.display());
        }
        let repo = Repo::init(path, None)?;
        let config = repo.config_path(crate::config::Scope::Local)?;
        crate::config::set_in_file(&config, "core.repositoryformatversion", "1")?;
        repo.remote_add("origin", url)?;
        crate::config::set_in_file(&config, "remote.origin.promisor", "true")?;
        crate::config::set_in_file(&config, "remote.origin.partialclonefilter", filter)?;
        let repo = Repo::open(path)?;

        let refs = repo.remote_refs("origin")?;
        let remote_head = refs.iter().find(|(n, ..)| n == "HEAD").and_then(|(_, id, _)| *id);
        let mut wants: Vec<ObjectId> = refs
            .iter()
            .filter(|(n, ..)| n.starts_with("refs/heads/") || n.starts_with("refs/tags/"))
            .filter_map(|(_, id, _)| *id)
            .collect();
        wants.sort();
        wants.dedup();
        if wants.is_empty() {
            return Ok((repo, None)); // an empty repository
        }
        let gurl = repo.remote_url("origin")?;
        let filtered = repo
            .fetch_exact(&gurl, &FetchRequest { wants, haves: Vec::new(), filter: Some(filter.to_owned()), depth })?;
        if !filtered {
            // The server can't filter: this is now a full clone, so don't pretend otherwise.
            crate::config::unset_in_file(&config, "remote.origin.promisor")?;
            crate::config::unset_in_file(&config, "remote.origin.partialclonefilter")?;
        }
        let repo = Repo::open(path)?;

        // Refs, like a normal clone.
        let mut default_branch = None;
        for (name, id, _) in &refs {
            let Some(id) = id else { continue };
            if let Some(b) = name.strip_prefix("refs/heads/") {
                repo.set_ref(&format!("refs/remotes/origin/{b}"), *id, None, "clone: from origin")?;
                if Some(*id) == remote_head && default_branch.is_none() {
                    default_branch = Some(b.to_owned());
                }
            } else if name.starts_with("refs/tags/") {
                repo.set_ref(name, *id, None, "clone: from origin")?;
            }
        }
        let Some(branch) =
            default_branch.or_else(|| refs.iter().find_map(|(n, ..)| n.strip_prefix("refs/heads/").map(str::to_owned)))
        else {
            return Ok((repo, None));
        };
        let tip = repo.ref_target(&format!("refs/remotes/origin/{branch}"))?.context("lost the default branch")?;
        repo.set_ref(&format!("refs/heads/{branch}"), tip, None, "clone: from origin")?;
        repo.set_head_symbolic(&format!("refs/heads/{branch}"), "clone")?;
        crate::config::set_in_file(&config, &format!("branch.{branch}.remote"), "origin")?;
        crate::config::set_in_file(&config, &format!("branch.{branch}.merge"), &format!("refs/heads/{branch}"))?;
        let _ = std::fs::write(
            repo.git_dir().join("refs").join("remotes").join("origin").join("HEAD"),
            format!("ref: refs/remotes/origin/{branch}\n"),
        );

        // Check out (sparse if asked): fetch exactly the blobs that will be written.
        let repo = Repo::open(path)?;
        let flat = repo.flat_from_commit(tip)?;
        let dirs = match scope {
            None => None,
            Some(crate::sparse::ScopeRequest::Dirs(d)) => Some(d),
            Some(crate::sparse::ScopeRequest::Named(name)) => {
                let entry = flat
                    .get(gix::bstr::BString::from(".veyra/scopes.toml").as_bstr())
                    .context("the repository has no .veyra/scopes.toml, so --scope names can't be resolved")?;
                repo.ensure_objects([entry.id])?;
                let scopes = crate::sparse::Scopes::parse(&repo.read_blob(entry.id)?.to_str_lossy())?;
                Some(scopes.get(&name)?.paths.clone())
            }
        };
        let sparse = match &dirs {
            Some(d) => Some(repo.write_sparse_cone(d)?),
            None => None,
        };
        repo.ensure_tree_blobs(&flat, sparse.as_ref())?;
        let writes: Vec<_> = flat
            .iter()
            .filter(|(p, _)| sparse.as_ref().is_none_or(|s| s.includes(p.as_ref())))
            .map(|(p, e)| (p.clone(), Some(*e)))
            .collect();
        let stats = repo.write_worktree(&writes)?;
        repo.write_index_from_flat(&flat, &stats)?;
        Ok((repo, sparse.map(|s| s.dirs)))
    }

    /// `fetch --refetch`-style backfill: download every blob reachable from HEAD that's missing
    /// (turns a blobless clone's current checkout fully local, e.g. before going offline).
    pub fn backfill(&self) -> Result<usize> {
        let Some(head) = self.head_commit_id()? else { return Ok(0) };
        let flat = self.flat_from_commit(head)?;
        let ids: Vec<ObjectId> = flat.values().filter(|e| e.kind != EntryKind::Commit).map(|e| e.id).collect();
        self.ensure_objects(ids)
    }
}

/// Parse `--filter` values Veyra accepts (the common ones Git servers support).
pub fn validate_filter(spec: &str) -> Result<()> {
    let ok = spec == "blob:none"
        || spec.strip_prefix("blob:limit=").is_some_and(|n| {
            let digits = n.trim_end_matches(['k', 'K', 'm', 'M', 'g', 'G']);
            !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
        })
        || spec.strip_prefix("tree:").is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()) && !n.is_empty());
    if !ok {
        bail!("unsupported --filter '{spec}' (use blob:none, blob:limit=<size>, or tree:<depth>)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn filters() {
        for ok in ["blob:none", "blob:limit=1m", "blob:limit=500", "tree:0"] {
            super::validate_filter(ok).unwrap();
        }
        for bad in ["blob", "blob:limit=", "tree:x", "sparse:oid=abc"] {
            assert!(super::validate_filter(bad).is_err(), "{bad}");
        }
    }
}
