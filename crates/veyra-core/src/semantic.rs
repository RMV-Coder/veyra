//! Syntax-aware features built on `veyra-semantic`: semantic diffs of changes, and structural
//! resolution of merge conflicts.
//!
//! Structural resolution is deliberately conservative. A conflicted file is resolved only when
//! base, ours and theirs all parse without errors, the two sides changed *different* top-level
//! symbols, and the merged result parses again with exactly the expected symbols. Everything else
//! keeps Git's conflict markers. Every resolution is reported, and `veyra.structuralMerge=false`
//! turns the feature off.

use std::collections::BTreeSet;

use anyhow::Result;
use gix::ObjectId;
use gix::bstr::{BString, ByteSlice};
use gix::objs::tree::EntryKind;
use serde::Serialize;

use crate::Repo;
use crate::flat::{FlatEntry, PathChange};

/// The semantic view of one changed file.
#[derive(Debug, Clone)]
pub struct SemanticFileDiff {
    pub path: BString,
    pub status: char,
    /// `None` when the language isn't supported or the file is binary.
    pub diff: Option<veyra_semantic::FileSemanticDiff>,
}

/// How Veyra resolved (or remembered) a conflict without the user editing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionMethod {
    /// Merged symbol by symbol (the sides changed different symbols).
    Structural,
    /// A resolution recorded earlier was replayed (`rerere`); `staged` with `rerere.autoUpdate`.
    Replayed { staged: bool },
    /// The user's resolution was recorded for next time.
    Recorded,
}

/// A conflict Veyra handled automatically. Always reported to the user.
#[derive(Debug, Clone, Serialize)]
pub struct AutoResolution {
    pub path: String,
    pub method: ResolutionMethod,
    /// Details, e.g. which symbol came from which side.
    pub notes: Vec<String>,
}

/// `veyra.structuralMerge`: `true` (default) resolves and continues, `stage` resolves but stops
/// for review (the resolved files are staged), `false` keeps Git's behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructuralMode {
    Off,
    Stage,
    Commit,
}

/// The trailer recording, in the resulting commit, each file Veyra merged structurally.
pub const STRUCTURAL_TRAILER: &str = "Veyra-Structural-Merge";

/// What the structural pass achieved for a conflicted merge.
pub(crate) enum StructuralPass {
    /// Every conflict was resolved: the clean merged tree, and trailers for the commit message.
    Clean(ObjectId, Vec<(String, String)>),
    /// Some (possibly zero) files were resolved; the rest keep their conflicts.
    Partial(Vec<(BString, FlatEntry)>),
}

fn is_text_blob(e: &FlatEntry) -> bool {
    matches!(e.kind, EntryKind::Blob | EntryKind::BlobExecutable)
}

impl Repo {
    /// Semantic diffs for a list of changes (the new side read from objects).
    pub fn semantic_diff_changes(&self, changes: &[PathChange]) -> Result<Vec<SemanticFileDiff>> {
        let mut out = Vec::with_capacity(changes.len());
        for change in changes {
            let path = change.path.to_str_lossy().into_owned();
            let supported = veyra_semantic::Lang::from_path(&path).is_some()
                && change.old.is_none_or(|e| is_text_blob(&e))
                && change.new.is_none_or(|e| is_text_blob(&e));
            let diff = if supported {
                let old = self.side_bytes(change.old, &change.path, false)?;
                let new = self.side_bytes(change.new, &change.path, false)?;
                match (std::str::from_utf8(&old), std::str::from_utf8(&new)) {
                    (Ok(o), Ok(n)) => veyra_semantic::semantic_diff(&path, o, n),
                    _ => None,
                }
            } else {
                None
            };
            out.push(SemanticFileDiff { path: change.path.clone(), status: change.status_letter(), diff });
        }
        Ok(out)
    }

