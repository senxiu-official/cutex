//! Explicit private ChatGPT file custody (a native `auth.json` holding a
//! ChatGPT login). Token refresh is native-owned; neither token bytes nor
//! their digest enter a review or error. Used by the per-profile switch
//! `privateChatgptAuthFile` (`docs/PROFILE-GENERIC-SWITCHES.md`) and by the
//! selected-profile ChatGPT file route.
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Native ChatGPT Codex endpoint of the subscription route.
pub const ENDPOINT: &str = "https://chatgpt.com/backend-api/codex";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewedChatgptAuth {
    pub version: u32,
    pub path: PathBuf,
    pub device: u64,
    pub inode: u64,
    pub owner: u32,
    pub directory_device: u64,
    pub directory_inode: u64,
    pub account_sha256: crate::role_revision::Sha256,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn review(path: PathBuf) -> anyhow::Result<ReviewedChatgptAuth> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    ensure!(
        path.file_name().is_some_and(|n| n == "auth.json"),
        "native auth path required"
    );
    ensure!(
        path.canonicalize()? == path,
        "canonical native auth path required"
    );
    let parent = path.parent().context("native auth parent missing")?;
    let dir = std::fs::symlink_metadata(parent)?;
    let uid = unsafe { libc::geteuid() };
    ensure!(
        dir.is_dir() && dir.uid() == uid && dir.mode() & 0o077 == 0,
        "native auth directory must be owner-private"
    );
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)?;
    let before = file.metadata()?;
    ensure!(
        before.is_file()
            && before.uid() == uid
            && before.mode() & 0o077 == 0
            && before.nlink() == 1
            && before.len() <= 1024 * 1024,
        "native auth custody invalid"
    );
    let mut bytes = Vec::new();
    (&mut file).take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    let named = std::fs::symlink_metadata(&path)?;
    ensure!(
        !named.file_type().is_symlink()
            && before.dev() == named.dev()
            && before.ino() == named.ino()
            && before.len() == after.len()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec(),
        "native auth changed during read"
    );
    let account_sha256 = account_identity(&bytes)?;
    Ok(ReviewedChatgptAuth {
        version: 1,
        path,
        device: before.dev(),
        inode: before.ino(),
        owner: uid,
        directory_device: dir.dev(),
        directory_inode: dir.ino(),
        account_sha256,
    })
}

