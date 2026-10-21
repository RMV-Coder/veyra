//! Deterministic risk analyzers for changes that are expensive to get wrong: database
//! migrations (SQL and the common ORMs) and infrastructure (Terraform, Kubernetes, Dockerfiles,
//! CI workflows). Rules look only at what a change *adds* (plus resources it removes), so
//! existing code doesn't produce noise.
//!
//! Repositories can add their own rules in the committed `.veyra/analyzers.toml`:
//!
//! ```toml
//! [[rule]]
//! id = "no-raw-delete"
//! paths = ["db/**/*.sql"]
//! pattern = "(?i)delete\\s+from\\s+payments"
//! severity = "high"
//! message = "Payments are never hard-deleted; use the archive procedure"
//! ```

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    High,
    Medium,
    Low,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::High => "high",
            Severity::Medium => "medium",
            Severity::Low => "low",
        }
    }

    pub fn parse(s: &str) -> Option<Severity> {
        match s.to_ascii_lowercase().as_str() {
            "high" => Some(Severity::High),
            "medium" => Some(Severity::Medium),
            "low" => Some(Severity::Low),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Area {
    Migration,
    Infra,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub area: Area,
    pub severity: Severity,
    pub rule: String,
    pub path: String,
    /// 1-based line in the new version (0 for whole-file findings such as removed resources).
    pub line: usize,
    pub message: String,
    pub advice: String,
    /// The offending line, trimmed.
    pub excerpt: String,
}

/// A changed file to analyze.
pub struct ChangedFile<'a> {
    pub path: &'a str,
    pub old: Option<&'a str>,
    pub new: Option<&'a str>,
}

struct Rule {
    id: &'static str,
    severity: Severity,
    pattern: &'static str,
    /// The rule doesn't apply when this also matches (e.g. a WHERE clause). Empty: no exception.
    unless: &'static str,
    message: &'static str,
    advice: &'static str,
}

const fn rule(
    id: &'static str,
    severity: Severity,
    pattern: &'static str,
    message: &'static str,
    advice: &'static str,
) -> Rule {
    Rule { id, severity, pattern, unless: "", message, advice }
}

const fn rule_unless(
    id: &'static str,
    severity: Severity,
    pattern: &'static str,
    unless: &'static str,
    message: &'static str,
    advice: &'static str,
) -> Rule {
    Rule { id, severity, pattern, unless, message, advice }
}

use Severity::{High, Low, Medium};

/// SQL statements (checked per statement, comments stripped).
const SQL_RULES: &[Rule] = &[
    rule(
        "drop-table",
        High,
        r"(?i)\bdrop\s+table\b",
        "drops a table: its data is gone",
        "back up or archive first; deploy code that no longer uses it before dropping",
    ),
    rule(
        "drop-database",
        High,
        r"(?i)\bdrop\s+(database|schema)\b",
        "drops a database or schema",
        "make sure this is intended and backed up",
    ),
    rule(
        "drop-column",
        High,
        r"(?i)\balter\s+table\b.*\bdrop\s+column\b",
        "drops a column: its data is gone, and old code reading it breaks",
        "stop reading/writing it in a release before, then drop",
    ),
    rule("truncate", High, r"(?i)\btruncate\b", "deletes every row", "make sure this runs only where intended"),
    rule_unless(
        "delete-without-where",
        High,
        r"(?is)^\s*delete\s+from\s+\S+",
        r"(?i)\bwhere\b",
        "DELETE without WHERE removes every row",
        "add a WHERE clause",
    ),
    rule_unless(
        "update-without-where",
        High,
        r"(?is)^\s*update\s+\S+\s+set\b",
        r"(?i)\bwhere\b",
        "UPDATE without WHERE changes every row",
        "add a WHERE clause",
    ),
    rule_unless(
        "add-not-null-without-default",
        High,
        r"(?i)\badd\s+(column\s+)?\w+\s+[^,]*\bnot\s+null\b",
        r"(?i)\bdefault\b",
        "adds a NOT NULL column (fails on non-empty tables unless it has a DEFAULT)",
        "add a DEFAULT, or add it nullable, backfill, then set NOT NULL",
    ),
    rule(
        "set-not-null",
        Medium,
        r"(?i)\bset\s+not\s+null\b",
        "SET NOT NULL scans the whole table under a lock and fails if NULLs exist",
        "backfill first; on Postgres add a NOT VALID check constraint, validate it, then set NOT NULL",
    ),
    rule(
        "alter-column-type",
        Medium,
        r"(?i)\b(alter\s+column\s+\w+\s+(set\s+data\s+)?type|modify\s+(column\s+)?\w+)\b",
        "changes a column type: may rewrite the table under a lock or truncate data",
        "check the table size and the conversion; consider add-copy-swap",
    ),
    rule(
        "rename",
        Medium,
        r"(?i)\brename\s+(column\s+\w+\s+)?to\b|\brename\s+column\b",
        "renames a table or column: code still using the old name breaks",
        "expand/contract: add the new name, migrate readers and writers, then drop the old",
    ),
    rule_unless(
        "index-without-concurrently",
        Medium,
        r"(?i)\bcreate\s+(unique\s+)?index\b",
        r"(?i)\bconcurrently\b",
        "creates an index without CONCURRENTLY: blocks writes on Postgres while it builds",
        "use CREATE INDEX CONCURRENTLY (outside a transaction) on large tables",
    ),
    rule_unless(
        "fk-without-not-valid",
        Low,
        r"(?i)\badd\s+constraint\b.*\bforeign\s+key\b",
        r"(?i)\bnot\s+valid\b",
        "adds a foreign key that is validated immediately under a lock",
        "add it NOT VALID, then VALIDATE CONSTRAINT separately",
    ),
    rule(
        "grant-broad",
        Medium,
        r"(?i)\bgrant\s+all\b|\bto\s+public\b",
        "grants broad privileges",
        "grant only what is needed to specific roles",
    ),
];

/// ORM migration helpers (checked per added line).
const ORM_RULES: &[Rule] = &[
    rule(
        "orm-drop-table",
        High,
        r"\b(drop_table|dropTable|DeleteModel|op\.drop_table|dropTableIfExists)\b",
        "drops a table",
        "archive data first; remove code usage in an earlier release",
    ),
    rule(
        "orm-drop-column",
        High,
        r"\b(remove_column|dropColumn|RemoveField|op\.drop_column)\b",
        "drops a column",
        "stop using it in an earlier release, then drop",
    ),
    rule(
        "orm-rename",
        Medium,
        r"\b(rename_column|renameColumn|RenameField|RenameModel|rename_table|renameTable)\b",
        "renames a column or table: old code breaks during deploys",
        "expand/contract instead of a rename",
    ),
    rule(
        "orm-change-column",
        Medium,
        r"\b(change_column|alterColumn|AlterField|op\.alter_column)\b",
        "changes a column definition: possible lock, rewrite or data loss",
        "check table size and data compatibility",
    ),
];

const TERRAFORM_RULES: &[Rule] = &[
    rule(
        "tf-open-ingress",
        High,
        r#"(cidr_blocks|ipv6_cidr_blocks|source_ranges)\s*=\s*\[[^\]]*"(0\.0\.0\.0/0|::/0)""#,
        "opens access to the whole internet",
        "restrict to known ranges",
    ),
    rule(
        "tf-public",
        High,
        r#"\b(publicly_accessible\s*=\s*true|acl\s*=\s*"public-read(-write)?"|block_public_acls\s*=\s*false|map_public_ip_on_launch\s*=\s*true)"#,
        "makes a resource public",
        "keep it private unless it must be public, and document why",
    ),
    rule(
        "tf-wildcard-iam",
        High,
        r#"("Action"\s*:\s*"\*"|actions\s*=\s*\[\s*"\*"\s*\]|"Resource"\s*:\s*"\*")"#,
        "grants wildcard IAM permissions",
        "list the specific actions and resources",
    ),
    rule(
        "tf-no-protection",
        Medium,
        r#"\b(prevent_destroy\s*=\s*false|deletion_protection\s*=\s*false|force_destroy\s*=\s*true|skip_final_snapshot\s*=\s*true)"#,
        "removes a safety net against destroying data",
        "keep deletion protection on for stateful resources",
    ),
    rule(
        "tf-unencrypted",
        Medium,
        r#"\b(encrypted|storage_encrypted|encryption)\s*=\s*false"#,
        "disables encryption",
        "enable encryption at rest",
    ),
];

const KUBERNETES_RULES: &[Rule] = &[
    rule(
        "k8s-privileged",
        High,
        r"\bprivileged:\s*true\b",
        "runs a privileged container",
        "drop privileges; grant specific capabilities instead",
    ),
    rule(
        "k8s-host-namespaces",
        High,
        r"\b(hostNetwork|hostPID|hostIPC):\s*true\b",
        "shares the host's namespaces",
        "avoid host namespaces",
    ),
    rule(
        "k8s-root",
        Medium,
        r"\b(runAsUser:\s*0\b|runAsNonRoot:\s*false\b|allowPrivilegeEscalation:\s*true\b)",
        "runs as root or allows privilege escalation",
        "set runAsNonRoot: true and allowPrivilegeEscalation: false",
    ),
    rule(
        "k8s-latest-image",
        Medium,
        r#"\bimage:\s*["']?[\w./-]+(:latest)?["']?\s*$"#,
        "uses an untagged or :latest image: deploys aren't reproducible",
        "pin a version tag or digest",
    ),
    rule(
        "k8s-zero-replicas",
        Medium,
        r"\breplicas:\s*0\b",
        "scales to zero replicas",
        "make sure the outage is intended",
    ),
];

const DOCKER_RULES: &[Rule] = &[
    rule_unless(
        "docker-latest",
        Low,
        r"(?i)^\s*from\s+[\w./-]+(:latest)?(\s+as\s+\w+)?\s*$",
        r"(?i)^\s*from\s+scratch\b",
        "uses an untagged or :latest base image",
        "pin a version tag or digest",
    ),
    rule("docker-root", Medium, r"(?i)^\s*user\s+(root|0)\s*$", "runs as root", "switch to an unprivileged user"),
    rule(
        "docker-curl-pipe",
        Medium,
        r"(?i)\b(curl|wget)\b[^|]*\|\s*(ba)?sh\b",
        "pipes a download straight into a shell",
        "download, verify a checksum, then run",
    ),
];

const WORKFLOW_RULES: &[Rule] = &[
    rule(
        "ci-pull-request-target",
        High,
        r"\bpull_request_target\b",
        "pull_request_target runs with secrets on untrusted pull requests",
        "use pull_request, or never check out the PR's code in this workflow",
    ),
    rule(
        "ci-write-all",
        Medium,
        r"\bpermissions:\s*write-all\b",
        "gives the workflow token write access to everything",
        "grant only the permissions the job needs",
    ),
    rule(
        "ci-unpinned-action",
        Low,
        r"\buses:\s*[\w.-]+/[\w./-]+@(main|master|latest)\b",
        "uses an action from a moving branch",
        "pin to a release tag or commit SHA",
    ),
];

/// Kinds of files the analyzers understand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Sql,
    OrmMigration,
    Terraform,
    Kubernetes,
    Dockerfile,
    Workflow,
}

