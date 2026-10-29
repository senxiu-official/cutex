//! Select a directly launched Windows PowerShell for DSH's existing shell
//! provider. Store activation can create children outside its command Job.
use std::path::{Path, PathBuf};

pub(super) fn patch() -> anyhow::Result<String> {
    patch_for_platform(cfg!(windows), || {
        select(
            &std::env::var_os("ProgramFiles").map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(r"C:\Program Files")),
            std::env::var_os("PATH").as_deref(),
            &std::env::var_os("SystemRoot").map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(r"C:\Windows")),
        )
    })
}

fn patch_for_platform(windows: bool, choose: impl FnOnce() -> anyhow::Result<PathBuf>) -> anyhow::Result<String> {
    if !windows { return Ok(String::new()); }
    let path = choose()?;
    let path = path.to_str().ok_or_else(|| anyhow::anyhow!("DSH PowerShell path must be Unicode"))?;
    // JSON strings are YAML scalars too: preserve spaces, quotes and backslashes.
    Ok(format!("- id: pwsh-sandbox\n  config:\n    pwshPath: {}\n", serde_json::to_string(path)?))
}

fn contains_windows_apps(path: &Path) -> bool {
    path.components().any(|part| part.as_os_str().eq_ignore_ascii_case("WindowsApps"))
}

fn eligible(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() || contains_windows_apps(path) { return None; }
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() == 0 { return None; }
    let canonical = path.canonicalize().ok()?;
    if contains_windows_apps(&canonical) { return None; }
    // Rust canonicalization uses \\?\ on Windows. SDK executable lookup must
    // receive an ordinary absolute spelling, including ordinary UNC paths.
    let ordinary = PathBuf::from(crate::agent_management::strip_verbatim_prefix(canonical.to_str()?));
    if !ordinary.is_absolute() { return None; }
    Some(ordinary)
}

fn select(program_files: &Path, search_path: Option<&std::ffi::OsStr>, system_root: &Path) -> anyhow::Result<PathBuf> {
    let mut candidates = vec![program_files.join("PowerShell/7/pwsh.exe")];
    if let Some(search_path) = search_path {
        candidates.extend(std::env::split_paths(search_path).map(|directory| directory.join("pwsh.exe")));
    }
    candidates.push(system_root.join("System32/WindowsPowerShell/v1.0/powershell.exe"));
    candidates.iter().find_map(|candidate| eligible(candidate)).ok_or_else(|| anyhow::anyhow!(
        "DSH requires a non-Store PowerShell executable: install standalone PowerShell 7 or enable System32 WindowsPowerShell v1.0/powershell.exe"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("dsh-shell-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        fn file(&self, relative: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, bytes).unwrap();
            path
        }
        fn ordinary(path: &Path) -> PathBuf { eligible(path).unwrap() }
    }
    impl Drop for Fixture {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    #[test]
    fn windows_shell_prefers_standalone_then_path_then_system_powershell() {
        let fixture = Fixture::new();
        let program_files = fixture.0.join("Program Files");
        let system_root = fixture.0.join("Windows");
        let standalone = fixture.file("Program Files/PowerShell/7/pwsh.exe", b"standalone");
        let path_shell = fixture.file("tools/pwsh.exe", b"path");
        let fallback = fixture.file("Windows/System32/WindowsPowerShell/v1.0/powershell.exe", b"system");
        let search = std::env::join_paths([path_shell.parent().unwrap()]).unwrap();
        assert_eq!(select(&program_files, Some(&search), &system_root).unwrap(), Fixture::ordinary(&standalone));
        std::fs::remove_file(standalone).unwrap();
        assert_eq!(select(&program_files, Some(&search), &system_root).unwrap(), Fixture::ordinary(&path_shell));
        std::fs::remove_file(path_shell).unwrap();
        assert_eq!(select(&program_files, Some(&search), &system_root).unwrap(), Fixture::ordinary(&fallback));
    }

    #[test]
    fn windows_shell_rejects_store_case_insensitive_zero_directories_and_relative_paths() {
        let fixture = Fixture::new();
        let store = fixture.file("wInDoWsApPs/package/pwsh.exe", b"packaged");
        let zero = fixture.file("alias/pwsh.exe", b"");
        let directory = fixture.0.join("directory/pwsh.exe");
        std::fs::create_dir_all(&directory).unwrap();
        for rejected in [&store, &zero, &directory] { assert!(eligible(rejected).is_none(), "{}", rejected.display()); }
        assert!(eligible(Path::new("pwsh.exe")).is_none());
        let near_name = fixture.file("WindowsAppsBackup/pwsh.exe", b"standalone");
        assert!(eligible(&near_name).is_some(), "match a segment, not an unrelated name");
        let system_root = fixture.0.join("Windows");
        let fallback = fixture.file("Windows/System32/WindowsPowerShell/v1.0/powershell.exe", b"system");
        let search = std::env::join_paths([store.parent().unwrap(), zero.parent().unwrap(), directory.parent().unwrap()]).unwrap();
        assert_eq!(select(&fixture.0.join("absent"), Some(&search), &system_root).unwrap(), Fixture::ordinary(&fallback));
        std::fs::remove_file(fallback).unwrap();
        assert!(select(&fixture.0.join("absent"), Some(&search), &system_root).unwrap_err().to_string().contains("non-Store PowerShell"));
    }

    #[cfg(any(windows, unix))]
    #[test]
    fn windows_shell_rejects_link_resolving_into_store() {
        let fixture = Fixture::new();
        let store = fixture.file("WindowsApps/package/pwsh.exe", b"packaged");
        let link = fixture.0.join("innocent");
        crate::platform::shared_assets::link_directory(store.parent().unwrap(), &link).unwrap();
        assert!(eligible(&link.join("pwsh.exe")).is_none());
        // Remove the link itself before fixture cleanup; never recurse through it.
        #[cfg(windows)]
        std::fs::remove_dir(link).unwrap();
        #[cfg(unix)]
        std::fs::remove_file(link).unwrap();
    }

    #[test]
    fn windows_shell_patch_is_one_existing_provider_override_with_json_path() {
        let fixture = Fixture::new();
        let path = fixture.file("Program Files/PowerShell/7/pwsh.exe", b"shell");
        let path = Fixture::ordinary(&path);
        let patch = patch_for_platform(true, || Ok(path.clone())).unwrap();
        let prefix = "- id: pwsh-sandbox\n  config:\n    pwshPath: ";
        let decoded: String = serde_json::from_str(patch.strip_prefix(prefix).unwrap().trim_end()).unwrap();
        assert_eq!(PathBuf::from(&decoded), path);
        assert!(!decoded.starts_with(r"\\?\"));
        assert_eq!(patch.matches("- id:").count(), 1);
        assert!(!patch.contains("insert:") && !patch.contains("disabled:"));
    }

    #[test]
    fn windows_shell_linux_patch_is_byte_for_byte_unchanged_and_does_not_select() {
        let mut patch = String::from("existing patch\n");
        patch.push_str(&patch_for_platform(false, || panic!("Linux must not inspect Windows executables")).unwrap());
        assert_eq!(patch.as_bytes(), b"existing patch\n");
    }
}
