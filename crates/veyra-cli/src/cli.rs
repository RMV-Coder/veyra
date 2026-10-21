//! Command-line definitions. Git spellings are canonical; Veyra names are aliases.

use clap::{Args, Parser, Subcommand};

const AFTER_HELP: &str = "\
Veyra keeps Git's command names: `veyra <git-command>` works for everything listed here.
Every mutating command is journaled: `veyra undo` reverses it, `veyra op log` shows history.

Plumbing commands keep their git names but are hidden here: rev-parse, cat-file, hash-object, ls-files, show-ref.
Docs: docs/COMMANDS.md (git → veyra mapping), docs/DESIGN.md.";

#[derive(Parser, Debug)]
#[command(name = "veyra", version, about = "Version control that understands your code. Git-compatible, written in Rust.", after_help = AFTER_HELP)]
pub struct Cli {
    /// Run as if started in <path>.
    #[arg(short = 'C', global = true, value_name = "path")]
    pub dir: Option<std::path::PathBuf>,
    /// Machine-readable JSON output (supported by status, log, branch, op log, checkpoint list, doctor, security, intent).
    #[arg(long, global = true)]
    pub json: bool,
    /// Confirm destructive actions (required in agent mode, VEYRA_AGENT=1).
    #[arg(long, short = 'y', global = true)]
    pub yes: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    // ----- repository -----
    /// Create an empty repository.
    Init {
        directory: Option<std::path::PathBuf>,
        /// Name of the initial branch (default: init.defaultBranch, else `main`).
        #[arg(short = 'b', long = "initial-branch")]
        initial_branch: Option<String>,
        /// Object id format: sha1 (default, maximally compatible) or sha256.
        #[arg(long, value_name = "sha1|sha256")]
        object_format: Option<String>,
    },
    /// Clone a repository into a new directory.
    Clone {
        url: String,
        directory: Option<std::path::PathBuf>,
        /// Check out only this scope from the repository's .veyra/scopes.toml.
        #[arg(long)]
        scope: Option<String>,
        /// Check out only these directories (cone-mode sparse checkout).
        #[arg(long, value_name = "dir")]
        sparse: Vec<String>,
    },
    /// Show the working tree status.
    Status {
        /// Short format (`XY path`).
        #[arg(short, long)]
        short: bool,
    },

