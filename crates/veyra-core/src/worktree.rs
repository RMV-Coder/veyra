//! The working tree: status, snapshots, and writing files back through Git's filters.
//!
//! All content that goes *into* the object database passes the clean filter
//! (`core.autocrlf`, `.gitattributes`, `filter.*.clean`), and all content written *out*
//! passes the smudge filter — exactly the pipeline Git itself uses. This is what keeps
//! Veyra and Git agreeing on what is "modified", especially on Windows.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use gix::ObjectId;
use gix::bstr::{BStr, BString, ByteSlice};
use gix::objs::tree::EntryKind;

use crate::Repo;
use crate::flat::{self, FlatEntry, FlatTree, PathChange};

/// Index-vs-worktree differences for tracked files, plus untracked files.
#[derive(Debug, Default, Clone)]
pub struct WorktreeChanges {
    /// Tracked files whose content, mode or type differ from the index.
    pub modified: Vec<BString>,
    /// Tracked files missing from the worktree.
    pub deleted: Vec<BString>,
    /// Untracked, non-ignored files (individual files, never collapsed directories).
    pub untracked: Vec<BString>,
    /// Paths with unresolved merge conflicts in the index.
    pub conflicted: Vec<BString>,
}

impl WorktreeChanges {
    pub fn is_clean(&self) -> bool {
        self.modified.is_empty() && self.deleted.is_empty() && self.conflicted.is_empty()
    }

    /// Every path that differs from the index in any way (excluding untracked files).
    pub fn dirty_tracked(&self) -> BTreeSet<BString> {
        self.modified.iter().chain(&self.deleted).chain(&self.conflicted).cloned().collect()
    }
}

/// `git status`, structured.
#[derive(Debug, Default, Clone)]
pub struct Status {
    /// HEAD → index.
    pub staged: Vec<PathChange>,
    /// Index → worktree, for tracked files.
    pub worktree: WorktreeChanges,
}

/// A captured snapshot of the working tree.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// The snapshot tree id (as `git add -A && git write-tree` would produce, without touching the index).
    pub tree: ObjectId,
    pub flat: FlatTree,
    /// Untracked or modified files that were too large to snapshot.
    pub skipped: Vec<BString>,
    /// Number of paths that differ from the index.
    pub changed_vs_index: usize,
}

/// Options for [`Repo::snapshot`].
#[derive(Debug, Clone)]
pub struct SnapshotOptions {
    /// Files larger than this are not captured (default 20 MiB).
    pub max_file_size: u64,
    /// Extra glob patterns to exclude (in addition to `.gitignore`).
    pub exclude: Vec<String>,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        SnapshotOptions { max_file_size: 20 * 1024 * 1024, exclude: Vec::new() }
    }
}

/// Paths selected by the user: directory/file prefixes, or globs like `*.rs`.
#[derive(Debug, Clone, Default)]
pub struct Pathspec {
    prefixes: Vec<BString>,
    globs: Option<globset_lite::GlobSet>,
    /// The original arguments, for "did not match" errors.
    pub args: Vec<(String, BString)>,
}

impl Pathspec {
    /// Match everything.
    pub fn all() -> Self {
        Pathspec { prefixes: vec![BString::default()], globs: None, args: Vec::new() }
    }

    /// Build a pathspec from user arguments relative to `cwd`.
    pub fn new(repo: &Repo, cwd: &Path, args: &[String]) -> Result<Self> {
        if args.is_empty() {
            return Ok(Self::all());
        }
        let mut prefixes = Vec::new();
        let mut patterns = Vec::new();
        let mut originals = Vec::new();
        for arg in args {
            if arg.contains(['*', '?', '[']) {
                // Glob: resolve relative to cwd's position inside the repo.
                let base = repo.to_repo_path(cwd, ".")?;
                let pattern =
                    if base.is_empty() { arg.replace('\\', "/") } else { format!("{base}/{}", arg.replace('\\', "/")) };
                patterns.push(pattern.clone());
                originals.push((arg.clone(), BString::from(pattern)));
            } else {
                let path = repo.to_repo_path(cwd, arg)?;
                originals.push((arg.clone(), path.clone()));
                prefixes.push(path);
            }
        }
        let globs = if patterns.is_empty() { None } else { Some(globset_lite::GlobSet::new(&patterns)) };
        Ok(Pathspec { prefixes, globs, args: originals })
    }

