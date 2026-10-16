//! Owner-configured notification address for external sources.
//!
//! Cutex Management writes a small owner-private file (schema
//! `cutex/external-notify/v1`) whenever the owner configures the inbound
//! mailbox; `serve --external-notify-file PATH` makes `prepare_remote` read it
//! on every call, so agents get the `-M` destination without knowing it and an
//! address change needs no Job Service restart. The file holds no secret; it is
//! still required to be a regular, owner-owned, not group/other-writable file.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

const MAX_FILE_BYTES: u64 = 4096;
pub const NOTIFY_SCHEMA: &str = "cutex/external-notify/v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyAddress {
    Configured(String),
    NotConfigured,
    Unreadable,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NotifyFile {
    schema: String,
    sources: BTreeMap<String, SourceNotify>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceNotify {
    mail_to: String,
}

/// Conservative address syntax: it ends up as one `qsub -M` argument, so no
/// whitespace, quotes, commas, shell metacharacters or leading `-`.
pub fn valid_mail_address(address: &str) -> bool {
    let Some((local, domain)) = address.split_once('@') else {
        return false;
    };
    let labels: Vec<&str> = domain.split('.').collect();
    address.len() <= 254
        && !local.is_empty()
        && local.len() <= 64
        && !local.starts_with(['-', '.'])
        && !local.ends_with('.')
        && !local.contains("..")
        && local
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._%+-".contains(&b))
        && domain.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

pub fn read_notify_address(path: Option<&Path>, source: &str) -> NotifyAddress {
    let Some(path) = path else {
        return NotifyAddress::NotConfigured;
    };
    match read_file(path) {
        Ok(None) => NotifyAddress::NotConfigured,
        Ok(Some(bytes)) => match serde_json::from_slice::<NotifyFile>(&bytes) {
            Ok(file) if file.schema == NOTIFY_SCHEMA => match file.sources.get(source) {
                Some(entry) if valid_mail_address(&entry.mail_to) => {
                    NotifyAddress::Configured(entry.mail_to.clone())
                }
                Some(_) => NotifyAddress::Unreadable,
                None => NotifyAddress::NotConfigured,
            },
            _ => NotifyAddress::Unreadable,
        },
        Err(()) => NotifyAddress::Unreadable,
    }
}

fn read_file(path: &Path) -> Result<Option<Vec<u8>>, ()> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    let meta = file.metadata().map_err(|_| ())?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o022 != 0
        || meta.len() > MAX_FILE_BYTES
    {
        return Err(());
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(());
    }
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn address_syntax_is_conservative() {
        for ok in [
            "a@b.au",
            "first.last+nci@mail.example.org",
            "x_y%z@gmail.com",
        ] {
            assert!(valid_mail_address(ok), "{ok}");
        }
        for bad in [
            "",
            "a",
            "a@b",
            "-a@b.au",
            "a b@c.au",
            "a@b..au",
            "a@-b.au",
            "a,b@c.au",
            "a@b.au,c@d.au",
            "\"a\"@b.au",
            "a@b.au;rm",
            ".a@b.au",
            "a..b@c.au",
        ] {
            assert!(!valid_mail_address(bad), "{bad}");
        }
    }

    #[test]
    fn reads_only_private_well_formed_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify.json");
        assert_eq!(
            read_notify_address(None, "nci"),
            NotifyAddress::NotConfigured
        );
        assert_eq!(
            read_notify_address(Some(&path), "nci"),
            NotifyAddress::NotConfigured
        );
        let write = |body: &str, mode: u32| {
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        write(
            r#"{"schema":"cutex/external-notify/v1","sources":{"nci":{"mailTo":"me@example.org"}}}"#,
            0o600,
        );
        assert_eq!(
            read_notify_address(Some(&path), "nci"),
            NotifyAddress::Configured("me@example.org".into())
        );
        assert_eq!(
            read_notify_address(Some(&path), "other"),
            NotifyAddress::NotConfigured
        );
        write(
            r#"{"schema":"cutex/external-notify/v1","sources":{"nci":{"mailTo":"me@example.org"}}}"#,
            0o622,
        );
        assert_eq!(
            read_notify_address(Some(&path), "nci"),
            NotifyAddress::Unreadable
        );
        write(
            r#"{"schema":"cutex/external-notify/v1","sources":{"nci":{"mailTo":"-x@example.org"}}}"#,
            0o600,
        );
        assert_eq!(
            read_notify_address(Some(&path), "nci"),
            NotifyAddress::Unreadable
        );
        write(r#"{"schema":"other","sources":{}}"#, 0o600);
        assert_eq!(
            read_notify_address(Some(&path), "nci"),
            NotifyAddress::Unreadable
        );
        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert_eq!(
            read_notify_address(Some(&link), "nci"),
            NotifyAddress::Unreadable
        );
    }
}
