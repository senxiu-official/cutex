//! Shared native assets remain live across isolated session homes.
use std::path::Path;

pub fn link_directory(source: &Path, destination: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(source.is_dir(), "shared asset source is not a directory");
    #[cfg(unix)]
    std::os::unix::fs::symlink(source, destination)?;
    #[cfg(windows)]
    {
        // Junctions work without Developer Mode or symlink privileges. Pass
        // paths as environment data, never interpolate them into shell code.
        // PowerShell 5 creates an invalid junction target from Rust's verbatim
        // \?\ canonical path. Give it the equivalent ordinary absolute path.
        let source = source.canonicalize()?;
        let source = source.to_str().ok_or_else(|| anyhow::anyhow!("shared asset path is not Unicode"))?;
        let source = if let Some(unc) = source.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{unc}")
        } else {
            source.strip_prefix(r"\\?\").unwrap_or(source).to_owned()
        };
        let result = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command",
                "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path $env:CUTEX_ASSET_LINK -Value $env:CUTEX_ASSET_SOURCE | Out-Null"])
            .env("CUTEX_ASSET_LINK", destination)
            .env("CUTEX_ASSET_SOURCE", source)
            .output()?;
        anyhow::ensure!(result.status.success(), "shared asset junction creation failed: {}", String::from_utf8_lossy(&result.stderr));
    }
    #[cfg(not(any(unix, windows)))]
    anyhow::bail!("shared asset directories are unsupported on this platform");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_directory_reads_updates_without_copying() {
        let root = std::env::temp_dir().join(format!("cutex-assets-{}", uuid::Uuid::new_v4()));
        let source = root.join("source with spaces");
        let link = root.join("linked assets");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("SKILL.md"), "first").unwrap();
        link_directory(&source, &link).unwrap();
        assert_eq!(std::fs::read_to_string(link.join("SKILL.md")).unwrap(), "first");
        std::fs::write(source.join("SKILL.md"), "updated").unwrap();
        assert_eq!(std::fs::read_to_string(link.join("SKILL.md")).unwrap(), "updated");
        #[cfg(unix)]
        std::fs::remove_file(&link).unwrap();
        #[cfg(windows)]
        std::fs::remove_dir(&link).unwrap();
        assert!(source.join("SKILL.md").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
