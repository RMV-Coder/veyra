//! `veyra ai …`, `commit --smart` / `--split` and `merge --assist`: optional AI assistance.
//!
//! Off until configured. Every request is scanned for secrets before it leaves the machine,
//! `--dry-run` shows exactly what would be sent, and every answer is a proposal: nothing changes
//! without the user's approval (`--yes` accepts in scripts; uncertain merge resolutions always
//! need a human).

use std::io::{BufRead, IsTerminal, Write};

use anyhow::{Context, Result, bail};
use serde_json::json;
use veyra_ai::{Provider, Reply, Request, Settings, tasks};
use veyra_core::diff::NewSide;
use veyra_core::{FlatTree, Repo, flat};

use super::analysis::scope;
use crate::cli::{AiCommand, CommitArgs, ScopeArgs};
use crate::{Ctx, ui};

pub(crate) fn settings(repo: &Repo) -> Settings {
    Settings::from_lookup(|k| repo.config_get(k))
}

/// Unified diff text of `old` → `new` (both in the object database).
pub(crate) fn diff_text(repo: &Repo, old: &FlatTree, new: &FlatTree) -> Result<String> {
    let changes = flat::diff(old, new);
    let diffs = repo.diff_changes(&changes, NewSide::Objects, 3)?;
    Ok(diffs.iter().map(|d| veyra_core::diff::render(d, None)).collect())
}

fn recent_subjects(repo: &Repo) -> Result<Vec<String>> {
    let Some(head) = repo.head_commit_id()? else { return Ok(Vec::new()) };
    Ok(repo.log(&[head], Some(15))?.iter().map(|c| c.summary().to_owned()).collect())
}

/// Refuse to send anything that looks like a high-confidence secret.
fn guard_secrets(repo: &Repo, request: &Request) -> Result<()> {
    let scanner = veyra_security::Scanner::new(&super::tools::load_policy(repo)?)?;
    let text = format!("{}\n{}", request.system, request.prompt);
    let findings: Vec<_> = scanner
        .scan("ai-request", &text, None)
        .into_iter()
        .filter(|f| f.confidence == veyra_security::Confidence::High)
        .collect();
    if findings.is_empty() {
        return Ok(());
    }
    for f in &findings {
        anstream::eprintln!("  {} {}: {}", ui::red("secret"), f.description, f.redacted);
    }
    bail!(
        "the request contains what looks like a secret, so nothing was sent\n  \
         hint: remove it, or allowlist a false positive in .veyra/secrets.toml (fingerprint {})",
        findings[0].fingerprint
    )
}

/// Send a request (or, with `dry_run`, print it). `None` means it was a dry run.
fn send(repo: &Repo, request: &Request, dry_run: bool) -> Result<Option<Reply>> {
    guard_secrets(repo, request)?;
    let s = settings(repo);
    if dry_run {
        anstream::println!("{}", ui::bold(&format!("Would send to: {}", s.describe())));
        anstream::println!("{}\n{}\n", ui::dim("--- system ---"), request.system);
        anstream::println!("{}\n{}", ui::dim("--- prompt ---"), request.prompt);
        if let Some(schema) = &request.schema {
            anstream::println!("{}\n{}", ui::dim("--- output schema ---"), serde_json::to_string_pretty(schema)?);
        }
        return Ok(None);
    }
    if s.provider != Provider::Off {
        anstream::eprintln!("{}", ui::dim(&format!("Asking {}…", s.describe())));
    }
    veyra_ai::complete(&s, request).map(Some)
}

enum Answer {
    Yes,
    Edit,
    No,
}

/// Ask the user to approve a proposal. `--yes` accepts; without a terminal it's refused.
fn ask(ctx: &Ctx, question: &str, allow_edit: bool) -> Result<Answer> {
    if ctx.yes {
        return Ok(Answer::Yes);
    }
    if ctx.agent || ctx.json || !std::io::stdin().is_terminal() {
        bail!("this is a proposal and needs approval; re-run with --yes to accept it (nothing was changed)");
    }
    eprint!("{question} [y{}/N] ", if allow_edit { "/e(dit)" } else { "" });
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Answer::Yes,
        "e" | "edit" if allow_edit => Answer::Edit,
        _ => Answer::No,
    })
}

