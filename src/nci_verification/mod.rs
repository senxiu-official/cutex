//! Optional NCI verifier. Mail is a hint; only pinned-SSH scheduler facts reach Jobs.
mod qstat;

use crate::management::v2::human_jobs::{self, ExternalIngestError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

static CONFIG_LOCK: Mutex<()> = Mutex::new(());
const MIN_QUERY_INTERVAL: u64 = 180;
const MAX_TRACKED: usize = 4096;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Config {
    enabled: bool,
    ssh: qstat::Config,
    credential_id: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConfigInput {
    /// Omitted: keep the current value (a new configuration starts disabled).
    enabled: Option<bool>,
    username: String,
    /// One pinned key (`hostKey`) or several (`hostKeys`), not both.
    host_key: Option<String>,
    host_keys: Option<Vec<String>>,
    #[serde(default)]
    host_key_confirmed: bool,
    private_key: Option<String>,
    /// Adopt the key generated on this host (`POST /v2/nci-verification/key`)
    /// instead of supplying `privateKey`.
    #[serde(default)]
    use_generated_key: bool,
    #[serde(default = "default_interval")]
    poll_interval_seconds: u64,
}
fn default_interval() -> u64 {
    21_600
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct State {
    next_query_at: u64,
    cursor: Option<String>,
    last_error: Option<String>,
    query_count: u64,
    jobs: BTreeMap<String, Observation>,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Observation {
    last_attempt_at: u64,
    last_success_at: u64,
    failures: u32,
    pending_event: Option<Value>,
    #[serde(default)]
    pending_credential_id: Option<String>,
    /// Short recheck after an end-of-job mail while qstat still shows the job
    /// running (0 = none scheduled).
    #[serde(default)]
    recheck_at: u64,
    #[serde(default)]
    rechecks: u32,
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn root() -> Result<PathBuf, &'static str> {
    crate::config::paths::runtime_dir()
        .map(|p| p.join("nci-verification"))
        .map_err(|_| "qstat_storage_unavailable")
}
fn read_private(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, &'static str> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("qstat_storage_unavailable"),
    };
    let m = file.metadata().map_err(|_| "qstat_storage_unavailable")?;
    if !m.is_file()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o077 != 0
        || m.nlink() != 1
        || m.len() > limit
    {
        return Err("qstat_storage_permissions");
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "qstat_storage_unavailable")?;
    if bytes.len() as u64 > limit {
        return Err("qstat_storage_limit");
    }
    Ok(Some(bytes))
}
fn load_config(path: &Path) -> Result<Option<Config>, &'static str> {
    let config: Option<Config> = read_private(&path.join("config.json"), 32768)?
        .map(|b| serde_json::from_slice(&b).map_err(|_| "qstat_invalid_config"))
        .transpose()?;
    if let Some(c) = &config {
        c.ssh.validate()?;
        if uuid::Uuid::parse_str(&c.credential_id)
            .ok()
            .map(|id| id.to_string())
            .as_deref()
            != Some(c.credential_id.as_str())
        {
            return Err("qstat_invalid_config");
        }
    }
    Ok(config)
}
fn load_state(path: &Path) -> Result<State, &'static str> {
    read_private(&path.join("state-v1.json"), 4 * 1024 * 1024)?
        .map(|b| serde_json::from_slice(&b).map_err(|_| "qstat_invalid_state"))
        .transpose()
        .map(|s| s.unwrap_or_default())
}
fn save_state(path: &Path, state: &State) -> Result<(), &'static str> {
    crate::config::atomic::write_private_pretty_json_atomic(
        &path.join("state-v1.json"),
        state,
        "NCI verification state",
    )
    .map_err(|_| "qstat_storage_unavailable")
}
/// A key generated on this host before any configuration exists.
const PENDING_KEY: &str = "pending-generated.key";
/// Comment of host-generated keys; Gadi's authorized_keys line ends with it.
const KEY_COMMENT: &str = "cutex-qstat";
pub const GADI_HOME_PLACEHOLDER: &str = "<GADI_HOME>";
/// The reviewed forced-command helper for Gadi's authorized_keys.
pub const HELPER_SCRIPT: &str = include_str!("../../scripts/nci-qstat-only.py");
pub const HELPER_FILE_NAME: &str = "nci-qstat-only.py";
pub const HELPER_INSTALL_PATH: &str = "~/.cutex/nci-qstat-only.py";

fn generated_marker(path: &Path, credential_id: &str) -> PathBuf {
    path.join(format!("{credential_id}.generated"))
}
fn credential_paths(path: &Path, config: &Config) -> (PathBuf, PathBuf) {
    (
        path.join(format!("{}.key", config.credential_id)),
        path.join(format!("{}.known_hosts", config.credential_id)),
    )
}
fn validate_credential_file(path: &Path) -> Result<(), &'static str> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(path).map_err(|_| "qstat_storage_unavailable")?;
    if !m.is_file()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o077 != 0
        || m.nlink() != 1
        || m.len() > 32768
    {
        return Err("qstat_storage_permissions");
    }
    Ok(())
}
fn remove_credentials(path: &Path, config: &Config) -> Result<(), &'static str> {
    let (key, hosts) = credential_paths(path, config);
    for p in [key, hosts, generated_marker(path, &config.credential_id)] {
        match std::fs::remove_file(p) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err("qstat_storage_unavailable"),
        }
    }
    Ok(())
}
fn remove_unused_credentials(path: &Path, keep: Option<&str>) -> Result<(), &'static str> {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("qstat_storage_unavailable"),
    };
    for entry in entries {
        let entry = entry.map_err(|_| "qstat_storage_unavailable")?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(id) = name
            .strip_suffix(".key")
            .or_else(|| name.strip_suffix(".known_hosts"))
            .or_else(|| name.strip_suffix(".generated"))
        else {
            continue;
        };
        if Some(id) != keep
            && uuid::Uuid::parse_str(id)
                .ok()
                .is_some_and(|u| u.to_string() == id)
        {
            std::fs::remove_file(entry.path()).map_err(|_| "qstat_storage_unavailable")?;
        }
    }
    Ok(())
}
pub fn configure(body: &[u8]) -> Result<Value, &'static str> {
    configure_at(&root()?, body)
}
fn configure_at(path: &Path, body: &[u8]) -> Result<Value, &'static str> {
    if body.len() > 32768 {
        return Err("qstat_invalid_config");
    }
    let input: ConfigInput = serde_json::from_slice(body).map_err(|_| "qstat_invalid_config")?;
    let host_keys = match (input.host_key, input.host_keys) {
        (Some(key), None) => vec![key],
        (None, Some(keys)) => keys,
        _ => return Err("qstat_invalid_host_key"),
    };
    let ssh = qstat::Config {
        username: input.username,
        host_keys,
        poll_interval_seconds: input.poll_interval_seconds,
    };
    ssh.validate()?;
    if !input.host_key_confirmed {
        return Err("qstat_host_key_confirmation_required");
    }
    let _guard = CONFIG_LOCK
        .lock()
        .map_err(|_| "qstat_storage_unavailable")?;
    let old = load_config(path)?;
    let enabled = input
        .enabled
        .unwrap_or_else(|| old.as_ref().is_some_and(|c| c.enabled));
    crate::management::client_tokens::ensure_private_dir(path)
        .map_err(|_| "qstat_storage_permissions")?;
    let pending_path = path.join(PENDING_KEY);
    let new_key = match (input.private_key, input.use_generated_key) {
        (Some(_), true) => return Err("qstat_invalid_config"),
        (Some(key), false) => Some((key, false)),
        (None, true) => {
            let bytes = read_private(&pending_path, 32768)?.ok_or("qstat_private_key_required")?;
            Some((
                String::from_utf8(bytes).map_err(|_| "qstat_invalid_private_key")?,
                true,
            ))
        }
        (None, false) => None,
    };
    let credential_id = if let Some((key, generated)) = new_key {
        if key.len() > 16384
            || !key.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----\n")
            || !key
                .trim_end()
                .ends_with("-----END OPENSSH PRIVATE KEY-----")
            || key.contains('\0')
        {
            return Err("qstat_invalid_private_key");
        }
        // Unattended use cannot unlock a passphrase; refuse such keys up front.
        match openssh_public_key(&key) {
            Some(public) if !public.encrypted => {}
            Some(_) => return Err("qstat_private_key_encrypted"),
            None => return Err("qstat_invalid_private_key"),
        }
        let id = uuid::Uuid::new_v4().to_string();
        let next = Config {
            enabled,
            ssh: ssh.clone(),
            credential_id: id.clone(),
        };
        let (key_path, hosts_path) = credential_paths(path, &next);
        crate::config::atomic::write_private_bytes_atomic(&key_path, key.as_bytes())
            .map_err(|_| "qstat_storage_unavailable")?;
        if crate::config::atomic::write_private_bytes_atomic(
            &hosts_path,
            ssh.known_hosts().as_bytes(),
        )
        .is_err()
            || (generated
                && crate::config::atomic::write_private_bytes_atomic(
                    &generated_marker(path, &id),
                    b"",
                )
                .is_err())
        {
            let _ = remove_credentials(path, &next);
            return Err("qstat_storage_unavailable");
        }
        id
    } else {
        old.as_ref()
            .filter(|c| c.ssh.username == ssh.username && c.ssh.host_keys == ssh.host_keys)
            .map(|c| c.credential_id.clone())
            .ok_or("qstat_private_key_required")?
    };
    let config = Config {
        enabled,
        ssh,
        credential_id,
    };
    if crate::config::atomic::write_private_pretty_json_atomic(
        &path.join("config.json"),
        &config,
        "NCI configuration",
    )
    .is_err()
    {
        if old
            .as_ref()
            .is_none_or(|c| c.credential_id != config.credential_id)
        {
            let _ = remove_credentials(path, &config);
        }
        return Err("qstat_storage_unavailable");
    }
    remove_unused_credentials(path, Some(&config.credential_id))?;
    // A configured key (generated or supplied) supersedes a pending one.
    remove_if_present(&pending_path)?;
    status_at(path)
}
fn remove_if_present(path: &Path) -> Result<(), &'static str> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("qstat_storage_unavailable"),
    }
}

