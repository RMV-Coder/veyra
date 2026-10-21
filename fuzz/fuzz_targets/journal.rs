#![no_main]
//! The operation journal parser must reject corruption with an error, never a panic.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = veyra_ops::parse_log(text);
    }
});