    pub fn is_all(&self) -> bool {
        self.globs.is_none() && self.prefixes.iter().any(|p| p.is_empty())
    }

    pub fn matches(&self, path: &BStr) -> bool {
        self.prefixes.iter().any(|p| flat::path_is_within(path, p.as_ref()))
            || self.globs.as_ref().is_some_and(|g| g.is_match(path))
    }

    /// Does the single user argument `arg` match `path`?
    pub fn arg_matches(arg: &BStr, path: &BStr) -> bool {
        if arg.contains_str("*") || arg.contains_str("?") || arg.contains_str("[") {
            globset_lite::GlobSet::new(&[arg.to_str_lossy().into_owned()]).is_match(path)
        } else {
            flat::path_is_within(path, arg)
        }
    }
}

/// A tiny glob matcher (`*`, `**`, `?`, `[...]`) so the core crate stays dependency-light.
mod globset_lite {
    use gix::bstr::BStr;

    #[derive(Debug, Clone)]
    pub struct GlobSet {
        patterns: Vec<Vec<u8>>,
    }

    impl GlobSet {
        pub fn new(patterns: &[String]) -> Self {
            GlobSet { patterns: patterns.iter().map(|p| p.as_bytes().to_vec()).collect() }
        }

        pub fn is_match(&self, path: &BStr) -> bool {
            self.patterns.iter().any(|p| matches(p, path))
        }
    }

    /// Match `path` against `pattern`. A pattern without `/` matches the file name anywhere
    /// below the pattern's directory, like Git pathspecs (`*.rs` matches `src/a.rs`).
    pub fn matches(pattern: &[u8], path: &[u8]) -> bool {
        if wildmatch(pattern, path) {
            return true;
        }
        // Allow `dir/*.rs` to match `dir/sub/x.rs` as git's pathspec glob does (`*` crosses `/`).
        false
    }

    fn wildmatch(p: &[u8], s: &[u8]) -> bool {
        // Classic backtracking matcher; in pathspec globs `*` may match `/` (git's default, non-`:(glob)` mode).
        let (mut pi, mut si) = (0, 0);
        let (mut star_p, mut star_s) = (usize::MAX, 0);
        while si < s.len() {
            if pi < p.len() {
                match p[pi] {
                    b'*' => {
                        while pi < p.len() && p[pi] == b'*' {
                            pi += 1;
                        }
                        star_p = pi;
                        star_s = si;
                        continue;
                    }
                    b'?' => {
                        pi += 1;
                        si += 1;
                        continue;
                    }
                    b'[' => {
                        if let Some((matched, next)) = class(&p[pi..], s[si]) {
                            if matched {
                                pi += next;
                                si += 1;
                                continue;
                            }
                        } else if s[si] == b'[' {
                            pi += 1;
                            si += 1;
                            continue;
                        }
                    }
                    c if c == s[si] => {
                        pi += 1;
                        si += 1;
                        continue;
                    }
                    _ => {}
                }
            }
            if star_p != usize::MAX {
                star_s += 1;
                si = star_s;
                pi = star_p;
                continue;
            }
            return false;
        }
        while pi < p.len() && p[pi] == b'*' {
            pi += 1;
        }
        pi == p.len()
    }

    /// Match a `[...]` class at the start of `p`. Returns (matched, length of class).
    fn class(p: &[u8], c: u8) -> Option<(bool, usize)> {
        let mut i = 1;
        let negate = p.get(1).is_some_and(|b| *b == b'!' || *b == b'^');
        if negate {
            i += 1;
        }
        let mut matched = false;
        let start = i;
        while i < p.len() && (p[i] != b']' || i == start) {
            if i + 2 < p.len() && p[i + 1] == b'-' && p[i + 2] != b']' {
                if p[i] <= c && c <= p[i + 2] {
                    matched = true;
                }
                i += 3;
            } else {
                if p[i] == c {
                    matched = true;
                }
                i += 1;
            }
        }
        if i >= p.len() {
            return None;
        }
        Some((matched != negate, i + 1))
    }

