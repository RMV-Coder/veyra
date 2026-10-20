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
    },
    /// Clone a repository into a new directory.
    Clone { url: String, directory: Option<std::path::PathBuf> },
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
    /// Update remote refs (not implemented yet — use `git push`).
    Push {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
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
    /// Inspect and restore the operation log.
    #[command(name = "op", alias = "operation")]
    Op(OpArgs),
    /// Snapshot the working tree without committing; list and restore checkpoints.
    Checkpoint(CheckpointArgs),

    // ----- intelligence & diagnostics -----
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
    /// Skip the secret scan and intent verification.
    #[arg(long)]
    pub no_verify: bool,
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
