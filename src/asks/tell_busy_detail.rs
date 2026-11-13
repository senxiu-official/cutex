//! Sender wording uses negotiated capability, never performs native IO.
use crate::agent_bus::{busy_delivery::{self, Capability}, delivery};

pub(super) fn detail(recipient: &str, state: &str, wake: bool, original: String) -> String {
    if state != "busy" || !wake { return original; }
    detail_with(state, wake, original, busy_delivery::for_session(recipient), delivery::wake_busy_wait())
}

/// A remote host reports its own observed capability and configured wait.
/// It never asks the sender to infer either from a runtime label.
pub(crate) fn policy_hint(recipient: &str) -> serde_json::Value {
    serde_json::json!({"capability":busy_delivery::for_session(recipient),"waitSecs":delivery::wake_busy_wait().map(|wait|wait.as_secs_f64())})
}

pub(crate) fn from_hint(state: &str, wake: bool, original: String, hint: &serde_json::Value) -> String {
    let capability = serde_json::from_value(hint["capability"].clone()).unwrap_or(Capability::None);
    let wait = hint["waitSecs"].as_f64().filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .and_then(|seconds| std::time::Duration::try_from_secs_f64(seconds).ok());
    detail_with(state, wake, original, capability, wait)
}

fn detail_with(state: &str, wake: bool, original: String, capability: Capability, wait: Option<std::time::Duration>) -> String {
    if state != "busy" || !wake || capability != Capability::Soon { return original; }
    let Some(wait) = wait else { return original };
    let replacement = if wait.is_zero() {
        "it reads your message at the next supported read point in the running turn, or starts a new turn if this turn ends first.".to_string()
    } else {
        format!("it waits up to {} s for the current turn to end; otherwise it reads your message at the next supported read point in the running turn. If the turn ends first, it starts a new turn.", wait.as_secs_f64())
    };
    original.replacen("it reads your message when its current turn ends, in a new turn.", &replacement, 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    const OLD: &str = "Sent as a wake. Worker is busy: it reads your message when its current turn ends, in a new turn. It expects no answer.";
    #[test]
    fn negotiated_wake_wait_is_conditional_and_keeps_tail() {
        let text = detail_with("busy", true, OLD.into(), Capability::Soon, Some(Duration::from_secs(120)));
        assert!(text.contains("waits up to 120 s"));
        assert!(text.contains("next supported read point in the running turn"));
        assert!(text.ends_with("It expects no answer."));
        assert!(!text.contains("when its current turn ends, in a new turn"));
    }
    #[test]
    fn zero_wait_says_next_read_point_and_fraction_is_preserved() {
        let text = detail_with("busy", true, OLD.into(), Capability::Soon, Some(Duration::ZERO));
        assert!(text.contains("at the next supported read point")); assert!(!text.contains("waits up to"));
        assert!(detail_with("busy", true, OLD.into(), Capability::Soon, Some(Duration::from_millis(1500))).contains("1.5 s"));
    }
    #[test]
    fn never_early_unsupported_and_other_send_kinds_keep_their_contract() {
        for (state, wake, capability, wait) in [
            ("busy", true, Capability::Soon, None),
            ("busy", true, Capability::AfterTurn, Some(Duration::ZERO)),
            ("busy", true, Capability::None, Some(Duration::ZERO)),
            ("idle", true, Capability::Soon, Some(Duration::ZERO)),
            ("busy", false, Capability::Soon, Some(Duration::ZERO)),
        ] { assert_eq!(detail_with(state, wake, OLD.into(), capability, wait), OLD); }
    }
    #[test]
    fn remote_hint_uses_receiving_hosts_wait_and_handles_old_hosts() {
        let value=serde_json::json!({"capability":"soon","waitSecs":45});
        assert!(from_hint("busy",true,OLD.into(),&value).contains("45 s"));
        for value in [serde_json::Value::Null,serde_json::json!({"capability":"soon","waitSecs":null}),serde_json::json!({"capability":"unknown","waitSecs":120})] {
            assert_eq!(from_hint("busy",true,OLD.into(),&value),OLD);
        }
    }
}
