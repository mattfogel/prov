//! Write-time secret redactor.
//!
//! Every prompt and conversation summary passes through `Redactor::redact` before
//! it touches the staging dir or the notes ref. Built-in typed detectors handle the
//! common cases (AWS, Stripe, GitHub PAT, JWT, GCP service-account JSON, PEM private
//! keys, DB URLs with embedded credentials, email addresses); a Shannon-entropy
//! detector catches high-entropy unknowns; a per-repo `.provignore` adds project-
//! specific patterns (customer names, internal codenames, private URLs).
//!
//! Replacement marker: `[REDACTED:<type>]` (e.g., `[REDACTED:aws-key]`,
//! `[REDACTED:provignore-rule:5]`). The original is never written.
//!
//! Detector ordering is deterministic: typed detectors run first (specific shapes
//! catch first), then `.provignore` rules in declaration order, then high-entropy
//! as the last-line generic catch. Once a span is redacted by an earlier detector,
//! later detectors see the `[REDACTED:...]` marker rather than the original text.
//!
//! False-negative awareness: the high-entropy detector misses base64-encoded
//! content (entropy < 4.0). The typed detectors above (GCP JSON, PEM, DB URL)
//! cover the most common base64-shaped secret formats.

pub mod detectors;
pub mod provignore;

use detectors::{DetectedSpan, DetectorKind};
use provignore::ProvIgnore;

/// Result of redacting a string.
#[derive(Debug, Clone)]
pub struct RedactedText {
    /// The text with every detected secret replaced by `[REDACTED:<type>]`.
    pub text: String,
    /// What was redacted, in detection order. Useful for surfacing detector hits
    /// to the user (e.g., the pre-push gate's audit-log entry).
    pub redactions: Vec<RedactionRecord>,
}

/// One redaction event recorded by `Redactor::redact`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactionRecord {
    /// What kind of detector matched.
    pub kind: DetectorKind,
    /// `[start, end)` byte offsets in the **original** input.
    pub span: (usize, usize),
}

/// Redactor pipeline: built-in typed detectors + optional `.provignore` rules.
#[derive(Debug, Default)]
pub struct Redactor {
    provignore: Option<ProvIgnore>,
    /// When true (the default), email addresses are redacted. Some users prefer
    /// to keep emails since prompts often mention `support@example.com`-style
    /// public addresses; configurable.
    pub redact_emails: bool,
}

impl Redactor {
    /// Construct a redactor with built-in detectors only (no `.provignore`).
    #[must_use]
    pub fn new() -> Self {
        Self {
            provignore: None,
            redact_emails: true,
        }
    }

    /// Attach a `.provignore` ruleset.
    #[must_use]
    pub fn with_provignore(mut self, p: ProvIgnore) -> Self {
        self.provignore = Some(p);
        self
    }

    /// Turn email redaction on/off (default: on).
    #[must_use]
    pub fn redact_emails(mut self, on: bool) -> Self {
        self.redact_emails = on;
        self
    }

    /// Redact `input`. Returns the scrubbed text plus a record of what changed.
    ///
    /// Detector order: typed detectors → `.provignore` rules → high-entropy.
    /// Each pass operates on the post-replacement string from the previous pass,
    /// so once a span becomes `[REDACTED:...]` it cannot match a downstream detector.
    pub fn redact(&self, input: &str) -> RedactedText {
        let mut text = input.to_string();
        let mut records: Vec<RedactionRecord> = Vec::new();

        // Pass 1: typed detectors. Order matters — JWT before high-entropy etc.
        for detector in detectors::built_in_detectors(self.redact_emails) {
            let spans = detector.scan(&text);
            apply_spans(&mut text, &mut records, &spans);
        }

        // Pass 2: provignore rules.
        if let Some(p) = &self.provignore {
            let spans = p.scan(&text);
            apply_spans(&mut text, &mut records, &spans);
        }

        // Pass 3: high-entropy (last-line generic catch).
        let spans = detectors::high_entropy_scan(&text);
        apply_spans(&mut text, &mut records, &spans);

        RedactedText {
            text,
            redactions: records,
        }
    }
}

/// Apply a batch of detected spans to `text` in reverse order so byte offsets
/// stay valid as we splice.
fn apply_spans(text: &mut String, records: &mut Vec<RedactionRecord>, spans: &[DetectedSpan]) {
    let mut sorted: Vec<&DetectedSpan> = spans.iter().collect();
    sorted.sort_by_key(|s| std::cmp::Reverse(s.span.0));

    for span in sorted {
        let (start, end) = span.span;
        if start >= text.len() || end > text.len() || start >= end {
            continue;
        }
        let marker = format!("[REDACTED:{}]", span.kind.as_marker());
        text.replace_range(start..end, &marker);
        records.push(RedactionRecord {
            kind: span.kind.clone(),
            span: (start, end),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redactor_no_secrets_returns_input_unchanged() {
        let r = Redactor::new();
        let result = r.redact("just a plain prompt with nothing sensitive");
        assert_eq!(result.text, "just a plain prompt with nothing sensitive");
        assert!(result.redactions.is_empty());
    }

    #[test]
    fn redactor_aws_key_is_replaced() {
        let r = Redactor::new();
        let result = r.redact("My AWS key is AKIAIOSFODNN7EXAMPLE for testing");
        assert!(result.text.contains("[REDACTED:aws-key]"));
        assert!(!result.text.contains("AKIAIOSFODNN7EXAMPLE"));
        assert_eq!(result.redactions.len(), 1);
        assert_eq!(result.redactions[0].kind.as_marker(), "aws-key");
    }

    #[test]
    fn redactor_multiple_distinct_secrets_all_caught() {
        let r = Redactor::new();
        let input =
            "key=AKIAIOSFODNN7EXAMPLE token=ghp_thisIsAFakePAT123456789012345678901234567890";
        let result = r.redact(input);
        assert!(result.text.contains("[REDACTED:aws-key]"));
        assert!(result.text.contains("[REDACTED:github-pat]"));
        assert_eq!(result.redactions.len(), 2);
    }

    #[test]
    fn redactor_email_off_skips_email() {
        let r = Redactor::new().redact_emails(false);
        let result = r.redact("contact alice@example.com about it");
        assert!(result.text.contains("alice@example.com"));
        assert!(result.redactions.is_empty());
    }

    #[test]
    fn redactor_email_on_redacts_email() {
        let r = Redactor::new();
        let result = r.redact("contact alice@example.com about it");
        assert!(result.text.contains("[REDACTED:email]"));
        assert!(!result.text.contains("alice@example.com"));
    }

    #[test]
    fn redactor_provignore_redacts_custom_pattern() {
        let p = ProvIgnore::from_str("Acme Corp\nProject Phoenix").unwrap();
        let r = Redactor::new().with_provignore(p);
        let result = r.redact("Working on the Acme Corp launch and Project Phoenix");
        assert!(result.text.contains("[REDACTED:provignore-rule:0]"));
        assert!(result.text.contains("[REDACTED:provignore-rule:1]"));
        assert!(!result.text.contains("Acme Corp"));
        assert!(!result.text.contains("Project Phoenix"));
    }
}
