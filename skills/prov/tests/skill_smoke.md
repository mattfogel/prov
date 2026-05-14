# Skill smoke test plan (manual)

These behavioral scenarios are the load-bearing verification for the
question-triggered shape of this skill. There is no automated harness that
tests trigger fidelity — run them against a real Claude Code session after
any meaningful edit to `SKILL.md`'s `description:` or body.

## Setup

1. Install the `prov` binary so it's on `PATH` and run
   `prov install --agent claude` in the fixture repo (gets the capture
   hooks in place so the fixture has notes to query).
2. Install this skill into the fixture repo:
   `npx skills add github.com/mattfogel/prov` (or
   `npx skills add <local path to this checkout>` when testing local
   edits).
3. Restart Claude Code so the skill registry reloads.
4. Use a fixture repo seeded with provenance notes — either a real repo
   where Claude Code has been used for a few sessions, or the fixture under
   `crates/prov-cli/tests/fixtures/` extended with a manually-crafted note
   via `git notes --ref=refs/notes/prov add`.

## Scenario 1 — "why" question on a specific line triggers the skill

**Prompt to Claude Code:**
> Why is `src/payments.ts:247` set to 90 days?

**Expected behavior:**
- Agent runs `prov log src/payments.ts:247` (with or without `--json`).
- Agent surfaces the originating prompt verbatim in the answer.
- Answer names the load-bearing constraint (e.g., "compliance requires
  90-day dedupe") and attributes it to the model + session.

**Pass criteria:** the agent ran `prov log` and quoted the prompt in its
answer.

## Scenario 2 — file-level history question triggers the skill

**Prompt to Claude Code:**
> What's the prompt history of `src/payments.ts`?

**Expected behavior:**
- Agent runs `prov log src/payments.ts`.
- Agent renders the captured edits with prompts, models, and timestamps,
  most recent first.

**Pass criteria:** the agent ran the whole-file query and listed at least
one prompt.

## Scenario 3 — search question triggers the skill

**Prompt to Claude Code:**
> Find the prompts where we talked about rate limiting.

**Expected behavior:**
- Agent runs `prov search "rate limiting"`.
- Agent renders the hits with the prompts and the files/commits they
  touched.

**Pass criteria:** the agent ran `prov search` rather than grepping the
codebase.

## Scenario 4 — drifted line surfaces the divergence

**Setup detail:** seed the fixture so line 247 has been hand-edited since
the original AI capture, so `prov log src/payments.ts:247 --json` returns
`status: "drifted"`.

**Prompt to Claude Code:**
> Why is `src/payments.ts:247` set to its current value?

**Expected behavior:**
- Agent runs `prov log src/payments.ts:247`.
- Answer surfaces both the *original* AI prompt AND the divergence (the
  current value differs from the AI capture, and `blame_author_after`
  shows who changed it).
- Agent flags that the current value may be a deliberate human override.

**Pass criteria:** the answer names both the original intent and the
drift.

## Scenario 5 — edit request does NOT trigger the skill

**Prompt to Claude Code:**
> Refactor `src/payments.ts` to extract the dedupe logic into a separate
> module.

**Expected behavior:**
- Agent does NOT preemptively run `prov log`.
- Agent proceeds with the refactor; it may read the file, plan, and edit
  as normal.

**Pass criteria:** no `prov log` invocation appears in the session log
unless the user asked a provenance follow-up.

This is the central regression the rewrite exists to prevent — the prior
shape of the skill ran `prov log` before any non-trivial edit, which
created unwanted context bloat and noise.

## Scenario 6 — no-provenance case is reported plainly

**Prompt to Claude Code:**
> Who wrote `src/utils.ts:5`?

**Setup detail:** ensure `src/utils.ts:5` has no captured note (a
human-authored line in a fresh repo works).

**Expected behavior:**
- Agent runs `prov log src/utils.ts:5`.
- Response is `status: "no_provenance"` (or similar).
- Agent answers plainly: "No provenance recorded for that line — likely
  human-authored or predates `prov install`," optionally falling back to
  `git blame`.

**Pass criteria:** the agent neither invents an explanation nor treats the
empty response as an error.

## Scenario 7 — greenfield / non-provenance question does NOT trigger

**Prompt to Claude Code:**
> Create a new file `src/utils/format.ts` with a function that formats a
> Date as `YYYY-MM-DD`.

**Expected behavior:**
- Agent does NOT run `prov log` or `prov search`.
- Agent writes the file as requested.

**Pass criteria:** no `prov` invocation in the session log.

## Iteration loop

If a scenario fails:

- **Trigger fails for a provenance question (false negative)** — the
  `description:` field is the lever. Add the missing phrasing pattern to
  both `description:` and the "Phrasings that should trigger" section of
  `references/triggers.md`. Re-test.
- **Trigger fires on an edit/refactor/greenfield ask (false positive)** —
  the rewrite's central failure mode. Strengthen the "Phrasings that
  should NOT trigger" section and reinforce the negative in
  `description:`. Re-test until the false positive goes away.
- **Drift state isn't surfaced** — the example in `references/querying.md`
  for drifted lines is the prompt the agent learns from. Make it louder
  and more specific.

## Content lints (automated)

These lints run in CI via `crates/prov-cli/tests/cli_skill_layout.rs`:

- `SKILL.md` exists and parses as YAML+Markdown.
- Frontmatter has `name` and `description` (both non-empty; `description`
  ≥ 60 characters).
- Body is at most 500 lines.
- `references/querying.md` and `references/triggers.md` exist and are
  referenced by name from `SKILL.md`.
- This smoke test plan exists at `tests/skill_smoke.md`.
