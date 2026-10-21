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
| Windows 10/11 (x86_64, MSVC) | Tested on every change (CI and development) |
| Linux (x86_64, aarch64, glibc) | Tested in CI |
| macOS (Apple silicon, Intel) | Tested in CI |

Minimum supported Rust version for building from source: **1.88**.

## Git versions

| Git | Status |
|---|---|
| 2.52 (Windows), the versions on GitHub's Linux and macOS runners | Tested |
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
| Reftable (`extensions.refStorage=reftable`) | ❌ | Not supported by gitoxide yet |
| Index v2/v3/v4, split index off | ✅ | Split index and untracked cache are not written |
| `.gitattributes` filters (`text`, `eol`, `ident`, external drivers) | ✅ | Through gitoxide's filter pipeline |
| Git LFS | Untested | LFS uses a filter driver, which the pipeline runs; not covered by tests |
| Sparse checkout (cone mode) | ✅ | Same files and skip-worktree bits as Git |
| Non-cone sparse patterns | ❌ | Veyra refuses to change them |
| Linked worktrees | ✅ | |
| Submodules | ✅ | Cloned in the embedded layout; `git submodule absorbgitdirs` converts |
| Shallow clones | Read | `clone --depth` is not offered yet |
| Partial clone (`--filter`) | ❌ | Waiting for gitoxide support |
| Commit/tag signing: OpenPGP, X.509, SSH | ✅ | |
| Hooks (`core.hooksPath`, husky, lint-staged) | ✅ | |
| `rerere` (`.git/rr-cache`) | ✅ | Shared with Git |
| Notes (`refs/notes/*`) | ✅ | Used for attestations |
| Merge strategies other than the default three-way merge (`ours`, `octopus`, `subtree`) | ❌ | |
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
