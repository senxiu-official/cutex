//! Optional owner-configured mailbox. Verified mail only triggers source verification.
pub mod imap;
pub mod nci;

use crate::management::v2::human_jobs::ExternalIngestError;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const MAX_PENDING: usize = 1024;
const MAX_PENDING_PER_CLUSTER: usize = 256;
const MAX_SEEN: usize = 8192;
static CONFIG_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Config {
    enabled: bool,
    server: String,
    port: u16,
    username: String,
    password: String,
    mailbox: String,
    sources: Vec<String>,
    /// Address NCI should mail (`qsub -M`); omitted = `username` when that is an address.
    /// Written only when set, so a config without it stays readable by older builds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    address: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConfigInput {
    enabled: bool,
    server: String,
    port: u16,
    username: String,
    password: Option<String>,
    #[serde(default = "inbox")]
    mailbox: String,
    #[serde(default)]
    sources: Vec<String>,
    #[serde(default)]
    address: Option<String>,
}
fn inbox() -> String {
    "INBOX".into()
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct State {
    last_connected_at: Option<u64>,
    last_error: Option<String>,
    processed: u64,
    ignored: u64,
    rejected: u64,
    duplicates: u64,
    applied: u64,
    #[serde(default)]
    discarded: u64,
    #[serde(default)]
    expired: u64,
    /// Why pending events are not being delivered (cleared on progress).
    #[serde(default)]
    delivery_error: Option<String>,
    seen: Vec<String>,
    pending: BTreeMap<String, Pending>,
    #[serde(default)]
    delivery_cursor: Option<String>,
    #[serde(default)]
    mail_cursor: Option<MailCursor>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MailCursor {
    account_key: String,
    uid_validity: u32,
    uid: u32,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pending {
    received_at: u64,
    event: Value,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn root() -> Result<PathBuf, &'static str> {
    crate::config::paths::runtime_dir()
        .map(|p| p.join("inbound-mail"))
        .map_err(|_| "private_storage_unavailable")
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
        Err(_) => return Err("private_storage_unavailable"),
    };
    let meta = file.metadata().map_err(|_| "private_storage_unavailable")?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
        || meta.nlink() != 1
        || meta.len() > limit
    {
        return Err("private_storage_permissions");
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "private_storage_unavailable")?;
    if bytes.len() as u64 > limit {
        return Err("private_storage_limit");
    }
    Ok(Some(bytes))
}
fn load_config() -> Result<Option<Config>, &'static str> {
    load_config_at(&root()?)
}
fn load_config_at(path: &Path) -> Result<Option<Config>, &'static str> {
    read_private(&path.join("config.json"), 16_384)?
        .map(|bytes| serde_json::from_slice(&bytes).map_err(|_| "invalid_mail_config"))
        .transpose()
}
fn load_state(path: &Path) -> Result<State, &'static str> {
    match read_private(&path.join("state-v1.json"), 4 * 1024 * 1024)? {
        Some(bytes) => serde_json::from_slice(&bytes).map_err(|_| "invalid_mail_state"),
        None => Ok(State::default()),
    }
}
fn save_state(path: &Path, state: &State) -> Result<(), &'static str> {
    crate::config::atomic::write_private_pretty_json_atomic(
        &path.join("state-v1.json"),
        state,
        "mail state",
    )
    .map_err(|_| "private_storage_unavailable")
}
fn valid_text(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control)
}
fn make_config(input: ConfigInput, old: Option<&Config>) -> Result<Config, &'static str> {
    if !valid_text(&input.server, 253)
        || !input
            .server
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
        || input.port == 0
        || !valid_text(&input.username, 320)
        || !valid_text(&input.mailbox, 128)
        || input.sources.iter().any(|s| s != "nci")
        || input.sources.len() > 1
        || input
            .address
            .as_deref()
            .is_some_and(|a| !valid_mail_address(a))
    {
        return Err("invalid_mail_config");
    }
    let password = input
        .password
        .or_else(|| {
            old.filter(|c| {
                c.server == input.server && c.port == input.port && c.username == input.username
            })
            .map(|c| c.password.clone())
        })
        .ok_or("mail_password_required")?;
    if !valid_text(&password, 4096) {
        return Err("invalid_mail_config");
    }
    Ok(Config {
        enabled: input.enabled,
        server: input.server,
        port: input.port,
        username: input.username,
        password,
        mailbox: input.mailbox,
        sources: input.sources,
        address: input.address,
    })
}

/// Same conservative syntax the Job Service accepts for `qsub -M` (one argument,
/// no whitespace/quotes/commas/shell metacharacters, no leading `-`).
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
                && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// File the Job Service reads (`serve --external-notify-file PATH`) on every
/// `prepare_remote`; it holds only the address, never a credential.
pub const NOTIFY_FILE_NAME: &str = "notify-address-v1.json";
const NOTIFY_SCHEMA: &str = "cutex/external-notify/v1";

