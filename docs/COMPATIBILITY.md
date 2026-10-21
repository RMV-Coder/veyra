# Compatibility

Veyra reads and writes ordinary Git repositories. A repository that Veyra has touched is a
repository Git can use, and the other way round. This page lists what that covers, what is tested,
and what is not supported yet.

## How compatibility is tested

* **Git is the oracle.** The end-to-end suite (`crates/veyra-cli/tests/cli.rs`) runs every
  workflow with the `veyra` binary and then checks the result with real Git: `git fsck --strict`,
  `git status --porcelain`, `git log`, `git rerere`, `git sparse-checkout list`, signature
  verification, and Git reading Veyra's pushes, submodules and worktrees.
* **Mixed use.** Tests interleave Git and Veyra commands in the same repository, including Git and
  Veyra running *at the same time* (`veyra_and_git_running_at_the_same_time_stay_consistent`).
* **Property tests and fuzzing** cover the parsers and encoders that see untrusted input
  (`mod properties` in each crate; `fuzz/`).

## Platforms

| Platform | Status |
|---|---|
| Windows 10/11 (x86_64, MSVC) | Tested: CI on every push, and the author's daily use |
| Linux (x86_64, glibc) | Tested: CI on every push (Ubuntu) |
| macOS (Apple silicon) | Tested: CI on every push |
| Linux (aarch64), macOS (Intel) | Tested when a release is built: the release workflow runs the suite before building |

The CI workflow (`.github/workflows/ci.yml`) runs the whole suite, including the end-to-end tests
that use the runner's Git as the oracle. So far only Windows has seen daily use, so reports from
Linux and macOS users are very welcome.

Minimum supported Rust version for building from source: **1.88**.

## Git versions

| Git | Status |
|---|---|
| 2.52 (Windows) | Tested |
| Any Git ≥ 2.30 | Expected to work: Veyra writes only long-standing formats (index v2–v4, loose objects, packs v2 with `.idx` v2, packed and loose refs, reflogs) |

Git itself is **not** required to use Veyra. It's only needed to run the test suite, and for Git
hooks that call `git`.

## Repository features

| Feature | Status | Notes |
|---|---|---|
| SHA-1 repositories | ✅ | |
| SHA-256 repositories (`extensions.objectFormat`) | ✅ | `veyra init --object-format sha256` |
| Loose objects, packs, multi-pack directories | ✅ | Read via gitoxide; Veyra writes packs for large `add`s and `maintenance` |
| `commit-graph`, multi-pack-index | Read | Not written; `git maintenance` can still maintain them |
| Packed refs, loose refs, symbolic refs, reflogs | ✅ | |
| Reftable (`extensions.refStorage=reftable`) | ❌ | Not supported by gitoxide yet. Veyra refuses such repositories with the fix: `git refs migrate --ref-format=files` (Git 2.46+) |
| Index v2/v3/v4, split index off | ✅ | Split index and untracked cache are not written |
| `.gitattributes` filters (`text`, `eol`, `ident`, external drivers) | ✅ | Through gitoxide's filter pipeline |
| Git LFS | ✅ | The filter process stores pointers on `add`, the `pre-push` hook uploads content on `push`, and checkouts smudge it back (tested with git-lfs 3.7) |
| Sparse checkout (cone mode) | ✅ | Same files and skip-worktree bits as Git |
| Non-cone sparse patterns | ✅ | `veyra sparse set --no-cone`; gitignore-style patterns, last match wins, shared with Git |
| Linked worktrees | ✅ | |
| Submodules | ✅ | Cloned in the embedded layout; `git submodule absorbgitdirs` converts |
| Shallow clones | ✅ | `clone --depth`, `fetch --depth`, `fetch --unshallow`; `.git/shallow` is Git's |
| Partial clone (`--filter`) | ✅ | `blob:none`, `blob:limit=<n>`, `tree:<depth>`, configured as Git does (promisor remote, `.promisor` packs); missing objects are fetched on demand, and with `--scope` only the scope's files are downloaded. `veyra backfill` fetches the rest |
| Commit/tag signing: OpenPGP, X.509, SSH | ✅ | |
| Hooks (`core.hooksPath`, husky, lint-staged) | ✅ | `pre-commit`, `prepare-commit-msg`, `commit-msg`, `post-commit`, `post-checkout`, `post-merge`, `pre-rebase`, `post-rewrite` (after `--amend`), `pre-push` |
| `rerere` (`.git/rr-cache`) | ✅ | Shared with Git |
| Notes (`refs/notes/*`) | ✅ | Used for attestations |
| Merge strategies (`-s ours`, `-s subtree`, octopus) and options (`-X ours`, `-X theirs`, `-X subtree=<dir>`) | ✅ | An octopus merge with a conflict is refused before anything changes, as in Git |
| Operations in progress: merge, rebase (`.git/rebase-merge`), cherry-pick/revert sequences (`.git/sequencer`), bisect | ✅ | Started by either tool, continued or aborted by the other |
| Object verification | ✅ | `veyra doctor --full` rehashes every object, checks pack checksums and connectivity (like `git fsck --full`) |
| Transports: HTTPS, SSH, local paths, `file://` | ✅ | Fetch and clone via gitoxide; push via Veyra's receive-pack client |
| Git protocol v2 for fetch | ✅ | Push uses v0/v1, like Git |

## State Veyra adds

Veyra's own state never changes how Git sees the repository:

| Path | Git sees it as |
|---|---|
| `.git/veyra/` | An unknown directory (ignored) |
| `refs/veyra/keep`, `refs/veyra/keep-<id>` | Ordinary refs that keep journal objects alive; not pushed by default |
| `.veyra/` (committed) | Ordinary files |

Deleting `.git/veyra/` and `refs/veyra/*` turns the repository back into one that Veyra has
never seen: only the undo history is lost.
