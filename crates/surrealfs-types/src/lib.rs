//! Core SurrealFS domain types: identifiers, repository paths, typed errors, and the
//! canonical byte encoding used for all content addressing and state-root digests.
//!
//! This crate has no database dependency. Version constants here are recorded in
//! `COMPATIBILITY.md` and must only change together with a schema/export migration.

pub mod canonical;
pub mod error;
pub mod id;
pub mod path;
pub mod state;
pub mod time;

pub use error::SfsError;
pub use id::*;
pub use path::RepoPath;

/// Version of the canonical encoding + digest scheme (`COMPATIBILITY.md`).
pub const HASH_VERSION: u32 = 1;
/// Version of the state-root layout (`COMPATIBILITY.md`).
pub const ROOT_FORMAT_VERSION: u32 = 1;
/// Version of the persistent schema this build writes.
pub const SCHEMA_VERSION: u32 = 1;

pub type Result<T> = std::result::Result<T, SfsError>;
