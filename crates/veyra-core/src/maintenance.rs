//! Repository maintenance without `git gc`: storage statistics, packing loose objects, and
//! pruning unreachable loose objects (with Git's two-week grace period).
//!
//! Safety rules:
//! * Packs are never rewritten or deleted.
//! * A loose object is deleted only after a pack containing it was written *and indexed*,
//!   or when it is unreachable from every ref, reflog entry, the index, in-progress state and
//!   Veyra's journal, *and* older than the grace period.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, anyhow};
use gix::ObjectId;

use crate::Repo;

/// `veyra storage stats` / `git count-objects -v`.
#[derive(Debug, Clone, Default)]
pub struct StorageStats {
    pub loose_objects: usize,
    pub loose_bytes: u64,
    pub packs: usize,
    pub pack_bytes: u64,
    /// Loose objects that are also present in a pack (safe to delete).
    pub loose_also_packed: usize,
}

/// What a maintenance run did.
#[derive(Debug, Clone, Default)]
pub struct MaintenanceReport {
    pub packed: usize,
    pub pack_bytes: u64,
    pub loose_removed: usize,
    pub pruned: usize,
    pub ops_pruned: usize,
}

static NEVER: AtomicBool = AtomicBool::new(false);

fn objects_dir(repo: &Repo) -> PathBuf {
    repo.common_dir().join("objects")
}

/// Every loose object as `(id, path, size, modified)`.
fn loose_objects(repo: &Repo) -> Result<Vec<(ObjectId, PathBuf, u64, std::time::SystemTime)>> {
    let mut out = Vec::new();
    let Ok(dirs) = std::fs::read_dir(objects_dir(repo)) else { return Ok(out) };
    for dir in dirs.flatten() {
        let name = dir.file_name().to_string_lossy().into_owned();
        if name.len() != 2 || !name.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        for file in std::fs::read_dir(dir.path())?.flatten() {
            let rest = file.file_name().to_string_lossy().into_owned();
            let Ok(id) = ObjectId::from_hex(format!("{name}{rest}").as_bytes()) else { continue };
            let md = file.metadata()?;
            out.push((id, file.path(), md.len(), md.modified().unwrap_or(std::time::UNIX_EPOCH)));
        }
    }
    Ok(out)
}

