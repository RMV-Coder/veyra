//! log, show, diff.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use serde_json::json;
use veyra_core::diff::{FileDiff, NewSide};
use veyra_core::worktree::SnapshotOptions;
use veyra_core::{FlatTree, ObjectId, PathChange, Repo, flat};

use super::{maybe_auto_checkpoint, pathspec};
use crate::cli::{DiffArgs, LogArgs};
use crate::{Ctx, ui};

/// Map commit id → decorations like `HEAD -> main`, `tag: v1`, `origin/main`.
fn decorations(repo: &Repo) -> Result<BTreeMap<ObjectId, Vec<String>>> {
    let mut out: BTreeMap<ObjectId, Vec<String>> = BTreeMap::new();
    let head = repo.head_state()?;
    let head_branch = head.branch_short().map(str::to_owned);
    if let Some(c) = head.commit()
        && head_branch.is_none()
    {
        out.entry(c).or_default().push("HEAD".into());
    }
    for (name, id) in repo.all_refs()? {
        let Ok(commit) = repo.peel_to_commit(id) else { continue };
        let label = if let Some(b) = name.strip_prefix("refs/heads/") {
            if head_branch.as_deref() == Some(b) { format!("HEAD -> {b}") } else { b.to_owned() }
        } else if let Some(t) = name.strip_prefix("refs/tags/") {
            format!("tag: {t}")
        } else if let Some(r) = name.strip_prefix("refs/remotes/") {
            r.to_owned()
        } else {
            continue;
        };
        out.entry(commit).or_default().push(label);
    }
    for labels in out.values_mut() {
        labels.sort_by_key(|l| (!l.starts_with("HEAD"), l.starts_with("tag:"), l.clone()));
    }
    Ok(out)
}

pub fn log(ctx: &Ctx, args: LogArgs) -> Result<()> {
    let repo = ctx.repo()?;
    maybe_auto_checkpoint(ctx, &repo);
    let mut tips = Vec::new();
    if args.all {
        for (_, id) in repo.all_refs()? {
            if let Ok(c) = repo.peel_to_commit(id) {
                tips.push(c);
            }
        }
        if let Some(h) = repo.head_commit_id()? {
            tips.push(h);
        }
    }
    for rev in &args.revisions {
        tips.push(repo.rev_parse_commit(rev)?);
    }
    if tips.is_empty() {
        match repo.head_commit_id()? {
            Some(h) => tips.push(h),
            None => bail!("your current branch '{}' does not have any commits yet", repo.head_label()?),
        }
    }
    tips.sort();
    tips.dedup();
    let trailer_filter = args.trailer.as_deref().map(|t| match t.split_once('=') {
        Some((k, v)) => (k.trim().to_owned(), Some(v.trim().to_owned())),
        None => (t.trim().to_owned(), None),
    });
    let limit = if trailer_filter.is_some() { None } else { args.max_count };
    let mut commits = repo.log(&tips, limit)?;
    if let Some((key, value)) = &trailer_filter {
        commits.retain(|c| {
            veyra_core::commit::parse_trailers(&c.message)
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case(key) && value.as_ref().is_none_or(|want| v == want))
        });
        if let Some(n) = args.max_count {
            commits.truncate(n);
        }
    }

    if ctx.json {
        let items: Vec<_> = commits
            .iter()
            .map(|c| {
                json!({
                    "id": c.id.to_string(),
                    "parents": c.parents.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
                    "author": {"name": c.author_name, "email": c.author_email, "time": c.author_time},
                    "committer": {"name": c.committer_name, "email": c.committer_email, "time": c.committer_time},
                    "summary": c.summary(),
                    "message": c.message,
                    "trailers": veyra_core::commit::parse_trailers(&c.message).into_iter().map(|(k, v)| json!({"key": k, "value": v})).collect::<Vec<_>>(),
                })
            })
            .collect();
        ui::json(&json!(items));
        return Ok(());
    }

    if args.graph {
        let commits = repo.topo_log(&tips, args.max_count)?;
        let decor = decorations(&repo)?;
        for row in veyra_core::annotate::render_graph(&commits) {
            match row {
                veyra_core::annotate::GraphRow::Edge(edge) => anstream::println!("{}", ui::red(&edge)),
                veyra_core::annotate::GraphRow::Commit { prefix, index } => {
                    let c = &commits[index];
                    let labels = decor
                        .get(&c.id)
                        .filter(|l| !l.is_empty())
                        .map(|l| format!(" ({})", l.join(", ")))
                        .unwrap_or_default();
                    anstream::println!(
                        "{}{}{} {}",
                        ui::red(&prefix),
                        ui::yellow(&veyra_core::short(&c.id)),
                        ui::yellow(&labels),
                        c.summary()
                    );
                }
            }
        }
        return Ok(());
    }

    let decor = decorations(&repo)?;
    let deco = |id: &ObjectId| -> String {
        match decor.get(id) {
            Some(labels) if !labels.is_empty() => format!(" ({})", labels.join(", ")),
            _ => String::new(),
        }
    };
    for (i, c) in commits.iter().enumerate() {
        if args.oneline {
            anstream::println!("{}{} {}", ui::yellow(&veyra_core::short(&c.id)), ui::yellow(&deco(&c.id)), c.summary());
            continue;
        }
        if i > 0 {
            anstream::println!();
        }
        anstream::println!("{}", ui::yellow(&format!("commit {}{}", c.id, deco(&c.id))));
        if c.parents.len() > 1 {
            anstream::println!("Merge: {}", c.parents.iter().map(veyra_core::short).collect::<Vec<_>>().join(" "));
        }
        anstream::println!("Author: {} <{}>", c.author_name, c.author_email);
        anstream::println!("Date:   {}", veyra_core::time::format_git(c.author_time, c.author_offset));
        anstream::println!();
        for line in c.message.trim_end().lines() {
            anstream::println!("    {line}");
        }
    }
    Ok(())
}

