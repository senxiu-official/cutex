use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use super::*;
use crate::config::atomic::test_hook::{self, Stage};

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn temp_dir() -> TempDir {
    let path = std::env::temp_dir().join(format!("cutex-text-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&path).unwrap();
    TempDir(path)
}

/// Owner-only: mode 0600 on Unix, a protected owner-only DACL on Windows.
fn assert_private(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{} has mode {mode:o}", path.display());
    }
    #[cfg(windows)]
    {
        let file = fs::File::open(path).unwrap();
        crate::platform::private_fs::validate_private_file(&file)
            .unwrap_or_else(|error| panic!("{} is not private: {error}", path.display()));
    }
}

fn leftovers(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp-"))
        .collect()
}

#[test]
fn credential_write_is_private_before_any_byte_and_atomic() {
    let dir = temp_dir();
    let path = dir.0.join("auth.json");
    fs::write(&path, "{\"old\":true}").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    }

    let seen = Rc::new(RefCell::new(Vec::new()));
    let target = path.clone();
    let hook_seen = seen.clone();
    let _guard = test_hook::observe(move |stage, tmp| {
        assert_eq!(tmp.parent(), target.parent(), "temp file lives next to the target");
        // Permissions are already owner-only when the file is created,
        // before the secret is written, and still when it is complete.
        assert_private(tmp);
        let written = fs::read_to_string(tmp).unwrap();
        match stage {
            Stage::Created => assert_eq!(written, ""),
            Stage::Written => assert_eq!(written, "{\"new\":true}"),
        }
        // The target is never written in place: it stays whole until the rename.
        assert_eq!(fs::read_to_string(&target).unwrap(), "{\"old\":true}");
        hook_seen.borrow_mut().push(stage);
    });

    write_optional_text_if_changed(&path, Some("{\"new\":true}")).unwrap();

    assert_eq!(*seen.borrow(), vec![Stage::Created, Stage::Written]);
    assert_eq!(fs::read_to_string(&path).unwrap(), "{\"new\":true}");
    assert_private(&path);
    assert!(leftovers(&dir.0).is_empty(), "{:?}", leftovers(&dir.0));
}

#[test]
fn new_config_is_created_private_and_none_removes_it() {
    let dir = temp_dir();
    let path = dir.0.join("config.toml");
    write_optional_text(&path, Some("model = \"x\"\n")).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "model = \"x\"\n");
    assert_private(&path);

    // Unchanged contents are not rewritten.
    let calls = Rc::new(RefCell::new(0));
    let hook_calls = calls.clone();
    {
        let _guard = test_hook::observe(move |_, _| *hook_calls.borrow_mut() += 1);
        write_optional_text_if_changed(&path, Some("model = \"x\"\n")).unwrap();
    }
    assert_eq!(*calls.borrow(), 0);

    write_optional_text(&path, None).unwrap();
    assert!(!path.exists());
    assert!(leftovers(&dir.0).is_empty());
}

#[cfg(unix)]
#[test]
fn symlinked_config_keeps_its_link() {
    let dir = temp_dir();
    let real = dir.0.join("dotfiles-config.toml");
    fs::write(&real, "model = \"old\"\n").unwrap();
    let link = dir.0.join("config.toml");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    write_optional_text(&link, Some("model = \"new\"\n")).unwrap();

    assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    assert_eq!(fs::read_to_string(&real).unwrap(), "model = \"new\"\n");
    assert_private(&real);
}
