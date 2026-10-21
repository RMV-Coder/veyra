//! The journal itself: storage, locking, transactions and garbage-collection protection.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use veyra_core::gix;
use veyra_core::gix::objs::tree::EntryKind;
use veyra_core::worktree::SnapshotOptions;
use veyra_core::{ObjectId, Repo};

use crate::view::{self, Captured, View, parse_id};

/// The ref that keeps the main worktree's journal objects alive through `git gc`
/// (linked worktrees use `refs/veyra/keep-<id>`; see `Repo::keep_ref`).
pub const KEEP_REF: &str = "refs/veyra/keep";

/// One journaled operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    pub id: String,
    /// Seconds since the Unix epoch.
    pub time: i64,
    /// The command line that caused it.
    pub command: String,
    /// commit, switch, branch, tag, reset, merge, add, checkpoint, undo, redo, external, …
    pub kind: String,
    pub summary: String,
    pub before: View,
    pub after: View,
    /// For undo/redo: the operation that was undone or redone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Paths that differed between index and worktree when `before` was captured.
    #[serde(default)]
    pub worktree_changes: usize,
}

impl Operation {
    pub fn short_id(&self) -> &str {
        &self.id[..self.id.len().min(8)]
    }

    /// An operation that changed nothing restorable (checkpoints, no-op commands).
    pub fn is_noop(&self) -> bool {
        self.before.same_repository_state(&self.after) && self.before.worktree == self.after.worktree
    }

    /// Can `veyra undo` target this operation?
    pub fn is_undoable(&self) -> bool {
        !matches!(self.kind.as_str(), "undo" | "redo" | "checkpoint" | "init" | "push") && !self.is_noop()
    }
}

/// Settings that shape snapshots and retention, read from `veyra.*` git config.
#[derive(Debug, Clone)]
pub struct Settings {
    pub snapshot: SnapshotOptions,
    /// Take checkpoints on read-only commands too (`veyra.autoCheckpoint`).
    pub auto_checkpoint: bool,
}

impl Settings {
    pub fn from_repo(repo: &Repo) -> Settings {
        let mut snapshot = SnapshotOptions::default();
        if let Some(size) = repo.config_int("veyra.checkpointMaxFileSize")
            && size > 0
        {
            snapshot.max_file_size = size as u64;
        }
        snapshot.exclude = repo.config_strings("veyra.checkpointExclude");
        Settings { snapshot, auto_checkpoint: repo.config_bool("veyra.autoCheckpoint").unwrap_or(false) }
    }
}

/// Access to a repository's operation log.
pub struct OpLog<'r> {
    pub(crate) repo: &'r Repo,
    pub(crate) dir: PathBuf,
    pub settings: Settings,
}

/// Exclusive lock on the journal; released on drop.
pub(crate) struct Lock {
    path: PathBuf,
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl<'r> OpLog<'r> {
    pub fn open(repo: &'r Repo) -> Result<Self> {
        let dir = repo.veyra_dir();
        Ok(OpLog { repo, dir, settings: Settings::from_repo(repo) })
    }

    pub fn repo(&self) -> &'r Repo {
        self.repo
    }

    fn log_path(&self) -> PathBuf {
        self.dir.join("oplog.jsonl")
    }

