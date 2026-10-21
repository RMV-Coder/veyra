//! The AI tasks Veyra offers: prompt, output schema and typed result for each.
//!
//! Every result is a proposal for a human to accept; callers validate it deterministically
//! before use (e.g. a split must cover exactly the staged files, a resolution must parse).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Request;

const PREAMBLE: &str = "You are assisting a developer inside Veyra, a Git-compatible version control \
system. Your output is a proposal that the developer reviews before anything happens. Be precise and \
concrete, ground every statement in the material provided, and say so when something can't be \
determined from it.";

fn strict_object(properties: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false })
}

fn nullable(t: &str) -> Value {
    json!({ "anyOf": [{ "type": t }, { "type": "null" }] })
}

fn fenced(label: &str, body: &str) -> String {
    format!("<{label}>\n{}\n</{label}>\n", body.trim_end())
}

// ------------------------------------------------------------------------------------------
// Commit messages

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct CommitMessage {
    pub subject: String,
    #[serde(default)]
    pub body: String,
}

impl CommitMessage {
    pub fn to_message(&self) -> String {
        let subject = self.subject.trim();
        let body = self.body.trim();
        if body.is_empty() { format!("{subject}\n") } else { format!("{subject}\n\n{body}\n") }
    }
}

/// Propose a message for the staged changes. `recent` are recent subjects (the house style).
pub fn commit_message(diff: &str, recent: &[String]) -> Request {
    let mut prompt = String::new();
    if !recent.is_empty() {
        prompt.push_str(&fenced("recent_commit_subjects", &recent.join("\n")));
    }
    prompt.push_str(&fenced("staged_diff", diff));
    prompt.push_str(
        "Write the commit message for this staged change. Follow the conventions visible in the recent \
         subjects (for example Conventional Commits prefixes) when there are any. The subject is \
         imperative and at most 72 characters. The body explains what changed and why, wrapped at 72 \
         columns; leave it empty for trivial changes. Do not invent motivation that the diff doesn't show.",
    );
    Request {
        system: PREAMBLE.into(),
        prompt,
        schema: Some(strict_object(
            json!({ "subject": { "type": "string" }, "body": { "type": "string" } }),
            &["subject", "body"],
        )),
    }
}

