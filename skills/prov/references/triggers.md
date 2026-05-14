# Triggers — recognizing a provenance question

This skill activates on user questions about the *origin*, *intent*, or
*history* of code that already exists. The lever for activation is the
user's phrasing, not the file or the kind of work in progress. This page
maps common phrasings to the right query.

## Phrasings that should trigger

### "Why does this code do X?" / "Why is this set to Y?"

The user wants the *reason* behind a specific behavior. The originating
prompt usually carries the reason (compliance constraint, perf trade-off,
a hard-won bug fix). Query with a **point lookup** on the line that encodes
the behavior:

```bash
prov log src/payments.ts:247
```

If the user named a function instead of a line, run `grep` or open the
file to find the relevant line, then look it up.

### "What was the prompt for this?" / "Show me the prompt that wrote this."

Direct ask. Same query — point lookup if they referenced a specific line
or block; whole-file lookup if they said "this file":

```bash
prov log src/payments.ts:247
prov log src/payments.ts
```

### "Who wrote this?" / "Did the agent write this?" / "Which model?"

The user wants attribution: human vs. AI, and which model. Run a point
lookup; the response includes `model`, `conversation_id`, `turn_index`,
and `blame_commit`. If `status` is `no_provenance`, fall back to
`git blame` and tell the user this looks human-authored or pre-dates
`prov install`.

### "What's the history of this file?" / "What prompts have shaped this?"

Whole-file lookup, most useful for files with several captured edits:

```bash
prov log src/payments.ts
```

If the user wants AI-on-AI rewrites surfaced too, add `--history`.

### "Has this line been hand-edited since the AI wrote it?" / "Is this drifted?"

Point lookup. The `status` field answers directly: `unchanged`, `drifted`,
or `no_provenance`. A `drifted` response includes `blame_author_after` so
you can tell the user who edited it.

### "Find prompts about X" / "Where did we decide to use X?"

Cross-file search:

```bash
prov search "rate limiting"
```

Returns matching prompts with the commits and files they touched. Pair the
results with `git log` if the user wants to walk the decision history.

### "What session was this from?" / "What conversation produced this?"

Point lookup. The response carries `conversation_id` and `turn_index`. The
user can use those to find the original transcript in their agent harness.

## Phrasings that should NOT trigger

The skill is only for *origin/intent/history* questions. Skip it for:

- **Edit requests** — "refactor this", "rename X to Y", "extract this into
  a function", "fix this bug". Even if the file is AI-authored, don't run
  `prov log` preemptively. If the user follows up with "why did the
  original prompt set X to Y", *then* it's a provenance question.
- **Greenfield asks** — "create a new file", "write a function that does
  X". There's nothing to query.
- **Pure code-reading asks** — "what does this function do" (the answer is
  in the code itself), "trace the call graph" (use grep/code reading), "is
  this used anywhere" (use grep). These are about the *current code*, not
  its origin.
- **Project-level questions** that aren't about specific code — "what's
  this project about" (read the README), "how is the codebase organized"
  (read the structure).

If the user's phrasing is ambiguous between "what does this code do" and
"why does this code do X", lean on whichever interpretation they emphasize.
"Why" and "what for" lean provenance; "what" and "how" lean code-reading.

## Gray-zone phrasings

These genuinely could go either way; use judgment:

- **"Explain this code"** — usually a code-reading ask, but if the code has
  unusual constants or non-obvious branches, the originating prompt may
  carry the explanation. Worth a quick point lookup; if `no_provenance`,
  fall back to explaining from the code.
- **"What's the intent here?"** — usually leans provenance ("intent" maps
  to the original prompt), but if the file has no captured notes, treat as
  a code-reading ask.
- **"Is this still correct?"** — leans code-review, but a provenance check
  can reveal whether the current behavior matches the original spec
  (drift). Worth a point lookup if the user is questioning correctness.

## How user follow-ups shift the work

After you answer a provenance question, the user often asks a follow-up
that *is* an edit ("OK, then change the window to 60 days"). That follow-up
isn't itself a provenance question, but the context you just established
should inform the edit:

- If the prompt called the value out as load-bearing ("compliance requires
  ..."), flag the constraint before making the change.
- If the line was already `drifted`, the existing value may be a human
  override worth preserving — ask before overwriting.
- If the prompt was an arbitrary default ("just pick a sensible window"),
  the change is uncontroversial.

That's not the skill firing again — it's you carrying the answer you
already produced into the next turn.
