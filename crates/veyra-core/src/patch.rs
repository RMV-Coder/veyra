//! Partial staging (`add -p`): split a file's index→worktree diff into hunks, and build the
//! staged content from the hunks the user accepts.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result};
use gix::bstr::{BString, ByteSlice};
use similar::{DiffOp, TextDiff};

use crate::Repo;
use crate::flat::FlatEntry;
use crate::worktree::Pathspec;

const CONTEXT: usize = 3;

/// One reviewable hunk.
#[derive(Debug, Clone)]
pub struct Hunk {
    /// Unified-diff text of the hunk, starting with its `@@ … @@` header.
    pub text: String,
    /// The changes (diff ops, keyed by their `(old, new)` start lines) this hunk contains.
    pub changes: Vec<(usize, usize)>,
}

impl Hunk {
    /// Can this hunk be split into smaller ones?
    pub fn can_split(&self) -> bool {
        self.changes.len() > 1
    }
}

/// What can be staged for one path.
#[derive(Debug, Clone)]
pub enum FilePatch {
    /// A text file with individually stageable hunks.
    Text { path: BString, index: String, worktree: String, hunks: Vec<Hunk> },
    /// A deleted file: stage the deletion or not.
    Deletion { path: BString },
    /// Binary or non-UTF-8 content: stage the whole file or not.
    Whole { path: BString },
}

impl FilePatch {
    pub fn path(&self) -> &BString {
        match self {
            FilePatch::Text { path, .. } | FilePatch::Deletion { path } | FilePatch::Whole { path } => path,
        }
    }
}

fn key(op: &DiffOp) -> (usize, usize) {
    (op.old_range().start, op.new_range().start)
}

/// Render `ops` (a contiguous run of the diff) as one unified hunk.
fn render(old_lines: &[&str], new_lines: &[&str], ops: &[DiffOp]) -> String {
    let (Some(first), Some(last)) = (ops.first(), ops.last()) else { return String::new() };
    let old_start = first.old_range().start;
    let new_start = first.new_range().start;
    let old_len = last.old_range().end - old_start;
    let new_len = last.new_range().end - new_start;
    let pos = |start: usize, len: usize| if len == 0 { start } else { start + 1 };
    let mut out = format!("@@ -{},{old_len} +{},{new_len} @@\n", pos(old_start, old_len), pos(new_start, new_len));
    let mut line = |prefix: char, text: &str| {
        out.push(prefix);
        out.push_str(text);
        if !text.ends_with('\n') {
            out.push_str("\n\\ No newline at end of file\n");
        }
    };
    for op in ops {
        match op {
            DiffOp::Equal { .. } => old_lines[op.old_range()].iter().for_each(|l| line(' ', l)),
            _ => {
                old_lines[op.old_range()].iter().for_each(|l| line('-', l));
                new_lines[op.new_range()].iter().for_each(|l| line('+', l));
            }
        }
    }
    out
}

/// The hunks of the diff `old → new`.
pub fn hunks(old: &str, new: &str) -> Vec<Hunk> {
    let diff = TextDiff::from_lines(old, new);
    let (old_lines, new_lines) = (diff.old_slices(), diff.new_slices());
    diff.grouped_ops(CONTEXT)
        .iter()
        .map(|ops| Hunk {
            text: render(old_lines, new_lines, ops),
            changes: ops.iter().filter(|o| !matches!(o, DiffOp::Equal { .. })).map(key).collect(),
        })
        .collect()
}

/// Split `hunk` into one hunk per change, each with up to three lines of context.
pub fn split(old: &str, new: &str, hunk: &Hunk) -> Vec<Hunk> {
    if !hunk.can_split() {
        return vec![hunk.clone()];
    }
    let diff = TextDiff::from_lines(old, new);
    let (old_lines, new_lines) = (diff.old_slices(), diff.new_slices());
    let ops = diff.ops();
    let mut out = Vec::new();
    for change in &hunk.changes {
        let Some(i) = ops.iter().position(|o| key(o) == *change) else { continue };
        let mut run = Vec::new();
        // Up to CONTEXT lines of the preceding equal run.
        if i > 0
            && let DiffOp::Equal { old_index, new_index, len } = ops[i - 1]
        {
            let take = len.min(CONTEXT);
            run.push(DiffOp::Equal { old_index: old_index + len - take, new_index: new_index + len - take, len: take });
        }
        run.push(ops[i]);
        if let Some(DiffOp::Equal { old_index, new_index, len }) = ops.get(i + 1).copied() {
            run.push(DiffOp::Equal { old_index, new_index, len: len.min(CONTEXT) });
        }
        out.push(Hunk { text: render(old_lines, new_lines, &run), changes: vec![*change] });
    }
    out
}

/// `old` with only the `accepted` changes of `old → new` applied.
pub fn apply(old: &str, new: &str, accepted: &HashSet<(usize, usize)>) -> String {
    let diff = TextDiff::from_lines(old, new);
    let (old_lines, new_lines) = (diff.old_slices(), diff.new_slices());
    let mut out = String::with_capacity(old.len().max(new.len()));
    for op in diff.ops() {
        let use_new = !matches!(op, DiffOp::Equal { .. }) && accepted.contains(&key(op));
        if use_new {
            new_lines[op.new_range()].iter().for_each(|l| out.push_str(l));
        } else {
            old_lines[op.old_range()].iter().for_each(|l| out.push_str(l));
        }
    }
    out
}

