//! Small text-file helpers used by profile/config materialization.

use std::fs;
use std::path::Path;

use anyhow::Context;

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
            fs::write(path, text)
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
