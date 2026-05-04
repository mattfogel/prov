//! `prov backfill` — best-effort historical capture from stored Claude Code
//! transcript files.
//!
//! For each session JSONL under `~/.claude/projects/<sanitized-cwd>/`, parse
//! turns and tool-use edits, then match the session to a single commit by
//! time-window + file overlap + content-hash overlap. The highest-scoring
//! commit per session gets a backfilled note marked `derived_from: backfill`,
//! and every backfilled prompt passes through the same redactor that live
//! capture uses.
//!
//! Idempotency: a commit that already carries a non-backfill note is left
//! alone. Re-running backfill replaces an existing backfill note with the
//! latest match (in case the algorithm or fixtures changed).
//!
//! This command is "best-effort" by design — it WILL miss commits whose
//! diffs were heavily reformatted between AI capture and commit, and it will
//! silently skip below-threshold matches. Both behaviors are documented in
//! the v1 plan.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use clap::Parser;

use prov_core::git::{Git, GitError};
use prov_core::redactor::Redactor;
use prov_core::schema::{DerivedFrom, Edit, Note};
use prov_core::storage::notes::NotesStore;
use prov_core::storage::sqlite::Cache;
use prov_core::storage::NOTES_REF_PUBLIC;
use prov_core::transcript::{parse_transcript, ParsedEdit, ParsedSession, TranscriptError};

use super::common::CACHE_FILENAME;

#[derive(Parser, Debug)]
pub struct Args {
    /// Skip the interactive consent prompt for transcript-file access.
    #[arg(long)]
    pub yes: bool,
    /// Allow backfilling commits authored by a different `user.email` (loud warning).
    #[arg(long)]
    pub cross_author: bool,
    /// Surface every backfilled note regardless of confidence score.
    #[arg(long)]
    pub include_low_confidence: bool,
    /// Override the auto-discovered Claude Code transcript directory or file.
    #[arg(long, value_name = "PATH")]
    pub transcript_path: Option<String>,
}

/// Confidence floor. Sessions below this score are skipped unless the user
/// passes `--include-low-confidence`. The threshold is deliberately permissive
/// — most real sessions match either at score 1.0 (every edit covered) or
/// near 0 (none covered); the floor is here to suppress accidental noise.
const DEFAULT_CONFIDENCE_FLOOR: f32 = 0.6;

/// Hours of slack on either side of a session when scanning candidate commits.
/// A 4-hour grace catches the typical "session ends Friday afternoon, commit
/// goes out Monday morning" pattern without sweeping in unrelated work.
const TIME_WINDOW_HOURS: i64 = 4;

#[allow(clippy::needless_pass_by_value)]
pub fn run(args: Args) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("could not read current directory")?;
    let git = match Git::discover(&cwd) {
        Ok(g) => g,
        Err(GitError::NotARepo) => return Err(anyhow!("not in a git repo")),
        Err(e) => return Err(e.into()),
    };

    let transcripts = discover_transcripts(&git, args.transcript_path.as_deref())?;
    if transcripts.files.is_empty() {
        println!(
            "prov backfill: no transcript files found under {}",
            transcripts.source.display()
        );
        return Ok(());
    }
    confirm_or_bail(&transcripts, args.yes)?;

    let ctx = RunCtx {
        user_email: git
            .capture(["config", "user.email"])
            .map(|s| s.trim().to_string())
            .unwrap_or_default(),
        floor: if args.include_low_confidence {
            0.0
        } else {
            DEFAULT_CONFIDENCE_FLOOR
        },
        cross_author: args.cross_author,
        store: NotesStore::new(git.clone(), NOTES_REF_PUBLIC),
        cache_path: git.git_dir().join(CACHE_FILENAME),
        redactor: Redactor::new(),
        candidates: load_candidate_commits(&git)?,
    };

    let mut counts = RunCounts::default();
    for transcript_path in &transcripts.files {
        process_transcript(transcript_path, &ctx, &mut counts);
    }

    println!(
        "prov backfill: {} note(s) written; {} session(s) without a match, \
         {} below confidence floor, \
         {} commit(s) already carry live notes, \
         {} commit(s) cross-author",
        counts.written,
        counts.skipped_no_match,
        counts.skipped_low_confidence,
        counts.skipped_existing_live,
        counts.skipped_cross_author,
    );
    Ok(())
}