pub fn classify(path: &str, content: &str) -> Option<FileKind> {
    let lower = path.to_ascii_lowercase();
    let file = lower.rsplit('/').next().unwrap_or(&lower);
    let in_migrations =
        lower.contains("migrations/") || lower.contains("migrate/") || lower.contains("alembic/versions/");
    if lower.ends_with(".sql") {
        return Some(FileKind::Sql);
    }
    if in_migrations && [".rb", ".py", ".js", ".ts", ".php", ".java", ".kt", ".go"].iter().any(|e| lower.ends_with(e)) {
        return Some(FileKind::OrmMigration);
    }
    if lower.ends_with(".tf") || lower.ends_with(".tfvars") {
        return Some(FileKind::Terraform);
    }
    if (lower.starts_with(".github/workflows/") && (lower.ends_with(".yml") || lower.ends_with(".yaml")))
        || file == ".gitlab-ci.yml"
    {
        return Some(FileKind::Workflow);
    }
    if file == "dockerfile" || file.starts_with("dockerfile.") || file.ends_with(".dockerfile") {
        return Some(FileKind::Dockerfile);
    }
    if (lower.ends_with(".yml") || lower.ends_with(".yaml"))
        && content.contains("apiVersion:")
        && content.contains("kind:")
    {
        return Some(FileKind::Kubernetes);
    }
    None
}