// ------------------------------------------------------------------------------------------
// Splitting a change into commits

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct SplitPlan {
    pub commits: Vec<PlannedCommit>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct PlannedCommit {
    pub message: String,
    pub files: Vec<String>,
}

impl SplitPlan {
    /// The plan must assign every staged file to exactly one commit, and nothing else.
    pub fn validate(&self, staged: &[String]) -> Result<(), String> {
        if self.commits.is_empty() {
            return Err("the plan has no commits".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for c in &self.commits {
            if c.files.is_empty() || c.message.trim().is_empty() {
                return Err("a planned commit has no files or no message".into());
            }
            for f in &c.files {
                if !staged.contains(f) {
                    return Err(format!("the plan mentions '{f}', which is not staged"));
                }
                if !seen.insert(f.clone()) {
                    return Err(format!("the plan puts '{f}' in more than one commit"));
                }
            }
        }
        if let Some(missing) = staged.iter().find(|f| !seen.contains(*f)) {
            return Err(format!("the plan leaves '{missing}' out"));
        }
        Ok(())
    }
}

pub fn split(diff: &str, files: &[String], recent: &[String]) -> Request {
    let mut prompt = String::new();
    if !recent.is_empty() {
        prompt.push_str(&fenced("recent_commit_subjects", &recent.join("\n")));
    }
    prompt.push_str(&fenced("staged_files", &files.join("\n")));
    prompt.push_str(&fenced("staged_diff", diff));
    prompt.push_str(
        "Group the staged files into a sequence of focused commits, each a coherent change that would make \
         sense on its own, in an order where each commit builds on the previous ones. Every staged file \
         must appear in exactly one commit, using the paths exactly as listed. If the change is already \
         one coherent unit, return a single commit. Give each commit a full message (subject line, blank \
         line, optional body) in the repository's style.",
    );
    Request {
        system: PREAMBLE.into(),
        prompt,
        schema: Some(strict_object(
            json!({ "commits": { "type": "array", "items": strict_object(
                json!({ "message": { "type": "string" }, "files": { "type": "array", "items": { "type": "string" } } }),
                &["message", "files"],
            ) } }),
            &["commits"],
        )),
    }
}

// ------------------------------------------------------------------------------------------
// Review

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Review {
    pub summary: String,
    pub findings: Vec<ReviewFinding>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ReviewFinding {
    /// `high` · `medium` · `low`
    pub severity: String,
    pub file: String,
    pub line: Option<u64>,
    pub title: String,
    pub detail: String,
}

/// Review a change. `context` is optional deterministic context (impact report, intents).
pub fn review(diff: &str, context: &str) -> Request {
    let mut prompt = String::new();
    if !context.trim().is_empty() {
        prompt.push_str(&fenced("deterministic_analysis", context));
    }
    prompt.push_str(&fenced("diff", diff));
    prompt.push_str(
        "Review this change as a careful senior engineer. Report real problems only: bugs, missing error \
         handling, security issues, broken contracts with callers, data loss, concurrency hazards, and \
         missing tests for risky logic. Skip style nits. For each finding give the file, the line in the \
         new version when you can tell it (otherwise null), a short title and a concrete explanation. \
         Use severity high for likely bugs or vulnerabilities, medium for probable issues, low for \
         worthwhile improvements. An empty list is a valid answer.",
    );
    Request {
        system: PREAMBLE.into(),
        prompt,
        schema: Some(strict_object(
            json!({
                "summary": { "type": "string" },
                "findings": { "type": "array", "items": strict_object(json!({
                    "severity": { "type": "string", "enum": ["high", "medium", "low"] },
                    "file": { "type": "string" },
                    "line": nullable("integer"),
                    "title": { "type": "string" },
                    "detail": { "type": "string" },
                }), &["severity", "file", "line", "title", "detail"]) },
            }),
            &["summary", "findings"],
        )),
    }
}

// ------------------------------------------------------------------------------------------
// Explain and summarize (plain text)

pub fn explain(diff: &str, message: Option<&str>) -> Request {
    let mut prompt = String::new();
    if let Some(m) = message {
        prompt.push_str(&fenced("commit_message", m));
    }
    prompt.push_str(&fenced("diff", diff));
    prompt.push_str(
        "Explain this change to a teammate who hasn't seen it: what it does, how it works, and what it \
         affects. Start with a one-sentence summary, then short paragraphs or a brief list. Plain text, \
         no headings.",
    );
    Request { system: PREAMBLE.into(), prompt, schema: None }
}

pub fn summarize(commits: &str, stat: &str) -> Request {
    let mut prompt = fenced("commits", commits);
    if !stat.trim().is_empty() {
        prompt.push_str(&fenced("files_changed", stat));
    }
    prompt.push_str(
        "Summarize this range of commits for a pull request description or release notes: a short overview \
         paragraph, then a bulleted list of the notable changes grouped by theme (features, fixes, \
         internal). Mention breaking changes explicitly. Use Markdown.",
    );
    Request { system: PREAMBLE.into(), prompt, schema: None }
}

// ------------------------------------------------------------------------------------------
// Risks (on top of the deterministic impact report)

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RiskAssessment {
    pub assessment: String,
    pub risks: Vec<RiskItem>,
    pub suggested_checks: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RiskItem {
    pub title: String,
    pub detail: String,
    /// What in the diff or the impact report supports this.
    pub evidence: String,
}

pub fn risks(impact_json: &str, diff: &str) -> Request {
    let mut prompt = fenced("impact_report", impact_json);
    prompt.push_str(&fenced("diff", diff));
    prompt.push_str(
        "Using the deterministic impact report and the diff, identify what could break or regress if this \
         change ships, and what should be checked before merging. Every risk must cite its evidence from \
         the report or the diff. Do not repeat the report verbatim; add judgment about behavior, \
         compatibility and operations.",
    );
    Request {
        system: PREAMBLE.into(),
        prompt,
        schema: Some(strict_object(
            json!({
                "assessment": { "type": "string" },
                "risks": { "type": "array", "items": strict_object(json!({
                    "title": { "type": "string" }, "detail": { "type": "string" }, "evidence": { "type": "string" },
                }), &["title", "detail", "evidence"]) },
                "suggested_checks": { "type": "array", "items": { "type": "string" } },
            }),
            &["assessment", "risks", "suggested_checks"],
        )),
    }
}

// ------------------------------------------------------------------------------------------
// Assisted conflict resolution

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Resolution {
    /// The complete resolved file.
    pub resolved: String,
    /// `high` · `medium` · `low`
    pub confidence: String,
    pub explanation: String,
}

pub fn resolve(path: &str, base: &str, ours: &str, theirs: &str, conflicted: &str) -> Request {
    let mut prompt = format!("File: {path}\n");
    prompt.push_str(&fenced("base_version", base));
    prompt.push_str(&fenced("ours_version", ours));
    prompt.push_str(&fenced("theirs_version", theirs));
    prompt.push_str(&fenced("conflicted_file", conflicted));
    prompt.push_str(
        "Resolve the merge conflict. Keep the intent of both sides wherever they are compatible; where they \
         genuinely contradict, prefer the behavior that keeps the code correct and explain the choice. \
         Return the complete resolved file with no conflict markers, preserving formatting and line \
         endings. Confidence is high only if both intents are fully preserved and the result is plainly \
         correct; low if you had to guess about intent.",
    );
    Request {
        system: PREAMBLE.into(),
        prompt,
        schema: Some(strict_object(
            json!({
                "resolved": { "type": "string" },
                "confidence": { "type": "string", "enum": ["high", "medium", "low"] },
                "explanation": { "type": "string" },
            }),
            &["resolved", "confidence", "explanation"],
        )),
    }
}

// ------------------------------------------------------------------------------------------
// Intent suggestions

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct IntentSuggestion {
    pub title: String,
    pub kind: String,
    pub objective: String,
    pub constraints: Vec<String>,
    pub guards: Vec<GuardSuggestion>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct GuardSuggestion {
    pub paths: Vec<String>,
    pub must_contain: Option<String>,
    pub must_not_contain: Option<String>,
    pub message: String,
}

pub fn intent_suggest(files: &str, existing: &str) -> Request {
    let mut prompt = fenced("files", files);
    if !existing.trim().is_empty() {
        prompt.push_str(&fenced("existing_intents", existing));
    }
    prompt.push_str(
        "Propose one design intent record for this code: what it is for, the constraints future changes \
         must respect, and deterministic guards that would catch a violation. A guard selects files with \
         glob patterns and requires (must_contain) or forbids (must_not_contain) a literal string; use \
         null for the check you don't need. Only propose guards that the current code satisfies. Kind is \
         one of feature, bugfix, refactor, decision, security, performance. Don't duplicate an existing \
         intent.",
    );
    Request {
        system: PREAMBLE.into(),
        prompt,
        schema: Some(strict_object(
            json!({
                "title": { "type": "string" },
                "kind": { "type": "string", "enum": ["feature", "bugfix", "refactor", "decision", "security", "performance"] },
                "objective": { "type": "string" },
                "constraints": { "type": "array", "items": { "type": "string" } },
                "guards": { "type": "array", "items": strict_object(json!({
                    "paths": { "type": "array", "items": { "type": "string" } },
                    "must_contain": nullable("string"),
                    "must_not_contain": nullable("string"),
                    "message": { "type": "string" },
                }), &["paths", "must_contain", "must_not_contain", "message"]) },
            }),
            &["title", "kind", "objective", "constraints", "guards"],
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_objects_are_strict(v: &Value) {
        if v["type"] == "object" {
            assert_eq!(v["additionalProperties"], false, "{v}");
            let props: Vec<&String> = v["properties"].as_object().unwrap().keys().collect();
            let required: Vec<&str> = v["required"].as_array().unwrap().iter().map(|r| r.as_str().unwrap()).collect();
            for p in props {
                assert!(required.contains(&p.as_str()), "{p} must be required");
            }
        }
        match v {
            Value::Object(m) => m.values().for_each(all_objects_are_strict),
            Value::Array(a) => a.iter().for_each(all_objects_are_strict),
            _ => {}
        }
    }

    #[test]
    fn schemas_are_valid_for_structured_outputs() {
        for r in [
            commit_message("d", &[]),
            split("d", &["a".into()], &[]),
            review("d", ""),
            risks("{}", "d"),
            resolve("p", "b", "o", "t", "c"),
            intent_suggest("f", ""),
        ] {
            all_objects_are_strict(r.schema.as_ref().unwrap());
        }
    }

    #[test]
    fn split_plans_must_partition_the_staged_files() {
        let staged = vec!["a".to_string(), "b".to_string()];
        let plan = |groups: &[&[&str]]| SplitPlan {
            commits: groups
                .iter()
                .map(|g| PlannedCommit { message: "m".into(), files: g.iter().map(|s| s.to_string()).collect() })
                .collect(),
        };
        assert!(plan(&[&["a"], &["b"]]).validate(&staged).is_ok());
        assert!(plan(&[&["a"]]).validate(&staged).unwrap_err().contains("leaves 'b' out"));
        assert!(plan(&[&["a", "b"], &["b"]]).validate(&staged).unwrap_err().contains("more than one"));
        assert!(plan(&[&["a", "b", "c"]]).validate(&staged).unwrap_err().contains("not staged"));
    }

    #[test]
    fn commit_message_formatting() {
        let m = CommitMessage { subject: " feat: x ".into(), body: "".into() };
        assert_eq!(m.to_message(), "feat: x\n");
        let m = CommitMessage { subject: "fix: y".into(), body: "Because.".into() };
        assert_eq!(m.to_message(), "fix: y\n\nBecause.\n");
    }
}
