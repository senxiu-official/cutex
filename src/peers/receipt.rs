//! Client-facing evidence for delivery between two daemon instances.

use super::Error;
use crate::asks::model::{
    AskRecord, CloseOutcome, ExecutionState, Hold, InterruptReason, Requester,
};
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RoutingRefusal {
    Source,
    Destination,
}

impl RoutingRefusal {
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::Source => "delegation_cross_host_source_routing_refused",
            Self::Destination => "delegation_cross_host_destination_routing_refused",
        }
    }

    pub(crate) const fn message(self) -> &'static str {
        match self {
            Self::Source => "The source project refused the cross-host route. Inspect its delegation routing policy.",
            Self::Destination => "The destination project refused the cross-host route. Its routing requirements cannot be met by this peer request.",
        }
    }

    pub(crate) fn from_code(code: &str) -> Option<Self> {
        [Self::Source, Self::Destination]
            .into_iter()
            .find(|side| side.code() == code)
    }

    pub(crate) fn ask_error(self) -> crate::asks::store::AskError {
        crate::asks::store::AskError::new(self.code(), self.message())
    }

    pub(crate) fn peer_error(self) -> Error {
        Error::new(self.code(), 403)
    }
}

/// Older receivers collapsed admissions and routing errors into this same
/// HTTP 400 response. Keep it separate from a transport outage.
pub(crate) const UNSPECIFIED_CREATE_REFUSAL: &str = "peer_create_refused_unspecified";

pub(crate) const MANAGEMENT_FEATURE: &str = "peer-delivery-receipts-v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Direction {
    Inbound,
    Outbound,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum DeliveryState {
    QueuedAtSource,
    DeliveredToPeer,
    Accepted,
    Failed,
    OutcomeUnknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Reason {
    SourceRoutingRefused,
    DestinationRoutingRefused,
    PeerRefusedUnspecified,
    PeerUnreachable,
    PeerCredentialRefused,
    PeerIdentityChanged,
    PeerVersionMismatch,
    PeerRemoved,
    PeerDisabled,
    PeerRejectedRequest,
    DestinationRejected,
    DestinationFailed,
    Cancelled,
    Interrupted,
    OutcomeUnknown,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Host<'a> {
    instance_id: &'a str,
    label: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Receipt<'a> {
    state: DeliveryState,
    reason: Option<Reason>,
    confirmed_at: Option<&'a str>,
}

#[derive(Serialize)]
pub(crate) struct CrossHost<'a> {
    direction: Direction,
    peer: Host<'a>,
    receipt: Receipt<'a>,
}

impl<'a> CrossHost<'a> {
    pub(crate) fn new(
        direction: Direction,
        instance_id: &'a str,
        label: &str,
        state: DeliveryState,
        reason: Option<Reason>,
        confirmed_at: Option<&'a str>,
    ) -> Option<Self> {
        // An old or damaged row may lack valid provenance. A label alone
        // cannot safely identify a host or enable navigation.
        super::valid_id(instance_id).then(|| Self {
            direction,
            peer: Host {
                instance_id,
                label: super::wire::sanitize_name(label),
            },
            receipt: Receipt {
                state,
                reason,
                confirmed_at,
            },
        })
    }
}

pub(crate) fn insert(value: &mut Value, projection: Option<CrossHost<'_>>) {
    if let Some(projection) = projection {
        value["crossHost"] = serde_json::json!(projection);
    }
}

pub(crate) fn execution_proves_admission(execution: ExecutionState) -> bool {
    matches!(
        execution,
        ExecutionState::Admitted | ExecutionState::Running | ExecutionState::Answered
    )
}

pub(crate) fn source_routing_held(sync: &super::model::Sync) -> bool {
    sync.state == "routingPending"
        && sync
            .last_error
            .as_deref()
            .and_then(|code| code.strip_prefix("routing_source:"))
            .is_some_and(|code| {
                code == RoutingRefusal::Source.code()
                    || code == "delegation_cross_host_routing_unsupported"
            })
}