impl FileKind {
    pub fn area(self) -> Area {
        match self {
            FileKind::Sql | FileKind::OrmMigration => Area::Migration,
            _ => Area::Infra,
        }
    }
}

/// Line numbers (1-based) of `new` that are not in `old` (a simple LCS-free approximation:
/// a line counts as added if it occurs more often in `new` than in `old`).
fn added_lines(old: Option<&str>, new: &str) -> BTreeSet<usize> {
    let Some(old) = old else { return (1..=new.lines().count()).collect() };
    let mut budget: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for l in old.lines() {
        *budget.entry(l.trim()).or_default() += 1;
    }
    let mut out = BTreeSet::new();
    for (i, l) in new.lines().enumerate() {
        match budget.get_mut(l.trim()) {
            Some(n) if *n > 0 => *n -= 1,
            _ => {
                out.insert(i + 1);
            }
        }
    }
    out
}

struct Compiled {
    pattern: Regex,
    unless: Option<Regex>,
}

impl Compiled {
    fn is_match(&self, text: &str) -> bool {
        self.pattern.is_match(text) && !self.unless.as_ref().is_some_and(|u| u.is_match(text))
    }
}

fn compiled(rules: &'static [Rule]) -> Vec<(&'static Rule, Compiled)> {
    rules
        .iter()
        .map(|r| {
            let pattern = Regex::new(r.pattern).expect("built-in rules are valid");
            let unless = (!r.unless.is_empty()).then(|| Regex::new(r.unless).expect("built-in rules are valid"));
            (r, Compiled { pattern, unless })
        })
        .collect()
}

