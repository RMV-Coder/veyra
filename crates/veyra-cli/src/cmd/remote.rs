//! remote, fetch, pull, push.

use anyhow::{Context, Result, bail};
use serde_json::json;
use veyra_core::merge::MergeOutcome;

use super::journal;
use crate::cli::{PushArgs, RemoteArgs, RemoteCommand};
use crate::{Ctx, ui};

pub fn remote(ctx: &Ctx, args: RemoteArgs) -> Result<()> {
    let repo = ctx.repo()?;
    match args.command {
        Some(RemoteCommand::Add { name, url }) => {
            journal(ctx, &repo, "remote", false, || {
                repo.remote_add(&name, &url)?;
                Ok(((), format!("add remote {name}")))
            })?;
            anstream::println!("Added remote '{name}' → {url}");
        }
        Some(RemoteCommand::Remove { name }) => {
            journal(ctx, &repo, "remote", false, || {
                repo.remote_remove(&name)?;
                Ok(((), format!("remove remote {name}")))
            })?;
            anstream::println!("Removed remote '{name}'");
        }
        Some(RemoteCommand::List { verbose }) => list(ctx, &repo, verbose)?,
        None => list(ctx, &repo, args.verbose)?,
    }
    Ok(())
}

fn list(ctx: &Ctx, repo: &veyra_core::Repo, verbose: bool) -> Result<()> {
    let remotes = repo.remotes()?;
    if ctx.json {
        ui::json(&json!(
            remotes
                .iter()
                .map(|r| json!({"name": r.name, "fetch": r.fetch_url, "push": r.push_url}))
                .collect::<Vec<_>>()
        ));
        return Ok(());
    }
    for r in remotes {
        if verbose {
            anstream::println!("{}\t{} (fetch)", r.name, r.fetch_url.as_deref().unwrap_or(""));
            anstream::println!("{}\t{} (push)", r.name, r.push_url.as_deref().unwrap_or(""));
        } else {
            anstream::println!("{}", r.name);
        }
    }
    Ok(())
}

pub fn fetch(ctx: &Ctx, remote: Option<String>) -> Result<()> {
    let repo = ctx.repo()?;
    let summary = journal(ctx, &repo, "fetch", false, || {
        let s = repo.fetch(remote.as_deref())?;
        let text = format!("fetch {}: {} ref(s) updated", s.remote, s.updated_refs.len());
        Ok((s, text))
    })?;
    if summary.updated_refs.is_empty() {
        anstream::println!("{}: already up to date", summary.remote);
    } else {
        anstream::println!("From {}", summary.remote);
        for (name, mode) in &summary.updated_refs {
            anstream::println!("  {mode:<12} {}", veyra_core::refs::shorten(name));
        }
    }
    Ok(())
}

pub fn pull(ctx: &Ctx, remote: Option<String>, branch: Option<String>, ff_only: bool) -> Result<()> {
    let repo = ctx.repo()?;
    let current = repo.head_state()?.branch_short().map(str::to_owned).context("cannot pull with a detached HEAD")?;
    let remote_name = match remote {
        Some(r) => r,
        None => repo.config_get(&format!("branch.{current}.remote")).unwrap_or_else(|| "origin".into()),
    };
    let upstream_branch = match branch {
        Some(b) => b,
        None => repo
            .config_get(&format!("branch.{current}.merge"))
            .map(|m| m.trim_start_matches("refs/heads/").to_owned())
            .unwrap_or_else(|| current.clone()),
    };
    fetch(ctx, Some(remote_name.clone()))?;
    let tracking = format!("refs/remotes/{remote_name}/{upstream_branch}");
    let theirs = repo
        .ref_target(&tracking)?
        .with_context(|| format!("'{remote_name}/{upstream_branch}' does not exist after fetching"))?;
    let label = format!("{remote_name}/{upstream_branch}");
    let outcome = journal(ctx, &repo, "pull", true, || {
        let o = repo.merge(theirs, &label, ff_only, false, None)?;
        Ok((o, format!("pull {label}")))
    })?;
    super::report_structural(&repo);
    match outcome {
        MergeOutcome::AlreadyUpToDate => anstream::println!("Already up to date."),
        MergeOutcome::FastForward { to, .. } => {
            anstream::println!("Fast-forwarded {current} to {}", veyra_core::short(&to))
        }
        MergeOutcome::Merged { commit } => {
            anstream::println!("Merged {label} into {current} ({})", veyra_core::short(&commit))
        }
        MergeOutcome::Conflicts { paths } => {
            for p in &paths {
                anstream::println!("CONFLICT: {p}");
            }
            bail!("pull stopped with conflicts; resolve and `veyra commit`, or `veyra merge --abort`");
        }
    }
    Ok(())
}

