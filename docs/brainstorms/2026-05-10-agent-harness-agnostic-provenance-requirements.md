---
date: 2026-05-10
topic: agent-harness-agnostic-provenance
---

# Agent-Harness-Agnostic Provenance

## Summary

Prov should reposition from Claude Code provenance to agent-harness-agnostic provenance, with Codex as the first proof beyond Claude. The release should establish a general adapter concept, preserve the existing Claude path, and make agent hook installation explicit and trustworthy.

---

## Problem Frame

Prov currently presents and implements itself as a Claude Code-oriented tool: the README describes Claude-driven edits, the installer wires Claude settings, and user-facing surfaces emphasize the Claude Code Skill. That creates a positioning ceiling. If Prov is meant to explain why AI-written code exists, the durable category is agent provenance, not one harness's provenance.

The immediate risk is overcorrecting by trying to support every coding agent at once. Codex, Cursor, and Pi all have plausible integration surfaces, but they differ in maturity and shape. A first expansion should prove the abstraction with one credible non-Claude adapter before taking on less certain harness behavior.

---

## Actors

- A1. Prov user: A developer who wants provenance captured for the agent harnesses they actually use in a repo.
- A2. Downstream coding agent: An agent that later queries Prov to recover prior intent before modifying AI-written code.
- A3. Prov maintainer: The person maintaining adapter quality, install safety, and product positioning.
- A4. Agent harness: Claude Code, Codex, or a future supported coding environment that can surface prompt and edit lifecycle events.

---

## Key Flows

- F1. Per-repo adapter setup
  - **Trigger:** A developer installs Prov in a repository.
  - **Actors:** A1, A4
  - **Steps:** The developer runs the trusted per-repo setup path, chooses which agent adapters to enable, and sees what categories of hooks/configuration will be modified before setup completes.
  - **Outcome:** Git provenance hooks are installed and only the selected agent adapter hooks are wired.
  - **Covered by:** R1, R2, R5, R6

- F2. Codex provenance capture
  - **Trigger:** A developer uses Codex to make changes in a Prov-enabled repo.
  - **Actors:** A1, A4
  - **Steps:** Prov receives Codex prompt/edit lifecycle events, normalizes them into the shared provenance model, and associates the captured context with the eventual commit.
  - **Outcome:** `prov log` can explain Codex-authored lines with the same product semantics as Claude-authored lines.
  - **Covered by:** R3, R4, R8

- F3. Future adapter evaluation
  - **Trigger:** A maintainer considers adding Cursor, Pi, or another harness.
  - **Actors:** A3, A4
  - **Steps:** The maintainer evaluates whether the harness can provide enough prompt, edit, session, and tool-call context to meet Prov's capture standard.
  - **Outcome:** Future adapters are accepted, deferred, or rejected against a consistent bar instead of one-off enthusiasm.
  - **Covered by:** R7, R9, R10

---

## Requirements

**Product Positioning**
- R1. Prov's public language must describe the product as agent-harness-agnostic provenance, not as Claude Code provenance with add-ons.
- R2. Claude Code support must remain a first-class supported adapter during the repositioning.

**Codex Adapter Proof**
- R3. The first non-Claude adapter must be Codex, and the release should treat Codex quality as the proof that Prov can support more than one harness.
- R4. Codex-captured provenance must be queryable through the same user-facing Prov surfaces as existing Claude-captured provenance.
- R5. The Codex adapter must meet the same privacy and non-blocking capture posture as the existing Claude hook path: capture failures should not break the agent loop, and prompt-like content should go through Prov's redaction posture before durable storage or sharing.

**Installation**
- R6. Per-repo installation must let users explicitly choose which agent adapter hooks to install.
- R7. The binary installer must not mutate repository hooks or agent configuration as part of a remote shell install path; repo and agent wiring belongs to the trusted per-repo setup command.
- R8. The install experience must make clear what was installed, what was skipped, and how to add or remove an adapter later.

**Adapter Model**
- R9. Prov must define a harness adapter concept with a consistent product contract: identify the harness, capture prompt/session/edit context when available, normalize it into Prov provenance, and degrade visibly when a harness cannot provide enough context.
- R10. Future harnesses such as Cursor and Pi must be positioned as future adapters until their capture reliability and integration shape are validated against the adapter contract.

---

## Acceptance Examples

- AE1. **Covers R3, R4.** Given a repo with Prov and the Codex adapter enabled, when Codex makes an edit that is committed, `prov log` can identify the originating Codex prompt context rather than treating the line as untracked AI work.
- AE2. **Covers R6, R8.** Given a developer runs per-repo setup in an environment with Claude and Codex available, when they choose only Codex, Prov wires Codex capture, leaves Claude capture untouched, and reports that choice clearly.
- AE3. **Covers R7.** Given a developer installs the Prov binary through the remote installer, when the installer completes, it has not modified repo-local git hooks or agent-harness config files.
- AE4. **Covers R9, R10.** Given a future harness has lifecycle hooks but cannot reliably expose edit context, when it is evaluated, Prov can defer the adapter or ship it with visibly limited capture rather than pretending it has full parity.

---

## Success Criteria

- A new reader understands Prov as a tool for AI coding provenance across harnesses, with Claude and Codex as concrete supported examples rather than Claude as the product identity.
- A developer can install Prov safely and predictably without surprise modifications to unrelated agent tools.
- Codex support is good enough that future planning can reuse the adapter framing rather than inventing a separate integration model per harness.
- Downstream planning has enough scope clarity to decide exact command flags, config mutation strategy, and payload normalization without inventing product behavior.

---

## Scope Boundaries

- Cursor support is deferred until its local, CLI, and cloud hook behavior is validated against Prov's adapter contract.
- Pi support is deferred because it appears to require an extension-style integration rather than the same hook-config shape as Claude or Codex.
- Automatic installation of every detected agent adapter is out of scope for the first non-Claude release.
- A remote installer wizard that modifies repo hooks and agent configuration is out of scope.
- A hosted service, telemetry layer, or account-based adapter registry is out of scope.
- Perfect parity across all future harnesses is out of scope; the release needs one strong second adapter and a clear standard for future ones.

---

## Key Decisions

- Codex first: Codex is the first non-Claude adapter because its hook model is close enough to prove harness independence without expanding the launch to multiple uncertain integration shapes.
- Explicit adapter selection: Users should choose agent adapters during per-repo setup so Prov does not surprise them by wiring tools they did not intend to use.
- Binary install stays narrow: The remote installer should install the Prov binary only; repo and agent configuration changes happen through the local setup command.
- Future adapters need a bar: Cursor and Pi should be discussed as roadmap candidates, but adapter inclusion depends on whether they can satisfy Prov's capture semantics reliably.

---

## Dependencies / Assumptions

- Codex hooks can provide enough prompt, tool-use, and lifecycle context to meet Prov's minimum capture standard.
- Existing Claude capture remains the baseline behavior during the repositioning.
- Users are more likely to trust explicit per-repo setup than a remote installer that changes multiple tool configs at once.
- The documentation and schema language can be generalized without breaking existing Claude-oriented data already produced by Prov.

---

## Outstanding Questions

### Deferred to Planning

- [Affects R4, R9][Technical] What is the exact normalized event model needed so Claude and Codex captures produce equivalent user-facing provenance?
- [Affects R6, R8][Technical] What command interface gives users the clearest adapter install, uninstall, and status experience?
- [Affects R9, R10][Needs research] What minimum evidence should be required before declaring a future harness adapter production-ready?
