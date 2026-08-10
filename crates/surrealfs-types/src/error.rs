//! Typed domain errors. Store adapters convert engine failures into these variants;
//! surfaces map them onto errno-equivalent or API errors.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SfsError {
    #[error("invalid identifier: {0}")]
    InvalidId(String),

    #[error("invalid path: {0}")]
    InvalidPath(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("already exists: {0}")]
    AlreadyExists(String),

    #[error("not a directory: {0}")]
    NotADirectory(String),

    #[error("is a directory: {0}")]
    IsADirectory(String),

    #[error("directory not empty: {0}")]
    DirectoryNotEmpty(String),

    /// Expected-head compare-and-swap failed: someone else advanced the branch.
    #[error("branch head conflict on {branch}: expected {expected}, found {actual}")]
    HeadConflict {
        branch: String,
        expected: String,
        actual: String,
    },

    /// The same request id was already applied with the same command hash.
    /// Carries the previously stored receipt so callers resolve idempotently.
    #[error("request {request_id} already applied")]
    ReplayedRequest { request_id: String },

    /// The same request id was seen with a different command hash: caller bug.
    #[error("request {request_id} reused with a different command")]
    RequestMismatch { request_id: String },

    #[error("workspace is {status}, expected OPEN")]
    WorkspaceClosed { status: String },

    /// Publication exceeds the preflight byte/key budget; never retryable as-is.
    #[error("publication over budget: {0}")]
    OverBudget(String),

    /// The host directory no longer matches the state a change set was computed against, so
    /// applying it would silently overwrite work SurrealFS never saw.
    #[error("host has drifted at {path}: {detail}")]
    HostDrift { path: String, detail: String },

    #[error("integrity failure: {0}")]
    Corruption(String),

    #[error("store is locked by another process: {0}")]
    StoreLocked(String),

    /// A key problem: missing, malformed, wrong for this repository, or supplied for a
    /// repository that is not encrypted.
    ///
    /// Deliberately distinct from `Corruption`. A wrong key produces the same authentication
    /// failure as tampered bytes, and calling that corruption would send someone hunting a
    /// data-integrity bug that does not exist.
    #[error("encryption: {0}")]
    Encryption(String),

    #[error("migration failure: {0}")]
    Migration(String),

    /// Ambiguous outcome: the transaction may or may not have committed.
    /// Resolve by receipt lookup, never by blind retry.
    #[error("ambiguous transaction outcome for request {request_id}: {detail}")]
    Ambiguous { request_id: String, detail: String },

    #[error("storage error: {0}")]
    Storage(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl SfsError {
    /// True when a caller may safely retry the same request id.
    pub fn is_retryable(&self) -> bool {
        matches!(self, SfsError::Storage(_))
    }
}
