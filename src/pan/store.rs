use super::*;
use base64::Engine;
use fs2::FileExt;
use ring::{
    aead,
    rand::{SecureRandom, SystemRandom},
};
use std::{fs, fs::OpenOptions};

#[derive(Clone, Serialize, Deserialize, Default)]
pub(super) struct Secrets {
    pub app: Option<App>,
    pub token: Option<Token>,
    pub auth: Option<Auth>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct App {
    pub app_id: String,
    pub client_id: String,
    pub client_secret: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Token {
    pub access: String,
    pub refresh: String,
    pub expires: i64,
    pub account: Value,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Auth {
    pub id: String,
    pub code: String,
    pub url: String,
    pub user: String,
    pub expires: i64,
    pub interval: i64,
    pub next_poll: i64,
    pub state: String,
    pub error: Option<Error>,
}

impl Service {
    pub(super) fn lock(&self, name: &str) -> Result<fs::File> {
        let path = self.root.join(name);
        if !path.exists() {
            // Serialize creators without replacing a lock another process holds.
            match create_private(&path) {
                Ok(_) => {}
                Err(_) if path.exists() => {}
                Err(e) => return Err(Error::storage(e)),
            }
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(Error::storage)?;
        file.lock_exclusive().map_err(Error::storage)?;
        Ok(file)
    }
    fn key(&self) -> Result<aead::LessSafeKey> {
        let path = self.root.join("key");
        let bytes = if path.exists() {
            fs::read(path).map_err(Error::storage)?
        } else {
            let mut key = [0u8; 32];
            SystemRandom::new().fill(&mut key).map_err(Error::storage)?;
            crate::config::atomic::write_private_bytes_atomic(&path, &key)
                .map_err(Error::storage)?;
            key.to_vec()
        };
        let key = aead::UnboundKey::new(&aead::AES_256_GCM, &bytes).map_err(Error::storage)?;
        Ok(aead::LessSafeKey::new(key))
    }
    // Callers hold auth.lock across load, refresh and replacement. The local
    // key and ciphertext are both owner-private (same-user access is trusted).
    pub(super) fn secrets(&self) -> Result<Secrets> {
        let path = self.root.join("credentials");
        if !path.exists() {
            return Ok(Secrets::default());
        }
        let encoded = fs::read(path).map_err(Error::storage)?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(Error::storage)?;
        if bytes.len() < 28 {
            return Err(Error::storage("invalid credential envelope"));
        }
        let nonce: [u8; 12] = bytes[..12].try_into().unwrap();
        let mut ciphertext = bytes[12..].to_vec();
        let key = self.key()?;
        let plain = key
            .open_in_place(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(b"cutex-pan-v1"),
                &mut ciphertext,
            )
            .map_err(Error::storage)?;
        serde_json::from_slice(plain).map_err(Error::storage)
    }
    pub(super) fn save_secrets(&self, secrets: &Secrets) -> Result<()> {
        let mut bytes = serde_json::to_vec(secrets).map_err(Error::storage)?;
        let mut nonce = [0u8; 12];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(Error::storage)?;
        self.key()?
            .seal_in_place_append_tag(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(b"cutex-pan-v1"),
                &mut bytes,
            )
            .map_err(Error::storage)?;
        let mut out = nonce.to_vec();
        out.extend(bytes);
        let encoded = base64::engine::general_purpose::STANDARD.encode(out);
        crate::config::atomic::write_private_bytes_atomic(
            &self.root.join("credentials"),
            encoded.as_bytes(),
        )
        .map_err(Error::storage)
    }
}

pub(super) fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(windows)]
    {
        crate::platform::private_fs::create_new_private_file(path).map_err(std::io::Error::other)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    }
}