/// Strip `--` and `/* */` comments from SQL, keeping line structure.
fn strip_sql_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let (mut in_block, mut in_line, mut in_str) = (false, false, false);
    while let Some(c) = chars.next() {
        if in_line {
            if c == '\n' {
                in_line = false;
                out.push('\n');
            }
            continue;
        }
        if in_block {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                in_block = false;
            } else if c == '\n' {
                out.push('\n');
            }
            continue;
        }
        if c == '\'' {
            in_str = !in_str;
        }
        if !in_str && c == '-' && chars.peek() == Some(&'-') {
            in_line = true;
            continue;
        }
        if !in_str && c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            in_block = true;
            continue;
        }
        out.push(c);
    }
    out
}

fn finding(area: Area, r: &Rule, path: &str, line: usize, excerpt: &str) -> Finding {
    Finding {
        area,
        severity: r.severity,
        rule: r.id.to_owned(),
        path: path.to_owned(),
        line,
        message: r.message.to_owned(),
        advice: r.advice.to_owned(),
        excerpt: excerpt.trim().chars().take(160).collect(),
    }
}

fn analyze_sql(path: &str, old: Option<&str>, new: &str, out: &mut Vec<Finding>) {
    let added = added_lines(old, new);
    let clean = strip_sql_comments(new);
    let rules = compiled(SQL_RULES);
    // Split into statements, remembering the line each starts on.
    let mut start_line = 1;
    let mut line = 1;
    let mut stmt = String::new();
    let mut stmt_lines = BTreeSet::new();
    let flush = |stmt: &str, first: usize, lines: &BTreeSet<usize>, out: &mut Vec<Finding>| {
        if stmt.trim().is_empty() || !lines.iter().any(|l| added.contains(l)) {
            return;
        }
        let flat = stmt.split_whitespace().collect::<Vec<_>>().join(" ");
        for (r, re) in &rules {
            if re.is_match(&flat) {
                let excerpt = new.lines().nth(first - 1).unwrap_or(&flat);
                out.push(finding(Area::Migration, r, path, first, excerpt));
            }
        }
    };
    for c in clean.chars() {
        if c == ';' {
            flush(&stmt, start_line, &stmt_lines, out);
            stmt.clear();
            stmt_lines.clear();
            continue;
        }
        if stmt.trim().is_empty() && !c.is_whitespace() {
            start_line = line;
        }
        if !c.is_whitespace() {
            stmt_lines.insert(line);
        }
        stmt.push(c);
        if c == '\n' {
            line += 1;
        }
    }
    flush(&stmt, start_line, &stmt_lines, out);
}

