//! Merging: fast-forward, clean three-way merges, and conflicted merges left for resolution.

use anyhow::{Context, Result, anyhow, bail};
use gix::ObjectId;
use gix::bstr::{BString, ByteSlice};

use crate::Repo;
use crate::commit::CommitRequest;
use crate::flat::FlatEntry;
use crate::semantic::StructuralPass;

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
            let commit =
                self.write_merge_commit(merged_tree, &[ours, theirs], &summary, crate::sign::SignMode::Config)?;
            return Ok(MergeOutcome::Merged { commit });
        }

        // Conflicts in supported languages may still merge cleanly symbol by symbol.
        let base_tree = match self.git.merge_base(ours, theirs).ok().flatten() {
            Some(base) => self.commit_tree_id(base.detach())?,
            None => self.empty_tree_id(),
        };
        let (ours_tree, theirs_tree) = (self.commit_tree_id(ours)?, self.commit_tree_id(theirs)?);
        let resolved =
            match self.structural_pass(&outcome.tree_merge, base_tree, ours_tree, theirs_tree, merged_tree)? {
                StructuralPass::Clean(tree, trailers) => {
                    self.checkout_tree(tree, false)?;
                    let message = crate::commit::add_trailers(&summary, &trailers);
                    let commit =
                        self.write_merge_commit(tree, &[ours, theirs], &message, crate::sign::SignMode::Config)?;
                    return Ok(MergeOutcome::Merged { commit });
                }
                StructuralPass::Partial(resolved) => resolved,
            };
        let paths = self.write_conflicted_result(&outcome.tree_merge, merged_tree, &resolved)?;

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

    /// Write a conflicted merge result like Git does: the merged tree (with conflict markers) goes
    /// to the worktree, and conflict stages 1–3 go to the index, so `git status`, editors and
    /// mergetools understand the state. Files in `resolved` (merged structurally) are written
    /// and staged as resolved instead. Returns the paths that are still conflicted.
    pub(crate) fn write_conflicted_result(
        &self,
        outcome: &gix::merge::tree::Outcome<'_>,
        merged_tree: ObjectId,
        resolved: &[(BString, FlatEntry)],
    ) -> Result<Vec<BString>> {
        let how = gix::merge::tree::TreatAsUnresolved::git();
        self.checkout_tree(merged_tree, false)?;
        let mut index = self.load_index()?;
        outcome.index_changed_after_applying_conflicts(
            &mut index,
            how,
            gix::merge::tree::apply_index_entries::RemovalMode::Prune,
        );
        if !resolved.is_empty() {
            let changes: Vec<(BString, Option<FlatEntry>)> =
                resolved.iter().map(|(p, e)| (p.clone(), Some(*e))).collect();
            let stats = self.write_worktree(&changes)?;
            index.remove_entries(|_, path, _| resolved.iter().any(|(p, _)| p.as_bstr() == path));
            for (path, entry) in resolved {
                let stat = stats.get(path).copied().unwrap_or_default();
                index.dangerously_push_entry(
                    stat,
                    entry.id,
                    gix::index::entry::Flags::empty(),
                    entry.index_mode(),
                    path.as_ref(),
                );
            }
            index.sort_entries();
        }
        index.remove_tree();
        index.write(gix::index::write::Options::default())?;
        let mut paths: Vec<BString> = outcome
            .conflicts
            .iter()
            .filter(|c| c.is_unresolved(how))
            .map(|c| c.ours.location().to_owned())
            .filter(|p| !resolved.iter().any(|(r, _)| r == p))
            .collect();
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    fn write_merge_commit(
        &self,
        tree: ObjectId,
        parents: &[ObjectId],
        message: &str,
        sign: crate::sign::SignMode,
    ) -> Result<ObjectId> {
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
        let id = self.write_commit_object(commit, self.should_sign(sign))?;
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
        let id = self.write_merge_commit(tree, &[ours, theirs], &message, request.sign)?;
        self.clear_merge_state()?;
        Ok(id)
    }
}

/// One node of the branch tree (`veyra branch graph`).
#[derive(Debug, Clone)]
pub struct BranchNode {
    pub name: String,
    pub tip: ObjectId,
    /// The branch it was forked from, by commit ancestry (`None` for the root).
    pub parent: Option<String>,
    /// Commits ahead of / behind the parent.
    pub ahead: usize,
    pub behind: usize,
}

impl Repo {
    /// Conflicting paths a merge of `theirs` into `ours` would produce, computed entirely in
    /// memory: nothing is written to the object database or the worktree.
    pub fn merge_preview(&self, ours: ObjectId, theirs: ObjectId) -> Result<Vec<BString>> {
        let scratch = gix::open(self.git_dir())?.with_object_memory();
        let labels = gix::merge::blob::builtin_driver::text::Labels { ancestor: None, current: None, other: None };
        let options: gix::merge::commit::Options = scratch.tree_merge_options()?.into();
        let outcome =
            scratch.merge_commits(ours, theirs, labels, options).map_err(|e| anyhow!("merge preview failed: {e}"))?;
        let how = gix::merge::tree::TreatAsUnresolved::git();
        let mut paths: Vec<BString> = outcome
            .tree_merge
            .conflicts
            .iter()
            .filter(|c| c.is_unresolved(how))
            .map(|c| c.ours.location().to_owned())
            .collect();
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    /// Local branches arranged by ancestry: a branch's parent is the branch it shares the most
    /// recent history with beyond what both share with the root (the default branch).
    pub fn branch_tree(&self) -> Result<Vec<BranchNode>> {
        let branches = self.branches(false)?;
        let root = self.default_branch()?.or_else(|| branches.first().map(|b| b.name.clone()));
        let Some(root) = root else { return Ok(Vec::new()) };
        let tips: Vec<(String, ObjectId)> = branches.iter().map(|b| (b.name.clone(), b.target)).collect();
        let root_tip = tips.iter().find(|(n, _)| *n == root).map(|(_, t)| *t);
        let merge_base = |a: ObjectId, b: ObjectId| self.git.merge_base(a, b).ok().flatten().map(|m| m.detach());
        let time = |id: ObjectId| self.commit_info(id).map(|c| c.committer_time).unwrap_or(0);

        let mut out = Vec::new();
        for (name, tip) in &tips {
            if *name == root {
                out.push(BranchNode { name: name.clone(), tip: *tip, parent: None, ahead: 0, behind: 0 });
                continue;
            }
            let fork_root = root_tip.and_then(|r| merge_base(*tip, r));
            let mut parent: Option<(String, ObjectId, i64)> = None;
            for (other, other_tip) in &tips {
                if other == name || *other == root {
                    continue;
                }
                let Some(fork) = merge_base(*tip, *other_tip) else { continue };
                // B fully contained in X means B is behind X, not branched from it.
                if fork == *tip {
                    continue;
                }
                // Only a parent if B shares strictly more history with X than with the root.
                let deeper = match fork_root {
                    Some(fr) => fr != fork && self.is_ancestor(fr, fork).unwrap_or(false),
                    None => true,
                };
                if deeper && parent.as_ref().is_none_or(|(_, _, t)| time(fork) > *t) {
                    parent = Some((other.clone(), *other_tip, time(fork)));
                }
            }
            let (parent_name, parent_tip) = match parent {
                Some((n, t, _)) => (n, t),
                None => (root.clone(), root_tip.unwrap_or(*tip)),
            };
            let (ahead, behind) = self.ahead_behind(*tip, parent_tip)?;
            out.push(BranchNode { name: name.clone(), tip: *tip, parent: Some(parent_name), ahead, behind });
        }
        Ok(out)
    }
}
