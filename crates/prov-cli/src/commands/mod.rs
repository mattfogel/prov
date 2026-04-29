//! Subcommand implementations. Each module owns one `prov <verb>`.
//!
//! In Phase 1 most commands are stubs that print `not yet implemented` and exit
//! non-zero. Stubs let `prov --help` enumerate the full command surface and let
//! later units fill in implementations without re-wiring the CLI.

pub mod backfill;
pub mod fetch;
pub mod gc;
pub mod hook;
pub mod install;
pub mod log;
pub mod mark_private;
pub mod notes_resolve;
pub mod pr_timeline;
pub mod push;
pub mod redact_history;
pub mod regenerate;
pub mod reindex;
pub mod repair;
pub mod search;
pub mod uninstall;

/// Shared helper used by every Phase-1 stub.
pub(crate) fn unimplemented_stub(name: &str) -> anyhow::Result<()> {
    eprintln!("prov {name}: not yet implemented (Phase 1 stub).");
    eprintln!("See docs/plans/ for the implementation roadmap.");
    std::process::exit(2);
}
