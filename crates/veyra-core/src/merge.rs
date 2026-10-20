//! Merging: fast-forward, clean three-way merges, and conflicted merges left for resolution.

use anyhow::{Context, Result, anyhow, bail};
use gix::ObjectId;
use gix::bstr::{BString, ByteSlice};

use crate::Repo;
use crate::commit::CommitRequest;

/// What a merge did.
#[derive(Debug, Clone)]
pub enum MergeOutcome {
    AlreadyUpToDate,
    FastForward {
        from: Option<ObjectId>,
        to: ObjectId,
    },
    Merged {
        commit: ObjectId,
    },
    /// The merge stopped with conflicts written to the worktree and index.
    Conflicts {
        paths: Vec<BString>,
    },
}

/// Paths of files describing an in-progress merge.
fn merge_files(repo: &Repo) -> [std::path::PathBuf; 3] {
    let dir = repo.git_dir();
    [dir.join("MERGE_HEAD"), dir.join("MERGE_MSG"), dir.join("MERGE_MODE")]
}

impl Repo {
    /// The commit being merged in, if a merge is in progress.
    pub fn merge_head(&self) -> Result<Option<ObjectId>> {
        let path = self.git_dir().join("MERGE_HEAD");
        match std::fs::read_to_string(&path) {
            Ok(s) => {
                let line = s.lines().next().unwrap_or_default().trim();
                Ok(Some(ObjectId::from_hex(line.as_bytes()).with_context(|| format!("corrupt {}", path.display()))?))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// The prepared merge message, if a merge is in progress.
    pub fn merge_message(&self) -> Option<String> {
        std::fs::read_to_string(self.git_dir().join("MERGE_MSG")).ok()
    }

    /// Forget an in-progress merge's bookkeeping files (not the worktree).
    pub fn clear_merge_state(&self) -> Result<()> {
        for f in merge_files(self) {
            match std::fs::remove_file(&f) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err).with_context(|| format!("could not remove {}", f.display())),
            }
        }
        Ok(())
    }

    /// Merge `theirs` into the current branch.
    pub fn merge(
        &self,
        theirs: ObjectId,
        label: &str,
        ff_only: bool,
        no_ff: bool,
        message: Option<String>,
    ) -> Result<MergeOutcome> {
        if self.merge_head()?.is_some() {
            bail!(
                "a merge is already in progress\n  hint: resolve conflicts and `veyra commit`, or `veyra merge --abort`"
            );
        }
        let ours = self.head_commit_id()?;
        let Some(ours) = ours else {
            // Unborn branch: merging means adopting their history.
            self.checkout_tree(self.commit_tree_id(theirs)?, false)?;
            self.update_head_target(theirs, None, &format!("merge {label}: Fast-forward"))?;
            return Ok(MergeOutcome::FastForward { from: None, to: theirs });
        };
        if self.is_ancestor(theirs, ours)? {
            return Ok(MergeOutcome::AlreadyUpToDate);
        }
        if self.is_ancestor(ours, theirs)? && !no_ff {
            self.checkout_tree(self.commit_tree_id(theirs)?, false)?;
            self.update_head_target(theirs, Some(ours), &format!("merge {label}: Fast-forward"))?;
            return Ok(MergeOutcome::FastForward { from: Some(ours), to: theirs });
        }
        if ff_only {
            bail!(
                "not possible to fast-forward: the branches have diverged\n  hint: run `veyra merge {label}` without --ff-only to create a merge commit"
            );
        }

        let head_label = self.head_label()?;
        let labels = gix::merge::blob::builtin_driver::text::Labels {
            ancestor: Some("merge base".into()),
            current: Some(head_label.as_bytes().as_bstr()),
            other: Some(label.as_bytes().as_bstr()),
        };
        let options: gix::merge::commit::Options = self.git.tree_merge_options()?.into();
        let mut outcome =
            self.git.merge_commits(ours, theirs, labels, options).map_err(|e| anyhow!("merge failed: {e}"))?;
        let how = gix::merge::tree::TreatAsUnresolved::git();
        let unresolved = outcome.tree_merge.has_unresolved_conflicts(how);
        let merged_tree = outcome.tree_merge.tree.write()?.detach();

        let summary = message.unwrap_or_else(|| {
            let current = self.head_state().ok().and_then(|h| h.branch_short().map(str::to_owned));
            match current {
                Some(b) if b != "main" && b != "master" => format!("Merge branch '{label}' into {b}\n"),
                _ => format!("Merge branch '{label}'\n"),
            }
        });

        if !unresolved {
            self.checkout_tree(merged_tree, false)?;
            let commit = self.write_merge_commit(merged_tree, &[ours, theirs], &summary)?;
            return Ok(MergeOutcome::Merged { commit });
        }

        // Conflicts: write the merged tree (with conflict markers) and record stages in the index,
        // exactly like Git, so `git status`, editors and mergetools understand the state.
        self.checkout_tree(merged_tree, false)?;
        let mut index = self.load_index()?;
        outcome.tree_merge.index_changed_after_applying_conflicts(
            &mut index,
            how,
            gix::merge::tree::apply_index_entries::RemovalMode::Prune,
        );
        index.remove_tree();
        index.write(gix::index::write::Options::default())?;
        let paths: Vec<BString> = outcome
            .tree_merge
            .conflicts
            .iter()
            .filter(|c| c.is_unresolved(how))
            .map(|c| c.ours.location().to_owned())
            .collect();

        let [merge_head, merge_msg, merge_mode] = merge_files(self);
        std::fs::write(&merge_head, format!("{theirs}\n"))?;
        let mut msg = summary.clone();
        msg.push_str("\n# Conflicts:\n");
        for p in &paths {
            msg.push_str(&format!("#\t{p}\n"));
        }
        std::fs::write(&merge_msg, msg)?;
        std::fs::write(&merge_mode, if no_ff { "no-ff" } else { "" })?;
        Ok(MergeOutcome::Conflicts { paths })
    }

    fn write_merge_commit(&self, tree: ObjectId, parents: &[ObjectId], message: &str) -> Result<ObjectId> {
        self.require_identity()?;
        let committer: gix::actor::Signature =
            self.git.committer().ok_or_else(|| anyhow!("no committer"))??.to_owned()?;
        let author: gix::actor::Signature = self.git.author().ok_or_else(|| anyhow!("no author"))??.to_owned()?;
        let commit = gix::objs::Commit {
            message: message.into(),
            tree,
            author,
            committer,
            encoding: None,
            parents: parents.iter().copied().collect(),
            extra_headers: Default::default(),
        };
        let id = self.git.write_object(&commit)?.detach();
        self.update_head_target(
            id,
            Some(parents[0]),
            &format!("merge: {}", message.lines().next().unwrap_or_default()),
        )?;
        Ok(id)
    }

    /// Commit the resolution of an in-progress merge.
    pub fn commit_merge(&self, request: &CommitRequest) -> Result<ObjectId> {
        let theirs = self.merge_head()?.ok_or_else(|| anyhow!("no merge in progress"))?;
        let ours = self.head_commit_id()?.ok_or_else(|| anyhow!("HEAD is unborn"))?;
        let index = self.load_index()?;
        if index.entries().iter().any(|e| e.stage_raw() != 0) {
            bail!(
                "cannot commit: there are unresolved conflicts\n  hint: edit the files, `veyra add` them, then `veyra commit`"
            );
        }
        let tree = self.write_flat(&crate::flat::flat_from_index_state(&index))?;
        let message = crate::commit::add_trailers(&request.message, &request.trailers);
        let id = self.write_merge_commit(tree, &[ours, theirs], &message)?;
        self.clear_merge_state()?;
        Ok(id)
    }
}
