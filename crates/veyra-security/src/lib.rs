//! Secret detection for Veyra.
//!
//! This crate is pure: it scans text and knows nothing about repositories. Callers decide
//! what to scan (added lines of staged files, a tree, the worktree) and what to do with
//! findings (block, warn).
//!
//! Detection is a safety net, not a guarantee. It complements dedicated scanners.

use std::collections::{BTreeSet, HashSet};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Deserialize;

/// How sure a rule is that a match is a real secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    Medium,
    High,
}

impl std::fmt::Display for Confidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Confidence::Medium => "medium",
            Confidence::High => "high",
        })
    }
}

struct Rule {
    id: String,
    description: String,
    confidence: Confidence,
    regex: Regex,
    /// When set, the secret is this capture group instead of the whole match.
    group: Option<usize>,
}

/// One detected potential secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub rule_id: String,
    pub description: String,
    pub confidence: Confidence,
    pub path: String,
    /// 1-based line number.
    pub line: usize,
    /// The match with most characters masked, safe to print.
    pub redacted: String,
    /// Stable identifier for allowlisting: a hash of rule, path and secret (never the secret itself).
    pub fingerprint: String,
}

/// Repository policy, read from the committed `.veyra/secrets.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// Fingerprints of reviewed false positives.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Glob patterns of paths that are never scanned (e.g. `tests/fixtures/**`).
    #[serde(default)]
    pub exclude_paths: Vec<String>,
    /// Additional, project-specific rules.
    #[serde(default, rename = "rule")]
    pub rules: Vec<CustomRule>,
}

/// A project-specific rule from `.veyra/secrets.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomRule {
    pub id: String,
    pub description: String,
    pub pattern: String,
    #[serde(default = "default_confidence")]
    pub confidence: Confidence,
}

fn default_confidence() -> Confidence {
    Confidence::High
}

impl Policy {
    pub fn from_toml(text: &str) -> Result<Policy> {
        toml::from_str(text).context("invalid .veyra/secrets.toml")
    }
}

/// Built-in rules: `(id, description, confidence, pattern, secret capture group)`.
const BUILTIN: &[(&str, &str, Confidence, &str, Option<usize>)] = &[
    (
        "private-key",
        "Private key block",
        Confidence::High,
        r"-----BEGIN (?:RSA |EC |DSA |OPENSSH |PGP |ENCRYPTED )?PRIVATE KEY(?: BLOCK)?-----",
        None,
    ),
    ("aws-access-key-id", "AWS access key ID", Confidence::High, r"\b(?:AKIA|ASIA|ABIA|ACCA)[0-9A-Z]{16}\b", None),
    (
        "aws-secret-access-key",
        "AWS secret access key",
        Confidence::Medium,
        r#"(?i)aws.{0,20}(?:secret|private).{0,20}['"]([0-9a-zA-Z/+]{40})['"]"#,
        Some(1),
    ),
    ("github-token", "GitHub token", Confidence::High, r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36,255}\b", None),
    (
        "github-fine-grained-pat",
        "GitHub fine-grained personal access token",
        Confidence::High,
        r"\bgithub_pat_[A-Za-z0-9_]{60,255}\b",
        None,
    ),
    ("gitlab-pat", "GitLab personal access token", Confidence::High, r"\bglpat-[A-Za-z0-9_\-]{20,}\b", None),
    ("slack-token", "Slack token", Confidence::High, r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b", None),
    (
        "slack-webhook",
        "Slack incoming webhook",
        Confidence::High,
        r"https://hooks\.slack\.com/services/T[A-Za-z0-9_]+/B[A-Za-z0-9_]+/[A-Za-z0-9_]+",
        None,
    ),
    ("stripe-live-key", "Stripe live secret key", Confidence::High, r"\b(?:sk|rk)_live_[A-Za-z0-9]{20,}\b", None),
    ("google-api-key", "Google API key", Confidence::High, r"\bAIza[0-9A-Za-z\-_]{35}\b", None),
    (
        "openai-api-key",
        "OpenAI API key",
        Confidence::High,
        r"\bsk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9_\-]{20,}T3BlbkFJ[A-Za-z0-9_\-]{20,}\b|\bsk-proj-[A-Za-z0-9_\-]{40,}\b",
        None,
    ),
    (
        "anthropic-api-key",
        "Anthropic API key",
        Confidence::High,
        r"\bsk-ant-(?:api|admin)\d{2}-[A-Za-z0-9_\-]{80,}\b",
        None,
    ),
    ("npm-token", "npm access token", Confidence::High, r"\bnpm_[A-Za-z0-9]{36}\b", None),
    ("xendit-secret-key", "Xendit secret key", Confidence::High, r"\bxnd_production_[A-Za-z0-9]{20,}\b", None),
    (
        "xendit-development-key",
        "Xendit development key",
        Confidence::Medium,
        r"\bxnd_development_[A-Za-z0-9]{20,}\b",
        None,
    ),
    (
        "jwt",
        "JSON Web Token",
        Confidence::Medium,
        r"\beyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b",
        None,
    ),
    (
        "connection-string-password",
        "Password in a connection URL",
        Confidence::Medium,
        r"\b(?:postgres(?:ql)?|mysql|mariadb|mongodb(?:\+srv)?|redis|rediss|amqps?)://[^\s:@/]+:([^\s@/]{3,})@",
        Some(1),
    ),
    (
        "generic-secret-assignment",
        "Hard-coded credential assignment",
        Confidence::Medium,
        r#"(?i)\b(?:password|passwd|pwd|secret|api[_-]?key|access[_-]?token|auth[_-]?token|client[_-]?secret|private[_-]?key)\b["']?\s*[:=]\s*["']([^"'\s]{8,})["']"#,
        Some(1),
    ),
];

/// Values that are obviously placeholders, not secrets.
fn is_placeholder(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    const MARKERS: &[&str] = &[
        "changeme",
        "change-me",
        "change_me",
        "example",
        "placeholder",
        "your",
        "xxxx",
        "****",
        "dummy",
        "sample",
        "redacted",
        "<",
        "${",
        "{{",
        "process.env",
        "env(",
        "os.environ",
        "getenv",
        "secret_here",
        "password123",
        "replace",
        "todo",
        "test",
        "fake",
    ];
    MARKERS.iter().any(|m| lower.contains(m)) || value.chars().all(|c| c == value.chars().next().unwrap_or('x'))
}

/// Compiled rules plus policy.
pub struct Scanner {
    rules: Vec<Rule>,
    allow: HashSet<String>,
    exclude: Vec<Regex>,
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new(&Policy::default()).expect("built-in rules compile")
    }
}

