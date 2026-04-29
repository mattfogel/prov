//! Phase 1 smoke tests for the `prov` binary. These confirm the CLI surface is
//! wired and parseable; they do NOT exercise capture / resolver behavior — that
//! lives in `prov-core`'s own tests and in the per-unit integration tests
//! introduced from U3 onward.

use assert_cmd::Command;
use predicates::prelude::*;

fn prov() -> Command {
    Command::cargo_bin("prov").expect("prov binary should build")
}

#[test]
fn version_prints_workspace_version() {
    prov()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn help_lists_every_v1_subcommand() {
    let output = prov().arg("--help").assert().success().get_output().clone();
    let stdout = String::from_utf8(output.stdout).expect("help output should be utf-8");

    // hook is hidden by design — not asserted here.
    let expected = [
        "log",
        "search",
        "pr-timeline",
        "reindex",
        "install",
        "uninstall",
        "fetch",
        "push",
        "notes-resolve",
        "mark-private",
        "redact-history",
        "repair",
        "gc",
        "regenerate",
        "backfill",
    ];

    for verb in expected {
        assert!(
            stdout.contains(verb),
            "`prov --help` should mention subcommand `{verb}`. Full help:\n{stdout}"
        );
    }
}

#[test]
fn no_subcommand_prints_help_and_errors() {
    // clap exits 2 by default when a required subcommand is missing.
    prov().assert().failure().code(2);
}

#[test]
fn unimplemented_stub_exits_with_code_2() {
    // U5 wired up the read CLI (log/search/reindex/pr-timeline/install/uninstall);
    // the remaining Phase-2/3 commands are still stubs. `repair` is one of them.
    prov()
        .arg("repair")
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("not yet implemented"));
}

#[test]
fn hook_subcommand_exists_but_is_hidden() {
    // Hook events parse and exit 0 (defensive Phase-1 default).
    prov()
        .args(["hook", "user-prompt-submit"])
        .assert()
        .success();
}
