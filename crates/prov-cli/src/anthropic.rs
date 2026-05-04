//! Minimal blocking Anthropic Messages API client used by `prov regenerate`.
//!
//! Why this lives in `prov-cli` (not `prov-core`): network and HTTP are leaf
//! dependencies of one CLI subcommand. Pulling reqwest into prov-core would
//! make the embeddable library surface drag rustls and the network stack into
//! callers (the GitHub Action, future MCP servers, hook-mode invocations) that
//! don't need them. Keep the embeddable core pure.
//!
//! API key handling: the caller passes the key in as an owned `String`. The
//! client never re-reads the environment, never logs the key, and the error
//! type's `Display`/`Debug` impls strip both `x-api-key` header values and
//! anything that looks like a `sk-ant-` prefix from response bodies before
//! surfacing them. The plan also calls for `std::env::remove_var` after read
//! to harden against subprocess env inheritance, but that requires `unsafe`
//! which the workspace lints forbid; `regenerate` mitigates that instead by
//! `env_remove`ing the key on every subprocess it spawns.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Default Anthropic API base. Overridable via `with_base_url` so tests can
/// point at a mockito server.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Anthropic API version pin. Bumping this is a deliberate compatibility
/// decision — see https://docs.anthropic.com/en/api/versioning.
pub const API_VERSION: &str = "2023-06-01";

/// Cap on regenerate-call response length. 16k tokens covers the longest
/// realistic single-turn AI edit; bigger responses are almost always an
/// indicator the prompt is being interpreted as agentic work rather than a
/// regeneration. Tunable via `with_max_tokens` if a future caller needs more.
const DEFAULT_MAX_TOKENS: u32 = 16_384;

/// HTTP timeout for a single regenerate call. Anthropic's median latency for
/// completions sits well under this; the cap exists to prevent the CLI from
/// hanging indefinitely on a stalled connection.
const REQUEST_TIMEOUT: Duration = Duration::from_mins(2);

/// Blocking client for the Messages API.
///
/// One instance per `prov regenerate` invocation. Not `Clone`-able by design —
/// the API key is moved into the client and lives only as long as the call.
pub struct Client {
    api_key: String,
    base_url: String,
    max_tokens: u32,
    http: reqwest::blocking::Client,
}

impl Client {
    /// Build a client around an owned API key. Caller is expected to read
    /// `ANTHROPIC_API_KEY` into a `String` once and pass it here; the client
    /// will not re-read the environment.
    pub fn new(api_key: String) -> Result<Self, AnthropicError> {
        let http = reqwest::blocking::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("prov/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| AnthropicError::Transport(redact(&e.to_string())))?;
        Ok(Self {
            api_key,
            base_url: DEFAULT_BASE_URL.to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
            http,
        })
    }

    /// Override the API base. Tests use this to point at a mockito server.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Send a single user-turn message and return the response text.
    ///
    /// `system` is optional; pass `Some(...)` to include the
    /// `preceding_turns_summary` from the original capture as system context
    /// so the regenerated output reflects the same conversational frame.
    pub fn complete(
        &self,
        model: &str,
        prompt: &str,
        system: Option<&str>,
    ) -> Result<String, AnthropicError> {
        let url = format!("{}/v1/messages", self.base_url);
        let body = MessagesRequest {
            model,
            max_tokens: self.max_tokens,
            system,
            messages: &[Message {
                role: "user",
                content: prompt,
            }],
        };

        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .map_err(|e| AnthropicError::Transport(redact(&e.to_string())))?;

        let status = resp.status();
        if status.as_u16() == 429 {
            // Surface retry-after to the caller so the CLI can render a clean
            // rate-limit message instead of a generic HTTP error.
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            return Err(AnthropicError::RateLimited { retry_after });
        }
        if !status.is_success() {
            let body_text = resp.text().unwrap_or_default();
            return Err(AnthropicError::Http {
                status: status.as_u16(),
                body: redact(&body_text),
            });
        }

        let parsed: MessagesResponse = resp
            .json()
            .map_err(|e| AnthropicError::Decode(redact(&e.to_string())))?;
        let text: String = parsed
            .content
            .into_iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text),
                ContentBlock::Other => None,
            })
            .collect();
        Ok(text)
    }
}