pub fn push(ctx: &Ctx, args: PushArgs) -> Result<()> {
    use veyra_core::push::{PushOptions, PushSpec, RefStatus, parse_refspec};

    let repo = ctx.repo()?;
    if args.force {
        ctx.confirm_destructive("push --force")?;
    }
    let current = repo.head_state()?.branch_short().map(str::to_owned);

    // Which remote: explicit, then branch.<b>.pushRemote, remote.pushDefault, branch.<b>.remote, origin.
    let remote_name = match &args.remote {
        Some(r) => r.clone(),
        None => current
            .as_ref()
            .and_then(|b| repo.config_get(&format!("branch.{b}.pushRemote")))
            .or_else(|| repo.config_get("remote.pushDefault"))
            .or_else(|| current.as_ref().and_then(|b| repo.config_get(&format!("branch.{b}.remote"))))
            .unwrap_or_else(|| "origin".into()),
    };
    if !repo.remotes()?.iter().any(|r| r.name == remote_name) {
        bail!("no such remote '{remote_name}'\n  hint: veyra remote add {remote_name} <url>");
    }

    // Which refs.
    let mut refspecs = args.refspecs.clone();
    if refspecs.is_empty() && !args.tags {
        let branch = current
            .clone()
            .context("HEAD is detached; say what to push, e.g. `veyra push origin HEAD:refs/heads/topic`")?;
        let upstream = repo
            .config_get(&format!("branch.{branch}.merge"))
            .filter(|_| repo.config_get(&format!("branch.{branch}.remote")).as_deref() == Some(remote_name.as_str()))
            .map(|m| m.trim_start_matches("refs/heads/").to_owned());
        refspecs.push(match upstream {
            Some(up) if up != branch => format!("{branch}:{up}"),
            _ => branch,
        });
    }
    let mut specs = Vec::new();
    for raw in &refspecs {
        let raw = if args.delete { format!(":{}", raw.trim_start_matches(':')) } else { raw.clone() };
        let (src, dst, plus) = parse_refspec(&repo, &raw)?;
        let id = src.as_deref().map(|s| repo.rev_parse(s)).transpose()?;
        specs.push(PushSpec {
            src: id,
            src_label: src.as_deref().map(|s| veyra_core::refs::shorten(s).to_owned()).unwrap_or_default(),
            dst,
            force: plus || args.force,
        });
    }
    if args.tags {
        for (name, id) in repo.tags()? {
            let full = format!("refs/tags/{name}");
            if !specs.iter().any(|s| s.dst == full) {
                specs.push(PushSpec { src: Some(id), src_label: name, dst: full, force: false });
            }
        }
    }

    let options = PushOptions { force_with_lease: args.force_with_lease, dry_run: args.dry_run };
    let outcome = journal(ctx, &repo, "push", false, || {
        // pre-push hook: `<remote> <url>`, stdin lines `<local ref> <local sha> <remote ref> <remote sha>`.
        let mut pre_push = |url: &str, plan: &[veyra_core::push::RefResult]| -> Result<()> {
            let null = veyra_core::ObjectId::null(repo.object_hash());
            let mut stdin = String::new();
            for r in plan
                .iter()
                .filter(|r| matches!(r.status, RefStatus::Created | RefStatus::Updated { .. } | RefStatus::Deleted))
            {
                let local_ref = if r.new.is_some() { r.src_label.as_str() } else { "(delete)" };
                stdin.push_str(&format!("{local_ref} {} {} {}\n", r.new.unwrap_or(null), r.dst, r.old.unwrap_or(null)));
            }
            if args.no_verify {
                return Ok(());
            }
            repo.run_hook("pre-push", &[&remote_name, url], Some(stdin.as_bytes())).map(|_| ())
        };
        let o = repo.push(&remote_name, &specs, &options, &mut pre_push)?;
        let accepted = o.results.iter().filter(|r| r.is_success() && r.status != RefStatus::UpToDate).count();
        Ok((o, format!("push to {remote_name}: {accepted} ref(s) updated")))
    })?;

    if ctx.json {
        ui::json(&json!({
            "remote": outcome.remote, "url": outcome.url, "dry_run": args.dry_run,
            "objects": outcome.objects_sent, "bytes": outcome.pack_bytes,
            "refs": outcome.results.iter().map(|r| json!({
                "ref": r.dst, "source": r.src_label,
                "old": r.old.map(|o| o.to_string()), "new": r.new.map(|n| n.to_string()),
                "status": format!("{:?}", r.status),
            })).collect::<Vec<_>>(),
        }));
    } else {
        anstream::println!("To {}", outcome.url);
        for r in &outcome.results {
            let dst = veyra_core::refs::shorten(&r.dst);
            let arrow = if r.src_label.is_empty() { dst.to_owned() } else { format!("{} -> {dst}", r.src_label) };
            let short = |o: Option<veyra_core::ObjectId>| o.map(|o| veyra_core::short(&o)).unwrap_or_default();
            let line = match &r.status {
                RefStatus::UpToDate => format!(" = {:<17} {arrow}", "[up to date]"),
                RefStatus::Created if r.dst.starts_with("refs/tags/") => {
                    ui::green(&format!(" * {:<17} {arrow}", "[new tag]"))
                }
                RefStatus::Created => ui::green(&format!(" * {:<17} {arrow}", "[new branch]")),
                RefStatus::Updated { forced: false } => {
                    format!("   {:<17} {arrow}", format!("{}..{}", short(r.old), short(r.new)))
                }
                RefStatus::Updated { forced: true } => ui::yellow(&format!(
                    " + {:<17} {arrow} (forced update)",
                    format!("{}...{}", short(r.old), short(r.new))
                )),
                RefStatus::Deleted => ui::red(&format!(" - {:<17} {dst}", "[deleted]")),
                RefStatus::RejectedLocally(why) | RefStatus::RejectedByRemote(why) => {
                    ui::red(&format!(" ! {:<17} {arrow} ({why})", "[rejected]"))
                }
            };
            anstream::println!("{line}");
        }
        if args.dry_run {
            anstream::println!("{}", ui::dim("(dry run: nothing was sent)"));
        } else if outcome.objects_sent > 0 {
            anstream::println!(
                "{}",
                ui::dim(&format!(
                    "{} objects ({} as deltas), {} sent",
                    outcome.objects_sent,
                    outcome.deltas,
                    human_bytes(outcome.pack_bytes)
                ))
            );
        }
    }

    if args.set_upstream && !args.dry_run {
        let config = repo.config_path(veyra_core::config::Scope::Local)?;
        for (spec, result) in specs.iter().zip(&outcome.results) {
            if !result.is_success() || !spec.dst.starts_with("refs/heads/") {
                continue;
            }
            let Some(branch) = current.as_ref().filter(|b| spec.src_label == **b) else { continue };
            veyra_core::config::set_in_file(&config, &format!("branch.{branch}.remote"), &remote_name)?;
            veyra_core::config::set_in_file(&config, &format!("branch.{branch}.merge"), &spec.dst)?;
            anstream::println!(
                "branch '{branch}' set up to track '{remote_name}/{}'.",
                veyra_core::refs::shorten(&spec.dst)
            );
        }
    }
    if !outcome.all_succeeded() {
        if outcome.results.iter().any(|r| matches!(&r.status, RefStatus::RejectedLocally(w) if w.starts_with("non-fast-forward") || w.starts_with("fetch first"))) {
            ui::hint("the remote has commits you don't have: `veyra pull`, then push again");
        }
        return Err(crate::Reported.into());
    }
    Ok(())
}

