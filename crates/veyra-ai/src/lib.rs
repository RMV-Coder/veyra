//! Optional AI assistance for Veyra.
//!
//! Principles (see docs/VISION.md):
//! * **Off by default.** Nothing is sent anywhere until a provider is configured explicitly.
//! * **Advisory.** Every AI output is a *proposal* the user approves; nothing here changes history.
//! * **Transparent.** Callers can show exactly what would be sent (`--dry-run`), and payloads are
//!   scanned for secrets before they leave the machine (done by the CLI with `veyra-security`).
//! * **Replaceable.** Besides the Claude API, any local model works through the `command`
//!   provider: Veyra writes a JSON request to the command's stdin and reads the answer from stdout.

pub mod anthropic;
pub mod command;
pub mod tasks;

use anyhow::{Result, bail};
use serde::Serialize;

/// The default model for the Claude API provider.
pub const DEFAULT_MODEL: &str = "claude-opus-5-5";

/// Which provider answers requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provider {
    /// AI features are disabled (the default).
    Off,
    /// The Claude API (`ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN`).
    Anthropic,
    /// A local command: JSON request on stdin, answer on stdout.
    Command(String),
}

/// Provider settings, normally read from `veyra.ai.*` Git config keys.
#[derive(Debug, Clone)]
pub struct Settings {
    pub provider: Provider,
    pub model: String,
    /// `low` · `medium` · `high` · `xhigh` · `max` (Claude API `output_config.effort`).
    pub effort: String,
    /// Server-side refusal fallbacks (Claude API only).
    pub fallbacks: bool,
    pub max_tokens: u32,
    pub base_url: String,
    pub timeout_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            provider: Provider::Off,
            model: DEFAULT_MODEL.into(),
            effort: "medium".into(),
            fallbacks: true,
            max_tokens: 16_000,
            base_url: "https://api.anthropic.com".into(),
            timeout_secs: 600,
        }
    }
}

impl Settings {
    /// Build settings from a config lookup (`key` → value), e.g. Git config.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Settings {
        let provider = match get("veyra.ai.provider").map(|p| p.trim().to_ascii_lowercase()).as_deref() {
            Some("anthropic" | "claude") => Provider::Anthropic,
            Some("command" | "local") => match get("veyra.ai.command") {
                Some(cmd) if !cmd.trim().is_empty() => Provider::Command(cmd),
                _ => Provider::Off,
            },
            _ => Provider::Off,
        };
        let mut s = Settings { provider, ..Settings::default() };
        if let Some(m) = get("veyra.ai.model").filter(|m| !m.trim().is_empty()) {
            s.model = m.trim().to_owned();
        }
        if let Some(e) = get("veyra.ai.effort").filter(|e| !e.trim().is_empty()) {
            s.effort = e.trim().to_ascii_lowercase();
        }
        if let Some(f) = get("veyra.ai.fallbacks") {
            s.fallbacks = !matches!(f.trim().to_ascii_lowercase().as_str(), "false" | "off" | "no" | "0");
        }
        if let Some(n) = get("veyra.ai.maxTokens").and_then(|n| n.trim().parse().ok()) {
            s.max_tokens = n;
        }
        if let Ok(url) = std::env::var("ANTHROPIC_BASE_URL")
            && !url.trim().is_empty()
        {
            s.base_url = url.trim().trim_end_matches('/').to_owned();
        }
        s
    }

    pub fn describe(&self) -> String {
        match &self.provider {
            Provider::Off => "off".into(),
            Provider::Anthropic => format!("Claude API ({}, effort {})", self.model, self.effort),
            Provider::Command(cmd) => format!("local command `{cmd}`"),
        }
    }
}

/// One request to the model.
#[derive(Debug, Clone, Serialize)]
pub struct Request {
    /// Instructions: who the model is and what to produce.
    pub system: String,
    /// The material (diffs, files, commit lists) and the question.
    pub prompt: String,
    /// When set, the answer must be JSON matching this schema (structured output).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<serde_json::Value>,
}

/// The model's answer.
#[derive(Debug, Clone)]
pub struct Reply {
    pub text: String,
    /// The model that actually answered (it can differ after a fallback).
    pub model: Option<String>,
}

/// Largest payload Veyra sends in one request. Larger inputs are refused, never truncated
/// silently: a truncated diff would produce confidently wrong advice.
pub const MAX_PROMPT_BYTES: usize = 1_500_000;

/// Send `request` to the configured provider.
pub fn complete(settings: &Settings, request: &Request) -> Result<Reply> {
    let size = request.system.len() + request.prompt.len();
    if size > MAX_PROMPT_BYTES {
        bail!(
            "the input is too large to send ({} KB, limit {} KB); narrow it down (paths, a smaller range)",
            size / 1024,
            MAX_PROMPT_BYTES / 1024
        );
    }
    match &settings.provider {
        Provider::Off => bail!(
            "AI features are off\n  hint: `veyra ai configure --provider anthropic` (uses ANTHROPIC_API_KEY), \
             or `--provider command --command <cmd>` for a local model"
        ),
        Provider::Anthropic => anthropic::complete(settings, request),
        Provider::Command(cmd) => command::complete(cmd, request),
    }
}

/// Parse a structured answer, tolerating a fenced ```json block from local models.
pub fn parse_json<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    let trimmed = text.trim();
    let body = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|s| s.strip_suffix("```"))
        .unwrap_or(trimmed);
    serde_json::from_str(body.trim())
        .map_err(|e| anyhow::anyhow!("the model's answer is not the expected JSON ({e}):\n{}", preview(text)))
}

fn preview(text: &str) -> String {
    let mut s: String = text.chars().take(400).collect();
    if text.chars().count() > 400 {
        s.push('…');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn settings_from_config() {
        let cfg: HashMap<&str, &str> = [("veyra.ai.provider", "anthropic"), ("veyra.ai.effort", "HIGH")].into();
        let s = Settings::from_lookup(|k| cfg.get(k).map(|v| v.to_string()));
        assert_eq!(s.provider, Provider::Anthropic);
        assert_eq!(s.model, DEFAULT_MODEL);
        assert_eq!(s.effort, "high");
        assert!(s.fallbacks);

        let off = Settings::from_lookup(|_| None);
        assert_eq!(off.provider, Provider::Off);
        // A command provider without a command stays off.
        let cfg: HashMap<&str, &str> = [("veyra.ai.provider", "command")].into();
        assert_eq!(Settings::from_lookup(|k| cfg.get(k).map(|v| v.to_string())).provider, Provider::Off);
    }

    #[test]
    fn off_and_oversized_requests_are_refused() {
        let r = Request { system: "s".into(), prompt: "p".into(), schema: None };
        assert!(complete(&Settings::default(), &r).unwrap_err().to_string().contains("AI features are off"));
        let big = Request { system: String::new(), prompt: "x".repeat(MAX_PROMPT_BYTES + 1), schema: None };
        let s = Settings { provider: Provider::Anthropic, ..Settings::default() };
        assert!(complete(&s, &big).unwrap_err().to_string().contains("too large"));
    }

    #[test]
    fn json_answers_with_or_without_fences() {
        #[derive(serde::Deserialize)]
        struct A {
            a: u32,
        }
        assert_eq!(parse_json::<A>("{\"a\": 1}").unwrap().a, 1);
        assert_eq!(parse_json::<A>("```json\n{\"a\": 2}\n```").unwrap().a, 2);
        assert!(parse_json::<A>("not json").is_err());
    }
}