struct RunCtx {
    user_email: String,
    floor: f32,
    cross_author: bool,
    store: NotesStore,
    cache_path: PathBuf,
    redactor: Redactor,
    candidates: Vec<CommitMeta>,
}

#[derive(Default)]
struct RunCounts {
    written: u32,
    skipped_no_match: u32,
    skipped_low_confidence: u32,
    skipped_existing_live: u32,
    skipped_cross_author: u32,
}

fn process_transcript(transcript_path: &Path, ctx: &RunCtx, counts: &mut RunCounts) {
    let session = match parse_transcript(transcript_path) {
        Ok(s) => s,
        Err(TranscriptError::Io(msg)) => {
            eprintln!("warning: skipping {}: {msg}", transcript_path.display());
            return;
        }
    };
    if session.session_id.is_empty() || session.edits.is_empty() {
        return;
    }
    let Some(best) = best_match(&session, &ctx.candidates) else {
        counts.skipped_no_match += 1;
        return;
    };
    if best.confidence < ctx.floor {
        counts.skipped_low_confidence += 1;
        return;
    }
    if !ctx.cross_author
        && !ctx.user_email.is_empty()
        && !best
            .candidate
            .author_email
            .eq_ignore_ascii_case(&ctx.user_email)
    {
        counts.skipped_cross_author += 1;
        eprintln!(
            "skipping {}: commit author {} != {} (pass --cross-author to override)",
            short_sha(&best.candidate.sha),
            best.candidate.author_email,
            ctx.user_email,
        );
        return;
    }
    match ctx.store.read(&best.candidate.sha) {
        Ok(Some(existing)) if !is_backfill_only(&existing) => {
            counts.skipped_existing_live += 1;
            return;
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!(
                "warning: could not read existing note for {}: {e}",
                short_sha(&best.candidate.sha)
            );
            return;
        }
    }
    let edits = build_note_edits(&session, &best, transcript_path, &ctx.redactor);
    if edits.is_empty() {
        counts.skipped_no_match += 1;
        return;
    }
    let note = Note::new(edits);
    if let Err(e) = ctx.store.write(&best.candidate.sha, &note) {
        eprintln!(
            "warning: failed to write note for {}: {e}",
            short_sha(&best.candidate.sha)
        );
        return;
    }
    update_cache(&ctx.cache_path, &ctx.store, &best.candidate.sha, &note);
    counts.written += 1;
    println!(
        "backfilled {} ← session {} (confidence {:.2})",
        short_sha(&best.candidate.sha),
        short_session(&session.session_id),
        best.confidence,
    );
}

// ============================================================
// Transcript discovery
// ============================================================

struct TranscriptSet {
    /// The directory or file the transcripts came from. Used in user-facing
    /// messages and as the source of truth for `derived_from.transcript_path`.
    source: PathBuf,
    files: Vec<PathBuf>,
}

fn discover_transcripts(git: &Git, override_path: Option<&str>) -> anyhow::Result<TranscriptSet> {
    if let Some(p) = override_path {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Ok(TranscriptSet {
                source: path.parent().unwrap_or(Path::new(".")).to_path_buf(),
                files: vec![path],
            });
        }
        if path.is_dir() {
            return Ok(TranscriptSet {
                files: jsonl_files_in(&path),
                source: path,
            });
        }
        return Err(anyhow!("--transcript-path {p}: not a file or directory"));
    }

    let home = std::env::var("HOME").context("HOME is not set")?;
    let project_dir = home_relative_project_dir(&home, git.work_tree());
    if !project_dir.exists() {
        return Err(anyhow!(
            "no Claude Code project directory found at {}; pass --transcript-path to override",
            project_dir.display()
        ));
    }
    Ok(TranscriptSet {
        files: jsonl_files_in(&project_dir),
        source: project_dir,
    })
}

