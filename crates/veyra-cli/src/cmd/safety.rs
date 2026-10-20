//! undo, redo, op, checkpoint — Veyra's safety net.

use anyhow::{Context, Result, bail};
use serde_json::json;
use veyra_core::diff::NewSide;
use veyra_core::{BString, flat};
use veyra_ops::{ApplyReport, OpLog, Operation, parse_id, targets};

use super::plural;
use crate::cli::{CheckpointArgs, CheckpointCommand, OpCommand};
use crate::{Ctx, ui};

fn report_lines(report: &ApplyReport) -> Vec<String> {
    let mut out = Vec::new();
    for r in &report.refs_changed {
        out.push(format!("ref      {}", veyra_core::refs::shorten(r)));
    }
    if report.head_changed {
        out.push("HEAD     moved".into());
    }
    if report.index_paths > 0 {
        out.push(format!("index    {}", plural(report.index_paths, "path")));
    }
    for p in report.worktree_paths.iter().take(15) {
        out.push(format!("file     {p}"));
    }
    if report.worktree_paths.len() > 15 {
        out.push(format!("file     … and {} more", report.worktree_paths.len() - 15));
    }
    out
}

fn describe(op: &Operation) -> String {
    format!("{} {}", ui::yellow(op.short_id()), op.summary)
}

pub fn undo(ctx: &Ctx, which: Option<String>, force: bool, dry_run: bool, what: Option<String>) -> Result<()> {
    let repo = ctx.repo()?;
    if force {
        ctx.confirm_destructive("undo --force")?;
    }
    let log = OpLog::open(&repo)?;
    if let Some(kind) = &what {
        // `veyra undo commit`: make sure the thing being undone is what the user expects.
        let ops = log.operations()?;
        let target = targets(&ops).0.context("nothing to undo")?;
        let matches = match kind.as_str() {
            "commit" => matches!(target.kind.as_str(), "commit" | "amend" | "merge"),
            other => target.kind == other,
        };
        if !matches && which.is_none() {
            bail!(
                "the last operation is `{}` ({}), not a {kind}\n  hint: `veyra op log` lists operations; `veyra undo --op <id>` undoes a specific one",
                target.kind,
                target.summary
            );
        }
    }
    let (target, op, report) = log.undo(which.as_deref(), force, dry_run, &ctx.argv)?;
    if ctx.json {
        ui::json(&json!({
            "undone": target.id, "kind": target.kind, "summary": target.summary,
            "dry_run": dry_run, "operation": op.as_ref().map(|o| o.id.clone()),
            "refs": report.refs_changed, "files": report.worktree_paths, "index_paths": report.index_paths,
        }));
        return Ok(());
    }
    let verb = if dry_run { "Would undo" } else { "Undid" };
    anstream::println!("{verb} {} {}", target.kind, describe(&target));
    for line in report_lines(&report) {
        anstream::println!("  {line}");
    }
    if report.is_empty() {
        anstream::println!("  (nothing needed to change)");
    }
    if !dry_run {
        ui::hint("`veyra redo` reapplies it");
    }
    Ok(())
}

pub fn redo(ctx: &Ctx, force: bool) -> Result<()> {
    let repo = ctx.repo()?;
    let log = OpLog::open(&repo)?;
    let (target, _, report) = log.redo(force, &ctx.argv)?;
    anstream::println!("Redid {} {}", target.kind, describe(&target));
    for line in report_lines(&report) {
        anstream::println!("  {line}");
    }
    Ok(())
}

