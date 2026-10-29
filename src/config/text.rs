//! Small text-file helpers used by profile/config materialization.
//!
//! Every caller writes a credential or a Codex config (`auth.json`,
//! `config.toml`, a profile's `models.json` / status items, the active
//! `~/.codex` copies), so writes go through the atomic helper and are
//! owner-private: the temp file is created 0600 (Unix) / with a protected
//! owner-only DACL (Windows) before a byte is written, synced, then renamed
//! over the target. A crash or a full disk leaves the old file whole, and a
//! secret never exists with wider permissions.

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;

use crate::config::atomic::write_private_bytes_atomic;

pub fn read_optional_text(path: &Path) -> anyhow::Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }

    let data = fs::read_to_string(path)
        .with_context(|| format!("Failed to read file: {}", path.display()))?;
    Ok(Some(data))
}

pub fn write_optional_text(path: &Path, contents: Option<&str>) -> anyhow::Result<()> {
    match contents {
        Some(text) => {
            let target = replace_target(path);
            write_private_bytes_atomic(&target, text.as_bytes())
                .with_context(|| format!("Failed to write file: {}", path.display()))?;
        }
        None => {
            if path.exists() {
                fs::remove_file(path)
                    .with_context(|| format!("Failed to remove file: {}", path.display()))?;
            }
        }
    }

    Ok(())
}

pub fn write_optional_text_if_changed(path: &Path, contents: Option<&str>) -> anyhow::Result<()> {
    let existing = read_optional_text(path)?;
    let next = contents.map(str::to_string);
    if existing == next {
        return Ok(());
    }

    write_optional_text(path, contents)
}

/// The in-place write this replaced followed a symlink (e.g. a dotfiles-managed
/// `~/.codex/config.toml`); replace the link's target so the link survives.
/// A dangling link falls back to the link path itself.
fn replace_target(path: &Path) -> PathBuf {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
        }
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
#[path = "text_tests.rs"]
mod tests;