/// The address agents get for NCI, why (not), and where it came from.
fn nci_mail_to(config: Option<&Config>) -> (Option<String>, &'static str, Option<&'static str>) {
    let Some(config) = config else {
        return (None, "mailbox_not_configured", None);
    };
    if !config.sources.iter().any(|s| s == "nci") {
        return (None, "nci_source_disabled", None);
    }
    if !config.enabled {
        return (None, "receiver_disabled", None);
    }
    if let Some(address) = &config.address {
        return (Some(address.clone()), "configured", Some("address"));
    }
    if valid_mail_address(&config.username) {
        return (Some(config.username.clone()), "configured", Some("username"));
    }
    (None, "address_required", None)
}

/// Keep the notify file equal to the effective address (removed when there is none).
fn sync_notify_file_at(path: &Path, config: Option<&Config>) -> Result<(), &'static str> {
    let file = path.join(NOTIFY_FILE_NAME);
    match nci_mail_to(config).0 {
        Some(address) => crate::config::atomic::write_private_pretty_json_atomic(
            &file,
            &json!({"schema": NOTIFY_SCHEMA, "sources": {"nci": {"mailTo": address}}}),
            "notification address",
        )
        .map_err(|_| "private_storage_unavailable"),
        None => match std::fs::remove_file(&file) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err("private_storage_unavailable"),
        },
    }
}

/// Startup reconciliation (e.g. after an upgrade); never creates the directory.
pub fn sync_notify_file() -> Result<(), &'static str> {
    let path = root()?;
    if !path.exists() {
        return Ok(());
    }
    let _guard = CONFIG_LOCK
        .lock()
        .map_err(|_| "private_storage_unavailable")?;
    sync_notify_file_at(&path, load_config_at(&path)?.as_ref())
}

pub fn configure(body: &[u8]) -> Result<Value, &'static str> {
    configure_at(&root()?, body)
}
fn configure_at(path: &Path, body: &[u8]) -> Result<Value, &'static str> {
    if body.len() > 16_384 {
        return Err("invalid_mail_config");
    }
    let input = serde_json::from_slice(body).map_err(|_| "invalid_mail_config")?;
    let _guard = CONFIG_LOCK
        .lock()
        .map_err(|_| "private_storage_unavailable")?;
    let config = make_config(input, load_config_at(path)?.as_ref())?;
    crate::management::client_tokens::ensure_private_dir(&path)
        .map_err(|_| "private_storage_permissions")?;
    crate::config::atomic::write_private_pretty_json_atomic(
        &path.join("config.json"),
        &config,
        "mail config",
    )
    .map_err(|_| "private_storage_unavailable")?;
    sync_notify_file_at(path, Some(&config))?;
    status_at(path)
}

pub fn clear_config() -> Result<Value, &'static str> {
    clear_config_at(&root()?)
}
fn clear_config_at(path: &Path) -> Result<Value, &'static str> {
    let _guard = CONFIG_LOCK
        .lock()
        .map_err(|_| "private_storage_unavailable")?;
    match std::fs::remove_file(path.join("config.json")) {
        Ok(()) => {
            std::fs::File::open(path)
                .and_then(|dir| dir.sync_all())
                .map_err(|_| "private_storage_unavailable")?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("private_storage_unavailable"),
    }
    if path.exists() {
        sync_notify_file_at(path, None)?;
    }
    status_at(path)
}

