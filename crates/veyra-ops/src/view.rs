//! Views: complete descriptions of repository state at one moment.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use veyra_core::gix::bstr::ByteSlice;
use veyra_core::worktree::SnapshotOptions;
use veyra_core::{FlatTree, HeadState, ObjectId, Repo};

/// Where HEAD pointed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HeadView {
    /// Symbolic ref to a (possibly unborn) branch, e.g. `refs/heads/main`.
    Symbolic(String),
    /// Detached at a commit (hex id).
    Detached(String),
}

/// A complete, restorable description of repository state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct View {
    pub head: HeadView,
    /// Every ref except `refs/veyra/*`, as hex ids.
    pub refs: BTreeMap<String, String>,
    /// Stage-0 index entries written as a tree.
    pub index_tree: String,
    /// Raw `.git/index` bytes, stored only when the index has conflict stages (which a tree can't hold).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_blob: Option<String>,
    /// Commit being merged (`MERGE_HEAD`), if a merge was in progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_head: Option<String>,
    /// Snapshot of tracked + untracked, non-ignored files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    /// Files that were too large to snapshot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
}

impl View {
    /// Do two views describe the same refs, HEAD, index and merge state (ignoring the worktree)?
    pub fn same_repository_state(&self, other: &View) -> bool {
        self.head == other.head
            && self.refs == other.refs
            && self.index_tree == other.index_tree
            && self.index_blob == other.index_blob
            && self.merge_head == other.merge_head
    }

    /// The commit HEAD resolves to in this view.
    pub fn head_commit(&self) -> Option<ObjectId> {
        match &self.head {
            HeadView::Detached(hex) => parse_id(hex).ok(),
            HeadView::Symbolic(name) => self.refs.get(name).and_then(|hex| parse_id(hex).ok()),
        }
    }

    pub fn worktree_id(&self) -> Option<ObjectId> {
        self.worktree.as_deref().and_then(|h| parse_id(h).ok())
    }

    pub fn index_tree_id(&self) -> Result<ObjectId> {
        parse_id(&self.index_tree)
    }

    /// A short description of where HEAD is.
    pub fn head_label(&self) -> String {
        match &self.head {
            HeadView::Symbolic(name) => veyra_core::refs::shorten(name).to_owned(),
            HeadView::Detached(hex) => format!("detached at {}", &hex[..hex.len().min(7)]),
        }
    }
}

pub fn parse_id(hex: &str) -> Result<ObjectId> {
    ObjectId::from_hex(hex.as_bytes()).with_context(|| format!("invalid object id '{hex}' in the operation log"))
}

/// What capturing a view produced, beyond the view itself.
pub struct Captured {
    pub view: View,
    /// The worktree as a flat tree, when a snapshot was taken.
    pub worktree_flat: Option<FlatTree>,
    /// Paths that differ between the index and the worktree.
    pub worktree_changes: usize,
}

/// Capture the repository's current state. With `snapshot`, the worktree is snapshotted too.
pub fn capture(repo: &Repo, snapshot: Option<&SnapshotOptions>) -> Result<Captured> {
    let head = match repo.head_state()? {
        HeadState::Branch { name, .. } => HeadView::Symbolic(name.to_str_lossy().into_owned()),
        HeadState::Detached(id) => HeadView::Detached(id.to_string()),
    };
    let refs = repo.all_refs()?.into_iter().map(|(k, v)| (k, v.to_string())).collect();
    let index = repo.load_index()?;
    let has_conflicts = index.entries().iter().any(|e| e.stage_raw() != 0);
    let index_tree = repo.write_flat(&veyra_core::flat::flat_from_index_state(&index))?.to_string();
    let index_blob = if has_conflicts {
        let bytes = std::fs::read(repo.index_path()).context("could not read the index file")?;
        Some(repo.write_blob(&bytes)?.to_string())
    } else {
        None
    };
    let merge_head = repo.merge_head()?.map(|id| id.to_string());

    let (worktree, worktree_flat, skipped, changes) = match snapshot {
        Some(opts) if repo.git().workdir().is_some() => {
            let snap = repo.snapshot(opts)?;
            (
                Some(snap.tree.to_string()),
                Some(snap.flat),
                snap.skipped.iter().map(|p| p.to_str_lossy().into_owned()).collect(),
                snap.changed_vs_index,
            )
        }
        _ => (None, None, Vec::new(), 0),
    };
    Ok(Captured {
        view: View { head, refs, index_tree, index_blob, merge_head, worktree, skipped },
        worktree_flat,
        worktree_changes: changes,
    })
}
