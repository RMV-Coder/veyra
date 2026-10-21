//! `stash` (alias `shelve`), compatible with Git's `refs/stash`.
//!
//! A stash entry is Git's structure, so `git stash list/apply` and `veyra stash list/apply`
//! are interchangeable:
//!
//! ```text
//!   W  "WIP on main: abc1234 subject"     tree = tracked files as in the worktree
//!   ├─ parent 1: HEAD at stash time
//!   ├─ parent 2: I  "index on main: …"     tree = the index
//!   └─ parent 3: U  "untracked files on …" tree = untracked files (only with -u)
//! ```
//!
//! The list of entries is the reflog of `refs/stash` (newest = `stash@{0}`).

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use gix::ObjectId;
use gix::bstr::{BString, ByteSlice};

use crate::Repo;
use crate::flat::{self, FlatTree};
use crate::worktree::SnapshotOptions;

pub const STASH_REF: &str = "refs/stash";

/// One stash entry.
#[derive(Debug, Clone)]
pub struct StashEntry {
    /// 0 = newest (`stash@{0}`).
    pub index: usize,
    pub commit: ObjectId,
    pub message: String,
    pub time: i64,
}

/// What applying a stash did.
#[derive(Debug, Clone)]
pub enum StashApply {
    Applied { paths: usize },
    Conflicts(Vec<BString>),
}

/// One raw reflog line: `<old> <new> <identity> <time> <tz>\t<message>`.
#[derive(Debug, Clone)]
struct LogLine {
    old: ObjectId,
    new: ObjectId,
    rest: String,
}

impl LogLine {
    fn parse(line: &str) -> Option<LogLine> {
        let (old, rest) = line.split_once(' ')?;
        let (new, rest) = rest.split_once(' ')?;
        Some(LogLine {
            old: ObjectId::from_hex(old.as_bytes()).ok()?,
            new: ObjectId::from_hex(new.as_bytes()).ok()?,
            rest: rest.to_owned(),
        })
    }

    fn message(&self) -> &str {
        self.rest.split_once('\t').map(|(_, m)| m).unwrap_or("")
    }

    fn time(&self) -> i64 {
        let ident = self.rest.split_once('\t').map(|(i, _)| i).unwrap_or(&self.rest);
        ident.rsplit_once('>').and_then(|(_, t)| t.split_whitespace().next()).and_then(|t| t.parse().ok()).unwrap_or(0)
    }

    fn render(&self) -> String {
        format!("{} {} {}\n", self.old, self.new, self.rest)
    }
}

impl Repo {
    fn stash_log_path(&self) -> PathBuf {
        self.common_dir().join("logs").join("refs").join("stash")
    }

    fn stash_log_lines(&self) -> Result<Vec<LogLine>> {
        match std::fs::read_to_string(self.stash_log_path()) {
            Ok(text) => Ok(text.lines().filter_map(LogLine::parse).collect()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e).context("could not read the stash reflog"),
        }
    }

    /// Stash entries, newest first.
    pub fn stash_list(&self) -> Result<Vec<StashEntry>> {
        let lines = self.stash_log_lines()?;
        Ok(lines
            .iter()
            .rev()
            .enumerate()
            .map(|(index, l)| StashEntry { index, commit: l.new, message: l.message().to_owned(), time: l.time() })
            .collect())
    }

    /// Resolve `stash@{n}` / `n` / `None` (= 0) to an entry.
    pub fn stash_entry(&self, spec: Option<&str>) -> Result<StashEntry> {
        let n = match spec {
            None => 0,
            Some(s) => {
                let inner = s.strip_prefix("stash@{").and_then(|r| r.strip_suffix('}')).unwrap_or(s);
                inner
                    .parse::<usize>()
                    .map_err(|_| anyhow!("'{s}' is not a stash entry (use a number or stash@{{n}})"))?
            }
        };
        let list = self.stash_list()?;
        if list.is_empty() {
            bail!("no stash entries");
        }
        list.into_iter().nth(n).ok_or_else(|| anyhow!("stash@{{{n}}} does not exist"))
    }

    fn stash_commit(&self, tree: ObjectId, parents: &[ObjectId], message: &str) -> Result<ObjectId> {
        let committer: gix::actor::Signature =
            self.git.committer().ok_or_else(|| anyhow!("no committer identity"))??.to_owned()?;
        let author: gix::actor::Signature =
            self.git.author().ok_or_else(|| anyhow!("no author identity"))??.to_owned()?;
        let commit = gix::objs::Commit {
            message: format!("{message}\n").as_str().into(),
            tree,
            author,
            committer,
            encoding: None,
            parents: parents.iter().copied().collect(),
            extra_headers: Default::default(),
        };
        Ok(self.git.write_object(&commit)?.detach())
    }