    /// All operations, oldest first.
    pub fn operations(&self) -> Result<Vec<Operation>> {
        let path = self.log_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err).with_context(|| format!("could not read {}", path.display())),
        };
        let mut out = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Operation>(line) {
                Ok(op) => out.push(op),
                // A torn final line (crash mid-append) is ignored rather than poisoning the log.
                Err(_) if i + 1 == text.lines().count() => {}
                Err(err) => bail!("operation log is corrupt at line {}: {err}", i + 1),
            }
        }
        Ok(out)
    }

    /// Find an operation by id or unique id prefix.
    pub fn find(&self, query: &str) -> Result<Operation> {
        let ops = self.operations()?;
        let matches: Vec<&Operation> = ops.iter().filter(|op| op.id.starts_with(query)).collect();
        match matches.as_slice() {
            [one] => Ok((*one).clone()),
            [] => bail!("no operation or checkpoint with id '{query}'\n  hint: see `veyra op log`"),
            _ => bail!("'{query}' is ambiguous; use more characters"),
        }
    }

    pub(crate) fn lock(&self) -> Result<Lock> {
        std::fs::create_dir_all(&self.dir).with_context(|| format!("could not create {}", self.dir.display()))?;
        let path = self.dir.join("lock");
        for attempt in 0..50 {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    let _ = writeln!(f, "{}", std::process::id());
                    return Ok(Lock { path });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    // A lock older than 10 minutes is left over from a crash.
                    let stale = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age.as_secs() > 600);
                    if stale {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if attempt == 49 {
                        bail!(
                            "another Veyra command is running (lock file {} exists)\n  hint: if no Veyra process is running, delete that file",
                            path.display()
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(err) => return Err(err).with_context(|| format!("could not create {}", path.display())),
            }
        }
        unreachable!()
    }

    /// Start journaling a mutating command. Captures the `before` view (with a worktree
    /// snapshot — the automatic checkpoint) and records changes made by plain `git` since
    /// the last operation as an `external` operation.
    pub fn begin(&self, kind: &str, command: &str, touches_worktree: bool) -> Result<Transaction<'_, 'r>> {
        let lock = self.lock()?;
        let before = view::capture(self.repo, Some(&self.settings.snapshot))?;
        self.record_external_changes(&before)?;
        Ok(Transaction {
            log: self,
            _lock: lock,
            kind: kind.to_owned(),
            command: command.to_owned(),
            before,
            touches_worktree,
            target: None,
        })
    }

    /// If refs/HEAD/index changed since the last journaled operation, record that as an
    /// `external` operation so undo stays correct after plain `git` commands.
    fn record_external_changes(&self, current: &Captured) -> Result<()> {
        let Some(last) = self.operations()?.pop() else {
            return Ok(());
        };
        if last.after.same_repository_state(&current.view) {
            return Ok(());
        }
        // Worktree effects of external commands can't be told apart from the user's own
        // edits, so the external op carries no worktree snapshots (see apply.rs).
        let mut before = last.after.clone();
        before.worktree = None;
        before.skipped.clear();
        let mut after = current.view.clone();
        after.worktree = None;
        after.skipped.clear();
        let summary = describe_external(&before, &after);
        let op = Operation {
            id: new_op_id(),
            time: veyra_core::time::now_seconds(),
            command: "(outside Veyra, e.g. plain git)".into(),
            kind: "external".into(),
            summary,
            before,
            after,
            target: None,
            worktree_changes: 0,
        };
        self.append(&op)
    }

    pub(crate) fn append(&self, op: &Operation) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let mut line = serde_json::to_string(op)?;
        line.push('\n');
        self.drop_torn_tail()?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path())
            .context("could not open the operation log")?;
        file.write_all(line.as_bytes())?;
        file.sync_data().ok();
        self.protect(op)?;
        Ok(())
    }

    /// A crash in the middle of an append leaves a partial last line. It's ignored when reading,
    /// but appending after it would turn it into a corrupt line in the middle of the log, so cut
    /// it off first (under the journal lock).
    fn drop_torn_tail(&self) -> Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        let path = self.log_path();
        let mut file = match std::fs::OpenOptions::new().read(true).write(true).open(&path) {
            Ok(f) => f,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err).context("could not open the operation log"),
        };
        let len = file.metadata()?.len();
        if len == 0 {
            return Ok(());
        }
        let mut last = [0u8; 1];
        file.seek(SeekFrom::Start(len - 1))?;
        file.read_exact(&mut last)?;
        if last[0] == b'\n' {
            return Ok(());
        }
        let mut data = Vec::new();
        file.seek(SeekFrom::Start(0))?;
        file.read_to_end(&mut data)?;
        let keep = data.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
        file.set_len(keep as u64)?;
        file.sync_data().ok();
        Ok(())
    }

    /// Record a worktree-only checkpoint. Returns `None` if nothing changed since the last snapshot.
    pub fn checkpoint(&self, message: Option<&str>, command: &str, force_new: bool) -> Result<Option<Operation>> {
        let tx = self.begin("checkpoint", command, false)?;
        let last_tree = self
            .operations()?
            .iter()
            .rev()
            .find_map(|op| op.after.worktree.clone().or_else(|| op.before.worktree.clone()));
        if !force_new && last_tree.is_some() && last_tree == tx.before.view.worktree {
            return Ok(None);
        }
        let summary = message.map(str::to_owned).unwrap_or_else(|| "checkpoint".into());
        Ok(Some(tx.commit(&summary)?))
    }

    /// Take a checkpoint if automatic checkpoints are enabled and something changed.
    pub fn auto_checkpoint(&self, command: &str) -> Result<Option<Operation>> {
        if !self.settings.auto_checkpoint || self.repo.git().workdir().is_none() {
            return Ok(None);
        }
        self.checkpoint(Some("auto"), command, false)
    }

    /// Keep everything an operation references reachable, so `git gc` can't prune it.
    /// `refs/veyra/keep` points to a commit whose tree holds every snapshot and index tree,
    /// and whose parents are the commits operations orphaned.
    fn protect(&self, op: &Operation) -> Result<()> {
        let repo = self.repo;
        let previous = repo.ref_target(&repo.keep_ref())?;
        let (base_tree, mut parents) = match previous {
            Some(keep) => {
                let info = repo.commit_info(keep)?;
                (info.tree, info.parents)
            }
            None => (repo.empty_tree_id(), Vec::new()),
        };
        let mut editor = repo.git().edit_tree(base_tree)?;
        let prefix = format!("ops/{}/{}", &op.id[..2], op.id);
        let mut add = |name: &str, kind: EntryKind, hex: &Option<String>| -> Result<()> {
            if let Some(hex) = hex {
                editor.upsert(format!("{prefix}/{name}"), kind, parse_id(hex)?)?;
            }
            Ok(())
        };
        add("before-worktree", EntryKind::Tree, &op.before.worktree)?;
        add("after-worktree", EntryKind::Tree, &op.after.worktree)?;
        add("before-index", EntryKind::Tree, &Some(op.before.index_tree.clone()))?;
        add("after-index", EntryKind::Tree, &Some(op.after.index_tree.clone()))?;
        add("before-index-raw", EntryKind::Blob, &op.before.index_blob)?;
        add("after-index-raw", EntryKind::Blob, &op.after.index_blob)?;
        let tree = editor.write()?.detach();

        let mut known: BTreeSet<ObjectId> = parents.iter().copied().collect();
        for orphan in orphaned_commits(repo, &op.before, &op.after) {
            if known.insert(orphan) {
                parents.push(orphan);
            }
        }
        write_keep_commit(repo, tree, &parents, previous)
    }

    /// Drop operations older than `older_than` seconds (always keeping the newest `keep_min`),
    /// then rebuild `refs/veyra/keep`. Returns the number of operations removed.
    pub fn prune(&self, older_than: i64, keep_min: usize) -> Result<usize> {
        let _lock = self.lock()?;
        let ops = self.operations()?;
        let cutoff = veyra_core::time::now_seconds() - older_than;
        let protected_from = ops.len().saturating_sub(keep_min);
        let (kept, removed): (Vec<_>, Vec<_>) =
            ops.into_iter().enumerate().partition(|(i, op)| *i >= protected_from || op.time >= cutoff);
        if removed.is_empty() {
            return Ok(0);
        }
        let kept: Vec<Operation> = kept.into_iter().map(|(_, op)| op).collect();

        // Rewrite the log atomically.
        let tmp = self.dir.join("oplog.jsonl.tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            for op in &kept {
                writeln!(f, "{}", serde_json::to_string(op)?)?;
            }
            f.sync_data().ok();
        }
        std::fs::rename(&tmp, self.log_path())?;

        // Rebuild the keep commit from scratch.
        let repo = self.repo;
        let previous = repo.ref_target(&repo.keep_ref())?;
        let mut flat = veyra_core::FlatTree::new();
        let mut parents = Vec::new();
        let mut known = BTreeSet::new();
        for op in &kept {
            let prefix = format!("ops/{}/{}", &op.id[..2], op.id);
            for (name, kind, hex) in [
                ("before-index-raw", EntryKind::Blob, &op.before.index_blob),
                ("after-index-raw", EntryKind::Blob, &op.after.index_blob),
            ] {
                if let Some(hex) = hex {
                    flat.insert(format!("{prefix}/{name}").into(), veyra_core::FlatEntry::new(kind, parse_id(hex)?));
                }
            }
            for orphan in orphaned_commits(repo, &op.before, &op.after) {
                if known.insert(orphan) {
                    parents.push(orphan);
                }
            }
        }
        // Trees can't be expressed in a FlatTree (it holds leaves), so add them with the editor.
        let base = repo.write_flat(&flat)?;
        let mut editor = repo.git().edit_tree(base)?;
        for op in &kept {
            let prefix = format!("ops/{}/{}", &op.id[..2], op.id);
            for (name, hex) in [
                ("before-worktree", op.before.worktree.clone()),
                ("after-worktree", op.after.worktree.clone()),
                ("before-index", Some(op.before.index_tree.clone())),
                ("after-index", Some(op.after.index_tree.clone())),
            ] {
                if let Some(hex) = hex {
                    editor.upsert(format!("{prefix}/{name}"), EntryKind::Tree, parse_id(&hex)?)?;
                }
            }
        }
        let tree = editor.write()?.detach();
        write_keep_commit(repo, tree, &parents, previous)?;
        Ok(removed.len())
    }
}