    #[cfg(test)]
    mod tests {
        use super::matches;

        #[test]
        fn globs() {
            assert!(matches(b"*.rs", b"main.rs"));
            assert!(matches(b"*.rs", b"src/main.rs"));
            assert!(matches(b"src/*.rs", b"src/main.rs"));
            assert!(!matches(b"*.rs", b"main.rsx"));
            assert!(matches(b"file?.txt", b"file1.txt"));
            assert!(matches(b"[ab].txt", b"a.txt"));
            assert!(!matches(b"[!ab].txt", b"a.txt"));
            assert!(matches(b"docs/**", b"docs/a/b.md"));
        }
    }
}

/// Reject repository paths that could escape the worktree or write into `.git`.
pub fn validate_worktree_path(path: &BStr) -> Result<()> {
    if path.is_empty() || path.starts_with(b"/") {
        bail!("refusing unsafe path {path:?}");
    }
    for comp in path.split(|b| *b == b'/') {
        let lower = comp.to_ascii_lowercase();
        let trimmed: Vec<u8> = lower.iter().rev().skip_while(|b| **b == b'.' || **b == b' ').copied().collect();
        let trimmed: Vec<u8> = trimmed.into_iter().rev().collect();
        if comp.is_empty() || comp == b"." || comp == b".." || trimmed == b".git" || lower == b"git~1" {
            bail!("refusing unsafe path {path:?}");
        }
        if cfg!(windows) && comp.iter().any(|b| matches!(b, b'\\' | b':' | b'<' | b'>' | b'"' | b'|' | b'?' | b'*')) {
            bail!("path {path:?} contains characters that are not allowed on Windows");
        }
    }
    Ok(())
}

impl Repo {
    /// Load the index from disk (fresh, not cached), or an empty one if it doesn't exist.
    pub fn load_index(&self) -> Result<gix::index::File> {
        let path = self.index_path();
        if path.exists() {
            Ok(self.git.open_index().context("could not read the index")?)
        } else {
            Ok(gix::index::File::from_state(gix::index::State::new(self.git.object_hash()), path))
        }
    }

    /// The index's stage-0 entries as a flat tree.
    pub fn flat_index(&self) -> Result<FlatTree> {
        Ok(flat::flat_from_index_state(&*self.load_index()?))
    }

