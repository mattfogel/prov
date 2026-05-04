//! PR intent timeline data model + JSON / Markdown renderers.
//!
//! The Markdown shape matches the rendering example in
//! `docs/plans/2026-04-28-001-feat-prompt-provenance-v1-plan.md` lines 362-393:
//! a sticky comment marker, a one-line summary, one section per session
//! grouping turns, a "lines without provenance" footer, and a generator-tag
//! footer. Both renderers consume the same `Timeline` so the JSON envelope
//! and Markdown body are kept in lockstep.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::Serialize;

/// HTML marker the GitHub Action uses to upsert the comment in place.
pub const STICKY_MARKER: &str = "<!-- prov:pr-timeline -->";

/// A complete PR intent timeline, ready to render as JSON or Markdown.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Timeline {
    /// One section per Claude Code session, in chronological order.
    pub sessions: Vec<Session>,
    /// Per-file ranges where the resolver returned no provenance.
    pub no_provenance: Vec<NoProvenanceRange>,
    /// Total resolved turns across all sessions (matches `sum(sessions[].turns.len())`).
    pub total_turns: u32,
    /// Total lines in the diff that resolved to no provenance.
    pub total_no_provenance_lines: u32,
    /// Prov version that generated the comment (for the footer). Retained for
    /// the existing JSON contract — `prov_version` is the canonical name shared
    /// across other CLI envelopes (`log`, `search`, `reindex`).
    pub generator_version: String,
    /// Prov version that emitted this envelope. Mirrors `generator_version`
    /// under the canonical name used by other CLI surfaces.
    pub prov_version: String,
}

/// One Claude Code session as it surfaced in the PR diff.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Session {
    /// Stable Claude Code session id.
    pub conversation_id: String,
    /// ISO-8601 timestamp of the first turn in the session within this PR.
    pub started_at: String,
    /// Model captured at session start.
    pub model: String,
    /// Turns in stable order, surviving turns first then superseded ones.
    pub turns: Vec<Turn>,
}

/// One turn within a session.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Turn {
    /// Zero-based turn index from the original session (preserved across rewrites).
    #[allow(clippy::struct_field_names)]
    pub turn_index: u32,
    /// Originating prompt (post-redaction).
    pub prompt: String,
    /// ISO-8601 timestamp captured at turn boundary.
    pub timestamp: String,
    /// Lines this turn contributed to the final head diff, grouped by file.
    /// Empty when the turn was superseded.
    pub files: Vec<TurnFileLines>,
    /// True when none of this turn's lines survive into the head diff.
    pub superseded: bool,
}

/// Per-file line count for a turn.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TurnFileLines {
    /// Repo-relative path.
    pub file: String,
    /// Number of lines this turn contributed to this file in the final diff.
    pub lines: u32,
}

/// One contiguous range of diff lines without resolvable provenance.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct NoProvenanceRange {
    /// Repo-relative path.
    pub file: String,
    /// Inclusive `[start, end]` line range in head.
    pub line_start: u32,
    /// Inclusive end.
    pub line_end: u32,
}

impl Timeline {
    /// Total count of sessions in the timeline.
    pub fn session_count(&self) -> u32 {
        u32::try_from(self.sessions.len()).unwrap_or(u32::MAX)
    }

    /// Render the timeline as a Markdown comment body, including the sticky marker.
    #[must_use]
    pub fn to_markdown(&self) -> String {
        let mut s = String::new();
        s.push_str(STICKY_MARKER);
        s.push('\n');
        s.push_str("## PR Intent Timeline\n\n");

        if self.sessions.is_empty() && self.no_provenance.is_empty() {
            s.push_str("No Prov-tracked turns in this PR.\n");
            self.push_footer(&mut s);
            return s;
        }

        s.push_str(&self.summary_line());
        s.push_str("\n\n");

        for (idx, session) in self.sessions.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let n = (idx + 1) as u32;
            session.append_markdown(&mut s, n);
            s.push('\n');
        }

        if !self.no_provenance.is_empty() {
            writeln!(
                s,
                "### {} line(s) without provenance",
                self.total_no_provenance_lines
            )
            .unwrap();
            for range in &self.no_provenance {
                let span = if range.line_start == range.line_end {
                    range.line_start.to_string()
                } else {
                    format!("{}-{}", range.line_start, range.line_end)
                };
                writeln!(
                    s,
                    "- `{}:{}` — pre-existing or human-authored. Run `prov backfill` to attempt historical capture.",
                    range.file, span
                )
                .unwrap();
            }
            s.push('\n');
        }

