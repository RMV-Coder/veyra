//! Moving the index and worktree between trees: switch, reset and restore.
//!
//! These all share one planner that carries over local changes on paths the move doesn't
//! touch, and refuses (rather than silently overwriting) when a path it must touch has
//! local changes. Higher layers snapshot the worktree before calling any of this, so even
//! `--force` remains recoverable.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use gix::ObjectId;
use gix::bstr::{BString, ByteSlice};

use crate::Repo;
use crate::flat::{self, FlatTree};
use crate::worktree::{Pathspec, SnapshotOptions};

/// Why a path blocks a checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocker {
    pub path: BString,
    pub reason: &'static str,
}

/// Raised when a checkout would overwrite local work.
#[derive(Debug)]
pub struct WouldOverwrite(pub Vec<Blocker>);

impl std::fmt::Display for WouldOverwrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "your local changes would be overwritten:")?;
        for b in self.0.iter().take(20) {
            writeln!(f, "    {}  ({})", b.path, b.reason)?;
        }
        if self.0.len() > 20 {
            writeln!(f, "    … and {} more", self.0.len() - 20)?;
        }
        write!(
            f,
            "  hint: commit them first, or pass --force (your work is checkpointed first, and `veyra undo` brings it back)"
        )
    }
}

impl std::error::Error for WouldOverwrite {}

/// How `reset` treats the index and worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

impl Repo {
    /// Move index and worktree from HEAD's tree to `target_tree`, carrying over unrelated local
    /// changes (like `git switch`). HEAD itself is not changed.
    pub fn checkout_tree(&self, target_tree: ObjectId, force: bool) -> Result<usize> {
        let head = self.flat_head()?;
        let index = self.flat_index()?;
        let target = self.flat_from_tree(target_tree)?;
        let changes = self.worktree_changes()?;
        let dirty: BTreeSet<BString> = changes.dirty_tracked();
        let untracked: BTreeSet<BString> = changes.untracked.iter().cloned().collect();

        let mut new_index = index.clone();
        let mut writes = Vec::new();
        let mut blockers = Vec::new();
        for change in flat::diff(&head, &target) {
            let p = &change.path;
            let staged = index.get(p).copied();
            if staged != change.old {
                if staged == change.new {
                    continue; // already staged exactly what the target has
                }
                blockers.push(Blocker { path: p.clone(), reason: "staged changes" });
                continue;
            }
            if dirty.contains(p) {
                blockers.push(Blocker { path: p.clone(), reason: "local changes" });
                continue;
            }
            if change.old.is_none() && change.new.is_some() {
                if untracked.contains(p) {
                    blockers.push(Blocker { path: p.clone(), reason: "untracked file would be overwritten" });
                    continue;
                }
                let mut prefix = p.clone();
                prefix.push(b'/');
                if untracked.iter().any(|u| u.starts_with(prefix.as_slice())) {
                    blockers.push(Blocker { path: p.clone(), reason: "untracked files in the way" });
                    continue;
                }
                if let Some(parent) = ancestors(p).find(|a| untracked.contains(a)) {
                    blockers.push(Blocker { path: parent, reason: "untracked file in the way" });
                    continue;
                }
            }
            match change.new {
                Some(e) => new_index.insert(p.clone(), e),
                None => new_index.remove(p),
            };
            writes.push((p.clone(), change.new));
        }
        if !blockers.is_empty() {
            if !force {
                return Err(WouldOverwrite(blockers).into());
            }
            for b in &blockers {
                let entry = target.get(&b.path).copied();
                match entry {
                    Some(e) => new_index.insert(b.path.clone(), e),
                    None => new_index.remove(&b.path),
                };
                writes.push((b.path.clone(), entry));
            }
        }
        let count = writes.len();
        let stats = self.write_worktree(&writes)?;
        self.write_index_from_flat(&new_index, &stats)?;
        Ok(count)
    }

    /// Switch to a branch (`refs/heads/<name>`) or detach at a commit.
    pub fn switch(&self, target: SwitchTarget, force: bool) -> Result<()> {
        let (commit, label) = match &target {
            SwitchTarget::Branch(full) => {
                let id = self.ref_target(full)?;
                (id, full.clone())
            }
            SwitchTarget::Detach(id) => (Some(*id), id.to_string()),
        };
        let tree = match commit {
            Some(c) => self.commit_tree_id(c)?,
            None => self.empty_tree_id(), // unborn branch
        };
        self.checkout_tree(tree, force)?;
        let from = self.head_label()?;
        let reflog = format!("checkout: moving from {from} to {}", crate::refs::shorten(&label));
        match target {
            SwitchTarget::Branch(full) => self.set_head_symbolic(&full, &reflog)?,
            SwitchTarget::Detach(id) => self.set_head_detached(id, &reflog)?,
        }
        Ok(())
    }