impl Repo {
    pub fn storage_stats(&self) -> Result<StorageStats> {
        let loose = loose_objects(self)?;
        let packed = self.packed_object_ids()?;
        let pack_dir = objects_dir(self).join("pack");
        let (mut packs, mut pack_bytes) = (0, 0);
        if let Ok(entries) = std::fs::read_dir(&pack_dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "pack" || x == "idx") {
                    pack_bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
                    if p.extension().is_some_and(|x| x == "pack") {
                        packs += 1;
                    }
                }
            }
        }
        Ok(StorageStats {
            loose_objects: loose.len(),
            loose_bytes: loose.iter().map(|(_, _, s, _)| s).sum(),
            packs,
            pack_bytes,
            loose_also_packed: loose.iter().filter(|(id, ..)| packed.contains(id)).count(),
        })
    }

    /// Ids of every object in every pack index.
    fn packed_object_ids(&self) -> Result<HashSet<ObjectId>> {
        let mut out = HashSet::new();
        let Ok(entries) = std::fs::read_dir(objects_dir(self).join("pack")) else { return Ok(out) };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "idx") {
                let index = gix::odb::pack::index::File::at(&p, self.object_hash())
                    .map_err(|e| anyhow!("could not read pack index {}: {e}", p.display()))?;
                out.extend(index.iter().map(|entry| entry.oid));
            }
        }
        Ok(out)
    }

    /// Pack all loose objects into one new pack, then delete the loose copies.
    pub fn pack_loose_objects(&self) -> Result<(usize, u64)> {
        let loose = loose_objects(self)?;
        let packed = self.packed_object_ids()?;
        let to_pack: Vec<ObjectId> = loose.iter().map(|(id, ..)| *id).filter(|id| !packed.contains(id)).collect();
        if !to_pack.is_empty() {
            let mut buf = Vec::new();
            crate::push::write_pack(self, &to_pack, &mut buf)?;
            let pack_dir = objects_dir(self).join("pack");
            std::fs::create_dir_all(&pack_dir)?;
            let outcome = gix::odb::pack::Bundle::write_to_directory(
                &mut std::io::BufReader::new(buf.as_slice()),
                Some(&pack_dir),
                &mut gix::progress::Discard,
                &NEVER,
                None::<gix::odb::Handle>,
                self.object_hash(),
                Default::default(),
            )
            .map_err(|e| anyhow!("could not index the new pack: {e}"))?;
            if let Some(keep) = &outcome.keep_path {
                let _ = std::fs::remove_file(keep);
            }
        }
        // Only now, with the pack indexed, remove loose copies of anything packed.
        let packed = self.packed_object_ids()?;
        let mut removed = 0;
        for (id, path, ..) in &loose {
            if packed.contains(id) && std::fs::remove_file(path).is_ok() {
                removed += 1;
                if let Some(dir) = path.parent() {
                    let _ = std::fs::remove_dir(dir);
                }
            }
        }
        let bytes = loose.iter().filter(|(id, ..)| to_pack.contains(id)).map(|(_, _, s, _)| s).sum();
        Ok((removed, bytes))
    }

    /// Every object reachable from refs, reflogs, HEAD, the index and in-progress state.
    pub fn reachable_objects(&self) -> Result<HashSet<ObjectId>> {
        let mut roots: Vec<ObjectId> = Vec::new();
        // All refs, including refs/veyra/keep and refs/stash.
        for reference in self.git.references()?.all()? {
            let reference = reference.map_err(|e| anyhow!("{e}"))?;
            if let gix::refs::TargetRef::Object(id) = reference.target() {
                roots.push(id.to_owned());
            }
        }
        if let Some(h) = self.head_commit_id()? {
            roots.push(h);
        }
        // Reflog entries (old and new values).
        let logs = self.common_dir().join("logs");
        let mut stack = vec![logs];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if let Ok(text) = std::fs::read_to_string(&p) {
                    for line in text.lines() {
                        for hex in line.split(' ').take(2) {
                            if let Ok(id) = ObjectId::from_hex(hex.as_bytes())
                                && !id.is_null()
                            {
                                roots.push(id);
                            }
                        }
                    }
                }
            }
        }
        // In-progress state files.
        for name in ["ORIG_HEAD", "MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD", "FETCH_HEAD"] {
            if let Ok(text) = std::fs::read_to_string(self.git_dir().join(name)) {
                for word in text.split_whitespace() {
                    if let Ok(id) = ObjectId::from_hex(word.as_bytes()) {
                        roots.push(id);
                    }
                }
            }
        }

        let mut seen: HashSet<ObjectId> = HashSet::new();
        // The index's blobs.
        for entry in self.load_index()?.entries() {
            seen.insert(entry.id);
        }
        let mut todo = roots;
        while let Some(id) = todo.pop() {
            if !seen.insert(id) {
                continue;
            }
            let Ok(object) = self.git.find_object(id) else { continue };
            match object.kind {
                gix::objs::Kind::Commit => {
                    let commit = object.into_commit();
                    let decoded = commit.decode()?;
                    todo.push(decoded.tree());
                    todo.extend(decoded.parents());
                }
                gix::objs::Kind::Tree => {
                    let tree = object.into_tree();
                    for e in tree.decode()?.entries {
                        if !e.mode.is_commit() {
                            todo.push(e.oid.to_owned());
                        }
                    }
                }
                gix::objs::Kind::Tag => {
                    todo.push(object.into_tag().target_id()?.detach());
                }
                gix::objs::Kind::Blob => {}
            }
        }
        Ok(seen)
    }

    /// Delete unreachable loose objects older than `grace` seconds. Returns how many.
    pub fn prune_loose(&self, grace: i64, dry_run: bool) -> Result<Vec<ObjectId>> {
        let reachable = self.reachable_objects()?;
        let cutoff = std::time::SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(grace.max(0) as u64))
            .unwrap_or(std::time::UNIX_EPOCH);
        let mut pruned = Vec::new();
        for (id, path, _, modified) in loose_objects(self)? {
            if reachable.contains(&id) || modified > cutoff {
                continue;
            }
            if !dry_run {
                std::fs::remove_file(&path).with_context(|| format!("could not remove {}", path.display()))?;
                if let Some(dir) = path.parent() {
                    let _ = std::fs::remove_dir(dir);
                }
            }
            pruned.push(id);
        }
        Ok(pruned)
    }
}
