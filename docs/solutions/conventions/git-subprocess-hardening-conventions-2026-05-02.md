---
title: Defensive patterns for shelling out to git from an agent-driven CLI
date: 2026-05-02
category: conventions
module: prov-cli
problem_type: convention
component: tooling
severity: low
applies_when:
  - shelling out to git as a child process from a Rust CLI
  - forwarding user-supplied positional arguments (refs, paths, remotes) to git
  - running git non-interactively in agent or CI contexts where a TTY prompt would hang
  - implementing git hook handlers that read stdin or parse ls-tree output
  - writing user-facing recovery hints that reference prov subcommand names
tags:
  - git
  - subprocess
  - cli-hardening
  - agent-safety
  - argument-injection
  - end-of-options
  - hooks
  - rust
related_components:
  - development_workflow
---

# Defensive patterns for shelling out to git from an agent-driven CLI

## Context

We ship `prov` as a Rust CLI that's primarily driven by AI agents and CI runners on top of git. Those callers can't answer TTY prompts, can't reason about git's flag-parsing quirks, and rely on our messages being literally correct — if a hint mentions a flag, the flag has to exist; if a "removed" message is printed, the data has to actually be gone.

A catch-up review of PRs #18-22 (a 12-reviewer parallel pass over ~2,343 lines and 24 files) surfaced a cluster of small but recurring weaknesses around how we shell out to git in `crates/prov-cli/src/commands/`. Seven findings were applied immediately on `fix/u7-u8-review-followups` (commit `10e062f`); eight larger items were deferred to GitHub issues #24-#31 (`.provignore` wiring, pre-push bypass hardening, cache schema, atomic redact, etc.) (session history).

The seven applied findings are:

- User-supplied positionals (remote names, commit-ish args) were passed to `git fetch` and `git rev-parse` with no separator, so a value beginning with `-` could be reinterpreted as a flag.
- `prov fetch` and `prov push` did not set `GIT_TERMINAL_PROMPT`, so a missing credential helper would hang an agent forever on a TTY prompt nothing was driving.
- The pre-push hook read stdin into an unbounded `String`, even though the sibling `read_stdin_json` helper already had a 4 MiB cap for the same threat.
- `list_note_blobs` accepted any `ls-tree` path, normalized away the fanout slashes, and printed it as a `commit <sha>` label in the gate's block message — a malformed entry would surface a meaningless identifier to the user.
- A `match (Option, Option)` arm bound `Some(_)` and then `unwrap()`ed the outer `Option` again to recover the value.
- The conflict-recovery hint named `prov notes resolve`; the actual subcommand is `prov notes-resolve`. Tracing back through prior sessions, this was a copy-paste slip during U7 authoring rather than a conscious naming choice (session history).
- The `redact-history` epilogue advertised `prov fetch --reset-from-remote`, a flag that was never implemented or even planned — a hallucinated identifier that shipped because no test exercised the hint string (session history).

The conventions below are what catches all of these uniformly going forward.

## Guidance

### 1. Separate user input from flags with `--` / `--end-of-options`

Any user-supplied string that becomes a positional argument to a child `git` command must be preceded by an option terminator. Use `--` for porcelain commands that accept it; use `--end-of-options` for `git rev-parse` (and other commands where a literal `--` is itself meaningful).

```rust
// fetch.rs — before
git.run(["fetch", &remote, FETCH_REFSPEC])

// fetch.rs — after
git.run(["fetch", "--", &remote, FETCH_REFSPEC])
```

```rust
// mark_private.rs — before
git.capture(["rev-parse", "--verify", &args.commit])

// mark_private.rs — after
// `--end-of-options` keeps a user-supplied value beginning with `-` from
// being parsed as a git flag (e.g., `prov mark-private --version`).
git.capture(["rev-parse", "--verify", "--end-of-options", &args.commit])
```

### 2. Default `GIT_TERMINAL_PROMPT=0` for any git network op, but respect an override

Network commands run by agents must fail fast when credentials are missing. We set the env var only if the caller hasn't already chosen, so a human running `GIT_TERMINAL_PROMPT=1 prov fetch` for a one-off interactive auth still gets the prompt. The helper lives in `fetch.rs` and is reused by `push.rs`:

