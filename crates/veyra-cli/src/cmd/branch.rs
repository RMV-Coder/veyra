//! branch, switch, checkout, merge, reset, tag.

use anyhow::{Context, Result, bail};
use serde_json::json;
use veyra_core::Repo;
use veyra_core::checkout::{ResetMode, SwitchTarget};
use veyra_core::merge::MergeOutcome;
use veyra_core::refs::{branch_full_name, tag_full_name};

use super::{journal, pathspec, plural};
use crate::cli::{BranchArgs, BranchCommand, TagArgs, TagCommand};
use crate::{Ctx, ui};

pub fn branch(ctx: &Ctx, args: BranchArgs) -> Result<()> {
    let repo = ctx.repo()?;
    if let Some(cmd) = args.command {
        return match cmd {
            BranchCommand::List { all } => list(ctx, &repo, all, true),
            BranchCommand::Create { name, start } => create(ctx, &repo, &name, start.as_deref()),
            BranchCommand::Delete { name, force } => delete(ctx, &repo, &name, force),
            BranchCommand::Rename { old, new } => rename(ctx, &repo, &old, &new),
            BranchCommand::Stale { days } => stale(ctx, &repo, days),
            BranchCommand::Cleanup { dry_run } => cleanup(ctx, &repo, dry_run),
            BranchCommand::Compare { branch, base } => compare(ctx, &repo, &branch, base.as_deref()),
        };
    }
    if args.delete || args.force_delete {
        let name = args.name.context("branch name required")?;
        return delete(ctx, &repo, &name, args.force_delete);
    }
    if args.rename {
        return match (args.name, args.start) {
            (Some(old), Some(new)) => rename(ctx, &repo, &old, &new),
            (Some(new), None) => {
                let old = repo
                    .head_state()?
                    .branch_short()
                    .map(str::to_owned)
                    .context("HEAD is detached; name the branch to rename")?;
                rename(ctx, &repo, &old, &new)
            }
            _ => bail!("usage: veyra branch -m [<old>] <new>"),
        };
    }
    match args.name {
        Some(name) => create(ctx, &repo, &name, args.start.as_deref()),
        None => list(ctx, &repo, args.all, args.verbose),
    }
}

fn list(ctx: &Ctx, repo: &Repo, all: bool, verbose: bool) -> Result<()> {
    let branches = repo.branches(all)?;
    if ctx.json {
        let items: Vec<_> = branches
            .iter()
            .map(|b| json!({"name": b.name, "ref": b.full_name, "target": b.target.to_string(), "current": b.is_head, "remote": b.is_remote}))
            .collect();
        ui::json(&json!(items));
        return Ok(());
    }
    if let veyra_core::HeadState::Detached(id) = repo.head_state()? {
        anstream::println!("* {}", ui::green(&format!("(HEAD detached at {})", veyra_core::short(&id))));
    }
    let width = branches.iter().map(|b| b.name.len() + if b.is_remote { 8 } else { 0 }).max().unwrap_or(0);
    for b in branches {
        let name = if b.is_remote { format!("remotes/{}", b.name) } else { b.name.clone() };
        let marker = if b.is_head { "* " } else { "  " };
        let painted = if b.is_head {
            ui::green(&name)
        } else if b.is_remote {
            ui::red(&name)
        } else {
            name.clone()
        };
        if verbose {
            let summary = repo.commit_info(b.target).map(|c| c.summary().to_owned()).unwrap_or_default();
            let pad = " ".repeat(width.saturating_sub(name.len()));
            anstream::println!("{marker}{painted}{pad} {} {summary}", ui::yellow(&veyra_core::short(&b.target)));
        } else {
            anstream::println!("{marker}{painted}");
        }
    }
    Ok(())
}

fn create(ctx: &Ctx, repo: &Repo, name: &str, start: Option<&str>) -> Result<()> {
    let full = branch_full_name(name)?;
    let target = match start {
        Some(s) => repo.rev_parse_commit(s)?,
        None => repo.head_commit_id()?.context("cannot create a branch: the current branch has no commits yet")?,
    };
    if repo.ref_target(&full)?.is_some() {
        bail!("a branch named '{name}' already exists");
    }
    journal(ctx, repo, "branch", false, || {
        repo.set_ref(&full, target, Some(None), &format!("branch: Created from {}", start.unwrap_or("HEAD")))?;
        Ok(((), format!("create branch {name} at {}", veyra_core::short(&target))))
    })?;
    anstream::println!("Created branch {} at {}", ui::green(name), veyra_core::short(&target));
    Ok(())
}