fn assisted_by(reply: &Reply, s: &Settings) -> String {
    match (&s.provider, &reply.model) {
        (_, Some(model)) => model.clone(),
        (Provider::Command(_), None) => "local model via veyra ai".into(),
        _ => s.model.clone(),
    }
}

/// `commit --smart`: propose a message for the staged changes; returns the approved message.
pub(crate) fn propose_commit_message(ctx: &Ctx, repo: &Repo) -> Result<String> {
    let head = repo.flat_head()?;
    let index = repo.flat_index()?;
    if flat::diff(&head, &index).is_empty() {
        bail!("nothing staged to describe");
    }
    let request = tasks::commit_message(&diff_text(repo, &head, &index)?, &recent_subjects(repo)?);
    let Some(reply) = send(repo, &request, false)? else { unreachable!() };
    let proposal: tasks::CommitMessage = veyra_ai::parse_json(&reply.text)?;
    let trailer = ("Assisted-by".to_owned(), assisted_by(&reply, &settings(repo)));
    let mut message = veyra_core::commit::add_trailers(&proposal.to_message(), &[trailer]);
    anstream::println!("{}\n", ui::bold("Proposed commit message:"));
    for line in message.lines() {
        anstream::println!("    {line}");
    }
    anstream::println!();
    match ask(ctx, "Commit with this message?", true)? {
        Answer::Yes => {}
        Answer::Edit => {
            let editor = crate::editor::commit_editor(repo);
            let file = repo.git_dir().join("COMMIT_EDITMSG");
            let edited =
                crate::editor::edit_text(&editor, &file, &message, "# Lines starting with '#' are ignored.\n")?;
            message = veyra_core::commit::clean_message(&edited, true);
        }
        Answer::No => bail!("commit cancelled; nothing was changed"),
    }
    Ok(message)
}

fn commit_args_like(args: &CommitArgs, message: String) -> CommitArgs {
    CommitArgs {
        message: vec![message],
        file: None,
        all: false,
        amend: false,
        no_edit: true,
        allow_empty: false,
        trailer: args.trailer.clone(),
        assisted_by: None,
        intent: args.intent.clone(),
        allow_secrets: args.allow_secrets,
        no_verify: args.no_verify,
        gpg_sign: args.gpg_sign,
        no_gpg_sign: args.no_gpg_sign,
        smart: false,
        split: false,
        dry_run: false,
    }
}

/// `commit --split`: propose a sequence of commits for the staged files, then make them.
pub(crate) fn split_commit(ctx: &Ctx, args: CommitArgs) -> Result<()> {
    let repo = ctx.repo()?;
    if repo.merge_head()?.is_some() || repo.pick_in_progress()?.is_some() || args.amend {
        bail!("--split can't be used while merging, cherry-picking or amending");
    }
    if args.all {
        repo.add_tracked()?;
    }
    let head = repo.flat_head()?;
    let index = repo.flat_index()?;
    let changes = flat::diff(&head, &index);
    let files: Vec<String> = changes.iter().map(|c| c.path.to_string()).collect();
    if files.len() < 2 {
        bail!(
            "only {} staged; there is nothing to split (try `veyra commit --smart`)",
            super::plural(files.len(), "file")
        );
    }
    let request = tasks::split(&diff_text(&repo, &head, &index)?, &files, &recent_subjects(&repo)?);
    let Some(reply) = send(&repo, &request, args.dry_run)? else { return Ok(()) };
    let plan: tasks::SplitPlan = veyra_ai::parse_json(&reply.text)?;
    plan.validate(&files).map_err(|e| anyhow::anyhow!("the proposed split is not usable: {e}; nothing was changed"))?;

    anstream::println!("{}", ui::bold(&format!("Proposed: {}", super::plural(plan.commits.len(), "commit"))));
    for (i, c) in plan.commits.iter().enumerate() {
        anstream::println!("\n  {}. {}", i + 1, c.message.lines().next().unwrap_or_default());
        for f in &c.files {
            anstream::println!("       {}", ui::dim(f));
        }
    }
    anstream::println!();
    if !matches!(ask(ctx, "Make these commits?", false)?, Answer::Yes) {
        bail!("split cancelled; nothing was changed");
    }

    let trailer = ("Assisted-by".to_owned(), assisted_by(&reply, &settings(&repo)));
    let mut partial = head.clone();
    for c in &plan.commits {
        for f in &c.files {
            let key: veyra_core::BString = f.as_str().into();
            match index.get(&key) {
                Some(e) => partial.insert(key, *e),
                None => partial.remove(&key),
            };
        }
        repo.write_index_from_flat(&partial, &Default::default())?;
        let message = veyra_core::commit::add_trailers(&c.message, std::slice::from_ref(&trailer));
        if let Err(err) = super::repo::commit(ctx, commit_args_like(&args, message)) {
            // Put the full staged state back so nothing is lost; earlier commits stay (undoable).
            repo.write_index_from_flat(&index, &Default::default())?;
            return Err(err);
        }
    }
    Ok(())
}

