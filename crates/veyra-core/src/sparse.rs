//! Sparse checkout in Git's **cone mode** (`core.sparseCheckout` + `core.sparseCheckoutCone`,
//! patterns in `.git/info/sparse-checkout`), and `clone --scope` built on it.
//!
//! In cone mode a set of directories is selected. Included are: every file at the root, every
//! file directly inside an ancestor of a selected directory, and everything below a selected
//! directory. Index entries outside the cone carry the `skip-worktree` flag and their files are
//! absent from the worktree — exactly what `git sparse-checkout set` produces.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use gix::bstr::{BStr, BString, ByteSlice};
use serde::Deserialize;

use crate::Repo;

/// The selected cone directories (slash-separated, no leading or trailing slash).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sparse {
    pub dirs: Vec<String>,
}

impl Sparse {
    pub fn new(dirs: &[String]) -> Sparse {
        let mut dirs: Vec<String> =
            dirs.iter().map(|d| d.replace('\\', "/").trim_matches('/').to_owned()).filter(|d| !d.is_empty()).collect();
        dirs.sort();
        dirs.dedup();
        Sparse { dirs }
    }

    /// Is `path` (a file) inside the cone?
    pub fn includes(&self, path: &BStr) -> bool {
        let path = path.to_str_lossy();
        let Some(slash) = path.rfind('/') else { return true };
        let dir = &path[..slash];
        self.dirs.iter().any(|d| {
            dir == d || dir.starts_with(&format!("{d}/")) || d.starts_with(&format!("{dir}/")) // a file directly inside an ancestor of a cone dir
        })
    }

    /// The `.git/info/sparse-checkout` content Git writes for this cone.
    pub fn to_patterns(&self) -> String {
        let mut out = String::from("/*\n!/*/\n");
        let mut parents: Vec<String> = Vec::new();
        for d in &self.dirs {
            let parts: Vec<&str> = d.split('/').collect();
            for i in 1..parts.len() {
                let parent = parts[..i].join("/");
                if !parents.contains(&parent) {
                    parents.push(parent);
                }
            }
        }
        parents.sort();
        for p in &parents {
            out.push_str(&format!("/{p}/\n!/{p}/*/\n"));
        }
        for d in &self.dirs {
            out.push_str(&format!("/{d}/\n"));
        }
        out
    }

    /// Read cone directories back from a patterns file (the positive `/dir/` lines that aren't parents).
    pub fn from_patterns(text: &str) -> Sparse {
        let lines: Vec<&str> = text.lines().map(str::trim).collect();
        let dirs: Vec<String> = lines
            .iter()
            .filter(|l| l.starts_with('/') && l.ends_with('/') && **l != "/*/" && !l.starts_with("!/"))
            .map(|l| l.trim_matches('/').to_owned())
            .filter(|d| !lines.contains(&format!("!/{d}/*/").as_str()))
            .collect();
        Sparse::new(&dirs)
    }
}

/// Does a sparse-checkout file contain only the pattern shapes Git writes in cone mode?
fn is_cone_patterns(text: &str) -> bool {
    let plain = |d: &str| !d.is_empty() && !d.contains(['*', '?', '[', '!', '\\']);
    text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).all(|l| {
        l == "/*"
            || l == "!/*/"
            || l.strip_prefix('/').and_then(|r| r.strip_suffix('/')).is_some_and(plain)
            || l.strip_prefix("!/").and_then(|r| r.strip_suffix("/*/")).is_some_and(plain)
    })
}

/// `.veyra/scopes.toml`: named sets of directories for `clone --scope` and `sparse set --scope`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Scopes {
    #[serde(default, rename = "scope")]
    pub scopes: BTreeMap<String, Scope>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Scope {
    pub paths: Vec<String>,
    #[serde(default)]
    pub description: String,
}

impl Scopes {
    pub fn parse(text: &str) -> Result<Scopes> {
        toml::from_str(text).context("invalid .veyra/scopes.toml")
    }

