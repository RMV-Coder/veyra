#![no_main]
//! Tree-sitter-based analysis must cope with any input, and a successful structural merge must
//! always produce a file that parses.

use libfuzzer_sys::fuzz_target;
use veyra_semantic::Lang;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else { return };
    let mut parts = text.splitn(3, '\u{0}');
    let (base, ours, theirs) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    for (path, lang) in [("a.rs", Lang::Rust), ("a.py", Lang::Python), ("a.ts", Lang::TypeScript), ("a.go", Lang::Go)] {
        let _ = veyra_semantic::semantic_diff(path, base, ours);
        if let Ok(m) = veyra_semantic::structural_merge(path, base, ours, theirs) {
            let parsed = veyra_semantic::parse(lang, &m.text).expect("parses");
            assert!(!parsed.has_errors(), "structural merge produced invalid {path}");
        }
        if let Some(p) = veyra_semantic::parse(lang, base) {
            let _ = veyra_semantic::imports(&p);
        }
    }
});
