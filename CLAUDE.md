# Working on Prov

Project-scoped instructions for Claude Code in this repo. Layered on top of
the user's global `~/.claude/CLAUDE.md`.

## Before opening a PR

Run `./scripts/check.sh` from the repo root. It runs the same four checks
CI does (build, test, `cargo fmt --check`, `cargo clippy -D warnings`) with
the same `RUSTFLAGS=-D warnings` flag. Local `cargo build && cargo test`
alone will not catch fmt or clippy regressions; CI does, and we want to
catch them before pushing.

A clean run is the bar. If the script fails, fix the underlying issue
rather than skipping it — the same check will block the PR otherwise.

## Documented solutions

`docs/solutions/` — documented solutions to past problems (bugs, best
practices, conventions, workflow patterns), organized by category with
YAML frontmatter (`module`, `tags`, `problem_type`). Relevant when
implementing or debugging in documented areas.
