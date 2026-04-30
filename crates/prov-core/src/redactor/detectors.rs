//! Built-in secret-pattern detectors.
//!
//! Each detector returns a list of `DetectedSpan` slots in the input string.
//! `Redactor` (in the parent module) splices `[REDACTED:<kind>]` over each span
//! in reverse order so byte offsets stay valid.

use regex::Regex;
use std::sync::OnceLock;

/// Categories the redactor knows how to label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectorKind {
    /// AWS access key (`AKIA...` / `ASIA...`).
    AwsKey,
    /// Stripe API key (`sk_live_...` / `sk_test_...`) and restricted/test variants.
    StripeKey,
    /// Stripe webhook signing secret (`whsec_...`).
    StripeWebhook,
    /// GitHub personal access token (`ghp_...` / `github_pat_...`) plus the
    /// OAuth/server family (`gho_`, `ghu_`, `ghs_`, `ghr_`).
    GithubPat,
    /// Anthropic API key (`sk-ant-api...` / `sk-ant-admin...`).
    AnthropicKey,
    /// OpenAI API key (`sk-...` with optional `proj-`/`svcacct-`/`admin-` prefix).
    OpenAiKey,
    /// Slack token family (`xoxa-` / `xoxb-` / `xoxp-` / `xoxr-` / `xoxs-`).
    SlackToken,
    /// Google API key (`AIza...`).
    GoogleApiKey,
    /// JSON Web Token (header.payload.signature).
    Jwt,
    /// GCP service-account JSON blob.
    GcpServiceAccount,
    /// PEM-encoded private key block.
    PemPrivateKey,
    /// Database URL with embedded credentials.
    DbUrl,
    /// `KEY=value` / `KEY: value` shaped credential leak whose value would
    /// otherwise slip past the entropy gate (short tokens, dictionary words).
    KeyValueSecret,
    /// Email address.
    Email,
    /// High-entropy unknown string (Shannon entropy ≥ 4.0 over ≥ 24 chars).
    HighEntropy,
    /// User-supplied `.provignore` rule, identified by zero-based rule index.
    ProvIgnoreRule(u32),
}

impl DetectorKind {
    /// Marker label used inside `[REDACTED:<marker>]`.
    #[must_use]
    pub fn as_marker(&self) -> String {
        match self {
            Self::AwsKey => "aws-key".into(),
            Self::StripeKey => "stripe-key".into(),
            Self::StripeWebhook => "stripe-webhook".into(),
            Self::GithubPat => "github-pat".into(),
            Self::AnthropicKey => "anthropic-key".into(),
            Self::OpenAiKey => "openai-key".into(),
            Self::SlackToken => "slack-token".into(),
            Self::GoogleApiKey => "google-api-key".into(),
            Self::Jwt => "jwt".into(),
            Self::GcpServiceAccount => "gcp-service-account".into(),
            Self::PemPrivateKey => "pem-private-key".into(),
            Self::DbUrl => "db-url".into(),
            Self::KeyValueSecret => "key-value-secret".into(),
            Self::Email => "email".into(),
            Self::HighEntropy => "high-entropy".into(),
            Self::ProvIgnoreRule(i) => format!("provignore-rule:{i}"),
        }
    }
}

/// One detected span.
#[derive(Debug, Clone)]
pub struct DetectedSpan {
    /// What detector type produced this span.
    pub kind: DetectorKind,
    /// `[start, end)` byte offsets in the input.
    pub span: (usize, usize),
}

/// A detector is anything that scans a string for spans to redact.
pub trait Detector: Send + Sync {
    /// Scan `input` and return spans to redact.
    fn scan(&self, input: &str) -> Vec<DetectedSpan>;
}

