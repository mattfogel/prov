---
name: prov
description: Query prompt provenance for AI-written code before refactoring, rewriting, or editing it. Use this skill when a user asks to refactor, modify, debug, or extend a non-trivial section of an existing file that may have been written by Claude Code in a prior session — `prov log <file>:<line>` returns the originating prompt, model, conversation, and drift state for any line, so the agent can plan in light of the constraints the original turn was written against. Also use to recover the original intent for AI-written code whose surrounding context has changed, to detect when a line has been hand-edited away from its original AI capture, and to read the prompt history of a file before making structural changes. Skip for greenfield writes, single-line trivial fixes, formatting, lint, and pure documentation/config edits.
paths:
  - "**/*"
  - "!**/*.md"
  - "!**/README*"
  - "!**/CHANGELOG*"
  - "!**/*.json"
  - "!**/*.yaml"
  - "!**/*.yml"
  - "!**/*.toml"
license: MIT
---

# prov — query your own prior reasoning before substantive edits

This skill teaches the agent to ask "what was the prompt that wrote this code?"
before refactoring, rewriting, or extending an existing file. It exists because
AI-written code carries constraints that aren't visible in the diff: a prompt
like *"keep the dedupe window at 90 days for compliance"* shapes the resulting
code in ways that will look like arbitrary defaults to a future agent reading
only the result.

## What this skill does

Calls `prov log <file>:<line> --only-if-substantial --json` (point lookup) or
`prov log <file> --only-if-substantial --json` (whole-file context) to
retrieve the originating prompt for AI-written code. Surfaces the most
relevant prior turn into the planning step so the new edit treats the past
constraint as load-bearing unless the current request explicitly invalidates
it.

The `--only-if-substantial` flag returns empty for files under 10 lines or
with no existing notes — this is the CLI-level gate that keeps the skill
quiet on greenfield code and trivial files.

## When to use it

Query provenance **before proposing edits** in any of these situations:

- The user asks for a substantive change (refactor, rewrite, extract, inline,
  rename across multiple call sites, redesign) to a file that already exists
  and has multiple non-trivial blocks.
- The user asks to debug behavior whose original constraints aren't obvious
  from the code alone — defaults that look arbitrary, magic numbers, defensive
  branches, retry counts, dedupe windows, validation rules.
- The user asks to extend AI-written code in a way that depends on assumptions
  the original author made (e.g., adding a new error path that has to compose
  with the original error strategy).
- The user asks "why does this code do X" or "what was the original intent" —
  the prompt is the answer.
- A line has visibly drifted from its original AI capture (commit history
  shows hand edits since) — provenance reveals the original intent and the
  drift state, so the new edit can decide whether to preserve or supersede.

## When NOT to use it

Skip provenance queries for:

- Greenfield writes — creating a new file or appending purely additive code
  to an existing one.
- Single-line trivial changes — typo fixes, comment edits, formatting, lint
  fixes, import sorting.
- Pure documentation or config edits — the `paths:` glob already excludes
  `*.md`, `README*`, `CHANGELOG*`, `*.json`, `*.yaml`, `*.yml`, `*.toml`, but
  if the user asks about a config-adjacent file inside excluded scope, don't
  override the exclusion.
- Files the skill returns empty for — `--only-if-substantial` already
  filtered them out; an empty response means "no provenance, proceed
  normally."

If unsure whether the change is substantive, see `references/triggers.md`.

## How to query

Two patterns. See `references/querying.md` for full examples and JSON shapes.

**Point lookup** — when the user asks about a specific line or a small
contiguous range:

```bash
prov log src/payments.ts:247 --only-if-substantial --json
```

Returns `{ status: "unchanged" | "drifted" | "no_provenance", prompt, model,
conversation_id, turn_index, blame_commit, ... }` for that line.

**Whole-file context** — when the change spans the file or you want the
file's prompt history before planning:

```bash
prov log src/payments.ts --only-if-substantial --json
```

Returns `{ edits: [{ line_start, line_end, prompt, model, timestamp, ... }],
history: [...] }`. An empty `edits` array means no provenance for this file
— proceed without it.

## How to use the result

When provenance is found:

1. **Cite the prompt verbatim** in the planning step before proposing the
   edit. Don't paraphrase. The exact wording carries the constraint.
2. **Identify the load-bearing constraint** — what did the original prompt
   ask for that shaped the current code? "Keep dedupe at 90 days", "validate
   email RFC 5322 strictly", "fail closed on rate limit" — these are
   constraints, not arbitrary choices.
3. **Decide whether the current request invalidates that constraint.** If
   the user explicitly asks to change it ("change the dedupe window to 30
   days"), the constraint is now stale — proceed. If the user asked for an
   adjacent change ("add retry logic"), preserve the constraint.
4. **If the line is `drifted`**, the current code no longer matches the
   original AI capture — a human has edited it since. Frame the explanation
   around both the original intent and the divergence. Be careful: a drifted
   line often encodes a deliberate human override of AI logic, and rewriting
   it back to match the prompt may regress whatever bug the human was fixing.

When provenance is empty:

- Proceed normally. Empty isn't an error — it just means the file is short,
  has no notes, or is excluded by `--only-if-substantial`.

## Failure modes

- `prov` not on PATH: the binary install hasn't run. Skip the query and
  proceed normally; surface a one-line note to the user that `prov log`
  is unavailable.
- Repo has no `.git/prov.db`: the user hasn't run `prov install` here. Skip
  and proceed.
- `prov log` returns `status: "no_provenance"`: the line has no resolvable
  note (could be human-authored, could be a fresh repo). Proceed without it.

Hooks fail non-blocking by design; absence of provenance is never a reason
to refuse the user's request.
