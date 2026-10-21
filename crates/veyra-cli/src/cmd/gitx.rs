//! Git-parity commands: shortlog, describe, grep, bisect, release, notes, format-patch, apply,
//! am, archive, bundle, ls-remote and plumbing (write-tree, commit-tree, update-ref,
//! for-each-ref, merge-base, pack-objects). State-changing commands are journaled, so
//! `veyra undo` reverses them like any other operation.

use std::io::{Read, Write};

use anyhow::{Context, Result, bail};
use serde_json::json;
use veyra_core::history::{BisectStep, DescribeOptions, GrepSource};
use veyra_core::patches::ApplyTo;
use veyra_core::{ByteSlice, ObjectId, Repo};

use super::{journal, pathspec, plural};
use crate::cli::{BisectCommand, BundleCommand, GrepArgs, NotesCommand, ReleaseCommand};
use crate::{Ctx, Reported, ui};

/// `A..B` → (B, [A]); `A` → (HEAD, [A]); none → (HEAD, []).
fn range(repo: &Repo, spec: Option<&str>) -> Result<(ObjectId, Vec<ObjectId>)> {
    match spec {
        Some(s) if s.contains("..") => {
            let (a, b) = s.split_once("..").expect("checked");
            let tip = repo.rev_parse_commit(if b.is_empty() { "HEAD" } else { b })?;
            Ok((tip, vec![repo.rev_parse_commit(a)?]))
        }
        Some(s) => Ok((repo.rev_parse_commit(s)?, Vec::new())),
        None => Ok((repo.head_commit_id()?.context("HEAD is unborn")?, Vec::new())),
    }
}

fn read_input(path: &str) -> Result<String> {
    if path == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        Ok(s)
    } else {
        std::fs::read_to_string(path).with_context(|| format!("could not read {path}"))
    }
}

// ------------------------------------------------------------------------------------------
// history

pub fn shortlog(ctx: &Ctx, spec: Option<String>, summary: bool, numbered: bool, email: bool) -> Result<()> {
    let repo = ctx.repo()?;
    let (tip, hidden) = range(&repo, spec.as_deref())?;
    let mut groups = repo.shortlog(tip, &hidden)?;
    if !numbered {
        groups.sort_by_key(|a| a.0.to_lowercase());
    }
    if ctx.json {
        ui::json(&json!(
            groups.iter().map(|(a, s)| json!({"author": a, "count": s.len(), "subjects": s})).collect::<Vec<_>>()
        ));
        return Ok(());
    }
    for (author, subjects) in &groups {
        let who = if email { author.clone() } else { author.split(" <").next().unwrap_or(author).to_owned() };
        if summary {
            anstream::println!("{:>6}\t{who}", subjects.len());
        } else {
            anstream::println!("{who} ({}):", subjects.len());
            for s in subjects {
                anstream::println!("      {s}");
            }
            anstream::println!();
        }
    }
    Ok(())
}

pub fn describe(ctx: &Ctx, revision: Option<String>, tags: bool, long: bool, always: bool, dirty: bool) -> Result<()> {
    let repo = ctx.repo()?;
    let commit = repo.rev_parse_commit(revision.as_deref().unwrap_or("HEAD"))?;
    let name = repo.describe(commit, &DescribeOptions { tags, long, always })?.with_context(|| {
        format!(
            "no tags can describe {}\n  hint: --tags uses lightweight tags too; --always falls back to the id",
            veyra_core::short(&commit)
        )
    })?;
    let suffix = if dirty && revision.is_none() {
        let s = repo.status()?;
        if s.staged.is_empty() && s.worktree.modified.is_empty() && s.worktree.deleted.is_empty() {
            ""
        } else {
            "-dirty"
        }
    } else {
        ""
    };
    anstream::println!("{name}{suffix}");
    Ok(())
}