pub(super) fn account_identity(bytes: &[u8]) -> anyhow::Result<crate::role_revision::Sha256> {
    use sha2::Digest;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid native auth JSON"))?;
    ensure!(
        value.as_object().is_some_and(|o| o.keys().all(|k| matches!(
            k.as_str(),
            "auth_mode" | "OPENAI_API_KEY" | "tokens" | "last_refresh"
        ))),
        "unsupported native auth fields"
    );
    ensure!(
        value.get("auth_mode").and_then(|v| v.as_str()) == Some("chatgpt")
            && value.get("OPENAI_API_KEY").is_none_or(|v| v.is_null()),
        "only native ChatGPT auth is supported"
    );
    for field in [
        "agent_identity",
        "personal_access_token",
        "bedrock_api_key",
        "bedrock_access_keys",
    ] {
        ensure!(
            value.get(field).is_none_or(|v| v.is_null()),
            "unsupported native auth capability"
        );
    }
    let tokens = value
        .get("tokens")
        .and_then(|v| v.as_object())
        .context("native ChatGPT tokens missing")?;
    ensure!(
        tokens.keys().all(|k| matches!(
            k.as_str(),
            "id_token" | "access_token" | "refresh_token" | "account_id"
        )),
        "unsupported native token fields"
    );
    for field in ["id_token", "access_token", "refresh_token"] {
        ensure!(
            tokens
                .get(field)
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.is_empty()),
            "native ChatGPT token field missing"
        );
    }
    let account = tokens
        .get("account_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .context("native ChatGPT account identity missing")?;
    // Match native login/token_data.rs metadata projection, not an OAuth
    // implementation or a credential signature verifier. Native authenticates.
    use base64::Engine;
    let parts: Vec<_> = tokens["id_token"].as_str().unwrap().split('.').collect();
    ensure!(
        parts.len() == 3 && parts.iter().all(|p| !p.is_empty()),
        "invalid native ID token structure"
    );
    let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .map_err(|_| anyhow::anyhow!("invalid native ID token metadata"))?;
    let claims: serde_json::Value = serde_json::from_slice(&claims)
        .map_err(|_| anyhow::anyhow!("invalid native ID token metadata"))?;
    let claims = &claims["https://api.openai.com/auth"];
    let user = claims
        .get("chatgpt_user_id")
        .or_else(|| claims.get("user_id"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .context("native account user identity missing")?;
    ensure!(
        claims
            .get("chatgpt_account_is_fedramp")
            .is_none_or(|v| v.as_bool() == Some(false)),
        "unsupported native account route"
    );
    let account_sha256 = crate::role_revision::Sha256::new(format!(
        "{:x}",
        sha2::Sha256::digest(serde_json::to_vec(&(
            crate::profiles::generic_switches::legacy::ACCOUNT_DIGEST_DOMAIN,
            account,
            user
        ))?)
    ))
    .map_err(|_| anyhow::anyhow!("invalid account identity digest"))?;
    Ok(account_sha256)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn review(_: PathBuf) -> anyhow::Result<ReviewedChatgptAuth> {
    anyhow::bail!("reviewed private ChatGPT auth file currently supports Linux only")
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn setup() -> (PathBuf, PathBuf, serde_json::Value) {
        let root = std::env::temp_dir().join(format!("chatgpt-auth-file-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join("auth.json");
        use base64::Engine;
        let id = format!(
            "header.{}.signature",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(br#"{"https://api.openai.com/auth":{"chatgpt_user_id":"fixture-user"}}"#)
        );
        let value = serde_json::json!({"auth_mode":"chatgpt","OPENAI_API_KEY":null,
            "tokens":{"id_token":id,"access_token":"secret-fixture-access",
                "refresh_token":"secret-fixture-refresh","account_id":"fixture-account"},"last_refresh":null});
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        (root, path, value)
    }
    #[test]
    fn chatgpt_auth_refresh_preserves_identity_but_account_and_replacement_do_not() {
        let (root, path, mut value) = setup();
        let original = review(path.clone()).unwrap();
        value["tokens"]["access_token"] = serde_json::json!("refreshed-token-with-different-size");
        value["tokens"]["refresh_token"] = serde_json::json!("refreshed-secret");
        value["last_refresh"] = serde_json::json!("2030-01-01T00:00:00Z");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(review(path.clone()).unwrap(), original);
        let serialized = serde_json::to_string(&original).unwrap();
        assert!(!serialized.contains("secret") && !serialized.contains("fixture-account"));
        value["tokens"]["account_id"] = serde_json::json!("different-account");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert_ne!(review(path.clone()).unwrap(), original);
        value["tokens"]["account_id"] = serde_json::json!("fixture-account");
        std::fs::rename(&path, root.join("old-auth")).unwrap();
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_ne!(review(path).unwrap(), original);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn chatgpt_auth_rejects_invalid_capability_public_file_and_symlink() {
        let (root, path, value) = setup();
        for changed in [
            serde_json::json!({"auth_mode":"api_key"}),
            serde_json::json!({"auth_mode":"chatgpt","tokens":{}}),
            {
                let mut v = value.clone();
                v["unknown_auth_capability"] = serde_json::json!(true);
                v
            },
        ] {
            std::fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
            assert!(review(path.clone()).is_err());
        }
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(review(path.clone()).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::rename(&path, root.join("real-auth")).unwrap();
        std::os::unix::fs::symlink(root.join("real-auth"), &path).unwrap();
        assert!(review(path).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn chatgpt_auth_same_account_different_user_and_unsupported_route_rejected() {
        use base64::Engine;
        let (root, path, mut value) = setup();
        let original = review(path.clone()).unwrap();
        for (user, fedramp) in [("other-user", false), ("fixture-user", true)] {
            let claims = serde_json::json!({"https://api.openai.com/auth": {
                "chatgpt_user_id":user,"chatgpt_account_is_fedramp":fedramp}});
            value["tokens"]["id_token"] = serde_json::json!(format!(
                "header.{}.signature",
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(serde_json::to_vec(&claims).unwrap())
            ));
            std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            if fedramp {
                assert!(review(path.clone()).is_err());
            } else {
                assert_ne!(review(path.clone()).unwrap(), original);
            }
        }
        value["tokens"]["id_token"] = serde_json::json!("not-a-native-token");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(review(path).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