pub fn status() -> Result<Value, &'static str> {
    status_at(&root()?)
}
fn status_at(path: &Path) -> Result<Value, &'static str> {
    let config = load_config_at(path)?;
    let state = load_state(path)?;
    let (mail_to, mail_to_status, mail_to_from) = nci_mail_to(config.as_ref());
    Ok(json!({
        "address": config.as_ref().and_then(|c| c.address.as_ref()),
        // What `prepare_remote` hands agents as `qsub -M` (null = none, see status).
        "nciMailTo": mail_to,
        "nciMailToStatus": mail_to_status,
        "nciMailToFrom": mail_to_from,
        "notifyAddressFile": path.join(NOTIFY_FILE_NAME),
        "enabled": config.as_ref().is_some_and(|c| c.enabled),
        "configured": config.is_some(),
        "server": config.as_ref().map(|c| &c.server),
        "port": config.as_ref().map(|c| c.port),
        "username": config.as_ref().map(|c| &c.username),
        "mailbox": config.as_ref().map(|c| &c.mailbox),
        "sources": config.as_ref().map(|c| c.sources.clone()).unwrap_or_default(),
        "lastConnectedAt": state.last_connected_at,
        "errorCode": state.last_error.as_ref().or(state.delivery_error.as_ref()),
        "deliveryErrorCode": state.delivery_error,
        // Mail only triggers scheduler verification; without the optional
        // `nci-qstat` build feature verified mail cannot complete any job.
        "nciVerificationAvailable": NCI_VERIFICATION_BUILT,
        "statusCode": pending_status_code(&state, NCI_VERIFICATION_BUILT),
        "counts": {"processed":state.processed,"ignored":state.ignored,"rejected":state.rejected,"duplicates":state.duplicates,"applied":state.applied,"pending":state.pending.len(),"discarded":state.discarded,"expired":state.expired}
    }))
}
const NCI_VERIFICATION_BUILT: bool = cfg!(feature = "nci-qstat");
/// Why events are not progressing, for the owner/desktop; `None` when nothing is held up.
fn pending_status_code(state: &State, verification_built: bool) -> Option<String> {
    if !verification_built
        && state
            .pending
            .values()
            .any(|pending| pending.event["source"] == "nci")
    {
        return Some(NCI_VERIFICATION_UNAVAILABLE.into());
    }
    state.delivery_error.clone()
}
const NCI_VERIFICATION_UNAVAILABLE: &str = "nci_verification_unavailable";
fn transport_error(error: imap::MailError) -> &'static str {
    match error.code() {
        "authentication" => "mail_authentication_failed",
        "protocol" => "mail_protocol_failed",
        _ => "mail_connection_failed",
    }
}
fn transport_config(config: &Config) -> imap::TransportConfig {
    imap::TransportConfig {
        server: config.server.clone(),
        port: config.port,
        username: config.username.clone(),
        password: config.password.clone(),
        mailbox: config.mailbox.clone(),
    }
}
pub fn test_connection(body: &[u8]) -> Result<Value, &'static str> {
    test_connection_at(&root()?, body, |config| {
        imap::Mailbox::connect(&transport_config(config))
            .map(|_| ())
            .map_err(transport_error)
    })
}
fn test_connection_at(
    path: &Path,
    body: &[u8],
    connect: impl FnOnce(&Config) -> Result<(), &'static str>,
) -> Result<Value, &'static str> {
    if body.len() > 16_384 {
        return Err("invalid_mail_config");
    }
    let saved = load_config_at(path)?;
    let config = if body.is_empty() || serde_json::from_slice::<Value>(body).ok() == Some(json!({}))
    {
        saved.ok_or("mail_not_configured")?
    } else {
        make_config(
            serde_json::from_slice(body).map_err(|_| "invalid_mail_config")?,
            saved.as_ref(),
        )?
    };
    connect(&config)?;
    Ok(json!({"ok":true}))
}

/// The management process owns this thread; no service or files exist by default.
pub fn start_worker() {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    // Rewrite the Job Service's notification-address file from the saved config.
    let _ = sync_notify_file();
    let _ = std::thread::Builder::new()
        .name("cutex-inbound-mail".into())
        .spawn(|| {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            loop {
                let result = worker_cycle(&runtime);
                if let Err(code) = result {
                    if let Ok(path) = root() {
                        if path.exists() {
                            use fs2::FileExt;
                            if let Ok(lock) = std::fs::OpenOptions::new()
                                .write(true)
                                .open(path.join("worker.lock"))
                            {
                                if lock.try_lock_exclusive().is_ok() {
                                    if let Ok(mut state) = load_state(&path) {
                                        state.last_error = Some(code.into());
                                        let _ = save_state(&path, &state);
                                    }
                                }
                            }
                        }
                    }
                }
                std::thread::sleep(Duration::from_secs(if result.is_err() { 10 } else { 2 }));
            }
        });
}

