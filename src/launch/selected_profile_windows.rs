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
    // Earlier releases replaced auth.json on an API-key edit with a file that
    // only inherits the private directory's owner ACE (no protected DACL), and
    // a native token refresh can do the same: re-protect such a file instead
    // of refusing the profile. A file with any other ACE is still refused below.
    if let Ok(true) = private_fs::repair_inherited_owner_only_file(&path) {
        eprintln!(
            "cutex: re-protected the inherited owner-only DACL of {}",
            path.display()
        );
    }
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
            let key = crate::profiles::api_key::from_auth(&value)
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

    /// r274.1 (C): an existing agent's stored model that the native list
    /// does not show is launched as stored: the review keeps the stored model
    /// over the profile's own model and never consults the list (only a new
    /// managed agent's launch review does, `validate_listed_model`).
    #[test]
    fn a_stored_unlisted_model_is_never_replaced_on_resume() {
        use base64::Engine;
        let root = fixture();
        let id_token = format!(
            "header.{}.signature",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(br#"{"https://api.openai.com/auth":{"chatgpt_user_id":"fixture-user"}}"#)
        );
        write(&root.0, &serde_json::json!({"auth_mode":"chatgpt","OPENAI_API_KEY":null,
            "tokens":{"id_token":id_token,"access_token":"fixture-access","refresh_token":"fixture-refresh","account_id":"fixture-account"},"last_refresh":null}));
        let config = "cutex_provider_mode='selected_profile_v2'\ncli_auth_credentials_store='file'\nmodel='gpt-6.1-sol'\n";
        let stored = "gpt-sol-6.1".to_string();
        let (projection, model, _) = Config::parse(config).unwrap()
            .review(&uuid::Uuid::new_v4().to_string(), root.0.join("auth.json"), Some(&stored), None)
            .expect("a stored unlisted model still reviews");
        assert_eq!(projection.route, Route::ChatgptFile);
        assert!(projection.catalog.is_none());
        assert_eq!(model, stored, "the stored model is launched, not the profile's");
    }

    #[test]
    fn lowercase_api_auth_reviews_as_api_profile_without_rewriting_credentials() {
        let root = fixture();
        let key = format!("fixture-{}", uuid::Uuid::new_v4());
        let config = "cutex_provider_mode='selected_profile_v2'\ncli_auth_credentials_store='file'\nmodel='manual-model'\nmodel_provider='relay'\n[model_providers.relay]\nname='relay'\nbase_url='http://127.0.0.1:9/v1'\nwire_api='responses'\nrequires_openai_auth=false\nenv_key='OPENAI_API_KEY'\n";
        for auth in [serde_json::json!({"openai_api_key":key,"auth_mode":"apikey","tokens":null}),
            serde_json::json!({"OPENAI_API_KEY":null,"openai_api_key":key,"auth_mode":"apikey","tokens":null})] {
            write(&root.0, &auth);
            let path = root.0.join("auth.json");
            let bytes = std::fs::read(&path).unwrap();
            let (projection, _, _) = Config::parse(config).unwrap()
                .review(&uuid::Uuid::new_v4().to_string(), path.clone(), None, None)
                .expect("lowercase legacy credential must select the API route");
            assert_eq!(projection.route, Route::ApiKey);
            assert!(read_api_key(&path).unwrap().0 == key, "selected credential differs");
            assert!(std::fs::read(&path).unwrap() == bytes, "reading must preserve credential bytes");
            assert!(!serde_json::to_string(&projection).unwrap().contains(&key), "projection must not expose credentials");
        }
    }
    #[test]
    fn windows_private_atomic_write_passes_the_credential_check() {
        let root = fixture();
        let path = root.0.join("auth.json");
        crate::config::atomic::write_private_pretty_json_atomic(
            &path,
            &serde_json::json!({"OPENAI_API_KEY":"test-key-atomic"}),
            "test auth",
        )
        .unwrap();
        assert_eq!(read_api_key(&path).unwrap().0, "test-key-atomic");
        // Replacing an existing private file keeps it private.
        crate::config::atomic::write_private_pretty_json_atomic(
            &path,
            &serde_json::json!({"OPENAI_API_KEY":"test-key-atomic-2"}),
            "test auth",
        )
        .unwrap();
        assert_eq!(read_api_key(&path).unwrap().0, "test-key-atomic-2");
    }
    #[test]
    fn windows_inherited_owner_only_auth_is_reprotected() {
        let root = fixture();
        let path = root.0.join("auth.json");
        // A plain create inherits the private directory's owner ACE only.
        std::fs::write(&path, br#"{"OPENAI_API_KEY":"test-key-inherited"}"#).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        assert!(private_fs::validate_private_file(&file).is_err());
        drop(file);
        assert_eq!(read_api_key(&path).unwrap().0, "test-key-inherited");
        let file = std::fs::File::open(&path).unwrap();
        private_fs::validate_private_file(&file).unwrap();
        drop(file);
        assert!(!private_fs::repair_inherited_owner_only_file(&path).unwrap());
    }
    #[test]
    fn windows_auth_shared_with_another_account_is_still_refused() {
        let root = fixture();
        let path = root.0.join("auth.json");
        std::fs::write(&path, br#"{"OPENAI_API_KEY":"test-key-shared"}"#).unwrap();
        // *S-1-1-0 = Everyone.
        let output = std::process::Command::new("icacls")
            .arg(&path)
            .args(["/grant", "*S-1-1-0:(R)"])
            .output()
            .unwrap();
        assert!(output.status.success(), "icacls failed: {output:?}");
        assert!(private_fs::repair_inherited_owner_only_file(&path).is_err());
        assert!(read_api_key(&path).is_err());
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