        self.push_footer(&mut s);
        s
    }

    fn summary_line(&self) -> String {
        let session_word = if self.session_count() == 1 {
            "session"
        } else {
            "sessions"
        };
        let turns_word = if self.total_turns == 1 {
            "turn"
        } else {
            "turns"
        };
        let mut s = format!(
            "This PR contains {} {turns_word} across {} Claude Code {session_word}",
            self.total_turns,
            self.session_count()
        );
        if self.total_no_provenance_lines > 0 {
            let line_word = if self.total_no_provenance_lines == 1 {
                "line"
            } else {
                "lines"
            };
            write!(
                s,
                ", plus {} {line_word} without provenance",
                self.total_no_provenance_lines
            )
            .unwrap();
        }
        s.push('.');
        s
    }

    fn push_footer(&self, s: &mut String) {
        writeln!(
            s,
            "[Generated by Prov v{} · query any turn with `prov log <file>:<line>`]",
            self.generator_version
        )
        .unwrap();
    }
}

impl Session {
    fn append_markdown(&self, s: &mut String, ordinal: u32) {
        let date = self.started_at.get(..10).unwrap_or(&self.started_at);
        writeln!(
            s,
            "### Session {ordinal} — `{}` · {} · {} turn(s) · {}\n",
            self.conversation_id,
            date,
            self.turns.len(),
            self.model
        )
        .unwrap();
        for (i, turn) in self.turns.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let n = (i + 1) as u32;
            turn.append_markdown(s, n);
        }
    }
}

impl Turn {
    fn append_markdown(&self, s: &mut String, ordinal: u32) {
        let prompt = quote_prompt(&self.prompt);
        if self.superseded {
            writeln!(
                s,
                "{ordinal}. ~~{prompt}~~ _(superseded — final code does not contain this turn's output)_"
            )
            .unwrap();
            return;
        }
        let files = self
            .files
            .iter()
            .map(|f| format!("{} ({} lines)", f.file, f.lines))
            .collect::<Vec<_>>()
            .join(", ");
        if files.is_empty() {
            writeln!(s, "{ordinal}. **{prompt}**").unwrap();
        } else {
            writeln!(s, "{ordinal}. **{prompt}** — _{files}_").unwrap();
        }
    }
}

