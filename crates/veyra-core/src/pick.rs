//! Replaying commits: `cherry-pick`, `revert`, and `rebase` (a sequence of picks).
//!
//! All three are three-way tree merges (`gix::Repository::merge_trees`):
//!
//! | operation       | base            | ours      | theirs          |
//! |-----------------|-----------------|-----------|-----------------|
//! | cherry-pick `C` | `C^` tree       | HEAD tree | `C` tree        |
//! | revert `C`      | `C` tree        | HEAD tree | `C^` tree       |
//!
//! Conflicts are written exactly like Git (markers in files, stages in the index) together with
//! `CHERRY_PICK_HEAD` / `REVERT_HEAD` and `MERGE_MSG`, so other tools understand the state.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use gix::ObjectId;
use gix::bstr::{BString, ByteSlice};

use crate::commit::CommitRequest;
use crate::{HeadState, Repo};

/// The result of replaying one commit.
#[derive(Debug, Clone)]
pub enum PickOutcome {
    /// A new commit was created.
    Committed(ObjectId),
    /// The change is already present; nothing to commit.
    Empty,
    /// Stopped with conflicts in these paths.
    Conflicts(Vec<BString>),
    /// `--no-commit`: the change is applied to index and worktree only.
    Applied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickKind {
    CherryPick,
    Revert,
}

impl PickKind {
    fn state_file(self) -> &'static str {
        match self {
            PickKind::CherryPick => "CHERRY_PICK_HEAD",
            PickKind::Revert => "REVERT_HEAD",
        }
    }
}

/// Options for [`Repo::pick`].
#[derive(Debug, Clone, Default)]
pub struct PickOptions {
    /// Apply the change without committing.
    pub no_commit: bool,
    /// Append `(cherry picked from commit …)` to the message.
    pub record_origin: bool,
    /// For merge commits: which parent (1-based) is the mainline.
    pub mainline: Option<usize>,
}

/// What to do with one commit during a rebase (the verbs of `git rebase -i`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebaseAction {
    Pick,
    /// Pick, then edit the message.
    Reword,
    /// Pick, then stop so the commit can be amended.
    Edit,
    /// Fold into the previous commit, combining both messages.
    Squash,
    /// Fold into the previous commit, keeping only the previous message.
    Fixup,
    /// Leave the commit out.
    Drop,
    /// Run a shell command; a failure stops the rebase.
    Exec,
    /// Stop here (resume with `rebase --continue`).
    Break,
}

impl RebaseAction {
    pub fn name(self) -> &'static str {
        match self {
            RebaseAction::Pick => "pick",
            RebaseAction::Reword => "reword",
            RebaseAction::Edit => "edit",
            RebaseAction::Squash => "squash",
            RebaseAction::Fixup => "fixup",
            RebaseAction::Drop => "drop",
            RebaseAction::Exec => "exec",
            RebaseAction::Break => "break",
        }
    }

    /// Parse a todo-list verb, including git's one-letter abbreviations.
    pub fn parse(word: &str) -> Option<RebaseAction> {
        Some(match word {
            "pick" | "p" => RebaseAction::Pick,
            "reword" | "r" => RebaseAction::Reword,
            "edit" | "e" => RebaseAction::Edit,
            "squash" | "s" => RebaseAction::Squash,
            "fixup" | "f" => RebaseAction::Fixup,
            "drop" | "d" => RebaseAction::Drop,
            "exec" | "x" => RebaseAction::Exec,
            "break" | "b" => RebaseAction::Break,
            _ => return None,
        })
    }
}

/// One line of a rebase plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseStep {
    pub action: RebaseAction,
    /// The commit to replay (the null id for `exec` and `break`).
    pub commit: ObjectId,
    /// The shell command of an `exec` step.
    pub command: Option<String>,
}

impl RebaseStep {
    pub fn new(action: RebaseAction, commit: ObjectId) -> Self {
        RebaseStep { action, commit, command: None }
    }
}

/// The state of a rebase in progress, stored in Git's own layout (`.git/rebase-merge/`, the
/// "merge" backend that `git rebase` uses), so `git rebase --continue / --skip / --abort` work on
/// a rebase Veyra started and `veyra rebase --continue` works on one Git started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseState {
    /// The branch being rebased (`refs/heads/x`), or `None` for a detached HEAD.
    pub branch: Option<String>,
    pub orig_head: ObjectId,
    pub onto: ObjectId,
    /// Steps still to apply, in order (`git-rebase-todo`).
    pub todo: Vec<RebaseStep>,
    /// The step whose replay stopped with conflicts (`REBASE_HEAD`).
    pub current: Option<RebaseStep>,
    /// Lines of the steps already processed (`done`).
    pub done: Vec<String>,
    /// Started as `rebase -i` (Git then says "interactive rebase in progress").
    pub interactive: bool,
    /// Stopped at an `edit` step: HEAD at the stop (`amend`).
    pub amend: Option<ObjectId>,
}

