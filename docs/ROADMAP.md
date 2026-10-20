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

## Phase 2 — Everyday Git parity (in progress)

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
- [ ] Delta-compressed packs for push (gix-pack's output pipeline)
- [x] `add -p` (per-hunk y/n/q/a/d)
- [ ] Hunk split/edit in `add -p`; `exec`/`break` in `rebase -i`
- [x] `branch cleanup [--dry-run]`, `branch compare`, `sync [--push]`
- [ ] `branch graph`, `branch explain`
- [ ] CI on Linux, macOS and Windows (the workflow exists but is blocked on account billing), and a
      published compatibility matrix

**Exit criterion:** a developer can use Veyra full-time on a GitHub project without falling back
to `git`, and the conformance suite covers every Phase 1–2 command on all three platforms.

## Phase 3 — Trust and scale

- Commit and tag signing (SSH, GPG), `verify commit/tag/--history`, attestations (in-toto/SLSA)
- `clone --scope` (declared scopes → partial clone + sparse cone), `maintenance`, `storage stats`
- Worktrees, submodules (write paths), sparse checkout, SHA-256 repositories as gix matures
- Performance budgets: `status` and checkpoint on a 100k-file repository within agreed limits

**Exit criterion:** benchmarks published; signed-history verification works on real projects.

## Phase 4 — Code intelligence (deterministic)

- `diff --semantic / --functions` using Tree-sitter grammars (TypeScript, JavaScript, Python, Rust,
  Go, Java, C/C++ first). Note: Tree-sitter grammars are C code compiled into the binary.
- Structural (syntax-aware) merge in Conservative mode, explained and never silently committed
- `impact [--staged] [--tests]`: import graphs, test mappings, evidence and limitations reported
- `merge resolutions` (rerere)

**Exit criterion:** the semantic tools are correct on a curated corpus of real-world changes, and
false-equivalence claims stay at zero.

## Phase 5 — Optional AI and domain analyzers

- `veyra ai configure / disable`, with local or remote providers. Never required; privacy mode by default.
- `commit --smart` (message proposals) and `commit --split` (hunk-level proposals). The developer
  always approves.
- Assisted and Experimental merge modes (approval required for uncertain resolutions)
- Migration and IaC analyzers as plugins (`analyze --migration`, Terraform and Kubernetes risk flags)
- AI-proposed intent guards (a human accepts them; verification stays deterministic)

**Exit criterion:** every AI feature has a deterministic fallback or degrades to "no suggestion",
and none of them can change history without explicit approval.

## Phase 6 — Production hardening → 1.0

- Fuzzing (index, packfile and ref parsing through gix, journal parsing), crash-consistency tests
  for interrupted writes, and concurrency tests (Veyra and Git running at the same time)
- Documentation site, installers (winget, Homebrew, cargo), signed release binaries
- A 1.0 compatibility promise for CLI flags, `--json` schemas and journal format
