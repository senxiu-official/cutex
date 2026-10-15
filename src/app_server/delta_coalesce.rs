//! Merge a backlog of streaming deltas before they are made durable.
//!
//! Every runtime event is made durable in the Management v2 log before it
//! is published (one fsync per group-committed batch, ~0.4 ms on an NVMe
//! disk; it used to be ~1-2 ms with a state rewrite per event), so a long
//! answer that streams thousands of one-token deltas can fall behind the
//! native runtime. When the event worker is behind, consecutive deltas for the same
//! item are additive and can be merged without loss: the merged notification
//! keeps the first one's envelope (`emittedAtMs` included) and carries the
//! concatenated `delta`. Nothing waits for more input: with no backlog every
//! delta is still published on its own, immediately.

use serde_json::Value;

use super::client::AppServerEvent;
use super::protocol::RpcNotification;

/// Notifications whose `params.delta` string is appended by clients.
pub(crate) const DELTA_METHODS: &[&str] = &[
    "item/agentMessage/delta",
    "item/reasoning/textDelta",
    "item/reasoning/summaryTextDelta",
    "item/commandExecution/outputDelta",
    "item/fileChange/outputDelta",
    "item/plan/delta",
];

/// Bounds one merged event (and the work done before it is published).
const MAX_MERGED_DELTA_BYTES: usize = 32 * 1024;
const MAX_MERGED_EVENTS: usize = 512;

fn is_delta(notification: &RpcNotification) -> bool {
    DELTA_METHODS.contains(&notification.method.as_str())
        && notification
            .params
            .as_ref()
            .and_then(|params| params.get("delta"))
            .is_some_and(Value::is_string)
}

fn emitted_at_ms(notification: &RpcNotification) -> Option<i64> {
    notification.raw.get("emittedAtMs").and_then(Value::as_i64)
}

/// Append `next` to `into` when it continues the same stream: same method
/// and identical params apart from `delta`. Never across a journal handoff
/// cut (`single_writer`): each side must journal exactly its own deltas.
fn merge(into: &mut RpcNotification, next: &RpcNotification, split_after: Option<i64>) -> bool {
    if into.method != next.method || !is_delta(next) {
        return false;
    }
    if let (Some(cut), Some(first), Some(following)) =
        (split_after, emitted_at_ms(into), emitted_at_ms(next))
    {
        if first <= cut && following > cut {
            return false;
        }
    }
    let (Some(Value::Object(current)), Some(Value::Object(incoming))) = (&into.params, &next.params)
    else {
        return false;
    };
    if current.len() != incoming.len()
        || current
            .iter()
            .any(|(key, value)| key != "delta" && incoming.get(key) != Some(value))
    {
        return false;
    }
    let (Some(Value::String(head)), Some(Value::String(tail))) = (current.get("delta"), incoming.get("delta"))
    else {
        return false;
    };
    if head.len() + tail.len() > MAX_MERGED_DELTA_BYTES {
        return false;
    }
    let merged = format!("{head}{tail}");
    if let Some(Value::Object(params)) = into.params.as_mut() {
        params.insert("delta".to_string(), Value::String(merged.clone()));
    }
    if let Some(Value::Object(params)) = into.raw.get_mut("params") {
        params.insert("delta".to_string(), Value::String(merged));
    }
    true
}

/// Merge deltas that are already waiting (`next_ready` must not block) into
/// `event`. Returns the first waiting event that could not be merged; the
/// caller handles it next, so ordering is unchanged.
#[cfg(test)]
pub(crate) fn coalesce_ready(
    event: &mut AppServerEvent,
    next_ready: impl FnMut() -> Option<AppServerEvent>,
) -> Option<AppServerEvent> {
    coalesce_ready_until(event, None, next_ready)
}