/// Generates the dedicated ed25519 qstat key on this host. The private key is
/// stored like a supplied one and never returned. With a configuration the
/// configured credential is replaced at once; otherwise the key waits for the
/// next `PUT config` with `useGeneratedKey`.
pub fn generate_key(body: &[u8]) -> Result<Value, &'static str> {
    generate_key_at(&root()?, body)
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GenerateInput {
    #[serde(default)]
    replace: bool,
}
fn generate_key_at(path: &Path, body: &[u8]) -> Result<Value, &'static str> {
    let input: GenerateInput = if body.iter().all(u8::is_ascii_whitespace) {
        GenerateInput { replace: false }
    } else if body.len() > 1024 {
        return Err("qstat_invalid_request");
    } else {
        serde_json::from_slice(body).map_err(|_| "qstat_invalid_request")?
    };
    let _guard = CONFIG_LOCK
        .lock()
        .map_err(|_| "qstat_storage_unavailable")?;
    let old = load_config(path)?;
    let pending_path = path.join(PENDING_KEY);
    let pending = std::fs::symlink_metadata(&pending_path).is_ok();
    if (old.is_some() || pending) && !input.replace {
        return Err("qstat_key_exists");
    }
    let key = generate_openssh_ed25519(KEY_COMMENT)?;
    crate::management::client_tokens::ensure_private_dir(path)
        .map_err(|_| "qstat_storage_permissions")?;
    match old {
        Some(old) => {
            let next = Config {
                credential_id: uuid::Uuid::new_v4().to_string(),
                ..old.clone()
            };
            let (key_path, hosts_path) = credential_paths(path, &next);
            let written = crate::config::atomic::write_private_bytes_atomic(&key_path, key.as_bytes())
                .and_then(|()| {
                    crate::config::atomic::write_private_bytes_atomic(
                        &hosts_path,
                        next.ssh.known_hosts().as_bytes(),
                    )
                })
                .and_then(|()| {
                    crate::config::atomic::write_private_bytes_atomic(
                        &generated_marker(path, &next.credential_id),
                        b"",
                    )
                })
                .and_then(|()| {
                    crate::config::atomic::write_private_pretty_json_atomic(
                        &path.join("config.json"),
                        &next,
                        "NCI configuration",
                    )
                });
            if written.is_err() {
                let _ = remove_credentials(path, &next);
                return Err("qstat_storage_unavailable");
            }
            remove_unused_credentials(path, Some(&next.credential_id))?;
            remove_if_present(&pending_path)?;
        }
        None => {
            crate::config::atomic::write_private_bytes_atomic(&pending_path, key.as_bytes())
                .map_err(|_| "qstat_storage_unavailable")?;
        }
    }
    status_at(path)
}

/// An unencrypted `openssh-key-v1` ed25519 private key (as `ssh-keygen -t
/// ed25519 -N ""` writes it), from the operating system's CSPRNG.
fn generate_openssh_ed25519(comment: &str) -> Result<String, &'static str> {
    use base64::Engine;
    use ring::rand::SecureRandom;
    use ring::signature::KeyPair;
    let rng = ring::rand::SystemRandom::new();
    let mut seed = [0u8; 32];
    let mut check = [0u8; 4];
    rng.fill(&mut seed).map_err(|_| "qstat_key_generation_failed")?;
    rng.fill(&mut check).map_err(|_| "qstat_key_generation_failed")?;
    let pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(&seed)
        .map_err(|_| "qstat_key_generation_failed")?;
    let public = pair.public_key().as_ref();
    fn put(out: &mut Vec<u8>, value: &[u8]) {
        out.extend_from_slice(&(value.len() as u32).to_be_bytes());
        out.extend_from_slice(value);
    }
    let mut blob = Vec::new();
    put(&mut blob, b"ssh-ed25519");
    put(&mut blob, public);
    let mut private = Vec::new();
    private.extend_from_slice(&check);
    private.extend_from_slice(&check);
    put(&mut private, b"ssh-ed25519");
    put(&mut private, public);
    let mut secret = seed.to_vec();
    secret.extend_from_slice(public);
    put(&mut private, &secret);
    put(&mut private, comment.as_bytes());
    let mut pad = 1u8;
    while private.len() % 8 != 0 {
        private.push(pad);
        pad += 1;
    }
    let mut raw = b"openssh-key-v1\0".to_vec();
    put(&mut raw, b"none");
    put(&mut raw, b"none");
    put(&mut raw, b"");
    raw.extend_from_slice(&1u32.to_be_bytes());
    put(&mut raw, &blob);
    put(&mut raw, &private);
    // Best effort: do not leave the seed in this stack frame.
    seed.fill(0);
    secret.fill(0);
    let encoded = base64::engine::general_purpose::STANDARD.encode(&raw);
    private.fill(0);
    raw.fill(0);
    let mut pem = String::from("-----BEGIN OPENSSH PRIVATE KEY-----\n");
    for chunk in encoded.as_bytes().chunks(70) {
        pem.push_str(std::str::from_utf8(chunk).map_err(|_| "qstat_key_generation_failed")?);
        pem.push('\n');
    }
    pem.push_str("-----END OPENSSH PRIVATE KEY-----\n");
    Ok(pem)
}

