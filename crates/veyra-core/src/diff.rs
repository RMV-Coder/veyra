//! Text diffs in Git's unified format.

use anyhow::Result;
use gix::ObjectId;
use gix::bstr::{BString, ByteSlice};
use gix::objs::tree::EntryKind;

use crate::Repo;
use crate::flat::{FlatEntry, PathChange};

/// Where the new side of a diff comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewSide {
    /// Use the blob in the change's `new` entry.
    Objects,
    /// Read the file from the worktree (through the clean filter).
    Worktree,
}

/// One file's diff.
#[derive(Debug, Clone)]
pub struct FileDiff {
    pub path: BString,
    pub old: Option<FlatEntry>,
    pub new: Option<FlatEntry>,
    pub binary: bool,
    /// Unified hunks (`@@ … @@` headers plus ` `, `+`, `-` lines). Empty for binary or mode-only changes.
    pub hunks: String,
    pub added: usize,
    pub removed: usize,
}

/// Heuristic used by Git: a NUL byte in the first 8000 bytes means binary.
pub fn is_binary(data: &[u8]) -> bool {
    data.iter().take(8000).any(|b| *b == 0)
}

/// Compute unified hunks between two texts.
pub fn unified(old: &[u8], new: &[u8], context: usize) -> (String, usize, usize) {
    let old = old.to_str_lossy();
    let new = new.to_str_lossy();
    let diff =
        similar::TextDiff::configure().algorithm(similar::Algorithm::Myers).diff_lines(old.as_ref(), new.as_ref());
    let mut added = 0;
    let mut removed = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    let mut out = String::new();
    for hunk in diff.unified_diff().context_radius(context).missing_newline_hint(true).iter_hunks() {
        out.push_str(&hunk.to_string());
    }
    (out, added, removed)
}

/// The git mode string, e.g. `100644`.
pub fn mode_str(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Tree => "040000",
        EntryKind::Blob => "100644",
        EntryKind::BlobExecutable => "100755",
        EntryKind::Link => "120000",
        EntryKind::Commit => "160000",
    }
}

impl Repo {
    fn side_bytes(&self, entry: Option<FlatEntry>, path: &BString, from_worktree: bool) -> Result<Vec<u8>> {
        if from_worktree {
            return Ok(self.read_worktree_as_git(path.as_ref())?.unwrap_or_default());
        }
        match entry {
            Some(e) if matches!(e.kind, EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link) => {
                self.read_blob(e.id)
            }
            Some(e) if e.kind == EntryKind::Commit => Ok(format!("Subproject commit {}\n", e.id).into_bytes()),
            _ => Ok(Vec::new()),
        }
    }

    /// Diff each change. For `NewSide::Worktree`, the new content is read from disk.
    pub fn diff_changes(&self, changes: &[PathChange], new_side: NewSide, context: usize) -> Result<Vec<FileDiff>> {
        let mut out = Vec::with_capacity(changes.len());
        for change in changes {
            let old = self.side_bytes(change.old, &change.path, false)?;
            let new =
                self.side_bytes(change.new, &change.path, new_side == NewSide::Worktree && change.new.is_some())?;
            let binary = is_binary(&old) || is_binary(&new);
            let (hunks, added, removed) = if binary { (String::new(), 0, 0) } else { unified(&old, &new, context) };
            out.push(FileDiff {
                path: change.path.clone(),
                old: change.old,
                new: change.new,
                binary,
                hunks,
                added,
                removed,
            });
        }
        Ok(out)
    }

    /// Hash bytes as a blob id without writing them (for `index a..b` lines of worktree diffs).
    pub fn hash_blob(&self, data: &[u8]) -> Result<ObjectId> {
        Ok(gix::objs::compute_hash(self.object_hash(), gix::objs::Kind::Blob, data)?)
    }
}

/// Render a file diff in `git diff` format (without colour).
pub fn render(diff: &FileDiff, worktree_new_id: Option<ObjectId>) -> String {
    let mut out = String::new();
    let p = diff.path.to_str_lossy();
    out.push_str(&format!("diff --git a/{p} b/{p}\n"));
    let null = "0000000";
    let old_id = diff.old.map(|e| crate::short(&e.id)).unwrap_or_else(|| null.into());
    let new_id = worktree_new_id.or(diff.new.map(|e| e.id)).map(|id| crate::short(&id)).unwrap_or_else(|| null.into());
    match (diff.old, diff.new) {
        (None, Some(n)) => {
            out.push_str(&format!("new file mode {}\n", mode_str(n.kind)));
            out.push_str(&format!("index {old_id}..{new_id}\n"));
        }
        (Some(o), None) => {
            out.push_str(&format!("deleted file mode {}\n", mode_str(o.kind)));
            out.push_str(&format!("index {old_id}..{new_id}\n"));
        }
        (Some(o), Some(n)) if o.kind != n.kind => {
            out.push_str(&format!("old mode {}\nnew mode {}\n", mode_str(o.kind), mode_str(n.kind)));
            out.push_str(&format!("index {old_id}..{new_id}\n"));
        }
        (Some(o), Some(_)) => out.push_str(&format!("index {old_id}..{new_id} {}\n", mode_str(o.kind))),
        (None, None) => {}
    }
    if diff.binary {
        out.push_str(&format!(
            "Binary files {} and {} differ\n",
            if diff.old.is_some() { format!("a/{p}") } else { "/dev/null".into() },
            if diff.new.is_some() { format!("b/{p}") } else { "/dev/null".into() }
        ));
        return out;
    }
    if diff.hunks.is_empty() {
        return out;
    }
    out.push_str(&format!("--- {}\n", if diff.old.is_some() { format!("a/{p}") } else { "/dev/null".into() }));
    out.push_str(&format!("+++ {}\n", if diff.new.is_some() { format!("b/{p}") } else { "/dev/null".into() }));
    out.push_str(&diff.hunks);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_counts_lines() {
        let (hunks, added, removed) = unified(b"a\nb\nc\n", b"a\nB\nc\nd\n", 3);
        assert_eq!((added, removed), (2, 1));
        assert!(hunks.starts_with("@@ -1,3 +1,4 @@"));
        assert!(hunks.contains("-b\n+B\n"));
    }

    #[test]
    fn binary_detection() {
        assert!(is_binary(b"abc\0def"));
        assert!(!is_binary(b"plain text"));
    }
}
