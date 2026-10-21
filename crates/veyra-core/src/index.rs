//! Staging: building, editing and writing the index.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use gix::bstr::{BStr, BString, ByteSlice};
use gix::index::entry::{Flags, Stat};

use crate::Repo;
use crate::flat::{self, FlatEntry, FlatTree};
use crate::worktree::Pathspec;

/// What `add` did.
#[derive(Debug, Default, Clone)]
pub struct AddOutcome {
    pub added: Vec<BString>,
    pub modified: Vec<BString>,
    pub removed: Vec<BString>,
}

impl AddOutcome {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.removed.is_empty()
    }
}

impl Repo {
    /// Replace the index with `flat`. Entries whose `(mode, id)` are unchanged keep their cached
    /// stat information; `fresh_stats` supplies stats for files that were just written.
    pub fn write_index_from_flat(&self, flat: &FlatTree, fresh_stats: &BTreeMap<BString, Stat>) -> Result<()> {
        let old = self.load_index()?;
        let mut old_entries: BTreeMap<&BStr, (&gix::index::Entry, FlatEntry)> = BTreeMap::new();
        for entry in old.entries() {
            if entry.stage_raw() != 0 {
                continue;
            }
            if let Some(kind) = FlatEntry::kind_from_index_mode(entry.mode) {
                old_entries.insert(entry.path(&old), (entry, FlatEntry { kind, id: entry.id }));
            }
        }
        let sparse = self.sparse()?;
        let mut state = gix::index::State::new(self.object_hash());
        for (path, entry) in flat {
            let stat = if let Some(stat) = fresh_stats.get(path) {
                *stat
            } else if let Some((old_entry, old_flat)) = old_entries.get(path.as_bstr())
                && old_flat == entry
            {
                old_entry.stat
            } else {
                Stat::default()
            };
            let flags = if sparse.as_ref().is_some_and(|s| !s.includes(path.as_ref())) {
                Flags::SKIP_WORKTREE | Flags::EXTENDED
            } else {
                Flags::empty()
            };
            state.dangerously_push_entry(stat, entry.id, flags, entry.index_mode(), path.as_ref());
        }
        state.sort_entries();
        self.write_index_state(state)
    }

    /// Write an index state to `.git/index` atomically.
    pub fn write_index_state(&self, state: gix::index::State) -> Result<()> {
        let mut file = gix::index::File::from_state(state, self.index_path());
        file.remove_tree();
        file.write(gix::index::write::Options::default()).context("could not write the index")?;
        Ok(())
    }

    /// Stage changes for the paths in `spec` (like `git add`, which also stages deletions).
    /// With `force`, explicitly named ignored files are added too.
    pub fn add(&self, spec: &Pathspec, force: bool) -> Result<AddOutcome> {
        let changes = self.worktree_changes()?;
        let mut index = self.load_index()?;
        let (mut pipeline, _) = self.git.filter_pipeline(None)?;
        let mut batch = crate::objects::BlobBatch::new(self);
        let mut outcome = AddOutcome::default();
        let mut matched_args = vec![false; spec.args.len()];
        let mut note_match = |path: &BStr| {
            for (i, (_, arg)) in spec.args.iter().enumerate() {
                if Pathspec::arg_matches(arg.as_ref(), path) {
                    matched_args[i] = true;
                }
            }
        };

        // Tracked paths matching the spec also count as matches even when unchanged (git doesn't error).
        for entry in index.entries() {
            let path = entry.path(&index);
            if spec.matches(path) {
                note_match(path);
            }
        }

        let mut upserts: Vec<(BString, FlatEntry, Stat, bool)> = Vec::new();
        let mut removals: Vec<BString> = Vec::new();
        for path in &changes.deleted {
            if spec.matches(path.as_ref()) {
                removals.push(path.clone());
            }
        }
        let candidates = changes
            .modified
            .iter()
            .map(|p| (p, false))
            .chain(changes.conflicted.iter().map(|p| (p, false)))
            .chain(changes.untracked.iter().map(|p| (p, true)));
        for (path, is_new) in candidates {
            if !spec.matches(path.as_ref()) {
                continue;
            }
            note_match(path.as_ref());
            let previous = index.entry_by_path(path.as_ref()).and_then(|e| FlatEntry::kind_from_index_mode(e.mode));
            match self.hash_worktree_file(&mut pipeline, &index, path.as_ref(), previous, &mut batch)? {
                Some((entry, stat)) => upserts.push((path.clone(), entry, stat, is_new)),
                None => removals.push(path.clone()),
            }
        }

        // Explicitly named, ignored files: only with --force.
        for (i, (arg, rela)) in spec.args.iter().enumerate() {
            if matched_args[i] {
                continue;
            }
            let fs_path = self.workdir()?.join(crate::repo::fs_path(rela.as_bstr())?);
            if fs_path.is_file() {
                if force {
                    match self.hash_worktree_file(&mut pipeline, &index, rela.as_ref(), None, &mut batch)? {
                        Some((entry, stat)) => upserts.push((rela.clone(), entry, stat, true)),
                        None => bail!("cannot add '{arg}'"),
                    }
                } else {
                    bail!(
                        "the path '{arg}' is ignored by .gitignore\n  hint: use `veyra add --force {arg}` to add it anyway"
                    );
                }
            } else if fs_path.is_dir() {
                // An empty or fully ignored directory: nothing to do, like git.
            } else {
                bail!("pathspec '{arg}' did not match any files");
            }
        }

        batch.finish()?;

        // Apply to the index.
        let removal_set: std::collections::BTreeSet<&BString> = removals.iter().collect();
        let upsert_paths: std::collections::BTreeSet<&BString> = upserts.iter().map(|(p, ..)| p).collect();
        index.remove_entries(|_, path, _| {
            let path = path.to_owned();
            removal_set.contains(&path) || upsert_paths.contains(&path)
        });
        for (path, entry, stat, is_new) in &upserts {
            index.dangerously_push_entry(*stat, entry.id, Flags::empty(), entry.index_mode(), path.as_ref());
            if *is_new {
                outcome.added.push(path.clone());
            } else {
                outcome.modified.push(path.clone());
            }
        }
        outcome.removed = removals;
        index.sort_entries();
        if !outcome.is_empty() {
            index.remove_tree();
            index.write(gix::index::write::Options::default()).context("could not write the index")?;
        }
        Ok(outcome)
    }

