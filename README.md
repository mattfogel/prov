# prov

> Git blame tells you who. Prov tells you why.

Prov captures the prompt-and-conversation context behind AI-agent-driven edits, attaches it to commits via git notes, and exposes it through thin read surfaces:

- **CLI** for humans: `prov log src/auth.ts:42` returns the originating prompt for any line.
- **Agent skills and hooks** for supported harnesses: Claude Code and Codex can capture provenance today, and agents can query prior reasoning before proposing edits.

## Status

**v1 in active development. This README is a skeleton.** See [`docs/plans/`](docs/plans/) for the implementation plan.

## What this is (and isn't)

Prov is an open-source tool I'm building because I want it to exist. **It is not a product.** No telemetry, no hosted service, no signups, no paid tier — and no commitment to a roadmap beyond what fits in nights-and-weekends maintenance.

Other tools have shipped with similar core architecture: per-line AI authorship in git notes, SQLite cache, rewrite preservation, multi-agent attribution. Prov is not novel on storage. The honest differentiators are:

- **Agent-first, harness-agnostic posture.** Giving an agent access to its own prior reasoning is a different category of capability — not just better tooling for humans, but better continuity across sessions and harnesses.
- **Redactor-by-default-when-shared.** Notes are local-only out of the box; opting in to team sharing (`prov sync enable origin`) turns on a write-time secret-detector pipeline plus a pre-push gate. The redaction story matters when you choose to share.

## Install

```bash
# Coming soon
cargo install prov               # via crates.io
brew install mattfogel/tap/prov  # via Homebrew tap
curl -fsSL https://raw.githubusercontent.com/mattfogel/prov/main/install.sh | sh  # cosign-verified
```

Each release will be signed with [Sigstore cosign](https://www.sigstore.dev/) keyless once the release workflow ships. The install script will check signatures before exec — SHA256 alone is not enough against a release-asset compromise.

## Quick start

```bash
# In any git repo, install shared git hooks/cache:
prov install

# Then explicitly wire the agent harnesses you use:
prov install --agent claude
prov install --agent codex
# or: prov install --agent all

# Restart/reopen the harness so it picks up repo-local hook config.
# Run an agent session, make some edits, commit. Then:
prov log src/auth.ts                 # see provenance for the whole file
prov log src/auth.ts:42              # see the originating prompt for one line
prov search "rate limiting"          # find prompts that mention rate limiting
```

Codex project-local hooks require Codex to trust the repo's `.codex/` config layer before they run.

By default, notes stay on your machine. To share with your team:

```bash
prov sync enable origin              # opt in to push/fetch for this repo
```

## Contributing

Run `./scripts/check.sh` before opening a PR — it mirrors CI (build, test,
`cargo fmt --check`, `cargo clippy -D warnings`) so a clean local run
gives high confidence the PR will go green.

## License

Dual-licensed under [MIT](LICENSE-MIT) **OR** [Apache-2.0](LICENSE-APACHE) at your option, matching Rust ecosystem convention.

## Funding

If a team finds Prov useful and wants to fund maintenance, [GitHub Sponsors](https://github.com/sponsors/mattfogel) is the answer.

## Posture

Maintained as time permits. No SLA. No roadmap commitments. Issues and PRs welcome; responsiveness varies. The codebase stays small and forkable on purpose — the whole thing should be auditable in an afternoon by anyone who wants to know what's running on their repo.
