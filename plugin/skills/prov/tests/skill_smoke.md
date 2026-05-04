# Skill smoke test plan (manual)

These four behavioral scenarios are the load-bearing verification for U12.
They run against a real Claude Code session — there is no automated harness
that tests trigger fidelity. Run them after every meaningful edit to
`SKILL.md`'s `description:` or body.

## Setup

1. Install the prov binary so it's on `PATH`.
2. Install this plugin (`/plugin install --plugin-dir <repo>/plugin` or
   marketplace).
3. Restart Claude Code so hooks reload.
4. Use a fixture repo seeded with prov notes — either:
   - A real repo where you've used Claude Code for a few sessions, or
   - The fixture under `crates/prov-cli/tests/fixtures/` extended with
     a manually-crafted note via `git notes --ref=refs/notes/prompts add`.

## Scenario 1 — substantive ask triggers the skill

**Prompt to Claude Code:**
> Refactor `src/payments.ts` to extract the dedupe logic into a separate
> module.

**Expected behavior:**
- Agent calls `prov log src/payments.ts` (or `:<line>`) before proposing
  edits.
- Agent surfaces the prior dedupe-window prompt in its plan, e.g.,
  *"the originating prompt called for a 90-day window for compliance — I'll
  preserve that."*
- Final edit preserves the load-bearing constraint.

**Pass criteria:** the agent runs `prov log` and cites the prompt before
writing code.

## Scenario 2 — trivial single-line change does NOT trigger

**Prompt to Claude Code:**
> Fix the typo on line 12 of `README.md`.

**Expected behavior:**
- Agent does not call `prov log`.
- The `paths:` glob excludes `*.md`, so the skill should not even surface.

**Pass criteria:** no `prov log` invocation in the session log.

## Scenario 3 — greenfield does NOT trigger

**Prompt to Claude Code:**
> Create a new file `src/utils/format.ts` with a function that formats a
> Date as `YYYY-MM-DD`.

**Expected behavior:**
- Agent does not call `prov log` — the file doesn't exist yet, so there's
  no provenance to query.
- If the agent does call it, the response should be empty (no notes for a
  non-existent file) and the agent should proceed without it.

**Pass criteria:** either the agent skips the query, or queries it and
correctly handles the empty response without surfacing it as a finding.

## Scenario 4 — drifted line surfaces drift state

**Prompt to Claude Code:**
> Explain `src/payments.ts:247`.

**Setup detail:** ensure the line at 247 has been hand-edited since the
original AI capture, so `prov log src/payments.ts:247 --json` returns
`status: "drifted"`.

**Expected behavior:**
- Agent calls `prov log src/payments.ts:247`.
- Agent's explanation references both the original prompt AND the drift
  state, e.g., *"originally written by Claude in turn 4 of session
  sess_abc123 against the prompt 'add a 90-day dedupe window'; the line
  has since been hand-edited (drifted)."*

**Pass criteria:** explanation surfaces both the original intent and the
divergence.

## Iteration loop

If a scenario fails:

- **Trigger fails for substantive asks (false negatives)** — the
  `description:` field is the lever. Add more trigger phrasing
  ("before refactoring", "before editing AI-written code",
  "to recover the original prompt"). Re-test.
- **Trigger fires for trivial asks (false positives)** — strengthen the
  "When NOT to use it" section in the body and add explicit exclusions to
  `description:`. Re-test.
- **Drift state isn't surfaced** — the `references/querying.md` example
  for drifted lines is the prompt the agent learns from. Make the example
  louder.

## Content lints (automated)

These lints run in CI via `crates/prov-cli/tests/cli_plugin_layout.rs`:

- `SKILL.md` exists and parses as YAML+Markdown.
- Frontmatter has `name` and `description` (both non-empty).
- Body is at most 500 lines.
- `references/querying.md` and `references/triggers.md` exist and are
  referenced by name from `SKILL.md`.
