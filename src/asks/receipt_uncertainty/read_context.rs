//! Explicit request IO boundary. Call only AFTER owned AskState read methods
//! return, never inside AskStore/owner/bus callbacks. No native RPC or writes.
use std::{collections::BTreeSet, sync::Arc};

use super::read_views::{CurrentObserver, ReceiptReadContext};
use crate::asks::model::AskRecord;
use crate::session::model::{
    CutexSessionArchiveState, CutexSessionRuntimeBackend, CutexSessionStore,
};

pub const MAX_CONTEXT_MESSAGES: usize = 4096;

mod observer;

/// Production entry after the caller's owned AskState read returns.
pub fn load(asks: &[&AskRecord]) -> ReceiptReadContext {
    load_for_asks(
        asks,
        crate::management::v2::agent_bus_state::receipt_read_inputs_default,
    )
}

pub fn required_message_ids(asks: &[&AskRecord]) -> Vec<String> {
    let mut ids = BTreeSet::new();
    'asks: for ask in asks {
        for revision in &ask.revisions {
            if let Some(id) = &revision.message_id {
                ids.insert(id.clone());
                if ids.len() >= MAX_CONTEXT_MESSAGES {
                    break 'asks;
                }
            }
        }
    }
    ids.into_iter().collect()
}

/// Parent supplies its coherent, typed message/Evidence read. Session identity
/// is loaded independently ONCE, after that callback has released bus guards.
/// An unavailable diagnostic source leaves original asks readable and unknown.
pub fn load_for_asks(
    asks: &[&AskRecord],
    read_messages: impl FnOnce(&[String]) -> anyhow::Result<ReceiptReadContext>,
) -> ReceiptReadContext {
    observer::attach(message_context(asks, read_messages))
}

pub fn load_with(
    asks: &[&AskRecord],
    read_messages: impl FnOnce(&[String]) -> anyhow::Result<ReceiptReadContext>,
    read_sessions: impl FnOnce() -> anyhow::Result<Arc<CutexSessionStore>>,
) -> ReceiptReadContext {
    attach_current_observers(message_context(asks, read_messages), read_sessions)
}

fn message_context(
    asks: &[&AskRecord],
    read_messages: impl FnOnce(&[String]) -> anyhow::Result<ReceiptReadContext>,
) -> ReceiptReadContext {
    let ids = required_message_ids(asks);
    if ids.is_empty() {
        return ReceiptReadContext::new();
    }
    let Ok(mut context) = read_messages(&ids) else {
        return ReceiptReadContext::new();
    };
    let requested: BTreeSet<_> = ids.iter().map(String::as_str).collect();
    context.retain(|id, input| {
        requested.contains(id.as_str())
            && id == &input.snapshot.message_id
            && asks.iter().any(|ask| {
                ask.target_cutex_session_id == input.snapshot.to_cutex_session_id
                    && ask
                        .revisions
                        .iter()
                        .any(|revision| revision.message_id.as_ref() == Some(id))
            })
    });
    context
}

/// Ordinary tell read views (for example the owner stuck-message list) have
/// no AskRecord. Message IDs must already come from an authorized owned list.
pub fn load_messages(
    message_ids: &[String],
    read_messages: impl FnOnce(&[String]) -> anyhow::Result<ReceiptReadContext>,
) -> ReceiptReadContext {
    observer::attach(messages_context(message_ids, read_messages))
}

pub fn load_messages_with(
    message_ids: &[String],
    read_messages: impl FnOnce(&[String]) -> anyhow::Result<ReceiptReadContext>,
    read_sessions: impl FnOnce() -> anyhow::Result<Arc<CutexSessionStore>>,
) -> ReceiptReadContext {
    attach_current_observers(messages_context(message_ids, read_messages), read_sessions)
}

fn messages_context(
    message_ids: &[String],
    read_messages: impl FnOnce(&[String]) -> anyhow::Result<ReceiptReadContext>,
) -> ReceiptReadContext {
    let ids: Vec<_> = message_ids
        .iter()
        .take(MAX_CONTEXT_MESSAGES)
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if ids.is_empty() {
        return ReceiptReadContext::new();
    }
    let Ok(mut context) = read_messages(&ids) else {
        return ReceiptReadContext::new();
    };
    context.retain(|id, input| ids.binary_search(id).is_ok() && id == &input.snapshot.message_id);
    context
}