impl Scanner {
    /// Built-in rules plus the policy's custom rules, allowlist and exclusions.
    pub fn new(policy: &Policy) -> Result<Scanner> {
        let mut rules = Vec::new();
        for (id, description, confidence, pattern, group) in BUILTIN {
            rules.push(Rule {
                id: (*id).to_owned(),
                description: (*description).to_owned(),
                confidence: *confidence,
                regex: Regex::new(pattern).with_context(|| format!("built-in rule {id} is invalid"))?,
                group: *group,
            });
        }
        for custom in &policy.rules {
            if rules.iter().any(|r| r.id == custom.id) {
                bail!("secret rule id '{}' is defined twice", custom.id);
            }
            rules.push(Rule {
                id: custom.id.clone(),
                description: custom.description.clone(),
                confidence: custom.confidence,
                regex: Regex::new(&custom.pattern)
                    .with_context(|| format!("secret rule '{}' has an invalid pattern", custom.id))?,
                group: None,
            });
        }
        let exclude = policy.exclude_paths.iter().map(|g| glob_to_regex(g)).collect::<Result<Vec<_>>>()?;
        Ok(Scanner { rules, allow: policy.allow.iter().cloned().collect(), exclude })
    }

    /// Is `path` excluded from scanning by policy?
    pub fn is_excluded(&self, path: &str) -> bool {
        self.exclude.iter().any(|r| r.is_match(path))
    }