    /// Stage everything (tracked modifications, deletions, and new files) — `git add -A`.
    pub fn add_all(&self) -> Result<AddOutcome> {
        self.add(&Pathspec::all(), false)
    }

    /// Stage modifications and deletions of tracked files only — `git add -u` / `commit -a`.
    pub fn add_tracked(&self) -> Result<AddOutcome> {
        let changes = self.worktree_changes()?;
        let mut paths: Vec<String> = changes
            .modified
            .iter()
            .chain(&changes.deleted)
            .chain(&changes.conflicted)
            .map(|p| p.to_str_lossy().into_owned())
            .collect();
        if paths.is_empty() {
            return Ok(AddOutcome::default());
        }
        paths.sort();
        let workdir = self.workdir()?.to_owned();
        let spec = Pathspec::new(self, &workdir, &paths)?;
        self.add(&spec, false)
    }

    /// Remove paths from the index (and the worktree unless `cached`).
    /// Without `force`, refuses when the file has changes that would be lost.
    pub fn remove(&self, spec: &Pathspec, cached: bool, force: bool, recursive: bool) -> Result<Vec<BString>> {
        let index = self.load_index()?;
        let head = self.flat_head()?;
        let staged = flat::flat_from_index_state(&index);
        let changes = self.worktree_changes()?;
        let dirty = changes.dirty_tracked();

        let mut targets = Vec::new();
        for (arg, rela) in &spec.args {
            let matches: Vec<BString> =
                staged.keys().filter(|p| Pathspec::arg_matches(rela.as_ref(), p.as_ref())).cloned().collect();
            if matches.is_empty() {
                bail!("pathspec '{arg}' did not match any tracked files");
            }
            if !recursive && matches.iter().any(|m| m != rela) && !rela.contains_str("*") {
                bail!("not removing '{arg}' recursively without -r");
            }
            targets.extend(matches);
        }
        targets.sort();
        targets.dedup();

        if !force {
            for path in &targets {
                let staged_differs = head.get(path) != staged.get(path);
                let worktree_differs = dirty.contains(path);
                if !cached && (staged_differs || worktree_differs) {
                    bail!(
                        "'{path}' has {} changes\n  hint: use --cached to keep the file, or --force to remove it anyway",
                        if staged_differs { "staged" } else { "local" }
                    );
                }
            }
        }

        let mut flat = staged.clone();
        for path in &targets {
            flat.remove(path);
        }
        self.write_index_from_flat(&flat, &BTreeMap::new())?;
        if !cached {
            let deletions: Vec<_> = targets.iter().map(|p| (p.clone(), None)).collect();
            self.write_worktree(&deletions)?;
        }
        Ok(targets)
    }

    /// Rename a tracked file in the index and on disk (`git mv`).
    pub fn rename_path(&self, from: &BStr, to: &BStr, force: bool) -> Result<()> {
        crate::worktree::validate_worktree_path(to)?;
        let mut flat = self.flat_index()?;
        let moved: Vec<(BString, FlatEntry)> =
            flat.iter().filter(|(p, _)| flat::path_is_within(p.as_ref(), from)).map(|(p, e)| (p.clone(), *e)).collect();
        if moved.is_empty() {
            bail!("'{from}' is not tracked");
        }
        let workdir = self.workdir()?;
        let src = workdir.join(crate::repo::fs_path(from)?);
        let dst = workdir.join(crate::repo::fs_path(to)?);
        let dst = if dst.is_dir() {
            let name = src.file_name().context("invalid source path")?;
            dst.join(name)
        } else {
            dst
        };
        let to_rel = self.to_repo_path(workdir, &dst.to_string_lossy())?;
        if dst.exists() && !force {
            bail!("destination '{}' exists\n  hint: use --force to overwrite it", to_rel);
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&src, &dst)
            .with_context(|| format!("could not move {} to {}", src.display(), dst.display()))?;
        for (path, entry) in moved {
            flat.remove(&path);
            let suffix = &path[from.len()..];
            let mut new_path = to_rel.clone();
            new_path.extend_from_slice(suffix);
            flat.insert(new_path, entry);
        }
        self.write_index_from_flat(&flat, &BTreeMap::new())
    }
}
