//! Merging: fast-forward, clean three-way merges, and conflicted merges left for resolution.

use anyhow::{Context, Result, anyhow, bail};
use gix::ObjectId;
use gix::bstr::{BString, ByteSlice};

use crate::Repo;
use crate::commit::CommitRequest;
use crate::flat::FlatEntry;
use crate::semantic::StructuralPass;

/// The merge strategy (`-s`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Strategy {
    /// The normal three-way merge (Git's `ort` / `recursive`).
    #[default]
    ThreeWay,
    /// Record the merge but keep our tree exactly (`-s ours`).
    Ours,
    /// Merge their tree into a subdirectory of ours (`-s subtree`, `-X subtree=<dir>`); the
    /// directory is found automatically when `None`.
    Subtree(Option<String>),
}

/// Which side wins conflicting hunks (`-X ours` / `-X theirs`); non-conflicting changes from
/// both sides are still merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Favor {
    Ours,
    Theirs,
}

/// How to merge.
#[derive(Debug, Clone, Default)]
pub struct MergeOptions {
    pub ff_only: bool,
    pub no_ff: bool,
    pub message: Option<String>,
    pub strategy: Strategy,
    pub favor: Option<Favor>,
}

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
        self.clear_merge_rr()?;
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
        let opts = MergeOptions { ff_only, no_ff, message, ..Default::default() };
        self.merge_with(&[(theirs, label.to_owned())], &opts)
    }

    /// Merge one or more heads (`label` is how each is named in messages) into the current branch.
    /// More than one head is an octopus merge.
    pub fn merge_with(&self, heads: &[(ObjectId, String)], opts: &MergeOptions) -> Result<MergeOutcome> {
        if self.merge_head()?.is_some() {
            bail!(
                "a merge is already in progress\n  hint: resolve conflicts and `veyra commit`, or `veyra merge --abort`"
            );
        }
        let Some((theirs, label)) = heads.first().cloned() else { bail!("nothing to merge") };
        if heads.len() > 1 {
            return self.merge_octopus(heads, opts);
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
        if self.is_ancestor(ours, theirs)? && !opts.no_ff {
            self.checkout_tree(self.commit_tree_id(theirs)?, false)?;
            self.update_head_target(theirs, Some(ours), &format!("merge {label}: Fast-forward"))?;
            return Ok(MergeOutcome::FastForward { from: Some(ours), to: theirs });
        }
        if opts.ff_only {
            bail!(
                "not possible to fast-forward: the branches have diverged\n  hint: run `veyra merge {label}` without --ff-only to create a merge commit"
            );
        }
        let summary = opts.message.clone().unwrap_or_else(|| self.merge_summary(std::slice::from_ref(&label)));
        let ours_tree = self.commit_tree_id(ours)?;

        // `-s ours`: record the merge, keep our tree exactly.
        if opts.strategy == Strategy::Ours {
            let commit =
                self.write_merge_commit(ours_tree, &[ours, theirs], &summary, crate::sign::SignMode::Config)?;
            return Ok(MergeOutcome::Merged { commit });
        }

        let head_label = self.head_label()?;
        let labels = gix::merge::blob::builtin_driver::text::Labels {
            ancestor: Some("merge base".into()),
            current: Some(head_label.as_bytes().as_bstr()),
            other: Some(label.as_bytes().as_bstr()),
        };
        let tree_options = self.git.tree_merge_options()?.with_file_favor(opts.favor.map(|f| match f {
            Favor::Ours => gix::merge::tree::FileFavor::Ours,
            Favor::Theirs => gix::merge::tree::FileFavor::Theirs,
        }));
        let base = self.git.merge_base(ours, theirs).ok().flatten().map(|b| b.detach());
        let base_tree = match base {
            Some(b) => self.commit_tree_id(b)?,
            None => self.empty_tree_id(),
        };
        let theirs_tree = self.commit_tree_id(theirs)?;

        if let Strategy::Subtree(prefix) = &opts.strategy {
            // Merge their whole tree into a subdirectory of ours.
            let prefix = match prefix {
                Some(p) => p.trim_matches('/').to_owned(),
                None => self.find_subtree_prefix(ours_tree, theirs_tree)?,
            };
            let (base_shifted, theirs_shifted) =
                (self.shift_tree(base_tree, &prefix)?, self.shift_tree(theirs_tree, &prefix)?);
            self.ensure_merge_blobs(base_shifted, ours_tree, theirs_shifted)?;
            let mut outcome = self
                .git
                .merge_trees(base_shifted, ours_tree, theirs_shifted, labels, tree_options)
                .map_err(|e| anyhow!("merge failed: {e}"))?;
            return self.finish_merge(
                &mut outcome,
                [base_shifted, ours_tree, theirs_shifted],
                ours,
                theirs,
                &summary,
                opts.no_ff,
            );
        }

        // Partial clones: fetch the blobs the merge will read, in one round trip.
        if self.is_partial() {
            self.ensure_merge_blobs(base_tree, ours_tree, theirs_tree)?;
        }
        let options: gix::merge::commit::Options = tree_options.into();
        let mut outcome =
            self.git.merge_commits(ours, theirs, labels, options).map_err(|e| anyhow!("merge failed: {e}"))?;
        self.finish_merge(
            &mut outcome.tree_merge,
            [base_tree, ours_tree, theirs_tree],
            ours,
            theirs,
            &summary,
            opts.no_ff,
        )
    }

    /// `Merge branch 'x' [into y]` / `Merge branches 'a', 'b' and 'c'`.
    fn merge_summary(&self, labels: &[String]) -> String {
        let current = self.head_state().ok().and_then(|h| h.branch_short().map(str::to_owned));
        let into = match current {
            Some(b) if b != "main" && b != "master" => format!(" into {b}"),
            _ => String::new(),
        };
        match labels {
            [one] => format!("Merge branch '{one}'{into}\n"),
            many => {
                let quoted: Vec<String> = many.iter().map(|l| format!("'{l}'")).collect();
                let (last, rest) = quoted.split_last().expect("at least two");
                format!("Merge branches {} and {last}{into}\n", rest.join(", "))
            }
        }
    }

    /// Commit a clean merge, or resolve what can be resolved and leave the rest for the user.
    fn finish_merge(
        &self,
        tree_merge: &mut gix::merge::tree::Outcome<'_>,
        [base_tree, ours_tree, theirs_tree]: [ObjectId; 3],
        ours: ObjectId,
        theirs: ObjectId,
        summary: &str,
        no_ff: bool,
    ) -> Result<MergeOutcome> {
        let how = gix::merge::tree::TreatAsUnresolved::git();
        let unresolved = tree_merge.has_unresolved_conflicts(how);
        let merged_tree = tree_merge.tree.write()?.detach();
        if !unresolved {
            self.checkout_tree(merged_tree, false)?;
            let commit =
                self.write_merge_commit(merged_tree, &[ours, theirs], summary, crate::sign::SignMode::Config)?;
            return Ok(MergeOutcome::Merged { commit });
        }
        // Conflicts in supported languages may still merge cleanly symbol by symbol.
        let resolved = match self.structural_pass(tree_merge, base_tree, ours_tree, theirs_tree, merged_tree)? {
            StructuralPass::Clean(tree, trailers) => {
                self.checkout_tree(tree, false)?;
                let message = crate::commit::add_trailers(summary, &trailers);
                let commit = self.write_merge_commit(tree, &[ours, theirs], &message, crate::sign::SignMode::Config)?;
                return Ok(MergeOutcome::Merged { commit });
            }
            StructuralPass::Partial(resolved) => resolved,
        };
        let paths = self.write_conflicted_result(tree_merge, merged_tree, &resolved)?;

        let [merge_head, merge_msg, merge_mode] = merge_files(self);
        std::fs::write(&merge_head, format!("{theirs}\n"))?;
        let mut msg = summary.to_owned();
        msg.push_str("\n# Conflicts:\n");
        for p in &paths {
            msg.push_str(&format!("#\t{p}\n"));
        }
        std::fs::write(&merge_msg, msg)?;
        std::fs::write(&merge_mode, if no_ff { "no-ff" } else { "" })?;
        Ok(MergeOutcome::Conflicts { paths })
    }

    /// An octopus merge: several heads in one commit. Like Git, it refuses to stop for
    /// conflicts — merge a conflicting head on its own first. Nothing is written until every
    /// head merged cleanly.
    fn merge_octopus(&self, heads: &[(ObjectId, String)], opts: &MergeOptions) -> Result<MergeOutcome> {
        let ours = self.head_commit_id()?.context("cannot make an octopus merge on an unborn branch")?;
        let mut todo = Vec::new();
        for (id, label) in heads {
            if !self.is_ancestor(*id, ours)? && *id != ours && !todo.iter().any(|(t, _)| t == id) {
                todo.push((*id, label.clone()));
            }
        }
        match todo.len() {
            0 => return Ok(MergeOutcome::AlreadyUpToDate),
            1 => return self.merge_with(&todo, opts),
            _ => {}
        }
        if opts.ff_only {
            bail!("an octopus merge can't be a fast-forward");
        }
        let labels: Vec<String> = todo.iter().map(|(_, l)| l.clone()).collect();
        let summary = opts.message.clone().unwrap_or_else(|| self.merge_summary(&labels));
        let ours_tree = self.commit_tree_id(ours)?;
        let mut parents = vec![ours];
        let tree = if opts.strategy == Strategy::Ours {
            ours_tree
        } else {
            let head_label = self.head_label()?;
            let mut current = ours_tree;
            for (id, label) in &todo {
                let base = self
                    .git
                    .merge_base(ours, *id)
                    .ok()
                    .flatten()
                    .with_context(|| format!("'{label}' has no common history with {head_label}"))?
                    .detach();
                let (base_tree, theirs_tree) = (self.commit_tree_id(base)?, self.commit_tree_id(*id)?);
                self.ensure_merge_blobs(base_tree, current, theirs_tree)?;
                let names = gix::merge::blob::builtin_driver::text::Labels {
                    ancestor: Some("merge base".into()),
                    current: Some(head_label.as_bytes().as_bstr()),
                    other: Some(label.as_bytes().as_bstr()),
                };
                let mut outcome = self
                    .git
                    .merge_trees(base_tree, current, theirs_tree, names, self.git.tree_merge_options()?)
                    .map_err(|e| anyhow!("merge failed: {e}"))?;
                if outcome.has_unresolved_conflicts(gix::merge::tree::TreatAsUnresolved::git()) {
                    bail!(
                        "merging '{label}' conflicts, and octopus merges don't stop for conflicts; nothing was changed\n  \
                         hint: merge '{label}' on its own first, then the others"
                    );
                }
                current = outcome.tree.write()?.detach();
            }
            current
        };
        parents.extend(todo.iter().map(|(id, _)| *id));
        if tree != ours_tree {
            self.checkout_tree(tree, false)?;
        }
        let commit = self.write_merge_commit(tree, &parents, &summary, crate::sign::SignMode::Config)?;
        Ok(MergeOutcome::Merged { commit })
    }

    /// `tree` with every entry moved under `prefix/`.
    fn shift_tree(&self, tree: ObjectId, prefix: &str) -> Result<ObjectId> {
        if prefix.is_empty() {
            return Ok(tree);
        }
        let flat = self.flat_from_tree(tree)?;
        let shifted: crate::FlatTree =
            flat.into_iter().map(|(p, e)| (BString::from(format!("{prefix}/{p}")), e)).collect();
        self.write_flat(&shifted)
    }

    /// For `-s subtree` without a prefix: the directory of ours whose contents look most like
    /// the root of theirs.
    fn find_subtree_prefix(&self, ours: ObjectId, theirs: ObjectId) -> Result<String> {
        let theirs_flat = self.flat_from_tree(theirs)?;
        let theirs_files: std::collections::BTreeSet<&BString> = theirs_flat.keys().collect();
        let ours_flat = self.flat_from_tree(ours)?;
        let mut dirs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for p in ours_flat.keys() {
            let p = p.to_str_lossy();
            let mut acc = String::new();
            for part in p.split('/').collect::<Vec<_>>().iter().rev().skip(1).rev() {
                if !acc.is_empty() {
                    acc.push('/');
                }
                acc.push_str(part);
                dirs.insert(acc.clone());
            }
        }
        let mut best: Option<(usize, String)> = None;
        for dir in dirs {
            let matching =
                theirs_files.iter().filter(|f| ours_flat.contains_key(&BString::from(format!("{dir}/{f}")))).count();
            if matching > 0 && best.as_ref().is_none_or(|(n, _)| matching > *n) {
                best = Some((matching, dir));
            }
        }
        match best {
            Some((n, dir)) if n * 2 >= theirs_files.len().max(1) => Ok(dir),
            _ => bail!("couldn't tell which directory to merge into\n  hint: name it with -X subtree=<directory>"),
        }
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
        let mut base_checksum = index.checksum();
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
        let mut paths: Vec<BString> = outcome
            .conflicts
            .iter()
            .filter(|c| c.is_unresolved(how))
            .map(|c| c.ours.location().to_owned())
            .filter(|p| !resolved.iter().any(|(r, _)| r == p))
            .collect();
        paths.sort();
        paths.dedup();
        // Replay resolutions recorded earlier (rerere); remember the new conflicts.
        let staged = self.rerere_replay(&mut index, &paths)?;
        paths.retain(|p| !staged.contains(p));
        let base = std::mem::take(&mut base_checksum);
        self.write_index_checked(&mut index, base)?;
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
        self.rerere_record()?;
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