    /// `git reset [--soft|--mixed|--hard] <commit>`.
    pub fn reset(&self, commit: ObjectId, mode: ResetMode) -> Result<()> {
        let old = self.head_commit_id()?;
        let target_tree = self.commit_tree_id(commit)?;
        let target = self.flat_from_tree(target_tree)?;
        if mode == ResetMode::Hard {
            let index = self.flat_index()?;
            let snapshot = self.snapshot(&SnapshotOptions::default())?;
            let relevant: BTreeSet<&BString> = index.keys().chain(target.keys()).collect();
            let current: FlatTree =
                snapshot.flat.iter().filter(|(p, _)| relevant.contains(p)).map(|(p, e)| (p.clone(), *e)).collect();
            let stats = self.apply_worktree(&current, &target)?;
            self.write_index_from_flat(&target, &stats)?;
        } else if mode == ResetMode::Mixed {
            self.write_index_from_flat(&target, &BTreeMap::new())?;
        }
        let reflog = format!("reset: moving to {}", crate::short(&commit));
        self.update_head_target(commit, old, &reflog)?;
        Ok(())
    }

    /// `git restore [--source=<tree>] <paths>` — make worktree files match the index (or a tree).
    /// Returns the paths that were rewritten or deleted.
    pub fn restore_worktree(&self, spec: &Pathspec, source_tree: Option<ObjectId>) -> Result<Vec<BString>> {
        let index = self.flat_index()?;
        let source = match source_tree {
            Some(t) => self.flat_from_tree(t)?,
            None => index.clone(),
        };
        let snapshot = self.snapshot(&SnapshotOptions::default())?;
        let tracked: BTreeSet<&BString> = index.keys().chain(source.keys()).collect();
        let pick = |t: &FlatTree| -> FlatTree {
            t.iter()
                .filter(|(p, _)| tracked.contains(p) && spec.matches(p.as_ref()))
                .map(|(p, e)| (p.clone(), *e))
                .collect()
        };
        let current = pick(&snapshot.flat);
        let target = pick(&source);
        if current.is_empty()
            && target.is_empty()
            && !spec.is_all()
            && let Some((arg, _)) = spec.args.first()
        {
            bail!("pathspec '{arg}' did not match any tracked file");
        }
        let changed: Vec<BString> = flat::diff(&current, &target).into_iter().map(|c| c.path).collect();
        let stats = self.apply_worktree(&current, &target)?;
        // Refresh stat info for files that now match the index exactly.
        if source_tree.is_none() && !stats.is_empty() {
            self.write_index_from_flat(&index, &stats)?;
        }
        Ok(changed)
    }

    /// `git restore --staged [--source=<tree>] <paths>` — make index entries match HEAD (or a tree).
    pub fn restore_staged(&self, spec: &Pathspec, source_tree: Option<ObjectId>) -> Result<Vec<BString>> {
        let index = self.flat_index()?;
        let source = match source_tree {
            Some(t) => self.flat_from_tree(t)?,
            None => self.flat_head()?,
        };
        let mut new_index = index.clone();
        let mut changed = Vec::new();
        for change in flat::diff(&index, &source) {
            if !spec.matches(change.path.as_ref()) {
                continue;
            }
            match change.new {
                Some(e) => new_index.insert(change.path.clone(), e),
                None => new_index.remove(&change.path),
            };
            changed.push(change.path);
        }
        if !changed.is_empty() {
            self.write_index_from_flat(&new_index, &BTreeMap::new())?;
        }
        Ok(changed)
    }
}

/// Where `switch` goes.
#[derive(Debug, Clone)]
pub enum SwitchTarget {
    /// A full branch name, e.g. `refs/heads/main`. May be unborn.
    Branch(String),
    /// Detach HEAD at a commit.
    Detach(ObjectId),
}

/// All proper ancestors of a slash-separated path, nearest first: `a/b/c` → `a/b`, `a`.
fn ancestors(path: &BString) -> impl Iterator<Item = BString> + '_ {
    let bytes = path.as_bytes();
    bytes.iter().enumerate().rev().filter(|(_, b)| **b == b'/').map(move |(i, _)| BString::from(&bytes[..i]))
}
