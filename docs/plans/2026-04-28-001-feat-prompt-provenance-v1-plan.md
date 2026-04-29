---
title: "feat: Prov v1 — Prompt Provenance for Claude Code"
type: feat
status: active
date: 2026-04-28
deepened: 2026-04-28
---

# feat: Prov v1 — Prompt Provenance for Claude Code

## Summary

Build Prov v1 as a Rust CLI plus a Claude Code Plugin (bundling hooks and a Skill) plus a thin TypeScript GitHub Action wrapper. The capture pipeline uses Claude Code hooks to stage prompt-and-edit metadata and a `post-commit` git hook to flush it into a JSON note attached to the right commit SHA under `refs/notes/prompts`, with a SQLite cache at `.git/prov.db` for sub-50ms reads. Privacy is defense-in-depth: a write-time redactor with built-in secret detectors plus `.provignore`, an explicit `# prov:private` opt-out, and a pre-push gate. The signature surface is the Skill, which teaches Claude Code to query its own prior reasoning before substantive edits; the GitHub Action renders a single per-session "PR intent timeline" comment that walks the conversation chronologically. Delivered in three phases: solo provenance (single-dev local capture and read), team provenance (privacy hardening, sync, history-rewrite handling), then agent and review surfaces.

---

## Problem Frame

Code is increasingly written by AI, but the systems that explain code (git blame, commit messages, code comments) assume the person whose name is on the change is also the person who reasoned about it. With AI-generated code that assumption is broken: the reasoning lives in a chat conversation that is, weeks later, effectively unrecoverable. This bites four moments of pain — code review (reviewers reviewing the artifact without intent), incident response ("you wrote this 3 weeks ago" — but you don't remember why), refactoring (Chesterton's fences accumulate at AI velocities), and AI agents working on AI-written code (no continuity across sessions). The original brief framing is preserved; this plan covers a v1 implementation against that frame.

