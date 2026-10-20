//! Intent records: *why* code exists, stored as reviewed TOML files in `.veyra/intents/`.
//!
//! An intent can carry **guards** — deterministic, explainable checks such as "some file
//! under `src/payment/**` must still contain `idempotencyKey`". Guards are how Veyra warns
//! a developer (or an AI agent) that a change may undo a recorded design decision.
//! Nothing here is heuristic or AI-driven: a guard either holds or it doesn't.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Directory (relative to the worktree root) holding intent files.
pub const INTENTS_DIR: &str = ".veyra/intents";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Active,
    Closed,
    Superseded,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Status::Active => "active",
            Status::Closed => "closed",
            Status::Superseded => "superseded",
        })
    }
}

/// A recorded design intent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    pub id: String,
    pub title: String,
    /// feature, bugfix, refactor, decision, security, performance, …
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub status: Status,
    /// ISO date, e.g. `2026-10-20`.
    pub created: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub objective: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub problem: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub solution: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<String>,
    /// Issue keys or URLs, e.g. `PAY-142`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related: Vec<String>,
    /// How the intent is verified (tests, procedures).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verification: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default, rename = "guard", skip_serializing_if = "Vec::is_empty")]
    pub guards: Vec<Guard>,
}

fn default_kind() -> String {
    "feature".into()
}

/// A deterministic check tied to an intent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Guard {
    /// Glob patterns (`src/payment/**`, `*.sql`) selecting the files the guard is about.
    pub paths: Vec<String>,
    /// At least one selected file must contain this literal text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub must_contain: Option<String>,
    /// At least one selected file must match this regular expression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub must_match: Option<String>,
    /// No selected file may contain this literal text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub must_not_contain: Option<String>,
    /// Shown when the guard fails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// A failed guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub intent_id: String,
    pub intent_title: String,
    pub guard_index: usize,
    pub message: String,
    /// What exactly was checked and why it failed.
    pub detail: String,
}

/// Read access to a set of files (the worktree, the index, or a commit).
pub trait FileSource {
    /// All file paths, slash-separated, relative to the repository root.
    fn paths(&self) -> Vec<String>;
    /// File content as text, or `None` if missing or binary.
    fn read(&self, path: &str) -> Option<String>;
}

/// An in-memory [`FileSource`], handy for tests and for callers that already have contents.
#[derive(Debug, Default, Clone)]
pub struct MemorySource(pub BTreeMap<String, String>);

impl FileSource for MemorySource {
    fn paths(&self) -> Vec<String> {
        self.0.keys().cloned().collect()
    }
    fn read(&self, path: &str) -> Option<String> {
        self.0.get(path).cloned()
    }
}

impl Intent {
    /// A new, active intent with today's date and a fresh id.
    pub fn new(title: &str, kind: &str, created: &str) -> Intent {
        Intent {
            id: new_id(),
            title: title.to_owned(),
            kind: kind.to_owned(),
            status: Status::Active,
            created: created.to_owned(),
            objective: String::new(),
            problem: String::new(),
            solution: String::new(),
            constraints: Vec::new(),
            related: Vec::new(),
            verification: Vec::new(),
            superseded_by: None,
            guards: Vec::new(),
        }
    }