    /// Scan `text` (the content of `path`). If `only_lines` is given, only those 1-based lines
    /// are reported — used to scan just the lines a commit adds.
    pub fn scan(&self, path: &str, text: &str, only_lines: Option<&BTreeSet<usize>>) -> Vec<Finding> {
        if self.is_excluded(path) {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (idx, line) in text.lines().enumerate() {
            let line_no = idx + 1;
            if only_lines.is_some_and(|set| !set.contains(&line_no)) {
                continue;
            }
            if line.len() > 4096 {
                continue; // minified or generated content: too noisy and too slow
            }
            for rule in &self.rules {
                for caps in rule.regex.captures_iter(line) {
                    let m = match rule.group {
                        Some(g) => caps.get(g),
                        None => caps.get(0),
                    };
                    let Some(m) = m else { continue };
                    let secret = m.as_str();
                    if rule.confidence == Confidence::Medium && is_placeholder(secret) {
                        continue;
                    }
                    let fingerprint = fingerprint(&rule.id, path, secret);
                    if self.allow.contains(&fingerprint) {
                        continue;
                    }
                    out.push(Finding {
                        rule_id: rule.id.clone(),
                        description: rule.description.clone(),
                        confidence: rule.confidence,
                        path: path.to_owned(),
                        line: line_no,
                        redacted: redact(secret),
                        fingerprint,
                    });
                }
            }
        }
        // A line matched by a specific rule shouldn't also be reported by the generic one.
        let specific: HashSet<(usize, String)> =
            out.iter().filter(|f| f.rule_id != "generic-secret-assignment").map(|f| (f.line, f.path.clone())).collect();
        out.retain(|f| f.rule_id != "generic-secret-assignment" || !specific.contains(&(f.line, f.path.clone())));
        out
    }
}

/// Mask all but a few characters of a secret.
pub fn redact(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 8 {
        return "*".repeat(chars.len());
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 2..].iter().collect();
    format!("{head}…{tail}")
}

/// Stable 64-bit FNV-1a fingerprint of `(rule, path, secret)` as 16 hex digits.
pub fn fingerprint(rule: &str, path: &str, secret: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in [rule, "\0", path, "\0", secret] {
        for byte in part.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

/// Convert a path glob (`*`, `**`, `?`) to an anchored regex.
fn glob_to_regex(glob: &str) -> Result<Regex> {
    let mut re = String::from("^");
    let chars: Vec<char> = glob.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                re.push_str(".*");
                i += 1;
                if chars.get(i + 1) == Some(&'/') {
                    i += 1;
                }
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    re.push('$');
    Regex::new(&re).with_context(|| format!("invalid glob '{glob}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test secrets are assembled at runtime so this file never contains a literal that
    // Veyra's own pre-commit scan would flag.
    fn fake(prefix: &str, body: &str) -> String {
        format!("{prefix}{body}")
    }

    #[test]
    fn detects_known_token_formats() {
        let scanner = Scanner::default();
        let token = fake("gh", &format!("p_{}", "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8"));
        let findings = scanner.scan("src/config.ts", &format!("const t = '{token}';\n"), None);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule_id, "github-token");
        assert_eq!(findings[0].confidence, Confidence::High);
        assert_eq!(findings[0].line, 1);
        assert!(!findings[0].redacted.contains(&token));

        let aws = fake("AK", "IAIOSFODNN7EXAMPLQ");
        assert_eq!(scanner.scan("a.env", &format!("KEY={aws}"), None)[0].rule_id, "aws-access-key-id");

        let pem = fake("-----BEGIN RSA PRIV", "ATE KEY-----");
        assert_eq!(scanner.scan("id_rsa", &pem, None)[0].rule_id, "private-key");
    }

    #[test]
    fn generic_assignments_ignore_placeholders() {
        let scanner = Scanner::default();
        let real = fake("pass", "word = \"hunter2hunter2\"");
        assert_eq!(scanner.scan("a.py", &real, None).len(), 1);
        let placeholder = fake("pass", "word = \"changeme-please\"");
        assert!(scanner.scan("a.py", &placeholder, None).is_empty());
        let env = fake("api_k", "ey: \"${API_KEY_FROM_ENV}\"");
        assert!(scanner.scan("a.yml", &env, None).is_empty());
    }

    #[test]
    fn only_requested_lines_are_reported() {
        let scanner = Scanner::default();
        let token = fake("xox", "b-1234567890-abcdefghij");
        let text = format!("{token}\nnothing\n{token}\n");
        let lines: BTreeSet<usize> = [3].into();
        let findings = scanner.scan("a.txt", &text, Some(&lines));
        assert_eq!(findings.iter().map(|f| f.line).collect::<Vec<_>>(), vec![3]);
    }

    #[test]
    fn allowlist_and_exclusions() {
        let token = fake("xox", "b-1234567890-abcdefghij");
        let fp = fingerprint("slack-token", "docs/example.md", &token);
        let policy = Policy { allow: vec![fp], exclude_paths: vec!["tests/fixtures/**".into()], rules: vec![] };
        let scanner = Scanner::new(&policy).unwrap();
        assert!(scanner.scan("docs/example.md", &token, None).is_empty());
        assert_eq!(scanner.scan("docs/other.md", &token, None).len(), 1);
        assert!(scanner.scan("tests/fixtures/deep/a.txt", &token, None).is_empty());
    }

    #[test]
    fn custom_rules_from_policy() {
        let policy = Policy::from_toml(
            "allow = []\n[[rule]]\nid = \"acme\"\ndescription = \"Acme token\"\npattern = \"acme_[a-z0-9]{8}\"\n",
        )
        .unwrap();
        let scanner = Scanner::new(&policy).unwrap();
        let findings = scanner.scan("a", &fake("acme_", "abcd1234"), None);
        assert_eq!(findings[0].rule_id, "acme");
        assert_eq!(findings[0].confidence, Confidence::High);
    }

    #[test]
    fn fingerprints_are_stable() {
        assert_eq!(fingerprint("r", "p", "s"), fingerprint("r", "p", "s"));
        assert_ne!(fingerprint("r", "p", "s"), fingerprint("r", "p2", "s"));
        assert_eq!(fingerprint("r", "p", "s").len(), 16);
    }

    #[test]
    fn redaction() {
        assert_eq!(redact("short"), "*****");
        assert_eq!(redact("abcdefghijkl"), "abcd…kl");
    }
}