```rust
// fetch.rs
/// Set `GIT_TERMINAL_PROMPT=0` for child git invocations unless the user has
/// explicitly opted into prompts. Shared between `prov fetch` and `prov push`
/// so the network commands fail fast instead of hanging on a missing
/// credential helper.
pub(crate) fn disable_git_terminal_prompt() {
    if std::env::var_os("GIT_TERMINAL_PROMPT").is_none() {
        std::env::set_var("GIT_TERMINAL_PROMPT", "0");
    }
}
```

```rust
// fetch.rs::run
disable_git_terminal_prompt();

// push.rs::run
super::fetch::disable_git_terminal_prompt();
```

Any future command that talks to a remote (`clone`, `ls-remote`, `push`, `fetch`) gets the same call.

### 3. Cap stdin reads in any handler that consumes piped input

`read_to_string` on `io::stdin()` is unbounded. Wrap it in `take(MAX + 1)` and check `bytes_read > MAX` after the read so the over-cap case becomes an explicit error instead of OOM. The pre-push hook already had this defense in `read_stdin_json`; the parallel `handle_pre_push` path was missing it.

```rust
// hook.rs::handle_pre_push
const MAX_PAYLOAD_BYTES: u64 = 4 * 1024 * 1024; // 4 MiB

let mut buf = String::new();
let bytes_read = io::stdin()
    .take(MAX_PAYLOAD_BYTES + 1)
    .read_to_string(&mut buf)
    .map_err(|e| HandlerError::Stdin(e.to_string()))?;
if u64::try_from(bytes_read).unwrap_or(u64::MAX) > MAX_PAYLOAD_BYTES {
    return Err(HandlerError::Stdin(format!(
        "pre-push payload exceeded {MAX_PAYLOAD_BYTES} bytes"
    )));
}
```

The cap is sized to the threat: git's pre-push contract emits one ref-update line per pushed ref; even a mass push is at most a few KB.

### 4. Validate untrusted SHA-shaped strings before printing them as commit identifiers

`git ls-tree` over a notes ref returns paths in fanout form (`ab/cd…`). We collapse the slashes to recover the annotated commit SHA, but a malformed tree entry could collapse to anything. Before using that string as a `commit <sha>` label in a user-facing block message, verify it really is a 40-character lowercase hex SHA.

```rust
// hook.rs
fn is_full_hex_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

// hook.rs::list_note_blobs
let normalized = path.replace('/', "");
if !is_full_hex_sha(&normalized) {
    continue;
}
out.push((normalized, sha.to_string()));
```

### 5. Bind values in the match arm instead of `unwrap()`-ing the matched Option/Result

If you've already pattern-matched on `Some`, bind the inner value. Re-`unwrap()`-ing the original `Option` on the right-hand side is noise at best and a future panic site at worst.

```rust
// fetch.rs — before
match (local_sha.as_deref(), tracking_sha.as_deref()) {
    (None, Some(_)) => {
        git.run([
            "update-ref",
            NOTES_REF_PUBLIC,
            tracking_sha.as_ref().unwrap(),
        ])
        .with_context(|| format!("update-ref {NOTES_REF_PUBLIC}"))?;
    }
    // ...
}

// fetch.rs — after
match (local_sha.as_deref(), tracking_sha.as_deref()) {
    (None, Some(tracking)) => {
        git.run(["update-ref", NOTES_REF_PUBLIC, tracking])
            .with_context(|| format!("update-ref {NOTES_REF_PUBLIC}"))?;
    }
    // ...
}
```

### 6. Keep remediation hints tied to the commands and flags that actually exist

Hints printed from error paths and doc comments are part of the CLI's contract. When a subcommand is renamed (e.g., `notes resolve` → `notes-resolve`) or a flag is removed/never landed (e.g., `prov fetch --reset-from-remote`), every string that names it has to be updated. Both of the slips fixed here trace back to authoring sessions where the typo or hallucinated flag was never grep-checked or test-covered before merge (session history). Grep for the old name as part of any rename, or write a test that exercises the hint string, so dead pointers don't ship to agents that follow them literally.

```rust
// fetch.rs — module doc
//! divergent notes surface as a merge in progress for `prov notes-resolve`
//! (U10) rather than silently picking a side.

// fetch.rs — conflict-recovery hint
.with_context(|| {
    "notes merge produced a conflict; run `prov notes-resolve` to finish"
        .to_string()
})?;
```

### 7. When you advertise that data has been "removed", spell out the residual surfaces