/// `~/.claude/projects/<sanitized-cwd>/` — slashes in the cwd become dashes,
/// and the leading slash becomes a leading dash. Verified empirically against
/// `~/.claude/projects/-Users-matt-Documents-GitHub-prov/`.
fn home_relative_project_dir(home: &str, work_tree: &Path) -> PathBuf {
    let canonical = work_tree
        .canonicalize()
        .unwrap_or_else(|_| work_tree.to_path_buf());
    let s = canonical.to_string_lossy();
    let sanitized: String = s
        .chars()
        .map(|c| match c {
            '/' | '.' => '-',
            other => other,
        })
        .collect();
    PathBuf::from(home).join(".claude/projects").join(sanitized)
}

fn jsonl_files_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("jsonl"))
        })
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

fn confirm_or_bail(set: &TranscriptSet, yes: bool) -> anyhow::Result<()> {
    use std::io::{IsTerminal, Write};

    println!(
        "prov backfill: scanning {} transcript file(s) under {}",
        set.files.len(),
        set.source.display()
    );
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        return Err(anyhow!(
            "stdin is not a TTY; pass --yes to confirm transcript-file access non-interactively"
        ));
    }
    eprint!("Read these files and write backfill notes? [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    let answer = answer.trim().to_lowercase();
    if answer == "y" || answer == "yes" {
        Ok(())
    } else {
        Err(anyhow!("aborted"))
    }
}

// ============================================================
// Candidate commit metadata
// ============================================================

#[derive(Debug, Clone)]
struct CommitMeta {
    sha: String,
    author_email: String,
    /// Unix epoch seconds.
    committed_at: i64,
    /// Map of repo-relative file path → added lines (in commit order).
    added_by_file: HashMap<String, Vec<AddedLine>>,
}

#[derive(Debug, Clone)]
struct AddedLine {
    line_no: u32,
    /// BLAKE3 hash of the line content. Compared against the parsed edit's
    /// per-line hashes to detect which session edits surface in this commit.
    hash: String,
}

/// Walk the commit history reachable from HEAD up to a defensive cap and
/// build [`CommitMeta`] for each. We pull added-line hashes per commit
/// up-front because matching is O(sessions × commits) and re-running `git
/// diff` per pair would be needlessly slow.
fn load_candidate_commits(git: &Git) -> anyhow::Result<Vec<CommitMeta>> {
    /// Defensive ceiling: backfill is best-effort and Claude Code transcripts
    /// rarely cover more than a few months of history. Capping at 5_000
    /// commits prevents pathological repos from making the user wait minutes.
    const MAX_COMMITS: usize = 5_000;

    let raw = git
        .capture([
            "log",
            "--no-merges",
            &format!("--max-count={MAX_COMMITS}"),
            "--format=%H%x09%ae%x09%ct",
        ])
        .context("git log failed")?;

    let mut out = Vec::new();
    for line in raw.lines() {
        let mut parts = line.splitn(3, '\t');
        let sha = parts.next().unwrap_or("").to_string();
        let email = parts.next().unwrap_or("").to_string();
        let ts: i64 = parts.next().unwrap_or("0").parse().unwrap_or(0);
        if sha.is_empty() {
            continue;
        }
        let added_by_file = collect_added_lines(git, &sha).unwrap_or_default();
        out.push(CommitMeta {
            sha,
            author_email: email,
            committed_at: ts,
            added_by_file,
        });
    }
    Ok(out)
}

fn collect_added_lines(git: &Git, sha: &str) -> Result<HashMap<String, Vec<AddedLine>>, GitError> {
    let parent_count = git
        .capture(["rev-list", "--count", &format!("{sha}^@")])
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);
    let raw = if parent_count == 0 {
        let empty_tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        git.capture(["diff", "-U0", empty_tree, sha])?
    } else {
        git.capture(["diff", "-U0", &format!("{sha}~1..{sha}")])?
    };
    Ok(parse_unified_diff_added(&raw))
}