pub fn run(ctx: &Ctx, command: AiCommand) -> Result<()> {
    match command {
        AiCommand::Status => status(ctx),
        AiCommand::Configure { provider, model, effort, command, global } => {
            configure(ctx, provider, model, effort, command, global)
        }
        AiCommand::Disable { global } => {
            let repo = ctx.repo()?;
            let scope = if global { veyra_core::config::Scope::Global } else { veyra_core::config::Scope::Local };
            veyra_core::config::set_in_file(&repo.config_path(scope)?, "veyra.ai.provider", "off")?;
            anstream::println!("AI features are off.");
            Ok(())
        }
        AiCommand::Commit { split, dry_run } => {
            let args = commit_args_like_empty(split, dry_run);
            if split { split_commit(ctx, args) } else { smart_commit(ctx, args) }
        }
        AiCommand::Explain { revision, staged, dry_run } => explain(ctx, revision, staged, dry_run),
        AiCommand::Review { scope: s, dry_run } => review(ctx, &s, dry_run),
        AiCommand::Summarize { range, dry_run } => summarize(ctx, range, dry_run),
        AiCommand::Risks { scope: s, dry_run } => risks(ctx, &s, dry_run),
        AiCommand::Resolve { paths, dry_run } => resolve(ctx, &paths, dry_run),
    }
}

fn commit_args_like_empty(split: bool, dry_run: bool) -> CommitArgs {
    let mut a = commit_args_like(&empty_commit_args(), String::new());
    a.message.clear();
    a.no_edit = false;
    a.smart = !split;
    a.split = split;
    a.dry_run = dry_run;
    a
}

fn empty_commit_args() -> CommitArgs {
    CommitArgs {
        message: vec![],
        file: None,
        all: false,
        amend: false,
        no_edit: false,
        allow_empty: false,
        trailer: vec![],
        assisted_by: None,
        intent: vec![],
        allow_secrets: false,
        no_verify: false,
        gpg_sign: false,
        no_gpg_sign: false,
        smart: false,
        split: false,
        dry_run: false,
    }
}

pub(crate) fn smart_commit(ctx: &Ctx, args: CommitArgs) -> Result<()> {
    if args.dry_run {
        let repo = ctx.repo()?;
        let (head, index) = (repo.flat_head()?, repo.flat_index()?);
        let request = tasks::commit_message(&diff_text(&repo, &head, &index)?, &recent_subjects(&repo)?);
        send(&repo, &request, true)?;
        return Ok(());
    }
    super::repo::commit(ctx, args)
}

fn status(ctx: &Ctx) -> Result<()> {
    let repo = ctx.repo()?;
    let s = settings(&repo);
    let has_key = ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"]
        .iter()
        .any(|k| std::env::var(k).is_ok_and(|v| !v.trim().is_empty()));
    if ctx.json {
        ui::json(&json!({
            "provider": match &s.provider { Provider::Off => "off", Provider::Anthropic => "anthropic", Provider::Command(_) => "command" },
            "model": s.model, "effort": s.effort, "fallbacks": s.fallbacks,
            "credentials": has_key,
        }));
        return Ok(());
    }
    anstream::println!("AI provider: {}", ui::bold(&s.describe()));
    match &s.provider {
        Provider::Off => ui::hint(
            "AI is optional. Turn it on with `veyra ai configure --provider anthropic` (Claude API) or \
             `--provider command --command <cmd>` (a local model)",
        ),
        Provider::Anthropic if !has_key => ui::warn("ANTHROPIC_API_KEY is not set in this environment"),
        _ => {}
    }
    anstream::println!(
        "{}",
        ui::dim(
            "Requests are scanned for secrets before sending; `--dry-run` shows exactly what would be sent. \
             AI output is always a proposal you approve."
        )
    );
    Ok(())
}