fn worker_cycle(runtime: &tokio::runtime::Runtime) -> Result<(), &'static str> {
    use fs2::FileExt;
    use std::os::unix::fs::OpenOptionsExt;
    let Some(config) = load_config()?.filter(|c| c.enabled) else {
        return Ok(());
    };
    if config.sources.is_empty() {
        return Ok(());
    }
    let path = root()?;
    // One owner of mailbox processing even if two management processes start.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path.join("worker.lock"))
        .map_err(|_| "private_storage_unavailable")?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(());
    }
    let mut state = load_state(&path)?;
    if config.sources.iter().any(|s| s == "nci") {
        drain_pending(&mut state, |event| {
            deliver_if_current(&config, event.clone())
        })?;
    }
    save_state(&path, &state)?;
    let mut mailbox =
        imap::Mailbox::connect(&transport_config(&config)).map_err(transport_error)?;
    state.last_connected_at = Some(now());
    state.last_error = None;
    save_state(&path, &state)?;
    use sha2::Digest;
    let account_key = format!(
        "{:x}",
        sha2::Sha256::digest(
            serde_json::to_vec(&(
                &config.server,
                config.port,
                &config.username,
                &config.mailbox
            ))
            .map_err(|_| "invalid_mail_config")?
        )
    );
    loop {
        if load_config()?.as_ref() != Some(&config) {
            return Ok(());
        }
        let after = state
            .mail_cursor
            .as_ref()
            .filter(|cursor| cursor.account_key == account_key)
            .map(|cursor| (cursor.uid_validity, cursor.uid));
        let batch = mailbox
            .fetch_unread(after)
            .map_err(|_| "mail_fetch_failed")?;
        for message in batch.messages {
            if !load_config()?.is_some_and(|current| current == config) {
                return Ok(());
            }
            if message.uid_validity == 0 {
                return Err("mail_protocol_failed");
            }
            let parsed = if message.oversized || !config.sources.iter().any(|s| s == "nci") {
                Err(nci::MailError::Ignore)
            } else {
                runtime.block_on(async {
                    tokio::time::timeout(
                        Duration::from_secs(30),
                        nci::verify_and_parse(&message.raw),
                    )
                    .await
                    .unwrap_or(Err(nci::MailError::Retry))
                })
            };
            if load_config()?.as_ref() != Some(&config) {
                return Ok(());
            }
            let acknowledge = record_mail(&mut state, parsed)?;
            state.mail_cursor = Some(MailCursor {
                account_key: account_key.clone(),
                uid_validity: message.uid_validity,
                uid: message.uid,
            });
            save_state(&path, &state)?;
            if acknowledge {
                mailbox.ack(message.uid).map_err(|_| "mail_ack_failed")?;
            }
        }
        state.mail_cursor = Some(MailCursor {
            account_key: account_key.clone(),
            uid_validity: batch.scan_cursor.0,
            uid: batch.scan_cursor.1,
        });
        if config.sources.iter().any(|s| s == "nci") {
            drain_pending(&mut state, |event| {
                deliver_if_current(&config, event.clone())
            })?;
        }
        save_state(&path, &state)?;
        if batch.caught_up {
            mailbox.wait_for_change().map_err(|_| "mail_idle_failed")?;
        }
    }
}

fn deliver_if_current(config: &Config, event: Value) -> Result<Value, ExternalIngestError> {
    deliver_if_current_with(config, load_config, || {
        #[cfg(feature = "nci-qstat")]
        { crate::nci_verification::verify_trigger(event) }
        #[cfg(not(feature = "nci-qstat"))]
        { let _ = event; Err(ExternalIngestError::Retry(NCI_VERIFICATION_UNAVAILABLE)) }
    })
}
fn deliver_if_current_with(
    config: &Config,
    current: impl FnOnce() -> Result<Option<Config>, &'static str>,
    deliver: impl FnOnce() -> Result<Value, ExternalIngestError>,
) -> Result<Value, ExternalIngestError> {
    {
        let _guard = CONFIG_LOCK
            .lock()
            .map_err(|_| ExternalIngestError::Retry("job_service_unavailable"))?;
        if current()
            .map_err(|_| ExternalIngestError::Retry("job_service_unavailable"))?
            .as_ref()
            != Some(config)
        {
            return Ok(json!({"status":"paused"}));
        }
    }
    // The admitted delivery may finish after disable/delete; configuration never waits on IO.
    deliver()
}

fn cluster_key(event: &Value) -> String {
    let source = event["source"].as_str().unwrap_or("unknown");
    let cluster = if source == "nci" { "gadi" } else { "unknown" };
    format!("{source}:{cluster}")
}
fn discard_oldest(state: &mut State, cluster: Option<&str>) {
    let oldest = state
        .pending
        .iter()
        .filter(|(_, pending)| cluster.is_none_or(|key| cluster_key(&pending.event) == key))
        .min_by_key(|(id, pending)| (pending.received_at, *id))
        .map(|(id, _)| id.clone());
    if let Some(id) = oldest {
        state.pending.remove(&id);
        state.discarded = state.discarded.saturating_add(1);
    }
}

fn record_mail(
    state: &mut State,
    parsed: Result<nci::ParsedMail, nci::MailError>,
) -> Result<bool, &'static str> {
    match parsed {
        Err(nci::MailError::Retry) => {
            state.last_error = Some("mail_verification_retry".into());
            return Ok(false);
        }
        Err(nci::MailError::Ignore) => state.ignored = state.ignored.saturating_add(1),
        Err(nci::MailError::Rejected) => state.rejected = state.rejected.saturating_add(1),
        Ok(mail) => {
            if state.seen.contains(&mail.event_id) {
                state.duplicates = state.duplicates.saturating_add(1);
            } else {
                let mut event = serde_json::to_value(&mail).map_err(|_| "invalid_mail_event")?;
                event
                    .as_object_mut()
                    .ok_or("invalid_mail_event")?
                    .remove("messageId");
                let cluster = cluster_key(&event);
                while state
                    .pending
                    .values()
                    .filter(|pending| cluster_key(&pending.event) == cluster)
                    .count()
                    >= MAX_PENDING_PER_CLUSTER
                {
                    discard_oldest(state, Some(&cluster));
                }
                while state.pending.len() >= MAX_PENDING {
                    discard_oldest(state, None);
                }
                state.pending.insert(
                    mail.event_id.clone(),
                    Pending {
                        received_at: now(),
                        event,
                    },
                );
                state.seen.push(mail.event_id);
                if state.seen.len() > MAX_SEEN {
                    state.seen.remove(0);
                }
            }
        }
    }
    state.processed = state.processed.saturating_add(1);
    Ok(true)
}
const PENDING_EXPIRY_SECS: u64 = 24 * 3600;