/// Wrap a prompt in straight double-quotes for the Markdown bullet, replacing
/// any internal `"` with `\"` so the rendering doesn't break visually.
fn quote_prompt(p: &str) -> String {
    let cleaned: String = p
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .map(|c| if c == '"' { '\'' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    let truncated = if trimmed.len() > 200 {
        format!("{}…", &trimmed[..200])
    } else {
        trimmed.to_string()
    };
    format!("\"{truncated}\"")
}

/// Builder used by [`crate::commands::pr_timeline`] to assemble a timeline
/// from per-line resolver results.
pub struct TimelineBuilder {
    sessions: BTreeMap<String, SessionAccumulator>,
    no_provenance: BTreeMap<String, Vec<u32>>,
    total_no_prov: u32,
    generator_version: String,
}

struct SessionAccumulator {
    conversation_id: String,
    model: String,
    earliest_ts: String,
    turns: BTreeMap<u32, TurnAccumulator>,
}

struct TurnAccumulator {
    turn_index: u32,
    prompt: String,
    timestamp: String,
    /// File → line count that survived into the diff.
    file_lines: BTreeMap<String, u32>,
}

impl TimelineBuilder {
    /// Construct an empty builder tagged with the prov version that will appear
    /// in the rendered footer.
    pub fn new(generator_version: impl Into<String>) -> Self {
        Self {
            sessions: BTreeMap::new(),
            no_provenance: BTreeMap::new(),
            total_no_prov: 0,
            generator_version: generator_version.into(),
        }
    }

    /// Record one resolved turn-line pair (a diff line whose blame-traced
    /// commit had a note covering that line).
    pub fn add_turn_line(&mut self, info: &TurnLineInfo<'_>) {
        let session = self
            .sessions
            .entry(info.conversation_id.to_string())
            .or_insert_with(|| SessionAccumulator {
                conversation_id: info.conversation_id.to_string(),
                model: info.model.to_string(),
                earliest_ts: info.timestamp.to_string(),
                turns: BTreeMap::new(),
            });
        if info.timestamp < session.earliest_ts.as_str() {
            session.earliest_ts = info.timestamp.to_string();
        }
        let turn = session
            .turns
            .entry(info.turn_index)
            .or_insert_with(|| TurnAccumulator {
                turn_index: info.turn_index,
                prompt: info.prompt.to_string(),
                timestamp: info.timestamp.to_string(),
                file_lines: BTreeMap::new(),
            });
        *turn.file_lines.entry(info.file.to_string()).or_insert(0) += 1;
    }

    /// Record one turn that exists on a PR commit but does not survive into the
    /// final diff — rendered as ~~strikethrough~~ in Markdown.
    pub fn add_superseded_turn(&mut self, info: &SupersededTurnInfo<'_>) {
        let session = self
            .sessions
            .entry(info.conversation_id.to_string())
            .or_insert_with(|| SessionAccumulator {
                conversation_id: info.conversation_id.to_string(),
                model: info.model.to_string(),
                earliest_ts: info.timestamp.to_string(),
                turns: BTreeMap::new(),
            });
        if info.timestamp < session.earliest_ts.as_str() {
            session.earliest_ts = info.timestamp.to_string();
        }
        // Don't overwrite a surviving turn with a superseded marker.
        session
            .turns
            .entry(info.turn_index)
            .or_insert_with(|| TurnAccumulator {
                turn_index: info.turn_index,
                prompt: info.prompt.to_string(),
                timestamp: info.timestamp.to_string(),
                file_lines: BTreeMap::new(),
            });
    }

    /// Record one diff line where the resolver returned no provenance.
    pub fn add_no_provenance_line(&mut self, file: &str, line: u32) {
        self.no_provenance
            .entry(file.to_string())
            .or_default()
            .push(line);
        self.total_no_prov = self.total_no_prov.saturating_add(1);
    }

    /// Finalize: sort sessions chronologically, sort turns by index, collapse
    /// the per-line no-provenance map into ranges.
    #[must_use]
    pub fn build(self) -> Timeline {
        let mut sessions: Vec<Session> = self
            .sessions
            .into_values()
            .map(|acc| {
                let mut turns: Vec<Turn> = acc
                    .turns
                    .into_values()
                    .map(|t| {
                        let files: Vec<TurnFileLines> = t
                            .file_lines
                            .into_iter()
                            .map(|(file, lines)| TurnFileLines { file, lines })
                            .collect();
                        let superseded = files.is_empty();
                        Turn {
                            turn_index: t.turn_index,
                            prompt: t.prompt,
                            timestamp: t.timestamp,
                            files,
                            superseded,
                        }
                    })
                    .collect();
                turns.sort_by_key(|t| t.turn_index);
                Session {
                    conversation_id: acc.conversation_id,
                    started_at: acc.earliest_ts,
                    model: acc.model,
                    turns,
                }
            })
            .collect();
        sessions.sort_by(|a, b| a.started_at.cmp(&b.started_at));

        let mut total_turns: u32 = 0;
        for s in &sessions {
            total_turns = total_turns.saturating_add(u32::try_from(s.turns.len()).unwrap_or(0));
        }

        let no_provenance = collapse_ranges(self.no_provenance);

        Timeline {
            sessions,
            no_provenance,
            total_turns,
            total_no_provenance_lines: self.total_no_prov,
            generator_version: self.generator_version.clone(),
            prov_version: self.generator_version,
        }
    }
}

/// Borrowed view passed to [`TimelineBuilder::add_turn_line`].
pub struct TurnLineInfo<'a> {
    pub file: &'a str,
    pub conversation_id: &'a str,
    pub turn_index: u32,
    pub prompt: &'a str,
    pub model: &'a str,
    pub timestamp: &'a str,
}

/// Borrowed view passed to [`TimelineBuilder::add_superseded_turn`].
pub struct SupersededTurnInfo<'a> {
    pub conversation_id: &'a str,
    pub turn_index: u32,
    pub prompt: &'a str,
    pub model: &'a str,
    pub timestamp: &'a str,
}

