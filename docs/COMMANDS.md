# Veyra Command Reference

This page maps Git commands to their Veyra equivalents and marks what is implemented today.

**Rule:** Git's spelling is canonical. Moving a script to Veyra should be close to `s/git/veyra/`.
New, clearer names exist only as aliases (see DESIGN.md §2, A5).

Legend: ✅ implemented and tested · 🟡 partial (see note) · 🗓 planned (phase in [ROADMAP.md](ROADMAP.md))

Veyra and Git share the same repository. Anything marked 🗓 can be done with `git` today, and
Veyra records it as an `external` operation, so `veyra undo` still works afterwards.

---

## Repository setup

| Git | Veyra | Status | Notes |
|---|---|---|---|
| `git init [dir]` | `veyra init [dir] [-b <branch>] [--object-format sha256]` | ✅ | Respects `init.defaultBranch`. SHA-256 repositories work end to end. |
| `git clone <url> [dir]` | `veyra clone <url> [dir]` | ✅ | HTTPS (rustls), SSH, `file://` and local paths through gitoxide |
| `git status` | `veyra status [--short] [--json]` | ✅ | `--short` output matches `git status --porcelain`. Paths are shown relative to the repository root. |
| `git config` | `veyra config [--global] <key> [<value>]`, `--unset`, `--list` | ✅ | Same files as Git |
| `git help` / `--version` | `veyra help`, `veyra --version` | ✅ | |
| `git clone --sparse` | `veyra clone <url> --scope <name>` / `--sparse <dir>…` | ✅ | Only the scope's directories are checked out (cone-mode sparse). Scopes are declared in `.veyra/scopes.toml`. gitoxide has no partial-clone filters yet, so all objects are still fetched. |
| `git sparse-checkout set/add/list/disable` | `veyra sparse set [dirs] [--scope s]` / `add` / `list` / `disable` | ✅ | Same `.git/info/sparse-checkout` and skip-worktree bits as Git (`git sparse-checkout list` agrees). Refuses to hide files with local changes. |

## Staging and committing

| Git | Veyra | Status | Notes |
|---|---|---|---|
| `git add <paths>` / `.` | `veyra add <paths>` (alias `stage`) | ✅ | Globs (`*.rs`) work even in PowerShell. Stages deletions, like Git 2.x. |
| `git add -A` / `-u` | `veyra add -A` / `-u` | ✅ | |
| `git add -f` | `veyra add --force` | ✅ | Needed for ignored files |
| `git add -p` | `veyra add -p [paths]` | ✅ | `y`/`n`/`q`/`a`/`d`/`s` (split) per hunk, deletions and binary files as a whole. No hunk editing (`e`) yet. |
| `git rm [--cached] [-r] [-f]` | `veyra rm` (alias `remove`) | ✅ | Refuses files with changes unless `-f` |
| `git mv` | `veyra mv` (alias `move`) | ✅ | |
| `git restore <paths>` | `veyra restore <paths>` / `veyra discard <paths>` | ✅ | **Difference:** the old contents are checkpointed first, so `veyra undo` recovers them |
| `git restore --staged` | `veyra restore --staged` / `veyra unstage` | ✅ | `--source`, `--worktree` supported |
| `git commit -m … [-a] [--amend] [--allow-empty]` | `veyra commit …` | ✅ | `-F file`, `--no-edit`, or the editor (`GIT_EDITOR` / `core.editor`) |
| `git commit --trailer` | `veyra commit --trailer "K: v"` | ✅ | |
| — | `veyra commit --assisted-by "<tool>"` | ✅ | Adds `Assisted-by:`. Provenance is declared, never guessed. |
| — | `veyra commit --intent <id>` | ✅ | Adds `Intent:` |
| — | secret scan + intent check on commit | ✅ | `--allow-secrets`, `--no-verify` |
| `git commit -S` / `commit.gpgSign` | `veyra commit -S` / `--no-gpg-sign` | ✅ | OpenPGP, X.509 or SSH per `gpg.format` / `user.signingKey` (via gitoxide). Merges, cherry-picks and rebases are signed too when `commit.gpgSign` is set. |
| Git hooks | `pre-commit`, `prepare-commit-msg`, `commit-msg`, `post-commit`, `pre-push`, `post-checkout`, `post-merge` | ✅ | `core.hooksPath` respected. Hooks run through Git for Windows' `sh` on Windows. Never run in repositories you don't own. `--no-verify` skips them. |
| — | `veyra commit --smart` / `--split` [`--dry-run`] | ✅ | Optional AI (see `veyra ai`). `--smart` proposes a message you accept, edit or reject; `--split` proposes a sequence of commits for the staged files, validated to cover every staged file exactly once before you see it. Each commit is a normal, undoable commit with an `Assisted-by:` trailer, and hooks and secret scanning still run. |

