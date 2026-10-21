//! Submodules: `.gitmodules`, gitlinks (mode 160000) in the index, and nested repositories.
//!
//! Veyra clones a submodule directly into its path (the "embedded" layout that Git still
//! supports; `git submodule absorbgitdirs` converts it to `.git/modules/<name>`), then checks
//! out the commit the superproject records, detached — exactly what `git submodule update`
//! produces for users.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use gix::ObjectId;
use gix::bstr::{BString, ByteSlice};
use gix::objs::tree::EntryKind;

use crate::Repo;
use crate::checkout::SwitchTarget;
use crate::flat::FlatEntry;

/// One submodule as the superproject sees it.
#[derive(Debug, Clone)]
pub struct SubmoduleInfo {
    pub name: String,
    pub path: String,
    pub url: String,
    /// The commit the superproject records (index gitlink).
    pub recorded: Option<ObjectId>,
    /// The commit checked out in the submodule, if it's cloned.
    pub checked_out: Option<ObjectId>,
}

impl SubmoduleInfo {
    /// Git's status prefix: `-` not initialized, `+` different commit, ` ` up to date.
    pub fn status_char(&self) -> char {
        match (self.checked_out, self.recorded) {
            (None, _) => '-',
            (Some(a), Some(b)) if a != b => '+',
            _ => ' ',
        }
    }
}

impl Repo {
    /// All submodules declared in `.gitmodules`.
    pub fn submodule_list(&self) -> Result<Vec<SubmoduleInfo>> {
        let Some(modules) = self.git.submodules()? else { return Ok(Vec::new()) };
        let index = self.flat_index()?;
        let workdir = self.workdir()?.to_owned();
        let mut out = Vec::new();
        for sm in modules {
            let path = sm.path()?.to_str_lossy().into_owned();
            let url = sm.url().map(|u| u.to_bstring().to_string()).unwrap_or_default();
            let recorded = index.get(path.as_bytes().as_bstr()).filter(|e| e.kind == EntryKind::Commit).map(|e| e.id);
            let checked_out = submodule_head(&workdir.join(&path));
            out.push(SubmoduleInfo { name: sm.name().to_str_lossy().into_owned(), path, url, recorded, checked_out });
        }
        Ok(out)
    }

    /// Resolve a submodule URL: `./x` and `../x` are relative to the superproject's `origin`
    /// URL (or, without a remote, to the superproject directory), like Git.
    fn resolve_submodule_url(&self, url: &str) -> Result<String> {
        if !(url.starts_with("./") || url.starts_with("../")) {
            return Ok(url.to_owned());
        }
        let base = self
            .config_get("remote.origin.url")
            .unwrap_or_else(|| self.workdir().map(|w| w.display().to_string()).unwrap_or_default());
        let mut base = base.trim_end_matches('/').trim_end_matches(".git").to_owned();
        let mut rest = url;
        loop {
            if let Some(r) = rest.strip_prefix("./") {
                rest = r;
            } else if let Some(r) = rest.strip_prefix("../") {
                rest = r;
                match base.rfind(['/', '\\', ':']) {
                    Some(i) => base.truncate(i),
                    None => bail!("cannot resolve relative submodule url '{url}' against '{base}'"),
                }
            } else {
                break;
            }
        }
        Ok(format!("{base}/{rest}"))
    }

    /// Record submodule URLs in `.git/config` (`git submodule init`). Returns the initialized names.
    pub fn submodule_init(&self, only: &[String]) -> Result<Vec<String>> {
        let config = self.config_path(crate::config::Scope::Local)?;
        let mut done = Vec::new();
        for sm in self.submodule_list()? {
            if !only.is_empty() && !only.iter().any(|p| *p == sm.path || *p == sm.name) {
                continue;
            }
            if self.config_get(&format!("submodule.{}.url", sm.name)).is_none() {
                let url = self.resolve_submodule_url(&sm.url)?;
                crate::config::set_in_file(&config, &format!("submodule.{}.url", sm.name), &url)?;
                crate::config::set_in_file(&config, &format!("submodule.{}.active", sm.name), "true")?;
                done.push(sm.name.clone());
            }
        }
        Ok(done)
    }

