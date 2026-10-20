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
            _ => return None,
        })
    }
}

/// One line of a rebase plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebaseStep {
    pub action: RebaseAction,
    pub commit: ObjectId,
}

/// Persisted state of a rebase that stopped (conflict or `edit`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseState {
    /// The branch being rebased (`refs/heads/x`), or `None` for a detached HEAD.
    pub branch: Option<String>,
    pub orig_head: ObjectId,
    pub onto: ObjectId,
    /// Steps still to apply, in order.
    pub todo: Vec<RebaseStep>,
    /// The step whose replay stopped with conflicts.
    pub current: Option<RebaseStep>,
}

impl RebaseState {
    fn to_text(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("branch {}\n", self.branch.as_deref().unwrap_or("-")));
        s.push_str(&format!("orig-head {}\n", self.orig_head));
        s.push_str(&format!("onto {}\n", self.onto));
        if let Some(c) = self.current {
            s.push_str(&format!("current {} {}\n", c.action.name(), c.commit));
        }
        for t in &self.todo {
            s.push_str(&format!("{} {}\n", t.action.name(), t.commit));
        }
        s
    }

    fn from_text(text: &str) -> Result<RebaseState> {
        let parse =
            |hex: &str| ObjectId::from_hex(hex.trim().as_bytes()).map_err(|e| anyhow!("corrupt rebase state: {e}"));
        let step = |value: &str| -> Result<RebaseStep> {
            match value.split_once(' ') {
                Some((verb, hex)) => Ok(RebaseStep {
                    action: RebaseAction::parse(verb)
                        .with_context(|| format!("corrupt rebase state: unknown action {verb}"))?,
                    commit: parse(hex)?,
                }),
                None => Ok(RebaseStep { action: RebaseAction::Pick, commit: parse(value)? }),
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
                verb => {
                    if let Some(action) = RebaseAction::parse(verb) {
                        todo.push(RebaseStep { action, commit: parse(value)? });
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
        })
    }
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
        let mut words = line.split_whitespace();
        let verb = words.next().unwrap_or_default();
        let action = RebaseAction::parse(verb).with_context(|| {
            format!("line {}: unknown action '{verb}' (use pick, reword, edit, squash, fixup or drop)", n + 1)
        })?;
        let id = words.next().with_context(|| format!("line {}: missing commit", n + 1))?;
        let commit = repo.rev_parse_commit(id).with_context(|| format!("line {}", n + 1))?;
        out.push(RebaseStep { action, commit });
    }
    if let Some(first) = out.iter().find(|s| s.action != RebaseAction::Drop)
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
        let merged = outcome.tree.write()?.detach();

        let message = match kind {
            PickKind::CherryPick => {
                let mut m = info.message.clone();
                if options.record_origin {
                    m = format!("{}\n\n(cherry picked from commit {commit})\n", m.trim_end());
                }
                m
            }
            PickKind::Revert => format!("Revert \"{}\"\n\nThis reverts commit {commit}.\n", info.summary()),
        };

        if outcome.has_unresolved_conflicts(how) {
            let paths = self.write_conflicted_result(&outcome, merged)?;
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
        self.clear_pick_state()?;
        Ok(id)
    }

    // ---------------------------------------------------------------------------------------
    // rebase

    fn rebase_state_path(&self) -> PathBuf {
        self.veyra_dir().join("rebase-state")
    }

    pub fn rebase_in_progress(&self) -> Result<Option<RebaseState>> {
        match std::fs::read_to_string(self.rebase_state_path()) {
            Ok(text) => Ok(Some(RebaseState::from_text(&text)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn save_rebase_state(&self, state: &RebaseState) -> Result<()> {
        std::fs::create_dir_all(self.veyra_dir())?;
        std::fs::write(self.rebase_state_path(), state.to_text())?;
        Ok(())
    }

    pub fn clear_rebase_state(&self) -> Result<()> {
        match std::fs::remove_file(self.rebase_state_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
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
                .map(|commit| RebaseStep { action: RebaseAction::Pick, commit })
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
        let state = RebaseState { branch, orig_head: head, onto, todo, current: None };
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
            if step.action == RebaseAction::Drop {
                skipped += 1;
                continue;
            }
            match self.pick(PickKind::CherryPick, step.commit, &PickOptions::default())? {
                PickOutcome::Committed(_) => {
                    picked += 1;
                    self.after_rebase_pick(step, editor)?;
                    if step.action == RebaseAction::Edit {
                        self.save_rebase_state(&state)?;
                        return Ok(RebaseOutcome::Edit { commit: step.commit, remaining: state.todo.len() });
                    }
                }
                PickOutcome::Empty => skipped += 1,
                PickOutcome::Applied => unreachable!("rebase always commits"),
                PickOutcome::Conflicts(paths) => {
                    state.current = Some(step);
                    let remaining = state.todo.len();
                    self.save_rebase_state(&state)?;
                    return Ok(RebaseOutcome::Stopped { commit: step.commit, paths, remaining });
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
    fn after_rebase_pick(&self, step: RebaseStep, editor: MessageEditor<'_>) -> Result<()> {
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
            let id = self.git.write_object(&commit)?.detach();
            self.set_head_detached(
                id,
                &format!("rebase ({}): {}", step.action.name(), message.lines().next().unwrap_or_default()),
            )?;
        }
        Ok(())
    }

    /// Abort a stopped cherry-pick or revert: back to HEAD, discarding the partial result.
    pub fn abort_pick(&self) -> Result<()> {
        self.pick_in_progress()?.context("no cherry-pick or revert in progress")?;
        let head = self.head_commit_id()?.context("HEAD is unborn")?;
        self.checkout_tree_forced_to(head)?;
        self.clear_pick_state()
    }

    /// Abort a stopped rebase: the branch was never moved, so return HEAD and files to it.
    pub fn rebase_abort(&self) -> Result<()> {
        let state = self.rebase_in_progress()?.context("no rebase in progress")?;
        self.checkout_tree_forced_to(state.orig_head)?;
        match &state.branch {
            Some(branch) => self.set_head_symbolic(branch, &format!("rebase (abort): returning to {branch}"))?,
            None => self.set_head_detached(state.orig_head, "rebase (abort)")?,
        }
        self.clear_pick_state()?;
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
        if self.pick_in_progress()?.is_some() {
            if skip {
                let head = self.head_commit_id()?.context("rebase lost HEAD")?;
                self.reset(head, crate::checkout::ResetMode::Hard)?;
                self.clear_pick_state()?;
                skipped += 1;
            } else {
                let index = self.load_index()?;
                if index.entries().iter().any(|e| e.stage_raw() != 0) {
                    bail!(
                        "there are unresolved conflicts\n  hint: edit the files, `veyra add` them, then `veyra rebase --continue`"
                    );
                }
                if self.flat_index()? == self.flat_head()? {
                    // The resolution made the commit empty: drop it, like `git rebase --continue`.
                    self.clear_pick_state()?;
                    skipped += 1;
                } else {
                    self.continue_pick(None)?;
                    picked += 1;
                    if let Some(step) = state.current {
                        self.after_rebase_pick(step, editor)?;
                        if step.action == RebaseAction::Edit {
                            state.current = None;
                            self.save_rebase_state(&state)?;
                            return Ok(RebaseOutcome::Edit { commit: step.commit, remaining: state.todo.len() });
                        }
                    }
                }
            }
        }
        state.current = None;
        self.run_rebase(state, picked, skipped, editor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebase_state_round_trips() {
        let a = ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap();
        let b = ObjectId::from_hex(b"2222222222222222222222222222222222222222").unwrap();
        let pick = |commit| RebaseStep { action: RebaseAction::Pick, commit };
        let state = RebaseState {
            branch: Some("refs/heads/topic".into()),
            orig_head: a,
            onto: b,
            todo: vec![pick(a), RebaseStep { action: RebaseAction::Squash, commit: b }],
            current: Some(RebaseStep { action: RebaseAction::Reword, commit: b }),
        };
        assert_eq!(RebaseState::from_text(&state.to_text()).unwrap(), state);
        let detached = RebaseState { branch: None, current: None, todo: vec![], ..state };
        assert_eq!(RebaseState::from_text(&detached.to_text()).unwrap(), detached);
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
