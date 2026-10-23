# Changelog

All notable changes are listed here. Veyra follows [semantic versioning](https://semver.org/);
see [docs/STABILITY.md](docs/STABILITY.md) for what 1.x keeps stable.

## Unreleased

- **Fixed:** `veyra attest run -- <words>` joined its words with spaces and handed them to `sh -c`,
  so a word containing a space or a backslash (any path under `C:\Program Files`) was split and
  unescaped, and the run was recorded as a failed test. Several words are now quoted, one word is
  still a shell line (`"npm test && npm run lint"`), the original argv is recorded as `argv`, and
  a command that can't be started (exit 126/127) is reported instead of attested as a failure.
- `veyra discard` points at `veyra restore --staged --worktree` when the paths have staged changes,
  and `veyra undo` of a commit says that its changes were left staged.
- A leading UTF-8 byte-order mark is dropped from commit messages (`commit -F` with a file written
  by Windows PowerShell 5.1).

## 1.3.1

The first release with downloadable binaries, and the first whose test suite has passed on Linux
and macOS as well as Windows (CI runs on all three on every push). Veyra itself is unchanged
from 1.3.0; the first CI runs only found problems in the tests and the release workflow.

- **Fixed (tests):** the interactive-rebase tests used GNU `sed -i` as their todo editor, which
  macOS's BSD `sed` doesn't accept; they use small awk scripts now.
- **Fixed (tests):** the `format-patch` / `am` test applied patches in directory-listing order,
  which is arbitrary on Linux.
- **Fixed (release):** the Intel macOS build used GitHub's retired `macos-13` runner, so the release
  could never finish; it runs on `macos-15-intel`.
- COMPATIBILITY.md lists Linux and macOS as tested by CI; the beta note stays until Veyra has seen
  daily use there.

## 1.3.0

Veyra is labelled **beta**: it is tested against real Git on Windows, but Linux and macOS have no
CI results yet (README, COMPATIBILITY.md and STABILITY.md now say so).

**Interoperability with Git**

- Rebases (`.git/rebase-merge`), bisect sessions (`BISECT_*`, `refs/bisect/*`) and multi-commit
  cherry-picks and reverts (`.git/sequencer`) use Git's own state, so either tool can continue,
  skip or abort what the other started. State left by older Veyra versions is migrated.
- Partial clones: `clone --filter=blob:none | blob:limit=<n> | tree:<depth>`, configured exactly as
  Git configures them. Missing objects are fetched on demand, in batches before checkouts and
  merges; `--filter` with `--scope` downloads only the scope's files; `veyra backfill` fetches the rest.
- Shallow clones: `clone --depth`, `fetch --depth`, `fetch --unshallow`.
- Merge strategies: `-s ours`, `-s subtree`, `-X ours`, `-X theirs`, `-X subtree=<dir>`, and octopus
  merges of several branches (refused before anything changes if any of them conflicts).
- Non-cone sparse checkout (`sparse set --no-cone`), with Git's pattern rules.
- Git LFS is covered by the tests: clean filter on `add`, upload by the `pre-push` hook, smudge on
  checkout.
- The `pre-rebase` and `post-rewrite` (after `commit --amend`) hooks run.
- `status --porcelain`.
- Reftable repositories are refused with the conversion command (`git refs migrate
  --ref-format=files`) instead of an obscure error.

**Veyra features**

- `veyra doctor --full`: every object read back, rehashed and parsed, pack checksums verified, and
  everything reachable from references, `HEAD` and the index checked to exist (allowing for
  partial and shallow clones).
- Code intelligence for C, C++, C#, Ruby, PHP, Kotlin, Swift, Scala, Bash, Lua and Elixir:
  semantic diff, structural merge and impact analysis, including import resolution (`#include`,
  `using`, `require`, PSR-4, packages, Swift targets, `source`, Lua modules, Elixir aliases).
- `commit --split` can split one file's change by hunk across several commits.
- Analyzer plugins: `[[plugin]]` programs in `.veyra/analyzers.toml` that read the change as JSON
  and report findings. They run only after you trust their exact definition locally
  (`veyra analyze --trust-plugin <id>`). See docs/PLUGINS.md.
- The documentation is an mdBook site (`book.toml`), published to GitHub Pages by
  `.github/workflows/docs.yml`.

**Fixed**

- Commits in a partial clone failed because the tree writer required every blob to be present.
- `clone --filter=tree:<depth>` failed at checkout; trees are now fetched when first needed (with
  `blob:none`, as Git does), and `doctor` no longer reports them as missing.

## 1.2.0

- `veyra serve`: a local, read-only web viewer for the data a Git host can't show — signature status,
  AI-assistance and intent trailers, attestations, notes and semantic changes per commit; the undo
  journal with the current position; checkpoints; intents and their guards; impact; the branch
  tree. Loopback only, one-time key, Host checking, GET only.
- A reusable GitHub Action (`.github/actions/veyra-review`) that posts Veyra's impact, analyzer,
  semantic-diff and signature results on pull requests.

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