pub fn grep(ctx: &Ctx, args: GrepArgs) -> Result<()> {
    let repo = ctx.repo()?;
    let mut pattern = if args.fixed_strings { regex::escape(&args.pattern) } else { args.pattern.clone() };
    if args.word_regexp {
        pattern = format!(r"\b(?:{pattern})\b");
    }
    let re =
        regex::RegexBuilder::new(&pattern).case_insensitive(args.ignore_case).build().context("invalid pattern")?;
    let source = match (&args.revision, args.cached) {
        (Some(rev), _) => GrepSource::Tree(repo.rev_parse_tree(rev)?),
        (None, true) => GrepSource::Index,
        (None, false) => GrepSource::Worktree,
    };
    let spec = pathspec(ctx, &repo, &args.paths)?;
    let matches = repo.grep(&re, source, &spec)?;
    if ctx.json {
        ui::json(&serde_json::to_value(&matches)?);
    } else {
        let prefix = args.revision.as_deref().map(|r| format!("{r}:")).unwrap_or_default();
        let mut last = "";
        for m in &matches {
            if args.files_with_matches {
                if m.path != last {
                    anstream::println!("{prefix}{}", m.path);
                }
            } else if !args.count {
                let line =
                    if args.line_number { format!("{}:", ui::green(&m.line.to_string())) } else { String::new() };
                anstream::println!("{}:{line}{}", ui::cyan(&format!("{prefix}{}", m.path)), m.text);
            }
            last = &m.path;
        }
        if args.count {
            let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
            for m in &matches {
                *counts.entry(&m.path).or_default() += 1;
            }
            for (p, n) in counts {
                anstream::println!("{prefix}{p}:{n}");
            }
        }
    }
    if matches.is_empty() {
        return Err(Reported.into()); // like git grep: exit 1 when nothing matches
    }
    Ok(())
}

fn report_bisect(repo: &Repo, step: &BisectStep) -> Result<bool> {
    match step {
        BisectStep::NeedMarks { bad, good } => {
            let what = match (bad, good) {
                (true, true) => "a bad and a good commit",
                (true, false) => "a bad commit",
                _ => "a good commit",
            };
            anstream::println!("Waiting for {what}: `veyra bisect bad|good [rev]`.");
            Ok(false)
        }
        BisectStep::Test { commit, remaining, steps } => {
            let info = repo.commit_info(*commit)?;
            anstream::println!(
                "Bisecting: {} left to test (about {}). Now at {} {}",
                plural(*remaining, "commit"),
                plural(*steps as usize, "step"),
                ui::yellow(&veyra_core::short(commit)),
                info.summary()
            );
            Ok(false)
        }
        BisectStep::Found(commit) => {
            let info = repo.commit_info(*commit)?;
            anstream::println!(
                "{} is the first bad commit\n\n    {}\n    Author: {} <{}>",
                ui::red(&commit.to_string()),
                info.summary(),
                info.author_name,
                info.author_email
            );
            anstream::println!("\n`veyra bisect reset` returns to where you started.");
            Ok(true)
        }
        BisectStep::OnlySkipped(commits) => {
            anstream::println!("Only skipped commits are left. The first bad commit is one of:");
            for c in commits {
                anstream::println!("  {} {}", veyra_core::short(c), repo.commit_info(*c)?.summary());
            }
            Ok(true)
        }
    }
}

fn bisect_mark(ctx: &Ctx, repo: &Repo, mark: &str, rev: Option<&str>) -> Result<BisectStep> {
    let commit = repo.rev_parse_commit(rev.unwrap_or("HEAD"))?;
    journal(ctx, repo, "bisect", true, || {
        let step = repo.bisect_mark(mark, commit)?;
        Ok((step, format!("bisect {mark} {}", veyra_core::short(&commit))))
    })
}

