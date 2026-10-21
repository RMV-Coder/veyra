//! Semantic diff: which functions, types and methods changed between two versions of a file.

use std::collections::{BTreeMap, HashMap};

use crate::items::{Item, ItemKey, ItemKind, Lang, Parsed, hash_tokens, items, parse, tokens_outside};

/// How one symbol changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Removed,
    /// The code changed. `signature` is true when the part before the body changed too.
    Modified {
        signature: bool,
    },
    /// Only whitespace, line breaks or comments changed.
    Reformatted,
    /// Same body, new name (the old name is in `SymbolChange::old_name`).
    Renamed,
}

impl ChangeKind {
    pub fn label(&self) -> &'static str {
        match self {
            ChangeKind::Added => "added",
            ChangeKind::Removed => "removed",
            ChangeKind::Modified { signature: true } => "signature changed",
            ChangeKind::Modified { signature: false } => "modified",
            ChangeKind::Reformatted => "reformatted",
            ChangeKind::Renamed => "renamed",
        }
    }
}

/// One changed symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolChange {
    pub change: ChangeKind,
    pub kind: ItemKind,
    /// `Container::name` (the new name for renames).
    pub name: String,
    pub old_name: Option<String>,
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
    pub old_signature: Option<String>,
    pub new_signature: Option<String>,
}

/// The semantic diff of one file.
#[derive(Debug, Clone)]
pub struct FileSemanticDiff {
    pub lang: Lang,
    pub changes: Vec<SymbolChange>,
    /// Code outside any symbol changed (imports, top-level statements, …).
    pub outside_changed: bool,
    /// One of the versions has syntax errors; the result is best-effort.
    pub parse_errors: bool,
}

impl FileSemanticDiff {
    /// Only formatting/comments changed anywhere in the file.
    pub fn formatting_only(&self) -> bool {
        !self.outside_changed && self.changes.iter().all(|c| c.change == ChangeKind::Reformatted)
    }
}

fn change(c: ChangeKind, old: Option<&Item>, new: Option<&Item>) -> SymbolChange {
    let main = new.or(old).expect("an item");
    SymbolChange {
        change: c,
        kind: main.kind,
        name: main.display_name(),
        old_name: old.filter(|_| new.is_some()).map(Item::display_name).filter(|n| *n != main.display_name()),
        old_line: old.map(|i| i.line),
        new_line: new.map(|i| i.line),
        old_signature: old.map(|i| i.signature.clone()),
        new_signature: new.map(|i| i.signature.clone()),
    }
}

/// Hash of everything outside top-level items.
pub(crate) fn outside_hash(parsed: &Parsed, items: &[Item]) -> u64 {
    let top: Vec<_> = items.iter().filter(|i| i.top_level).map(|i| i.range.clone()).collect();
    hash_tokens(tokens_outside(parsed, &top).into_iter())
}

