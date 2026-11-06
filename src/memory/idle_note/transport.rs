//! Controller-side middleware; no new native protocol, adapter or wake source.
use super::{best_effort, NoteStore, Pending, Snapshot};
use crate::app_server::client::{
    AppServerClientError, AppServerEvent, AppServerHandle, RequestHook,
};
use crate::app_server::external_input::{Delivery, Envelope, Message, Source, SourceKind};
use crate::app_server::protocol::{RpcResponse, RpcResponseOutcome};
use crate::session::model::CutexSessionRuntimeBackend;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Native,
    Legacy,
}

pub struct Hook {
    pub session: String,
    pub thread: String,
    pub generation: u64,
    pub route: Route,
    pub store: Arc<NoteStore>,
    pub(super) environment: Arc<dyn Environment>,
}

/// Injected in protocol tests; production always uses the host clock, current
/// project membership, direct index bytes and native-only activity projection.
pub(super) trait Environment: Send + Sync {
    fn now(&self) -> i64;
    fn activity(&self, session: &str) -> Result<Option<i64>>;
    fn snapshot(&self, session: &str) -> Result<Option<Snapshot>>;
    fn binding(&self, session: &str, thread: &str, generation: u64) -> Result<bool>;
}
struct LiveEnvironment;
impl Environment for LiveEnvironment {
    fn now(&self) -> i64 {
        chrono::Utc::now().timestamp_millis()
    }
    fn activity(&self, session: &str) -> Result<Option<i64>> {
        Ok(crate::management::v2::activity::session_activity(session)?
            .and_then(|s| s.conversation_at())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
            .map(|t| t.timestamp_millis()))
    }
    fn snapshot(&self, session: &str) -> Result<Option<Snapshot>> {
        Snapshot::current(session)
    }
    fn binding(&self, session: &str, thread: &str, generation: u64) -> Result<bool> {
        Ok(
            crate::session::store::shared_session_row(session)?.is_some_and(|r| {
                r.runtime_generation == generation && r.app_server_thread_id() == Some(thread)
            }),
        )
    }
}

fn candidate_method(method: &str) -> bool {
    matches!(
        method,
        "turn/start"
            | "dsh/input/submit"
            | "claude/input/submit"
            | "thread/inter_agent_message"
            | "thread/externalInput/submit"
    )
}

pub fn triggers_turn(message: &Value, thread: &str) -> bool {
    let method = message["method"].as_str().unwrap_or("");
    let p = &message["params"];
    if !candidate_method(method) || p["threadId"] != thread {
        return false;
    }
    match method {
        "turn/start" | "dsh/input/submit" => true,
        "thread/externalInput/submit" => p["message"]["delivery"] != "passive",
        "thread/inter_agent_message" => p["deliveryMode"] != "passive",
        "claude/input/submit" => p["metadata"]["deliveryMode"] != "passive",
        _ => false,
    }
}