/// Return the ordered list of built-in typed detectors.
///
/// Ordering matters: more-specific detectors (JWT, PEM, GCP-JSON) run before
/// generic ones (DB-URL, email) so a JWT containing what looks like a `.` doesn't
/// get partially redacted by the email detector.
///
/// Anthropic and OpenAI run BEFORE Stripe because Anthropic's `sk-ant-...` and
/// OpenAI's `sk-...` (dash, not underscore) would otherwise be over-matched by
/// the more generic Stripe rule if it were widened. Today Stripe uses `_` as a
/// separator and the others use `-`, but ordering keeps the labels accurate as
/// each provider's format evolves.
#[must_use]
pub fn built_in_detectors(redact_emails: bool) -> Vec<Box<dyn Detector>> {
    let mut detectors: Vec<Box<dyn Detector>> = vec![
        Box::new(RegexDetector {
            kind: DetectorKind::AwsKey,
            re: aws_re(),
        }),
        Box::new(RegexDetector {
            kind: DetectorKind::AnthropicKey,
            re: anthropic_re(),
        }),
        Box::new(RegexDetector {
            kind: DetectorKind::OpenAiKey,
            re: openai_re(),
        }),
        Box::new(RegexDetector {
            kind: DetectorKind::StripeWebhook,
            re: stripe_webhook_re(),
        }),
        Box::new(RegexDetector {
            kind: DetectorKind::StripeKey,
            re: stripe_re(),
        }),
        Box::new(RegexDetector {
            kind: DetectorKind::GithubPat,
            re: github_pat_re(),
        }),
        Box::new(RegexDetector {
            kind: DetectorKind::SlackToken,
            re: slack_re(),
        }),
        Box::new(RegexDetector {
            kind: DetectorKind::GoogleApiKey,
            re: google_api_re(),
        }),
        Box::new(RegexDetector {
            kind: DetectorKind::Jwt,
            re: jwt_re(),
        }),
        Box::new(PemDetector),
        Box::new(GcpServiceAccountDetector),
        Box::new(RegexDetector {
            kind: DetectorKind::DbUrl,
            re: db_url_re(),
        }),
        Box::new(KeyValueSecretDetector),
    ];
    if redact_emails {
        detectors.push(Box::new(RegexDetector {
            kind: DetectorKind::Email,
            re: email_re(),
        }));
    }
    detectors
}

/// Generic regex-backed detector.
struct RegexDetector {
    kind: DetectorKind,
    re: &'static Regex,
}

impl Detector for RegexDetector {
    fn scan(&self, input: &str) -> Vec<DetectedSpan> {
        self.re
            .find_iter(input)
            .map(|m| DetectedSpan {
                kind: self.kind.clone(),
                span: (m.start(), m.end()),
            })
            .collect()
    }
}

/// PEM private-key detector. Matches a `-----BEGIN ... PRIVATE KEY-----`
/// header through its matching END marker so the entire key body is removed.
struct PemDetector;

impl Detector for PemDetector {
    fn scan(&self, input: &str) -> Vec<DetectedSpan> {
        let begin_re = pem_begin_re();
        let end_re = pem_end_re();
        let mut out = Vec::new();
        for m in begin_re.find_iter(input) {
            let start = m.start();
            // Find the matching END marker after this BEGIN.
            if let Some(end_m) = end_re.find_at(input, m.end()) {
                out.push(DetectedSpan {
                    kind: DetectorKind::PemPrivateKey,
                    span: (start, end_m.end()),
                });
            }
        }
        out
    }
}

/// GCP service-account detector: redact a JSON object that contains both
/// `"type": "service_account"` and `"private_key"`.
///
/// Implementation is intentionally simple — we look for the `"type"` substring
/// and walk outward to find object braces. Service-account JSON is usually
/// pasted as-is (one object), so this catches the common case without needing
/// a full JSON parser.
struct GcpServiceAccountDetector;

impl Detector for GcpServiceAccountDetector {
    fn scan(&self, input: &str) -> Vec<DetectedSpan> {
        let probe = service_account_probe_re();
        let mut out = Vec::new();
        for m in probe.find_iter(input) {
            // Walk left to find the opening `{`.
            let Some(start) = input[..m.start()].rfind('{') else {
                continue;
            };
            // Walk right to find the matching `}`.
            let Some(rel_end) = balanced_brace_end(&input[start..]) else {
                continue;
            };
            let end = start + rel_end + 1;
            // Sanity check: the bracketed text must contain a private_key field too.
            if input[start..end].contains("\"private_key\"") {
                out.push(DetectedSpan {
                    kind: DetectorKind::GcpServiceAccount,
                    span: (start, end),
                });
            }
        }
        out
    }
}

/// `KEY=value` / `KEY: value` detector for short or low-entropy secrets that
/// would otherwise bypass the entropy gate (e.g., `password=hunter2`,
/// `API_TOKEN=abc123`). Only redacts the value half so the user can still see
/// which key was leaked.
struct KeyValueSecretDetector;