fn configure(
    ctx: &Ctx,
    provider: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    command: Option<String>,
    global: bool,
) -> Result<()> {
    let repo = ctx.repo()?;
    let scope = if global { veyra_core::config::Scope::Global } else { veyra_core::config::Scope::Local };
    let path = repo.config_path(scope)?;
    let provider = match (provider.as_deref(), &command) {
        (Some(p), _) => Some(p.to_ascii_lowercase()),
        (None, Some(_)) => Some("command".to_owned()),
        (None, None) if model.is_some() || effort.is_some() => None,
        (None, None) => Some("anthropic".to_owned()),
    };
    if let Some(p) = &provider {
        if !matches!(p.as_str(), "anthropic" | "command" | "off") {
            bail!("unknown provider '{p}' (anthropic, command or off)");
        }
        if p == "command" && command.is_none() && repo.config_get("veyra.ai.command").is_none() {
            bail!("--provider command needs --command <cmd> (it receives a JSON request on stdin)");
        }
        veyra_core::config::set_in_file(&path, "veyra.ai.provider", p)?;
    }
    if let Some(c) = &command {
        veyra_core::config::set_in_file(&path, "veyra.ai.command", c)?;
    }
    if let Some(m) = &model {
        veyra_core::config::set_in_file(&path, "veyra.ai.model", m)?;
    }
    if let Some(e) = &effort {
        if !matches!(e.as_str(), "low" | "medium" | "high" | "xhigh" | "max") {
            bail!("effort must be low, medium, high, xhigh or max");
        }
        veyra_core::config::set_in_file(&path, "veyra.ai.effort", e)?;
    }
    status(ctx)
}

fn explain(ctx: &Ctx, revision: Option<String>, staged: bool, dry_run: bool) -> Result<()> {
    let repo = ctx.repo()?;
    let (old, new, message) = match &revision {
        Some(rev) => {
            let id = repo.rev_parse_commit(rev)?;
            let info = repo.commit_info(id)?;
            let old = match info.parents.first() {
                Some(p) => repo.flat_from_commit(*p)?,
                None => FlatTree::new(),
            };
            (old, repo.flat_from_tree(info.tree)?, Some(info.message))
        }
        None => {
            let s = ScopeArgs { staged, commit: None, branch: None };
            let (old, new, _) = scope(&repo, &s)?;
            (old, new, None)
        }
    };
    let diff = diff_text(&repo, &old, &new)?;
    if diff.is_empty() {
        bail!("no changes to explain");
    }
    let Some(reply) = send(&repo, &tasks::explain(&diff, message.as_deref()), dry_run)? else { return Ok(()) };
    anstream::println!("{}", reply.text.trim());
    Ok(())
}

fn review(ctx: &Ctx, s: &ScopeArgs, dry_run: bool) -> Result<()> {
    let repo = ctx.repo()?;
    let (old, new, what) = scope(&repo, s)?;
    let diff = diff_text(&repo, &old, &new)?;
    if diff.is_empty() {
        bail!("no changes in {what} to review");
    }
    let impact = repo.impact(&old, &new, &veyra_core::impact::ImpactOptions { history_limit: 200 })?;
    let context = serde_json::to_string_pretty(&json!({
        "dependents": impact.dependents.iter().take(20).collect::<Vec<_>>(),
        "risk": impact.risk,
    }))?;
    let Some(reply) = send(&repo, &tasks::review(&diff, &context), dry_run)? else { return Ok(()) };
    let review: tasks::Review = veyra_ai::parse_json(&reply.text)?;
    if ctx.json {
        ui::json(&serde_json::to_value(&review)?);
        return Ok(());
    }
    anstream::println!("{} of {what}\n\n{}\n", ui::bold("Review"), review.summary.trim());
    if review.findings.is_empty() {
        anstream::println!("{}", ui::green("No problems found."));
    }
    for f in &review.findings {
        let sev = match f.severity.as_str() {
            "high" => ui::red("high  "),
            "medium" => ui::yellow("medium"),
            _ => ui::dim("low   "),
        };
        let loc = f.line.map(|l| format!("{}:{l}", f.file)).unwrap_or_else(|| f.file.clone());
        anstream::println!("{sev} {}  {}", ui::bold(&f.title), ui::dim(&loc));
        for line in f.detail.trim().lines() {
            anstream::println!("       {line}");
        }
    }
    anstream::println!("\n{}", ui::dim("AI review is advisory; it can miss problems and be wrong."));
    Ok(())
}