fn delete(ctx: &Ctx, repo: &Repo, name: &str, force: bool) -> Result<()> {
    let full = branch_full_name(name)?;
    let target = repo.ref_target(&full)?.with_context(|| format!("branch '{name}' not found"))?;
    if repo.head_state()?.branch_short() == Some(name) {
        bail!("cannot delete branch '{name}': it is checked out");
    }
    if !force {
        let head = repo.head_commit_id()?;
        let merged = head.is_some_and(|h| repo.is_ancestor(target, h).unwrap_or(false));
        if !merged {
            bail!(
                "the branch '{name}' is not fully merged into HEAD\n  hint: `veyra branch -D {name}` deletes it anyway (`veyra undo` restores it)"
            );
        }
    } else {
        ctx.confirm_destructive("branch -D")?;
    }
    journal(ctx, repo, "branch", false, || {
        repo.delete_ref(&full, Some(target))?;
        Ok(((), format!("delete branch {name} (was {})", veyra_core::short(&target))))
    })?;
    anstream::println!("Deleted branch {name} (was {}). `veyra undo` restores it.", veyra_core::short(&target));
    Ok(())
}

fn rename(ctx: &Ctx, repo: &Repo, old: &str, new: &str) -> Result<()> {
    let (old_full, new_full) = (branch_full_name(old)?, branch_full_name(new)?);
    journal(ctx, repo, "branch", false, || {
        repo.rename_branch(&old_full, &new_full)?;
        Ok(((), format!("rename branch {old} → {new}")))
    })?;
    anstream::println!("Renamed branch {old} → {}", ui::green(new));
    Ok(())
}

fn stale(ctx: &Ctx, repo: &Repo, days: i64) -> Result<()> {
    let now = veyra_core::time::now_seconds();
    let base_name = repo.default_branch()?;
    let base = match &base_name {
        Some(b) => repo.ref_target(&format!("refs/heads/{b}"))?,
        None => repo.head_commit_id()?,
    };
    let mut rows = Vec::new();
    for b in repo.branches(false)? {
        if Some(&b.name) == base_name.as_ref() {
            continue;
        }
        let info = repo.commit_info(b.target)?;
        let age_days = (now - info.committer_time) / 86_400;
        if age_days < days {
            continue;
        }
        let merged = base.is_some_and(|base| repo.is_ancestor(b.target, base).unwrap_or(false));
        rows.push((b.name, age_days, merged));
    }
    if ctx.json {
        ui::json(&json!(
            rows.iter()
                .map(|(n, a, m)| json!({"branch": n, "days_since_last_commit": a, "merged": m}))
                .collect::<Vec<_>>()
        ));
        return Ok(());
    }
    if rows.is_empty() {
        anstream::println!("No branches without commits for {days}+ days.");
        return Ok(());
    }
    let base_label = base_name.unwrap_or_else(|| "HEAD".into());
    anstream::println!("{}", ui::bold(&format!("STALE BRANCHES (no commits for {days}+ days)")));
    for (name, age, merged) in rows {
        anstream::println!(
            "\n{name}\n  Last commit:     {age} days ago\n  Merged into {base_label}: {}\n  Safe to delete:  {}",
            if merged { "yes" } else { "no" },
            if merged {
                ui::green("likely — `veyra branch -d` will accept it")
            } else {
                ui::yellow("needs review (unmerged work)")
            }
        );
    }
    Ok(())
}