    // ----- staging & committing -----
    /// Stage file contents (also stages deletions).
    #[command(visible_alias = "stage")]
    Add {
        paths: Vec<String>,
        /// Stage all changes, including untracked files.
        #[arg(short = 'A', long)]
        all: bool,
        /// Stage modifications and deletions of tracked files only.
        #[arg(short = 'u', long)]
        update: bool,
        /// Allow adding ignored files.
        #[arg(short, long)]
        force: bool,
        /// Choose hunks to stage interactively.
        #[arg(short, long)]
        patch: bool,
    },
    /// Remove files from the index and the working tree.
    #[command(visible_alias = "remove")]
    Rm {
        #[arg(required = true)]
        paths: Vec<String>,
        /// Only remove from the index; keep the file.
        #[arg(long)]
        cached: bool,
        #[arg(short, long)]
        force: bool,
        #[arg(short)]
        recursive: bool,
    },
    /// Move or rename a tracked file.
    #[command(visible_alias = "move")]
    Mv {
        source: String,
        destination: String,
        #[arg(short, long)]
        force: bool,
    },
    /// Restore working tree files (or, with --staged, index entries).
    Restore(RestoreArgs),
    /// Discard local changes to files (alias for `restore <paths>`).
    Discard {
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Unstage files (alias for `restore --staged <paths>`).
    Unstage {
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Record changes to the repository.
    Commit(CommitArgs),

    // ----- history -----
    /// Show commit history.
    Log(LogArgs),
    /// Show a commit and its changes.
    Show {
        revision: Option<String>,
        #[arg(long)]
        stat: bool,
    },
    /// Show changes between the worktree, index and commits.
    Diff(DiffArgs),
    /// Show who last changed each line of a file.
    Blame {
        file: String,
        /// Blame as of this revision (default: HEAD).
        #[arg(long)]
        rev: Option<String>,
        /// Only lines in this range, e.g. -L 10,20.
        #[arg(short = 'L', value_name = "start,end")]
        lines: Option<String>,
    },

    // ----- branches -----
    /// List, create, delete or rename branches.
    Branch(BranchArgs),
    /// Switch branches.
    Switch {
        branch: Option<String>,
        /// Create a new branch and switch to it.
        #[arg(short = 'c', long = "create", value_name = "new-branch")]
        create: Option<String>,
        /// Detach HEAD at the given commit.
        #[arg(short, long)]
        detach: bool,
        /// Overwrite local changes (they are checkpointed first).
        #[arg(short, long, alias = "discard-changes")]
        force: bool,
    },
    /// Switch branches, or restore files with `checkout -- <paths>`.
    Checkout {
        target: Option<String>,
        /// Create a new branch.
        #[arg(short = 'b')]
        new_branch: Option<String>,
        #[arg(short, long)]
        force: bool,
        #[arg(last = true)]
        paths: Vec<String>,
    },
    /// Join histories together.
    Merge {
        revision: Option<String>,
        #[arg(long)]
        ff_only: bool,
        #[arg(long)]
        no_ff: bool,
        #[arg(short, long)]
        message: Option<String>,
        /// Abort the in-progress merge.
        #[arg(long)]
        abort: bool,
        /// Conclude the merge after resolving conflicts (same as `commit`).
        #[arg(long = "continue")]
        continue_: bool,
    },
    /// Move HEAD (and optionally the index and worktree) to a commit, or unstage paths.
    Reset {
        #[arg(long, group = "mode")]
        soft: bool,
        #[arg(long, group = "mode")]
        mixed: bool,
        #[arg(long, group = "mode")]
        hard: bool,
        /// A commit, optionally followed by paths (after `--`) to unstage.
        target: Option<String>,
        #[arg(last = true)]
        paths: Vec<String>,
    },
    /// Create, list or delete tags.
    Tag(TagArgs),
    /// Apply the changes of existing commits onto the current branch.
    #[command(name = "cherry-pick")]
    CherryPick(SequenceArgs),
    /// Create commits that reverse existing commits (safe on shared branches).
    Revert(SequenceArgs),
    /// Replay the current branch's commits on top of another base.
    Rebase {
        /// Commits not in <upstream> are replayed (default: the branch's upstream).
        upstream: Option<String>,
        /// Replay onto this commit instead of <upstream>.
        #[arg(long)]
        onto: Option<String>,
        /// Edit the list of commits first: reorder, reword, edit, squash, fixup or drop them.
        #[arg(short, long)]
        interactive: bool,
        #[arg(long = "continue", group = "seq")]
        continue_: bool,
        #[arg(long, group = "seq")]
        skip: bool,
        #[arg(long, group = "seq")]
        abort: bool,
    },

    // ----- remotes -----
    /// Manage remotes.
    Remote(RemoteArgs),
    /// Download objects and refs from a remote.
    Fetch { remote: Option<String> },
    /// Fetch and integrate the current branch's upstream.
    Pull {
        remote: Option<String>,
        branch: Option<String>,
        #[arg(long)]
        ff_only: bool,
    },
    /// Update remote refs along with associated objects.
    Push(PushArgs),
    /// Fetch, then fast-forward or rebase the current branch onto its upstream (push only with --push).
    Sync {
        /// Also push the result.
        #[arg(long)]
        push: bool,
    },

    // ----- safety net -----
    /// Undo the last operation (any Veyra command, or a change made with plain git).
    Undo {
        /// Undo a specific operation instead of the latest.
        #[arg(long, value_name = "op-id")]
        op: Option<String>,
        /// Overwrite changes made after that operation (they are checkpointed first).
        #[arg(short, long)]
        force: bool,
        /// Show what would change without changing anything.
        #[arg(short = 'n', long)]
        dry_run: bool,
        /// `veyra undo commit`: same as `veyra undo` when the last operation is a commit.
        what: Option<String>,
    },
    /// Redo the last undone operation.
    Redo {
        #[arg(short, long)]
        force: bool,
    },
    /// Remove untracked files (checkpointed first, so `veyra undo` brings them back).
    Clean {
        /// Required unless --dry-run (like git's clean.requireForce).
        #[arg(short, long)]
        force: bool,
        /// Only show what would be removed.
        #[arg(short = 'n', long)]
        dry_run: bool,
        /// Also remove files in untracked directories.
        #[arg(short)]
        d: bool,
        /// Limit to these paths.
        paths: Vec<String>,
    },
    /// Set local changes aside (git-compatible refs/stash) and restore them later.
    Stash(StashArgs),
    /// Stash local changes (alias for `stash push`).
    Shelve {
        #[arg(short, long)]
        message: Option<String>,
        #[arg(short = 'u', long)]
        include_untracked: bool,
    },
    /// Re-apply and drop the latest stash (alias for `stash pop`).
    Unshelve { entry: Option<String> },
    /// Inspect and restore the operation log.
    #[command(name = "op", alias = "operation")]
    Op(OpArgs),
    /// Snapshot the working tree without committing; list and restore checkpoints.
    Checkpoint(CheckpointArgs),

    // ----- intelligence & diagnostics -----
    /// Verify signatures of commits and tags.
    Verify(VerifyArgs),
    /// Record verifiable evidence about commits (in-toto statements in git notes).
    Attest {
        #[command(subcommand)]
        command: AttestCommand,
    },
    /// Restrict the working tree to some directories (Git's cone-mode sparse checkout).
    Sparse {
        #[command(subcommand)]
        command: SparseCommand,
    },
    /// Initialize, update or inspect submodules.
    Submodule {
        #[command(subcommand)]
        command: Option<SubmoduleCommand>,
    },
    /// Manage multiple working trees attached to this repository.
    Worktree {
        #[command(subcommand)]
        command: WorktreeCommand,
    },
    /// Repository storage information.
    Storage {
        #[command(subcommand)]
        command: StorageCommand,
    },
    /// Keep the repository fast and small (packing, pruning) — no `git gc` needed.
    Maintenance {
        #[command(subcommand)]
        command: MaintenanceCommand,
    },
    /// Explain what a git or veyra command does, how dangerous it is, and how to recover.
    Explain {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Check repository health.
    Doctor,
    /// Scan for committed or staged secrets.
    Security(SecurityArgs),
    /// Record and verify *why* code exists.
    Intent(IntentArgs),
    /// Get and set configuration (same files as `git config`).
    Config(ConfigArgs),

    // ----- plumbing (hidden from the main help) -----
    #[command(hide = true, name = "rev-parse")]
    RevParse {
        #[arg(required = true)]
        specs: Vec<String>,
        #[arg(long)]
        short: bool,
    },
    #[command(hide = true, name = "cat-file")]
    CatFile {
        #[arg(short = 't', group = "what")]
        kind: bool,
        #[arg(short = 's', group = "what")]
        size: bool,
        #[arg(short = 'p', group = "what")]
        pretty: bool,
        object: String,
    },
    #[command(hide = true, name = "hash-object")]
    HashObject {
        #[arg(short = 'w')]
        write: bool,
        #[arg(required = true)]
        files: Vec<std::path::PathBuf>,
    },
    #[command(hide = true, name = "ls-files")]
    LsFiles {
        #[arg(short = 's', long)]
        stage: bool,
    },
    #[command(hide = true, name = "show-ref")]
    ShowRef,
}

#[derive(Args, Debug)]
pub struct RestoreArgs {
    #[arg(required = true)]
    pub paths: Vec<String>,
    /// Restore the index instead of the working tree.
    #[arg(short = 'S', long)]
    pub staged: bool,
    /// With --staged, also restore the working tree.
    #[arg(short = 'W', long)]
    pub worktree: bool,
    /// Restore from this commit instead of the index (or HEAD for --staged).
    #[arg(short, long)]
    pub source: Option<String>,
}

#[derive(Args, Debug)]
pub struct CommitArgs {
    /// Commit message (multiple -m become separate paragraphs).
    #[arg(short, long)]
    pub message: Vec<String>,
    /// Read the message from a file.
    #[arg(short = 'F', long)]
    pub file: Option<std::path::PathBuf>,
    /// Stage modified and deleted tracked files first.
    #[arg(short, long)]
    pub all: bool,
    /// Replace the last commit.
    #[arg(long)]
    pub amend: bool,
    /// With --amend, keep the previous message.
    #[arg(long)]
    pub no_edit: bool,
    #[arg(long)]
    pub allow_empty: bool,
    /// Add a trailer, e.g. --trailer "Reviewed-by: A <a@b>".
    #[arg(long, value_name = "key: value")]
    pub trailer: Vec<String>,
    /// Declare AI assistance (adds an `Assisted-by:` trailer).
    #[arg(long, value_name = "tool")]
    pub assisted_by: Option<String>,
    /// Link the commit to an intent (adds an `Intent:` trailer).
    #[arg(long, value_name = "intent-id")]
    pub intent: Vec<String>,
    /// Commit even if the secret scan finds high-confidence secrets.
    #[arg(long)]
    pub allow_secrets: bool,
    /// Skip hooks, the secret scan and intent verification.
    #[arg(long)]
    pub no_verify: bool,
    /// Sign the commit (gpg, x509 or ssh, per `gpg.format`).
    #[arg(short = 'S', long = "gpg-sign")]
    pub gpg_sign: bool,
    /// Don't sign, even if `commit.gpgSign` is set.
    #[arg(long)]
    pub no_gpg_sign: bool,
}

#[derive(Args, Debug)]
pub struct LogArgs {
    /// Revisions to start from (default: HEAD).
    pub revisions: Vec<String>,
    #[arg(long)]
    pub oneline: bool,
    #[arg(short = 'n', long = "max-count")]
    pub max_count: Option<usize>,
    /// Start from all refs.
    #[arg(long)]
    pub all: bool,
    /// Draw the history as a graph.
    #[arg(long)]
    pub graph: bool,
    /// Show only commits with this trailer, e.g. --trailer Intent=int-8a92c1.
    #[arg(long, value_name = "key=value")]
    pub trailer: Option<String>,
}

#[derive(Args, Debug)]
pub struct DiffArgs {
    /// Zero, one or two revisions.
    pub revisions: Vec<String>,
    /// Compare HEAD with the index.
    #[arg(long, alias = "cached")]
    pub staged: bool,
    #[arg(long)]
    pub stat: bool,
    #[arg(long)]
    pub name_only: bool,
    #[arg(long)]
    pub name_status: bool,
    #[arg(short = 'U', long = "unified", default_value_t = 3)]
    pub context: usize,
    /// Show which functions, methods and types changed (Tree-sitter), not lines.
    #[arg(long, visible_alias = "functions")]
    pub semantic: bool,
    #[arg(last = true)]
    pub paths: Vec<String>,
}

#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct BranchArgs {
    #[command(subcommand)]
    pub command: Option<BranchCommand>,
    /// Create a branch with this name (git style), or list when omitted.
    pub name: Option<String>,
    /// Start point for a new branch.
    pub start: Option<String>,
    /// Delete a merged branch.
    #[arg(short = 'd', long = "delete")]
    pub delete: bool,
    /// Delete a branch even if unmerged.
    #[arg(short = 'D')]
    pub force_delete: bool,
    /// Rename a branch: `branch -m <old> <new>` or `branch -m <new>`.
    #[arg(short = 'm', long = "move")]
    pub rename: bool,
    /// List remote-tracking branches too.
    #[arg(short = 'a', long)]
    pub all: bool,
    /// Show the tip commit of each branch.
    #[arg(short = 'v', long)]
    pub verbose: bool,
}

#[derive(Subcommand, Debug)]
pub enum BranchCommand {
    /// List branches.
    List {
        #[arg(short, long)]
        all: bool,
    },
    /// Create a branch.
    Create { name: String, start: Option<String> },
    /// Delete a branch.
    Delete {
        name: String,
        #[arg(short, long)]
        force: bool,
    },
    /// Rename a branch.
    Rename { old: String, new: String },
    /// Show how branches relate (by commit ancestry, not by name).
    Graph,
    /// Explain a branch: its base, how far it diverged, what it touches, and likely conflicts.
    Explain { name: Option<String> },
    /// Delete local branches that are fully merged into the default branch (undoable).
    Cleanup {
        /// Only list what would be deleted.
        #[arg(short = 'n', long)]
        dry_run: bool,
    },
    /// Compare two branches: commits ahead/behind and files that differ.
    Compare {
        /// The branch to compare.
        branch: String,
        /// The branch to compare against (default: the current branch).
        base: Option<String>,
    },
    /// Branches with no commits for a while, and whether they are merged.
    Stale {
        #[arg(long, default_value_t = 90)]
        days: i64,
    },
}

#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct TagArgs {
    #[command(subcommand)]
    pub command: Option<TagCommand>,
    pub name: Option<String>,
    pub target: Option<String>,
    #[arg(short = 'a', long)]
    pub annotate: bool,
    #[arg(short, long)]
    pub message: Option<String>,
    #[arg(short = 'd', long)]
    pub delete: bool,
    #[arg(short, long)]
    pub force: bool,
    /// Make a signed, annotated tag.
    #[arg(short = 's', long)]
    pub sign: bool,
}

#[derive(Subcommand, Debug)]
pub enum TagCommand {
    List,
    Create {
        name: String,
        target: Option<String>,
        #[arg(short, long)]
        message: Option<String>,
    },
    Delete {
        name: String,
    },
}

#[derive(Args, Debug)]
pub struct RemoteArgs {
    #[command(subcommand)]
    pub command: Option<RemoteCommand>,
    #[arg(short, long)]
    pub verbose: bool,
}

#[derive(Subcommand, Debug)]
pub enum RemoteCommand {
    Add {
        name: String,
        url: String,
    },
    #[command(alias = "rm")]
    Remove {
        name: String,
    },
    List {
        #[arg(short, long)]
        verbose: bool,
    },
}

#[derive(Args, Debug)]
pub struct OpArgs {
    #[command(subcommand)]
    pub command: OpCommand,
}

#[derive(Subcommand, Debug)]
pub enum OpCommand {
    /// List operations, newest first.
    Log {
        #[arg(short = 'n', long, default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        all: bool,
    },
    /// Show one operation in detail.
    Show { id: String },
    /// Make the whole repository match the state after an operation (or before, with --before).
    Restore {
        id: String,
        #[arg(long)]
        before: bool,
    },
    /// Delete operation records older than a duration (e.g. 14d).
    Prune {
        #[arg(long, default_value = "30d")]
        older_than: String,
    },
}

#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct CheckpointArgs {
    #[command(subcommand)]
    pub command: Option<CheckpointCommand>,
    /// Describe the checkpoint.
    #[arg(short, long)]
    pub message: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum CheckpointCommand {
    /// List checkpoints (every operation snapshots the worktree first).
    List {
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
    },
    /// Show what a checkpoint contains compared to the working tree.
    Diff {
        id: String,
        #[arg(last = true)]
        paths: Vec<String>,
    },
    /// Restore worktree files from a checkpoint (all, or the given paths).
    Restore {
        id: String,
        paths: Vec<String>,
        /// Use the snapshot taken *before* that operation ran (default for non-checkpoint operations).
        #[arg(long)]
        before: bool,
    },
    /// Take checkpoints on every veyra command, including read-only ones.
    Enable,
    /// Only checkpoint on mutating commands (the default).
    Disable,
    /// Keep taking checkpoints at an interval while you work (Ctrl-C to stop).
    Watch {
        #[arg(long, default_value = "10m")]
        interval: String,
    },
    /// Delete old checkpoints (and operation records).
    Prune {
        #[arg(long, default_value = "14d")]
        older_than: String,
    },
}

#[derive(Args, Debug)]
pub struct SecurityArgs {
    #[command(subcommand)]
    pub command: SecurityCommand,
}

#[derive(Subcommand, Debug)]
pub enum SecurityCommand {
    /// Scan the worktree (default), the index, or a revision for secrets.
    #[command(alias = "secrets")]
    Scan {
        #[arg(long, group = "source")]
        staged: bool,
        #[arg(long, group = "source")]
        rev: Option<String>,
    },
    /// Allowlist a reviewed false positive by fingerprint (written to .veyra/secrets.toml).
    Allow { fingerprint: String },
}

#[derive(Args, Debug)]
pub struct IntentArgs {
    #[command(subcommand)]
    pub command: IntentCommand,
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)] // parsed once per process
pub enum IntentCommand {
    /// Record a new intent in .veyra/intents/.
    Create {
        #[arg(long)]
        title: String,
        #[arg(long, default_value = "feature")]
        kind: String,
        #[arg(long)]
        objective: Option<String>,
        #[arg(long)]
        problem: Option<String>,
        #[arg(long)]
        solution: Option<String>,
        #[arg(long)]
        constraint: Vec<String>,
        #[arg(long)]
        related: Vec<String>,
        #[arg(long)]
        verification: Vec<String>,
        /// Guard paths (glob), used with --guard-contains.
        #[arg(long)]
        guard_path: Vec<String>,
        /// Text that must remain in at least one file matching --guard-path.
        #[arg(long)]
        guard_contains: Option<String>,
        #[arg(long)]
        guard_message: Option<String>,
    },
    List {
        #[arg(long)]
        all: bool,
    },
    Show {
        id: String,
    },
    /// Check active intents' guards against the worktree, the index, or a revision.
    Verify {
        #[arg(long, group = "source")]
        staged: bool,
        #[arg(long, group = "source")]
        rev: Option<String>,
    },
    /// Mark an intent closed (or superseded).
    Close {
        id: String,
        #[arg(long)]
        superseded_by: Option<String>,
    },
    /// Commits linked to an intent via `Intent:` trailers.
    Log {
        id: String,
    },
}

