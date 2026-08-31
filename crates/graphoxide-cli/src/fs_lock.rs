//! Cross-platform detection of advisory file-lock contention.
//!
//! Contention for an already-held lock range surfaces with a platform-specific
//! raw signature: Unix `flock` reports `EWOULDBLOCK`
//! (`ErrorKind::WouldBlock`), while Windows `LockFileEx` (what `fs2` uses)
//! reports `ERROR_LOCK_VIOLATION` (os error 33), which Rust decodes as
//! `ErrorKind::Uncategorized`. Callers that treat only `WouldBlock` as
//! contention misclassify Windows contention as an I/O failure and fail the
//! whole operation instead of waiting for, or skipping, the contended owner.

use std::io;

/// Reports whether an advisory lock failure means another owner currently
/// holds the requested range.
pub(crate) fn is_lock_contention(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(windows)]
    if error.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION as i32) {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::is_lock_contention;
    use std::io;

    #[test]
    fn would_block_is_contention_on_every_platform() {
        let error = io::Error::from(io::ErrorKind::WouldBlock);
        assert!(is_lock_contention(&error));
    }

    #[test]
    fn unknown_raw_error_codes_are_not_contention() {
        // 999 is not a lock-contention code on any supported platform, so
        // it must not be treated as contention even though Rust decodes
        // unknown codes as `Uncategorized` (the same kind Windows lock
        // violations decode to).
        let error = io::Error::from_raw_os_error(999);
        assert!(!is_lock_contention(&error));
    }

    #[cfg(windows)]
    #[test]
    fn lock_violation_is_contention_on_windows() {
        // Raw ERROR_LOCK_VIOLATION (33) is how `fs2` surfaces a contended
        // `LockFileEx`. Rust decodes it as `Uncategorized`, not
        // `WouldBlock`, so the raw-code check is the only signal.
        let error = io::Error::from_raw_os_error(
            windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION as i32,
        );
        assert_eq!(error.kind(), io::ErrorKind::Uncategorized);
        assert!(is_lock_contention(&error));
    }

    #[cfg(unix)]
    #[test]
    fn unix_errors_other_than_would_block_are_not_contention() {
        // Raw 33 is not a lock-contention code on Unix (EDEADLK on Linux,
        // EDOM on Darwin), so the check must be platform-specific rather
        // than a raw-code match that happens to work on one OS.
        let error = io::Error::from_raw_os_error(33);
        assert!(!is_lock_contention(&error));
        let error = io::Error::from_raw_os_error(libc::EDEADLK);
        assert!(!is_lock_contention(&error));
        let error = io::Error::from(io::ErrorKind::PermissionDenied);
        assert!(!is_lock_contention(&error));
    }
}