fn resolve_switch_target(repo: &Repo, name: &str) -> Result<SwitchTarget> {
    let full = format!("refs/heads/{name}");
    if repo.ref_target(&full)?.is_some() {
        return Ok(SwitchTarget::Branch(full));
    }
    // `veyra switch feature` with only `origin/feature`: create a tracking branch like git (DWIM).
    let remotes: Vec<_> = repo
        .branches(true)?
        .into_iter()
        .filter(|b| b.is_remote && b.name.split_once('/').map(|x| x.1) == Some(name))
        .collect();
    if let [remote] = remotes.as_slice() {
        repo.set_ref(&full, remote.target, Some(None), &format!("branch: Created from {}", remote.name))?;
        let (remote_name, _) = remote.name.split_once('/').expect("checked");
        let config = repo.config_path(veyra_core::config::Scope::Local)?;
        veyra_core::config::set_in_file(&config, &format!("branch.{name}.remote"), remote_name)?;
        veyra_core::config::set_in_file(&config, &format!("branch.{name}.merge"), &full)?;
        anstream::println!("branch '{name}' set up to track '{}'", remote.name);
        return Ok(SwitchTarget::Branch(full));
    }
    bail!(
        "invalid reference: {name}\n  hint: `veyra switch -c {name}` creates it, `veyra switch --detach <commit>` checks out a commit"
    );
}

pub fn switch(ctx: &Ctx, branch: Option<String>, create: Option<String>, detach: bool, force: bool) -> Result<()> {
    let repo = ctx.repo()?;
    if force {
        ctx.confirm_destructive("switch --force")?;
    }
    if let Some(new) = create {
        let full = branch_full_name(&new)?;
        if repo.ref_target(&full)?.is_some() {
            bail!("a branch named '{new}' already exists");
        }
        let start = match &branch {
            Some(s) => Some(repo.rev_parse_commit(s)?),
            None => repo.head_commit_id()?,
        };
        let previous = repo.head_commit_id()?;
        journal(ctx, &repo, "switch", true, || {
            if let Some(start) = start {
                repo.set_ref(
                    &full,
                    start,
                    Some(None),
                    &format!("branch: Created from {}", branch.as_deref().unwrap_or("HEAD")),
                )?;
            }
            repo.switch(SwitchTarget::Branch(full.clone()), force)?;
            Ok(((), format!("create and switch to {new}")))
        })?;
        anstream::println!("Switched to a new branch '{}'", ui::green(&new));
        post_checkout(&repo, previous);
        return Ok(());
    }
    let name = branch.context("which branch? usage: veyra switch <branch> | -c <new> | --detach <commit>")?;
    let name = if name == "-" { previous_branch(&repo)? } else { name };
    let target =
        if detach { SwitchTarget::Detach(repo.rev_parse_commit(&name)?) } else { resolve_switch_target(&repo, &name)? };
    if let SwitchTarget::Branch(full) = &target
        && repo.head_state()?
            == (veyra_core::HeadState::Branch { name: full.as_str().into(), target: repo.ref_target(full)? })
    {
        anstream::println!("Already on '{name}'");
        return Ok(());
    }
    let previous = repo.head_commit_id()?;
    journal(ctx, &repo, "switch", true, || {
        repo.switch(target.clone(), force)?;
        Ok(((), format!("switch to {name}")))
    })?;
    post_checkout(&repo, previous);
    if detach {
        anstream::println!("HEAD is now at {} (detached)", name);
    } else {
        anstream::println!("Switched to branch '{}'", ui::green(&name));
    }
    Ok(())
}

/// Run the `post-checkout` hook like git: `<previous HEAD> <new HEAD> 1` (1 = branch checkout).
fn post_checkout(repo: &Repo, previous: Option<veyra_core::ObjectId>) {
    let null = veyra_core::ObjectId::null(repo.object_hash()).to_string();
    let old = previous.map(|p| p.to_string()).unwrap_or_else(|| null.clone());
    let new = repo.head_commit_id().ok().flatten().map(|n| n.to_string()).unwrap_or(null);
    if let Some(err) = repo.run_post_hook("post-checkout", &[&old, &new, "1"]) {
        ui::warn(&err);
    }
}

/// `veyra switch -`: the branch checked out before the current one (from HEAD's reflog).
fn previous_branch(repo: &Repo) -> Result<String> {
    let head = repo.git().head()?;
    let prior = head.prior_checked_out_branches()?.unwrap_or_default();
    prior
        .into_iter()
        .map(|(name, _)| name.to_string())
        .find(|n| Some(n.as_str()) != repo.head_state().ok().as_ref().and_then(|h| h.branch_short()))
        .context("no previous branch")
}