fn write_keep_commit(repo: &Repo, tree: ObjectId, parents: &[ObjectId], previous: Option<ObjectId>) -> Result<()> {
    let now = veyra_core::time::now_seconds();
    let signature = gix::actor::Signature {
        name: "Veyra".into(),
        email: "veyra@localhost".into(),
        time: gix::date::Time { seconds: now, offset: 0 },
    };
    let commit = gix::objs::Commit {
        message: "veyra: keep\n\nKeeps Veyra's operation log and checkpoints safe from `git gc`.\nManaged by Veyra; see docs/DESIGN.md §4.5. Hide with `git log --exclude='refs/veyra/*' --all`.\n".into(),
        tree,
        author: signature.clone(),
        committer: signature,
        encoding: None,
        parents: parents.iter().copied().collect(),
        extra_headers: Default::default(),
    };
    let id = repo.git().write_object(&commit)?.detach();
    repo.set_ref(&repo.keep_ref(), id, Some(previous), "veyra: keep")?;
    Ok(())
}

/// Commits that moved away from a ref (or lost their ref) without remaining reachable from the new value.
fn orphaned_commits(repo: &Repo, before: &View, after: &View) -> Vec<ObjectId> {
    let mut out = Vec::new();
    let mut consider = |old: Option<&String>, new: Option<&String>| {
        let Some(old) = old.and_then(|h| parse_id(h).ok()) else { return };
        let Ok(old) = repo.peel_to_commit(old) else { return };
        let new = new.and_then(|h| parse_id(h).ok()).and_then(|n| repo.peel_to_commit(n).ok());
        match new {
            Some(n) if n == old => {}
            Some(n) if repo.is_ancestor(old, n).unwrap_or(false) => {}
            _ => out.push(old),
        }
    };
    for (name, old) in &before.refs {
        consider(Some(old), after.refs.get(name));
    }
    if let (Some(old), new) = (before.head_commit(), after.head_commit()) {
        let old = old.to_string();
        let new = new.map(|n| n.to_string());
        consider(Some(&old), new.as_ref());
    }
    out
}

