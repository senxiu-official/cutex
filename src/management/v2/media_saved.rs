//! Native r55 local images: registration is metadata only; only an owner media
//! GET opens the file. Never walk arbitrary tool results looking for paths.
use super::{sniff, write_atomic, MediaStore, Meta, Replacer, REF_PREFIX};
use anyhow::ensure;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
pub(super) const MAX_META_BYTES: u64 = 128 * 1024;
const MAX_DESCRIPTORS: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Source {
    saved_path: String,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    byte_size: Option<u64>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl Source {
    fn valid(&self, typed: bool) -> bool {
        let path = Path::new(&self.saved_path);
        path.is_absolute()
            && self.saved_path.len() <= 32 * 1024
            && !path.components().any(|c| matches!(c, Component::ParentDir))
            && self.byte_size.is_none_or(|n| n > 0 && n <= MAX_FILE_BYTES)
            && self.sha256.as_ref().is_none_or(|s| {
                s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            })
            && self.mime_type.as_deref().is_none_or(|m| {
                matches!(m, "image/png" | "image/jpeg" | "image/gif" | "image/webp")
            })
            && (!typed
                || (self.byte_size.is_some()
                    && self.sha256.is_some()
                    && self.mime_type.is_some()
                    && self.width.is_some_and(|v| v > 0)
                    && self.height.is_some_and(|v| v > 0)))
    }

    pub(super) fn read(&self) -> anyhow::Result<(String, Vec<u8>)> {
        ensure!(self.valid(false), "invalid saved image metadata");
        let path = Path::new(&self.saved_path);
        // Reject linked ancestors as well as the leaf. The opened handle is
        // checked again, preventing a substituted leaf from bypassing the cap.
        for parent in path.ancestors() {
            let metadata = fs::symlink_metadata(parent)?;
            ensure!(!linked(&metadata), "linked saved image path");
        }
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x00200000).share_mode(1); // no reparse / no concurrent writes
        }
        let file = options.open(path)?;
        verify_opened_path(&file, path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && !linked(&metadata) && metadata.len() <= MAX_FILE_BYTES,
            "saved image is not a bounded regular file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let before = fs::symlink_metadata(path)?;
            ensure!(
                metadata.nlink() == 1
                    && metadata.dev() == before.dev()
                    && metadata.ino() == before.ino(),
                "linked or replaced saved image"
            );
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
            };
            let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            ensure!(
                unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0
                    && info.nNumberOfLinks == 1,
                "linked saved image"
            );
        }
        ensure!(
            self.byte_size.is_none_or(|n| n == metadata.len()),
            "saved image size changed"
        );
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_FILE_BYTES && bytes.len() as u64 == metadata.len(),
            "saved image size changed"
        );
        ensure!(
            self.sha256
                .as_ref()
                .is_none_or(|expected| hex(&bytes) == *expected),
            "saved image hash changed"
        );
        let mime = sniff(&bytes);
        ensure!(
            mime != "application/octet-stream"
                && self
                    .mime_type
                    .as_deref()
                    .is_none_or(|expected| expected == mime),
            "saved image MIME changed"
        );
        let actual = dimensions(&bytes, mime)?;
        ensure!(
            actual.0 > 0 && actual.1 > 0,
            "invalid saved image dimensions"
        );
        if let (Some(width), Some(height)) = (self.width, self.height) {
            ensure!(actual == (width, height), "saved image dimensions changed");
        }
        // imageGeneration carries no content hash. Validate its pixels as well
        // as its signature, with a separate decompression allocation limit.
        // Its native output is PNG; unsupported codecs fail explicitly.
        if self.sha256.is_none() {
            let mut reader =
                image::ImageReader::new(std::io::Cursor::new(&bytes)).with_guessed_format()?;
            let mut limits = image::Limits::default();
            limits.max_alloc = Some(64 * 1024 * 1024);
            limits.max_image_width = Some(16384);
            limits.max_image_height = Some(16384);
            reader.limits(limits);
            reader.decode()?;
        }
        Ok((mime.to_string(), bytes))
    }
}

fn dimensions(bytes: &[u8], mime: &str) -> anyhow::Result<(u32, u32)> {
    // PNG/JPEG codecs already ship in cutex. GIF and WebP dimensions can be
    // read from their bounded headers without adding codecs or decoding pixels.
    if mime == "image/gif" && bytes.len() >= 10 {
        return Ok((
            u16::from_le_bytes([bytes[6], bytes[7]]) as u32,
            u16::from_le_bytes([bytes[8], bytes[9]]) as u32,
        ));
    }
    if mime == "image/webp" {
        if bytes.len() >= 30 && &bytes[12..16] == b"VP8X" {
            let le24 = |s: &[u8]| u32::from_le_bytes([s[0], s[1], s[2], 0]) + 1;
            return Ok((le24(&bytes[24..27]), le24(&bytes[27..30])));
        }
        if bytes.len() >= 25 && &bytes[12..16] == b"VP8L" && bytes[20] == 0x2f {
            let v = u32::from_le_bytes(bytes[21..25].try_into()?);
            return Ok(((v & 0x3fff) + 1, ((v >> 14) & 0x3fff) + 1));
        }
        if bytes.len() >= 30 && &bytes[12..16] == b"VP8 " && bytes[23..26] == [0x9d, 1, 0x2a] {
            return Ok((
                (u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3fff) as u32,
                (u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3fff) as u32,
            ));
        }
        anyhow::bail!("invalid saved WebP header");
    }
    Ok(image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()?
        .into_dimensions()?)
}

