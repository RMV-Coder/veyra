//! Pre-commit checks, doctor, security, intent, config, and plumbing commands.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::json;
use veyra_core::gix::bstr::ByteSlice;
use veyra_core::gix::objs::tree::EntryKind;
use veyra_core::{FlatTree, ObjectId, Repo, flat};
use veyra_intent::{FileSource, Intent};
use veyra_security::{Confidence, Finding, Policy, Scanner};

use super::{journal, plural};
use crate::cli::{ConfigArgs, IntentCommand, SecurityCommand};
use crate::{Ctx, Reported, ui};

const MAX_SCAN_BYTES: u64 = 1024 * 1024;
const SECRETS_FILE: &str = ".veyra/secrets.toml";

// ------------------------------------------------------------------------------------------
// File sources (worktree, index, or commit) shared by security scans and intent checks.

/// Files in a flat tree (the index or a commit).
struct TreeSource<'r> {
    repo: &'r Repo,
    flat: FlatTree,
}

impl FileSource for TreeSource<'_> {
    fn paths(&self) -> Vec<String> {
        self.flat
            .iter()
            .filter(|(_, e)| matches!(e.kind, EntryKind::Blob | EntryKind::BlobExecutable))
            .map(|(p, _)| p.to_string())
            .collect()
    }
    fn read(&self, path: &str) -> Option<String> {
        let entry = self.flat.get(path.as_bytes().as_bstr())?;
        let header = self.repo.git().find_header(entry.id).ok()?;
        if header.size() > MAX_SCAN_BYTES {
            return None;
        }
        let data = self.repo.read_blob(entry.id).ok()?;
        (!veyra_core::diff::is_binary(&data)).then(|| data.to_str_lossy().into_owned())
    }
}

/// Files in the working tree: tracked (minus deleted) plus untracked, non-ignored.
struct WorktreeSource {
    root: std::path::PathBuf,
    paths: Vec<String>,
}

impl WorktreeSource {
    fn new(repo: &Repo) -> Result<Self> {
        let changes = repo.worktree_changes()?;
        let deleted: BTreeSet<_> = changes.deleted.iter().collect();
        let mut paths: Vec<String> =
            repo.flat_index()?.into_keys().filter(|p| !deleted.contains(p)).map(|p| p.to_string()).collect();
        paths.extend(changes.untracked.iter().map(|p| p.to_string()));
        paths.sort();
        paths.dedup();
        Ok(WorktreeSource { root: repo.workdir()?.to_owned(), paths })
    }
}

impl FileSource for WorktreeSource {
    fn paths(&self) -> Vec<String> {
        self.paths.clone()
    }
    fn read(&self, path: &str) -> Option<String> {
        let full = self.root.join(path);
        if std::fs::metadata(&full).ok()?.len() > MAX_SCAN_BYTES {
            return None;
        }
        let data = std::fs::read(full).ok()?;
        (!veyra_core::diff::is_binary(&data)).then(|| data.to_str_lossy().into_owned())
    }
}

fn load_policy(repo: &Repo) -> Result<Policy> {
    let path = repo.workdir()?.join(SECRETS_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => Policy::from_toml(&text),
        Err(_) => Ok(Policy::default()),
    }
}

fn scan_source(scanner: &Scanner, source: &dyn FileSource) -> Vec<Finding> {
    let mut out = Vec::new();
    for path in source.paths() {
        if scanner.is_excluded(&path) || path == SECRETS_FILE {
            continue;
        }
        if let Some(text) = source.read(&path) {
            out.extend(scanner.scan(&path, &text, None));
        }
    }
    out
}

fn print_findings(findings: &[Finding]) {
    for f in findings {
        let level = if f.confidence == Confidence::High { ui::red("HIGH  ") } else { ui::yellow("MEDIUM") };
        anstream::println!(
            "  {level} {}:{}  {}  {}",
            f.path,
            f.line,
            f.description,
            ui::dim(&format!("{}  [{}]", f.redacted, f.fingerprint))
        );
    }
}

/// Load intents from `.veyra/intents/*.toml` entries of a flat tree.
fn intents_in(source: &TreeSource<'_>) -> Result<Vec<Intent>> {
    let files: Vec<(String, String)> = source
        .paths()
        .into_iter()
        .filter(|p| p.starts_with(veyra_intent::INTENTS_DIR) && p.ends_with(".toml"))
        .filter_map(|p| source.read(&p).map(|t| (p, t)))
        .collect();
    veyra_intent::load_from_texts(&files)
}

