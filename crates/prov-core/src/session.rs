//! Session identifier used across the capture pipeline.
//!
//! `SessionId` is the stable Claude Code conversation id surfaced on every hook
//! payload. We treat it as an opaque string but constrain its lexical shape so
//! it can safely be used as a directory name under `.git/prov-staging/`.
//!
//! Turn indices are kept as raw `u32` rather than a newtype — every consumer
//! reaches for the underlying integer immediately (file naming, comparison).
//! When U10 grows turn semantics that warrant a richer type, reintroduce the
//! newtype here.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Stable id for one Claude Code conversation.
///
/// Constructed via `SessionId::parse` which rejects path-traversal and other
/// shapes that would compromise the staging directory layout. Empty strings,
/// embedded slashes, leading dots, and embedded `..` are all refused.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    /// Validate and wrap an opaque session id from a hook payload.
    ///
    /// Rejects empty strings and any value that contains characters that would
    /// be unsafe as a single directory-name component on the filesystems prov
    /// targets. The validation is conservative on purpose: hook payloads come
    /// from a tool we don't fully control, and a malformed id must not leak
    /// into a `..` traversal of the staging tree.
    pub fn parse(s: impl Into<String>) -> Result<Self, SessionIdError> {
        let s = s.into();
        if s.is_empty() {
            return Err(SessionIdError::Empty);
        }
        if s.starts_with('.') {
            return Err(SessionIdError::LeadingDot);
        }
        for c in s.chars() {
            // Allow alphanumeric, `_`, `-`, `:` (Claude Code session ids are
            // typically `sess_<base32>` or UUID-shaped). Reject everything else.
            let allowed = c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':');
            if !allowed {
                return Err(SessionIdError::InvalidChar(c));
            }
        }
        Ok(Self(s))
    }

    /// Borrow as a `str` (for path construction, comparison, logging).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the owned string.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for SessionId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Errors raised by `SessionId::parse`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SessionIdError {
    /// An empty session id was provided.
    #[error("session id is empty")]
    Empty,
    /// The id starts with a `.` — would shadow `.` or `..` in the staging tree.
    #[error("session id starts with `.`, which would conflict with the staging directory layout")]
    LeadingDot,
    /// The id contained a character outside the allowed alphabet.
    #[error(
        "session id contains disallowed character `{0}`; allowed: alphanumeric, `_`, `-`, `:`"
    )]
    InvalidChar(char),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_typical_claude_code_id() {
        let id = SessionId::parse("sess_abc123XYZ").unwrap();
        assert_eq!(id.as_str(), "sess_abc123XYZ");
    }

    #[test]
    fn parse_rejects_empty() {
        assert_eq!(SessionId::parse(""), Err(SessionIdError::Empty));
    }

    #[test]
    fn parse_rejects_leading_dot() {
        assert_eq!(SessionId::parse(".sneaky"), Err(SessionIdError::LeadingDot));
    }

    #[test]
    fn parse_rejects_path_separator() {
        assert!(matches!(
            SessionId::parse("a/b"),
            Err(SessionIdError::InvalidChar('/'))
        ));
    }

    #[test]
    fn parse_rejects_dotdot() {
        // ".." starts with '.', caught by LeadingDot.
        assert_eq!(SessionId::parse(".."), Err(SessionIdError::LeadingDot));
    }

    #[test]
    fn parse_rejects_null_byte_and_backslash() {
        assert!(matches!(
            SessionId::parse("a\0b"),
            Err(SessionIdError::InvalidChar('\0'))
        ));
        assert!(matches!(
            SessionId::parse("a\\b"),
            Err(SessionIdError::InvalidChar('\\'))
        ));
    }

    #[test]
    fn session_id_display_matches_inner() {
        let id = SessionId::parse("abc").unwrap();
        assert_eq!(id.to_string(), "abc");
        assert_eq!(id.as_ref(), "abc");
    }
}