fn parse_unified_diff_added(raw: &str) -> HashMap<String, Vec<AddedLine>> {
    let mut out: HashMap<String, Vec<AddedLine>> = HashMap::new();
    let mut current_file: Option<String> = None;
    let mut next_line_no: u32 = 0;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            current_file = Some(rest.to_string());
            continue;
        }
        if line.starts_with("+++ ") || line.starts_with("--- ") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@ ") {
            let plus = rest
                .split_whitespace()
                .find(|t| t.starts_with('+'))
                .unwrap_or("+0");
            let trimmed = plus.trim_start_matches('+');
            let start = trimmed
                .split(',')
                .next()
                .and_then(|n| n.parse::<u32>().ok())
                .unwrap_or(0);
            next_line_no = start;
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            if let Some(file) = &current_file {
                let hash = blake3::hash(rest.as_bytes()).to_hex().to_string();
                out.entry(file.clone()).or_default().push(AddedLine {
                    line_no: next_line_no,
                    hash,
                });
                next_line_no = next_line_no.saturating_add(1);
            }
        } else if line.starts_with('-') {
            // removed line — does not advance the new-side counter.
        } else if !line.starts_with("@@") && current_file.is_some() {
            next_line_no = next_line_no.saturating_add(1);
        }
    }
    out
}

// ============================================================
// Session ↔ commit matching
// ============================================================

#[derive(Debug)]
struct SessionMatch<'a> {
    candidate: &'a CommitMeta,
    confidence: f32,
    /// One entry per session edit that matched. Non-matching edits are
    /// dropped at note-build time.
    per_edit: BTreeMap<usize, EditMatch>,
}

#[derive(Debug, Clone)]
struct EditMatch {
    /// Repo-relative file path (matches the diff's `+++ b/` path).
    file: String,
    line_range: [u32; 2],
    /// BLAKE3 hashes of the matched window's added lines. Stored on the note
    /// so the resolver's drift detection treats backfilled notes the same as
    /// live-captured ones.
    content_hashes: Vec<String>,
}

fn best_match<'a>(
    session: &ParsedSession,
    candidates: &'a [CommitMeta],
) -> Option<SessionMatch<'a>> {
    let session_window = session_unix_window(session)?;
    let total_edits = session.edits.len();
    if total_edits == 0 {
        return None;
    }
    // Pre-hash session edit lines once. The same session is scored against
    // every candidate, so paying for hashing per-line per-candidate would be
    // wasteful.
    let edit_hashes: Vec<Vec<String>> = session
        .edits
        .iter()
        .map(|e| {
            e.new_string
                .split('\n')
                .map(|l| blake3::hash(l.as_bytes()).to_hex().to_string())
                .collect()
        })
        .collect();

    let mut best: Option<SessionMatch<'_>> = None;
    for c in candidates {
        if !time_overlap(session_window, c.committed_at) {
            continue;
        }
        let per_edit = score_edits_against_commit(session, &edit_hashes, c);
        if per_edit.is_empty() {
            continue;
        }
        // Confidence: fraction of session edits that found a home in this
        // commit. Simple and surprisingly effective on real sessions —
        // either every edit lands (1.0) or only a handful do.
        #[allow(clippy::cast_precision_loss)]
        let confidence = per_edit.len() as f32 / total_edits as f32;
        let candidate_match = SessionMatch {
            candidate: c,
            confidence,
            per_edit,
        };
        match &best {
            None => best = Some(candidate_match),
            Some(prev) if candidate_match.confidence > prev.confidence => {
                best = Some(candidate_match);
            }
            _ => {}
        }
    }
    best
}

fn session_unix_window(session: &ParsedSession) -> Option<(i64, i64)> {
    let start = parse_iso_unix(session.started_at.as_deref()?)?;
    let end = session
        .ended_at
        .as_deref()
        .and_then(parse_iso_unix)
        .unwrap_or(start);
    Some((start, end))
}