fn linked(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    metadata.file_type().is_symlink()
}

fn verify_opened_path(file: &fs::File, expected: &Path) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    let actual = {
        use std::os::fd::AsRawFd;
        fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?
    };
    #[cfg(target_os = "macos")]
    let actual = {
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStrExt;
        let mut buffer = [0u8; 4096];
        ensure!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buffer.as_mut_ptr()) } == 0,
            "saved image handle path unavailable"
        );
        let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
        PathBuf::from(std::ffi::OsStr::from_bytes(&buffer[..end]))
    };
    #[cfg(windows)]
    let actual = {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;
        let mut buffer = vec![0u16; 32768];
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                0,
            )
        };
        ensure!(
            length > 0 && (length as usize) < buffer.len(),
            "saved image handle path unavailable"
        );
        PathBuf::from(crate::agent_management::strip_verbatim_prefix(
            &String::from_utf16_lossy(&buffer[..length as usize]),
        ))
    };
    let expected: PathBuf = expected.components().collect();
    #[cfg(windows)]
    ensure!(
        actual
            .to_string_lossy()
            .replace('/', "\\")
            .eq_ignore_ascii_case(
                &crate::agent_management::strip_verbatim_prefix(&expected.to_string_lossy())
                    .replace('/', "\\")
            ),
        "saved image path changed while opening"
    );
    #[cfg(not(windows))]
    ensure!(actual == expected, "saved image path changed while opening");
    Ok(())
}