fn print_diffs(repo: &Repo, diffs: &[FileDiff], stat: bool, name_only: bool, name_status: bool) {
    if name_only {
        for d in diffs {
            anstream::println!("{}", d.path);
        }
        return;
    }
    if name_status {
        for d in diffs {
            let change = PathChange { path: d.path.clone(), old: d.old, new: d.new };
            anstream::println!("{}\t{}", change.status_letter(), d.path);
        }
        return;
    }
    if stat {
        let width = diffs.iter().map(|d| d.path.len()).max().unwrap_or(0).min(60);
        let (mut ins, mut del) = (0, 0);
        for d in diffs {
            ins += d.added;
            del += d.removed;
            let bar = if d.binary {
                "Bin".to_owned()
            } else {
                let total = d.added + d.removed;
                let scale = if total > 50 { 50.0 / total as f64 } else { 1.0 };
                format!(
                    "{:>4} {}{}",
                    total,
                    ui::green(&"+".repeat((d.added as f64 * scale).ceil() as usize)),
                    ui::red(&"-".repeat((d.removed as f64 * scale).ceil() as usize))
                )
            };
            anstream::println!(" {:<width$} | {bar}", d.path.to_string());
        }
        anstream::println!(
            " {} changed, {} insertions(+), {} deletions(-)",
            super::plural(diffs.len(), "file"),
            ins,
            del
        );
        return;
    }
    let _ = repo;
    for d in diffs {
        anstream::print!("{}", ui::colour_diff(&veyra_core::diff::render(d, None)));
    }
}

/// Worktree state restricted to paths tracked in `tracked_in` (git diff ignores untracked files).
fn worktree_flat(repo: &Repo, tracked_in: &[&FlatTree]) -> Result<FlatTree> {
    let snap = repo.snapshot(&SnapshotOptions::default())?;
    let tracked: BTreeSet<_> = tracked_in.iter().flat_map(|t| t.keys()).collect();
    Ok(snap.flat.into_iter().filter(|(p, _)| tracked.contains(p)).collect())
}