/// The helper script for Gadi's authorized_keys forced command.
pub fn helper() -> Value {
    json!({
        "fileName": HELPER_FILE_NAME,
        "installPath": HELPER_INSTALL_PATH,
        "script": HELPER_SCRIPT,
        "sha256": format!("{:x}", Sha256::digest(HELPER_SCRIPT.as_bytes())),
    })
}
pub fn clear_config() -> Result<Value, &'static str> {
    clear_at(&root()?)
}
fn clear_at(path: &Path) -> Result<Value, &'static str> {
    let _guard = CONFIG_LOCK
        .lock()
        .map_err(|_| "qstat_storage_unavailable")?;
    match std::fs::remove_file(path.join("config.json")) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(_) => return Err("qstat_storage_unavailable"),
    }
    // Also clear a key left behind if a previous configuration write crashed,
    // and a generated key that was never configured.
    remove_unused_credentials(path, None)?;
    remove_if_present(&path.join(PENDING_KEY))?;
    if path.exists() {
        std::fs::File::open(path)
            .and_then(|f| f.sync_all())
            .map_err(|_| "qstat_storage_unavailable")?;
    }
    status_at(path)
}
pub fn status() -> Result<Value, &'static str> {
    status_at(&root()?)
}
fn status_at(path: &Path) -> Result<Value, &'static str> {
    let config = load_config(path)?;
    let state = load_state(path)?;
    // Public parts only: the private key never leaves the private store.
    let read_public = |key_path: &Path| {
        validate_credential_file(key_path).ok()?;
        let bytes = read_private(key_path, 32768).ok()??;
        openssh_public_key(std::str::from_utf8(&bytes).ok()?)
    };
    let configured_key = config.as_ref().and_then(|c| {
        let (key_path, _) = credential_paths(path, c);
        read_public(&key_path).map(|k| (k, generated_marker(path, &c.credential_id).is_file()))
    });
    let pending_key = if config.is_none() {
        read_public(&path.join(PENDING_KEY))
    } else {
        None
    };
    let private_key_stored = configured_key.is_some();
    let pending_generated = pending_key.is_some();
    let (public_key, key_source) = match (configured_key, pending_key) {
        (Some((k, generated)), _) => (Some(k), Some(if generated { "generated" } else { "uploaded" })),
        (None, Some(k)) => (Some(k), Some("generated")),
        (None, None) => (None, None),
    };
    let public_line = public_key.as_ref().map(|k| {
        if key_source == Some("generated") {
            format!("{} {KEY_COMMENT}", k.line)
        } else {
            k.line.clone()
        }
    });
    let authorized_keys_template = public_key.as_ref().map(|k| {
        format!(
            "restrict,command=\"/usr/bin/python3 -I {GADI_HOME_PLACEHOLDER}/.cutex/{HELPER_FILE_NAME}\" {} {KEY_COMMENT}",
            k.line
        )
    });
    let host_keys = config.as_ref().map(|c| c.ssh.host_keys.clone()).unwrap_or_default();
    let fingerprints: Vec<Option<String>> = host_keys.iter().map(|k| host_key_fingerprint(k)).collect();
    let max_time = |f: fn(&Observation) -> u64| state.jobs.values().map(f).max().filter(|t| *t > 0);
    Ok(
        json!({"configured":config.is_some(),"enabled":config.as_ref().is_some_and(|c|c.enabled),"host":"gadi.nci.org.au","pollIntervalSeconds":config.as_ref().map(|c|c.ssh.poll_interval_seconds).unwrap_or(default_interval()),"minimumQueryIntervalSeconds":MIN_QUERY_INTERVAL,"queryCount":state.query_count,"errorCode":state.last_error,
            "username":config.as_ref().map(|c|&c.ssh.username),
            "hostKeys":host_keys,
            "hostKeyFingerprints":fingerprints,
            "privateKeyStored":private_key_stored,
            "pendingGeneratedKey":pending_generated,
            "keySource":key_source,
            "publicKey":public_line,
            "authorizedKeysTemplate":authorized_keys_template,
            "gadiHomePlaceholder":GADI_HOME_PLACEHOLDER,
            "publicKeyFingerprint":public_key.as_ref().map(|k|&k.fingerprint),
            "trackedJobs":state.jobs.len(),
            "lastQueryAt":max_time(|o| o.last_attempt_at),
            "lastSuccessAt":max_time(|o| o.last_success_at),
            "nextQueryAllowedAt":(state.next_query_at > 0).then_some(state.next_query_at)}),
    )
}

/// `SHA256:<base64, no padding>` of an SSH public key blob (as `ssh-keygen -l` prints it).
fn ssh_fingerprint(blob: &[u8]) -> String {
    use base64::Engine;
    format!(
        "SHA256:{}",
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(Sha256::digest(blob))
    )
}
fn host_key_fingerprint(host_key: &str) -> Option<String> {
    use base64::Engine;
    let (_, encoded) = host_key.split_once(' ')?;
    let blob = base64::engine::general_purpose::STANDARD.decode(encoded).ok()?;
    Some(ssh_fingerprint(&blob))
}

struct OpensshPublic {
    /// `ALGORITHM BASE64` (an authorized_keys key without options or comment).
    line: String,
    fingerprint: String,
    encrypted: bool,
}
/// Reads the unencrypted public header of an `openssh-key-v1` private key.
fn openssh_public_key(key: &str) -> Option<OpensshPublic> {
    use base64::Engine;
    let body: String = key
        .trim()
        .strip_prefix("-----BEGIN OPENSSH PRIVATE KEY-----")?
        .strip_suffix("-----END OPENSSH PRIVATE KEY-----")?
        .split_whitespace()
        .collect();
    let raw = base64::engine::general_purpose::STANDARD.decode(body).ok()?;
    let mut rest = raw.strip_prefix(b"openssh-key-v1\0".as_slice())?;
    fn take<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
        let len = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let value = rest.get(4..4usize.checked_add(len)?)?;
        *rest = &rest[4 + len..];
        Some(value)
    }
    let cipher = take(&mut rest)?;
    let _kdf = take(&mut rest)?;
    let _kdf_options = take(&mut rest)?;
    let count = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?);
    rest = &rest[4..];
    if count != 1 {
        return None;
    }
    let blob = take(&mut rest)?;
    let mut inner = blob;
    let algorithm = std::str::from_utf8(take(&mut inner)?).ok()?;
    if algorithm.is_empty() || !algorithm.bytes().all(|b| b.is_ascii_graphic()) {
        return None;
    }
    Some(OpensshPublic {
        line: format!(
            "{algorithm} {}",
            base64::engine::general_purpose::STANDARD.encode(blob)
        ),
        fingerprint: ssh_fingerprint(blob),
        encrypted: cipher != b"none",
    })
}
fn state_key(target: &qstat::Target) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!("{}:{}", target.remote_id, target.correlation_token))
    )
}
fn retry_delay(failures: u32) -> u64 {
    300u64
        .saturating_mul(1u64 << failures.saturating_sub(1).min(4))
        .min(3600)
}
fn due(observation: &Observation, time: u64, interval: u64, triggered: bool) -> bool {
    if time
        < observation
            .last_attempt_at
            .saturating_add(MIN_QUERY_INTERVAL)
    {
        return false;
    }
    if observation.failures > 0 {
        return time
            >= observation
                .last_attempt_at
                .saturating_add(retry_delay(observation.failures));
    }
    triggered
        || observation.last_success_at == 0
        || (observation.recheck_at != 0 && time >= observation.recheck_at)
        || time >= observation.last_success_at.saturating_add(interval)
}
/// Rechecks after an end mail: 5, 10, 20, 40, 60, 60 minutes, then the normal interval.
const MAX_RECHECKS: u32 = 6;
fn lock_worker(path: &Path) -> Result<std::fs::File, &'static str> {
    use fs2::FileExt;
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path.join("worker.lock"))
        .map_err(|_| "qstat_storage_unavailable")?;
    file.try_lock_exclusive().map_err(|_| "qstat_busy")?;
    Ok(file)
}

