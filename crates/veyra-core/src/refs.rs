//! Reading and editing references (branches, tags, remote-tracking refs).

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail};
use gix::ObjectId;
use gix::bstr::ByteSlice;
use gix::refs::transaction::{Change, PreviousValue, RefEdit, RefLog};

use crate::Repo;

/// The namespace Veyra uses for its own bookkeeping refs. Never part of a user-facing view.
pub const VEYRA_REF_PREFIX: &str = "refs/veyra/";

/// Validate a short branch name and return `refs/heads/<name>`.
pub fn branch_full_name(short: &str) -> Result<String> {
    if short == "HEAD" || short.starts_with('-') || short.starts_with("refs/") {
        bail!("'{short}' is not a valid branch name");
    }
    let full = format!("refs/heads/{short}");
    gix::validate::reference::branch_name(full.as_bytes().as_bstr())
        .map_err(|err| anyhow!("'{short}' is not a valid branch name: {err}"))?;
    Ok(full)
}

/// Validate a short tag name and return `refs/tags/<name>`.
pub fn tag_full_name(short: &str) -> Result<String> {
    let full = format!("refs/tags/{short}");
    gix::validate::reference::name(full.as_bytes().as_bstr())
        .map_err(|err| anyhow!("'{short}' is not a valid tag name: {err}"))?;
    Ok(full)
}

/// Strip the well-known prefixes for display.
pub fn shorten(full: &str) -> &str {
    for prefix in ["refs/heads/", "refs/tags/", "refs/remotes/"] {
        if let Some(s) = full.strip_prefix(prefix) {
            return s;
        }
    }
    full
}

/// A branch and the commit it points to.
#[derive(Debug, Clone)]
pub struct BranchInfo {
    pub name: String,
    pub full_name: String,
    pub target: ObjectId,
    pub is_head: bool,
    pub is_remote: bool,
}

impl Repo {
    /// Every reference except Veyra's own, mapped to the object it points to (unpeeled).
    pub fn all_refs(&self) -> Result<BTreeMap<String, ObjectId>> {
        let mut out = BTreeMap::new();
        let platform = self.git.references()?;
        for reference in platform.all()? {
            let reference = reference.map_err(|e| anyhow!("could not read reference: {e}"))?;
            let name = reference.name().as_bstr().to_str_lossy().into_owned();
            if name.starts_with(VEYRA_REF_PREFIX) {
                continue;
            }
            if let gix::refs::TargetRef::Object(id) = reference.target() {
                out.insert(name, id.to_owned());
            }
        }
        Ok(out)
    }

    /// Local branches, plus remote-tracking branches when `include_remote` is set.
    pub fn branches(&self, include_remote: bool) -> Result<Vec<BranchInfo>> {
        let head = self.head_state()?;
        let head_name = match &head {
            crate::HeadState::Branch { name, .. } => Some(name.to_str_lossy().into_owned()),
            crate::HeadState::Detached(_) => None,
        };
        let mut out = Vec::new();
        for (full, target) in self.all_refs()? {
            let (short, is_remote) = if let Some(s) = full.strip_prefix("refs/heads/") {
                (s.to_owned(), false)
            } else if let Some(s) = full.strip_prefix("refs/remotes/") {
                if !include_remote || s.ends_with("/HEAD") {
                    continue;
                }
                (s.to_owned(), true)
            } else {
                continue;
            };
            let target = self.peel_to_commit(target).unwrap_or(target);
            out.push(BranchInfo {
                is_head: head_name.as_deref() == Some(full.as_str()),
                name: short,
                full_name: full,
                target,
                is_remote,
            });
        }
        Ok(out)
    }

    /// Tags as `(short name, target object)`.
    pub fn tags(&self) -> Result<Vec<(String, ObjectId)>> {
        Ok(self
            .all_refs()?
            .into_iter()
            .filter_map(|(full, id)| full.strip_prefix("refs/tags/").map(|s| (s.to_owned(), id)))
            .collect())
    }

    /// Follow tag objects until a commit is reached.
    pub fn peel_to_commit(&self, id: ObjectId) -> Result<ObjectId> {
        let object = self.git.find_object(id)?;
        Ok(object.peel_to_kind(gix::object::Kind::Commit)?.id)
    }

    /// The object a reference points to, if it exists.
    pub fn ref_target(&self, full_name: &str) -> Result<Option<ObjectId>> {
        let Some(mut reference) = self.git.try_find_reference(full_name)? else {
            return Ok(None);
        };
        match reference.target() {
            gix::refs::TargetRef::Object(id) => Ok(Some(id.to_owned())),
            gix::refs::TargetRef::Symbolic(_) => Ok(Some(reference.peel_to_id()?.detach())),
        }
    }

    /// Create or move a reference. `expected` guards against concurrent modification:
    /// `Some(None)` = must not exist, `Some(Some(id))` = must currently point to `id`,
    /// `None` = no check.
    pub fn set_ref(
        &self,
        full_name: &str,
        target: ObjectId,
        expected: Option<Option<ObjectId>>,
        reflog: &str,
    ) -> Result<()> {
        let name: gix::refs::FullName =
            full_name.try_into().with_context(|| format!("invalid ref name {full_name}"))?;
        let previous = match expected {
            None => PreviousValue::Any,
            Some(None) => PreviousValue::MustNotExist,
            Some(Some(id)) => PreviousValue::MustExistAndMatch(gix::refs::Target::Object(id)),
        };
        self.git
            .edit_reference(RefEdit::update(name, target, previous, reflog))
            .with_context(|| format!("could not update {full_name}"))?;
        Ok(())
    }

