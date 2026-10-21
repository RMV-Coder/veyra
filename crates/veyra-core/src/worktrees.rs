//! Linked worktrees (`git worktree`), via gitoxide's implementation of Git's layout:
//! `<common-dir>/worktrees/<id>/{HEAD,commondir,gitdir,index}` plus a `.git` file in the
//! worktree pointing back. Each linked worktree has its own Veyra journal (see `veyra_dir`).

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, anyhow, bail};
use gix::ObjectId;
use gix::bstr::ByteSlice;

use crate::{HeadState, Repo};

static NEVER: AtomicBool = AtomicBool::new(false);

/// One worktree, main or linked.
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    /// `None` for the main worktree.
    pub id: Option<String>,
    pub branch: Option<String>,
    pub head: Option<ObjectId>,
    pub locked: bool,
    /// The worktree directory is gone; `worktree prune` will remove its bookkeeping.
    pub prunable: bool,
}

/// What to check out in a new worktree.
#[derive(Debug, Clone)]
pub enum NewWorktreeHead {
    /// An existing branch (`refs/heads/<name>`).
    Branch(String),
    /// Create this branch at `start` first.
    NewBranch { name: String, start: ObjectId },
    /// Detach at a commit.
    Detached(ObjectId),
}

fn info_for(repo: &gix::Repository, path: PathBuf, id: Option<String>, locked: bool) -> WorktreeInfo {
    let wrapped = Repo::from_gix(repo.clone());
    let (branch, head) = match wrapped.head_state() {
        Ok(HeadState::Branch { name, target }) => (Some(crate::refs::shorten(&name.to_str_lossy()).to_owned()), target),
        Ok(HeadState::Detached(id)) => (None, Some(id)),
        Err(_) => (None, None),
    };
    WorktreeInfo { path, id, branch, head, locked, prunable: false }
}

impl Repo {
    /// The repository of the main worktree (this repository, when it is the main one).
    fn main_gix(&self) -> Result<gix::Repository> {
        Ok(self.git.main_repo()?)
    }

    /// All worktrees: the main one first, then linked ones.
    pub fn worktree_list(&self) -> Result<Vec<WorktreeInfo>> {
        let main = self.main_gix()?;
        let mut out = Vec::new();
        if let Some(w) = main.workdir() {
            out.push(info_for(&main, w.to_owned(), None, false));
        }
        for proxy in main.worktrees()? {
            let id = proxy.id().map(|i| i.to_str_lossy().into_owned()).ok();
            let locked = proxy.is_locked();
            let prunable = proxy.is_prunable();
            let path = proxy.base().unwrap_or_default();
            if prunable {
                out.push(WorktreeInfo { path, id, branch: None, head: None, locked, prunable: true });
                continue;
            }
            match proxy.into_repo_with_possibly_inaccessible_worktree() {
                Ok(repo) => out.push(info_for(&repo, path, id, locked)),
                Err(_) => out.push(WorktreeInfo { path, id, branch: None, head: None, locked, prunable: true }),
            }
        }
        Ok(out)
    }

    /// Create a linked worktree at `dest` and check `head` out there.
    pub fn worktree_add(&self, dest: &Path, head: NewWorktreeHead) -> Result<PathBuf> {
        let main = self.main_gix()?;
        let head = match head {
            NewWorktreeHead::Branch(name) => {
                let full = crate::refs::branch_full_name(&name)?;
                if self.ref_target(&full)?.is_none() {
                    bail!("branch '{name}' does not exist\n  hint: use -b {name} to create it");
                }
                gix::worktree::add::Head::Attached(full.as_str().try_into()?)
            }
            NewWorktreeHead::NewBranch { name, start } => {
                let full = crate::refs::branch_full_name(&name)?;
                if self.ref_target(&full)?.is_some() {
                    bail!("a branch named '{name}' already exists");
                }
                self.set_ref(&full, start, Some(None), &format!("branch: Created for worktree {}", dest.display()))?;
                gix::worktree::add::Head::Attached(full.as_str().try_into()?)
            }
            NewWorktreeHead::Detached(id) => gix::worktree::add::Head::Detached(id),
        };
        let (repo, _) = main
            .add_worktree(dest, head, gix::progress::Discard, &NEVER)
            .map_err(|e| anyhow!("could not add the worktree: {e}"))?;
        Ok(repo.workdir().map(Path::to_path_buf).unwrap_or_else(|| dest.to_owned()))
    }

    /// Remove a linked worktree (refusing if it has changes, unless `force`).
    pub fn worktree_remove(&self, target: &Path, force: bool) -> Result<()> {
        let main = self.main_gix()?;
        let force =
            if force { gix::worktree::remove::Force::DiscardChanges } else { gix::worktree::remove::Force::Never };
        main.remove_worktree(target, force, gix::progress::Discard)
            .map_err(|e| anyhow!("could not remove the worktree: {e}\n  hint: --force discards its local changes"))?;
        Ok(())
    }

    /// Remove bookkeeping of worktrees whose directories no longer exist. Returns their ids.
    pub fn worktree_prune(&self) -> Result<Vec<String>> {
        let main = self.main_gix()?;
        let mut pruned = Vec::new();
        for proxy in main.worktrees()? {
            if proxy.is_prunable() && !proxy.is_locked() {
                let id = proxy.id().map(|i| i.to_str_lossy().into_owned()).unwrap_or_default();
                let dir = proxy.git_dir().to_owned();
                std::fs::remove_dir_all(&dir).with_context(|| format!("could not remove {}", dir.display()))?;
                pruned.push(id);
            }
        }
        Ok(pruned)
    }
}
