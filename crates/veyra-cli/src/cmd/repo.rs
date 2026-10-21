//! init, clone, status, add, rm, mv, restore, commit.

use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use serde_json::json;
use veyra_core::commit::CommitRequest;
use veyra_core::{HeadState, Repo};

use super::{journal, maybe_auto_checkpoint, pathspec, plural};
use crate::cli::{CommitArgs, RestoreArgs};
use crate::{Ctx, ui};

pub fn init(
    ctx: &Ctx,
    directory: Option<std::path::PathBuf>,
    initial_branch: Option<String>,
    object_format: Option<String>,
) -> Result<()> {
    let hash = match object_format.as_deref() {
        None | Some("sha1") => None,
        Some("sha256") => Some(veyra_core::gix::hash::Kind::Sha256),
        Some(other) => bail!("unknown object format '{other}' (use sha1 or sha256)"),
    };
    let path = match directory {
        Some(d) if d.is_absolute() => d,
        Some(d) => ctx.cwd.join(d),
        None => ctx.cwd.clone(),
    };
    let existed = path.join(".git").exists();
    let repo =
        if existed { Repo::open(&path)? } else { Repo::init_with_format(&path, initial_branch.as_deref(), hash)? };
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

pub fn clone(
    ctx: &Ctx,
    url: &str,
    directory: Option<std::path::PathBuf>,
    scope: Option<String>,
    sparse: Vec<String>,
    depth: Option<u32>,
    filter: Option<String>,
) -> Result<()> {
    use veyra_core::remote::Depth;
    let name = directory.unwrap_or_else(|| {
        let trimmed = url.trim_end_matches('/').trim_end_matches(".git");
        std::path::PathBuf::from(trimmed.rsplit(['/', '\\', ':']).next().unwrap_or("repository"))
    });
    let path = if name.is_absolute() { name } else { ctx.cwd.join(name) };
    anstream::eprintln!("Cloning into '{}'...", path.display());
    let request = match (scope, sparse.is_empty()) {
        (Some(s), _) => Some(veyra_core::sparse::ScopeRequest::Named(s)),
        (None, false) => Some(veyra_core::sparse::ScopeRequest::Dirs(sparse)),
        (None, true) => None,
    };
    let shallow = depth.map(Depth::Last).unwrap_or(Depth::Unchanged);
    let repo = match (filter, request) {
        (Some(f), req) => {
            veyra_core::promisor::validate_filter(&f)?;
            let (repo, dirs) = Repo::clone_partial(url, &path, &f, depth, req)?;
            if repo.is_partial() {
                anstream::println!("Partial clone ({f}): missing objects are fetched when needed.");
            } else {
                ui::warn("the server doesn't support filters, so this is a full clone");
            }
            if let Some(d) = dirs {
                anstream::println!("Sparse checkout of: {}", d.join(", "));
            }
            repo
        }
        (None, Some(req)) => {
            let (repo, dirs) = Repo::clone_scoped_with(url, &path, req, shallow)?;
            anstream::println!("Sparse checkout of: {}", dirs.join(", "));
            repo
        }
        (None, None) => Repo::clone_from_with(url, &path, shallow)?,
    };
    if depth.is_some() {
        anstream::println!("Shallow clone: the last {} of history.", plural(depth.unwrap_or(0) as usize, "commit"));
    }
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
    } else if let Some(state) = repo.rebase_in_progress()? {
        let current = state.current.map(|c| veyra_core::short(&c.commit)).unwrap_or_default();
        anstream::println!(
            "You are rebasing onto {} (stopped at {current}, {} left).
  (fix conflicts, `veyra add` them, then `veyra rebase --continue`; or `--skip` / `--abort`)",
            veyra_core::short(&state.onto),
            plural(state.todo.len(), "commit")
        );
    } else if let Some((kind, commit)) = repo.pick_in_progress()? {
        let name = if kind == veyra_core::pick::PickKind::CherryPick { "cherry-pick" } else { "revert" };
        anstream::println!(
            "You are in the middle of a {name} of {}.
  (fix conflicts, `veyra add` them, then `veyra {name} --continue`; or `veyra {name} --abort`)",
            veyra_core::short(&commit)
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
    crate::editor::edit_text(
        &crate::editor::commit_editor(repo),
        &path,
        template,
        "\n# Please enter the commit message for your changes. Lines starting\n# with '#' will be ignored, and an empty message aborts the commit.\n",
    )
}

/// Message editor for `rebase` reword/squash steps.
pub(crate) fn rebase_message_editor(repo: &Repo) -> impl FnMut(&str) -> Result<String> + '_ {
    move |template: &str| edit_message(repo, template)
}

pub fn commit(ctx: &Ctx, args: CommitArgs) -> Result<()> {
    if args.split {
        return super::ai::split_commit(ctx, args);
    }
    if args.smart && args.dry_run {
        return super::ai::smart_commit(ctx, args);
    }
    let repo = ctx.repo()?;
    repo.require_identity()?;
    let picking = repo.pick_in_progress()?.is_some();
    let merging = repo.merge_head()?.is_some() || picking;
    if merging && args.amend {
        bail!("you are in the middle of a merge, cherry-pick or revert; cannot amend");
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

    let log = veyra_ops::OpLog::open(&repo)?;
    let tx = log.begin(if args.amend { "amend" } else { "commit" }, &ctx.argv, false)?;
    let original_index = tx.before.view.index_tree_id()?;
    // Like git's temporary index for `commit -a`: if anything below fails, the index is put back.
    let restore_index = |repo: &veyra_core::Repo| -> Result<()> {
        let flat = repo.flat_from_tree(original_index)?;
        repo.write_index_from_flat(&flat, &Default::default())
    };
    let prepared = (|| -> Result<String> {
        if args.all {
            repo.add_tracked()?;
        }
        if !args.no_verify {
            repo.run_hook("pre-commit", &[], None)?;
            super::tools::pre_commit_checks(&repo, args.allow_secrets)?;
        }
        let message = if args.smart {
            super::ai::propose_commit_message(ctx, &repo)?
        } else {
            commit_message(&repo, &args, merging)?
        };
        // prepare-commit-msg and commit-msg may rewrite the message file, like in git.
        let msg_file = repo.git_dir().join("COMMIT_EDITMSG");
        std::fs::write(&msg_file, &message)?;
        let msg_path = msg_file.to_string_lossy().into_owned();
        let source = if !args.message.is_empty() || args.file.is_some() || args.smart {
            "message"
        } else if merging {
            "merge"
        } else if args.amend {
            "commit"
        } else {
            "template"
        };
        let mut hook_args = vec![msg_path.as_str(), source];
        let head_hex = repo.head_commit_id()?.map(|h| h.to_string());
        if args.amend
            && let Some(h) = &head_hex
        {
            hook_args.push(h.as_str());
        }
        repo.run_hook("prepare-commit-msg", &hook_args, None)?;
        if !args.no_verify {
            repo.run_hook("commit-msg", &[msg_path.as_str()], None)?;
        }
        let from_editor = args.message.is_empty() && args.file.is_none() && !args.no_edit && !args.smart;
        let message = veyra_core::commit::clean_message(&std::fs::read_to_string(&msg_file)?, from_editor);
        if message.trim().is_empty() {
            bail!("aborting commit due to empty commit message");
        }
        Ok(message)
    })();
    let message = match prepared {
        Ok(m) => m,
        Err(err) => {
            if args.all {
                let _ = restore_index(&repo);
            }
            return Err(err);
        }
    };
    let sign = if args.gpg_sign {
        veyra_core::sign::SignMode::Always
    } else if args.no_gpg_sign {
        veyra_core::sign::SignMode::Never
    } else {
        veyra_core::sign::SignMode::Config
    };
    let request =
        CommitRequest { message, amend: args.amend, allow_empty: args.allow_empty, trailers, author: None, sign };
    let parent_tree = repo.head_tree_id()?;
    let id = if picking {
        // Finishing a stopped cherry-pick/revert keeps the original author, like git.
        repo.continue_pick(Some(veyra_core::commit::add_trailers(&request.message, &request.trailers)))?
    } else if merging {
        repo.commit_merge(&request)?
    } else {
        repo.commit(&request)?
    };
    let info = repo.commit_info(id)?;
    let changes = veyra_core::flat::diff(&repo.flat_from_tree(parent_tree)?, &repo.flat_from_tree(info.tree)?);
    let summary =
        format!("{} {} {}", if args.amend { "amend" } else { "commit" }, veyra_core::short(&id), info.summary());
    tx.commit(&summary)?;
    if let Some(err) = repo.run_post_hook("post-commit", &[]) {
        ui::warn(&err);
    }

    let branch = repo.head_label()?;
    let root = if info.parents.is_empty() { " (root-commit)" } else { "" };
    anstream::println!("[{branch}{root} {}] {}", veyra_core::short(&id), info.summary());
    super::report_auto_resolutions(&repo);
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

pub fn clean(ctx: &Ctx, force: bool, dry_run: bool, dirs: bool, paths: Vec<String>) -> Result<()> {
    let repo = ctx.repo()?;
    if !force && !dry_run && repo.config_bool("clean.requireForce").unwrap_or(true) {
        bail!(
            "refusing to clean without -f or -n (clean.requireForce)\n  hint: `veyra clean -n` shows what would go; removed files are checkpointed, so `veyra undo` restores them"
        );
    }
    let spec = pathspec(ctx, &repo, &paths)?;
    let removed = if dry_run {
        repo.clean(&spec, dirs, true)?
    } else {
        ctx.confirm_destructive("clean")?;
        journal(ctx, &repo, "clean", true, || {
            let removed = repo.clean(&spec, dirs, false)?;
            let n = removed.len();
            Ok((removed, format!("clean {}", plural(n, "untracked file"))))
        })?
    };
    for p in &removed {
        anstream::println!("{} {p}", if dry_run { "Would remove" } else { "Removing" });
    }
    if !dry_run && !removed.is_empty() {
        ui::hint("the files were checkpointed first; `veyra undo` brings them back");
    }
    Ok(())
}

/// `veyra add -p`: review each hunk and stage the ones you want.
pub fn add_patch(ctx: &Ctx, paths: Vec<String>) -> Result<()> {
    use std::io::{BufRead, Write};
    use veyra_core::patch::FilePatch;

    let repo = ctx.repo()?;
    let spec = pathspec(ctx, &repo, &paths)?;
    let candidates = repo.patch_candidates(&spec)?;
    if candidates.is_empty() {
        anstream::println!("No changes.");
        return Ok(());
    }
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut ask = |prompt: &str| -> String {
        anstream::print!("{}", ui::cyan(prompt));
        let _ = std::io::stdout().flush();
        match lines.next() {
            Some(Ok(l)) => l.trim().to_lowercase(),
            _ => "q".into(), // end of input = quit, keeping what was chosen
        }
    };
    const HELP: &str = "y - stage this hunk\nn - do not stage this hunk\nq - quit; do not stage this hunk or any remaining ones\na - stage this hunk and all later hunks in the file\nd - do not stage this hunk or any later hunks in the file\ns - split the current hunk into smaller hunks\n? - print help\n(editing hunks is not supported yet)\n";

    let staged = journal(ctx, &repo, "add", false, || {
        let mut staged = 0usize;
        'files: for candidate in &candidates {
            let path = candidate.path().to_string();
            match candidate {
                FilePatch::Deletion { path: p } => loop {
                    anstream::println!("{}", ui::bold(&format!("deleted file: {path}")));
                    match ask("Stage deletion [y,n,q,?]? ").as_str() {
                        "y" => {
                            repo.stage_deletion(p)?;
                            staged += 1;
                            break;
                        }
                        "n" | "d" => break,
                        "q" => break 'files,
                        _ => anstream::print!("{HELP}"),
                    }
                },
                FilePatch::Whole { path: p } => loop {
                    anstream::println!("{}", ui::bold(&format!("{path}: binary or mode change")));
                    match ask("Stage the whole file [y,n,q,?]? ").as_str() {
                        "y" => {
                            repo.stage_whole(p)?;
                            staged += 1;
                            break;
                        }
                        "n" | "d" => break,
                        "q" => break 'files,
                        _ => anstream::print!("{HELP}"),
                    }
                },
                FilePatch::Text { path: p, index, worktree, hunks } => {
                    let mut queue: Vec<veyra_core::patch::Hunk> = hunks.clone();
                    let mut accepted: std::collections::HashSet<(usize, usize)> = Default::default();
                    let mut quit = false;
                    let mut i = 0;
                    while i < queue.len() {
                        let hunk = queue[i].clone();
                        anstream::println!("{}", ui::bold(&format!("diff --git a/{path} b/{path}")));
                        anstream::print!("{}", ui::colour_diff(&hunk.text));
                        let options = if hunk.can_split() { "y,n,q,a,d,s,?" } else { "y,n,q,a,d,?" };
                        match ask(&format!("({}/{}) Stage this hunk [{options}]? ", i + 1, queue.len())).as_str() {
                            "y" => {
                                accepted.extend(hunk.changes.iter().copied());
                                i += 1;
                            }
                            "n" => i += 1,
                            "a" => {
                                for h in &queue[i..] {
                                    accepted.extend(h.changes.iter().copied());
                                }
                                break;
                            }
                            "d" => break,
                            "s" if hunk.can_split() => {
                                let parts = veyra_core::patch::split(index, worktree, &hunk);
                                anstream::println!("Split into {} hunks.", parts.len());
                                queue.splice(i..=i, parts);
                            }
                            "q" => {
                                quit = true;
                                break;
                            }
                            _ => anstream::print!("{HELP}"),
                        }
                    }
                    if !accepted.is_empty() {
                        let content = veyra_core::patch::apply(index, worktree, &accepted);
                        repo.stage_content(p, content.as_bytes())?;
                        staged += accepted.len();
                    }
                    if quit {
                        break 'files;
                    }
                }
            }
        }
        Ok((staged, format!("stage {} interactively", plural(staged, "change"))))
    })?;
    anstream::println!("Staged {}.", plural(staged, "change"));
    Ok(())
}
