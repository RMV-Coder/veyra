//! A full object check (`veyra doctor --full`): every stored object is read back, rehashed and
//! parsed, every pack and pack index has its checksum verified, and every object reachable from
//! a reference, `HEAD` or the index must exist — the same ground `git fsck --full` covers.
//!
//! Partial and shallow clones are missing objects on purpose: objects the promisor remote
//! promised and parents cut off by the shallow boundary are counted, not reported.

use std::collections::HashSet;
use std::sync::atomic::AtomicBool;

use anyhow::{Result, anyhow};
use gix::ObjectId;
use gix::bstr::ByteSlice;
use gix::objs::Kind;

use crate::Repo;

static NEVER: AtomicBool = AtomicBool::new(false);

/// What a full check found.
#[derive(Debug, Default)]
pub struct FsckReport {
    /// Distinct objects read back and rehashed.
    pub objects: usize,
    /// Packs whose pack and index checksums were verified.
    pub packs: usize,
    /// Objects reachable from references, `HEAD` and the index.
    pub reachable: usize,
    /// Reachable objects that are absent because a partial clone left them on the remote.
    pub promised: usize,
    /// Stored objects that are corrupt: unreadable, hashing to another id, or unparseable.
    pub corrupt: Vec<String>,
    /// Packs or pack indexes failing their checksum.
    pub bad_packs: Vec<String>,
    /// Reachable objects that are missing (and not promised).
    pub missing: Vec<String>,
}

impl FsckReport {
    pub fn is_ok(&self) -> bool {
        self.corrupt.is_empty() && self.bad_packs.is_empty() && self.missing.is_empty()
    }
}

impl Repo {
    /// Verify every object, pack and reachable link in the repository.
    pub fn fsck(&self) -> Result<FsckReport> {
        let mut report = FsckReport::default();
        self.fsck_packs(&mut report);
        self.fsck_objects(&mut report)?;
        self.fsck_connectivity(&mut report)?;
        Ok(report)
    }