fn summarize(ctx: &Ctx, range: Option<String>, dry_run: bool) -> Result<()> {
    let repo = ctx.repo()?;
    let head = repo.head_commit_id()?.context("HEAD is unborn")?;
    let (base, tip) = match range.as_deref() {
        Some(r) if r.contains("..") => {
            let (a, b) = r.split_once("..").unwrap();
            (Some(repo.rev_parse_commit(a)?), repo.rev_parse_commit(if b.is_empty() { "HEAD" } else { b })?)
        }
        Some(r) => (Some(repo.rev_parse_commit(r)?), head),
        None => {
            let base = repo
                .default_branch()?
                .and_then(|b| repo.rev_parse_commit(&b).ok())
                .and_then(|b| repo.git().merge_base(b, head).ok().flatten().map(|m| m.detach()))
                .filter(|b| *b != head);
            (base, head)
        }
    };
    let mut commits = String::new();
    let mut n = 0;
    for c in repo.log(&[tip], Some(500))? {
        if Some(c.id) == base {
            break;
        }
        n += 1;
        commits.push_str(&format!("commit {}\n{}\n\n", veyra_core::short(&c.id), c.message.trim()));
    }
    if n == 0 {
        bail!("no commits to summarize");
    }
    let stat = match base {
        Some(b) => flat::diff(&repo.flat_from_commit(b)?, &repo.flat_from_commit(tip)?)
            .iter()
            .map(|c| format!("{} {}", c.status_letter(), c.path))
            .collect::<Vec<_>>()
            .join("\n"),
        None => String::new(),
    };
    let Some(reply) = send(&repo, &tasks::summarize(&commits, &stat), dry_run)? else { return Ok(()) };
    anstream::println!("{}", reply.text.trim());
    Ok(())
}

fn risks(ctx: &Ctx, s: &ScopeArgs, dry_run: bool) -> Result<()> {
    let repo = ctx.repo()?;
    let (old, new, what) = scope(&repo, s)?;
    let diff = diff_text(&repo, &old, &new)?;
    if diff.is_empty() {
        bail!("no changes in {what}");
    }
    let impact = repo.impact(&old, &new, &veyra_core::impact::ImpactOptions::default())?;
    let impact_json = serde_json::to_string_pretty(&impact)?;
    let Some(reply) = send(&repo, &tasks::risks(&impact_json, &diff), dry_run)? else { return Ok(()) };
    let a: tasks::RiskAssessment = veyra_ai::parse_json(&reply.text)?;
    if ctx.json {
        ui::json(&json!({"deterministic": impact, "ai": a}));
        return Ok(());
    }
    anstream::println!(
        "{} of {what}: deterministic risk {:?} (score {})\n\n{}\n",
        ui::bold("Risks"),
        impact.risk.level,
        impact.risk.score,
        a.assessment.trim()
    );
    for r in &a.risks {
        anstream::println!(
            "• {}\n    {}\n    {}",
            ui::bold(&r.title),
            r.detail.trim(),
            ui::dim(&format!("evidence: {}", r.evidence.trim()))
        );
    }
    if !a.suggested_checks.is_empty() {
        anstream::println!("\n{}", ui::bold("Check before merging"));
        for c in &a.suggested_checks {
            anstream::println!("  - {c}");
        }
    }
    Ok(())
}

/// The conflicted paths in the index, with their stage 1/2/3 blob ids.
type Stages = (Option<veyra_core::ObjectId>, Option<veyra_core::ObjectId>, Option<veyra_core::ObjectId>);

