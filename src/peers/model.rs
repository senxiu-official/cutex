use super::policy::Caps;
use crate::asks::model::{
    AnswerRecord, AskOptions, CloseOutcome, ExecutionState, Hold, InterruptReason, ReviewState,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Remote diagnostic prose is never a local state/delivery identifier.
pub fn sanitize_diagnostic(value: &str) -> String {
    let mut out = String::new();
    let mut space = false;
    for c in value.chars() {
        if c.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if c.is_control()
            || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            continue;
        }
        if out.len() + c.len_utf8() + usize::from(space) > 512 {
            break;
        }
        if space {
            out.push(' ');
        }
        out.push(c);
        space = false;
    }
    out
}

fn serialize_diagnostic<S: Serializer>(
    value: &Option<String>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    value
        .as_deref()
        .map(sanitize_diagnostic)
        .serialize(serializer)
}

fn deserialize_diagnostic<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?
        .as_deref()
        .map(sanitize_diagnostic))
}

pub fn diagnostic_preview(value: &str) -> Option<String> {
    crate::asks::views::preview(value).map(|value| sanitize_diagnostic(&value))
}

/// Stored/viewed snapshot metadata. Full answer text exists only in artifacts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSnapshot(super::wire::WireSnapshotV1);

impl From<super::wire::WireSnapshotV1> for StoredSnapshot {
    fn from(mut snapshot: super::wire::WireSnapshotV1) -> Self {
        snapshot.last_error = snapshot.last_error.as_deref().map(sanitize_diagnostic);
        if let Some(answer) = &mut snapshot.answer {
            answer.metadata.last_error = answer
                .metadata
                .last_error
                .as_deref()
                .map(sanitize_diagnostic);
            answer.preview = snapshot
                .text
                .as_deref()
                .or(answer.preview.as_deref())
                .and_then(diagnostic_preview);
        }
        snapshot.text = None;
        Self(snapshot)
    }
}

impl Serialize for StoredSnapshot {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut value = serde_json::to_value(&self.0).map_err(serde::ser::Error::custom)?;
        value.as_object_mut().unwrap().remove("text");
        value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for StoredSnapshot {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        super::wire::WireSnapshotV1::deserialize(deserializer).map(Into::into)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LateAnswer {
    #[serde(flatten)]
    pub snapshot: StoredSnapshot,
    pub artifact_id: Option<String>,
    pub preview: Option<String>,
}

impl LateAnswer {
    pub fn new(snapshot: super::wire::WireSnapshotV1, artifact_id: Option<String>) -> Self {
        let preview = snapshot
            .text
            .as_deref()
            .or_else(|| {
                snapshot
                    .answer
                    .as_ref()
                    .and_then(|answer| answer.preview.as_deref())
            })
            .and_then(diagnostic_preview);
        Self {
            snapshot: snapshot.into(),
            artifact_id,
            preview,
        }
    }
}

impl<'de> Deserialize<'de> for LateAnswer {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Stored {
            #[serde(flatten)]
            snapshot: super::wire::WireSnapshotV1,
            #[serde(default)]
            artifact_id: Option<String>,
            #[serde(default)]
            preview: Option<String>,
        }
        let value = Stored::deserialize(deserializer)?;
        // Earlier writers already put the text in the content-addressed store,
        // but kept only the inline snapshot. Recover that existing reference.
        let artifact_id = value.artifact_id.or_else(|| {
            value
                .snapshot
                .text
                .as_ref()
                .map(|text| format!("art-{}", crate::asks::sha256_hex(text.as_bytes())))
        });
        let mut answer = Self::new(value.snapshot, artifact_id);
        if answer.preview.is_none() {
            answer.preview = value.preview.as_deref().and_then(diagnostic_preview);
        }
        Ok(answer)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Requester {
    pub cutex_session_id: String,
    pub name: Option<String>,
    pub caps: Caps,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    into = "super::wire::WireCreateV1",
    try_from = "super::wire::WireCreateV1"
)]
pub struct Create {
    pub requester: Requester,
    pub target: String,
    pub title: String,
    pub text: String,
    pub options: AskOptions,
    pub hops: u32,
    pub request_sha256: String,
}
impl Create {
    pub fn digest(&self) -> String {
        super::wire::WireCreateV1::from(self).digest()
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Op {
    pub op_id: String,
    pub op: String,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sync {
    /// The owner or expiry ended this mirror locally. Remote updates are diagnostic only.
    #[serde(default)]
    pub local_terminal: bool,
    #[serde(default)]
    pub reconcile_at: Option<String>,
    #[serde(default)]
    pub interrupt_reason: Option<InterruptReason>,
    pub state: String,
    pub last_remote_revision: u64,
    pub pending_ops: Vec<Op>,
    #[serde(
        default,
        serialize_with = "serialize_diagnostic",
        deserialize_with = "deserialize_diagnostic"
    )]
    pub last_error: Option<String>,
    pub next_attempt_at: Option<String>,
    pub last_received_at: String,
    pub failures: u32,
    pub accepted: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Remote {
    pub peer_id: String,
    pub label: String,
    pub target_name: String,
    pub request: Create,
    pub sync: Sync,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_state: Option<StoredSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub late_answer: Option<LateAnswer>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub ask_id: String,
    pub revision: u64,
    pub execution: ExecutionState,
    pub hold: Option<Hold>,
    pub closed: Option<CloseOutcome>,
    pub review: Option<ReviewState>,
    pub answer: Option<AnswerRecord>,
    pub text: Option<String>,
    pub interrupt_reason: Option<InterruptReason>,
    #[serde(
        default,
        serialize_with = "serialize_diagnostic",
        deserialize_with = "deserialize_diagnostic"
    )]
    pub last_error: Option<String>,
    /// A state this release cannot interpret. Never apply fallback enum values to an ask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_state: Option<super::wire::WireSnapshotV1>,
}
impl Snapshot {
    pub fn unknown_state_message(&self) -> Option<String> {
        let states = self.remote_state.as_ref()?.unknown_states();
        (!states.is_empty()).then(|| {
            sanitize_diagnostic(&format!(
                "对端状态：{}（本机版本不认识）",
                states.join(", ")
            ))
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub event_id: String,
    pub ask_id: String,
    pub revision: u64,
    pub snapshot: Snapshot,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Inbound {
    #[serde(default)]
    pub dead_letters: Vec<DeadLetter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_at: Option<String>,
    #[serde(default)]
    pub result_undeliverable: bool,
    #[serde(default)]
    pub delivery_stopped: bool,
    #[serde(
        default,
        serialize_with = "serialize_diagnostic",
        deserialize_with = "deserialize_diagnostic"
    )]
    pub last_error: Option<String>,
    pub request: Create,
    pub peer_deliveries: Vec<Event>,
    pub received_ops: Vec<Op>,
    pub next_attempt_at: Option<String>,
    pub failures: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeadLetter {
    pub event: Event,
    pub failed_at: String,
    pub code: String,
}