pub fn op(ctx: &Ctx, command: OpCommand) -> Result<()> {
    let repo = ctx.repo()?;
    let log = OpLog::open(&repo)?;
    match command {
        OpCommand::Log { limit, all } => {
            let ops = log.operations()?;
            let (undo_target, redo_target) = targets(&ops);
            let undone: std::collections::BTreeSet<&str> = {
                let mut set = std::collections::BTreeSet::new();
                for op in &ops {
                    match (op.kind.as_str(), op.target.as_deref()) {
                        ("undo", Some(t)) => {
                            set.insert(t);
                        }
                        ("redo", Some(t)) => {
                            set.remove(t);
                        }
                        _ => {}
                    }
                }
                set
            };
            let shown: Vec<&Operation> =
                ops.iter().rev().filter(|o| all || o.kind != "checkpoint" || o.summary != "auto").take(limit).collect();
            if ctx.json {
                ui::json(&json!(
                    shown
                        .iter()
                        .map(|o| json!({
                            "id": o.id, "time": o.time, "kind": o.kind, "summary": o.summary, "command": o.command,
                            "undone": undone.contains(o.id.as_str()), "target": o.target,
                        }))
                        .collect::<Vec<_>>()
                ));
                return Ok(());
            }
            if shown.is_empty() {
                anstream::println!(
                    "No operations recorded yet. Every veyra command that changes the repository will appear here."
                );
                return Ok(());
            }
            let now = veyra_core::time::now_seconds();
            for o in shown {
                let mut tags = Vec::new();
                if undo_target.is_some_and(|u| u.id == o.id) {
                    tags.push(ui::green("← undo"));
                }
                if redo_target.is_some_and(|r| r.id == o.id) {
                    tags.push(ui::cyan("← redo"));
                }
                if undone.contains(o.id.as_str()) {
                    tags.push(ui::dim("(undone)"));
                }
                anstream::println!(
                    "{}  {:<16} {:<11} {} {}",
                    ui::yellow(o.short_id()),
                    ui::dim(&veyra_core::time::format_relative(o.time, now)),
                    o.kind,
                    o.summary,
                    tags.join(" ")
                );
            }
        }
        OpCommand::Show { id } => {
            let o = log.find(&id)?;
            anstream::println!("{}", ui::yellow(&format!("operation {}", o.id)));
            anstream::println!("Kind:     {}", o.kind);
            anstream::println!("Time:     {}", veyra_core::time::format_local(o.time));
            anstream::println!("Command:  {}", o.command);
            anstream::println!("Summary:  {}", o.summary);
            if let Some(t) = &o.target {
                anstream::println!("Target:   {t}");
            }
            if o.before.head != o.after.head {
                anstream::println!("HEAD:     {} → {}", o.before.head_label(), o.after.head_label());
            }
            let names: std::collections::BTreeSet<&String> = o.before.refs.keys().chain(o.after.refs.keys()).collect();
            for n in names {
                let (b, a) = (o.before.refs.get(n), o.after.refs.get(n));
                if b != a {
                    let short = |h: Option<&String>| h.map(|h| h[..7].to_owned()).unwrap_or_else(|| "(none)".into());
                    anstream::println!("Ref:      {} {} → {}", veyra_core::refs::shorten(n), short(b), short(a));
                }
            }
            if o.before.index_tree != o.after.index_tree {
                let changes = flat::diff(
                    &repo.flat_from_tree(parse_id(&o.before.index_tree)?)?,
                    &repo.flat_from_tree(parse_id(&o.after.index_tree)?)?,
                );
                for c in changes {
                    anstream::println!("Index:    {} {}", c.status_letter(), c.path);
                }
            }
            if let (Some(b), Some(a)) = (o.before.worktree_id(), o.after.worktree_id())
                && b != a
            {
                for c in flat::diff(&repo.flat_from_tree(b)?, &repo.flat_from_tree(a)?) {
                    anstream::println!("File:     {} {}", c.status_letter(), c.path);
                }
            }
            if !o.before.skipped.is_empty() {
                anstream::println!("Skipped (too large to snapshot): {}", o.before.skipped.join(", "));
            }
        }
        OpCommand::Restore { id, before } => {
            ctx.confirm_destructive("op restore")?;
            let (target, _, report) = log.restore(&id, before, &ctx.argv)?;
            anstream::println!(
                "Restored the repository to its state {} {}",
                if before { "before" } else { "after" },
                describe(&target)
            );
            for line in report_lines(&report) {
                anstream::println!("  {line}");
            }
            ui::hint("`veyra undo` reverts this restore");
        }
        OpCommand::Prune { older_than } => prune(&log, &older_than)?,
    }
    Ok(())
}

fn prune(log: &OpLog<'_>, older_than: &str) -> Result<()> {
    let secs = veyra_core::time::parse_duration(older_than)
        .with_context(|| format!("invalid duration '{older_than}' (examples: 12h, 14d, 2w)"))?;
    let removed = log.prune(secs, 20)?;
    anstream::println!(
        "Pruned {} older than {older_than} (the newest 20 are always kept).",
        plural(removed, "operation")
    );
    if removed > 0 {
        ui::hint("run `git gc` to reclaim the space");
    }
    Ok(())
}

/// Checkpoints are operations with worktree snapshots; consecutive identical snapshots are folded.
fn checkpoints(log: &OpLog<'_>) -> Result<Vec<(Operation, String)>> {
    let mut out: Vec<(Operation, String)> = Vec::new();
    let mut last: Option<String> = None;
    for op in log.operations()? {
        let tree = if op.kind == "checkpoint" { op.after.worktree.clone() } else { op.before.worktree.clone() };
        let Some(tree) = tree else { continue };
        if last.as_ref() == Some(&tree) {
            continue;
        }
        last = Some(tree.clone());
        out.push((op, tree));
    }
    Ok(out)
}

