//! init, clone, status, add, rm, mv, restore, commit.

use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use serde_json::json;
use veyra_core::commit::CommitRequest;
use veyra_core::{HeadState, Repo};

use super::{journal, maybe_auto_checkpoint, pathspec, plural};
use crate::cli::{CommitArgs, RestoreArgs};
use crate::{Ctx, ui};

pub fn init(ctx: &Ctx, directory: Option<std::path::PathBuf>, initial_branch: Option<String>) -> Result<()> {
    let path = match directory {
        Some(d) if d.is_absolute() => d,
        Some(d) => ctx.cwd.join(d),
        None => ctx.cwd.clone(),
    };
    let existed = path.join(".git").exists();
    let repo = if existed { Repo::open(&path)? } else { Repo::init(&path, initial_branch.as_deref())? };
    let git_dir = std::fs::canonicalize(repo.git_dir()).unwrap_or_else(|_| repo.git_dir().to_owned());
    // Drop Windows' verbatim prefix (`\\?\`) for display.
    let git_dir = std::path::PathBuf::from(git_dir.to_string_lossy().trim_start_matches(r"\\?\"));
    if existed {
        anstream::println!("Reinitialized existing repository in {}", git_dir.display());
    } else {
        anstream::println!("Initialized empty Veyra (Git-compatible) repository in {}", git_dir.display());
    }
    Ok(())
}

pub fn clone(ctx: &Ctx, url: &str, directory: Option<std::path::PathBuf>) -> Result<()> {
    let name = directory.unwrap_or_else(|| {
        let trimmed = url.trim_end_matches('/').trim_end_matches(".git");
        std::path::PathBuf::from(trimmed.rsplit(['/', '\\', ':']).next().unwrap_or("repository"))
    });
    let path = if name.is_absolute() { name } else { ctx.cwd.join(name) };
    anstream::eprintln!("Cloning into '{}'...", path.display());
    let repo = Repo::clone_from(url, &path)?;
    let label = repo.head_label().unwrap_or_default();
    anstream::println!("Cloned {url} ({label})");
    Ok(())
}

fn status_letter(c: &veyra_core::PathChange) -> &'static str {
    match c.status_letter() {
        'A' => "new file",
        'D' => "deleted",
        'T' => "typechange",
        _ => "modified",
    }
}

pub fn status(ctx: &Ctx, short: bool) -> Result<()> {
    let repo = ctx.repo()?;
    maybe_auto_checkpoint(ctx, &repo);
    let head = repo.head_state()?;
    let status = repo.status()?;
    let merge = repo.merge_head()?;
    let w = &status.worktree;

    if ctx.json {
        let staged: Vec<_> = status
            .staged
            .iter()
            .map(|c| json!({"path": c.path.to_string(), "status": c.status_letter().to_string()}))
            .collect();
        let mut unstaged: Vec<_> = w.modified.iter().map(|p| json!({"path": p.to_string(), "status": "M"})).collect();
        unstaged.extend(w.deleted.iter().map(|p| json!({"path": p.to_string(), "status": "D"})));
        ui::json(&json!({
            "branch": head.branch_short(),
            "head": head.commit().map(|c| c.to_string()),
            "detached": matches!(head, HeadState::Detached(_)),
            "merging": merge.map(|m| m.to_string()),
            "staged": staged,
            "unstaged": unstaged,
            "untracked": w.untracked.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
            "conflicted": w.conflicted.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
            "clean": status.staged.is_empty() && w.is_clean() && w.untracked.is_empty(),
        }));
        return Ok(());
    }

    if short {
        let mut lines: std::collections::BTreeMap<String, (char, char)> = Default::default();
        for c in &status.staged {
            lines.entry(c.path.to_string()).or_insert((' ', ' ')).0 = c.status_letter();
        }
        for p in &w.modified {
            lines.entry(p.to_string()).or_insert((' ', ' ')).1 = 'M';
        }
        for p in &w.deleted {
            lines.entry(p.to_string()).or_insert((' ', ' ')).1 = 'D';
        }
        for p in &w.conflicted {
            lines.insert(p.to_string(), ('U', 'U'));
        }
        for (path, (x, y)) in lines {
            anstream::println!("{}{} {path}", ui::green(&x.to_string()), ui::red(&y.to_string()));
        }
        for p in &w.untracked {
            anstream::println!("{} {p}", ui::red("??"));
        }
        return Ok(());
    }

    match &head {
        HeadState::Branch { target: None, .. } => {
            anstream::println!("On branch {}\n\nNo commits yet\n", head.branch_short().unwrap_or("?"));
        }
        HeadState::Branch { .. } => anstream::println!("On branch {}", head.branch_short().unwrap_or("?")),
        HeadState::Detached(id) => {
            anstream::println!("{}", ui::red(&format!("HEAD detached at {}", veyra_core::short(id))))
        }
    }
    if let Some(m) = merge {
        anstream::println!(
            "You are merging {}.\n  (fix conflicts, `veyra add` them, then `veyra commit`; or `veyra merge --abort`)",
            veyra_core::short(&m)
        );
    } else if let Some(state) = repo.in_progress() {
        anstream::println!("A git operation is in progress: {state}");
    }
    let mut printed = false;
    if !w.conflicted.is_empty() {
        anstream::println!("\nUnmerged paths:\n  (use \"veyra add <file>...\" to mark resolution)");
        for p in &w.conflicted {
            anstream::println!("\t{}", ui::red(&format!("both modified:   {p}")));
        }
        printed = true;
    }
    if !status.staged.is_empty() {
        anstream::println!("\nChanges to be committed:\n  (use \"veyra unstage <file>...\" to unstage)");
        for c in &status.staged {
            anstream::println!("\t{}", ui::green(&format!("{:<12}{}", format!("{}:", status_letter(c)), c.path)));
        }
        printed = true;
    }
    let conflicted: BTreeSet<_> = w.conflicted.iter().collect();
    let unstaged: Vec<_> = w
        .modified
        .iter()
        .map(|p| ("modified:", p))
        .chain(w.deleted.iter().map(|p| ("deleted:", p)))
        .filter(|(_, p)| !conflicted.contains(p))
        .collect();
    if !unstaged.is_empty() {
        anstream::println!(
            "\nChanges not staged for commit:\n  (use \"veyra add <file>...\" to stage, \"veyra discard <file>...\" to discard — recoverable with `veyra undo`)"
        );
        let mut sorted = unstaged;
        sorted.sort_by(|a, b| a.1.cmp(b.1));
        for (what, p) in sorted {
            anstream::println!("\t{}", ui::red(&format!("{what:<12}{p}")));
        }
        printed = true;
    }
    if !w.untracked.is_empty() {
        anstream::println!("\nUntracked files:\n  (use \"veyra add <file>...\" to include in what will be committed)");
        for p in &w.untracked {
            anstream::println!("\t{}", ui::red(&p.to_string()));
        }
        printed = true;
    }
    if !printed {
        anstream::println!("nothing to commit, working tree clean");
    } else if status.staged.is_empty() {
        anstream::println!("\nno changes added to commit (use \"veyra add\" and/or \"veyra commit -a\")");
    }
    Ok(())
}

pub fn add(ctx: &Ctx, paths: Vec<String>, all: bool, update: bool, force: bool) -> Result<()> {
    let repo = ctx.repo()?;
    if paths.is_empty() && !all && !update {
        bail!("nothing specified, nothing added\n  hint: maybe you wanted `veyra add .` or `veyra add -A`");
    }
    let outcome = journal(ctx, &repo, "add", false, || {
        let out = if update && paths.is_empty() {
            repo.add_tracked()?
        } else if all && paths.is_empty() {
            repo.add_all()?
        } else {
            repo.add(&pathspec(ctx, &repo, &paths)?, force)?
        };
        let n = out.added.len() + out.modified.len() + out.removed.len();
        Ok((out, format!("stage {}", plural(n, "path"))))
    })?;
    for p in &outcome.added {
        anstream::println!("{} {p}", ui::green("add"));
    }
    for p in &outcome.modified {
        anstream::println!("{} {p}", ui::green("update"));
    }
    for p in &outcome.removed {
        anstream::println!("{} {p}", ui::red("remove"));
    }
    Ok(())
}

pub fn rm(ctx: &Ctx, paths: Vec<String>, cached: bool, force: bool, recursive: bool) -> Result<()> {
    let repo = ctx.repo()?;
    let spec = pathspec(ctx, &repo, &paths)?;
    let removed = journal(ctx, &repo, "rm", !cached, || {
        let removed = repo.remove(&spec, cached, force, recursive)?;
        let n = removed.len();
        Ok((removed, format!("remove {}", plural(n, "path"))))
    })?;
    for p in removed {
        anstream::println!("rm '{p}'");
    }
    Ok(())
}

pub fn mv(ctx: &Ctx, source: &str, destination: &str, force: bool) -> Result<()> {
    let repo = ctx.repo()?;
    let from = repo.to_repo_path(&ctx.cwd, source)?;
    let to = repo.to_repo_path(&ctx.cwd, destination)?;
    journal(ctx, &repo, "mv", true, || {
        repo.rename_path(from.as_ref(), to.as_ref(), force)?;
        Ok(((), format!("move {from} → {to}")))
    })?;
    anstream::println!("renamed {from} → {to}");
    Ok(())
}

pub fn restore(ctx: &Ctx, args: RestoreArgs) -> Result<()> {
    let repo = ctx.repo()?;
    let spec = pathspec(ctx, &repo, &args.paths)?;
    let source_tree = args.source.as_deref().map(|s| repo.rev_parse_tree(s)).transpose()?;
    let touches_worktree = !args.staged || args.worktree;
    if touches_worktree {
        ctx.confirm_destructive("restore")?;
    }
    let changed = journal(ctx, &repo, if args.staged { "unstage" } else { "restore" }, touches_worktree, || {
        let mut changed = Vec::new();
        if args.staged {
            changed.extend(repo.restore_staged(&spec, source_tree)?);
        }
        if touches_worktree {
            changed.extend(repo.restore_worktree(&spec, if args.staged { None } else { source_tree })?);
        }
        let n = changed.len();
        let what = if args.staged && !args.worktree { "unstage" } else { "restore" };
        Ok((changed, format!("{what} {}", plural(n, "path"))))
    })?;
    if changed.is_empty() {
        anstream::println!("nothing to restore");
    } else {
        for p in &changed {
            anstream::println!("{} {p}", if args.staged && !args.worktree { "unstaged" } else { "restored" });
        }
        if touches_worktree {
            ui::hint("the previous contents were checkpointed; `veyra undo` brings them back");
        }
    }
    Ok(())
}

/// Compose the commit message from -m/-F, an editor, or a pending merge message.
fn commit_message(repo: &Repo, args: &CommitArgs, merging: bool) -> Result<String> {
    if !args.message.is_empty() {
        return Ok(veyra_core::commit::clean_message(&args.message.join("\n\n"), false));
    }
    if let Some(file) = &args.file {
        let text = std::fs::read_to_string(file).with_context(|| format!("could not read {}", file.display()))?;
        return Ok(veyra_core::commit::clean_message(&text, false));
    }
    let previous = if args.amend {
        let head = repo.head_commit_id()?.context("nothing to amend")?;
        Some(repo.commit_info(head)?.message)
    } else {
        None
    };
    if args.no_edit {
        return previous.context("--no-edit requires --amend (or a merge in progress)").or_else(|e| {
            if merging {
                repo.merge_message().map(|m| veyra_core::commit::clean_message(&m, true)).context("no merge message")
            } else {
                Err(e)
            }
        });
    }
    let template = previous.or_else(|| if merging { repo.merge_message() } else { None }).unwrap_or_default();
    let edited = edit_message(repo, &template)?;
    Ok(veyra_core::commit::clean_message(&edited, true))
}

fn edit_message(repo: &Repo, template: &str) -> Result<String> {
    let path = repo.git_dir().join("COMMIT_EDITMSG");
    let mut text = String::from(template);
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("\n# Please enter the commit message for your changes. Lines starting\n# with '#' will be ignored, and an empty message aborts the commit.\n");
    std::fs::write(&path, &text)?;
    let editor = std::env::var("GIT_EDITOR")
        .ok()
        .or_else(|| repo.config_get("core.editor"))
        .or_else(|| std::env::var("VISUAL").ok())
        .or_else(|| std::env::var("EDITOR").ok())
        .unwrap_or_else(|| if cfg!(windows) { "notepad".into() } else { "vi".into() });
    let quoted = format!("\"{}\"", path.display());
    let status = if cfg!(windows) {
        std::process::Command::new("cmd").args(["/C", &format!("{editor} {quoted}")]).status()
    } else {
        std::process::Command::new("sh").args(["-c", &format!("{editor} {quoted}")]).status()
    }
    .with_context(|| {
        format!("could not start the editor '{editor}'\n  hint: pass -m \"message\" or set core.editor")
    })?;
    if !status.success() {
        bail!("the editor '{editor}' exited with an error; aborting commit");
    }
    Ok(std::fs::read_to_string(&path)?)
}

pub fn commit(ctx: &Ctx, args: CommitArgs) -> Result<()> {
    let repo = ctx.repo()?;
    repo.require_identity()?;
    let merging = repo.merge_head()?.is_some();
    if merging && args.amend {
        bail!("you are in the middle of a merge; cannot amend");
    }

    // Trailers: provenance and intent links (declared, never inferred).
    let mut trailers = Vec::new();
    for t in &args.trailer {
        let (k, v) = t
            .split_once(':')
            .or_else(|| t.split_once('='))
            .with_context(|| format!("invalid trailer '{t}': expected 'Key: value'"))?;
        trailers.push((k.trim().to_owned(), v.trim().to_owned()));
    }
    if let Some(tool) = &args.assisted_by {
        trailers.push(("Assisted-by".into(), tool.clone()));
    }
    for id in &args.intent {
        let root = repo.workdir()?;
        let all = veyra_intent::load_all(root)?;
        let found = veyra_intent::find(&all, id)?;
        trailers.push(("Intent".into(), found.1.id.clone()));
    }

    let message = commit_message(&repo, &args, merging)?;
    if message.trim().is_empty() {
        bail!("aborting commit due to empty commit message");
    }

    for caveat in repo.commit_caveats() {
        ui::warn(&caveat);
    }

    let log = veyra_ops::OpLog::open(&repo)?;
    let tx = log.begin(if args.amend { "amend" } else { "commit" }, &ctx.argv, false)?;
    if args.all {
        repo.add_tracked()?;
    }
    if !args.no_verify {
        super::tools::pre_commit_checks(&repo, args.allow_secrets)?;
    }
    let request = CommitRequest { message, amend: args.amend, allow_empty: args.allow_empty, trailers };
    let parent_tree = repo.head_tree_id()?;
    let id = if merging { repo.commit_merge(&request)? } else { repo.commit(&request)? };
    let info = repo.commit_info(id)?;
    let changes = veyra_core::flat::diff(&repo.flat_from_tree(parent_tree)?, &repo.flat_from_tree(info.tree)?);
    let summary =
        format!("{} {} {}", if args.amend { "amend" } else { "commit" }, veyra_core::short(&id), info.summary());
    tx.commit(&summary)?;

    let branch = repo.head_label()?;
    let root = if info.parents.is_empty() { " (root-commit)" } else { "" };
    anstream::println!("[{branch}{root} {}] {}", veyra_core::short(&id), info.summary());
    let added = changes.iter().filter(|c| c.old.is_none()).count();
    let deleted = changes.iter().filter(|c| c.new.is_none()).count();
    let mut line = format!(" {} changed", plural(changes.len(), "file"));
    if added > 0 {
        line.push_str(&format!(", {added} created"));
    }
    if deleted > 0 {
        line.push_str(&format!(", {deleted} deleted"));
    }
    anstream::println!("{line}");
    Ok(())
}