impl RebaseState {
    /// The format of `.git/veyra/rebase-state`, written by Veyra before 1.3; still read so a
    /// rebase stopped by an older version can be continued.
    fn from_legacy_text(text: &str) -> Result<RebaseState> {
        let parse =
            |hex: &str| ObjectId::from_hex(hex.trim().as_bytes()).map_err(|e| anyhow!("corrupt rebase state: {e}"));
        let null = ObjectId::null(gix::hash::Kind::Sha1);
        let step = |value: &str| -> Result<RebaseStep> {
            if value == "break" {
                return Ok(RebaseStep::new(RebaseAction::Break, null));
            }
            if let Some(cmd) = value.strip_prefix("exec ") {
                return Ok(RebaseStep { action: RebaseAction::Exec, commit: null, command: Some(cmd.to_owned()) });
            }
            match value.split_once(' ') {
                Some((verb, hex)) => Ok(RebaseStep {
                    command: None,
                    action: RebaseAction::parse(verb)
                        .with_context(|| format!("corrupt rebase state: unknown action {verb}"))?,
                    commit: parse(hex)?,
                }),
                None => Ok(RebaseStep::new(RebaseAction::Pick, parse(value)?)),
            }
        };
        let (mut branch, mut orig, mut onto, mut current, mut todo) = (None, None, None, None, Vec::new());
        for line in text.lines() {
            let (key, value) = line.split_once(' ').unwrap_or((line, ""));
            match key {
                "branch" if value != "-" => branch = Some(value.to_owned()),
                "branch" => {}
                "orig-head" => orig = Some(parse(value)?),
                "onto" => onto = Some(parse(value)?),
                "current" => current = Some(step(value)?),
                "break" => todo.push(RebaseStep::new(RebaseAction::Break, null)),
                "exec" => {
                    todo.push(RebaseStep { action: RebaseAction::Exec, commit: null, command: Some(value.to_owned()) })
                }
                verb => {
                    if let Some(action) = RebaseAction::parse(verb) {
                        todo.push(RebaseStep::new(action, parse(value)?));
                    }
                }
            }
        }
        Ok(RebaseState {
            branch,
            orig_head: orig.context("corrupt rebase state: no orig-head")?,
            onto: onto.context("corrupt rebase state: no onto")?,
            todo,
            current,
            done: Vec::new(),
            interactive: false,
            amend: None,
        })
    }
}

/// Quote a value for Git's `author-script` (a shell fragment): `'it'\''s'`.
fn sq_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Read one `KEY='value'` line of an `author-script`.
fn sq_unquote(value: &str) -> String {
    let mut out = String::new();
    let mut quoted = false;
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('\'', _) => quoted = !quoted,
            ('\\', false) => {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            }
            (c, _) => out.push(c),
        }
    }
    out
}

/// Git's `author-script` for a signature.
fn author_script(author: &gix::actor::SignatureRef<'_>) -> String {
    let time = author.time().unwrap_or_default();
    let sign = if time.offset < 0 { '-' } else { '+' };
    let off = time.offset.unsigned_abs();
    format!(
        "GIT_AUTHOR_NAME={}\nGIT_AUTHOR_EMAIL={}\nGIT_AUTHOR_DATE={}\n",
        sq_quote(&author.name.to_str_lossy()),
        sq_quote(&author.email.to_str_lossy()),
        sq_quote(&format!("@{} {sign}{:02}{:02}", time.seconds, off / 3600, (off % 3600) / 60)),
    )
}

/// Parse an `author-script` back into a signature.
fn parse_author_script(text: &str) -> Option<gix::actor::Signature> {
    let mut name = None;
    let mut email = None;
    let mut date = None;
    for line in text.lines() {
        let (k, v) = line.split_once('=')?;
        let v = sq_unquote(v);
        match k {
            "GIT_AUTHOR_NAME" => name = Some(v),
            "GIT_AUTHOR_EMAIL" => email = Some(v),
            "GIT_AUTHOR_DATE" => date = Some(v),
            _ => {}
        }
    }
    let date = date?;
    let (secs, offset) = date.trim_start_matches('@').split_once(' ')?;
    let seconds: i64 = secs.parse().ok()?;
    let (sign, digits) = offset.split_at(1);
    let hours: i32 = digits.get(..2)?.parse().ok()?;
    let minutes: i32 = digits.get(2..4)?.parse().ok()?;
    let mut off = hours * 3600 + minutes * 60;
    if sign == "-" {
        off = -off;
    }
    Some(gix::actor::Signature {
        name: name?.as_str().into(),
        email: email?.as_str().into(),
        time: gix::date::Time { seconds, offset: off },
    })
}

/// What a rebase did.
#[derive(Debug, Clone)]
pub enum RebaseOutcome {
    UpToDate,
    /// Finished: the branch now points at `head`. `skipped` commits were already upstream or dropped.
    Done {
        head: ObjectId,
        picked: usize,
        skipped: usize,
    },
    /// Stopped: resolve conflicts in `paths`, then `rebase --continue`.
    Stopped {
        commit: ObjectId,
        paths: Vec<BString>,
        remaining: usize,
    },
    /// Stopped at an `edit` step: amend the commit, then `rebase --continue`.
    Edit {
        commit: ObjectId,
        remaining: usize,
    },
    /// Stopped at a `break` step.
    Break {
        at: ObjectId,
        remaining: usize,
    },
    /// An `exec` step failed; fix things, then `rebase --continue`.
    ExecFailed {
        command: String,
        error: String,
        remaining: usize,
    },
}

/// Edits a commit message during `reword` and `squash` (the CLI opens an editor).
pub type MessageEditor<'a> = &'a mut dyn FnMut(&str) -> Result<String>;