impl Detector for KeyValueSecretDetector {
    fn scan(&self, input: &str) -> Vec<DetectedSpan> {
        let re = key_value_secret_re();
        let mut out = Vec::new();
        for caps in re.captures_iter(input) {
            // The named `value` group is the half we redact; the key half
            // (and the `=`/`:` separator) stay in the output.
            if let Some(value) = caps.name("value") {
                let v = value.as_str();
                // Skip values an earlier detector already replaced. Without
                // this guard we would double-mark `key=[REDACTED:openai-key]`
                // as `key=[REDACTED:key-value-secret]`.
                if v.starts_with("[REDACTED:") {
                    continue;
                }
                out.push(DetectedSpan {
                    kind: DetectorKind::KeyValueSecret,
                    span: (value.start(), value.end()),
                });
            }
        }
        out
    }
}

fn key_value_secret_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // Match a credential-shaped key followed by `=` or `:` then non-whitespace
    // value (capped at 256 chars to avoid eating across statements). The value
    // group is the only thing redacted; `[REDACTED:key-value-secret]` lands in
    // the value's bytes, leaving the key/separator visible.
    R.get_or_init(|| {
        Regex::new(
            r#"(?i)\b(?:api[_-]?key|secret|password|passwd|token|auth|credential)\s*[:=]\s*(?P<value>[^\s"',]{1,256})"#,
        )
        .unwrap()
    })
}

fn balanced_brace_end(s: &str) -> Option<usize> {
    let mut depth = 0_i32;
    for (i, ch) in s.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// High-entropy scanner. Word-by-word: any token of length ≥ 24 with Shannon
/// entropy ≥ 3.5 is flagged as a likely opaque secret.
///
/// Operates on whitespace-delimited tokens so natural prose doesn't false-positive
/// (English words rarely exceed 24 chars, and the few that do — e.g.,
/// "antidisestablishmentarianism" — have low entropy due to repeated letters).
///
/// Skips tokens that contain `[REDACTED:` so the scanner doesn't re-redact the
/// markers placed by earlier detectors in the pipeline (the markers are
/// long-and-mixed-character enough to clear the entropy bar).
///
/// Threshold = 3.5 catches 32-char hex strings (entropy ≈ 3.9) and base64
/// tokens (entropy ≈ 4.5+). Going higher misses real hex secrets; going lower
/// risks false-positives on long natural prose tokens.
#[must_use]
pub fn high_entropy_scan(input: &str) -> Vec<DetectedSpan> {
    let mut spans = Vec::new();
    let mut byte_pos = 0;
    for token in input.split_whitespace() {
        let token_start = input[byte_pos..]
            .find(token)
            .map_or(byte_pos, |off| byte_pos + off);
        let token_end = token_start + token.len();
        byte_pos = token_end;
        if token.contains("[REDACTED:") {
            continue;
        }
        if token.len() >= 24 && shannon_entropy(token) >= 3.5 {
            spans.push(DetectedSpan {
                kind: DetectorKind::HighEntropy,
                span: (token_start, token_end),
            });
        }
    }
    spans
}

#[allow(clippy::cast_precision_loss)]
fn shannon_entropy(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let len = s.len() as f64;
    let mut counts = [0_u32; 256];
    for b in s.bytes() {
        counts[b as usize] += 1;
    }
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = f64::from(c) / len;
            -p * p.log2()
        })
        .sum()
}

// --- regex constants ---

fn aws_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b").unwrap())
}

fn stripe_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // Broadened to include restricted-key prefix `rk_` alongside `sk_`. Both
    // come in `live` and `test` flavors with the same 24+ char body.
    R.get_or_init(|| Regex::new(r"\b(?:sk|rk)_(?:live|test)_[0-9a-zA-Z]{24,}\b").unwrap())
}

fn stripe_webhook_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\bwhsec_[0-9a-zA-Z]{32,}\b").unwrap())
}

fn github_pat_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // ghp_/gho_/ghu_/ghs_/ghr_ tokens (PAT, OAuth, user-server, server, refresh)
    // are 40 chars total ("ghX_" + 36); `github_pat_` fine-grained tokens are
    // much longer (≥ 60 chars after the prefix).
    R.get_or_init(|| {
        Regex::new(r"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{60,})\b").unwrap()
    })
}

