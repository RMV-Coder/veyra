//! Out-of-process analyzers ("plugins"), declared in `.veyra/analyzers.toml`:
//!
//! ```toml
//! [[plugin]]
//! id = "orm-lint"
//! command = ["python", "tools/orm_lint.py"]
//! paths = ["app/models/**/*.py"]   # optional: which changed files it sees
//! timeout_seconds = 30             # optional
//! ```
//!
//! Veyra runs the command from the repository root and writes one JSON request to its stdin:
//!
//! ```json
//! {"version": 1, "plugin": "orm-lint",
//!  "files": [{"path": "app/models/user.py", "old": "…", "new": "…"}]}
//! ```
//!
//! (`old` is `null` for an added file, `new` for a deleted one.) The plugin answers on stdout:
//!
//! ```json
//! {"findings": [{"severity": "high", "rule": "nullable-fk", "path": "app/models/user.py",
//!                "line": 12, "message": "…", "advice": "…"}]}
//! ```
//!
//! A plugin is a program, and `.veyra/analyzers.toml` is committed: anyone who can push to the
//! repository can change it. So the caller runs only plugins whose exact definition the local
//! user has trusted (see [`TrustStore`]); this module just runs what it is given.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{Area, ChangedFile, Finding, Severity};

const MAX_OUTPUT: u64 = 16 * 1024 * 1024;
const MAX_FINDINGS: usize = 1000;

fn default_timeout() -> u64 {
    60
}

/// One `[[plugin]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginSpec {
    pub id: String,
    /// The program and its arguments; a relative program path is relative to the repository root.
    pub command: Vec<String>,
    /// Only changed files matching these globs are sent; all of them when empty.
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}

#[derive(Serialize)]
struct Request<'a> {
    version: u32,
    plugin: &'a str,
    files: Vec<RequestFile<'a>>,
}

#[derive(Serialize)]
struct RequestFile<'a> {
    path: &'a str,
    old: Option<&'a str>,
    new: Option<&'a str>,
}

#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    findings: Vec<ResponseFinding>,
}

#[derive(Deserialize)]
struct ResponseFinding {
    severity: String,
    rule: String,
    path: String,
    #[serde(default)]
    line: usize,
    message: String,
    #[serde(default)]
    advice: String,
    #[serde(default)]
    excerpt: String,
}

impl PluginSpec {
    fn matcher(&self) -> Result<Option<globset::GlobSet>> {
        if self.paths.is_empty() {
            return Ok(None);
        }
        let mut b = globset::GlobSetBuilder::new();
        for p in &self.paths {
            b.add(globset::Glob::new(p).with_context(|| format!("plugin {}: bad path glob {p}", self.id))?);
        }
        Ok(Some(b.build()?))
    }

