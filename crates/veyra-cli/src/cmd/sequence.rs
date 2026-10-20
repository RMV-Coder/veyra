//! cherry-pick, revert, rebase.

use anyhow::{Context, Result, bail};
use veyra_core::pick::{PickKind, PickOptions, PickOutcome, RebaseOutcome};

use super::{journal, plural};
use crate::cli::SequenceArgs;
use crate::{Ctx, Reported, ui};

fn verb(kind: PickKind) -> &'static str {
    match kind {
        PickKind::CherryPick => "cherry-pick",
        PickKind::Revert => "revert",
    }
}

pub fn pick(ctx: &Ctx, kind: PickKind, args: SequenceArgs) -> Result<()> {
    let repo = ctx.repo()?;
    let name = verb(kind);
    if args.abort {
        journal(ctx, &repo, &format!("{name}-abort"), true, || {
            repo.abort_pick()?;
            Ok(((), format!("abort {name}")))
        })?;
        anstream::println!("{name} aborted.");
        return Ok(());
    }
    if args.continue_ {
        let id = journal(ctx, &repo, name, false, || {
            let id = repo.continue_pick(None)?;
            Ok((id, format!("{name} --continue: {}", veyra_core::short(&id))))
        })?;
        let info = repo.commit_info(id)?;
        anstream::println!("[{} {}] {}", repo.head_label()?, veyra_core::short(&id), info.summary());
        return Ok(());
    }
    if args.commits.is_empty() {
        bail!("usage: veyra {name} <commit>... | --continue | --abort");
    }
    let commits: Vec<_> = args.commits.iter().map(|c| repo.rev_parse_commit(c)).collect::<Result<_>>()?;
    let options = PickOptions { no_commit: args.no_commit, record_origin: args.record_origin, mainline: args.mainline };
    for (spec, commit) in args.commits.iter().zip(commits) {
        let outcome = journal(ctx, &repo, name, true, || {
            let o = repo.pick(kind, commit, &options)?;
            let summary = match &o {
                PickOutcome::Committed(id) => format!("{name} {spec} → {}", veyra_core::short(id)),
                PickOutcome::Empty => format!("{name} {spec}: nothing to do"),
                PickOutcome::Applied => format!("{name} --no-commit {spec}"),
                PickOutcome::Conflicts(p) => format!("{name} {spec}: stopped with {}", plural(p.len(), "conflict")),
            };
            Ok((o, summary))
        })?;
        match outcome {
            PickOutcome::Committed(id) => {
                let info = repo.commit_info(id)?;
                anstream::println!("[{} {}] {}", repo.head_label()?, veyra_core::short(&id), info.summary());
            }
            PickOutcome::Empty => anstream::println!("{spec}: the change is already present; nothing to commit"),
            PickOutcome::Applied => anstream::println!("Applied {spec} to the index and working tree (not committed)"),
            PickOutcome::Conflicts(paths) => {
                for p in &paths {
                    anstream::println!("CONFLICT (content): merge conflict in {p}");
                }
                anstream::println!(
                    "{name} of {spec} stopped. Fix the conflicts, `veyra add` them, then `veyra {name} --continue` \
                     (or `veyra {name} --abort`)."
                );
                return Err(Reported.into());
            }
        }
    }
    Ok(())
}

