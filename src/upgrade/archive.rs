//! Safe `.tar.gz` unpacking for release artifacts (RFC §8.4).
//!
//! Only regular files and directories are accepted; absolute paths, `..`,
//! drive prefixes, backslashes, symlinks, hard links and device entries are
//! refused. ustar names (`prefix/name`), GNU long names (`L`) and pax `path`
//! records (`x`) are understood; global pax headers are skipped. The caller
//! unpacks into a fresh staging directory and renames it into place.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context};

const BLOCK: usize = 512;
pub const MAX_ENTRIES: usize = 20_000;
pub const MAX_TOTAL_BYTES: u64 = 4 << 30;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub size: u64,
    pub executable: bool,
}

fn octal(field: &[u8]) -> anyhow::Result<u64> {
    // GNU base-256 for large sizes.
    if field.first().is_some_and(|b| b & 0x80 != 0) {
        let mut value: u64 = 0;
        for (i, b) in field.iter().enumerate() {
            let b = if i == 0 { b & 0x7f } else { *b };
            value = value.checked_mul(256).and_then(|v| v.checked_add(b as u64)).context("tar size overflow")?;
        }
        return Ok(value);
    }
    let text: String = field.iter().take_while(|b| **b != 0).map(|b| *b as char).collect();
    let text = text.trim();
    if text.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(text, 8).context("tar octal field malformed")
}

fn cstr(field: &[u8]) -> String {
    String::from_utf8_lossy(&field[..field.iter().position(|b| *b == 0).unwrap_or(field.len())]).into_owned()
}

/// A safe relative path (`a/b/c`), `./` prefixes removed; trailing `/` dropped.
pub fn sanitize(path: &str) -> anyhow::Result<String> {
    let mut parts = Vec::new();
    ensure!(!path.contains('\\') && !path.contains('\0'), "archive path `{path}` has forbidden characters");
    ensure!(!path.starts_with('/'), "archive path `{path}` is absolute");
    for part in path.split('/') {
        match part {
            "" | "." => continue,
            ".." => bail!("archive path `{path}` leaves the destination"),
            part => {
                ensure!(!part.contains(':'), "archive path `{path}` has a drive or stream prefix");
                parts.push(part);
            }
        }
    }
    ensure!(!parts.is_empty(), "archive path `{path}` is empty");
    Ok(parts.join("/"))
}

fn pax_path(data: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(data);
    let mut rest: &str = &text;
    let mut found = None;
    while !rest.is_empty() {
        let (len, _) = rest.split_once(' ')?;
        let len: usize = len.parse().ok()?;
        let record = rest.get(..len)?;
        rest = &rest[len..];
        let body = record.split_once(' ')?.1.trim_end_matches('\n');
        if let Some(path) = body.strip_prefix("path=") {
            found = Some(path.to_string());
        }
    }
    found
}

fn read_exact_or_eof(reader: &mut impl Read, buf: &mut [u8]) -> anyhow::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = reader.read(&mut buf[filled..])?;
        if n == 0 {
            ensure!(filled == 0, "archive truncated");
            return Ok(false);
        }
        filled += n;
    }
    Ok(true)
}

fn read_data(reader: &mut impl Read, size: u64, limit: usize) -> anyhow::Result<Vec<u8>> {
    ensure!(size as usize <= limit, "archive header record too large");
    let padded = (size as usize).div_ceil(BLOCK) * BLOCK;
    let mut data = vec![0u8; padded];
    ensure!(read_exact_or_eof(reader, &mut data)?, "archive truncated");
    data.truncate(size as usize);
    Ok(data)
}

/// Unpack a gzip-compressed tar stream into `dest` (created if missing).
pub fn unpack_tar_gz(reader: impl Read, dest: &Path) -> anyhow::Result<Vec<Entry>> {
    unpack_tar(flate2::read::MultiGzDecoder::new(reader), dest)
}

pub fn unpack_tar_gz_file(archive: &Path, dest: &Path) -> anyhow::Result<Vec<Entry>> {
    let file = std::fs::File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    unpack_tar_gz(std::io::BufReader::new(file), dest)
}

/// Like [`unpack_tar_gz_file`], but materialise safe file links as regular files. This is reserved
/// for the pinned python-build-standalone payload: component/release archives still refuse links.
pub fn unpack_tar_gz_materialized_links_file(archive: &Path, dest: &Path) -> anyhow::Result<Vec<Entry>> {
    let file = std::fs::File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    unpack_tar_inner(flate2::read::MultiGzDecoder::new(std::io::BufReader::new(file)), dest, true)
}

pub fn unpack_tar(mut reader: impl Read, dest: &Path) -> anyhow::Result<Vec<Entry>> {
    unpack_tar_inner(&mut reader, dest, false)
}

