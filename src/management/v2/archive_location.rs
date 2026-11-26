use std::{
    fs,
    io::{self, Read},
    net::TcpStream,
    path::{Component, Path, PathBuf},
};

use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::{atomic::write_private_pretty_json_atomic, paths::config_dir};
use crate::http::server::{write_json_response, SimpleHttpRequest};

const PATH: &str = "/v2/storage/archive-location";
const FILE: &str = "archive-location-v1.json";
const MAX_BYTES: u64 = 64 * 1024;
const MAX_PATH_BYTES: usize = 8192;

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Location {
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub binding: Option<(u64, u64)>,
    #[serde(default)]
    pub selected_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutRequest {
    #[serde(deserialize_with = "Deserialize::deserialize")]
    path: Option<PathBuf>,
}

pub(super) fn is_path(path: &str) -> bool {
    path == PATH || super::archive_jobs::handles(path)
}

pub(super) fn read(home: &Path) -> anyhow::Result<Location> {
    let file = match fs::File::open(home.join(FILE)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Location::default()),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "archive location state is too large"
    );
    serde_json::from_slice(&bytes).context("invalid archive location state")
}

pub(super) fn linked(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    metadata.file_type().is_symlink()
}

fn overlaps(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    let (a, b) = (
        PathBuf::from(a.to_string_lossy().to_lowercase()),
        PathBuf::from(b.to_string_lossy().to_lowercase()),
    );
    a.starts_with(&b) || b.starts_with(&a)
}

/// Existing directory chains must be ordinary directories. The configured
/// endpoint must also be owner-private. Only a missing final leaf is created.
pub(super) fn validate(home: &Path, input: &Path, create: bool) -> anyhow::Result<PathBuf> {
    ensure!(
        input.as_os_str().len() <= MAX_PATH_BYTES,
        "archive location path is too long"
    );
    ensure!(
        input.is_absolute()
            && input.parent().is_some_and(|parent| parent != input)
            && input.file_name().is_some()
            && !input
                .components()
                .any(|component| matches!(component, Component::ParentDir)),
        "archive location must be an absolute directory path"
    );
    #[cfg(windows)]
    ensure!(
        matches!(
            input.components().next(),
            Some(Component::Prefix(prefix))
                if matches!(prefix.kind(), std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_))
        ),
        "archive location must use a local drive path"
    );

    for dir in input.ancestors().filter(|dir| dir.has_root()) {
        match fs::symlink_metadata(dir) {
            Ok(metadata) => ensure!(
                metadata.is_dir() && !linked(&metadata),
                "archive location contains a link or a non-directory component: {}",
                dir.display()
            ),
            Err(error) if dir == input && create && error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("archive location is unavailable"),
        }
    }

    let home = home.canonicalize()?;
    let parent = input
        .parent()
        .context("archive location has no parent")?
        .canonicalize()?;
    let location = parent.join(input.file_name().context("archive location has no name")?);
    let resolved = match location.canonicalize() {
        Ok(path) => path,
        Err(error) if create && error.kind() == io::ErrorKind::NotFound => location.clone(),
        Err(error) => return Err(error).context("archive location is unavailable"),
    };
    ensure!(
        !overlaps(&home, &resolved),
        "archive location must be outside the Cutex home"
    );

    if create && !location.exists() {
        super::storage_cleanup_fs::create_archive_directory(&location)?;
    }
    super::storage_cleanup_fs::archive_directory_identity(&location)
        .context("archive location must be an available owner-private directory")?;
    Ok(location.canonicalize()?)
}

