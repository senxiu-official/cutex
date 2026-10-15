//! Version 1 peer messages. Never embed the private ask-store representation here.
//!
//! Additive object fields are ignored. Unknown enum values are retained for display,
//! and must never drive the local scheduler or change the meaning of a command.
use super::{model, policy};
use crate::asks::model as ask;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{json, Value};

pub const PROTOCOL: u32 = 1;
pub const FEATURES: &[&str] = &[
    "asks.v1",
    "asks.snapshot.v1",
    "asks.close.v1",
    "agents.list.v1",
];

fn v1() -> u32 {
    1
}

/// Agent-visible names are a single line, bounded by bytes at a UTF-8 boundary.
pub fn sanitize_name(value: &str) -> String {
    let mut out = String::new();
    let mut space = false;
    for c in value.chars() {
        if c.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
            continue;
        }
        let needed = c.len_utf8() + usize::from(space);
        if out.len() + needed > 128 {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownValue(pub String);

impl std::fmt::Display for UnknownValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unsupported peer protocol value: {}", self.0)
    }
}
impl std::error::Error for UnknownValue {}

macro_rules! wire_enum {
    ($name:ident, $internal:ty, {$($variant:ident => $text:literal),+ $(,)?}) => {
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub enum $name {
            $($variant,)+
            Unknown { raw: String },
        }
        impl $name {
            pub fn as_str(&self) -> &str {
                match self {
                    $(Self::$variant => $text,)+
                    Self::Unknown { raw } => raw,
                }
            }
            pub fn unknown(&self) -> Option<&str> {
                match self { Self::Unknown { raw } => Some(raw), _ => None }
            }
        }
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(deserializer)?;
                Ok(match raw.as_str() {
                    $($text => Self::$variant,)+
                    _ => Self::Unknown { raw },
                })
            }
        }
        impl From<$internal> for $name {
            fn from(value: $internal) -> Self {
                match value { $(<$internal>::$variant => Self::$variant,)+ }
            }
        }
        impl TryFrom<$name> for $internal {
            type Error = UnknownValue;
            fn try_from(value: $name) -> Result<Self, Self::Error> {
                match value {
                    $($name::$variant => Ok(Self::$variant),)+
                    $name::Unknown { raw } => Err(UnknownValue(raw)),
                }
            }
        }
    };
}