fn analyze_lines(area: Area, rules: &'static [Rule], path: &str, old: Option<&str>, new: &str, out: &mut Vec<Finding>) {
    let added = added_lines(old, new);
    let compiled = compiled(rules);
    for (i, l) in new.lines().enumerate() {
        if !added.contains(&(i + 1)) || l.trim_start().starts_with('#') || l.trim_start().starts_with("//") {
            continue;
        }
        for (r, re) in &compiled {
            if re.is_match(l) {
                out.push(finding(area, r, path, i + 1, l));
            }
        }
    }
}

/// `resource "type" "name"` blocks in Terraform.
fn terraform_resources(text: &str) -> BTreeSet<String> {
    let re = Regex::new(r#"(?m)^\s*resource\s+"([^"]+)"\s+"([^"]+)""#).expect("valid");
    re.captures_iter(text).map(|c| format!("{}.{}", &c[1], &c[2])).collect()
}

/// Run the built-in analyzers on one changed file.
pub fn analyze_file(file: &ChangedFile<'_>) -> Vec<Finding> {
    let mut out = Vec::new();
    let content = file.new.or(file.old).unwrap_or("");
    let Some(kind) = classify(file.path, content) else { return out };
    match (kind, file.new) {
        (FileKind::Terraform, new) => {
            let before = file.old.map(terraform_resources).unwrap_or_default();
            let after = new.map(terraform_resources).unwrap_or_default();
            for gone in before.difference(&after) {
                out.push(Finding {
                    area: Area::Infra,
                    severity: High,
                    rule: "tf-resource-removed".into(),
                    path: file.path.to_owned(),
                    line: 0,
                    message: format!("removes resource {gone}: `terraform apply` will destroy it"),
                    advice:
                        "if it was moved or renamed, add a `moved` block; otherwise confirm the destroy in the plan"
                            .into(),
                    excerpt: String::new(),
                });
            }
            if let Some(new) = new {
                analyze_lines(Area::Infra, TERRAFORM_RULES, file.path, file.old, new, &mut out);
            }
        }
        (FileKind::Sql, Some(new)) => analyze_sql(file.path, file.old, new, &mut out),
        (FileKind::OrmMigration, Some(new)) => {
            analyze_lines(Area::Migration, ORM_RULES, file.path, file.old, new, &mut out)
        }
        (FileKind::Kubernetes, Some(new)) => {
            analyze_lines(Area::Infra, KUBERNETES_RULES, file.path, file.old, new, &mut out)
        }
        (FileKind::Dockerfile, Some(new)) => {
            analyze_lines(Area::Infra, DOCKER_RULES, file.path, file.old, new, &mut out)
        }
        (FileKind::Workflow, Some(new)) => {
            analyze_lines(Area::Infra, WORKFLOW_RULES, file.path, file.old, new, &mut out)
        }
        _ => {}
    }
    out
}

// ------------------------------------------------------------------------------------------
// Repository rules (.veyra/analyzers.toml)

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomRule {
    pub id: String,
    pub paths: Vec<String>,
    pub pattern: String,
    #[serde(default = "default_severity")]
    pub severity: String,
    pub message: String,
    #[serde(default)]
    pub advice: String,
}

fn default_severity() -> String {
    "medium".into()
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, rename = "rule")]
    pub rules: Vec<CustomRule>,
    /// Built-in rule ids to turn off for this repository.
    #[serde(default)]
    pub disable: Vec<String>,
}

