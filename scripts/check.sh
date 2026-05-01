#!/usr/bin/env bash
# Run the same four checks CI runs (`.github/workflows/ci.yml`) so a clean
# local run gives high confidence the PR will go green:
#   1. cargo build  --workspace --all-targets --locked
#   2. cargo test   --workspace --all-targets --locked
#   3. cargo fmt    --all -- --check
#   4. cargo clippy --workspace --all-targets --locked -- -D warnings
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

printf '\nAll checks passed.\n'
