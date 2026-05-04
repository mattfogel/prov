# Querying provenance

Concrete patterns for calling `prov log` from the agent loop, with example
JSON output and how to integrate findings into the planning step.

## Two query shapes

### Point lookup — `prov log <file>:<line> --json`

Use when the user references a specific line, or when the proposed change
touches a small contiguous range and you want the originating prompt for
that range.

```bash
prov log src/payments.ts:247 --only-if-substantial --json
```

Example output for an unchanged line:

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

Example output for a drifted line (current code no longer matches AI capture):

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

Example output when no provenance is available:

```json
{
  "file": "src/payments.ts",
  "line": 247,
  "status": "no_provenance",
  "no_provenance_reason": "no note attached to the originating commit",
  "prov_version": "0.1.1"
}
```

### Whole-file context — `prov log <file> --json`

Use when the change spans the file or you want the prompt history before
planning a structural edit.

```bash
prov log src/payments.ts --only-if-substantial --json
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

The `edits` array is ordered by recency (most recent first). `history`
carries superseded prior prompts when an AI rewrite replaced an earlier AI
edit on the same span.

## Empty result handling

`--only-if-substantial` returns empty for short files (< 10 lines) or files
with no existing notes:

```json
{ "file": "src/utils.ts", "edits": [], "history": [], "prov_version": "0.1.1" }
```

Empty is not an error. Proceed with the edit normally.

## Integrating into planning

When the result has provenance, surface the prompt into your planning step
**before** proposing code changes. Example planning frame:

> Before refactoring `src/payments.ts:247`, I checked the originating prompt
> via `prov log`. The line was written in turn 4 of session `sess_abc123` on
> 2026-03-12 against the prompt:
>
> > "Add a 90-day dedupe window on payment intents — compliance requires we
> > never charge twice within a quarter even if the idempotency key collides."
>
> The 90-day window is therefore a load-bearing constraint, not an arbitrary
> default. The user's current request ("rename the field to `windowDays`")
> is a cosmetic change and should preserve the value. I'll rename without
> changing the constant.

If the line is `drifted`, frame both the original intent AND the divergence:

> `prov log` reports this line as drifted — the current code differs from
> the original AI capture (the prompt asked for 90 days; the current value
> is 30). `blame_author_after` shows alice@example.com edited it on
> 2026-04-02. Likely a deliberate human override; before changing it again
> I'll ask whether the 30-day value is intentional.

## Bash idioms

If you want a single field from the JSON output, pipe through `jq`:

```bash
prov log src/payments.ts:247 --only-if-substantial --json | jq -r '.prompt'
```

For shell-based agents that don't have `jq` available, parse the JSON
directly in your tool runtime. The shape is stable across `prov_version`
within v0.x; consult `prov_version` in the envelope before trusting future
fields not documented here.

## Failure modes to handle gracefully

- `prov: command not found` — the binary isn't on PATH. Skip and proceed.
- `not in a git repo` — the cwd isn't inside a git repo. Skip.
- `.git/prov.db` missing — user hasn't run `prov install` here. Skip.
- Empty stdout (with `--only-if-substantial`) — file is short or has no
  notes. Proceed without provenance.
- `status: "no_provenance"` on a point lookup — the specific line has no
  resolvable note. Proceed without it.

In every failure case, the right move is to proceed with the edit. Provenance
is additive context; its absence never blocks the user's request.