fn link_target(path: &str, link: &str, hard: bool) -> anyhow::Result<String> {
    ensure!(!link.contains('\\') && !link.contains('\0') && !link.starts_with('/'), "archive link `{path}` has unsafe target `{link}`");
    let mut parts: Vec<&str> = if hard { Vec::new() } else { path.rsplit_once('/').map(|(parent, _)| parent.split('/').collect()).unwrap_or_default() };
    for part in link.split('/') {
        match part {
            "" | "." => {}
            ".." => ensure!(parts.pop().is_some(), "archive link `{path}` leaves the destination"),
            part => {
                ensure!(!part.contains(':'), "archive link `{path}` has a drive or stream prefix");
                parts.push(part);
            }
        }
    }
    sanitize(&parts.join("/"))
}

fn unpack_tar_inner(mut reader: impl Read, dest: &Path, materialize_links: bool) -> anyhow::Result<Vec<Entry>> {
    std::fs::create_dir_all(dest)?;
    let mut entries = Vec::new();
    let mut pending_links: Vec<(String, String, bool)> = Vec::new();
    let mut entry_count = 0usize;
    let mut total: u64 = 0;
    let mut long_name: Option<String> = None;
    let mut header = [0u8; BLOCK];
    loop {
        if !read_exact_or_eof(&mut reader, &mut header)? {
            break; // tolerate a missing end-of-archive marker
        }
        if header.iter().all(|b| *b == 0) {
            break;
        }
        let computed: u64 = header.iter().enumerate().map(|(i, b)| if (148..156).contains(&i) { 32 } else { *b as u64 }).sum();
        ensure!(octal(&header[148..156])? == computed, "archive header checksum mismatch");
        let size = octal(&header[124..136])?;
        let typeflag = header[156];
        match typeflag {
            b'L' | b'x' => {
                let data = read_data(&mut reader, size, 64 * 1024)?;
                long_name = if typeflag == b'L' { Some(cstr(&data)) } else { pax_path(&data).or(long_name) };
                continue;
            }
            b'g' => {
                read_data(&mut reader, size, 64 * 1024)?;
                continue;
            }
            _ => {}
        }
        let name = match long_name.take() {
            Some(name) => name,
            None => {
                let name = cstr(&header[0..100]);
                let prefix = if &header[257..262] == b"ustar" { cstr(&header[345..500]) } else { String::new() };
                if prefix.is_empty() { name } else { format!("{prefix}/{name}") }
            }
        };
        let path = sanitize(&name)?;
        entry_count += 1;
        ensure!(entry_count <= MAX_ENTRIES, "archive has too many entries");
        let target = dest.join(PathBuf::from(&path));
        match typeflag {
            b'5' => {
                std::fs::create_dir_all(&target)?;
                ensure!(size == 0, "directory entry with data");
            }
            b'0' | 0 | b'7' => {
                total = total.checked_add(size).context("archive size overflow")?;
                ensure!(total <= MAX_TOTAL_BYTES, "archive expands beyond the size limit");
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                // Never follow anything already at the target.
                if let Ok(meta) = std::fs::symlink_metadata(&target) {
                    ensure!(meta.is_file(), "archive entry `{path}` replaces a non-file");
                    std::fs::remove_file(&target)?;
                }
                let mut out = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)
                    .with_context(|| format!("create {}", target.display()))?;
                let mut remaining = size;
                let mut buf = [0u8; 64 * 1024];
                while remaining > 0 {
                    let want = remaining.min(buf.len() as u64) as usize;
                    let n = reader.read(&mut buf[..want])?;
                    ensure!(n > 0, "archive truncated");
                    out.write_all(&buf[..n])?;
                    remaining -= n as u64;
                }
                out.sync_all()?;
                let pad = (BLOCK - (size as usize % BLOCK)) % BLOCK;
                if pad > 0 {
                    let mut skip = vec![0u8; pad];
                    ensure!(read_exact_or_eof(&mut reader, &mut skip)?, "archive truncated");
                }
                let mode = octal(&header[100..108]).unwrap_or(0o644);
                let executable = mode & 0o111 != 0;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }))?;
                }
                entries.push(Entry { path, size, executable });
            }
            b'1' | b'2' if materialize_links => {
                ensure!(size == 0, "archive link `{path}` has data");
                let link = cstr(&header[157..257]);
                pending_links.push((path.clone(), link_target(&path, &link, typeflag == b'1')?, octal(&header[100..108]).unwrap_or(0o644) & 0o111 != 0));
            }
            b'1' | b'2' => bail!("archive entry `{path}` is a link (refused)"),
            other => bail!("archive entry `{path}` has unsupported type `{}`", other as char),
        }
    }
    while !pending_links.is_empty() {
        let mut remaining = Vec::new();
        let mut progressed = false;
        for (path, source_path, executable) in pending_links {
            let source = dest.join(PathBuf::from(&source_path));
            if !source.is_file() {
                remaining.push((path, source_path, executable));
                continue;
            }
            let target = dest.join(PathBuf::from(&path));
            if let Some(parent) = target.parent() { std::fs::create_dir_all(parent)?; }
            ensure!(!target.exists(), "archive link `{path}` replaces an existing entry");
            let size = std::fs::copy(&source, &target)?;
            total = total.checked_add(size).context("archive size overflow")?;
            ensure!(total <= MAX_TOTAL_BYTES, "archive expands beyond the size limit");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }))?;
            }
            entries.push(Entry { path, size, executable });
            progressed = true;
        }
        ensure!(progressed, "archive has unresolved or non-file links");
        pending_links = remaining;
    }
    Ok(entries)
}

