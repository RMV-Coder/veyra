//! Moving the repository between views: undo, redo, op restore and checkpoint restore.
//!
//! Everything is built on one primitive, [`veyra_core::flat::three_way`]: apply the
//! difference `from → to` to the *current* state, touching only keys where `from` and `to`
//! differ, and only where the current state still equals `from`. Anything changed again
//! since is a conflict and is left alone unless `force` is given. Because every apply runs
//! inside a transaction whose `before` view includes a fresh snapshot, forced applies can
//! themselves be undone.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use veyra_core::gix::bstr::BString;
use veyra_core::{FlatTree, ObjectId, Repo, flat};

use crate::log::{OpLog, Operation, Transaction};
use crate::view::{HeadView, View, parse_id};

/// What an apply changed (or refused to change).
#[derive(Debug, Default, Clone)]
pub struct ApplyReport {
    pub refs_changed: Vec<String>,
    pub head_changed: bool,
    pub index_paths: usize,
    pub worktree_paths: Vec<String>,
    pub conflicts: Vec<String>,
}

impl ApplyReport {
    pub fn is_empty(&self) -> bool {
        self.refs_changed.is_empty() && !self.head_changed && self.index_paths == 0 && self.worktree_paths.is_empty()
    }
}

/// Error returned when an apply would clobber changes made after the target operation.
#[derive(Debug)]
pub struct ApplyConflict(pub Vec<String>);

impl std::fmt::Display for ApplyConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "these changed again after that operation, so Veyra left everything untouched:")?;
        for c in self.0.iter().take(20) {
            writeln!(f, "    {c}")?;
        }
        if self.0.len() > 20 {
            writeln!(f, "    … and {} more", self.0.len() - 20)?;
        }
        write!(
            f,
            "  hint: pass --force to apply anyway (the current state is checkpointed first, so this is undoable too)"
        )
    }
}

impl std::error::Error for ApplyConflict {}

fn flat_of(repo: &Repo, tree_hex: &str) -> Result<FlatTree> {
    repo.flat_from_tree(parse_id(tree_hex)?)
}