## History and inspection

| Git | Veyra | Status | Notes |
|---|---|---|---|
| `git log [revs] [-n N] [--oneline] [--all]` | `veyra log …` | ✅ | Decorations. `--json`. `--trailer Intent=<id>`. `--all` hides `refs/veyra/*`. |
| `git log --graph` | `veyra log --graph` | ✅ | Topological order, lane rendering like Git's |
| `git show [rev]` | `veyra show [rev] [--stat]` | ✅ | |
| `git diff` / `--staged` / `<rev>` / `<a> <b>` / `a..b` | `veyra diff …` | ✅ | `--stat`, `--name-only`, `--name-status`, `-U<n>`, `-- <paths>`. Hunks match `git diff`. |
| `git blame [-L a,b]` | `veyra blame <file> [-L a,b] [--rev R] [--json]` | ✅ | Line attribution matches `git blame` |
| `git reflog` | `veyra op log` | ✅ | Records *every* operation, including staging and uncommitted files |
| `git shortlog`, `describe`, `grep`, `bisect` | same names | 🗓 2–3 | |
| — | `veyra diff --semantic` / `--functions` [`--json`] | ✅ | Functions, methods, classes and types that were added, removed, modified, had their signature changed, were only reformatted, or renamed (renames need a token-identical, non-trivial body). Rust, TypeScript/TSX, JavaScript, Python, Go, Java (Tree-sitter). Other files are listed as unsupported. |

## Branches

| Git | Veyra | Status | Notes |
|---|---|---|---|
| `git branch` / `-a` / `-v` | `veyra branch` / `branch list` | ✅ | `--json` |
| `git branch <name> [start]` | `veyra branch <name>` / `branch create <name>` | ✅ | A branch literally named `list`, `create`, … needs the `create` form |
| `git branch -d` / `-D` | `veyra branch -d/-D` / `branch delete [--force]` | ✅ | `veyra undo` restores deleted branches |
| `git branch -m` | `veyra branch -m` / `branch rename` | ✅ | Keeps the reflog and `branch.<name>.*` upstream config |
| — | `veyra branch stale [--days N]` | ✅ | Age + merged status |
| — | `veyra branch cleanup [--dry-run]` | ✅ | Deletes local branches fully merged into the default branch. `veyra undo` restores them. |
| — | `veyra branch compare <branch> [base]` | ✅ | Ahead/behind, merge base, changed files and a suggestion. `--json`. |
| — | `veyra branch graph` | ✅ | The branch tree by commit ancestry (not names), with ahead/behind counts |
| — | `veyra branch explain [name]` | ✅ | Base, ahead/behind, areas touched, predicted conflicts (in-memory merge, writes nothing), suggested action |
| `git switch <b>` / `-c` / `--detach` / `-` | `veyra switch …` | ✅ | Refuses to overwrite local changes. `--force` checkpoints first. Creates tracking branches from `origin/<b>`. |
| `git checkout <b>` / `-b` / `-- <paths>` | `veyra checkout …` | ✅ | Prefer `switch` and `restore` |

## Merging and rewriting

