//! File identity stamps for in-memory caches of large JSON stores.
//!
//! Every durable store this is used for is replaced atomically (temp file +
//! rename), so a new version is a new file: on Unix a new inode and change
//! time, everywhere a new modification time. A cache keyed by the stamp is
//! therefore reparsed exactly when another writer (this or another process)
//! replaced the file.

use std::path::Path;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    #[cfg(unix)]
    inode: (u64, u64),
    #[cfg(unix)]
    changed: (i64, i64),
}

impl FileStamp {
    /// `None` when the file does not exist or is not a regular file.
    pub fn of(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        if !metadata.is_file() {
            return None;
        }
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Some(Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            #[cfg(unix)]
            inode: (metadata.dev(), metadata.ino()),
            #[cfg(unix)]
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_replace_with_same_size_changes_the_stamp() {
        let dir = std::env::temp_dir().join(format!("cutex-file-stamp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("store.json");
        std::fs::write(&path, b"{\"a\":1}").unwrap();
        let first = FileStamp::of(&path).unwrap();
        assert_eq!(FileStamp::of(&path).unwrap(), first);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let temp = dir.join("store.json.tmp");
        std::fs::write(&temp, b"{\"a\":2}").unwrap();
        std::fs::rename(&temp, &path).unwrap();
        assert_ne!(FileStamp::of(&path).unwrap(), first);
        assert!(FileStamp::of(&dir.join("missing.json")).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
