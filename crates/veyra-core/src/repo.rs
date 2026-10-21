//! Opening, initialising and inspecting repositories.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use gix::ObjectId;
use gix::bstr::{BString, ByteSlice};

/// A Git repository opened through gitoxide.
pub struct Repo {
    pub(crate) git: gix::Repository,
    /// Conflicts resolved automatically (structurally or from recorded resolutions) since the
    /// last `take_auto_resolutions` call.
    pub(crate) auto_resolutions: std::sync::Mutex<Vec<crate::semantic::AutoResolution>>,
}

/// Where `HEAD` points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadState {
    /// `HEAD` is a symbolic reference to a branch, which may not exist yet (unborn).
    Branch { name: BString, target: Option<ObjectId> },
    /// `HEAD` points directly at a commit.
    Detached(ObjectId),
}

impl HeadState {
    /// The commit `HEAD` resolves to, if any.
    pub fn commit(&self) -> Option<ObjectId> {
        match self {
            HeadState::Branch { target, .. } => *target,
            HeadState::Detached(id) => Some(*id),
        }
    }

    /// The short branch name (`main` for `refs/heads/main`), if on a branch.
    pub fn branch_short(&self) -> Option<&str> {
        match self {
            HeadState::Branch { name, .. } => name.to_str().ok().map(|n| n.strip_prefix("refs/heads/").unwrap_or(n)),
            HeadState::Detached(_) => None,
        }
    }
}

impl Repo {
    /// Find the repository containing `start` (or one of its parents).
    pub fn discover(start: &Path) -> Result<Self> {
        let git = gix::discover(start).map_err(|err| {
            anyhow!("not a git repository (or any of the parent directories): {}\n  cause: {err}", start.display())
        })?;
        Ok(Repo { git, auto_resolutions: Default::default() })
    }