impl Hook {
    fn native_envelope(&self, pending: &Pending) -> Envelope {
        let mut envelope = Envelope {
            version: 1,
            owner_id: self.session.clone(),
            thread_id: self.thread.clone(),
            runtime_generation: self.generation,
            message: Message {
                id: pending.id.clone(),
                source: Source {
                    kind: SourceKind::Service,
                    id: "cutex-memory".into(),
                },
                event_type: "memory_index_changed".into(),
                delivery: Delivery::Passive,
                text: pending.text.clone(),
            },
            view: None,
            semantic_sha256: String::new(),
        };
        envelope.semantic_sha256 = envelope.digest();
        envelope
    }
    fn legacy_params(
        &self,
        pending: &Pending,
    ) -> crate::app_server::commands::ThreadInterAgentMessageParams {
        crate::app_server::commands::ThreadInterAgentMessageParams {
            thread_id: self.thread.clone(),
            message_id: pending.id.clone(),
            author: "cutex-memory".into(),
            author_metadata: Some(
                crate::app_server::commands::ParticipantPresentationMetadata {
                    display_name: Some("cutex-memory".into()),
                    ..Default::default()
                },
            ),
            recipient: self.session.clone(),
            recipient_metadata: None,
            other_recipients: Vec::new(),
            content: pending.text.clone(),
            delivery_mode: crate::agent_bus::delivery::AgentDeliveryMode::Passive,
        }
    }
    pub fn submission(&self, pending: &Pending) -> (&'static str, Value) {
        match self.route {
            Route::Native => (
                "thread/externalInput/submit",
                serde_json::to_value(self.native_envelope(pending)).expect("serialize envelope"),
            ),
            Route::Legacy => (
                "thread/inter_agent_message",
                serde_json::to_value(self.legacy_params(pending)).expect("serialize note"),
            ),
        }
    }
    fn status(&self, handle: &AppServerHandle, pending: &Pending) -> Result<String> {
        let (method, params) = match self.route {
            Route::Native => {
                let e = self.native_envelope(pending);
                (
                    "thread/externalInput/status",
                    json!({"version":1,"ownerId":self.session,"threadId":self.thread,
                    "runtimeGeneration":self.generation,"messages":[e.key()]}),
                )
            }
            Route::Legacy => {
                let p = self.legacy_params(pending);
                let digest = crate::app_server::bus_bridge::inter_agent_semantic_sha256(&p);
                (
                    "thread/inter_agent_message/status",
                    json!({"threadId":self.thread,
                    "messages":[{"messageId":pending.id,"semanticSha256":digest}]}),
                )
            }
        };
        let response = handle.request(method, params)?;
        let expected_digest = match self.route {
            Route::Native => {
                let e = self.native_envelope(pending);
                let parsed: crate::app_server::external_input::Response =
                    serde_json::from_value(response.clone())?;
                parsed.validate(
                    &crate::launch::stock::ExternalInputBinding {
                        version: 1,
                        owner_id: self.session.clone(),
                        thread_id: self.thread.clone(),
                        runtime_generation: self.generation,
                        canonical_byte_limit: Default::default(),
                    },
                    &[e.key()],
                )?;
                e.semantic_sha256
            }
            Route::Legacy => {
                anyhow::ensure!(
                    response["schema"] == "cutex/inter-agent-delivery-status/v1"
                        && response["threadId"] == self.thread,
                    "memory note legacy status occurrence mismatch"
                );
                crate::app_server::bus_bridge::inter_agent_semantic_sha256(
                    &self.legacy_params(pending),
                )
            }
        };
        let rows = response["statuses"]
            .as_array()
            .context("memory note status has no statuses")?;
        let row = rows
            .iter()
            .find(|r| r["messageId"] == pending.id)
            .context("memory note status is missing its identity")?;
        anyhow::ensure!(
            row["semanticSha256"] == expected_digest,
            "memory note receipt digest mismatch"
        );
        let field = if self.route == Route::Native {
            "deliveryState"
        } else {
            "state"
        };
        if row[field] == "context_persisted" {
            anyhow::ensure!(
                row["receipt"]["turnId"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty()),
                "memory note has no turn receipt"
            );
        }
        Ok(row[field]
            .as_str()
            .context("memory note delivery state missing")?
            .into())
    }
    fn reconcile(&self, handle: &AppServerHandle) -> Result<Option<(Pending, String)>> {
        let Some(pending) = self.store.get(&self.session)?.and_then(|r| r.value.pending) else {
            return Ok(None);
        };
        if !self.binding_current()? {
            return Ok(None);
        }
        let status = self.status(handle, &pending)?;
        if !self.binding_current()? {
            return Ok(None);
        }
        if status == "context_persisted" {
            self.store
                .delivered(&self.session, &pending.id, self.environment.now())?;
            return Ok(None);
        }
        Ok(Some((pending, status)))
    }
    fn binding_current(&self) -> Result<bool> {
        self.environment
            .binding(&self.session, &self.thread, self.generation)
    }
    fn idle(&self, handle: &AppServerHandle, now: i64) -> Result<bool> {
        // Only when a changed index could qualify. Never read a whole history.
        let page = handle.request(
            "thread/turns/list",
            json!({"threadId":self.thread,
            "sortDirection":"desc","limit":1,"itemsView":"summary"}),
        )?;
        let turns = page["data"]
            .as_array()
            .context("memory reminder cannot establish native idle state")?;
        let Some(latest) = turns.first() else {
            return Ok(true);
        };
        if latest["status"] == "inProgress" {
            return Ok(false);
        }
        let stamp = self.store.activity(&self.session)?;
        if let Some(id) = latest["id"].as_str() {
            if stamp.as_ref().is_none_or(|s| s.turn != id) {
                // A native turn completed without this consumer observing it
                // (e.g. while management was offline). Its exact idle age is
                // unknown: conservatively start the quiet window now.
                self.store.turn_activity(&self.session, id, now, false)?;
                return Ok(false);
            }
        }
        Ok(!stamp.is_some_and(|s| s.busy))
    }
    fn before_turn(&self, handle: &AppServerHandle) -> Result<()> {
        // Reconcile first, including after daemon restart. Lost replies are not
        // permission to allocate another ID or resend a pending known input.
        let pending = self.reconcile(handle)?;
        let Some(row) = self.store.get(&self.session)? else {
            return Ok(());
        };
        let now = self.environment.now();
        let activity = self.environment.activity(&self.session)?;
        let stamp = self.store.activity(&self.session)?;
        let activity = activity
            .into_iter()
            .chain(stamp.as_ref().map(|s| s.at))
            .max();
        if !row.value.eligible(now, activity, false) {
            return Ok(());
        }
        if !self.binding_current()? {
            return Ok(());
        }
        let Some(current) = self.environment.snapshot(&self.session)? else {
            return Ok(());
        };
        if pending.is_none() && current.revision == row.value.seen.revision {
            return Ok(());
        }
        if !self.idle(handle, now)? {
            return Ok(());
        }
        if let Some((note, status)) = pending {
            if matches!(status.as_str(), "unknown" | "not_found") {
                // Same immutable identity and text; an accepted/held/uncertain
                // note is never duplicated or silently replaced with new data.
                let (method, params) = self.submission(&note);
                let _ = handle.request(method, params)?;
            }
            return Ok(());
        }
        if let Some(note) = self
            .store
            .reserve(&self.session, current, now, activity, false)?
        {
            let (method, params) = self.submission(&note);
            let _ = handle.request(method, params)?;
        }
        Ok(())
    }
}