    pub fn get(&self, name: &str) -> Result<&Scope> {
        self.scopes.get(name).with_context(|| {
            format!("no scope named '{name}' (defined: {})", self.scopes.keys().cloned().collect::<Vec<_>>().join(", "))
        })
    }
}

impl Repo {
    fn sparse_file(&self) -> std::path::PathBuf {
        self.common_dir().join("info").join("sparse-checkout")
    }

    /// The active cone, if sparse checkout is enabled.
    pub fn sparse(&self) -> Result<Option<Sparse>> {
        // Read the local config file fresh: `sparse set` changes it within one process.
        let local = crate::config::list_file(&self.config_path(crate::config::Scope::Local)?).unwrap_or_default();
        let enabled = local
            .iter()
            .rev()
            .find(|(k, _)| k == "core.sparsecheckout")
            .map(|(_, v)| v == "true")
            .unwrap_or_else(|| self.config_bool("core.sparseCheckout").unwrap_or(false));
        if !enabled {
            return Ok(None);
        }
        let cone_setting = local
            .iter()
            .rev()
            .find(|(k, _)| k == "core.sparsecheckoutcone")
            .map(|(_, v)| v == "true")
            .or_else(|| self.config_bool("core.sparseCheckoutCone"));
        match std::fs::read_to_string(self.sparse_file()) {
            Ok(text) => {
                // Non-cone patterns are gitignore-style globs with different semantics; reading
                // them as a cone would check out the wrong files, so refuse instead.
                if cone_setting == Some(false) || !is_cone_patterns(&text) {
                    bail!(
                        "this repository uses non-cone sparse-checkout patterns, which Veyra doesn't support\n  \
                         hint: `git sparse-checkout set --cone <dirs>` converts it, or keep using git here"
                    );
                }
                Ok(Some(Sparse::from_patterns(&text)))
            }
            Err(_) => Ok(None),
        }
    }

    /// Restrict the worktree to `dirs` (cone mode). Returns (files written, files removed).
    pub fn sparse_set(&self, dirs: &[String]) -> Result<(usize, usize)> {
        let sparse = Sparse::new(dirs);
        let changes = self.worktree_changes()?;
        let dirty: Vec<_> = changes.dirty_tracked().into_iter().filter(|p| !sparse.includes(p.as_ref())).collect();
        if !dirty.is_empty() {
            bail!(
                "these files have local changes and would leave the sparse checkout: {}\n  hint: commit or stash them first",
                dirty.iter().take(5).map(|p| p.to_string()).collect::<Vec<_>>().join(", ")
            );
        }
        let file = self.sparse_file();
        std::fs::create_dir_all(file.parent().expect("has parent"))?;
        std::fs::write(&file, sparse.to_patterns())?;
        let config = self.config_path(crate::config::Scope::Local)?;
        crate::config::set_in_file(&config, "core.sparseCheckout", "true")?;
        crate::config::set_in_file(&config, "core.sparseCheckoutCone", "true")?;
        self.reapply_sparse(Some(&sparse))
    }

    /// Turn sparse checkout off and materialize every file. Returns files written.
    pub fn sparse_disable(&self) -> Result<usize> {
        let config = self.config_path(crate::config::Scope::Local)?;
        crate::config::set_in_file(&config, "core.sparseCheckout", "false")?;
        Ok(self.reapply_sparse(None)?.0)
    }

    /// Make the index flags and the worktree agree with `sparse`.
    fn reapply_sparse(&self, sparse: Option<&Sparse>) -> Result<(usize, usize)> {
        let index = self.flat_index()?;
        let workdir = self.workdir()?.to_owned();
        let mut writes = Vec::new();
        let mut removed = 0;
        for (path, entry) in &index {
            let on_disk = workdir.join(crate::repo::fs_path(path.as_ref())?).symlink_metadata().is_ok();
            let wanted = sparse.is_none_or(|s| s.includes(path.as_ref()));
            if wanted && !on_disk {
                writes.push((path.clone(), Some(*entry)));
            } else if !wanted && on_disk {
                removed += 1;
                writes.push((path.clone(), None));
            }
        }
        let written = writes.iter().filter(|(_, e)| e.is_some()).count();
        // Deletions of excluded paths must happen even though write_worktree skips excluded writes.
        let stats = self.write_worktree_with(&writes, None)?;
        self.write_index_from_flat(&index, &stats)?;
        Ok((written, removed))
    }