fn conflicted(repo: &Repo) -> Result<Vec<(String, Stages)>> {
    use veyra_core::ByteSlice;
    let index = repo.load_index()?;
    let mut out: Vec<(String, Stages)> = Vec::new();
    for e in index.entries() {
        let stage = e.stage_raw();
        if stage == 0 {
            continue;
        }
        let path = e.path(&index).to_str_lossy().into_owned();
        if out.last().is_none_or(|(p, _)| *p != path) {
            out.push((path, (None, None, None)));
        }
        let slot = &mut out.last_mut().unwrap().1;
        match stage {
            1 => slot.0 = Some(e.id),
            2 => slot.1 = Some(e.id),
            _ => slot.2 = Some(e.id),
        }
    }
    Ok(out)
}

/// `veyra ai resolve` / `merge --assist`: propose resolutions for conflicted files.
pub(crate) fn resolve(ctx: &Ctx, only: &[String], dry_run: bool) -> Result<()> {
    let repo = ctx.repo()?;
    let workdir = repo.workdir()?.to_owned();
    let all = conflicted(&repo)?;
    let targets: Vec<_> = all.into_iter().filter(|(p, _)| only.is_empty() || only.contains(p)).collect();
    if targets.is_empty() {
        bail!("no conflicted files{}", if only.is_empty() { "" } else { " among those paths" });
    }
    let text = |id: Option<veyra_core::ObjectId>| -> Result<String> {
        Ok(match id {
            Some(id) => String::from_utf8_lossy(&repo.read_blob(id)?).into_owned(),
            None => String::new(),
        })
    };
    let mut resolved = 0;
    for (path, (base, ours, theirs)) in &targets {
        let file = workdir.join(path);
        let current = std::fs::read_to_string(&file).with_context(|| format!("could not read {path}"))?;
        let request = tasks::resolve(path, &text(*base)?, &text(*ours)?, &text(*theirs)?, &current);
        let Some(reply) = send(&repo, &request, dry_run)? else { continue };
        let r: tasks::Resolution = veyra_ai::parse_json(&reply.text)?;

        // Deterministic checks before the user even sees it.
        if r.resolved.lines().any(|l| l.starts_with("<<<<<<<") || l.starts_with(">>>>>>>") || l == "=======") {
            ui::warn(&format!("{path}: the proposal still has conflict markers; skipped"));
            continue;
        }
        if let Some(lang) = veyra_core::semantic_engine::Lang::from_path(path)
            && veyra_core::semantic_engine::parse(lang, &r.resolved).is_none_or(|p| p.has_errors())
        {
            ui::warn(&format!("{path}: the proposal doesn't parse as {}; skipped", lang.name()));
            continue;
        }
        let (hunks, added, removed) = veyra_core::diff::unified(text(*ours)?.as_bytes(), r.resolved.as_bytes(), 3);
        anstream::println!(
            "{} {path}  confidence {}  {}",
            ui::bold("Proposed resolution for"),
            match r.confidence.as_str() {
                "high" => ui::green("high"),
                "medium" => ui::yellow("medium"),
                _ => ui::red("low"),
            },
            ui::dim(&format!("(+{added} -{removed} vs ours)"))
        );
        anstream::println!("{}", r.explanation.trim());
        anstream::println!("{}", ui::colour_diff(&hunks));
        // Uncertain resolutions always need a human, even with --yes.
        let accepted = if r.confidence != "high" && ctx.yes {
            ui::warn(&format!("{path}: confidence is {}, so it needs interactive approval; left as is", r.confidence));
            false
        } else {
            matches!(ask(ctx, &format!("Use this resolution for {path}?"), false)?, Answer::Yes)
        };
        if accepted {
            // Journaled (with a snapshot of the conflicted file), so `veyra undo` brings it back.
            super::journal(ctx, &repo, "resolve", true, || {
                std::fs::write(&file, &r.resolved)?;
                let spec = veyra_core::worktree::Pathspec::new(&repo, &workdir, std::slice::from_ref(path))?;
                repo.add(&spec, false)?;
                Ok(((), format!("resolve {path} (AI-assisted, {} confidence)", r.confidence)))
            })?;
            anstream::println!("{} {path}", ui::green("Resolved and staged"));
            resolved += 1;
        }
    }
    if !dry_run {
        anstream::println!(
            "{} of {} resolved. Review with `veyra diff --staged`; `veyra undo` reverts.",
            resolved,
            super::plural(targets.len(), "conflicted file")
        );
    }
    Ok(())
}