pub(super) fn read_meta(path: &Path) -> Option<Meta> {
    let file = fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_META_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_META_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_META_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

impl MediaStore {
    fn register_saved(&self, source: &Source, session: &str) -> anyhow::Result<String> {
        ensure!(source.valid(false), "invalid saved image descriptor");
        let descriptor = serde_json::to_vec(source)?;
        // Different namespace from legacy base64-text hashes and from the raw
        // pixel hash: a path-only image has no pixel digest until GET.
        let sha = hex(&[b"cutex-saved-image-v1:".as_slice(), &descriptor].concat());
        let _lock = self.lock()?;
        let (_, path) = self.paths(&sha);
        let mut meta = read_meta(&path).unwrap_or_default();
        ensure!(
            meta.source.as_ref().is_none_or(|old| old == source),
            "saved descriptor collision"
        );
        meta.source = Some(source.clone());
        meta.mime = source.mime_type.clone().unwrap_or_default();
        if !meta.sessions.iter().any(|known| known == session) {
            meta.sessions.push(session.into());
        }
        let bytes = serde_json::to_vec(&meta)?;
        ensure!(
            bytes.len() as u64 <= MAX_META_BYTES,
            "saved image grants exceed metadata limit"
        );
        write_atomic(&path, &bytes)?;
        Ok(sha)
    }
}

pub(super) fn evict_descriptors(dir: &Path) {
    evict_descriptors_to(dir, MAX_DESCRIPTORS);
}

fn evict_descriptors_to(dir: &Path, limit: usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut descriptors: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|e| {
            e.path().extension().is_some_and(|x| x == "json")
                && !e.path().with_extension("bin").exists()
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    descriptors.sort();
    let excess = descriptors.len().saturating_sub(limit);
    for (_, path) in descriptors.into_iter().take(excess) {
        let _ = fs::remove_file(path);
    }
}

impl Replacer<'_> {
    fn saved_reference(&mut self, source: Source) -> Option<String> {
        match self.store.register_saved(&source, self.session) {
            Ok(sha) => {
                self.replaced += 1;
                self.recorded
                    .push((sha.clone(), source.mime_type.unwrap_or_default()));
                Some(format!("{REF_PREFIX}{}/{sha}", self.session))
            }
            Err(_) => {
                self.failed += 1;
                None
            }
        }
    }

    /// Entry points are fixed history envelopes, never arbitrary nested JSON.
    pub(super) fn replace_saved_body(&mut self, body: &mut Value) {
        if !self.saved_files {
            return;
        }
        if let Some(turns) = body
            .pointer_mut("/native/result/thread/turns")
            .and_then(Value::as_array_mut)
        {
            for turn in turns {
                self.replace_saved_turn(turn);
            }
        }
    }

    pub(in crate::management::v2) fn replace_saved_turn(&mut self, turn: &mut Value) {
        if !self.saved_files {
            return;
        }
        let Some(items) = turn.get_mut("items").and_then(Value::as_array_mut) else {
            return;
        };
        for item in items {
            if !item.get("id").is_some_and(Value::is_string) {
                continue;
            }
            match item.get("type").and_then(Value::as_str) {
                Some("imageGeneration")
                    if item
                        .get("result")
                        .is_some_and(|v| v.is_null() || v.as_str() == Some("")) =>
                {
                    let Some(path) = item.get("savedPath").and_then(Value::as_str) else {
                        continue;
                    };
                    let source = Source {
                        saved_path: path.into(),
                        mime_type: None,
                        sha256: None,
                        byte_size: None,
                        width: None,
                        height: None,
                    };
                    if source.valid(false) {
                        if let Some(reference) = self.saved_reference(source) {
                            item["result"] = Value::String(reference);
                        }
                    }
                }
                Some("functionCallOutput") => {
                    if let Some(parts) = item.get_mut("output").and_then(Value::as_array_mut) {
                        for part in parts {
                            self.replace_saved_part(part);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn replace_saved_part(&mut self, part: &mut Value) {
        if part.get("type").and_then(Value::as_str) != Some("input_image") {
            return;
        }
        let Some(value) = part.get("saved_image") else {
            return;
        };
        let Ok(source) = serde_json::from_value::<Source>(value.clone()) else {
            self.failed += 1;
            return;
        };
        if !source.valid(true) {
            self.failed += 1;
            return;
        }
        if let Some(reference) = self.saved_reference(source) {
            let map = part.as_object_mut().expect("typed image object");
            map.remove("saved_image");
            map.insert("image_url".into(), Value::String(reference));
        }
    }
}

fn supports_version(claude: bool, version: Option<&str>) -> bool {
    !claude
        && version
            .and_then(|v| v.rsplit_once("-r"))
            .and_then(|(base, r)| {
                // Only exact release identities, not r55-dev or a path containing r55.
                // Numeric hotfix releases (for example r55.1) retain the feature.
                (base.split('.').count() == 3
                    && base
                        .split('.')
                        .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                    && r.split('.').all(|s| {
                        !s.is_empty()
                            && s.bytes().all(|b| b.is_ascii_digit())
                            && s.parse::<u32>().is_ok()
                    }))
                .then(|| r.split('.').next()?.parse::<u32>().ok())
                .flatten()
            })
            .is_some_and(|revision| revision >= 55)
}

/// Cache the expensive byte identity by the running executable's path and file
/// identity. Every call still observes the process's actual executable. A new
/// occurrence or an executable replacement cannot borrow the selected pin.
fn running_version(
    registry: &crate::launch::version_registry::NativeVersionRegistry,
    pid: u32,
) -> Option<String> {
    #[cfg(target_os = "linux")]
    let executable = fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    #[cfg(target_os = "macos")]
    let executable = crate::platform::macos_proc::exe(pid).ok()??;
    #[cfg(windows)]
    let executable = {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return None;
        }
        let mut buffer = vec![0u16; 32768];
        let mut length = buffer.len() as u32;
        let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut length) };
        unsafe { CloseHandle(handle) };
        if ok == 0 {
            return None;
        }
        PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize]))
    };
    let metadata = fs::metadata(&executable).ok()?;
    #[cfg(unix)]
    let file_id = {
        use std::os::unix::fs::MetadataExt;
        (metadata.dev(), metadata.ino())
    };
    #[cfg(not(unix))]
    let file_id = (0u64, 0u64);
    type Identity = (PathBuf, u64, std::time::SystemTime, (u64, u64));
    static VERSIONS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<Identity, String>>,
    > = std::sync::OnceLock::new();
    let identity = (
        executable.clone(),
        metadata.len(),
        metadata.modified().ok()?,
        file_id,
    );
    let mut versions = VERSIONS.get_or_init(Default::default).lock().ok()?;
    if let Some(version) = versions.get(&identity) {
        return Some(version.clone());
    }
    let version = registry.runtime_version_for_executable(&executable)?;
    if versions.len() >= 256 {
        versions.clear();
    }
    versions.insert(identity, version.clone());
    Some(version)
}

fn occurrence_version(
    pid: Option<u32>,
    running: impl FnOnce(u32) -> Option<String>,
    pinned: impl FnOnce() -> Option<String>,
) -> Option<String> {
    match pid {
        Some(pid) => running(pid),
        None => pinned(),
    }
}

/// Resolve this session's runtime, never the globally selected/latest runtime.
/// A running occurrence must not fall back to its next-launch contract.
pub fn session_supports_saved_images(session: &str) -> bool {
    let Ok(Some(record)) = crate::session::store::with_cutex_session_store(|store| {
        store.sessions.get(session).cloned()
    }) else {
        return false;
    };
    if record.runtime_backend
        == crate::session::model::CutexSessionRuntimeBackend::ClaudeSubscription
        || record.claude_session.is_some()
    {
        return false;
    }
    let Some(registry) = crate::launch::version_registry::load_relaxed() else {
        return false;
    };
    let version = occurrence_version(
        record
            .app_server_runtime
            .as_ref()
            .map(|r| r.pid)
            .or(record.runtime_pid),
        |pid| running_version(&registry, pid),
        || {
            record
                .explicit_launch
                .as_ref()
                .and_then(|contract| registry.runtime_version_for_launch_contract(contract))
        },
    );
    supports_version(false, version.as_deref())
}

#[cfg(test)]
#[path = "media_saved_tests.rs"]
mod tests;