/// Minimal ustar + gzip writer (tests and `cutex upgrade` fixtures).
#[cfg(test)]
pub(crate) mod writer {
    use super::*;

    pub enum Item<'a> {
        File { path: &'a str, data: &'a [u8], mode: u32 },
        Dir(&'a str),
        Symlink { path: &'a str, target: &'a str },
        Hardlink { path: &'a str, target: &'a str },
        LongFile { path: &'a str, data: &'a [u8] },
    }

    fn header(name: &str, size: u64, mode: u32, typeflag: u8, link: &str) -> [u8; BLOCK] {
        let mut h = [0u8; BLOCK];
        h[..name.len().min(100)].copy_from_slice(&name.as_bytes()[..name.len().min(100)]);
        h[100..107].copy_from_slice(format!("{mode:07o}").as_bytes());
        h[108..115].copy_from_slice(b"0000000");
        h[116..123].copy_from_slice(b"0000000");
        h[124..135].copy_from_slice(format!("{size:011o}").as_bytes());
        h[136..147].copy_from_slice(b"00000000000");
        h[156] = typeflag;
        h[157..157 + link.len()].copy_from_slice(link.as_bytes());
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        h[148..156].copy_from_slice(b"        ");
        let sum: u64 = h.iter().map(|b| *b as u64).sum();
        h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        h
    }

    fn data(out: &mut Vec<u8>, bytes: &[u8]) {
        out.extend_from_slice(bytes);
        out.resize(out.len().div_ceil(BLOCK) * BLOCK, 0);
    }

    pub fn tar(items: &[Item<'_>]) -> Vec<u8> {
        let mut out = Vec::new();
        for item in items {
            match item {
                Item::File { path, data: bytes, mode } => {
                    out.extend_from_slice(&header(path, bytes.len() as u64, *mode, b'0', ""));
                    data(&mut out, bytes);
                }
                Item::Dir(path) => out.extend_from_slice(&header(path, 0, 0o755, b'5', "")),
                Item::Symlink { path, target } => out.extend_from_slice(&header(path, 0, 0o777, b'2', target)),
                Item::Hardlink { path, target } => out.extend_from_slice(&header(path, 0, 0o777, b'1', target)),
                Item::LongFile { path, data: bytes } => {
                    let mut name = path.as_bytes().to_vec();
                    name.push(0);
                    out.extend_from_slice(&header("././@LongLink", name.len() as u64, 0o644, b'L', ""));
                    data(&mut out, &name);
                    out.extend_from_slice(&header("truncated", bytes.len() as u64, 0o644, b'0', ""));
                    data(&mut out, bytes);
                }
            }
        }
        out.extend_from_slice(&[0u8; BLOCK * 2]);
        out
    }

    pub fn tar_gz(items: &[Item<'_>]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&tar(items)).unwrap();
        encoder.finish().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::writer::{tar_gz, Item};
    use super::*;

    fn temp() -> PathBuf {
        std::env::temp_dir().join(format!("cutex-archive-{}", uuid::Uuid::new_v4().simple()))
    }

    #[test]
    fn unpacks_files_dirs_and_long_names() {
        let dest = temp();
        let long = format!("dir/{}/file.txt", "x".repeat(120));
        let bytes = tar_gz(&[
            Item::Dir("bin/"),
            Item::File { path: "bin/cutex", data: b"#!/bin/sh\necho hi\n", mode: 0o755 },
            Item::File { path: "./cutex-mcp", data: &vec![7u8; 1500], mode: 0o755 },
            Item::File { path: "receipt.json", data: b"{}", mode: 0o644 },
            Item::LongFile { path: &long, data: b"long" },
        ]);
        let entries = unpack_tar_gz(&bytes[..], &dest).unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(std::fs::read(dest.join("cutex-mcp")).unwrap().len(), 1500);
        assert_eq!(std::fs::read_to_string(dest.join(&long)).unwrap(), "long");
        assert!(entries.iter().find(|e| e.path == "bin/cutex").unwrap().executable);
        assert!(!entries.iter().find(|e| e.path == "receipt.json").unwrap().executable);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dest.join("bin/cutex")).unwrap().permissions().mode() & 0o777, 0o755);
        }
        std::fs::remove_dir_all(dest).unwrap();
    }