/// Diff two parsed versions.
pub fn diff_parsed(old: &Parsed, new: &Parsed) -> FileSemanticDiff {
    let old_items = items(old);
    let new_items = items(new);
    let old_by: HashMap<ItemKey, &Item> = old_items.iter().map(|i| (i.key(), i)).collect();
    let new_by: HashMap<ItemKey, &Item> = new_items.iter().map(|i| (i.key(), i)).collect();

    let mut changes = Vec::new();
    for n in &new_items {
        let Some(o) = old_by.get(&n.key()) else { continue };
        if n.is_container() {
            // Members are reported on their own; only the container's own code counts here.
            if o.own_hash != n.own_hash {
                changes.push(change(ChangeKind::Modified { signature: o.signature != n.signature }, Some(o), Some(n)));
            }
        } else if o.text_hash == n.text_hash {
        } else if o.token_hash == n.token_hash {
            changes.push(change(ChangeKind::Reformatted, Some(o), Some(n)));
        } else {
            changes.push(change(ChangeKind::Modified { signature: o.signature != n.signature }, Some(o), Some(n)));
        }
    }

    let removed: Vec<&Item> = old_items.iter().filter(|i| !new_by.contains_key(&i.key())).collect();
    let added: Vec<&Item> = new_items.iter().filter(|i| !old_by.contains_key(&i.key())).collect();

    // A rename: one removed and one added item of the same kind and container whose code is
    // token-identical once their own names are masked. Ambiguous matches stay add + remove.
    type Bucket<'a> = (Vec<&'a Item>, Vec<&'a Item>);
    let mut buckets: BTreeMap<(ItemKind, Option<String>, u64), Bucket<'_>> = BTreeMap::new();
    // Tiny items (`fn f() {}`) look alike by accident, so they are never paired.
    const MIN_RENAME_TOKENS: usize = 10;
    for r in removed.iter().filter(|r| r.token_count >= MIN_RENAME_TOKENS) {
        buckets.entry((r.kind, r.container.clone(), r.anonymous_hash)).or_default().0.push(r);
    }
    for a in added.iter().filter(|a| a.token_count >= MIN_RENAME_TOKENS) {
        buckets.entry((a.kind, a.container.clone(), a.anonymous_hash)).or_default().1.push(a);
    }
    let mut renamed_old = Vec::new();
    let mut renamed_new = Vec::new();
    for (rs, ads) in buckets.values() {
        if let ([r], [a]) = (rs.as_slice(), ads.as_slice()) {
            changes.push(change(ChangeKind::Renamed, Some(r), Some(a)));
            renamed_old.push(r.range.clone());
            renamed_new.push(a.range.clone());
        }
    }
    for r in removed.iter().filter(|r| !renamed_old.contains(&r.range)) {
        // Members of a removed container are implied by it.
        if !removed.iter().any(|c| {
            c.is_container() && c.range != r.range && c.range.start <= r.range.start && r.range.end <= c.range.end
        }) {
            changes.push(change(ChangeKind::Removed, Some(r), None));
        }
    }
    for a in added.iter().filter(|a| !renamed_new.contains(&a.range)) {
        if !added.iter().any(|c| {
            c.is_container() && c.range != a.range && c.range.start <= a.range.start && a.range.end <= c.range.end
        }) {
            changes.push(change(ChangeKind::Added, None, Some(a)));
        }
    }
    changes.sort_by_key(|c| (c.new_line.or(c.old_line).unwrap_or(0), c.old_line.unwrap_or(0)));

    FileSemanticDiff {
        lang: new.lang,
        changes,
        outside_changed: outside_hash(old, &old_items) != outside_hash(new, &new_items),
        parse_errors: old.has_errors() || new.has_errors(),
    }
}

/// Diff two versions of the file at `path`. `None` when the language isn't supported.
pub fn semantic_diff(path: &str, old: &str, new: &str) -> Option<FileSemanticDiff> {
    let lang = Lang::from_path(path)?;
    Some(diff_parsed(&parse(lang, old)?, &parse(lang, new)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(path: &str, old: &str, new: &str) -> Vec<(String, &'static str)> {
        semantic_diff(path, old, new).unwrap().changes.iter().map(|c| (c.name.clone(), c.change.label())).collect()
    }

    #[test]
    fn modified_added_removed() {
        let old = "fn a() -> u32 { 1 }\nfn b() {}\n";
        let new = "fn a() -> u32 { 2 }\nfn c(x: u8) { let _ = x; }\n";
        assert_eq!(
            labels("x.rs", old, new),
            vec![("a".into(), "modified"), ("c".into(), "added"), ("b".into(), "removed")]
        );
    }

    #[test]
    fn reformat_only_is_recognised() {
        let d = semantic_diff("x.rs", "fn a() { let x = 1; }\n", "fn a() {\n    // note\n    let x = 1;\n}\n").unwrap();
        assert_eq!(d.changes[0].change, ChangeKind::Reformatted);
        assert!(d.formatting_only());
    }

    #[test]
    fn rename_needs_identical_non_trivial_body() {
        assert_eq!(labels("x.rs", "fn gone() {}\n", "fn fresh() {}\n").len(), 2);
        assert_eq!(
            labels("x.py", "def old(a):\n    return a + 1\n", "def new(a):\n    return a + 1\n"),
            vec![("new".into(), "renamed")]
        );
        let changed = labels("x.py", "def old(a):\n    return a + 1\n", "def new(a):\n    return a + 2\n");
        assert_eq!(changed.len(), 2, "{changed:?}");
    }

    #[test]
    fn method_change_does_not_flag_the_class() {
        let old = "class A {\n  m() { return 1 }\n  n() { return 1 }\n}\n";
        let new = "class A {\n  m() { return 2 }\n  n() { return 1 }\n}\n";
        assert_eq!(labels("x.ts", old, new), vec![("A::m".into(), "modified")]);
    }

    #[test]
    fn signature_changes_and_outside_changes() {
        let d = semantic_diff(
            "x.go",
            "package p\nfunc F(a int) int { return a }\n",
            "package p\nimport \"fmt\"\nfunc F(a, b int) int { return a }\n",
        )
        .unwrap();
        assert_eq!(d.changes[0].change, ChangeKind::Modified { signature: true });
        assert!(d.outside_changed);
    }
}
