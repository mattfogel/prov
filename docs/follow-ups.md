# Implementation Follow-ups

Items discovered during implementation that should be addressed before the
parent unit is considered fully done, or rolled into a later unit. Distinct
from the v1 plan's "Deferred to Implementation" section: those entries were
chosen to defer at planning time. The entries here surfaced after the unit
shipped and need a home so they don't get lost.

When closing one out, delete the entry (or move it to a `## Closed` section
with a commit/PR link).

---

## U3 — Capture pipeline (PR #3)

- [ ] **Surface "captured but not yet flushed" state to the user.** Capture
  fires on PostToolUse and lives in `<git-dir>/prov-staging/<session_id>/`
  until `git commit` runs the post-commit handler, which is when the note
  is actually written and `prov log` / `prov search` see it. The natural
  user mental model is "prov captures on PostToolUse" and the gap between
  that and "prov shows on commit" is invisible — staged sessions can sit
  for hours with no signal that they exist. Surfaced while dogfooding U5:
  user ran two Claude Code sessions, didn't commit, and was surprised
  `prov log` didn't reflect them. Options to weigh when picking this up
  (don't pre-commit a solution): (a) a `prov status` command that lists
  staged sessions and their staleness; (b) auto-include the staging count
  in `prov log` / `prov search` output when non-empty ("note: 2 staged
  sessions not yet committed"); (c) a `prov flush` command that
  synthesizes a preview note against `git diff --cached` without
  committing; (d) docs-only fix: README + Skill description make the
  capture/flush split explicit. Owner: open — likely sits with whoever
  picks up Phase 2 polish or Phase 3 Skill work, since the Skill (U12)
  also has to know about commit-vs-staging to behave correctly.

- [ ] **Partial-match cleanup of `edits.jsonl`.** The post-commit handler
  removes the entire session dir only when *every* staged edit matched
  the commit's diff. Partial-match cleanup (rewriting `edits.jsonl` to
  drop just the matched entries) is currently deferred. Risk: a session
  with one matched edit and one unmatched edit will re-attempt the
  matched edit on the next commit and re-write it as a duplicate.
  Owner: U9 (history-rewrite handling) — it touches the same staging-tree
  invariants and benefits from being designed alongside the rewrite path.

---

## U8 — Sync (fetch/push helpers + pre-push gate)

- [ ] **`prov.scanAllPushes` config to extend the gate to non-notes refs.** The
  v1 gate scopes to `refs/notes/prompts*` per R6's "negligible overhead on
  regular pushes" requirement. The plan calls out `prov.scanAllPushes` as the
  opt-in escape hatch for users who want the redactor to also scan diffs of
  every code commit being pushed. Implementing it requires walking
  `<remote-sha>..<local-sha>` and running the redactor over each commit's
  diff text — straightforward but adds latency proportional to the diff size,
  and needs care around binary blobs and very large pushes. Owner: open;
  natural pickup once a real user asks for it.

- [ ] **Audit log when `--no-verify` is used directly via `git push`.** When
  the user runs `prov push --no-verify`, the audit log records the bypass
  before invoking git (see `commands/push.rs`). When the user instead runs
  plain `git push --no-verify` (skipping `prov push` entirely), git suppresses
  every hook — there is no in-band place to record the bypass. Options: (a)
  a periodic background reconciliation that diffs the local notes ref against
  the remote tracking ref and warns if a delta exists that the gate would
  have caught; (b) docs-only — make it clear that `--no-verify` via `git
  push` directly is unaudited and should be avoided. Owner: open; revisit if
  team-mode adopters report drift.

- [ ] **Single-commit pinpointing in pre-push error messages.** The gate
  reports the *annotated commit* SHA carried by each note blob, which is
  what a user wants to investigate. But for a note attached via squash/merge
  the user may need extra context to reach the offending source line; pairing
  the SHA with the file path(s) named in the note's `edits[]` would help.
  Cheap to add — gate already parses the note. Owner: open; pickup with
  any future redactor-message polish pass.

---

## U13 — GitHub Action (PR #46)

- [ ] **Pin cosign verification to the release workflow's OIDC identity.**
  `action/src/download.ts:defaultVerifier` calls `sigstore.verify(bundle,
  artifact)` with the library's default policy — it confirms the
  signature chains to Fulcio and the Rekor entry exists, but does NOT
  assert *which* workflow produced the signature. A compromised
  unrelated workflow in `mattfogel/prov` (or any future repo with write
  access to the release ref) could in principle sign a bad binary that
  would still verify. The strict check is to require the OIDC subject
  match `https://github.com/mattfogel/prov/.github/workflows/release.yml@refs/tags/v*`
  (exact subject TBD once the release workflow exists). sigstore-js v3
  supports this via the `certificateIdentities` option on the verifier.
  Cannot be implemented yet — the release workflow itself doesn't exist
  (deferred to U1's release-plz/cargo-dist setup), so we don't know the
  exact OIDC subject string to pin against. Verification: once the first
  signed release lands, inspect the bundle's certificate (`cosign verify
  --certificate-identity-regexp ...`), copy the subject into the
  verifier, add a test that rejects a bundle signed by a different
  workflow. Owner: whoever turns on the release workflow.

- [ ] **Sanity-check `sigstore.verify` API shape against a real signed
  bundle.** `defaultVerifier` is the right call shape per sigstore-js v3
  docs but has not been exercised against a real release bundle —
  there are no signed releases yet. The verifier is isolated behind
  the `Verifier` type so a correction stays contained. Verification:
  once the first signed release lands, run the Action against it in a
  scratch repo workflow and confirm verification passes; if the call
  signature has drifted, update `defaultVerifier` and the optional
  `verifier` parameter on `downloadProv`. Owner: whoever turns on the
  release workflow.

---

## docs/solutions — refresh hints

- [ ] **Refresh `git-subprocess-hardening-conventions-2026-05-02.md` Pattern 7
  when issue #29 lands.** Pattern 7 currently gives a manual `git reflog
  expire --expire=now --all && git gc --prune=now` recipe as the remediation
  for `prov redact-history` not scrubbing residuals. Once #29 ships a
  built-in `prov gc-secrets` (or `prov redact-history --gc`), Pattern 7
  becomes stale: it will describe a manual workflow the CLI now automates
  and won't cross-reference the polarity convention in
  `defensive-default-polarity-conventions-2026-05-03.md` that explains *why*
  the prune cutoff has to be defensive. Verification: run
  `/ce-compound-refresh git-subprocess-hardening-conventions-2026-05-02`
  after #29 closes; confirm Pattern 7 names the new subcommand and links to
  the polarity doc. Owner: whoever closes #29.

---

## How to add an entry

```
- [ ] **Short title.** One paragraph: what's missing, where the marker is
  (file:line if in code), how to verify the fix, suggested owner.
```