#[derive(Args, Debug)]
pub struct ConfigArgs {
    pub key: Option<String>,
    pub value: Option<String>,
    #[arg(long)]
    pub global: bool,
    #[arg(long)]
    pub unset: bool,
    #[arg(short, long)]
    pub list: bool,
}

#[derive(Args, Debug)]
pub struct PushArgs {
    /// Remote name (default: the branch's push remote, then `origin`).
    pub remote: Option<String>,
    /// Refspecs: `main`, `main:release`, `+main`, `:old` (delete). Default: the current branch.
    pub refspecs: Vec<String>,
    /// Allow non-fast-forward updates (prefer --force-with-lease).
    #[arg(short, long)]
    pub force: bool,
    /// Overwrite only if the remote still has what we last fetched.
    #[arg(long)]
    pub force_with_lease: bool,
    /// Remember the remote branch as upstream (`branch.<name>.remote/merge`).
    #[arg(short = 'u', long)]
    pub set_upstream: bool,
    /// Also push all tags.
    #[arg(long)]
    pub tags: bool,
    /// Delete the named remote refs.
    #[arg(short, long)]
    pub delete: bool,
    /// Show what would be pushed without sending anything.
    #[arg(short = 'n', long)]
    pub dry_run: bool,
    /// Skip the pre-push hook.
    #[arg(long)]
    pub no_verify: bool,
}

