//! Flattened trees: one uniform representation for HEAD, the index, snapshots and trees.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use gix::ObjectId;
use gix::bstr::{BStr, BString, ByteSlice};
use gix::objs::tree::EntryKind;

use crate::Repo;

/// A single non-tree entry: a blob, executable blob, symlink or submodule commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FlatEntry {
    pub kind: EntryKind,
    pub id: ObjectId,
}

impl FlatEntry {
    pub fn new(kind: EntryKind, id: ObjectId) -> Self {
        FlatEntry { kind, id }
    }

    /// The index mode corresponding to this entry kind.
    pub fn index_mode(&self) -> gix::index::entry::Mode {
        use gix::index::entry::Mode;
        match self.kind {
            EntryKind::Blob => Mode::FILE,
            EntryKind::BlobExecutable => Mode::FILE_EXECUTABLE,
            EntryKind::Link => Mode::SYMLINK,
            EntryKind::Commit => Mode::COMMIT,
            EntryKind::Tree => Mode::DIR,
        }
    }

    /// Convert an index mode into an entry kind, if it denotes a trackable entry.
    pub fn kind_from_index_mode(mode: gix::index::entry::Mode) -> Option<EntryKind> {
        mode.to_tree_entry_mode().map(|m| m.kind())
    }
}

/// Repository-relative, slash-separated path → entry. Ordered byte-wise like the index.
pub type FlatTree = BTreeMap<BString, FlatEntry>;

/// A change of a single path between two flat trees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathChange {
    pub path: BString,
    pub old: Option<FlatEntry>,
    pub new: Option<FlatEntry>,
}

impl PathChange {
    pub fn status_letter(&self) -> char {
        match (&self.old, &self.new) {
            (None, Some(_)) => 'A',
            (Some(_), None) => 'D',
            (Some(o), Some(n)) if o.kind != n.kind && o.id == n.id => 'T',
            _ => 'M',
        }
    }
}

/// All paths whose entries differ between `old` and `new`, in path order.
pub fn diff(old: &FlatTree, new: &FlatTree) -> Vec<PathChange> {
    let mut out = Vec::new();
    let mut a = old.iter().peekable();
    let mut b = new.iter().peekable();
    loop {
        match (a.peek(), b.peek()) {
            (None, None) => break,
            (Some((pa, ea)), None) => {
                out.push(PathChange { path: (*pa).clone(), old: Some(**ea), new: None });
                a.next();
            }
            (None, Some((pb, eb))) => {
                out.push(PathChange { path: (*pb).clone(), old: None, new: Some(**eb) });
                b.next();
            }
            (Some((pa, ea)), Some((pb, eb))) => match pa.cmp(pb) {
                std::cmp::Ordering::Less => {
                    out.push(PathChange { path: (*pa).clone(), old: Some(**ea), new: None });
                    a.next();
                }
                std::cmp::Ordering::Greater => {
                    out.push(PathChange { path: (*pb).clone(), old: None, new: Some(**eb) });
                    b.next();
                }
                std::cmp::Ordering::Equal => {
                    if ea != eb {
                        out.push(PathChange { path: (*pa).clone(), old: Some(**ea), new: Some(**eb) });
                    }
                    a.next();
                    b.next();
                }
            },
        }
    }
    out
}

/// The result of a path-wise three-way restore (see DESIGN.md §4.4).
#[derive(Debug, Default)]
pub struct ThreeWay {
    /// The new value of `current` after applying the change.
    pub result: FlatTree,
    /// Paths that changed again after `from → to` and therefore were not touched.
    pub conflicts: Vec<BString>,
    /// Paths that were actually changed in `current`.
    pub applied: Vec<PathChange>,
}

