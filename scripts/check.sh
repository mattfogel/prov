#!/usr/bin/env bash
# Run the same checks CI runs (`.github/workflows/ci.yml`) so a clean local
# run gives high confidence the PR will go green:
#   Cargo (matches the `test` and `lint` jobs):
#     1. cargo build  --workspace --all-targets --locked
#     2. cargo test   --workspace --all-targets --locked
#     3. cargo fmt    --all -- --check
#     4. cargo clippy --workspace --all-targets --locked -- -D warnings
#   GitHub Action (matches the `action` job, when action/ has changed since
#   the merge-base with origin/main; otherwise skipped to keep the no-op
#   path fast):
#     5. tsc --noEmit
#     6. jest
#     7. ncc build
#     8. dist/ freshness check
#   Cross-language sentinels:
#     9. STICKY_MARKER drift check (Rust render module vs TS Action)
#
# `RUSTFLAGS=-D warnings` matches the workflow env and promotes all warnings
# to hard errors. Run from the repo root: `./scripts/check.sh`.

set -euo pipefail

export RUSTFLAGS="${RUSTFLAGS:-} -D warnings"
export CARGO_TERM_COLOR=always

step() { printf '\n==> %s\n' "$*"; }

step "cargo build"
cargo build --workspace --all-targets --locked

step "cargo test"
cargo test --workspace --all-targets --locked

step "cargo fmt --check"
cargo fmt --all -- --check

step "cargo clippy"
cargo clippy --workspace --all-targets --locked -- -D warnings

# Action checks: only run when action/ touched since origin/main, since `npm
# ci` + `ncc build` adds 30-60s to a no-op cargo-only run. CI always runs
# the action job in parallel, so skipping locally never hides a regression
# CI wouldn't otherwise catch.
if git rev-parse --verify origin/main >/dev/null 2>&1; then
  base=$(git merge-base HEAD origin/main 2>/dev/null || git rev-parse HEAD)
else
  base=$(git rev-parse HEAD)
fi
if [ -n "$(git diff --name-only "$base" -- action 2>/dev/null)" ] || [ -n "$(git status --porcelain action 2>/dev/null)" ]; then
  step "action: npm ci + tsc + jest + ncc build + dist/ freshness"
  (
    cd action
    npm ci --silent
    npm run lint
    npm test --silent
    npm run build --silent
    # Compare against the index, not HEAD: a freshly-rebuilt dist/ that
    # already matches what the dev has staged is fine — they will commit
    # source + dist/ together. The check fires only if `npm run build`
    # produced different bytes than the working-tree-going-into-the-commit.
    if ! git diff --quiet -- dist/; then
      printf 'ERROR: action/dist/ is stale — `npm run build` produced different bytes than the working tree. Stage the rebuild.\n' >&2
      git --no-pager diff dist/ | head -40
      exit 1
    fi
  )
else
  step "action: skipped (no changes vs $base)"
fi

# Cross-language sentinel drift: STICKY_MARKER lives in both the Rust render
# module and the TS Action. Rename or typo in either silently breaks the
# sticky-comment upsert (duplicate comments) or weakens spoof defense.
step "STICKY_MARKER drift check"
rust_marker=$(grep -E 'pub const STICKY_MARKER' crates/prov-cli/src/render/timeline.rs | sed -E 's/.*= "([^"]*)".*/\1/')
ts_marker=$(grep -E "export const STICKY_MARKER" action/src/github.ts | sed -E "s/.*= '([^']*)'.*/\1/")
if [ -z "$rust_marker" ] || [ -z "$ts_marker" ]; then
  printf 'ERROR: could not extract STICKY_MARKER from one of the source files.\n' >&2
  printf '  Rust: %s\n' "$rust_marker" >&2
  printf '  TS:   %s\n' "$ts_marker" >&2
  exit 1
fi
if [ "$rust_marker" != "$ts_marker" ]; then
  printf 'ERROR: STICKY_MARKER drift between Rust and TS:\n' >&2
  printf '  Rust (crates/prov-cli/src/render/timeline.rs): %s\n' "$rust_marker" >&2
  printf '  TS   (action/src/github.ts):                   %s\n' "$ts_marker" >&2
  exit 1
fi

printf '\nAll checks passed.\n'