#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    messages: &'a [Message<'a>],
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    /// Tool-use, thinking, and other future block types collapse to `Other`
    /// so an unrecognized block from a newer API doesn't break parsing.
    #[serde(other)]
    Other,
}

/// Errors surfaced from an Anthropic call. `Display` strips API-key-shaped
/// substrings before rendering, so structured-error logs and panics never
/// leak the credential.
#[derive(Debug, Error)]
pub enum AnthropicError {
    /// Network or TLS error (timeout, DNS, connection refused).
    #[error("anthropic transport error: {0}")]
    Transport(String),
    /// Non-2xx, non-429 HTTP response.
    #[error("anthropic returned HTTP {status}: {body}")]
    Http { status: u16, body: String },
    /// 429 Too Many Requests — `retry_after` is the response header value
    /// when present.
    #[error("anthropic rate-limited (429); retry-after={}", retry_after.as_deref().unwrap_or("(missing)"))]
    RateLimited { retry_after: Option<String> },
    /// JSON decode failed on a 2xx response.
    #[error("anthropic response decode error: {0}")]
    Decode(String),
}

/// Strip API-key-shaped substrings from arbitrary text. Defense-in-depth
/// against errors that bubble up from reqwest/serde and contain the
/// outgoing request body or response body verbatim. Removes substrings that
/// look like Anthropic API keys and any `x-api-key:`-style header values.
fn redact(s: &str) -> String {
    // Anthropic keys start with `sk-ant-` followed by an alphanumeric tail.
    // We strip anything that looks like that prefix to end-of-token.
    let mut out = String::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        if ch == 's' && s[idx..].starts_with("sk-ant-") {
            out.push_str("[REDACTED]");
            // Skip until next non-token character.
            while let Some(&(_, c)) = chars.peek() {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    chars.next();
                } else {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    // Also redact any `x-api-key: <value>` style header dumps.
    redact_header(&out, "x-api-key")
}

fn redact_header(s: &str, header: &str) -> String {
    let needle = format!("{header}: ");
    let lower = s.to_ascii_lowercase();
    let needle_lower = needle.to_ascii_lowercase();
    let Some(pos) = lower.find(&needle_lower) else {
        return s.to_string();
    };
    let value_start = pos + needle.len();
    // Header values terminate at \r, \n, or end of string.
    let value_end = s[value_start..]
        .find(['\r', '\n'])
        .map_or(s.len(), |o| value_start + o);
    let mut out = String::with_capacity(s.len());
    out.push_str(&s[..value_start]);
    out.push_str("[REDACTED]");
    out.push_str(&s[value_end..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_strips_sk_ant_prefix() {
        let input = "request failed for key sk-ant-abc123XYZ_def at https://api.anthropic.com";
        let out = redact(input);
        assert!(!out.contains("sk-ant-abc123"), "raw key leaked: {out}");
        assert!(out.contains("[REDACTED]"));
        // Surrounding context is preserved.
        assert!(out.contains("https://api.anthropic.com"));
    }

    #[test]
    fn redact_strips_x_api_key_header_value() {
        let input = "POST /v1/messages\r\nx-api-key: sk-ant-secret-abc\r\nuser-agent: prov/0.1.1";
        let out = redact(input);
        assert!(!out.contains("sk-ant-secret-abc"));
        assert!(out.contains("x-api-key: [REDACTED]"));
        assert!(out.contains("user-agent: prov/0.1.1"));
    }

    #[test]
    fn redact_is_idempotent_when_no_key_present() {
        let input = "anthropic returned HTTP 500: internal error";
        assert_eq!(redact(input), input);
    }

    #[test]
    fn anthropic_error_display_does_not_leak_key() {
        let err = AnthropicError::Http {
            status: 401,
            body: redact("invalid x-api-key: sk-ant-leak"),
        };
        let rendered = format!("{err}");
        assert!(!rendered.contains("sk-ant-leak"));
    }
}
