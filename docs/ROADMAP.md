# Veyra Roadmap

Each phase ends with an **exit criterion** that has to be met before the next phase starts. Git
compatibility is the main engineering challenge (DESIGN.md §6), so every phase adds conformance
tests that use real Git as the oracle.

## Phase 1 — Foundation + safety net ✅ (v0.1, current)

The order was changed from the original brainstorm (A14): recovery comes *first*, because the
journal has to wrap every mutating command from the start.

- [x] Cargo workspace: `veyra-core`, `veyra-ops`, `veyra-security`, `veyra-intent`, `veyra-cli`
- [x] Repository facade on gitoxide 0.89: objects, refs, index, status, clean/smudge filters
- [x] Porcelain: init, clone, status, add, rm, mv, restore/discard/unstage, commit (amend,
      trailers, editor), log, show, diff, branch, switch, checkout, merge (ff / clean 3-way /
      conflicts / abort / continue), reset, tag, remote, fetch, pull, config, plumbing basics
- [x] Operation log with undo / redo / op restore, external-change detection, gc protection
- [x] Automatic and manual checkpoints, watch mode, retention
- [x] Secret scanning on commit, `security scan`, fingerprint allowlists
- [x] Intents with deterministic guards, `Intent:` / `Assisted-by:` trailers
- [x] `doctor`, `explain`, `branch stale`, `--json`, agent mode
- [x] Differential tests against Git (fsck, status, log, diff, CRLF, gc survival, merge conflicts)

**Exit criterion:** met. 66 tests pass on Windows, and every repository Veyra writes passes
`git fsck --strict`.

Known gaps carried forward: push, stash, rebase, cherry-pick, revert, hooks, commit signing,
`add -p`, `log --graph`, blame, submodule writes, sparse checkout writes, SHA-256 repositories.

## Phase 2 — Everyday Git parity ✅

- [x] `veyra push` on gitoxide's transport and handshake: the receive-pack protocol, `-u`, `--tags`,
      `--delete`, `+refspec`, `--force`, `--force-with-lease`, `--dry-run`, credential helpers. Journaled,
      and `veyra undo` refuses to silently unpublish pushed commits (B10).
- [x] `stash` / `shelve` / `unshelve`, compatible with `refs/stash` and its reflog
- [x] `clean`, with checkpoint-first
- [x] `cherry-pick`, `revert`, `rebase` and `rebase -i` (pick/reword/edit/squash/fixup/drop and
      reordering), with Git-compatible conflict state
- [x] `blame`, `log --graph`
- [x] Git hooks (`pre-commit`, `prepare-commit-msg`, `commit-msg`, `post-commit`, `pre-push`,
      `post-checkout`, `post-merge`), with Git for Windows' `sh` discovery and a trust check
- [x] `branch -m` keeps the reflog and upstream configuration
- [x] Delta-compressed (thin) packs for push: a block-matching delta encoder, `REF_DELTA` against the previous version of each path
- [x] `add -p` (per-hunk y/n/q/a/d/s) and `exec`/`break` in `rebase -i`
- [x] `branch cleanup [--dry-run]`, `branch compare`, `sync [--push]`
- [x] `branch graph`, `branch explain` (with an in-memory merge preview)
- [x] CI on Linux, macOS and Windows (first green run in 1.3, once the repository went public), and a
      published compatibility matrix

**Exit criterion:** a developer can use Veyra full-time on a GitHub project without falling back
to `git`, and the conformance suite covers every Phase 1–2 command on all three platforms.

## Phase 3 — Trust and scale ✅

- [x] Commit and tag signing (OpenPGP, X.509, SSH) via gitoxide, honouring `commit.gpgSign` / `tag.gpgSign`;
      `verify commit`, `verify tag`, `verify --history`
- [x] Attestations: in-toto v1 Statements (test results, reviews) in Git notes, optionally signed;
      `verify --provenance`
