//! `.provignore` parser.
//!
//! Same shape as `.gitignore`, but for content patterns rather than file paths.
//! Each non-comment, non-empty line is a regex; matches are redacted with the
//! marker `[REDACTED:provignore-rule:<index>]` so users can trace which rule
//! caught what during incident review.

use regex::Regex;

use crate::redactor::detectors::{DetectedSpan, DetectorKind};

/// Compiled `.provignore` ruleset.
#[derive(Debug)]
pub struct ProvIgnore {
    rules: Vec<Regex>,
}

impl ProvIgnore {
    /// Parse `.provignore` text. Lines starting with `#` are comments; blank
    /// lines are ignored. Each remaining line is compiled as a regex.
    ///
    /// Returns an error on the first invalid regex (with line number).
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Result<Self, ProvIgnoreError> {
        let mut rules = Vec::new();
        for (line_no, line) in s.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let re = Regex::new(trimmed).map_err(|e| ProvIgnoreError::InvalidRegex {
                line: line_no + 1,
                pattern: trimmed.to_string(),
                error: e.to_string(),
            })?;
            rules.push(re);
        }
        Ok(Self { rules })
    }

    /// Parse from a file at `path`. Empty path or missing file returns an empty
    /// ruleset (no error) — `.provignore` is optional.
    pub fn from_path<P: AsRef<std::path::Path>>(path: P) -> Result<Self, ProvIgnoreError> {
        match std::fs::read_to_string(path.as_ref()) {
            Ok(s) => Self::from_str(&s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self { rules: Vec::new() }),
            Err(e) => Err(ProvIgnoreError::Io(e.to_string())),
        }
    }

    /// Number of compiled rules.
    #[must_use]
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Scan `input` for matches across every rule. Each hit carries the rule
    /// index so the marker `[REDACTED:provignore-rule:<idx>]` is traceable.
    #[must_use]
    pub fn scan(&self, input: &str) -> Vec<DetectedSpan> {
        let mut out = Vec::new();
        for (idx, re) in self.rules.iter().enumerate() {
            for m in re.find_iter(input) {
                #[allow(clippy::cast_possible_truncation)]
                let i = idx as u32;
                out.push(DetectedSpan {
                    kind: DetectorKind::ProvIgnoreRule(i),
                    span: (m.start(), m.end()),
                });
            }
        }
        out
    }
}

/// Errors raised by `ProvIgnore`.
#[derive(Debug, thiserror::Error)]
pub enum ProvIgnoreError {
    /// A line failed regex compilation.
    #[error(".provignore line {line}: invalid regex `{pattern}`: {error}")]
    InvalidRegex {
        /// One-based line number in the source `.provignore` file.
        line: usize,
        /// The offending pattern text (verbatim).
        pattern: String,
        /// Underlying regex compiler error.
        error: String,
    },
    /// Filesystem I/O error reading a `.provignore` file.
    #[error("provignore I/O error: {0}")]
    Io(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_compiles_to_empty_ruleset() {
        let p = ProvIgnore::from_str("").unwrap();
        assert_eq!(p.rule_count(), 0);
    }

    #[test]
    fn comments_and_blanks_are_skipped() {
        let p = ProvIgnore::from_str("# header comment\n\nFoo\n# trailing\nBar\n").unwrap();
        assert_eq!(p.rule_count(), 2);
    }

    #[test]
    fn invalid_regex_is_rejected_with_line_number() {
        let bad = "valid\n[unbalanced\n";
        match ProvIgnore::from_str(bad) {
            Err(ProvIgnoreError::InvalidRegex { line: 2, .. }) => {}
            other => panic!("expected InvalidRegex on line 2, got {other:?}"),
        }
    }

    #[test]
    fn scan_returns_indexed_hits() {
        let p = ProvIgnore::from_str("Acme\nPhoenix").unwrap();
        let spans = p.scan("Working on Acme product and Phoenix initiative");
        assert_eq!(spans.len(), 2);
        // Acme is rule 0, Phoenix is rule 1.
        let kinds: Vec<&str> = spans
            .iter()
            .map(|s| match &s.kind {
                DetectorKind::ProvIgnoreRule(_) => "rule",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, vec!["rule", "rule"]);
    }

    #[test]
    fn missing_file_is_empty_ruleset() {
        let p = ProvIgnore::from_path("/tmp/does-not-exist-prov-test").unwrap();
        assert_eq!(p.rule_count(), 0);
    }
}