/// Move `current` from `from` towards `to`, but only on paths where `from` and `to` differ,
/// and only where `current` still equals `from` (otherwise: conflict).
///
/// Undo is `three_way(op.after, op.before, current)`; redo is `three_way(op.before, op.after, current)`.
/// With `force`, conflicting paths are overwritten with `to` as well.
pub fn three_way(from: &FlatTree, to: &FlatTree, current: &FlatTree, force: bool) -> ThreeWay {
    let mut result = current.clone();
    let mut conflicts = Vec::new();
    let mut applied = Vec::new();
    for change in diff(from, to) {
        let cur = current.get(&change.path).copied();
        if cur == change.new {
            continue; // already in the desired state
        }
        if cur != change.old && !force {
            conflicts.push(change.path.clone());
            continue;
        }
        match change.new {
            Some(e) => result.insert(change.path.clone(), e),
            None => result.remove(&change.path),
        };
        applied.push(PathChange { path: change.path, old: cur, new: change.new });
    }
    ThreeWay { result, conflicts, applied }
}

/// Return `true` if `path` equals `prefix` or lies below the directory `prefix`.
pub fn path_is_within(path: &BStr, prefix: &BStr) -> bool {
    if prefix.is_empty() {
        return true;
    }
    path == prefix || (path.starts_with(prefix.as_bytes()) && path.get(prefix.len()) == Some(&b'/'))
}

impl Repo {
    /// Flatten the tree `tree_id` (recursively) into a [`FlatTree`].
    pub fn flat_from_tree(&self, tree_id: ObjectId) -> Result<FlatTree> {
        if tree_id == ObjectId::empty_tree(self.git.object_hash()) {
            return Ok(FlatTree::new());
        }
        let index = self.git.index_from_tree(&tree_id).with_context(|| format!("could not read tree {tree_id}"))?;
        Ok(flat_from_index_state(&index))
    }

    /// Flatten the tree of `commit_id`.
    pub fn flat_from_commit(&self, commit_id: ObjectId) -> Result<FlatTree> {
        let tree = self.commit_tree_id(commit_id)?;
        self.flat_from_tree(tree)
    }

    /// Flatten HEAD's tree, or return an empty tree if HEAD is unborn.
    pub fn flat_head(&self) -> Result<FlatTree> {
        match self.head_commit_id()? {
            Some(id) => self.flat_from_commit(id),
            None => Ok(FlatTree::new()),
        }
    }

    /// Paths of the non-tree entries that differ between two trees, descending only into
    /// subtrees whose ids differ (much cheaper than flattening both).
    pub fn changed_paths(&self, old: Option<ObjectId>, new: Option<ObjectId>) -> Result<Vec<BString>> {
        let mut out = Vec::new();
        self.changed_paths_into(old, new, &BString::default(), &mut out)?;
        Ok(out)
    }

    fn changed_paths_into(
        &self,
        old: Option<ObjectId>,
        new: Option<ObjectId>,
        prefix: &BString,
        out: &mut Vec<BString>,
    ) -> Result<()> {
        if old == new {
            return Ok(());
        }
        let entries = |id: Option<ObjectId>| -> Result<BTreeMap<BString, (bool, ObjectId)>> {
            let Some(id) = id else { return Ok(BTreeMap::new()) };
            let tree = self.git.find_tree(id)?;
            let mut map = BTreeMap::new();
            for entry in tree.iter() {
                let entry = entry?;
                map.insert(entry.filename().to_owned(), (entry.mode().is_tree(), entry.oid().to_owned()));
            }
            Ok(map)
        };
        let (a, b) = (entries(old)?, entries(new)?);
        let names: std::collections::BTreeSet<&BString> = a.keys().chain(b.keys()).collect();
        for name in names {
            let path: BString = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}").into() };
            let (x, y) = (a.get(name), b.get(name));
            if x.map(|e| e.1) == y.map(|e| e.1) && x.map(|e| e.0) == y.map(|e| e.0) {
                continue;
            }
            let sub = |e: Option<&(bool, ObjectId)>| e.filter(|e| e.0).map(|e| e.1);
            let (tx, ty) = (sub(x), sub(y));
            if tx.is_some() || ty.is_some() {
                self.changed_paths_into(tx, ty, &path, out)?;
            }
            if x.is_some_and(|e| !e.0) || y.is_some_and(|e| !e.0) {
                out.push(path);
            }
        }
        Ok(())
    }

    /// Write `flat` as a tree object (and all needed subtrees) and return its id.
    pub fn write_flat(&self, flat: &FlatTree) -> Result<ObjectId> {
        if flat.is_empty() {
            let empty = ObjectId::empty_tree(self.git.object_hash());
            // The empty tree is virtual in gix; make sure it physically exists for git.
            self.git.write_object(gix::objs::Tree::empty())?;
            return Ok(empty);
        }
        let mut editor = self.git.edit_tree(ObjectId::empty_tree(self.git.object_hash()))?;
        for (path, entry) in flat {
            editor.upsert(path, entry.kind, entry.id).with_context(|| format!("invalid path in tree: {path}"))?;
        }
        Ok(editor.write()?.detach())
    }
}

