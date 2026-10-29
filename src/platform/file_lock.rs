//! Advisory file-lock helpers shared by every non-blocking lock site.

/// A non-blocking file lock failed because another handle holds it ("busy,
/// try again"), not because of an I/O fault. Unix reports `WouldBlock`;
/// fs2 uses `LockFileEx` on Windows, whose competing-lock error is
/// `ERROR_LOCK_VIOLATION` (33), which std does not map to `WouldBlock`.
pub fn lock_contended(error: &std::io::Error) -> bool {
    const ERROR_LOCK_VIOLATION: i32 = 33;
    error.kind() == std::io::ErrorKind::WouldBlock
        || (cfg!(windows) && error.raw_os_error() == Some(ERROR_LOCK_VIOLATION))
}

#[cfg(test)]
mod tests {
    use super::lock_contended;
    use fs2::FileExt;
    use std::io::{Error, ErrorKind};

    #[test]
    fn only_platform_contention_counts_as_busy() {
        assert!(lock_contended(&Error::from(ErrorKind::WouldBlock)));
        assert_eq!(lock_contended(&Error::from_raw_os_error(33)), cfg!(windows));
        for kind in [ErrorKind::PermissionDenied, ErrorKind::NotFound, ErrorKind::Other] {
            assert!(!lock_contended(&Error::from(kind)));
        }
        #[cfg(windows)]
        for code in [5, 32, 6, 3] {
            assert!(!lock_contended(&Error::from_raw_os_error(code)));
        }
    }

    #[test]
    fn a_lock_held_by_another_handle_is_contended() {
        let path = std::env::temp_dir().join(format!("cutex-file-lock-{}.lock", uuid::Uuid::new_v4()));
        let open = || std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&path).unwrap();
        let (held, other) = (open(), open());
        held.try_lock_exclusive().unwrap();
        let error = other.try_lock_exclusive().unwrap_err();
        assert!(lock_contended(&error), "{error:?} (raw {:?})", error.raw_os_error());
        FileExt::unlock(&held).unwrap();
        other.try_lock_exclusive().unwrap();
        drop((held, other));
        let _ = std::fs::remove_file(path);
    }
}
