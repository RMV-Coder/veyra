//! Git's delta format (see `Documentation/gitformat-pack.txt`, "Deltified representation").
//!
//! A delta is `varint(base size) varint(result size)` followed by instructions:
//!
//! * **copy** — `1xxxxxxx` + up to 4 offset bytes and 3 size bytes (bits select which are present):
//!   copy `size` bytes from `offset` in the base (size 0 means 0x10000);
//! * **insert** — `0nnnnnnn` (1..=127) followed by that many literal bytes.
//!
//! The encoder indexes the base in 16-byte blocks and greedily extends matches in the target,
//! which is simple, deterministic, and plenty for source files.

use std::collections::HashMap;

const BLOCK: usize = 16;
/// Candidate positions examined per block hash.
const MAX_CANDIDATES: usize = 8;
const MAX_COPY: usize = 0xff_ffff;

fn push_varint(out: &mut Vec<u8>, mut n: usize) {
    loop {
        let mut byte = (n & 0x7f) as u8;
        n >>= 7;
        if n > 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if n == 0 {
            break;
        }
    }
}

fn flush_insert(out: &mut Vec<u8>, pending: &mut Vec<u8>) {
    for chunk in pending.chunks(0x7f) {
        out.push(chunk.len() as u8);
        out.extend_from_slice(chunk);
    }
    pending.clear();
}

fn push_copy(out: &mut Vec<u8>, mut offset: usize, mut size: usize) {
    while size > 0 {
        let n = size.min(MAX_COPY);
        let mut cmd = 0x80u8;
        let mut args = Vec::with_capacity(7);
        for k in 0..4 {
            let b = ((offset >> (8 * k)) & 0xff) as u8;
            if b != 0 {
                cmd |= 1 << k;
                args.push(b);
            }
        }
        for k in 0..3 {
            let b = ((n >> (8 * k)) & 0xff) as u8;
            if b != 0 {
                cmd |= 0x10 << k;
                args.push(b);
            }
        }
        out.push(cmd);
        out.extend_from_slice(&args);
        offset += n;
        size -= n;
    }
}

/// Encode `target` as a delta against `base`. Returns `None` when a delta can't help.
pub fn create(base: &[u8], target: &[u8]) -> Option<Vec<u8>> {
    if base.len() < BLOCK || target.len() < BLOCK || base.len() > u32::MAX as usize {
        return None;
    }
    let mut index: HashMap<&[u8], Vec<u32>> = HashMap::with_capacity(base.len() / BLOCK + 1);
    let mut pos = 0;
    while pos + BLOCK <= base.len() {
        index.entry(&base[pos..pos + BLOCK]).or_default().push(pos as u32);
        pos += BLOCK;
    }

    let mut out = Vec::with_capacity(target.len() / 4);
    push_varint(&mut out, base.len());
    push_varint(&mut out, target.len());
    let mut pending: Vec<u8> = Vec::new();
    let mut i = 0;
    while i + BLOCK <= target.len() {
        let mut best: Option<(usize, usize)> = None;
        if let Some(candidates) = index.get(&target[i..i + BLOCK]) {
            for &c in candidates.iter().take(MAX_CANDIDATES) {
                let c = c as usize;
                let mut len = BLOCK;
                while c + len < base.len() && i + len < target.len() && base[c + len] == target[i + len] {
                    len += 1;
                }
                if best.is_none_or(|(_, l)| len > l) {
                    best = Some((c, len));
                }
            }
        }
        match best {
            Some((start, forward)) => {
                // Extend the match backwards over bytes we were about to insert. That lengthens
                // the copy, but the cursor only advances by the forward part.
                let (mut offset, mut len) = (start, forward);
                while !pending.is_empty() && offset > 0 && base[offset - 1] == *pending.last().expect("non-empty") {
                    pending.pop();
                    offset -= 1;
                    len += 1;
                }
                flush_insert(&mut out, &mut pending);
                push_copy(&mut out, offset, len);
                i += forward;
            }
            None => {
                pending.push(target[i]);
                i += 1;
            }
        }
    }
    pending.extend_from_slice(&target[i.min(target.len())..]);
    flush_insert(&mut out, &mut pending);
    Some(out)
}