    fn fsck_packs(&self, report: &mut FsckReport) {
        use gix::odb::pack;
        let kind = self.git.object_hash();
        let dir = self.git.objects.store_ref().path().join("pack");
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        let mut indexes: Vec<_> =
            entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "idx")).collect();
        indexes.sort();
        for idx_path in indexes {
            let name = idx_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let pack_path = idx_path.with_extension("pack");
            let mut progress = gix::progress::Discard;
            let checked = (|| -> Result<()> {
                let index = pack::index::File::at(&idx_path, kind).map_err(|e| anyhow!("{e}"))?;
                index.verify_checksum(&mut progress, &NEVER).map_err(|e| anyhow!("index checksum: {e}"))?;
                let data = pack::data::File::at(&pack_path, kind).map_err(|e| anyhow!("{e}"))?;
                let actual = data.verify_checksum(&mut progress, &NEVER).map_err(|e| anyhow!("pack checksum: {e}"))?;
                if actual != index.pack_checksum() {
                    return Err(anyhow!("the index belongs to another pack"));
                }
                Ok(())
            })();
            match checked {
                Ok(()) => report.packs += 1,
                Err(err) => report.bad_packs.push(format!("{name}: {err}")),
            }
        }
    }

    fn fsck_objects(&self, report: &mut FsckReport) -> Result<()> {
        let kind = self.git.object_hash();
        let mut seen = HashSet::new();
        let mut buf = Vec::new();
        for id in self.git.objects.iter().map_err(|e| anyhow!("could not list objects: {e}"))? {
            let id = match id {
                Ok(id) => id,
                Err(err) => {
                    report.corrupt.push(format!("unlistable object: {err}"));
                    continue;
                }
            };
            if !seen.insert(id) {
                continue;
            }
            report.objects += 1;
            let problem = match gix::prelude::Find::try_find(&self.git.objects, &id, &mut buf) {
                Err(err) => Some(format!("unreadable: {err}")),
                Ok(None) => Some("listed but unreadable".to_owned()),
                Ok(Some(data)) => match gix::objs::compute_hash(kind, data.kind, data.data) {
                    Err(err) => Some(format!("could not hash: {err}")),
                    Ok(actual) if actual != id => Some(format!("content hashes to {actual}")),
                    Ok(_) => parse_problem(data.kind, data.data, kind),
                },
            };
            if let Some(problem) = problem {
                report.corrupt.push(format!("{id}: {problem}"));
            }
        }
        Ok(())
    }

    fn fsck_connectivity(&self, report: &mut FsckReport) -> Result<()> {
        let hash = self.git.object_hash();
        let partial = self.is_partial();
        let shallow: HashSet<ObjectId> =
            self.git.shallow_commits().ok().flatten().map(|c| c.iter().copied().collect()).unwrap_or_default();

        // (object, expected kind, who points to it)
        let mut todo: Vec<(ObjectId, Option<Kind>, String)> = Vec::new();
        for reference in self.git.references()?.all()? {
            let reference = reference.map_err(|e| anyhow!("could not read reference: {e}"))?;
            if let gix::refs::TargetRef::Object(id) = reference.target() {
                todo.push((id.to_owned(), None, reference.name().as_bstr().to_str_lossy().into_owned()));
            }
        }
        if let Some(head) = self.head_commit_id()? {
            todo.push((head, Some(Kind::Commit), "HEAD".into()));
        }
        if let Ok(index) = self.load_index() {
            for entry in index.entries() {
                if entry.mode == gix::index::entry::Mode::COMMIT
                    || entry.flags.contains(gix::index::entry::Flags::INTENT_TO_ADD)
                {
                    continue;
                }
                let path = entry.path(&index).to_str_lossy().into_owned();
                todo.push((entry.id, Some(Kind::Blob), format!("index entry {path}")));
            }
        }

        let mut seen = HashSet::new();
        let mut buf = Vec::new();
        while let Some((id, expected, from)) = todo.pop() {
            if !seen.insert(id) {
                continue;
            }
            let data = match gix::prelude::Find::try_find(&self.git.objects, &id, &mut buf) {
                Ok(Some(data)) => data,
                // Corruption was reported by the object pass; don't report it twice.
                Err(_) => continue,
                Ok(None) => {
                    // A partial clone leaves blobs (and, for tree filters, trees) on the remote.
                    if partial && !matches!(expected, Some(Kind::Commit | Kind::Tag)) {
                        report.promised += 1;
                    } else {
                        report.missing.push(format!("{id} (from {from})"));
                    }
                    continue;
                }
            };
            report.reachable += 1;
            if let Some(expected) = expected
                && expected != data.kind
                && !(expected == Kind::Commit && data.kind == Kind::Tag)
            {
                report.corrupt.push(format!("{id}: {from} expects a {expected}, found a {}", data.kind));
                continue;
            }
            match data.kind {
                Kind::Commit => {
                    let Ok(commit) = gix::objs::CommitRef::from_bytes(data.data, hash) else { continue };
                    let short = crate::short(&id);
                    todo.push((commit.tree(), Some(Kind::Tree), format!("commit {short}")));
                    if !shallow.contains(&id) {
                        for parent in commit.parents() {
                            todo.push((parent, Some(Kind::Commit), format!("commit {short}")));
                        }
                    }
                }
                Kind::Tree => {
                    let Ok(tree) = gix::objs::TreeRef::from_bytes(data.data, hash) else { continue };
                    let short = crate::short(&id);
                    for entry in tree.entries {
                        let kind = match entry.mode.kind() {
                            gix::objs::tree::EntryKind::Tree => Kind::Tree,
                            gix::objs::tree::EntryKind::Commit => continue, // submodule
                            _ => Kind::Blob,
                        };
                        todo.push((entry.oid.to_owned(), Some(kind), format!("tree {short}")));
                    }
                }
                Kind::Tag => {
                    let Ok(tag) = gix::objs::TagRef::from_bytes(data.data, hash) else { continue };
                    todo.push((tag.target(), Some(tag.target_kind), format!("tag {}", crate::short(&id))));
                }
                Kind::Blob => {}
            }
        }
        Ok(())
    }
}

/// Why `data` doesn't parse as an object of `kind`, if it doesn't.
fn parse_problem(kind: Kind, data: &[u8], hash: gix::hash::Kind) -> Option<String> {
    let parsed = match kind {
        Kind::Blob => return None,
        Kind::Tree => gix::objs::TreeRef::from_bytes(data, hash).map(|_| ()),
        Kind::Commit => gix::objs::CommitRef::from_bytes(data, hash).map(|_| ()),
        Kind::Tag => gix::objs::TagRef::from_bytes(data, hash).map(|_| ()),
    };
    parsed.err().map(|err| format!("malformed {kind}: {err}"))
}
