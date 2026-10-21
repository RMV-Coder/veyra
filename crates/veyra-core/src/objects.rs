//! Bulk object writing. Writing thousands of small loose objects is slow on some filesystems
//! (Windows with antivirus scanning especially: several milliseconds per file), so snapshots and
//! large `add`s collect new blobs in memory and write them as a single pack instead.

use std::collections::HashSet;
use std::io::Write;
use std::sync::atomic::AtomicBool;

use anyhow::{Result, anyhow};
use gix::ObjectId;

use crate::Repo;

/// Batches up to this many new objects stay loose (a pack isn't worth it).
const PACK_THRESHOLD: usize = 64;
/// Flush to a pack when pending data exceeds this many bytes.
const FLUSH_BYTES: usize = 64 * 1024 * 1024;

static NEVER: AtomicBool = AtomicBool::new(false);

/// Collects new blobs and writes them efficiently.
pub struct BlobBatch<'r> {
    repo: &'r Repo,
    /// An object-store handle that never rescans for new packs: every new blob is a lookup
    /// miss, and the default handle would rescan the objects directory on each one.
    odb: gix::OdbHandle,
    pending: Vec<(ObjectId, Vec<u8>)>,
    pending_bytes: usize,
    seen: HashSet<ObjectId>,
}

impl<'r> BlobBatch<'r> {
    pub fn new(repo: &'r Repo) -> Self {
        // One deliberate miss with the default handle makes the shared store pick up packs
        // written since it last looked (e.g. by the snapshot just before an `add`); after that
        // this handle never rescans.
        let _ = repo.git.has_object(ObjectId::null(repo.object_hash()));
        let mut odb = repo.git.objects.clone();
        odb.refresh_never();
        BlobBatch { repo, odb, pending: Vec::new(), pending_bytes: 0, seen: HashSet::new() }
    }

    /// Hash `data` as a blob and schedule it for writing unless it already exists.
    pub fn add(&mut self, data: Vec<u8>) -> Result<ObjectId> {
        let id = gix::objs::compute_hash(self.repo.object_hash(), gix::objs::Kind::Blob, &data)?;
        if self.seen.insert(id) && !gix::objs::Exists::exists(&self.odb, &id) {
            self.pending_bytes += data.len();
            self.pending.push((id, data));
            if self.pending_bytes > FLUSH_BYTES {
                self.flush()?;
            }
        }
        Ok(id)
    }

    /// Write everything pending: loose objects for small batches, one pack otherwise.
    pub fn flush(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending);
        self.pending_bytes = 0;
        if pending.len() <= PACK_THRESHOLD {
            for (_, data) in &pending {
                self.repo.write_blob(data)?;
            }
            return Ok(());
        }
        write_blobs_as_pack(self.repo, &pending)
    }

    /// Flush and finish.
    pub fn finish(mut self) -> Result<()> {
        self.flush()
    }
}

impl Drop for BlobBatch<'_> {
    fn drop(&mut self) {
        // Best effort: never lose objects that were handed out ids.
        let _ = self.flush();
    }
}

/// Write `blobs` (already hashed) into a new, indexed pack.
fn write_blobs_as_pack(repo: &Repo, blobs: &[(ObjectId, Vec<u8>)]) -> Result<()> {
    let mut pack = Vec::with_capacity(blobs.iter().map(|(_, d)| d.len() / 2 + 32).sum::<usize>() + 32);
    let mut hasher = gix::hash::hasher(repo.object_hash());
    let mut emit = |pack: &mut Vec<u8>, bytes: &[u8]| {
        hasher.update(bytes);
        pack.extend_from_slice(bytes);
    };
    emit(&mut pack, b"PACK");
    emit(&mut pack, &2u32.to_be_bytes());
    emit(&mut pack, &(blobs.len() as u32).to_be_bytes());
    let level = repo.git.pack_compression().unwrap_or_default();
    for (_, data) in blobs {
        let mut size = data.len() as u64;
        let mut header = Vec::with_capacity(10);
        let mut byte = (3u8 << 4) | (size & 0x0f) as u8;
        size >>= 4;
        while size > 0 {
            header.push(byte | 0x80);
            byte = (size & 0x7f) as u8;
            size >>= 7;
        }
        header.push(byte);
        emit(&mut pack, &header);
        let mut compressed = Vec::with_capacity(data.len() / 2 + 16);
        {
            let mut deflate = gix::zlib::stream::deflate::Write::new(&mut compressed, level);
            deflate.write_all(data)?;
            deflate.flush()?;
        }
        emit(&mut pack, &compressed);
    }
    let digest = hasher.try_finalize()?;
    pack.extend_from_slice(digest.as_bytes());

    let pack_dir = repo.common_dir().join("objects").join("pack");
    std::fs::create_dir_all(&pack_dir)?;
    let outcome = gix::odb::pack::Bundle::write_to_directory(
        &mut std::io::BufReader::new(pack.as_slice()),
        Some(&pack_dir),
        &mut gix::progress::Discard,
        &NEVER,
        None::<gix::odb::Handle>,
        repo.object_hash(),
        Default::default(),
    )
    .map_err(|e| anyhow!("could not write a pack of new objects: {e}"))?;
    if let Some(keep) = &outcome.keep_path {
        let _ = std::fs::remove_file(keep);
    }
    Ok(())
}