fn anthropic_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"\bsk-ant-(?:api|admin)\d{2,}-[A-Za-z0-9_-]{32,}\b").unwrap()
    })
}

fn openai_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // OpenAI tokens use `-` as a separator (vs. Stripe's `_`), with optional
    // project/service-account/admin prefixes. Run before Stripe so the more
    // specific shape wins on labelling.
    R.get_or_init(|| {
        Regex::new(r"\bsk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9_-]{20,}\b").unwrap()
    })
}

fn slack_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // Slack bot tokens have 3 numeric/id segments before the body
    // (`xoxb-T...-B...-secret`); some user/admin variants have 4. Accept
    // either shape, and a body of mixed alphanumerics (not just hex).
    R.get_or_init(|| Regex::new(r"\bxox[abprs]-\d+-\d+(?:-\d+)?-[A-Za-z0-9]+\b").unwrap())
}

fn google_api_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\bAIza[A-Za-z0-9_-]{35}\b").unwrap())
}

fn jwt_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"\beyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b").unwrap()
    })
}

fn db_url_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"\b(?:postgres(?:ql)?|mysql|mongodb(?:\+srv)?|redis|amqps?)://[^\s:@]+:[^\s@]+@[^\s/]+(?:/[^\s]*)?").unwrap()
    })
}

fn email_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // Pragmatic email regex; not strictly RFC-5322, but catches typical addresses
    // without false-positiving on `foo@bar` shell variables or markdown like `@user`.
    R.get_or_init(|| Regex::new(r"\b[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}\b").unwrap())
}

fn pem_begin_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"-----BEGIN [A-Z ]*PRIVATE KEY-----").unwrap())
}

fn pem_end_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"-----END [A-Z ]*PRIVATE KEY-----").unwrap())
}

