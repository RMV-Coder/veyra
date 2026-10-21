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
- **Code intelligence.** `veyra diff --semantic` lists the functions and types a change touched,
  conflicts in *different* functions merge on their own (and say so), `veyra impact` shows what a
  change can affect and which tests to run, with the evidence for each claim, and `veyra analyze`
  flags risky migrations and infrastructure changes.
- **Optional AI, off by default.** With the Claude API or a local model: `commit --smart`,
  `commit --split`, `merge --assist`, `ai review`. Everything is a proposal you approve, payloads
  are secret-scanned, and `--dry-run` shows exactly what would be sent.
- **`veyra explain reset --hard`** describes what a Git command does, how risky it is, and how to
  recover.
- **`veyra doctor`** checks repository health and reports PASS/WARN/FAIL with evidence.
- **Built for scripts and agents:** `--json` output, `--assisted-by` provenance trailers, and an
  agent mode (`VEYRA_AGENT=1`) that requires `--yes` for destructive commands.

Command names are Git's (`veyra add`, `veyra commit -m`, `veyra switch -c`, …). See
[docs/COMMANDS.md](docs/COMMANDS.md) for the full Git → Veyra mapping and implementation status.

## Status

**v1.0 — all six roadmap phases are complete.** Everyday local and remote workflows (commit,
branch, merge, rebase including `-i`, cherry-pick, revert, stash, blame, clone/fetch/pull/push,
hooks) are tested against real Git, together with the safety net, signing and attestations,
worktrees, submodules, sparse checkout, SHA-256 repositories, code intelligence and the optional AI
layer. What is and isn't supported is listed in [docs/COMPATIBILITY.md](docs/COMPATIBILITY.md);
what stays stable across 1.x is in [docs/STABILITY.md](docs/STABILITY.md). The remaining gaps
(partial clone, reftable, hunk-level `commit --split`) are tracked in [docs/ROADMAP.md](docs/ROADMAP.md).

## Install

From source (Rust 1.88+):

```bash
cargo install --path crates/veyra-cli
```

Release binaries for Linux, macOS and Windows are built by the release workflow when a version
tag is pushed, with SHA-256 checksums and build-provenance attestations. The install scripts
verify both:

```bash
curl -fsSL https://raw.githubusercontent.com/RMV-Coder/veyra/main/install/install.sh | sh
```

```powershell
irm https://raw.githubusercontent.com/RMV-Coder/veyra/main/install/install.ps1 | iex
```

Homebrew and winget manifests are in [packaging/](packaging/).

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
         ├── veyra-intent (why-records, guards)          ┐
         ├── veyra-security (secret detection)           │ pure: no repository access
         ├── veyra-analyze (migration / IaC rules)       │
         ├── veyra-ai (optional providers and prompts)   ┘
         └── veyra-core (repository facade) ── veyra-semantic (Tree-sitter)
                                            └── gitoxide (gix) ── .git/
```

- **Git's on-disk format is the only format.** Veyra-only state lives in `.git/veyra/` (local) and
  `.veyra/` (committed, reviewable policy).
- **gitoxide** provides objects, packs, refs, the index, filters, status, merge and fetch. Veyra
  adds porcelain and intelligence on top.
- **AI is never required.** Everything except the `veyra ai` features is deterministic and works
  offline.

Read [docs/DESIGN.md](docs/DESIGN.md) for the architecture and the reasoning behind each decision,
and [docs/VISION.md](docs/VISION.md) for the original idea and how each part of it was adopted.

## Development

```bash
cargo test --workspace     # unit, property and end-to-end tests (against real git)
cargo clippy --workspace --all-targets
cargo test --release -p veyra-cli --test perf -- --ignored   # 10k-file performance budgets
```

Fuzz targets for the parsers that see untrusted input are in [fuzz/](fuzz/) (cargo-fuzz, nightly).

The end-to-end tests in `crates/veyra-cli/tests/cli.rs` use `git` as the oracle: every repository
Veyra writes must pass `git fsck --strict` and agree with `git status`, `git log` and `git diff`.
See [CONTRIBUTING.md](CONTRIBUTING.md), including the **clean-room rule**: Git is GPL-2.0, so we
read its documentation and test its behaviour, but never port its source code.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
