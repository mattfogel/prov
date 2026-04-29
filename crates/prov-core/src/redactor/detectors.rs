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
    /// Stripe API key (`sk_live_...` / `sk_test_...`).
    StripeKey,
    /// GitHub personal access token (`ghp_...` / `github_pat_...`).
    GithubPat,
    /// JSON Web Token (header.payload.signature).
    Jwt,
    /// GCP service-account JSON blob.
    GcpServiceAccount,
    /// PEM-encoded private key block.
    PemPrivateKey,
    /// Database URL with embedded credentials.
    DbUrl,
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
            Self::GithubPat => "github-pat".into(),
            Self::Jwt => "jwt".into(),
            Self::GcpServiceAccount => "gcp-service-account".into(),
            Self::PemPrivateKey => "pem-private-key".into(),
            Self::DbUrl => "db-url".into(),
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
#[must_use]
pub fn built_in_detectors(redact_emails: bool) -> Vec<Box<dyn Detector>> {
    let mut detectors: Vec<Box<dyn Detector>> = vec![
        Box::new(RegexDetector {
            kind: DetectorKind::AwsKey,
            re: aws_re(),
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
            kind: DetectorKind::Jwt,
            re: jwt_re(),
        }),
        Box::new(PemDetector),
        Box::new(GcpServiceAccountDetector),
        Box::new(RegexDetector {
            kind: DetectorKind::DbUrl,
            re: db_url_re(),
        }),
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
    R.get_or_init(|| Regex::new(r"\bsk_(?:live|test)_[0-9a-zA-Z]{24,}\b").unwrap())
}

fn github_pat_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // ghp_ tokens are 40 chars total ("ghp_" + 36); github_pat_ is much longer.
    R.get_or_init(|| {
        Regex::new(r"\b(?:ghp_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{60,})\b").unwrap()
    })
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
    fn shannon_entropy_zero_for_single_char_strings() {
        assert!((shannon_entropy("aaaaaaaaaa") - 0.0).abs() < 1e-9);
    }

    #[test]
    fn shannon_entropy_high_for_random_strings() {
        let h = shannon_entropy("a8f9d2b4c7e1a0f6d3b8e5c2a9f4d7b1");
        assert!(h > 3.5, "entropy of random hex was {h}");
    }
}