/// Secret scan of added lines + intent verification, run by `veyra commit`.
pub fn pre_commit_checks(repo: &Repo, allow_secrets: bool) -> Result<()> {
    let policy_mode = repo.config_get("veyra.secretsPolicy").unwrap_or_else(|| "block".into());
    let head = repo.flat_head()?;
    let index = repo.flat_index()?;

    if policy_mode != "off" {
        let scanner = Scanner::new(&load_policy(repo)?)?;
        let mut findings = Vec::new();
        for change in flat::diff(&head, &index) {
            let Some(new) = change.new.filter(|e| matches!(e.kind, EntryKind::Blob | EntryKind::BlobExecutable)) else {
                continue;
            };
            let path = change.path.to_string();
            if scanner.is_excluded(&path) || path == SECRETS_FILE {
                continue;
            }
            if repo.git().find_header(new.id)?.size() > MAX_SCAN_BYTES {
                continue;
            }
            let new_bytes = repo.read_blob(new.id)?;
            if veyra_core::diff::is_binary(&new_bytes) {
                continue;
            }
            let new_text = new_bytes.to_str_lossy();
            let old_text = match change.old {
                Some(old) if matches!(old.kind, EntryKind::Blob | EntryKind::BlobExecutable) => {
                    repo.read_blob(old.id)?.to_str_lossy().into_owned()
                }
                _ => String::new(),
            };
            let mut added = BTreeSet::new();
            let diff = similar::TextDiff::from_lines(old_text.as_str(), new_text.as_ref());
            for c in diff.iter_all_changes() {
                if c.tag() == similar::ChangeTag::Insert
                    && let Some(i) = c.new_index()
                {
                    added.insert(i + 1);
                }
            }
            findings.extend(scanner.scan(&path, &new_text, Some(&added)));
        }
        if !findings.is_empty() {
            let high = findings.iter().any(|f| f.confidence == Confidence::High);
            anstream::eprintln!(
                "{} potential secret(s) in the staged changes:",
                if high { ui::red("SECRET SCAN") } else { ui::yellow("SECRET SCAN") }
            );
            for f in &findings {
                let level = if f.confidence == Confidence::High { ui::red("high") } else { ui::yellow("medium") };
                anstream::eprintln!(
                    "  {level}  {}:{}  {}  {}",
                    f.path,
                    f.line,
                    f.description,
                    ui::dim(&format!("{}  fingerprint {}", f.redacted, f.fingerprint))
                );
            }
            if high && policy_mode == "block" && !allow_secrets {
                bail!(
                    "commit blocked: high-confidence secrets are staged\n  \
                     options:\n    • remove the credential (and rotate it if it was ever shared)\n    \
                     • mark a false positive: veyra security allow <fingerprint>\n    \
                     • commit anyway: veyra commit --allow-secrets\n    \
                     • change policy: veyra config veyra.secretsPolicy warn"
                );
            }
        }
    }

    let intent_mode = repo.config_get("veyra.intentPolicy").unwrap_or_else(|| "warn".into());
    if intent_mode != "off" {
        let source = TreeSource { repo, flat: index };
        let mut violations = Vec::new();
        for intent in intents_in(&source)? {
            violations.extend(intent.verify(&source)?);
        }
        if !violations.is_empty() {
            anstream::eprintln!("{} this commit may contradict recorded intent:", ui::yellow("INTENT CHECK"));
            for v in &violations {
                anstream::eprintln!(
                    "  {}  {} — {}\n         {}",
                    ui::yellow(&v.intent_id),
                    v.intent_title,
                    v.message,
                    ui::dim(&v.detail)
                );
            }
            if intent_mode == "block" {
                bail!(
                    "commit blocked by veyra.intentPolicy=block\n  hint: review the intent (`veyra intent show <id>`), update or close it, or commit with --no-verify"
                );
            }
            ui::hint(
                "review with `veyra intent show <id>`; if the decision changed, update or close the intent in the same commit",
            );
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------

pub fn security(ctx: &Ctx, command: SecurityCommand) -> Result<()> {
    let repo = ctx.repo()?;
    match command {
        SecurityCommand::Scan { staged, rev } => {
            let scanner = Scanner::new(&load_policy(&repo)?)?;
            let (label, findings) = if staged {
                ("index", scan_source(&scanner, &TreeSource { repo: &repo, flat: repo.flat_index()? }))
            } else if let Some(rev) = &rev {
                (
                    "revision",
                    scan_source(
                        &scanner,
                        &TreeSource { repo: &repo, flat: repo.flat_from_tree(repo.rev_parse_tree(rev)?)? },
                    ),
                )
            } else {
                ("working tree", scan_source(&scanner, &WorktreeSource::new(&repo)?))
            };
            if ctx.json {
                ui::json(&json!(
                    findings
                        .iter()
                        .map(|f| json!({
                            "rule": f.rule_id, "description": f.description, "confidence": f.confidence.to_string(),
                            "path": f.path, "line": f.line, "redacted": f.redacted, "fingerprint": f.fingerprint,
                        }))
                        .collect::<Vec<_>>()
                ));
            } else if findings.is_empty() {
                anstream::println!("No potential secrets found in the {label}.");
            } else {
                anstream::println!("{} in the {label}:", plural(findings.len(), "potential secret"));
                print_findings(&findings);
                anstream::println!(
                    "\n{}",
                    ui::dim(
                        "Allowlist a reviewed false positive with `veyra security allow <fingerprint>`. Scanning is a safety net, not a guarantee."
                    )
                );
            }
            if findings.iter().any(|f| f.confidence == Confidence::High) {
                return Err(Reported.into());
            }
        }
        SecurityCommand::Allow { fingerprint } => {
            if fingerprint.len() != 16 || !fingerprint.chars().all(|c| c.is_ascii_hexdigit()) {
                bail!(
                    "'{fingerprint}' is not a fingerprint (expected 16 hex digits, as printed by `veyra security scan`)"
                );
            }
            let path = repo.workdir()?.join(SECRETS_FILE);
            let mut table: toml::Table = match std::fs::read_to_string(&path) {
                Ok(t) => toml::from_str(&t).context("invalid .veyra/secrets.toml")?,
                Err(_) => toml::Table::new(),
            };
            let allow = table.entry("allow").or_insert_with(|| toml::Value::Array(Vec::new()));
            let list = allow.as_array_mut().context("`allow` in .veyra/secrets.toml must be an array")?;
            if list.iter().any(|v| v.as_str() == Some(fingerprint.as_str())) {
                anstream::println!("{fingerprint} is already allowlisted.");
                return Ok(());
            }
            list.push(toml::Value::String(fingerprint.clone()));
            std::fs::create_dir_all(path.parent().expect("has parent"))?;
            let mut text = String::from(
                "# Veyra secret-scan policy. Fingerprints identify reviewed false positives\n# (a hash of rule + path + match — never the secret itself). Commit this file.\n",
            );
            text.push_str(&toml::to_string_pretty(&table)?);
            std::fs::write(&path, text)?;
            anstream::println!(
                "Allowlisted {fingerprint} in {SECRETS_FILE} (commit it so the whole team shares the decision)."
            );
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------

pub fn intent(ctx: &Ctx, command: IntentCommand) -> Result<()> {
    let repo = ctx.repo()?;
    let root = repo.workdir()?.to_owned();
    match command {
        IntentCommand::Create {
            title,
            kind,
            objective,
            problem,
            solution,
            constraint,
            related,
            verification,
            guard_path,
            guard_contains,
            guard_message,
        } => {
            let today = jiff::Zoned::now().strftime("%Y-%m-%d").to_string();
            let mut intent = Intent::new(&title, &kind, &today);
            intent.objective = objective.unwrap_or_default();
            intent.problem = problem.unwrap_or_default();
            intent.solution = solution.unwrap_or_default();
            intent.constraints = constraint;
            intent.related = related;
            intent.verification = verification;
            match (guard_path.is_empty(), guard_contains) {
                (false, Some(text)) => intent.guards.push(veyra_intent::Guard {
                    paths: guard_path,
                    must_contain: Some(text),
                    message: guard_message,
                    ..Default::default()
                }),
                (true, None) => {}
                _ => bail!("--guard-path and --guard-contains must be used together"),
            }
            let path = veyra_intent::save(&root, &intent)?;
            let rel = path.strip_prefix(&root).unwrap_or(&path).display().to_string();
            if ctx.json {
                ui::json(&json!({"id": intent.id, "path": rel}));
            } else {
                anstream::println!("Intent created: {}  {}", ui::yellow(&intent.id), intent.title);
                anstream::println!("  file: {rel}  (edit it to add detail or more guards; commit it with your change)");
                anstream::println!("  link commits with: veyra commit --intent {}", intent.id);
            }
        }
        IntentCommand::List { all } => {
            let intents = veyra_intent::load_all(&root)?;
            let shown: Vec<_> =
                intents.iter().filter(|(_, i)| all || i.status == veyra_intent::Status::Active).collect();
            if ctx.json {
                ui::json(&json!(shown.iter().map(|(_, i)| json!({"id": i.id, "title": i.title, "kind": i.kind, "status": i.status.to_string(), "created": i.created, "guards": i.guards.len()})).collect::<Vec<_>>()));
                return Ok(());
            }
            if shown.is_empty() {
                anstream::println!(
                    "No {}intents recorded. `veyra intent create --title \"…\"` records one.",
                    if all { "" } else { "active " }
                );
            }
            for (_, i) in shown {
                anstream::println!(
                    "{}  {:<10} {:<9} {}  {}",
                    ui::yellow(&i.id),
                    i.kind,
                    i.status,
                    i.title,
                    ui::dim(&format!("({}, {})", i.created, plural(i.guards.len(), "guard")))
                );
            }
        }
        IntentCommand::Show { id } => {
            let all = veyra_intent::load_all(&root)?;
            let (path, i) = veyra_intent::find(&all, &id)?;
            anstream::println!("{}", ui::yellow(&format!("intent {}", i.id)));
            anstream::println!(
                "Title:      {}\nKind:       {}\nStatus:     {}\nCreated:    {}",
                i.title,
                i.kind,
                i.status,
                i.created
            );
            for (label, value) in [("Objective", &i.objective), ("Problem", &i.problem), ("Solution", &i.solution)] {
                if !value.is_empty() {
                    anstream::println!("{label}:{}{value}", " ".repeat(11 - label.len()));
                }
            }
            for (label, list) in [("Constraint", &i.constraints), ("Related", &i.related), ("Verify", &i.verification)]
            {
                for v in list {
                    anstream::println!("{label}:{}{v}", " ".repeat(11 - label.len()));
                }
            }
            for g in &i.guards {
                let check = g
                    .must_contain
                    .as_ref()
                    .map(|c| format!("must contain `{c}`"))
                    .or(g.must_match.as_ref().map(|m| format!("must match /{m}/")))
                    .or(g.must_not_contain.as_ref().map(|m| format!("must not contain `{m}`")))
                    .unwrap_or_default();
                anstream::println!("Guard:      {} {check}", g.paths.join(", "));
            }
            anstream::println!("File:       {}", path.strip_prefix(&root).unwrap_or(path).display());
        }
        IntentCommand::Verify { staged, rev } => {
            let (intents, violations) = if staged || rev.is_some() {
                let flat = if staged {
                    repo.flat_index()?
                } else {
                    repo.flat_from_tree(repo.rev_parse_tree(rev.as_deref().expect("checked"))?)?
                };
                let source = TreeSource { repo: &repo, flat };
                let intents = intents_in(&source)?;
                let mut v = Vec::new();
                for i in &intents {
                    v.extend(i.verify(&source)?);
                }
                (intents.len(), v)
            } else {
                let source = WorktreeSource::new(&repo)?;
                let intents = veyra_intent::load_all(&root)?;
                let mut v = Vec::new();
                for (_, i) in &intents {
                    v.extend(i.verify(&source)?);
                }
                (intents.len(), v)
            };
            if ctx.json {
                ui::json(
                    &json!({"intents": intents, "violations": violations.iter().map(|v| json!({"intent": v.intent_id, "title": v.intent_title, "guard": v.guard_index, "message": v.message, "detail": v.detail})).collect::<Vec<_>>()}),
                );
            } else if violations.is_empty() {
                anstream::println!("All guards of {} hold.", plural(intents, "intent"));
            } else {
                for v in &violations {
                    anstream::println!(
                        "{}\n\n  Intent:   {} {}\n  Expected: {}\n  Found:    {}\n",
                        ui::red("POTENTIAL INTENT VIOLATION"),
                        ui::yellow(&v.intent_id),
                        v.intent_title,
                        v.message,
                        v.detail
                    );
                }
                anstream::println!(
                    "Review the original decision (`veyra intent show <id>`). If it changed, update or close the intent."
                );
            }
            if !violations.is_empty() {
                return Err(Reported.into());
            }
        }
        IntentCommand::Close { id, superseded_by } => {
            let all = veyra_intent::load_all(&root)?;
            let (_, found) = veyra_intent::find(&all, &id)?;
            let mut intent = found.clone();
            intent.status =
                if superseded_by.is_some() { veyra_intent::Status::Superseded } else { veyra_intent::Status::Closed };
            intent.superseded_by = superseded_by;
            veyra_intent::save(&root, &intent)?;
            anstream::println!("Intent {} is now {}. Commit the change to share it.", intent.id, intent.status);
        }
        IntentCommand::Log { id } => {
            let all = veyra_intent::load_all(&root)?;
            let (_, intent) = veyra_intent::find(&all, &id)?;
            let Some(head) = repo.head_commit_id()? else {
                anstream::println!("No commits yet.");
                return Ok(());
            };
            let commits: Vec<_> = repo
                .log(&[head], None)?
                .into_iter()
                .filter(|c| {
                    veyra_core::commit::parse_trailers(&c.message)
                        .iter()
                        .any(|(k, v)| k.eq_ignore_ascii_case("Intent") && v == &intent.id)
                })
                .collect();
            if commits.is_empty() {
                anstream::println!(
                    "No commits reference {} yet (link with `veyra commit --intent {}`).",
                    intent.id,
                    intent.id
                );
            }
            for c in commits {
                anstream::println!("{} {}", ui::yellow(&veyra_core::short(&c.id)), c.summary());
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------

pub fn config(ctx: &Ctx, args: ConfigArgs) -> Result<()> {
    use veyra_core::config::{self as cfg, Scope};
    let repo = ctx.repo().ok();
    let scope = if args.global { Scope::Global } else { Scope::Local };
    let path_for = |scope: Scope| -> Result<std::path::PathBuf> {
        match (scope, &repo) {
            (Scope::Global, _) => cfg::global_config_path(),
            (Scope::Local, Some(r)) => r.config_path(Scope::Local),
            (Scope::Local, None) => bail!("not in a repository; use --global"),
        }
    };
    if args.list {
        let mut files = vec![cfg::global_config_path()?];
        if !args.global
            && let Some(r) = &repo
        {
            files.push(r.config_path(Scope::Local)?);
        }
        for f in files {
            for (k, v) in cfg::list_file(&f)? {
                anstream::println!("{k}={v}");
            }
        }
        return Ok(());
    }
    let key = args.key.context("usage: veyra config [--global] <key> [<value>] | --unset <key> | --list")?;
    if args.unset {
        if !cfg::unset_in_file(&path_for(scope)?, &key)? {
            bail!("{key} is not set");
        }
        return Ok(());
    }
    match args.value {
        Some(value) => {
            let path = path_for(scope)?;
            match &repo {
                Some(r) if scope == Scope::Local => journal(ctx, r, "config", false, || {
                    cfg::set_in_file(&path, &key, &value)?;
                    Ok(((), format!("config {key}")))
                })?,
                _ => cfg::set_in_file(&path, &key, &value)?,
            }
        }
        None => {
            let value = match &repo {
                Some(r) => r.config_get(&key),
                None => cfg::list_file(&cfg::global_config_path()?)?
                    .into_iter()
                    .rev()
                    .find(|(k, _)| k.eq_ignore_ascii_case(&key))
                    .map(|(_, v)| v),
            };
            match value {
                Some(v) => anstream::println!("{v}"),
                None => return Err(Reported.into()),
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------
// doctor

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    Pass,
    Info,
    Warn,
    Fail,
}

struct Check {
    name: &'static str,
    level: Level,
    evidence: String,
    fix: Option<String>,
}

fn dir_stats(dir: &Path) -> (u64, u64) {
    let mut count = 0;
    let mut bytes = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if let Ok(md) = e.metadata()
                && md.is_file()
            {
                count += 1;
                bytes += md.len();
            }
        }
    }
    (count, bytes)
}

fn human(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GiB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MiB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KiB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

pub fn doctor(ctx: &Ctx) -> Result<()> {
    let repo = ctx.repo()?;
    let mut checks: Vec<Check> = Vec::new();

    // Identity
    checks.push(match repo.identity() {
        Some((n, e)) => Check { name: "Identity", level: Level::Pass, evidence: format!("{n} <{e}>"), fix: None },
        None => Check {
            name: "Identity",
            level: Level::Warn,
            evidence: "user.name / user.email are not set".into(),
            fix: Some("veyra config --global user.name \"Your Name\"".into()),
        },
    });

    // Integrity: refs resolve; commits and their trees exist along history (bounded walk).
    {
        let mut problems = Vec::new();
        let refs = repo.all_refs()?;
        let mut tips = Vec::new();
        for (name, id) in &refs {
            if !repo.git().has_object(id) {
                problems.push(format!("{name} points to missing object {}", veyra_core::short(id)));
            } else if let Ok(c) = repo.peel_to_commit(*id) {
                tips.push(c);
            }
        }
        tips.sort();
        tips.dedup();
        let mut walked = 0;
        if problems.is_empty() && !tips.is_empty() {
            match repo.log(&tips, Some(20_000)) {
                Ok(commits) => {
                    walked = commits.len();
                    for c in &commits {
                        if !repo.git().has_object(c.tree) {
                            problems.push(format!(
                                "commit {} references missing tree {}",
                                veyra_core::short(&c.id),
                                veyra_core::short(&c.tree)
                            ));
                        }
                    }
                }
                Err(err) => problems.push(format!("history walk failed: {err}")),
            }
        }
        if let Err(err) = repo.load_index().and_then(|i| i.verify_integrity().map_err(|e| anyhow::anyhow!("{e}"))) {
            problems.push(format!("index: {err}"));
        }
        checks.push(if problems.is_empty() {
            Check {
                name: "Integrity",
                level: Level::Pass,
                evidence: format!(
                    "{} refs resolve; {} commits and their trees present; index checksum ok",
                    refs.len(),
                    walked
                ),
                fix: None,
            }
        } else {
            Check {
                name: "Integrity",
                level: Level::Fail,
                evidence: problems.join("; "),
                fix: Some("git fsck --full   (then restore missing objects from a remote or backup)".into()),
            }
        });
    }

    // In-progress operations
    if let Some(m) = repo.merge_head()? {
        checks.push(Check {
            name: "In progress",
            level: Level::Warn,
            evidence: format!("a merge with {} is in progress", veyra_core::short(&m)),
            fix: Some("resolve and `veyra commit`, or `veyra merge --abort`".into()),
        });
    } else if let Some(s) = repo.in_progress() {
        checks.push(Check {
            name: "In progress",
            level: Level::Warn,
            evidence: format!("git operation in progress: {s}"),
            fix: Some("finish or abort it with git".into()),
        });
    }

    // Storage
    {
        let objects = repo.common_dir().join("objects");
        let (mut loose, mut loose_bytes) = (0, 0);
        if let Ok(entries) = std::fs::read_dir(&objects) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.len() == 2 && name.chars().all(|c| c.is_ascii_hexdigit()) {
                    let (c, b) = dir_stats(&e.path());
                    loose += c;
                    loose_bytes += b;
                }
            }
        }
        let packs = std::fs::read_dir(objects.join("pack"))
            .map(|d| d.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "pack")).count())
            .unwrap_or(0);
        let (_, pack_bytes) = dir_stats(&objects.join("pack"));
        let evidence = format!(
            "{} total; {loose} loose objects ({}), {packs} packs ({})",
            human(loose_bytes + pack_bytes),
            human(loose_bytes),
            human(pack_bytes)
        );
        let needs_gc = loose > 6700 || packs > 50;
        checks.push(Check {
            name: "Storage",
            level: if needs_gc { Level::Warn } else { Level::Pass },
            evidence,
            fix: needs_gc.then(|| "veyra maintenance run".into()),
        });
    }

    // Large files and secrets in HEAD
    if let Some(head) = repo.head_commit_id()? {
        let flat = repo.flat_from_commit(head)?;
        let mut large = Vec::new();
        for (path, e) in &flat {
            if matches!(e.kind, EntryKind::Blob | EntryKind::BlobExecutable)
                && let Ok(h) = repo.git().find_header(e.id)
                && h.size() > 10 * 1024 * 1024
            {
                large.push(format!("{path} ({})", human(h.size())));
            }
        }
        checks.push(if large.is_empty() {
            Check {
                name: "Large files",
                level: Level::Pass,
                evidence: "no blobs over 10 MiB in HEAD".into(),
                fix: None,
            }
        } else {
            Check {
                name: "Large files",
                level: Level::Warn,
                evidence: format!(
                    "{}: {}",
                    plural(large.len(), "large blob"),
                    large.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
                ),
                fix: Some("consider Git LFS for binary assets".into()),
            }
        });

        let scanner = Scanner::new(&load_policy(&repo)?)?;
        let findings = scan_source(&scanner, &TreeSource { repo: &repo, flat });
        let high = findings.iter().filter(|f| f.confidence == Confidence::High).count();
        checks.push(match findings.len() {
            0 => Check {
                name: "Secrets",
                level: Level::Pass,
                evidence: "no potential secrets in HEAD".into(),
                fix: None,
            },
            n => Check {
                name: "Secrets",
                level: if high > 0 { Level::Fail } else { Level::Warn },
                evidence: format!(
                    "{n} potential secret(s) in HEAD ({high} high-confidence), e.g. {}:{}",
                    findings[0].path, findings[0].line
                ),
                fix: Some(
                    "veyra security scan --rev HEAD   (rotate real credentials; history rewriting alone is not enough)"
                        .into(),
                ),
            },
        });
    }

    // Branches
    {
        let now = veyra_core::time::now_seconds();
        let base = repo.default_branch()?.and_then(|b| repo.ref_target(&format!("refs/heads/{b}")).ok().flatten());
        let (mut stale, mut merged) = (0, 0);
        let branches = repo.branches(false)?;
        for b in &branches {
            if b.is_head {
                continue;
            }
            if base.is_some_and(|base| base != b.target && repo.is_ancestor(b.target, base).unwrap_or(false)) {
                merged += 1;
            }
            if repo.commit_info(b.target).is_ok_and(|c| now - c.committer_time > 90 * 86_400) {
                stale += 1;
            }
        }
        let level = if stale + merged > 0 { Level::Info } else { Level::Pass };
        checks.push(Check {
            name: "Branches",
            level,
            evidence: format!(
                "{}; {stale} stale (90+ days), {merged} fully merged",
                plural(branches.len(), "local branch")
            ),
            fix: (stale + merged > 0).then(|| "veyra branch stale".into()),
        });
    }

    // Veyra journal
    {
        let log = veyra_ops::OpLog::open(&repo)?;
        let ops = log.operations()?;
        let keep = repo.ref_target(&repo.keep_ref())?;
        let lock = repo.veyra_dir().join("lock");
        let level = if !ops.is_empty() && keep.is_none() {
            Level::Warn
        } else if lock.exists() {
            Level::Info
        } else {
            Level::Pass
        };
        let checkpoints = ops.iter().filter(|o| o.before.worktree.is_some()).count();
        let mut evidence =
            format!("{} recorded, {checkpoints} with worktree snapshots", plural(ops.len(), "operation"));
        if lock.exists() {
            evidence.push_str("; a lock file exists (another veyra command running, or a crash)");
        }
        checks.push(Check {
            name: "Journal",
            level,
            evidence,
            fix: (ops.len() > 5000).then(|| "veyra op prune --older-than 30d".into()),
        });
    }

    // Line endings
    if cfg!(windows) && repo.config_get("core.autocrlf").is_none() && repo.config_get("core.eol").is_none() {
        checks.push(Check {
            name: "Line endings",
            level: Level::Info,
            evidence: "core.autocrlf is not set; files are stored with the line endings you write".into(),
            fix: Some("add a .gitattributes with `* text=auto` for consistent line endings across platforms".into()),
        });
    }

    let fails = checks.iter().filter(|c| c.level == Level::Fail).count();
    let warns = checks.iter().filter(|c| c.level == Level::Warn).count();
    if ctx.json {
        ui::json(&json!({
            "checks": checks.iter().map(|c| json!({
                "name": c.name,
                "status": match c.level { Level::Pass => "pass", Level::Info => "info", Level::Warn => "warn", Level::Fail => "fail" },
                "evidence": c.evidence, "fix": c.fix,
            })).collect::<Vec<_>>(),
            "fail": fails, "warn": warns,
        }));
    } else {
        anstream::println!("{}\n", ui::bold("VEYRA REPOSITORY HEALTH"));
        for c in &checks {
            let tag = match c.level {
                Level::Pass => ui::green("PASS"),
                Level::Info => ui::cyan("INFO"),
                Level::Warn => ui::yellow("WARN"),
                Level::Fail => ui::red("FAIL"),
            };
            anstream::println!("  {tag}  {:<14} {}", c.name, c.evidence);
            if let Some(fix) = &c.fix {
                anstream::println!("        {:<14} {}", "", ui::dim(&format!("→ {fix}")));
            }
        }
        anstream::println!(
            "\n{} pass, {warns} warn, {fails} fail",
            checks.iter().filter(|c| c.level == Level::Pass).count()
        );
    }
    if fails > 0 {
        return Err(Reported.into());
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------
// plumbing

pub fn rev_parse(ctx: &Ctx, specs: Vec<String>, short: bool) -> Result<()> {
    let repo = ctx.repo()?;
    for s in specs {
        let id = repo.rev_parse(&s)?;
        anstream::println!("{}", if short { veyra_core::short(&id) } else { id.to_string() });
    }
    Ok(())
}

pub fn cat_file(ctx: &Ctx, kind: bool, size: bool, _pretty: bool, object: &str) -> Result<()> {
    let repo = ctx.repo()?;
    let id = repo.rev_parse(object)?;
    let obj = repo.git().find_object(id)?;
    if kind {
        anstream::println!("{}", obj.kind);
        return Ok(());
    }
    if size {
        anstream::println!("{}", obj.data.len());
        return Ok(());
    }
    match obj.kind {
        veyra_core::gix::objs::Kind::Tree => {
            let tree = obj.into_tree();
            for e in tree.decode()?.entries {
                let kind = if e.mode.is_tree() {
                    "tree"
                } else if e.mode.is_commit() {
                    "commit"
                } else {
                    "blob"
                };
                anstream::println!("{} {kind} {}\t{}", veyra_core::diff::mode_str(e.mode.kind()), e.oid, e.filename);
            }
        }
        _ => {
            let mut out = std::io::stdout().lock();
            out.write_all(&obj.data)?;
        }
    }
    Ok(())
}

pub fn hash_object(ctx: &Ctx, write: bool, files: Vec<std::path::PathBuf>) -> Result<()> {
    let repo = ctx.repo().ok();
    for file in files {
        let full = if file.is_absolute() { file.clone() } else { ctx.cwd.join(&file) };
        // Inside a worktree, apply the clean filter like `git hash-object <path>` does.
        let data = match &repo {
            Some(r) => match r.to_repo_path(&ctx.cwd, &file.to_string_lossy()) {
                Ok(rel) => {
                    r.read_worktree_as_git(rel.as_ref())?.with_context(|| format!("cannot read {}", file.display()))?
                }
                Err(_) => std::fs::read(&full)?,
            },
            None => std::fs::read(&full).with_context(|| format!("cannot read {}", file.display()))?,
        };
        let id: ObjectId = match (&repo, write) {
            (Some(r), true) => r.write_blob(&data)?,
            (Some(r), false) => r.hash_blob(&data)?,
            (None, true) => bail!("--write requires a repository"),
            (None, false) => veyra_core::gix::objs::compute_hash(
                veyra_core::gix::hash::Kind::Sha1,
                veyra_core::gix::objs::Kind::Blob,
                &data,
            )?,
        };
        anstream::println!("{id}");
    }
    Ok(())
}

pub fn ls_files(ctx: &Ctx, stage: bool) -> Result<()> {
    let repo = ctx.repo()?;
    let index = repo.load_index()?;
    for e in index.entries() {
        let path = e.path(&index);
        if stage {
            anstream::println!("{:06o} {} {}\t{path}", e.mode.bits(), e.id, e.stage_raw());
        } else {
            anstream::println!("{path}");
        }
    }
    Ok(())
}

pub fn show_ref(ctx: &Ctx) -> Result<()> {
    let repo = ctx.repo()?;
    for (name, id) in repo.all_refs()? {
        anstream::println!("{id} {name}");
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------
// verify

fn print_verification(label: &str, v: &veyra_core::sign::Verification) -> bool {
    match &v.outcome {
        None => {
            anstream::println!("{}  {label}  {}", ui::yellow("UNSIGNED"), ui::dim("no signature"));
            false
        }
        Some(o) => {
            let tag = if o.valid { ui::green("GOOD    ") } else { ui::red("BAD     ") };
            let who = o.signer.clone().or(o.key.clone()).unwrap_or_else(|| "(unknown signer)".into());
            anstream::println!("{tag}  {label}  {} {who} [{}, trust: {}]", o.format, o.status, o.trust);
            o.valid
        }
    }
}

pub fn verify(ctx: &Ctx, args: crate::cli::VerifyArgs) -> Result<()> {
    use crate::cli::VerifyCommand;
    let repo = ctx.repo()?;
    let json_of = |label: String, v: &veyra_core::sign::Verification| {
        json!({
            "object": v.id.to_string(), "label": label, "signed": v.outcome.is_some(),
            "valid": v.outcome.as_ref().is_some_and(|o| o.valid),
            "format": v.outcome.as_ref().map(|o| o.format.clone()),
            "status": v.outcome.as_ref().map(|o| o.status.clone()),
            "trust": v.outcome.as_ref().map(|o| o.trust.clone()),
            "signer": v.outcome.as_ref().and_then(|o| o.signer.clone()),
        })
    };
    match (args.command, args.history) {
        (Some(VerifyCommand::Commit { revision }), _) => {
            let id = repo.rev_parse_commit(revision.as_deref().unwrap_or("HEAD"))?;
            let v = repo.verify_commit(id)?;
            if ctx.json {
                ui::json(&json_of(veyra_core::short(&id), &v));
            } else {
                let ok = print_verification(&veyra_core::short(&id), &v);
                if let Some(o) = &v.outcome
                    && !o.output.is_empty()
                {
                    anstream::println!("{}", ui::dim(&o.output));
                }
                if !ok {
                    return Err(Reported.into());
                }
            }
            if !v.outcome.as_ref().is_some_and(|o| o.valid) {
                return Err(Reported.into());
            }
        }
        (Some(VerifyCommand::Tag { name }), _) => {
            let id = repo.rev_parse(&name)?;
            let v = repo.verify_tag(id)?;
            if ctx.json {
                ui::json(&json_of(name.clone(), &v));
            } else {
                print_verification(&name, &v);
            }
            if !v.outcome.as_ref().is_some_and(|o| o.valid) {
                return Err(Reported.into());
            }
        }
        (None, true) => {
            let start = repo.rev_parse_commit(args.rev.as_deref().unwrap_or("HEAD"))?;
            let commits = repo.log(&[start], Some(args.max_count))?;
            let mut rows = Vec::new();
            let (mut good, mut bad, mut unsigned) = (0, 0, 0);
            for c in &commits {
                let v = repo.verify_commit(c.id)?;
                match &v.outcome {
                    None => unsigned += 1,
                    Some(o) if o.valid => good += 1,
                    Some(_) => bad += 1,
                }
                if ctx.json {
                    rows.push(json_of(c.summary().to_owned(), &v));
                } else {
                    print_verification(&format!("{} {}", veyra_core::short(&c.id), c.summary()), &v);
                }
            }
            if ctx.json {
                ui::json(&json!({"commits": rows, "good": good, "bad": bad, "unsigned": unsigned}));
            } else {
                anstream::println!("\n{good} good, {bad} bad, {unsigned} unsigned (of {} commits)", commits.len());
            }
            if bad > 0 {
                return Err(Reported.into());
            }
        }
        (None, false) if args.provenance => {
            let id = repo.rev_parse_commit(args.rev.as_deref().unwrap_or("HEAD"))?;
            return show_attestations(ctx, &repo, id, true);
        }
        (None, false) => {
            bail!("usage: veyra verify commit [rev] | verify tag <name> | verify --history | verify --provenance")
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------
// storage & maintenance

pub fn storage(ctx: &Ctx, command: crate::cli::StorageCommand) -> Result<()> {
    let repo = ctx.repo()?;
    match command {
        crate::cli::StorageCommand::Stats => {
            let s = repo.storage_stats()?;
            if ctx.json {
                ui::json(&json!({
                    "loose_objects": s.loose_objects, "loose_bytes": s.loose_bytes, "packs": s.packs,
                    "pack_bytes": s.pack_bytes, "loose_also_packed": s.loose_also_packed,
                }));
            } else {
                anstream::println!(
                    "count: {}\nsize: {}\nin-pack loose copies: {}\npacks: {}\nsize-pack: {}",
                    s.loose_objects,
                    human(s.loose_bytes),
                    s.loose_also_packed,
                    s.packs,
                    human(s.pack_bytes)
                );
            }
        }
    }
    Ok(())
}

pub fn maintenance(ctx: &Ctx, command: crate::cli::MaintenanceCommand) -> Result<()> {
    use crate::cli::MaintenanceCommand;
    let repo = ctx.repo()?;
    let duration =
        |s: &str| veyra_core::time::parse_duration(s).with_context(|| format!("invalid duration '{s}' (e.g. 2w, 14d)"));
    match command {
        MaintenanceCommand::PackLoose => {
            let (removed, bytes) = repo.pack_loose_objects()?;
            anstream::println!("Packed {} ({} loose).", plural(removed, "object"), human(bytes));
        }
        MaintenanceCommand::Prune { expire, dry_run } => {
            let pruned = repo.prune_loose(duration(&expire)?, dry_run)?;
            for id in pruned.iter().take(20) {
                anstream::println!("{} {id}", if dry_run { "would prune" } else { "pruned" });
            }
            anstream::println!(
                "{} {} unreachable loose object(s) older than {expire}.",
                if dry_run { "Would prune" } else { "Pruned" },
                pruned.len()
            );
        }
        MaintenanceCommand::Run { prune_expire } => {
            let before = repo.storage_stats()?;
            let (packed, bytes) = repo.pack_loose_objects()?;
            let pruned = repo.prune_loose(duration(&prune_expire)?, false)?;
            let after = repo.storage_stats()?;
            if ctx.json {
                ui::json(
                    &json!({"packed": packed, "pruned": pruned.len(), "loose_before": before.loose_objects, "loose_after": after.loose_objects, "packs": after.packs}),
                );
            } else {
                anstream::println!(
                    "Packed {} ({}), pruned {} unreachable.",
                    plural(packed, "loose object"),
                    human(bytes),
                    pruned.len()
                );
                anstream::println!(
                    "Loose objects: {} → {}; packs: {} ({}).",
                    before.loose_objects,
                    after.loose_objects,
                    after.packs,
                    human(after.pack_bytes)
                );
                ui::hint("Veyra's journal snapshots are reachable through refs/veyra/keep, so they are never pruned");
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------
// worktrees

pub fn worktree(ctx: &Ctx, command: crate::cli::WorktreeCommand) -> Result<()> {
    use crate::cli::WorktreeCommand;
    use veyra_core::worktrees::NewWorktreeHead;
    let repo = ctx.repo()?;
    match command {
        WorktreeCommand::Add { path, commitish, new_branch, detach } => {
            let dest = if path.is_absolute() { path.clone() } else { ctx.cwd.join(&path) };
            let head = if detach {
                NewWorktreeHead::Detached(repo.rev_parse_commit(commitish.as_deref().unwrap_or("HEAD"))?)
            } else if let Some(name) = new_branch {
                NewWorktreeHead::NewBranch {
                    name,
                    start: repo.rev_parse_commit(commitish.as_deref().unwrap_or("HEAD"))?,
                }
            } else if let Some(c) = commitish {
                if repo.ref_target(&format!("refs/heads/{c}"))?.is_some() {
                    NewWorktreeHead::Branch(c)
                } else {
                    NewWorktreeHead::Detached(repo.rev_parse_commit(&c)?)
                }
            } else {
                let name = dest.file_name().context("invalid worktree path")?.to_string_lossy().into_owned();
                if repo.ref_target(&format!("refs/heads/{name}"))?.is_some() {
                    NewWorktreeHead::Branch(name)
                } else {
                    NewWorktreeHead::NewBranch {
                        name,
                        start: repo.head_commit_id()?.context("HEAD has no commits yet")?,
                    }
                }
            };
            let label = match &head {
                NewWorktreeHead::Branch(b) | NewWorktreeHead::NewBranch { name: b, .. } => format!("branch '{b}'"),
                NewWorktreeHead::Detached(id) => format!("detached HEAD {}", veyra_core::short(id)),
            };
            let created = journal(ctx, &repo, "worktree", false, || {
                let p = repo.worktree_add(&dest, head.clone())?;
                Ok((p, format!("add worktree {} ({label})", dest.display())))
            })?;
            anstream::println!("Preparing worktree ({label}) at {}", created.display());
        }
        WorktreeCommand::List => {
            let list = repo.worktree_list()?;
            if ctx.json {
                ui::json(&json!(
                    list.iter()
                        .map(|w| json!({
                            "path": w.path.display().to_string(), "id": w.id, "branch": w.branch,
                            "head": w.head.map(|h| h.to_string()), "locked": w.locked, "prunable": w.prunable,
                        }))
                        .collect::<Vec<_>>()
                ));
                return Ok(());
            }
            for w in list {
                let head = w.head.map(|h| veyra_core::short(&h)).unwrap_or_else(|| "-------".into());
                let what = match (&w.branch, w.prunable) {
                    (_, true) => ui::red("(prunable)"),
                    (Some(b), _) => format!("[{b}]"),
                    (None, _) => "(detached HEAD)".into(),
                };
                let lock = if w.locked { " locked" } else { "" };
                anstream::println!("{}  {head} {what}{lock}", w.path.display());
            }
        }
        WorktreeCommand::Remove { path, force } => {
            let target = if path.is_absolute() { path.clone() } else { ctx.cwd.join(&path) };
            if force {
                ctx.confirm_destructive("worktree remove --force")?;
            }
            journal(ctx, &repo, "worktree", false, || {
                repo.worktree_remove(&target, force)?;
                Ok(((), format!("remove worktree {}", target.display())))
            })?;
            anstream::println!("Removed worktree {}", target.display());
        }
        WorktreeCommand::Prune => {
            let pruned = repo.worktree_prune()?;
            for id in &pruned {
                anstream::println!("Removing worktrees/{id}: gitdir file points to a non-existent location");
            }
            if pruned.is_empty() {
                anstream::println!("Nothing to prune.");
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------
// submodules

pub fn submodule(ctx: &Ctx, command: crate::cli::SubmoduleCommand) -> Result<()> {
    use crate::cli::SubmoduleCommand;
    let repo = ctx.repo()?;
    match command {
        SubmoduleCommand::Status => {
            let list = repo.submodule_list()?;
            if ctx.json {
                ui::json(&json!(list.iter().map(|s| json!({
                    "name": s.name, "path": s.path, "url": s.url,
                    "recorded": s.recorded.map(|c| c.to_string()), "checked_out": s.checked_out.map(|c| c.to_string()),
                    "status": s.status_char().to_string(),
                })).collect::<Vec<_>>()));
                return Ok(());
            }
            for s in list {
                let id = s.checked_out.or(s.recorded).map(|c| c.to_string()).unwrap_or_else(|| "0".repeat(40));
                anstream::println!("{}{id} {}", s.status_char(), s.path);
            }
        }
        SubmoduleCommand::Init { paths } => {
            for name in repo.submodule_init(&paths)? {
                anstream::println!("Submodule '{name}' registered");
            }
        }
        SubmoduleCommand::Update { init, recursive, paths } => {
            let changed = journal(ctx, &repo, "submodule", false, || {
                let c = repo.submodule_update(init, recursive, &paths)?;
                let n = c.len();
                Ok((c, format!("submodule update: {}", plural(n, "submodule"))))
            })?;
            for (path, id) in &changed {
                anstream::println!("Submodule path '{path}': checked out '{id}'");
            }
            if changed.is_empty() {
                anstream::println!("Submodules are up to date.");
            }
        }
        SubmoduleCommand::Add { url, path } => {
            let path = match path {
                Some(p) => p,
                None => url
                    .trim_end_matches('/')
                    .trim_end_matches(".git")
                    .rsplit(['/', '\\', ':'])
                    .next()
                    .unwrap_or("submodule")
                    .to_owned(),
            };
            let head = journal(ctx, &repo, "submodule", false, || {
                let head = repo.submodule_add(&url, &path)?;
                Ok((head, format!("add submodule {path}")))
            })?;
            anstream::println!(
                "Added submodule '{path}' at {} (staged with .gitmodules; commit to record it)",
                veyra_core::short(&head)
            );
        }
        SubmoduleCommand::Foreach { command } => repo.submodule_foreach(&command)?,
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------
// sparse checkout

pub fn sparse(ctx: &Ctx, command: crate::cli::SparseCommand) -> Result<()> {
    use crate::cli::SparseCommand;
    let repo = ctx.repo()?;
    match command {
        SparseCommand::Set { dirs, scope } => {
            let dirs = match scope {
                Some(name) => {
                    let text = std::fs::read_to_string(repo.workdir()?.join(".veyra/scopes.toml"))
                        .context("no .veyra/scopes.toml in this repository")?;
                    veyra_core::sparse::Scopes::parse(&text)?.get(&name)?.paths.clone()
                }
                None => dirs,
            };
            if dirs.is_empty() {
                bail!("name at least one directory (or --scope)");
            }
            let (written, removed) = journal(ctx, &repo, "sparse", true, || {
                let r = repo.sparse_set(&dirs)?;
                Ok((r, format!("sparse set {}", dirs.join(" "))))
            })?;
            anstream::println!("Sparse checkout: {} ({written} files written, {removed} removed)", dirs.join(", "));
        }
        SparseCommand::Add { dirs } => {
            let mut all = repo.sparse()?.map(|s| s.dirs).unwrap_or_default();
            all.extend(dirs);
            let (written, _) = journal(ctx, &repo, "sparse", true, || {
                let r = repo.sparse_set(&all)?;
                Ok((r, format!("sparse add -> {}", all.join(" "))))
            })?;
            anstream::println!("Sparse checkout: {} ({written} files written)", all.join(", "));
        }
        SparseCommand::List => match repo.sparse()? {
            Some(s) => s.dirs.iter().for_each(|d| anstream::println!("{d}")),
            None => anstream::println!("Sparse checkout is not enabled."),
        },
        SparseCommand::Disable => {
            let written = journal(ctx, &repo, "sparse", true, || {
                let n = repo.sparse_disable()?;
                Ok((n, "sparse disable".to_owned()))
            })?;
            anstream::println!("Sparse checkout disabled ({written} files written).");
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------
// attestations

fn show_attestations(ctx: &Ctx, repo: &Repo, commit: ObjectId, check: bool) -> Result<()> {
    use veyra_core::attest::SignatureCheck;
    let list = repo.attestations(commit)?;
    let mut bad = 0;
    let mut rows = Vec::new();
    for env in &list {
        let check_result = if check { repo.check_attestation(env)? } else { SignatureCheck::Unsigned };
        if matches!(check_result, SignatureCheck::Bad(_)) {
            bad += 1;
        }
        let status = match (&env.signature, &check_result) {
            (None, _) => "unsigned".to_owned(),
            (Some(_), SignatureCheck::Good) => "signature good".to_owned(),
            (Some(_), SignatureCheck::Bad(why)) => format!("signature BAD: {why}"),
            (Some(_), SignatureCheck::Unsigned) => "signed (not checked)".to_owned(),
        };
        if ctx.json {
            rows.push(json!({"statement": env.statement, "signed": env.signature.is_some(), "status": status}));
        } else {
            let p = &env.statement["predicate"];
            let detail = match env.predicate_type() {
                veyra_core::attest::TEST_PREDICATE => format!(
                    "test `{}` {} in {} ms",
                    p["command"].as_str().unwrap_or("?"),
                    if p["success"].as_bool() == Some(true) { ui::green("passed") } else { ui::red("failed") },
                    p["duration_ms"]
                ),
                veyra_core::attest::REVIEW_PREDICATE => format!(
                    "review by {}: {}",
                    p["reviewer"].as_str().unwrap_or("?"),
                    p["decision"].as_str().unwrap_or("?")
                ),
                other => other.to_owned(),
            };
            anstream::println!("  {detail}  {}", ui::dim(&format!("[{status}]")));
        }
    }
    if ctx.json {
        ui::json(&json!({"commit": commit.to_string(), "attestations": rows, "bad": bad}));
    } else if list.is_empty() {
        anstream::println!("No attestations for {}.", veyra_core::short(&commit));
    } else {
        anstream::println!("{} for {}.", plural(list.len(), "attestation"), veyra_core::short(&commit));
    }
    if bad > 0 {
        return Err(Reported.into());
    }
    Ok(())
}

pub fn attest(ctx: &Ctx, command: crate::cli::AttestCommand) -> Result<()> {
    use crate::cli::AttestCommand;
    use veyra_core::attest::{REVIEW_PREDICATE, TEST_PREDICATE, statement};
    let repo = ctx.repo()?;
    let now = jiff::Timestamp::now().to_string();
    match command {
        AttestCommand::Run { rev, sign, command } => {
            let commit = repo.rev_parse_commit(rev.as_deref().unwrap_or("HEAD"))?;
            let at_head = repo.head_commit_id()? == Some(commit);
            let clean = repo.worktree_changes()?.is_clean() && repo.flat_index()? == repo.flat_head()?;
            let line = command.join(" ");
            let started = std::time::Instant::now();
            let mut shell = veyra_core::gix::path::env::shell_command();
            shell.arg("-c").arg(&line).current_dir(repo.workdir()?);
            let status = shell.status().with_context(|| format!("could not run `{line}`"))?;
            let predicate = json!({
                "command": line,
                "exit_code": status.code(),
                "success": status.success(),
                "duration_ms": started.elapsed().as_millis() as u64,
                "finished_at": now,
                // The result only describes the commit if it ran on exactly that tree.
                "ran_on_commit": at_head,
                "worktree_clean": clean,
                "runner": { "os": std::env::consts::OS, "arch": std::env::consts::ARCH, "veyra": env!("CARGO_PKG_VERSION") },
            });
            repo.add_attestation(commit, statement(commit, TEST_PREDICATE, predicate), sign)?;
            anstream::println!(
                "Attested: `{line}` {} for {}{}",
                if status.success() { ui::green("passed") } else { ui::red("failed") },
                veyra_core::short(&commit),
                if at_head && clean {
                    String::new()
                } else {
                    ui::yellow(" (note: not run on a clean checkout of that commit)")
                }
            );
            if !status.success() {
                return Err(Reported.into());
            }
        }
        AttestCommand::Review { rev, reviewer, reject, message, sign } => {
            let commit = repo.rev_parse_commit(rev.as_deref().unwrap_or("HEAD"))?;
            let reviewer = reviewer
                .or_else(|| repo.identity().map(|(n, e)| format!("{n} <{e}>")))
                .context("who reviewed? pass --reviewer")?;
            let predicate = json!({
                "reviewer": reviewer,
                "decision": if reject { "rejected" } else { "approved" },
                "note": message,
                "reviewed_at": now,
            });
            repo.add_attestation(commit, statement(commit, REVIEW_PREDICATE, predicate), sign)?;
            anstream::println!(
                "Recorded review of {} ({}).",
                veyra_core::short(&commit),
                if reject { "rejected" } else { "approved" }
            );
        }
        AttestCommand::List { rev } => {
            let commit = repo.rev_parse_commit(rev.as_deref().unwrap_or("HEAD"))?;
            show_attestations(ctx, &repo, commit, false)?;
        }
    }
    Ok(())
}

pub fn rerere(ctx: &Ctx, command: Option<crate::cli::RerereCommand>) -> Result<()> {
    use crate::cli::RerereCommand as C;
    let repo = ctx.repo()?;
    let config = repo.config_path(veyra_core::config::Scope::Local)?;
    match command.unwrap_or(C::Status) {
        C::Status => {
            let enabled = repo.rerere_enabled();
            let list = repo.rerere_list()?;
            let current: Vec<_> = list.iter().filter(|c| c.path.is_some()).collect();
            if ctx.json {
                ui::json(&json!({"enabled": enabled, "recorded": list.len(), "current": current}));
                return Ok(());
            }
            anstream::println!(
                "Recorded resolutions are {} ({} known conflict(s), {} resolved).",
                if enabled { ui::green("on") } else { ui::yellow("off") },
                list.len(),
                list.iter().filter(|c| c.resolved).count()
            );
            if !enabled {
                ui::hint("turn it on with `veyra rerere enable` (shared with `git rerere`)");
            }
            for c in current {
                let state = if c.resolved { "has a recorded resolution" } else { "will be recorded when you conclude" };
                anstream::println!("  {}  {}", c.path.as_deref().unwrap_or_default(), ui::dim(state));
            }
        }
        C::List => {
            let list = repo.rerere_list()?;
            if ctx.json {
                ui::json(&serde_json::to_value(&list)?);
                return Ok(());
            }
            for c in &list {
                let state = if c.resolved { ui::green("resolved") } else { ui::dim("conflict only") };
                let path = c.path.as_deref().map(|p| format!("  ({p})")).unwrap_or_default();
                anstream::println!("{}  {state}{path}", &c.id[..c.id.len().min(12)]);
            }
            if list.is_empty() {
                anstream::println!("No recorded conflicts.");
            }
        }
        C::Forget { path } => {
            if repo.rerere_forget(&path)? {
                anstream::println!("Forgot the recorded resolution for {path}.");
            } else {
                bail!("no recorded resolution for '{path}'");
            }
        }
        C::Clear => {
            let n = repo.rerere_clear()?;
            anstream::println!("Deleted {}.", plural(n, "recorded conflict"));
        }
        C::Enable { auto_stage } => {
            veyra_core::config::set_in_file(&config, "rerere.enabled", "true")?;
            if auto_stage {
                veyra_core::config::set_in_file(&config, "rerere.autoUpdate", "true")?;
            }
            anstream::println!(
                "Recorded resolutions are on{}.",
                if auto_stage { "; replayed resolutions are staged" } else { "" }
            );
        }
        C::Disable => {
            veyra_core::config::set_in_file(&config, "rerere.enabled", "false")?;
            anstream::println!("Recorded resolutions are off (existing recordings are kept).");
        }
    }
    Ok(())
}