/// The changes of all `hunks` whose flag in `accepted` is set (convenience for whole-hunk choices).
pub fn accepted_changes(hunks: &[Hunk], accepted: &[bool]) -> HashSet<(usize, usize)> {
    hunks.iter().zip(accepted).filter(|(_, a)| **a).flat_map(|(h, _)| h.changes.iter().copied()).collect()
}

impl Repo {
    /// Every tracked change under `spec` that `add -p` can offer.
    pub fn patch_candidates(&self, spec: &Pathspec) -> Result<Vec<FilePatch>> {
        let changes = self.worktree_changes()?;
        let index = self.flat_index()?;
        let mut out = Vec::new();
        for path in changes.modified.iter().filter(|p| spec.matches(p.as_ref())) {
            let Some(entry) = index.get(path) else { continue };
            let old = self.read_blob(entry.id)?;
            let new = self.read_worktree_as_git(path.as_ref())?.unwrap_or_default();
            match (std::str::from_utf8(&old), std::str::from_utf8(&new)) {
                (Ok(o), Ok(n)) if !crate::diff::is_binary(&old) && !crate::diff::is_binary(&new) => {
                    let h = hunks(o, n);
                    if h.is_empty() {
                        out.push(FilePatch::Whole { path: path.clone() }); // mode-only change
                    } else {
                        out.push(FilePatch::Text {
                            path: path.clone(),
                            index: o.to_owned(),
                            worktree: n.to_owned(),
                            hunks: h,
                        });
                    }
                }
                _ => out.push(FilePatch::Whole { path: path.clone() }),
            }
        }
        for path in changes.deleted.iter().filter(|p| spec.matches(p.as_ref())) {
            out.push(FilePatch::Deletion { path: path.clone() });
        }
        out.sort_by(|a, b| a.path().cmp(b.path()));
        Ok(out)
    }

    /// Stage `content` as the new index version of `path` (keeping its mode).
    pub fn stage_content(&self, path: &BString, content: &[u8]) -> Result<()> {
        let mut flat = self.flat_index()?;
        let entry = *flat.get(path).with_context(|| format!("{path} is not tracked"))?;
        let id = self.write_blob(content)?;
        flat.insert(path.clone(), FlatEntry { kind: entry.kind, id });
        self.write_index_from_flat(&flat, &BTreeMap::new())
    }

    /// Stage the deletion of `path`.
    pub fn stage_deletion(&self, path: &BString) -> Result<()> {
        let mut flat = self.flat_index()?;
        flat.remove(path);
        self.write_index_from_flat(&flat, &BTreeMap::new())
    }

    /// Stage the whole worktree version of `path` (binary files, mode changes).
    pub fn stage_whole(&self, path: &BString) -> Result<()> {
        let workdir = self.workdir()?.to_owned();
        let spec = Pathspec::new(self, &workdir, &[path.to_str_lossy().into_owned()])?;
        self.add(&spec, false)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n";
    const NEW: &str = "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nL\n";

    #[test]
    fn distant_changes_become_separate_hunks() {
        let h = hunks(OLD, NEW);
        assert_eq!(h.len(), 2);
        assert!(h[0].text.starts_with("@@ -1,4 +1,4 @@"));
        assert!(h[1].text.contains("+L"));
    }

    #[test]
    fn applying_a_subset_of_hunks() {
        let h = hunks(OLD, NEW);
        let pick = |a: &[bool]| apply(OLD, NEW, &accepted_changes(&h, a));
        assert_eq!(pick(&[true, false]), OLD.replacen("a\n", "A\n", 1));
        assert_eq!(pick(&[false, true]), OLD.replacen("l\n", "L\n", 1));
        assert_eq!(pick(&[true, true]), NEW);
        assert_eq!(pick(&[false, false]), OLD);
    }

    #[test]
    fn a_hunk_with_two_changes_splits_into_two() {
        let old = "1\n2\n3\n4\n5\n";
        let new = "1\nTWO\n3\nFOUR\n5\n";
        let h = hunks(old, new);
        assert_eq!(h.len(), 1, "close changes share one hunk");
        assert!(h[0].can_split());
        let parts = split(old, new, &h[0]);
        assert_eq!(parts.len(), 2);
        assert!(parts[0].text.contains("+TWO") && !parts[0].text.contains("+FOUR"));
        let only_second: HashSet<_> = parts[1].changes.iter().copied().collect();
        assert_eq!(apply(old, new, &only_second), "1\n2\n3\nFOUR\n5\n");
    }

    #[test]
    fn insertions_and_deletions() {
        let old = "1\n2\n3\n";
        let new = "1\n3\n4\n";
        let h = hunks(old, new);
        assert_eq!(h.len(), 1);
        assert_eq!(apply(old, new, &accepted_changes(&h, &[true])), new);
        assert_eq!(apply(old, new, &accepted_changes(&h, &[false])), old);
    }
}