    /// Save local changes to a new stash entry and reset them away. Returns the stash commit,
    /// or `None` if there was nothing to save.
    pub fn stash_push(
        &self,
        message: Option<&str>,
        include_untracked: bool,
        keep_index: bool,
    ) -> Result<Option<ObjectId>> {
        self.require_identity()?;
        let head = self.head_commit_id()?.context("you do not have the initial commit yet")?;
        let index_file = self.load_index()?;
        if index_file.entries().iter().any(|e| e.stage_raw() != 0) {
            bail!("cannot stash: there are unresolved conflicts");
        }
        let head_flat = self.flat_from_commit(head)?;
        let index = flat::flat_from_index_state(&index_file);
        let snapshot = self.snapshot(&SnapshotOptions::default())?;
        let tracked: FlatTree =
            snapshot.flat.iter().filter(|(p, _)| index.contains_key(*p)).map(|(p, e)| (p.clone(), *e)).collect();
        let untracked: FlatTree =
            snapshot.flat.iter().filter(|(p, _)| !index.contains_key(*p)).map(|(p, e)| (p.clone(), *e)).collect();
        let has_untracked = include_untracked && !untracked.is_empty();
        if tracked == head_flat && index == head_flat && !has_untracked {
            return Ok(None);
        }

        let info = self.commit_info(head)?;
        let branch = match self.head_state()? {
            crate::HeadState::Branch { name, .. } => crate::refs::shorten(&name.to_str_lossy()).to_owned(),
            crate::HeadState::Detached(_) => "(no branch)".into(),
        };
        let subject = format!("{} {}", crate::short(&head), info.summary());
        let i = self.stash_commit(self.write_flat(&index)?, &[head], &format!("index on {branch}: {subject}"))?;
        let mut parents = vec![head, i];
        if has_untracked {
            let u = self.stash_commit(
                self.write_flat(&untracked)?,
                &[],
                &format!("untracked files on {branch}: {subject}"),
            )?;
            parents.push(u);
        }
        let label = match message {
            Some(m) => format!("On {branch}: {m}"),
            None => format!("WIP on {branch}: {subject}"),
        };
        let w = self.stash_commit(self.write_flat(&tracked)?, &parents, &label)?;

        // Record it like git: update refs/stash and append to its reflog.
        {
            use gix::refs::transaction::{LogChange, PreviousValue, RefEdit, RefLog};
            self.git.edit_reference(RefEdit::update_with_log(
                STASH_REF.try_into().expect("valid"),
                w,
                PreviousValue::Any,
                LogChange { mode: RefLog::AndReference, force_create_reflog: true, message: label.as_str().into() },
            ))?;
        }

        // Reset the changes away (keeping the index with --keep-index). Only tracked paths, plus
        // untracked files that went into the stash, are touched; other untracked files stay.
        let target_index = if keep_index { index.clone() } else { head_flat.clone() };
        let relevant: std::collections::BTreeSet<&BString> = head_flat.keys().chain(index.keys()).collect();
        let mut current: FlatTree =
            snapshot.flat.iter().filter(|(p, _)| relevant.contains(p)).map(|(p, e)| (p.clone(), *e)).collect();
        if has_untracked {
            current.extend(untracked.iter().map(|(p, e)| (p.clone(), *e)));
        }
        let stats = self.apply_worktree(&current, &target_index)?;
        self.write_index_from_flat(&target_index, &stats)?;
        Ok(Some(w))
    }

