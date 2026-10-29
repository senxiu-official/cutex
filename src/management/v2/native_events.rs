use chrono::Utc;
use serde_json::json;

use crate::app_server::client::AppServerEvent;
use crate::app_server::protocol::correlations;

use super::model::AppServerSchema;
use super::model::CutexMessage;
use super::model::EventCorrelation;
use super::model::EventSource;
use super::model::NativeMessage;
use super::model::NativeMessageKind;
use super::model::PendingEvent;

/// Pure method recognition. Callers must separately verify the native's
/// fullTextItemTerminal capability before removing legacy journal semantics.
/// Payload validity must not make an enabled live notification fall back to disk.
pub fn is_live_text_delta_method(method: &str) -> bool {
    matches!(method, "item/agentMessage/delta" | "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta")
}

pub fn is_live_text_delta(event: &AppServerEvent) -> bool {
    matches!(event, AppServerEvent::Notification(n) if is_live_text_delta_method(&n.method))
}

/// Validate presentation routing without retaining unresolved foreign payloads
/// or performing a lineage RPC. The bool identifies the bound thread; verified
/// descendants may display text but never update their parent's activity.
pub fn live_text_pending(
    context: &NativeEventContext,
    event: &AppServerEvent,
    verified_descendant: impl FnOnce(&str) -> bool,
) -> anyhow::Result<Option<(PendingEvent, bool)>> {
    if !is_live_text_delta(event) || context.runtime_generation == 0
        || context.schema.capabilities.pointer("/features/fullTextItemTerminal") != Some(&serde_json::Value::Bool(true)) {
        return Ok(None);
    }
    let AppServerEvent::Notification(notification) = event else { return Ok(None) };
    let correlation = correlations(&notification.raw);
    let Some(thread) = correlation.thread_id.as_deref().filter(|id| !id.is_empty()) else { return Ok(None) };
    let bound = thread == context.thread_id;
    if !bound && !verified_descendant(thread) { return Ok(None); }
    if notification.raw.pointer("/params/delta").and_then(serde_json::Value::as_str).is_none()
        || correlation.item_id.as_deref().is_none_or(str::is_empty) {
        return Ok(None);
    }
    let pending = match pending_event_from_app_server(context, event)? {
        NativeEventDisposition::Publish(pending) | NativeEventDisposition::OtherThread { pending, .. } => pending,
    };
    Ok(Some((pending, bound)))
}

