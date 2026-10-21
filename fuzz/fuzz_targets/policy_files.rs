#![no_main]
//! Committed policy files (.veyra/*.toml) come from other people: parsing must never panic.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else { return };
    let _ = veyra_intent::Intent::from_toml(text);
    let _ = veyra_security::Policy::from_toml(text);
    if let Ok(analyzer) = veyra_analyze::Analyzer::from_toml(text) {
        let file = veyra_analyze::ChangedFile { path: "db/migrations/a.sql", old: None, new: Some(text) };
        let _ = analyzer.analyze(&[file]);
    }
});
