//! Conservative structural merge.
//!
//! A file is split into top-level symbols, each carrying the text before it (attributes, doc
//! comments, blank lines, imports, …), plus the tail after the last one. The three versions are
//! then merged per symbol: a symbol changed on one side only takes that side; a symbol changed
//! identically on both sides takes either; anything else is refused. The merge only succeeds when
//! every symbol resolves, the result parses without errors, and it contains exactly the expected
//! symbols — otherwise the caller falls back to ordinary conflict markers.

use std::collections::{HashMap, HashSet};

use crate::items::{ItemKey, Lang, Parsed, items, parse};

/// A successful structural merge.
#[derive(Debug, Clone)]
pub struct StructuralMerge {
    pub text: String,
    /// Human-readable notes: which symbol came from which side.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum SegKey {
    Item(ItemKey),
    Tail,
}

struct Segments {
    order: Vec<SegKey>,
    text: HashMap<SegKey, String>,
    names: HashMap<SegKey, String>,
}

fn segments(parsed: &Parsed) -> Option<Segments> {
    let mut order = Vec::new();
    let mut text = HashMap::new();
    let mut names = HashMap::new();
    let mut cursor = 0;
    for item in items(parsed).into_iter().filter(|i| i.top_level) {
        if item.range.start < cursor {
            return None; // overlapping top-level items: don't guess
        }
        let key = SegKey::Item(item.key());
        let seg = parsed.source.get(cursor..item.range.end)?.to_owned();
        if text.insert(key.clone(), seg).is_some() {
            return None;
        }
        names.insert(key.clone(), format!("{} {}", item.kind.label(), item.display_name()));
        order.push(key);
        cursor = item.range.end;
    }
    text.insert(SegKey::Tail, parsed.source.get(cursor..)?.to_owned());
    Some(Segments { order, text, names })
}

/// The base keys in the order a side has them.
fn base_order<'a>(side: &'a Segments, base: &HashSet<&SegKey>) -> Vec<&'a SegKey> {
    side.order.iter().filter(|k| base.contains(k)).collect()
}

