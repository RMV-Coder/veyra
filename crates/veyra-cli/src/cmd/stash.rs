//! stash / shelve / unshelve.

use anyhow::Result;
use serde_json::json;
use veyra_core::diff::NewSide;
use veyra_core::stash::StashApply;

use super::{journal, plural};
use crate::cli::{StashArgs, StashCommand};
use crate::{Ctx, Reported, ui};

pub fn stash(ctx: &Ctx, args: StashArgs) -> Result<()> {
    let command = args.command.unwrap_or(StashCommand::Push {
        message: args.message,
        include_untracked: args.include_untracked,
        keep_index: args.keep_index,
    });
    run(ctx, command)
}

pub fn run(ctx: &Ctx, command: StashCommand) -> Result<()> {
    let repo = ctx.repo()?;
    match command {
        StashCommand::Push { message, include_untracked, keep_index } => {
            let saved = journal(ctx, &repo, "stash", true, || {
                let w = repo.stash_push(message.as_deref(), include_untracked, keep_index)?;
                let summary = match w {
                    Some(id) => format!("stash {}", veyra_core::short(&id)),
                    None => "stash: nothing to save".into(),
                };
                Ok((w, summary))
            })?;
            match saved {
                Some(_) => {
                    let entry = repo.stash_entry(None)?;
                    anstream::println!("Saved working directory and index state {}", entry.message);
                }
                None => anstream::println!("No local changes to save"),
            }
        }
        StashCommand::List => {
            let list = repo.stash_list()?;
            if ctx.json {
                ui::json(&json!(
                    list.iter()
                        .map(|e| json!({"index": e.index, "commit": e.commit.to_string(), "message": e.message, "time": e.time}))
                        .collect::<Vec<_>>()
                ));
            } else {
                for e in list {
                    anstream::println!("{}: {}", ui::yellow(&format!("stash@{{{}}}", e.index)), e.message);
                }
            }
        }
        StashCommand::Show { entry, patch } => {
            let e = repo.stash_entry(entry.as_deref())?;
            let w = repo.commit_info(e.commit)?;
            let base = repo.flat_from_commit(w.parents[0])?;
            let changes = veyra_core::flat::diff(&base, &repo.flat_from_tree(w.tree)?);
            let diffs = repo.diff_changes(&changes, NewSide::Objects, 3)?;
            for d in &diffs {
                if patch {
                    anstream::print!("{}", ui::colour_diff(&veyra_core::diff::render(d, None)));
                } else {
                    anstream::println!(
                        " {} | {} {}",
                        d.path,
                        ui::green(&format!("+{}", d.added)),
                        ui::red(&format!("-{}", d.removed))
                    );
                }
            }
            if !patch {
                anstream::println!(" {} changed", plural(diffs.len(), "file"));
            }
        }
        StashCommand::Apply { entry, index } => apply(ctx, &repo, entry, index, false)?,
        StashCommand::Pop { entry, index } => apply(ctx, &repo, entry, index, true)?,
        StashCommand::Drop { entry } => {
            let e = repo.stash_entry(entry.as_deref())?;
            journal(ctx, &repo, "stash-drop", false, || {
                repo.stash_drop(&e)?;
                Ok(((), format!("drop stash@{{{}}}", e.index)))
            })?;
            anstream::println!("Dropped stash@{{{}}} ({})", e.index, veyra_core::short(&e.commit));
        }
        StashCommand::Clear => {
            ctx.confirm_destructive("stash clear")?;
            let n = journal(ctx, &repo, "stash-clear", false, || {
                let n = repo.stash_clear()?;
                Ok((n, format!("clear {n} stash entries")))
            })?;
            anstream::println!("Cleared {}.", plural(n, "stash entry"));
        }
    }
    Ok(())
}

fn apply(ctx: &Ctx, repo: &veyra_core::Repo, entry: Option<String>, index: bool, pop: bool) -> Result<()> {
    let e = repo.stash_entry(entry.as_deref())?;
    let verb = if pop { "pop" } else { "apply" };
    let outcome = journal(ctx, repo, &format!("stash-{verb}"), true, || {
        let outcome = repo.stash_apply(&e, index)?;
        if pop && matches!(outcome, StashApply::Applied { .. }) {
            repo.stash_drop(&e)?;
        }
        Ok((outcome, format!("stash {verb} stash@{{{}}}", e.index)))
    })?;
    super::report_structural(repo);
    match outcome {
        StashApply::Applied { paths } => {
            anstream::println!("Applied stash@{{{}}} ({}): {}", e.index, plural(paths, "path"), e.message);
            if pop {
                anstream::println!("Dropped stash@{{{}}} ({})", e.index, veyra_core::short(&e.commit));
            }
        }
        StashApply::Conflicts(paths) => {
            for p in &paths {
                anstream::println!("CONFLICT (content): merge conflict in {p}");
            }
            anstream::println!("The stash entry is kept. Resolve the conflicts, then `veyra stash drop` it when done.");
            return Err(Reported.into());
        }
    }
    Ok(())
}