/// Largest amount of source text sent for an intent suggestion.
const MAX_SUGGEST_BYTES: usize = 400_000;

/// `veyra intent suggest <paths>`: an AI-proposed intent whose guards are checked before you see it.
pub(crate) fn intent_suggest(ctx: &Ctx, paths: &[String], dry_run: bool) -> Result<()> {
    use veyra_intent::FileSource;
    let repo = ctx.repo()?;
    let root = repo.workdir()?.to_owned();
    let source = super::tools::WorktreeSource::new(&repo)?;
    let cwd_rel = |p: &str| -> String {
        let abs = ctx.cwd.join(p);
        abs.strip_prefix(&root).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or_else(|_| p.replace('\\', "/"))
    };
    let wanted: Vec<String> = paths.iter().map(|p| cwd_rel(p).trim_end_matches('/').to_owned()).collect();
    let selected: Vec<String> = source
        .paths()
        .into_iter()
        .filter(|p| wanted.iter().any(|w| w.is_empty() || w == "." || p == w || p.starts_with(&format!("{w}/"))))
        .collect();
    if selected.is_empty() {
        bail!("no files match {}", paths.join(", "));
    }
    let mut files = String::new();
    for p in &selected {
        if let Some(text) = source.read(p) {
            files.push_str(&format!("=== {p} ===\n{text}\n"));
        }
        if files.len() > MAX_SUGGEST_BYTES {
            bail!("these files are too large to send ({} KB); pick fewer or smaller files", files.len() / 1024);
        }
    }
    let existing: String = veyra_intent::load_all(&root)?
        .iter()
        .map(|(_, i)| format!("{}: {}", i.id, i.title))
        .collect::<Vec<_>>()
        .join("\n");
    let Some(reply) = send(&repo, &tasks::intent_suggest(&files, &existing), dry_run)? else { return Ok(()) };
    let s: tasks::IntentSuggestion = veyra_ai::parse_json(&reply.text)?;

    let today = jiff::Zoned::now().strftime("%Y-%m-%d").to_string();
    let mut intent = veyra_intent::Intent::new(&s.title, &s.kind, &today);
    intent.objective = s.objective;
    intent.constraints = s.constraints;
    intent.verification = vec!["veyra intent verify".into()];
    let mut dropped = Vec::new();
    for g in s.guards {
        if g.paths.is_empty() || (g.must_contain.is_none() && g.must_not_contain.is_none()) {
            continue;
        }
        let guard = veyra_intent::Guard {
            paths: g.paths,
            must_contain: g.must_contain.filter(|t| !t.is_empty()),
            must_not_contain: g.must_not_contain.filter(|t| !t.is_empty()),
            message: Some(g.message),
            ..Default::default()
        };
        // Keep only guards that hold right now (and whose globs are valid).
        let mut probe = intent.clone();
        probe.guards = vec![guard.clone()];
        match probe.verify(&source) {
            Ok(v) if v.is_empty() => intent.guards.push(guard),
            Ok(v) => dropped.push(v[0].detail.clone()),
            Err(e) => dropped.push(e.to_string()),
        }
    }
    intent.validate()?;
    anstream::println!("{}\n", ui::bold("Proposed intent:"));
    anstream::println!("{}", intent.to_toml()?);
    for d in &dropped {
        ui::warn(&format!("dropped a proposed guard that the current code doesn't satisfy: {d}"));
    }
    if !matches!(ask(ctx, "Record this intent?", false)?, Answer::Yes) {
        bail!("not recorded");
    }
    let path = veyra_intent::save(&root, &intent)?;
    let rel = path.strip_prefix(&root).unwrap_or(&path).display().to_string();
    anstream::println!("Intent recorded: {}  {}  ({rel}; review and commit it)", ui::yellow(&intent.id), intent.title);
    Ok(())
}