    /// Compare index and worktree. Uses gix's stat cache, so unchanged files are not re-read.
    pub fn worktree_changes(&self) -> Result<WorktreeChanges> {
        use gix::status::index_worktree::Item;
        use gix::status::plumbing::index_as_worktree::{Change, EntryStatus};

        self.workdir()?;
        let mut out = WorktreeChanges::default();
        let mut conflicted = BTreeSet::new();
        let platform = self
            .git
            .status(gix::progress::Discard)?
            .untracked_files(gix::status::UntrackedFiles::Files)
            .index_worktree_rewrites(None)
            .index_worktree_submodules(gix::status::Submodule::Given {
                ignore: gix::submodule::config::Ignore::All,
                check_dirty: false,
            });
        let mut iter = platform.into_index_worktree_iter(Vec::<BString>::new())?;
        let mut refreshed: Vec<(usize, gix::index::entry::Stat)> = Vec::new();
        for item in iter.by_ref() {
            let item = item.map_err(|e| anyhow!("status failed: {e}"))?;
            match item {
                Item::Modification { rela_path, status, entry_index, .. } => match status {
                    EntryStatus::Conflict { .. } => {
                        conflicted.insert(rela_path);
                    }
                    EntryStatus::Change(Change::Removed) => out.deleted.push(rela_path),
                    EntryStatus::Change(Change::Type { .. } | Change::Modification { .. }) => {
                        out.modified.push(rela_path)
                    }
                    EntryStatus::Change(Change::SubmoduleModification(_)) => {}
                    EntryStatus::IntentToAdd => out.modified.push(rela_path),
                    EntryStatus::NeedsUpdate(stat) => refreshed.push((entry_index, stat)),
                },
                Item::DirectoryContents { entry, .. } => {
                    if entry.status == gix::dir::entry::Status::Untracked
                        && matches!(entry.disk_kind, Some(gix::dir::entry::Kind::File | gix::dir::entry::Kind::Symlink))
                    {
                        out.untracked.push(entry.rela_path);
                    }
                }
                Item::Rewrite { .. } => {}
            }
        }
        // Persist refreshed stat information like `git status` does, so the next scan is fast —
        // but only onto the exact index the scan read (checked under the lock); it's just a cache.
        let scanned = iter.outcome_mut().and_then(|o| match &o.worktree_index {
            gix::worktree::IndexPersistedOrInMemory::Persisted(f) => f.checksum(),
            _ => None,
        });
        if !refreshed.is_empty()
            && let Some(base) = scanned
            && let Ok(mut fresh) = self.load_index()
            && fresh.checksum() == Some(base)
        {
            let entries = fresh.entries_mut();
            if refreshed.iter().all(|(i, _)| *i < entries.len()) {
                for (i, stat) in refreshed {
                    entries[i].stat = stat;
                }
                let _ = self.write_index_checked(&mut fresh, Some(base));
            }
        }
        out.conflicted = conflicted.into_iter().collect();
        out.modified.sort();
        out.deleted.sort();
        out.untracked.sort();
        Ok(out)
    }

    /// Full status: staged changes (HEAD → index) and worktree changes (index → worktree).
    pub fn status(&self) -> Result<Status> {
        let head = self.flat_head()?;
        let index = self.flat_index()?;
        Ok(Status { staged: flat::diff(&head, &index), worktree: self.worktree_changes()? })
    }