wire_enum!(WireExecutionState, ask::ExecutionState, {
    PendingReview => "pendingReview", Queued => "queued", Admitted => "admitted",
    Running => "running", Answered => "answered", Failed => "failed",
    Interrupted => "interrupted", Cancelled => "cancelled", Rejected => "rejected"
});
wire_enum!(WireCloseOutcome, ask::CloseOutcome, {
    Completed => "completed", Accepted => "accepted", Abandoned => "abandoned",
    Stopped => "stopped", Cancelled => "cancelled", Superseded => "superseded"
});
wire_enum!(WireReviewState, ask::ReviewState, {
    AwaitingAcceptance => "awaitingAcceptance", Accepted => "accepted",
    ChangesRequested => "changesRequested", Abandoned => "abandoned"
});
wire_enum!(WireInterruptReason, ask::InterruptReason, {
    PeerRemoved => "peerRemoved", PeerDisabled => "peerDisabled", PeersDisabled => "peersDisabled",
    RemovedByPeer => "removedByPeer", PeerStripped => "peerStripped", Stopped => "stopped",
    OwnerStopped => "ownerStopped", RuntimeRestart => "runtimeRestart", OutcomeUnknown => "outcomeUnknown"
});
wire_enum!(WireAnswerStatus, ask::AnswerStatus, {
    Completed => "completed", Failed => "failed", Interrupted => "interrupted"
});
wire_enum!(WireIfBusy, ask::IfBusy, { Queue => "queue", Fail => "fail" });
wire_enum!(WireQuestionRoute, ask::QuestionRoute, { Requester => "requester", Owner => "owner" });
wire_enum!(WireNotify, ask::Notify, { Turn => "turn", Passive => "passive" });
wire_enum!(WirePausedBy, ask::PausedBy, { Owner => "owner", PlanMode => "planMode" });
wire_enum!(WireBlockedReason, ask::BlockedReason, {
    Login => "login", Quota => "quota", RuntimeDown => "runtimeDown", Budget => "budget"
});
wire_enum!(WireAwaitUntil, ask::AwaitUntil, { All => "all", Any => "any" });

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WireContext {
    None,
    Brief,
    Recent { turns: u32 },
    Fork,
    Unknown { raw: String },
}
impl Serialize for WireContext {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::None => json!({"kind":"none"}),
            Self::Brief => json!({"kind":"brief"}),
            Self::Recent { turns } => json!({"kind":"recent", "turns":turns}),
            Self::Fork => json!({"kind":"fork"}),
            Self::Unknown { raw } => json!({"kind":raw}),
        }
        .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for WireContext {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let raw = value["kind"]
            .as_str()
            .ok_or_else(|| serde::de::Error::custom("context.kind must be a string"))?;
        Ok(match raw {
            "none" => Self::None,
            "brief" => Self::Brief,
            "fork" => Self::Fork,
            "recent" => Self::Recent {
                turns: serde_json::from_value(value["turns"].clone())
                    .map_err(serde::de::Error::custom)?,
            },
            _ => Self::Unknown { raw: raw.into() },
        })
    }
}
impl From<&ask::ContextOption> for WireContext {
    fn from(value: &ask::ContextOption) -> Self {
        match value {
            ask::ContextOption::None => Self::None,
            ask::ContextOption::Brief => Self::Brief,
            ask::ContextOption::Recent { turns } => Self::Recent { turns: *turns },
            ask::ContextOption::Fork => Self::Fork,
        }
    }
}
impl TryFrom<WireContext> for ask::ContextOption {
    type Error = UnknownValue;
    fn try_from(value: WireContext) -> Result<Self, Self::Error> {
        Ok(match value {
            WireContext::None => Self::None,
            WireContext::Brief => Self::Brief,
            WireContext::Recent { turns } => Self::Recent { turns },
            WireContext::Fork => Self::Fork,
            WireContext::Unknown { raw } => return Err(UnknownValue(raw)),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WireOptionsV1 {
    pub if_busy: WireIfBusy,
    pub review: bool,
    pub questions: WireQuestionRoute,
    pub notify: WireNotify,
    pub context: WireContext,
}
impl Default for WireOptionsV1 {
    fn default() -> Self {
        Self {
            if_busy: WireIfBusy::Queue,
            review: false,
            questions: WireQuestionRoute::Requester,
            notify: WireNotify::Turn,
            context: WireContext::Brief,
        }
    }
}
impl From<&ask::AskOptions> for WireOptionsV1 {
    fn from(value: &ask::AskOptions) -> Self {
        Self {
            if_busy: value.if_busy.into(),
            review: value.review,
            questions: value.questions.into(),
            notify: value.notify.into(),
            context: (&value.context).into(),
        }
    }
}
impl TryFrom<WireOptionsV1> for ask::AskOptions {
    type Error = UnknownValue;
    fn try_from(value: WireOptionsV1) -> Result<Self, Self::Error> {
        Ok(Self {
            if_busy: value.if_busy.try_into()?,
            review: value.review,
            questions: value.questions.try_into()?,
            notify: value.notify.try_into()?,
            context: value.context.try_into()?,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WireCapsV1 {
    pub sandbox: Option<String>,
    pub network_access: bool,
    pub unrestricted_services: bool,
}
impl From<&policy::Caps> for WireCapsV1 {
    fn from(value: &policy::Caps) -> Self {
        Self {
            sandbox: value.sandbox.clone(),
            network_access: value.network_access,
            unrestricted_services: value.unrestricted_services,
        }
    }
}
impl From<WireCapsV1> for policy::Caps {
    fn from(value: WireCapsV1) -> Self {
        Self {
            sandbox: value.sandbox,
            network_access: value.network_access,
            unrestricted_services: value.unrestricted_services,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireRequesterV1 {
    pub cutex_session_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub caps: WireCapsV1,
}
impl From<&model::Requester> for WireRequesterV1 {
    fn from(value: &model::Requester) -> Self {
        Self {
            cutex_session_id: value.cutex_session_id.clone(),
            name: value.name.as_deref().map(sanitize_name),
            caps: (&value.caps).into(),
        }
    }
}
impl From<WireRequesterV1> for model::Requester {
    fn from(value: WireRequesterV1) -> Self {
        Self {
            cutex_session_id: value.cutex_session_id,
            name: value.name.as_deref().map(sanitize_name),
            caps: value.caps.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireCreateV1 {
    #[serde(default = "v1")]
    pub schema_version: u32,
    pub requester: WireRequesterV1,
    pub target: String,
    pub title: String,
    pub text: String,
    #[serde(default)]
    pub options: WireOptionsV1,
    #[serde(default)]
    pub hops: u32,
    pub request_sha256: String,
}
impl WireCreateV1 {
    /// The complete, frozen digest input for v1; future DTO/internal fields cannot enter it.
    pub fn digest_value(&self) -> Value {
        json!({
            "schemaVersion": self.schema_version,
            "requester": {
                "cutexSessionId": self.requester.cutex_session_id,
                "name": self.requester.name.as_deref().map(sanitize_name),
                "caps": {
                    "sandbox": self.requester.caps.sandbox,
                    "networkAccess": self.requester.caps.network_access,
                    "unrestrictedServices": self.requester.caps.unrestricted_services
                }
            },
            "target": self.target, "title": self.title, "text": self.text,
            "options": {
                "ifBusy": self.options.if_busy.as_str(), "review": self.options.review,
                "questions": self.options.questions.as_str(), "notify": self.options.notify.as_str(),
                "context": self.options.context
            },
            "hops": self.hops
        })
    }
    pub fn digest(&self) -> String {
        crate::approvals::request_digest(&self.digest_value())
    }
}
impl From<&model::Create> for WireCreateV1 {
    fn from(value: &model::Create) -> Self {
        Self {
            schema_version: 1,
            requester: (&value.requester).into(),
            target: value.target.clone(),
            title: value.title.clone(),
            text: value.text.clone(),
            options: (&value.options).into(),
            hops: value.hops,
            request_sha256: value.request_sha256.clone(),
        }
    }
}
impl From<model::Create> for WireCreateV1 {
    fn from(value: model::Create) -> Self {
        Self::from(&value)
    }
}
impl TryFrom<WireCreateV1> for model::Create {
    type Error = UnknownValue;
    fn try_from(value: WireCreateV1) -> Result<Self, Self::Error> {
        if value.schema_version != 1 {
            return Err(UnknownValue(format!(
                "create.schemaVersion={}",
                value.schema_version
            )));
        }
        Ok(Self {
            requester: value.requester.into(),
            target: value.target,
            title: value.title,
            text: value.text,
            options: value.options.try_into()?,
            hops: value.hops,
            request_sha256: value.request_sha256,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WireHold {
    PeerUnreachable,
    PeerIdentityChanged,
    PeerVersionMismatch,
    PeerUnknown {
        raw: String,
    },
    TargetBusy {
        position: Option<usize>,
    },
    Paused {
        by: WirePausedBy,
    },
    HeldByRuntime {
        native: Option<String>,
    },
    Blocked {
        by: WireBlockedReason,
    },
    WaitingForApproval,
    WaitingForInput,
    ProfileSwitchPending,
    RuntimeUnsupported {
        missing: String,
    },
    WaitingForEvent {
        jobs: Vec<String>,
        until: WireAwaitUntil,
        since: String,
        timeout_at: Option<String>,
    },
    Unknown {
        raw: String,
    },
}
impl Serialize for WireHold {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut value = match self {
            Self::PeerUnreachable => json!({"reason":"peerUnreachable"}),
            Self::PeerIdentityChanged => json!({"reason":"peerIdentityChanged"}),
            Self::PeerVersionMismatch => json!({"reason":"peerVersionMismatch"}),
            Self::PeerUnknown { raw } => json!({"reason":"peerUnknown", "raw":raw}),
            Self::TargetBusy { position } => json!({"reason":"targetBusy", "position":position}),
            Self::Paused { by } => json!({"reason":"paused", "by":by}),
            Self::HeldByRuntime { native } => json!({"reason":"heldByRuntime", "native":native}),
            Self::Blocked { by } => json!({"reason":"blocked", "by":by}),
            Self::WaitingForApproval => json!({"reason":"waitingForApproval"}),
            Self::WaitingForInput => json!({"reason":"waitingForInput"}),
            Self::ProfileSwitchPending => json!({"reason":"profileSwitchPending"}),
            Self::RuntimeUnsupported { missing } => {
                json!({"reason":"runtimeUnsupported", "missing":missing})
            }
            Self::WaitingForEvent {
                jobs,
                until,
                since,
                timeout_at,
            } => {
                json!({"reason":"waitingForEvent", "jobs":jobs, "until":until, "since":since, "timeoutAt":timeout_at})
            }
            Self::Unknown { raw } => json!({"reason":raw}),
        };
        value.as_object_mut().unwrap().retain(|_, v| !v.is_null());
        value.serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for WireHold {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let reason = value["reason"]
            .as_str()
            .ok_or_else(|| serde::de::Error::custom("hold.reason must be a string"))?;
        fn field<T: serde::de::DeserializeOwned, E: serde::de::Error>(
            value: &Value,
            name: &str,
        ) -> Result<T, E> {
            serde_json::from_value(value[name].clone()).map_err(E::custom)
        }
        Ok(match reason {
            "peerUnreachable" => Self::PeerUnreachable,
            "peerIdentityChanged" => Self::PeerIdentityChanged,
            "peerVersionMismatch" => Self::PeerVersionMismatch,
            "peerUnknown" => Self::PeerUnknown {
                raw: field::<_, D::Error>(&value, "raw")?,
            },
            "targetBusy" => Self::TargetBusy {
                position: field::<_, D::Error>(&value, "position")?,
            },
            "paused" => Self::Paused {
                by: field::<_, D::Error>(&value, "by")?,
            },
            "heldByRuntime" => Self::HeldByRuntime {
                native: field::<_, D::Error>(&value, "native")?,
            },
            "blocked" => Self::Blocked {
                by: field::<_, D::Error>(&value, "by")?,
            },
            "waitingForApproval" => Self::WaitingForApproval,
            "waitingForInput" => Self::WaitingForInput,
            "profileSwitchPending" => Self::ProfileSwitchPending,
            "runtimeUnsupported" => Self::RuntimeUnsupported {
                missing: field::<_, D::Error>(&value, "missing")?,
            },
            "waitingForEvent" => Self::WaitingForEvent {
                jobs: field::<_, D::Error>(&value, "jobs")?,
                until: field::<_, D::Error>(&value, "until")?,
                since: field::<_, D::Error>(&value, "since")?,
                timeout_at: field::<_, D::Error>(&value, "timeoutAt")?,
            },
            _ => Self::Unknown { raw: reason.into() },
        })
    }
}
impl WireHold {
    pub fn unknown(&self) -> Option<&str> {
        match self {
            Self::Unknown { raw } | Self::PeerUnknown { raw } => Some(raw),
            Self::Paused { by } => by.unknown(),
            Self::Blocked { by } => by.unknown(),
            Self::WaitingForEvent { until, .. } => until.unknown(),
            _ => None,
        }
    }
}
impl From<&ask::Hold> for WireHold {
    fn from(value: &ask::Hold) -> Self {
        match value {
            ask::Hold::PeerUnreachable => Self::PeerUnreachable,
            ask::Hold::PeerIdentityChanged => Self::PeerIdentityChanged,
            ask::Hold::PeerVersionMismatch => Self::PeerVersionMismatch,
            ask::Hold::PeerUnknown { raw } => Self::PeerUnknown { raw: raw.clone() },
            ask::Hold::TargetBusy { position } => Self::TargetBusy {
                position: *position,
            },
            ask::Hold::Paused { by } => Self::Paused { by: (*by).into() },
            ask::Hold::HeldByRuntime { native } => Self::HeldByRuntime {
                native: native.clone(),
            },
            ask::Hold::Blocked { by } => Self::Blocked { by: (*by).into() },
            ask::Hold::WaitingForApproval => Self::WaitingForApproval,
            ask::Hold::WaitingForInput => Self::WaitingForInput,
            ask::Hold::ProfileSwitchPending => Self::ProfileSwitchPending,
            ask::Hold::RuntimeUnsupported { missing } => Self::RuntimeUnsupported {
                missing: missing.clone(),
            },
            ask::Hold::WaitingForEvent {
                jobs,
                until,
                since,
                timeout_at,
            } => Self::WaitingForEvent {
                jobs: jobs.clone(),
                until: (*until).into(),
                since: since.clone(),
                timeout_at: timeout_at.clone(),
            },
        }
    }
}
impl TryFrom<WireHold> for ask::Hold {
    type Error = UnknownValue;
    fn try_from(value: WireHold) -> Result<Self, Self::Error> {
        Ok(match value {
            WireHold::PeerUnreachable => Self::PeerUnreachable,
            WireHold::PeerIdentityChanged => Self::PeerIdentityChanged,
            WireHold::PeerVersionMismatch => Self::PeerVersionMismatch,
            WireHold::PeerUnknown { raw } | WireHold::Unknown { raw } => {
                return Err(UnknownValue(raw))
            }
            WireHold::TargetBusy { position } => Self::TargetBusy { position },
            WireHold::Paused { by } => Self::Paused { by: by.try_into()? },
            WireHold::HeldByRuntime { native } => Self::HeldByRuntime { native },
            WireHold::Blocked { by } => Self::Blocked { by: by.try_into()? },
            WireHold::WaitingForApproval => Self::WaitingForApproval,
            WireHold::WaitingForInput => Self::WaitingForInput,
            WireHold::ProfileSwitchPending => Self::ProfileSwitchPending,
            WireHold::RuntimeUnsupported { missing } => Self::RuntimeUnsupported { missing },
            WireHold::WaitingForEvent {
                jobs,
                until,
                since,
                timeout_at,
            } => Self::WaitingForEvent {
                jobs,
                until: until.try_into()?,
                since,
                timeout_at,
            },
        })
    }
}

/// Portable metadata only. Artifact ids, native diff/turn ids, model/profile,
/// token usage, and local included-note ids never cross this boundary.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WireAnswerMetadataV1 {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub owner_steered: bool,
    pub shared_turn: bool,
    pub compacted: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireAnswerV1 {
    pub answer_id: String,
    pub ask_id: String,
    pub attempt: u32,
    pub revision: u32,
    pub status: WireAnswerStatus,
    pub bytes: usize,
    #[serde(default)]
    pub metadata: WireAnswerMetadataV1,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}
impl From<&ask::AnswerRecord> for WireAnswerV1 {
    fn from(value: &ask::AnswerRecord) -> Self {
        Self {
            answer_id: value.answer_id.clone(),
            ask_id: value.ask_id.clone(),
            attempt: value.attempt,
            revision: value.revision,
            status: value.status.into(),
            bytes: value.bytes,
            metadata: WireAnswerMetadataV1 {
                duration_ms: value.metadata.duration_ms,
                last_error: value.metadata.last_error.clone(),
                owner_steered: value.metadata.owner_steered,
                shared_turn: value.metadata.shared_turn,
                compacted: value.metadata.compacted,
            },
            created_at: value.created_at.clone(),
            preview: value.preview.clone(),
        }
    }
}
impl TryFrom<WireAnswerV1> for ask::AnswerRecord {
    type Error = UnknownValue;
    fn try_from(value: WireAnswerV1) -> Result<Self, Self::Error> {
        Ok(Self {
            answer_id: value.answer_id,
            ask_id: value.ask_id,
            attempt: value.attempt,
            revision: value.revision,
            status: value.status.try_into()?,
            artifact_id: None,
            bytes: value.bytes,
            metadata: ask::AnswerMetadata {
                duration_ms: value.metadata.duration_ms,
                last_error: value.metadata.last_error,
                owner_steered: value.metadata.owner_steered,
                shared_turn: value.metadata.shared_turn,
                compacted: value.metadata.compacted,
                ..Default::default()
            },
            created_at: value.created_at,
            preview: value.preview,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireSnapshotV1 {
    #[serde(default = "v1")]
    pub schema_version: u32,
    pub ask_id: String,
    pub revision: u64,
    pub execution: WireExecutionState,
    #[serde(default)]
    pub hold: Option<WireHold>,
    #[serde(default)]
    pub closed: Option<WireCloseOutcome>,
    #[serde(default)]
    pub review: Option<WireReviewState>,
    #[serde(default)]
    pub answer: Option<WireAnswerV1>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub interrupt_reason: Option<WireInterruptReason>,
    #[serde(default)]
    pub last_error: Option<String>,
}
impl WireSnapshotV1 {
    pub fn unknown_states(&self) -> Vec<String> {
        let mut states = Vec::new();
        if self.schema_version != 1 {
            states.push(format!("snapshot.schemaVersion={}", self.schema_version));
        }
        for (field, raw) in [
            ("execution", self.execution.unknown()),
            ("hold", self.hold.as_ref().and_then(WireHold::unknown)),
            (
                "closed",
                self.closed.as_ref().and_then(WireCloseOutcome::unknown),
            ),
            (
                "review",
                self.review.as_ref().and_then(WireReviewState::unknown),
            ),
            (
                "answer.status",
                self.answer.as_ref().and_then(|a| a.status.unknown()),
            ),
            (
                "interruptReason",
                self.interrupt_reason
                    .as_ref()
                    .and_then(WireInterruptReason::unknown),
            ),
        ] {
            if let Some(raw) = raw {
                states.push(format!("{field}={}", sanitize_name(raw)));
            }
        }
        states
    }
}
impl From<&model::Snapshot> for WireSnapshotV1 {
    fn from(value: &model::Snapshot) -> Self {
        if let Some(raw) = &value.remote_state {
            return raw.clone();
        }
        Self {
            schema_version: 1,
            ask_id: value.ask_id.clone(),
            revision: value.revision,
            execution: value.execution.into(),
            hold: value.hold.as_ref().map(Into::into),
            closed: value.closed.map(Into::into),
            review: value.review.map(Into::into),
            answer: value.answer.as_ref().map(Into::into),
            text: value.text.clone(),
            interrupt_reason: value.interrupt_reason.map(Into::into),
            last_error: value.last_error.clone(),
        }
    }
}
impl From<model::Snapshot> for WireSnapshotV1 {
    fn from(value: model::Snapshot) -> Self {
        Self::from(&value)
    }
}
impl From<WireSnapshotV1> for model::Snapshot {
    fn from(value: WireSnapshotV1) -> Self {
        // Fallbacks are storage placeholders, not decisions. apply() must detect
        // remote_state and expose PeerUnknown instead of applying these values.
        let remote_state = (!value.unknown_states().is_empty()).then(|| value.clone());
        Self {
            ask_id: value.ask_id,
            revision: value.revision,
            execution: value
                .execution
                .try_into()
                .unwrap_or(ask::ExecutionState::Queued),
            hold: value.hold.and_then(|v| v.try_into().ok()),
            closed: value.closed.and_then(|v| v.try_into().ok()),
            review: value.review.and_then(|v| v.try_into().ok()),
            answer: value.answer.and_then(|v| v.try_into().ok()),
            text: value.text,
            interrupt_reason: value.interrupt_reason.and_then(|v| v.try_into().ok()),
            last_error: value.last_error,
            remote_state,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireEventV1 {
    pub event_id: String,
    pub ask_id: String,
    pub revision: u64,
    pub snapshot: WireSnapshotV1,
}
impl From<&model::Event> for WireEventV1 {
    fn from(value: &model::Event) -> Self {
        Self {
            event_id: value.event_id.clone(),
            ask_id: value.ask_id.clone(),
            revision: value.revision,
            snapshot: (&value.snapshot).into(),
        }
    }
}
impl From<WireEventV1> for model::Event {
    fn from(value: WireEventV1) -> Self {
        Self {
            event_id: value.event_id,
            ask_id: value.ask_id,
            revision: value.revision,
            snapshot: value.snapshot.into(),
        }
    }
}
