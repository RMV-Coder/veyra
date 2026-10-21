//! Command implementations.

mod branch;
mod history;
mod remote;
mod repo;
mod safety;
mod sequence;
mod stash;
mod tools;

use anyhow::Result;
use veyra_core::Repo;
use veyra_ops::OpLog;

use crate::cli::Command;
use crate::{Ctx, ui};

pub fn run(ctx: &Ctx, command: Command) -> Result<()> {
    use Command::*;
    match command {
        Init { directory, initial_branch, object_format } => repo::init(ctx, directory, initial_branch, object_format),
        Clone { url, directory, scope, sparse } => repo::clone(ctx, &url, directory, scope, sparse),
        Status { short } => repo::status(ctx, short),
        Add { paths, patch: true, .. } => repo::add_patch(ctx, paths),
        Add { paths, all, update, force, patch: false } => repo::add(ctx, paths, all, update, force),
        Rm { paths, cached, force, recursive } => repo::rm(ctx, paths, cached, force, recursive),
        Mv { source, destination, force } => repo::mv(ctx, &source, &destination, force),
        Restore(args) => repo::restore(ctx, args),
        Discard { paths } => {
            repo::restore(ctx, crate::cli::RestoreArgs { paths, staged: false, worktree: false, source: None })
        }
        Unstage { paths } => {
            repo::restore(ctx, crate::cli::RestoreArgs { paths, staged: true, worktree: false, source: None })
        }
        Commit(args) => repo::commit(ctx, args),
        Log(args) => history::log(ctx, args),
        Show { revision, stat } => history::show(ctx, revision, stat),
        Diff(args) => history::diff(ctx, args),
        Blame { file, rev, lines } => history::blame(ctx, &file, rev, lines),
        Clean { force, dry_run, d, paths } => repo::clean(ctx, force, dry_run, d, paths),
        Branch(args) => branch::branch(ctx, args),
        Switch { branch, create, detach, force } => branch::switch(ctx, branch, create, detach, force),
        Checkout { target, new_branch, force, paths } => branch::checkout(ctx, target, new_branch, force, paths),
        Merge { revision, ff_only, no_ff, message, abort, continue_ } => {
            branch::merge(ctx, revision, ff_only, no_ff, message, abort, continue_)
        }
        Reset { soft, mixed: _, hard, target, paths } => branch::reset(ctx, soft, hard, target, paths),
        Tag(args) => branch::tag(ctx, args),
        CherryPick(args) => sequence::pick(ctx, veyra_core::pick::PickKind::CherryPick, args),
        Revert(args) => sequence::pick(ctx, veyra_core::pick::PickKind::Revert, args),
        Rebase { upstream, onto, interactive, continue_, skip, abort } => {
            sequence::rebase(ctx, upstream, onto, interactive, continue_, skip, abort)
        }
        Remote(args) => remote::remote(ctx, args),
        Fetch { remote } => remote::fetch(ctx, remote),
        Pull { remote, branch, ff_only } => remote::pull(ctx, remote, branch, ff_only),
        Push(args) => remote::push(ctx, args),
        Sync { push } => remote::sync(ctx, push),
        Undo { op, force, dry_run, what } => safety::undo(ctx, op, force, dry_run, what),
        Redo { force } => safety::redo(ctx, force),
        Op(args) => safety::op(ctx, args.command),
        Stash(args) => stash::stash(ctx, args),
        Shelve { message, include_untracked } => {
            stash::run(ctx, crate::cli::StashCommand::Push { message, include_untracked, keep_index: false })
        }
        Unshelve { entry } => stash::run(ctx, crate::cli::StashCommand::Pop { entry, index: false }),
        Checkpoint(args) => safety::checkpoint(ctx, args),
        Explain { command } => crate::explain::run(ctx, &command),
        Doctor => tools::doctor(ctx),
        Verify(args) => tools::verify(ctx, args),
        Attest { command } => tools::attest(ctx, command),
        Storage { command } => tools::storage(ctx, command),
        Worktree { command } => tools::worktree(ctx, command),
        Sparse { command } => tools::sparse(ctx, command),
        Submodule { command } => tools::submodule(ctx, command.unwrap_or(crate::cli::SubmoduleCommand::Status)),
        Maintenance { command } => tools::maintenance(ctx, command),
        Security(args) => tools::security(ctx, args.command),
        Intent(args) => tools::intent(ctx, args.command),
        Config(args) => tools::config(ctx, args),
        RevParse { specs, short } => tools::rev_parse(ctx, specs, short),
        CatFile { kind, size, pretty, object } => tools::cat_file(ctx, kind, size, pretty, &object),
        HashObject { write, files } => tools::hash_object(ctx, write, files),
        LsFiles { stage } => tools::ls_files(ctx, stage),
        ShowRef => tools::show_ref(ctx),
    }
}

/// Run `f` as a journaled operation: snapshot before, record after.
/// `f` returns the value and the one-line summary stored in the operation log.
pub(crate) fn journal<T>(
    ctx: &Ctx,
    repo: &Repo,
    kind: &str,
    touches_worktree: bool,
    f: impl FnOnce() -> Result<(T, String)>,
) -> Result<T> {
    let profile = std::env::var_os("VEYRA_PROFILE").is_some();
    let t0 = std::time::Instant::now();
    let log = OpLog::open(repo)?;
    let tx = log.begin(kind, &ctx.argv, touches_worktree)?;
    let t1 = std::time::Instant::now();
    let (value, summary) = f()?;
    let t2 = std::time::Instant::now();
    tx.commit(&summary)?;
    if profile {
        anstream::eprintln!(
            "[profile] journal begin {:?} · {kind} {:?} · journal commit {:?}",
            t1 - t0,
            t2 - t1,
            t2.elapsed()
        );
    }
    Ok(value)
}

/// For read-only commands: take a checkpoint if `veyra.autoCheckpoint` is enabled.
pub(crate) fn maybe_auto_checkpoint(ctx: &Ctx, repo: &Repo) {
    if let Ok(log) = OpLog::open(repo)
        && let Err(err) = log.auto_checkpoint(&ctx.argv)
    {
        ui::warn(&format!("automatic checkpoint failed: {err}"));
    }
}

/// Paths given on the command line, as a pathspec relative to the current directory.
pub(crate) fn pathspec(ctx: &Ctx, repo: &Repo, paths: &[String]) -> Result<veyra_core::worktree::Pathspec> {
    veyra_core::worktree::Pathspec::new(repo, &ctx.cwd, paths)
}

pub(crate) fn plural(n: usize, word: &str) -> String {
    if n == 1 { format!("{n} {word}") } else { format!("{n} {word}s") }
}