fn attach_current_observers(
    mut context: ReceiptReadContext,
    read_sessions: impl FnOnce() -> anyhow::Result<Arc<CutexSessionStore>>,
) -> ReceiptReadContext {
    if context.is_empty() {
        return context;
    }
    let sessions = read_sessions().ok();
    for input in context.values_mut() {
        // Ignore any observer copied into the message-provider result: the
        // authenticated session record is an independent source of identity.
        input.observer = None;
        let Some(record) = sessions
            .as_ref()
            .and_then(|sessions| sessions.sessions.get(&input.snapshot.to_cutex_session_id))
        else {
            continue;
        };
        if record.cutex_session_id != input.snapshot.to_cutex_session_id
            || record.runtime_backend != CutexSessionRuntimeBackend::ClaudeSubscription
            || record.archive_state != CutexSessionArchiveState::Active
            || record.current_runtime_agent_id.is_none()
        {
            continue;
        }
        if let Some(thread) = record
            .claude_session
            .as_ref()
            .map(|session| session.thread_id.as_str())
            .filter(|thread| !thread.is_empty())
        {
            input.observer = Some(CurrentObserver {
                cutex_session_id: record.cutex_session_id.clone(),
                thread_id: thread.into(),
                runtime_generation: record.runtime_generation,
            });
        }
    }
    context
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::receipt_uncertainty::read_views::{project_bus_read, tests::input};
    use crate::asks::{
        model::{RevisionLevel, RevisionRecord},
        store::{tests as fixtures, AskState},
    };
    use serde_json::json;

    fn ask() -> AskRecord {
        let mut state = AskState::default();
        let mut ask = state
            .create_ask(
                fixtures::new_ask("read-context", "synthetic"),
                chrono::Utc::now(),
            )
            .unwrap()
            .0;
        ask.revisions.push(
            serde_json::from_value::<RevisionRecord>(json!({"id":"v2","text":"synthetic",
            "messageId":"tell","sentAt":"2026-10-27T08:00:00Z","level":"sent"}))
            .unwrap(),
        );
        ask
    }
    pub(super) fn sessions() -> Arc<CutexSessionStore> {
        let mut sessions = CutexSessionStore::default();
        let mut record = crate::session::model::CutexSessionRecord::new(
            fixtures::TARGET.into(),
            None,
            "test-host".into(),
            "synthetic".into(),
            None,
        )
        .unwrap();
        record.runtime_backend = CutexSessionRuntimeBackend::ClaudeSubscription;
        record.runtime_generation = 9;
        record.current_runtime_agent_id = Some("synthetic-runtime".into());
        // Strings only: no deployment validation, config or credential reads.
        record.claude_session = Some(serde_json::from_value(json!({"deployment":{
            "python":"synthetic","adapter_source":"synthetic","claude_cli":"synthetic",
            "claude_home":"synthetic","tui_binary":"synthetic"},"thread_id":"thread","state_dir":"synthetic","model":"synthetic"})).unwrap());
        sessions.sessions.insert(fixtures::TARGET.into(), record);
        Arc::new(sessions)
    }

    #[test]
    fn message_read_finishes_before_independent_current_observer_read() {
        let ask = ask();
        let ended = std::cell::Cell::new(false);
        let context = load_with(
            &[&ask],
            |ids| {
                assert_eq!(ids, &["tell".to_string()]);
                ended.set(true);
                let mut input = input();
                input.observer.as_mut().unwrap().runtime_generation = 111;
                Ok(ReceiptReadContext::from([("tell".into(), input)]))
            },
            || {
                assert!(ended.get());
                Ok(sessions())
            },
        );
        assert_eq!(
            context["tell"]
                .observer
                .as_ref()
                .unwrap()
                .runtime_generation,
            9
        );
        assert_eq!(
            project_bus_read(&context["tell"], RevisionLevel::Sent)["state"],
            "uncertain"
        );
    }

    #[test]
    fn read_failure_keeps_original_message_and_a_forged_provider_observer_is_discarded() {
        let ask = ask();
        let context = load_with(
            &[&ask],
            |_| Ok(ReceiptReadContext::from([("tell".into(), input())])),
            || Err(anyhow::anyhow!("synthetic current identity read failure")),
        );
        assert!(context["tell"].observer.is_none());
        assert!(context["tell"].terminal.is_some());
        assert_eq!(
            project_bus_read(&context["tell"], RevisionLevel::Sent)["state"],
            "unknown"
        );
        let failed = load_with(
            &[&ask],
            |_| Err(anyhow::anyhow!("synthetic message read failure")),
            || panic!("no observer read after failed message read"),
        );
        assert!(failed.is_empty());
    }

    #[test]
    fn unrequested_or_wrong_recipient_rows_are_excluded_and_empty_requests_do_no_io() {
        let mut ask = ask();
        ask.target_cutex_session_id = "different".into();
        let context = load_with(
            &[&ask],
            |_| {
                Ok(ReceiptReadContext::from([
                    ("tell".into(), input()),
                    ("unrequested".into(), input()),
                ]))
            },
            || Ok(sessions()),
        );
        assert!(context.is_empty());
        let empty = load_with(
            &[],
            |_| panic!("empty request must not read messages"),
            || panic!("empty request must not read sessions"),
        );
        assert!(empty.is_empty());
    }

    #[test]
    fn stale_or_non_claude_current_session_does_not_authenticate_historical_evidence() {
        let ask = ask();
        for backend in [
            CutexSessionRuntimeBackend::Host,
            CutexSessionRuntimeBackend::ClaudeSubscription,
        ] {
            let mut sessions = (*sessions()).clone();
            let record = sessions.sessions.get_mut(fixtures::TARGET).unwrap();
            record.runtime_backend = backend;
            record.runtime_generation = 10;
            let context = load_with(
                &[&ask],
                |_| Ok(ReceiptReadContext::from([("tell".into(), input())])),
                || Ok(Arc::new(sessions)),
            );
            assert_eq!(
                project_bus_read(&context["tell"], RevisionLevel::Sent)["state"],
                "unknown"
            );
        }
    }
}