    /// Hash a worktree file through the clean filter and write it to the object database.
    /// Returns `None` if the file doesn't exist or can't be tracked.
    pub(crate) fn hash_worktree_file(
        &self,
        pipeline: &mut gix::filter::Pipeline<'_>,
        index: &gix::index::State,
        path: &BStr,
        previous_kind: Option<EntryKind>,
        batch: &mut crate::objects::BlobBatch<'_>,
    ) -> Result<Option<(FlatEntry, gix::index::entry::Stat)>> {
        use gix::filter::plumbing::pipeline::convert::ToGitOutcome;
        let rela = crate::repo::fs_path(path)?;
        let fs_path = self.workdir()?.join(&rela);
        let md = match std::fs::symlink_metadata(&fs_path) {
            Ok(md) => md,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("could not read {path}")),
        };
        let (data, kind) = if md.is_symlink() {
            let target = std::fs::read_link(&fs_path).with_context(|| format!("could not read {path}"))?;
            (gix::path::into_bstr(target)?.as_bytes().to_vec(), EntryKind::Link)
        } else if md.is_file() {
            let file = std::fs::File::open(&fs_path).with_context(|| format!("could not read {path}"))?;
            let mut data = Vec::with_capacity(md.len() as usize);
            match pipeline
                .convert_to_git(file, &rela, index)
                .with_context(|| format!("clean filter failed for {path}"))?
            {
                ToGitOutcome::Unchanged(mut f) => {
                    f.read_to_end(&mut data)?;
                }
                ToGitOutcome::Buffer(buf) => data.extend_from_slice(buf),
                ToGitOutcome::Process(mut r) => {
                    r.read_to_end(&mut data)?;
                }
            }
            let kind = if gix::fs::is_executable(&md) { EntryKind::BlobExecutable } else { EntryKind::Blob };
            (data, kind)
        } else {
            // Directories (nested repositories) and special files are not snapshotted or added implicitly.
            return Ok(None);
        };
        let id = batch.add(data)?;
        let caps = self.git.filesystem_options()?;
        let kind = adjust_kind(previous_kind, kind, &caps);
        let meta = gix::index::fs::Metadata::from_path_no_follow(&fs_path)
            .with_context(|| format!("could not stat {path}"))?;
        let stat = gix::index::entry::Stat::from_fs(&meta).unwrap_or_default();
        Ok(Some((FlatEntry { kind, id }, stat)))
    }

    /// Capture the working tree as a tree object without touching the index or HEAD.
    pub fn snapshot(&self, options: &SnapshotOptions) -> Result<Snapshot> {
        let changes = self.worktree_changes()?;
        let index = self.load_index()?;
        let mut flat = flat::flat_from_index_state(&index);
        let excluded =
            |path: &BStr| options.exclude.iter().any(|p| Pathspec::arg_matches(p.as_bytes().as_bstr(), path));

        let mut skipped = Vec::new();
        let mut changed = 0usize;
        if !(changes.modified.is_empty()
            && changes.deleted.is_empty()
            && changes.untracked.is_empty()
            && changes.conflicted.is_empty())
        {
            let (mut pipeline, _) = self.git.filter_pipeline(None)?;
            let mut batch = crate::objects::BlobBatch::new(self);
            let workdir = self.workdir()?.to_owned();
            for path in changes.deleted.iter() {
                if flat.remove(path).is_some() {
                    changed += 1;
                }
            }
            for path in changes.modified.iter().chain(&changes.conflicted).chain(&changes.untracked) {
                if excluded(path.as_ref()) {
                    continue;
                }
                let fs_path = workdir.join(crate::repo::fs_path(path.as_bstr())?);
                if let Ok(md) = std::fs::symlink_metadata(&fs_path)
                    && md.len() > options.max_file_size
                {
                    skipped.push(path.clone());
                    continue;
                }
                let previous = flat.get(path).map(|e| e.kind);
                match self.hash_worktree_file(&mut pipeline, &index, path.as_ref(), previous, &mut batch)? {
                    Some((entry, _)) => {
                        if flat.insert(path.clone(), entry) != Some(entry) {
                            changed += 1;
                        }
                    }
                    None => {
                        if flat.remove(path).is_some() {
                            changed += 1;
                        }
                    }
                }
            }
            batch.finish()?;
        }
        let tree = self.write_flat(&flat)?;
        Ok(Snapshot { tree, flat, skipped, changed_vs_index: changed })
    }

    /// Read a worktree file and convert it to its git-internal form (clean filter) without
    /// writing it to the object database. Used for diffs.
    pub fn read_worktree_as_git(&self, path: &BStr) -> Result<Option<Vec<u8>>> {
        let fs_path = self.workdir()?.join(crate::repo::fs_path(path)?);
        let md = match std::fs::symlink_metadata(&fs_path) {
            Ok(md) => md,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("could not read {path}")),
        };
        if md.is_symlink() {
            let target = std::fs::read_link(&fs_path)?;
            return Ok(Some(gix::path::into_bstr(target)?.as_bytes().to_vec()));
        }
        if !md.is_file() {
            return Ok(None);
        }
        let (mut pipeline, index) = self.git.filter_pipeline(None)?;
        let file = std::fs::File::open(&fs_path)?;
        let rela = crate::repo::fs_path(path)?;
        let mut out = Vec::new();
        use gix::filter::plumbing::pipeline::convert::ToGitOutcome;
        match pipeline.convert_to_git(file, &rela, &index)? {
            ToGitOutcome::Unchanged(mut f) => {
                f.read_to_end(&mut out)?;
            }
            ToGitOutcome::Buffer(buf) => out.extend_from_slice(buf),
            ToGitOutcome::Process(mut r) => {
                r.read_to_end(&mut out)?;
            }
        }
        Ok(Some(out))
    }

    /// Write the given entries into the worktree (through the smudge filter), or delete the
    /// path when the entry is `None`. Deletions happen first so that file↔directory swaps work.
    /// Returns fresh stat information for every written file.
    pub fn write_worktree(
        &self,
        changes: &[(BString, Option<FlatEntry>)],
    ) -> Result<BTreeMap<BString, gix::index::entry::Stat>> {
        let sparse = self.sparse()?;
        self.write_worktree_with(changes, sparse.as_ref())
    }

    /// Like [`Repo::write_worktree`], with an explicit sparse cone: paths outside it are never
    /// written (but may be deleted).
    pub fn write_worktree_with(
        &self,
        changes: &[(BString, Option<FlatEntry>)],
        sparse: Option<&crate::sparse::Sparse>,
    ) -> Result<BTreeMap<BString, gix::index::entry::Stat>> {
        let workdir = self.workdir()?.to_owned();
        let caps = self.git.filesystem_options()?;
        let mut stats = BTreeMap::new();
        if changes.is_empty() {
            return Ok(stats);
        }
        for (path, _) in changes {
            validate_worktree_path(path.as_ref())?;
        }
        // Partial clones: fetch the blobs about to be written in one round trip.
        if self.is_partial() {
            self.ensure_objects(changes.iter().filter_map(|(p, e)| {
                e.filter(|e| e.kind != EntryKind::Commit && sparse.is_none_or(|s| s.includes(p.as_ref()))).map(|e| e.id)
            }))?;
        }

        // 1) Deletions, deepest first.
        let mut deletions: Vec<&BString> = changes.iter().filter(|(_, e)| e.is_none()).map(|(p, _)| p).collect();
        deletions.sort_by(|a, b| b.cmp(a));
        for path in deletions {
            let fs_path = workdir.join(crate::repo::fs_path(path.as_bstr())?);
            match std::fs::symlink_metadata(&fs_path) {
                Ok(md) if md.is_dir() && !md.is_symlink() => {
                    // A submodule directory or an empty dir: only remove if empty.
                    let _ = std::fs::remove_dir(&fs_path);
                }
                Ok(_) => {
                    remove_file_force(&fs_path).with_context(|| format!("could not delete {}", fs_path.display()))?;
                }
                Err(_) => {}
            }
            prune_empty_parents(&workdir, &fs_path);
        }

        // 2) Writes.
        let (mut pipeline, _index) = self.git.filter_pipeline(None)?;
        for (path, entry) in changes {
            let Some(entry) = entry else { continue };
            if sparse.is_some_and(|s| !s.includes(path.as_ref())) {
                continue; // outside the sparse checkout: stays skip-worktree
            }
            let fs_path = workdir.join(crate::repo::fs_path(path.as_bstr())?);
            if let Some(parent) = fs_path.parent() {
                ensure_dir(&workdir, parent)?;
            }
            // A directory may be in the way of a file.
            if let Ok(md) = std::fs::symlink_metadata(&fs_path)
                && md.is_dir()
                && !md.is_symlink()
            {
                if entry.kind == EntryKind::Commit {
                    continue;
                }
                std::fs::remove_dir(&fs_path)
                    .map_err(|_| anyhow!("cannot write {}: a non-empty directory is in the way", fs_path.display()))?;
            }
            match entry.kind {
                EntryKind::Commit => {
                    std::fs::create_dir_all(&fs_path)?;
                    continue;
                }
                EntryKind::Tree => continue,
                EntryKind::Link if caps.symlink => {
                    let target = self.read_blob(entry.id)?;
                    let _ = remove_file_force(&fs_path);
                    create_symlink(&crate::repo::fs_path(target.as_bstr())?, &fs_path)?;
                }
                EntryKind::Link => {
                    let target = self.read_blob(entry.id)?;
                    write_file(&fs_path, &target)?;
                }
                EntryKind::Blob | EntryKind::BlobExecutable => {
                    let data = self.read_blob(entry.id)?;
                    let opts = gix::filter::plumbing::pipeline::convert::to_worktree::Options::default();
                    let mut outcome = pipeline
                        .convert_to_worktree(&data, path.as_ref(), opts)
                        .with_context(|| format!("smudge filter failed for {path}"))?;
                    if let Some(bytes) = outcome.as_bytes() {
                        write_file(&fs_path, bytes)?;
                    } else if let Some(read) = outcome.as_read() {
                        let mut buf = Vec::new();
                        read.read_to_end(&mut buf)?;
                        write_file(&fs_path, &buf)?;
                    } else {
                        bail!("delayed filter output is not supported (path {path})");
                    }
                    set_executable(&fs_path, entry.kind == EntryKind::BlobExecutable && caps.executable_bit)?;
                }
            }
            if let Ok(meta) = gix::index::fs::Metadata::from_path_no_follow(&fs_path) {
                stats.insert(path.clone(), gix::index::entry::Stat::from_fs(&meta).unwrap_or_default());
            }
        }
        Ok(stats)
    }

    /// Make the worktree match `target` for the paths where `current` and `target` differ.
    /// Returns stats for written files.
    pub fn apply_worktree(
        &self,
        current: &FlatTree,
        target: &FlatTree,
    ) -> Result<BTreeMap<BString, gix::index::entry::Stat>> {
        let changes: Vec<_> = flat::diff(current, target).into_iter().map(|c| (c.path, c.new)).collect();
        self.write_worktree(&changes)
    }
}