fn time_overlap((start, end): (i64, i64), commit_ts: i64) -> bool {
    let pad = TIME_WINDOW_HOURS * 3600;
    commit_ts >= start - pad && commit_ts <= end + pad
}

fn score_edits_against_commit(
    session: &ParsedSession,
    edit_hashes: &[Vec<String>],
    commit: &CommitMeta,
) -> BTreeMap<usize, EditMatch> {
    let mut matched = BTreeMap::new();
    for (idx, edit) in session.edits.iter().enumerate() {
        let Some((file_key, added)) = locate_file_in_diff(&edit.file, &commit.added_by_file) else {
            continue;
        };
        let needle = &edit_hashes[idx];
        let Some((line_range, hashes)) = best_window(needle, added) else {
            continue;
        };
        matched.insert(
            idx,
            EditMatch {
                file: file_key,
                line_range,
                content_hashes: hashes,
            },
        );
    }
    matched
}

/// Locate the captured `file` path inside the commit's per-file added-lines
/// map. The captured path is typically absolute (Claude Code surfaces full
/// paths in `tool_input.file_path`); the diff is keyed by repo-relative
/// paths. Try the relative form first, then the trailing-segment fallback
/// (matches captures from a different machine where the absolute path
/// prefix differs).
fn locate_file_in_diff<'a>(
    captured: &str,
    added_by_file: &'a HashMap<String, Vec<AddedLine>>,
) -> Option<(String, &'a [AddedLine])> {
    if let Some(v) = added_by_file.get(captured) {
        return Some((captured.to_string(), v.as_slice()));
    }
    // Fallback: longest suffix of `captured` that matches a diff key. Handles
    // cross-machine absolute paths and arbitrary path prefixes.
    for key in added_by_file.keys() {
        if captured.ends_with(key) {
            return Some((key.clone(), added_by_file[key].as_slice()));
        }
    }
    None
}

/// Find the longest contiguous run of lines from `needle` that appears as a
/// contiguous sub-sequence of `added`'s hashes, requiring at least 50% of
/// `needle`'s lines to land. Returns the line range (in commit-side
/// coordinates) and the matched hashes.
///
/// Rationale: the captured `new_string` may include lines that the commit's
/// formatter or post-edit cleanup mutated (trailing whitespace, wrapping),
/// but a contiguous sub-run of unaltered lines is a strong-enough signal
/// for backfill. Finer-grained matching is tracked as a v1.x follow-up.
fn best_window(needle: &[String], added: &[AddedLine]) -> Option<([u32; 2], Vec<String>)> {
    if needle.is_empty() || added.is_empty() {
        return None;
    }
    let added_hashes: Vec<&str> = added.iter().map(|a| a.hash.as_str()).collect();
    let mut best: Option<(usize, usize, usize)> = None; // (needle_start, added_start, length)
    for ns in 0..needle.len() {
        for a_start in 0..added.len() {
            let mut len = 0;
            while ns + len < needle.len()
                && a_start + len < added.len()
                && needle[ns + len] == added_hashes[a_start + len]
            {
                len += 1;
            }
            if len > best.map_or(0, |(_, _, l)| l) {
                best = Some((ns, a_start, len));
            }
        }
    }
    let (_, a_start, len) = best?;
    if len * 2 < needle.len() {
        return None;
    }
    let first = added[a_start].line_no;
    let last = added[a_start + len - 1].line_no;
    let hashes: Vec<String> = added[a_start..a_start + len]
        .iter()
        .map(|l| l.hash.clone())
        .collect();
    Some(([first, last], hashes))
}