pub fn diff(ctx: &Ctx, args: DiffArgs) -> Result<()> {
    let repo = ctx.repo()?;
    maybe_auto_checkpoint(ctx, &repo);
    let spec = pathspec(ctx, &repo, &args.paths)?;
    let (old, new) = match (args.staged, args.revisions.as_slice()) {
        (true, []) => (repo.flat_head()?, repo.flat_index()?),
        (true, [rev]) => (repo.flat_from_tree(repo.rev_parse_tree(rev)?)?, repo.flat_index()?),
        (false, []) => {
            let index = repo.flat_index()?;
            let wt = worktree_flat(&repo, &[&index])?;
            (index, wt)
        }
        (false, [rev]) => {
            if let Some((a, b)) = rev.split_once("..") {
                (
                    repo.flat_from_tree(repo.rev_parse_tree(a)?)?,
                    repo.flat_from_tree(repo.rev_parse_tree(if b.is_empty() { "HEAD" } else { b })?)?,
                )
            } else {
                let base = repo.flat_from_tree(repo.rev_parse_tree(rev)?)?;
                let index = repo.flat_index()?;
                let wt = worktree_flat(&repo, &[&base, &index])?;
                (base, wt)
            }
        }
        (false, [a, b]) => {
            (repo.flat_from_tree(repo.rev_parse_tree(a)?)?, repo.flat_from_tree(repo.rev_parse_tree(b)?)?)
        }
        _ => bail!("too many revisions"),
    };
    let changes: Vec<PathChange> =
        flat::diff(&old, &new).into_iter().filter(|c| spec.matches(c.path.as_ref())).collect();
    if args.semantic {
        return print_semantic(ctx, &repo.semantic_diff_changes(&changes)?);
    }
    let diffs = repo.diff_changes(&changes, NewSide::Objects, args.context)?;
    print_diffs(&repo, &diffs, args.stat, args.name_only, args.name_status);
    Ok(())
}

/// `veyra diff --semantic`: changed symbols per file.
fn print_semantic(ctx: &Ctx, files: &[veyra_core::semantic::SemanticFileDiff]) -> Result<()> {
    use veyra_core::semantic_engine::ChangeKind;
    if ctx.json {
        let out: Vec<_> = files
            .iter()
            .map(|f| match &f.diff {
                None => json!({"path": f.path.to_string(), "status": f.status.to_string(), "supported": false}),
                Some(d) => json!({
                    "path": f.path.to_string(),
                    "status": f.status.to_string(),
                    "supported": true,
                    "language": d.lang.name(),
                    "parse_errors": d.parse_errors,
                    "outside_symbols_changed": d.outside_changed,
                    "formatting_only": d.formatting_only(),
                    "changes": d.changes.iter().map(|c| json!({
                        "change": c.change.label(),
                        "kind": c.kind.label(),
                        "name": c.name,
                        "old_name": c.old_name,
                        "old_line": c.old_line,
                        "new_line": c.new_line,
                        "old_signature": c.old_signature,
                        "new_signature": c.new_signature,
                    })).collect::<Vec<_>>(),
                }),
            })
            .collect();
        ui::json(&json!(out));
        return Ok(());
    }
    for f in files {
        let Some(d) = &f.diff else {
            anstream::println!(
                "{} {}  {}",
                f.status,
                ui::bold(&f.path.to_string()),
                ui::dim("(no syntax support; use `veyra diff`)")
            );
            continue;
        };
        let mut header = format!("{} {}  {}", f.status, ui::bold(&f.path.to_string()), ui::dim(d.lang.name()));
        if d.formatting_only() && !d.changes.is_empty() {
            header.push_str(&format!("  {}", ui::dim("formatting only")));
        }
        anstream::println!("{header}");
        if d.parse_errors {
            anstream::println!("    {}", ui::yellow("! syntax errors in one version; results are best-effort"));
        }
        for c in &d.changes {
            let (mark, colour): (&str, fn(&str) -> String) = match c.change {
                ChangeKind::Added => ("+", ui::green),
                ChangeKind::Removed => ("-", ui::red),
                ChangeKind::Modified { .. } => ("~", ui::yellow),
                ChangeKind::Reformatted => ("=", ui::dim),
                ChangeKind::Renamed => (">", ui::cyan),
            };
            let what = match (&c.change, &c.old_name) {
                (ChangeKind::Renamed, Some(old)) => format!("{} {old} → {}", c.kind.label(), c.name),
                _ => format!("{} {}", c.kind.label(), c.name),
            };
            let line = c.new_line.or(c.old_line).map(|l| format!(" (line {l})")).unwrap_or_default();
            anstream::println!("    {} {}  {}", colour(mark), what, ui::dim(&format!("{}{line}", c.change.label())));
            if let (ChangeKind::Modified { signature: true }, Some(old), Some(new)) =
                (&c.change, &c.old_signature, &c.new_signature)
            {
                anstream::println!("        {} {}", ui::red("-"), old);
                anstream::println!("        {} {}", ui::green("+"), new);
            }
        }
        if d.outside_changed {
            anstream::println!(
                "    {} {}",
                ui::yellow("~"),
                ui::dim("code outside any symbol (imports, top-level statements)")
            );
        }
    }
    Ok(())
}