/// The lookup authenticates and matches registration before any network query.
pub(crate) fn verify_trigger(event: Value) -> Result<Value, ExternalIngestError> {
    trigger_with(
        event,
        human_jobs::lookup_external_event,
        |target, end_hint| {
            verify_target(
                &root().map_err(ExternalIngestError::Retry)?,
                target,
                true,
                end_hint,
            )
            .map_err(ExternalIngestError::Retry)
        },
    )
}
fn trigger_with(
    event: Value,
    lookup: impl FnOnce(Value) -> Result<Value, ExternalIngestError>,
    verify: impl FnOnce(qstat::Target, bool) -> Result<Value, ExternalIngestError>,
) -> Result<Value, ExternalIngestError> {
    // The mail's claimed state only schedules earlier (still rate-limited)
    // rechecks; it never reaches the Job Service.
    let end_hint = matches!(
        event["state"].as_str(),
        Some("exited" | "failed" | "cancelled")
    );
    let reply = lookup(
        json!({"source":event["source"],"remoteId":event["remoteId"],"correlationToken":event["correlationToken"]}),
    )?;
    if reply["status"] != "matched" {
        return Ok(reply);
    }
    let target = serde_json::from_value(reply["target"].clone())
        .map_err(|_| ExternalIngestError::Retry("qstat_invalid_target"))?;
    verify(target, end_hint)
}

fn verify_target(
    path: &Path,
    target: qstat::Target,
    triggered: bool,
    end_hint: bool,
) -> Result<Value, &'static str> {
    let config = load_config(path)?
        .filter(|c| c.enabled)
        .ok_or("qstat_not_configured")?;
    let _lock = lock_worker(path)?;
    let mut state = load_state(path)?;
    let result = verify_with(
        path,
        &config,
        &mut state,
        &target,
        triggered,
        end_hint,
        now(),
        |c, t, k, h| {
            validate_credential_file(k)?;
            validate_credential_file(h)?;
            qstat::query(c, t, k, h)
        },
        |event| {
            human_jobs::ingest_external_event(event).map_err(|e| match e {
                ExternalIngestError::Permanent => "job_service_rejected",
                ExternalIngestError::Retry(c) => c,
            })
        },
    );
    if !matches!(result, Err("qstat_deferred" | "qstat_busy")) {
        state.last_error = result.as_ref().err().map(|c| c.to_string());
    }
    save_state(path, &state)?;
    result
}
fn verify_with(
    path: &Path,
    config: &Config,
    state: &mut State,
    target: &qstat::Target,
    triggered: bool,
    end_hint: bool,
    time: u64,
    mut query: impl FnMut(
        &qstat::Config,
        &qstat::Target,
        &Path,
        &Path,
    ) -> Result<Option<Value>, &'static str>,
    mut ingest: impl FnMut(Value) -> Result<Value, &'static str>,
) -> Result<Value, &'static str> {
    let key = state_key(target);
    if !state.jobs.contains_key(&key) && state.jobs.len() >= MAX_TRACKED {
        state
            .jobs
            .retain(|_, o| time.saturating_sub(o.last_attempt_at) < 30 * 86400);
        if state.jobs.len() >= MAX_TRACKED {
            return Err("qstat_tracking_limit");
        }
    }
    let observation = state.jobs.entry(key.clone()).or_default();
    if observation.pending_credential_id.as_deref() != Some(&config.credential_id) {
        observation.pending_event = None;
        observation.pending_credential_id = None;
    }
    if observation.pending_event.is_none() {
        if time < state.next_query_at
            || !due(
                observation,
                time,
                config.ssh.poll_interval_seconds,
                triggered,
            )
        {
            return Err("qstat_deferred");
        }
        observation.last_attempt_at = time;
        state.next_query_at = time.saturating_add(MIN_QUERY_INTERVAL);
        state.query_count = state.query_count.saturating_add(1);
        save_state(path, state)?;
        let (key_path, hosts_path) = credential_paths(path, config);
        let result = query(&config.ssh, target, &key_path, &hosts_path);
        let observation = state.jobs.get_mut(&key).expect("inserted observation");
        match result {
            Ok(event) => {
                let nonterminal = event
                    .as_ref()
                    .is_none_or(|e| matches!(e["state"].as_str(), Some("queued" | "running")));
                observation.last_success_at = time;
                observation.failures = 0;
                // Mail reported the end but the scheduler has not recorded it yet
                // (e.g. still exiting): look again soon instead of after the full interval.
                if nonterminal
                    && (end_hint || observation.recheck_at != 0)
                    && observation.rechecks < MAX_RECHECKS
                {
                    observation.rechecks += 1;
                    observation.recheck_at = time.saturating_add(retry_delay(observation.rechecks));
                } else {
                    observation.recheck_at = 0;
                    if !nonterminal {
                        observation.rechecks = 0;
                    }
                }
                let Some(mut event) = event else {
                    save_state(path, state)?;
                    return Ok(json!({"status":"observed"}));
                };
                // Nonterminal observations refresh freshness without consuming the finite receipt budget.
                if nonterminal {
                    event["eventId"] = json!(format!(
                        "{}:{time}",
                        event["eventId"].as_str().ok_or("qstat_invalid_result")?
                    ));
                }
                observation.pending_event = Some(event);
                observation.pending_credential_id = Some(config.credential_id.clone());
                save_state(path, state)?;
            }
            Err(code) => {
                observation.failures = observation.failures.saturating_add(1);
                return Err(code);
            }
        }
    }
    if load_config(path)?.as_ref() != Some(config) {
        return Err("qstat_configuration_changed");
    }
    let event = state.jobs[&key]
        .pending_event
        .clone()
        .ok_or("qstat_invalid_state")?;
    let reply = ingest(event)?;
    if matches!(
        reply["status"].as_str(),
        Some("applied" | "duplicate" | "superseded" | "terminal_ignored")
    ) {
        let observation = state.jobs.get_mut(&key).expect("observation");
        observation.pending_event = None;
        observation.pending_credential_id = None;
    } else if reply["status"] == "permanent_conflict" {
        let observation = state.jobs.get_mut(&key).expect("observation");
        observation.pending_event = None;
        observation.pending_credential_id = None;
    } else {
        return Err("job_service_rejected");
    }
    Ok(reply)
}

