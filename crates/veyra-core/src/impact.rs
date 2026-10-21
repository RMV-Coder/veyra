//! Impact analysis: what a change can affect, with the evidence for every claim.
//!
//! Deterministic and heuristic by design — nothing is inferred that the report can't point at:
//! * **Dependents**: files that import a changed file (per-language module resolution) and/or
//!   mention a changed symbol by name. Both together is high confidence; one alone is medium
//!   (import) or low (name only — it may be a different symbol with the same name).
//! * **Tests**: test files among the dependents, test files named after a changed file, and
//!   in-file Rust tests.
//! * **Co-change history**: files that historically changed in the same commits.
//! * **Risk**: a score with the reasons that produced it, plus the analysis' limitations.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::Result;
use gix::ObjectId;
use gix::bstr::ByteSlice;
use gix::objs::tree::EntryKind;
use serde::Serialize;
use veyra_semantic::{ChangeKind, Lang};

use crate::Repo;
use crate::flat::{self, FlatTree};

/// Files larger than this are not parsed for the dependency index.
const MAX_PARSE_BYTES: usize = 512 * 1024;
/// At most this many files are parsed for the dependency index.
const MAX_INDEXED_FILES: usize = 20_000;
/// Names too generic to count as a reference.
const COMMON_NAMES: &[&str] = &[
    "new",
    "init",
    "main",
    "get",
    "set",
    "run",
    "default",
    "from",
    "into",
    "len",
    "test",
    "self",
    "this",
    "value",
    "data",
    "name",
    "path",
    "error",
    "result",
    "item",
    "items",
    "key",
    "map",
    "list",
    "next",
    "push",
    "pop",
    "add",
    "remove",
    "update",
    "create",
    "delete",
    "read",
    "write",
    "open",
    "close",
    "start",
    "stop",
    "call",
    "apply",
    "build",
    "parse",
    "load",
    "save",
    "clone",
    "drop",
    "fmt",
    "eq",
    "hash",
    "id",
    "index",
    "size",
    "count",
    "render",
    "handle",
    "process",
    "execute",
    "toString",
    "constructor",
    "__init__",
    "setUp",
    "tearDown",
];

/// Options for [`Repo::impact`].
#[derive(Debug, Clone)]
pub struct ImpactOptions {
    /// How many commits of history to mine for co-changes (0 disables it).
    pub history_limit: usize,
}

