//! Reuse recorded resolutions (`git rerere`), stored in Git's own `.git/rr-cache` format so that
//! Git and Veyra share them.
//!
//! A conflict is identified by hashing its hunks with the two sides in a canonical order (so the
//! same conflict met from the other direction, e.g. during a rebase, has the same id). When a
//! conflict is written, a recorded resolution (`postimage`) is replayed if there is one; otherwise
//! the normalized conflict (`preimage`) is saved and listed in `.git/MERGE_RR`. When the operation
//! is concluded (`commit` of a merge, `cherry-pick/revert/rebase --continue`), the resolved file
//! becomes the `postimage`.
//!
//! Enabled like Git: `rerere.enabled=true`, or an existing `.git/rr-cache` directory.
//! `rerere.autoUpdate=true` also stages replayed resolutions.

use std::path::PathBuf;

use anyhow::{Context, Result};
use gix::bstr::{BStr, BString, ByteSlice};
use serde::Serialize;

use crate::Repo;
use crate::semantic::{AutoResolution, ResolutionMethod};

const MARKER_LEN: usize = 7;

/// A conflicted file in canonical form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Normalized {
    /// Hex id of the conflict (the `rr-cache` directory name).
    pub id: String,
    /// The file with conflict labels removed and the sides of each hunk in canonical order.
    pub preimage: Vec<u8>,
    pub hunks: usize,
}

fn marker(line: &[u8], c: u8) -> bool {
    line.len() >= MARKER_LEN
        && line[..MARKER_LEN].iter().all(|b| *b == c)
        && line.get(MARKER_LEN).is_none_or(|b| b.is_ascii_whitespace())
}

/// Normalize a file with conflict markers. `None` when it has no (well-formed) conflicts.
pub fn normalize(content: &[u8], hash: gix::hash::Kind) -> Option<Normalized> {
    enum State {
        Outside,
        Ours,
        Base,
        Theirs,
    }
    let mut state = State::Outside;
    let mut out = Vec::with_capacity(content.len());
    let (mut one, mut two) = (Vec::new(), Vec::new());
    let mut hasher = gix::hash::hasher(hash);
    let mut hunks = 0;
    for line in content.split_inclusive(|b| *b == b'\n') {
        match state {
            State::Outside if marker(line, b'<') => state = State::Ours,
            State::Outside => out.extend_from_slice(line),
            State::Ours | State::Base if marker(line, b'=') => state = State::Theirs,
            State::Ours if marker(line, b'|') => state = State::Base,
            State::Ours => one.extend_from_slice(line),
            State::Base => {}
            State::Theirs if marker(line, b'>') => {
                if one > two {
                    std::mem::swap(&mut one, &mut two);
                }
                hasher.update(&one);
                hasher.update(&[0]);
                hasher.update(&two);
                hasher.update(&[0]);
                out.extend_from_slice(b"<<<<<<<\n");
                out.extend_from_slice(&one);
                out.extend_from_slice(b"=======\n");
                out.extend_from_slice(&two);
                out.extend_from_slice(b">>>>>>>\n");
                one.clear();
                two.clear();
                hunks += 1;
                state = State::Outside;
            }
            State::Theirs if marker(line, b'<') || marker(line, b'=') => return None, // nested: don't guess
            State::Theirs => two.extend_from_slice(line),
        }
    }
    if hunks == 0 || !matches!(state, State::Outside) {
        return None;
    }
    let id = hasher.try_finalize().ok()?.to_hex().to_string();
    Some(Normalized { id, preimage: out, hunks })
}

/// One recorded conflict.
#[derive(Debug, Clone, Serialize)]
pub struct RecordedConflict {
    pub id: String,
    /// Whether a resolution has been recorded (otherwise only the conflict is known).
    pub resolved: bool,
    /// The path it was last seen at, if it is part of the current operation.
    pub path: Option<String>,
}

impl Repo {
    fn rr_cache(&self) -> PathBuf {
        self.common_dir().join("rr-cache")
    }

    fn merge_rr_path(&self) -> PathBuf {
        self.git_dir().join("MERGE_RR")
    }

    /// Is rerere on (`rerere.enabled`, or an existing `rr-cache` like Git)?
    pub fn rerere_enabled(&self) -> bool {
        self.config_bool("rerere.enabled").unwrap_or_else(|| self.rr_cache().is_dir())
    }

    /// The conflicts of the operation in progress: `(id, path)`.
    fn read_merge_rr(&self) -> Vec<(String, BString)> {
        let Ok(data) = std::fs::read(self.merge_rr_path()) else { return Vec::new() };
        data.split(|b| *b == 0)
            .filter_map(|rec| {
                let (id, path) = rec.split_once_str(b"\t")?;
                Some((id.to_str().ok()?.to_owned(), path.into()))
            })
            .collect()
    }