fn parse_iso_unix(s: &str) -> Option<i64> {
    // Tiny ISO-8601 parser tolerant of fractional seconds and a `Z` or
    // `+HH:MM` suffix. Live capture only emits second-precision `Z`, but
    // Claude Code transcript timestamps carry millisecond precision and a
    // mix of suffixes. We treat all timestamps as UTC for window matching —
    // a few hours of TZ skew is well within the default 4-hour grace.
    let s = s.trim();
    let (date, time) = s.split_once('T')?;
    let mut dp = date.split('-');
    let year: i64 = dp.next()?.parse().ok()?;
    let month: i64 = dp.next()?.parse().ok()?;
    let day: i64 = dp.next()?.parse().ok()?;
    // Strip the trailing `Z` or `±HH:MM` offset, leaving just `HH:MM:SS[.fff]`.
    let time = time.trim_end_matches('Z');
    let time = match time.rfind(['+', '-']) {
        Some(idx) if idx >= 5 => &time[..idx], // 5 = "HH:MM" (don't eat date dashes — already split off)
        _ => time,
    };
    let mut tp = time.split(':');
    let hour: i64 = tp.next()?.parse().ok()?;
    let minute: i64 = tp.next()?.parse().ok()?;
    let second_part = tp.next().unwrap_or("0");
    let second: i64 = second_part.split('.').next().unwrap_or("0").parse().ok()?;
    Some(civil_to_epoch(year, month, day, hour, minute, second))
}

#[allow(clippy::similar_names)]
fn civil_to_epoch(year: i64, month: i64, day: i64, hour: i64, minute: i64, second: i64) -> i64 {
    // Howard Hinnant's days_from_civil. Mirrors the reverse direction in
    // prov_core::time but unfortunately not exposed there — keep this local.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + hour * 3600 + minute * 60 + second
}

// ============================================================
// Note construction
// ============================================================

