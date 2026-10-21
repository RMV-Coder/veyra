//! `veyra impact`: deterministic change-impact analysis.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use veyra_core::impact::{Confidence, ImpactOptions, ImpactReport, RiskLevel};
use veyra_core::worktree::SnapshotOptions;
use veyra_core::{FlatTree, Repo};

use crate::cli::{AnalyzeArgs, ImpactArgs, ScopeArgs};
use crate::{Ctx, ui};

/// The old and new trees for the requested scope, plus a description of it.
pub(crate) fn scope(repo: &Repo, args: &ScopeArgs) -> Result<(FlatTree, FlatTree, String)> {
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
    let (old, new, what) = scope(&repo, &args.scope)?;
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

/// The committed repository rules for `veyra analyze`.
pub const ANALYZERS_FILE: &str = ".veyra/analyzers.toml";

pub fn analyze(ctx: &Ctx, args: AnalyzeArgs) -> Result<()> {
    use veyra_analyze::{Analyzer, Area, ChangedFile, Severity};
    use veyra_core::ByteSlice;
    let repo = ctx.repo()?;
    let threshold = match &args.fail_on {
        Some(s) => Some(Severity::parse(s).context("--fail-on takes high, medium or low")?),
        None => None,
    };
    let (old, new, what) = scope(&repo, &args.scope)?;
    let analyzer = match std::fs::read_to_string(repo.workdir()?.join(ANALYZERS_FILE)) {
        Ok(text) => Analyzer::from_toml(&text)?,
        Err(_) => Analyzer::new(&Default::default())?,
    };
    let text = |e: Option<veyra_core::FlatEntry>| -> Result<Option<String>> {
        Ok(match e {
            Some(e)
                if matches!(
                    e.kind,
                    veyra_core::gix::objs::tree::EntryKind::Blob
                        | veyra_core::gix::objs::tree::EntryKind::BlobExecutable
                ) =>
            {
                let data = repo.read_blob(e.id)?;
                (!veyra_core::diff::is_binary(&data)).then(|| data.to_str_lossy().into_owned())
            }
            _ => None,
        })
    };
    let changes = veyra_core::flat::diff(&old, &new);
    let mut contents = Vec::new();
    for c in &changes {
        contents.push((c.path.to_str_lossy().into_owned(), text(c.old)?, text(c.new)?));
    }
    let files: Vec<ChangedFile<'_>> =
        contents.iter().map(|(p, o, n)| ChangedFile { path: p, old: o.as_deref(), new: n.as_deref() }).collect();
    let both = !args.migration && !args.infra;
    let mut findings: Vec<_> = analyzer
        .analyze(&files)
        .into_iter()
        .filter(|f| match f.area {
            Area::Migration => both || args.migration,
            Area::Infra => both || args.infra,
            Area::Custom | Area::Plugin => true,
        })
        .collect();
    let plugins_failed = run_plugins(&repo, &analyzer, &args, &files, &mut findings)?;
    findings.sort_by(|a, b| a.severity.cmp(&b.severity).then_with(|| a.path.cmp(&b.path)).then(a.line.cmp(&b.line)));
    if ctx.json {
        ui::json(&serde_json::to_value(&findings)?);
    } else if findings.is_empty() {
        anstream::println!("No migration or infrastructure risks found in {what}.");
    } else {
        anstream::println!("{} in {what}\n", ui::bold(&format!("{} risk finding(s)", findings.len())));
        for f in &findings {
            let sev = match f.severity {
                Severity::High => ui::red("high  "),
                Severity::Medium => ui::yellow("medium"),
                Severity::Low => ui::dim("low   "),
            };
            let loc = if f.line > 0 { format!("{}:{}", f.path, f.line) } else { f.path.clone() };
            anstream::println!("{sev} {loc}  {}", ui::dim(&format!("[{}]", f.rule)));
            anstream::println!("       {}", f.message);
            if !f.excerpt.is_empty() {
                anstream::println!("       {}", ui::dim(&f.excerpt));
            }
            if !f.advice.is_empty() {
                anstream::println!("       {} {}", ui::cyan("→"), f.advice);
            }
        }
        anstream::println!(
            "\n{}",
            ui::dim(&format!(
                "Advisory and pattern-based. Silence a rule with `disable = [\"<rule>\"]` in {ANALYZERS_FILE}."
            ))
        );
    }
    if let Some(t) = threshold
        && (plugins_failed || findings.iter().any(|f| f.severity <= t))
    {
        return Err(crate::Reported.into());
    }
    Ok(())
}

/// Where this user's plugin trust lives: the Git directory, never committed or cloned.
fn plugin_trust_path(repo: &veyra_core::Repo) -> std::path::PathBuf {
    repo.common_dir().join("veyra").join("trusted-plugins.json")
}

/// Run the trusted `[[plugin]]`s and add their findings; say why the others didn't run.
/// Returns whether a trusted plugin failed (which fails `--fail-on`, so CI never passes silently).
fn run_plugins(
    repo: &veyra_core::Repo,
    analyzer: &veyra_analyze::Analyzer,
    args: &AnalyzeArgs,
    files: &[veyra_analyze::ChangedFile<'_>],
    findings: &mut Vec<veyra_analyze::Finding>,
) -> Result<bool> {
    use veyra_analyze::plugin::TrustStore;
    let path = plugin_trust_path(repo);
    let mut store = TrustStore::load(&path)?;
    let mut changed = false;
    for id in &args.revoke_plugin {
        if store.revoke(id) {
            anstream::eprintln!("Plugin `{id}` is no longer trusted here.");
            changed = true;
        }
    }
    for id in &args.trust_plugin {
        let spec = analyzer
            .plugins()
            .find(|p| p.id == *id)
            .with_context(|| format!("there is no plugin `{id}` in {ANALYZERS_FILE}"))?;
        store.trust(spec);
        changed = true;
        anstream::eprintln!("Trusted plugin `{id}`: it runs `{}` from the repository root.", spec.command.join(" "));
    }
    if changed {
        store.save(&path)?;
    }
    if args.no_plugins {
        return Ok(false);
    }
    let owned = repo.git().git_dir_trust() == veyra_core::gix::sec::Trust::Full;
    let mut failed = false;
    for spec in analyzer.plugins() {
        if !owned {
            ui::warn(&format!("plugin `{}` skipped: this repository isn't owned by you", spec.id));
            continue;
        }
        if !store.is_trusted(spec) {
            ui::warn(&format!(
                "plugin `{}` isn't trusted here (it would run `{}`)\n  hint: review it in {ANALYZERS_FILE}, then `veyra analyze --trust-plugin {}`",
                spec.id,
                spec.command.join(" "),
                spec.id
            ));
            continue;
        }
        match spec.run(repo.workdir()?, files) {
            Ok(found) => findings.extend(analyzer.keep_enabled(found)),
            Err(err) => {
                ui::warn(&format!("{err:#}"));
                failed = true;
            }
        }
    }
    Ok(failed)
}