/// Apply `from → to` onto the current state captured in `tx.before`.
fn apply_views(
    repo: &Repo,
    from: &View,
    to: &View,
    tx: &Transaction<'_, '_>,
    force: bool,
    dry_run: bool,
) -> Result<ApplyReport> {
    let current = &tx.before.view;
    let mut report = ApplyReport::default();

    // --- refs -------------------------------------------------------------------------
    let mut ref_edits: Vec<(String, Option<ObjectId>)> = Vec::new();
    let names: BTreeSet<&String> = from.refs.keys().chain(to.refs.keys()).collect();
    for name in names {
        let (f, t, c) = (from.refs.get(name), to.refs.get(name), current.refs.get(name));
        if f == t || c == t {
            continue;
        }
        if c != f && !force {
            report.conflicts.push(format!(
                "ref {} (now {})",
                veyra_core::refs::shorten(name),
                c.map(|h| &h[..7]).unwrap_or("deleted")
            ));
            continue;
        }
        ref_edits.push((name.clone(), t.map(|h| parse_id(h)).transpose()?));
        report.refs_changed.push(name.clone());
    }

    // --- HEAD -------------------------------------------------------------------------
    let set_head = from.head != to.head && current.head != to.head;
    if set_head {
        if current.head != from.head && !force {
            report.conflicts.push(format!("HEAD (now at {})", current.head_label()));
        } else {
            report.head_changed = true;
        }
    }

    // --- index ------------------------------------------------------------------------
    let index_from = flat_of(repo, &from.index_tree)?;
    let index_to = flat_of(repo, &to.index_tree)?;
    let index_cur = flat_of(repo, &current.index_tree)?;
    let index_change = flat::three_way(&index_from, &index_to, &index_cur, force);
    for p in &index_change.conflicts {
        report.conflicts.push(format!("staged {p}"));
    }
    report.index_paths = index_change.applied.len();
    let raw_index = (to.index_blob.is_some() && index_cur == index_from).then(|| to.index_blob.clone()).flatten();

    // --- worktree ---------------------------------------------------------------------
    let current_wt = tx.before.worktree_flat.clone().unwrap_or_default();
    let mut worktree_writes: Vec<(BString, Option<veyra_core::FlatEntry>)> = Vec::new();
    match (&from.worktree, &to.worktree) {
        (Some(f), Some(t)) => {
            let change = flat::three_way(&flat_of(repo, f)?, &flat_of(repo, t)?, &current_wt, force);
            for p in &change.conflicts {
                report.conflicts.push(format!("file {p}"));
            }
            worktree_writes = change.applied.into_iter().map(|c| (c.path, c.new)).collect();
        }
        _ => {
            // No snapshots (an `external` operation). If HEAD switched to another branch or
            // commit (`git checkout`/`git switch`), git rewrote files to match the index; follow
            // the index back for files the user hasn't touched since. A branch merely moving
            // (`git commit`, `git reset`) is indistinguishable from the user's own edits, so
            // files are left alone — the conservative choice never discards work.
            if from.head != to.head {
                let change = flat::three_way(&index_from, &index_to, &current_wt, false);
                worktree_writes = change.applied.into_iter().map(|c| (c.path, c.new)).collect();
            }
        }
    }
    report.worktree_paths = worktree_writes.iter().map(|(p, _)| p.to_string()).collect();

    if !report.conflicts.is_empty() {
        return Err(ApplyConflict(report.conflicts).into());
    }
    if dry_run {
        return Ok(report);
    }

    // --- perform, worktree first so index stats are fresh ------------------------------
    let stats: BTreeMap<BString, _> = repo.write_worktree(&worktree_writes)?;
    if let Some(blob) = raw_index {
        let bytes = repo.read_blob(parse_id(&blob)?)?;
        let path = repo.index_path();
        let tmp = path.with_extension("veyra-tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path).context("could not restore the index")?;
    } else if !index_change.applied.is_empty() || !stats.is_empty() {
        repo.write_index_from_flat(&index_change.result, &stats)?;
    }
    if !ref_edits.is_empty() {
        repo.set_refs(&ref_edits, "veyra: undo/restore")?;
    }
    if report.head_changed {
        match &to.head {
            HeadView::Symbolic(name) => repo.set_head_symbolic(name, "veyra: undo/restore")?,
            HeadView::Detached(hex) => repo.set_head_detached(parse_id(hex)?, "veyra: undo/restore")?,
        }
    }
    // Merge bookkeeping follows the view.
    if from.merge_head != to.merge_head {
        match &to.merge_head {
            Some(hex) => std::fs::write(repo.git_dir().join("MERGE_HEAD"), format!("{hex}\n"))?,
            None => repo.clear_merge_state()?,
        }
    }
    Ok(report)
}

/// Which operation `veyra undo` would undo, and which `veyra redo` would redo.
pub fn targets(ops: &[Operation]) -> (Option<&Operation>, Option<&Operation>) {
    let mut undone: BTreeSet<&str> = BTreeSet::new();
    let mut redo_stack: Vec<&str> = Vec::new();
    for op in ops {
        match (op.kind.as_str(), op.target.as_deref()) {
            ("undo", Some(t)) => {
                undone.insert(t);
                redo_stack.push(t);
            }
            ("redo", Some(t)) => {
                undone.remove(t);
                if redo_stack.last() == Some(&t) {
                    redo_stack.pop();
                }
            }
            _ if op.is_undoable() => redo_stack.clear(),
            _ => {}
        }
    }
    let undo = ops.iter().rev().find(|op| op.is_undoable() && !undone.contains(op.id.as_str()));
    let redo = redo_stack.last().and_then(|id| ops.iter().find(|op| op.id == *id));
    (undo, redo)
}

impl OpLog<'_> {
    /// Undo the most recent undoable operation (or `which`, if given).
    pub fn undo(
        &self,
        which: Option<&str>,
        force: bool,
        dry_run: bool,
        command: &str,
    ) -> Result<(Operation, Option<Operation>, ApplyReport)> {
        let mut tx = self.begin("undo", command, true)?;
        let ops = self.operations()?; // includes any `external` op just recorded
        let target = match which {
            Some(q) => self.find(q)?,
            None => match targets(&ops).0 {
                Some(op) => op.clone(),
                None => bail!("nothing to undo"),
            },
        };
        if matches!(target.kind.as_str(), "undo" | "redo") {
            bail!(
                "operation {} is itself an {}; use `veyra redo` / `veyra undo` instead",
                target.short_id(),
                target.kind
            );
        }
        if !force {
            let pushed = published_commits(self.repo, &target)?;
            if !pushed.is_empty() {
                bail!(
                    "{} already pushed ({}); undoing it here would not remove it from the remote, and the next push would be rejected\n  \
                     hint: `veyra revert <commit>` adds a commit that reverses it — safe on shared branches\n  \
                     hint: `veyra undo --force` undoes it locally anyway (you would then need `veyra push --force-with-lease`)",
                    if pushed.len() == 1 { "that commit is" } else { "those commits are" },
                    pushed.join(", ")
                );
            }
        }
        let report = apply_views(self.repo, &target.after, &target.before, &tx, force, dry_run)?;
        if dry_run {
            return Ok((target, None, report));
        }
        tx.set_target(&target.id);
        let op = tx.commit(&format!("undo {}: {}", target.short_id(), target.summary))?;
        Ok((target, Some(op), report))
    }

    /// Redo the most recently undone operation.
    pub fn redo(&self, force: bool, command: &str) -> Result<(Operation, Operation, ApplyReport)> {
        let mut tx = self.begin("redo", command, true)?;
        let ops = self.operations()?;
        let Some(target) = targets(&ops).1.cloned() else {
            bail!("nothing to redo");
        };
        let report = apply_views(self.repo, &target.before, &target.after, &tx, force, false)?;
        tx.set_target(&target.id);
        let op = tx.commit(&format!("redo {}: {}", target.short_id(), target.summary))?;
        Ok((target, op, report))
    }

    /// Make the whole repository — refs, HEAD, index and worktree — match the state right
    /// after operation `id` (or before it, with `before`).
    pub fn restore(&self, id: &str, before: bool, command: &str) -> Result<(Operation, Operation, ApplyReport)> {
        let target = self.find(id)?;
        let tx = self.begin("restore", command, true)?;
        let desired = if before { target.before.clone() } else { target.after.clone() };
        let mut desired = desired;
        if desired.worktree.is_none() {
            // Operations without snapshots (external): keep the current worktree.
            desired.worktree = tx.before.view.worktree.clone();
        }
        let current = tx.before.view.clone();
        let report = apply_views(self.repo, &current, &desired, &tx, true, false)?;
        let label = if before { "before" } else { "after" };
        let op = tx.commit(&format!("restore to {label} {}: {}", target.short_id(), target.summary))?;
        Ok((target, op, report))
    }

    /// Restore worktree files from a checkpoint (all files, or those under `paths`).
    /// The index and HEAD are not touched.
    pub fn restore_checkpoint(
        &self,
        id: &str,
        paths: &[BString],
        before: bool,
        command: &str,
    ) -> Result<(Operation, Operation, Vec<String>)> {
        let target = self.find(id)?;
        let tree = if before {
            target.before.worktree.clone()
        } else {
            target.after.worktree.clone().or(target.before.worktree.clone())
        };
        let Some(tree) = tree else {
            bail!("operation {} has no worktree snapshot", target.short_id());
        };
        let tx = self.begin("checkpoint-restore", command, true)?;
        let repo = self.repo;
        let wanted = flat_of(repo, &tree)?;
        let current = tx.before.worktree_flat.clone().unwrap_or_default();
        let selected =
            |p: &BString| paths.is_empty() || paths.iter().any(|q| flat::path_is_within(p.as_ref(), q.as_ref()));
        let pick = |t: &FlatTree| -> FlatTree {
            t.iter().filter(|(p, _)| selected(p)).map(|(p, e)| (p.clone(), *e)).collect()
        };
        let (cur, want) = (pick(&current), pick(&wanted));
        if want.is_empty() && !paths.is_empty() {
            bail!(
                "checkpoint {} has no files under {}",
                target.short_id(),
                paths.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")
            );
        }
        let changed: Vec<String> = flat::diff(&cur, &want).into_iter().map(|c| c.path.to_string()).collect();
        repo.apply_worktree(&cur, &want)?;
        let op = tx.commit(&format!("restore {} file(s) from checkpoint {}", changed.len(), target.short_id()))?;
        Ok((target, op, changed))
    }

    /// Abort an in-progress merge by undoing the operation that started it.
    pub fn abort_merge(&self, command: &str) -> Result<Operation> {
        let repo = self.repo;
        let Some(merge_head) = repo.merge_head()? else {
            bail!("there is no merge to abort");
        };
        let ops = self.operations()?;
        let started = ops
            .iter()
            .rev()
            .find(|op| {
                op.after.merge_head.as_deref() == Some(merge_head.to_string().as_str())
                    && op.before.merge_head.is_none()
            })
            .cloned();
        match started {
            Some(op) => {
                let mut tx = self.begin("merge-abort", command, true)?;
                apply_views(repo, &op.after, &op.before, &tx, true, false)?;
                tx.set_target(&op.id);
                tx.commit(&format!("abort merge started by {}", op.short_id()))
            }
            None => {
                // The merge was started by plain git: reset index and worktree to HEAD.
                let tx = self.begin("merge-abort", command, true)?;
                let head = repo.head_commit_id()?.context("HEAD is unborn")?;
                repo.reset(head, veyra_core::checkout::ResetMode::Hard)?;
                repo.clear_merge_state()?;
                tx.commit("abort merge (started outside Veyra)")
            }
        }
    }
}