pub fn checkout(
    ctx: &Ctx,
    target: Option<String>,
    new_branch: Option<String>,
    force: bool,
    paths: Vec<String>,
) -> Result<()> {
    if !paths.is_empty() {
        return super::repo::restore(
            ctx,
            crate::cli::RestoreArgs { paths, staged: false, worktree: false, source: target },
        );
    }
    if let Some(new) = new_branch {
        return switch(ctx, target, Some(new), false, force);
    }
    let target = target.context("usage: veyra checkout <branch> | -b <new> | -- <paths>")?;
    let repo = ctx.repo()?;
    if repo.ref_target(&format!("refs/heads/{target}"))?.is_some() {
        return switch(ctx, Some(target), None, false, force);
    }
    if repo.rev_parse_commit(&target).is_ok() {
        return switch(ctx, Some(target), None, true, force);
    }
    // Not a branch or commit: maybe a path, like `git checkout file.txt`.
    if repo.flat_index()?.keys().any(|p| {
        veyra_core::flat::path_is_within(p.as_ref(), repo.to_repo_path(&ctx.cwd, &target).unwrap_or_default().as_ref())
    }) {
        return super::repo::restore(
            ctx,
            crate::cli::RestoreArgs { paths: vec![target], staged: false, worktree: false, source: None },
        );
    }
    resolve_switch_target(&repo, &target).map(|_| ()).and_then(|_| switch(ctx, Some(target), None, false, force))
}

pub fn merge(
    ctx: &Ctx,
    revision: Option<String>,
    ff_only: bool,
    no_ff: bool,
    message: Option<String>,
    abort: bool,
    continue_: bool,
) -> Result<()> {
    let repo = ctx.repo()?;
    if abort {
        let log = veyra_ops::OpLog::open(&repo)?;
        log.abort_merge(&ctx.argv)?;
        anstream::println!("Merge aborted; the repository is back to its state before the merge.");
        return Ok(());
    }
    if continue_ {
        return super::repo::commit(
            ctx,
            crate::cli::CommitArgs {
                message: message.into_iter().collect(),
                file: None,
                all: false,
                amend: false,
                no_edit: true,
                allow_empty: false,
                trailer: vec![],
                assisted_by: None,
                intent: vec![],
                allow_secrets: false,
                no_verify: false,
            },
        );
    }
    let rev = revision.context("usage: veyra merge <branch> | --abort | --continue")?;
    let theirs = repo.rev_parse_commit(&rev)?;
    let outcome = journal(ctx, &repo, "merge", true, || {
        let outcome = repo.merge(
            theirs,
            &rev,
            ff_only,
            no_ff,
            message.clone().map(|m| veyra_core::commit::clean_message(&m, false)),
        )?;
        let summary = match &outcome {
            MergeOutcome::AlreadyUpToDate => format!("merge {rev}: already up to date"),
            MergeOutcome::FastForward { to, .. } => format!("merge {rev}: fast-forward to {}", veyra_core::short(to)),
            MergeOutcome::Merged { commit } => format!("merge {rev}: {}", veyra_core::short(commit)),
            MergeOutcome::Conflicts { paths } => {
                format!("merge {rev}: stopped with {}", plural(paths.len(), "conflict"))
            }
        };
        Ok((outcome, summary))
    })?;
    match outcome {
        MergeOutcome::AlreadyUpToDate => anstream::println!("Already up to date."),
        MergeOutcome::FastForward { from, to } => anstream::println!(
            "Updating {}..{}\nFast-forward",
            from.map(|f| veyra_core::short(&f)).unwrap_or_else(|| "(unborn)".into()),
            veyra_core::short(&to)
        ),
        MergeOutcome::Merged { commit } => anstream::println!("Merge made: {}", veyra_core::short(&commit)),
        MergeOutcome::Conflicts { paths } => {
            for p in &paths {
                anstream::println!("CONFLICT (content): merge conflict in {p}");
            }
            anstream::println!(
                "Automatic merge failed; fix conflicts, `veyra add` them, then `veyra commit`.\n`veyra merge --abort` (or `veyra undo`) returns to the state before the merge."
            );
            return Err(crate::Reported.into());
        }
    }
    if let Some(err) = repo.run_post_hook("post-merge", &["0"]) {
        ui::warn(&err);
    }
    Ok(())
}