/// Flatten the stage-0 entries of an index. Conflicted (stage 1–3) entries are skipped.
pub fn flat_from_index_state(index: &gix::index::State) -> FlatTree {
    let mut out = FlatTree::new();
    for entry in index.entries() {
        if entry.stage_raw() != 0 {
            continue;
        }
        if entry.flags.contains(gix::index::entry::Flags::INTENT_TO_ADD) {
            continue;
        }
        let Some(kind) = FlatEntry::kind_from_index_mode(entry.mode) else {
            continue;
        };
        if kind == EntryKind::Tree {
            continue; // sparse directory entries
        }
        out.insert(entry.path(index).to_owned(), FlatEntry { kind, id: entry.id });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> ObjectId {
        let mut bytes = [0u8; 20];
        bytes[19] = n;
        ObjectId::from_bytes_or_panic(&bytes)
    }

    fn tree(entries: &[(&str, u8)]) -> FlatTree {
        entries.iter().map(|(p, n)| (BString::from(*p), FlatEntry::new(EntryKind::Blob, id(*n)))).collect()
    }

    #[test]
    fn diff_reports_adds_deletes_and_modifications_in_order() {
        let old = tree(&[("a", 1), ("b", 2), ("d", 4)]);
        let new = tree(&[("a", 1), ("b", 3), ("c", 5)]);
        let changes = diff(&old, &new);
        let summary: Vec<_> = changes.iter().map(|c| (c.path.to_string(), c.status_letter())).collect();
        assert_eq!(summary, vec![("b".into(), 'M'), ("c".into(), 'A'), ("d".into(), 'D')]);
    }

    #[test]
    fn three_way_reverts_untouched_paths_and_reports_conflicts() {
        // An operation changed a: 1→2 and added b. Since then, the user changed b again.
        let before = tree(&[("a", 1)]);
        let after = tree(&[("a", 2), ("b", 3)]);
        let current = tree(&[("a", 2), ("b", 9), ("c", 7)]);

        let undo = three_way(&after, &before, &current, false);
        assert_eq!(undo.conflicts, vec![BString::from("b")]);
        assert_eq!(undo.result, tree(&[("a", 1), ("b", 9), ("c", 7)]));

        let forced = three_way(&after, &before, &current, true);
        assert!(forced.conflicts.is_empty());
        assert_eq!(forced.result, tree(&[("a", 1), ("c", 7)]));
    }

    #[test]
    fn three_way_is_a_no_op_when_already_in_target_state() {
        let before = tree(&[("a", 1)]);
        let after = tree(&[("a", 2)]);
        let undo = three_way(&after, &before, &before, false);
        assert!(undo.applied.is_empty());
        assert!(undo.conflicts.is_empty());
    }

    #[test]
    fn path_within_respects_component_boundaries() {
        assert!(path_is_within("src/a.rs".into(), "src".into()));
        assert!(path_is_within("src".into(), "src".into()));
        assert!(!path_is_within("srcx/a.rs".into(), "src".into()));
        assert!(path_is_within("anything".into(), "".into()));
    }
}
