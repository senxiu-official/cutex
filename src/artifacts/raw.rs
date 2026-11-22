//! Handle identity recorded for external raw downloads. No process-wide cache.
use serde::{Deserialize, Serialize};
use std::{fs::File, time::UNIX_EPOCH};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceStamp {
    pub identity: String,
    pub bytes: u64,
    pub modified_ns: u128,
}

impl SourceStamp {
    pub fn of(file: &File) -> std::io::Result<Self> {
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(std::io::Error::other("not a regular file"));
        }
        #[cfg(unix)]
        let identity = {
            use std::os::unix::fs::MetadataExt;
            format!("{}:{}", metadata.dev(), metadata.ino())
        };
        #[cfg(windows)]
        let identity = {
            let id = crate::platform::private_fs::identity(file).map_err(std::io::Error::other)?;
            format!("{}:{:02x?}", id.volume, id.file_id)
        };
        #[cfg(not(any(unix, windows)))]
        let identity = return Err(std::io::Error::other("file identity unsupported"));
        Ok(Self {
            identity,
            bytes: metadata.len(),
            modified_ns: metadata
                .modified()?
                .duration_since(UNIX_EPOCH)
                .map_err(std::io::Error::other)?
                .as_nanos(),
        })
    }
}
