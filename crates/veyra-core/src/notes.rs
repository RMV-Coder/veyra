//! Git notes (`refs/notes/commits` by default): text attached to commits without changing them.
//! The layout is Git's: a commit history on the notes ref whose tree maps an object id (possibly
//! fanned out as `ab/cdef…`) to a blob.

use anyhow::{Context, Result};
use gix::ObjectId;
use gix::bstr::{BString, ByteSlice};
use gix::objs::tree::EntryKind;

use crate::Repo;
use crate::flat::{FlatEntry, FlatTree};

pub const DEFAULT_NOTES_REF: &str = "refs/notes/commits";

fn find(flat: &FlatTree, object: &ObjectId) -> Option<(BString, FlatEntry)> {
    let hex = object.to_string();
    flat.iter().find(|(p, _)| p.to_str_lossy().replace('/', "") == hex).map(|(p, e)| (p.clone(), *e))
}

impl Repo {
    /// The note attached to `object` on `notes_ref`.
    pub fn note(&self, notes_ref: &str, object: ObjectId) -> Result<Option<String>> {
        let Some(tip) = self.ref_target(notes_ref)? else { return Ok(None) };
        let flat = self.flat_from_commit(tip)?;
        match find(&flat, &object) {
            Some((_, e)) => Ok(Some(String::from_utf8_lossy(&self.read_blob(e.id)?).into_owned())),
            None => Ok(None),
        }
    }

    /// All notes on `notes_ref`: `(annotated object, note blob)`.
    pub fn notes(&self, notes_ref: &str) -> Result<Vec<(ObjectId, ObjectId)>> {
        let Some(tip) = self.ref_target(notes_ref)? else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        for (path, entry) in self.flat_from_commit(tip)? {
            let hex = path.to_str_lossy().replace('/', "");
            if let Ok(id) = ObjectId::from_hex(hex.as_bytes()) {
                out.push((id, entry.id));
            }
        }
        Ok(out)
    }

    /// Set (`Some`) or remove (`None`) the note on `object`, recording a notes commit like Git.
    pub fn set_note(&self, notes_ref: &str, object: ObjectId, text: Option<&str>, message: &str) -> Result<()> {
        let previous = self.ref_target(notes_ref)?;
        let mut flat = match previous {
            Some(p) => self.flat_from_commit(p)?,
            None => FlatTree::new(),
        };
        let existing = find(&flat, &object);
        match text {
            Some(text) => {
                let mut content = text.to_owned();
                if !content.ends_with('\n') {
                    content.push('\n');
                }
                let blob = self.write_blob(content.as_bytes())?;
                let path = existing.map(|(p, _)| p).unwrap_or_else(|| BString::from(object.to_string()));
                flat.insert(path, FlatEntry::new(EntryKind::Blob, blob));
            }
            None => {
                let (path, _) = existing.with_context(|| format!("no note for {}", crate::short(&object)))?;
                flat.remove(&path);
            }
        }
        let tree = self.write_flat(&flat)?;
        let signature = self.git.committer().context("no identity")??.to_owned()?;
        let commit = gix::objs::Commit {
            message: format!("{message}\n").as_str().into(),
            tree,
            author: signature.clone(),
            committer: signature,
            encoding: None,
            parents: previous.into_iter().collect(),
            extra_headers: Default::default(),
        };
        let id = self.write_commit_object(commit, false)?;
        self.set_ref(notes_ref, id, Some(previous), message)?;
        Ok(())
    }
}