    /// Run the plugin on the files it is interested in. Nothing runs if none of them changed.
    pub fn run(&self, root: &Path, files: &[ChangedFile<'_>]) -> Result<Vec<Finding>> {
        let Some(program) = self.command.first() else { bail!("plugin {}: `command` is empty", self.id) };
        let matcher = self.matcher()?;
        let sent: Vec<RequestFile<'_>> = files
            .iter()
            .filter(|f| matcher.as_ref().is_none_or(|m| m.is_match(f.path)))
            .map(|f| RequestFile { path: f.path, old: f.old, new: f.new })
            .collect();
        if sent.is_empty() {
            return Ok(Vec::new());
        }
        let known: std::collections::BTreeSet<&str> = sent.iter().map(|f| f.path).collect();
        let input = serde_json::to_vec(&Request { version: 1, plugin: &self.id, files: sent })?;

        let program = if program.contains('/') || program.contains('\\') {
            let p = PathBuf::from(program);
            if p.is_relative() { root.join(p) } else { p }
        } else {
            PathBuf::from(program)
        };
        let mut child = Command::new(&program)
            .args(&self.command[1..])
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("plugin {}: could not run `{}`", self.id, self.command.join(" ")))?;
        let mut stdin = child.stdin.take().context("no stdin")?;
        let writer = std::thread::spawn(move || {
            // A plugin that exits without reading everything closes the pipe; that's its business.
            let _ = stdin.write_all(&input);
        });
        let mut stdout = child.stdout.take().context("no stdout")?;
        let reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = (&mut stdout).take(MAX_OUTPUT).read_to_end(&mut buf);
            buf
        });
        let mut stderr = child.stderr.take().context("no stderr")?;
        let errors = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = (&mut stderr).take(64 * 1024).read_to_end(&mut buf);
            buf
        });
        let deadline = Instant::now() + Duration::from_secs(self.timeout_seconds.max(1));
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("plugin {}: no answer within {}s", self.id, self.timeout_seconds);
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let _ = writer.join();
        let output = reader.join().unwrap_or_default();
        let stderr = String::from_utf8_lossy(&errors.join().unwrap_or_default()).trim().to_owned();
        if !status.success() {
            let code = status.code().map(|c| format!("exit code {c}")).unwrap_or_else(|| "killed".into());
            bail!(
                "plugin {} failed ({code}){}",
                self.id,
                if stderr.is_empty() { String::new() } else { format!(": {stderr}") }
            );
        }
        let response: Response = serde_json::from_slice(&output)
            .with_context(|| format!("plugin {}: the answer is not the expected JSON", self.id))?;
        let mut out = Vec::new();
        for f in response.findings.into_iter().take(MAX_FINDINGS) {
            let severity = Severity::parse(&f.severity)
                .with_context(|| format!("plugin {}: severity must be high, medium or low", self.id))?;
            if !known.contains(f.path.as_str()) {
                bail!("plugin {}: a finding names {}, which it wasn't given", self.id, f.path);
            }
            out.push(Finding {
                area: Area::Plugin,
                severity,
                rule: format!("{}/{}", self.id, f.rule),
                path: f.path,
                line: f.line,
                message: f.message,
                advice: f.advice,
                excerpt: f.excerpt.trim().chars().take(160).collect(),
            });
        }
        Ok(out)
    }
}

/// The plugins this user has agreed to run in this repository, each with the exact definition
/// that was trusted: a changed command or path list (say, from a pulled commit) is not trusted
/// until it is reviewed again. It lives in the repository's Git directory, so it is never
/// committed or cloned.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TrustStore {
    #[serde(default)]
    trusted: BTreeMap<String, PluginSpec>,
}

impl TrustStore {
    pub fn load(path: &Path) -> Result<TrustStore> {
        match std::fs::read(path) {
            Ok(data) => serde_json::from_slice(&data).with_context(|| format!("{} is damaged", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TrustStore::default()),
            Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path).with_context(|| format!("could not write {}", path.display()))
    }

    pub fn is_trusted(&self, spec: &PluginSpec) -> bool {
        self.trusted.get(&spec.id) == Some(spec)
    }

    pub fn trust(&mut self, spec: &PluginSpec) {
        self.trusted.insert(spec.id.clone(), spec.clone());
    }

    pub fn revoke(&mut self, id: &str) -> bool {
        self.trusted.remove(id).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(command: &[&str]) -> PluginSpec {
        PluginSpec {
            id: "p".into(),
            command: command.iter().map(|s| s.to_string()).collect(),
            paths: vec![],
            timeout_seconds: 60,
        }
    }

    #[test]
    fn trust_is_bound_to_the_exact_definition() {
        let mut store = TrustStore::default();
        let a = spec(&["lint", "--strict"]);
        assert!(!store.is_trusted(&a));
        store.trust(&a);
        assert!(store.is_trusted(&a));
        assert!(!store.is_trusted(&spec(&["lint", "--strict", "; curl evil"])), "a changed command needs new trust");
        let mut narrowed = a.clone();
        narrowed.paths = vec!["src/**".into()];
        assert!(!store.is_trusted(&narrowed));
        assert!(store.revoke("p"));
        assert!(!store.is_trusted(&a));
    }

    #[test]
    fn plugins_see_only_matching_files_and_do_not_run_without_any() {
        let mut s = spec(&["definitely-not-a-real-program-xyz"]);
        s.paths = vec!["db/**".into()];
        let files = [ChangedFile { path: "src/a.rs", old: None, new: Some("x") }];
        // Nothing matches, so the (missing) program isn't even started.
        assert!(s.run(Path::new("."), &files).unwrap().is_empty());
        let files = [ChangedFile { path: "db/a.sql", old: None, new: Some("x") }];
        assert!(s.run(Path::new("."), &files).unwrap_err().to_string().contains("could not run"));
    }
}
