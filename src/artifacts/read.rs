//! Reading a version's bytes in windows: the blob, or for `external`
//! versions the registered file while it still matches (size + mtime, and
//! the path rules at read time).

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use super::store::ArtifactStore;
use super::ArtifactVersion;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Located {
    /// The file to read, `None` when the content is unavailable.
    pub path: Option<PathBuf>,
    /// `None` for blobs; for external versions whether the file on disk no
    /// longer matches the registered size / mtime (or is gone / refused).
    pub external_changed: Option<bool>,
}

pub fn locate(store: &ArtifactStore, version: &ArtifactVersion) -> Located {
    if version.storage == "blob" {
        let path = version.sha256.as_deref().map(|sha| store.blob_path(sha)).filter(|path| path.is_file());
        return Located { path, external_changed: None };
    }
    let path = PathBuf::from(&version.source_path);
    let current = super::paths::canonical(&path).filter(|real| super::paths::forbidden_reason(real).is_none());
    let unchanged = current.as_ref().and_then(|real| std::fs::metadata(real).ok()).is_some_and(|metadata| {
        metadata.is_file() && metadata.len() == version.bytes && super::paths::mtime_ms(&metadata) == version.source_mtime_ms
    });
    Located { path: if unchanged { current } else { None }, external_changed: Some(!unchanged) }
}

/// Up to `max` bytes from `offset`, and the file's size.
pub fn window(path: &std::path::Path, offset: u64, max: usize) -> std::io::Result<(Vec<u8>, u64)> {
    let mut file = std::fs::File::open(path)?;
    let total = file.metadata()?.len();
    let mut bytes = Vec::new();
    if offset < total {
        file.seek(SeekFrom::Start(offset))?;
        file.take(max as u64).read_to_end(&mut bytes)?;
    }
    Ok((bytes, total))
}

/// The window as text: a character cut at the end is left for the next
/// window when more follows. `None` when the bytes are not UTF-8 text.
pub fn text_of(bytes: &[u8], more_follows: bool) -> Option<String> {
    if bytes.contains(&0) {
        return None;
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => Some(text.to_string()),
        Err(error) if error.error_len().is_none() && more_follows => {
            Some(String::from_utf8_lossy(&bytes[..error.valid_up_to()]).to_string())
        }
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_and_text_boundaries() {
        let dir = std::env::temp_dir().join(format!("cutex-artifact-read-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("t.txt");
        std::fs::write(&file, "aé b").unwrap();
        let (bytes, total) = window(&file, 0, 2).unwrap();
        assert_eq!(total, 5);
        assert_eq!(text_of(&bytes, true).as_deref(), Some("a"), "é is cut: left for the next window");
        let (bytes, _) = window(&file, 1, 10).unwrap();
        assert_eq!(text_of(&bytes, false).as_deref(), Some("é b"));
        let (bytes, _) = window(&file, 9, 10).unwrap();
        assert!(bytes.is_empty());
        assert_eq!(text_of(&[0xff, 0x41], false), None);
        assert_eq!(text_of(&[0x41, 0], false), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