pub fn start_worker() {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("cutex-nci-verification".into())
        .spawn(|| loop {
            if let Ok(path) = root() {
                if let Err(code) = poll_once(&path) {
                    if path.exists() && !matches!(code, "qstat_busy" | "qstat_deferred") {
                        if let Ok(_lock) = lock_worker(&path) {
                            if let Ok(mut state) = load_state(&path) {
                                state.last_error = Some(code.into());
                                let _ = save_state(&path, &state);
                            }
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(30));
        });
}
fn poll_once(path: &Path) -> Result<(), &'static str> {
    let Some(config) = load_config(path)?.filter(|c| c.enabled) else {
        return Ok(());
    };
    let snapshot = {
        let _lock = lock_worker(path)?;
        let state = load_state(path)?;
        if now() < state.next_query_at {
            return Err("qstat_deferred");
        }
        state
    };
    let cursor = snapshot.cursor.clone();
    let reply = human_jobs::list_external_verification_targets(json!({"limit":64,"cursor":cursor}))
        .map_err(|_| "job_service_unavailable")?;
    let targets = reply["targets"]
        .as_array()
        .ok_or("job_service_operation_unsupported")?;
    let targets = order_targets(targets, &snapshot)?;
    let mut error = None;
    for target in targets {
        // A fresh lookup prevents a manually settled job from starting a new query.
        let matched = human_jobs::lookup_external_event(json!({"source":"nci","remoteId":target.remote_id,"correlationToken":target.correlation_token})).map_err(|_| "job_service_unavailable")?;
        if matched["status"] != "matched" {
            continue;
        }
        let current = serde_json::from_value(matched["target"].clone())
            .map_err(|_| "qstat_invalid_target")?;
        match verify_target(path, current, false, false) {
            Ok(_) | Err("qstat_deferred") => (),
            Err(code) => {
                error = Some(code);
            }
        }
        if load_config(path)?.as_ref() != Some(&config) {
            return Ok(());
        }
    }
    let _lock = lock_worker(path)?;
    let mut state = load_state(path)?;
    state.cursor = reply["nextCursor"].as_str().map(str::to_owned);
    if let Some(code) = error {
        state.last_error = Some(code.into());
    }
    save_state(path, &state)
}

fn order_targets(values: &[Value], state: &State) -> Result<Vec<qstat::Target>, &'static str> {
    let mut targets: Vec<qstat::Target> = values
        .iter()
        .map(|v| serde_json::from_value(v.clone()).map_err(|_| "qstat_invalid_target"))
        .collect::<Result<_, _>>()?;
    targets.sort_by_cached_key(|t| {
        let key = state_key(t);
        (
            state.jobs.get(&key).map(|o| o.last_attempt_at).unwrap_or(0),
            key,
        )
    });
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    fn config_body() -> Value {
        let mut key = (11u32).to_be_bytes().to_vec();
        key.extend_from_slice(b"ssh-ed25519");
        key.extend_from_slice(&(32u32).to_be_bytes());
        key.extend_from_slice(&[1; 32]);
        json!({"enabled":true,"username":"owner","hostKey":format!("ssh-ed25519 {}",base64::engine::general_purpose::STANDARD.encode(key)),"hostKeyConfirmed":true,"privateKey":fixture_private_key("none")})
    }
    fn ssh_string(out: &mut Vec<u8>, value: &[u8]) {
        out.extend_from_slice(&(value.len() as u32).to_be_bytes());
        out.extend_from_slice(value);
    }
    fn fixture_public_blob() -> Vec<u8> {
        let mut blob = Vec::new();
        ssh_string(&mut blob, b"ssh-ed25519");
        ssh_string(&mut blob, &[9; 32]);
        blob
    }
    /// Synthetic `openssh-key-v1` structure (no real key material).
    fn fixture_private_key(cipher: &str) -> String {
        let mut raw = b"openssh-key-v1\0".to_vec();
        ssh_string(&mut raw, cipher.as_bytes());
        ssh_string(&mut raw, if cipher == "none" { b"none" } else { b"bcrypt" });
        ssh_string(&mut raw, b"");
        raw.extend_from_slice(&1u32.to_be_bytes());
        ssh_string(&mut raw, &fixture_public_blob());
        ssh_string(&mut raw, b"fixture-only private section");
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        let lines: Vec<&str> = encoded.as_bytes().chunks(70).map(|c| std::str::from_utf8(c).unwrap()).collect();
        format!("-----BEGIN OPENSSH PRIVATE KEY-----\n{}\n-----END OPENSSH PRIVATE KEY-----\n", lines.join("\n"))
    }
    fn host_key_body() -> Value {
        let mut body = config_body();
        body.as_object_mut().unwrap().remove("privateKey");
        body
    }
    #[test]
    fn host_generated_key_is_pending_then_adopted_and_never_returned() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("cutex-nci-{}", uuid::Uuid::new_v4()));
        // Without a key nothing can be configured from the generated path.
        let mut body = host_key_body();
        body["useGeneratedKey"] = json!(true);
        assert_eq!(
            configure_at(&path, &serde_json::to_vec(&body).unwrap()).unwrap_err(),
            "qstat_private_key_required"
        );
        let status = generate_key_at(&path, b"").unwrap();
        assert_eq!(status["configured"], false);
        assert_eq!(status["pendingGeneratedKey"], true);
        assert_eq!(status["privateKeyStored"], false);
        assert_eq!(status["keySource"], "generated");
        let public = status["publicKey"].as_str().unwrap().to_string();
        assert!(public.starts_with("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5") && public.ends_with(" cutex-qstat"));
        let fingerprint = status["publicKeyFingerprint"].as_str().unwrap().to_string();
        assert!(fingerprint.starts_with("SHA256:"));
        let template = status["authorizedKeysTemplate"].as_str().unwrap();
        assert!(template.starts_with(
            "restrict,command=\"/usr/bin/python3 -I <GADI_HOME>/.cutex/nci-qstat-only.py\" ssh-ed25519 "
        ));
        assert!(template.ends_with(" cutex-qstat"));
        assert!(!status.to_string().contains("PRIVATE KEY"));
        let pending = path.join(PENDING_KEY);
        assert_eq!(std::fs::metadata(&pending).unwrap().permissions().mode() & 0o777, 0o600);
        let pem = std::fs::read_to_string(&pending).unwrap();
        let parsed = openssh_public_key(&pem).unwrap();
        assert!(!parsed.encrypted);
        assert_eq!(format!("{} cutex-qstat", parsed.line), public);
        // A second generation needs replace.
        assert_eq!(generate_key_at(&path, b"{}").unwrap_err(), "qstat_key_exists");
        assert_eq!(generate_key_at(&path, br#"{"bogus":1}"#).unwrap_err(), "qstat_invalid_request");
        let replaced = generate_key_at(&path, br#"{"replace":true}"#).unwrap();
        assert_ne!(replaced["publicKey"].as_str().unwrap(), public);
        let public = replaced["publicKey"].as_str().unwrap().to_string();
        // privateKey and useGeneratedKey together are ambiguous.
        let mut both = body.clone();
        both["privateKey"] = json!(fixture_private_key("none"));
        assert_eq!(
            configure_at(&path, &serde_json::to_vec(&both).unwrap()).unwrap_err(),
            "qstat_invalid_config"
        );
        let status = configure_at(&path, &serde_json::to_vec(&body).unwrap()).unwrap();
        assert_eq!(status["configured"], true);
        assert_eq!(status["privateKeyStored"], true);
        assert_eq!(status["pendingGeneratedKey"], false);
        assert_eq!(status["keySource"], "generated");
        assert_eq!(status["publicKey"].as_str().unwrap(), public);
        assert!(!pending.exists());
        let config = load_config(&path).unwrap().unwrap();
        let (key_path, _) = credential_paths(&path, &config);
        assert_eq!(std::fs::read_to_string(&key_path).unwrap(), pem_for(&path, &config));
        // Replacing with a configuration swaps the configured credential at once.
        assert_eq!(generate_key_at(&path, b"").unwrap_err(), "qstat_key_exists");
        let swapped = generate_key_at(&path, br#"{"replace":true}"#).unwrap();
        let next = load_config(&path).unwrap().unwrap();
        assert_ne!(next.credential_id, config.credential_id);
        assert_eq!(next.ssh, config.ssh);
        assert_eq!(next.enabled, config.enabled);
        assert!(!key_path.exists() && !generated_marker(&path, &config.credential_id).exists());
        assert_eq!(swapped["keySource"], "generated");
        assert_ne!(swapped["publicKey"].as_str().unwrap(), public);
        // A supplied key replaces it and reports as uploaded.
        let status = configure_at(&path, &serde_json::to_vec(&config_body()).unwrap()).unwrap();
        assert_eq!(status["keySource"], "uploaded");
        assert!(!generated_marker(&path, &next.credential_id).exists());
        assert!(!status["publicKey"].as_str().unwrap().ends_with("cutex-qstat"));
        // Clearing removes a pending key too.
        clear_at(&path).unwrap();
        generate_key_at(&path, b"").unwrap();
        clear_at(&path).unwrap();
        assert!(!pending.exists());
        let status = status_at(&path).unwrap();
        assert_eq!(status["publicKey"], Value::Null);
        assert_eq!(status["keySource"], Value::Null);
        assert_eq!(status["authorizedKeysTemplate"], Value::Null);
        std::fs::remove_dir_all(path).unwrap();
    }
    fn pem_for(path: &Path, config: &Config) -> String {
        std::fs::read_to_string(credential_paths(path, config).0).unwrap()
    }
    #[test]
    fn generated_key_matches_openssh_layout_and_helper_is_embedded() {
        use base64::Engine;
        let pem = generate_openssh_ed25519("cutex-qstat").unwrap();
        let body: String = pem
            .trim()
            .strip_prefix("-----BEGIN OPENSSH PRIVATE KEY-----")
            .unwrap()
            .strip_suffix("-----END OPENSSH PRIVATE KEY-----")
            .unwrap()
            .split_whitespace()
            .collect();
        assert!(pem.lines().all(|l| l.len() <= 70 || l.starts_with("-----")));
        let raw = base64::engine::general_purpose::STANDARD.decode(body).unwrap();
        let mut rest = raw.strip_prefix(b"openssh-key-v1\0".as_slice()).unwrap();
        fn take<'a>(rest: &mut &'a [u8]) -> &'a [u8] {
            let len = u32::from_be_bytes(rest[..4].try_into().unwrap()) as usize;
            let value = &rest[4..4 + len];
            *rest = &rest[4 + len..];
            value
        }
        assert_eq!(take(&mut rest), b"none");
        assert_eq!(take(&mut rest), b"none");
        assert_eq!(take(&mut rest), b"");
        rest = &rest[4..];
        let blob = take(&mut rest).to_vec();
        let mut private = take(&mut rest);
        assert!(rest.is_empty());
        assert_eq!(private.len() % 8, 0);
        assert_eq!(private[..4], private[4..8]);
        private = &private[8..];
        assert_eq!(take(&mut private), b"ssh-ed25519");
        let public = take(&mut private).to_vec();
        let secret = take(&mut private).to_vec();
        assert_eq!(take(&mut private), b"cutex-qstat");
        assert!(private.iter().enumerate().all(|(i, b)| *b == i as u8 + 1));
        assert_eq!(&secret[32..], public.as_slice());
        // The seed really derives the public key.
        use ring::signature::KeyPair;
        let pair = ring::signature::Ed25519KeyPair::from_seed_and_public_key(&secret[..32], &public).unwrap();
        assert_eq!(pair.public_key().as_ref(), public.as_slice());
        let mut expected_blob = Vec::new();
        ssh_string(&mut expected_blob, b"ssh-ed25519");
        ssh_string(&mut expected_blob, &public);
        assert_eq!(blob, expected_blob);
        assert_ne!(generate_openssh_ed25519("cutex-qstat").unwrap(), pem);
        let helper = helper();
        assert_eq!(helper["fileName"], "nci-qstat-only.py");
        assert!(helper["script"].as_str().unwrap().contains("SSH_ORIGINAL_COMMAND"));
        assert_eq!(helper["sha256"].as_str().unwrap().len(), 64);
    }
    #[test]
    fn status_reports_public_parts_and_refuses_encrypted_keys() {
        let (path, config, _) = fixture();
        let status = status_at(&path).unwrap();
        let blob = fixture_public_blob();
        assert_eq!(status["username"], "owner");
        assert_eq!(status["privateKeyStored"], true);
        assert_eq!(
            status["publicKey"],
            format!("ssh-ed25519 {}", base64::engine::general_purpose::STANDARD.encode(&blob))
        );
        assert_eq!(status["publicKeyFingerprint"], ssh_fingerprint(&blob));
        assert_eq!(status["hostKeys"], json!(config.ssh.host_keys));
        let fingerprint = status["hostKeyFingerprints"][0].as_str().unwrap();
        assert!(fingerprint.starts_with("SHA256:") && !fingerprint.ends_with('='));
        assert_eq!(status["trackedJobs"], 0);
        assert_eq!(status["lastQueryAt"], Value::Null);
        let mut body = config_body();
        body["privateKey"] = json!(fixture_private_key("aes256-ctr"));
        assert_eq!(
            configure_at(&path, &serde_json::to_vec(&body).unwrap()).unwrap_err(),
            "qstat_private_key_encrypted"
        );
        body["privateKey"] = json!("-----BEGIN OPENSSH PRIVATE KEY-----\nbm90LWEta2V5\n-----END OPENSSH PRIVATE KEY-----\n");
        assert_eq!(
            configure_at(&path, &serde_json::to_vec(&body).unwrap()).unwrap_err(),
            "qstat_invalid_private_key"
        );
        // ssh-keygen -l style: SHA256 of the blob, unpadded base64.
        assert_eq!(ssh_fingerprint(b"abc"), "SHA256:ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0");
        std::fs::remove_dir_all(path).unwrap();
    }
    fn fixture() -> (PathBuf, Config, qstat::Target) {
        let path = std::env::temp_dir().join(format!("cutex-nci-{}", uuid::Uuid::new_v4()));
        configure_at(&path, &serde_json::to_vec(&config_body()).unwrap()).unwrap();
        let config = load_config(&path).unwrap().unwrap();
        let target = qstat::Target {
            remote_id: "123.gadi-pbs".into(),
            correlation_token: "a".repeat(32),
            remote_name: None,
            project: None,
            current_state: "registered".into(),
        };
        (path, config, target)
    }
    fn event() -> Value {
        json!({"source":"nci","remoteId":"123.gadi-pbs","correlationToken":"a".repeat(32),"eventId":"qstat:fixture","state":"exited","exitCode":0})
    }
    #[test]
    fn dedicated_credentials_are_write_only_confirmed_and_deleted() {
        use std::os::unix::fs::PermissionsExt;
        let (path, config, _) = fixture();
        let status = status_at(&path).unwrap();
        assert_eq!(status["pollIntervalSeconds"], 21600);
        assert!(!status.to_string().contains("fixture-only"));
        assert!(status.get("privateKey").is_none());
        let (key, hosts) = credential_paths(&path, &config);
        assert_eq!(
            std::fs::metadata(&key).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let mut body = config_body();
        body["hostKeyConfirmed"] = json!(false);
        assert_eq!(
            configure_at(&path, &serde_json::to_vec(&body).unwrap()).unwrap_err(),
            "qstat_host_key_confirmation_required"
        );
        body["hostKeyConfirmed"] = json!(true);
        body.as_object_mut().unwrap().remove("privateKey");
        body["pollIntervalSeconds"] = json!(3600);
        configure_at(&path, &serde_json::to_vec(&body).unwrap()).unwrap();
        assert_eq!(
            load_config(&path).unwrap().unwrap().credential_id,
            config.credential_id
        );
        let orphan = path.join(format!("{}.key", uuid::Uuid::new_v4()));
        crate::config::atomic::write_private_bytes_atomic(&orphan, b"test-only orphan").unwrap();
        clear_at(&path).unwrap();
        assert!(!key.exists() && !hosts.exists() && !orphan.exists());
        assert_eq!(status_at(&path).unwrap()["configured"], false);
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn scheduler_only_facts_cached_across_delivery_failure_and_restart() {
        let (path, config, target) = fixture();
        let mut state = State::default();
        let result = verify_with(
            &path,
            &config,
            &mut state,
            &target,
            true,
            false,
            1000,
            |_, _, _, _| Ok(Some(event())),
            |_| Err("offline"),
        );
        assert_eq!(result.unwrap_err(), "offline");
        let mut state = load_state(&path).unwrap();
        let result = verify_with(
            &path,
            &config,
            &mut state,
            &target,
            true,
            false,
            1001,
            |_, _, _, _| panic!("must replay durable facts without SSH"),
            |e| {
                assert_eq!(e, event());
                Ok(json!({"status":"applied"}))
            },
        )
        .unwrap();
        assert_eq!(result["status"], "applied");
        assert!(state.jobs[&state_key(&target)].pending_event.is_none());
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn missing_history_is_retryable_and_limits_survive_restart() {
        let (path, config, target) = fixture();
        let mut state = State::default();
        assert_eq!(
            verify_with(
                &path,
                &config,
                &mut state,
                &target,
                true,
                false,
                1000,
                |_, _, _, _| Err("qstat_query_failed"),
                |_| panic!("no facts on failure")
            )
            .unwrap_err(),
            "qstat_query_failed"
        );
        save_state(&path, &state).unwrap();
        let mut state = load_state(&path).unwrap();
        assert_eq!(
            verify_with(
                &path,
                &config,
                &mut state,
                &target,
                true,
                false,
                1180,
                |_, _, _, _| panic!("backoff"),
                |_| panic!()
            )
            .unwrap_err(),
            "qstat_deferred"
        );
        verify_with(
            &path,
            &config,
            &mut state,
            &target,
            true,
            false,
            1300,
            |_, _, _, _| Ok(Some(event())),
            |_| Ok(json!({"status":"applied"})),
        )
        .unwrap();
        assert_eq!(state.query_count, 2);
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn fallback_defaults_to_six_hours_mail_respects_three_minute_limit() {
        let o = Observation {
            last_attempt_at: 1000,
            last_success_at: 1000,
            ..Default::default()
        };
        assert!(!due(&o, 1179, 21600, true));
        assert!(due(&o, 1180, 21600, true));
        assert!(!due(&o, 22599, 21600, false));
        assert!(due(&o, 22600, 21600, false));
        assert!(due(&o, 4600, 3600, false));
    }
    #[test]
    fn changed_credentials_never_replay_previously_verified_facts() {
        let (path, config, target) = fixture();
        let mut state = State::default();
        verify_with(
            &path,
            &config,
            &mut state,
            &target,
            true,
            false,
            1000,
            |_, _, _, _| Ok(Some(event())),
            |_| Err("offline"),
        )
        .unwrap_err();
        configure_at(&path, &serde_json::to_vec(&config_body()).unwrap()).unwrap();
        let config = load_config(&path).unwrap().unwrap();
        assert_eq!(
            verify_with(
                &path,
                &config,
                &mut state,
                &target,
                true,
                false,
                1180,
                |_, _, _, _| Err("new_key_unavailable"),
                |_| panic!("old facts must not apply")
            )
            .unwrap_err(),
            "new_key_unavailable"
        );
        assert!(state.jobs[&state_key(&target)].pending_event.is_none());
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn mail_is_only_a_matching_hint_and_unmatched_mail_never_queries() {
        let mut hint = event();
        hint["host"] = json!("attacker.invalid");
        hint["state"] = json!("failed");
        hint["reason"] = json!("walltime_exceeded");
        let response = trigger_with(
            hint.clone(),
            |params| {
                assert_eq!(params.as_object().unwrap().len(), 3);
                assert!(params.get("state").is_none());
                Ok(json!({"status":"unmatched"}))
            },
            |_, _| panic!("unmatched must not run SSH"),
        )
        .unwrap();
        assert_eq!(response["status"], "unmatched");
        trigger_with(hint, |_|Ok(json!({"status":"matched","target":{"remoteId":"123.gadi-pbs","correlationToken":"a".repeat(32),"remoteName":null,"project":"registered-project","currentState":"running"}})), |target, end_hint| {
            assert!(end_hint, "a failed-mail hint schedules end rechecks");
            assert_eq!(target.project.as_deref(),Some("registered-project"));
            assert_eq!(target.current_state,"running");
            Ok(json!({"status":"applied"}))
        }).unwrap();
    }
    #[test]
    fn private_file_checks_reject_symlinks_and_shared_permissions() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let (path, config, _) = fixture();
        let (key, _) = credential_paths(&path, &config);
        let alias = path.join("alias");
        symlink(&key, &alias).unwrap();
        assert_eq!(
            validate_credential_file(&alias).unwrap_err(),
            "qstat_storage_permissions"
        );
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            validate_credential_file(&key).unwrap_err(),
            "qstat_storage_permissions"
        );
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn no_mail_polling_observes_terminal_and_preserves_global_admission() {
        let (path, config, target) = fixture();
        let mut state = State::default();
        let running = || {
            let mut e = event();
            e["state"] = json!("running");
            e.as_object_mut().unwrap().remove("exitCode");
            e
        };
        verify_with(
            &path,
            &config,
            &mut state,
            &target,
            false,
            false,
            1000,
            |_, _, _, _| Ok(Some(running())),
            |e| {
                assert_eq!(e["state"], "running");
                Ok(json!({"status":"applied"}))
            },
        )
        .unwrap();
        let mut other = target.clone();
        other.remote_id = "456.gadi-pbs".into();
        assert_eq!(
            verify_with(
                &path,
                &config,
                &mut state,
                &other,
                true,
                false,
                1001,
                |_, _, _, _| panic!("global throttle"),
                |_| panic!()
            )
            .unwrap_err(),
            "qstat_deferred"
        );
        assert_eq!(
            verify_with(
                &path,
                &config,
                &mut state,
                &target,
                false,
                false,
                22599,
                |_, _, _, _| panic!("six hours"),
                |_| panic!()
            )
            .unwrap_err(),
            "qstat_deferred"
        );
        verify_with(
            &path,
            &config,
            &mut state,
            &target,
            false,
            false,
            22600,
            |_, _, _, _| Ok(Some(event())),
            |e| {
                assert_eq!(e["state"], "exited");
                Ok(json!({"status":"applied"}))
            },
        )
        .unwrap();
        assert_eq!(state.query_count, 2);
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn scheduler_does_not_advance_pages_without_budget_and_prefers_untried_jobs() {
        let (path, _, target) = fixture();
        let mut state = State {
            next_query_at: now() + 180,
            cursor: Some("page1".into()),
            ..Default::default()
        };
        state.jobs.insert(
            state_key(&target),
            Observation {
                last_attempt_at: now(),
                failures: 5,
                ..Default::default()
            },
        );
        save_state(&path, &state).unwrap();
        assert_eq!(poll_once(&path).unwrap_err(), "qstat_deferred");
        assert_eq!(load_state(&path).unwrap().cursor.as_deref(), Some("page1"));
        let mut untried = target.clone();
        untried.remote_id = "456.gadi-pbs".into();
        let ordered = order_targets(
            &[
                serde_json::to_value(&target).unwrap(),
                serde_json::to_value(&untried).unwrap(),
            ],
            &state,
        )
        .unwrap();
        assert_eq!(ordered[0].remote_id, untried.remote_id);
        std::fs::remove_dir_all(path).unwrap();
    }
    fn running_event() -> Value {
        let mut e = event();
        e["state"] = json!("running");
        e.as_object_mut().unwrap().remove("exitCode");
        e
    }
    #[test]
    fn end_mail_while_still_running_schedules_short_bounded_rechecks() {
        let (path, config, target) = fixture();
        let mut state = State::default();
        let applied = |_| Ok(json!({"status":"applied"}));
        verify_with(
            &path,
            &config,
            &mut state,
            &target,
            true,
            true,
            1000,
            |_, _, _, _| Ok(Some(running_event())),
            applied,
        )
        .unwrap();
        let key = state_key(&target);
        assert_eq!(state.jobs[&key].recheck_at, 1300);
        // Not the full six-hour interval, but no earlier than the recheck.
        assert_eq!(
            verify_with(
                &path,
                &config,
                &mut state,
                &target,
                false,
                false,
                1299,
                |_, _, _, _| panic!("too early"),
                applied
            )
            .unwrap_err(),
            "qstat_deferred"
        );
        verify_with(
            &path,
            &config,
            &mut state,
            &target,
            false,
            false,
            1300,
            |_, _, _, _| Ok(Some(running_event())),
            applied,
        )
        .unwrap();
        assert_eq!(state.jobs[&key].recheck_at, 1900, "then the normal backoff");
        // The global 180 s admission still applies to every recheck.
        state.next_query_at = 2000;
        assert_eq!(
            verify_with(
                &path,
                &config,
                &mut state,
                &target,
                false,
                false,
                1900,
                |_, _, _, _| panic!("global limit"),
                applied
            )
            .unwrap_err(),
            "qstat_deferred"
        );
        verify_with(
            &path,
            &config,
            &mut state,
            &target,
            false,
            false,
            2000,
            |_, _, _, _| Ok(Some(event())),
            applied,
        )
        .unwrap();
        assert_eq!(
            (state.jobs[&key].recheck_at, state.jobs[&key].rechecks),
            (0, 0)
        );
        // Bounded: after MAX_RECHECKS the ordinary poll interval resumes.
        let mut time = 10_000;
        verify_with(
            &path,
            &config,
            &mut state,
            &target,
            true,
            true,
            time,
            |_, _, _, _| Ok(Some(running_event())),
            applied,
        )
        .unwrap();
        for _ in 0..MAX_RECHECKS {
            time = state.jobs[&key].recheck_at;
            state.next_query_at = 0;
            verify_with(
                &path,
                &config,
                &mut state,
                &target,
                false,
                false,
                time,
                |_, _, _, _| Ok(Some(running_event())),
                applied,
            )
            .unwrap();
        }
        assert_eq!(state.jobs[&key].recheck_at, 0);
        assert!(!due(&state.jobs[&key], time + 3600, 21600, false));
        // A trigger without an end hint (e.g. "Begun execution") schedules nothing.
        let mut other = target.clone();
        other.remote_id = "456.gadi-pbs".into();
        state.next_query_at = 0;
        verify_with(
            &path,
            &config,
            &mut state,
            &other,
            true,
            false,
            time,
            |_, _, _, _| Ok(Some(running_event())),
            applied,
        )
        .unwrap();
        assert_eq!(state.jobs[&state_key(&other)].recheck_at, 0);
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn nonfinal_observation_without_event_is_success_not_error() {
        let (path, config, target) = fixture();
        let mut state = State::default();
        let reply = verify_with(
            &path,
            &config,
            &mut state,
            &target,
            true,
            false,
            1000,
            |_, _, _, _| Ok(None),
            |_| panic!("nothing to deliver"),
        )
        .unwrap();
        assert_eq!(reply["status"], "observed");
        let o = &state.jobs[&state_key(&target)];
        assert_eq!((o.failures, o.last_success_at), (0, 1000));
        assert!(o.pending_event.is_none());
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn update_without_enabled_keeps_value_and_multiple_host_keys_are_pinned() {
        let (path, config, _) = fixture();
        assert!(config.enabled);
        let mut body = config_body();
        body.as_object_mut().unwrap().remove("enabled");
        body.as_object_mut().unwrap().remove("privateKey");
        body["pollIntervalSeconds"] = json!(7200);
        configure_at(&path, &serde_json::to_vec(&body).unwrap()).unwrap();
        assert!(
            load_config(&path).unwrap().unwrap().enabled,
            "omitted enabled kept"
        );
        body["enabled"] = json!(false);
        configure_at(&path, &serde_json::to_vec(&body).unwrap()).unwrap();
        assert!(!load_config(&path).unwrap().unwrap().enabled);
        body.as_object_mut().unwrap().remove("enabled");
        configure_at(&path, &serde_json::to_vec(&body).unwrap()).unwrap();
        assert!(!load_config(&path).unwrap().unwrap().enabled);

        let first = body["hostKey"].clone();
        let mut second = (11u32).to_be_bytes().to_vec();
        second.extend_from_slice(b"ssh-ed25519");
        second.extend_from_slice(&(32u32).to_be_bytes());
        second.extend_from_slice(&[2; 32]);
        let second = format!(
            "ssh-ed25519 {}",
            base64::engine::general_purpose::STANDARD.encode(second)
        );
        let mut both = config_body();
        both["hostKeys"] = json!([first, second]);
        assert_eq!(
            configure_at(&path, &serde_json::to_vec(&both).unwrap()).unwrap_err(),
            "qstat_invalid_host_key"
        );
        both.as_object_mut().unwrap().remove("hostKey");
        configure_at(&path, &serde_json::to_vec(&both).unwrap()).unwrap();
        let config = load_config(&path).unwrap().unwrap();
        assert_eq!(config.ssh.host_keys.len(), 2);
        let (_, hosts) = credential_paths(&path, &config);
        assert_eq!(std::fs::read_to_string(hosts).unwrap().lines().count(), 2);
        // Changing the pinned set requires the key again.
        let mut changed = both.clone();
        changed.as_object_mut().unwrap().remove("privateKey");
        changed["hostKeys"] = json!([first]);
        assert_eq!(
            configure_at(&path, &serde_json::to_vec(&changed).unwrap()).unwrap_err(),
            "qstat_private_key_required"
        );
        std::fs::remove_dir_all(path).unwrap();
    }
}