#[derive(Args, Debug)]
pub struct SequenceArgs {
    /// Commits to apply, in order.
    pub commits: Vec<String>,
    /// Apply to index and worktree without committing.
    #[arg(short = 'n', long)]
    pub no_commit: bool,
    /// Append "(cherry picked from commit …)" to the message.
    #[arg(short = 'x')]
    pub record_origin: bool,
    /// Parent number of the mainline when replaying a merge commit.
    #[arg(short = 'm', long)]
    pub mainline: Option<usize>,
    /// Commit the resolved conflict and go on.
    #[arg(long = "continue", group = "seq")]
    pub continue_: bool,
    /// Give up and return to the state before.
    #[arg(long, group = "seq")]
    pub abort: bool,
}

#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct StashArgs {
    #[command(subcommand)]
    pub command: Option<StashCommand>,
    /// Message for the new entry (bare `veyra stash` = push).
    #[arg(short, long)]
    pub message: Option<String>,
    #[arg(short = 'u', long)]
    pub include_untracked: bool,
    #[arg(long)]
    pub keep_index: bool,
}

#[derive(Subcommand, Debug)]
pub enum StashCommand {
    /// Save local changes and reset them away.
    Push {
        #[arg(short, long)]
        message: Option<String>,
        /// Also stash untracked files.
        #[arg(short = 'u', long)]
        include_untracked: bool,
        /// Keep staged changes in the index.
        #[arg(long)]
        keep_index: bool,
    },
    /// List entries, newest first.
    List,
    /// Show the changes in an entry.
    Show {
        entry: Option<String>,
        #[arg(short, long)]
        patch: bool,
    },
    /// Apply an entry and keep it.
    Apply {
        entry: Option<String>,
        #[arg(long)]
        index: bool,
    },
    /// Apply an entry and drop it if it applied cleanly.
    Pop {
        entry: Option<String>,
        #[arg(long)]
        index: bool,
    },
    /// Delete an entry.
    Drop { entry: Option<String> },
    /// Delete all entries.
    Clear,
}

