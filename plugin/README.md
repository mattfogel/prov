# prov — Claude Code plugin

This directory is the [Claude Code plugin](https://code.claude.com/docs/en/plugins) shape
for [prov](https://github.com/mattfogel/prov), the prompt-provenance tool. It bundles:

- `hooks/hooks.json` — registers four capture hooks on the Claude Code session
  (`UserPromptSubmit`, `PostToolUse` matched on `Edit|Write|MultiEdit`, `Stop`,
  `SessionStart`). Each hook calls `prov hook <event>` with a 5-second timeout.
- `skills/prov/SKILL.md` — teaches the agent to query its own prior reasoning
  (`prov log <file>:<line>`, `prov search <query>`) when the user asks about
  code provenance ("why does this do X", "what was the prompt for this line",
  "what's the history of this file"). The skill is question-triggered, not
  edit-triggered — it does not preemptively query before refactors or edits.

## Prerequisites

The plugin assumes the `prov` binary is on `PATH`. Install it first via one of:

```bash
cargo install prov                                                            # crates.io
brew install mattfogel/tap/prov                                               # Homebrew tap
curl -fsSL https://raw.githubusercontent.com/mattfogel/prov/main/install.sh | sh  # cosign-verified
```

Each release is Sigstore-signed; the curl-pipe-sh script verifies the signature before
exec.

## Install paths

You have two ways to wire prov into a repo. Pick one:

### Option A — install the plugin

Use the Claude Code plugin marketplace install:

```text
/plugin install prov
```

Plugin install drops the hooks into Claude Code itself; capture works in every
repo where the binary is on `PATH`.

### Option B — per-repo wiring

If you don't want a global plugin install, run this inside each repo you care about:

```bash
prov install --agent claude
```

`prov install` is idempotent. It writes:

- `.git/hooks/post-commit`, `.git/hooks/pre-push`, `.git/hooks/post-rewrite` (chained
  inside `# >>> prov` / `# <<< prov` delimiters so it composes with your own hooks).
- `.claude/settings.json` (merges this plugin's hook entries into the project-scope
  Claude Code settings — same hook list as the marketplace install).
- `.git/prov.db` (SQLite read cache).

Run plain `prov install` when you only want shared git hooks/cache and no agent
adapter config. Re-run `prov install --agent claude` after pulling a prov upgrade;
it self-heals legacy entries and reports installed adapters without duplicating config.

## Verify it's working

After install, restart Claude Code (so hooks reload), run a session, and commit.
Then:

```bash
prov log <file>:<line>
```

should print the originating prompt for any line that came out of an AI edit.
If it returns "no provenance", check `.git/prov-staging/log` for capture errors —
hooks are non-blocking by design and write diagnostics there rather than crashing
the session.

## Plugin metadata

See `.claude-plugin/plugin.json` for the manifest. Hooks are auto-discovered from
`hooks/hooks.json`; skills are auto-discovered from `skills/<name>/SKILL.md`.

## License

MIT — same as the rest of the prov repo.
