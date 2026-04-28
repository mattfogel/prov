//! Core library for prov: schema, resolver, storage, redactor.
//!
//! This crate is the embeddable surface of prov. It is dependency-light by design:
//! no network I/O, no Anthropic-API client, no hook-runtime concerns. Those live in
//! `prov-cli`. Other tools (an editor plugin, a future LSP, an analytics pipeline)
//! can depend on this crate without inheriting the CLI's surface.

pub mod git;
pub mod schema;
pub mod storage;

pub use schema::SCHEMA_VERSION;

/// Returns the package version (`CARGO_PKG_VERSION`).
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_non_empty() {
        assert!(!version().is_empty());
    }

    #[test]
    fn schema_version_is_one() {
        assert_eq!(SCHEMA_VERSION, 1);
    }
}
