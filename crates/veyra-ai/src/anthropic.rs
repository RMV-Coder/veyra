//! The Claude API provider, over plain HTTPS (there is no official Rust SDK).
//!
//! `POST /v1/messages` with the model's default adaptive thinking, an explicit
//! `output_config.effort`, structured outputs (`output_config.format`) when a schema is given,
//! and server-side refusal fallbacks (`fallbacks: "default"`) on the models that accept them.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::{Reply, Request, Settings};

const API_VERSION: &str = "2023-06-01";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const OAUTH_BETA: &str = "oauth-2025-04-20";
/// Models that accept `fallbacks: "default"`.
const FALLBACK_MODELS: &[&str] = &["claude-fable-5-1", "claude-opus-5-5", "claude-opus-5", "claude-sonnet-5-5"];
const MAX_RETRIES: u32 = 2;

enum Credential {
    ApiKey(String),
    Bearer(String),
}

fn credential() -> Result<Credential> {
    let non_empty = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    if let Some(key) = non_empty("ANTHROPIC_API_KEY") {
        return Ok(Credential::ApiKey(key));
    }
    if let Some(token) = non_empty("ANTHROPIC_AUTH_TOKEN") {
        return Ok(Credential::Bearer(token));
    }
    bail!(
        "no Claude API credentials: set ANTHROPIC_API_KEY (or ANTHROPIC_AUTH_TOKEN)\n  \
         hint: keys are read from the environment only and never stored in Git config"
    )
}

fn uses_fallbacks(settings: &Settings) -> bool {
    settings.fallbacks && FALLBACK_MODELS.contains(&settings.model.as_str())
}

/// The JSON body for a request.
pub fn request_body(settings: &Settings, request: &Request) -> Value {
    let mut output_config = json!({ "effort": settings.effort });
    if let Some(schema) = &request.schema {
        output_config["format"] = json!({ "type": "json_schema", "schema": schema });
    }
    let mut body = json!({
        "model": settings.model,
        "max_tokens": settings.max_tokens,
        "system": request.system,
        "messages": [{ "role": "user", "content": request.prompt }],
        "output_config": output_config,
    });
    if uses_fallbacks(settings) {
        body["fallbacks"] = json!("default");
    }
    body
}

/// Extract the answer from a Messages API response.
pub fn parse_response(value: &Value) -> Result<Reply> {
    match value["stop_reason"].as_str() {
        Some("refusal") => {
            let category = value["stop_details"]["category"].as_str().unwrap_or("unspecified");
            bail!("the model declined this request (category: {category}); nothing was changed")
        }
        Some("max_tokens") => {
            bail!(
                "the answer was cut off at the token limit\n  hint: raise it with `veyra config veyra.ai.maxTokens 32000`"
            )
        }
        _ => {}
    }
    let blocks = value["content"].as_array().context("unexpected response: no content")?;
    let text: String =
        blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("");
    if text.trim().is_empty() {
        bail!("the model returned no text");
    }
    Ok(Reply { text, model: value["model"].as_str().map(str::to_owned) })
}

pub fn complete(settings: &Settings, request: &Request) -> Result<Reply> {
    send(settings, request, &credential()?)
}