pub fn reset(ctx: &Ctx, soft: bool, hard: bool, target: Option<String>, paths: Vec<String>) -> Result<()> {
    let repo = ctx.repo()?;
    // `veyra reset <paths>` / `veyra reset -- <paths>`: unstage.
    let target_is_commit = target.as_deref().is_some_and(|t| repo.rev_parse_commit(t).is_ok());
    if !paths.is_empty() || (target.is_some() && !target_is_commit) {
        let mut all = paths;
        if let Some(t) = target.filter(|_| !target_is_commit) {
            all.insert(0, t);
        }
        let spec = pathspec(ctx, &repo, &all)?;
        let changed = journal(ctx, &repo, "unstage", false, || {
            let changed = repo.restore_staged(&spec, None)?;
            let n = changed.len();
            Ok((changed, format!("unstage {}", plural(n, "path"))))
        })?;
        for p in changed {
            anstream::println!("Unstaged {p}");
        }
        return Ok(());
    }
    let mode = if soft {
        ResetMode::Soft
    } else if hard {
        ResetMode::Hard
    } else {
        ResetMode::Mixed
    };
    if mode == ResetMode::Hard {
        ctx.confirm_destructive("reset --hard")?;
    }
    let spec = target.unwrap_or_else(|| "HEAD".into());
    let commit = repo.rev_parse_commit(&spec)?;
    journal(ctx, &repo, "reset", mode == ResetMode::Hard, || {
        repo.reset(commit, mode)?;
        Ok(((), format!("reset --{} to {}", format!("{mode:?}").to_lowercase(), veyra_core::short(&commit))))
    })?;
    let summary = repo.commit_info(commit)?.summary().to_owned();
    anstream::println!("HEAD is now at {} {summary}", veyra_core::short(&commit));
    if mode == ResetMode::Hard {
        ui::hint("local changes were checkpointed first; `veyra undo` brings them back");
    }
    Ok(())
}

pub fn tag(ctx: &Ctx, args: TagArgs) -> Result<()> {
    let repo = ctx.repo()?;
    let (name, target, message, delete) = match args.command {
        Some(TagCommand::List) => (None, None, None, false),
        Some(TagCommand::Create { name, target, message }) => (Some(name), target, message, false),
        Some(TagCommand::Delete { name }) => (Some(name), None, None, true),
        None => (args.name, args.target, args.message.or(args.annotate.then(String::new)), args.delete),
    };
    let Some(name) = name else {
        let tags = repo.tags()?;
        if ctx.json {
            ui::json(&json!(
                tags.iter().map(|(n, id)| json!({"name": n, "target": id.to_string()})).collect::<Vec<_>>()
            ));
        } else {
            for (n, _) in tags {
                anstream::println!("{n}");
            }
        }
        return Ok(());
    };
    let full = tag_full_name(&name)?;
    if delete {
        let old = repo.ref_target(&full)?.with_context(|| format!("tag '{name}' not found"))?;
        journal(ctx, &repo, "tag", false, || {
            repo.delete_ref(&full, Some(old))?;
            Ok(((), format!("delete tag {name}")))
        })?;
        anstream::println!("Deleted tag '{name}' (was {})", veyra_core::short(&old));
        return Ok(());
    }
    let target_id = repo.rev_parse_commit(target.as_deref().unwrap_or("HEAD"))?;
    let exists = repo.ref_target(&full)?;
    if exists.is_some() && !args.force {
        bail!("tag '{name}' already exists (use --force to move it)");
    }
    journal(ctx, &repo, "tag", false, || {
        match &message {
            Some(msg) => {
                repo.require_identity()?;
                let msg =
                    if msg.is_empty() { format!("{name}\n") } else { veyra_core::commit::clean_message(msg, false) };
                let tagger = repo.git().committer().context("no identity")??;
                let constraint = if exists.is_some() {
                    veyra_core::gix::refs::transaction::PreviousValue::Any
                } else {
                    veyra_core::gix::refs::transaction::PreviousValue::MustNotExist
                };
                repo.git().tag(
                    &name,
                    target_id,
                    veyra_core::gix::objs::Kind::Commit,
                    Some(tagger),
                    &msg,
                    constraint,
                )?;
            }
            None => repo.set_ref(&full, target_id, if exists.is_some() { None } else { Some(None) }, "tag")?,
        }
        Ok(((), format!("tag {name} at {}", veyra_core::short(&target_id))))
    })?;
    anstream::println!("Tagged {} as {}", veyra_core::short(&target_id), ui::green(&name));
    Ok(())
}