fn service_account_probe_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#""type"\s*:\s*"service_account""#).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_with_first_detector(kind: &DetectorKind, input: &str) -> Vec<DetectedSpan> {
        let detectors = built_in_detectors(true);
        for d in detectors {
            let hits = d.scan(input);
            if !hits.is_empty() && hits[0].kind == *kind {
                return hits;
            }
        }
        Vec::new()
    }

    #[test]
    fn aws_access_key_matches() {
        let hits =
            scan_with_first_detector(&DetectorKind::AwsKey, "key: AKIAIOSFODNN7EXAMPLE here");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn aws_temp_key_matches() {
        let hits = scan_with_first_detector(
            &DetectorKind::AwsKey,
            "creds ASIAIOSFODNN7EXAMPLE temporary",
        );
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn stripe_live_key_matches() {
        let hits = scan_with_first_detector(
            &DetectorKind::StripeKey,
            "set sk_live_4eC39HqLyjWDarjtT1zdp7dc as the key",
        );
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn jwt_matches() {
        let hits = scan_with_first_detector(
            &DetectorKind::Jwt,
            "Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
        );
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn db_url_with_creds_matches() {
        let hits = scan_with_first_detector(
            &DetectorKind::DbUrl,
            "DATABASE_URL=postgres://alice:hunter2@db.example.com:5432/prod",
        );
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn pem_block_matches_full_block() {
        let input = "Note: -----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA...\n-----END RSA PRIVATE KEY-----\nrest of prompt";
        let hits = PemDetector.scan(input);
        assert_eq!(hits.len(), 1);
        let (s, e) = hits[0].span;
        assert!(input[s..e].starts_with("-----BEGIN RSA PRIVATE KEY-----"));
        assert!(input[s..e].ends_with("-----END RSA PRIVATE KEY-----"));
    }

    #[test]
    fn gcp_service_account_matches() {
        let json = r#"
        {
          "type": "service_account",
          "project_id": "my-proj",
          "private_key_id": "abc",
          "private_key": "-----BEGIN PRIVATE KEY-----\nMII...\n-----END PRIVATE KEY-----\n",
          "client_email": "svc@my-proj.iam.gserviceaccount.com"
        }
        "#;
        let hits = GcpServiceAccountDetector.scan(json);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn high_entropy_catches_long_random_token() {
        let token = "a8f9d2b4c7e1a0f6d3b8e5c2a9f4d7b1"; // 32 hex chars, high entropy
        let input = format!("opaque {token} embedded");
        let hits = high_entropy_scan(&input);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn high_entropy_does_not_catch_natural_prose() {
        let input = "this is just a normal sentence with reasonable words inside it";
        let hits = high_entropy_scan(input);
        assert!(hits.is_empty(), "false positive: {hits:?}");
    }

    #[test]
    fn high_entropy_skips_short_tokens() {
        let input = "abc def ghi jkl mno pqr"; // none ≥ 24 chars
        let hits = high_entropy_scan(input);
        assert!(hits.is_empty());
    }

    #[test]
    fn email_matches() {
        let hits = scan_with_first_detector(
            &DetectorKind::Email,
            "ping alice.smith+tag@example.co.uk for details",
        );
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn no_false_positive_on_aws_lookalike() {
        // 16-char alphanumeric that isn't AKIA/ASIA-prefixed.
        let hits = scan_with_first_detector(&DetectorKind::AwsKey, "ZZZAIOSFODNN7EXAMPLE");
        assert!(hits.is_empty());
    }

    #[test]
    fn anthropic_api_key_matches() {
        let hits = scan_with_first_detector(
            &DetectorKind::AnthropicKey,
            "Authorization: Bearer sk-ant-api03-aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789_-aB here",
        );
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn anthropic_admin_key_matches() {
        let hits = scan_with_first_detector(
            &DetectorKind::AnthropicKey,
            "ANTHROPIC_ADMIN_KEY=sk-ant-admin01-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-Z",
        );
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn slack_bot_token_matches() {
        let hits = scan_with_first_detector(
            &DetectorKind::SlackToken,
            "slack: xoxb-1234567890-9876543210-abcdef0123456789abcdef0123456789",
        );
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn google_api_key_matches() {
        // Real Google API keys are AIza + exactly 35 chars from [A-Za-z0-9_-].
        let hits = scan_with_first_detector(
            &DetectorKind::GoogleApiKey,
            "key=AIzaSyD-9tSrke72PouQMnMX-a7eZSW0jkFMBWY end",
        );
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn stripe_restricted_key_matches() {
        let hits = scan_with_first_detector(
            &DetectorKind::StripeKey,
            "set rk_live_4eC39HqLyjWDarjtT1zdp7dc and friends",
        );
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn stripe_webhook_secret_matches() {
        let hits = scan_with_first_detector(
            &DetectorKind::StripeWebhook,
            "STRIPE_WEBHOOK=whsec_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789",
        );
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn key_value_secret_redacts_short_value() {
        // `password=hunter2` should be caught even though "hunter2" has low
        // entropy and would slip past `high_entropy_scan`.
        let hits = KeyValueSecretDetector.scan("password=hunter2 and more");
        assert_eq!(hits.len(), 1);
        let (s, e) = hits[0].span;
        // Only the value half is in the span; the `password=` prefix stays
        // visible.
        assert_eq!(&"password=hunter2 and more"[s..e], "hunter2");
    }

    #[test]
    fn key_value_secret_redacts_with_colon_separator() {
        let hits = KeyValueSecretDetector.scan("API_KEY: secret_value_here ");
        assert_eq!(hits.len(), 1);
        let (s, e) = hits[0].span;
        assert_eq!(&"API_KEY: secret_value_here "[s..e], "secret_value_here");
    }

    #[test]
    fn key_value_secret_does_not_match_unrelated_text() {
        // A `password` mention without a key/value separator must not trigger.
        let hits = KeyValueSecretDetector.scan("we should reset the password later");
        assert!(hits.is_empty());
    }

    #[test]
    fn github_oauth_token_matches() {
        // gho_ tokens (OAuth user-to-server) follow the same shape as ghp_.
        let hits = scan_with_first_detector(
            &DetectorKind::GithubPat,
            "gho_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn shannon_entropy_zero_for_single_char_strings() {
        assert!((shannon_entropy("aaaaaaaaaa") - 0.0).abs() < 1e-9);
    }

    #[test]
    fn shannon_entropy_high_for_random_strings() {
        let h = shannon_entropy("a8f9d2b4c7e1a0f6d3b8e5c2a9f4d7b1");
        assert!(h > 3.5, "entropy of random hex was {h}");
    }
}
