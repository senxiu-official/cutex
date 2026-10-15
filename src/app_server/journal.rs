//! Bounded owner-only diagnostics for native app-server envelopes.

use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Context;
use chrono::Utc;
use serde::Serialize;
use serde_json::Value;

use super::protocol::CorrelationIds;
use super::protocol::InboundMessage;

pub const APP_SERVER_SCHEMA_VERSION: &str = "0.144.1+cutex-inter-agent-v2";
pub const APP_SERVER_EXPERIMENTAL_SCHEMA_SHA256: &str =
    "d2d79395722b9bfa4cef2cd081e3026fd13b4817c256b1d7352afc7e0d4a5531";
pub const DEFAULT_JOURNAL_MAX_BYTES: u64 = 8 * 1024 * 1024;
pub const DEFAULT_JOURNAL_MAX_RECORD_BYTES: usize = 256 * 1024;
pub const DEFAULT_JOURNAL_ROTATIONS: usize = 1;

const MAX_METADATA_CHARS: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppServerSchemaIdentity {
    pub version: String,
    pub sha256: String,
}

impl Default for AppServerSchemaIdentity {
    fn default() -> Self {
        Self {
            version: APP_SERVER_SCHEMA_VERSION.to_string(),
            sha256: APP_SERVER_EXPERIMENTAL_SCHEMA_SHA256.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticJournalOptions {
    pub path: PathBuf,
    pub schema: AppServerSchemaIdentity,
    pub max_bytes: u64,
    pub max_record_bytes: usize,
    pub rotations: usize,
}

impl DiagnosticJournalOptions {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            schema: AppServerSchemaIdentity::default(),
            max_bytes: DEFAULT_JOURNAL_MAX_BYTES,
            max_record_bytes: DEFAULT_JOURNAL_MAX_RECORD_BYTES,
            rotations: DEFAULT_JOURNAL_ROTATIONS,
        }
    }
}

#[derive(Clone)]
pub struct DiagnosticJournal {
    options: DiagnosticJournalOptions,
    writer_lock: Arc<Mutex<()>>,
}

impl DiagnosticJournal {
    pub fn new(options: DiagnosticJournalOptions) -> anyhow::Result<Self> {
        if options.max_bytes == 0 {
            anyhow::bail!("app-server diagnostic journal max_bytes must be positive");
        }
        if options.max_record_bytes == 0 {
            anyhow::bail!("app-server diagnostic journal max_record_bytes must be positive");
        }
        Ok(Self {
            options,
            writer_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn path(&self) -> &Path {
        &self.options.path
    }

    pub fn append(&self, message: &InboundMessage) -> anyhow::Result<JournalAppendResult> {
        let _guard = self.writer_lock.lock().map_err(|_| {
            anyhow::anyhow!("app-server diagnostic journal writer lock was poisoned")
        })?;
        if let Some(parent) = self.options.path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create app-server diagnostic journal directory {}",
                    parent.display()
                )
            })?;
        }

        let raw_bytes = serde_json::to_vec(message.raw())?.len();
        let mut record =
            DiagnosticJournalRecord::from_message(message, &self.options.schema, raw_bytes);
        let mut encoded = serde_json::to_vec(&record)?;
        if encoded.len() > self.options.max_record_bytes {
            record.raw = Value::Null;
            record.raw_truncated = true;
            encoded = serde_json::to_vec(&record)?;
        }
        let incoming_bytes = encoded.len().saturating_add(1) as u64;
        if encoded.len() > self.options.max_record_bytes || incoming_bytes > self.options.max_bytes
        {
            anyhow::bail!("app-server diagnostic journal metadata exceeds configured record limit");
        }

        let rotated = rotate_if_needed(
            &self.options.path,
            incoming_bytes,
            self.options.max_bytes,
            self.options.rotations,
        )?;
        let mut open_options = OpenOptions::new();
        open_options.create(true).append(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open_options.mode(0o600);
        }
        let mut file = open_options.open(&self.options.path).with_context(|| {
            format!(
                "failed to open app-server diagnostic journal {}",
                self.options.path.display()
            )
        })?;
        enforce_owner_only(&self.options.path)?;
        file.write_all(&encoded)?;
        file.write_all(b"\n")?;
        file.flush()?;

