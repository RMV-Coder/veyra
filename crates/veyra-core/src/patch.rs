//! Partial staging (`add -p`): split a file's index→worktree diff into hunks, and build the
//! staged content from the hunks the user accepts.

use std::collections::BTreeMap;

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

/// The hunks of the diff `old → new`.
pub fn hunks(old: &str, new: &str) -> Vec<Hunk> {
    let diff = TextDiff::from_lines(old, new);
    diff.unified_diff().context_radius(CONTEXT).iter_hunks().map(|h| Hunk { text: h.to_string() }).collect()
}

/// `old` with only the `accepted` hunks of `old → new` applied.
pub fn apply(old: &str, new: &str, accepted: &[bool]) -> String {
    let diff = TextDiff::from_lines(old, new);
    let groups = diff.grouped_ops(CONTEXT);
    // Which change ops belong to an accepted hunk, keyed by their position in the diff.
    let mut take: BTreeMap<(usize, usize), bool> = BTreeMap::new();
    for (g, ops) in groups.iter().enumerate() {
        for op in ops {
            if !matches!(op, DiffOp::Equal { .. }) {
                take.insert((op.old_range().start, op.new_range().start), accepted.get(g).copied().unwrap_or(false));
            }
        }
    }
    let old_lines: Vec<&str> = diff.old_slices().to_vec();
    let new_lines: Vec<&str> = diff.new_slices().to_vec();
    let mut out = String::with_capacity(old.len().max(new.len()));
    for op in diff.ops() {
        let accept = take.get(&(op.old_range().start, op.new_range().start)).copied().unwrap_or(false);
        let (keep_old, use_new) = match op {
            DiffOp::Equal { .. } => (true, false),
            _ => (!accept, accept),
        };
        if keep_old {
            for l in &old_lines[op.old_range()] {
                out.push_str(l);
            }
        }
        if use_new {
            for l in &new_lines[op.new_range()] {
                out.push_str(l);
            }
        }
    }
    out
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
        assert_eq!(apply(OLD, NEW, &[true, false]), OLD.replacen("a\n", "A\n", 1));
        assert_eq!(apply(OLD, NEW, &[false, true]), OLD.replacen("l\n", "L\n", 1));
        assert_eq!(apply(OLD, NEW, &[true, true]), NEW);
        assert_eq!(apply(OLD, NEW, &[false, false]), OLD);
    }

    #[test]
    fn insertions_and_deletions() {
        let old = "1\n2\n3\n";
        let new = "1\n3\n4\n";
        let h = hunks(old, new);
        assert_eq!(h.len(), 1);
        assert_eq!(apply(old, new, &[true]), new);
        assert_eq!(apply(old, new, &[false]), old);
    }
}
