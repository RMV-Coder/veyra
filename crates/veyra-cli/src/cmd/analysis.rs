//! `veyra impact`: deterministic change-impact analysis.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use veyra_core::impact::{Confidence, ImpactOptions, ImpactReport, RiskLevel};
use veyra_core::worktree::SnapshotOptions;
use veyra_core::{FlatTree, Repo};

use crate::cli::ImpactArgs;
use crate::{Ctx, ui};

/// The old and new trees for the requested scope, plus a description of it.
fn scope(repo: &Repo, args: &ImpactArgs) -> Result<(FlatTree, FlatTree, String)> {
    if let Some(rev) = &args.commit {
        let id = repo.rev_parse_commit(rev)?;
        let info = repo.commit_info(id)?;
        let old = match info.parents.first() {
            Some(p) => repo.flat_from_commit(*p)?,
            None => FlatTree::new(),
        };
        return Ok((old, repo.flat_from_tree(info.tree)?, format!("commit {}", veyra_core::short(&id))));
    }
    if let Some(base) = &args.branch {
        let base_name = if base.is_empty() {
            repo.default_branch()?.context("no default branch (main/master); name the base: --branch <base>")?
        } else {
            base.clone()
        };
        let base_id = repo.rev_parse_commit(&base_name)?;
        let head = repo.head_commit_id()?.context("HEAD is unborn")?;
        let fork = repo
            .git()
            .merge_base(base_id, head)
            .ok()
            .flatten()
            .map(|m| m.detach())
            .with_context(|| format!("HEAD has no common history with {base_name}"))?;
        return Ok((
            repo.flat_from_commit(fork)?,
            repo.flat_head()?,
            format!("{} since it forked from {base_name}", repo.head_label()?),
        ));
    }
    let head = repo.flat_head()?;
    let index = repo.flat_index()?;
    if args.staged {
        return Ok((head, index, "staged changes".into()));
    }
    // Uncommitted changes: tracked files as they are on disk (the snapshot stores their blobs).
    let snap = repo.snapshot(&SnapshotOptions::default())?;
    let tracked: BTreeSet<_> = head.keys().chain(index.keys()).cloned().collect();
    let new: FlatTree = snap.flat.into_iter().filter(|(p, _)| tracked.contains(p)).collect();
    Ok((head, new, "uncommitted changes".into()))
}

pub fn impact(ctx: &Ctx, args: ImpactArgs) -> Result<()> {
    let repo = ctx.repo()?;
    let (old, new, what) = scope(&repo, &args)?;
    let report = repo.impact(&old, &new, &ImpactOptions { history_limit: args.history })?;
    if ctx.json {
        if args.tests {
            ui::json(&serde_json::to_value(&report.tests)?);
        } else {
            ui::json(&serde_json::to_value(&report)?);
        }
        return Ok(());
    }
    if args.tests {
        for t in &report.tests {
            anstream::println!("{}  {}", t.path, ui::dim(&t.reason));
        }
        if report.tests.is_empty() {
            anstream::println!("No tests found for {what}.");
        }
        return Ok(());
    }
    print_report(&report, &what);
    Ok(())
}

fn print_report(r: &ImpactReport, what: &str) {
    if r.changed_files.is_empty() {
        anstream::println!("No changes in {what}.");
        return;
    }
    let level = match r.risk.level {
        RiskLevel::Low => ui::green("LOW"),
        RiskLevel::Medium => ui::yellow("MEDIUM"),
        RiskLevel::High => ui::red("HIGH"),
    };
    anstream::println!("{} of {what}: risk {level} (score {})", ui::bold("Impact"), r.risk.score);
    for reason in &r.risk.reasons {
        anstream::println!("  • {reason}");
    }

    anstream::println!("\n{}", ui::bold("Changed"));
    for f in &r.changed_files {
        let lang = f.language.as_deref().map(|l| format!("  {}", ui::dim(l))).unwrap_or_default();
        anstream::println!("  {} {}  +{} -{}{lang}", f.status, f.path, f.added_lines, f.removed_lines);
        for s in &f.symbols {
            let name = match &s.old_name {
                Some(old) => format!("{old} → {}", s.name),
                None => s.name.clone(),
            };
            anstream::println!("      {} {name}  {}", s.kind, ui::dim(&s.change));
        }
    }

    if !r.dependents.is_empty() {
        anstream::println!("\n{}", ui::bold("May be affected"));
        for d in r.dependents.iter().take(40) {
            let conf = match d.confidence {
                Confidence::High => ui::red("high  "),
                Confidence::Medium => ui::yellow("medium"),
                Confidence::Low => ui::dim("low   "),
            };
            let test = if d.is_test { ui::dim(" (test)") } else { String::new() };
            anstream::println!("  {conf} {}{test}", d.path);
            for e in d.evidence.iter().take(3) {
                anstream::println!("           {}", ui::dim(e));
            }
        }
        if r.dependents.len() > 40 {
            anstream::println!("  … and {} more (`--json` lists all)", r.dependents.len() - 40);
        }
    }

    anstream::println!("\n{}", ui::bold("Tests to run"));
    if r.tests.is_empty() {
        anstream::println!("  {}", ui::yellow("none found"));
    }
    for t in &r.tests {
        anstream::println!("  {}  {}", t.path, ui::dim(&t.reason));
    }

    if !r.co_changes.is_empty() {
        anstream::println!("\n{}", ui::bold("Usually changed together (not in this change)"));
        for c in &r.co_changes {
            anstream::println!("  {}  {}", c.path, ui::dim(&format!("{} of {} commits", c.together, c.of)));
        }
    }

    anstream::println!("\n{}", ui::dim("Limitations:"));
    for l in &r.limitations {
        anstream::println!("  {}", ui::dim(&format!("- {l}")));
    }
}