#[derive(Debug, Clone)]
pub struct NativeEventContext {
    pub cutex_session_id: String,
    pub thread_id: String,
    pub host_id: String,
    pub runtime_generation: u64,
    pub runtime_backend: String,
    pub schema: AppServerSchema,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NativeEventDisposition {
    Publish(PendingEvent),
    /// A native notification or server request addressed to a thread other
    /// than the bound one (for example a Codex sub-agent thread). The caller
    /// decides from verified sub-agent lineage whether to publish it; the
    /// native `threadId` stays intact in `pending`.
    OtherThread {
        thread_id: String,
        pending: PendingEvent,
    },
}

pub fn pending_event_from_app_server(
    context: &NativeEventContext,
    event: &AppServerEvent,
) -> anyhow::Result<NativeEventDisposition> {
    let mut pending = match event {
        AppServerEvent::Notification(notification) => PendingEvent {
            cutex_session_id: context.cutex_session_id.clone(),
            host_id: context.host_id.clone(),
            source: EventSource::AppServer,
            schema: Some(context.schema.clone()),
            correlation: correlation_for_raw(&notification.raw, context.runtime_generation, None),
            native: Some(NativeMessage {
                kind: NativeMessageKind::Notification,
                message: notification.raw.clone(),
            }),
            cutex: None,
        },
        AppServerEvent::ServerRequest(request) => PendingEvent {
            cutex_session_id: context.cutex_session_id.clone(),
            host_id: context.host_id.clone(),
            source: EventSource::AppServer,
            schema: Some(context.schema.clone()),
            correlation: correlation_for_raw(
                &request.raw,
                context.runtime_generation,
                Some(request.id.clone()),
            ),
            native: Some(NativeMessage {
                kind: NativeMessageKind::ServerRequest,
                message: request.raw.clone(),
            }),
            cutex: None,
        },
        AppServerEvent::ProtocolViolation { message } => PendingEvent {
            cutex_session_id: context.cutex_session_id.clone(),
            host_id: context.host_id.clone(),
            source: EventSource::Cutex,
            schema: None,
            correlation: EventCorrelation::default(),
            native: None,
            cutex: Some(CutexMessage {
                method: "cutex/runtime/protocolViolation".to_string(),
                params: json!({
                    "code": "invalid_native_message",
                    "message": message,
                    "nativeEventId": null,
                    "resyncRequired": true,
                    "detectedAt": Utc::now().to_rfc3339(),
                }),
            }),
        },
        AppServerEvent::Disconnected { reason } => PendingEvent {
            cutex_session_id: context.cutex_session_id.clone(),
            host_id: context.host_id.clone(),
            source: EventSource::Cutex,
            schema: None,
            correlation: EventCorrelation::default(),
            native: None,
            cutex: Some(CutexMessage {
                method: "cutex/runtime/disconnected".to_string(),
                params: json!({
                    "runtimeGeneration": context.runtime_generation,
                    "backend": context.runtime_backend,
                    "status": "error",
                    "runtimeAgentId": null,
                    "reason": reason,
                    "occurredAt": Utc::now().to_rfc3339(),
                    "error": {
                        "source": "cutex",
                        "code": "app_server_disconnected",
                        "message": reason,
                        "retryable": true,
                        "details": {},
                    }
                }),
            }),
        },
    };
    if matches!(event, AppServerEvent::Disconnected { .. }) {
        use crate::session::runtime_stop::{state, StopState};
        if let Some(StopState::Offline) =
            state(&context.cutex_session_id, context.runtime_generation)
        {
            let message = pending.cutex.as_mut().expect("disconnect cutex event");
            message.method = "cutex/runtime/offline".into();
            let params = &mut message.params;
            params["status"] = json!("offline");
            params.as_object_mut().expect("runtime event params").remove("error");
        }
    }
    pending.validate()?;

    let thread_id = validated_thread_correlation(&pending)?;
    if let Some(thread_id) = thread_id {
        if thread_id != context.thread_id {
            let thread_id = thread_id.to_string();
            return Ok(NativeEventDisposition::OtherThread { thread_id, pending });
        }
    }
    Ok(NativeEventDisposition::Publish(pending))
}

/// A closing snapshot is exposed only after durable observation registration.
/// Setup/persistence failure propagates; overflow is never silently dropped.
pub fn append_observed_event(pending: PendingEvent) -> anyhow::Result<Option<super::model::EventEnvelope>> {
    use crate::session::runtime_stop::{state, StopState};
    pending.validate()?;
    let generation = pending.cutex.as_ref()
        .filter(|m| m.method == "cutex/runtime/disconnected")
        .and_then(|m| m.params["runtimeGeneration"].as_u64());
    if let Some(generation) = generation {
        if state(&pending.cutex_session_id, generation) == Some(StopState::Closing) {
            stop_observer()?.register(&pending, generation)?;
            return Ok(None);
        }
    }
    let mut pending = pending;
    if let Some(generation) = generation { resolve_stop_terminal(&mut pending, generation); }
    super::repository::management_v2_repository()?.append(pending).map(Some)
}

// One worker, fixed-size batches and SQLite cache. Overflow stays on disk.
const STOP_OBSERVER_BATCH: usize = 32;
const STOP_OBSERVER_TICK: std::time::Duration = std::time::Duration::from_millis(250);
const STOP_OBSERVER_RECHECK_MS: i64 = 1000;

struct StopObserverQueue {
    connection: std::sync::Mutex<rusqlite::Connection>,
    worker_lock: std::path::PathBuf,
}

impl StopObserverQueue {
    fn open(path: &std::path::Path) -> anyhow::Result<Self> {
        crate::profiles::materialize::create_private_dir_all(path.parent().expect("observer parent"))?;
        let connection = rusqlite::Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA cache_size=-256;
            CREATE TABLE IF NOT EXISTS pending_stop (
                id INTEGER PRIMARY KEY, session TEXT NOT NULL, generation TEXT NOT NULL,
                receipt_path TEXT NOT NULL, host TEXT NOT NULL, message TEXT NOT NULL,
                fallback INTEGER NOT NULL, due INTEGER NOT NULL DEFAULT 0,
                UNIQUE(session,generation));
            CREATE INDEX IF NOT EXISTS pending_stop_due ON pending_stop(due,id);")?;
        Ok(Self { connection: std::sync::Mutex::new(connection), worker_lock: path.with_extension("worker.lock") })
    }

    fn register(&self, pending: &PendingEvent, generation: u64) -> anyhow::Result<()> {
        let message = pending.cutex.as_ref().expect("disconnect event");
        let fallback = message.params["error"]["code"] == "runtime_stop_unconfirmed";
        let path = crate::session::runtime_stop::path(&pending.cutex_session_id, generation)?;
        self.connection.lock().unwrap_or_else(std::sync::PoisonError::into_inner).execute(
            "INSERT INTO pending_stop(session,generation,receipt_path,host,message,fallback)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(session,generation) DO UPDATE SET
               message=CASE WHEN pending_stop.fallback=1 THEN excluded.message ELSE pending_stop.message END,
               fallback=MIN(pending_stop.fallback,excluded.fallback)",
            rusqlite::params![pending.cutex_session_id, generation.to_string(),
                serde_json::to_string(&path)?, pending.host_id, serde_json::to_string(message)?, fallback])?;
        Ok(())
    }