    /// Clone `url` into `path`, but check out only the scope's directories (`clone --scope`).
    pub fn clone_scoped(url: &str, path: &Path, dirs_or_scope: ScopeRequest) -> Result<(Repo, Vec<String>)> {
        let repo = Repo::clone_from_no_checkout(url, path)?;
        let dirs = match dirs_or_scope {
            ScopeRequest::Dirs(d) => d,
            ScopeRequest::Named(name) => {
                let head = repo.head_commit_id()?.context("the cloned repository has no commits")?;
                let flat = repo.flat_from_commit(head)?;
                let entry = flat
                    .get(BString::from(".veyra/scopes.toml").as_bstr())
                    .context("the repository has no .veyra/scopes.toml, so --scope names can't be resolved")?;
                let scopes = Scopes::parse(&repo.read_blob(entry.id)?.to_str_lossy())?;
                scopes.get(&name)?.paths.clone()
            }
        };
        let sparse = Sparse::new(&dirs);
        let file = repo.sparse_file();
        std::fs::create_dir_all(file.parent().expect("has parent"))?;
        std::fs::write(&file, sparse.to_patterns())?;
        let config = repo.config_path(crate::config::Scope::Local)?;
        crate::config::set_in_file(&config, "core.sparseCheckout", "true")?;
        crate::config::set_in_file(&config, "core.sparseCheckoutCone", "true")?;
        let head = repo.head_commit_id()?.context("the cloned repository has no commits")?;
        let flat = repo.flat_from_commit(head)?;
        let writes: Vec<_> =
            flat.iter().filter(|(p, _)| sparse.includes(p.as_ref())).map(|(p, e)| (p.clone(), Some(*e))).collect();
        let stats = repo.write_worktree(&writes)?;
        repo.write_index_from_flat(&flat, &stats)?;
        Ok((repo, sparse.dirs))
    }
}

/// What `clone --scope` should check out.
#[derive(Debug, Clone)]
pub enum ScopeRequest {
    /// A scope name from `.veyra/scopes.toml`.
    Named(String),
    /// Explicit directories.
    Dirs(Vec<String>),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(dirs: &[&str]) -> Sparse {
        Sparse::new(&dirs.iter().map(|d| d.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn cone_and_non_cone_patterns() {
        assert!(is_cone_patterns(&Sparse::new(&["a/b".into(), "c".into()]).to_patterns()));
        assert!(!is_cone_patterns("/*\n!/*/\n*.md\n"));
        assert!(!is_cone_patterns("/src/**/*.rs\n"));
    }

    #[test]
    fn cone_semantics() {
        let sp = s(&["frontend", "packages/ui"]);
        for yes in ["README.md", "frontend/a.ts", "frontend/deep/b.ts", "packages/package.json", "packages/ui/x.ts"] {
            assert!(sp.includes(yes.into()), "{yes} should be included");
        }
        for no in ["backend/main.rs", "packages/types/t.ts", "frontendx/a.ts"] {
            assert!(!sp.includes(no.into()), "{no} should be excluded");
        }
    }

    #[test]
    fn patterns_round_trip_like_git() {
        let sp = s(&["frontend", "packages/ui"]);
        let text = sp.to_patterns();
        assert_eq!(text, "/*\n!/*/\n/packages/\n!/packages/*/\n/frontend/\n/packages/ui/\n");
        assert_eq!(Sparse::from_patterns(&text), sp);
    }

    #[test]
    fn scopes_file() {
        let scopes =
            Scopes::parse("[scope.frontend]\npaths = [\"frontend\", \"packages/ui\"]\ndescription = \"web\"\n")
                .unwrap();
        assert_eq!(scopes.get("frontend").unwrap().paths.len(), 2);
        assert!(scopes.get("mobile").is_err());
    }
}