fn request_failure(code: &str) -> Reason {
    match RoutingRefusal::from_code(code) {
        Some(RoutingRefusal::Source) => return Reason::SourceRoutingRefused,
        Some(RoutingRefusal::Destination) => return Reason::DestinationRoutingRefused,
        None => {}
    }
    match code {
        UNSPECIFIED_CREATE_REFUSAL => Reason::PeerRefusedUnspecified,
        "peer_unreachable" => Reason::PeerUnreachable,
        "remote_credential_refused" | "peer_credential_required" => Reason::PeerCredentialRefused,
        "peerRemoved" | "removedByPeer" => Reason::PeerRemoved,
        "peerDisabled" | "peersDisabled" => Reason::PeerDisabled,
        _ => Reason::PeerRejectedRequest,
    }
}

pub(crate) fn tell_failure(error: &Error) -> (DeliveryState, Reason) {
    if error.is_transport() || error.is_protocol() {
        (DeliveryState::OutcomeUnknown, Reason::OutcomeUnknown)
    } else {
        (DeliveryState::Failed, request_failure(&error.code))
    }
}

fn terminal(
    ask: &AskRecord,
    interrupted: Option<InterruptReason>,
) -> Option<(DeliveryState, Reason)> {
    let reason = match ask.execution {
        ExecutionState::Failed => Reason::DestinationFailed,
        ExecutionState::Rejected => Reason::DestinationRejected,
        ExecutionState::Cancelled => Reason::Cancelled,
        ExecutionState::Interrupted if interrupted == Some(InterruptReason::OutcomeUnknown) => {
            return Some((DeliveryState::OutcomeUnknown, Reason::OutcomeUnknown))
        }
        ExecutionState::Interrupted => Reason::Interrupted,
        _ => return None,
    };
    Some((DeliveryState::Failed, reason))
}

fn outbound(ask: &AskRecord, remote: &super::model::Remote) -> (DeliveryState, Option<Reason>) {
    use DeliveryState::*;
    let sync = &remote.sync;
    // The worker sets the sync state to a closed failure code on a definitive
    // refusal. Snapshot diagnostic text is untrusted and cannot set a reason.
    if let Some((state, fallback)) = terminal(ask, sync.interrupt_reason) {
        let reason = match sync.state.as_str() {
            "synced"
            | "remoteStateUnknown"
            | "queued"
            | "routingPending"
            | "createOutcomeUnknown" => fallback,
            "outcomeUnknown" => Reason::OutcomeUnknown,
            code => request_failure(code),
        };
        return (state, Some(reason));
    }
    // A local close can retain a queued execution state. Even if a late
    // snapshot confirms delivery, it must not reopen the closed receipt.
    if let Some(closed) = ask.closed.filter(|_| {
        matches!(
            ask.execution,
            ExecutionState::Queued | ExecutionState::PendingReview
        )
    }) {
        let reason = match closed {
            CloseOutcome::Abandoned | CloseOutcome::Cancelled | CloseOutcome::Stopped => {
                Reason::Cancelled
            }
            _ => Reason::Interrupted,
        };
        return (Failed, Some(reason));
    }
    let reason = match ask.hold {
        Some(Hold::PeerIdentityChanged) => Some(Reason::PeerIdentityChanged),
        Some(Hold::PeerVersionMismatch) => Some(Reason::PeerVersionMismatch),
        Some(Hold::PeerUnreachable)
            if sync.failures > 0
                && matches!(
                    sync.last_error.as_deref(),
                    Some("remote_credential_refused" | "peer_credential_required")
                ) =>
        {
            Some(Reason::PeerCredentialRefused)
        }
        Some(Hold::PeerUnreachable) if sync.failures > 0 => Some(Reason::PeerUnreachable),
        _ if source_routing_held(sync) => Some(Reason::SourceRoutingRefused),
        _ => None,
    };
    if sync.accepted {
        let state = if sync.admitted
            || ask.answer_id.is_some()
            || execution_proves_admission(ask.execution)
        {
            Accepted
        } else {
            DeliveredToPeer
        };
        return (state, reason);
    }
    if sync.state == "createOutcomeUnknown" {
        return (
            OutcomeUnknown,
            Some(reason.unwrap_or(Reason::OutcomeUnknown)),
        );
    }
    (QueuedAtSource, reason)
}