pub fn bisect(ctx: &Ctx, command: BisectCommand) -> Result<()> {
    let repo = ctx.repo()?;
    match command {
        BisectCommand::Start { bad, good } => {
            repo.bisect_start()?;
            anstream::println!("Bisection started.");
            let mut step = None;
            if let Some(b) = bad {
                step = Some(bisect_mark(ctx, &repo, "bad", Some(&b))?);
            }
            for g in &good {
                step = Some(bisect_mark(ctx, &repo, "good", Some(g))?);
            }
            if let Some(s) = step {
                report_bisect(&repo, &s)?;
            }
        }
        BisectCommand::Bad { revision } => {
            let s = bisect_mark(ctx, &repo, "bad", revision.as_deref())?;
            report_bisect(&repo, &s)?;
        }
        BisectCommand::Good { revisions } => {
            let revs = if revisions.is_empty() { vec!["HEAD".to_owned()] } else { revisions };
            let mut last = None;
            for r in &revs {
                last = Some(bisect_mark(ctx, &repo, "good", Some(r))?);
            }
            report_bisect(&repo, &last.expect("at least one"))?;
        }
        BisectCommand::Skip { revision } => {
            let s = bisect_mark(ctx, &repo, "skip", revision.as_deref())?;
            report_bisect(&repo, &s)?;
        }
        BisectCommand::Run { command } => {
            let cmdline = command.join(" ");
            loop {
                let state = repo.bisect_state()?.context("no bisection in progress")?;
                if state.bad.is_none() || state.good.is_empty() {
                    bail!("mark a bad and a good commit before `bisect run`");
                }
                let status = gix_shell(&cmdline, repo.workdir()?)?;
                let mark = match status {
                    0 => "good",
                    125 => "skip",
                    1..=127 => "bad",
                    code => bail!("`{cmdline}` exited with {code}; stopping (exit codes above 127 abort a bisect run)"),
                };
                anstream::println!("{} {}", ui::dim(&format!("`{cmdline}` → {mark}")), "");
                let step = bisect_mark(ctx, &repo, mark, None)?;
                if report_bisect(&repo, &step)? {
                    break;
                }
            }
        }
        BisectCommand::Log => {
            let state = repo.bisect_state()?.context("no bisection in progress")?;
            for l in &state.log {
                anstream::println!("{l}");
            }
        }
        BisectCommand::Reset => {
            let back = journal(ctx, &repo, "bisect", true, || {
                let b = repo.bisect_reset()?;
                Ok((b.clone(), format!("bisect reset to {b}")))
            })?;
            anstream::println!("Bisection ended; back on {back}.");
        }
    }
    Ok(())
}

/// Run a shell command in `dir`; returns its exit code.
fn gix_shell(command: &str, dir: &std::path::Path) -> Result<i32> {
    let status = veyra_core::gix::path::env::shell_command()
        .arg("-c")
        .arg(command)
        .current_dir(dir)
        .status()
        .with_context(|| format!("could not run `{command}`"))?;
    Ok(status.code().unwrap_or(255))
}

// ------------------------------------------------------------------------------------------
// releases