/// Keep an existing index mode when the filesystem can't express it.
fn adjust_kind(previous: Option<EntryKind>, found: EntryKind, caps: &gix::fs::Capabilities) -> EntryKind {
    match (previous, found) {
        (Some(EntryKind::BlobExecutable), EntryKind::Blob) if !caps.executable_bit => EntryKind::BlobExecutable,
        (Some(EntryKind::Link), EntryKind::Blob | EntryKind::BlobExecutable) if !caps.symlink => EntryKind::Link,
        (_, kind) => kind,
    }
}

fn ensure_dir(workdir: &Path, dir: &Path) -> Result<()> {
    // If a file is in the way of a directory, remove it (it was deleted/replaced as part of the change set).
    let mut cur = PathBuf::new();
    for comp in dir.strip_prefix(workdir).unwrap_or(dir).components() {
        cur.push(comp);
        let full = workdir.join(&cur);
        if let Ok(md) = std::fs::symlink_metadata(&full)
            && !md.is_dir()
        {
            remove_file_force(&full)?;
        }
    }
    std::fs::create_dir_all(dir).with_context(|| format!("could not create directory {}", dir.display()))
}

fn write_file(path: &Path, data: &[u8]) -> Result<()> {
    // Write to a temporary file next to the target, then rename: never leave a half-written file.
    let tmp = path.with_file_name(format!(
        ".{}.veyra-tmp",
        path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    ));
    std::fs::write(&tmp, data).with_context(|| format!("could not write {}", path.display()))?;
    if path.exists() {
        // Read-only files (common on Windows) would make the rename fail.
        let _ = make_writable(path);
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&tmp);
            Err(err).with_context(|| format!("could not replace {}", path.display()))
        }
    }
}

