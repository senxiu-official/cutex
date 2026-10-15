//! Preserve daemon-owned native thread trees across a client disconnection.
//! Older pinned runtimes remain launchable; an unknown config key is not proof
//! that they implement retention. Probe the verified app-server before opting in.
use super::{
    command::LaunchCommand,
    stock::{StockBundle, VerifiedFile},
};
use anyhow::Context;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime},
};

#[derive(Clone, PartialEq, Eq)]
struct Key {
    sha256: String,
    len: u64,
    modified: Option<SystemTime>,
}
static CACHE: Mutex<Option<HashMap<PathBuf, (Key, bool)>>> = Mutex::new(None);

fn key(file: &VerifiedFile) -> anyhow::Result<Key> {
    let metadata = std::fs::metadata(&file.path)?;
    anyhow::ensure!(metadata.is_file(), "native executable is not a file");
    Ok(Key {
        sha256: file.sha256.as_str().to_owned(),
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

fn advertised(bytes: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|v| {
            v.get("threadRetentionVersion")
                .and_then(serde_json::Value::as_u64)
        })
        == Some(1)
}

fn supported(file: &VerifiedFile, home: &Path) -> anyhow::Result<bool> {
    let before = key(file).context("native thread-retention probe executable unavailable")?;
    if let Some((cached, result)) = CACHE
        .lock()
        .map_err(|_| anyhow::anyhow!("native retention cache poisoned"))?
        .as_ref()
        .and_then(|cache| cache.get(&file.path))
    {
        if *cached == before {
            return Ok(*result);
        }
    }
    file.validate()?;
    let result = match crate::profiles::native_models::run_cli(
        file,
        &["--thread-retention-capability"],
        home,
        Duration::from_secs(10),
    ) {
        Ok(bytes) => advertised(&bytes),
        // Older clap-based natives reject the unknown probe with exit 2.
        // Operational failures must abort this launch, never cache a false
        // capability result or start a new native without its retention policy.
        Err(error)
            if error
                .downcast_ref::<crate::profiles::native_models::NativeCliExit>()
                .is_some_and(|exit| exit.status.code() == Some(2)) =>
        {
            false
        }
        Err(error) => {
            return Err(
                error.context("native thread-retention capability probe failed; retry launch")
            )
        }
    };
    anyhow::ensure!(
        key(file).ok().as_ref() == Some(&before),
        "native executable changed during retention probe"
    );
    CACHE
        .lock()
        .map_err(|_| anyhow::anyhow!("native retention cache poisoned"))?
        .get_or_insert_with(HashMap::new)
        .insert(file.path.clone(), (before, result));
    Ok(result)
}

fn override_for(launch: LaunchCommand, supported: bool) -> LaunchCommand {
    if supported {
        launch.arg("-c").arg("thread_unload_enabled=false")
    } else {
        launch
    }
}

/// Called after profile/shared overrides, only for the owned server process.
/// Remote TUI clients, Claude adapters, and temporary bootstrap servers do not
/// pass through this policy.
pub fn apply(
    launch: LaunchCommand,
    bundle: &StockBundle,
    home: &Path,
) -> anyhow::Result<LaunchCommand> {
    let support = bundle.common_ingress() && supported(&bundle.executable, home)?;
    Ok(override_for(launch, support))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_requires_exact_advertised_contract_and_preserves_old_launches() {
        for bytes in [
            b"{}".as_slice(),
            br#"{"threadRetentionVersion":true}"#,
            br#"{"threadRetentionVersion":2}"#,
            b"not json",
        ] {
            assert!(!advertised(bytes));
        }
        assert!(advertised(br#"{"threadRetentionVersion":1}"#));
        let launch = LaunchCommand::new("native").args(["-c", "thread_unload_enabled=true"]);
        assert_eq!(override_for(launch.clone(), false), launch);
        assert_eq!(
            &override_for(launch, true).args[2..],
            &["-c", "thread_unload_enabled=false"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn verified_probe_is_cached_and_binary_replacement_is_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("cutex-retention-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("native");
        std::fs::write(&path, b"#!/bin/sh\n[ \"$1\" = --thread-retention-capability ] || exit 2\nprintf '{\"threadRetentionVersion\":1}'\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let file = VerifiedFile {
            path: path.clone(),
            sha256: crate::agent_management::file_sha256(&path).unwrap(),
        };
        assert!(supported(&file, &dir).unwrap());
        // Cached lookups do not need to execute the binary again.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(supported(&file, &dir).unwrap());
        std::fs::write(&path, b"replaced").unwrap();
        assert!(supported(&file, &dir).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn operational_probe_failure_is_not_cached_but_legacy_exit_two_is_supported() {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("cutex-retention-retry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("native");
        std::fs::write(&path, b"#!/bin/sh\n[ -e \"$CODEX_HOME/ready\" ] || exit 1\nprintf '{\"threadRetentionVersion\":1}'\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let file = VerifiedFile {
            path: path.clone(),
            sha256: crate::agent_management::file_sha256(&path).unwrap(),
        };
        assert!(supported(&file, &dir).is_err());
        std::fs::write(dir.join("ready"), b"ready").unwrap();
        assert!(
            supported(&file, &dir).unwrap(),
            "retry unchanged binary after transient failure"
        );
        let legacy = dir.join("legacy");
        std::fs::write(&legacy, b"#!/bin/sh\nexit 2\n").unwrap();
        std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o700)).unwrap();
        let legacy = VerifiedFile {
            sha256: crate::agent_management::file_sha256(&legacy).unwrap(),
            path: legacy,
        };
        assert!(!supported(&legacy, &dir).unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