/// Parse an edited todo list (`pick <id> <subject>` lines; `#` comments and blank lines ignored).
pub fn parse_todo(repo: &Repo, text: &str) -> Result<Vec<RebaseStep>> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (verb, rest) = line.split_once(char::is_whitespace).map(|(v, r)| (v, r.trim())).unwrap_or((line, ""));
        let action = RebaseAction::parse(verb).with_context(|| {
            format!(
                "line {}: unknown action '{verb}' (use pick, reword, edit, squash, fixup, drop, exec or break)",
                n + 1
            )
        })?;
        match action {
            RebaseAction::Exec => {
                if rest.is_empty() {
                    bail!("line {}: exec needs a command", n + 1);
                }
                out.push(RebaseStep {
                    action,
                    commit: ObjectId::null(repo.object_hash()),
                    command: Some(rest.to_owned()),
                });
                continue;
            }
            RebaseAction::Break => {
                out.push(RebaseStep::new(action, ObjectId::null(repo.object_hash())));
                continue;
            }
            _ => {}
        }
        let id = rest.split_whitespace().next().with_context(|| format!("line {}: missing commit", n + 1))?;
        let commit = repo.rev_parse_commit(id).with_context(|| format!("line {}", n + 1))?;
        out.push(RebaseStep::new(action, commit));
    }
    if let Some(first) =
        out.iter().find(|s| !matches!(s.action, RebaseAction::Drop | RebaseAction::Exec | RebaseAction::Break))
        && matches!(first.action, RebaseAction::Squash | RebaseAction::Fixup)
    {
        bail!("cannot '{}' without a previous commit", first.action.name());
    }
    Ok(out)
}

impl Repo {
    fn pick_state_path(&self, kind: PickKind) -> PathBuf {
        self.git_dir().join(kind.state_file())
    }

    /// The commit an in-progress cherry-pick or revert is replaying.
    pub fn pick_in_progress(&self) -> Result<Option<(PickKind, ObjectId)>> {
        for kind in [PickKind::CherryPick, PickKind::Revert] {
            if let Ok(text) = std::fs::read_to_string(self.pick_state_path(kind)) {
                let id = ObjectId::from_hex(text.trim().as_bytes()).context("corrupt sequencer state")?;
                return Ok(Some((kind, id)));
            }
        }
        Ok(None)
    }

