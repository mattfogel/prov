# Querying provenance

Concrete patterns for calling `prov log` and `prov search` when the user has
asked about the origin, intent, or history of code. Example JSON shapes and
notes on how to compose the answer.

## Three query shapes

### Point lookup — `prov log <file>:<line> [--json]`

Use when the user asks about a specific line or a small contiguous range
("why is line 247 doing X", "what was the prompt for this if-branch").

```bash
prov log src/payments.ts:247 --json
```

Example output for an **unchanged** line (current code still matches the
original AI capture):

```json
{
  "file": "src/payments.ts",
  "line": 247,
  "status": "unchanged",
  "prompt": "Add a 90-day dedupe window on payment intents — compliance requires we never charge twice within a quarter even if the idempotency key collides.",
  "model": "claude-sonnet-4-5",
  "timestamp": "2026-03-12T14:22:08Z",
  "conversation_id": "sess_abc123",
  "turn_index": 4,
  "blame_commit": "a1b2c3d4e5f6",
  "prov_version": "0.1.1"
}
```

Example output for a **drifted** line (current code differs from the
original AI capture — someone hand-edited it):

```json
{
  "file": "src/payments.ts",
  "line": 247,
  "status": "drifted",
  "prompt": "Add a 90-day dedupe window on payment intents — ...",
  "model": "claude-sonnet-4-5",
  "timestamp": "2026-03-12T14:22:08Z",
  "conversation_id": "sess_abc123",
  "turn_index": 4,
  "blame_commit": "a1b2c3d4e5f6",
  "blame_author_after": "alice@example.com",
  "prov_version": "0.1.1"
}
```

Example output when there is no provenance for the line:

```json
{
  "file": "src/payments.ts",
  "line": 247,
  "status": "no_provenance",
  "no_provenance_reason": "no note attached to the originating commit",
  "prov_version": "0.1.1"
}
```

### Whole-file history — `prov log <file> [--json]`

Use when the user asks about the file overall ("what's the history of
`src/payments.ts`", "what prompts have shaped this file", "show me every AI
edit on this file").

```bash
prov log src/payments.ts --json
```

Example output:

```json
{
  "file": "src/payments.ts",
  "edits": [
    {
      "commit_sha": "a1b2c3d4e5f6",
      "line_start": 240,
      "line_end": 268,
      "prompt": "Add a 90-day dedupe window on payment intents — ...",
      "model": "claude-sonnet-4-5",
      "timestamp": "2026-03-12T14:22:08Z",
      "conversation_id": "sess_abc123"
    },
    {
      "commit_sha": "f6e5d4c3b2a1",
      "line_start": 12,
      "line_end": 58,
      "prompt": "Implement the PaymentIntent factory — accept an idempotency key, ...",
      "model": "claude-opus-4-7",
      "timestamp": "2026-02-28T09:11:42Z",
      "conversation_id": "sess_def456"
    }
  ],
  "history": [],
  "prov_version": "0.1.1"
}
```

The `edits` array is ordered most-recent first. `history` is populated when
you pass `--history`: it carries superseded prior prompts that an AI rewrite
replaced.

### Prompt search — `prov search <query> [--json]`

Use when the user asks "where did we decide X" or "find the prompts where
we talked about Y" — the question isn't about a specific file or line, it's
about a topic that may have surfaced in multiple prompts.

```bash
prov search "rate limiting" --json
```

Returns matching prompts with the commits and files they touched. Useful
for tracing the lineage of a decision across a codebase ("when did we first
introduce rate limiting? which prompts framed it?").

## Useful flags

- `--json` — machine-readable envelope. Use whenever you'll pick a single
  field or correlate with other tool output. The human-readable rendering
  is fine when you'll just quote the prompt back to the user.
- `--history` — on `prov log`, walks `derived_from` so AI-on-AI rewrites
  show the superseded prior prompts. Useful for "what did this look like
  before".
- `--only-if-substantial` — returns empty for files under 10 lines or files
  with no captured notes. Rarely needed here: the user has *already* asked
  about a specific file, so substantiality is implicit. Use it only if you
  want to suppress noise on tiny files.
- `--full` — reserved for future transcript expansion; currently prints the
  stored `preceding_turns_summary`.

## Composing the answer

When provenance is present, **quote the prompt verbatim** — paraphrase
loses constraint nuance ("90-day dedupe window" carries the unit and the
implied compliance horizon; "a long dedupe window" doesn't).

Include the load-bearing metadata: model, conversation id, turn index,
timestamp, commit. The user often cares about *which session* introduced
the code so they can find the conversation.

For drifted lines, name both the original intent and the divergence so the
user can decide which to trust:

> Originally written by `claude-sonnet-4-5` against the prompt "add a 90-day
> dedupe window for compliance"; the current value is 30 days, edited by
> `alice@example.com` on 2026-04-02 in commit `b3c4d5e`. Two plausible
> reads: Alice deliberately overrode the compliance default, or this was an
> ad-hoc tweak that broke the original constraint. Worth confirming with
> Alice.

For `no_provenance` or empty results, say so plainly. Don't invent
explanations — the line is either human-authored, predates `prov install`,
or comes from a teammate who hasn't shared their notes ref.

## Bash idioms

Pick a single field from JSON output with `jq`:

```bash
prov log src/payments.ts:247 --json | jq -r '.prompt'
prov log src/payments.ts --json | jq -r '.edits[] | "\(.timestamp) \(.prompt)"'
```

If `jq` isn't available, parse the JSON in your tool runtime. The shape is
stable across `prov_version` within v0.x; consult the `prov_version` field
before relying on fields not documented here.

## Failure modes to handle gracefully

- `prov: command not found` — the binary isn't on PATH. Tell the user
  `prov` isn't installed and offer the install command from the project
  README.
- `not in a git repo` — answer "I can't query provenance outside a git
  repo."
- `.git/prov.db` missing — `prov install` hasn't been run here. Tell the
  user.
- Empty stdout / `edits: []` — no captured edits. Tell the user plainly.
- `status: "no_provenance"` on a point lookup — the line has no note.
  Likely human-authored or predates install.

In every failure case the right move is to surface the absence to the user
and answer their question with what you can derive from other sources
(`git blame`, `git log`, the code itself).
