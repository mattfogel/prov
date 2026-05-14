# agent-hooks/ — Claude Code capture hooks

`agent-hooks/hooks.json` is the Claude Code hook bundle that prov uses to
capture each turn's prompt and tool calls during a session. The file is
embedded into the `prov` binary at compile time and written into a repo's
`.claude/settings.json` by `prov install --agent claude`.

Four hooks, each with a 5-second timeout:

- `SessionStart` → `prov hook session-start` — capture the active model.
- `UserPromptSubmit` → `prov hook user-prompt-submit` — stage the prompt.
- `PostToolUse` (matched on `Edit|Write|MultiEdit`) → `prov hook post-tool-use`
  — stage the edit.
- `Stop` → `prov hook stop` — mark the turn complete.

The hooks only write to `<git-dir>/prov-staging/`; nothing they emit reaches
the agent's prompt. The staged content is flushed into a git note on
`refs/notes/prov` by the `post-commit` git hook (also installed by
`prov install`).

## Install

```bash
prov install --agent claude
```

That merges these entries into `.claude/settings.json` in the current repo
(idempotent — re-run safely after upgrades). Restart Claude Code so the
hooks reload.

## Read surface

The optional skill at [`../skills/prov/`](../skills/prov) teaches Claude
Code (or any harness that supports Anthropic-style Skills) to answer user
questions about provenance using `prov log` and `prov search`. Install it
separately with [Vercel's `skills` CLI](https://github.com/vercel-labs/skills):

```bash
npx skills add mattfogel/prov
```

The skill is independent of these hooks — capture works fine without the
skill, and the skill works fine in any repo where the `prov` binary is on
`PATH` (whether or not these hooks are installed locally).
