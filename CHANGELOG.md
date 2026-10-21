# Changelog

All notable changes are listed here. Veyra follows [semantic versioning](https://semver.org/);
see [docs/STABILITY.md](docs/STABILITY.md) for what 1.x keeps stable.

## 1.1.0

Git parity for the remaining commands, each checked against Git in the end-to-end tests:
`shortlog`, `describe`, `grep`, `bisect` (with `run`), `format-patch`, `apply`, `am`, `notes`,
`archive` (tar, tar.gz, zip), `bundle`, `remote refs` / `ls-remote`, and the plumbing commands
`write-tree`, `commit-tree`, `update-ref`, `for-each-ref`, `merge-base`, `pack-objects`.
New: `veyra release notes / prepare / verify`.

## 1.0.0

Hardening and the 1.0 promise.

- Property tests and cargo-fuzz targets for the delta codec, the journal, conflict normalization,
  Tree-sitter analysis, policy files and secret scanning.
- **Fixed:** decoding a malformed Git delta could panic (found by the property tests).
- **Fixed:** a status run could overwrite the index with a stale copy if Git staged something at
  the same moment; every index write is now compare-and-swap under `index.lock`.
- **Fixed:** commits Git made while a checkpoint was running were attributed to the checkpoint,
  so `undo` could refuse to undo them; checkpoints now record no state change of their own.
- **Fixed:** on Windows, commands racing for the journal lock could fail with "Access is
  denied"; they now wait their turn (up to 30 s).
- Non-cone sparse-checkout patterns are refused instead of being misread.
- Release workflow, install scripts with checksum and provenance verification, Homebrew and
  winget manifests; compatibility matrix and stability promise.

## 0.5.0

Optional AI (Claude API or a local command; off by default): `ai explain / review / summarize /
risks / resolve`, `commit --smart / --split`, `merge --assist`, `intent suggest`. Deterministic
`veyra analyze --migration / --infra`. A torn journal line no longer corrupts the log.

## 0.4.0

Code intelligence: `diff --semantic`, structural merge of conflicts in different symbols,
`veyra impact`, rerere in Git's rr-cache format. `merge --abort` after an `undo` now restores the
right state.

## 0.3.0

Signing and verification, attestations, worktrees, submodules, sparse checkout and
`clone --scope`, maintenance, SHA-256 repositories, performance budgets.

## 0.2.0

Push (with deltas), stash, cherry-pick, revert, rebase and `rebase -i`, `add -p`, hooks, blame,
`log --graph`, `clean`, `sync`, `branch graph / explain`.

## 0.1.0

The first release: a Git-compatible core with undo for every operation, checkpoints, secret
scanning, intents, `explain` and `doctor`.
