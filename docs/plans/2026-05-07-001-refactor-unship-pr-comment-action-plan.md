---
title: "refactor: Unship the U13 PR-comment GitHub Action"
type: refactor
status: completed
date: 2026-05-07
---

# refactor: Unship the U13 PR-comment GitHub Action

## Summary

Remove the TypeScript GitHub Action that posts the per-session "PR intent timeline" comment on PRs (U13, shipped in PR #46). The Rust renderer at `crates/prov-cli/src/render/timeline.rs` and the `prov pr-timeline` CLI command stay — they remain a local query surface that fits Prov's "queryable by users and models" framing. The Action surface (`action/` directory, CI `action` job, `scripts/check.sh` Action block + STICKY_MARKER cross-language sentinel) is deleted; README positioning is tightened to drop the `## GitHub Action` H2, drop the third differentiator entirely (the differentiators list goes from three to two — Skill, redactor-by-default-when-shared), and delete the pre-release cosign blockquote that claimed verification for code that does not exist (`install.sh` is also absent from the repo today). The Action remains revivable from git history (commit `e1dfbd8`) if it later returns as opt-in behavior — though revival should follow evidence of demand, not just a config-flag rebuild.

## Problem Frame

The U13 Action posts a sticky comment to every PR. PRs are already noisy with code-review comments, and an automatically-posted timeline comment is more noise than signal for most users. Prov's job is to **track prompts and their related code changes and make them queryable** — not to push that data back into review surfaces by default. The Action also carries real maintenance weight (a TypeScript surface, ~85K LOC of committed `dist/` bundle, a separate CI job, a cross-language sentinel that constrains the Rust renderer's `STICKY_MARKER` constant, and two open security follow-ups in `docs/follow-ups.md` waiting on a release workflow that doesn't exist). Removing it now is cheap; reviving it later as opt-in is straightforward via `git checkout e1dfbd8 -- action/` plus restoring the CI job. The CLI command `prov pr-timeline --markdown/--json` is preserved as the local query escape hatch — reviewers who want the timeline can still produce it on demand.

## Requirements

- R1. The `action/` directory is removed from the working tree (source, tests, manifest, package files, committed `dist/` bundle).
- R2. The CI workflow no longer builds, tests, or freshness-checks the Action — `.github/workflows/ci.yml` retains only `test` and `lint` jobs.
- R3. `scripts/check.sh` no longer runs the Action's npm/jest/ncc steps and no longer enforces the cross-language `STICKY_MARKER` drift sentinel.
- R4. The README no longer advertises a GitHub Action and no longer makes verification claims about code that does not exist: the `## GitHub Action` H2 is removed, the `for reviewers` bullet in the surfaces list is removed, the third differentiator (`PR intent timeline as a review artifact`) is deleted entirely so the differentiators list goes from three to two (`Agent-first via the Claude Code Skill` + `Redactor-by-default-when-shared`), the `and the GitHub Action` clause in the Install-section cosign sentence is removed, and the entire pre-release cosign paragraph at README line 36 is deleted (both halves of that paragraph — the Action and `install.sh` — are forward-looking claims about code that does not exist; `install.sh` is referenced in the install-options block but is not present in the repo).
- R5. `docs/dogfooding.md` no longer carries the `### GitHub Action — U13` section. The CLI-level `### PR timeline` section above it stays.
- R6. `docs/follow-ups.md` no longer carries the `## U13 — GitHub Action (PR #46)` block (the cosign-pin and sigstore-API sanity-check follow-ups).
- R7. The v1 plan (`docs/plans/2026-04-28-001-feat-prompt-provenance-v1-plan.md`) marks R12 as dropped, adds a `Status: Dropped` note to U13, and edits the now-stale R7 parenthetical at line 33, mirroring the existing R13/U14 strike pattern for the requirement and unit. No deeper restructuring of the v1 plan.
- R8. `prov pr-timeline` CLI command, `crates/prov-cli/src/render/timeline.rs`, the `STICKY_MARKER` constant, and all Rust-side tests for the renderer are preserved unchanged.
- R9. After all changes, `./scripts/check.sh` exits clean and `cargo test --workspace --all-targets --locked` passes.

---

## Scope Boundaries

- The `prov pr-timeline` CLI command, the Rust timeline renderer, and the `STICKY_MARKER` constant are explicitly preserved. Removing them is out of scope.
- Restructuring or re-numbering implementation units in the v1 plan is out of scope. Only R12-strike + U13 `Status: Dropped` annotation, matching the existing R13 precedent.
- Building a replacement review-surface (e.g., a Status check, a CI artifact, a different bot) is out of scope. The user's stance is "this could come later," not "replace it now."
- Touching `docs/solutions/` references to `prov pr-timeline` is out of scope — the CLI command stays, those references remain accurate.
- Touching the Plugin (`plugin/`) or the Skill (`plugin/skills/`) is out of scope — neither references the Action.
- Removing the `STICKY_MARKER` constant from `crates/prov-cli/src/render/timeline.rs` is out of scope. It remains valid in the rendered Markdown body produced by `prov pr-timeline --markdown` and is harmless dead-letter for spoof-defense purposes today.

### Deferred to Follow-Up Work

- **Reviving the Action as opt-in.** If the Action returns later, the path is `git checkout e1dfbd8 -- action/` to recover the source, restore the CI job, and add an explicit opt-in story in the README. **Two security concerns must be re-litigated at revival time** (preserving the institutional memory the deleted `docs/follow-ups.md` U13 block carried): (a) **pin cosign verification to the release workflow's OIDC subject** via `certificateIdentities` on `defaultVerifier` — the deleted block at commit `e1dfbd8 -- docs/follow-ups.md` documented this as gated on the release workflow existing; (b) **sanity-check sigstore-js's `verify` API shape** against a real signed bundle once the first release exists. Not planned today.

---

## Context & Research

### Relevant Code and Patterns

- **The R13/U14 drop precedent** at `docs/plans/2026-04-28-001-feat-prompt-provenance-v1-plan.md:39` shows the existing pattern for marking a requirement dropped after the fact: a strike-through on the requirement bullet plus a parenthetical date and rationale. Mirror this for R12.
- **The U13 commit (`e1dfbd8`)** is the revival anchor for the Action source and `dist/` bundle. Plan deletes from working tree only — git history retains everything.
- **`scripts/check.sh` mirrors `.github/workflows/ci.yml` by design** (per its own header comment). Both files need to lose the Action-related steps in lockstep so a clean local run stays predictive of CI.

### Institutional Learnings

- **`docs/solutions/`** has no entry that materially shapes this plan. The `defensive-default-polarity-conventions-2026-05-03.md` doc references `prov pr-timeline` in passing as part of the read-surface JSON envelope contract — that reference stays accurate because the CLI command stays.

### External References

None used. This is a removal task with no novel technical territory.

---

## Key Technical Decisions

- **Delete `action/` outright; do not archive in-tree.** Git history (`e1dfbd8`) preserves the Action source, tests, manifest, and `dist/` bundle. An in-tree `archive/action/` would add ~85K LOC of dead weight without adding anything `git checkout` can't already give. Rationale: this project's "small and forkable on purpose" posture in the README footer.
- **Preserve `STICKY_MARKER` in `crates/prov-cli/src/render/timeline.rs`.** The constant is part of the rendered Markdown body's contract and is harmless when no Action is consuming it. Removing it is a separate decision tied to whether the timeline shape itself should change — out of scope here.
- **Light-touch the v1 plan.** Strike R12 (matching the R13 precedent) and add a `Status: Dropped` line to U13. Do not re-number U-IDs, do not surgically delete the U13 unit body, do not edit Summary or High-Level Technical Design prose that happens to mention the Action. Rationale: the plan is a historical artifact of the v1 design as it shipped; a `Status: Dropped` annotation matches how U-ID stability is supposed to work (gaps preserved, IDs never renumbered).
- **Drop the CI `action` job entirely** rather than gating it behind a path filter. With `action/` deleted, there is nothing to gate.
- **Strip the cross-language `STICKY_MARKER` drift sentinel from `scripts/check.sh`** in the same unit that strips the Action's npm/jest/ncc steps. The sentinel exists only to detect drift between the Rust constant and the TS source — once the TS source is gone, the sentinel reports false positives forever.
- **Remove the README Install-section claim that the Action verifies cosign signatures**, but keep the sentence's claim about the install script. The install script's verification is independent of the Action and remains accurate. Rationale: avoid stranded prose about a surface that no longer exists.

---

## Open Questions

### Resolved During Planning

- **Should the `prov pr-timeline` CLI command and Rust renderer stay or go?**: Stay. They're a local query surface that matches Prov's "queryable by users and models" framing.
- **Should the README differentiator about PR intent timeline survive in some form?**: No — delete entirely. Three reviewers in the 2026-05-07 ce-doc-review pass converged: the original wedge was the surface (a sticky PR comment), not the rendering capability. A CLI-anchored rewrite would read as filler; the README's two remaining differentiators (Skill, redactor-by-default-when-shared) stand on their own.
- **Should the README's pre-release cosign paragraph at line 36 survive?**: No — delete entirely. The paragraph claims `the verification path exists in the Action and install.sh today`, but neither code path exists today (the Action is being deleted in this plan, and `install.sh` is referenced in the install-options block but absent from the repo). Both halves of the paragraph are false; the cosign sentence at line 34 already hedges with `Each release will be signed... once the release workflow ships`, which is sufficient.
- **How aggressively should the v1 plan be edited?**: Minimal. R12 strike + U13 `Status: Dropped` + R7 parenthetical edit (the only stale forward-reference that becomes nonsensical after unship). Deeper restructuring (re-numbering U-IDs, editing the Summary or High-Level Technical Design prose) is out of scope.

### Deferred to Implementation

- **Whether to leave the `pr-timeline` mentions in the v1 plan's Implementation Units (U5) untouched, since they describe the CLI command that stays.** Default: leave untouched. Anything that mentions the Action specifically gets the U13 `Status: Dropped` cross-reference; the R7 parenthetical edit at line 33 (called out in U3) is the one targeted exception.

---

## Implementation Units

- U1. **Remove the Action surface and its CI integration**

**Goal:** Delete the `action/` directory and remove every CI/local-script reference to it. After this unit, `cargo test` and `./scripts/check.sh` should both pass.

**Requirements:** R1, R2, R3, R9

**Dependencies:** None

**Files:**
- Delete: `action/` (entire directory tree — `action.yml`, `package.json`, `package-lock.json`, `tsconfig.json`, `jest.config.js`, `.gitignore`, `src/`, `__tests__/`, `dist/`, `node_modules/` if present)
- Modify: `.github/workflows/ci.yml`
- Modify: `scripts/check.sh`

**Approach:**
- Delete the `action/` directory in one move.
- In `.github/workflows/ci.yml`, remove the entire `action:` job (the third job in the `jobs:` block, including its `defaults`, all six `steps`, and the `dist/` freshness check).
- In `scripts/check.sh`, remove (a) the conditional npm/jest/ncc Action block guarded by the `git diff --name-only "$base" -- action` check, including the `step "action: ..."` heading and the surrounding `if/else` that prints `step "action: skipped"`, and (b) the `STICKY_MARKER drift check` block at the end of the file. Update the file's header comment to drop the Action and STICKY_MARKER bullet items.
- The `RUSTFLAGS=-D warnings` and the four cargo steps stay untouched — they are the script's reason to exist.

**Patterns to follow:**
- The cargo-only structure of `scripts/check.sh` before U13 added the Action block (recoverable by viewing the script in commit `49adee1`).

**Test scenarios:**
- Happy path: with the working tree mutated, `./scripts/check.sh` runs the four cargo steps cleanly and exits zero with no Action or STICKY_MARKER output.
- Happy path: `cargo test --workspace --all-targets --locked` passes — the Rust-side `prov pr-timeline` tests in `crates/prov-cli/tests/cli_read.rs` and `crates/prov-cli/tests/cli_smoke.rs` remain green (the renderer and CLI command are untouched).
- Edge case: `grep -r "action/" .github/ scripts/` returns no matches outside paths the plan intentionally leaves alone.
- Edge case: `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets --locked -- -D warnings` both pass — the Rust crate is unchanged so this should be free, but the script verifies it.

**Verification:**
- `action/` does not exist in the working tree.
- `.github/workflows/ci.yml` declares exactly two jobs: `test` and `lint`.
- `scripts/check.sh` runs only cargo steps and exits with `All checks passed.`
- `cargo test --workspace --all-targets --locked` exits zero.

---

- U2. **Update README — drop Action section, drop third differentiator, delete pre-release cosign paragraph**

**Goal:** Make the README honest about what ships. The PR-comment Action is no longer a surface, and the README should not claim verification for code that does not exist (`install.sh` is referenced but not present).

**Requirements:** R4

**Dependencies:** None (independent of U1; can land in either order, but landing both in the same PR keeps the README from briefly advertising a surface that doesn't exist).

**Files:**
- Modify: `README.md`

**Approach:**
- Remove the `**GitHub Action** for reviewers: posts a single per-session "PR intent timeline" comment on each PR…` bullet at line 9 from the surfaces list. The remaining surfaces list keeps the CLI and Skill bullets.
- **Delete** the third differentiator (currently `**PR intent timeline as a review artifact.**` plus its supporting paragraph) **entirely**. The differentiators list goes from three to two: `Agent-first via the Claude Code Skill` and `Redactor-by-default-when-shared`. Rationale: the original wedge was the surface (a sticky PR comment), not the rendering capability — any tool with a markdown renderer can produce a chronological log on demand. A CLI-anchored rewrite would read as filler. Honest positioning beats a thin claim.
- Remove the `## GitHub Action` H2 and its entire body (the YAML workflow example, the `permissions:` block reference, the `fetch-depth: 0` paragraph, the cosign verification paragraph). The next H2 (`## Contributing`) follows directly.
- In the Install section's cosign sentence at line 34, remove the `and the GitHub Action` clause. The remaining clause about `install.sh` stays as a forward-looking install-script claim.
- **Delete** the entire pre-release cosign paragraph at line 36 (the `> **Pre-release status:** ...` blockquote ending in `Tracked in [docs/follow-ups.md](docs/follow-ups.md#u13--github-action-pr-46)`). Both halves of this paragraph — the Action and `install.sh` — are forward-looking claims about code that does not exist (`install.sh` is referenced in the install-options block at line 31 but is not present in the repo). The cosign sentence at line 34 already hedges with "Coming soon" / "Each release will be signed... once the release workflow ships," which is sufficient. The deleted blockquote also points to an anchor (`docs/follow-ups.md#u13--github-action-pr-46`) that U3 deletes anyway.

**Patterns to follow:**
- The two surviving differentiator bullets (`Agent-first via the Claude Code Skill`, `Redactor-by-default-when-shared`) stand on their own without the third. Do not strengthen them to compensate for the dropped third — leave them at their current length and tone.

**Test scenarios:**
- Happy path: `grep -nE "GitHub Action|action/|prov-pr-timeline|pull-requests: write|PR intent timeline" README.md` returns no matches.
- Happy path: the README's surfaces list at the top reads as exactly two surfaces (CLI, Skill). The differentiators section reads as exactly two differentiators (Skill, redactor-by-default-when-shared).
- Happy path: `grep -nE "Pre-release status|verifier confirms|Fulcio|Rekor" README.md` returns no matches — the deleted blockquote's tells are gone.
- Edge case: the cosign sentence at line 34 still claims `Each release will be signed with Sigstore cosign keyless once the release workflow ships` (forward-looking, accurate), with the `and the GitHub Action` clause removed.

**Verification:**
- README renders cleanly on GitHub (visual sanity-check via `gh pr view --web` or a local Markdown preview).
- No reference to a `## GitHub Action` heading, `pull-requests: write` permission, `mattfogel/prov@<commit-sha>` Action invocation, `action/` directory, `PR intent timeline`, `Pre-release status` blockquote, or `docs/follow-ups.md#u13--github-action-pr-46` anchor survives in the README.

---

- U3. **Doc bookkeeping — v1 plan strike, dogfooding section removal, follow-ups removal**

**Goal:** Bring the supporting docs in line with the unship. Light-touch on the v1 plan (matching the R13/U14 drop precedent), full removal of dogfooding's Action smoke section, full removal of the U13 follow-ups block.

**Requirements:** R5, R6, R7

**Dependencies:** None (independent of U1 and U2). All three doc edits land in the same unit because each is a small, self-contained surgery.

**Files:**
- Modify: `docs/plans/2026-04-28-001-feat-prompt-provenance-v1-plan.md`
- Modify: `docs/dogfooding.md`
- Modify: `docs/follow-ups.md`

**Approach:**
- **v1 plan** (`docs/plans/2026-04-28-001-feat-prompt-provenance-v1-plan.md`):
  - Strike R12 at line 38 by wrapping it in `~~ ~~` and adding a parenthetical rationale, mirroring the R13 line at line 39: `~~R12.~~ *(dropped 2026-05-07 — unshipped per docs/plans/2026-05-07-001-refactor-unship-pr-comment-action-plan.md. The CLI command `prov pr-timeline` stays as a local query surface; the GitHub Action that posted the comment to PRs was removed because automatically-posted review-time comments don't fit the tool's "queryable by users and models" framing. Revival anchor: commit `e1dfbd8`.)*`
  - At U13's heading line (around line 938, `- U13. **GitHub Action (PR intent timeline comment)**`), prepend a new line below the heading that reads `**Status:** Dropped — see `docs/plans/2026-05-07-001-refactor-unship-pr-comment-action-plan.md`. Revival anchor: commit `e1dfbd8`.` Do not edit U13's body. Do not re-number any U-IDs.
  - Edit the R7 parenthetical at line 33. Currently: `prov pr-timeline --base <ref> --head <ref>` (local preview of the GitHub Action's comment)`. After unship the GitHub Action no longer exists, so the parenthetical reads as a stale forward-reference. Rewrite to a CLI-self-referential framing such as `(renders the PR intent timeline locally — see `crates/prov-cli/src/render/timeline.rs`)`. This is a one-line surgical edit; the rest of U5's body that mentions Action consumption stays as historical context per the Status: Dropped pattern.
  - Do not edit the v1 plan's Summary, Honest Positioning, Scope Boundaries, High-Level Technical Design, Implementation Units U5 body, Documentation/Operational notes, or Alternative Approaches sections beyond the R7 parenthetical above. The R12 strike + U13 status note + R7 parenthetical edit is the entire v1-plan touch.
- **Dogfooding** (`docs/dogfooding.md`):
  - Delete the `### GitHub Action — U13` section in its entirety, from its heading at line 494 through the paragraph ending `…verify the comment appears on a test PR.` (just before the next H2, `## 5. Cache / reindex`).
  - Leave the `### PR timeline` section above it (the CLI smoke block) untouched — it covers `prov pr-timeline --markdown` and `--json` directly and remains accurate.
- **Follow-ups** (`docs/follow-ups.md`):
  - Delete the `## U13 — GitHub Action (PR #46)` block in its entirety, from line 79 through the line immediately before the next `---` separator and the `## U15 — `prov backfill` (PR #45)` heading. Both follow-ups (cosign OIDC pin, sigstore-js API sanity-check) reference code that no longer exists.

**Patterns to follow:**
- The existing R13 strike at `docs/plans/2026-04-28-001-feat-prompt-provenance-v1-plan.md:39` is the exact template for the R12 strike — same `~~R12.~~ *(dropped YYYY-MM-DD — …)*` shape.
- The existing `## U3 — Capture pipeline (PR #3)` block in `docs/follow-ups.md` is the structural template for U13-style entries; deleting U13's block in full leaves U3, U8, and U15 surrounding it cleanly.

**Test scenarios:**
- Happy path: `grep -n "R12\." docs/plans/2026-04-28-001-feat-prompt-provenance-v1-plan.md` shows the strike-through line and no live `R12.` requirement bullet.
- Happy path: `grep -n "U13" docs/plans/2026-04-28-001-feat-prompt-provenance-v1-plan.md` returns the `**Status:** Dropped` line (and any in-place mentions in U5/U9 bodies that the synthesis explicitly leaves untouched). The U13 unit heading itself remains, matching U-ID stability.
- Happy path: `grep -n "GitHub Action" docs/dogfooding.md` returns no matches.
- Happy path: `grep -n "U13" docs/follow-ups.md` returns no matches.
- Edge case: the v1 plan's Implementation Units section still has U13's heading and body intact (only the new `**Status:** Dropped` line is added), preserving U-ID stability per the v1 plan's own conventions.

**Verification:**
- A reader scanning the v1 plan's Requirements section sees R12 with a strike-through and a clear pointer to this unship plan.
- A reader scanning `docs/dogfooding.md` finds the CLI-level `### PR timeline` smoke section but no Action smoke section.
- `docs/follow-ups.md` lists U3, U8, and U15 entries with no U13 entry between them.

---

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| Removing the `STICKY_MARKER` cross-language sentinel later masks a real Rust-side rename or typo. | Today, no consumer reads `STICKY_MARKER` — it's emitted as part of the rendered Markdown body and that's it. If/when the Action is revived, the sentinel comes back with it. Tests in `crates/prov-cli/tests/cli_read.rs` already assert the literal `<!-- prov:pr-timeline -->` string in rendered output, so a Rust-side typo would still fail Rust tests. |
| Deleting `action/dist/` removes ~85K LOC and may surprise reviewers who scan the diff stat. | The unship is one of the points of the plan — diff size is expected. The PR description should call out the LOC delta explicitly so it doesn't read as accidental. |
| The v1 plan still describes an Action as part of the v1 design narrative (Summary, High-Level Technical Design, U5/U13 cross-references). A reader landing on the v1 plan without reading this unship plan could think the Action ships. | The R12 strike + U13 `Status: Dropped` line is the canonical signal, mirroring the existing R13/U14 precedent. Deeper rewrites of the v1 plan are out of scope per the synthesis (bookkeeping only). The unship plan itself is the authoritative record. |
| Dropping the third differentiator weakens positioning. The "PR intent timeline as a review artifact" phrasing was load-bearing in the v1 narrative; going from three differentiators to two could read as a quieter retreat. | Honest positioning beats over-claiming. The 2026-05-07 ce-doc-review pass identified that a CLI-anchored rewrite would read as filler — any tool with a markdown renderer can produce a chronological log on demand. The two surviving differentiators (Skill, redactor-by-default-when-shared) stand on their own; reviving the wedge later requires reviving the surface (an Action or equivalent), not just renaming the existing CLI. |
| The two U13 security follow-ups (cosign OIDC pin, sigstore-js API sanity-check) are deleted with the U13 follow-ups block. If the Action is revived later, those concerns must be remembered. | Both follow-ups are reconstructable from the deleted code path itself: the cosign OIDC pin is a `certificateIdentities` parameter on `defaultVerifier`, and the sigstore-js sanity-check is a one-time empirical exercise once a real signed bundle exists. Revival of the Action would re-introduce both as natural pre-merge concerns. The deferred-to-follow-up note in Scope Boundaries records the revival anchor and explicitly names both security concerns so the institutional memory does not depend on revival-time archaeology of deleted code. |
| Revival from `git checkout e1dfbd8 -- action/` could fail silently because the Action shells `prov pr-timeline --base <base> --head <head> --markdown` and parses the rendered Markdown body. Six months of CLI evolution without the cross-language `STICKY_MARKER` sentinel could drift the args, the JSON envelope, or the Markdown shape — and the recovered TS would break on first invocation. | Accept the drift surface — the contract is small (three flags + the `<!-- prov:pr-timeline -->` line + section heading shape), and `crates/prov-cli/tests/cli_read.rs` already pins the marker string + `### Session` heading shape via assertion. Revival should re-run those Rust tests against the recovered TS as a smoke check before merging the revival PR, not blindly trust `git checkout`. |

---

## Documentation / Operational Notes

- **Commit/PR shape.** One PR with three commits matching the three units, or one squashed commit if the user prefers. The PR description should call out: (a) the LOC delta from `action/dist/index.js`, (b) the preserved `prov pr-timeline` CLI command, (c) the revival anchor (`e1dfbd8`).
- **No release coordination needed.** The Action was never tied to a published release (the release workflow doesn't exist yet, per `docs/follow-ups.md`'s open U13 entry). No external consumers can be broken by this removal.
- **Branch naming.** Per the project's CLAUDE.md, work on a feature branch (don't commit to `main`). Suggested name: `refactor/unship-pr-comment-action`.
- **Pre-PR checks.** `./scripts/check.sh` must pass clean. The script itself is being modified in U1, so verify the post-modification version passes — running the pre-modification version is irrelevant.

---

## Deferred / Open Questions

### From 2026-05-07 ce-doc-review

These observations from the multi-persona doc-review pass are advisory: they do not block this unship, but they are worth carrying forward as the next planning surface for Prov touches review-time framing or the `prov pr-timeline` CLI.

- **Orphan-CLI cognitive load.** *(product-lens, anchor 50)* `prov pr-timeline` and the `STICKY_MARKER` constant persist after unship with no obvious consumer. The asserted purpose ("local query surface", "PR-prep aid") is hypothetical — there is no evidence in this plan that any user has asked for a local timeline rendering, and reviewers do not typically run a CLI tool against a local checkout to read a teammate's PR. Worth deciding, in a future planning pass, whether to (a) name a concrete user persona/workflow that justifies the surface, or (b) consider whether `prov pr-timeline` itself should be deleted alongside the Action with `prov log --range` covering the queryable frame more directly.
- **Team-mode positioning still implicit after unship.** *(product-lens, anchor 50)* The v1 design invests heavily in team-mode infrastructure: `prov sync`, the redaction pipeline, the pre-push gate, the cross-machine notes ref — all of which exist *because* prompts are meant to be shared with a team. The Action was the consumption side of that investment. After unship, no PR-adjacent surface consumes the team-mode investment. The plan would be stronger long-term if the README or v1 plan stated explicitly whether team-mode is still a first-class use case or whether Prov is quietly becoming a single-developer tool, so future decisions (about `prov sync`, redaction, PR-adjacent features) have a coherent frame to reference.
- **Revival framing avoids the content question.** *(product-lens, anchor 50)* The "revivable from git history" framing treats this as a default-off vs default-on question, but the Problem Frame's actual diagnosis ("more noise than signal for most users") is a content/value diagnosis. A future revived-as-opt-in Action might solve a real user need or just re-ship the same shape with a config flag. Better framing for any revival: *deferred pending evidence of demand*, not *deferred pending opt-in story*.
- **`No external consumers` claim is unverified.** *(adversarial, anchor 50)* Commit `e1dfbd8` has been on `main` since 2026-05-04 (3 days). GitHub Actions can be referenced by commit SHA, not just by release tag — anyone could pin `mattfogel/prov@e1dfbd8` in their workflow today. The plan asserts confidently that "No external consumers can be broken by this removal" without documented verification. Worth running `gh search code 'mattfogel/prov@'` once before merge, or softening the claim to "no known external consumers" in the PR description.

---

## Sources & References

- **Origin commit:** `e1dfbd8 feat(action): U13 — GitHub Action posts PR intent timeline comment (#46)` — the revival anchor for restoring `action/`.
- **R13/U14 drop precedent:** `docs/plans/2026-04-28-001-feat-prompt-provenance-v1-plan.md:39` — template for the R12 strike.
- **U13 follow-ups block:** `docs/follow-ups.md:79-110` — the deleted block.
- **Dogfooding Action smoke section:** `docs/dogfooding.md:494-531` — the deleted section.
- **CI Action job:** `.github/workflows/ci.yml:47-73` — the deleted job.
- **scripts/check.sh Action + sentinel blocks:** `scripts/check.sh:41-89` — the deleted blocks (line 91, `printf 'All checks passed.'`, stays).
- **Rust renderer (preserved):** `crates/prov-cli/src/render/timeline.rs`, `crates/prov-cli/src/commands/pr_timeline.rs`.