A "redact" or "purge" command that only rewrites refs leaves three residual copies behind: the local reflog, unreferenced blobs the GC hasn't pruned yet, and copies already pushed to remotes/forks/teammate clones. The user-facing message has to call those out explicitly and provide the remediation commands — otherwise a caller who reads "redacted N matches" believes the secret is gone.

```rust
// redact_history.rs::run epilogue
eprintln!("Heads up:");
eprintln!("  - Local notes refs are scrubbed, but already-pushed copies, forks, and");
eprintln!("    teammate clones still hold the pre-rewrite content. Rotate the");
eprintln!("    underlying secret independently.");
eprintln!("  - Local reflog and unreferenced blob objects still hold the pre-rewrite");
eprintln!("    content until pruned. Run:");
eprintln!("        git reflog expire --expire=now --all");
eprintln!("        git gc --prune=now");
eprintln!("  - Teammates can re-sync after you re-push the rewritten ref:");
eprintln!("        git fetch <remote> +refs/notes/prompts:refs/notes/prompts");
eprintln!("        prov reindex");
```

## Why This Matters

Each pattern maps to a concrete failure mode we'd otherwise inherit:

- **No `--` / `--end-of-options`.** A user (or agent) with a remote, branch, or commit-ish that begins with `-` causes `git` to interpret it as a flag. At minimum that's a confusing error; at worst it's flag injection — `prov mark-private --version` would have asked `rev-parse` to print git's version instead of resolving a commit.
- **No `GIT_TERMINAL_PROMPT=0`.** An unconfigured credential helper makes git open `/dev/tty` and block on a username/password prompt. An agent or CI runner has nothing to type into that prompt and the process hangs forever instead of returning a useful error.
- **Unbounded stdin.** A malicious or buggy producer pipes 50 GB into `prov hook pre-push` and the handler tries to materialize it all in a `String`, exhausting memory before any check runs.
- **Unvalidated `ls-tree` paths.** A malformed tree entry collapses to a non-SHA string, and the gate's block message prints `in note for commit <bogus>`, sending a user or agent off to chase a commit that doesn't exist.
- **`unwrap()` after a successful match.** The compiler can't tell you the two sides drifted apart; a future refactor that changes the matched `Option` introduces a panic that the original arm-binding form would have caught at compile time.
- **Hints pointing at non-existent flags or renamed commands.** Agents follow remediation hints literally. `prov fetch --reset-from-remote` returned a clap error and the agent had no path forward; `prov notes resolve` produced a "no such command" instead of letting the operator finish a notes merge.
- **Silent residuals after a "redact".** A user reads "redacted N matches", believes the secret is gone, and skips rotation. Meanwhile the reflog, unreferenced blobs, and already-pushed copies all still contain the original secret.

## When to Apply

- Any new code path that shells out to `git` with a user-supplied string as a positional → use `--` or `--end-of-options`.
- Any command that performs a network operation against a remote (`fetch`, `push`, `clone`, `ls-remote`) → call `disable_git_terminal_prompt()`.
- Any handler that reads from `stdin` (hooks, filters, pipes) → use `io::stdin().take(MAX + 1).read_to_string(...)` with an over-cap check.
- Any external string used as an identifier in user-facing output (SHAs, refs, paths) → validate before printing.
- Any `match` arm that destructures `Some` / `Ok` and then refers back to the matched variable → bind the inner value instead.
- Any error message, doc comment, or epilogue that names a `prov` subcommand or flag → grep-check it whenever subcommands or flags are renamed/removed; ideally cover with a test.
- Any "destructive" or "removal" command that only rewrites refs → enumerate the residual surfaces (reflog, unreferenced blobs, pushed copies) and provide the user-facing remediation commands.

## Examples

### `--` separator on `git fetch`

```rust
// Before
git.run(["fetch", &remote, FETCH_REFSPEC])
    .with_context(|| format!("git fetch {remote} {FETCH_REFSPEC}"))?;

// After
// `--` separates options from positionals so a remote name beginning
// with `-` is interpreted as a repository, not a flag.
git.run(["fetch", "--", &remote, FETCH_REFSPEC])
    .with_context(|| format!("git fetch {remote} {FETCH_REFSPEC}"))?;
```

### `--end-of-options` on `git rev-parse`

