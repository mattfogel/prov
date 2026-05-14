---
name: prov
description: Use when the user asks about code provenance — "why does this code do X", "what was the original prompt for this line", "who or what wrote this", "what's the history of this file", "find the prompts where we decided Y", "has this been rewritten and what came before", "was this AI-written, by which model, in which session". Calls `prov log <file>[:<line>]` to surface the originating prompt, model, conversation, and drift state, and `prov search <query>` to find prompts by content. Use whenever the user wants the *why* behind code that's already on disk, the prompt history of a file, or the conversation a line came out of. Do NOT auto-run before edits, refactors, or debugging — only when the user has actually asked an origin/history/intent question.
license: MIT
---

# prov — answer user questions about code provenance

`prov` is a local tool that records the prompt-and-conversation context behind
AI-authored edits and attaches it to commits via git notes. This skill is how
you turn that buried context into answers when the user asks about it.

## What prov is

- **Capture happens automatically** in the background: harness hooks stage the
  prompt + edits during a session, and a `post-commit` git hook attaches the
  staged context to the resulting commit as a note on `refs/notes/prov` (or
  `refs/notes/prov-private`).
- **You read it via the CLI.** The two surfaces you'll use are `prov log` and
  `prov search`. Output is human-readable by default; pass `--json` when you
  want a machine-readable envelope.
- **Notes are local-only by default.** If `prov log` returns no provenance,
  the file is either older than the install, was authored by a human, or
  comes from a teammate who hasn't pushed notes.

## When to use this skill

Activate when the user asks for the *origin*, *intent*, or *history* of code
that already exists. Concrete user phrasings that should trigger you:

- "Why does `X` do this?" / "Why is `Y` set to `Z` here?"
- "What was the prompt that wrote this line / function / file?"
- "Did I write this, or did the agent?" / "Which model wrote this?"
- "What's the history of this file?" / "Show me the prompts behind
  `src/foo.rs`."
- "Has this been rewritten? What did it look like before?"
- "Find the prompts where we talked about rate limiting / dedupe / retries."
- "What session was this from?" / "What conversation produced this code?"
- "Has someone hand-edited this since the AI wrote it?" (drift question)

If the user is asking you to *change* code — refactor, debug, extend,
rewrite, fix — **do not** preemptively run `prov log`. Only query if the
user themselves asks an origin/intent question.

For the full mapping of user phrasings to queries, see
`references/triggers.md`.

## How to query

Three patterns cover almost every question. Full example output and JSON
shapes in `references/querying.md`.

**Point lookup — "who/what wrote this specific line"**

```bash
prov log src/payments.ts:247
```

Returns the originating prompt, model, conversation id, turn index, and
drift status (`unchanged` | `drifted` | `no_provenance`) for that line.

**Whole-file history — "what prompts have shaped this file"**

```bash
prov log src/payments.ts
```

Returns every captured edit on the file, most recent first.

**Cross-file search — "where did we decide X"**

```bash
prov search "rate limiting"
```

Full-text search across captured prompts. Returns matching prompts with the
files and commits they touched.

Add `--json` when you want to parse the response programmatically (e.g., to
pick a single field or correlate with other tool output). Add `--history`
to `prov log` to walk the `derived_from` chain and surface superseded prior
prompts when an AI rewrite replaced an earlier AI edit.

## How to answer with the result

Cite the prompt **verbatim**. The exact wording is the provenance — the
original phrasing carries constraints that paraphrase will lose.

A good answer for an unchanged line:

> Line 247 came from turn 4 of session `sess_abc123` on 2026-03-12, written
> by `claude-sonnet-4-5` against this prompt:
>
> > "Add a 90-day dedupe window on payment intents — compliance requires we
> > never charge twice within a quarter even if the idempotency key
> > collides."
>
> So the 90-day window is a deliberate compliance constraint, not an
> arbitrary default.

A good answer for a drifted line — surface both the original intent and the
divergence:

> `prov log` reports this line as drifted. The original AI capture (prompt:
> "add a 90-day dedupe window for compliance") asked for 90 days; the
> current value is 30. `blame_author_after` shows `alice@example.com`
> changed it on 2026-04-02 in commit `b3c4d5e`. That looks like a deliberate
> human override — worth checking with Alice before assuming the 30-day
> value is wrong.

A good answer when there's no provenance:

> No provenance recorded for that line. Likely human-authored, or it
> predates `prov install` in this repo.

## What to do with the result besides answering

The user may use your answer to inform their next ask. If they follow up
with "OK, then change it to 60 days", you now know the constraint was
load-bearing — flag the compliance angle before making the change. But
running `prov log` is only justified once the user has asked an
origin/intent question, not as a preemptive check.

## Failure modes

All of these mean "skip the query and tell the user plainly":

- `prov: command not found` — the binary isn't installed. Say so.
- `not in a git repo` — the cwd isn't inside a git repo.
- `.git/prov.db` missing — `prov install` hasn't been run here.
- `status: "no_provenance"` (point lookup) — the line has no resolvable
  note. Could be human-authored, could be from before the install.
- Empty `edits` array (whole-file lookup) — no captured edits on this file.

None of these are errors. They just mean the answer to the user's question
is "we don't have provenance for that," which is a perfectly fine answer.
