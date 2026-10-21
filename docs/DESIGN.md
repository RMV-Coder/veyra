# Veyra Design

> **Veyra** is version control that understands your code, written in Rust and compatible with Git.

This document is the engineering design for Veyra. It started from a brainstorm, summarised in
[VISION.md](VISION.md), about what a next-generation Git would look like. This document keeps what
held up, changes what didn't, and adds what the brainstorm missed. Every claim about
"implemented" behaviour can be checked against [COMMANDS.md](COMMANDS.md) and the test suite.

---

## 1. The non-negotiable rule

> **No intelligent feature may ever compromise repository integrity, deterministic core
> operations, or a developer's ability to recover their work.**

The rest of this document follows from that rule. Five consequences show up throughout:

1. **Git's on-disk format is the source of truth.** Veyra never stores code history anywhere Git
   can't read.
2. **Veyra and Git can be used on the same repository at any time, in either order.** There are no
   lock-in states and no "please only use veyra here" warnings.
3. **Before any command discards work, Veyra snapshots that work.** This covers `switch`,
   `reset --hard`, `restore`, `undo`, `checkpoint restore` and `op restore`.
4. **Every mutating command is journaled,** so every one of them can be undone.
5. **AI is never required, and it never decides on its own.** It proposes, a deterministic layer
   verifies, and a human approves.

---

## 2. Review of the original proposal: what changes before implementation

The brainstorm ([VISION.md](VISION.md)) is a good product vision. The changes below are the
engineering corrections that make it buildable and safe. Each is labelled **Keep**, **Adjust** or
**Add**.

### 2.1 Adjustments

