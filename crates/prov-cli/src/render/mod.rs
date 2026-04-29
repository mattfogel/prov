//! Rendering surfaces for the read-side CLI.
//!
//! The PR intent timeline renderer is the only resident in v1; both the local
//! `prov pr-timeline` invocation and the GitHub Action share this Rust
//! implementation so the comment shape has a single source of truth.

pub mod timeline;