    fn tick(&self, repository: &super::repository::EventRepository, now: i64) -> anyhow::Result<usize> {
        use fs2::FileExt;
        // Cross-process leader for this batch only. No sleeps under locks.
        let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
            .open(&self.worker_lock)?;
        match lock.try_lock_exclusive() {
            Ok(()) => {},
            Err(e) if e.raw_os_error() == fs2::lock_contended_error().raw_os_error() => return Ok(0),
            Err(e) => return Err(e.into()),
        }
        let rows = {
            let connection = self.connection.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut statement = connection.prepare(
                "SELECT id,session,generation,receipt_path FROM pending_stop WHERE due<=?1 ORDER BY due,id LIMIT ?2")?;
            let rows = statement.query_map(rusqlite::params![now, STOP_OBSERVER_BATCH as i64], |row|
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?, row.get::<_, String>(3)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let count = rows.len();
        for (id, session, generation, path) in rows {
            let generation = generation.parse::<u64>()?;
            let path: std::path::PathBuf = serde_json::from_str(&path)?;
            // Receipt IO outside queue mutex, retry after a second, not 25ms per session.
            let state = crate::session::runtime_stop::state_at(&path, &session, generation);
            let mut connection = self.connection.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if state == Some(crate::session::runtime_stop::StopState::Closing) {
                connection.execute("UPDATE pending_stop SET due=?1 WHERE id=?2",
                    rusqlite::params![now + STOP_OBSERVER_RECHECK_MS, id])?;
                continue;
            }
            let transaction = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            // Registration racing receipt IO may replace fallback with real diagnostic.
            let (host, message): (String, String) = transaction.query_row(
                "SELECT host,message FROM pending_stop WHERE id=?1", [id], |row| Ok((row.get(0)?, row.get(1)?)))?;
            let mut message: CutexMessage = serde_json::from_str(&message)?;
            if state == Some(crate::session::runtime_stop::StopState::Offline) {
                message.method = "cutex/runtime/offline".into();
                message.params["status"] = json!("offline");
                message.params.as_object_mut().expect("runtime event params").remove("error");
            }
            let pending = PendingEvent { cutex_session_id: session, host_id: host,
                source: EventSource::Cutex, schema: None, native: None,
                correlation: EventCorrelation::default(), cutex: Some(message) };
            match repository.append(pending) {
                Ok(_) => { transaction.execute("DELETE FROM pending_stop WHERE id=?1", [id])?; },
                Err(error) => {
                    transaction.execute("UPDATE pending_stop SET due=?1 WHERE id=?2",
                        rusqlite::params![now + STOP_OBSERVER_RECHECK_MS, id])?;
                    eprintln!("runtime stop terminal event persistence will retry: {error:#}");
                }
            }
            transaction.commit()?;
        }
        Ok(count)
    }
}

/// Management startup must resume durable work even when no new snapshot or
/// transport observes Closing. Uses the same process singleton as registration.
pub fn start_stop_observer() -> anyhow::Result<()> {
    stop_observer().map(|_| ())
}

#[cfg(test)]
static STOP_OBSERVER_STARTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn stop_observer() -> anyhow::Result<&'static std::sync::Arc<StopObserverQueue>> {
    static OBSERVER: std::sync::Mutex<Option<&'static std::sync::Arc<StopObserverQueue>>> = std::sync::Mutex::new(None);
    let mut slot = OBSERVER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(observer) = *slot { return Ok(observer); }
    let repository = super::repository::management_v2_repository()?;
    let observer = std::sync::Arc::new(StopObserverQueue::open(&repository.root().join("pending-stops.sqlite"))?);
    let worker = observer.clone();
    std::thread::Builder::new().name("runtime-stop-observer".into()).spawn(move || loop {
        if let Err(error) = worker.tick(repository, Utc::now().timestamp_millis()) {
            eprintln!("runtime stop observer will retry durable backlog: {error:#}");
        }
        std::thread::sleep(STOP_OBSERVER_TICK);
    })?;
    #[cfg(test)]
    STOP_OBSERVER_STARTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let observer = Box::leak(Box::new(observer));
    *slot = Some(observer);
    Ok(observer)
}

/// Any snapshot which exposes closing also arranges its terminal event, even
/// when this process never had a transport connection to that runtime.
pub fn observe_stop_from_snapshot(session: &str, generation: u64, backend: &str,
    last_error: Option<&str>) -> anyhow::Result<()> {
    append_observed_event(PendingEvent {
        cutex_session_id: session.into(), host_id: crate::platform::host::current_host_name(),
        source: EventSource::Cutex, schema: None, native: None,
        correlation: EventCorrelation::default(),
        cutex: Some(CutexMessage { method: "cutex/runtime/disconnected".into(), params: json!({
            "runtimeGeneration": generation, "backend": backend, "status": "error",
            "runtimeAgentId": null, "reason": "requested stop outcome unconfirmed",
            "occurredAt": Utc::now().to_rfc3339(), "error": {
                "source": "cutex", "code": "runtime_stop_unconfirmed",
                "message": last_error.unwrap_or("stop owner exited without confirmed process stop"),
                "retryable": true, "details": {}
            }
        }) }),
    }).map(drop)
}

fn resolve_stop_terminal(pending: &mut PendingEvent, generation: u64) {
    if crate::session::runtime_stop::state(&pending.cutex_session_id, generation)
        == Some(crate::session::runtime_stop::StopState::Offline) {
        let message = pending.cutex.as_mut().expect("transport event");
        message.method = "cutex/runtime/offline".into();
        message.params["status"] = json!("offline");
        message.params.as_object_mut().expect("runtime event params").remove("error");
    }
}

#[cfg(test)]
pub(crate) fn stop_test_events(session: &str) -> Vec<super::model::EventEnvelope> {
    let mut query = super::repository::ReplayQuery {
        limit: 1000, cutex_session_id: Some(session.into()), ..Default::default()
    };
    let mut events = Vec::new();
    loop {
        let page = super::repository::management_v2_repository().unwrap().page(query.clone()).unwrap();
        events.extend(page.events);
        if !page.has_more { return events; }
        query.stream_id = Some(page.stream_id);
        query.after = page.next_cursor;
    }
}

fn validated_thread_correlation(pending: &PendingEvent) -> anyhow::Result<Option<&str>> {
    let Some(native) = pending.native.as_ref() else {
        return Ok(pending.correlation.thread_id.as_deref());
    };
    let params = native.message.get("params").unwrap_or(&native.message);
    let Some(params) = params.as_object() else {
        return Ok(pending.correlation.thread_id.as_deref());
    };

    let mut thread_id = None;
    for field in ["threadId", "thread_id"] {
        let Some(value) = params.get(field) else {
            continue;
        };
        let value = value
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("native event has an invalid {field} correlation"))?;
        if let Some(previous) = thread_id {
            if previous != value {
                anyhow::bail!("native event has conflicting thread correlations");
            }
        }
        thread_id = Some(value);
    }