fn send(settings: &Settings, request: &Request, credential: &Credential) -> Result<Reply> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(settings.timeout_secs))
        .user_agent(concat!("veyra/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let body = serde_json::to_vec(&request_body(settings, request))?;
    let url = format!("{}/v1/messages", settings.base_url);
    let mut betas = Vec::new();
    if uses_fallbacks(settings) {
        betas.push(FALLBACK_BETA);
    }
    let mut attempt = 0;
    loop {
        let mut req = client
            .post(&url)
            .header("content-type", "application/json")
            .header("anthropic-version", API_VERSION)
            .body(body.clone());
        let mut these_betas = betas.clone();
        req = match credential {
            Credential::ApiKey(key) => req.header("x-api-key", key),
            Credential::Bearer(token) => {
                these_betas.push(OAUTH_BETA);
                req.header("authorization", format!("Bearer {token}"))
            }
        };
        if !these_betas.is_empty() {
            req = req.header("anthropic-beta", these_betas.join(","));
        }
        let result = req.send();
        let retry_after = match &result {
            Ok(resp) => resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok()),
            Err(_) => None,
        };
        let retryable = match &result {
            Ok(resp) => matches!(resp.status().as_u16(), 408 | 409 | 429 | 500..=599),
            Err(e) => e.is_connect() || e.is_timeout(),
        };
        if retryable && attempt < MAX_RETRIES {
            attempt += 1;
            std::thread::sleep(Duration::from_secs(retry_after.unwrap_or(1 << attempt).min(30)));
            continue;
        }
        let resp = result.map_err(|e| anyhow!("could not reach the Claude API at {url}: {e}"))?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !status.is_success() {
            let kind = value["error"]["type"].as_str().unwrap_or("error");
            let message = value["error"]["message"].as_str().unwrap_or(text.trim());
            let hint = match status.as_u16() {
                401 => "\n  hint: check ANTHROPIC_API_KEY",
                404 => "\n  hint: check the model name (`veyra ai configure --model …`)",
                429 => "\n  hint: rate limited; try again shortly",
                _ => "",
            };
            bail!("Claude API error {} ({kind}): {message}{hint}", status.as_u16());
        }
        return parse_response(&value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Provider;

    fn settings() -> Settings {
        Settings { provider: Provider::Anthropic, ..Settings::default() }
    }

    #[test]
    fn body_has_effort_schema_and_fallbacks() {
        let req = Request { system: "sys".into(), prompt: "hi".into(), schema: Some(json!({"type": "object"})) };
        let body = request_body(&settings(), &req);
        assert_eq!(body["model"], "claude-opus-5-5");
        assert_eq!(body["output_config"]["effort"], "medium");
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        assert_eq!(body["fallbacks"], "default");
        assert_eq!(body["messages"][0]["content"], "hi");
        assert!(body.get("thinking").is_none(), "adaptive thinking is the default; never disable it");

        let other = Settings { model: "claude-haiku-5-5".into(), ..settings() };
        assert!(request_body(&other, &req).get("fallbacks").is_none());
        let off = Settings { fallbacks: false, ..settings() };
        assert!(request_body(&off, &req).get("fallbacks").is_none());
    }

    /// A one-shot HTTP server: returns the raw request it received and answers with `status`/`body`.
    fn mock(status: u16, body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = sock.read(&mut chunk).unwrap();
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let len = text[..end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + len {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(reply.as_bytes()).unwrap();
            String::from_utf8_lossy(&buf).to_string()
        });
        (url, handle)
    }

    #[test]
    fn talks_to_the_messages_api() {
        let (url, server) = mock(
            200,
            r#"{"model":"claude-opus-5-5","stop_reason":"end_turn","content":[{"type":"text","text":"{\"subject\":\"feat: x\",\"body\":\"\"}"}]}"#,
        );
        let s = Settings { base_url: url, ..settings() };
        let req = Request { system: "sys".into(), prompt: "diff".into(), schema: None };
        let reply = send(&s, &req, &Credential::ApiKey("test-key".into())).unwrap();
        assert_eq!(reply.model.as_deref(), Some("claude-opus-5-5"));
        assert!(reply.text.contains("feat: x"));
        let raw = server.join().unwrap().to_ascii_lowercase();
        assert!(raw.starts_with("post /v1/messages "), "{raw}");
        assert!(raw.contains("x-api-key: test-key"));
        assert!(raw.contains("anthropic-version: 2023-06-01"));
        assert!(raw.contains("anthropic-beta: server-side-fallback-2026-07-01"));
        assert!(raw.contains("\"fallbacks\":\"default\""));
    }

    #[test]
    fn api_errors_are_reported_with_their_message() {
        let (url, server) =
            mock(400, r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad model"}}"#);
        let s = Settings { base_url: url, ..settings() };
        let req = Request { system: "s".into(), prompt: "p".into(), schema: None };
        let err = send(&s, &req, &Credential::Bearer("tok".into())).unwrap_err().to_string();
        assert!(err.contains("400") && err.contains("bad model"), "{err}");
        let raw = server.join().unwrap().to_ascii_lowercase();
        assert!(raw.contains("authorization: bearer tok") && raw.contains("oauth-2025-04-20"), "{raw}");
    }

    #[test]
    fn responses_refusals_and_truncation() {
        let ok = json!({"model": "claude-opus-5-5", "stop_reason": "end_turn",
            "content": [{"type": "thinking", "thinking": ""}, {"type": "text", "text": "Hello"}]});
        assert_eq!(parse_response(&ok).unwrap().text, "Hello");
        let refusal = json!({"stop_reason": "refusal", "stop_details": {"category": "cyber"}, "content": []});
        assert!(parse_response(&refusal).unwrap_err().to_string().contains("declined"));
        let cut = json!({"stop_reason": "max_tokens", "content": [{"type": "text", "text": "par"}]});
        assert!(parse_response(&cut).unwrap_err().to_string().contains("cut off"));
    }
}