/// Apply a delta to `base` (used to verify deltas before sending them).
pub fn apply(base: &[u8], delta: &[u8]) -> Option<Vec<u8>> {
    let mut pos = 0;
    let read_varint = |pos: &mut usize| -> Option<usize> {
        let mut n = 0usize;
        let mut shift = 0;
        loop {
            let b = *delta.get(*pos)?;
            *pos += 1;
            if shift >= usize::BITS {
                return None; // malformed: longer than any real size
            }
            n |= ((b & 0x7f) as usize) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                return Some(n);
            }
        }
    };
    let base_len = read_varint(&mut pos)?;
    let result_len = read_varint(&mut pos)?;
    if base_len != base.len() {
        return None;
    }
    // Don't trust the declared size for the allocation (a corrupt delta could claim terabytes).
    let mut out = Vec::with_capacity(result_len.min(16 << 20));
    while pos < delta.len() {
        let cmd = delta[pos];
        pos += 1;
        if cmd & 0x80 != 0 {
            let mut offset = 0usize;
            let mut size = 0usize;
            for k in 0..4 {
                if cmd & (1 << k) != 0 {
                    offset |= (*delta.get(pos)? as usize) << (8 * k);
                    pos += 1;
                }
            }
            for k in 0..3 {
                if cmd & (0x10 << k) != 0 {
                    size |= (*delta.get(pos)? as usize) << (8 * k);
                    pos += 1;
                }
            }
            if size == 0 {
                size = 0x10000;
            }
            out.extend_from_slice(base.get(offset..offset.checked_add(size)?)?);
        } else if cmd != 0 {
            let n = cmd as usize;
            out.extend_from_slice(delta.get(pos..pos + n)?);
            pos += n;
        } else {
            return None; // reserved
        }
    }
    (out.len() == result_len).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(base: &[u8], target: &[u8]) -> usize {
        let delta = create(base, target).expect("delta");
        assert_eq!(apply(base, &delta).as_deref(), Some(target), "delta must reproduce the target");
        delta.len()
    }

    #[test]
    fn small_edit_in_a_large_file_is_tiny() {
        let base: Vec<u8> = (0..20_000).map(|i| format!("line {i}\n")).collect::<String>().into_bytes();
        let mut target = base.clone();
        target.splice(5_000..5_010, b"CHANGED!!!".iter().copied());
        target.extend_from_slice(b"appended line\n");
        let n = round_trip(&base, &target);
        assert!(n < 200, "delta was {n} bytes for a {}-byte file", target.len());
    }

    #[test]
    fn unrelated_content_still_round_trips() {
        let base = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_vec();
        let target = b"the quick brown fox jumps over the lazy dog, twice over".to_vec();
        round_trip(&base, &target);
    }

    #[test]
    fn insertions_at_both_ends() {
        let base: Vec<u8> = (0..500).map(|i| (i % 251) as u8).collect();
        let mut target = b"prefix-bytes".to_vec();
        target.extend_from_slice(&base);
        target.extend_from_slice(b"-suffix");
        round_trip(&base, &target);
    }

    #[test]
    fn copies_longer_than_the_size_limit_are_split() {
        let base: Vec<u8> = (0..(MAX_COPY + 1000)).map(|i| (i * 7 % 253) as u8).collect();
        let mut target = base.clone();
        target.push(1);
        round_trip(&base, &target);
    }

    #[test]
    fn tiny_inputs_are_not_deltified() {
        assert!(create(b"short", b"short too").is_none());
    }
}

#[cfg(test)]
mod properties {
    use proptest::prelude::*;

    fn similar_pair() -> impl Strategy<Value = (Vec<u8>, Vec<u8>)> {
        // A base, and a target made of slices of the base plus fresh bytes.
        (
            proptest::collection::vec(any::<u8>(), 0..4096),
            proptest::collection::vec(
                (any::<u16>(), any::<u16>(), proptest::collection::vec(any::<u8>(), 0..64)),
                0..32,
            ),
        )
            .prop_map(|(base, edits)| {
                let mut target = Vec::new();
                for (start, len, fresh) in edits {
                    if !base.is_empty() {
                        let s = start as usize % base.len();
                        let e = (s + len as usize % 512).min(base.len());
                        target.extend_from_slice(&base[s..e]);
                    }
                    target.extend_from_slice(&fresh);
                }
                (base, target)
            })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn deltas_round_trip((base, target) in similar_pair()) {
            if let Some(delta) = super::create(&base, &target) {
                prop_assert_eq!(super::apply(&base, &delta), Some(target));
            }
        }

        #[test]
        fn applying_garbage_never_panics(base in proptest::collection::vec(any::<u8>(), 0..512), delta in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = super::apply(&base, &delta);
        }
    }
}