    /// Open the repository at exactly `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let git = gix::open(path).with_context(|| format!("could not open repository at {}", path.display()))?;
        Ok(Repo { git, auto_resolutions: Default::default() })
    }

    /// Initialise a new repository with a worktree at `path`.
    pub fn init(path: &Path, initial_branch: Option<&str>) -> Result<Self> {
        Self::init_with_format(path, initial_branch, None)
    }

    /// Initialise a new repository, optionally with SHA-256 object ids (`--object-format sha256`).
    pub fn init_with_format(
        path: &Path,
        initial_branch: Option<&str>,
        object_hash: Option<gix::hash::Kind>,
    ) -> Result<Self> {
        std::fs::create_dir_all(path).with_context(|| format!("could not create {}", path.display()))?;
        let git = match object_hash {
            None => gix::init(path),
            Some(kind) => gix::ThreadSafeRepository::init_opts(
                path,
                gix::create::Kind::WithWorktree,
                gix::create::Options { object_hash: Some(kind), ..Default::default() },
                gix::open::Options::default(),
            )
            .map(|r| r.to_thread_local()),
        }
        .with_context(|| format!("could not initialise repository at {}", path.display()))?;
        let repo = Repo { git, auto_resolutions: Default::default() };
        if let Some(branch) = initial_branch {
            let name = crate::refs::branch_full_name(branch)?;
            repo.set_head_symbolic(&name, "veyra init")?;
        }
        Ok(repo)
    }

    /// Wrap an already opened gitoxide repository.
    pub fn from_gix(git: gix::Repository) -> Self {
        Repo { git, auto_resolutions: Default::default() }
    }

    /// Access the underlying gitoxide repository.
    pub fn git(&self) -> &gix::Repository {
        &self.git
    }

    /// Re-read configuration and caches after on-disk changes (e.g. config edits).
    pub fn reload(&mut self) -> Result<()> {
        let path = self.git.path().to_owned();
        self.git = gix::open(&path)?;
        Ok(())
    }

    /// The worktree root, or an error for bare repositories.
    pub fn workdir(&self) -> Result<&Path> {
        self.git.workdir().ok_or_else(|| anyhow!("this operation needs a working tree, but the repository is bare"))
    }

    /// The `.git` directory of this worktree.
    pub fn git_dir(&self) -> &Path {
        self.git.git_dir()
    }

    /// The directory shared by all worktrees (equal to `git_dir` for the main worktree).
    pub fn common_dir(&self) -> &Path {
        self.git.common_dir()
    }

    /// Where Veyra keeps its local, never-pushed state.
    pub fn veyra_dir(&self) -> PathBuf {
        // Each linked worktree keeps its own journal: their HEADs, indexes and files are separate.
        match self.linked_worktree_id() {
            Some(_) => self.git_dir().join("veyra"),
            None => self.common_dir().join("veyra"),
        }
    }

    /// The id of this linked worktree (`.git/worktrees/<id>`), or `None` for the main worktree.
    pub fn linked_worktree_id(&self) -> Option<String> {
        let (git_dir, common) = (self.git_dir(), self.common_dir());
        if git_dir == common || std::fs::canonicalize(git_dir).ok() == std::fs::canonicalize(common).ok() {
            return None;
        }
        git_dir.file_name().map(|n| n.to_string_lossy().into_owned())
    }

    /// The ref that keeps this worktree's journal objects alive: `refs/veyra/keep`, or
    /// `refs/veyra/keep-<id>` for a linked worktree.
    pub fn keep_ref(&self) -> String {
        match self.linked_worktree_id() {
            Some(id) => format!("refs/veyra/keep-{id}"),
            None => "refs/veyra/keep".into(),
        }
    }

    /// Path to the index file.
    pub fn index_path(&self) -> PathBuf {
        self.git.index_path()
    }

    pub fn object_hash(&self) -> gix::hash::Kind {
        self.git.object_hash()
    }

    pub fn empty_tree_id(&self) -> ObjectId {
        ObjectId::empty_tree(self.git.object_hash())
    }

    /// Current state of `HEAD`.
    pub fn head_state(&self) -> Result<HeadState> {
        let head = self.git.head().context("could not read HEAD")?;
        Ok(match head.kind {
            gix::head::Kind::Symbolic(reference) => {
                let target = match reference.target {
                    gix::refs::Target::Object(id) => Some(id),
                    gix::refs::Target::Symbolic(_) => {
                        let mut r = self.git.find_reference(reference.name.as_ref())?;
                        Some(r.peel_to_id()?.detach())
                    }
                };
                HeadState::Branch { name: reference.name.as_bstr().to_owned(), target }
            }
            gix::head::Kind::Unborn(name) => HeadState::Branch { name: name.as_bstr().to_owned(), target: None },
            gix::head::Kind::Detached { target, peeled } => HeadState::Detached(peeled.unwrap_or(target)),
        })
    }

    /// The commit `HEAD` points to, or `None` if the current branch is unborn.
    pub fn head_commit_id(&self) -> Result<Option<ObjectId>> {
        Ok(self.head_state()?.commit())
    }

    /// The tree of `HEAD`, or the empty tree if unborn.
    pub fn head_tree_id(&self) -> Result<ObjectId> {
        match self.head_commit_id()? {
            Some(id) => self.commit_tree_id(id),
            None => Ok(self.empty_tree_id()),
        }
    }

    /// The tree a commit points to.
    pub fn commit_tree_id(&self, commit: ObjectId) -> Result<ObjectId> {
        let commit = self.git.find_commit(commit).with_context(|| format!("{commit} is not a commit"))?;
        Ok(commit.tree_id()?.detach())
    }

    /// Resolve any revision spec (`HEAD~2`, `main`, `v1.0^{tree}`, abbreviated ids…) to an object id.
    pub fn rev_parse(&self, spec: &str) -> Result<ObjectId> {
        let id = self.git.rev_parse_single(spec).map_err(|err| anyhow!("unknown revision '{spec}': {err}"))?;
        Ok(id.detach())
    }

    /// Resolve a revision spec and peel it to a commit.
    pub fn rev_parse_commit(&self, spec: &str) -> Result<ObjectId> {
        let id = self.rev_parse(spec)?;
        let object = self.git.find_object(id)?;
        let commit = object
            .peel_to_kind(gix::object::Kind::Commit)
            .map_err(|_| anyhow!("'{spec}' does not point to a commit"))?;
        Ok(commit.id)
    }

    /// Resolve a revision spec and peel it to a tree.
    pub fn rev_parse_tree(&self, spec: &str) -> Result<ObjectId> {
        let id = self.rev_parse(spec)?;
        let object = self.git.find_object(id)?;
        let tree =
            object.peel_to_kind(gix::object::Kind::Tree).map_err(|_| anyhow!("'{spec}' does not point to a tree"))?;
        Ok(tree.id)
    }

    /// Point `HEAD` at the branch `full_name` (which may not exist yet).
    pub fn set_head_symbolic(&self, full_name: &str, reflog: &str) -> Result<()> {
        use gix::refs::transaction::{LogChange, PreviousValue, RefEdit, RefLog};
        let target: gix::refs::FullName =
            full_name.try_into().with_context(|| format!("invalid ref name {full_name}"))?;
        self.git.edit_reference(RefEdit::update_with_log(
            "HEAD".try_into().expect("valid"),
            gix::refs::Target::Symbolic(target),
            PreviousValue::Any,
            LogChange { mode: RefLog::AndReference, force_create_reflog: false, message: reflog.into() },
        ))?;
        Ok(())
    }

    /// Point `HEAD` directly at `commit`.
    pub fn set_head_detached(&self, commit: ObjectId, reflog: &str) -> Result<()> {
        use gix::refs::transaction::{PreviousValue, RefEdit};
        self.git.edit_reference(RefEdit::update(
            "HEAD".try_into().expect("valid"),
            commit,
            PreviousValue::Any,
            reflog,
        ))?;
        Ok(())
    }

    /// The configured identity as `(name, email)`, if both are set.
    pub fn identity(&self) -> Option<(String, String)> {
        let committer = self.git.committer()?.ok()?;
        Some((committer.name.to_string(), committer.email.to_string()))
    }

    /// Fail with an actionable message unless `user.name` and `user.email` are configured.
    pub fn require_identity(&self) -> Result<()> {
        if self.identity().is_none() {
            bail!(
                "author identity unknown\n\n  Tell Veyra (and Git) who you are:\n\n    veyra config --global user.name \"Your Name\"\n    veyra config --global user.email \"you@example.com\""
            );
        }
        Ok(())
    }

    /// Git's in-progress operation (merge, rebase, cherry-pick, …), if any.
    pub fn in_progress(&self) -> Option<String> {
        self.git.state().map(|s| format!("{s:?}"))
    }

    /// Read a blob's raw (git-internal) content.
    pub fn read_blob(&self, id: ObjectId) -> Result<Vec<u8>> {
        match self.git.find_blob(id) {
            Ok(blob) => Ok(blob.data.clone()),
            // A partial clone: fetch the blob from the promisor remote, then read it.
            Err(_) if self.is_partial() && !self.git.has_object(id) => {
                self.ensure_objects([id])?;
                Ok(self
                    .git
                    .find_blob(id)
                    .with_context(|| format!("missing blob {id} (not on the promisor remote either)"))?
                    .data
                    .clone())
            }
            Err(e) => Err(anyhow::Error::from(e).context(format!("missing blob {id}"))),
        }
    }

    /// Write raw bytes as a blob.
    pub fn write_blob(&self, data: &[u8]) -> Result<ObjectId> {
        Ok(self.git.write_blob(data)?.detach())
    }

    /// Is `ancestor` reachable from `descendant` (or equal to it)?
    pub fn is_ancestor(&self, ancestor: ObjectId, descendant: ObjectId) -> Result<bool> {
        if ancestor == descendant {
            return Ok(true);
        }
        match self.git.merge_base(ancestor, descendant) {
            Ok(base) => Ok(base.map(|b| b.detach()) == Some(ancestor)),
            Err(_) => Ok(false),
        }
    }

    /// A worktree-relative path for display, using the platform separator.
    pub fn display_path(&self, rela: &gix::bstr::BStr) -> String {
        rela.to_str_lossy().into_owned()
    }

    /// Convert a user-supplied filesystem path (relative to `cwd`) into a repository-relative path.
    pub fn to_repo_path(&self, cwd: &Path, user_path: &str) -> Result<BString> {
        let workdir = std::fs::canonicalize(self.workdir()?)?;
        let joined = cwd.join(user_path);
        let abs = normalize_lexically(&joined);
        let abs = match std::fs::canonicalize(&abs) {
            Ok(p) => p,
            Err(_) => {
                // The path may not exist (deleted file). Canonicalise its parent instead.
                let parent = abs.parent().map(Path::to_path_buf).unwrap_or_default();
                let name = abs.file_name().map(|n| n.to_owned());
                let parent = std::fs::canonicalize(&parent).unwrap_or(parent);
                match name {
                    Some(n) => parent.join(n),
                    None => parent,
                }
            }
        };
        let rel = abs
            .strip_prefix(&workdir)
            .map_err(|_| anyhow!("'{user_path}' is outside the repository at {}", workdir.display()))?;
        let mut out = String::new();
        for (i, comp) in rel.components().enumerate() {
            if i > 0 {
                out.push('/');
            }
            out.push_str(&comp.as_os_str().to_string_lossy());
        }
        Ok(out.into())
    }
}

/// Resolve `.` and `..` components without touching the filesystem.
fn normalize_lexically(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Convert a repository-relative, slash-separated path into a native relative path.
pub(crate) fn fs_path(path: &gix::bstr::BStr) -> Result<PathBuf> {
    Ok(gix::path::from_bstr(path)
        .map_err(|e| anyhow!("path {path:?} cannot be represented on this platform: {e}"))?
        .into_owned())
}
