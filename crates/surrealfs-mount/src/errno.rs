//! Translating domain errors into errno values.
//!
//! Mount protocols carry an integer, so every `SfsError` has to become one. The mapping is
//! here rather than in the kernel because it is a protocol concern: an SDK caller wants the
//! typed error and its message, a FUSE client can only be told `ENOTEMPTY`.
//!
//! Constants come from `libc` rather than being written out, because they genuinely differ by
//! platform — `ENOTEMPTY` is 39 on Linux and 66 on macOS, and hardcoding either would produce
//! a filesystem that reports nonsense on the other.

use surrealfs_types::SfsError;

/// The errno a mount protocol should report for this error.
pub fn errno_for(error: &SfsError) -> i32 {
    match error {
        SfsError::NotFound(_) => libc::ENOENT,
        SfsError::AlreadyExists(_) => libc::EEXIST,
        SfsError::NotADirectory(_) => libc::ENOTDIR,
        SfsError::IsADirectory(_) => libc::EISDIR,
        SfsError::DirectoryNotEmpty(_) => libc::ENOTEMPTY,
        SfsError::InvalidPath(_) | SfsError::InvalidId(_) => libc::EINVAL,

        // A lost expected-head race is a transient conflict, not a broken filesystem. EAGAIN
        // tells a client the operation may succeed if retried, which is exactly true here.
        SfsError::HeadConflict { .. } => libc::EAGAIN,

        // The caller reused a request id with different content, or the host moved under an
        // apply. Both are the caller's state being wrong, not the filesystem's.
        SfsError::RequestMismatch { .. } | SfsError::HostDrift { .. } => libc::EINVAL,

        // Over the publication budget: the write is too large to commit as one unit, and
        // retrying unchanged will not help.
        SfsError::OverBudget(_) => libc::EFBIG,

        SfsError::WorkspaceClosed { .. } => libc::ESTALE,
        SfsError::StoreLocked(_) => libc::EBUSY,

        // The caller cannot read this content with the key they have. That is a permission
        // problem, not a missing file and not a broken one — reporting it as either would send
        // an operator looking in entirely the wrong place.
        SfsError::Encryption(_) => libc::EACCES,

        // Integrity failures must never look like ordinary absence — a client that sees
        // ENOENT will happily recreate the file and destroy the evidence.
        SfsError::Corruption(_) => libc::EIO,
        SfsError::Migration(_) => libc::EIO,
        SfsError::Ambiguous { .. } => libc::EIO,
        SfsError::Storage(_) => libc::EIO,

        SfsError::Io(err) => err.raw_os_error().unwrap_or(libc::EIO),

        // A replayed request is a success from the protocol's point of view.
        SfsError::ReplayedRequest { .. } => 0,
    }
}

/// Convert a result into the `Result<T, i32>` shape mount adapters work in.
pub fn to_errno<T>(result: Result<T, SfsError>) -> Result<T, i32> {
    result.map_err(|e| errno_for(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_common_filesystem_errors_map_as_posix_expects() {
        assert_eq!(errno_for(&SfsError::NotFound("x".into())), libc::ENOENT);
        assert_eq!(
            errno_for(&SfsError::AlreadyExists("x".into())),
            libc::EEXIST
        );
        assert_eq!(
            errno_for(&SfsError::NotADirectory("x".into())),
            libc::ENOTDIR
        );
        assert_eq!(errno_for(&SfsError::IsADirectory("x".into())), libc::EISDIR);
        assert_eq!(
            errno_for(&SfsError::DirectoryNotEmpty("x".into())),
            libc::ENOTEMPTY
        );
    }

    /// The distinction that matters most: corruption must not look like a missing file, or a
    /// client will recreate it and overwrite the evidence.
    #[test]
    fn integrity_failures_are_io_errors_not_missing_files() {
        for err in [
            SfsError::Corruption("bad digest".into()),
            SfsError::Migration("half-applied".into()),
            SfsError::Ambiguous {
                request_id: "r".into(),
                detail: "d".into(),
            },
            SfsError::Storage("engine".into()),
        ] {
            assert_eq!(errno_for(&err), libc::EIO, "{err:?} must report EIO");
            assert_ne!(errno_for(&err), libc::ENOENT);
        }
    }

    #[test]
    fn a_lost_race_is_retryable_and_an_oversized_write_is_not() {
        assert_eq!(
            errno_for(&SfsError::HeadConflict {
                branch: "main".into(),
                expected: "a".into(),
                actual: "b".into(),
            }),
            libc::EAGAIN,
            "a concurrent publication may succeed on retry"
        );
        assert_eq!(
            errno_for(&SfsError::OverBudget("too big".into())),
            libc::EFBIG,
            "an oversized publication will not succeed unchanged"
        );
    }

    #[test]
    fn io_errors_keep_their_original_number_when_they_have_one() {
        let err = SfsError::Io(std::io::Error::from_raw_os_error(libc::EACCES));
        assert_eq!(errno_for(&err), libc::EACCES);
        // And a synthetic one still lands somewhere sensible.
        let synthetic = SfsError::Io(std::io::Error::other("no os code"));
        assert_eq!(errno_for(&synthetic), libc::EIO);
    }

    /// A wrong key and tampered bytes fail identically inside AES-GCM, so the distinction has to
    /// be made by the error type rather than rediscovered from the symptom.
    #[test]
    fn a_key_problem_is_a_permission_error_not_corruption() {
        let err = SfsError::Encryption("the key is wrong for this repository".into());
        assert_eq!(errno_for(&err), libc::EACCES);
        assert_ne!(
            errno_for(&err),
            libc::EIO,
            "a key problem is not corruption"
        );
        assert_ne!(errno_for(&err), libc::ENOENT, "the file is there");
    }

    #[test]
    fn a_closed_workspace_is_stale_rather_than_missing() {
        assert_eq!(
            errno_for(&SfsError::WorkspaceClosed {
                status: "CLOSED".into()
            }),
            libc::ESTALE
        );
    }
}
