#![no_main]
//! Git delta decoding must never panic or over-allocate, and our encoder must round-trip.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let split = data.first().copied().unwrap_or(0) as usize;
    let rest = data.get(1..).unwrap_or_default();
    let (base, other) = rest.split_at(split.min(rest.len()));
    // Arbitrary bytes as a delta.
    let _ = veyra_core::delta::apply(base, other);
    // Arbitrary bytes as a target: create → apply must reproduce it.
    if let Some(delta) = veyra_core::delta::create(base, other) {
        assert_eq!(veyra_core::delta::apply(base, &delta).as_deref(), Some(other));
    }
});