- [x] `clone --scope` / `--sparse` and `sparse set/add/list/disable` (Git's cone mode, skip-worktree bits)
- [x] `storage stats`, `maintenance run / pack-loose / prune` (no `git gc` needed)
- [x] Worktrees (each with its own journal) and submodules (status/init/update/add/foreach)
- [x] SHA-256 repositories (`init --object-format sha256`)
- [x] Performance budgets: `cargo test --release -p veyra-cli --test perf -- --ignored` (10k files).
      New blobs are written as packs instead of thousands of loose files.
- [x] Partial-clone object filters (`--filter=blob:none`, `blob:limit`, `tree:`), driven by Veyra on
      gitoxide's transport, with on-demand fetching of missing objects (1.3)

**Exit criterion:** met. Signed history verifies in both Git and Veyra, and the 10k-file budgets pass.

## Phase 4 — Code intelligence (deterministic) ✅

- [x] `diff --semantic` / `--functions` with Tree-sitter (Rust, TypeScript/TSX, JavaScript, Python, Go,
      Java): added, removed, modified, signature changed, reformatted-only and renamed symbols.
      The grammars are C code compiled into the binary.
- [x] Structural merge in conservative mode: conflicts in *different* top-level symbols are merged
      per symbol, re-parsed and verified; reported on screen and as a `Veyra-Structural-Merge`
      trailer; `veyra.structuralMerge = stage` stops for review instead of committing
- [x] `impact [--staged | --commit | --branch] [--tests]`: import resolution per language, name
      references, test mapping, co-change history, risk with reasons, limitations reported
- [x] `merge resolutions` (rerere) in Git's rr-cache format
- [x] C, C++, C#, Ruby, PHP, Kotlin, Swift, Scala, Bash, Lua and Elixir grammars (1.3)

**Exit criterion:** met (for the original six languages; eleven more were added in 1.3). False-equivalence guards: "reformatted" requires an
identical token sequence *and* tree shape (so re-indenting Python is a change), renames require a
token-identical, non-trivial body, and structural merges refuse anything they can't verify.

## Phase 5 — Optional AI and domain analyzers ✅

- [x] `veyra ai configure / disable / status`: the Claude API (raw HTTPS; there is no official Rust
      SDK) or any local model through the `command` provider. Off by default; keys only from the
      environment; every payload is secret-scanned; `--dry-run` shows what would be sent.
- [x] `commit --smart` (message proposals) and `commit --split` (file-level proposals, validated
      before they are shown). The developer always approves.
- [x] Assisted merge (`merge --assist`, `ai resolve`): proposals are checked for markers and syntax,
      shown as a diff with a confidence, and uncertain ones always need a person
- [x] `ai explain / review / summarize / risks` (risks build on the deterministic impact report)
- [x] Migration and IaC analyzers (`analyze --migration / --infra`), extensible with repository rules
      in `.veyra/analyzers.toml`
- [x] AI-proposed intents (`intent suggest`): guards that don't hold are dropped; a human accepts
- [x] Hunk-level `commit --split`; out-of-process analyzer plugins with local trust (1.3)

**Exit criterion:** met. Every AI feature degrades to "AI features are off" with no side effects,
proposals are validated deterministically, and nothing changes history without explicit approval
(`--yes` in scripts; never for low-confidence merge resolutions).

## Phase 6 — Production hardening → 1.0 ✅

- [x] Property tests for every parser and encoder that sees untrusted input (delta, journal,
      conflict normalization, Tree-sitter analysis, policy files, secret scanning) and cargo-fuzz
      targets for the same (`fuzz/`). Git's own formats are parsed by gitoxide, fuzzed upstream.
- [x] Crash consistency: a torn journal record is tolerated and cleaned up on the next write
- [x] Concurrency: Veyra commands queue on the journal lock; Veyra and Git can run at the same
      time — index writes are compare-and-swap under Git's `index.lock`, so a concurrent `git add`
      is never silently lost, and changes Git makes during a checkpoint are journaled separately
- [x] Release workflow (Linux, macOS, Windows binaries, SHA-256 checksums, build-provenance
      attestations), install scripts that verify both, Homebrew and winget manifests
- [x] [COMPATIBILITY.md](COMPATIBILITY.md) and the 1.x [STABILITY.md](STABILITY.md) promise for CLI
      flags, `--json` output, the journal format and committed policy files
- [x] The documentation as an mdBook site, published to GitHub Pages by `.github/workflows/docs.yml` (1.3)
- [ ] Submit the Homebrew and winget manifests

**Exit criterion:** met for 1.0.

## After 1.0

- [x] `veyra serve`: local read-only viewer (1.2)
- [x] Pull-request review action (1.2)
- [x] Rebase, bisect and cherry-pick/revert sequences shared with Git (1.3)
- [x] Partial and shallow clones, merge strategies, non-cone sparse patterns, `doctor --full`,
      Git LFS coverage, `pre-rebase` / `post-rewrite` hooks (1.3)
- [x] Eleven more languages, hunk-level `commit --split`, analyzer plugins, documentation site (1.3)

- Reftable, once gitoxide reads it (until then Veyra refuses reftable repositories with the
  `git refs migrate` fix)
- Dropping the beta label once Veyra has seen daily use on Linux and macOS