| Git | Veyra | Status | Notes |
|---|---|---|---|
| `git merge <rev> [--ff-only] [--no-ff] [-m]` | `veyra merge …` | ✅ | Fast-forward, clean 3-way merge, or conflicts written exactly like Git (markers + index stages + `MERGE_HEAD`) |
| `git merge --abort` / `--continue` | `veyra merge --abort` / `--continue` | ✅ | Abort replays the operation log |
| `git reset [--soft\|--mixed\|--hard] [rev]` | `veyra reset …` | ✅ | **Difference:** `--hard` checkpoints first |
| `git reset <paths>` | `veyra reset <paths>` / `unstage` | ✅ | |
| `git cherry-pick [-x] [-n] [-m N]` | `veyra cherry-pick` | ✅ | Keeps the original author. Conflicts leave `CHERRY_PICK_HEAD` like Git. `--continue` / `--abort`. |
| `git revert [-n] [-m N]` | `veyra revert` | ✅ | The safe way to reverse pushed commits. `--continue` / `--abort`. |
| `git rebase <upstream> [--onto X]` | `veyra rebase` | ✅ | Defaults to the branch's upstream. `--continue` / `--skip` / `--abort`. State lives in `.git/veyra/rebase-state`, so plain `git rebase --continue` doesn't know about it. `veyra undo` restores the pre-rebase branch. |
| `git rebase -i` | `veyra rebase -i <upstream>` | ✅ | `pick`, `reword`, `edit`, `squash`, `fixup`, `drop`, `exec`, `break` (and their one-letter forms), plus reordering. Uses `GIT_SEQUENCE_EDITOR` / `sequence.editor`, then the commit editor. |
| `git rerere` / `rerere forget` / `rerere clear` | `veyra merge resolutions [status\|list\|forget <path>\|clear\|enable [--auto-stage]\|disable]` (alias `veyra rerere …`) | ✅ | Git's own `.git/rr-cache` format and conflict ids: resolutions recorded by Veyra are replayed by Git and vice versa. Enabled by `rerere.enabled` (or an existing rr-cache). Replays apply to merges, cherry-picks, reverts, rebases and stash applications, slightly shifted conflicts included; `rerere.autoUpdate` stages them. Every replay is reported. |
| — | structural merge (automatic in `merge`, `pull`, `cherry-pick`, `revert`, `rebase`, `stash apply/pop`) | ✅ | When Git's line merge conflicts but the two sides changed *different* top-level symbols, Veyra merges the file symbol by symbol. It refuses on syntax errors, same-symbol edits, delete-vs-modify and reorders on both sides, and re-parses the result. Every resolution is printed, and recorded as a `Veyra-Structural-Merge:` trailer. `veyra.structuralMerge` = `true` · `stage` (resolve, then stop for review) · `false`. |
| — | `veyra merge <branch> --assist` / `veyra ai resolve [paths]` | ✅ | AI-proposed resolutions for the conflicts that remain after structural merge and rerere. Proposals that still contain markers or don't parse are discarded; the rest are shown as a diff with a confidence level. Accepting stages the file (journaled, so `veyra undo` restores the conflict). `--yes` accepts only *high*-confidence proposals; uncertain ones always need a person. |

## Undo, recovery and checkpoints (Veyra-only)

| Command | Status | What it does |
|---|---|---|
| `veyra undo [--dry-run] [--force] [--op <id>]` | ✅ | Undoes the latest operation: any Veyra command, or a change made with plain git. Touches only what that operation changed, and refuses if those things changed again (unless `--force`, which is itself undoable). |
| `veyra undo commit` | ✅ | The same, but checks that the last operation really is a commit (≈ `git reset --soft HEAD~1`) |
| `veyra redo [--force]` | ✅ | Re-applies the last undone operation |
| `veyra op log [-n N] [--all] [--json]` | ✅ | The operation journal, newest first, with `← undo` / `← redo` markers |
| `veyra op show <id>` | ✅ | Refs, HEAD, index and file changes of one operation |
| `veyra op restore <id> [--before]` | ✅ | Makes the whole repository match that moment |
| `veyra op prune --older-than 30d` | ✅ | Retention (the newest 20 are always kept) |
| `veyra checkpoint [-m msg]` | ✅ | Snapshots tracked and untracked, non-ignored files. No commit, index untouched, never pushed. |
| `veyra checkpoint list / diff <id> / restore <id> [paths]` | ✅ | `restore` takes a fresh checkpoint first |
| `veyra checkpoint enable / disable` | ✅ | Toggles `veyra.autoCheckpoint` (snapshot on *every* command) |
| `veyra checkpoint watch --interval 10m` | ✅ | Foreground timer, no daemon |
| `veyra checkpoint prune --older-than 14d` | ✅ | |
| `veyra stash [push] [-m] [-u] [--keep-index]` / `list` / `show [-p]` / `apply [--index]` / `pop` / `drop` / `clear` (aliases `shelve`, `unshelve`) | ✅ | Same `refs/stash` structure and reflog as Git: `git stash` and `veyra stash` see each other's entries. A conflicted pop keeps the entry. |
| `veyra clean -f [-d] [-n] [paths]` | ✅ | Like `git clean`, but the files are checkpointed first, so `veyra undo` brings them back. Ignored files are never removed (no `-x` yet). |

## Worktrees and submodules