pub struct Analyzer {
    custom: Vec<(CustomRule, globset::GlobSet, Regex, Severity)>,
    disabled: BTreeSet<String>,
}

impl Analyzer {
    pub fn new(config: &Config) -> Result<Analyzer> {
        let mut custom = Vec::new();
        for r in &config.rules {
            let mut b = globset::GlobSetBuilder::new();
            for p in &r.paths {
                b.add(globset::Glob::new(p).with_context(|| format!("rule {}: bad path glob {p}", r.id))?);
            }
            let re = Regex::new(&r.pattern).with_context(|| format!("rule {}: bad pattern", r.id))?;
            let sev = Severity::parse(&r.severity)
                .with_context(|| format!("rule {}: severity must be high, medium or low", r.id))?;
            custom.push((r.clone(), b.build()?, re, sev));
        }
        Ok(Analyzer { custom, disabled: config.disable.iter().cloned().collect() })
    }

    pub fn from_toml(text: &str) -> Result<Analyzer> {
        Analyzer::new(&toml::from_str(text).context("invalid .veyra/analyzers.toml")?)
    }

    pub fn analyze(&self, files: &[ChangedFile<'_>]) -> Vec<Finding> {
        let mut out = Vec::new();
        for f in files {
            out.extend(analyze_file(f).into_iter().filter(|x| !self.disabled.contains(&x.rule)));
            let Some(new) = f.new else { continue };
            let added = added_lines(f.old, new);
            for (rule, globs, re, sev) in &self.custom {
                if !globs.is_match(f.path) {
                    continue;
                }
                for (i, l) in new.lines().enumerate() {
                    if added.contains(&(i + 1)) && re.is_match(l) {
                        out.push(Finding {
                            area: Area::Custom,
                            severity: *sev,
                            rule: rule.id.clone(),
                            path: f.path.to_owned(),
                            line: i + 1,
                            message: rule.message.clone(),
                            advice: rule.advice.clone(),
                            excerpt: l.trim().chars().take(160).collect(),
                        });
                    }
                }
            }
        }
        out.sort_by(|a, b| a.severity.cmp(&b.severity).then_with(|| a.path.cmp(&b.path)).then(a.line.cmp(&b.line)));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules_for(path: &str, old: Option<&str>, new: &str) -> Vec<String> {
        analyze_file(&ChangedFile { path, old, new: Some(new) }).into_iter().map(|f| f.rule).collect()
    }

    #[test]
    fn sql_rules() {
        let sql = "-- drop table old (comment, ignored)\nALTER TABLE users DROP COLUMN email;\nDELETE FROM sessions;\nDELETE FROM s WHERE id = 1;\nUPDATE users SET a = 1;\nUPDATE users SET a = 1 WHERE id = 2;\nALTER TABLE t ADD COLUMN c integer NOT NULL;\nALTER TABLE t ADD COLUMN d integer NOT NULL DEFAULT 0;\nCREATE INDEX i ON t (c);\nCREATE INDEX CONCURRENTLY j ON t (c);\n";
        let r = rules_for("db/migrations/001.sql", None, sql);
        assert_eq!(r.iter().filter(|x| *x == "drop-column").count(), 1, "{r:?}");
        assert_eq!(r.iter().filter(|x| *x == "delete-without-where").count(), 1, "{r:?}");
        assert_eq!(r.iter().filter(|x| *x == "update-without-where").count(), 1, "{r:?}");
        assert_eq!(r.iter().filter(|x| *x == "index-without-concurrently").count(), 1, "{r:?}");
        assert!(r.contains(&"add-not-null-without-default".to_string()), "{r:?}");
        assert!(!r.contains(&"drop-table".to_string()), "comments are ignored: {r:?}");
    }

    #[test]
    fn only_added_lines_count() {
        let old = "DROP TABLE legacy;\n";
        let new = "DROP TABLE legacy;\nSELECT 1;\n";
        assert!(rules_for("x.sql", Some(old), new).is_empty());
    }

    #[test]
    fn orm_terraform_kubernetes_docker_ci() {
        assert_eq!(
            rules_for("db/migrate/2024_x.rb", None, "  remove_column :users, :email\n"),
            vec!["orm-drop-column"]
        );
        let tf_old = "resource \"aws_s3_bucket\" \"logs\" {}\nresource \"aws_db_instance\" \"main\" {}\n";
        let tf_new = "resource \"aws_s3_bucket\" \"logs\" {\n  acl = \"public-read\"\n}\nresource \"aws_security_group\" \"sg\" {\n  cidr_blocks = [\"0.0.0.0/0\"]\n}\n";
        let r = rules_for("infra/main.tf", Some(tf_old), tf_new);
        assert!(
            r.contains(&"tf-resource-removed".into())
                && r.contains(&"tf-public".into())
                && r.contains(&"tf-open-ingress".into()),
            "{r:?}"
        );
        let k8s = "apiVersion: apps/v1\nkind: Deployment\nspec:\n  replicas: 0\n  template:\n    spec:\n      containers:\n        - image: nginx:latest\n          securityContext:\n            privileged: true\n        - image: nginx:1.27\n";
        let r = rules_for("k8s/web.yaml", None, k8s);
        assert_eq!(r, vec!["k8s-zero-replicas", "k8s-latest-image", "k8s-privileged"], "{r:?}");
        assert_eq!(
            rules_for("Dockerfile", None, "FROM node\nRUN curl -sL x.sh | bash\nUSER root\n"),
            vec!["docker-latest", "docker-curl-pipe", "docker-root"]
        );
        assert_eq!(
            rules_for(".github/workflows/ci.yml", None, "on: pull_request_target\n"),
            vec!["ci-pull-request-target"]
        );
        assert!(rules_for("src/main.rs", None, "DROP TABLE x;").is_empty());
    }

    #[test]
    fn custom_rules_and_disabling() {
        let a = Analyzer::from_toml(
            "disable = [\"index-without-concurrently\"]\n[[rule]]\nid = \"no-payments-delete\"\npaths = [\"db/**\"]\npattern = \"(?i)delete from payments\"\nseverity = \"high\"\nmessage = \"never hard-delete payments\"\n",
        )
        .unwrap();
        let files = [ChangedFile {
            path: "db/x.sql",
            old: None,
            new: Some("DELETE FROM payments WHERE id = 1;\nCREATE INDEX i ON t (c);\n"),
        }];
        let f = a.analyze(&files);
        let ids: Vec<_> = f.iter().map(|x| x.rule.as_str()).collect();
        assert_eq!(ids, vec!["no-payments-delete"], "{f:?}");
        assert!(Analyzer::from_toml("[[rule]]\nid='x'\npaths=['*']\npattern='('\nmessage='m'\n").is_err());
    }
}