pub(super) fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    use super::server::write_v2_error;
    if !super::storage_cleanup_access::cleanup_allowed() {
        return write_v2_error(
            stream,
            403,
            "Forbidden",
            "archive_location_desktop_only",
            "archive location requires the owner's local or SSH desktop credential",
            false,
            json!({}),
        );
    }
    let home = config_dir()?;
    let path = request.path.split('?').next().unwrap_or(&request.path);
    if path == super::archive_inventory::PATH {
        if request.method != "POST" {
            return write_v2_error(
                stream,
                405,
                "Method Not Allowed",
                "method_not_allowed",
                "archive migration preview supports POST",
                false,
                json!({}),
            );
        }
        return match serde_json::from_slice(&request.body)
            .map_err(anyhow::Error::from)
            .and_then(|body| super::archive_jobs::preview(&home, body))
        {
            Ok(value) => write_json_response(stream, 200, "OK", &value),
            Err(error) => write_v2_error(
                stream,
                400,
                "Bad Request",
                "archive_inventory_unavailable",
                &format!("{error:#}"),
                false,
                json!({}),
            ),
        };
    }
    if super::archive_jobs::handles(path) {
        return super::archive_jobs::handle(stream, request, &home, path);
    }
    let value = match request.method.as_str() {
        "GET" => read(&home)?,
        "PUT" => {
            let _requests = super::archive_jobs::request_lock(&home)?;
            let parsed = (|| -> anyhow::Result<Location> {
                let request: PutRequest = serde_json::from_slice(&request.body)?;
                let Some(path) = request.path else {
                    return Ok(Location::default());
                };
                let path = validate(&home, &path, true)?;
                let binding = super::storage_cleanup_fs::archive_directory_identity(&path)?;
                let previous = read(&home)?;
                let selected_at =
                    if previous.path.as_ref() == Some(&path) && previous.binding == Some(binding) {
                        previous.selected_at.unwrap_or_else(chrono::Utc::now)
                    } else {
                        chrono::Utc::now()
                    };
                Ok(Location {
                    path: Some(path),
                    binding: Some(binding),
                    selected_at: Some(selected_at),
                })
            })();
            let location = match parsed {
                Ok(value) => value,
                Err(error) => {
                    return write_v2_error(
                        stream,
                        400,
                        "Bad Request",
                        "archive_location_invalid",
                        &format!("{error:#}"),
                        false,
                        json!({}),
                    );
                }
            };
            write_private_pretty_json_atomic(&home.join(FILE), &location, "archive location")?;
            location
        }
        _ => {
            return write_v2_error(
                stream,
                405,
                "Method Not Allowed",
                "method_not_allowed",
                "archive location supports GET and PUT",
                false,
                json!({}),
            );
        }
    };
    write_json_response(stream, 200, "OK", &json!({"path":value.path}))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("archive-location-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn archive_location_creates_private_leaf_and_preserves_config_on_clear() {
        let parent = Fixture::new();
        let home = parent.path().join("home");
        fs::create_dir(&home).unwrap();
        let target = parent.path().join("archive path");
        assert!(read(&home).unwrap().path.is_none());
        let canonical = validate(&home, &target, true).unwrap();
        assert_eq!(validate(&home, &target, false).unwrap(), canonical);

        write_private_pretty_json_atomic(
            &home.join(FILE),
            &Location {
                path: Some(canonical.clone()),
                ..Location::default()
            },
            "test archive location",
        )
        .unwrap();
        assert_eq!(read(&home).unwrap().path, Some(canonical));
        write_private_pretty_json_atomic(
            &home.join(FILE),
            &Location::default(),
            "test archive location",
        )
        .unwrap();
        assert!(read(&home).unwrap().path.is_none());
        assert!(target.is_dir());
    }

    #[test]
    fn archive_location_rejects_relative_overlap_and_files() {
        let parent = Fixture::new();
        let home = parent.path().join("home");
        fs::create_dir(&home).unwrap();
        let file = parent.path().join("file");
        fs::write(&file, b"fixture").unwrap();
        for path in [
            Path::new("relative"),
            parent.path(),
            &home,
            &home.join("inside"),
            &file,
        ] {
            assert!(
                validate(&home, path, true).is_err(),
                "accepted {}",
                path.display()
            );
        }
        assert!(!home.join("inside").exists());
        assert_eq!(fs::read(file).unwrap(), b"fixture");
    }

    #[cfg(unix)]
    #[test]
    fn archive_location_rejects_symlink_and_broad_permissions() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let parent = Fixture::new();
        let home = parent.path().join("home");
        fs::create_dir(&home).unwrap();
        let target = parent.path().join("archive");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(validate(&home, &target, false).is_err());
        let alias = parent.path().join("alias");
        symlink(&target, &alias).unwrap();
        assert!(validate(&home, &alias.join("archive"), true).is_err());
        assert!(!target.join("archive").exists());
    }
}
