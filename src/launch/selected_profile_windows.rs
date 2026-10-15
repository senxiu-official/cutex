//! Read selected credentials through Windows file identities and private ACLs.
use super::*;
use crate::platform::private_fs;
use std::io::Read;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

fn read(path: &Path, route: &Route) -> anyhow::Result<(AuthCustody, Option<ApiKey>)> {
    ensure!(path.is_absolute(), "absolute auth path required");
    let parent = path
        .parent()
        .context("auth parent missing")?
        .canonicalize()?;
    let path = parent.join(path.file_name().context("auth filename missing")?);
    let (_directory, parent_id) = private_fs::open_validated_directory(&parent)
        .with_context(|| format!("Validate profile credential directory {}", parent.display()))?;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(0x0020_0000)
        .open(&path)?; // FILE_FLAG_OPEN_REPARSE_POINT
    private_fs::validate_private_file(&file)
        .with_context(|| format!("Validate profile credential file {}", path.display()))?;
    let id = private_fs::identity(&file)?;
    let before = file.metadata()?;
    ensure!(before.len() <= 1024 * 1024, "auth file too large");
    let mut bytes = Vec::new();
    (&mut file).take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    let named = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(0x0020_0000)
        .open(&path)?;
    private_fs::validate_private_file(&named)?;
    ensure!(
        bytes.len() <= 1024 * 1024
            && private_fs::identity(&named)? == id
            && before.len() == after.len()
            && before.last_write_time() == after.last_write_time(),
        "auth file changed while reading"
    );
    private_fs::validate_binding(&parent, parent_id)?;
    let (account, key) = match route {
        Route::ChatgptFile => (
            Some(super::super::chatgpt_auth_file::account_identity(&bytes)?),
            None,
        ),
        Route::GlmApiKey | Route::ApiKey => {
            let value: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("invalid API credential JSON"))?;
            ensure!(
                value.get("tokens").is_none_or(|v| v.is_null())
                    && value
                        .get("auth_mode")
                        .is_none_or(|v| v.is_null() || v.as_str() == Some("apikey")),
                "API profile requires API credentials"
            );
            let key = value["OPENAI_API_KEY"]
                .as_str()
                .filter(|key| !key.is_empty() && !key.chars().any(char::is_control))
                .context("API credential missing")?;
            (None, Some(ApiKey(key.into())))
        }
    };
    Ok((
        AuthCustody {
            path,
            parent_device: parent_id.volume,
            parent_inode: u64::from_le_bytes(parent_id.file_id[..8].try_into().unwrap()),
            owner: 0, // Windows validates the process token SID, not a POSIX uid.
            account,
            api_file: key.as_ref().map(|_| super::super::job_mcp::PrivateObject {
                device: id.volume,
                inode: u64::from_le_bytes(id.file_id[..8].try_into().unwrap()),
                size: before.len(),
                changed_seconds: (before.last_write_time() / 10_000_000) as i64,
                changed_nanos: ((before.last_write_time() % 10_000_000) * 100) as i64,
            }),
            windows_parent_file_id: Some(parent_id.file_id),
            windows_api_file_id: key.as_ref().map(|_| id.file_id),
        },
        key,
    ))
}

pub fn review_auth(path: &Path, route: &Route) -> anyhow::Result<AuthCustody> {
    read(path, route).map(|(custody, _)| custody)
}

pub(super) fn read_api_key(path: &Path) -> anyhow::Result<ApiKey> {
    read(path, &Route::GlmApiKey)?
        .1
        .context("API credential missing")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    struct Root(PathBuf);
    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn fixture() -> Root {
        let path =
            std::env::temp_dir().join(format!("cutex-windows-auth-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        private_fs::secure_directory(&path).unwrap();
        Root(path)
    }
    fn write(root: &Path, value: &serde_json::Value) {
        let (_, id) = private_fs::open_validated_directory(root).unwrap();
        let mut file =
            private_fs::open_child(root, id, "auth.json", libc::O_RDWR | libc::O_CREAT, true)
                .unwrap();
        file.set_len(0).unwrap();
        file.write_all(&serde_json::to_vec(value).unwrap()).unwrap();
        file.sync_all().unwrap();
    }
    #[test]
    fn windows_api_profile_reads_key_and_detects_replacement() {
        let root = fixture();
        write(
            &root.0,
            &serde_json::json!({"OPENAI_API_KEY":"test-key-one"}),
        );
        let path = root.0.join("auth.json");
        let before = review_auth(&path, &Route::GlmApiKey).unwrap();
        assert_eq!(read_api_key(&path).unwrap().0, "test-key-one");
        assert!(!serde_json::to_string(&before)
            .unwrap()
            .contains("test-key-one"));
        std::fs::rename(&path, root.0.join("old.json")).unwrap();
        write(
            &root.0,
            &serde_json::json!({"OPENAI_API_KEY":"test-key-two"}),
        );
        assert_ne!(review_auth(&path, &Route::GlmApiKey).unwrap(), before);
    }
    #[test]
    fn windows_chatgpt_refresh_preserves_account_identity() {
        use base64::Engine;
        let root = fixture();
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"https://api.openai.com/auth":{"chatgpt_user_id":"user-test"}}"#);
        let mut value = serde_json::json!({"auth_mode":"chatgpt","tokens":{
            "id_token":format!("header.{claims}.signature"),"access_token":"fake-access", "refresh_token":"fake-refresh", "account_id":"account-test"
        }});
        write(&root.0, &value);
        let path = root.0.join("auth.json");
        let before = review_auth(&path, &Route::ChatgptFile).unwrap();
        value["tokens"]["access_token"] = "fake-refreshed-access".into();
        write(&root.0, &value);
        assert_eq!(review_auth(&path, &Route::ChatgptFile).unwrap(), before);
        value["tokens"]["account_id"] = "different-account".into();
        write(&root.0, &value);
        assert_ne!(review_auth(&path, &Route::ChatgptFile).unwrap(), before);
    }
}