/// Commits an operation added to local branches that are already reachable from a
/// remote-tracking ref (i.e. pushed), as `short-id on origin/main`.
fn published_commits(repo: &Repo, op: &Operation) -> Result<Vec<String>> {
    let remote_tips: Vec<(String, ObjectId)> = repo
        .all_refs()?
        .into_iter()
        .filter(|(name, _)| name.starts_with("refs/remotes/") && !name.ends_with("/HEAD"))
        .filter_map(|(name, id)| repo.peel_to_commit(id).ok().map(|c| (name, c)))
        .collect();
    if remote_tips.is_empty() {
        return Ok(Vec::new());
    }
    // Commits that were already on a remote when the operation ran came *from* the remote
    // (e.g. a fast-forward pull); undoing those locally is harmless.
    let tips_before: Vec<ObjectId> = op
        .before
        .refs
        .iter()
        .filter(|(name, _)| name.starts_with("refs/remotes/"))
        .filter_map(|(_, hex)| parse_id(hex).ok().and_then(|id| repo.peel_to_commit(id).ok()))
        .collect();
    let mut out = Vec::new();
    for (name, after) in &op.after.refs {
        if !name.starts_with("refs/heads/") || op.before.refs.get(name) == Some(after) {
            continue;
        }
        let Ok(commit) = parse_id(after).and_then(|id| repo.peel_to_commit(id)) else { continue };
        if tips_before.iter().any(|tip| repo.is_ancestor(commit, *tip).unwrap_or(false)) {
            continue;
        }
        if let Some((remote, _)) = remote_tips.iter().find(|(_, tip)| repo.is_ancestor(commit, *tip).unwrap_or(false)) {
            out.push(format!("{} on {}", veyra_core::short(&commit), veyra_core::refs::shorten(remote)));
        }
    }
    Ok(out)
}