    /// How conflicts are resolved structurally (`veyra.structuralMerge`).
    pub fn structural_mode(&self) -> StructuralMode {
        match self.config_get("veyra.structuralMerge").map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("false" | "off" | "no" | "0") => StructuralMode::Off,
            Some("stage" | "review") => StructuralMode::Stage,
            _ => StructuralMode::Commit,
        }
    }

    /// Is structural conflict resolution enabled at all?
    pub fn structural_merge_enabled(&self) -> bool {
        self.structural_mode() != StructuralMode::Off
    }

    /// The files that merges, cherry-picks, reverts, rebases and stash applications on this
    /// handle resolved structurally since the last call.
    pub fn take_auto_resolutions(&self) -> Vec<AutoResolution> {
        std::mem::take(&mut *self.auto_resolutions.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Try to resolve the unresolved conflicts of `outcome` symbol by symbol, given the trees
    /// it merged (`base`, `ours`, `theirs`) and the tree it produced (with conflict markers).
    pub(crate) fn structural_pass(
        &self,
        outcome: &gix::merge::tree::Outcome<'_>,
        base: ObjectId,
        ours: ObjectId,
        theirs: ObjectId,
        merged: ObjectId,
    ) -> Result<StructuralPass> {
        if !self.structural_merge_enabled() {
            return Ok(StructuralPass::Partial(Vec::new()));
        }
        let how = gix::merge::tree::TreatAsUnresolved::git();
        let mut unresolved = BTreeSet::new();
        for c in outcome.conflicts.iter().filter(|c| c.is_unresolved(how)) {
            unresolved.insert(c.ours.location().to_owned());
            unresolved.insert(c.theirs.location().to_owned());
        }
        let (bf, of, tf) = (self.flat_from_tree(base)?, self.flat_from_tree(ours)?, self.flat_from_tree(theirs)?);
        let mut resolved = Vec::new();
        let mut notes = Vec::new();
        for path in &unresolved {
            // Only plain content conflicts: the file exists with the same mode in all three.
            let (Some(b), Some(o), Some(t)) = (bf.get(path), of.get(path), tf.get(path)) else { continue };
            if !is_text_blob(b) || b.kind != o.kind || o.kind != t.kind {
                continue;
            }
            let (bb, ob, tb) = (self.read_blob(b.id)?, self.read_blob(o.id)?, self.read_blob(t.id)?);
            let (Ok(bs), Ok(os), Ok(ts)) =
                (std::str::from_utf8(&bb), std::str::from_utf8(&ob), std::str::from_utf8(&tb))
            else {
                continue;
            };
            let name = path.to_str_lossy().into_owned();
            if let Ok(m) = veyra_semantic::structural_merge(&name, bs, os, ts) {
                let id = self.write_blob(m.text.as_bytes())?;
                resolved.push((path.clone(), FlatEntry::new(o.kind, id)));
                notes.push(AutoResolution { path: name, method: ResolutionMethod::Structural, notes: m.notes });
            }
        }
        self.auto_resolutions.lock().unwrap_or_else(|e| e.into_inner()).extend(notes);
        if !resolved.is_empty()
            && resolved.len() == unresolved.len()
            && self.structural_mode() == StructuralMode::Commit
        {
            let trailers =
                resolved.iter().map(|(p, _)| (STRUCTURAL_TRAILER.to_owned(), p.to_str_lossy().into_owned())).collect();
            let mut flat = self.flat_from_tree(merged)?;
            for (path, entry) in resolved {
                flat.insert(path, entry);
            }
            return Ok(StructuralPass::Clean(self.write_flat(&flat)?, trailers));
        }
        Ok(StructuralPass::Partial(resolved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_by_config() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path(), Some("main")).unwrap();
        assert!(repo.structural_merge_enabled());
        crate::config::set_in_file(&repo.git_dir().join("config"), "veyra.structuralMerge", "false").unwrap();
        let repo = Repo::open(dir.path()).unwrap();
        assert!(!repo.structural_merge_enabled());
    }
}
