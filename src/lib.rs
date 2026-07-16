//! git-storage library crate: the storage engine behind the CLI.
//!
//! DESIGN.md is the authoritative spec; IMPLEMENTATION-PLAN.md maps modules
//! to milestones. Exposed as a library so integration tests (and later the
//! SDK surface) can drive the engine directly.

pub mod backend;
pub mod chunker;
pub mod crypto;
pub mod engine;
pub mod gitrepo;
pub mod manifest;