pub fn release(ctx: &Ctx, command: ReleaseCommand) -> Result<()> {
    let repo = ctx.repo()?;
    match command {
        ReleaseCommand::Notes { range: spec, title } => {
            let (tip, base) = match spec.as_deref() {
                Some(_) => {
                    let (t, h) = range(&repo, spec.as_deref())?;
                    (t, h.first().copied())
                }
                None => {
                    let head = repo.head_commit_id()?.context("HEAD is unborn")?;
                    (head, repo.previous_tag(head)?.map(|(_, id)| id))
                }
            };
            let entries = repo.release_entries(tip, base)?;
            if ctx.json {
                ui::json(&serde_json::to_value(&entries)?);
            } else {
                let title = title.unwrap_or_else(|| "Unreleased".into());
                anstream::println!("{}", Repo::render_release_notes(&title, &entries).trim_end());
            }
        }
        ReleaseCommand::Prepare { version, dry_run, sign, force } => {
            let checks = repo.release_checks(&version)?;
            let blocked = checks.iter().any(|c| c.blocking && !c.ok);
            if ctx.json && dry_run {
                ui::json(&serde_json::to_value(&checks)?);
                return Ok(());
            }
            for c in &checks {
                let mark = match (c.ok, c.blocking) {
                    (true, _) => ui::green("PASS"),
                    (false, true) => ui::red("FAIL"),
                    (false, false) => ui::yellow("WARN"),
                };
                anstream::println!("{mark}  {:<32} {}", c.name, ui::dim(&c.detail));
            }
            if blocked && !force {
                anstream::println!("\nNot tagged: fix the failing checks (or pass --force).");
                return Err(Reported.into());
            }
            if dry_run {
                anstream::println!("\nReady to tag {version} (dry run: nothing was created).");
                return Ok(());
            }
            let head = repo.head_commit_id()?.context("HEAD is unborn")?;
            let base = repo.previous_tag(head)?.map(|(_, id)| id);
            let notes = Repo::render_release_notes(&version, &repo.release_entries(head, base)?);
            let message = format!("Release {version}\n\n{notes}");
            let sign = sign || repo.config_bool("tag.gpgSign").unwrap_or(false);
            journal(ctx, &repo, "tag", false, || {
                repo.create_tag(&version, head, &message, sign, false)?;
                Ok(((), format!("release {version}")))
            })?;
            anstream::println!(
                "\nTagged {version} at {}{}.",
                veyra_core::short(&head),
                if sign { " (signed)" } else { "" }
            );
            anstream::println!("Publish it with `veyra push origin {version}`.");
        }
        ReleaseCommand::Verify { tag } => {
            let v = repo.release_verify(&tag)?;
            if ctx.json {
                ui::json(&serde_json::to_value(&v)?);
            } else {
                let sig = match v.tag_signed {
                    Some(true) => ui::green("good signature"),
                    Some(false) => ui::red("BAD signature"),
                    None => ui::yellow("not signed"),
                };
                anstream::println!("Tag {tag} → {}: {sig}", &v.commit[..12]);
                anstream::println!(
                    "Commits in this release: {} — {} good, {} unsigned, {} bad{}",
                    v.commits,
                    v.good_signatures,
                    v.unsigned,
                    v.bad_signatures.len(),
                    if v.bad_signatures.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", v.bad_signatures.join(", "))
                    }
                );
                if v.attestations.is_empty() {
                    anstream::println!("Attestations: none");
                } else {
                    anstream::println!("Attestations: {}", v.attestations.join(", "));
                }
            }
            if !v.ok() {
                return Err(Reported.into());
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------
// notes

pub fn notes(ctx: &Ctx, command: Option<NotesCommand>, notes_ref: Option<String>) -> Result<()> {
    let repo = ctx.repo()?;
    let notes_ref = notes_ref
        .map(|r| if r.starts_with("refs/") { r } else { format!("refs/notes/{r}") })
        .unwrap_or_else(|| veyra_core::notes::DEFAULT_NOTES_REF.into());
    let target = |rev: &Option<String>| repo.rev_parse(rev.as_deref().unwrap_or("HEAD"));
    match command.unwrap_or(NotesCommand::List) {
        NotesCommand::List => {
            for (object, blob) in repo.notes(&notes_ref)? {
                anstream::println!("{blob} {object}");
            }
        }
        NotesCommand::Show { revision } => {
            let id = target(&revision)?;
            let note =
                repo.note(&notes_ref, id)?.with_context(|| format!("no note found for {}", veyra_core::short(&id)))?;
            anstream::print!("{note}");
        }
        NotesCommand::Add { message, revision, force } => {
            let id = target(&revision)?;
            if repo.note(&notes_ref, id)?.is_some() && !force {
                bail!(
                    "{} already has a note\n  hint: -f replaces it; `notes append` adds to it",
                    veyra_core::short(&id)
                );
            }
            let text = message.join("\n\n");
            journal(ctx, &repo, "notes", false, || {
                repo.set_note(&notes_ref, id, Some(&text), "Notes added by 'veyra notes add'")?;
                Ok(((), format!("note on {}", veyra_core::short(&id))))
            })?;
        }
        NotesCommand::Append { message, revision } => {
            let id = target(&revision)?;
            let mut text = repo.note(&notes_ref, id)?.unwrap_or_default();
            if !text.is_empty() {
                text = format!("{}\n\n", text.trim_end());
            }
            text.push_str(&message.join("\n\n"));
            journal(ctx, &repo, "notes", false, || {
                repo.set_note(&notes_ref, id, Some(&text), "Notes added by 'veyra notes append'")?;
                Ok(((), format!("note on {}", veyra_core::short(&id))))
            })?;
        }
        NotesCommand::Remove { revision } => {
            let id = target(&revision)?;
            journal(ctx, &repo, "notes", false, || {
                repo.set_note(&notes_ref, id, None, "Notes removed by 'veyra notes remove'")?;
                Ok(((), format!("remove note on {}", veyra_core::short(&id))))
            })?;
            anstream::println!("Removed the note on {}.", veyra_core::short(&id));
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------
// patches

pub fn format_patch(ctx: &Ctx, spec: &str, dir: Option<std::path::PathBuf>, stdout: bool) -> Result<()> {
    let repo = ctx.repo()?;
    let (tip, hidden) = if spec.contains("..") {
        range(&repo, Some(spec))?
    } else {
        // `format-patch <base>`: everything since base, like git.
        (repo.head_commit_id()?.context("HEAD is unborn")?, vec![repo.rev_parse_commit(spec)?])
    };
    let mut commits: Vec<ObjectId> =
        repo.git().rev_walk([tip]).with_hidden(hidden).all()?.map(|i| i.map(|i| i.id)).collect::<Result<_, _>>()?;
    commits.retain(|c| repo.commit_info(*c).is_ok_and(|i| i.parents.len() <= 1));
    commits.reverse();
    if commits.is_empty() {
        bail!("no commits in {spec}");
    }
    let patches = repo.format_patches(&commits)?;
    if stdout {
        let mut out = std::io::stdout().lock();
        for (_, text) in &patches {
            out.write_all(text.as_bytes())?;
        }
        return Ok(());
    }
    let dir = dir.map(|d| ctx.cwd.join(d)).unwrap_or_else(|| ctx.cwd.clone());
    std::fs::create_dir_all(&dir)?;
    for (name, text) in &patches {
        let path = dir.join(name);
        std::fs::write(&path, text)?;
        anstream::println!("{}", path.strip_prefix(&ctx.cwd).unwrap_or(&path).display());
    }
    Ok(())
}

pub fn apply(ctx: &Ctx, inputs: &[String], cached: bool, index: bool, check: bool, reverse: bool) -> Result<()> {
    let repo = ctx.repo()?;
    let mut patches = Vec::new();
    for input in inputs {
        patches.extend(veyra_core::patches::parse_patch(&read_input(input)?)?);
    }
    let to = if cached {
        ApplyTo::Index
    } else if index {
        ApplyTo::Both
    } else {
        ApplyTo::Worktree
    };
    if check {
        let paths = repo.apply_patches(&patches, to, reverse, true)?;
        anstream::println!("The patch applies cleanly ({}).", plural(paths.len(), "file"));
        return Ok(());
    }
    let paths = journal(ctx, &repo, "apply", true, || {
        let p = repo.apply_patches(&patches, to, reverse, false)?;
        Ok((p.clone(), format!("apply patch to {}", plural(p.len(), "file"))))
    })?;
    for p in &paths {
        anstream::println!("{} {p}", if reverse { "Reverted" } else { "Applied" });
    }
    Ok(())
}

pub fn am(ctx: &Ctx, inputs: &[String]) -> Result<()> {
    let repo = ctx.repo()?;
    let mut mails = Vec::new();
    for input in inputs {
        mails.extend(veyra_core::patches::parse_mbox(&read_input(input)?)?);
    }
    for (i, mail) in mails.iter().enumerate() {
        let subject = mail.message.lines().next().unwrap_or_default().to_owned();
        let result = journal(ctx, &repo, "am", true, || {
            let id = repo.apply_mail(mail)?;
            Ok((id, format!("am {}", subject)))
        });
        match result {
            Ok(id) => anstream::println!("Applied: {} {subject}", ui::yellow(&veyra_core::short(&id))),
            Err(err) => {
                anstream::println!("{} patch {} of {}: {subject}", ui::red("Failed"), i + 1, mails.len());
                return Err(err.context(
                    "the patch does not apply; earlier patches stay committed (`veyra undo` reverts them one by one)",
                ));
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------
// archive and bundles

pub fn archive(
    ctx: &Ctx,
    revision: Option<String>,
    paths: &[String],
    format: Option<String>,
    prefix: Option<String>,
    output: Option<std::path::PathBuf>,
) -> Result<()> {
    use veyra_core::archive::ArchiveFormat;
    let repo = ctx.repo()?;
    let commit = repo.rev_parse_commit(revision.as_deref().unwrap_or("HEAD"))?;
    let format = match (&format, &output) {
        (Some(f), _) => ArchiveFormat::parse(f).with_context(|| format!("unknown format '{f}' (tar, tgz, zip)"))?,
        (None, Some(o)) => ArchiveFormat::from_file_name(&o.to_string_lossy()).unwrap_or(ArchiveFormat::Tar),
        (None, None) => ArchiveFormat::Tar,
    };
    let mut prefix = prefix.unwrap_or_default();
    if !prefix.is_empty() && !prefix.ends_with('/') {
        prefix.push('/');
    }
    let bytes = repo.archive(commit, format, &prefix, paths)?;
    match output {
        Some(path) => {
            let path = ctx.cwd.join(path);
            std::fs::write(&path, &bytes).with_context(|| format!("could not write {}", path.display()))?;
        }
        None => {
            if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                bail!("refusing to write an archive to the terminal\n  hint: -o <file>, or redirect the output");
            }
            std::io::stdout().lock().write_all(&bytes)?;
        }
    }
    Ok(())
}

pub fn bundle(ctx: &Ctx, command: BundleCommand) -> Result<()> {
    let repo = ctx.repo()?;
    match command {
        BundleCommand::Create { file, revs } => {
            let mut refs: Vec<(String, ObjectId)> = Vec::new();
            let mut exclude = Vec::new();
            let resolve_ref = |name: &str| -> Result<(String, ObjectId)> {
                let candidates = if name.starts_with("refs/") {
                    vec![name.to_owned()]
                } else {
                    vec![format!("refs/heads/{name}"), format!("refs/tags/{name}")]
                };
                for full in candidates {
                    if let Some(id) = repo.ref_target(&full)? {
                        return Ok((full, id));
                    }
                }
                if name == "HEAD" {
                    return Ok(("HEAD".into(), repo.head_commit_id()?.context("HEAD is unborn")?));
                }
                bail!("'{name}' is not a branch or tag")
            };
            if revs.is_empty() || revs.iter().any(|r| r == "--all") {
                refs.extend(
                    repo.all_refs()?
                        .into_iter()
                        .filter(|(n, _)| n.starts_with("refs/heads/") || n.starts_with("refs/tags/")),
                );
            }
            for r in revs.iter().filter(|r| *r != "--all") {
                if let Some((a, b)) = r.split_once("..") {
                    exclude.push(repo.rev_parse_commit(a)?);
                    refs.push(resolve_ref(if b.is_empty() { "HEAD" } else { b })?);
                } else if let Some(x) = r.strip_prefix('^') {
                    exclude.push(repo.rev_parse_commit(x)?);
                } else {
                    refs.push(resolve_ref(r)?);
                }
            }
            refs.sort();
            refs.dedup();
            let path = ctx.cwd.join(&file);
            let header = repo.bundle_create(&path, &refs, &exclude)?;
            anstream::println!(
                "Wrote {} with {}{}.",
                file.display(),
                plural(header.refs.len(), "ref"),
                if header.prerequisites.is_empty() {
                    String::new()
                } else {
                    format!(
                        " (needs {} on the receiving side)",
                        plural(header.prerequisites.len(), "prerequisite commit")
                    )
                }
            );
        }
        BundleCommand::Verify { file } => {
            let h = repo.bundle_verify(&ctx.cwd.join(&file))?;
            anstream::println!(
                "{} is okay: {}, {}.",
                file.display(),
                plural(h.refs.len(), "ref"),
                plural(h.prerequisites.len(), "prerequisite")
            );
        }
        BundleCommand::ListHeads { file } => {
            let data = std::fs::read(ctx.cwd.join(&file))?;
            let (h, _) = veyra_core::bundle::read_header(&data)?;
            for (id, name) in h.refs {
                anstream::println!("{id} {name}");
            }
        }
        BundleCommand::Unbundle { file } => {
            for (id, name) in repo.bundle_unbundle(&ctx.cwd.join(&file))? {
                anstream::println!("{id} {name}");
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------
// remote refs and plumbing

pub fn ls_remote(ctx: &Ctx, remote: Option<String>, heads: bool, tags: bool) -> Result<()> {
    let repo = ctx.repo()?;
    let remote = remote.unwrap_or_else(|| "origin".into());
    let refs = repo.remote_refs(&remote)?;
    let wanted =
        |n: &str| (!heads && !tags) || (heads && n.starts_with("refs/heads/")) || (tags && n.starts_with("refs/tags/"));
    if ctx.json {
        let v: Vec<_> = refs
            .iter()
            .filter(|(n, ..)| wanted(n))
            .map(|(n, id, peeled)| json!({"name": n, "id": id.map(|i| i.to_string()), "peeled": peeled.map(|p| p.to_string())}))
            .collect();
        ui::json(&json!(v));
        return Ok(());
    }
    for (name, id, peeled) in refs.iter().filter(|(n, ..)| wanted(n)) {
        let Some(id) = id else { continue };
        anstream::println!("{id}\t{name}");
        if let Some(p) = peeled {
            anstream::println!("{p}\t{name}^{{}}");
        }
    }
    Ok(())
}

pub fn write_tree(ctx: &Ctx) -> Result<()> {
    let repo = ctx.repo()?;
    if repo.load_index()?.entries().iter().any(|e| e.stage_raw() != 0) {
        bail!("the index has unresolved conflicts");
    }
    anstream::println!("{}", repo.write_flat(&repo.flat_index()?)?);
    Ok(())
}

pub fn commit_tree(ctx: &Ctx, tree: &str, parents: &[String], message: &[String]) -> Result<()> {
    let repo = ctx.repo()?;
    repo.require_identity()?;
    let tree = repo.rev_parse_tree(tree)?;
    let parents: Vec<ObjectId> = parents.iter().map(|p| repo.rev_parse_commit(p)).collect::<Result<_>>()?;
    let committer = repo.git().committer().context("no committer")??.to_owned()?;
    let author = repo.git().author().context("no author")??.to_owned()?;
    let commit = veyra_core::gix::objs::Commit {
        message: format!("{}\n", message.join("\n\n")).as_str().into(),
        tree,
        author,
        committer,
        encoding: None,
        parents: parents.into_iter().collect(),
        extra_headers: Default::default(),
    };
    let id = repo.write_commit_object(commit, repo.should_sign(veyra_core::sign::SignMode::Config))?;
    anstream::println!("{id}");
    Ok(())
}

pub fn update_ref(
    ctx: &Ctx,
    delete: bool,
    name: &str,
    new: Option<String>,
    old: Option<String>,
    message: Option<String>,
) -> Result<()> {
    let repo = ctx.repo()?;
    if !name.starts_with("refs/") && name != "HEAD" {
        bail!("'{name}' is not a full ref name (e.g. refs/heads/main)");
    }
    let (old, new) =
        if delete { (new, None) } else { (old, Some(new.context("usage: update-ref <ref> <new> [<old>]")?)) };
    let expected = match old.as_deref() {
        None => None,
        Some(o) if o.chars().all(|c| c == '0') => Some(None),
        Some(o) => Some(Some(repo.rev_parse(o)?)),
    };
    let reflog = message.unwrap_or_else(|| "update-ref".into());
    journal(ctx, &repo, "update-ref", false, || {
        match &new {
            Some(n) => {
                let id = repo.rev_parse(n)?;
                repo.set_ref(name, id, expected, &reflog)?;
            }
            None => repo.delete_ref(name, expected.flatten())?,
        }
        Ok(((), format!("{} {name}", if new.is_some() { "update" } else { "delete" })))
    })
}

pub fn for_each_ref(ctx: &Ctx, patterns: &[String], format: Option<String>) -> Result<()> {
    let repo = ctx.repo()?;
    let format = format.unwrap_or_else(|| "%(objectname) %(objecttype)\t%(refname)".into());
    for (name, id) in repo.all_refs()? {
        if name.starts_with("refs/veyra/") {
            continue;
        }
        if !patterns.is_empty() && !patterns.iter().any(|p| name.starts_with(p.trim_end_matches('*'))) {
            continue;
        }
        let object = repo.git().find_object(id)?;
        let kind = object.kind.to_string();
        let subject = match object.kind {
            veyra_core::gix::object::Kind::Commit => repo.commit_info(id)?.summary().to_owned(),
            veyra_core::gix::object::Kind::Tag => object
                .into_tag()
                .decode()
                .map(|t| t.message.lines().next().unwrap_or_default().to_str_lossy().into_owned())
                .unwrap_or_default(),
            _ => String::new(),
        };
        let line = format
            .replace("%(refname:short)", veyra_core::refs::shorten(&name))
            .replace("%(refname)", &name)
            .replace("%(objectname:short)", &veyra_core::short(&id))
            .replace("%(objectname)", &id.to_string())
            .replace("%(objecttype)", &kind)
            .replace("%(subject)", &subject);
        anstream::println!("{line}");
    }
    Ok(())
}

pub fn merge_base(ctx: &Ctx, a: &str, b: &str, is_ancestor: bool) -> Result<()> {
    let repo = ctx.repo()?;
    let (a, b) = (repo.rev_parse_commit(a)?, repo.rev_parse_commit(b)?);
    if is_ancestor {
        return if repo.is_ancestor(a, b)? || a == b { Ok(()) } else { Err(Reported.into()) };
    }
    match repo.git().merge_base(a, b).ok().flatten() {
        Some(m) => {
            anstream::println!("{}", m.detach());
            Ok(())
        }
        None => Err(Reported.into()),
    }
}

pub fn pack_objects(ctx: &Ctx) -> Result<()> {
    let repo = ctx.repo()?;
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let ids: Vec<ObjectId> = input
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .map(|h| ObjectId::from_hex(h.as_bytes()).with_context(|| format!("not an object id: {h}")))
        .collect::<Result<_>>()?;
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    veyra_core::push::write_pack(&repo, &ids, &mut out)?;
    out.flush()?;
    Ok(())
}