    /// Apply a stash entry to the worktree (and the index, with `restore_index`).
    pub fn stash_apply(&self, entry: &StashEntry, restore_index: bool) -> Result<StashApply> {
        let w = self.commit_info(entry.commit)?;
        let (base, index_commit, untracked_commit) = match w.parents.as_slice() {
            [b, i] => (*b, *i, None),
            [b, i, u] => (*b, *i, Some(*u)),
            _ => bail!("{} is not a stash commit", crate::short(&entry.commit)),
        };
        if self.load_index()?.entries().iter().any(|e| e.stage_raw() != 0) {
            bail!("cannot apply a stash while there are unresolved conflicts");
        }
        let head = self.head_commit_id()?.context("HEAD is unborn")?;
        let head_flat = self.flat_from_commit(head)?;

        // Untracked files must not overwrite anything.
        let untracked = match untracked_commit {
            Some(u) => self.flat_from_commit(u)?,
            None => FlatTree::new(),
        };
        let workdir = self.workdir()?.to_owned();
        for p in untracked.keys() {
            if workdir.join(crate::repo::fs_path(p.as_ref())?).exists() {
                bail!(
                    "{p} already exists, so the stash's untracked files can't be restored\n  hint: move it away and try again"
                );
            }
        }

        let base_tree = self.commit_tree_id(base)?;
        let ours = self.commit_tree_id(head)?;
        let labels = gix::merge::blob::builtin_driver::text::Labels {
            ancestor: Some("stash base".into()),
            current: Some("Updated upstream".into()),
            other: Some("Stashed changes".into()),
        };
        let mut outcome = self
            .git
            .merge_trees(base_tree, ours, w.tree, labels, self.git.tree_merge_options()?)
            .map_err(|e| anyhow!("could not apply the stash: {e}"))?;
        let mut merged = outcome.tree.write()?.detach();
        if outcome.has_unresolved_conflicts(gix::merge::tree::TreatAsUnresolved::git()) {
            match self.structural_pass(&outcome, base_tree, ours, w.tree, merged)? {
                crate::semantic::StructuralPass::Clean(tree, _) => merged = tree,
                crate::semantic::StructuralPass::Partial(resolved) => {
                    let paths = self.write_conflicted_result(&outcome, merged, &resolved)?;
                    return Ok(StashApply::Conflicts(paths));
                }
            }
        }
        self.checkout_tree(merged, false)?;
        let merged_flat = self.flat_from_tree(merged)?;
        let changed = flat::diff(&head_flat, &merged_flat).len();

        // Index: like git, changes come back unstaged, except new files (re-staged so they
        // aren't lost as untracked). With --index, restore the stashed index exactly when possible.
        let new_index = if restore_index && base == head {
            self.flat_from_commit(index_commit)?
        } else {
            if restore_index {
                bail!("--index needs HEAD to be where it was when the stash was made ({})", crate::short(&base));
            }
            let mut idx = head_flat.clone();
            for (p, e) in &merged_flat {
                if !head_flat.contains_key(p) {
                    idx.insert(p.clone(), *e);
                }
            }
            idx
        };
        self.write_index_from_flat(&new_index, &BTreeMap::new())?;

        if !untracked.is_empty() {
            let writes: Vec<_> = untracked.iter().map(|(p, e)| (p.clone(), Some(*e))).collect();
            self.write_worktree(&writes)?;
        }
        Ok(StashApply::Applied { paths: changed + untracked.len() })
    }

    /// Remove a stash entry, rewriting the reflog like `git reflog delete --rewrite --updateref`.
    pub fn stash_drop(&self, entry: &StashEntry) -> Result<()> {
        let mut lines = self.stash_log_lines()?;
        let idx = lines.len().checked_sub(1 + entry.index).context("no such stash entry")?;
        lines.remove(idx);
        let null = ObjectId::null(self.object_hash());
        for i in 0..lines.len() {
            lines[i].old = if i == 0 { null } else { lines[i - 1].new };
        }
        match lines.last() {
            None => {
                self.delete_ref(STASH_REF, None)?;
                let _ = std::fs::remove_file(self.stash_log_path());
            }
            Some(last) => {
                self.set_ref(STASH_REF, last.new, None, "stash drop")?;
                let text: String = lines.iter().map(LogLine::render).collect();
                let path = self.stash_log_path();
                let tmp = path.with_extension("veyra-tmp");
                std::fs::write(&tmp, text)?;
                std::fs::rename(&tmp, &path).context("could not rewrite the stash reflog")?;
            }
        }
        Ok(())
    }

    /// Delete all stash entries.
    pub fn stash_clear(&self) -> Result<usize> {
        let n = self.stash_list()?.len();
        if self.ref_target(STASH_REF)?.is_some() {
            self.delete_ref(STASH_REF, None)?;
        }
        let _ = std::fs::remove_file(self.stash_log_path());
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::LogLine;

    #[test]
    fn reflog_lines_parse_and_render() {
        let raw = "0000000000000000000000000000000000000000 1111111111111111111111111111111111111111 Ada <ada@x> 1792483200 +0800\tWIP on main: abc subject";
        let line = LogLine::parse(raw).unwrap();
        assert_eq!(line.message(), "WIP on main: abc subject");
        assert_eq!(line.time(), 1_792_483_200);
        assert_eq!(line.render(), format!("{raw}\n"));
    }
}