pub fn show(ctx: &Ctx, revision: Option<String>, stat: bool) -> Result<()> {
    let repo = ctx.repo()?;
    let id = repo.rev_parse_commit(revision.as_deref().unwrap_or("HEAD"))?;
    let c = repo.commit_info(id)?;
    let decor = decorations(&repo)?;
    let deco = decor.get(&id).filter(|l| !l.is_empty()).map(|l| format!(" ({})", l.join(", "))).unwrap_or_default();
    anstream::println!("{}", ui::yellow(&format!("commit {id}{deco}")));
    if c.parents.len() > 1 {
        anstream::println!("Merge: {}", c.parents.iter().map(veyra_core::short).collect::<Vec<_>>().join(" "));
    }
    anstream::println!("Author: {} <{}>", c.author_name, c.author_email);
    anstream::println!("Date:   {}\n", veyra_core::time::format_git(c.author_time, c.author_offset));
    for line in c.message.trim_end().lines() {
        anstream::println!("    {line}");
    }
    anstream::println!();
    let parent = match c.parents.first() {
        Some(p) => repo.flat_from_commit(*p)?,
        None => FlatTree::new(),
    };
    let changes = flat::diff(&parent, &repo.flat_from_tree(c.tree)?);
    let diffs = repo.diff_changes(&changes, NewSide::Objects, 3)?;
    print_diffs(&repo, &diffs, stat, false, false);
    Ok(())
}

pub fn blame(ctx: &Ctx, file: &str, rev: Option<String>, lines: Option<String>) -> Result<()> {
    let repo = ctx.repo()?;
    let path = repo.to_repo_path(&ctx.cwd, file)?;
    let commit = repo.rev_parse_commit(rev.as_deref().unwrap_or("HEAD"))?;
    let range = match lines {
        Some(spec) => {
            let (a, b) = spec.split_once(',').unwrap_or((spec.as_str(), spec.as_str()));
            let start: usize = a.trim().parse().map_err(|_| anyhow::anyhow!("invalid -L range '{spec}'"))?;
            let end: usize = b.trim().parse().map_err(|_| anyhow::anyhow!("invalid -L range '{spec}'"))?;
            Some((start.max(1), end.max(start)))
        }
        None => None,
    };
    let blamed = repo.blame(path.as_ref(), commit)?;
    let shown: Vec<_> = blamed.iter().filter(|l| range.is_none_or(|(a, b)| l.line >= a && l.line <= b)).collect();
    if ctx.json {
        ui::json(&json!(
            shown
                .iter()
                .map(|l| json!({
                    "line": l.line, "commit": l.commit.to_string(), "author": l.author, "time": l.time,
                    "source_path": l.source_path, "text": l.text,
                }))
                .collect::<Vec<_>>()
        ));
        return Ok(());
    }
    let author_width = shown.iter().map(|l| l.author.chars().count()).max().unwrap_or(0).min(20);
    let line_width = shown.last().map(|l| l.line.to_string().len()).unwrap_or(1);
    for l in shown {
        let date = jiff::Timestamp::from_second(l.time)
            .ok()
            .and_then(|t| {
                jiff::tz::Offset::from_seconds(l.offset)
                    .ok()
                    .map(|o| t.to_zoned(o.to_time_zone()).strftime("%Y-%m-%d").to_string())
            })
            .unwrap_or_default();
        let author: String = l.author.chars().take(author_width).collect();
        let renamed = l.source_path.as_ref().map(|p| format!(" {p}")).unwrap_or_default();
        anstream::println!(
            "{}{} ({:<author_width$} {date} {:>line_width$}) {}",
            ui::yellow(&veyra_core::short(&l.commit)),
            ui::dim(&renamed),
            author,
            l.line,
            l.text
        );
    }
    Ok(())
}