pub fn rebase(
    ctx: &Ctx,
    upstream: Option<String>,
    onto: Option<String>,
    interactive: bool,
    continue_: bool,
    skip: bool,
    abort: bool,
) -> Result<()> {
    let repo = ctx.repo()?;
    if abort {
        journal(ctx, &repo, "rebase-abort", true, || {
            repo.rebase_abort()?;
            Ok(((), "abort rebase".to_owned()))
        })?;
        anstream::println!("Rebase aborted; {} is back where it was.", repo.head_label()?);
        return Ok(());
    }
    let outcome = if continue_ || skip {
        journal(ctx, &repo, "rebase", true, || {
            let mut editor = super::repo::rebase_message_editor(&repo);
            let o = repo.rebase_continue(skip, &mut editor)?;
            Ok((o.clone(), describe(&o)))
        })?
    } else {
        let upstream_spec = match upstream {
            Some(u) => u,
            None => {
                let branch = repo
                    .head_state()?
                    .branch_short()
                    .map(str::to_owned)
                    .context("HEAD is detached; name the upstream")?;
                let remote = repo.config_get(&format!("branch.{branch}.remote"));
                let merge = repo.config_get(&format!("branch.{branch}.merge"));
                match (remote, merge) {
                    (Some(r), Some(m)) => format!("refs/remotes/{r}/{}", m.trim_start_matches("refs/heads/")),
                    _ => {
                        bail!("there is no upstream for '{branch}'; say what to rebase onto, e.g. `veyra rebase main`")
                    }
                }
            }
        };
        let upstream_id = repo.rev_parse_commit(&upstream_spec)?;
        let onto_id = onto.as_deref().map(|o| repo.rev_parse_commit(o)).transpose()?;
        let plan = if interactive { Some(edit_todo(&repo, upstream_id)?) } else { None };
        if plan.as_ref().is_some_and(Vec::is_empty) {
            anstream::println!("Nothing to do: the todo list is empty.");
            return Ok(());
        }
        journal(ctx, &repo, "rebase", true, || {
            let mut editor = super::repo::rebase_message_editor(&repo);
            let o = repo.rebase(upstream_id, onto_id, plan.clone(), &mut editor)?;
            Ok((o.clone(), describe(&o)))
        })?
    };
    match outcome {
        RebaseOutcome::UpToDate => anstream::println!("Current branch {} is up to date.", repo.head_label()?),
        RebaseOutcome::Done { head, picked, skipped } => {
            let mut line =
                format!("Successfully rebased {} ({} replayed", repo.head_label()?, plural(picked, "commit"));
            if skipped > 0 {
                line.push_str(&format!(", {skipped} already upstream"));
            }
            line.push_str(&format!(") — now at {}", veyra_core::short(&head)));
            anstream::println!("{line}");
            ui::hint("`veyra undo` puts the branch back exactly as it was");
        }
        RebaseOutcome::Edit { commit, remaining } => {
            let info = repo.commit_info(repo.head_commit_id()?.unwrap_or(commit))?;
            anstream::println!("Stopped at {} {}", veyra_core::short(&commit), info.summary());
            anstream::println!(
                "Amend it now (`veyra commit --amend`), then `veyra rebase --continue` ({} left).",
                plural(remaining, "commit")
            );
        }
        RebaseOutcome::Break { at, remaining } => {
            anstream::println!(
                "Stopped at {} (break). `veyra rebase --continue` resumes ({} left).",
                veyra_core::short(&at),
                plural(remaining, "step")
            );
        }
        RebaseOutcome::ExecFailed { command, error, remaining } => {
            anstream::println!("{error}");
            anstream::println!(
                "The rebase stopped after `{command}`. Fix things (amend if needed), then `veyra rebase --continue` ({} left).",
                plural(remaining, "step")
            );
            return Err(Reported.into());
        }
        RebaseOutcome::Stopped { commit, paths, remaining } => {
            let info = repo.commit_info(commit)?;
            anstream::println!("Could not apply {} {}", veyra_core::short(&commit), info.summary());
            for p in &paths {
                anstream::println!("CONFLICT (content): merge conflict in {p}");
            }
            anstream::println!(
                "Resolve, `veyra add` the files, then `veyra rebase --continue` ({} left). \
                 `veyra rebase --skip` drops this commit; `veyra rebase --abort` returns to where you started.",
                plural(remaining, "commit")
            );
            return Err(Reported.into());
        }
    }
    Ok(())
}

fn describe(o: &RebaseOutcome) -> String {
    match o {
        RebaseOutcome::UpToDate => "rebase: up to date".into(),
        RebaseOutcome::Done { head, picked, .. } => {
            format!("rebase: {picked} commit(s) replayed, now at {}", veyra_core::short(head))
        }
        RebaseOutcome::Stopped { commit, .. } => format!("rebase: stopped at {}", veyra_core::short(commit)),
        RebaseOutcome::Edit { commit, .. } => format!("rebase: stopped to edit {}", veyra_core::short(commit)),
        RebaseOutcome::Break { at, .. } => format!("rebase: break at {}", veyra_core::short(at)),
        RebaseOutcome::ExecFailed { command, .. } => format!("rebase: exec failed: {command}"),
    }
}

const TODO_HELP: &str = "
# Commands:
# p, pick <commit>   = use commit
# r, reword <commit> = use commit, but edit the commit message
# e, edit <commit>   = use commit, but stop for amending
# s, squash <commit> = use commit, but meld into previous commit
# f, fixup <commit>  = like \"squash\", but keep only the previous commit's message
# d, drop <commit>   = remove commit
# x, exec <command>  = run command (the rest of the line) using shell
# b, break           = stop here (continue rebase later with 'veyra rebase --continue')
#
# Lines can be re-ordered; they are executed from top to bottom.
# Removing every line aborts the rebase. `veyra undo` reverts a finished rebase.
";

/// Write the todo list, let the user edit it in the sequence editor, and parse it.
fn edit_todo(repo: &veyra_core::Repo, upstream: veyra_core::ObjectId) -> Result<Vec<veyra_core::pick::RebaseStep>> {
    let commits = repo.commits_to_replay(upstream)?;
    let mut text = String::new();
    for c in &commits {
        text.push_str(&format!("pick {} {}\n", veyra_core::short(c), repo.commit_info(*c)?.summary()));
    }
    let path = repo.veyra_dir().join("rebase-todo");
    std::fs::create_dir_all(repo.veyra_dir())?;
    let edited = crate::editor::edit_text(
        &crate::editor::sequence_editor(repo),
        &path,
        &text,
        &format!("\n# Rebase {} commits onto {}{TODO_HELP}", commits.len(), veyra_core::short(&upstream)),
    )?;
    let _ = std::fs::remove_file(&path);
    veyra_core::pick::parse_todo(repo, &edited)
}