```rust
// Before
let resolved = git
    .capture(["rev-parse", "--verify", &args.commit])
    .map_err(|e| anyhow!("could not resolve commit `{}`: {e}", args.commit))?
    .trim()
    .to_string();

// After
let resolved = git
    .capture(["rev-parse", "--verify", "--end-of-options", &args.commit])
    .map_err(|e| anyhow!("could not resolve commit `{}`: {e}", args.commit))?
    .trim()
    .to_string();
```

### `disable_git_terminal_prompt` helper and call sites

```rust
// crates/prov-cli/src/commands/fetch.rs
pub(crate) fn disable_git_terminal_prompt() {
    if std::env::var_os("GIT_TERMINAL_PROMPT").is_none() {
        std::env::set_var("GIT_TERMINAL_PROMPT", "0");
    }
}

// fetch.rs::run
disable_git_terminal_prompt();

// push.rs::run
super::fetch::disable_git_terminal_prompt();
```

### 4 MiB stdin cap in the pre-push hook

```rust
// crates/prov-cli/src/commands/hook.rs::handle_pre_push
const MAX_PAYLOAD_BYTES: u64 = 4 * 1024 * 1024; // 4 MiB

let mut buf = String::new();
let bytes_read = io::stdin()
    .take(MAX_PAYLOAD_BYTES + 1)
    .read_to_string(&mut buf)
    .map_err(|e| HandlerError::Stdin(e.to_string()))?;
if u64::try_from(bytes_read).unwrap_or(u64::MAX) > MAX_PAYLOAD_BYTES {
    return Err(HandlerError::Stdin(format!(
        "pre-push payload exceeded {MAX_PAYLOAD_BYTES} bytes"
    )));
}
```

### `is_full_hex_sha` validation in `list_note_blobs`

```rust
// crates/prov-cli/src/commands/hook.rs
fn is_full_hex_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

// list_note_blobs (after collapsing fanout slashes)
let normalized = path.replace('/', "");
if !is_full_hex_sha(&normalized) {
    continue;
}
out.push((normalized, sha.to_string()));
```

### Bind in the match arm

```rust
// Before
match (local_sha.as_deref(), tracking_sha.as_deref()) {
    (None, Some(_)) => {
        git.run([
            "update-ref",
            NOTES_REF_PUBLIC,
            tracking_sha.as_ref().unwrap(),
        ])
        .with_context(|| format!("update-ref {NOTES_REF_PUBLIC}"))?;
    }
    // ...
}

// After
match (local_sha.as_deref(), tracking_sha.as_deref()) {
    (None, Some(tracking)) => {
        git.run(["update-ref", NOTES_REF_PUBLIC, tracking])
            .with_context(|| format!("update-ref {NOTES_REF_PUBLIC}"))?;
    }
    // ...
}
```

### Hints that match real subcommands

```rust
// Before
"notes merge produced a conflict; run `prov notes resolve` to finish"

// After
"notes merge produced a conflict; run `prov notes-resolve` to finish"
```

### `redact-history` epilogue with full residual remediation

```rust
// Before
eprintln!("  - Teammates can re-sync with: `prov fetch --reset-from-remote && prov reindex`");
eprintln!("    (once you have re-pushed the rewritten ref).");

// After
eprintln!("  - Local reflog and unreferenced blob objects still hold the pre-rewrite");
eprintln!("    content until pruned. Run:");
eprintln!("        git reflog expire --expire=now --all");
eprintln!("        git gc --prune=now");
eprintln!("  - Teammates can re-sync after you re-push the rewritten ref:");
eprintln!("        git fetch <remote> +refs/notes/prompts:refs/notes/prompts");
eprintln!("        prov reindex");
```

## Related

- Source commit: `10e062f` on branch `fix/u7-u8-review-followups` — `fix(privacy,sync): U7/U8 review small fixes`
- Originating PRs reviewed: #18 (cache-refresh on post-commit), #19 (U3 follow-up doc), #20 (`scripts/check.sh` + `CLAUDE.md`), #21 (U7 privacy/`prov:private`/redact-history), #22 (U8 fetch/push + pre-push gate)
- Open follow-up issues from the same review: #29 (`redact-history` reflog/unpruned-blob residuals — directly motivates pattern 7), #28 (chained pre-push hook stdin handling — relates to pattern 3), #27 (pre-push fails open when `prov` off PATH), #24-#26, #30, #31
- `docs/follow-ups.md` "U8 — Sync" section for the broader follow-up backlog