| Git | Veyra | Status | Notes |
|---|---|---|---|
| `git worktree add/list/remove/prune` | `veyra worktree add <path> [branch] [-b new] [--detach]` / `list` / `remove [--force]` / `prune` | ✅ | Via gitoxide. Each linked worktree has its **own** operation log and keep ref, so `veyra undo` there only touches that worktree. |
| `git submodule status/init/update/add/foreach` | `veyra submodule [status]` / `init` / `update [--init] [--recursive]` / `add <url> [path]` / `foreach <cmd>` | ✅ | Submodules are cloned into their path (Git's embedded layout). Relative URLs resolve against `origin`. |

## Remotes

| Git | Veyra | Status | Notes |
|---|---|---|---|
| `git remote [-v]`, `add`, `remove` | `veyra remote …` | ✅ | |
| `git fetch [remote]` | `veyra fetch` | ✅ | |
| `git pull [--ff-only]` | `veyra pull` | ✅ | Fetch + fast-forward or merge |
| `git push [remote] [refspec…]` | `veyra push` | ✅ | Implemented on gix's transport (HTTPS, SSH, local). Supports `-u`, `--tags`, `--delete`, `+refspec`, `--force`, `--force-with-lease`, `--dry-run`, `--no-verify`. Credential helpers are used. Changed blobs are sent as deltas against their previous version (a thin pack, like `git push --thin`). |
| `git ls-remote` | `veyra remote refs` | 🗓 2 | |
| — | `veyra sync [--push]` | ✅ | Fetch, then fast-forward, or rebase onto the upstream if both sides moved. Refuses with uncommitted changes. Pushes only with `--push`. |

## Tags and releases

| Git | Veyra | Status | Notes |
|---|---|---|---|
| `git tag` / `<name> [rev]` / `-a -m` / `-s` / `-d` / `-f` | `veyra tag …` / `tag create\|delete\|list` | ✅ | `-s` (or `tag.gpgSign`) signs. Annotated tags have a tagger. |
| `git verify-commit` / `git verify-tag` | `veyra verify commit [rev]` / `veyra verify tag <name>` | ✅ | Uses Git's verification config (`gpg.ssh.allowedSignersFile`, `gpg.program`, …) |
| — | `veyra release notes / prepare / verify` | 🗓 3 | |

## Diagnostics, security and intent (Veyra-only)

| Command | Status | What it does |
|---|---|---|
| `veyra doctor [--json]` | ✅ | PASS/WARN/FAIL with evidence for identity, integrity, in-progress ops, storage, large files, secrets, branches, journal and compatibility. Exits non-zero on FAIL. |
| `veyra security scan [--staged \| --rev R] [--json]` | ✅ | Built-in and custom rules, redacted output, fingerprints. Exits non-zero on high-confidence findings. |
| `veyra security allow <fingerprint>` | ✅ | Allowlists a reviewed false positive in `.veyra/secrets.toml` |
| `veyra intent create --title … [--guard-path G --guard-contains T]` | ✅ | Writes `.veyra/intents/<id>.toml` |
| `veyra intent list / show / verify [--staged\|--rev] / close / log` | ✅ | `verify` exits non-zero on violations |
| `veyra explain <git command…>` | ✅ | What it does, risk, recovery, Veyra equivalent |
| `git fsck` | `veyra doctor` | 🟡 | doctor checks refs, commits and trees. Use `git fsck --full` for blobs. |
| `git count-objects -v` | `veyra storage stats [--json]` | ✅ | |
| `git gc` / `git maintenance run` | `veyra maintenance run [--prune-expire 2w]` | ✅ | Packs loose objects (Veyra's own pack writer, indexed by gitoxide), then prunes unreachable loose objects past the grace period. Reachability includes every ref, reflog entry, the index, in-progress state and Veyra's journal. Packs are never rewritten. |
| `git prune` | `veyra maintenance prune [--expire 2w] [-n]` | ✅ | |
| — | `veyra impact [--staged \| --commit R \| --branch [BASE]] [--tests] [--history N] [--json]` | ✅ | Changed symbols; files that import a changed file or mention a changed symbol (high / medium / low confidence, with the evidence for each); tests to run (tests among the dependents, tests named after changed files, in-file Rust tests); files usually changed together; a risk score with its reasons; and the analysis' limitations. Deterministic: no AI involved. |
| — | `veyra verify --history [--rev R] [-n N]` | ✅ | Signature status of every commit (good / bad / unsigned) |
| — | `veyra attest run [--sign] -- <cmd>` / `attest review [--reject] [--sign]` / `attest list` | ✅ | in-toto v1 Statements in Git notes (`refs/notes/veyra/attestations`), shareable with `veyra push origin refs/notes/veyra/attestations` |
| — | `veyra verify --provenance [--rev R]` | ✅ | Lists a commit's attestations and checks their signatures |
| — | `veyra ai [status]` / `ai configure [--provider anthropic\|command] [--model M] [--effort E] [--command CMD] [--global]` / `ai disable` | ✅ | Off by default. `anthropic` uses the Claude API (default model `claude-opus-5-5`; the key comes from `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN` and is never stored). `command` runs any local model: the request arrives as JSON on stdin, the answer is read from stdout. |
| — | `veyra ai explain [REV\|--staged]` / `ai review [scope]` / `ai summarize [RANGE]` / `ai risks [scope]` | ✅ | Explanations, reviews (findings with severity and location, `--json`), PR/release summaries, and risk assessments built on the deterministic `impact` report. Every request is scanned for secrets first (nothing is sent if one is found), and `--dry-run` prints exactly what would be sent. |
| — | `veyra intent suggest <paths> [--dry-run]` | ✅ | An AI-proposed intent with guards. Guards the current code doesn't satisfy are dropped before you see the proposal; you approve it before it's written. Verification stays deterministic. |
| — | `veyra analyze [--migration] [--infra] [scope] [--fail-on high\|medium\|low] [--json]` | ✅ | Deterministic risk rules for what a change *adds*: SQL (DROP/TRUNCATE, DELETE/UPDATE without WHERE, NOT NULL without default, blocking index builds, renames, type changes…), ORM migrations (Rails, Django, Alembic, Knex), Terraform (destroyed resources, open ingress, public buckets, wildcard IAM, disabled protection/encryption), Kubernetes (privileged, host namespaces, root, `:latest`, zero replicas), Dockerfiles and CI workflows. Repository rules and `disable = [...]` in `.veyra/analyzers.toml`. `--fail-on` makes it a CI gate. |

## Plumbing

Plumbing commands keep their Git names at the top level (hidden from the main help).

| Git | Veyra | Status |
|---|---|---|
| `git rev-parse <spec>` | `veyra rev-parse [--short]` | ✅ |
| `git cat-file -t/-s/-p` | `veyra cat-file` | ✅ |
| `git hash-object [-w]` | `veyra hash-object` | ✅ (applies the clean filter inside a worktree, like Git) |
| `git ls-files [-s]` | `veyra ls-files` | ✅ |
| `git show-ref` | `veyra show-ref` | ✅ |
| `write-tree`, `commit-tree`, `update-ref`, `for-each-ref`, `merge-base`, `pack-objects`, `bundle`, `archive`, `notes`, `apply`, `am`, `format-patch`, … | same names | 🗓 2–3 |

Full Git compatibility will be claimed only when a published conformance matrix, backed by
automated tests, supports it.

---

## Configuration reference (`veyra.*` keys in Git config)

| Key | Default | Meaning |
|---|---|---|
| `veyra.autoCheckpoint` | `false` | Also checkpoint on read-only commands (`status`, `diff`, `log`) |
| `veyra.checkpointMaxFileSize` | `20m` | Larger files are skipped by snapshots (and reported) |
| `veyra.checkpointExclude` | — | Multi-valued globs never snapshotted (beyond `.gitignore`) |
| `veyra.secretsPolicy` | `block` | `block` · `warn` · `off` for high-confidence findings on commit |
| `veyra.intentPolicy` | `warn` | `warn` · `block` · `off` for guard violations on commit |
| `veyra.ai.provider` | `off` | `anthropic` · `command` · `off` |
| `veyra.ai.model` | `claude-opus-5-5` | Claude API model |
| `veyra.ai.effort` | `medium` | `low` · `medium` · `high` · `xhigh` · `max` |
| `veyra.ai.command` | — | Local model command for `provider = command` |
| `veyra.ai.fallbacks` | `true` | Claude API server-side refusal fallbacks (`fallbacks: "default"`) |
| `veyra.ai.maxTokens` | `16000` | Output limit per request |
| `veyra.structuralMerge` | `true` | `true` · `stage` · `false`: resolve conflicts in different symbols automatically, resolve and stop for review, or never |

Committed, shared policy:

| File | Purpose |
|---|---|
| `.veyra/intents/*.toml` | Intent records (see DESIGN.md §5.2) |
| `.veyra/secrets.toml` | `allow = [fingerprints]`, `exclude_paths = [globs]`, `[[rule]]` custom patterns |

## Environment

| Variable | Effect |
|---|---|
| `VEYRA_AGENT=1` | Agent mode: destructive commands (`reset --hard`, `branch -D`, `switch --force`, `discard`/`restore`, `undo --force`, `op restore`, `checkpoint restore`) require `--yes` |
| `NO_COLOR` | Disables colour |
| `GIT_EDITOR`, `VISUAL`, `EDITOR` | Commit message editor (after `core.editor`), run like Git: `sh -c '<editor> "$@"'` |
| `GIT_SEQUENCE_EDITOR` | Todo-list editor for `rebase -i` (before `sequence.editor`) |

## Exit codes

`0` success · `1` error, failed check (`doctor` FAIL, `security scan` high-confidence finding,
`intent verify` violation) or merge conflicts · `2` usage error