impl RequestHook for Hook {
    fn before(&self, handle: &AppServerHandle, message: &Value) {
        if triggers_turn(message, &self.thread) {
            best_effort(&self.session, || self.before_turn(handle));
        }
    }
    fn after(
        &self,
        handle: &AppServerHandle,
        method: &str,
        response: &Result<RpcResponse, AppServerClientError>,
    ) {
        if candidate_method(method)
            && response
                .as_ref()
                .is_ok_and(|r| matches!(r.outcome, RpcResponseOutcome::Result(_)))
        {
            best_effort(&self.session, || {
                self.reconcile(handle)?;
                Ok(())
            });
        }
    }
    fn observe(&self, handle: &AppServerHandle, event: &AppServerEvent) {
        let AppServerEvent::Notification(n) = event else {
            return;
        };
        let Some(p) = &n.params else { return };
        if p["threadId"].as_str().is_some_and(|id| id != self.thread) {
            return;
        }
        let compacted = n.method == "thread/compacted"
            || (n.method == "item/completed" && p["item"]["type"] == "contextCompaction");
        let memory_status = n.method == "thread/externalInput/statusChanged"
            && p["messageId"]
                .as_str()
                .is_some_and(|id| id.starts_with("memory-index-"));
        if !compacted
            && !memory_status
            && !matches!(n.method.as_str(), "turn/started" | "turn/completed")
        {
            return;
        }
        best_effort(&self.session, || {
            if !self.binding_current()? {
                return Ok(());
            }
            let now = self.environment.now();
            if matches!(n.method.as_str(), "turn/started" | "turn/completed") {
                if let Some(turn) = p.pointer("/turn/id").and_then(Value::as_str) {
                    self.store.turn_activity(
                        &self.session,
                        turn,
                        now,
                        n.method == "turn/started",
                    )?;
                }
            }
            if compacted {
                self.store.compacted(&self.session, now)?;
            }
            if n.method == "turn/completed" || memory_status {
                self.reconcile(handle)?;
            }
            Ok(())
        });
    }
}

pub fn install(handle: &AppServerHandle, session: &str, thread: &str, generation: u64) {
    best_effort(session, || {
        let Some(snapshot) = Snapshot::current(session)? else {
            return Ok(());
        };
        let record = crate::session::store::shared_session_row(session)?
            .context("memory session missing")?;
        let route = if matches!(
            record.runtime_backend,
            CutexSessionRuntimeBackend::Dsh | CutexSessionRuntimeBackend::Kimi
        ) {
            Route::Legacy
        } else if handle
            .initialize_response()
            .is_some_and(|i| i["externalInputVersion"] == 1)
        {
            Route::Native
        } else {
            return Ok(());
        };
        let store = Arc::new(NoteStore::open(&crate::config::paths::config_dir()?)?);
        store.seed_if_absent(session, snapshot, chrono::Utc::now().timestamp_millis())?;
        handle.install_request_hook(Arc::new(Hook {
            session: session.into(),
            thread: thread.into(),
            generation,
            route,
            store,
            environment: Arc::new(LiveEnvironment),
        }));
        Ok(())
    });
}
