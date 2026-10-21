# Fuzzing

Coverage-guided fuzz targets for the parsers and encoders that see untrusted input. They use
[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) (libFuzzer) and need a nightly toolchain;
they are deliberately outside the main workspace, so `cargo test` doesn't build them.

| Target | What it checks |
|---|---|
| `delta_apply` | Git delta decoding never panics or over-allocates; `create` → `apply` round-trips |
| `journal` | The operation-log parser rejects corruption with an error |
| `conflict_markers` | rerere normalization is total and idempotent |
| `semantic` | Tree-sitter analysis copes with any input; structural merges always parse |
| `policy_files` | `.veyra/intents`, `secrets.toml` and `analyzers.toml` parsing never panics |

```bash
cargo install cargo-fuzz
cd fuzz
cargo +nightly fuzz run delta_apply -- -max_total_time=300
```

Git's own formats (index, packs, refs) are parsed by gitoxide, which is fuzzed upstream. Faster
property tests covering the same invariants run with every `cargo test` (search for
`mod properties`).