/// Collapse a per-file vec of line numbers into contiguous ranges.
fn collapse_ranges(map: BTreeMap<String, Vec<u32>>) -> Vec<NoProvenanceRange> {
    let mut out = Vec::new();
    for (file, mut lines) in map {
        if lines.is_empty() {
            continue;
        }
        lines.sort_unstable();
        lines.dedup();
        let mut start = lines[0];
        let mut end = lines[0];
        for &l in &lines[1..] {
            if l == end + 1 {
                end = l;
            } else {
                out.push(NoProvenanceRange {
                    file: file.clone(),
                    line_start: start,
                    line_end: end,
                });
                start = l;
                end = l;
            }
        }
        out.push(NoProvenanceRange {
            file,
            line_start: start,
            line_end: end,
        });
    }
    out.sort_by(|a, b| a.file.cmp(&b.file).then(a.line_start.cmp(&b.line_start)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty() -> Timeline {
        Timeline {
            sessions: Vec::new(),
            no_provenance: Vec::new(),
            total_turns: 0,
            total_no_provenance_lines: 0,
            generator_version: "0.1.0".into(),
            prov_version: "0.1.0".into(),
        }
    }

    #[test]
    fn empty_timeline_renders_no_turns_message() {
        let md = empty().to_markdown();
        assert!(md.starts_with("<!-- prov:pr-timeline -->"));
        assert!(md.contains("No Prov-tracked turns in this PR."));
        assert!(md.contains("Generated by Prov v0.1.0"));
    }

    #[test]
    fn single_session_two_turns_renders_expected_shape() {
        let mut b = TimelineBuilder::new("0.1.0");
        for _ in 0..3 {
            b.add_turn_line(&TurnLineInfo {
                file: "src/payments.ts",
                conversation_id: "sess_abc",
                turn_index: 0,
                prompt: "Add Stripe webhook handling",
                model: "claude-opus-4-7",
                timestamp: "2026-04-26T10:00:00Z",
            });
        }
        b.add_turn_line(&TurnLineInfo {
            file: "src/payments.ts",
            conversation_id: "sess_abc",
            turn_index: 1,
            prompt: "Use a 24h dedupe window",
            model: "claude-opus-4-7",
            timestamp: "2026-04-26T10:05:00Z",
        });
        let t = b.build();
        let md = t.to_markdown();
        assert!(md.contains("### Session 1 — `sess_abc`"));
        assert!(md.contains("2026-04-26"));
        assert!(md.contains("claude-opus-4-7"));
        assert!(md.contains("**\"Add Stripe webhook handling\"** — _src/payments.ts (3 lines)_"));
        assert!(md.contains("**\"Use a 24h dedupe window\"** — _src/payments.ts (1 lines)_"));
    }

    #[test]
    fn superseded_turn_renders_strikethrough() {
        let mut b = TimelineBuilder::new("0.1.0");
        b.add_turn_line(&TurnLineInfo {
            file: "src/x.ts",
            conversation_id: "sess_1",
            turn_index: 0,
            prompt: "first prompt",
            model: "claude-haiku-4-5",
            timestamp: "2026-04-26T10:00:00Z",
        });
        b.add_superseded_turn(&SupersededTurnInfo {
            conversation_id: "sess_1",
            turn_index: 1,
            prompt: "Fix the type error",
            model: "claude-haiku-4-5",
            timestamp: "2026-04-26T10:05:00Z",
        });
        let t = b.build();
        let md = t.to_markdown();
        assert!(md.contains("~~\"Fix the type error\"~~"));
        assert!(md.contains("(superseded"));
    }

    #[test]
    fn multiple_sessions_sort_by_earliest_timestamp() {
        let mut b = TimelineBuilder::new("0.1.0");
        b.add_turn_line(&TurnLineInfo {
            file: "a.rs",
            conversation_id: "sess_late",
            turn_index: 0,
            prompt: "later",
            model: "m",
            timestamp: "2026-04-27T10:00:00Z",
        });
        b.add_turn_line(&TurnLineInfo {
            file: "b.rs",
            conversation_id: "sess_early",
            turn_index: 0,
            prompt: "earlier",
            model: "m",
            timestamp: "2026-04-26T10:00:00Z",
        });
        let t = b.build();
        let md = t.to_markdown();
        let early_pos = md.find("sess_early").unwrap();
        let late_pos = md.find("sess_late").unwrap();
        assert!(early_pos < late_pos);
    }

    #[test]
    fn no_provenance_ranges_collapse_consecutive_lines() {
        let mut b = TimelineBuilder::new("0.1.0");
        for line in [5, 6, 7, 12] {
            b.add_no_provenance_line("src/index.ts", line);
        }
        let t = b.build();
        assert_eq!(t.total_no_provenance_lines, 4);
        assert_eq!(t.no_provenance.len(), 2);
        assert_eq!(t.no_provenance[0].line_start, 5);
        assert_eq!(t.no_provenance[0].line_end, 7);
        assert_eq!(t.no_provenance[1].line_start, 12);
        assert_eq!(t.no_provenance[1].line_end, 12);

        let md = t.to_markdown();
        assert!(md.contains("4 line(s) without provenance"));
        assert!(md.contains("`src/index.ts:5-7`"));
        assert!(md.contains("`src/index.ts:12`"));
    }

    #[test]
    fn quote_prompt_strips_internal_quotes_and_truncates() {
        let q = quote_prompt("Add \"Stripe\" handling\nplus extra context");
        assert!(q.starts_with("\"Add 'Stripe' handling"));
        assert!(!q.contains('\n'));
        let long = "x".repeat(300);
        let qq = quote_prompt(&long);
        assert!(qq.contains('…'));
    }

    #[test]
    fn summary_line_pluralizes_correctly() {
        let mut b = TimelineBuilder::new("0.1.0");
        b.add_turn_line(&TurnLineInfo {
            file: "a",
            conversation_id: "s",
            turn_index: 0,
            prompt: "p",
            model: "m",
            timestamp: "2026-01-01T00:00:00Z",
        });
        let t = b.build();
        assert!(t
            .summary_line()
            .contains("1 turn across 1 Claude Code session."));
    }
}