    if thread_id != pending.correlation.thread_id.as_deref() {
        anyhow::bail!("native event thread correlation could not be validated");
    }
    Ok(thread_id)
}

fn correlation_for_raw(
    raw: &serde_json::Value,
    runtime_generation: u64,
    native_request_id: Option<serde_json::Value>,
) -> EventCorrelation {
    let ids = correlations(raw);
    EventCorrelation {
        runtime_generation: Some(runtime_generation),
        thread_id: ids.thread_id,
        turn_id: ids.turn_id,
        item_id: ids.item_id,
        client_user_message_id: ids.client_user_message_id,
        native_request_id,
        ..EventCorrelation::default()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::app_server::protocol::RpcNotification;
    use crate::app_server::protocol::RpcServerRequest;
    use crate::management::v2::model::AppServerSchemaChannel;

    fn context() -> NativeEventContext {
        NativeEventContext {
            cutex_session_id: "cutex.session-1".to_string(),
            thread_id: "thread-1".to_string(),
            host_id: "devbox".to_string(),
            runtime_generation: 9,
            runtime_backend: "cute_alden".to_string(),
            schema: AppServerSchema {
                protocol: "codex-app-server".to_string(),
                major_version: 2,
                version: "0.144.1+cutex-inter-agent-v2".to_string(),
                sha256: "a".repeat(64),
                channel: AppServerSchemaChannel::Experimental,
                capabilities: json!({ "experimentalApi": true, "features":{"fullTextItemTerminal":true} }),
                extensions: vec!["cutex-inter-agent-v2".to_string()],
            },
        }
    }

    #[test]
    fn live_text_routing_never_buffers_unknown_threads_and_keeps_native_indices() {
        for method in ["item/agentMessage/delta", "item/reasoning/textDelta", "item/reasoning/summaryTextDelta"] {
            let raw = json!({"method":method,"params":{"threadId":"thread-1","turnId":"turn-1","itemId":"item-1","delta":"汉🙂","contentIndex":2,"summaryIndex":3,"future":{"x":true}}});
            let mut event = AppServerEvent::Notification(RpcNotification { method: method.into(), params: raw.get("params").cloned(), raw: raw.clone() });
            let (pending, bound) = live_text_pending(&context(), &event, |_| panic!("bound thread needs no lineage query")).unwrap().unwrap();
            assert!(bound);
            assert_eq!(pending.native.unwrap().message, raw);
            if let AppServerEvent::Notification(n) = &mut event { n.raw["params"]["threadId"] = json!("foreign"); }
            assert!(live_text_pending(&context(), &event, |_| false).unwrap().is_none());
            let (_, bound) = live_text_pending(&context(), &event, |id| id == "foreign").unwrap().unwrap();
            assert!(!bound);
            if let AppServerEvent::Notification(n) = &mut event { n.raw["params"].as_object_mut().unwrap().remove("threadId"); }
            assert!(live_text_pending(&context(), &event, |_| true).unwrap().is_none());
        }
    }

    #[test]
    fn live_only_allowlist_does_not_classify_requests_or_other_delta_types() {
        let request = AppServerEvent::ServerRequest(RpcServerRequest { id: json!(1), method: "item/agentMessage/delta".into(), params: None, raw: json!({"id":1,"method":"item/agentMessage/delta"}) });
        assert!(!is_live_text_delta(&request));
        assert!(!is_live_text_delta_method("item/commandExecution/outputDelta"));
        let raw = json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-1","itemId":"i","delta":5}});
        let event = AppServerEvent::Notification(RpcNotification {method:"item/agentMessage/delta".into(),params:raw.get("params").cloned(),raw});
        assert!(is_live_text_delta(&event), "malformed live payload must not fall back to durable");
        assert!(live_text_pending(&context(), &event, |_| false).unwrap().is_none());
    }

    #[test]
    fn live_text_requires_exact_capability_and_legacy_conversion_remains_durable() {
        let root = std::env::temp_dir().join(format!("native-live-gate-{}", uuid::Uuid::new_v4()));
        let repository = super::super::repository::EventRepository::open(&root, "devbox").unwrap();
        let raw = json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-1","turnId":"turn-1","itemId":"item-1","delta":"legacy text"}});
        let event = AppServerEvent::Notification(RpcNotification {method:"item/agentMessage/delta".into(),params:raw.get("params").cloned(),raw:raw.clone()});
        for feature in [None, Some(json!(false)), Some(json!("true")), Some(json!(1))] {
            let mut context = context();
            context.schema.capabilities = json!({});
            if let Some(feature) = feature { context.schema.capabilities["features"] = json!({"fullTextItemTerminal":feature}); }
            assert!(live_text_pending(&context, &event, |_| false).unwrap().is_none());
            let NativeEventDisposition::Publish(pending) = pending_event_from_app_server(&context, &event).unwrap() else { panic!("bound legacy event"); };
            assert_eq!(repository.append(pending).unwrap().native.unwrap().message, raw);
        }
        let (pending, _) = live_text_pending(&context(), &event, |_| false).unwrap().unwrap();
        assert!(repository.append(pending).is_err(), "negotiated live-only text cannot fall back into the journal");
        drop(repository);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unknown_notification_is_preserved_without_projection() {
        let raw = json!({
            "method": "future/native/event",
            "params": {
                "threadId": "thread-1",
                "turnId": "turn-1",
                "item": {
                    "id": "item-1",
                    "clientId": "client-message-1",
                    "explicitNull": null,
                    "future": { "mustSurvive": true }
                }
            },
            "futureTopLevel": null
        });
        let disposition = pending_event_from_app_server(
            &context(),
            &AppServerEvent::Notification(RpcNotification {
                method: "future/native/event".to_string(),
                params: raw.get("params").cloned(),
                raw: raw.clone(),
            }),
        )
        .expect("convert notification");
        let NativeEventDisposition::Publish(pending) = disposition else {
            panic!("root notification must publish");
        };
        assert_eq!(pending.native.unwrap().message, raw);
        assert_eq!(pending.correlation.thread_id.as_deref(), Some("thread-1"));
        assert_eq!(
            pending.correlation.client_user_message_id.as_deref(),
            Some("client-message-1")
        );
    }

    #[test]
    fn thread_reverted_notification_is_published_to_the_session_stream() {
        let raw = json!({ "method": "thread/reverted", "params": { "threadId": "thread-1" } });
        let disposition = pending_event_from_app_server(
            &context(),
            &AppServerEvent::Notification(RpcNotification {
                method: "thread/reverted".to_string(),
                params: raw.get("params").cloned(),
                raw: raw.clone(),
            }),
        )
        .expect("convert thread/reverted");
        let NativeEventDisposition::Publish(pending) = disposition else {
            panic!("thread/reverted for the bound thread must publish");
        };
        assert_eq!(pending.native.unwrap().message, raw);
        assert_eq!(pending.correlation.thread_id.as_deref(), Some("thread-1"));
    }

    #[test]
    fn server_request_keeps_full_signed_id_domain() {
        let id = json!(-9_223_372_036_854_775_808_i64);
        let raw = json!({
            "id": id,
            "method": "future/native/request",
            "params": { "threadId": "thread-1", "explicitNull": null }
        });
        let disposition = pending_event_from_app_server(
            &context(),
            &AppServerEvent::ServerRequest(RpcServerRequest {
                id: id.clone(),
                method: "future/native/request".to_string(),
                params: raw.get("params").cloned(),
                raw: raw.clone(),
            }),
        )
        .expect("convert server request");
        let NativeEventDisposition::Publish(pending) = disposition else {
            panic!("root server request must publish");
        };
        assert_eq!(pending.correlation.native_request_id, Some(id));
        assert_eq!(pending.correlation.runtime_generation, Some(9));
        assert_eq!(pending.native.unwrap().message, raw);
    }

    #[test]
    fn foreign_thread_notification_is_explicitly_ignored_after_validation() {
        let raw = json!({
            "method": "item/completed",
            "params": {
                "threadId": "foreign-thread",
                "turnId": "turn-1",
                "item": { "type": "agentMessage", "id": "item-1", "text": "no" }
            }
        });
        let disposition = pending_event_from_app_server(
            &context(),
            &AppServerEvent::Notification(RpcNotification {
                method: "item/completed".to_string(),
                params: raw.get("params").cloned(),
                raw,
            }),
        )
        .expect("well-formed foreign notification");
        let NativeEventDisposition::OtherThread { thread_id, pending } = disposition else {
            panic!("other-thread notification must be handed to the lineage router");
        };
        assert_eq!(thread_id, "foreign-thread");
        assert_eq!(pending.correlation.thread_id.as_deref(), Some("foreign-thread"));
        assert_eq!(
            pending.native.unwrap().message["params"]["threadId"],
            "foreign-thread"
        );
    }

    #[test]
    fn foreign_thread_server_request_is_handed_to_the_lineage_router() {
        let raw = json!({
            "id": "request-1",
            "method": "future/native/request",
            "params": { "threadId": "foreign-thread" }
        });
        let result = pending_event_from_app_server(
            &context(),
            &AppServerEvent::ServerRequest(RpcServerRequest {
                id: json!("request-1"),
                method: "future/native/request".to_string(),
                params: raw.get("params").cloned(),
                raw,
            }),
        );
        assert!(matches!(
            result,
            Ok(NativeEventDisposition::OtherThread { ref thread_id, .. })
                if thread_id == "foreign-thread"
        ));
    }

    #[test]
    fn malformed_foreign_thread_correlation_is_rejected_before_filtering() {
        let raw = json!({
            "method": "item/completed",
            "params": {
                "threadId": 7,
                "item": { "type": "agentMessage", "id": "item-1" }
            }
        });
        let result = pending_event_from_app_server(
            &context(),
            &AppServerEvent::Notification(RpcNotification {
                method: "item/completed".to_string(),
                params: raw.get("params").cloned(),
                raw,
            }),
        );
        assert!(result.is_err());
    }

    #[test]
    fn requested_disconnect_tracks_stop_success_without_hiding_unexpected_resets() {
        let _home = crate::config::paths::test_home_env_lock();
        use crate::session::runtime_stop::RequestedStop;
        let mut context = context();
        context.cutex_session_id = format!("cutex.stop-{}", uuid::Uuid::new_v4());
        context.runtime_generation = 7;
        let event = AppServerEvent::Disconnected { reason: "connection reset (os error 10054)".into() };
        let status = |context: &NativeEventContext| {
            let NativeEventDisposition::Publish(pending) = pending_event_from_app_server(context, &event).unwrap()
                else { panic!("disconnect must publish") };
            pending.cutex.unwrap().params
        };
        assert_eq!(status(&context)["status"], "error");
        let stop = RequestedStop::begin(&context.cutex_session_id, 7).unwrap();
        // Conversion retains the raw disconnect until append_observed_event
        // can defer it. A pending stop is not yet evidence of success.
        assert_eq!(status(&context)["status"], "error");
        stop.finish(true).unwrap();
        assert_eq!(status(&context)["status"], "offline");
        context.runtime_generation = 8;
        assert_eq!(status(&context)["status"], "error");
        let stop = RequestedStop::begin(&context.cutex_session_id, 8).unwrap();
        stop.finish(false).unwrap();
        assert_eq!(status(&context)["error"]["code"], "app_server_disconnected");
    }

    #[test]
    fn stop_observer_publishes_terminal_after_success_failure_and_conversion_race() {
        let _home = crate::config::paths::test_home_env_lock();
        use crate::session::runtime_stop::RequestedStop;
        for (success, finish_before_append) in [(true, false), (false, false), (true, true)] {
            let mut context = context();
            context.host_id = crate::platform::host::current_host_name();
            context.cutex_session_id = format!("cutex.stop-events-{}", uuid::Uuid::new_v4());
            context.runtime_generation = 12;
            let stop = RequestedStop::begin(&context.cutex_session_id, 12).unwrap();
            let NativeEventDisposition::Publish(pending) = pending_event_from_app_server(&context,
                &AppServerEvent::Disconnected { reason: "transport ended".into() }).unwrap()
                else { panic!("transport event") };
            if finish_before_append {
                stop.finish(success).unwrap();
                let envelope = append_observed_event(pending).unwrap().unwrap();
                assert_eq!(envelope.cutex.unwrap().method, "cutex/runtime/offline");
            } else {
                observe_stop_from_snapshot(&context.cutex_session_id, 12, &context.runtime_backend, None).unwrap();
                observe_stop_from_snapshot(&context.cutex_session_id, 12, &context.runtime_backend, None).unwrap();
                assert!(append_observed_event(pending).unwrap().is_none());
                if success { stop.finish(true).unwrap(); } else { drop(stop); }
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let events = stop_test_events(&context.cutex_session_id);
                if let Some(event) = events.first() {
                    assert_eq!(events.len(), 1);
                    let message = event.cutex.as_ref().unwrap();
                    assert_eq!(message.method, if success { "cutex/runtime/offline" } else { "cutex/runtime/disconnected" });
                    assert_eq!(message.params["runtimeGeneration"], 12);
                    assert_eq!(message.params.get("error").is_some(), !success);
                    if !success { assert_eq!(message.params["error"]["code"], "app_server_disconnected"); }
                    break;
                }
                assert!(std::time::Instant::now() < deadline, "missing terminal event");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }

    #[test]
    fn bounded_stop_observer_spills_reopens_and_drains_every_obligation() {
        let _home = crate::config::paths::test_home_env_lock();
        use crate::session::runtime_stop::{RequestedStop, StopState};
        let root = std::env::temp_dir().join(format!("stop-backlog-{}", uuid::Uuid::new_v4()));
        let repository = super::super::repository::EventRepository::open(root.join("events"),
            crate::platform::host::current_host_name()).unwrap();
        let path = root.join("queue.sqlite");
        let queue = StopObserverQueue::open(&path).unwrap();
        let mut stops = Vec::new();
        let total = STOP_OBSERVER_BATCH * 3 + 1;
        for index in 0..total {
            let mut context = context();
            context.host_id = crate::platform::host::current_host_name();
            context.cutex_session_id = format!("cutex.backlog-{index}-{}", uuid::Uuid::new_v4());
            context.runtime_generation = 12;
            let guard = RequestedStop::begin(&context.cutex_session_id, 12).unwrap();
            let NativeEventDisposition::Publish(pending) = pending_event_from_app_server(&context,
                &AppServerEvent::Disconnected { reason: "actual transport error".into() }).unwrap()
                else { panic!("disconnect") };
            queue.register(&pending, 12).unwrap();
            queue.register(&pending, 12).unwrap();
            stops.push((context.cutex_session_id, guard));
        }
        let count = |queue: &StopObserverQueue| -> usize {
            queue.connection.lock().unwrap().query_row("SELECT count(*) FROM pending_stop", [], |r| r.get::<_, i64>(0)).unwrap() as usize
        };
        assert_eq!(count(&queue), total, "duplicate registration is coalesced");
        for expected in [32, 32, 32, 1, 0] {
            assert_eq!(queue.tick(&repository, 0).unwrap(), expected);
        }
        assert_eq!(count(&queue), total, "pending never discarded to meet budget");
        assert!(repository.page(Default::default()).unwrap().events.is_empty());
        drop(queue);
        let queue = StopObserverQueue::open(&path).unwrap();
        assert_eq!(count(&queue), total, "overflow survives process queue reopen");
        let newest_id = stops[0].0.clone();
        let newer = RequestedStop::begin(&newest_id, 13).unwrap();
        for (index, (_, stop)) in stops.into_iter().enumerate() { stop.finish(index % 2 == 0).unwrap(); }
        for expected in [32, 32, 32, 1, 0] {
            assert_eq!(queue.tick(&repository, 1000).unwrap(), expected);
        }
        assert_eq!(count(&queue), 0);
        let page = repository.page(super::super::repository::ReplayQuery { limit: 1000, ..Default::default() }).unwrap();
        assert_eq!(page.events.len(), total);
        for (index, event) in page.events.iter().enumerate() {
            let message = event.cutex.as_ref().unwrap();
            assert_eq!(message.params["runtimeGeneration"], 12);
            assert_eq!(message.method, if index % 2 == 0 { "cutex/runtime/offline" } else { "cutex/runtime/disconnected" });
        }
        assert_eq!(crate::session::runtime_stop::state(&newest_id, 13), Some(StopState::Closing),
            "old terminal publication cannot finish newer generation");
        newer.finish(true).unwrap();
    }

    #[test]
    fn stop_observer_registration_failure_is_explicit_and_leader_contention_retains_work() {
        let _home = crate::config::paths::test_home_env_lock();
        use fs2::FileExt;
        let root = std::env::temp_dir().join(format!("stop-queue-error-{}", uuid::Uuid::new_v4()));
        let queue = StopObserverQueue::open(&root.join("queue.sqlite")).unwrap();
        let repository = super::super::repository::EventRepository::open(root.join("events"),
            crate::platform::host::current_host_name()).unwrap();
        let mut context = context();
        context.cutex_session_id = format!("cutex.queue-error-{}", uuid::Uuid::new_v4());
        context.host_id = crate::platform::host::current_host_name();
        let NativeEventDisposition::Publish(pending) = pending_event_from_app_server(&context,
            &AppServerEvent::Disconnected { reason: "error".into() }).unwrap() else { panic!() };
        queue.connection.lock().unwrap().execute_batch("PRAGMA query_only=ON").unwrap();
        assert!(queue.register(&pending, 12).is_err());
        queue.connection.lock().unwrap().execute_batch("PRAGMA query_only=OFF").unwrap();
        queue.register(&pending, 12).unwrap();
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true)
            .open(&queue.worker_lock).unwrap();
        lock.lock_exclusive().unwrap();
        assert_eq!(queue.tick(&repository, 0).unwrap(), 0);
        drop(lock);
        assert_eq!(queue.tick(&repository, 0).unwrap(), 1);
        assert_eq!(repository.page(Default::default()).unwrap().events.len(), 1);
    }

    #[test]
    fn stop_observer_startup_child() {
        let Ok(mode) = std::env::var("CUTEX_STOP_STARTUP_CHILD") else { return };
        let prefix = std::env::var("CUTEX_STOP_STARTUP_PREFIX").unwrap();
        let repository = super::super::repository::management_v2_repository().unwrap();
        if mode == "seed" {
            // Simulate the old observer process dying with durable obligations.
            // No worker drains these registrations before exit.
            let queue = StopObserverQueue::open(&repository.root().join("pending-stops.sqlite")).unwrap();
            for index in 0..3 {
                let mut context = context();
                context.host_id = crate::platform::host::current_host_name();
                context.cutex_session_id = format!("{prefix}-{index}");
                context.runtime_generation = 12;
                let stop = crate::session::runtime_stop::RequestedStop::begin(&context.cutex_session_id, 12).unwrap();
                let NativeEventDisposition::Publish(pending) = pending_event_from_app_server(&context,
                    &AppServerEvent::Disconnected { reason: "old process transport ended".into() }).unwrap()
                    else { panic!("disconnect") };
                queue.register(&pending, 12).unwrap();
                match index {
                    0 => stop.finish(true).unwrap(),
                    1 => stop.finish(false).unwrap(),
                    _ => std::mem::forget(stop), // exit releases OS lock without receipt Drop
                }
            }
            std::process::exit(23);
        }
        assert_eq!(mode, "recover");
        assert_eq!(STOP_OBSERVER_STARTS.load(std::sync::atomic::Ordering::SeqCst), 0);
        for index in 0..3 {
            let id = format!("{prefix}-{index}");
            assert_ne!(crate::session::runtime_stop::state(&id, 12),
                Some(crate::session::runtime_stop::StopState::Closing));
            assert!(stop_test_events(&id).is_empty());
        }
        // Exactly the startup hook called before management listeners; no
        // Closing read, snapshot annotation, transport append, or register call.
        crate::management::server::initialize_runtime_observation().unwrap();
        crate::management::server::initialize_runtime_observation().unwrap();
        assert_eq!(STOP_OBSERVER_STARTS.load(std::sync::atomic::Ordering::SeqCst), 1);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let events: Vec<_> = (0..3).map(|i| stop_test_events(&format!("{prefix}-{i}"))).collect();
            if events.iter().all(|events| !events.is_empty()) {
                for (index, events) in events.iter().enumerate() {
                    assert_eq!(events.len(), 1);
                    let message = events[0].cutex.as_ref().unwrap();
                    assert_eq!(message.method, if index == 0 { "cutex/runtime/offline" } else { "cutex/runtime/disconnected" });
                    assert_eq!(message.params["runtimeGeneration"], 12);
                    assert_eq!(message.params.get("error").is_some(), index != 0);
                }
                // Check disk directly, never call the lazy initializer as a test aid.
                let queue = StopObserverQueue::open(&repository.root().join("pending-stops.sqlite")).unwrap();
                let remaining: i64 = queue.connection.lock().unwrap().query_row(
                    "SELECT count(*) FROM pending_stop", [], |r| r.get(0)).unwrap();
                if remaining == 0 { break; }
            }
            assert!(std::time::Instant::now() < deadline, "startup did not drain preexisting pending");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn management_startup_recovers_old_pending_without_new_closing_or_registration() {
        let _home = crate::config::paths::test_home_env_lock();
        let home = std::path::PathBuf::from(std::env::var_os("CUTEX_TEST_PRIVATE_HOME").unwrap())
            .join(format!("observer-startup-{}", uuid::Uuid::new_v4()));
        crate::profiles::materialize::create_private_dir_all(&home).unwrap();
        std::fs::write(home.join(".cutex-test-private-home"), b"private observer recovery test").unwrap();
        let prefix = format!("cutex.startup-{}", uuid::Uuid::new_v4());
        for mode in ["seed", "recover"] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "management::v2::native_events::tests::stop_observer_startup_child", "--nocapture"])
                .env("HOME", &home).env("CUTEX_TEST_PRIVATE_HOME", &home)
                .env("CUTEX_STOP_STARTUP_CHILD", mode).env("CUTEX_STOP_STARTUP_PREFIX", &prefix)
                .spawn().unwrap();
            eprintln!("owned startup test child {mode} PID={}", child.id());
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() { break status; }
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill(); let _ = child.wait();
                    panic!("owned startup child {mode} timed out");
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            };
            assert_eq!(status.code(), Some(if mode == "seed" { 23 } else { 0 }));
        }
    }

    #[test]
    fn stop_observer_owner_child() {
        let Ok(id) = std::env::var("CUTEX_STOP_OBSERVER_CHILD") else { return };
        let signal = std::path::PathBuf::from(std::env::var_os("CUTEX_STOP_OBSERVER_SIGNAL").unwrap());
        let _guard = crate::session::runtime_stop::RequestedStop::begin(&id, 12).unwrap();
        std::fs::write(signal.with_extension("ready"), b"ready").unwrap();
        while !signal.with_extension("exit").exists() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::process::exit(23);
    }

    #[test]
    fn stop_observer_survives_stop_owner_process_death_and_emits_error() {
        let _home = crate::config::paths::test_home_env_lock();
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
        }
        let mut context = context();
        context.host_id = crate::platform::host::current_host_name();
        context.cutex_session_id = format!("cutex.stop-death-{}", uuid::Uuid::new_v4());
        context.runtime_generation = 12;
        let signal = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let mut child = Child(std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "management::v2::native_events::tests::stop_observer_owner_child", "--nocapture"])
            .env("CUTEX_STOP_OBSERVER_CHILD", &context.cutex_session_id)
            .env("CUTEX_STOP_OBSERVER_SIGNAL", &signal).spawn().unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !signal.with_extension("ready").exists() {
            assert!(std::time::Instant::now() < deadline);
            assert!(child.0.try_wait().unwrap().is_none());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // The snapshot observer alone must converge; no transport event exists.
        observe_stop_from_snapshot(&context.cutex_session_id, 12, &context.runtime_backend, None).unwrap();
        std::fs::write(signal.with_extension("exit"), b"exit").unwrap();
        assert_eq!(child.0.wait().unwrap().code(), Some(23));
        loop {
            let events = stop_test_events(&context.cutex_session_id);
            if let Some(event) = events.first() {
                assert_eq!(events.len(), 1);
                let message = event.cutex.as_ref().unwrap();
                assert_eq!(message.method, "cutex/runtime/disconnected");
                assert!(message.params.get("error").is_some());
                assert_eq!(message.params["error"]["code"], "runtime_stop_unconfirmed");
                assert_eq!(crate::session::runtime_stop::state(&context.cutex_session_id, 12), None);
                break;
            }
            assert!(std::time::Instant::now() < deadline, "owner death left closing without terminal event");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn disconnect_is_namespaced_cutex_state_not_a_native_item() {
        let disposition = pending_event_from_app_server(
            &context(),
            &AppServerEvent::Disconnected {
                reason: "socket closed".to_string(),
            },
        )
        .expect("convert disconnect");
        let NativeEventDisposition::Publish(pending) = disposition else {
            panic!("disconnect must publish");
        };
        assert_eq!(pending.source, EventSource::Cutex);
        assert!(pending.native.is_none());
        assert_eq!(pending.cutex.unwrap().method, "cutex/runtime/disconnected");
    }
}