/// [`coalesce_ready`] that keeps deltas emitted after `split_after` apart
/// from earlier ones.
pub(crate) fn coalesce_ready_until(
    event: &mut AppServerEvent,
    split_after: Option<i64>,
    mut next_ready: impl FnMut() -> Option<AppServerEvent>,
) -> Option<AppServerEvent> {
    let AppServerEvent::Notification(notification) = event else {
        return None;
    };
    if !is_delta(notification) {
        return None;
    }
    for _ in 1..MAX_MERGED_EVENTS {
        match next_ready()? {
            AppServerEvent::Notification(next) if merge(notification, &next, split_after) => {}
            other => return Some(other),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;

    fn delta(method: &str, item: &str, text: &str, emitted: u64) -> AppServerEvent {
        let params = json!({ "threadId": "t", "turnId": "u", "itemId": item, "delta": text });
        AppServerEvent::Notification(RpcNotification {
            method: method.to_string(),
            params: Some(params.clone()),
            raw: json!({ "method": method, "params": params, "emittedAtMs": emitted }),
        })
    }

    fn other(method: &str) -> AppServerEvent {
        AppServerEvent::Notification(RpcNotification {
            method: method.to_string(),
            params: Some(json!({})),
            raw: json!({ "method": method, "params": {} }),
        })
    }

    fn notification(event: &AppServerEvent) -> &RpcNotification {
        match event {
            AppServerEvent::Notification(notification) => notification,
            _ => panic!("not a notification"),
        }
    }

    #[test]
    fn waiting_deltas_of_one_item_merge_and_the_rest_keeps_its_order() {
        let mut queue = VecDeque::from([
            delta("item/agentMessage/delta", "a", "lo", 2),
            delta("item/agentMessage/delta", "a", ", world", 3),
            delta("item/agentMessage/delta", "b", "x", 4),
            delta("item/agentMessage/delta", "b", "y", 5),
        ]);
        let mut first = delta("item/agentMessage/delta", "a", "hel", 1);
        let carried = coalesce_ready(&mut first, || queue.pop_front()).expect("item b is carried");
        let merged = notification(&first);
        assert_eq!(merged.params.as_ref().unwrap()["delta"], "hello, world");
        assert_eq!(merged.raw["params"]["delta"], "hello, world");
        assert_eq!(merged.raw["emittedAtMs"], 1);
        assert_eq!(notification(&carried).params.as_ref().unwrap()["itemId"], "b");
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn nothing_waits_and_other_events_are_never_merged() {
        let mut lone = delta("item/agentMessage/delta", "a", "x", 1);
        assert!(coalesce_ready(&mut lone, || None).is_none());
        assert_eq!(notification(&lone).params.as_ref().unwrap()["delta"], "x");

        let mut started = other("item/started");
        let mut pulled = 0;
        assert!(coalesce_ready(&mut started, || {
            pulled += 1;
            None
        })
        .is_none());
        assert_eq!(pulled, 0);

        let mut queue = VecDeque::from([
            delta("item/reasoning/textDelta", "a", "r", 2),
            other("item/completed"),
        ]);
        let mut text = delta("item/agentMessage/delta", "a", "t", 1);
        let carried = coalesce_ready(&mut text, || queue.pop_front()).unwrap();
        assert_eq!(notification(&carried).method, "item/reasoning/textDelta");
        assert_eq!(notification(&text).params.as_ref().unwrap()["delta"], "t");
    }

    #[test]
    fn deltas_are_never_merged_across_a_handoff_cut() {
        let mut queue = VecDeque::from([
            delta("item/agentMessage/delta", "a", "b", 10),
            delta("item/agentMessage/delta", "a", "c", 11),
            delta("item/agentMessage/delta", "a", "d", 12),
        ]);
        let mut first = delta("item/agentMessage/delta", "a", "a", 9);
        let carried = coalesce_ready_until(&mut first, Some(10), || queue.pop_front())
            .expect("the first delta after the cut starts a new event");
        assert_eq!(notification(&first).params.as_ref().unwrap()["delta"], "ab");
        let mut after = carried;
        assert!(coalesce_ready_until(&mut after, Some(10), || queue.pop_front()).is_none());
        assert_eq!(notification(&after).params.as_ref().unwrap()["delta"], "cd");
        assert_eq!(notification(&after).raw["emittedAtMs"], 11);
    }

    #[test]
    fn merged_deltas_stay_bounded() {
        let big = "x".repeat(MAX_MERGED_DELTA_BYTES - 1);
        let mut queue = VecDeque::from([delta("item/agentMessage/delta", "a", "yy", 2)]);
        let mut first = delta("item/agentMessage/delta", "a", &big, 1);
        assert!(coalesce_ready(&mut first, || queue.pop_front()).is_some());
        assert_eq!(
            notification(&first).params.as_ref().unwrap()["delta"].as_str().unwrap().len(),
            MAX_MERGED_DELTA_BYTES - 1
        );
    }
}
