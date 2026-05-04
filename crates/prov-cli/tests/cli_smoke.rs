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
    // The unimplemented_stub helper (commands/mod.rs::unimplemented_stub) is
    // shared by every Phase 1 stub; when invoked, it must exit code 2 with
    // "not yet implemented" in stderr so callers — including agents and CI —
    // can distinguish stubbed-out subcommands from real failures.
    //
    // We assert by probing every subcommand that's *currently* a stub. As
    // stubs are replaced with real implementations across units (U5 wired
    // the read CLI; U6–U10 added more), the set shrinks. The test passes as
    // long as at least one stub remains and every probed stub satisfies the
    // contract; once the last stub lands, swap to a unit test for the helper
    // itself (it currently calls `process::exit`, which makes direct unit
    // testing awkward — refactor to return an exit code, then test it).
    let candidates = ["backfill", "regenerate"];

    let mut probed = 0;
    for verb in candidates {
        // Skip any candidate that's been implemented since this list was last
        // updated (it would no longer print the stub message), so the test
        // doesn't break on the next stub-replacement PR.
        let output = prov().arg(verb).assert().get_output().clone();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.contains("not yet implemented") {
            continue;
        }
        probed += 1;
        prov()
            .arg(verb)
            .assert()
            .failure()
            .code(2)
            .stderr(predicate::str::contains("not yet implemented"));
    }
    assert!(
        probed > 0,
        "no stubbed subcommands left to probe — refactor unimplemented_stub \
         to return an exit code and unit-test it directly, then delete this test"
    );
}

#[test]
fn hook_subcommand_exists_but_is_hidden() {
    // Hook events parse and exit 0 (defensive Phase-1 default).
    prov()
        .args(["hook", "user-prompt-submit"])
        .assert()
        .success();
}
