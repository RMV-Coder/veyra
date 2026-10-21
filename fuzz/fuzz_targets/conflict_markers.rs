#![no_main]
//! Conflict normalization (rerere ids) must be total and idempotent.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let kind = veyra_core::gix::hash::Kind::Sha1;
    if let Some(n) = veyra_core::rerere::normalize(data, kind) {
        let again = veyra_core::rerere::normalize(&n.preimage, kind).expect("a preimage is a conflict");
        assert_eq!(again.id, n.id);
        assert_eq!(again.preimage, n.preimage);
    }
});