    pub fn from_toml(text: &str) -> Result<Intent> {
        let intent: Intent = toml::from_str(text)?;
        intent.validate()?;
        Ok(intent)
    }

    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    pub fn validate(&self) -> Result<()> {
        if !self.id.starts_with("int-")
            || self.id.len() < 5
            || !self.id[4..].chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            bail!("invalid intent id '{}': expected int-<letters/digits>", self.id);
        }
        if self.title.trim().is_empty() {
            bail!("intent {} has an empty title", self.id);
        }
        for (i, guard) in self.guards.iter().enumerate() {
            if guard.paths.is_empty() {
                bail!("intent {} guard #{} has no paths", self.id, i + 1);
            }
            if guard.must_contain.is_none() && guard.must_match.is_none() && guard.must_not_contain.is_none() {
                bail!(
                    "intent {} guard #{} checks nothing (set must_contain, must_match or must_not_contain)",
                    self.id,
                    i + 1
                );
            }
            if let Some(re) = &guard.must_match {
                regex::Regex::new(re)
                    .with_context(|| format!("intent {} guard #{}: invalid must_match", self.id, i + 1))?;
            }
            build_globset(&guard.paths)
                .with_context(|| format!("intent {} guard #{}: invalid paths", self.id, i + 1))?;
        }
        Ok(())
    }

    /// Evaluate this intent's guards against `files`. Inactive intents never fail.
    pub fn verify(&self, files: &dyn FileSource) -> Result<Vec<Violation>> {
        if self.status != Status::Active {
            return Ok(Vec::new());
        }
        let all_paths = files.paths();
        let mut out = Vec::new();
        for (i, guard) in self.guards.iter().enumerate() {
            let globs = build_globset(&guard.paths)?;
            let selected: Vec<&String> = all_paths.iter().filter(|p| globs.is_match(p.as_str())).collect();
            let message = guard.message.clone().unwrap_or_else(|| self.title.clone());
            let violation = |detail: String| Violation {
                intent_id: self.id.clone(),
                intent_title: self.title.clone(),
                guard_index: i,
                message: message.clone(),
                detail,
            };
            let patterns = guard.paths.join(", ");
            if selected.is_empty() {
                out.push(violation(format!("no files match {patterns} — were they moved or deleted?")));
                continue;
            }
            if let Some(needle) = &guard.must_contain
                && !selected.iter().any(|p| files.read(p).is_some_and(|t| t.contains(needle.as_str())))
            {
                out.push(violation(format!("no file in {patterns} contains `{needle}` any more")));
            }
            if let Some(re) = &guard.must_match {
                let re = regex::Regex::new(re)?;
                if !selected.iter().any(|p| files.read(p).is_some_and(|t| re.is_match(&t))) {
                    out.push(violation(format!("no file in {patterns} matches /{re}/ any more")));
                }
            }
            if let Some(needle) = &guard.must_not_contain {
                let offenders: Vec<&str> = selected
                    .iter()
                    .filter(|p| files.read(p).is_some_and(|t| t.contains(needle.as_str())))
                    .map(|p| p.as_str())
                    .collect();
                if !offenders.is_empty() {
                    out.push(violation(format!("`{needle}` appears in {}", offenders.join(", "))));
                }
            }
        }
        Ok(out)
    }
}

fn build_globset(patterns: &[String]) -> Result<globset::GlobSet> {
    let mut builder = globset::GlobSetBuilder::new();
    for p in patterns {
        builder.add(globset::GlobBuilder::new(p).literal_separator(true).build()?);
    }
    Ok(builder.build()?)
}

/// A fresh id like `int-8a92c1`, random enough that two branches won't collide.
pub fn new_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    let nanos =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
    hasher.write_u128(nanos);
    hasher.write_u32(std::process::id());
    format!("int-{:06x}", hasher.finish() & 0xff_ffff)
}

/// All intents in `<root>/.veyra/intents`, sorted by creation date then id.
pub fn load_all(root: &Path) -> Result<Vec<(PathBuf, Intent)>> {
    let dir = root.join(INTENTS_DIR);
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(out);
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).with_context(|| format!("could not read {}", path.display()))?;
        let intent = Intent::from_toml(&text).with_context(|| format!("invalid intent file {}", path.display()))?;
        out.push((path, intent));
    }
    out.sort_by(|a, b| (&a.1.created, &a.1.id).cmp(&(&b.1.created, &b.1.id)));
    Ok(out)
}

/// Load intents from in-memory `(path, text)` pairs (e.g. read from the index or a commit).
pub fn load_from_texts(files: &[(String, String)]) -> Result<Vec<Intent>> {
    let mut out = Vec::new();
    for (path, text) in files {
        out.push(Intent::from_toml(text).with_context(|| format!("invalid intent file {path}"))?);
    }
    out.sort_by(|a, b| (&a.created, &a.id).cmp(&(&b.created, &b.id)));
    Ok(out)
}