**Honest positioning vs prior art.** [Git AI](https://usegitai.com/) shipped v1.0 in late 2025 with a near-identical core architecture: per-line AI authorship in git notes, SQLite cache, rewrite preservation, multi-agent attribution. Prov is **not** novel on storage — claiming so would be dishonest. Real differentiators that hold up: (a) push-by-default with redaction (Git AI keeps transcripts local), (b) agent-first via the Claude Code Skill (no equivalent surface in Git AI), (c) the PR intent timeline as a review-time artifact rather than per-line annotations. Position Prov as the "agent-native + team-shareable" entry, and treat Git AI interop as an open question for v1.1.

---

## Requirements

- R1. Capture every prompt and the resulting file edits made by Claude Code in a session, keyed by `session_id` and `tool_use_id`.
- R2. Attach captured metadata to the eventual commit SHA via a `post-commit` git hook, surviving rebase/amend/squash/cherry-pick.
- R3. Resolver returns the originating prompt (with drift state) for any `(file, line)` in under 50ms on a warm SQLite cache.
- R4. Redact known-secret patterns (AWS, Stripe, GitHub PAT, JWT, email, high-entropy strings, project-specific `.provignore` regexes) at write time, before the note is created.
- R5. Mark prompts private via inline `# prov:private` magic phrase or retroactive `prov mark-private <commit>`. Private notes never push.
- R6. Pre-push gate scans notes being pushed for known-secret patterns and blocks the push (overridable with `--no-verify`).
- R7. Read CLI: `prov log <file>[:<line>]`, `prov log <file> --history`, `prov log <file>:<line> --full`, `prov search <query>`, `prov reindex`, `prov pr-timeline --base <ref> --head <ref>` (local preview of the GitHub Action's comment).
- R8. Sync CLI: `prov fetch`, `prov push`, `prov install` (configures git refspecs, `notes.displayRef`, `notes.rewriteRef`, registers hooks), `prov uninstall`.
- R9. History/rewrite CLI: `prov repair` (walks reflog, reattaches orphaned notes), `prov gc` (culls notes whose target commit no longer exists), `prov notes resolve` (manual JSON-aware merge for the notes ref).
- R10. Privacy CLI: `prov mark-private <commit>`, `prov redact-history <pattern>` (rewrite the notes ref to retroactively scrub a newly discovered secret format).
- R11. Claude Code Skill at `skills/prov/SKILL.md` teaches the agent to query `prov log` before substantive edits to files with existing provenance; surfaces relevant prior prompts in its planning.
- R12. GitHub Action posts one "PR intent timeline" comment per session per PR, edits in place on PR updates, organizes turns chronologically, collapses superseded turns, drops turns whose edits did not survive into the final diff.
- R13. `prov regenerate <file>:<line> --model <name>` replays the original prompt against a chosen model and renders a diff against the stored `original_blob_sha`.
- R14. `prov backfill` reads stored Claude Code session logs (`~/.claude/projects/<sanitized-cwd>/<session-uuid>.jsonl` — sanitized-cwd replaces `/` with `-`, leading `-` preserved) and creates approximate notes for historical commits via fuzzy matching, marking each as `(approximate)`.
- R15. Distribution: single static binary via `cargo install`, Homebrew tap (`brew install matt/tap/prov`), and `curl | sh` script. macOS unsigned for v1; document `xattr` workaround.
- R16. No telemetry, no hosted services, no analytics. Apache 2.0 licensed.

---

## Scope Boundaries

- Provenance for non-Claude-Code tools (Cursor, Aider, Copilot, generic SDK wrappers). The schema is tool-agnostic so they can be added later.
- IDE plugins, LSP servers, hover popovers in editors.
- Real-time collaboration on prompts (live shared sessions).
- AI agent decision logs beyond code edits (the agent's "thinking" / reasoning trace).
- Provenance for human-written code.
- Cost or token tracking.
- Hosted dashboard, web UI, or any SaaS component.
- Telemetry, analytics, or usage tracking of any kind.

### Deferred to Follow-Up Work

- **Custom git notes merge driver** (v1.1): v1 ships with `notes.mergeStrategy=manual` and `prov notes resolve` for JSON-aware union; a registered merge driver that runs automatically on `git fetch` is a v1.1 polish.
- **Sidecar-repo storage prototype** (v1.x): the brief flagged a notes-vs-sidecar comparison as worth prototyping; v1 commits to notes only. Revisit if notes UX rough edges prove blocking.
- **Automated/gated Skill triggering** (v1.x): v1 enforces "only on substantive edits to files with existing provenance" through Skill prose. A real triggering policy (e.g., file globs in `paths:`, edit-size thresholds) is a v1.x iteration informed by user feedback.
- **Per-conversation grouping in the GitHub Action** (v1.x): v1 groups by `session_id`. Multi-conversation grouping (multiple PRs from the same conversation, or multiple authors) is deferred.
- **Backfill quality threshold tuning** (v1.x): v1 surfaces every backfilled note with an `(approximate)` marker. Confidence scoring and threshold cutoffs come after real-world feedback.
- **Git AI notes-format interop** (v1.1): if Git AI's schema is stable enough, Prov could read its notes for cross-tool continuity. Out of v1 scope — needs a real interop conversation with that project first.
- **Notarized macOS binaries** (v1.1): v1 ships unsigned with `xattr` workaround documented. Add notarization if user friction is real.

---

## Context & Research

### Relevant Code and Patterns

Greenfield repo at `prov/` (currently empty). No prior code, no learnings doc to mine.

**Local conventions observed in `~/.claude/`:**

- The user's `~/.claude/settings.json` has zero `hooks` block today — no user-scope hook collisions to worry about.
- Plugin precedent (sampled across 9 enabled plugins) uses `python3 ${CLAUDE_PLUGIN_ROOT}/hooks/<name>.py` with explicit `timeout` and `matcher`. Prov departs from this by invoking the Rust binary directly — defensible because the binary IS the tool, but worth documenting in the README so users understand why a Python shim isn't used.
- SKILL.md frontmatter the harness expects: `name`, `description` (long, trigger-rich prose — this *is* the trigger mechanism), optional `argument-hint`, optional `paths` (glob filter for auto-activation). No required `version` or `triggers`.
- The user's global `CLAUDE.md` mandates Conventional Commits and feature-branch workflow (no commits to `main`). Prov's own dev workflow follows these.
- No prior published Rust CLI in `~/Documents/GitHub/`, so v1 picks `cargo-dist` + `release-plz` fresh without contradicting precedent.

### Institutional Learnings

None — `prov/docs/solutions/` does not exist yet. As v1 ships, capture lessons under `prov/docs/solutions/` per the compound-engineering convention.

### External References

- **Claude Code hooks reference**: <https://code.claude.com/docs/en/hooks> and <https://code.claude.com/docs/en/hooks.md> — payload schemas, registration, scoping.
- **Claude Code skills reference**: <https://code.claude.com/docs/en/skills> — SKILL.md frontmatter, discovery, auto-activation via `paths:`.
- **Claude Code plugins reference**: <https://code.claude.com/docs/en/plugins> — `.claude-plugin/plugin.json` shape, `hooks/hooks.json`, `skills/<name>/SKILL.md`, marketplace install.
- **Git AI (prior art)**: <https://usegitai.com/>, <https://github.com/git-ai-project/git-ai>, <https://usegitai.com/docs/how-git-ai-works> — the existing leader in this space.
- **Git notes operational guide**: <https://git-scm.com/docs/git-notes>, <https://tylercipriani.com/blog/2022/11/19/git-notes-gits-coolest-most-unloved-feature/>, <https://www.codestudy.net/blog/git-how-to-push-messages-added-by-git-notes-to-the-central-git-server/>.
- **post-rewrite hook reference**: <https://git-scm.com/docs/githooks>.
- **Notes merge driver precedent**: <https://github.com/Praqma/git-merge-driver>.
- **Rust release automation in 2026**: <https://blog.orhun.dev/automated-rust-releases/>, <https://crates.io/crates/cargo-dist>, <https://crates.io/crates/release-plz>.
- **macOS CLI signing for OSS**: <https://tuist.dev/blog/2024/12/31/signing-macos-clis>, <https://crates.io/crates/apple-codesign>.
- **gitoxide gaps (why we shell out to git)**: <https://github.com/GitoxideLabs/gitoxide> — hooks, push, full merge not yet implemented.
- **musl static linking for Linux portability**: <https://github.com/rust-cross/rust-musl-cross>.
- **AI authorship academic context** (orthogonal but relevant framing): <https://arxiv.org/html/2601.17406v1>.

---

## Key Technical Decisions

- **Implementation language: Rust.** Sub-10ms binary cold start matters for hooks that fire on every prompt and edit; static linking with `musl` (Linux) and `rustls` (TLS) gives a single dependency-free binary across platforms. Go would also work; Node/TS would be unacceptable for hook latency.
- **Repo layout: Cargo workspace** with `crates/prov-core` (schema, resolver, storage, redactor, git wrapper), `crates/prov-cli` (binary entrypoint, commands, hook subcommand), plus `plugin/` (Claude Code plugin assets), `action/` (TypeScript GitHub Action wrapper), and `githooks/` (git hook scripts that shell to `prov hook ...`).
- **Distribution shape: Claude Code Plugin** (`.claude-plugin/plugin.json` + `hooks/hooks.json` + `skills/prov/SKILL.md`). `prov install` either points the user at the marketplace install (preferred) or writes a project-scope `.claude/settings.json` referencing the binary on PATH (escape hatch for users who don't want plugin coupling).
- **Hook invocation: direct binary**, not Python shim. Hook commands are `prov hook user-prompt-submit`, `prov hook post-tool-use`, `prov hook stop`. Plugin install ensures the binary is on PATH; project-scope install validates PATH at `prov install` time.
- **Git library: shell out to `git`**, not gitoxide or git2-rs. Rationale: hooks already run in an environment with the user's full git config and credential helpers; gitoxide explicitly lacks hook/push/merge coverage as of 2026; git2-rs adds a C dependency that complicates static musl builds. `prov-core::git` wraps `Command::new("git")` with a typed interface.
- **Notes ref namespace: `refs/notes/prompts`** (scoped, not `refs/notes/*`). Avoids pulling Gerrit / GitLab CI notes via wildcard refspec.
- **Notes auto-fetch / auto-push: tracking-ref refspec via `--add`** during `prov install` (`git config --add remote.<name>.fetch 'refs/notes/prompts:refs/notes/origin/prompts'`). The non-forced refspec fetches the remote into a tracking ref (`refs/notes/origin/prompts`), and `prov fetch` then runs `git notes merge` against the local `refs/notes/prompts`. **Why:** a forced (`+`) refspec into the local notes ref would silently overwrite locally-staged-but-unpushed notes on every `git fetch`, causing data loss. Auto-push is handled via the `pre-push` hook to avoid clobbering `push.default=simple`.
- **Notes merge strategy v1: `manual`** with `prov notes resolve` for JSON-aware union (key by `commit + session_id + edit timestamp`, deduplicate). Custom merge driver deferred to v1.1 because none of git's built-in strategies (`union`, `cat_sort_uniq`, `ours`, `theirs`) handle structured JSON correctly.
- **Rewrite handling owns all paths:** `notes.rewrite.amend = false` and `notes.rewrite.rebase = false` are set by `prov install`. This disables git's built-in note copying (which would `concatenate` JSON note bodies into invalid blobs on squash) and makes the `post-rewrite` hook (U9) the single writer for amend/rebase/squash. Cherry-pick is still handled via `post-commit` + `CHERRY_PICK_HEAD` since `post-rewrite` does not fire on cherry-pick. The post-commit handler reads `CHERRY_PICK_HEAD`, copies the source commit's note, and adjusts `derived_from` to point back to the original.
- **Schema versioning: explicit `version: 1` field**, with refusal-to-read on unknown future versions. Migration strategy lives in `prov-core::schema::migrate`; v1 is a no-op.
- **PR intent timeline rendering: single sticky comment per PR**, identified by hidden HTML marker (`<!-- prov:pr-timeline -->`), edited in place on every Action run. Multi-session PRs render as multiple `## Session N` sections within one comment. See High-Level Technical Design for the rendering shape.
- **Skill auto-activation: `paths:` globs** in SKILL.md. Activate when the file being edited matches a glob the user can configure (default: `**/*` once `.git/prov.db` exists in the repo). The Skill body explicitly instructs the agent to skip provenance lookups for trivial edits — runtime gating is prose-only in v1.
- **Test strategy: integration-first**. Fixture git repos under `crates/prov-core/tests/fixtures/`, golden-file tests for the redactor, end-to-end tests that simulate full Claude Code sessions by invoking hooks with synthesized JSON payloads. Unit tests for individual secret detectors and JSON schema serialization.
- **Release automation: `release-plz` + `cargo-dist`**. Conventional commits drive auto-version-bump PRs (`release-plz`); merging the PR tags and triggers `cargo-dist` to build platform tarballs, generate the Homebrew formula in a separate tap repo, and attach a `curl | sh` installer.
- **Git hook scope: project-local** under `.git/hooks/` (installed by `prov install` from `githooks/` templates). Respects `core.hooksPath` if set; no global git hook installation.
- **Staging directory: `.git/prov-staging/`** holds in-flight session state between hook invocations and post-commit flush. Per-session JSONL files keyed by `session_id`. Garbage collected after N days (default 14) by `prov gc`.

---

## Open Questions

### Resolved During Planning

- **Plugin vs project-scope skill** (open question 1 from the brief, expanded by research): Ship as a Claude Code Plugin (preferred installable bundle pattern per the docs); project-scope install is the escape hatch. Resolved.
- **GitHub Action comment volume** (open question 4): Single per-session sticky comment with PR intent timeline framing (user-confirmed during synthesis). Resolved.
- **Naming**: Commit to "prov" as the binary and crate name. Verify `crates.io`, `npm` (for the Action wrapper), and Homebrew availability in U1. If `prov` is taken on `crates.io`, fall back to `prov-cli` for the binary crate while keeping `prov` as the binary name.
- **Notes merge approach**: `manual` + `prov notes resolve` in v1; custom merge driver deferred. Resolved.

### Deferred to Implementation

- **`tool_response.structuredPatch` exact format**: Anthropic docs show `"..."` placeholder. U3 needs empirical inspection of the JSON shape against a live Claude Code session to lock the parser. Risk: if the format is unstable across Claude Code versions, the parser needs version detection.
- **`tool_use_id` reliability**: Documented as a common field on tool events but not formally enumerated as guaranteed. Verify presence on every PostToolUse payload during U3; fall back to `(session_id, turn_index, edit_index)` correlation if absent.
- **Where Claude Code session transcripts actually live on disk** for `prov backfill`: `transcript_path` is on every hook payload (so it's discoverable at capture time), but the standalone path conventions for *historical* transcripts are not formally documented. U15 needs to inspect the user's `~/.claude/projects/<project-id>/` layout empirically.
- **Backfill match confidence threshold** (open question 2 from the brief): All matches surface with `(approximate)` marker in v1; threshold tuning happens after real-world feedback. Implementation simply records the highest-confidence match per commit.
- **Skill triggering precision** (open question 3 from the brief): Prose-only gating in v1. Real triggering policy emerges from user feedback in v1.x.
- **Action — handling PRs with multiple Claude Code conversations from different authors**: v1 groups strictly by `session_id`. Cross-author multi-conversation rendering is deferred.
- **Anthropic API key handling for `prov regenerate`**: Use `ANTHROPIC_API_KEY` env var; fail explicitly if absent. Storing keys, key rotation, and rate-limit backoff are deferred to v1.x.
- **Git AI notes-format interop**: Worth a real conversation with that project before committing. v1 defines its own schema; v1.1 may add a reader.
- **Compaction at 90 days**: brief proposes dropping `preceding_turns_summary` for old notes. Implementation as a `prov gc --compact` flag, default off in v1; turn on once storage growth is observed in real use.

### Deferred from Document Review (2026-04-28)

These were surfaced by the post-write document review and deferred for explicit user judgment rather than auto-resolved into the plan body. Each describes a strategic or product question that would change the plan's identity or scope, not a mechanical fix.

**Resolved at ce-work entry (2026-04-28):**
- **Scope:** All 15 units stay in v1. (User affirmed the current plan over the trim-to-13/12 alternatives.)
- **Push posture: INVERTED to local-only by default.** Notes do NOT push automatically. `prov install` does NOT configure a remote-side fetch refspec, does NOT install the pre-push gate as a global hook, and does NOT auto-push on `git push`. `prov push <remote>` becomes an explicit opt-in command that the user runs deliberately. `prov install --enable-push <remote>` (or post-install `prov sync enable <remote>`) is what configures the refspec, registers the pre-push gate, and turns on team mode. The redactor still runs at write-time (defense in depth — local notes should still be scrubbed in case the user opts in later or pushes ad-hoc). The pre-push gate still runs whenever `prov push` is invoked or when push is enabled. **Implementer note:** when implementing U5/U7/U8, treat the existing approach sections as describing "team-mode behavior" — the install steps that wire push happen only on `--enable-push` or `prov sync enable`. Update README positioning to remove "push-by-default with redaction" from the differentiators list; the wedge becomes "agent-first via Skill + PR intent timeline + redactor-by-default-when-shared" (a softer claim that doesn't require betting the product on a single false negative).
- **Skill triggering:** Prose-only gating with the narrowed `paths:` default and `--only-if-substantial` CLI flag stays as-is. Real policy in v1.x.

- **Push-by-default — invert or defend?** The single most differentiated wedge claim ("push-by-default with redaction") is also the highest-rated risk in the Risks table (a single redactor false-negative leaks secrets to a remote and ends project credibility). Git AI's local-only posture exists precisely because that bet is hard to win. Resolve by either (a) gathering concrete user evidence that team-shared notes drive v1 adoption, or (b) inverting the default — ship local-only in v1, make `prov push` opt-in per-repo, treat shared notes as a v1.x feature once the redactor has a regression corpus from real use. Either resolution is defensible; the current plan picks (a) without naming the evidence. *(product-lens, P1)*

- **Agent-first wedge depends on deferred Skill triggering policy.** R11 / U12 names the Skill as one of three claimed differentiators, but its triggering is "prose-only in v1" with policy deferred to v1.x. If the agent over-queries (latency/noise on every edit) or under-queries (silent failure), the wedge collapses — and v1 ships with no automated way to know which is happening. Decide whether to (a) move basic policy gating (file globs + edit-size threshold beyond what was applied here) into v1 so trigger behavior is testable, or (b) downgrade the Skill from "real differentiator" to "experimental surface" in the README until v1.x lands real triggering. *(product-lens, P1)*

- **Scope vs maintained-as-time-permits posture.** README footer commits to "Not a product. Maintained as time permits." but v1 ships 15 implementation units (Rust workspace, Claude Code plugin, TS GitHub Action, custom JSON-aware notes merger, Anthropic API client, transcript fuzzy-matching backfill, history-rewrite handling). Adopters who trust the posture will route around the tool the first time a Claude Code schema change breaks capture and the fix takes weeks. Decide whether to (a) reduce v1 scope (defer `prov regenerate`, `prov backfill`, the GitHub Action, and `prov redact-history` to post-v1) or (b) reframe the posture honestly as "serious tool, serious maintenance." *(product-lens, P1)*

- **5-minute first-run happy path.** First-time adoption requires installing the binary + running `prov install` + installing the Plugin + enabling the GitHub Action + learning `.provignore`. Define an explicit first-run acceptance criterion: after `cargo install prov && prov install` in a Claude-Code-configured repo, the user should see provenance from their next session within 60 seconds and one git commit, with no plugin/Action/`.provignore` setup required. Treat plugin and Action as opt-in upgrades, not prerequisites. If the current architecture can't deliver that path, the architecture needs a rethink. *(product-lens, P2)*

- **PR intent timeline value to reviewers — behavioral acceptance criterion.** The PR timeline is one of three claimed differentiators. Plan's only verification is "visually confirm the comment." Define a behavioral acceptance criterion before declaring the Action shipped: in N real PRs reviewed by M reviewers who didn't write the code, ask whether the timeline comment changed their review (caught a missed context, surfaced a constraint, or was ignored). If ignored in >half of cases, redesign the surface (in-line annotations? expandable in the diff view?) before doubling down. *(product-lens, P2)*

- **`prov regenerate` as v1 vs v1.x.** Adds an Anthropic API client (HTTP, auth, rate limits, retry, model deprecations) to a tool whose stated posture is "no hosted services, no network requests." User value is narrow (re-run a stored prompt against a different model and diff the output); maintenance surface is wide. For a one-maintainer time-permits OSS project, defer U14 to v1.x as a separate companion crate or community-maintained extension? The core "capture, resolve, share, redact" value proposition does not depend on it. *(product-lens, P3 — but interacts with the scope-vs-posture question above)*

- **`prov backfill` against undocumented transcript format — defer to v1.1?** U15 builds a parser for a transcript JSONL format that is not formally documented and requires "empirical inspection." If the format changes between Claude Code versions, backfill silently produces zero matches with no user-visible error. Defer U15 to v1.1 with the explicit condition "ship once the transcript path convention is confirmed via empirical inspection and documented"? Interim `prov backfill` stub can print "Run `prov hook user-prompt-submit` once to discover your transcript path, then file an issue with the format — backfill will be calibrated against real data." *(scope-guardian, P2 — interacts with the regenerate question and the scope-vs-posture question)*

---

## Output Structure

The repo layout `prov install` and the user expect to see after v1 ships:

    prov/
    ├── Cargo.toml                          # workspace manifest
    ├── README.md                           # marketing + install + posture
    ├── LICENSE-APACHE
    ├── LICENSE-MIT                         # dual-licensed for downstream comfort
    ├── crates/
    │   ├── prov-core/                      # library: schema, resolver, storage, redactor, git wrapper
    │   │   ├── Cargo.toml
    │   │   ├── src/
    │   │   │   ├── lib.rs
    │   │   │   ├── schema.rs               # version 1 JSON shape + serde types
    │   │   │   ├── storage/
    │   │   │   │   ├── mod.rs
    │   │   │   │   ├── notes.rs            # read/write notes via `git notes`
    │   │   │   │   ├── sqlite.rs           # cache schema + population
    │   │   │   │   └── staging.rs          # .git/prov-staging/ helpers
    │   │   │   ├── resolver.rs             # blame → note → range → hash
    │   │   │   ├── redactor/
    │   │   │   │   ├── mod.rs
    │   │   │   │   ├── detectors.rs        # AWS, Stripe, GH PAT, JWT, email, entropy
    │   │   │   │   └── provignore.rs       # .provignore parser
    │   │   │   ├── git.rs                  # typed wrapper around shell-to-git
    │   │   │   └── session.rs              # session_id + turn_index types
    │   │   └── tests/
    │   │       ├── fixtures/               # fixture git repos
    │   │       ├── resolver.rs
    │   │       ├── redactor_golden.rs
    │   │       └── end_to_end.rs
    │   └── prov-cli/                       # binary
    │       ├── Cargo.toml
    │       ├── src/
    │       │   ├── main.rs
    │       │   ├── commands/
    │       │   │   ├── log.rs
    │       │   │   ├── search.rs
    │       │   │   ├── install.rs
    │       │   │   ├── uninstall.rs
    │       │   │   ├── reindex.rs
    │       │   │   ├── repair.rs
    │       │   │   ├── gc.rs
    │       │   │   ├── fetch.rs
    │       │   │   ├── push.rs
    │       │   │   ├── notes_resolve.rs
    │       │   │   ├── mark_private.rs
    │       │   │   ├── redact_history.rs
    │       │   │   ├── regenerate.rs
    │       │   │   ├── backfill.rs
    │       │   │   ├── pr_timeline.rs      # local preview of the GitHub Action's comment
    │       │   │   └── hook.rs             # `prov hook <event>` dispatch
    │       │   └── render/
    │       │       ├── mod.rs
    │       │       └── timeline.rs         # shared with the GitHub Action
    │       └── tests/
    │           └── cli_smoke.rs
    ├── plugin/                             # Claude Code plugin bundle
    │   ├── .claude-plugin/
    │   │   └── plugin.json
    │   ├── hooks/
    │   │   └── hooks.json                  # registers UserPromptSubmit, PostToolUse, Stop
    │   └── skills/
    │       └── prov/
    │           ├── SKILL.md
    │           └── references/
    │               ├── querying.md         # how the agent uses `prov log`
    │               └── triggers.md         # what counts as substantive
    ├── action/                             # GitHub Action wrapper
    │   ├── action.yml
    │   ├── package.json
    │   ├── tsconfig.json
    │   ├── src/
    │   │   ├── index.ts                    # entrypoint
    │   │   ├── download.ts                 # fetch prov binary from GH Releases
    │   │   ├── timeline.ts                 # render PR intent timeline
    │   │   └── github.ts                   # sticky-comment upsert via Octokit
    │   └── dist/                           # ncc-bundled output, committed
    ├── githooks/                           # templates installed into .git/hooks
    │   ├── post-commit
    │   ├── post-rewrite
    │   └── pre-push
    ├── install.sh                          # curl|sh installer (downloads binary + verifies via Sigstore cosign)
    ├── docs/
    │   ├── plans/                          # this directory
    │   └── solutions/                      # filled as we ship and learn
    └── .github/
        └── workflows/
            ├── ci.yml                      # cargo test + ncc build + lint
            ├── release-plz.yml             # auto-version-bump PR
            └── release.yml                 # cargo-dist on tag

This is a scope declaration of expected output shape. The implementer may adjust the layout if implementation reveals a better structure; the per-unit `**Files:**` lists below remain authoritative.

---

## High-Level Technical Design

> *This section illustrates the intended approach as directional guidance for review, not implementation specification. Pseudo-code and sketches are for design validation; implementing units should treat them as context, not code to reproduce.*

### Capture pipeline

The capture pipeline spans Claude Code's hook lifecycle and a git hook. The crux is that staged edits accumulate across hook invocations and only flush to a note when a commit happens — and the post-commit handler must figure out which staged edits belong to which commit.

```
Claude Code session lifecycle                Git lifecycle
─────────────────────────────                ─────────────
UserPromptSubmit (turn N)                    .
  → write turn metadata to                   .
    .git/prov-staging/<sid>/turn-N.json      .
                                             .
PostToolUse(Edit|Write|MultiEdit)            .
  → append edit record to                    .
    .git/prov-staging/<sid>/edits.jsonl      .
    (file, line_range, before, after,        .
     content_hashes, tool_use_id)            .
                                             .
Stop (turn N done)                           .
  → mark turn-N complete in staging          .
                                             .
... (more turns, more edits) ...             .
                                             .
                                             User runs git commit
                                             ────────────────────
                                             post-commit fires:
                                              1. Diff HEAD~1..HEAD
                                                 (or full tree for initial)
                                              2. Walk all staging dirs
                                              3. For each staged edit,
                                                 check if its post-edit
                                                 content hash appears in
                                                 the diff's added lines.
                                                 Match → include in note.
                                                 No match → leave staged
                                                 (defer to future commit).
                                              4. Write note JSON to
                                                 refs/notes/prompts at HEAD
                                              5. Mark matched staging
                                                 entries as committed.
```

Edits not matched after `gc.staging_ttl_days` (default 14) are pruned by `prov gc` — they represent abandoned work or external edits that bypassed Claude Code.

### Note JSON schema (v1)

```json
{
  "version": 1,
  "edits": [
    {
      "file": "src/auth.ts",
      "line_range": [42, 67],
      "content_hashes": ["a1b2...", "c3d4..."],
      "original_blob_sha": "e5f6...",
      "prompt": "make this faster but readable",
      "conversation_id": "sess_abc123",
      "turn_index": 3,
      "tool_use_id": "toolu_01abc",
      "preceding_turns_summary": "Refactor of auth; previously discussed rate limiting...",
      "model": "claude-sonnet-4-5",
      "tool": "claude-code",
      "timestamp": "2026-04-28T11:32:00Z",
      "derived_from": null
    }
  ]
}
```

`derived_from` is a tagged union with three variants: `null` (no prior attribution), `{ kind: "rewrite", source_commit, source_edit }` (AI-on-AI rewrite — references the prior note), or `{ kind: "backfill", confidence, transcript_path }` (note created by `prov backfill` rather than live capture).

### Resolver flow

```
resolve(file, line) →
  1. git blame -C -M --line-porcelain <file> -L <line>,<line>
       → (commit_sha, original_line_in_that_commit)
  2. SQLite: SELECT json FROM notes WHERE commit_sha = ?
       miss → git notes show refs/notes/prompts <commit_sha>, populate cache
  3. Find edit entry whose line_range contains original_line
  4. Compute hash of current line content
  5. Compare to stored content_hashes[line_idx]
       match  → status: unchanged
       differ → status: drifted, blame_author_after_drift
       absent → status: no_provenance_for_this_line
  6. Return { prompt, model, timestamp, conversation_id, status,
              derived_from, regenerate_url }
```

Same function called by CLI, Skill (via CLI), and the GitHub Action.

### PR intent timeline rendering shape

The Action posts one comment per PR, edited in place on each push. Example body for a PR with two sessions:

```
<!-- prov:pr-timeline -->
## PR Intent Timeline

This PR contains 17 turns across 2 Claude Code sessions, plus 4 lines without provenance.

### Session 1 — `sess_abc123` · 2026-04-26 · 10 turns · claude-opus-4-7

1. **"Add Stripe webhook handling"** — _src/payments.ts (87 lines), src/types.ts (12 lines)_
2. **"Use a 24h dedupe window because Stripe retries can span 23h"** — _src/payments.ts (8 lines)_
3. **"Tests for the dedupe behavior"** — _test/payments.test.ts (54 lines)_
4. ~~"Fix the type error"~~ _(superseded — final code does not contain this turn's output)_
5. <details><summary>5 more turns…</summary> ... </details>

### Session 2 — `sess_def456` · 2026-04-27 · 7 turns · claude-sonnet-4-6

1. **"Add a feature flag for the new dedupe path"** — _src/flags.ts (15 lines), src/payments.ts (3 lines)_
2. ...

### 4 lines without provenance
- `src/index.ts:1-4` — pre-existing or human-authored. Run `prov backfill` to attempt historical capture.

[Generated by Prov v1.0.0 · regenerate any turn with `prov regenerate <file>:<line> --model <name>`]
```

The `<details>` collapse and the "superseded" handling are what make this scale to noisy conversations. The "lines without provenance" footer is intentionally small — the Action does not annotate them.

### Skill mental model

The Skill body is short prose teaching the agent two patterns:

1. *Before substantive edits*: if the file being edited has provenance (`prov log <file> --json` returns non-empty), surface the most relevant prior turn(s) into planning. "Before I rewrite this dedupe logic, the original prompt was 'use a 24h window because Stripe retries span 23h' — I should preserve that constraint."
2. *Treat your own past reasoning as load-bearing context*. The agent that wrote the code three weeks ago is the same agent that's editing it now, just without memory.

Skip provenance queries on greenfield writes, format-only changes, and single-line trivial fixes. v1 enforces this through prose; v1.x adds policy.

---

## Implementation Units

Units are grouped into three phases that map to user-visible milestones. U-IDs are stable across plan edits.

### Phase 1 — Solo Provenance (single-dev local capture and read)

A single developer can install Prov, run Claude Code in their repo, commit changes, and run `prov log` to recover the originating prompts. No sync, no sharing, no agent surface.

> **Note on U6 placement:** U6 (Redactor) is a hard dependency of U3 and ships in Phase 1, even though it is also exercised by Phase 2's privacy mechanisms. Without U6 in Phase 1, staged prompts in `.git/prov-staging/` would hold raw secrets — a P0 issue. The unit definition appears under Phase 2 below for narrative grouping with the rest of privacy hardening, but its order-of-implementation is Phase 1.

- U1. **Project bootstrap**

**Goal:** Create the Cargo workspace, declare crates, set up CI scaffolding, and verify name availability on crates.io / Homebrew / npm.

**Requirements:** R15, R16

**Dependencies:** None

**Files:**
- Create: `Cargo.toml` (workspace), `crates/prov-core/Cargo.toml`, `crates/prov-cli/Cargo.toml`, `crates/prov-core/src/lib.rs`, `crates/prov-cli/src/main.rs`, `LICENSE-APACHE`, `LICENSE-MIT`, `README.md` (skeleton with positioning), `.gitignore`, `.github/workflows/ci.yml`
- Test: `crates/prov-cli/tests/cli_smoke.rs` — smoke test that `prov --version` exits 0

**Approach:**
- Workspace declares `crates/prov-core` and `crates/prov-cli` as members.
- `prov-cli` depends on `prov-core` via `path =` for now; published crates use semver.
- `prov-cli` uses `clap` with derive macros for command parsing; `main.rs` dispatches subcommands defined in `commands/`.
- `prov-core` exposes the public API (resolver, schema types, storage traits) and re-exports nothing from `prov-cli`.
- Verify: `cargo search prov`, `gh search repos prov`, `brew search prov`, `npm view prov` — confirm namespace availability or document the fallback (`prov-cli` crate name with `prov` binary name via `[[bin]] name = "prov"`).
- README skeleton states posture explicitly: not a product, no telemetry, Apache 2.0, Git AI is the prior art and Prov differs by being agent-first + push-by-default + redacted.

**Patterns to follow:**
- Cargo workspace conventions in any well-maintained Rust monorepo (e.g., ripgrep, fd).
- Conventional Commits per the user's global `CLAUDE.md`.

**Test scenarios:**
- *Happy path* — `cargo build --workspace` succeeds; `prov --version` prints the workspace version.
- *Edge case* — `prov` with no subcommand prints clap-generated help and exits 0.
- Test expectation: minimal — most of this unit is bootstrap.

**Verification:**
- `cargo build --workspace` and `cargo test --workspace` both pass on macOS and Linux CI.
- `prov --help` lists every subcommand the plan defines (most as stubs that print "not yet implemented" — fine for Phase 1).
- Name availability documented in README's "Naming" footnote.

---

- U2. **Schema, storage, and SQLite cache**

**Goal:** Define the v1 JSON schema and implement the storage layer that reads/writes notes via `git notes` and populates the SQLite cache.

**Requirements:** R3 (cache enables sub-50ms reads), R7 (cache supports `search`)

**Dependencies:** U1

**Files:**
- Create: `crates/prov-core/src/schema.rs`, `crates/prov-core/src/storage/mod.rs`, `crates/prov-core/src/storage/notes.rs`, `crates/prov-core/src/storage/sqlite.rs`, `crates/prov-core/src/git.rs`
- Test: `crates/prov-core/tests/storage.rs`, `crates/prov-core/tests/fixtures/sample-note.json`

**Approach:**
- `schema::Note` and `schema::Edit` are serde-derived structs matching the v1 JSON shape from High-Level Technical Design. `version` field is checked on read; unknown versions error explicitly. `derived_from` is a tagged union (`enum DerivedFrom { Rewrite { source_commit, source_edit }, Backfill { confidence, transcript_path }, None }`) so AI-on-AI rewrites and backfilled notes are distinguishable in storage and downstream rendering.
- `git::Git` wraps `Command::new("git")` with typed methods: `notes_show(ref, sha)`, `notes_add(ref, sha, content)`, `blame_porcelain(file, line)`, `diff_range(commit, parent)`, `cat_file_blob(sha)`, etc. All git errors surface as typed `GitError`.
- `storage::notes::NotesStore` owns the `(repo_path, ref_name)` and exposes `read(commit_sha) -> Option<Note>` and `write(commit_sha, Note) -> Result<()>`.
- `storage::sqlite::Cache` schema:
  - `notes(commit_sha PRIMARY KEY, json BLOB, fetched_at INTEGER)`
  - `edits(commit_sha, edit_idx, file, line_start, line_end, prompt, conversation_id, model, ts)` indexed on `(file)`, `(file, line_start, line_end)`, FTS5 virtual table on `prompt`
  - `content_hashes(commit_sha, edit_idx, line_idx, hash)` indexed on `(commit_sha, edit_idx)`
  - `schema_version` table with single row tracked by Prov for cache migrations
  - `cache_meta(key TEXT PRIMARY KEY, value TEXT)` — stores the SHA of `refs/notes/prompts` at last reindex (`notes_ref_sha`) so cache reads can detect drift.
- Cache rebuild from notes ref is `Cache::reindex_from(&NotesStore)` and updates `cache_meta.notes_ref_sha`.
- **Coherency check on read:** every `Cache::get_*` call first checks whether `git rev-parse refs/notes/prompts` matches the stored `notes_ref_sha`. On mismatch, log a warning and trigger a background reindex (or a synchronous one for `prov log` if the user prefers correctness over latency via `--no-stale`). Drift happens whenever `prov fetch`, `git fetch`, post-rewrite, or any external `git notes` write touches the ref.

**Patterns to follow:**
- `rusqlite` for SQLite, with bundled feature so users don't need a system sqlite.
- `serde_json` for serialization. **Do NOT** apply `deny_unknown_fields` on top-level `Note`/`Edit` structs — that would block forward compatibility (a v1.x release adding an optional field would be unparseable by prior v1 readers). Use the explicit `version` field as the schema gate; tolerate unknown fields silently. `prov doctor` (a future utility) can warn about unknown fields without rejecting them.

**Test scenarios:**
- *Happy path* — write a `Note` to a fixture repo's `refs/notes/prompts`, read it back, assert structural equality.
- *Happy path* — populate cache from a notes ref containing 100 fixture notes; verify FTS5 returns hits for prompt substrings.
- *Edge case* — read a note with `version: 99` returns `Err(SchemaError::UnknownVersion)`.
- *Edge case* — read from a commit with no note returns `Ok(None)`, not an error.
- *Edge case* — `deny_unknown_fields` rejects a JSON note with extra fields and surfaces a clear error.
- *Error path* — `git notes` failure (corrupt repo) surfaces as typed `GitError::CommandFailed`.
- *Integration* — full round-trip: write 50 notes via `NotesStore`, run `Cache::reindex_from`, query SQLite, assert all 50 are reachable.

**Verification:**
- All tests pass on a fresh clone with no global git config dependencies (use `GIT_CONFIG_GLOBAL=/dev/null` in tests).
- `cargo doc --no-deps` builds without warnings for the public API of `prov-core`.

---

- U3. **Capture pipeline (Claude Code hooks + post-commit flush)**

**Goal:** Implement the hook-side state machine that stages turn and edit metadata, plus the `post-commit` git hook that flushes staged edits into a note attached to HEAD.

**Requirements:** R1, R2 (partial — rebase/squash/cherry-pick handled in U9), R4 (via U6 — staged prompts pass through redactor before write)

**Dependencies:** U2, U6 (Redactor must be available before staging writes any prompt content)

**Files:**
- Create: `crates/prov-core/src/storage/staging.rs`, `crates/prov-core/src/session.rs`, `crates/prov-cli/src/commands/hook.rs`, `githooks/post-commit`, `plugin/hooks/hooks.json`
- Test: `crates/prov-core/tests/staging.rs`, `crates/prov-cli/tests/hook_capture.rs`, `crates/prov-core/tests/fixtures/hook-payloads/` (fixture JSON for each hook event)

**Approach:**
- All hook subcommands first run `git rev-parse --git-dir` and silently exit 0 if it fails (no git repo, nothing to capture). Capture is repo-scoped only.
- `prov hook user-prompt-submit` reads JSON from stdin (per Claude Code hook contract), extracts `session_id` + `prompt` + `cwd` + `transcript_path`, runs `prompt` through `Redactor::redact` (U6) before storage, writes to `.git/prov-staging/<session_id>/turn-<N>.json`. Turn index = count of existing turn files for that session.
- Staging directory `.git/prov-staging/` is created with mode 0700 (explicit `DirBuilder::mode(0o700)`, not relying on umask); individual JSONL files written with mode 0600. This applies even before Phase 2's redactor hardening — staged content must not be world-readable on shared dev environments.
- `prov hook post-tool-use` reads stdin JSON, filters for `tool_name in {Edit, Write, MultiEdit}`, parses `tool_input` and `tool_response` to derive `(file, line_range, before, after, content_hashes, tool_use_id)`. Writes one record per affected file region to `.git/prov-staging/<session_id>/edits.jsonl`. For MultiEdit, decompose into one record per inner edit.
- `prov hook stop` reads stdin JSON, marks the current turn complete in `.git/prov-staging/<session_id>/turn-<N>.json`. Records the model name discovered from `SessionStart` (handled by adding a fourth hook in `hooks.json`) into the session state file.
- `githooks/post-commit` shells `prov hook post-commit`, which:
  1. Computes `git diff HEAD~1..HEAD` (or `git diff --cached --staged` for initial commit handling).
  2. Walks every `.git/prov-staging/<session_id>/` directory.
  3. For each staged edit, attempts to match against the diff's added lines via three strategies in order:
     a. **Exact match** — hash the post-edit content fragment, look for it in the diff's added lines.
     b. **Normalized match** (formatter-resilience fallback) — strip trailing whitespace, normalize quote style, collapse internal whitespace runs, then hash and compare. Tolerates prettier/black/rustfmt running between PostToolUse and commit.
     c. **Line-range proximity** — if the staged edit's `(file, line_range)` overlaps a diff hunk in the same file by ≥ 50%, treat as a probable match with `match_quality: "proximity"` annotation in the note.
  4. **Cross-session disambiguation:** when multiple sessions' staged edits hash-match the same diff line, prefer the session whose most recent `Stop` timestamp is closest to the commit time. Log the ambiguity to `.git/prov-staging/log` so the user can audit.
  5. Edits whose hashes (or proximity match) tie to the diff get bundled into a `Note` and written via `NotesStore::write(HEAD, note)`.
  6. Matched staging entries are removed; unmatched ones stay (deferred to a future commit).
  7. **Cherry-pick path:** if `.git/CHERRY_PICK_HEAD` exists at handler entry, capture its value before continuing (it may be cleared by the time later steps run). Use that source SHA to copy the source commit's note via U9's logic.
- Hook subcommand exits 0 quickly; failures log to `.git/prov-staging/log` and exit 0 anyway (never block the user's commit on Prov errors).
- `plugin/hooks/hooks.json` registers `UserPromptSubmit`, `PostToolUse` (matcher: `Edit|Write|MultiEdit`), `Stop`, `SessionStart` with `command: "prov hook <event>"` and `timeout: 5`.
- **Empirical risk** (deferred to implementation): `tool_response.structuredPatch` exact format. U3 implementation includes a "log first run output and pin parser to observed shape" step before parsing in earnest.

**Patterns to follow:**
- Defensive hook semantics: never block the agent loop; never block the commit. Log to staging/log, exit 0.
- Use `BLAKE3` for content hashing (fast, cryptographic, no external dependency).

**Test scenarios:**
- *Happy path* — fire `prov hook user-prompt-submit` with fixture payload; verify staging file exists with correct turn metadata.
- *Happy path* — fire a sequence (`UserPromptSubmit`, `PostToolUse` × 3, `Stop`); verify staging contains the expected JSONL.
- *Happy path* — full session in fixture repo: stage edits → run `git commit` → assert note written to HEAD with the staged edits.
- *Edge case* — MultiEdit with 5 inner edits decomposes to 5 records in `edits.jsonl`.
- *Edge case* — staged edit whose content does not appear in the commit's diff stays in staging after post-commit.
- *Edge case* — initial commit (no `HEAD~1`): post-commit handler uses the full tree as the "added lines" set.
- *Error path* — malformed JSON on stdin: hook logs error, exits 0, does not crash the agent loop.
- *Error path* — `.git/prov-staging/` not writable: hook logs error, exits 0.
- *Integration* — two concurrent sessions stage independently; commit attributes only the matching session's edits.

**Verification:**
- `prov hook ...` invoked manually with fixture JSON produces the expected staging state.
- A full fixture-repo end-to-end test (Phase 1 milestone): "Claude Code session A" stages 3 turns of edits, `git commit -m foo` runs, `prov log <file>:<line>` returns the originating prompt.

---

- U4. **Resolver**

**Goal:** Implement the `(file, line) → prompt + drift_state` lookup pipeline.

**Requirements:** R3

**Dependencies:** U2

**Files:**
- Create: `crates/prov-core/src/resolver.rs`
- Test: `crates/prov-core/tests/resolver.rs`

**Approach:**
- `Resolver::new(repo_path, cache, notes_store)` constructs the resolver.
- `Resolver::resolve(file, line) -> ResolveResult` runs the pipeline from High-Level Technical Design.
- `ResolveResult` enum: `Unchanged { prompt, ... }`, `Drifted { prompt, original_blob_sha, blame_author_after, ... }`, `NoProvenance { reason }`.
- Cache miss path: shell `git notes show refs/notes/prompts <sha>`, populate cache, retry lookup.
- Use `BLAKE3` to hash current line content, compare against `content_hashes[line_idx]` in the matching edit entry.
- Performance target: warm-cache resolve under 50ms for the 95th percentile in tests on a 10k-note fixture.

**Patterns to follow:**
- The `git blame -C -M --line-porcelain` invocation is the only way to follow copies/moves correctly. Don't try to reimplement.

**Test scenarios:**
- *Happy path* — line content matches stored hash → returns `Unchanged` with the original prompt.
- *Happy path* — line content differs → returns `Drifted` with blame author.
- *Edge case* — line not in any edit's range → returns `NoProvenance { reason: NoMatchingNote }`.
- *Edge case* — commit has no note → returns `NoProvenance { reason: NoNoteForCommit }`.
- *Edge case* — file moved across commits: `-C -M` follows the move; resolver still returns the original prompt.
- *Edge case* — line at the boundary of an edit range (start or end): inclusive-vs-exclusive handling correct.
- *Error path* — corrupt note JSON: returns `NoProvenance { reason: SchemaError(...) }` rather than crashing.
- *Performance* — 1000 random `(file, line)` queries against a 10k-note fixture cache complete in < 50s wall (50ms each at p50).

**Verification:**
- All test scenarios pass.
- Manual smoke: in the prov repo itself, after Phase 1 land, `prov log src/main.rs:1` returns a prompt or a clear `no provenance` reason.

---

- U5. **Read CLI: `log`, `search`, `reindex`, `pr-timeline`, `install`, `uninstall`**

**Goal:** Wire the resolver and storage into the user-facing CLI subcommands that Phase 1 needs (including `pr-timeline` so R7's PR-preview command lives in Phase 1 alongside the rest of the read CLI).

**Requirements:** R7, R8 (install/uninstall portions)

**Dependencies:** U2, U3, U4

**Files:**
- Create: `crates/prov-cli/src/commands/log.rs`, `crates/prov-cli/src/commands/search.rs`, `crates/prov-cli/src/commands/reindex.rs`, `crates/prov-cli/src/commands/pr_timeline.rs`, `crates/prov-cli/src/commands/install.rs`, `crates/prov-cli/src/commands/uninstall.rs`, `crates/prov-cli/src/render/mod.rs`, `crates/prov-cli/src/render/timeline.rs`
- Modify: `crates/prov-cli/src/main.rs` to wire commands into clap
- Test: `crates/prov-cli/tests/cli_log.rs`, `crates/prov-cli/tests/cli_search.rs`, `crates/prov-cli/tests/cli_install.rs`, `crates/prov-cli/tests/cli_pr_timeline.rs`

**Approach:**
- `prov log <file>:<line>` → resolver → terminal-formatted output (file path, line, prompt, drift state, model, timestamp). Use `--json` for machine-readable.
- `prov log <file>` → all unique edit entries for the file, ordered by commit timestamp.
- `prov log <file> --history` → walks `derived_from` chain to surface superseded prior prompts at the same range.
- `prov log <file>:<line> --full` → expands `preceding_turns_summary` into the full transcript text (read from `transcript_path` if note's session is recent and transcript still exists; else "summary only").
- `prov log <file>` (and the `:<line>` form) supports `--only-if-substantial`: returns empty (no notes) when the file has fewer than N lines (default 10) or no existing notes — used by the Skill (U12) to avoid querying provenance for trivial edits.
- `prov search <query>` → SQLite FTS5 against `edits.prompt`, sorted by recency, prints commit + file + prompt snippet.
- `prov pr-timeline --base <ref> --head <ref> [--json | --markdown]` → resolves every AI-attributed line in the diff between `<base>` and `<head>`, groups by `session_id`, renders the PR intent timeline. `--json` for the structured payload (consumed by the GitHub Action and other tooling); `--markdown` for the human-readable comment body. The Action (U13) shells this command with `--markdown` rather than re-implementing the renderer in TypeScript.
- `prov reindex` → clears `.git/prov.db`, repopulates from `refs/notes/prompts`. Records the source notes-ref SHA in a `cache_meta` row so subsequent reads can detect drift (see U2).
- `prov install` → idempotent project-scope installer:
  - Validates `prov` is on PATH (or the binary is locatable for plugin-mode install).
  - Configures git: `git config --add remote.origin.fetch 'refs/notes/prompts:refs/notes/origin/prompts'`, `git config notes.displayRef refs/notes/prompts`, `git config notes.rewrite.amend false`, `git config notes.rewrite.rebase false` (see Key Technical Decisions for why rewrite is disabled here), `git config notes.mergeStrategy manual`.
  - Copies hook templates from the binary's bundled assets into `.git/hooks/post-commit`, `.git/hooks/post-rewrite`, `.git/hooks/pre-push` (chained with any existing hook content via a `# >>> prov` / `# <<< prov` block to avoid clobbering user hooks). For `pre-push` specifically, the chained wrapper captures stdin to a temp file once and replays it to each chained sub-hook (git pipes the ref-update list once; naive chaining would consume it on the first hook and leave subsequent hooks empty).
  - Writes or updates `.claude/settings.json` (project-scope) to add the `hooks` registrations from `plugin/hooks/hooks.json`. Or, if the user opts for plugin-mode (`prov install --plugin`), prints the marketplace install command instead.
  - Creates `.git/prov.db` and runs an initial reindex.
- `prov uninstall` reverses the above precisely (removes `# >>> prov` / `# <<< prov` blocks, removes git config keys it added, leaves notes ref + cache untouched unless `--purge` is passed).

**Patterns to follow:**
- Idempotent install: re-running `prov install` is safe and reports "already installed" without duplicating config.
- Clearly-marked hook block delimiters so users can see what Prov added.

**Test scenarios:**
- *Happy path* — `prov log <file>:<line>` on a fixture repo returns the staged prompt with correct drift state.
- *Happy path* — `prov search "dedupe"` returns the relevant edit when fixture notes contain that prompt.
- *Happy path* — `prov install` in a fresh fixture repo is idempotent (running twice produces identical state).
- *Happy path* — `prov uninstall` removes all Prov config; running it after `install` leaves the repo in its original state.
- *Edge case* — `prov log <file>:<line>` for a file with no provenance prints a clear `no provenance` reason.
- *Edge case* — `prov install` in a repo with existing `.git/hooks/post-commit` content preserves that content and adds Prov inside delimiters.
- *Edge case* — `prov reindex` on a repo with no notes ref prints "no notes to index" and exits 0.
- *Error path* — `prov log` outside a git repo prints "not in a git repo" and exits 1.
- *Error path* — `prov install` with no write permissions on `.git/hooks/` prints actionable error.
- *Integration* — full Phase 1 milestone: in a fixture repo, install → simulate Claude Code session via hook fixtures → commit → `prov log <file>:<line>` returns the originating prompt.

**Verification:**
- All test scenarios pass.
- Manual dogfood: install Prov in a real Claude Code session against a scratch repo, run a few prompts, commit, and `prov log` works end-to-end.
- `prov log` warm-cache p95 latency under 50ms (measure in test harness).

---

### Phase 2 — Team Provenance (sharing notes safely)

A team can fetch and push provenance notes between machines, with airtight redaction of secrets, an explicit private-prompt mechanism, and graceful handling of rebase/squash/cherry-pick.

- U6. **Redactor (built-in detectors + `.provignore`)**

**Goal:** Implement the write-time redactor that scrubs secrets from prompts and conversation summaries before they reach `refs/notes/prompts`.

**Requirements:** R4

**Dependencies:** U1

**Files:**
- Create: `crates/prov-core/src/redactor/mod.rs`, `crates/prov-core/src/redactor/detectors.rs`, `crates/prov-core/src/redactor/provignore.rs`
- Test: `crates/prov-core/tests/redactor_golden.rs`, `crates/prov-core/tests/fixtures/redactor/` (golden input/output pairs)

**Approach:**
- `Redactor::redact(text) -> RedactedText` returns the scrubbed string plus a `Vec<RedactionRecord>` describing what was replaced (type, span, replacement marker).
- Built-in detectors (each is a function `&str -> Vec<DetectedSpan>`):
  - AWS access keys (`AKIA[0-9A-Z]{16}`, `ASIA[0-9A-Z]{16}`)
  - Stripe (`sk_live_[0-9a-zA-Z]{24,}`, `sk_test_[0-9a-zA-Z]{24,}`)
  - GitHub PATs (`ghp_[A-Za-z0-9]{36,}`, `github_pat_[A-Za-z0-9_]{80,}`)
  - JWT structure (`eyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+`)
  - GCP service account JSON (detect by the `"type": "service_account"` substring combined with a `"private_key"` field; redact the entire JSON object)
  - PEM private-key blocks (`-----BEGIN [A-Z ]*PRIVATE KEY-----` through matching END marker)
  - Database URLs with embedded credentials (`(postgres|mysql|mongodb|redis|amqp)://[^:]+:[^@]+@`)
  - Email addresses (RFC-5322-pragmatic regex; configurable on/off, default on)
  - High-entropy strings (Shannon entropy ≥ 4.0 over ≥ 24 chars, alpha-numeric-symbol) — known-imperfect on base64-encoded content (entropy < 4.0); the upstream typed detectors above catch the most common base64-shaped secret formats
- `.provignore` parser supports `.gitignore`-style syntax adapted for content patterns: each non-comment line is a regex; lines starting with `#` are comments; blank lines ignored. Loaded from `<repo_root>/.provignore`.
- Replacement marker: `[REDACTED:<type>]` (e.g., `[REDACTED:aws-key]`, `[REDACTED:provignore-rule:5]`).
- `Redactor` is exposed to the hook subcommand; every prompt and `preceding_turns_summary` passes through it before being staged.

**Patterns to follow:**
- Apply detectors in a deterministic order (specific → generic) so the same input always produces the same output.
- Detectors that overlap a span: the first match wins; subsequent detectors run on the post-replacement string.

**Test scenarios:**
- *Happy path* — golden test for each detector: input `"My AWS key is AKIAIOSFODNN7EXAMPLE"` → output `"My AWS key is [REDACTED:aws-key]"`.
- *Happy path* — `.provignore` with `Acme Corp` rule scrubs `"working on the Acme Corp launch"` correctly.
- *Edge case* — input with no secrets returns identical text and empty `RedactionRecord` list.
- *Edge case* — overlapping matches (a JWT that happens to contain an email-like span): JWT detector wins.
- *Edge case* — high-entropy detector does NOT match natural language (golden test with 100-char Markdown paragraph; expect no false positive).
- *Edge case* — high-entropy detector DOES match a 32-char base64 string in the middle of a sentence.
- *Edge case* — `.provignore` regex with invalid syntax fails the load with a clear error rather than silently dropping the rule.
- *Integration* — capture pipeline (U3) calls `Redactor` before staging; staged file does not contain raw secrets.

**Verification:**
- All golden tests pass.
- Manual review: a curated "secrets in prompts" corpus (private to the dev) is scrubbed cleanly with zero false negatives for the built-in detector set.

---

- U7. **Privacy mechanisms (`# prov:private`, `prov mark-private`, `prov redact-history`)**

**Goal:** Implement the explicit private-prompt opt-out and retroactive scrub commands.

**Requirements:** R5, R10

**Dependencies:** U2, U3, U6

**Files:**
- Create: `crates/prov-cli/src/commands/mark_private.rs`, `crates/prov-cli/src/commands/redact_history.rs`
- Modify: `crates/prov-cli/src/commands/hook.rs` (detect `# prov:private` magic phrase in `UserPromptSubmit` payload)
- Test: `crates/prov-cli/tests/cli_privacy.rs`

**Approach:**
- `# prov:private` detection in the `user-prompt-submit` hook: if the **first or last line of the prompt** matches `(?i)^#\s*prov:private\s*$` (case-insensitive — `# Prov:Private` and `# PROV:PRIVATE` also match), the entire turn is staged under `.git/prov-staging/<session_id>/private/` instead of the regular dir. **Restricting to first/last line** avoids false-trigger when a developer pastes code that happens to contain a `# prov:private` comment inside a code block. On post-commit flush, private edits go to `refs/notes/prompts-private` (a separate ref that `prov install` does NOT add to remote refspecs).
- **Private-context exclusion from summaries:** the `preceding_turns_summary` field on a public note is generated only from the public turns of the conversation. Any turn marked `# prov:private` is excluded from the summarization input so private content never leaks into a public note's summary text. (Generated by U3's `Stop`-handler summarization step; documented as a U3 invariant tested in U7 scenarios.)
- `prov mark-private <commit>`: moves the named commit's note from `refs/notes/prompts` to `refs/notes/prompts-private`. Implemented as `git notes --ref=prompts copy <commit>` to private ref + `git notes --ref=prompts remove <commit>`.
- `prov redact-history <pattern>`: scans every note in `refs/notes/prompts`, applies a fresh redaction pass with the new pattern added to the active redactor, rewrites the notes ref. Implemented by walking the notes tree, re-serializing each note with the redaction applied, and `git update-ref` on the new tree. Prints a summary (notes scanned, notes rewritten, secrets redacted). After a rewrite, `prov redact-history` purges `.git/prov.db` (so cached pre-rewrite content is dropped) and prints a teammate-runnable command (`prov fetch --reset-from-remote && prov reindex`) so collaborators can re-sync. **Important caveat surfaced in stderr:** rewriting the notes ref locally and force-pushing does NOT scrub already-distributed clones, forks, or teammate caches — the underlying secret MUST be rotated independently. See `docs/privacy.md` for the full incident-response playbook.
- The pre-push gate (U8) checks both `refs/notes/prompts` (allowed if no secrets) and `refs/notes/prompts-private` (always blocked from push regardless of remote-ref destination).

**Patterns to follow:**
- Two-ref design: regular and private. Refspecs in `prov install` only configure regular.
- `prov redact-history` is `filter-branch`-equivalent in spirit; document that it rewrites the notes ref and any teammate must `prov fetch --force` after.

**Test scenarios:**
- *Happy path* — prompt containing `# prov:private` → note ends up only in `refs/notes/prompts-private`, never in `refs/notes/prompts`.
- *Edge case* — prompt containing `# Prov:Private` or `# PROV:PRIVATE` (mixed case) → same routing applies (case-insensitive match).
- *Happy path* — `prov mark-private <commit>` moves the note correctly; `prov log <file>:<line>` still shows the prompt locally; `prov push` does not push it.
- *Happy path* — `prov redact-history "/Acme Corp/"` rewrites notes containing that pattern; subsequent `prov log` output shows `[REDACTED:provignore-rule:cli]`.
- *Edge case* — `# prov:private` on a turn with no edits: nothing to stage, but the turn marker is still recorded so future audits can confirm the opt-out fired.
- *Edge case* — `prov redact-history` with a pattern that matches nothing: prints "0 notes rewritten" and exits 0 without rewriting the ref.
- *Edge case* — `prov mark-private` on a commit with no note: prints a clear message and exits 0.
- *Error path* — `prov redact-history` with an invalid regex: errors out before touching the ref.

**Verification:**
- All scenarios pass.
- Manual: stage a fixture session with a `# prov:private` turn and a regular turn, commit, `prov push` (against a local bare repo), verify only the regular turn's note appears on the remote.

---

- U8. **Sync (fetch/push helpers + pre-push gate)**

**Goal:** Wire git refspecs and a `pre-push` hook so notes flow between machines and a write-time redactor failure can't leak secrets to a remote.

**Requirements:** R6, R8 (fetch/push portions)

**Dependencies:** U6, U7

**Files:**
- Create: `crates/prov-cli/src/commands/fetch.rs`, `crates/prov-cli/src/commands/push.rs`, `githooks/pre-push`
- Modify: `crates/prov-cli/src/commands/install.rs` (configure refspecs and install pre-push hook)
- Test: `crates/prov-cli/tests/cli_sync.rs`, `crates/prov-cli/tests/cli_pre_push.rs`

**Approach:**
- `prov fetch [<remote>]` shells `git fetch <remote> refs/notes/prompts:refs/notes/origin/prompts` (tracking-ref refspec — does not overwrite local), then runs `git notes --ref=prompts merge refs/notes/origin/prompts` to merge into the local ref. Defaults to `origin`. Prints note counts before and after.
- `prov push [<remote>]` shells `git push <remote> refs/notes/prompts:refs/notes/prompts`. Pre-push gate runs as part of the user's normal `git push` (because the hook is registered globally), but `prov push` triggers it too.
- `githooks/pre-push` reads stdin (per githooks(5): `<local ref> <local sha> <remote ref> <remote sha>` lines), filters by **default** for pushes that touch `refs/notes/prompts` or `refs/notes/prompts-private` (opt-in to scan every push via `git config --local prov.scanAllPushes true`), and for each note being newly pushed:
  1. Diff the new content against the remote's known content for that ref.
  2. Run the redactor over the new content. If the redactor finds any secret patterns, abort the push with a clear error listing the detected patterns and the commit SHAs they appear in.
  3. Provide remediation: `Re-run after fixing, or override with --no-verify`.
  4. **Audit:** when `--no-verify` is used to override a detected secret, log the override event to `.git/prov-staging/log` with timestamp, refs being pushed, and a redacted-detector summary. The audit log gives the user (and any downstream incident response) a record that the gate fired and was bypassed.
- The hook filters on **both** the local ref name AND the remote ref name from each stdin line. Block any push where the local ref is `refs/notes/prompts-private` regardless of what the remote ref is — prevents the user-manual-mapping bypass (`git push origin refs/notes/prompts-private:refs/notes/prompts`).

**Patterns to follow:**
- Pre-push hook is non-interactive (no prompts). Print clear actionable messages and exit non-zero on block.

**Test scenarios:**
- *Happy path* — clean notes ref pushes successfully.
- *Happy path* — `prov fetch` from a local bare remote retrieves notes that exist remotely.
- *Happy path* — pre-push gate detects an unredacted `AKIA...` in a new note, blocks the push, prints the offending commit SHA.
- *Edge case* — `prov push` to a remote with no `refs/notes/prompts` yet creates the ref.
- *Edge case* — `prov push` when local and remote refs are identical: no-op, exits 0.
- *Happy path* — push of a regular branch (no notes refs touched) skips the gate and incurs negligible overhead — this is the **default** scoping per R6.
- *Error path* — pre-push hook with malformed stdin: logs error, exits 0 (defensive: don't break user's push on Prov bug).
- *Integration* — install → stage session → commit → push → pre-push gate fires → user sees the block.

**Verification:**
- All scenarios pass against a local bare repo as the remote.
- Pre-push gate added < 200ms to a normal git push (no notes touched) — measure.

---

- U9. **History rewrite handling (post-rewrite, cherry-pick, repair, gc)**

**Goal:** Make notes survive rebase, amend, squash, and cherry-pick. Provide repair and gc commands for when things still go wrong.

**Requirements:** R2 (full coverage), R9

**Dependencies:** U2, U3

**Files:**
- Create: `crates/prov-cli/src/commands/repair.rs`, `crates/prov-cli/src/commands/gc.rs`, `githooks/post-rewrite`
- Modify: `githooks/post-commit` (detect cherry-pick via `CHERRY_PICK_HEAD`)
- Test: `crates/prov-cli/tests/cli_rewrite.rs`, `crates/prov-cli/tests/cli_repair.rs`

**Approach:**
- `notes.rewrite.amend = false` and `notes.rewrite.rebase = false` are set by `prov install`. **Why:** git's built-in rewrite handling defaults to `notes.rewriteMode = concatenate`, which appends raw bytes — for JSON notes this produces invalid `{...}{...}{...}` blobs on squash before `post-rewrite` fires. Disabling rewriteRef makes `post-rewrite` the sole writer.
- `githooks/post-rewrite` script shells `prov hook post-rewrite`, which:
  1. Reads stdin (`<old-sha> <new-sha>` lines) and the first arg (`amend` or `rebase`).
  2. Groups by `new-sha`. For 1:1 mapping (amend or simple rebase), reads the old SHA's note and writes it verbatim to the new SHA.
  3. For N:1 mapping (squash), reads the notes for each old SHA, merges their `edits` arrays into a single Note (deduplicated by `tool_use_id` and `(conversation_id, turn_index)`, sorted by `timestamp`), writes to the new SHA.
  4. In both cases, deletes the old notes after the new write succeeds (atomic via `git update-ref`).
- `githooks/post-commit` extension: if `.git/CHERRY_PICK_HEAD` exists, read it, find the source commit's note, copy it to HEAD, set `derived_from` on each edit to point at the source commit. (`notes.rewriteRef` does NOT cover cherry-pick.)
- `prov repair` walks `git reflog` for the last N days (default 14), finds rewrite events whose new-SHA has no note, looks for orphaned notes attached to old-SHAs that the reflog says were rewritten to known new-SHAs, and reattaches.
- `prov gc` performs three jobs:
  1. Cull notes attached to commits no longer reachable from any ref.
  2. Prune `.git/prov-staging/` entries older than `gc.staging_ttl_days`.
  3. Optional `--compact` flag: rewrite notes older than 90 days to drop `preceding_turns_summary` and any `original_blob_sha` whose blob is no longer reachable.

**Patterns to follow:**
- Always operate on a transient branch of `refs/notes/prompts` then atomically `git update-ref` to swap, so a crash mid-operation leaves the original ref intact.

**Test scenarios:**
- *Happy path* — `git commit --amend` to a commit with a note: post-rewrite preserves the note on the new SHA (`notes.rewriteRef` does this; verify).
- *Happy path* — `git rebase` reordering 3 commits each with a note: all 3 notes end up on the new SHAs.
- *Happy path* — `git rebase -i` squashing 3 commits into 1: post-rewrite merges the 3 notes' edits into a single note on the squashed commit.
- *Happy path* — `git cherry-pick <sha>` onto a different branch: post-commit detects `CHERRY_PICK_HEAD`, copies the note, sets `derived_from`.
- *Happy path* — `prov repair` after a rebase that bypassed the hooks (e.g., done in a different shell with `core.hooksPath` set elsewhere): walks reflog and reattaches.
- *Happy path* — `prov gc` removes a note attached to a commit that was unreachable after a `git push --force` (simulated locally).
- *Edge case* — squash where one old SHA had no note and one had a note: merged note contains the one note's edits.
- *Edge case* — cherry-pick conflict requiring manual resolution: post-commit fires only on the final resolved commit, behaves correctly.
- *Edge case* — `prov gc --compact` on notes < 90 days old: no-op for those notes.
- *Error path* — post-rewrite with malformed stdin: logs error, exits 0.
- *Integration* — full flow: stage → commit → amend → rebase → cherry-pick → `prov log` still resolves correctly through every rewrite.

**Verification:**
- All scenarios pass on fixture repos.
- The squash test is the linchpin — manually exercise it before declaring done.

---

- U10. **Notes merge resolution (`prov notes resolve` + manual strategy config)**

**Goal:** Handle the case where two team members annotate the same commit and both push.

**Requirements:** R9 (notes resolve portion)

**Dependencies:** U2

**Files:**
- Create: `crates/prov-cli/src/commands/notes_resolve.rs`
- Modify: `crates/prov-cli/src/commands/install.rs` (set `git config notes.mergeStrategy manual`)
- Test: `crates/prov-cli/tests/cli_notes_resolve.rs`

**Approach:**
- `prov install` sets `git config --local notes.mergeStrategy manual`. This means `git fetch` won't auto-merge conflicting notes; instead, the user (or `prov notes resolve`) handles it.
- `prov notes resolve` checks for the merge state (`git notes --ref=prompts merge --commit` precondition: an in-progress merge, or `refs/notes/prompts-NOTES_MERGE_*` workspace dirs).
- For each conflicting commit: read the local note, read the incoming note, JSON-aware merge by:
  1. Take both `edits[]` arrays.
  2. Deduplicate by `(conversation_id, turn_index, tool_use_id)` (or by content hash if those keys collide).
  3. Sort by `timestamp`.
  4. Write the merged note to the merge workspace.
- `git notes --ref=prompts merge --commit` finalizes the merge.
- Document v1.1 follow-up in plan: register a real `notes` merge driver so `git fetch` resolves automatically without `prov notes resolve`.

**Patterns to follow:**
- JSON-aware merging treats each Note as a set of edit records; union with deduplication is the safe default.

**Test scenarios:**
- *Happy path* — fixture: dev A and dev B add notes to the same commit (different sessions); after `prov fetch && prov notes resolve`, the commit's note contains edits from both sessions.
- *Edge case* — both devs annotated the SAME `tool_use_id` (impossible in practice; defensive): keep the one with the later timestamp.
- *Edge case* — conflict where one side has a v1 note and the other has a hypothetical v2 note: error out cleanly with "schema version mismatch" rather than corrupting.
- *Error path* — `prov notes resolve` when no merge is in progress: prints "no merge to resolve" and exits 0.

**Verification:**
- Two-clone fixture test: clone fixture twice, annotate the same commit from each, push both, fetch and resolve, assert merged state.

---

### Phase 3 — Agent and Review Surfaces (the differentiated value)

The Skill makes Claude Code aware of its own past reasoning; the GitHub Action gives reviewers a PR intent timeline; `regenerate` and `backfill` round out the CLI.

- U11. **Claude Code Plugin packaging**

**Goal:** Bundle the hook scripts, the Skill, and binary-distribution wiring into a `.claude-plugin` shape installable via Claude Code marketplace or local `--plugin-dir`.

**Requirements:** R11 (plugin shape), R15 (distribution)

**Dependencies:** U3 (hooks), U12 (Skill content), U5 (install)

**Files:**
- Create: `plugin/.claude-plugin/plugin.json`, `plugin/hooks/hooks.json`, `plugin/README.md`
- Modify: `crates/prov-cli/src/commands/install.rs` (add `--plugin` flag that prints marketplace install instructions)
- Test: `crates/prov-cli/tests/cli_plugin_layout.rs` (validates the plugin/ directory matches the documented schema)

**Approach:**
- `plugin/.claude-plugin/plugin.json` follows the documented Claude Code plugin schema (name, version, description, hooks, skills).
- `plugin/hooks/hooks.json` registers the four hook events (`UserPromptSubmit`, `PostToolUse` matched on `Edit|Write|MultiEdit`, `Stop`, `SessionStart`) with `command: "prov hook <event>"` and `timeout: 5`.
- The plugin assumes `prov` is on PATH; the README explains both install paths (Homebrew/cargo/curl|sh for the binary, then either `/plugin install prov` from a marketplace or `prov install --project-scope`).
- `prov install --plugin` prints the marketplace install command and exits — does not modify the project's `.claude/`.

**Patterns to follow:**
- Documented Claude Code plugin examples (link in README): `code.claude.com/docs/en/plugins`.

**Test scenarios:**
- *Happy path* — `plugin/.claude-plugin/plugin.json` validates against the documented schema (use a JSON schema validator in the test).
- *Happy path* — `plugin/hooks/hooks.json` lists all four hook events with correct matchers.
- *Edge case* — `prov install --plugin` exits with the marketplace install command in stdout, without touching `.claude/`.

**Verification:**
- Manual install of the plugin from a local `--plugin-dir` against a real Claude Code session, with the binary on PATH, captures a session end-to-end.
- Plugin description and trigger phrasing reviewed against the SKILL frontmatter conventions documented in the research.

---

- U12. **The Skill (SKILL.md content + reference files)**

**Goal:** Write the Skill body that teaches Claude Code to query its own provenance before substantive edits.

**Requirements:** R11

**Dependencies:** U5 (CLI it calls into)

**Files:**
- Create: `plugin/skills/prov/SKILL.md`, `plugin/skills/prov/references/querying.md`, `plugin/skills/prov/references/triggers.md`
- Test: `plugin/skills/prov/tests/skill_smoke.md` (manual test plan; not automated — this is content)

**Approach:**
- SKILL.md frontmatter:
  - `name: prov`
  - `description: ...` (long, trigger-rich; ~600 chars; mentions "before refactoring", "before editing AI-written code", "to recover the original prompt", etc.)
  - `paths:` glob — narrow default that excludes documentation and config (`!**/*.md`, `!**/README*`, `!**/CHANGELOG*`, `!**/*.json`, `!**/*.yaml`, `!**/*.yml`, `!**/*.toml`) and matches everything else once `.git/prov.db` exists in the repo. The exclusion list keeps the Skill quiet on docs-only and config-only edits, where provenance lookup is high-noise low-signal.
- Skill prose instructs the agent to use `prov log <file> --only-if-substantial --json` rather than the bare `prov log` form. The `--only-if-substantial` flag (defined in U5) returns empty for files under N lines or with no existing notes, providing a CLI-level gate that complements the prose-only gating.
- SKILL.md body, ≤ 500 lines, structured:
  1. *What this Skill does*: one paragraph framing.
  2. *When to use it*: bullet list — substantive edits to files with existing provenance, refactors that span multiple AI-written sections, debugging behavior whose original constraints aren't obvious.
  3. *When NOT to use it*: greenfield writes, formatting/lint fixes, single-line trivial changes.
  4. *Query patterns*: link to `references/querying.md`. Two patterns: `prov log <file>:<line> --json` for point lookup, `prov log <file> --json` for whole-file context.
  5. *How to use the result*: surface the most relevant prior turn(s) into planning. Cite the prompt verbatim. Treat the past constraint as load-bearing unless explicitly invalidated by the current request.
- `references/querying.md`: concrete examples of how to call `prov log`, parse JSON output, and integrate findings into a planning step.
- `references/triggers.md`: heuristic guide for "substantive vs trivial" — referenced by the agent when it's unsure whether to query.

**Patterns to follow:**
- SKILL conventions from `references/skills.md` in the user's existing skills (sampled in research).
- Trigger-rich `description:` — this is what causes the agent to load the skill.

**Test scenarios:**
- *Behavioral / manual* — In a fresh Claude Code session with the plugin installed and Prov capture data present in a fixture repo, ask "refactor `src/payments.ts` to extract the dedupe logic" → agent calls `prov log src/payments.ts` before proposing edits and surfaces the prior dedupe-window prompt in its plan.
- *Behavioral / manual — negative trigger* — In the same session, ask "fix the typo on line 12 of `README.md`" → agent does NOT query provenance (trivial single-line change).
- *Behavioral / manual — greenfield* — Ask "create a new file `src/utils/format.ts` with a date formatter" → agent does NOT query provenance for a file that doesn't exist yet.
- *Behavioral / manual — drifted line* — Ask the agent to explain `src/payments.ts:247` where the line content has been hand-edited after the original AI write → agent calls `prov log src/payments.ts:247`, surfaces the original prompt with the drift state, and frames its explanation around both the original intent and the divergence.
- *Content lint* — SKILL.md frontmatter passes a JSON-schema-style validator for required fields (`name`, `description`); SKILL.md body does not exceed 500 lines.
- *Content lint* — `references/querying.md` and `references/triggers.md` exist and are referenced from SKILL.md by name.

**Verification:**
- All four behavioral scenarios pass in a real Claude Code session against a fixture repo with seeded Prov notes. Iterate on the `description` and body until the trigger fires reliably for substantive asks and stays quiet on trivial ones.
- Content lints (file existence, frontmatter validity, line-count cap) pass in CI.

---

- U13. **GitHub Action (PR intent timeline comment)**

**Goal:** Implement the CI-side surface that posts the per-session timeline comment on PRs.

**Requirements:** R12

**Dependencies:** U2 (resolver), U5 (CLI for `prov log --json`)

**Files:**
- Create: `action/action.yml`, `action/package.json`, `action/tsconfig.json`, `action/src/index.ts`, `action/src/download.ts`, `action/src/timeline.ts`, `action/src/github.ts`
- Modify: `crates/prov-cli/src/render/timeline.rs` (the **single** Markdown renderer; the GitHub Action shells `prov pr-timeline --markdown` rather than re-implementing the renderer in TypeScript)
- Test: `action/__tests__/timeline.test.ts` (Jest), `crates/prov-cli/tests/cli_pr_timeline.rs`

**Approach:**
- `action.yml` declares inputs (`github-token` required; `prov-version` optional, defaults to latest release). The README usage example documents minimum required permissions in a `permissions:` block (`pull-requests: write` + `contents: read`) and pins the Action to a full commit SHA — a token scoped narrower than the repo's default mitigates blast radius if the Action itself is ever compromised.
- `action.yml` also documents the workflow requirement `actions/checkout@v4` with `fetch-depth: 0` so `git blame` (used by the resolver) has the full history needed for line-attribution; without full history, `prov pr-timeline` falls back to "no provenance" for lines whose origin commit is shallow-pruned.
- `action/src/download.ts` fetches the prov binary from GitHub Releases for the runner's OS/arch and verifies it via Sigstore cosign keyless verification (the release workflow signs each artifact with its OIDC identity). The cosign bundle is fetched alongside the binary; verification confirms both the artifact integrity and the signing identity. SHA256 alone is not enough — a release-asset-replacement attack would substitute checksum and binary atomically.
- Action runs `prov pr-timeline --base ${{ github.base_ref }} --head HEAD --markdown`. Output is the rendered Markdown body, ready to post. **No TypeScript renderer.** The Rust binary is the single source of truth for the comment shape; `action/src/timeline.ts` is a thin pass-through that posts whatever stdout the binary produces.
- The Rust `prov pr-timeline --markdown` implementation:
  - Resolves the PR diff one file at a time using `git blame -C -M` per file (one invocation per touched file, batched — not per line).
  - Caps at a configurable `max_lines_resolved` (default 5000) to prevent timeout on monorepo-scale PRs; lines beyond the cap are summarized in the "lines without provenance" footer.
  - Produces the rendering shape shown in High-Level Technical Design (sessions, turns, superseded collapse, lines-without-provenance footer).
- `action/src/github.ts` upserts a sticky comment identified by the `<!-- prov:pr-timeline -->` HTML marker. Algorithm: list PR comments, **filter to those authored by the bot identity** (`github-actions[bot]` or the configured token's user), find one whose body starts with the marker, edit it (PATCH); if none, create. Filtering by author prevents marker-spoofing — a contributor with PR-comment access cannot pre-place a comment with the marker and have the Action edit it.

**Patterns to follow:**
- Sticky-comment via hidden marker is the standard idiom for PR-comment automation.
- Use `@actions/core` and `@actions/github` (Octokit wrapper).
- Bundle the action with `@vercel/ncc` so `dist/` is a single committed JS file (standard practice for JS Actions).

**Test scenarios:**
- *Happy path* — fixture PR JSON + fixture resolver output → expected Markdown body matches a golden file.
- *Happy path* — first run on a PR creates the comment; second run edits it in place (verify via mocked Octokit).
- *Happy path* — multi-session PR: two `## Session N` blocks rendered in chronological order.
- *Edge case* — PR with no AI-attributed lines: comment body is "No Prov-tracked turns in this PR" plus the lines-without-provenance footer.
- *Edge case* — PR with 50 turns in one session: turns 6+ collapse into `<details>` block by default (`max_visible_turns` config knob).
- *Edge case* — superseded turn detection: a turn whose edits were entirely overwritten by later turns shows as `~~strikethrough~~ (superseded)`.
- *Edge case* — comment body exceeding GitHub's 65,536 char limit: gracefully truncates with `<details>` and a "see full timeline at <link>" footer.
- *Error path* — runner can't download the prov binary (network error): action exits with a clear error message.
- *Integration* — full action run against a fixture repo with a fake remote, verify Octokit was called with the expected upsert payload.

**Verification:**
- Snapshot tests (`expect(rendered).toMatchSnapshot()`) on the timeline renderer pass.
- Manual test: trigger the action against a real PR in a scratch repo and visually confirm the comment.

---

- U14. **`prov regenerate <file>:<line> --model <name>`**

**Goal:** Replay the original prompt for a given line against a chosen model and render a diff against the stored `original_blob_sha`.

**Requirements:** R13

**Dependencies:** U4 (resolver), U2 (storage; needs `original_blob_sha` blob still reachable)

**Files:**
- Create: `crates/prov-cli/src/commands/regenerate.rs`, `crates/prov-cli/src/anthropic.rs` (thin Anthropic API client — kept in prov-cli to avoid leaking network/HTTP dependencies into prov-core, which is the embeddable library surface)
- Test: `crates/prov-cli/tests/cli_regenerate.rs` (uses a mock HTTP server)

**Approach:**
- `prov regenerate <file>:<line> [--model <name>]` resolves the line, reads `original_blob_sha` content via `git cat-file blob`, calls Anthropic's API with the original `prompt` + `preceding_turns_summary` (if available), receives a fresh response, renders a side-by-side or unified diff against the original.
- Default model: same as the stored note's `model` field. `--model` overrides.
- **API key handling:** read `ANTHROPIC_API_KEY` from env at startup, immediately move it into an owned `String`, then call `std::env::remove_var("ANTHROPIC_API_KEY")` so any subprocess (or panic backtrace dump) cannot recover it from the environment. Explicit error if absent. The Anthropic HTTP client wraps its error type in a redacting `Display` impl so `Authorization` header values are never printed to logs (including `.git/prov-staging/log` and any structured-error output). Never write env-dump diagnostics to staging/log.
- HTTP client: `reqwest` with `rustls` feature.

**Patterns to follow:**
- Anthropic Messages API (cite docs); use prompt caching for the system prompt if applicable.

**Test scenarios:**
- *Happy path* — mocked API returns deterministic text → diff renders correctly.
- *Edge case* — `original_blob_sha` no longer reachable (gc'd): error with clear message and suggestion to regenerate without diff (still call API, just no comparison).
- *Edge case* — line has `derived_from` chain: regenerate uses the most recent prompt by default, `--root` flag uses the original.
- *Error path* — `ANTHROPIC_API_KEY` unset: clear error, exit 1, no API call attempted.
- *Error path* — API returns 429 (rate limit): print the retry-after header and exit non-zero.

**Verification:**
- Mock HTTP tests pass.
- Manual test: regenerate a real line against `claude-haiku-4-5` (cheap), verify diff renders.

---

- U15. **`prov backfill`**

**Goal:** Best-effort historical capture from stored Claude Code session transcripts.

**Requirements:** R14

**Dependencies:** U2 (storage), U6 (redactor — backfilled prompts must redact too)

**Files:**
- Create: `crates/prov-cli/src/commands/backfill.rs`, `crates/prov-core/src/transcript.rs` (parser for Claude Code transcript JSONL)
- Test: `crates/prov-cli/tests/cli_backfill.rs`, `crates/prov-core/tests/transcript_parser.rs`, `crates/prov-core/tests/fixtures/transcripts/` (small synthetic transcript files)

**Approach:**
- Transcript discovery: read `~/.claude/projects/<sanitized-cwd>/<session-uuid>.jsonl` (sanitized-cwd replaces `/` with `-`, leading `-` preserved — verified by inspection of existing `~/.claude/projects/-Users-matt-Documents-GitHub-*/` directories).
- Before reading any transcript file, `prov backfill` confirms the project-id derivation against the current cwd, prints the list of files it will read, and requires `--yes` (or interactive confirmation) before proceeding. Refuses if the project-id mapping is ambiguous; supports `--transcript-path <path>` for explicit override.
- **Author check:** for each candidate commit, compare `git log -1 --format=%ae <sha>` against `git config user.email`. Refuse to backfill commits authored by a different user unless `--cross-author` is passed (with a loud warning in stderr that backfill may attribute the wrong dev's reasoning).
- Default confidence floor (e.g., `0.6`) below which backfill silently skips a commit; `--include-low-confidence` opt-in for everything.
- Parse each JSONL line as one event; reconstruct sessions by `session_id`.
- For each session, extract turn boundaries (`UserPromptSubmit` markers) and infer edits from tool-use events present in the transcript.
- Match each session's edits to a commit by:
  1. Time window: commits within session start ± grace period (default 4 hours).
  2. File overlap: commit's diff touches a file the session edited.
  3. Content overlap: commit's added lines contain content fragments from session's `new_string`s (BLAKE3 hash match).
- Highest-scoring commit per session gets the backfilled note, marked with `derived_from: { kind: "backfill", confidence: <score>, transcript_path: <path> }` (per the schema variant defined in U2).
- Every backfilled prompt passes through the redactor before storage.
- All notes get `(approximate)` rendering treatment in CLI output (controlled by a `confidence < 1.0` flag).

**Patterns to follow:**
- Best-effort: never crash on malformed transcript lines; log and skip.
- Idempotent: re-running backfill on a repo with existing notes does NOT overwrite existing notes; skips commits that already have a non-approximate note.

**Test scenarios:**
- *Happy path* — synthetic transcript + fixture commit history: backfill writes notes for the matching commits with correct content.
- *Happy path* — re-run backfill is idempotent (no duplicate notes, no overwrites).
- *Edge case* — transcript with no matching commits: prints "0 commits backfilled" and exits 0.
- *Edge case* — commit matches multiple sessions (developer worked on the same area in two sessions): highest-confidence session wins; note records the alternative in `derived_from`.
- *Edge case* — transcript file unreadable (permission denied): prints clear error, skips that file, continues with others.
- *Edge case* — backfilled prompt contains an AWS key: redactor scrubs before storage.

**Verification:**
- Tests pass on synthetic fixtures.
- Manual: run `prov backfill` in a real repo with a few weeks of Claude Code history; spot-check 3-5 backfilled notes against memory of what was actually prompted.

---

## System-Wide Impact

- **Interaction graph.** Hooks fire in the user's normal Claude Code loop and git workflow. Failures are non-blocking by design (every hook subcommand exits 0 even on internal error and logs to `.git/prov-staging/log`). The pre-push gate is the one place Prov can block a user action — and that block is intentional for the secret-detection use case, with a clear `--no-verify` escape.
- **Error propagation.** All Prov errors stay local to Prov. Hook failures never crash Claude Code; git hook failures never crash git operations (post-commit/post-rewrite/pre-push internally exit 0 unless intentionally blocking). Errors surface via `prov status` (a future utility) or via the staging log.
- **State lifecycle risks.** The biggest risk is staging-state corruption: a session's staging files get inconsistent state if the hook is killed mid-write. Mitigation: append-only JSONL for edits, atomic write-and-rename for turn metadata, defensive parsing on read (skip malformed lines, log).
- **API surface parity.** The CLI, Skill (via CLI), and GitHub Action all call the same `prov-core::resolver`. Adding a new resolution capability lights up everywhere.
- **Integration coverage.** The end-to-end fixture test in U5 (install → simulated session → commit → `prov log` works) is the load-bearing integration test. Phase 2 milestone adds a multi-machine integration test (clone twice, push notes between, conflicts resolved). Phase 3 milestone adds a real-PR action test.
- **Unchanged invariants.** Prov never modifies user code, never rewrites the user's branch refs, never auto-pushes anything (push happens only on explicit `git push` or `prov push`), never runs network requests except in `prov regenerate` (Anthropic) and `prov push`/`prov fetch` (the configured remote). The pre-push gate may block a user push but cannot mutate it.

---

## Risks & Dependencies

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Anthropic changes Claude Code hook payload schema between versions | Medium | High (breaks capture silently) | Pin to documented schema, add integration test that runs against a real Claude Code session in CI nightly, surface schema-mismatch errors loudly in staging log. |
| `tool_response.structuredPatch` format is undocumented and changes | Medium | Medium (breaks edit attribution) | U3 includes empirical pinning step; parser tolerates new fields via `serde(deny_unknown_fields = false)` for this nested type only. |
| Notes merge driver gap (v1 ships with `manual` only) frustrates teams | Medium | Medium | Document `prov notes resolve` clearly; v1.1 adds custom merge driver. |
| Pre-push gate has a false negative (secret slips through) | Low | Critical (secrets leak to remote) | Defense-in-depth: write-time redactor is the primary; pre-push is the second line; `prov redact-history` can scrub retroactively. Add a curated regression corpus that grows with reported escapes. |
| Pre-push gate has a high false positive rate (blocks innocent pushes) | Medium | Medium (user trust erosion) | Each detector ships with a confidence test against a "natural prose" corpus before going live. `--no-verify` always available. |
| Git AI ships a breaking interop format change while we're building | Low | Medium | Don't claim format compat in v1; defer interop to v1.1 conversation. |
| Plugin install via marketplace unavailable (user's harness too old) | Medium | Low | Project-scope install is the documented escape hatch; works on every Claude Code version that supports `.claude/settings.json` hooks. |
| `git blame -C -M` performance regression on huge files | Low | Medium | Cache blame results in SQLite keyed by `(file, blob_sha)`; recompute only on file-content change. |
| Storage growth at high-frequency-Claude-Code teams | Medium | Medium | 90-day compaction (`prov gc --compact`); encourage `git gc --aggressive` periodically; document expected growth (low MB/year per active dev). |
| User runs `prov uninstall` and loses staging | Low | Low | Uninstall preserves notes ref + cache by default; staging cleared only with `--purge`. Documented. |
| Rust binary fails to download in CI (rate limit / outage) | Medium | Low | Action retries with backoff; cache binary in actions cache keyed by version. |

---

## Documentation Plan

- **README.md** is the marketing — explain the problem, position vs Git AI honestly, show the agent-first angle, cover install for all three surfaces, walk through "what does it actually do" with screenshots.
- **`docs/install.md`** — step-by-step for binary install, `prov install`, plugin install, GitHub Action enablement.
- **`docs/privacy.md`** — the redaction model, the `# prov:private` opt-out (case-insensitive matching, first/last-line-only restriction), `.provignore` syntax, what gets pushed and what doesn't, how to scrub retroactively, and the **incident-response playbook** for when a secret is discovered in pushed notes (including the explicit caveat that `prov redact-history` is a local-and-future-clone scrub only — the underlying secret MUST be rotated independently). Also documents the threat model around the pre-push gate's `--no-verify` bypass and the audit-log entry the gate emits when bypassed.
- **`docs/architecture.md`** — capture pipeline, storage, resolver, with the High-Level Technical Design diagrams from this plan.
- **`docs/cli.md`** — every subcommand with examples, exit codes, environment variables.
- **`docs/troubleshooting.md`** — orphaned notes, manual merge resolution, `prov repair`, what to do when capture stops working, how to inspect `.git/prov-staging/log`.
- **`plugin/README.md`** — plugin-specific install and configuration.
- **Status/posture footer in README**: "Not a product. No telemetry. Apache 2.0. Maintained as time permits — fork freely."

---

## Operational / Rollout Notes

- **Release cadence**: tag-driven via `release-plz` auto-PRs. Conventional commits required (matches the user's global preference). Each release artifact is signed with Sigstore cosign keyless (using GitHub Actions OIDC identity); both the GitHub Action and `install.sh` verify the signature before exec. SHA256-only verification is insufficient — a release-asset compromise can substitute checksum and binary atomically.
- **Versioning**: semver. `prov-core` is the public Rust API surface; v1.x must not break it (or bump major). Note schema version is independent — `version: 1` for v1.x; bumps follow when the JSON shape changes.
- **Telemetry**: explicitly none. Document this in README.
- **Reporting bugs**: GitHub Issues on the `prov` repo; no private bug bounty for v1.
- **Pre-1.0 compatibility**: notes written by `0.x` releases must be readable by `1.0` (forward compat one direction only).
- **Phased rollout to dogfood users**: ship Phase 1 to the maintainer's own machines first; ship Phase 2 once one external user has fetched/pushed notes successfully; ship Phase 3 once the Skill works reliably in three real refactor sessions.

---

## Alternative Approaches Considered

- **Sidecar git repo for provenance** (instead of notes ref). Pros: more visible in the GitHub UI, doesn't suffer notes UX rough edges. Cons: loses per-commit attachment, requires separate clone for backfill, harder to keep in sync. Rejected for v1; revisit only if notes UX issues prove blocking.
- **Hosted backend** (cloud DB, web dashboard). Pros: nicer UX, no merge-conflict problems, easier search. Cons: betrays the project posture, requires running a service, kills auditability. Rejected categorically.
- **Capture via Claude Code SDK wrapper** (instead of hooks). Pros: tool-agnostic from day one. Cons: requires users to change how they invoke Claude Code; doesn't compose with the rest of the harness. Rejected for v1 — hooks are the lower-friction path.
- **Per-line PR annotations** (instead of single timeline comment). Pros: line-precise, no scrolling. Cons: comment-volume nightmare, doesn't surface conversational arc. Rejected after user feedback during synthesis.
- **TypeScript / Node implementation** (instead of Rust). Pros: shared language with the GitHub Action. Cons: Node startup latency unacceptable for hooks that fire on every prompt; harder to ship as a single static binary. Rejected.
- **gitoxide instead of shelling to `git`**. Pros: in-process, faster, no fork overhead. Cons: explicit gaps in 2026 (hooks, push, full merge not implemented); adds complexity in environments where the user's git config matters. Rejected for v1; consider for the cache-rebuild path in v1.x where commit traversal is the bottleneck.

---

## Sources & References

- Claude Code hooks: <https://code.claude.com/docs/en/hooks>, <https://code.claude.com/docs/en/hooks.md>
- Claude Code skills: <https://code.claude.com/docs/en/skills>
- Claude Code plugins: <https://code.claude.com/docs/en/plugins>
- Git AI (prior art): <https://usegitai.com/>, <https://github.com/git-ai-project/git-ai>
- Git notes documentation: <https://git-scm.com/docs/git-notes>
- Tyler Cipriani on git notes UX: <https://tylercipriani.com/blog/2022/11/19/git-notes-gits-coolest-most-unloved-feature/>
- Pushing git notes operationally: <https://www.codestudy.net/blog/git-how-to-push-messages-added-by-git-notes-to-the-central-git-server/>
- Praqma git-merge-driver (precedent for custom notes merge): <https://github.com/Praqma/git-merge-driver>
- post-rewrite hook reference: <https://git-scm.com/docs/githooks>
- Rust release automation in 2026: <https://blog.orhun.dev/automated-rust-releases/>, <https://crates.io/crates/cargo-dist>, <https://crates.io/crates/release-plz>
- macOS CLI signing for OSS: <https://tuist.dev/blog/2024/12/31/signing-macos-clis>, <https://crates.io/crates/apple-codesign>
- gitoxide gap status: <https://github.com/GitoxideLabs/gitoxide>
- musl static linking: <https://github.com/rust-cross/rust-musl-cross>
- AI authorship academic context: <https://arxiv.org/html/2601.17406v1>, <https://dl.acm.org/doi/10.1145/3733799.3762964>
- RAI footers (complementary convention): <https://dev.to/anchildress1/signing-your-name-on-ai-assisted-commits-with-rai-footers-2b0o>