fn human_bytes(b: u64) -> String {
    match b {
        b if b >= 1 << 20 => format!("{:.1} MiB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KiB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

/// `veyra sync`: fetch, then bring the current branch up to date with its upstream when the
/// working tree is clean — fast-forward if possible, otherwise rebase. Pushes only with --push.
pub fn sync(ctx: &Ctx, push_after: bool) -> Result<()> {
    let repo = ctx.repo()?;
    let branch = repo.head_state()?.branch_short().map(str::to_owned).context("cannot sync a detached HEAD")?;
    let remote = repo
        .config_get(&format!("branch.{branch}.remote"))
        .with_context(|| format!("'{branch}' has no upstream\n  hint: `veyra push -u` sets one"))?;
    let merge = repo.config_get(&format!("branch.{branch}.merge")).unwrap_or_else(|| format!("refs/heads/{branch}"));
    let upstream_branch = merge.trim_start_matches("refs/heads/").to_owned();
    fetch(ctx, Some(remote.clone()))?;
    let tracking = format!("refs/remotes/{remote}/{upstream_branch}");
    let ours = repo.head_commit_id()?.context("the branch has no commits")?;
    match repo.ref_target(&tracking)? {
        None => anstream::println!("{remote}/{upstream_branch} doesn't exist yet."),
        Some(theirs) => {
            let (ahead, behind) = repo.ahead_behind(ours, theirs)?;
            if behind == 0 {
                anstream::println!("{branch} is up to date with {remote}/{upstream_branch} ({ahead} ahead).");
            } else {
                let changes = repo.worktree_changes()?;
                if !changes.is_clean() || repo.flat_index()? != repo.flat_head()? {
                    bail!(
                        "{branch} is {behind} behind, but you have uncommitted changes\n  hint: commit or `veyra stash` them, then `veyra sync` again"
                    );
                }
                let label = format!("{remote}/{upstream_branch}");
                if ahead == 0 {
                    journal(ctx, &repo, "sync", true, || {
                        repo.merge(theirs, &label, true, false, None)?;
                        Ok(((), format!("sync: fast-forward to {label}")))
                    })?;
                    anstream::println!("Fast-forwarded {branch} to {label} ({}).", veyra_core::short(&theirs));
                } else {
                    super::sequence::rebase(ctx, Some(tracking.clone()), None, false, false, false, false)?;
                }
            }
        }
    }
    if push_after {
        push(
            ctx,
            PushArgs {
                remote: None,
                refspecs: vec![],
                force: false,
                force_with_lease: false,
                set_upstream: false,
                tags: false,
                delete: false,
                dry_run: false,
                no_verify: false,
            },
        )?;
    }
    Ok(())
}
