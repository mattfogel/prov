---
title: Agent harness adapter readiness conventions
date: 2026-05-10
category: conventions
module: prov-cli
problem_type: convention
component: capture
severity: medium
applies_when:
  - adding a new agent harness adapter
  - changing hook payload parsing
  - evaluating Cursor, Pi, or another future coding agent
tags:
  - agent-harnesses
  - hooks
  - capture
  - privacy
  - install
  - rust
related_components:
  - development_workflow
---

# Agent harness adapter readiness conventions

## Context

Prov should be agent-harness-agnostic, but every harness has different hook
lifecycle guarantees, config trust rules, and payload shapes. Adding a harness
too early can create silent provenance gaps that look like successful capture.

Codex is the first non-Claude adapter. The bar below is the minimum evidence
needed before declaring another harness supported.

## Guidance

### 1. Prove lifecycle coverage before writing installer code

An adapter needs prompt, session/model, edit, and stop/turn-boundary coverage.
If any of those are missing, document the degraded behavior in the plan and
CLI output before implementation starts.

### 2. Pin payloads with fixtures

Every adapter must land fixture JSON for the supported lifecycle events and at
least one end-to-end capture test that writes a normal note readable by
`prov log` without a harness-specific read path.

### 3. Keep privacy and failure behavior shared

Adapter parsing should normalize into the existing staging model before
redaction, private routing, post-commit matching, and cache updates. Hook
runtime failures must log and exit success so a Prov bug does not block the
agent loop.

### 4. Make installation explicit and reversible

Agent config should only be written when the user asks for that adapter.
Install must preserve unrelated user config, be idempotent, and uninstall must
remove Prov-owned entries without deleting user-owned hooks or settings.

### 5. Do not overclaim future harnesses

Cursor, Pi, and other candidates stay in evaluation language until they satisfy
the same fixture, privacy, non-blocking, install, and read-surface parity checks
as Claude Code and Codex.
