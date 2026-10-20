//! remote, fetch, pull, push.

use anyhow::{Context, Result, bail};
use serde_json::json;
use veyra_core::merge::MergeOutcome;

use super::journal;
use crate::cli::{RemoteArgs, RemoteCommand};
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

pub fn push(_ctx: &Ctx) -> Result<()> {
    bail!(
        "`veyra push` is not implemented yet (planned for Phase 2: gitoxide has no push, so Veyra will build it on gix-protocol).\n  \
         Veyra and Git share the same repository, so `git push` works right now; `veyra undo` keeps working afterwards."
    )
}