    /// Delete a reference (and its reflog). `expected` works as in [`Repo::set_ref`].
    pub fn delete_ref(&self, full_name: &str, expected: Option<ObjectId>) -> Result<()> {
        let name: gix::refs::FullName =
            full_name.try_into().with_context(|| format!("invalid ref name {full_name}"))?;
        let previous = match expected {
            None => PreviousValue::MustExist,
            Some(id) => PreviousValue::MustExistAndMatch(gix::refs::Target::Object(id)),
        };
        self.git
            .edit_reference(RefEdit::new(name, Change::Delete { expected: previous, log: RefLog::AndReference }))
            .with_context(|| format!("could not delete {full_name}"))?;
        Ok(())
    }

    /// Apply many ref updates/deletions in one transaction. `None` deletes.
    pub fn set_refs(&self, edits: &[(String, Option<ObjectId>)], reflog: &str) -> Result<()> {
        let mut out = Vec::new();
        for (full, target) in edits {
            let name: gix::refs::FullName =
                full.as_str().try_into().with_context(|| format!("invalid ref name {full}"))?;
            out.push(match target {
                Some(id) => RefEdit::update(name, *id, PreviousValue::Any, reflog),
                None => RefEdit::new(name, Change::Delete { expected: PreviousValue::Any, log: RefLog::AndReference }),
            });
        }
        if !out.is_empty() {
            self.git.edit_references(out)?;
        }
        Ok(())
    }

    /// Update whatever `HEAD` points to (the current branch, or `HEAD` itself when detached).
    pub fn update_head_target(&self, new: ObjectId, expected: Option<ObjectId>, reflog: &str) -> Result<()> {
        let previous = match expected {
            Some(id) => PreviousValue::MustExistAndMatch(gix::refs::Target::Object(id)),
            None => PreviousValue::MustNotExist,
        };
        self.git
            .edit_reference(RefEdit::update("HEAD".try_into().expect("valid"), new, previous, reflog).with_deref(true))
            .context("could not update HEAD")?;
        Ok(())
    }

    /// The default branch to compare against: `init.defaultBranch`, else `main`, else `master`.
    pub fn default_branch(&self) -> Result<Option<String>> {
        let configured = self.git.config_snapshot().string("init.defaultBranch").map(|s| s.to_string());
        for candidate in configured.into_iter().chain(["main".to_owned(), "master".to_owned()]) {
            if self.ref_target(&format!("refs/heads/{candidate}"))?.is_some() {
                return Ok(Some(candidate));
            }
        }
        Ok(None)
    }
}

impl Repo {
    /// Rename a branch like `git branch -m`: the new branch inherits the old one's reflog, and
    /// `HEAD` follows if it pointed at the old branch.
    pub fn rename_branch(&self, old_full: &str, new_full: &str) -> Result<()> {
        let target = self.ref_target(old_full)?.with_context(|| format!("branch '{}' not found", shorten(old_full)))?;
        if self.ref_target(new_full)?.is_some() {
            bail!("a branch named '{}' already exists", shorten(new_full));
        }
        let logs = self.common_dir().join("logs");
        let old_log = std::fs::read(logs.join(old_full)).ok();
        let msg = format!("Branch: renamed {old_full} to {new_full}");
        self.set_ref(new_full, target, Some(None), &msg)?;
        if let Some(old) = old_log {
            // Keep the history: old entries first, then the rename entry gix just wrote.
            let new_path = logs.join(new_full);
            let created = std::fs::read(&new_path).unwrap_or_default();
            let mut merged = old;
            merged.extend_from_slice(&created);
            if let Some(parent) = new_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&new_path, merged)?;
        }
        let head_follows = matches!(self.head_state()?, crate::HeadState::Branch { ref name, .. } if name == old_full);
        if head_follows {
            self.set_head_symbolic(new_full, &msg)?;
        }
        self.delete_ref(old_full, Some(target))?;
        // Upstream configuration moves with the branch.
        let (old_short, new_short) = (shorten(old_full).to_owned(), shorten(new_full).to_owned());
        let config = self.config_path(crate::config::Scope::Local)?;
        for key in ["remote", "merge", "pushRemote", "rebase"] {
            if let Some(v) = self.config_get(&format!("branch.{old_short}.{key}")) {
                crate::config::set_in_file(&config, &format!("branch.{new_short}.{key}"), &v)?;
            }
        }
        crate::config::remove_section_in_file(&config, "branch", Some(&old_short))?;
        Ok(())
    }
}

impl Repo {
    /// How many commits `a` has that `b` doesn't (ahead), and vice versa (behind).
    pub fn ahead_behind(&self, a: ObjectId, b: ObjectId) -> Result<(usize, usize)> {
        let count = |from: ObjectId, hide: ObjectId| -> Result<usize> {
            let walk = self.git.rev_walk([from]).with_hidden([hide]).all()?;
            let mut n = 0;
            for info in walk {
                info.map_err(|e| anyhow!("history traversal failed: {e}"))?;
                n += 1;
            }
            Ok(n)
        };
        Ok((count(a, b)?, count(b, a)?))
    }
}