impl Default for ImpactOptions {
    fn default() -> Self {
        ImpactOptions { history_limit: 500 }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangedSymbol {
    pub name: String,
    pub kind: String,
    pub change: String,
    /// The symbol's old name, for renames.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangedFile {
    pub path: String,
    pub status: char,
    pub language: Option<String>,
    pub added_lines: usize,
    pub removed_lines: usize,
    pub symbols: Vec<ChangedSymbol>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Serialize)]
pub struct Dependent {
    pub path: String,
    pub confidence: Confidence,
    pub evidence: Vec<String>,
    pub is_test: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TestSuggestion {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CoChange {
    pub path: String,
    /// Commits that changed this file together with one of the changed files…
    pub together: usize,
    /// …out of the commits that changed any of the changed files.
    pub of: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Serialize)]
pub struct Risk {
    pub level: RiskLevel,
    pub score: u32,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImpactReport {
    pub changed_files: Vec<ChangedFile>,
    pub dependents: Vec<Dependent>,
    pub tests: Vec<TestSuggestion>,
    pub co_changes: Vec<CoChange>,
    pub risk: Risk,
    pub limitations: Vec<String>,
}

/// Does this path look like a test?
pub fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let file = lower.rsplit('/').next().unwrap_or(&lower);
    let original_file = path.rsplit('/').next().unwrap_or(path);
    lower.starts_with("test/")
        || lower.starts_with("tests/")
        || lower.contains("/test/")
        || lower.contains("/tests/")
        || lower.contains("__tests__/")
        || lower.contains("/spec/")
        || file.starts_with("test_")
        || file.contains("_test.")
        || file.contains(".test.")
        || file.contains(".spec.")
        || file.contains("_spec.")
        // JVM, .NET, Swift and PHP conventions: `TestUser.java`, `UserTest.kt`, `UserTests.cs`, `UserSpec.scala`.
        || ([".java", ".kt", ".scala", ".cs", ".swift", ".php"].iter().any(|e| original_file.ends_with(e))
            && (original_file.starts_with("Test")
                || original_file.contains("Test.")
                || original_file.contains("Tests.")
                || original_file.contains("Spec.")))
}

/// The path without its extension, and with index-like file names folded into their directory
/// (`src/util/index.ts` → `src/util`, `pkg/__init__.py` → `pkg`, `src/x/mod.rs` → `src/x`).
fn module_stem(path: &str) -> String {
    let stem = path.rsplit_once('.').map(|(s, _)| s).unwrap_or(path);
    for index in ["/index", "/__init__", "/mod"] {
        if let Some(dir) = stem.strip_suffix(index) {
            return dir.to_owned();
        }
    }
    stem.to_owned()
}

fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// Join `rel` (with `./` and `../`) onto `dir`.
fn join_relative(dir: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn ends_with_segments(stem: &str, suffix: &str) -> bool {
    !suffix.is_empty() && (stem == suffix || stem.ends_with(&format!("/{suffix}")))
}

/// Could `import` (written in `importer`) refer to the file `target`?
pub fn import_resolves_to(lang: Lang, importer: &str, import: &veyra_semantic::Import, target: &str) -> bool {
    let stem = module_stem(target);
    let module = import.module.as_str();
    match lang {
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
            if module.starts_with('.') {
                let resolved = join_relative(dir_of(importer), module);
                module_stem(&resolved) == stem || resolved == stem
            } else {
                // Path aliases such as `@/lib/x` or `~/lib/x`.
                let trimmed = module.trim_start_matches("@/").trim_start_matches("~/");
                trimmed != module && ends_with_segments(&stem, trimmed)
            }
        }
        Lang::Python => {
            let dots = module.chars().take_while(|c| *c == '.').count();
            let rest = module[dots..].replace('.', "/");
            let base = if dots > 0 {
                let mut dir = dir_of(importer).to_owned();
                for _ in 1..dots {
                    dir = dir_of(&dir).to_owned();
                }
                join_relative(&dir, &rest)
            } else {
                rest
            };
            let candidates = std::iter::once(base.clone()).chain(import.names.iter().map(|n| join_relative(&base, n)));
            candidates.into_iter().any(|c| if dots > 0 { c == stem } else { ends_with_segments(&stem, &c) })
        }
        Lang::Rust => {
            let segs: Vec<&str> = module.split("::").collect();
            let (anchor, rest): (String, &[&str]) = match segs.first().copied() {
                Some("crate") => (rust_crate_root(importer), &segs[1..]),
                Some("self") => (rust_module_dir(importer), &segs[1..]),
                Some("super") => (dir_of(&rust_module_dir(importer)).to_owned(), &segs[1..]),
                Some(name) => match rust_crate_dir_for(name, target) {
                    Some(root) => (root, &segs[1..]),
                    None => return false,
                },
                None => return false,
            };
            let mut paths: Vec<String> = Vec::new();
            for k in (0..=rest.len()).rev() {
                paths.push(join_relative(&anchor, &rest[..k].join("/")));
            }
            for name in &import.names {
                let mut with = rest.to_vec();
                with.push(name);
                paths.push(join_relative(&anchor, &with.join("/")));
            }
            paths.iter().any(|p| {
                *p == stem
                    || (p == &anchor
                        && (target.ends_with("/lib.rs") || target.ends_with("/main.rs"))
                        && dir_of(target) == anchor)
            })
        }
        Lang::Go => {
            let dir = dir_of(target);
            !dir.is_empty() && (module == dir || module.ends_with(&format!("/{dir}")))
        }
        Lang::Java => {
            let base = module.replace('.', "/");
            if import.names.is_empty() {
                ends_with_segments(dir_of(target), &base)
            } else {
                import.names.iter().any(|n| ends_with_segments(&stem, &format!("{base}/{n}")))
            }
        }
        Lang::Kotlin | Lang::Scala => {
            // Like Java, except that a lower-case name is a top-level function or value, which
            // can live in any file of the package.
            let base = module.replace('.', "/");
            if import.names.is_empty() {
                ends_with_segments(dir_of(target), &base)
            } else {
                import.names.iter().any(|n| {
                    ends_with_segments(&stem, &format!("{base}/{n}"))
                        || (n.starts_with(char::is_lowercase) && ends_with_segments(dir_of(target), &base))
                })
            }
        }
        Lang::C | Lang::Cpp => {
            // `#include "x.h"`: next to the includer, or below an include directory.
            module == target
                || join_relative(dir_of(importer), module) == target
                || target.ends_with(&format!("/{module}"))
        }
        Lang::CSharp => {
            // Namespaces usually mirror directories, often without the root namespace.
            let segs: Vec<&str> = module.split('.').collect();
            let dir = dir_of(target);
            let shortest = segs.len().min(2);
            (0..segs.len())
                .any(|skip| segs.len() - skip >= shortest && ends_with_segments(dir, &segs[skip..].join("/")))
        }
        Lang::Ruby => {
            if module.starts_with('.') {
                join_relative(dir_of(importer), module) == stem
            } else {
                ends_with_segments(&stem, module)
            }
        }
        Lang::Php => {
            if module.contains('/') || module.ends_with(".php") {
                join_relative(dir_of(importer), module) == target || ends_with_segments(target, module)
            } else {
                // PSR-4: `App\Models\User` lives in `app/Models/User.php` or `src/Models/User.php`.
                let base = module.replace('\\', "/");
                import.names.iter().any(|n| {
                    let full = format!("{base}/{n}");
                    let below_root = full.split_once('/').map(|(_, rest)| rest).unwrap_or(&full);
                    ends_with_segments(&stem, &full) || ends_with_segments(&stem, below_root)
                })
            }
        }
        Lang::Swift => {
            // A module is a target: `Sources/<Module>/…`.
            let name = module.split('.').next().unwrap_or(module);
            target.starts_with(&format!("Sources/{name}/")) || target.contains(&format!("/Sources/{name}/"))
        }
        Lang::Bash => {
            // `source "$DIR/lib.sh"`: drop a leading variable.
            let module = if module.starts_with('$') { module.split_once('/').map_or("", |(_, r)| r) } else { module };
            let module = module.trim_start_matches("./");
            !module.is_empty()
                && (join_relative(dir_of(importer), module) == target
                    || module == target
                    || target.ends_with(&format!("/{module}")))
        }
        Lang::Lua => {
            let path = module.replace('.', "/");
            ends_with_segments(&stem, &path) || stem.strip_suffix("/init").is_some_and(|s| ends_with_segments(s, &path))
        }
        Lang::Elixir => {
            // `Acme.UserRepo` lives in `lib/acme/user_repo.ex`.
            let path = module.split('.').map(snake_case).collect::<Vec<_>>().join("/");
            ends_with_segments(&stem, &path)
                || import.names.iter().any(|n| ends_with_segments(&stem, &format!("{path}/{}", snake_case(n))))
        }
    }
}

/// `UserRepo` → `user_repo`, `HTTPClient` → `http_client` (Elixir's `Macro.underscore`).
fn snake_case(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_uppercase() {
            let prev = i.checked_sub(1).map(|p| chars[p]);
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            if prev.is_some_and(|p| p.is_lowercase() || p.is_ascii_digit() || (p.is_uppercase() && next_lower)) {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// `crates/foo/src/a/b.rs` → `crates/foo/src`.
fn rust_crate_root(file: &str) -> String {
    match file.rfind("/src/").or_else(|| file.starts_with("src/").then_some(0)) {
        Some(0) if file.starts_with("src/") => "src".to_owned(),
        Some(i) => file[..i + 4].to_owned(),
        None => dir_of(file).to_owned(),
    }
}

/// The directory holding a Rust file's child modules: `a/b.rs` → `a/b`, `a/mod.rs` → `a`.
fn rust_module_dir(file: &str) -> String {
    let name = file.rsplit('/').next().unwrap_or(file);
    if matches!(name, "mod.rs" | "lib.rs" | "main.rs") { dir_of(file).to_owned() } else { module_stem(file) }
}

/// For `use other_crate::x`, the `src` dir of that crate when `target` belongs to it.
fn rust_crate_dir_for(crate_name: &str, target: &str) -> Option<String> {
    let dashed = crate_name.replace('_', "-");
    for name in [dashed.as_str(), crate_name] {
        let marker = format!("{name}/src/");
        if target.starts_with(&marker) {
            return Some(format!("{name}/src"));
        }
        if let Some(i) = target.find(&format!("/{marker}")) {
            return Some(target[..i + 1 + marker.len() - 1].to_owned());
        }
    }
    None
}

struct Indexed {
    lang: Lang,
    imports: Vec<veyra_semantic::Import>,
    identifiers: BTreeSet<String>,
}

fn index_files(files: Vec<(String, Lang, String)>) -> HashMap<String, Indexed> {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 16);
    let chunk = files.len().div_ceil(threads).max(1);
    let mut out = HashMap::new();
    std::thread::scope(|scope| {
        let handles: Vec<_> = files
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || {
                    part.iter()
                        .filter_map(|(path, lang, text)| {
                            let parsed = veyra_semantic::parse(*lang, text)?;
                            Some((
                                path.clone(),
                                Indexed {
                                    lang: *lang,
                                    imports: veyra_semantic::imports(&parsed),
                                    identifiers: veyra_semantic::identifiers(&parsed),
                                },
                            ))
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for h in handles {
            if let Ok(part) = h.join() {
                out.extend(part);
            }
        }
    });
    out
}

/// Sensitive kinds of files, with a reason.
fn sensitive(path: &str) -> Option<&'static str> {
    let lower = path.to_ascii_lowercase();
    let file = lower.rsplit('/').next().unwrap_or(&lower);
    if lower.ends_with(".sql") || lower.contains("migrations/") || lower.contains("/migrate/") {
        Some("database migration or schema")
    } else if lower.ends_with(".tf") || lower.ends_with(".tfvars") || lower.contains("k8s/") || lower.contains("helm/")
    {
        Some("infrastructure definition")
    } else if lower.starts_with(".github/workflows/") || file == ".gitlab-ci.yml" || file == "jenkinsfile" {
        Some("CI pipeline")
    } else if file == "dockerfile" || file.starts_with("docker-compose") {
        Some("container build/deployment")
    } else if matches!(
        file,
        "cargo.toml" | "package.json" | "go.mod" | "pyproject.toml" | "requirements.txt" | "pom.xml" | "build.gradle"
    ) {
        Some("dependency manifest")
    } else if lower.contains("auth") || lower.contains("security") || lower.contains("crypto") {
        Some("security-sensitive code")
    } else {
        None
    }
}

impl Repo {
    /// Analyse the impact of changing `old` into `new` (both trees whose blobs are in the ODB).
    pub fn impact(&self, old: &FlatTree, new: &FlatTree, options: &ImpactOptions) -> Result<ImpactReport> {
        let changes = flat::diff(old, new);
        let mut limitations = vec![
            "References are matched by name, without type resolution: same-named symbols elsewhere can show up as \
             low-confidence dependents."
                .to_owned(),
            "Dynamic imports, reflection, macros, code generation and cross-repository users are not seen.".to_owned(),
        ];

        // 1. What changed, symbol by symbol.
        let semantic = self.semantic_diff_changes(&changes)?;
        let mut changed_files = Vec::new();
        let mut unsupported = 0;
        for (change, sem) in changes.iter().zip(&semantic) {
            let path = change.path.to_str_lossy().into_owned();
            let (mut added, mut removed) = (0, 0);
            let is_blob = |e: Option<crate::FlatEntry>| {
                e.is_none_or(|e| matches!(e.kind, EntryKind::Blob | EntryKind::BlobExecutable))
            };
            if is_blob(change.old) && is_blob(change.new) {
                let o = self.side_bytes(change.old, &change.path, false)?;
                let n = self.side_bytes(change.new, &change.path, false)?;
                if !crate::diff::is_binary(&o) && !crate::diff::is_binary(&n) {
                    let (_, a, r) = crate::diff::unified(&o, &n, 0);
                    (added, removed) = (a, r);
                }
            }
            let symbols = sem
                .diff
                .as_ref()
                .map(|d| {
                    d.changes
                        .iter()
                        .filter(|c| c.change != ChangeKind::Reformatted)
                        .map(|c| ChangedSymbol {
                            name: c.name.clone(),
                            kind: c.kind.label().to_owned(),
                            change: c.change.label().to_owned(),
                            old_name: c.old_name.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            if sem.diff.is_none() && Lang::from_path(&path).is_none() {
                unsupported += 1;
            }
            changed_files.push(ChangedFile {
                path,
                status: change.status_letter(),
                language: sem.diff.as_ref().map(|d| d.lang.name().to_owned()),
                added_lines: added,
                removed_lines: removed,
                symbols,
            });
        }
        if unsupported > 0 {
            limitations.push(format!(
                "{unsupported} changed file(s) have no syntax support; only their co-change history is analysed."
            ));
        }

        // 2. Dependency index of the new tree.
        let mut to_index = Vec::new();
        let mut skipped = 0;
        for (path, entry) in new {
            if !matches!(entry.kind, EntryKind::Blob | EntryKind::BlobExecutable) {
                continue;
            }
            let p = path.to_str_lossy();
            let Some(lang) = Lang::from_path(&p) else { continue };
            if to_index.len() >= MAX_INDEXED_FILES {
                skipped += 1;
                continue;
            }
            let data = self.read_blob(entry.id)?;
            if data.len() > MAX_PARSE_BYTES {
                skipped += 1;
                continue;
            }
            if let Ok(text) = String::from_utf8(data) {
                to_index.push((p.into_owned(), lang, text));
            }
        }
        if skipped > 0 {
            limitations.push(format!("{skipped} large or excess source file(s) were not indexed."));
        }
        let index = index_files(to_index);

        // 3. Dependents.
        let changed_paths: BTreeSet<&str> = changed_files.iter().map(|f| f.path.as_str()).collect();
        let mut evidence: BTreeMap<String, (bool, bool, Vec<String>)> = BTreeMap::new(); // (import, reference, evidence)
        for (path, info) in &index {
            if changed_paths.contains(path.as_str()) {
                continue;
            }
            for file in &changed_files {
                if file.status == 'A' {
                    continue; // nothing can depend on a brand-new file yet (other than in this change)
                }
                if let Some(imp) = info.imports.iter().find(|i| import_resolves_to(info.lang, path, i, &file.path)) {
                    let e = evidence.entry(path.clone()).or_default();
                    e.0 = true;
                    e.2.push(format!("imports {} (`{}`, line {})", file.path, imp.module, imp.line));
                }
            }
        }
        // Symbols whose users may break or behave differently.
        let mut watched: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new(); // name → (file, change)
        for file in &changed_files {
            for s in &file.symbols {
                if s.change == "added" {
                    continue;
                }
                let name = s.old_name.as_deref().unwrap_or(&s.name);
                let short = name.rsplit("::").next().unwrap_or(name).to_owned();
                if short.chars().count() < 3 || COMMON_NAMES.contains(&short.as_str()) {
                    continue;
                }
                watched.entry(short).or_default().push((file.path.clone(), s.change.clone()));
            }
        }
        for (path, info) in &index {
            if changed_paths.contains(path.as_str()) {
                continue;
            }
            for (name, sources) in &watched {
                if info.identifiers.contains(name) {
                    let e = evidence.entry(path.clone()).or_default();
                    e.1 = true;
                    let (src, change) = &sources[0];
                    e.2.push(format!("mentions `{name}` ({change} in {src})"));
                }
            }
        }
        let mut dependents: Vec<Dependent> = evidence
            .into_iter()
            .map(|(path, (import, reference, evidence))| Dependent {
                confidence: match (import, reference) {
                    (true, true) => Confidence::High,
                    (true, false) => Confidence::Medium,
                    _ => Confidence::Low,
                },
                is_test: is_test_path(&path),
                path,
                evidence,
            })
            .collect();
        dependents.sort_by(|a, b| a.confidence.cmp(&b.confidence).then_with(|| a.path.cmp(&b.path)));

        // 4. Tests.
        let mut tests: BTreeMap<String, String> = BTreeMap::new();
        for file in &changed_files {
            if is_test_path(&file.path) && file.status != 'D' {
                tests.entry(file.path.clone()).or_insert_with(|| "changed in this change".into());
            }
        }
        for d in dependents.iter().filter(|d| d.is_test && d.confidence != Confidence::Low) {
            tests.entry(d.path.clone()).or_insert_with(|| d.evidence[0].clone());
        }
        let all_paths: Vec<String> = new.keys().map(|p| p.to_str_lossy().into_owned()).collect();
        for file in changed_files.iter().filter(|f| !is_test_path(&f.path)) {
            let file_name = file.path.rsplit('/').next().unwrap_or(&file.path);
            let stem = file_name.split('.').next().unwrap_or(file_name);
            if stem.len() < 3 || matches!(stem, "lib" | "mod" | "main" | "index" | "__init__") {
                continue;
            }
            let stem_lower = stem.to_ascii_lowercase();
            for candidate in all_paths.iter().filter(|p| is_test_path(p)) {
                let cand_file = candidate.rsplit('/').next().unwrap_or(candidate).to_ascii_lowercase();
                let cand_stem = cand_file.split('.').next().unwrap_or(&cand_file);
                let cand_core = cand_stem
                    .trim_start_matches("test_")
                    .trim_end_matches("_test")
                    .trim_end_matches("_spec")
                    .trim_start_matches("test")
                    .trim_end_matches("test");
                if cand_core == stem_lower {
                    tests.entry(candidate.clone()).or_insert_with(|| format!("named after {}", file.path));
                }
            }
            if file.path.ends_with(".rs")
                && file.status != 'D'
                && let Some(entry) = new.get(file.path.as_bytes().as_bstr())
                && self.read_blob(entry.id)?.find(b"#[cfg(test)]").is_some()
            {
                tests.entry(file.path.clone()).or_insert_with(|| "contains its own unit tests".into());
            }
        }
        let tests: Vec<TestSuggestion> =
            tests.into_iter().map(|(path, reason)| TestSuggestion { path, reason }).collect();

        // 5. Co-change history.
        let co_changes = if options.history_limit > 0 {
            self.co_changes(&changed_paths, options.history_limit)?
        } else {
            Vec::new()
        };

        // 6. Risk.
        let mut score = 0u32;
        let mut reasons = Vec::new();
        let breaking: Vec<String> = changed_files
            .iter()
            .flat_map(|f| f.symbols.iter())
            .filter(|s| matches!(s.change.as_str(), "removed" | "renamed" | "signature changed"))
            .map(|s| s.old_name.clone().unwrap_or_else(|| s.name.clone()))
            .filter(|name| {
                let short = name.rsplit("::").next().unwrap_or(name);
                dependents.iter().any(|d| d.evidence.iter().any(|e| e.contains(&format!("`{short}`"))))
            })
            .collect();
        if !breaking.is_empty() {
            score += 3 * breaking.len().min(3) as u32;
            reasons.push(format!(
                "{} used elsewhere had its signature changed, was renamed or removed: {}",
                if breaking.len() == 1 { "a symbol" } else { "symbols" },
                breaking.join(", ")
            ));
        }
        let strong = dependents.iter().filter(|d| d.confidence != Confidence::Low).count();
        if strong > 0 {
            score += strong.min(10) as u32;
            reasons.push(format!("{strong} file(s) import a changed file"));
        }
        let source_changed = changed_files.iter().any(|f| f.language.is_some() && !is_test_path(&f.path));
        if source_changed && tests.is_empty() {
            score += 2;
            reasons.push("no tests found for the changed code".into());
        }
        let mut sensitive_kinds = BTreeSet::new();
        for f in &changed_files {
            if let Some(kind) = sensitive(&f.path) {
                sensitive_kinds.insert(kind);
            }
        }
        for kind in &sensitive_kinds {
            score += 2;
            reasons.push(format!("touches a {kind}"));
        }
        let lines: usize = changed_files.iter().map(|f| f.added_lines + f.removed_lines).sum();
        if lines > 500 {
            score += 2;
            reasons.push(format!("large change ({lines} lines)"));
        }
        let level = match score {
            0..=2 => RiskLevel::Low,
            3..=6 => RiskLevel::Medium,
            _ => RiskLevel::High,
        };

        Ok(ImpactReport {
            changed_files,
            dependents,
            tests,
            co_changes,
            risk: Risk { level, score, reasons },
            limitations,
        })
    }

    /// Files that changed together with `paths` in the last `limit` non-merge commits from HEAD.
    fn co_changes(&self, paths: &BTreeSet<&str>, limit: usize) -> Result<Vec<CoChange>> {
        let Some(head) = self.head_commit_id()? else { return Ok(Vec::new()) };
        let mut together: BTreeMap<String, usize> = BTreeMap::new();
        let mut of = 0;
        let walk = self.git.rev_walk([head]).all()?;
        for info in walk.take(limit) {
            let info = info?;
            let commit = self.git.find_commit(info.id)?;
            let parents: Vec<ObjectId> = commit.parent_ids().map(|p| p.detach()).collect();
            if parents.len() > 1 {
                continue;
            }
            let new_tree = commit.tree_id()?.detach();
            let old_tree = match parents.first() {
                Some(p) => Some(self.commit_tree_id(*p)?),
                None => None,
            };
            let touched = self.changed_paths(old_tree, Some(new_tree))?;
            if touched.len() > 100 {
                continue; // sweeping commits (formatting, vendoring) say nothing about coupling
            }
            if !touched.iter().any(|t| paths.contains(t.to_str_lossy().as_ref())) {
                continue;
            }
            of += 1;
            for t in &touched {
                let t = t.to_str_lossy();
                if !paths.contains(t.as_ref()) {
                    *together.entry(t.into_owned()).or_default() += 1;
                }
            }
        }
        let mut out: Vec<CoChange> = together
            .into_iter()
            .filter(|(_, n)| *n >= 2 && (*n as f64) / (of.max(1) as f64) >= 0.3)
            .map(|(path, n)| CoChange { path, together: n, of })
            .collect();
        out.sort_by(|a, b| b.together.cmp(&a.together).then_with(|| a.path.cmp(&b.path)));
        out.truncate(10);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use veyra_semantic::Import;

    fn imp(module: &str, names: &[&str]) -> Import {
        Import { module: module.into(), names: names.iter().map(|s| s.to_string()).collect(), line: 1 }
    }

    #[test]
    fn javascript_and_typescript_resolution() {
        let i = imp("./util", &[]);
        assert!(import_resolves_to(Lang::TypeScript, "src/app.ts", &i, "src/util.ts"));
        assert!(import_resolves_to(Lang::TypeScript, "src/app.ts", &i, "src/util/index.ts"));
        assert!(!import_resolves_to(Lang::TypeScript, "src/app.ts", &i, "lib/util.ts"));
        assert!(import_resolves_to(Lang::JavaScript, "src/a/b.js", &imp("../c", &[]), "src/c.jsx"));
        assert!(import_resolves_to(Lang::Tsx, "app/page.tsx", &imp("@/lib/db", &[]), "src/lib/db.ts"));
        assert!(!import_resolves_to(Lang::TypeScript, "src/app.ts", &imp("react", &[]), "src/react.ts"));
    }

    #[test]
    fn python_resolution() {
        assert!(import_resolves_to(Lang::Python, "app/main.py", &imp("app.models", &[]), "app/models.py"));
        assert!(import_resolves_to(Lang::Python, "app/main.py", &imp("app", &["models"]), "app/models.py"));
        assert!(import_resolves_to(
            Lang::Python,
            "app/api/v1.py",
            &imp("..models", &["User"]),
            "app/models/__init__.py"
        ));
        assert!(import_resolves_to(Lang::Python, "app/main.py", &imp(".models", &[]), "app/models.py"));
        assert!(!import_resolves_to(Lang::Python, "app/main.py", &imp("os.path", &[]), "app/models.py"));
    }

    #[test]
    fn rust_resolution() {
        let t = "crates/core/src/merge.rs";
        assert!(import_resolves_to(Lang::Rust, "crates/core/src/pick.rs", &imp("crate::merge", &["MergeOutcome"]), t));
        assert!(import_resolves_to(Lang::Rust, "crates/core/src/pick.rs", &imp("crate", &["merge"]), t));
        assert!(import_resolves_to(Lang::Rust, "crates/cli/src/main.rs", &imp("core::merge", &["X"]), t));
        assert!(import_resolves_to(Lang::Rust, "crates/core/src/lib.rs", &imp("self::merge", &[]), t));
        assert!(import_resolves_to(Lang::Rust, "src/main.rs", &imp("crate::a::b", &[]), "src/a/b/mod.rs"));
        assert!(!import_resolves_to(Lang::Rust, "crates/core/src/pick.rs", &imp("std::fmt", &[]), t));
        assert!(!import_resolves_to(Lang::Rust, "crates/core/src/pick.rs", &imp("crate::diff", &[]), t));
    }

    #[test]
    fn go_and_java_resolution() {
        assert!(import_resolves_to(
            Lang::Go,
            "cmd/x/main.go",
            &imp("github.com/me/p/internal/util", &[]),
            "internal/util/str.go"
        ));
        assert!(!import_resolves_to(Lang::Go, "cmd/x/main.go", &imp("fmt", &[]), "internal/util/str.go"));
        assert!(import_resolves_to(
            Lang::Java,
            "src/main/java/a/App.java",
            &imp("com.acme.util", &["Strings"]),
            "src/main/java/com/acme/util/Strings.java"
        ));
    }

    #[test]
    fn resolution_in_more_languages() {
        let r = import_resolves_to;
        // C/C++ includes: relative to the includer, or below an include directory.
        assert!(r(Lang::C, "src/main.c", &imp("util.h", &[]), "src/util.h"));
        assert!(r(Lang::Cpp, "src/app/main.cpp", &imp("geo/shape.hpp", &[]), "include/geo/shape.hpp"));
        assert!(!r(Lang::C, "src/main.c", &imp("stdio.h", &[]), "src/util.h"));
        // C#: namespace ↔ directory, with or without the root namespace.
        assert!(r(Lang::CSharp, "Api/Program.cs", &imp("Acme.Core.Models", &[]), "Core/Models/User.cs"));
        assert!(!r(Lang::CSharp, "Api/Program.cs", &imp("Acme.Core.Models", &[]), "Other/User.cs"));
        // Ruby.
        assert!(r(Lang::Ruby, "app/main.rb", &imp("./models/user", &[]), "app/models/user.rb"));
        assert!(r(Lang::Ruby, "bin/run.rb", &imp("acme/billing", &[]), "lib/acme/billing.rb"));
        // PHP: PSR-4 namespaces and include paths.
        assert!(r(Lang::Php, "app/Http/C.php", &imp("App\\Models", &["User"]), "app/Models/User.php"));
        assert!(r(Lang::Php, "src/a.php", &imp("App\\Models", &["User"]), "src/Models/User.php"));
        assert!(r(Lang::Php, "public/index.php", &imp("../lib/helpers.php", &[]), "lib/helpers.php"));
        // Kotlin / Scala: classes by name, top-level functions by package.
        assert!(r(
            Lang::Kotlin,
            "a/App.kt",
            &imp("com.acme.models", &["User"]),
            "src/main/kotlin/com/acme/models/User.kt"
        ));
        assert!(r(
            Lang::Kotlin,
            "a/App.kt",
            &imp("com.acme.util", &["slugify"]),
            "src/main/kotlin/com/acme/util/Strings.kt"
        ));
        assert!(!r(
            Lang::Kotlin,
            "a/App.kt",
            &imp("com.acme.models", &["User"]),
            "src/main/kotlin/com/acme/models/Team.kt"
        ));
        assert!(r(Lang::Scala, "a/App.scala", &imp("com.acme", &["Repo"]), "src/main/scala/com/acme/Repo.scala"));
        // Swift modules are package targets.
        assert!(r(Lang::Swift, "Sources/App/main.swift", &imp("Networking", &[]), "Sources/Networking/Client.swift"));
        assert!(!r(Lang::Swift, "Sources/App/main.swift", &imp("Foundation", &[]), "Sources/Networking/Client.swift"));
        // Bash, Lua, Elixir.
        assert!(r(Lang::Bash, "scripts/deploy.sh", &imp("./lib/common.sh", &[]), "scripts/lib/common.sh"));
        assert!(r(Lang::Bash, "scripts/deploy.sh", &imp("$DIR/lib/common.sh", &[]), "scripts/lib/common.sh"));
        assert!(r(Lang::Lua, "main.lua", &imp("lib.util", &[]), "lib/util.lua"));
        assert!(r(Lang::Lua, "main.lua", &imp("lib", &[]), "lib/init.lua"));
        assert!(r(Lang::Elixir, "lib/acme/web.ex", &imp("Acme.UserRepo", &[]), "lib/acme/user_repo.ex"));
        assert!(r(Lang::Elixir, "lib/acme/web.ex", &imp("Acme", &["HTTPClient"]), "lib/acme/http_client.ex"));
        assert_eq!(snake_case("HTTPClient"), "http_client");
        assert_eq!(snake_case("UserRepo2"), "user_repo2");
    }

    #[test]
    fn test_paths() {
        for p in [
            "tests/cli.rs",
            "src/__tests__/a.ts",
            "a.test.ts",
            "b.spec.js",
            "test_x.py",
            "x_test.go",
            "src/FooTest.java",
        ] {
            assert!(is_test_path(p), "{p}");
        }
        for p in ["src/latest.rs", "src/contest.py", "src/Testing.md"] {
            assert!(!is_test_path(p), "{p}");
        }
    }
}