        Ok(JournalAppendResult {
            bytes_written: incoming_bytes,
            raw_truncated: record.raw_truncated,
            rotated,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalAppendResult {
    pub bytes_written: u64,
    pub raw_truncated: bool,
    pub rotated: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticJournalRecord {
    received_at: String,
    adapter_version: &'static str,
    schema_version: String,
    schema_sha256: String,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<Value>,
    correlation: JournalCorrelationIds,
    raw_bytes: usize,
    raw_truncated: bool,
    raw: Value,
}

impl DiagnosticJournalRecord {
    fn from_message(
        message: &InboundMessage,
        schema: &AppServerSchemaIdentity,
        raw_bytes: usize,
    ) -> Self {
        let (kind, request_id) = match message {
            InboundMessage::Response(response) => {
                ("response", Some(bounded_id(response.id.clone())))
            }
            InboundMessage::Notification(_) => ("notification", None),
            InboundMessage::ServerRequest(request) => {
                ("serverRequest", Some(bounded_id(request.id.clone())))
            }
        };
        Self {
            received_at: Utc::now().to_rfc3339(),
            adapter_version: env!("CARGO_PKG_VERSION"),
            schema_version: schema.version.clone(),
            schema_sha256: schema.sha256.clone(),
            kind,
            method: message
                .method()
                .map(|method| bounded_string(method, MAX_METADATA_CHARS)),
            request_id,
            correlation: JournalCorrelationIds::from(message.correlations()),
            raw_bytes,
            raw_truncated: false,
            raw: message.raw().clone(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JournalCorrelationIds {
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    item_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_user_message_id: Option<String>,
}

impl From<CorrelationIds> for JournalCorrelationIds {
    fn from(ids: CorrelationIds) -> Self {
        Self {
            thread_id: ids
                .thread_id
                .map(|value| bounded_string(&value, MAX_METADATA_CHARS)),
            turn_id: ids
                .turn_id
                .map(|value| bounded_string(&value, MAX_METADATA_CHARS)),
            item_id: ids
                .item_id
                .map(|value| bounded_string(&value, MAX_METADATA_CHARS)),
            client_user_message_id: ids
                .client_user_message_id
                .map(|value| bounded_string(&value, MAX_METADATA_CHARS)),
        }
    }
}

fn bounded_id(id: Value) -> Value {
    match id {
        Value::String(value) => Value::String(bounded_string(&value, MAX_METADATA_CHARS)),
        value => value,
    }
}

fn bounded_string(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn rotate_if_needed(
    path: &Path,
    incoming_bytes: u64,
    max_bytes: u64,
    rotations: usize,
) -> anyhow::Result<bool> {
    let current_bytes = match fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to stat app-server diagnostic journal {}",
                    path.display()
                )
            });
        }
    };
    if current_bytes == 0 || current_bytes.saturating_add(incoming_bytes) <= max_bytes {
        return Ok(false);
    }
    if rotations == 0 {
        fs::remove_file(path).with_context(|| {
            format!(
                "failed to truncate app-server diagnostic journal {}",
                path.display()
            )
        })?;
        return Ok(true);
    }

    remove_if_present(&rotated_path(path, rotations))?;
    for index in (1..rotations).rev() {
        let from = rotated_path(path, index);
        if !from.exists() {
            continue;
        }
        let to = rotated_path(path, index + 1);
        remove_if_present(&to)?;
        fs::rename(&from, &to).with_context(|| {
            format!(
                "failed to rotate app-server diagnostic journal {} to {}",
                from.display(),
                to.display()
            )
        })?;
    }
    let first = rotated_path(path, 1);
    remove_if_present(&first)?;
    fs::rename(path, &first).with_context(|| {
        format!(
            "failed to rotate app-server diagnostic journal {} to {}",
            path.display(),
            first.display()
        )
    })?;
    Ok(true)
}

fn rotated_path(path: &Path, index: usize) -> PathBuf {
    let extension = match path.extension().and_then(|value| value.to_str()) {
        Some(extension) if !extension.is_empty() => format!("{extension}.{index}"),
        _ => index.to_string(),
    };
    path.with_extension(extension)
}

fn remove_if_present(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to remove rotated journal {}", path.display())),
    }
}

#[cfg(unix)]
fn enforce_owner_only(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).with_context(|| {
        format!(
            "failed to secure app-server diagnostic journal {}",
            path.display()
        )
    })
}

#[cfg(not(unix))]
fn enforce_owner_only(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_server::protocol::classify_inbound;
    use serde_json::json;

    #[test]
    fn journal_bounds_raw_payload_and_rotates() {
        let directory = std::env::temp_dir().join(format!(
            "cutex-app-server-journal-test-{}",
            uuid::Uuid::new_v4()
        ));
        let path = directory.join("native-events.jsonl");
        let mut options = DiagnosticJournalOptions::new(path.clone());
        options.max_bytes = 600;
        options.max_record_bytes = 800;
        let journal = DiagnosticJournal::new(options).expect("create journal");
        let message = classify_inbound(json!({
            "method": "future/native/event",
            "params": {
                "threadId": "thread-1",
                "payload": "x".repeat(2_000)
            }
        }))
        .expect("parse notification");

        let first = journal.append(&message).expect("append first record");
        assert!(first.raw_truncated);
        let second = journal.append(&message).expect("append second record");
        assert!(second.rotated);
        assert!(rotated_path(&path, 1).exists());

        let current = fs::read_to_string(&path).expect("read journal");
        let record: Value = serde_json::from_str(current.trim()).expect("parse journal record");
        assert_eq!(record["method"], "future/native/event");
        assert_eq!(record["correlation"]["threadId"], "thread-1");
        assert_eq!(record["rawTruncated"], true);
        assert_eq!(record["raw"], Value::Null);
        assert_eq!(
            record["schemaSha256"],
            APP_SERVER_EXPERIMENTAL_SCHEMA_SHA256
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(&path)
                    .expect("journal metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        fs::remove_dir_all(directory).expect("remove journal directory");
    }

    #[test]
    fn journal_preserves_unknown_server_request_identity() {
        let directory = std::env::temp_dir().join(format!(
            "cutex-app-server-request-journal-test-{}",
            uuid::Uuid::new_v4()
        ));
        let path = directory.join("native-events.jsonl");
        let journal = DiagnosticJournal::new(DiagnosticJournalOptions::new(path.clone()))
            .expect("create journal");
        let message = classify_inbound(json!({
            "id": "request-1",
            "method": "future/request",
            "params": { "threadId": "thread-1", "itemId": "item-1" }
        }))
        .expect("parse server request");

        let result = journal.append(&message).expect("append request");
        assert!(!result.raw_truncated);
        let record: Value =
            serde_json::from_str(fs::read_to_string(&path).expect("read journal").trim())
                .expect("parse journal record");
        assert_eq!(record["kind"], "serverRequest");
        assert_eq!(record["requestId"], "request-1");
        assert_eq!(record["correlation"]["itemId"], "item-1");

        fs::remove_dir_all(directory).expect("remove journal directory");
    }
}
