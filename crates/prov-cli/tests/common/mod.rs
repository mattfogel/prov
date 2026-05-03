//! Shared helpers for integration tests that drive the `prov` binary against
//! a real on-disk git repo.
//!
//! Cargo will compile any file directly under `tests/` as its own test binary,
//! but `tests/common/mod.rs` is only compiled if a sibling test file references
//! it via `mod common;` — so this module isn't its own test target.
//!
//! All git invocations scrub `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_SYSTEM` so a
//! contributor's personal git config (signing keys, hooks, aliases) doesn't
//! leak into the fixtures and produce flaky behavior on different machines.

#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

use assert_cmd::Command as AssertCommand;
use tempfile::TempDir;

pub fn run_git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?} failed");
}

pub fn git_capture(cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git")
}

pub fn head_sha(cwd: &Path) -> String {
    let out = git_capture(cwd, &["rev-parse", "HEAD"]);
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

pub fn init_bare_remote() -> TempDir {
    let tmp = TempDir::new().unwrap();
    run_git(tmp.path(), &["init", "-q", "--bare"]);
    tmp
}

pub fn prov_in(cwd: &Path) -> AssertCommand {
    let mut c = AssertCommand::cargo_bin("prov").unwrap();
    c.current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    c
}