fn make_writable(path: &Path) -> std::io::Result<()> {
    let mut perms = std::fs::metadata(path)?.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(path, perms)
}

fn remove_file_force(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            make_writable(path)?;
            std::fs::remove_file(path)
        }
        Err(err) => Err(err),
    }
}

fn prune_empty_parents(workdir: &Path, path: &Path) {
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d == workdir || !d.starts_with(workdir) {
            break;
        }
        if std::fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

#[cfg(unix)]
fn set_executable(path: &Path, executable: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    let mode = perms.mode();
    let new = if executable { mode | 0o111 } else { mode & !0o111 };
    if new != mode {
        perms.set_mode(new);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path, _executable: bool) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link).with_context(|| format!("could not create symlink {}", link.display()))
}

#[cfg(windows)]
fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::windows::fs::symlink_file(target, link)
        .with_context(|| format!("could not create symlink {}", link.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_paths_that_escape_or_touch_git_dir() {
        for bad in ["../x", "a/../../x", ".git/config", "a/.GIT/hooks", "a/.git./x", "git~1/config", "/abs", "a//b"] {
            assert!(validate_worktree_path(bad.into()).is_err(), "{bad} should be rejected");
        }
        for good in ["a", "src/main.rs", ".gitignore", "a/.github/workflows/ci.yml", "x.git"] {
            assert!(validate_worktree_path(good.into()).is_ok(), "{good} should be accepted");
        }
    }
}