/// Try to merge `ours` and `theirs` (both derived from `base`) symbol by symbol.
pub fn structural_merge(path: &str, base: &str, ours: &str, theirs: &str) -> Result<StructuralMerge, String> {
    let lang = Lang::from_path(path).ok_or("not a supported language")?;
    let parse_ok = |s: &str| parse(lang, s).filter(|p| !p.has_errors());
    let (Some(b), Some(o), Some(t)) = (parse_ok(base), parse_ok(ours), parse_ok(theirs)) else {
        return Err("a version has syntax errors".into());
    };
    let (Some(bs), Some(os), Some(ts)) = (segments(&b), segments(&o), segments(&t)) else {
        return Err("ambiguous symbols".into());
    };

    let base_keys: HashSet<&SegKey> = bs.order.iter().collect();
    let base_seq: Vec<&SegKey> = bs.order.iter().collect();
    let ours_seq = base_order(&os, &base_keys);
    let theirs_seq = base_order(&ts, &base_keys);
    let in_base_order = |seq: &[&SegKey]| {
        let kept: HashSet<&SegKey> = seq.iter().copied().collect();
        let filtered: Vec<&SegKey> = base_seq.iter().copied().filter(|k| kept.contains(k)).collect();
        filtered == seq
    };
    // The skeleton is the side that reordered (if only one did); new symbols of the other side
    // are inserted after the symbol that precedes them there.
    let (skeleton, other) = match (in_base_order(&ours_seq), in_base_order(&theirs_seq)) {
        (true, true) | (false, true) => (&os, &ts),
        (true, false) => (&ts, &os),
        (false, false) => return Err("both sides reordered symbols".into()),
    };
    let mut order: Vec<SegKey> = skeleton.order.clone();
    let mut last_known: Option<SegKey> = None;
    for key in &other.order {
        let new_here = !base_keys.contains(key) && !skeleton.text.contains_key(key);
        if new_here {
            let at = match &last_known {
                Some(prev) => order.iter().position(|k| k == prev).map(|i| i + 1).unwrap_or(order.len()),
                None => 0,
            };
            order.insert(at, key.clone());
        }
        last_known = Some(key.clone());
    }
    // Base symbols the skeleton side deleted: fine only if the other side left them alone.
    for key in &bs.order {
        if !order.contains(key) {
            if other.text.get(key).is_some_and(|t| Some(t) != bs.text.get(key)) {
                let name = bs.names.get(key).cloned().unwrap_or_default();
                return Err(format!("{name} was removed on one side and changed on the other"));
            }
            order.push(key.clone()); // resolves to "removed" below
        }
    }
    order.push(SegKey::Tail);

    let mut notes = Vec::new();
    let mut text = String::new();
    let mut expected = HashSet::new();
    for key in &order {
        let bv = bs.text.get(key);
        let ov = os.text.get(key);
        let tv = ts.text.get(key);
        let name = os
            .names
            .get(key)
            .or_else(|| ts.names.get(key))
            .or_else(|| bs.names.get(key))
            .cloned()
            .unwrap_or_else(|| "the end of the file".into());
        let chosen = if ov == tv {
            ov
        } else if ov == bv {
            if tv.is_some() || bv.is_some() {
                notes.push(match (bv, tv) {
                    (None, Some(_)) => format!("{name}: added on theirs"),
                    (Some(_), None) => format!("{name}: removed on theirs"),
                    _ => format!("{name}: taken from theirs"),
                });
            }
            tv
        } else if tv == bv {
            notes.push(match (bv, ov) {
                (None, Some(_)) => format!("{name}: added on ours"),
                (Some(_), None) => format!("{name}: removed on ours"),
                _ => format!("{name}: taken from ours"),
            });
            ov
        } else {
            return Err(format!("{name} changed on both sides"));
        };
        if let Some(seg) = chosen {
            text.push_str(seg);
            if let SegKey::Item(k) = key {
                expected.insert(k.clone());
            }
        }
    }
    notes.retain(|n| !n.starts_with("the end of the file"));

    // Verify: the result must parse cleanly and contain exactly the merged symbols.
    let merged = parse_ok(&text).ok_or("the merged result does not parse")?;
    let got: HashSet<ItemKey> = items(&merged).into_iter().filter(|i| i.top_level).map(|i| i.key()).collect();
    if got != expected {
        return Err("the merged result has unexpected symbols".into());
    }
    Ok(StructuralMerge { text, notes })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "use std::fmt;\n\nfn a() -> u32 {\n    1\n}\n\nfn b() -> u32 {\n    1\n}\n";

    #[test]
    fn disjoint_symbol_edits_merge() {
        let ours = BASE.replace("fn a() -> u32 {\n    1", "fn a() -> u32 {\n    2");
        let theirs = BASE.replace("fn b() -> u32 {\n    1", "fn b() -> u32 {\n    3");
        let m = structural_merge("x.rs", BASE, &ours, &theirs).unwrap();
        assert!(m.text.contains("fn a() -> u32 {\n    2"));
        assert!(m.text.contains("fn b() -> u32 {\n    3"));
        assert_eq!(m.notes, vec!["function a: taken from ours", "function b: taken from theirs"]);
    }

    #[test]
    fn same_symbol_edits_are_refused() {
        let ours = BASE.replace("    1\n}\n\nfn b", "    2\n}\n\nfn b");
        let theirs = BASE.replace("    1\n}\n\nfn b", "    3\n}\n\nfn b");
        assert!(structural_merge("x.rs", BASE, &ours, &theirs).unwrap_err().contains("both sides"));
    }

    #[test]
    fn additions_on_both_sides_keep_their_place() {
        let ours = format!("{BASE}\nfn c() {{}}\n");
        let theirs = BASE.replace("fn b()", "fn z() {}\n\nfn b()");
        let m = structural_merge("x.rs", BASE, &ours, &theirs).unwrap();
        let (z, b, c) = (m.text.find("fn z").unwrap(), m.text.find("fn b").unwrap(), m.text.find("fn c").unwrap());
        assert!(z < b && b < c, "{}", m.text);
    }

    #[test]
    fn delete_vs_modify_is_refused_and_syntax_errors_bail() {
        let ours = BASE.replace("\nfn b() -> u32 {\n    1\n}\n", "");
        let theirs = BASE.replace("fn b() -> u32 {\n    1", "fn b() -> u32 {\n    9");
        assert!(structural_merge("x.rs", BASE, &ours, &theirs).is_err());
        assert!(structural_merge("x.rs", BASE, "fn a( {", BASE).is_err());
        assert!(structural_merge("x.txt", BASE, BASE, BASE).is_err());
    }

    #[test]
    fn python_methods_in_different_top_level_functions() {
        let base = "def f():\n    return 1\n\n\ndef g():\n    return 1\n";
        let ours = base.replace("def f():\n    return 1", "def f():\n    return 2");
        let theirs = base.replace("def g():\n    return 1", "def g():\n    return 3");
        let m = structural_merge("m.py", base, &ours, &theirs).unwrap();
        assert_eq!(m.text, "def f():\n    return 2\n\n\ndef g():\n    return 3\n");
    }
}