    /// Remove `CHERRY_PICK_HEAD`, `REVERT_HEAD` and `MERGE_MSG`.
    pub fn clear_pick_state(&self) -> Result<()> {
        self.clear_merge_rr()?;
        for f in [
            self.pick_state_path(PickKind::CherryPick),
            self.pick_state_path(PickKind::Revert),
            self.git_dir().join("MERGE_MSG"),
        ] {
            match std::fs::remove_file(&f) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("could not remove {}", f.display())),
            }
        }
        Ok(())
    }

    fn pick_parent(&self, commit: ObjectId, mainline: Option<usize>) -> Result<Option<ObjectId>> {
        let info = self.commit_info(commit)?;
        match (info.parents.len(), mainline) {
            (0, _) => Ok(None),
            (1, None | Some(1)) => Ok(Some(info.parents[0])),
            (1, Some(_)) => bail!("commit {} is not a merge, so --mainline doesn't apply", crate::short(&commit)),
            (n, None) => bail!(
                "commit {} is a merge with {n} parents; choose the mainline with -m 1 (usually) or -m 2",
                crate::short(&commit)
            ),
            (n, Some(m)) if m == 0 || m > n => bail!("commit {} has no parent #{m}", crate::short(&commit)),
            (_, Some(m)) => Ok(Some(info.parents[m - 1])),
        }
    }

    /// Cherry-pick or revert `commit` onto HEAD.
    pub fn pick(&self, kind: PickKind, commit: ObjectId, options: &PickOptions) -> Result<PickOutcome> {
        if self.merge_head()?.is_some() || self.pick_in_progress()?.is_some() {
            bail!(
                "another merge, cherry-pick or revert is in progress\n  hint: finish it with `veyra commit`, or abort it"
            );
        }
        let head = self.head_commit_id()?.context("cannot replay onto an unborn branch")?;
        let index = self.flat_index()?;
        if index != self.flat_head()? {
            bail!("your index has staged changes; commit or unstage them first");
        }
        let info = self.commit_info(commit)?;
        let parent = self.pick_parent(commit, options.mainline)?;
        let parent_tree = match parent {
            Some(p) => self.commit_tree_id(p)?,
            None => self.empty_tree_id(),
        };
        let (base, theirs) = match kind {
            PickKind::CherryPick => (parent_tree, info.tree),
            PickKind::Revert => (info.tree, parent_tree),
        };
        let ours = self.commit_tree_id(head)?;

        let short = crate::short(&commit);
        let head_label = self.head_label()?;
        let their_label = format!("{short} ({})", info.summary());
        let labels = gix::merge::blob::builtin_driver::text::Labels {
            ancestor: Some(if kind == PickKind::CherryPick {
                "parent of picked commit".into()
            } else {
                "reverted commit".into()
            }),
            current: Some(head_label.as_bytes().as_bstr()),
            other: Some(their_label.as_bytes().as_bstr()),
        };
        let outcome = self
            .git
            .merge_trees(base, ours, theirs, labels, self.git.tree_merge_options()?)
            .map_err(|e| anyhow!("could not apply {short}: {e}"))?;
        let mut outcome = outcome;
        let how = gix::merge::tree::TreatAsUnresolved::git();
        let mut merged = outcome.tree.write()?.detach();

        let mut message = match kind {
            PickKind::CherryPick => {
                let mut m = info.message.clone();
                if options.record_origin {
                    m = format!("{}\n\n(cherry picked from commit {commit})\n", m.trim_end());
                }
                m
            }
            PickKind::Revert => format!("Revert \"{}\"\n\nThis reverts commit {commit}.\n", info.summary()),
        };

        let mut resolved = Vec::new();
        let mut conflicted = outcome.has_unresolved_conflicts(how);
        if conflicted {
            match self.structural_pass(&outcome, base, ours, theirs, merged)? {
                crate::semantic::StructuralPass::Clean(tree, trailers) => {
                    message = crate::commit::add_trailers(&message, &trailers);
                    merged = tree;
                    conflicted = false;
                }
                crate::semantic::StructuralPass::Partial(r) => resolved = r,
            }
        }
        if conflicted {
            let paths = self.write_conflicted_result(&outcome, merged, &resolved)?;
            std::fs::write(self.pick_state_path(kind), format!("{commit}\n"))?;
            let mut msg = message.clone();
            msg.push_str("\n# Conflicts:\n");
            for p in &paths {
                msg.push_str(&format!("#\t{p}\n"));
            }
            std::fs::write(self.git_dir().join("MERGE_MSG"), msg)?;
            return Ok(PickOutcome::Conflicts(paths));
        }
        if merged == ours {
            return Ok(PickOutcome::Empty);
        }
        self.checkout_tree(merged, false)?;
        if options.no_commit {
            return Ok(PickOutcome::Applied);
        }
        let author = match kind {
            PickKind::CherryPick => Some(self.git.find_commit(commit)?.author()?.to_owned()?),
            PickKind::Revert => None,
        };
        let id = self.commit(&CommitRequest { message, author, ..Default::default() })?;
        Ok(PickOutcome::Committed(id))
    }

    /// Commit the resolution of a stopped cherry-pick or revert.
    pub fn continue_pick(&self, message_override: Option<String>) -> Result<ObjectId> {
        let (kind, commit) = self.pick_in_progress()?.context("no cherry-pick or revert in progress")?;
        let index = self.load_index()?;
        if index.entries().iter().any(|e| e.stage_raw() != 0) {
            bail!("there are unresolved conflicts\n  hint: edit the files, `veyra add` them, then continue");
        }
        let message = message_override
            .or_else(|| std::fs::read_to_string(self.git_dir().join("MERGE_MSG")).ok())
            .map(|m| crate::commit::clean_message(&m, true))
            .unwrap_or_default();
        let author = match kind {
            PickKind::CherryPick => Some(self.git.find_commit(commit)?.author()?.to_owned()?),
            PickKind::Revert => None,
        };
        let id = self.commit(&CommitRequest { message, author, allow_empty: true, ..Default::default() })?;
        self.rerere_record()?;
        self.clear_pick_state()?;
        Ok(id)
    }

    // ---------------------------------------------------------------------------------------
    // rebase

    fn legacy_rebase_state_path(&self) -> PathBuf {
        self.veyra_dir().join("rebase-state")
    }

    fn rebase_dir(&self) -> PathBuf {
        self.git_dir().join("rebase-merge")
    }

    /// One todo/done line: `pick <id> <subject>`, as Git writes them.
    fn step_line(&self, step: &RebaseStep) -> String {
        match step.action {
            RebaseAction::Exec => format!("exec {}", step.command.as_deref().unwrap_or_default()),
            RebaseAction::Break => "break".into(),
            a => {
                let subject = self.commit_info(step.commit).map(|c| c.summary().to_owned()).unwrap_or_default();
                format!("{} {} {subject}", a.name(), step.commit)
            }
        }
    }

    /// Parse a `git-rebase-todo` written by Git or Veyra. Steps Veyra can't run (from
    /// `--rebase-merges` or `--update-refs`) are reported so the user can continue with Git.
    pub fn parse_git_todo(&self, text: &str) -> Result<Vec<RebaseStep>> {
        let null = ObjectId::null(self.object_hash());
        let mut out = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (verb, rest) = line.split_once(char::is_whitespace).map(|(v, r)| (v, r.trim())).unwrap_or((line, ""));
            match verb {
                "noop" => continue,
                "exec" | "x" => {
                    out.push(RebaseStep { action: RebaseAction::Exec, commit: null, command: Some(rest.to_owned()) });
                    continue;
                }
                "break" | "b" => {
                    out.push(RebaseStep::new(RebaseAction::Break, null));
                    continue;
                }
                "label" | "l" | "reset" | "t" | "merge" | "m" | "update-ref" | "u" => bail!(
                    "this rebase uses `{verb}` steps (from --rebase-merges or --update-refs), which Veyra can't run yet\n  \
                     hint: continue it with `git rebase --continue`"
                ),
                _ => {}
            }
            let action = RebaseAction::parse(verb).with_context(|| format!("unknown rebase step `{line}`"))?;
            let id =
                rest.split_whitespace().next().with_context(|| format!("rebase step without a commit: `{line}`"))?;
            if id.starts_with('-') {
                bail!(
                    "`{line}`: fixup -C / -c isn't supported by Veyra yet\n  hint: continue it with `git rebase --continue`"
                );
            }
            out.push(RebaseStep::new(action, self.rev_parse_commit(id)?));
        }
        Ok(out)
    }

    pub fn rebase_in_progress(&self) -> Result<Option<RebaseState>> {
        let dir = self.rebase_dir();
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).map(|s| s.trim_end().to_owned());
        let Ok(head_name) = read("head-name") else {
            // A rebase stopped by Veyra < 1.3.
            return match std::fs::read_to_string(self.legacy_rebase_state_path()) {
                Ok(text) => Ok(Some(RebaseState::from_legacy_text(&text)?)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            };
        };
        let id = |name: &str| -> Result<ObjectId> {
            let text = read(name).with_context(|| format!("incomplete rebase state: no {name}"))?;
            self.rev_parse_commit(text.trim())
        };
        let branch = (head_name != "detached HEAD").then_some(head_name);
        let todo = self.parse_git_todo(&read("git-rebase-todo").unwrap_or_default())?;
        let done: Vec<String> = read("done").unwrap_or_default().lines().map(str::to_owned).collect();
        let amend = read("amend").ok().and_then(|h| ObjectId::from_hex(h.trim().as_bytes()).ok());
        // Stopped with conflicts: Git records the commit being applied in REBASE_HEAD. (It writes
        // REBASE_HEAD at `edit` stops too, but those also have `amend`.)
        let rebase_head =
            if amend.is_some() { None } else { std::fs::read_to_string(self.git_dir().join("REBASE_HEAD")).ok() };
        let current = match rebase_head.ok_or(()) {
            Ok(hex) => {
                let commit = self.rev_parse_commit(hex.trim())?;
                let action = done
                    .last()
                    .and_then(|l| l.split_whitespace().next())
                    .and_then(RebaseAction::parse)
                    .unwrap_or(RebaseAction::Pick);
                Some(RebaseStep::new(action, commit))
            }
            Err(_) => None,
        };
        Ok(Some(RebaseState {
            branch,
            orig_head: id("orig-head")?,
            onto: id("onto")?,
            todo,
            current,
            done,
            interactive: dir.join("interactive").exists(),
            amend,
        }))
    }

    /// Write the state in Git's layout. `stop` describes the commit a conflict stopped on.
    fn save_rebase_state(&self, state: &RebaseState) -> Result<()> {
        let dir = self.rebase_dir();
        std::fs::create_dir_all(&dir)?;
        let write = |name: &str, content: String| std::fs::write(dir.join(name), content);
        write("head-name", format!("{}\n", state.branch.as_deref().unwrap_or("detached HEAD")))?;
        write("onto", format!("{}\n", state.onto))?;
        write("orig-head", format!("{}\n", state.orig_head))?;
        let todo: String = state.todo.iter().map(|s| format!("{}\n", self.step_line(s))).collect();
        write("git-rebase-todo", todo)?;
        let done: String = state.done.iter().map(|l| format!("{l}\n")).collect();
        write("done", done)?;
        write("msgnum", format!("{}\n", state.done.len()))?;
        write("end", format!("{}\n", state.done.len() + state.todo.len()))?;
        if state.interactive {
            write("interactive", String::new())?;
        }
        match state.amend {
            Some(id) => write("amend", format!("{id}\n"))?,
            None => remove_if_exists(&dir.join("amend"))?,
        }
        remove_if_exists(&self.legacy_rebase_state_path())?;
        Ok(())
    }

    /// Record a conflict stop the way Git does: `REBASE_HEAD`, `stopped-sha`, the message and the
    /// original author (so `git rebase --continue` commits the resolution correctly).
    fn record_rebase_stop(&self, commit: ObjectId) -> Result<()> {
        let dir = self.rebase_dir();
        let c = self.git.find_commit(commit)?;
        let info = self.commit_info(commit)?;
        std::fs::write(self.git_dir().join("REBASE_HEAD"), format!("{commit}\n"))?;
        std::fs::write(dir.join("stopped-sha"), format!("{commit}\n"))?;
        std::fs::write(dir.join("message"), &info.message)?;
        std::fs::write(dir.join("author-script"), author_script(&c.author()?))?;
        // Git doesn't keep CHERRY_PICK_HEAD during a rebase; REBASE_HEAD takes its place.
        remove_if_exists(&self.pick_state_path(PickKind::CherryPick))?;
        Ok(())
    }

    fn clear_rebase_stop(&self) -> Result<()> {
        let dir = self.rebase_dir();
        for f in [
            self.git_dir().join("REBASE_HEAD"),
            dir.join("stopped-sha"),
            dir.join("message"),
            dir.join("author-script"),
        ] {
            remove_if_exists(&f)?;
        }
        self.clear_pick_state()
    }

    pub fn clear_rebase_state(&self) -> Result<()> {
        self.clear_rebase_stop()?;
        match std::fs::remove_dir_all(self.rebase_dir()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        remove_if_exists(&self.legacy_rebase_state_path())
    }

    /// Commits on HEAD that are not on `upstream`, oldest first, excluding merges (like `git rebase`).
    pub fn commits_to_replay(&self, upstream: ObjectId) -> Result<Vec<ObjectId>> {
        let head = self.head_commit_id()?.context("cannot rebase an unborn branch")?;
        let walk = self
            .git
            .rev_walk([head])
            .with_hidden([upstream])
            .sorting(gix::revision::walk::Sorting::ByCommitTime(Default::default()))
            .all()?;
        let mut out = Vec::new();
        for info in walk {
            let info = info.map_err(|e| anyhow!("history traversal failed: {e}"))?;
            if info.parent_ids().count() <= 1 {
                out.push(info.id);
            }
        }
        out.reverse();
        Ok(out)
    }

    /// Start a rebase of the current branch onto `onto`, replaying commits not in `upstream`.
    /// With `plan` (from an edited todo list) this is `rebase -i`.
    pub fn rebase(
        &self,
        upstream: ObjectId,
        onto: Option<ObjectId>,
        plan: Option<Vec<RebaseStep>>,
        editor: MessageEditor<'_>,
    ) -> Result<RebaseOutcome> {
        if self.rebase_in_progress()?.is_some() {
            bail!("a rebase is already in progress\n  hint: `veyra rebase --continue`, `--skip` or `--abort`");
        }
        if self.git_dir().join("rebase-apply").exists() {
            bail!("a `git am` or apply-backend rebase is in progress (.git/rebase-apply); finish it with git first");
        }
        if self.merge_head()?.is_some() || self.pick_in_progress()?.is_some() {
            bail!("a merge, cherry-pick or revert is in progress; finish or abort it first");
        }
        let changes = self.worktree_changes()?;
        if !changes.is_clean() || self.flat_index()? != self.flat_head()? {
            bail!("cannot rebase: you have uncommitted changes\n  hint: commit them, or `veyra stash` them first");
        }
        let head = self.head_commit_id()?.context("cannot rebase an unborn branch")?;
        let onto = onto.unwrap_or(upstream);
        let interactive = plan.is_some();
        let todo = match plan {
            Some(p) => p,
            None => self
                .commits_to_replay(upstream)?
                .into_iter()
                .map(|commit| RebaseStep::new(RebaseAction::Pick, commit))
                .collect(),
        };
        // Already based on `onto`: nothing to replay (like git's "Current branch is up to date").
        if !interactive && self.is_ancestor(onto, head)? && (onto == upstream || todo.is_empty()) {
            return Ok(RebaseOutcome::UpToDate);
        }
        let branch = match self.head_state()? {
            HeadState::Branch { name, .. } => Some(name.to_str_lossy().into_owned()),
            HeadState::Detached(_) => None,
        };
        std::fs::write(self.git_dir().join("ORIG_HEAD"), format!("{head}\n"))?;
        // Detach at `onto` and replay.
        self.checkout_tree(self.commit_tree_id(onto)?, false)?;
        self.set_head_detached(onto, &format!("rebase (start): checkout {}", crate::short(&onto)))?;
        let state = RebaseState {
            branch,
            orig_head: head,
            onto,
            todo,
            current: None,
            done: Vec::new(),
            interactive,
            amend: None,
        };
        self.save_rebase_state(&state)?;
        self.run_rebase(state, 0, 0, editor)
    }

    fn run_rebase(
        &self,
        mut state: RebaseState,
        mut picked: usize,
        mut skipped: usize,
        editor: MessageEditor<'_>,
    ) -> Result<RebaseOutcome> {
        while !state.todo.is_empty() {
            let step = state.todo.remove(0);
            state.done.push(self.step_line(&step));
            match step.action {
                RebaseAction::Drop => {
                    skipped += 1;
                    continue;
                }
                RebaseAction::Break => {
                    self.save_rebase_state(&state)?;
                    let head = self.head_commit_id()?.unwrap_or(state.onto);
                    return Ok(RebaseOutcome::Break { at: head, remaining: state.todo.len() });
                }
                RebaseAction::Exec => {
                    let command = step.command.clone().unwrap_or_default();
                    if let Err(err) = self.run_exec(&command) {
                        self.save_rebase_state(&state)?;
                        return Ok(RebaseOutcome::ExecFailed {
                            command,
                            error: err.to_string(),
                            remaining: state.todo.len(),
                        });
                    }
                    continue;
                }
                _ => {}
            }
            match self.pick(PickKind::CherryPick, step.commit, &PickOptions::default())? {
                PickOutcome::Committed(_) => {
                    picked += 1;
                    self.after_rebase_pick(&step, editor)?;
                    if step.action == RebaseAction::Edit {
                        state.amend = self.head_commit_id()?;
                        self.save_rebase_state(&state)?;
                        self.record_rebase_stop(step.commit)?;
                        return Ok(RebaseOutcome::Edit { commit: step.commit, remaining: state.todo.len() });
                    }
                }
                PickOutcome::Empty => skipped += 1,
                PickOutcome::Applied => unreachable!("rebase always commits"),
                PickOutcome::Conflicts(paths) => {
                    let commit = step.commit;
                    state.current = Some(step);
                    let remaining = state.todo.len();
                    self.save_rebase_state(&state)?;
                    self.record_rebase_stop(commit)?;
                    return Ok(RebaseOutcome::Stopped { commit, paths, remaining });
                }
            }
        }
        // Finish: move the branch to the new tip and re-attach HEAD.
        let head = self.head_commit_id()?.context("rebase lost HEAD")?;
        if let Some(branch) = &state.branch {
            self.set_ref(
                branch,
                head,
                Some(Some(state.orig_head)),
                &format!("rebase (finish): {branch} onto {}", crate::short(&state.onto)),
            )?;
            self.set_head_symbolic(branch, &format!("rebase (finish): returning to {branch}"))?;
        }
        self.clear_rebase_state()?;
        Ok(RebaseOutcome::Done { head, picked, skipped })
    }

    /// Apply `reword`, `squash` and `fixup` to the commit that was just picked (HEAD).
    /// Run an `exec` step's command through the shell, from the worktree root.
    fn run_exec(&self, command: &str) -> Result<()> {
        let mut cmd = gix::path::env::shell_command();
        cmd.arg("-c").arg(command).current_dir(self.workdir()?);
        let status = cmd.status().with_context(|| format!("could not run `{command}`"))?;
        if !status.success() {
            bail!(
                "`{command}` failed ({})",
                status.code().map(|c| format!("exit code {c}")).unwrap_or_else(|| "killed".into())
            );
        }
        Ok(())
    }

    fn after_rebase_pick(&self, step: &RebaseStep, editor: MessageEditor<'_>) -> Result<()> {
        let head = self.head_commit_id()?.context("rebase lost HEAD")?;
        let picked = self.git.find_commit(head)?;
        let info = self.commit_info(head)?;
        let new_commit = match step.action {
            RebaseAction::Reword => {
                let message = crate::commit::clean_message(&editor(&info.message)?, true);
                if message.trim().is_empty() {
                    bail!("empty commit message for {}; aborting the reword", crate::short(&step.commit));
                }
                let author: gix::actor::Signature = picked.author()?.to_owned()?;
                Some((info.tree, info.parents.clone(), author, message))
            }
            RebaseAction::Squash | RebaseAction::Fixup => {
                let parent = *info.parents.first().context("nothing to squash into")?;
                let prev = self.commit_info(parent)?;
                let prev_author: gix::actor::Signature = self.git.find_commit(parent)?.author()?.to_owned()?;
                let message = if step.action == RebaseAction::Fixup {
                    prev.message.clone()
                } else {
                    let combined = format!(
                        "# This is a combination of 2 commits.\n{}\n\n{}",
                        prev.message.trim_end(),
                        info.message.trim_end()
                    );
                    crate::commit::clean_message(&editor(&combined)?, true)
                };
                Some((info.tree, prev.parents.clone(), prev_author, message))
            }
            _ => None,
        };
        if let Some((tree, parents, author, message)) = new_commit {
            let committer: gix::actor::Signature =
                self.git.committer().context("no committer identity")??.to_owned()?;
            let commit = gix::objs::Commit {
                message: message.as_str().into(),
                tree,
                author,
                committer,
                encoding: None,
                parents: parents.into_iter().collect(),
                extra_headers: Default::default(),
            };
            let id = self.write_commit_object(commit, self.should_sign(crate::sign::SignMode::Config))?;
            self.set_head_detached(
                id,
                &format!("rebase ({}): {}", step.action.name(), message.lines().next().unwrap_or_default()),
            )?;
        }
        Ok(())
    }

    /// Abort a stopped cherry-pick or revert: back to HEAD, discarding the partial result. For a
    /// multi-commit sequence, back to where the whole sequence started (like `git cherry-pick --abort`).
    pub fn abort_pick(&self) -> Result<()> {
        let start = self.sequencer_start();
        if self.pick_in_progress()?.is_none() && start.is_none() {
            bail!("no cherry-pick or revert in progress");
        }
        let head = self.head_commit_id()?.context("HEAD is unborn")?;
        let safe = std::fs::read_to_string(self.sequencer_dir().join("abort-safety"))
            .ok()
            .and_then(|h| ObjectId::from_hex(h.trim().as_bytes()).ok());
        match start {
            // Rewind only if HEAD is still where the sequence stopped, like Git.
            Some(start) if safe.is_none_or(|s| s == head) => {
                self.checkout_tree_forced_to(start)?;
                self.update_head_target(start, Some(head), "cherry-pick: abort")?;
            }
            _ => self.checkout_tree_forced_to(head)?,
        }
        self.clear_sequencer()?;
        self.clear_pick_state()
    }

    // ---------------------------------------------------------------------------------------
    // multi-commit sequences, in Git's `.git/sequencer/` format

    fn sequencer_dir(&self) -> PathBuf {
        self.git_dir().join("sequencer")
    }

    fn sequencer_start(&self) -> Option<ObjectId> {
        let text = std::fs::read_to_string(self.sequencer_dir().join("head")).ok()?;
        ObjectId::from_hex(text.trim().as_bytes()).ok()
    }

    /// The commits a stopped multi-commit cherry-pick or revert still has to apply; the first
    /// one is the commit it stopped on. Empty when there is no such sequence.
    pub fn sequencer_todo(&self) -> Result<Vec<(PickKind, ObjectId)>> {
        let Ok(text) = std::fs::read_to_string(self.sequencer_dir().join("todo")) else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let (verb, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            let kind = match verb {
                "pick" | "p" => PickKind::CherryPick,
                "revert" => PickKind::Revert,
                other => bail!("unsupported step `{other}` in .git/sequencer/todo; continue with git"),
            };
            let id = rest.split_whitespace().next().context("sequencer step without a commit")?;
            out.push((kind, self.rev_parse_commit(id)?));
        }
        Ok(out)
    }

    /// Record a stopped sequence: where it started and what is left (the stopped commit first).
    pub fn save_sequencer(&self, start: ObjectId, todo: &[(PickKind, ObjectId)]) -> Result<()> {
        let dir = self.sequencer_dir();
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("head"), format!("{start}\n"))?;
        if let Some(head) = self.head_commit_id()? {
            std::fs::write(dir.join("abort-safety"), format!("{head}\n"))?;
        }
        let todo: String = todo
            .iter()
            .map(|(kind, id)| {
                let verb = if *kind == PickKind::Revert { "revert" } else { "pick" };
                let subject = self.commit_info(*id).map(|c| c.summary().to_owned()).unwrap_or_default();
                format!("{verb} {id} {subject}\n")
            })
            .collect();
        std::fs::write(dir.join("todo"), todo)?;
        Ok(())
    }

    pub fn clear_sequencer(&self) -> Result<()> {
        match std::fs::remove_dir_all(self.sequencer_dir()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Where a stopped sequence started, if one is in progress.
    pub fn sequence_start(&self) -> Option<ObjectId> {
        self.sequencer_start()
    }

    /// Abort a stopped rebase: the branch was never moved, so return HEAD and files to it.
    pub fn rebase_abort(&self) -> Result<()> {
        let state = self.rebase_in_progress()?.context("no rebase in progress")?;
        self.checkout_tree_forced_to(state.orig_head)?;
        match &state.branch {
            Some(branch) => self.set_head_symbolic(branch, &format!("rebase (abort): returning to {branch}"))?,
            None => self.set_head_detached(state.orig_head, "rebase (abort)")?,
        }
        self.clear_rebase_state()
    }

    /// Make index and worktree match `commit` exactly for tracked paths (untracked files stay).
    fn checkout_tree_forced_to(&self, commit: ObjectId) -> Result<()> {
        let target = self.flat_from_commit(commit)?;
        let snapshot = self.snapshot(&crate::worktree::SnapshotOptions::default())?;
        let index = self.load_index()?;
        let mut relevant: std::collections::BTreeSet<BString> =
            index.entries().iter().map(|e| e.path(&index).to_owned()).collect();
        relevant.extend(target.keys().cloned());
        let current: crate::FlatTree = snapshot.flat.into_iter().filter(|(p, _)| relevant.contains(p)).collect();
        let stats = self.apply_worktree(&current, &target)?;
        self.write_index_from_flat(&target, &stats)
    }

    /// Continue a stopped rebase after resolving conflicts (or skip the stopped commit), or after
    /// amending at an `edit` step.
    pub fn rebase_continue(&self, skip: bool, editor: MessageEditor<'_>) -> Result<RebaseOutcome> {
        let mut state = self.rebase_in_progress()?.context("no rebase in progress")?;
        let mut picked = 0;
        let mut skipped = 0;
        let stopped = state.current.clone().or_else(|| {
            // A rebase stopped by Veyra < 1.3 marked conflicts with CHERRY_PICK_HEAD.
            self.pick_in_progress().ok().flatten().map(|(_, c)| RebaseStep::new(RebaseAction::Pick, c))
        });
        if let Some(step) = stopped {
            if skip {
                let head = self.head_commit_id()?.context("rebase lost HEAD")?;
                self.reset(head, crate::checkout::ResetMode::Hard)?;
                skipped += 1;
            } else {
                let index = self.load_index()?;
                if index.entries().iter().any(|e| e.stage_raw() != 0) {
                    bail!(
                        "there are unresolved conflicts\n  hint: edit the files, `veyra add` them, then `veyra rebase --continue`"
                    );
                }
                if self.flat_index()? == self.flat_head()? {
                    // The resolution made the commit empty (or it was committed by hand): move on.
                    skipped += 1;
                } else {
                    let dir = self.rebase_dir();
                    let original = self.commit_info(step.commit)?;
                    let message = std::fs::read_to_string(dir.join("message")).unwrap_or(original.message);
                    let author = std::fs::read_to_string(dir.join("author-script"))
                        .ok()
                        .and_then(|t| parse_author_script(&t))
                        .map(Ok)
                        .unwrap_or_else(|| {
                            self.git.find_commit(step.commit)?.author()?.to_owned().map_err(anyhow::Error::from)
                        })?;
                    self.clear_pick_state()?;
                    self.commit(&CommitRequest {
                        message,
                        author: Some(author),
                        allow_empty: true,
                        ..Default::default()
                    })?;
                    picked += 1;
                    self.after_rebase_pick(&step, editor)?;
                    if step.action == RebaseAction::Edit {
                        state.current = None;
                        state.amend = self.head_commit_id()?;
                        self.save_rebase_state(&state)?;
                        self.record_rebase_stop(step.commit)?;
                        return Ok(RebaseOutcome::Edit { commit: step.commit, remaining: state.todo.len() });
                    }
                }
            }
            self.clear_rebase_stop()?;
        } else if let Some(amend) = state.amend {
            // Stopped at `edit`: staged changes amend that commit, like `git rebase --continue`.
            if self.head_commit_id()? == Some(amend) && self.flat_index()? != self.flat_head()? {
                let info = self.commit_info(amend)?;
                self.commit(&CommitRequest { message: info.message, amend: true, ..Default::default() })?;
            }
            self.clear_rebase_stop()?;
        }
        state.current = None;
        state.amend = None;
        self.run_rebase(state, picked, skipped, editor)
    }
}

fn remove_if_exists(path: &std::path::Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("could not remove {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_rebase_state_is_still_read() {
        let text = "branch refs/heads/topic\norig-head 1111111111111111111111111111111111111111\nonto 2222222222222222222222222222222222222222\ncurrent reword 2222222222222222222222222222222222222222\npick 1111111111111111111111111111111111111111\nexec cargo test\nbreak\n";
        let state = RebaseState::from_legacy_text(text).unwrap();
        assert_eq!(state.branch.as_deref(), Some("refs/heads/topic"));
        assert_eq!(state.todo.len(), 3);
        assert_eq!(state.current.unwrap().action, RebaseAction::Reword);
    }

    #[test]
    fn author_scripts_round_trip_like_git() {
        let sig = gix::actor::Signature {
            name: "Jo O'Neil".into(),
            email: "jo@example.com".into(),
            time: gix::date::Time { seconds: 1_700_000_000, offset: -5 * 3600 - 1800 },
        };
        let script = author_script(&sig.to_ref(&mut Default::default()));
        assert!(script.contains("GIT_AUTHOR_NAME='Jo O'\\''Neil'"), "{script}");
        assert!(script.contains("GIT_AUTHOR_DATE='@1700000000 -0530'"), "{script}");
        assert_eq!(parse_author_script(&script).unwrap(), sig);
    }

    #[test]
    fn action_verbs_and_abbreviations() {
        for (w, a) in [
            ("p", RebaseAction::Pick),
            ("reword", RebaseAction::Reword),
            ("f", RebaseAction::Fixup),
            ("drop", RebaseAction::Drop),
        ] {
            assert_eq!(RebaseAction::parse(w), Some(a));
        }
        assert_eq!(RebaseAction::parse("frobnicate"), None);
    }
}