    fn write_merge_rr(&self, entries: &[(String, BString)]) -> Result<()> {
        if entries.is_empty() {
            return self.clear_merge_rr();
        }
        let mut data = Vec::new();
        for (id, path) in entries {
            data.extend_from_slice(id.as_bytes());
            data.push(b'\t');
            data.extend_from_slice(path);
            data.push(0);
        }
        std::fs::write(self.merge_rr_path(), data).context("could not write MERGE_RR")
    }

    pub(crate) fn clear_merge_rr(&self) -> Result<()> {
        match std::fs::remove_file(self.merge_rr_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn workdir_file(&self, path: &BStr) -> Result<PathBuf> {
        Ok(self.workdir()?.join(crate::repo::fs_path(path)?))
    }

    /// After conflicts were written to the worktree: replay recorded resolutions and remember
    /// new conflicts. Returns the paths whose replayed resolution was staged (`rerere.autoUpdate`),
    /// which are no longer conflicted.
    pub(crate) fn rerere_replay(&self, index: &mut gix::index::File, paths: &[BString]) -> Result<Vec<BString>> {
        if !self.rerere_enabled() {
            return Ok(Vec::new());
        }
        let auto_update = self.config_bool("rerere.autoUpdate").unwrap_or(false);
        let cache = self.rr_cache();
        let mut merge_rr = Vec::new();
        let mut staged = Vec::new();
        let mut notes = Vec::new();
        for path in paths {
            let file = self.workdir_file(path.as_ref())?;
            let Ok(content) = std::fs::read(&file) else { continue };
            let Some(this) = normalize(&content, self.object_hash()) else { continue };
            let dir = cache.join(&this.id);
            merge_rr.push((this.id.clone(), path.clone()));
            let post = std::fs::read(dir.join("postimage")).ok();
            let pre = std::fs::read(dir.join("preimage")).ok();
            let resolution = match (post, pre) {
                (Some(post), Some(pre)) if pre == this.preimage => Some(post),
                (Some(post), Some(pre)) => three_way(&pre, &post, &this.preimage),
                (Some(post), None) => Some(post),
                (None, _) => {
                    std::fs::create_dir_all(&dir)?;
                    std::fs::write(dir.join("preimage"), &this.preimage)?;
                    None
                }
            };
            let Some(resolved) = resolution else { continue };
            std::fs::write(&file, &resolved).with_context(|| format!("could not write {}", file.display()))?;
            if auto_update {
                let blob = self.read_worktree_as_git(path.as_ref())?.unwrap_or(resolved);
                let id = self.write_blob(&blob)?;
                let mode = index
                    .entries()
                    .iter()
                    .find(|e| e.path(index) == path.as_bstr() && e.stage_raw() == 2)
                    .map(|e| e.mode)
                    .unwrap_or(gix::index::entry::Mode::FILE);
                index.remove_entries(|_, p, _| p == path.as_bstr());
                index.dangerously_push_entry(
                    Default::default(),
                    id,
                    gix::index::entry::Flags::empty(),
                    mode,
                    path.as_ref(),
                );
                index.sort_entries();
                staged.push(path.clone());
            }
            notes.push(AutoResolution {
                path: path.to_str_lossy().into_owned(),
                method: ResolutionMethod::Replayed { staged: auto_update },
                notes: Vec::new(),
            });
        }
        self.write_merge_rr(&merge_rr)?;
        self.auto_resolutions.lock().unwrap_or_else(|e| e.into_inner()).extend(notes);
        Ok(staged)
    }

    /// The conflicted operation is being concluded: record how each remembered conflict was
    /// resolved. Returns the number of recorded resolutions.
    pub(crate) fn rerere_record(&self) -> Result<usize> {
        let entries = self.read_merge_rr();
        if entries.is_empty() || !self.rerere_enabled() {
            self.clear_merge_rr()?;
            return Ok(0);
        }
        let mut recorded = 0;
        let mut notes = Vec::new();
        for (id, path) in &entries {
            let dir = self.rr_cache().join(id);
            if !dir.join("preimage").exists() || dir.join("postimage").exists() {
                continue;
            }
            let Ok(content) = std::fs::read(self.workdir_file(path.as_ref())?) else { continue };
            if normalize(&content, self.object_hash()).is_some() {
                continue; // still has conflict markers
            }
            std::fs::write(dir.join("postimage"), &content)?;
            recorded += 1;
            notes.push(AutoResolution {
                path: path.to_str_lossy().into_owned(),
                method: ResolutionMethod::Recorded,
                notes: Vec::new(),
            });
        }
        self.clear_merge_rr()?;
        self.auto_resolutions.lock().unwrap_or_else(|e| e.into_inner()).extend(notes);
        Ok(recorded)
    }

    /// All recorded conflicts.
    pub fn rerere_list(&self) -> Result<Vec<RecordedConflict>> {
        let current = self.read_merge_rr();
        let mut out = Vec::new();
        let Ok(dir) = std::fs::read_dir(self.rr_cache()) else { return Ok(out) };
        for entry in dir {
            let entry = entry?;
            let id = entry.file_name().to_string_lossy().into_owned();
            if !entry.path().join("preimage").exists() {
                continue;
            }
            let path = current.iter().find(|(i, _)| *i == id).map(|(_, p)| p.to_str_lossy().into_owned());
            out.push(RecordedConflict { resolved: entry.path().join("postimage").exists(), id, path });
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    /// Forget the recorded resolution of the conflict currently at `path` (or with this id).
    /// The conflict is re-armed: the next resolution is recorded afresh.
    pub fn rerere_forget(&self, path_or_id: &str) -> Result<bool> {
        let id = match self.read_merge_rr().into_iter().find(|(_, p)| p == path_or_id) {
            Some((id, _)) => id,
            None => path_or_id.to_owned(),
        };
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(false);
        }
        let post = self.rr_cache().join(&id).join("postimage");
        if post.exists() {
            std::fs::remove_file(post)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Delete every recorded conflict and resolution.
    pub fn rerere_clear(&self) -> Result<usize> {
        let n = self.rerere_list()?.len();
        match std::fs::remove_dir_all(self.rr_cache()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        self.clear_merge_rr()?;
        Ok(n)
    }
}

/// Apply the change `pre → post` to `this` (a slightly different version of the same conflict).
fn three_way(pre: &[u8], post: &[u8], this: &[u8]) -> Option<Vec<u8>> {
    use gix::merge::blob::builtin_driver::text;
    let mut out = Vec::new();
    let mut input = gix::diff::blob::InternedInput::default();
    let resolution = gix::merge::blob::builtin_driver::text(
        &mut out,
        &mut input,
        text::Labels::default(),
        post,
        pre,
        this,
        text::Options::default(),
    );
    (resolution == gix::merge::blob::Resolution::Complete).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA1: gix::hash::Kind = gix::hash::Kind::Sha1;

    #[test]
    fn normalization_is_side_order_independent_and_drops_labels_and_base() {
        let a = b"x\n<<<<<<< ours\nB\n=======\nA\n>>>>>>> theirs\ny\n";
        let b = b"x\n<<<<<<< HEAD\nA\n||||||| base\nO\n=======\nB\n>>>>>>> topic\ny\n";
        let (na, nb) = (normalize(a, SHA1).unwrap(), normalize(b, SHA1).unwrap());
        assert_eq!(na.id, nb.id);
        assert_eq!(na.preimage, b"x\n<<<<<<<\nA\n=======\nB\n>>>>>>>\ny\n");
        assert_eq!(na.hunks, 1);
    }

    #[test]
    fn id_matches_git() {
        // Git: sha1("A\n\0B\n\0") for a single hunk with sides "A\n" and "B\n".
        let n = normalize(b"<<<<<<< a\nA\n=======\nB\n>>>>>>> b\n", SHA1).unwrap();
        let mut h = gix::hash::hasher(SHA1);
        h.update(b"A\n\0B\n\0");
        assert_eq!(n.id, h.try_finalize().unwrap().to_hex().to_string());
    }

    #[test]
    fn no_or_broken_conflicts() {
        assert!(normalize(b"plain\n", SHA1).is_none());
        assert!(normalize(b"<<<<<<< a\nA\n=======\n", SHA1).is_none());
        assert!(normalize(b"<<<<<<<< not a marker\n", SHA1).is_none());
    }

    #[test]
    fn three_way_replays_onto_a_shifted_conflict() {
        let pre = b"head\n<<<<<<<\nA\n=======\nB\n>>>>>>>\ntail\n";
        let post = b"head\nAB\ntail\n";
        let this = b"new head line\nhead\n<<<<<<<\nA\n=======\nB\n>>>>>>>\ntail\n";
        assert_eq!(three_way(pre, post, this).unwrap(), b"new head line\nhead\nAB\ntail\n");
    }
}

#[cfg(test)]
mod properties {
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn normalization_is_idempotent_and_side_order_free(
            before in "[a-z \n]{0,40}", ours in "[a-z]{1,10}\n", theirs in "[a-z]{1,10}\n", after in "[a-z \n]{0,40}",
        ) {
            let before = if before.is_empty() || before.ends_with('\n') { before } else { format!("{before}\n") };
            let file = format!("{before}<<<<<<< a\n{ours}=======\n{theirs}>>>>>>> b\n{after}");
            let swapped = format!("{before}<<<<<<< x\n{theirs}=======\n{ours}>>>>>>> y\n{after}");
            let kind = gix::hash::Kind::Sha1;
            let n = super::normalize(file.as_bytes(), kind).unwrap();
            prop_assert_eq!(&super::normalize(swapped.as_bytes(), kind).unwrap().id, &n.id);
            let again = super::normalize(&n.preimage, kind).unwrap();
            prop_assert_eq!(again.preimage, n.preimage.clone());
            prop_assert_eq!(again.id, n.id);
        }

        #[test]
        fn arbitrary_text_never_panics(text in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let _ = super::normalize(&text, gix::hash::Kind::Sha1);
        }
    }
}