fn describe_external(before: &View, after: &View) -> String {
    let mut parts = Vec::new();
    if before.head != after.head {
        parts.push(format!("HEAD {} → {}", before.head_label(), after.head_label()));
    }
    let changed: Vec<&String> = before
        .refs
        .keys()
        .chain(after.refs.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|k| before.refs.get(*k) != after.refs.get(*k))
        .collect();
    if !changed.is_empty() {
        let names: Vec<&str> = changed.iter().take(3).map(|k| veyra_core::refs::shorten(k)).collect();
        let more = if changed.len() > 3 { format!(" +{}", changed.len() - 3) } else { String::new() };
        parts.push(format!("refs changed: {}{more}", names.join(", ")));
    }
    if before.index_tree != after.index_tree {
        parts.push("index changed".into());
    }
    if parts.is_empty() {
        "repository changed outside Veyra".into()
    } else {
        format!("outside Veyra: {}", parts.join("; "))
    }
}

/// A random-enough, 16-hex-digit operation id.
pub fn new_op_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    let nanos =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
    hasher.write_u128(nanos);
    hasher.write_u32(std::process::id());
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    format!("{:016x}", hasher.finish())
}

/// An operation in progress. Dropping it without [`Transaction::commit`] records nothing.
pub struct Transaction<'l, 'r> {
    log: &'l OpLog<'r>,
    _lock: Lock,
    kind: String,
    command: String,
    pub before: Captured,
    touches_worktree: bool,
    target: Option<String>,
}

impl Transaction<'_, '_> {
    /// Mark this transaction as undoing/redoing `op_id`.
    pub fn set_target(&mut self, op_id: &str) {
        self.target = Some(op_id.to_owned());
    }

    /// Declare that the command changed worktree files after all.
    pub fn touched_worktree(&mut self) {
        self.touches_worktree = true;
    }

    /// Capture the `after` view and append the operation.
    pub fn commit(self, summary: &str) -> Result<Operation> {
        let repo = self.log.repo;
        let after = if self.touches_worktree {
            view::capture(repo, Some(&self.log.settings.snapshot))?.view
        } else {
            let mut v = view::capture(repo, None)?.view;
            v.worktree = self.before.view.worktree.clone();
            v.skipped = self.before.view.skipped.clone();
            v
        };
        let op = Operation {
            id: new_op_id(),
            time: veyra_core::time::now_seconds(),
            command: self.command.clone(),
            kind: self.kind.clone(),
            summary: summary.to_owned(),
            before: self.before.view.clone(),
            after,
            target: self.target.clone(),
            worktree_changes: self.before.worktree_changes,
        };
        self.log
            .append(&op)
            .map_err(|e| anyhow!("the command succeeded, but recording it in the operation log failed: {e}"))?;
        Ok(op)
    }
}