fn build_note_edits(
    session: &ParsedSession,
    matched: &SessionMatch<'_>,
    transcript_path: &Path,
    redactor: &Redactor,
) -> Vec<Edit> {
    // Cache redacted prompts keyed by turn_index so the same prompt isn't
    // redacted N times for an N-edit turn.
    let mut redacted_prompts: HashMap<u32, String> = HashMap::new();
    let mut prompt_for = |turn_index: u32| -> String {
        if let Some(s) = redacted_prompts.get(&turn_index) {
            return s.clone();
        }
        let raw = session
            .turns
            .iter()
            .find(|t| t.turn_index == turn_index)
            .map(|t| t.prompt.clone())
            .unwrap_or_default();
        let red = redactor.redact(&raw).text;
        redacted_prompts.insert(turn_index, red.clone());
        red
    };

    let transcript_str = transcript_path.to_string_lossy().to_string();
    let confidence = matched.confidence;

    let mut out = Vec::new();
    for (idx, m) in &matched.per_edit {
        let parsed: &ParsedEdit = &session.edits[*idx];
        let prompt = prompt_for(parsed.turn_index);
        let model = parsed
            .model
            .clone()
            .or_else(|| session.model.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let timestamp = parsed
            .timestamp
            .clone()
            .or_else(|| session.started_at.clone())
            .unwrap_or_default();
        out.push(Edit {
            file: m.file.clone(),
            line_range: m.line_range,
            content_hashes: m.content_hashes.clone(),
            original_blob_sha: None,
            prompt,
            conversation_id: session.session_id.clone(),
            turn_index: parsed.turn_index,
            tool_use_id: parsed.tool_use_id.clone(),
            preceding_turns_summary: None,
            model,
            tool: "claude-code".into(),
            timestamp,
            derived_from: Some(DerivedFrom::Backfill {
                confidence,
                transcript_path: transcript_str.clone(),
            }),
        });
    }
    out
}

/// True when every edit in the existing note carries a `Backfill` derivation.
/// A live-captured note has at least one edit with `derived_from == None` (or
/// `Rewrite`), and we refuse to overwrite those — backfill is opt-in
/// approximate data, never authoritative.
fn is_backfill_only(note: &Note) -> bool {
    if note.edits.is_empty() {
        return false;
    }
    note.edits
        .iter()
        .all(|e| matches!(e.derived_from, Some(DerivedFrom::Backfill { .. })))
}

fn update_cache(cache_path: &Path, store: &NotesStore, sha: &str, note: &Note) {
    if !cache_path.exists() {
        return;
    }
    let Ok(mut cache) = Cache::open(cache_path) else {
        return;
    };
    let new_ref_sha = store.ref_sha().ok().flatten();
    let _ = cache.upsert_note(sha, note, new_ref_sha.as_deref());
}

fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

fn short_session(s: &str) -> &str {
    &s[..s.len().min(8)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_relative_project_dir_replaces_slashes_and_dots() {
        let p = home_relative_project_dir("/Users/me", Path::new("/tmp/repo.git"));
        assert!(p.ends_with("-tmp-repo-git"));
    }

    #[test]
    fn parse_iso_unix_handles_z_and_fractional_seconds() {
        let a = parse_iso_unix("2026-04-28T12:34:56Z").unwrap();
        let b = parse_iso_unix("2026-04-28T12:34:56.789Z").unwrap();
        assert_eq!(a, b);
        assert_eq!(a, 1_777_379_696);
    }

    #[test]
    fn parse_iso_unix_handles_timezone_offset() {
        let a = parse_iso_unix("2026-04-28T12:34:56+00:00").unwrap();
        assert_eq!(a, 1_777_379_696);
    }

    #[test]
    fn time_overlap_respects_grace_window() {
        let window = (1_777_379_000, 1_777_379_200);
        assert!(time_overlap(window, 1_777_379_100));
        // Inside grace window (4h)
        assert!(time_overlap(window, 1_777_379_200 + 3 * 3600));
        // Outside grace window
        assert!(!time_overlap(window, 1_777_379_200 + 5 * 3600));
    }

    #[test]
    fn best_window_finds_full_match() {
        let needle = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let added = vec![
            AddedLine {
                line_no: 10,
                hash: "x".into(),
            },
            AddedLine {
                line_no: 11,
                hash: "a".into(),
            },
            AddedLine {
                line_no: 12,
                hash: "b".into(),
            },
            AddedLine {
                line_no: 13,
                hash: "c".into(),
            },
            AddedLine {
                line_no: 14,
                hash: "y".into(),
            },
        ];
        let (range, hashes) = best_window(&needle, &added).unwrap();
        assert_eq!(range, [11, 13]);
        assert_eq!(hashes, vec!["a", "b", "c"]);
    }

    #[test]
    fn best_window_rejects_below_50_percent() {
        let needle = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        let added = vec![AddedLine {
            line_no: 1,
            hash: "a".into(),
        }];
        assert!(best_window(&needle, &added).is_none());
    }

    #[test]
    fn locate_file_falls_back_to_suffix_match() {
        let mut map = HashMap::new();
        map.insert("src/main.rs".to_string(), vec![]);
        let (key, _) = locate_file_in_diff("/Users/x/repo/src/main.rs", &map).unwrap();
        assert_eq!(key, "src/main.rs");
    }

    #[test]
    fn is_backfill_only_distinguishes_live_notes() {
        use prov_core::schema::Edit;
        let live_edit = Edit {
            file: "x".into(),
            line_range: [1, 1],
            content_hashes: vec![],
            original_blob_sha: None,
            prompt: "p".into(),
            conversation_id: "s".into(),
            turn_index: 0,
            tool_use_id: None,
            preceding_turns_summary: None,
            model: "m".into(),
            tool: "claude-code".into(),
            timestamp: "2026-04-28T00:00:00Z".into(),
            derived_from: None,
        };
        let mut backfill_edit = live_edit.clone();
        backfill_edit.derived_from = Some(DerivedFrom::Backfill {
            confidence: 0.9,
            transcript_path: "/tmp/a.jsonl".into(),
        });
        assert!(!is_backfill_only(&Note::new(vec![live_edit.clone()])));
        assert!(is_backfill_only(&Note::new(vec![backfill_edit.clone()])));
        // Mixed: any non-backfill edit means we treat the note as live.
        assert!(!is_backfill_only(&Note::new(vec![
            live_edit,
            backfill_edit
        ])));
    }
}