    #[test]
    fn refuses_escapes_links_and_garbage() {
        for items in [
            vec![Item::File { path: "../evil", data: b"x", mode: 0o644 }],
            vec![Item::File { path: "/etc/evil", data: b"x", mode: 0o644 }],
            vec![Item::File { path: "a/../../evil", data: b"x", mode: 0o644 }],
            vec![Item::File { path: "c:evil", data: b"x", mode: 0o644 }],
            vec![Item::Symlink { path: "link", target: "/etc/passwd" }],
        ] {
            let dest = temp();
            assert!(unpack_tar_gz(&tar_gz(&items)[..], &dest).is_err());
            let _ = std::fs::remove_dir_all(dest);
        }
        let dest = temp();
        assert!(unpack_tar_gz(&b"not a gzip stream"[..], &dest).is_err());
        let mut truncated = super::writer::tar(&[Item::File { path: "f", data: &[1u8; 2000], mode: 0o644 }]);
        truncated.truncate(1200);
        assert!(unpack_tar(&truncated[..], &dest).is_err());
        let _ = std::fs::remove_dir_all(dest);
    }

    #[test]
    fn pinned_payload_mode_materializes_safe_links_only() {
        let dest = temp();
        let archive = dest.with_extension("tar.gz");
        std::fs::write(&archive, tar_gz(&[
            Item::Symlink { path: "python/bin/python", target: "python3" },
            Item::Symlink { path: "python/bin/python3", target: "python3.13" },
            Item::Hardlink { path: "python/bin/python-hard", target: "python/bin/python3.13" },
            Item::File { path: "python/bin/python3.13", data: b"python", mode: 0o755 },
        ])).unwrap();
        let entries = unpack_tar_gz_materialized_links_file(&archive, &dest).unwrap();
        assert_eq!(std::fs::read(dest.join("python/bin/python3")).unwrap(), b"python");
        assert_eq!(std::fs::read(dest.join("python/bin/python")).unwrap(), b"python");
        assert_eq!(std::fs::read(dest.join("python/bin/python-hard")).unwrap(), b"python");
        assert!(!std::fs::symlink_metadata(dest.join("python/bin/python3")).unwrap().file_type().is_symlink());
        assert_eq!(entries.len(), 4);
        std::fs::remove_dir_all(&dest).unwrap();
        std::fs::remove_file(&archive).unwrap();

        let dest = temp();
        let archive = dest.with_extension("tar.gz");
        std::fs::write(&archive, tar_gz(&[Item::Symlink { path: "python/bin/python3", target: "../../../escape" }])).unwrap();
        assert!(unpack_tar_gz_materialized_links_file(&archive, &dest).is_err());
        let _ = std::fs::remove_dir_all(&dest);
        std::fs::remove_file(&archive).unwrap();

        for target in ["..\\escape", "cycle-b"] {
            let dest = temp();
            let archive = dest.with_extension("tar.gz");
            let items = if target == "cycle-b" {
                vec![Item::Symlink { path: "cycle-a", target: "cycle-b" }, Item::Symlink { path: "cycle-b", target: "cycle-a" }]
            } else {
                vec![Item::Symlink { path: "python/link", target }]
            };
            std::fs::write(&archive, tar_gz(&items)).unwrap();
            assert!(unpack_tar_gz_materialized_links_file(&archive, &dest).is_err());
            let _ = std::fs::remove_dir_all(&dest);
            std::fs::remove_file(&archive).unwrap();
        }
    }

    #[test]
    fn sanitize_paths() {
        assert_eq!(sanitize("./a//b/").unwrap(), "a/b");
        for bad in ["", ".", "..", "a/..", "a\\b", "/x"] {
            assert!(sanitize(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn pax_path_records() {
        let record = |k: &str, v: &str| {
            let body = format!(" {k}={v}\n");
            let mut n = body.len() + 1;
            loop {
                let total = body.len() + n.to_string().len();
                if total == n {
                    break;
                }
                n = total;
            }
            format!("{n}{body}")
        };
        let data = format!("{}{}", record("mtime", "1"), record("path", "deep/name.txt"));
        assert_eq!(pax_path(data.as_bytes()).as_deref(), Some("deep/name.txt"));
    }
}
