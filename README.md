# Veyra

**Version control that understands your code.** Git-compatible, written in Rust.

Veyra is a version control system that reads and writes **standard Git repositories**. You can
use it alongside `git` in either order, push to GitHub or GitLab as usual, and stop using it at
any time. What it adds is a safety net Git doesn't have, plus tools that help you understand
changes.

```console
$ veyra reset --hard          # oops: four hours of uncommitted work
HEAD is now at 3c15442 feat: totals
hint: local changes were checkpointed first; `veyra undo` brings them back

$ veyra undo
Undid reset 131f50e9 reset --hard to 3c15442
  file     total.ts
```

## What's different

- **`veyra undo` works for everything.** Every command that changes the repository is journaled:
  commits, branch deletions, resets, merges, staging, even `git` commands you ran yourself. Undo
  reverses exactly what that operation changed, and refuses to overwrite anything you changed
  afterwards.
- **Automatic checkpoints.** Before any command touches your files, Veyra snapshots the working
  tree, including untracked files. `veyra checkpoint list` / `restore` brings back work that was
  never committed. Checkpoints are stored as Git objects, never pushed, and survive `git gc`.
- **Secret scanning on commit.** Staged additions are checked for API keys, tokens and private
  keys before they enter history. Findings are redacted, and false positives are allowlisted by
  fingerprint.
- **Intents.** Record *why* code exists (`veyra intent create`) along with deterministic guards
  such as "`src/payment/**` must still contain `idempotencyKey`". Veyra warns when a change, made
  by a person or an AI agent, would remove that mechanism.
- **`veyra explain reset --hard`** describes what a Git command does, how risky it is, and how to
  recover.
- **`veyra doctor`** checks repository health and reports PASS/WARN/FAIL with evidence.
- **Built for scripts and agents:** `--json` output, `--assisted-by` provenance trailers, and an
  agent mode (`VEYRA_AGENT=1`) that requires `--yes` for destructive commands.

Command names are Git's (`veyra add`, `veyra commit -m`, `veyra switch -c`, …). See
[docs/COMMANDS.md](docs/COMMANDS.md) for the full Git → Veyra mapping and implementation status.

## Status

**v0.5 — Phases 1–5 of 6 complete.** Everyday local and remote workflows work and are tested
against real Git: commit, branch, switch, merge, rebase, cherry-pick, revert, stash, clean, blame,
`log --graph`, clone/fetch/pull/**push**, and Git hooks (husky and lint-staged keep working), plus the
full safety net, plus `rebase -i`, `add -p`, `branch graph/explain`, delta-compressed pushes,
signed commits and tags with verification, attestations, worktrees, submodules, sparse checkout and
`clone --scope`, SHA-256 repositories, and `maintenance` (no `git gc` needed). Phase 4 adds code
intelligence: `diff --semantic`, structural merges of conflicts in different functions, `impact`
analysis with evidence, and rerere that shares Git's rr-cache. Phase 5 adds optional AI (off by
default; Claude API or a local model): `commit --smart/--split`, `merge --assist`, `ai review`, and
deterministic migration/infrastructure analyzers (`veyra analyze`). See [docs/ROADMAP.md](docs/ROADMAP.md).

## Install from source

Requires Rust 1.88+.

```bash
cargo install --path crates/veyra-cli
```

Or build it with `cargo build --release`; the binary is `target/release/veyra`.

## Quick start

```bash
veyra init my-project && cd my-project
veyra config --global user.name "Your Name"
veyra config --global user.email "you@example.com"

echo "# Hello" > README.md
veyra add .
veyra commit -m "feat: initial commit" --assisted-by "Claude Opus 5.5"

veyra checkpoint -m "before refactor"   # snapshot without committing
veyra op log                            # everything that happened
veyra undo                              # reverse the last operation
veyra doctor                            # health report
```

## How it's built

```text
veyra-cli ── veyra-ops (journal, undo, checkpoints)
         ├── veyra-intent (why-records, guards)        pure, no repository access
         ├── veyra-security (secret detection)         pure, no repository access
         └── veyra-core (repository facade) ── gitoxide (gix) ── .git/
```

- **Git's on-disk format is the only format.** Veyra-only state lives in `.git/veyra/` (local) and
  `.veyra/` (committed, reviewable policy).
- **gitoxide** provides objects, packs, refs, the index, filters, status, merge and fetch. Veyra
  adds porcelain and intelligence on top.
- **AI is never required.** All Phase 1 features are deterministic and work offline.

Read [docs/DESIGN.md](docs/DESIGN.md) for the architecture and the reasoning behind each decision,
and [docs/VISION.md](docs/VISION.md) for the original idea and how each part of it was adopted.

## Development

```bash
cargo test --workspace     # unit tests + end-to-end tests against real git
cargo clippy --workspace --all-targets
```

The end-to-end tests in `crates/veyra-cli/tests/cli.rs` use `git` as the oracle: every repository
Veyra writes must pass `git fsck --strict` and agree with `git status`, `git log` and `git diff`.
See [CONTRIBUTING.md](CONTRIBUTING.md), including the **clean-room rule**: Git is GPL-2.0, so we
read its documentation and test its behaviour, but never port its source code.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