| # | Original idea | Adjustment | Why |
|---|---|---|---|
| A1 | Two repository modes: Git-compatible and a "native" `.veyra/objects` store | **Git object format only.** No native store. Veyra-only state lives in `.git/veyra/` (local, never pushed). Shared, reviewable policy lives in a committed `.veyra/` directory. | Two storage formats means two of everything: two fsck implementations, two GC implementations, two sets of bugs. Git already has a SHA-256 transition. Interoperability *is* the product. If a native format is ever justified, a measured benchmark has to justify it first. |
| A2 | Implement the Git layers ourselves, or "evaluate gitoxide" | **Build on [gitoxide](https://github.com/GitoxideLabs/gitoxide) (`gix`)** for objects, packs, refs, index, filters, status, merge and fetch. Wrap it behind `veyra-core` so it can be replaced. Where gix has gaps (push), implement the missing pieces on gix's protocol crates and offer them upstream. | gix is pure Rust, MIT/Apache-licensed, and has years of work on Git edge cases. Rewriting packfile deltas and the index format adds risk and no user value. |
| A3 | 16 crates from day one | **5 crates now** (`core`, `ops`, `security`, `intent`, `cli`). Split further only when a boundary has proven itself. | Premature crate boundaries freeze the wrong APIs and slow compilation. |
| A4 | Tokio for async | **No async runtime in the core.** VCS work is disk-bound, and gix is blocking. Use threads where they help. | async/await adds complexity with no throughput gain for local operations. |
| A5 | Rename commands (`rm` → `remove`, `branch <n>` → `branch create <n>`, `stash` → `shelve`) and a second "modern" CLI (`save`, `stage`, `publish`) | **Git spellings are canonical and always accepted.** New names are aliases only. Interface B is dropped. | Muscle memory and existing scripts are worth more than nicer verbs. The original itself noted that `save` is ambiguous. Moving a script to Veyra should be close to `s/git/veyra/`. |
| A6 | Several recovery vocabularies: `undo --history`, `history operations`, `recover --list`, `recover <id>` | **One concept: the operation log.** The commands are `veyra undo`, `veyra redo`, `veyra op log`, `veyra op show` and `veyra op restore`. Checkpoints are operations too. | Three names for one idea is exactly the confusion Veyra is meant to remove. |
| A7 | `checkpoint enable` with a timer daemon | **Snapshot on every mutating Veyra command** (Jujutsu-style), optionally on read-only commands too (`veyra.autoCheckpoint`). Add a foreground `checkpoint watch --interval`. No background service by default. | Daemons are a support burden, especially as Windows services and on laptops. Command-time snapshots cover the dangerous moments, and `watch` covers long editing sessions. |
| A8 | `doctor` shows an "Overall Health: 87/100" score | **Per-check PASS / WARN / FAIL with evidence and a fix.** No composite score. | A single number suggests a precision the checks don't have. |
| A9 | Intent violations detected by analysis or AI | **Intents carry machine-checkable guards** (path glob plus a pattern that must remain). They are stored as files in the repository and reviewed in pull requests like code. | Deterministic, explainable and branch-aware. AI may *suggest* guards but never *decides* that something is a violation. |
| A10 | AI provenance records | **Standard commit trailers** (`Assisted-by:`, `Intent:`), declared by the tool or the human, never inferred. | Trailers already work on GitHub and GitLab and with `git interpret-trailers`. Guessing whether code "looks AI-written" is unreliable. |
| A11 | Build semantic diff and merge in-house | **Reuse first:** difftastic-style structural diffing on Tree-sitter. Before borrowing from mergiraf, check its license (GPL-3.0 can't be linked into an MIT/Apache crate). | These are hard research problems with good existing tools. |
| A12 | `veyra sync` does fetch, pull and push | **Specified precisely:** fetch, then fast-forward or rebase the current branch if the worktree is clean, and push only with `--push`. | "Do the right thing" commands surprise people at the worst moments. |
| A13 | `clone --scope frontend` with automatic dependency detection | **Scopes are declared** in `.veyra/scopes.toml` and map to Git's partial clone plus cone-mode sparse checkout. | Dependency inference belongs in impact analysis. Clone has to be predictable. |
| A14 | Recovery in Phase 3 | **Recovery is Phase 1.** The operation journal wraps every mutating command from the first release. | Adding a journal to dozens of commands later is much harder than building it in from the start. Recovery is also the feature that sets Veyra apart. |
| A15 | `git cat-file` → `veyra plumbing cat-file` | **Plumbing keeps its Git name at the top level** (`veyra cat-file`). It is hidden from the main help, and the help footer lists it. | Scripts port unchanged, and the help screen stays short. |
| A16 | Config in `.veyra/config.toml` | **Local settings are Git config keys** under `veyra.*` (`veyra.autoCheckpoint`, `veyra.secretsPolicy`, …). | One configuration system. `git config veyra.autoCheckpoint true` works. |

### 2.2 Additions

| # | Addition | Summary |
|---|---|---|
| B1 | **Prior-art study** | [Jujutsu](https://github.com/jj-vcs/jj) (operation log, working-copy snapshots, the closest prior art), Sapling (smartlog, undo), git-branchless (undo, smartlog), difftastic, mergiraf, Gitless (a study of simplified concepts). Veyra differs by being Git-format-only, safety-first and intent-aware. It learns from these projects instead of re-inventing them. |
| B2 | **Machine-readable output** | `--json` on the commands people script against, plus stable exit codes. Scripts, CI and AI agents are first-class users. |
| B3 | **Agent safety mode** | `VEYRA_AGENT=1`: every mutation is checkpointed, and destructive commands require an explicit `--yes`. AI coding agents are now heavy VCS users, and their mistakes have to be cheap to undo. |
| B4 | **Coexistence with Git** | At the start of every command, Veyra compares the repository to the end of its operation log. Changes made by plain `git` are recorded as `external` operations, so `veyra undo` can undo a `git commit` too. |
| B5 | **Differential conformance testing** | The integration tests use real Git as an oracle: `git fsck` must stay clean, and `git status`/`git log` must agree with Veyra. A compatibility matrix states exactly what is supported. |
| B6 | **Clean-room licensing policy** | Git is GPL-2.0, and Veyra is MIT OR Apache-2.0. Contributors may read Git's *documentation* (including `Documentation/technical/*`) and observe Git's *behaviour*, but must not translate Git's *source code*. See §8. |
| B7 | **Windows as a first-class platform** | CRLF and `core.autocrlf` through Git's own filter rules, case-insensitive filesystems, reserved NTFS names, long paths and missing symlinks. The primary development machine runs Windows. |
| B8 | **Untrusted-repository safety** | Never run hooks, filters or editors configured by a repository the user doesn't own (gix's trust model and `safe.directory`). Reject path attacks such as `.git` aliases and `..` (gix-validate). Secret scanning runs on commit by default. |
| B9 | **Snapshot-before-discard** | Any command that would overwrite worktree content snapshots it first (see §1). |
| B10 | **Remote-aware undo** | Undoing an operation whose commits were already pushed is refused unless `--force`, and Veyra suggests `revert` instead. Push operations are journaled. This is Phase 2, which is when push lands. |

---

## 3. Architecture

```text
┌──────────────────────────────────────────────────────────────────┐
│ veyra-cli        argument parsing, output (human / --json),      │
│                  orchestration, explain, doctor                  │
├───────────────┬───────────────┬──────────────────┬───────────────┤
│ veyra-ops     │ veyra-intent  │ veyra-security   │ (Phase 4+)    │
│ operation log │ intent files  │ secret scanner   │ semantic diff │
│ undo / redo   │ guards        │ allowlist        │ impact        │
│ checkpoints   │ verification  │                  │ AI providers  │
├───────────────┴───────────────┴──────────────────┴───────────────┤
│ veyra-core       repository facade: status, add, commit, refs,   │
│                  worktree read/write, flat trees, diff, merge    │
├──────────────────────────────────────────────────────────────────┤
│ gix (gitoxide)   objects · packs · refs · index · filters ·      │
│                  status · merge · fetch · config                 │
├──────────────────────────────────────────────────────────────────┤
│ .git/            a standard Git repository                       │
└──────────────────────────────────────────────────────────────────┘
```

### 3.1 Crates

| Crate | Responsibility | Depends on |
|---|---|---|
| `veyra-core` | `Repo` facade over gix. `FlatTree`, a path-to-entry map used to compare HEAD, the index, worktree snapshots and arbitrary trees uniformly. Worktree materialisation with smudge filters. Index construction. Porcelain primitives. | gix |
| `veyra-ops` | The operation journal, views, undo/redo, op restore, checkpoints and retention. | core |
| `veyra-security` | Secret detection rules, fingerprints and allowlists. Pure, with no repository access. | regex |
| `veyra-intent` | Intent records (TOML), guard evaluation and ID generation. Pure, with no repository access. | toml, globset, regex |
| `veyra-cli` | The `veyra` binary. | all of the above |

The pure crates (`security`, `intent`) take bytes and paths instead of repositories. That makes
them trivial to unit-test and to reuse in CI tools and editor plugins.

### 3.2 Where state lives

| Location | Contents | Shared? |
|---|---|---|
| `.git/objects`, `.git/refs`, `.git/index` | Normal Git data | Pushed as Git normally pushes |
| `.git/config`, `[veyra]` section | Local Veyra settings, e.g. `veyra.autoCheckpoint`. `veyra config` and `git config` read and write the same keys. | No |
| `.git/veyra/oplog.jsonl` | The operation journal, one JSON record per line | No |
| `.git/veyra/lock` | Journal lock | No |
| `refs/veyra/keep` | A commit that keeps snapshot trees, index trees and orphaned commits reachable so `git gc` won't prune them | Not pushed by default |
| `.veyra/intents/*.toml` | Intent records, committed and reviewed | Yes |
| `.veyra/secrets.toml` | Secret-scan allowlist and custom rules, committed | Yes |
| `.veyra/scopes.toml` | Named directory sets for `clone --scope` / `sparse set --scope`, committed | Yes |
| `refs/notes/veyra/attestations` | in-toto attestations about commits (Git notes) | Push explicitly |
| `.git/rr-cache/`, `.git/MERGE_RR` | Recorded conflict resolutions — Git's own format, shared with `git rerere` | No |
| `.git/worktrees/<id>/veyra/`, `refs/veyra/keep-<id>` | A linked worktree's own journal and keep ref | No |

Using Git config for local settings means one configuration system instead of two (there is no
`config.toml`).

> **Note on `git log --all`:** `refs/veyra/keep` is a real ref, so `git log --all` shows a single
> `veyra: keep` commit, plus any commits that only the journal still references, such as an undone
> commit (much like reflog entries). Use `git log --all --exclude='refs/veyra/*'` to hide them.
> `veyra log --all` excludes them automatically.

---

## 4. The operation log

The operation log is Veyra's central safety mechanism. It is inspired by Jujutsu's, but it is
stored entirely as Git objects plus one append-only file.

### 4.1 Model

A **view** is a full description of repository state at one moment:

```jsonc
{
  "head": { "symbolic": "refs/heads/main" },          // or { "detached": "<oid>" }
  "refs": { "refs/heads/main": "<oid>", "refs/tags/v1": "<oid>", ... },  // everything except refs/veyra/*
  "index_tree": "<tree oid>",       // stage-0 index entries written as a tree (dedupes subtrees)
  "index_conflicts": false,         // true → the raw index file is also stored, as index_blob
  "worktree": "<tree oid>"          // snapshot of tracked + untracked, non-ignored files
}
```

An **operation** is a pair of views plus metadata:

```jsonc
{
  "id": "7f3a9c2e01b4d8aa",
  "time": 1792483200,
  "command": "veyra commit -m 'feat: login'",
  "kind": "commit",                 // commit | switch | branch | reset | merge | checkpoint | undo | redo | external | ...
  "summary": "commit a8f42bc feat: login",
  "before": { ...view... },
  "after":  { ...view... },
  "target": null                    // for undo/redo: the id of the operation that was undone or redone
}
```

### 4.2 Recording

Every mutating command runs inside a transaction:

1. Take the journal lock (`.git/veyra/lock`, created exclusively, like Git's `.lock` files). A lock
   older than 10 minutes is treated as left over from a crash.
2. Capture the `before` view, including a worktree snapshot. This is the automatic checkpoint.
3. **Detect external changes.** If the `before` view's HEAD, refs, index or merge state differ from
   the last operation's `after` view, append an `external` operation first. That keeps undo correct
   after plain `git` commands.
4. Run the command.
5. Capture the `after` view, append the record, update `refs/veyra/keep`, and release the lock.
   Commands that can't change files (`commit`, `add`, `branch`, `tag`, …) reuse the `before`
   snapshot instead of scanning the worktree again.

If the command fails, nothing is appended. The `before` snapshot objects are already in the object
database, so they become ordinary garbage and are removed by GC.

**External operations carry no worktree snapshots.** Veyra can't tell what a `git` command did to
files apart from what the user edited, so undoing an external operation restores refs, HEAD and the
index. It rewrites files only when HEAD was switched to a different branch or commit
(`git checkout`/`git switch`), and then only files the user hasn't touched since. A branch that
merely moved (`git commit`, `git reset`) leaves files alone. That is the conservative choice, and it
never discards work.

### 4.3 Worktree snapshots

A snapshot is the tree that `git add -A` *would* produce, written without touching the index:

* It starts from the index's stage-0 entries.
* Modified and untracked files (found by gix's stat-cached status) are hashed **through the clean
  filter**, so CRLF handling matches Git exactly.
* Ignored files are excluded, and so are paths matching `veyra.checkpointExclude`.
* Files larger than `veyra.checkpointMaxFileSize` (default 20 MiB) are skipped with a warning.
* When nothing has changed, the snapshot *is* the index tree and costs only a status scan.

Restoring a snapshot writes files **through the smudge filter**, which is the same path Git's own
checkout uses.

### 4.4 Undo and redo: path-wise three-way restore

Undoing operation **X** must not clobber work done after X. Veyra uses one generic primitive,
applied to refs, index entries and worktree paths alike:

```text
for each key k where X.before[k] ≠ X.after[k]:
    if current[k] == X.after[k]:  set current[k] := X.before[k]
    elif current[k] == X.before[k]: nothing to do
    else: CONFLICT(k)             # changed again since X: refuse unless --force
```

Redo is the same function with `before` and `after` swapped. Because the undo is itself recorded
as an operation, and its `before` view includes a fresh snapshot, even `undo --force` can be
undone.

**Which operation does `veyra undo` target?** Walk the log chronologically while keeping an undo
stack. `undo(X)` pushes X and `redo(X)` pops it. Any other undoable operation clears the stack,
the same as the redo stack in a text editor. The undo target is the latest undoable operation that
is not currently undone. The redo target is the top of the stack. Repeated `veyra undo` therefore
walks back through history instead of toggling.

`veyra op restore <id>` is the blunt instrument: it makes the whole repository (refs, HEAD, index
and worktree) match that operation's `after` view. Current state is snapshotted first.

### 4.5 Retention and garbage collection

`refs/veyra/keep` points at one synthetic commit:

* **Tree:** `ops/<id[0..2]>/<id>/{before,after}-{worktree,index}` entries, which keep every
  retained snapshot reachable. The two-level fan-out keeps per-operation rewrites small.
* **Parents:** commits that an operation **orphaned**, meaning a ref moved from A to B where A is
  not an ancestor of B, or a ref was deleted. These are exactly the commits a user might want back.

`veyra op prune --older-than 14d` (and `checkpoint prune`) drops old records and rebuilds the keep
commit. After that, `git gc` reclaims the space.

---

## 5. Other Phase-1 subsystems

### 5.1 Secret scanning (`veyra-security`)

* Rules are regular expressions with an ID, a description and a confidence level: private-key
  headers, AWS, GitHub, GitLab, Slack, Stripe, Google, OpenAI and Anthropic key formats, JWTs, and
  generic `password = "…"` assignments.
* `veyra commit` scans **only the added lines** of staged text blobs. High-confidence findings
  block the commit when `veyra.secretsPolicy=block` (the default). Lower-confidence findings only
  warn.
* False positives are allowlisted by **fingerprint** (a hash of rule, path and matched text) in the
  committed `.veyra/secrets.toml`, so the allowlist itself never contains the secret.
* `veyra security scan` scans the worktree, the index or a revision.
* Secret scanning is a safety net, not a guarantee. It complements dedicated scanners and does not
  replace them.

### 5.2 Intents (`veyra-intent`)

An intent records *why* code exists, as a reviewed file:

```toml
# .veyra/intents/int-8a92c1.toml
id = "int-8a92c1"
title = "Prevent duplicate payment processing"
kind = "bugfix"
status = "active"
created = "2026-10-20"
objective = "Concurrent requests must not create duplicate payment records."
solution = "Idempotency keys on payment creation."
constraints = ["Maintain backward compatibility"]
related = ["PAY-142"]
verification = ["Concurrent payment tests"]

[[guard]]
paths = ["src/payment/**"]
must_contain = "idempotencyKey"
message = "Idempotency validation must remain in the payment service"
```

* `veyra commit --intent int-8a92c1` adds an `Intent: int-8a92c1` trailer.
* `veyra intent verify` evaluates every active intent's guards against the worktree, the index
  (`--staged`) or a revision. `veyra commit` runs it on the index and warns, or blocks when
  `veyra.intentPolicy=block`.
* Guards are deterministic and explainable, and they live on branches like code. Superseding an
  intent is an ordinary reviewed change to its file.

### 5.3 `veyra explain`

`veyra explain <git or veyra command…>` uses a built-in knowledge base to describe what a command
does, how dangerous it is, how to recover from it, and what the Veyra equivalent is. For example,
`veyra explain reset --hard`. Explanations match on the longest known command and flag prefix.

### 5.4 `veyra doctor`

Each check reports PASS, WARN or FAIL with evidence and a suggested command. The checks cover:
integrity (refs resolve, commits and trees exist along the history, the index checksum), storage
(loose-object count, pack count and sizes), large blobs in HEAD, secrets in HEAD, stale and merged
branches, Git operations in progress, journal health, and identity and line-ending configuration.

---

## 6. Git compatibility strategy

* **Read compatibility:** anything gix can read (all current Git repository formats with SHA-1
  objects; SHA-256 as gix matures).
* **Write compatibility:** every object, ref and index Veyra writes must pass `git fsck` and leave
  `git status` consistent. The integration tests enforce this.
* **Behavioural compatibility:** porcelain commands follow Git's semantics where Veyra keeps the
  Git name. Where Veyra deliberately differs (for example, `switch` snapshots before acting),
  [COMMANDS.md](COMMANDS.md) documents the difference.
* **Honesty:** Veyra does not claim "full Git compatibility" until a published conformance matrix
  backed by automated tests says so.

---

## 7. Non-goals (for now)

* A native, non-Git object format (see A1).
* A hosting server or forge.
* Replacing dedicated security scanners, CI systems or code-review tools.
* Mandatory AI features.

---

## 8. Clean-room and licensing policy

Veyra is dual-licensed **MIT OR Apache-2.0**, following the Rust ecosystem convention.

| Source | May we… | Notes |
|---|---|---|
| Git documentation, including `Documentation/technical/*` format specs | Read and implement from it | Specifications of file formats and protocols are what interoperability requires |
| Git's observable behaviour | Test against it and match it | Differential testing is encouraged |
| Git's C source code (GPL-2.0) | **Not translate or port it** | A line-by-line translation would be a derivative work and would force GPL onto Veyra |
| gitoxide (MIT/Apache-2.0) | Depend on it, read it, contribute to it | Compatible license |
| mergiraf (GPL-3.0) | Learn from published ideas only | Not linkable into Veyra |
| difftastic (MIT), Jujutsu (Apache-2.0) | Read; reuse code with attribution | Compatible licenses |

If a contributor has studied a specific Git C function closely, they should describe the behaviour
in a test and let someone else implement it.