pub fn checkpoint(ctx: &Ctx, args: CheckpointArgs) -> Result<()> {
    let repo = ctx.repo()?;
    let log = OpLog::open(&repo)?;
    let Some(command) = args.command else {
        match log.checkpoint(args.message.as_deref(), &ctx.argv, args.message.is_some())? {
            Some(op) => anstream::println!(
                "Checkpoint {} created ({} differing from the index)",
                ui::yellow(op.short_id()),
                plural(op.worktree_changes, "file")
            ),
            None => anstream::println!("Nothing changed since the last checkpoint."),
        }
        return Ok(());
    };
    match command {
        CheckpointCommand::List { limit } => {
            let all = checkpoints(&log)?;
            let shown: Vec<_> = all.iter().rev().take(limit).collect();
            if ctx.json {
                ui::json(&json!(shown.iter().map(|(o, tree)| json!({
                    "id": o.id, "time": o.time, "tree": tree, "files_changed": o.worktree_changes,
                    "source": if o.kind == "checkpoint" { o.summary.clone() } else { format!("before {}", o.kind) },
                })).collect::<Vec<_>>()));
                return Ok(());
            }
            if shown.is_empty() {
                anstream::println!(
                    "No checkpoints yet. `veyra checkpoint` takes one; every mutating veyra command takes one automatically."
                );
                return Ok(());
            }
            anstream::println!("{}", ui::bold(&format!("{:<10} {:<17} {:>5}  {}", "ID", "CREATED", "FILES", "SOURCE")));
            for (o, _) in shown {
                let source = if o.kind == "checkpoint" { o.summary.clone() } else { format!("before: {}", o.summary) };
                anstream::println!(
                    "{} {:<17} {:>5}  {source}",
                    ui::yellow(&format!("{:<10}", o.short_id())),
                    veyra_core::time::format_local(o.time),
                    o.worktree_changes
                );
            }
            anstream::println!(
                "\n{}",
                ui::dim(
                    "FILES = files that differed from the index. Restore with `veyra checkpoint restore <id> [paths]`."
                )
            );
        }
        CheckpointCommand::Diff { id, paths } => {
            let op = log.find(&id)?;
            let tree = if op.kind == "checkpoint" { op.after.worktree_id() } else { op.before.worktree_id() }
                .context("that operation has no snapshot")?;
            let snap = repo.snapshot(&log.settings.snapshot)?;
            let spec = super::pathspec(ctx, &repo, &paths)?;
            let changes: Vec<_> = flat::diff(&repo.flat_from_tree(tree)?, &snap.flat)
                .into_iter()
                .filter(|c| spec.matches(c.path.as_ref()))
                .collect();
            if changes.is_empty() {
                anstream::println!("The working tree matches checkpoint {}.", op.short_id());
            }
            for d in repo.diff_changes(&changes, NewSide::Objects, 3)? {
                anstream::print!("{}", ui::colour_diff(&veyra_core::diff::render(&d, None)));
            }
        }
        CheckpointCommand::Restore { id, paths, before } => {
            ctx.confirm_destructive("checkpoint restore")?;
            let rel: Vec<BString> = paths.iter().map(|p| repo.to_repo_path(&ctx.cwd, p)).collect::<Result<_>>()?;
            let op = log.find(&id)?;
            let use_before = before || op.kind != "checkpoint";
            let (target, _, changed) = log.restore_checkpoint(&id, &rel, use_before, &ctx.argv)?;
            if changed.is_empty() {
                anstream::println!("Nothing to restore: those files already match checkpoint {}.", target.short_id());
            } else {
                anstream::println!(
                    "Restored {} from checkpoint {}:",
                    plural(changed.len(), "file"),
                    ui::yellow(target.short_id())
                );
                for p in &changed {
                    anstream::println!("  {p}");
                }
                ui::hint("the files you had before were checkpointed too; `veyra undo` swaps back");
            }
        }
        CheckpointCommand::Enable => {
            let path = repo.config_path(veyra_core::config::Scope::Local)?;
            veyra_core::config::set_in_file(&path, "veyra.autoCheckpoint", "true")?;
            anstream::println!(
                "Automatic checkpoints enabled: every veyra command (including status, diff and log) now snapshots changed files."
            );
            ui::hint("for long editing sessions, `veyra checkpoint watch --interval 10m` checkpoints on a timer");
        }
        CheckpointCommand::Disable => {
            let path = repo.config_path(veyra_core::config::Scope::Local)?;
            veyra_core::config::set_in_file(&path, "veyra.autoCheckpoint", "false")?;
            anstream::println!(
                "Automatic checkpoints on read-only commands disabled. Mutating commands still checkpoint (that is what makes `veyra undo` safe)."
            );
        }
        CheckpointCommand::Watch { interval } => {
            let secs = veyra_core::time::parse_duration(&interval)
                .with_context(|| format!("invalid interval '{interval}'"))?;
            if secs < 10 {
                bail!("the interval must be at least 10s");
            }
            anstream::println!(
                "Watching {} — a checkpoint every {interval} when files changed. Ctrl-C to stop.",
                repo.workdir()?.display()
            );
            loop {
                match log.checkpoint(Some("watch"), &ctx.argv, false) {
                    Ok(Some(op)) => anstream::println!(
                        "{}  checkpoint {} ({})",
                        veyra_core::time::format_local(op.time),
                        ui::yellow(op.short_id()),
                        plural(op.worktree_changes, "file")
                    ),
                    Ok(None) => {}
                    Err(err) => ui::warn(&format!("checkpoint failed: {err}")),
                }
                std::thread::sleep(std::time::Duration::from_secs(secs as u64));
            }
        }
        CheckpointCommand::Prune { older_than } => prune(&log, &older_than)?,
    }
    Ok(())
}