    /// Clone missing submodules and check out the recorded commits (`git submodule update --init`).
    /// Returns `(path, commit)` for every submodule that changed.
    pub fn submodule_update(&self, init: bool, recursive: bool, only: &[String]) -> Result<Vec<(String, ObjectId)>> {
        if init && !self.submodule_init(only)?.is_empty() {
            // `init` wrote URLs to .git/config; re-open so the configuration snapshot sees them.
            return Repo::open(self.workdir()?)?.submodule_update(false, recursive, only);
        }
        let workdir = self.workdir()?.to_owned();
        let mut changed = Vec::new();
        for sm in self.submodule_list()? {
            if !only.is_empty() && !only.iter().any(|p| *p == sm.path || *p == sm.name) {
                continue;
            }
            let Some(target) = sm.recorded else { continue };
            let Some(url) = self.config_get(&format!("submodule.{}.url", sm.name)) else {
                continue; // not initialized
            };
            let dest = workdir.join(&sm.path);
            let sub = if submodule_head(&dest).is_none() {
                if dest.exists() && std::fs::read_dir(&dest).map(|mut d| d.next().is_some()).unwrap_or(false) {
                    bail!("submodule path '{}' is not empty", sm.path);
                }
                Repo::clone_from(&url, &dest)
                    .with_context(|| format!("could not clone submodule '{}' from {url}", sm.name))?
            } else {
                Repo::open(&dest)?
            };
            if sub.head_commit_id()? == Some(target) {
                continue;
            }
            if !sub.git.has_object(target) {
                sub.fetch(Some("origin")).with_context(|| format!("could not fetch submodule '{}'", sm.name))?;
            }
            if !sub.git.has_object(target) {
                bail!("submodule '{}': the recorded commit {} is not on its remote", sm.name, crate::short(&target));
            }
            sub.switch(SwitchTarget::Detach(target), false)
                .with_context(|| format!("could not check out {} in submodule '{}'", crate::short(&target), sm.name))?;
            if recursive {
                sub.submodule_update(true, true, &[])?;
            }
            changed.push((sm.path.clone(), target));
        }
        Ok(changed)
    }

    /// Add a submodule: clone `url` into `path`, record it in `.gitmodules`, and stage both.
    pub fn submodule_add(&self, url: &str, path: &str) -> Result<ObjectId> {
        let workdir = self.workdir()?.to_owned();
        crate::worktree::validate_worktree_path(path.as_bytes().as_bstr())?;
        let dest = workdir.join(path);
        if dest.exists() && std::fs::read_dir(&dest).map(|mut d| d.next().is_some()).unwrap_or(false) {
            bail!("'{path}' already exists and is not empty");
        }
        let resolved = self.resolve_submodule_url(url)?;
        let sub = Repo::clone_from(&resolved, &dest)?;
        let head = sub.head_commit_id()?.context("the submodule repository has no commits")?;

        let gitmodules = workdir.join(".gitmodules");
        let name = path.to_owned();
        crate::config::set_in_file(&gitmodules, &format!("submodule.{name}.path"), path)?;
        crate::config::set_in_file(&gitmodules, &format!("submodule.{name}.url"), url)?;
        let config = self.config_path(crate::config::Scope::Local)?;
        crate::config::set_in_file(&config, &format!("submodule.{name}.url"), &resolved)?;
        crate::config::set_in_file(&config, &format!("submodule.{name}.active"), "true")?;

        let mut flat = self.flat_index()?;
        flat.insert(BString::from(path), FlatEntry::new(EntryKind::Commit, head));
        let modules_id = self.write_blob(&std::fs::read(&gitmodules)?)?;
        flat.insert(BString::from(".gitmodules"), FlatEntry::new(EntryKind::Blob, modules_id));
        self.write_index_from_flat(&flat, &BTreeMap::new())?;
        Ok(head)
    }

    /// Run `command` in every checked-out submodule (`git submodule foreach`).
    pub fn submodule_foreach(&self, command: &str) -> Result<()> {
        let workdir = self.workdir()?.to_owned();
        for sm in self.submodule_list()? {
            let dir = workdir.join(&sm.path);
            if submodule_head(&dir).is_none() {
                continue;
            }
            let mut cmd = gix::path::env::shell_command();
            cmd.arg("-c")
                .arg(command)
                .current_dir(&dir)
                .env("name", &sm.name)
                .env("sm_path", &sm.path)
                .env("displaypath", &sm.path)
                .env("sha1", sm.checked_out.map(|c| c.to_string()).unwrap_or_default())
                .env("toplevel", &workdir);
            let status = cmd.status().map_err(|e| anyhow!("could not run `{command}`: {e}"))?;
            if !status.success() {
                bail!("`{command}` failed in submodule '{}'", sm.path);
            }
        }
        Ok(())
    }
}

/// The commit checked out in the repository at `dir`, if there is one.
fn submodule_head(dir: &Path) -> Option<ObjectId> {
    if !dir.join(".git").exists() {
        return None;
    }
    Repo::open(dir).ok()?.head_commit_id().ok().flatten()
}