#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct VerifyArgs {
    #[command(subcommand)]
    pub command: Option<VerifyCommand>,
    /// Report the signature status of every commit in the history.
    #[arg(long)]
    pub history: bool,
    /// Show and check the attestations recorded for a commit.
    #[arg(long)]
    pub provenance: bool,
    /// Where the history starts (default: HEAD).
    #[arg(long, value_name = "rev")]
    pub rev: Option<String>,
    #[arg(short = 'n', long, default_value_t = 50)]
    pub max_count: usize,
}

#[derive(Subcommand, Debug)]
pub enum VerifyCommand {
    /// Verify one commit's signature (`git verify-commit`).
    Commit { revision: Option<String> },
    /// Verify an annotated tag's signature (`git verify-tag`).
    Tag { name: String },
}

#[derive(Subcommand, Debug)]
pub enum StorageCommand {
    /// Object counts and sizes (`git count-objects -v`).
    Stats,
}

#[derive(Subcommand, Debug)]
pub enum MaintenanceCommand {
    /// Pack loose objects, then prune unreachable ones older than the grace period.
    Run {
        /// Grace period for unreachable loose objects.
        #[arg(long, default_value = "2w")]
        prune_expire: String,
    },
    /// Move loose objects into a new pack.
    PackLoose,
    /// Delete unreachable loose objects older than the grace period.
    Prune {
        #[arg(long, default_value = "2w")]
        expire: String,
        #[arg(short = 'n', long)]
        dry_run: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum WorktreeCommand {
    /// Create a worktree at <path> (branch defaults to the directory name, created from HEAD if new).
    Add {
        path: std::path::PathBuf,
        /// An existing branch or commit to check out.
        commitish: Option<String>,
        /// Create this new branch.
        #[arg(short = 'b')]
        new_branch: Option<String>,
        /// Detach HEAD at the commit.
        #[arg(long)]
        detach: bool,
    },
    /// List worktrees.
    List,
    /// Remove a worktree.
    Remove {
        path: std::path::PathBuf,
        #[arg(short, long)]
        force: bool,
    },
    /// Forget worktrees whose directories were deleted.
    Prune,
}

#[derive(Subcommand, Debug)]
pub enum SubmoduleCommand {
    /// Show each submodule's recorded and checked-out commit.
    Status,
    /// Copy submodule URLs from .gitmodules into .git/config.
    Init { paths: Vec<String> },
    /// Clone missing submodules and check out the recorded commits.
    Update {
        #[arg(long)]
        init: bool,
        #[arg(long)]
        recursive: bool,
        paths: Vec<String>,
    },
    /// Add a repository as a submodule.
    Add { url: String, path: Option<String> },
    /// Run a shell command in each checked-out submodule.
    Foreach { command: String },
}

#[derive(Subcommand, Debug)]
pub enum SparseCommand {
    /// Check out only these directories (plus root files).
    Set {
        dirs: Vec<String>,
        /// Use a named scope from .veyra/scopes.toml.
        #[arg(long)]
        scope: Option<String>,
    },
    /// Add directories to the sparse checkout.
    Add { dirs: Vec<String> },
    /// Show the selected directories.
    List,
    /// Check out everything again.
    Disable,
}

#[derive(Subcommand, Debug)]
pub enum AttestCommand {
    /// Run a command (e.g. your tests) and attest its result for a commit.
    Run {
        #[arg(long)]
        rev: Option<String>,
        /// Sign the attestation with your commit-signing key.
        #[arg(long)]
        sign: bool,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Attest a code review decision for a commit.
    Review {
        #[arg(long)]
        rev: Option<String>,
        #[arg(long)]
        reviewer: Option<String>,
        /// Record a rejection instead of an approval.
        #[arg(long)]
        reject: bool,
        #[arg(short, long)]
        message: Option<String>,
        #[arg(long)]
        sign: bool,
    },
    /// List a commit's attestations.
    List { rev: Option<String> },
}