/// Find an intent by id or unique id prefix.
pub fn find<'a>(intents: &'a [(PathBuf, Intent)], query: &str) -> Result<&'a (PathBuf, Intent)> {
    if let Some(exact) = intents.iter().find(|(_, i)| i.id == query) {
        return Ok(exact);
    }
    let matches: Vec<_> = intents.iter().filter(|(_, i)| i.id.starts_with(query)).collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => bail!("no intent with id '{query}'"),
        _ => bail!(
            "'{query}' is ambiguous: {}",
            matches.iter().map(|(_, i)| i.id.as_str()).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// Write `intent` to `<root>/.veyra/intents/<id>.toml`.
pub fn save(root: &Path, intent: &Intent) -> Result<PathBuf> {
    intent.validate()?;
    let dir = root.join(INTENTS_DIR);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.toml", intent.id));
    let mut text = String::from("# Veyra intent — reviewed like code. See docs/DESIGN.md §5.2.\n");
    text.push_str(&intent.to_toml()?);
    std::fs::write(&path, text).with_context(|| format!("could not write {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payment_intent() -> Intent {
        let mut i = Intent::new("Prevent duplicate payment processing", "bugfix", "2026-10-20");
        i.id = "int-8a92c1".into();
        i.related = vec!["PAY-142".into()];
        i.guards.push(Guard {
            paths: vec!["src/payment/**".into()],
            must_contain: Some("idempotencyKey".into()),
            message: Some("Idempotency validation must remain".into()),
            ..Default::default()
        });
        i
    }

    fn files(entries: &[(&str, &str)]) -> MemorySource {
        MemorySource(entries.iter().map(|(p, t)| ((*p).to_owned(), (*t).to_owned())).collect())
    }

    #[test]
    fn toml_round_trip() {
        let intent = payment_intent();
        let text = intent.to_toml().unwrap();
        assert!(text.contains("[[guard]]"));
        assert_eq!(Intent::from_toml(&text).unwrap(), intent);
    }

    #[test]
    fn guard_passes_while_the_mechanism_exists() {
        let src = files(&[("src/payment/service.ts", "if (!req.idempotencyKey) throw ...")]);
        assert!(payment_intent().verify(&src).unwrap().is_empty());
    }

    #[test]
    fn guard_fails_when_the_mechanism_is_removed() {
        let src = files(&[("src/payment/service.ts", "createPayment(req)")]);
        let violations = payment_intent().verify(&src).unwrap();
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].message, "Idempotency validation must remain");
        assert!(violations[0].detail.contains("idempotencyKey"));
    }

    #[test]
    fn guard_fails_when_files_disappear() {
        let src = files(&[("src/other.ts", "idempotencyKey")]);
        let v = payment_intent().verify(&src).unwrap();
        assert!(v[0].detail.contains("no files match"));
    }

    #[test]
    fn closed_intents_never_fail() {
        let mut intent = payment_intent();
        intent.status = Status::Closed;
        assert!(intent.verify(&files(&[])).unwrap().is_empty());
    }

    #[test]
    fn must_not_contain_and_must_match() {
        let mut intent = payment_intent();
        intent.guards = vec![
            Guard { paths: vec!["**/*.sql".into()], must_not_contain: Some("DROP TABLE".into()), ..Default::default() },
            Guard { paths: vec!["src/*.rs".into()], must_match: Some(r"fn\s+verify_\w+".into()), ..Default::default() },
        ];
        let src = files(&[("db/m1.sql", "DROP TABLE users;"), ("src/lib.rs", "fn verify_token() {}")]);
        let v = intent.verify(&src).unwrap();
        assert_eq!(v.len(), 1);
        assert!(v[0].detail.contains("db/m1.sql"));
    }

    #[test]
    fn validation_rejects_bad_records() {
        let mut intent = payment_intent();
        intent.guards[0].must_contain = None;
        assert!(intent.validate().is_err());
        let mut intent = payment_intent();
        intent.id = "nope".into();
        assert!(intent.validate().is_err());
    }

    #[test]
    fn save_load_find() {
        let dir = tempfile::tempdir().unwrap();
        let intent = payment_intent();
        save(dir.path(), &intent).unwrap();
        let all = load_all(dir.path()).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(find(&all, "int-8a").unwrap().1.id, "int-8a92c1");
        assert!(find(&all, "int-zz").is_err());
    }

    #[test]
    fn ids_have_the_expected_shape() {
        let id = new_id();
        assert!(id.starts_with("int-") && id.len() == 10, "{id}");
    }
}
