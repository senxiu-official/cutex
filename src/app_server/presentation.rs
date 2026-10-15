//! Independent durable display facts. Never an input, A4, wake, or business ACK.
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

use super::external_input::{ExternalInputClient, Source, SourceKind};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Format {
    PlainText,
    Markdown,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReferenceKind {
    ExternalInput,
    McpInvocation,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Reference {
    pub kind: ReferenceKind,
    pub id: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Presentation {
    pub id: String,
    pub source: Source,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
    pub format: Format,
    pub references: Vec<Reference>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Receipt {
    pub version: u32,
    pub owner_id: String,
    pub origin_thread_id: String,
    pub presentation: Presentation,
    pub semantic_sha256: String,
    pub receipt_id: String,
}
fn framed(domain: &[u8], fields: &[&str]) -> String {
    let mut h = Sha256::new();
    h.update(domain);
    for field in fields {
        h.update((field.len() as u64).to_be_bytes());
        h.update(field.as_bytes());
    }
    format!("{:x}", h.finalize())
}
impl Receipt {
    pub fn prepare(
        owner: String,
        thread: String,
        presentation: Presentation,
    ) -> anyhow::Result<Self> {
        let mut r = Self {
            version: 1,
            owner_id: owner,
            origin_thread_id: thread,
            presentation,
            semantic_sha256: String::new(),
            receipt_id: String::new(),
        };
        r.semantic_sha256 = r.semantic_digest();
        r.receipt_id = r.receipt_digest();
        r.validate()?;
        Ok(r)
    }
    pub fn semantic_digest(&self) -> String {
        let p = &self.presentation;
        let mut fields = vec![
            self.owner_id.as_str(),
            self.origin_thread_id.as_str(),
            p.id.as_str(),
            match p.source.kind {
                SourceKind::Agent => "agent",
                SourceKind::Service => "service",
            },
            p.source.id.as_str(),
            p.title.as_str(),
            p.body.as_str(),
            match p.format {
                Format::PlainText => "plainText",
                Format::Markdown => "markdown",
            },
        ];
        for r in &p.references {
            fields.push(match r.kind {
                ReferenceKind::ExternalInput => "externalInput",
                ReferenceKind::McpInvocation => "mcpInvocation",
            });
            fields.push(&r.id);
        }
        framed(b"codex:presentation:semantic:v1\0", &fields)
    }
    pub fn receipt_digest(&self) -> String {
        framed(
            b"codex:presentation:receipt:v1\0",
            &[
                &self.owner_id,
                &self.origin_thread_id,
                &self.presentation.id,
                &self.semantic_sha256,
            ],
        )
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(self.version == 1, "unsupported presentation version");
        for id in [
            &self.owner_id,
            &self.origin_thread_id,
            &self.presentation.id,
            &self.presentation.source.id,
        ] {
            ensure!(
                !id.is_empty() && id.len() <= 256,
                "presentation identity byte limit"
            );
        }
        let p = &self.presentation;
        ensure!(
            p.title.len() <= 256
                && p.body.len() <= 65536
                && (!p.title.is_empty() || !p.body.is_empty()),
            "presentation text byte limit/empty"
        );
        ensure!(
            p.references.len() <= 2
                && p.references
                    .iter()
                    .all(|r| !r.id.is_empty() && r.id.len() <= 256),
            "presentation reference limit"
        );
        ensure!(
            self.semantic_sha256 == self.semantic_digest()
                && self.receipt_id == self.receipt_digest(),
            "presentation digest conflict"
        );
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Response {
    version: u32,
    owner_id: String,
    thread_id: String,
    runtime_generation: u64,
    #[serde(deserialize_with = "required_receipt")]
    receipt: Option<Receipt>,
}
fn required_receipt<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Receipt>, D::Error> {
    Option::<Receipt>::deserialize(d)
}

/// Uses the exact same pinned occurrence and pre/post RPC fences as ingress.
/// No caller can inject an endpoint or an arbitrary native binding.
pub struct PresentationClient {
    inner: ExternalInputClient,
}
impl PresentationClient {
    pub fn connect(path: &Path, owner: &str, generation: u64) -> anyhow::Result<Self> {
        let inner = ExternalInputClient::connect(path, owner, generation)?;
        inner.require_presentation_capability()?;
        Ok(Self { inner })
    }
    pub fn binding(&self) -> &crate::launch::stock::ExternalInputBinding {
        self.inner.binding()
    }
    pub(crate) fn fence(&self) -> anyhow::Result<()> {
        self.inner.fence()
    }
    fn request(&self, expected: &Receipt, append: bool) -> anyhow::Result<Option<Receipt>> {
        expected.validate()?;
        let b = self.binding();
        ensure!(
            expected.owner_id == b.owner_id && expected.origin_thread_id == b.thread_id,
            "presentation target conflict"
        );
        let mut params = serde_json::json!({"version":1,"ownerId":b.owner_id,"threadId":b.thread_id,"runtimeGeneration":b.runtime_generation,"semanticSha256":expected.semantic_sha256});
        let method = if append {
            params["presentation"] = serde_json::to_value(&expected.presentation)?;
            "thread/presentation/append"
        } else {
            params["presentationId"] = expected.presentation.id.clone().into();
            "thread/presentation/status"
        };
        let r: Response = serde_json::from_value(self.inner.request(method, params)?)?;
        ensure!(
            r.version == 1
                && r.owner_id == b.owner_id
                && r.thread_id == b.thread_id
                && r.runtime_generation == b.runtime_generation,
            "presentation response occurrence conflict"
        );
        if let Some(receipt) = &r.receipt {
            receipt.validate()?;
            ensure!(receipt == expected, "presentation receipt conflict");
        }
        ensure!(
            !append || r.receipt.is_some(),
            "presentation append missing receipt"
        );
        Ok(r.receipt)
    }
    pub fn status(&self, expected: &Receipt) -> anyhow::Result<Option<Receipt>> {
        self.request(expected, false)
    }
    pub fn append(&self, expected: &Receipt) -> anyhow::Result<Receipt> {
        self.request(expected, true)?
            .context("presentation append missing receipt")
    }
}

/// Explicit local service policy, default absent. It cannot be set by a Job request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateJobPolicy {
    pub version: u32,
    pub recipients: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<JobTemplate>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobTemplate {
    Suppress,
    ServiceSummary,
}
impl PrivateJobPolicy {
    pub fn permits(&self, owner: &str) -> anyhow::Result<bool> {
        ensure!(
            matches!(self.version, 1 | 2)
                && (self.version != 1 || self.template.is_none())
                && !self.recipients.is_empty()
                && self.recipients.len() <= 32,
            "invalid private Job presentation policy"
        );
        for id in &self.recipients {
            crate::role_revision::CutexSessionId::new(id.clone())
                .map_err(|_| anyhow::anyhow!("invalid private presentation recipient"))?;
            let native = id
                .strip_prefix("cutex.")
                .context("private presentation recipient must be exact durable ID")?;
            ensure!(
                uuid::Uuid::parse_str(native).is_ok_and(|u| !u.is_nil() && u.to_string() == native),
                "private presentation recipient must be canonical durable ID"
            );
        }
        Ok(self.recipients.iter().any(|id| id == owner))
    }
    pub fn obligation(
        &self,
        message: &crate::agent_bus::model::AgentBusMessage,
        canonical_sha256: &str,
    ) -> anyhow::Result<Option<Obligation>> {
        if !self.permits(message.to_cutex_session_id.as_deref().unwrap_or(""))? {
            return Ok(None);
        }
        if message.control_type.as_deref() == Some(crate::agent_bus::job_completion::SCHEMA_V2) {
            crate::agent_bus::job_completion::FrozenProjection::from_message(message)?;
            ensure!(
                self.version == 2,
                "Job v2 requires presentation policy v2; legacy notice intent is not reinterpreted"
            );
            // The v2 producer has no independent summary field. Neither
            // suppress nor service_summary invents a second status notice.
            return Ok(None);
        }
        if self.version == 1 {
            return Obligation::job(message, canonical_sha256).map(Some);
        }
        match self.template.unwrap_or(JobTemplate::Suppress) {
            JobTemplate::Suppress => Ok(None),
            JobTemplate::ServiceSummary => {
                let obligation = Obligation::job_version(message, canonical_sha256, 2)?;
                // Missing/blank data is not a summary. No fallback status prose,
                // extra query, or content-similarity judgment.
                Ok((!obligation.presentation.body.trim().is_empty()).then_some(obligation))
            }
        }
    }
}

/// Frozen at canonical acceptance, even when the native recipient is offline.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Obligation {
    pub version: u32,
    pub canonical_sha256: String,
    pub presentation: Presentation,
    pub frozen: Option<Receipt>,
    pub receipt: Option<Receipt>,
    pub commit_generation: Option<u64>,
    pub last_error: Option<String>,
    #[serde(default)]
    pub next_attempt_at: i64,
}
impl Obligation {
    pub fn job(
        message: &crate::agent_bus::model::AgentBusMessage,
        canonical_sha256: &str,
    ) -> anyhow::Result<Self> {
        Self::job_version(message, canonical_sha256, 1)
    }
    pub(crate) fn job_version(
        message: &crate::agent_bus::model::AgentBusMessage,
        canonical_sha256: &str,
        version: u32,
    ) -> anyhow::Result<Self> {
        ensure!(
            matches!(version, 1 | 2),
            "unsupported Job presentation template"
        );
        use crate::agent_bus::model::{
            AgentMessageKind, JobServiceCompletionRequest, JOB_SERVICE_COMPLETION_SCHEMA,
        };
        ensure!(
            message.sender_kind == AgentMessageKind::JobServiceSystem
                && message.from == "cutex-job-service"
                && message.from_cutex_session_id.is_none()
                && message.control_type.as_deref() == Some(JOB_SERVICE_COMPLETION_SCHEMA),
            "Job presentation provenance conflict"
        );
        let m: JobServiceCompletionRequest = serde_json::from_value(
            message
                .control_payload
                .clone()
                .context("Job metadata absent")?,
        )?;
        ensure!(
            message.to_cutex_session_id.as_deref() == Some(&m.target_cutex_session_id),
            "Job presentation target conflict"
        );
        let status = serde_json::to_value(&m.terminal_status)?;
        let status = status.as_str().context("Job status shape")?;
        let id = format!(
            "p{}_{}",
            version,
            framed(
                b"cutex:job-presentation:v1\0",
                &[
                    &m.target_cutex_session_id,
                    &message.id,
                    &version.to_string()
                ]
            )
        );
        let presentation = Presentation {
            id,
            source: Source {
                kind: SourceKind::Service,
                id: "cutex-job-service".into(),
            },
            title: if version == 1 {
                "Job 终态通知"
            } else {
                "Job summary"
            }
            .into(),
            body: if version == 1 {
                format!(
                    "Job: {}\n状态：{}\n输出读取状态：未观测。\n摘要（外部数据）：{}",
                    m.job_id,
                    status,
                    m.summary.as_deref().unwrap_or("未提供")
                )
            } else {
                m.summary.unwrap_or_default()
            },
            format: Format::PlainText,
            references: vec![Reference {
                kind: ReferenceKind::ExternalInput,
                id: message.id.clone(),
            }],
        };
        Ok(Self {
            version,
            canonical_sha256: canonical_sha256.into(),
            presentation,
            frozen: None,
            receipt: None,
            commit_generation: None,
            last_error: None,
            next_attempt_at: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn vector() -> Receipt {
        Receipt::prepare(
            "owner".into(),
            "thread".into(),
            Presentation {
                id: "p".into(),
                source: Source {
                    kind: SourceKind::Service,
                    id: "s".into(),
                },
                title: String::new(),
                body: "only display 世界".into(),
                format: Format::PlainText,
                references: vec![],
            },
        )
        .unwrap()
    }
    #[test]
    fn job_policy_v2_is_explicit_and_old_serialization_unchanged() {
        let id = "cutex.11111111-1111-4111-8111-111111111111";
        let legacy = serde_json::json!({"version":1,"recipients":[id]});
        let policy: PrivateJobPolicy = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(serde_json::to_value(policy).unwrap(), legacy);
        for (version, template, valid) in [
            (1, "suppress", false),
            (1, "service_summary", false),
            (2, "suppress", true),
            (2, "service_summary", true),
            (3, "suppress", false),
        ] {
            let p: PrivateJobPolicy = serde_json::from_value(
                serde_json::json!({"version":version,"recipients":[id],"template":template}),
            )
            .unwrap();
            assert_eq!(p.permits(id).is_ok(), valid);
        }
        assert!(serde_json::from_value::<PrivateJobPolicy>(
            serde_json::json!({"version":2,"recipients":[id],"template":"automatic"})
        )
        .is_err());
    }
    #[test]
    fn native_foundation_independent_digest_vector() {
        let r = vector();
        assert_eq!(
            r.semantic_sha256,
            "4e7717dc740b4db1f097b77d23f199666a0ac0f2e3e0620f01c1322274b85e3d"
        );
        assert_eq!(
            r.receipt_id,
            "29e63d453d9e61e1b9fdb345d8697a86a18797c4c154d9bbbcc79e47bca4a8fb"
        );
        let mut bad = r.clone();
        bad.presentation.body.push('!');
        assert!(bad.validate().is_err());
        bad = r.clone();
        bad.version = 2;
        assert!(bad.validate().is_err());
        let mut json = serde_json::to_value(r).unwrap();
        json["presentation"]["source"]["kind"] = "human".into();
        assert!(serde_json::from_value::<Receipt>(json).is_err());
    }
    #[test]
    fn unicode_limits_and_unknown_fields_fail_closed() {
        let mut p = vector().presentation;
        p.body = "界".repeat(21845) + "a";
        assert!(Receipt::prepare("o".into(), "t".into(), p.clone()).is_ok());
        p.body.push('b');
        assert!(Receipt::prepare("o".into(), "t".into(), p).is_err());
        let mut json = serde_json::to_value(vector()).unwrap();
        json["runtimeGeneration"] = 1.into();
        assert!(serde_json::from_value::<Receipt>(json).is_err());
        assert!(PrivateJobPolicy {
            version: 2,
            recipients: vec![],
            template: None,
        }
        .permits("x")
        .is_err());
    }
    #[test]
    fn status_requires_explicit_null_not_omitted_receipt() {
        let mut v = serde_json::json!({"version":1,"ownerId":"owner","threadId":"thread","runtimeGeneration":1});
        assert!(serde_json::from_value::<Response>(v.clone()).is_err());
        v["receipt"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<Response>(v.clone())
            .unwrap()
            .receipt
            .is_none());
        v["caller"] = "root".into();
        assert!(serde_json::from_value::<Response>(v).is_err());
    }
    #[test]
    fn reference_order_is_semantic_and_policy_is_explicit() {
        let mut p = vector().presentation;
        p.references = vec![
            Reference {
                kind: ReferenceKind::ExternalInput,
                id: "a".into(),
            },
            Reference {
                kind: ReferenceKind::McpInvocation,
                id: "b".into(),
            },
        ];
        let first = Receipt::prepare("o".into(), "t".into(), p.clone()).unwrap();
        p.references.reverse();
        assert_ne!(
            first.semantic_sha256,
            Receipt::prepare("o".into(), "t".into(), p)
                .unwrap()
                .semantic_sha256
        );
        assert!(crate::profiles::model::CodezConfig::default()
            .private_job_presentation
            .is_none());
        let id = "cutex.11111111-1111-4111-8111-111111111111";
        let policy = PrivateJobPolicy {
            version: 1,
            recipients: vec![id.into()],
            template: None,
        };
        assert!(policy.permits(id).unwrap());
        assert!(!policy.permits("foreign").unwrap());
        assert!(serde_json::from_value::<PrivateJobPolicy>(
            serde_json::json!({"version":1,"recipients":[id],"enabled":true})
        )
        .is_err());
    }
}