pub(crate) fn delegation(ask: &AskRecord) -> Option<CrossHost<'_>> {
    if let Some(remote) = &ask.remote {
        let (state, reason) = outbound(ask, remote);
        return CrossHost::new(
            Direction::Outbound,
            &remote.peer_id,
            &remote.label,
            state,
            reason,
            remote
                .sync
                .accepted
                .then_some(&remote.sync.last_received_at),
        );
    }
    let Requester::Peer { peer_id, label, .. } = &ask.requester else {
        return None;
    };
    let (state, reason) = match terminal(
        ask,
        ask.current_attempt()
            .and_then(|attempt| attempt.interrupt_reason),
    ) {
        Some((state, reason)) => (state, Some(reason)),
        None if execution_proves_admission(ask.execution) || !ask.attempts.is_empty() => {
            (DeliveryState::Accepted, None)
        }
        None => (DeliveryState::DeliveredToPeer, None),
    };
    CrossHost::new(
        Direction::Inbound,
        peer_id,
        label,
        state,
        reason,
        Some(&ask.created_at),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, to_value};

    #[test]
    fn stage_uses_positive_evidence_and_distinguishes_failure_from_uncertainty() {
        fn view(ask: &AskRecord) -> Value {
            to_value(delegation(ask)).unwrap()
        }
        fn receipt(ask: &AskRecord) -> Value {
            view(ask)["receipt"].clone()
        }

        let (_, mut ask, peer) = super::super::tests::remote_record();
        assert_eq!(view(&ask)["peer"]["instanceId"], peer.peer_id);
        assert_eq!(
            receipt(&ask),
            json!({"state":"queuedAtSource","reason":null,"confirmedAt":null})
        );

        ask.remote.as_mut().unwrap().sync.state = "createOutcomeUnknown".into();
        assert_eq!(receipt(&ask)["state"], "outcomeUnknown");

        let sync = &mut ask.remote.as_mut().unwrap().sync;
        sync.state = "synced".into();
        sync.accepted = true;
        ask.execution = ExecutionState::PendingReview;
        assert_eq!(receipt(&ask)["state"], "deliveredToPeer");
        assert!(receipt(&ask)["confirmedAt"].is_string());

        ask.execution = ExecutionState::Queued;
        let sync = &mut ask.remote.as_mut().unwrap().sync;
        sync.admitted = true;
        sync.last_error = Some(RoutingRefusal::Destination.code().into());
        assert_eq!(
            receipt(&ask)["state"],
            "accepted",
            "an already confirmed admission is sticky"
        );
        assert!(
            receipt(&ask)["reason"].is_null(),
            "snapshot diagnostics have no routing authority"
        );

        let sync = &mut ask.remote.as_mut().unwrap().sync;
        sync.state = RoutingRefusal::Destination.code().into();
        sync.local_terminal = true;
        ask.execution = ExecutionState::Failed;
        assert_eq!(receipt(&ask)["state"], "failed");
        assert_eq!(receipt(&ask)["reason"], "destinationRoutingRefused");
    }

    #[test]
    fn closing_before_confirmation_projects_cancellation_even_after_late_receipt() {
        let (mut state, ask, _) = super::super::tests::remote_record();
        let id = &ask.ask_id;
        super::super::asks::queue_op(&mut state, id, "abandoned", chrono::Utc::now()).unwrap();
        assert_eq!(state.asks[id].execution, ExecutionState::Queued);
        let before = to_value(delegation(&state.asks[id])).unwrap();
        assert_eq!(
            before["receipt"],
            json!({"state":"failed","reason":"cancelled","confirmedAt":null})
        );

        state
            .asks
            .get_mut(id)
            .unwrap()
            .remote
            .as_mut()
            .unwrap()
            .sync
            .accepted = true;
        let late = to_value(delegation(&state.asks[id])).unwrap();
        assert_eq!(late["receipt"]["state"], "failed");
        assert_eq!(late["receipt"]["reason"], "cancelled");
        assert!(late["receipt"]["confirmedAt"].is_string());
    }

    #[test]
    fn old_unadmitted_sync_row_deserializes_without_assuming_acceptance() {
        let (_, ask, _) = super::super::tests::remote_record();
        let sync = ask.remote.unwrap().sync;
        let old = to_value(&sync).unwrap();
        assert!(old.get("admitted").is_none());
        assert_eq!(
            serde_json::from_value::<super::super::model::Sync>(old).unwrap(),
            sync
        );
    }
}