fn cleanup(ctx: &Ctx, repo: &Repo, dry_run: bool) -> Result<()> {
    let base_name = repo.default_branch()?.context("no default branch (main/master) to compare against")?;
    let base = repo.ref_target(&format!("refs/heads/{base_name}"))?.context("default branch vanished")?;
    let merged: Vec<_> = repo
        .branches(false)?
        .into_iter()
        .filter(|b| !b.is_head && b.name != base_name && repo.is_ancestor(b.target, base).unwrap_or(false))
        .collect();
    if merged.is_empty() {
        anstream::println!("No branches are fully merged into {base_name}.");
        return Ok(());
    }
    if dry_run {
        for b in &merged {
            anstream::println!("Would delete {} ({})", b.name, veyra_core::short(&b.target));
        }
        return Ok(());
    }
    let edits: Vec<(String, Option<veyra_core::ObjectId>)> =
        merged.iter().map(|b| (b.full_name.clone(), None)).collect();
    journal(ctx, repo, "branch", false, || {
        repo.set_refs(&edits, &format!("branch cleanup: merged into {base_name}"))?;
        Ok(((), format!("cleanup: delete {} merged into {base_name}", plural(merged.len(), "branch"))))
    })?;
    for b in &merged {
        anstream::println!("Deleted {} (was {})", b.name, veyra_core::short(&b.target));
    }
    ui::hint("`veyra undo` restores them all");
    Ok(())
}

fn compare(ctx: &Ctx, repo: &Repo, branch: &str, base: Option<&str>) -> Result<()> {
    let a = repo.rev_parse_commit(branch)?;
    let base_label = match base {
        Some(b) => b.to_owned(),
        None => repo.head_label()?,
    };
    let b = repo.rev_parse_commit(base.unwrap_or("HEAD"))?;
    let (ahead, behind) = repo.ahead_behind(a, b)?;
    let merge_base = repo.git().merge_base(a, b).ok().flatten().map(|m| m.detach());
    let changed = match merge_base {
        Some(mb) => veyra_core::flat::diff(&repo.flat_from_commit(mb)?, &repo.flat_from_commit(a)?),
        None => Vec::new(),
    };
    if ctx.json {
        ui::json(&json!({
            "branch": branch, "base": base_label, "ahead": ahead, "behind": behind,
            "merge_base": merge_base.map(|m| m.to_string()),
            "changed_files": changed.iter().map(|c| c.path.to_string()).collect::<Vec<_>>(),
        }));
        return Ok(());
    }
    anstream::println!("{}", ui::bold(&format!("{branch} vs {base_label}")));
    anstream::println!("  Commits ahead:   {ahead}");
    anstream::println!("  Commits behind:  {behind}");
    match merge_base {
        Some(mb) => anstream::println!("  Merge base:      {}", veyra_core::short(&mb)),
        None => anstream::println!("  Merge base:      none (unrelated histories)"),
    }
    anstream::println!("  Changed files:   {} (since the merge base)", changed.len());
    for c in changed.iter().take(20) {
        anstream::println!("    {} {}", c.status_letter(), c.path);
    }
    if changed.len() > 20 {
        anstream::println!("    … and {} more", changed.len() - 20);
    }
    let advice = match (ahead, behind) {
        (0, 0) => "identical".to_owned(),
        (0, _) => format!("{branch} is behind; `veyra switch {branch} && veyra merge {base_label}` fast-forwards it"),
        (_, 0) => format!("fast-forwardable: `veyra merge {branch}` on {base_label}"),
        _ => format!("diverged: `veyra rebase {base_label}` on {branch}, or merge"),
    };
    anstream::println!("  Suggested:       {advice}");
    Ok(())
}