fn drain_pending(
    state: &mut State,
    mut deliver: impl FnMut(&Value) -> Result<Value, ExternalIngestError>,
) -> Result<(), &'static str> {
    // Events kept pending by a retryable delivery error still expire after 24 h.
    let cutoff = now().saturating_sub(PENDING_EXPIRY_SECS);
    let before = state.pending.len();
    state.pending.retain(|_, pending| pending.received_at >= cutoff);
    state.expired = state
        .expired
        .saturating_add((before - state.pending.len()) as u64);
    if state.pending.is_empty() {
        state.delivery_error = None;
    }
    let mut ids: Vec<_> = state.pending.keys().cloned().collect();
    if let Some(cursor) = &state.delivery_cursor {
        let offset = ids.partition_point(|id| id <= cursor);
        ids.rotate_left(offset);
    }
    ids.truncate(64);
    for id in ids {
        state.delivery_cursor = Some(id.clone());
        let pending = &state.pending[&id];
        match deliver(&pending.event) {
            Ok(reply)
                if matches!(
                    reply["status"].as_str(),
                    Some("applied" | "duplicate" | "superseded" | "terminal_ignored" | "observed")
                ) =>
            {
                state.pending.remove(&id);
                state.applied = state.applied.saturating_add(1);
                state.delivery_error = None;
            }
            Ok(reply) if reply["status"] == "paused" => break,
            Ok(reply) if reply["status"] == "unmatched" => {
                // A fast scheduler can send mail before the model registers;
                // the 24 h expiry above bounds the wait.
                state.delivery_error = None;
            }
            // The event conflicts with recorded facts (eventId reuse with other
            // bytes, ambiguous match, 256-event cap): retrying cannot help.
            Ok(reply) if reply["status"] == "permanent_conflict" => {
                state.pending.remove(&id);
                state.discarded = state.discarded.saturating_add(1);
                state.delivery_error = None;
            }
            #[cfg(any(test, feature = "nci-qstat"))]
            Err(ExternalIngestError::Permanent) => {
                state.pending.remove(&id);
                state.discarded = state.discarded.saturating_add(1);
            }
            // A Job Service started without `--external-source nci` is an owner
            // misconfiguration; keep verified events until it is fixed.
            Ok(reply) if reply["status"] == "disabled_source" => {
                state.delivery_error = Some("job_service_source_disabled".into());
                break;
            }
            Err(ExternalIngestError::Retry(code)) => {
                state.delivery_error = Some(code.into());
                break;
            }
            Ok(_) => {
                state.delivery_error = Some("job_service_rejected".into());
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn input() -> ConfigInput {
        serde_json::from_value(json!({"enabled":true,"server":"imap.example.org","port":993,"username":"mail@example.org","password":"private-app-password","sources":["nci"]})).unwrap()
    }
    #[test]
    fn credentials_are_retained_only_for_same_account() {
        let old = make_config(input(), None).unwrap();
        let mut next = input();
        next.password = None;
        assert!(make_config(next, Some(&old)).is_ok());
        let mut next = input();
        next.password = None;
        next.server = "other.example.org".into();
        assert_eq!(
            make_config(next, Some(&old)).err(),
            Some("mail_password_required")
        );
        let mut next = input();
        next.server = "host\r\nLOGOUT".into();
        assert!(make_config(next, None).is_err());
    }
    #[test]
    fn retry_does_not_ack_or_increment_processed() {
        let mut state = State::default();
        assert!(!record_mail(&mut state, Err(nci::MailError::Retry)).unwrap());
        assert_eq!(state.processed, 0);
        assert!(record_mail(&mut state, Err(nci::MailError::Rejected)).unwrap());
        assert_eq!(state.rejected, 1);
    }
    #[test]
    fn pending_survives_registration_race_and_restart() {
        let mut state = State::default();
        state.pending.insert(
            "event".into(),
            Pending {
                received_at: now(),
                event: json!({"eventId":"event"}),
            },
        );
        drain_pending(&mut state, |_| Ok(json!({"status":"unmatched"}))).unwrap();
        let bytes = serde_json::to_vec(&state).unwrap();
        let mut restored: State = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(restored.pending.len(), 1);
        drain_pending(&mut restored, |_| Ok(json!({"status":"applied"}))).unwrap();
        assert!(restored.pending.is_empty());
        assert_eq!(restored.applied, 1);
    }
    fn mail() -> nci::ParsedMail {
        nci::ParsedMail {
            source: "nci",
            remote_id: "123.gadi-pbs".into(),
            correlation_token: "a".repeat(32),
            state: "failed",
            exit_code: None,
            reason: Some("walltime_exceeded"),
            resources: BTreeMap::new(),
            event_id: "nci-test-event".into(),
            message_id: Some("unsigned-private-message-id".into()),
        }
    }
    #[test]
    fn verified_event_is_durable_before_ack_and_replay_is_idempotent() {
        let mut state = State::default();
        assert!(record_mail(&mut state, Ok(mail())).unwrap());
        let bytes = serde_json::to_vec(&state).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("unsigned-private-message-id"));
        let mut restored: State = serde_json::from_slice(&bytes).unwrap();
        assert!(record_mail(&mut restored, Ok(mail())).unwrap());
        assert_eq!(restored.pending.len(), 1);
        assert_eq!(restored.duplicates, 1);
        drain_pending(&mut restored, |_| Err(ExternalIngestError::Retry("job_service_unavailable"))).unwrap();
        assert_eq!(restored.pending.len(), 1);
        drain_pending(&mut restored, |event| {
            assert_eq!(event["reason"], "walltime_exceeded");
            assert!(event.get("exitCode").is_none());
            Ok(json!({"status":"applied"}))
        })
        .unwrap();
        assert!(restored.pending.is_empty());
    }
    #[test]
    fn pending_limit_evicts_oldest_and_keeps_processing() {
        let mut state = State::default();
        for index in 0..MAX_PENDING {
            state.pending.insert(
                format!("event-{index:04}"),
                Pending {
                    received_at: now() - MAX_PENDING as u64 + index as u64,
                    event: json!({"source":format!("other-{index}")}),
                },
            );
        }
        assert!(record_mail(&mut state, Ok(mail())).unwrap());
        assert_eq!(state.pending.len(), MAX_PENDING);
        assert!(!state.pending.contains_key("event-0000"));
        for index in 0..MAX_PENDING_PER_CLUSTER + 4 {
            let mut event = mail();
            event.event_id = format!("nci-{index}");
            assert!(record_mail(&mut state, Ok(event)).unwrap());
        }
        assert_eq!(
            state
                .pending
                .values()
                .filter(|p| cluster_key(&p.event) == "nci:gadi")
                .count(),
            MAX_PENDING_PER_CLUSTER
        );
        assert!(state.discarded >= 5);
        drain_pending(&mut state, |_| Ok(json!({"status":"unmatched"}))).unwrap();
        let first = state.delivery_cursor.clone();
        drain_pending(&mut state, |_| Ok(json!({"status":"unmatched"}))).unwrap();
        assert_ne!(state.delivery_cursor, first);
    }
    #[test]
    fn permanent_rejection_is_discarded_and_later_events_deliver() {
        let mut state = State::default();
        for id in ["a", "b"] {
            let mut event = mail();
            event.event_id = id.into();
            record_mail(&mut state, Ok(event)).unwrap();
        }
        drain_pending(&mut state, |event| {
            if event["eventId"] == "a" {
                Err(ExternalIngestError::Permanent)
            } else {
                Ok(json!({"status":"applied"}))
            }
        })
        .unwrap();
        assert!(state.pending.is_empty());
        assert_eq!(state.discarded, 1);
        assert_eq!(state.applied, 1);
    }
    #[test]
    fn only_permanent_conflict_is_discarded_by_status() {
        let mut state = State::default();
        record_mail(&mut state, Ok(mail())).unwrap();
        drain_pending(&mut state, |_| {
            Ok(json!({"status":"permanent_conflict","retryable":false}))
        })
        .unwrap();
        assert!(state.pending.is_empty());
        assert_eq!(state.discarded, 1);
    }
    #[test]
    fn misconfiguration_keeps_events_pending_and_surfaces_code() {
        let path = std::env::temp_dir().join(format!("cutex-mail-retry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        let cases: [(&str, Result<Value, ExternalIngestError>); 5] = [
            ("job_service_source_disabled", Ok(json!({"status":"disabled_source","retryable":false}))),
            ("job_service_unauthorized", Err(ExternalIngestError::Retry("job_service_unauthorized"))),
            ("job_service_operation_unsupported", Err(ExternalIngestError::Retry("job_service_operation_unsupported"))),
            ("job_service_rejected", Err(ExternalIngestError::Retry("job_service_rejected"))),
            ("job_service_unavailable", Err(ExternalIngestError::Retry("job_service_unavailable"))),
        ];
        for (code, reply) in cases {
            let mut state = State::default();
            record_mail(&mut state, Ok(mail())).unwrap();
            drain_pending(&mut state, |_| reply.clone()).unwrap();
            assert_eq!(state.pending.len(), 1, "{code}");
            assert_eq!(state.discarded, 0, "{code}");
            assert_eq!(state.delivery_error.as_deref(), Some(code));
            save_state(&path, &state).unwrap();
            let status = status_at(&path).unwrap();
            assert_eq!(status["deliveryErrorCode"], code);
            assert_eq!(status["errorCode"], code);
            assert_eq!(status["counts"]["pending"], 1);
            // Once the owner fixes the Job Service the same event delivers.
            drain_pending(&mut state, |_| Ok(json!({"status":"applied"}))).unwrap();
            assert!(state.pending.is_empty());
            assert_eq!(state.applied, 1);
            assert_eq!(state.delivery_error, None);
        }
        std::fs::remove_dir_all(&path).unwrap();
    }
    #[test]
    fn retryable_events_still_expire_after_24_hours() {
        let mut state = State::default();
        record_mail(&mut state, Ok(mail())).unwrap();
        state.pending.values_mut().next().unwrap().received_at = now() - PENDING_EXPIRY_SECS - 1;
        let mut fresh = mail();
        fresh.event_id = "fresh".into();
        record_mail(&mut state, Ok(fresh)).unwrap();
        drain_pending(&mut state, |_| Err(ExternalIngestError::Retry("job_service_unauthorized"))).unwrap();
        assert_eq!(state.pending.keys().collect::<Vec<_>>(), vec!["fresh"]);
        assert_eq!(state.expired, 1);
        assert_eq!(state.discarded, 0);
    }
    #[test]
    fn draft_connection_does_not_persist_and_delete_removes_credentials() {
        let path = std::env::temp_dir().join(format!("cutex-mail-draft-{}", uuid::Uuid::new_v4()));
        let draft = br#"{"enabled":true,"server":"imap.example.org","port":993,"username":"test@example.org","password":"fixture-password","sources":["nci"]}"#;
        test_connection_at(&path, draft, |config| {
            assert_eq!(config.server, "imap.example.org");
            assert_eq!(config.password, "fixture-password");
            Ok(())
        })
        .unwrap();
        assert!(!path.exists());
        let reply = configure_at(&path, draft).unwrap();
        assert!(reply.get("password").is_none());
        let before = std::fs::read(path.join("config.json")).unwrap();
        let other = br#"{"enabled":false,"server":"imap.other.test","port":993,"username":"other","password":"other-fixture"}"#;
        test_connection_at(&path, other, |_| Err("mail_connection_failed")).unwrap_err();
        assert_eq!(before, std::fs::read(path.join("config.json")).unwrap());
        test_connection_at(&path, b"{}", |config| {
            assert_eq!(config.password, "fixture-password");
            Ok(())
        })
        .unwrap();
        let status = clear_config_at(&path).unwrap();
        assert_eq!(status["configured"], false);
        assert_eq!(status["enabled"], false);
        assert!(!path.join("config.json").exists());
        clear_config_at(&path).unwrap();
        assert_eq!(
            test_connection_at(&path, b"{}", |_| Ok(())).unwrap_err(),
            "mail_not_configured"
        );
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn notification_address_file_follows_owner_configuration() {
        let path = std::env::temp_dir().join(format!("cutex-mail-notify-{}", uuid::Uuid::new_v4()));
        let notify = path.join(NOTIFY_FILE_NAME);
        let read = || -> Value { serde_json::from_slice(&std::fs::read(&notify).unwrap()).unwrap() };
        // Username is an address: it is what agents get.
        let status = configure_at(&path, br#"{"enabled":true,"server":"imap.example.org","port":993,"username":"nci.owner@example.org","password":"fixture-password","sources":["nci"]}"#).unwrap();
        assert_eq!(status["nciMailTo"], "nci.owner@example.org");
        assert_eq!(status["nciMailToFrom"], "username");
        assert_eq!(status["nciMailToStatus"], "configured");
        assert_eq!(status["address"], Value::Null);
        assert_eq!(read(), json!({"schema":"cutex/external-notify/v1","sources":{"nci":{"mailTo":"nci.owner@example.org"}}}));
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(std::fs::metadata(&notify).unwrap().mode() & 0o077, 0);
        }
        // A config without `address` is written exactly as before (rollback-safe).
        assert!(!String::from_utf8(std::fs::read(path.join("config.json")).unwrap()).unwrap().contains("address"));
        // An explicit address wins (password retained for the same account).
        let status = configure_at(&path, br#"{"enabled":true,"server":"imap.example.org","port":993,"username":"nci.owner@example.org","address":"alias+nci@example.org","sources":["nci"]}"#).unwrap();
        assert_eq!(status["nciMailTo"], "alias+nci@example.org");
        assert_eq!(status["nciMailToFrom"], "address");
        assert_eq!(read()["sources"]["nci"]["mailTo"], "alias+nci@example.org");
        // Unsafe addresses are refused before anything is written.
        for bad in ["-x@example.org", "a b@example.org", "a@example.org,b@example.org", "nope"] {
            let body = json!({"enabled":true,"server":"imap.example.org","port":993,"username":"nci.owner@example.org","address":bad,"sources":["nci"]});
            assert_eq!(configure_at(&path, &serde_json::to_vec(&body).unwrap()).unwrap_err(), "invalid_mail_config");
        }
        assert_eq!(read()["sources"]["nci"]["mailTo"], "alias+nci@example.org");
        // Disabled receiver, missing NCI source or a non-address login: no address handed out.
        for (body, code) in [
            (r#"{"enabled":false,"server":"imap.example.org","port":993,"username":"nci.owner@example.org","sources":["nci"]}"#, "receiver_disabled"),
            (r#"{"enabled":true,"server":"imap.example.org","port":993,"username":"nci.owner@example.org","sources":[]}"#, "nci_source_disabled"),
            (r#"{"enabled":true,"server":"imap.example.org","port":993,"username":"login-name","password":"p","sources":["nci"]}"#, "address_required"),
        ] {
            let status = configure_at(&path, body.as_bytes()).unwrap();
            assert_eq!(status["nciMailToStatus"], code);
            assert_eq!(status["nciMailTo"], Value::Null);
            assert!(!notify.exists(), "{code}");
        }
        configure_at(&path, br#"{"enabled":true,"server":"imap.example.org","port":993,"username":"login-name","address":"owner@example.org","sources":["nci"]}"#).unwrap();
        assert!(notify.exists());
        let status = clear_config_at(&path).unwrap();
        assert_eq!(status["nciMailToStatus"], "mailbox_not_configured");
        assert!(!notify.exists());
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn mail_address_syntax_matches_job_service() {
        for ok in ["a@b.au", "first.last+nci@mail.example.org", "x_y%z@gmail.com"] {
            assert!(valid_mail_address(ok), "{ok}");
        }
        for bad in ["", "a", "a@b", "-a@b.au", "a b@c.au", "a@b..au", "a@-b.au", "a,b@c.au", "\"a\"@b.au", "a@b.au;rm", ".a@b.au", "a..b@c.au"] {
            assert!(!valid_mail_address(bad), "{bad}");
        }
    }
    #[test]
    fn blocked_delivery_does_not_hold_configuration_lock() {
        let config = make_config(input(), None).unwrap();
        let (admitted_tx, admitted_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let delivery = std::thread::spawn(move || {
            deliver_if_current_with(
                &config,
                || Ok(Some(config.clone())),
                || {
                    admitted_tx.send(()).unwrap();
                    resume_rx.recv().unwrap();
                    Ok(json!({"status":"applied"}))
                },
            )
        });
        admitted_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let (configured_tx, configured_rx) = std::sync::mpsc::channel();
        let configure = std::thread::spawn(move || {
            let guard = CONFIG_LOCK.lock().unwrap();
            configured_tx.send(()).unwrap();
            drop(guard);
        });
        let did_configure = configured_rx.recv_timeout(Duration::from_secs(2)).is_ok();
        resume_tx.send(()).unwrap();
        delivery.join().unwrap().unwrap();
        configure.join().unwrap();
        assert!(did_configure, "configuration waited on a socket delivery");
        let config = make_config(input(), None).unwrap();
        assert_eq!(
            deliver_if_current_with(
                &config,
                || Ok(None),
                || panic!("deleted configuration must not deliver")
            )
            .unwrap()["status"],
            "paused"
        );
    }
    #[test]
    fn private_storage_rejects_permissive_files_and_symlinks() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let path = std::env::temp_dir().join(format!("cutex-mail-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        let file = path.join("secret");
        std::fs::write(&file, b"test-only").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_private(&file, 100).is_err());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_private(&file, 100).unwrap().is_some());
        let link = path.join("link");
        symlink(&file, &link).unwrap();
        assert!(read_private(&link, 100).is_err());
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn status_says_when_nci_verification_is_not_built() {
        let mut state = State::default();
        assert_eq!(pending_status_code(&state, false), None);
        record_mail(&mut state, Ok(mail())).unwrap();
        assert_eq!(
            pending_status_code(&state, false).as_deref(),
            Some("nci_verification_unavailable")
        );
        assert_eq!(pending_status_code(&state, true), None);
        state.delivery_error = Some("qstat_not_configured".into());
        assert_eq!(pending_status_code(&state, true).as_deref(), Some("qstat_not_configured"));
        let path = std::env::temp_dir().join(format!("cutex-mail-nci-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        save_state(&path, &state).unwrap();
        let status = status_at(&path).unwrap();
        assert_eq!(status["nciVerificationAvailable"], cfg!(feature = "nci-qstat"));
        assert_eq!(
            status["statusCode"],
            if cfg!(feature = "nci-qstat") { "qstat_not_configured" } else { "nci_verification_unavailable" }
        );
        std::fs::remove_dir_all(&path).unwrap();
    }
    #[test]
    fn scheduler_observation_without_new_state_consumes_the_trigger() {
        let mut state = State::default();
        record_mail(&mut state, Ok(mail())).unwrap();
        drain_pending(&mut state, |_| Ok(json!({"status":"observed"}))).unwrap();
        assert!(state.pending.is_empty());
        assert_eq!(state.delivery_error, None);
    }
}
