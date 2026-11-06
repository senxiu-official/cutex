//! Explicit owner repair of one observed profile directory, never a tree walk.
use super::Finding;
use anyhow::{ensure, Context};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub(super) fn repair(root: &Path, finding: &Finding) -> anyhow::Result<PathBuf> {
    repair_with_sync(root, finding, |directory| directory.sync_all())
}

fn repair_with_sync(
    root: &Path,
    finding: &Finding,
    sync: impl FnOnce(&fs::File) -> std::io::Result<()>,
) -> anyhow::Result<PathBuf> {
    ensure!(
        finding.code == "profile_directory_writable",
        "wrong profile finding"
    );
    let path = Path::new(
        finding.detail["path"]
            .as_str()
            .context("profile finding path missing")?,
    );
    ensure!(
        path == root || path.parent() == Some(root),
        "profile directory outside instance profiles"
    );
    // Final-component symlinks are never followed. The observed device/inode
    // also bind the opened object if a parent was renamed since the scan.
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .context("open observed profile directory without following links")?;
    let metadata = directory.metadata()?;
    let mode = metadata.permissions().mode() & 0o7777;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() }
            && finding.detail["uid"].as_u64() == Some(metadata.uid().into())
            && finding.detail["device"].as_u64() == Some(metadata.dev())
            && finding.detail["inode"].as_u64() == Some(metadata.ino())
            && finding.detail["mode"].as_str() == Some(format!("{mode:04o}").as_str())
            && mode & 0o022 != 0,
        "finding_changed: profile directory identity/owner/mode changed; scan again"
    );
    directory.set_permissions(fs::Permissions::from_mode(0o700))?;
    sync(&directory).map_err(|error| {
        super::RepairAlreadyApplied(
            anyhow::Error::new(error)
                .context("directory sync failed; health audit was not written"),
        )
    })?;
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::super::profile_directory_findings;
    use super::*;

    #[test]
    fn explicit_repair_changes_only_the_selected_directory_and_never_auth_contents() {
        let home = crate::profiles::lifecycle::tests::TestHome::new();
        let root = home.root().join("profiles");
        let selected = root.join("selected");
        let sibling = root.join("sibling");
        fs::create_dir_all(selected.join("nested")).unwrap();
        fs::create_dir(&sibling).unwrap();
        for path in [&selected, &sibling, &selected.join("nested")] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o775)).unwrap();
        }
        let auth = selected.join("auth.json");
        fs::write(&auth, b"private fixture bytes").unwrap();
        let before = fs::read(&auth).unwrap();
        let finding = profile_directory_findings(&root)
            .unwrap()
            .into_iter()
            .find(|f| f.detail["path"] == serde_json::json!(selected))
            .unwrap();
        repair(&root, &finding).unwrap();
        assert_eq!(
            fs::metadata(&selected).unwrap().permissions().mode() & 0o7777,
            0o700
        );
        assert_eq!(
            fs::metadata(&sibling).unwrap().permissions().mode() & 0o7777,
            0o775
        );
        assert_eq!(
            fs::metadata(selected.join("nested"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o775
        );
        assert_eq!(fs::read(auth).unwrap(), before);
        assert!(
            repair(&root, &finding).is_err(),
            "stale observation cannot authorize another write"
        );
    }

    #[test]
    fn replacement_symlink_changed_mode_and_outside_paths_are_refused() {
        let home = crate::profiles::lifecycle::tests::TestHome::new();
        let root = home.root().join("profiles");
        let selected = root.join("selected");
        fs::create_dir_all(&selected).unwrap();
        fs::set_permissions(&selected, fs::Permissions::from_mode(0o775)).unwrap();
        let finding = profile_directory_findings(&root)
            .unwrap()
            .into_iter()
            .find(|f| f.detail["path"] == serde_json::json!(selected))
            .unwrap();
        fs::rename(&selected, root.join("original")).unwrap();
        fs::create_dir(&selected).unwrap();
        fs::set_permissions(&selected, fs::Permissions::from_mode(0o775)).unwrap();
        assert!(repair(&root, &finding).is_err());
        fs::remove_dir(&selected).unwrap();
        std::os::unix::fs::symlink(root.join("original"), &selected).unwrap();
        assert!(repair(&root, &finding).is_err());
        fs::remove_file(&selected).unwrap();
        fs::rename(root.join("original"), &selected).unwrap();
        fs::set_permissions(&selected, fs::Permissions::from_mode(0o770)).unwrap();
        assert!(repair(&root, &finding).is_err());
        assert!(repair(&home.root().join("other-profiles"), &finding).is_err());
    }

    #[test]
    fn sync_failure_reports_the_already_changed_permissions() {
        let home = crate::profiles::lifecycle::tests::TestHome::new();
        let root = home.root().join("profiles");
        let selected = root.join("selected");
        fs::create_dir_all(&selected).unwrap();
        fs::set_permissions(&selected, fs::Permissions::from_mode(0o775)).unwrap();
        let finding = profile_directory_findings(&root)
            .unwrap()
            .into_iter()
            .find(|f| f.detail["path"] == serde_json::json!(selected))
            .unwrap();
        let selected_ids = [finding.id.clone()].into_iter().collect();
        let failure =
            super::super::apply_findings(&[finding.clone()], &selected_ids, false, |_| {
                repair_with_sync(&root, &finding, |_| {
                    Err(std::io::Error::other("injected sync failure"))
                })
                .map(|path| path.to_string_lossy().into_owned())
            })
            .err()
            .unwrap();
        assert_eq!(failure.applied, vec![finding.id]);
        assert!(failure.to_string().contains("audit was not written"));
        assert_eq!(
            fs::metadata(selected).unwrap().permissions().mode() & 0o7777,
            0o700
        );
    }

    #[test]
    fn directories_without_owner_read_permission_remain_report_only() {
        let home = crate::profiles::lifecycle::tests::TestHome::new();
        let root = home.root().join("profiles");
        let selected = root.join("selected");
        fs::create_dir_all(&selected).unwrap();
        fs::set_permissions(&selected, fs::Permissions::from_mode(0o333)).unwrap();
        let finding = profile_directory_findings(&root)
            .unwrap()
            .into_iter()
            .find(|f| f.detail["path"] == serde_json::json!(selected))
            .unwrap();
        assert!(finding.fix.is_none());
        fs::set_permissions(selected, fs::Permissions::from_mode(0o700)).unwrap();
    }
}
