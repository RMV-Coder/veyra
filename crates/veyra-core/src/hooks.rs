//! Running Git hooks (`pre-commit`, `commit-msg`, `pre-push`, …) the way Git does, so tools
//! like husky, lint-staged and pre-commit keep working under Veyra.
//!
//! * Hooks live in `core.hooksPath` (relative to the worktree root) or `<git-dir>/hooks`.
//! * On Windows they run through the `sh` that ships with Git for Windows; elsewhere they are
//!   executed directly and must be executable (Git skips non-executable hooks too).
//! * They run from the worktree root, with Git's environment conventions.
//! * Hooks of repositories the current user doesn't own are **never** run (gitoxide's trust
//!   model): a cloned repository must not be able to execute code just because you ran
//!   `veyra commit` in it.

use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result, bail};

use crate::Repo;

/// What running a hook did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookResult {
    /// No such hook is installed.
    Absent,
    /// The hook ran and exited successfully.
    Passed,
    /// The hook is installed but was not run (untrusted repository or not executable).
    Skipped,
}

impl Repo {
    /// The directory hooks are looked up in.
    pub fn hooks_dir(&self) -> PathBuf {
        match self.config_get("core.hooksPath") {
            Some(p) => {
                let p = PathBuf::from(p);
                if p.is_relative() { self.workdir().map(|w| w.join(&p)).unwrap_or(p) } else { p }
            }
            None => self.common_dir().join("hooks"),
        }
    }

    /// The path of an installed hook (ignoring Git's `*.sample` files).
    pub fn hook_path(&self, name: &str) -> Option<PathBuf> {
        let path = self.hooks_dir().join(name);
        path.is_file().then_some(path)
    }

    /// Run hook `name` with `args` (and `stdin`). Fails if the hook exits non-zero.
    pub fn run_hook(&self, name: &str, args: &[&str], stdin: Option<&[u8]>) -> Result<HookResult> {
        let Some(path) = self.hook_path(name) else {
            return Ok(HookResult::Absent);
        };
        if self.git.git_dir_trust() != gix::sec::Trust::Full {
            return Ok(HookResult::Skipped);
        }
        let mut command = if cfg!(windows) {
            let mut c = gix::path::env::shell_command();
            c.arg(&path);
            c
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&path)?.permissions().mode();
                if mode & 0o111 == 0 {
                    return Ok(HookResult::Skipped);
                }
            }
            std::process::Command::new(&path)
        };
        command.args(args);
        let cwd = match self.git.workdir() {
            Some(w) => w.to_owned(),
            None => self.git_dir().to_owned(),
        };
        command
            .current_dir(&cwd)
            .env("GIT_INDEX_FILE", self.index_path())
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
        if name == "pre-commit" || name == "commit-msg" || name == "prepare-commit-msg" {
            // Like git: a hook must not try to open an editor of its own.
            command.env("GIT_EDITOR", ":");
        }
        let mut child =
            command.spawn().with_context(|| format!("could not run the {name} hook ({})", path.display()))?;
        if let Some(input) = stdin
            && let Some(mut pipe) = child.stdin.take()
        {
            // A hook that exits without reading stdin closes the pipe; that's not an error.
            let _ = pipe.write_all(input);
        }
        let status = child.wait()?;
        if !status.success() {
            bail!(
                "the {name} hook failed ({})\n  hint: fix what it reports, or skip hooks with --no-verify",
                status.code().map(|c| format!("exit code {c}")).unwrap_or_else(|| "killed".into())
            );
        }
        Ok(HookResult::Passed)
    }

    /// Run a hook whose failure must not fail the command (post-* hooks).
    pub fn run_post_hook(&self, name: &str, args: &[&str]) -> Option<String> {
        self.run_hook(name, args, None).err().map(|e| e.to_string())
    }

    /// Like [`Repo::run_post_hook`], with input on stdin (`post-rewrite`).
    pub fn run_post_hook_with_input(&self, name: &str, args: &[&str], stdin: &[u8]) -> Option<String> {
        self.run_hook(name, args, Some(stdin)).err().map(|e| e.to_string())
    }
}
