//! Bridge the authoritative ordered sources to the bound Claude observer.
use super::{claude_message_params, manager};
use anyhow::Context;
use chrono::Utc;
use cutex::app_server::client::AppServerClientError;
use cutex::app_server::external_input::automatic_batch::{
    capacity_params, validate_capacity, CAPACITY_METHOD,
};
use cutex::app_server::external_input::size::{ReceiverLimit, TRANSPORT_TEXT_MAX};
use cutex::app_server::pre_turn_notes::{Budget as StartBudget, Report};
use cutex::asks::{directory::Directory, store::AskStore};
use cutex::launch::stock::ExternalInputBinding;
use cutex::management::v2::{
    admission,
    agent_bus_state::AgentBusMessageRepository,
    automatic_input::{
        manifest::{Member, Origin, Outcome, ResultBudget},
        AutomaticInputQueue, Source,
    },
    input_disposal,
};
use cutex::session::model::CutexSessionStore;
use serde_json::{json, Value};
use std::ops::ControlFlow::{self, Break, Continue};
use std::time::{Duration, Instant};

pub(crate) fn supported(capabilities: &Value) -> bool {
    cutex::agent_bus::busy_delivery::claude_ordered(capabilities)
}

fn probe_legacy_inbox(
    result: Result<Value, AppServerClientError>,
) -> anyhow::Result<ControlFlow<(), Option<Value>>> {
    match result {
        Err(AppServerClientError::Timeout { .. }) => Ok(Break(())),
        result => super::super::claude_ingress::legacy_receipt(result).map(Continue),
    }
}

pub(super) fn drive(
    session: &str,
    thread: &str,
    generation: u64,
    sessions: &CutexSessionStore,
    bus: &AgentBusMessageRepository,
) -> anyhow::Result<()> {
    drive_at_boundary(session, thread, generation, sessions, bus, None).map(|_| ())
}

pub(super) fn retire_previous(
    session: &str,
    thread: &str,
    generation: u64,
    bus: &AgentBusMessageRepository,
) -> anyhow::Result<()> {
    let management = cutex::config::paths::runtime_dir()?.join("management-v2");
    let queue = AutomaticInputQueue::at_management_root(&management);
    let store = AskStore::open_default()?;
    queue.retire_replaced_bindings(&store, bus, session, thread, generation, Utc::now())
}

pub(crate) fn synchronize_start(
    session: &str,
    thread: &str,
    generation: u64,
    bus: &AgentBusMessageRepository,
    budget: StartBudget,
) -> anyhow::Result<Report> {
    let sessions = cutex::session::store::load_cutex_session_store_shared()?;
    drive_at_boundary(session, thread, generation, &sessions, bus, Some(budget))
}

fn drive_at_boundary(
    session: &str,
    thread: &str,
    generation: u64,
    sessions: &CutexSessionStore,
    bus: &AgentBusMessageRepository,
    startup: Option<StartBudget>,
) -> anyhow::Result<Report> {
    let queue = AutomaticInputQueue::at_management_root(
        &cutex::config::paths::runtime_dir()?.join("management-v2"),
    );
    let store = AskStore::open_default()?;
    let handle = manager().handle_for_generation(session, generation)?;
    let binding = ExternalInputBinding {
        version: 1,
        owner_id: session.into(),
        thread_id: thread.into(),
        runtime_generation: generation,
        canonical_byte_limit: Default::default(),
    };
    let rides_start = startup.is_some();
    let mut prepared = None;
    let mut report = Report::default();
    let outcome = queue.publish_with(
        &store,
        bus,
        session,
        thread,
        generation,
        Utc::now(),
        rides_start,
        || {
            let Some(status) = manager().status(session)?.filter(|status| {
                status.connected
                    && status.runtime_generation == generation
                    && status.thread_id == thread
            }) else {
                return Ok(None);
            };
            if cutex::asks::pause_admission::is_held(&store, session)? {
                return Ok(None);
            }
            let Some((state, mut candidates)) =
                queue.coherent_pending(session, &store, bus, rides_start)?
            else {
                return Ok(None);
            };
            if cutex::asks::control::stop_held(&state, session) {
                return Ok(None);
            }
            let active = status.active_turn_id.is_some();
            if (rides_start && active)
                || (!rides_start
                    && state.leases.get(session).is_some_and(|lease| {
                        lease.turn_id.is_none() || lease.turn_id != status.active_turn_id
                    }))
            {
                return Ok(None);
            }
            let now = Utc::now();
            if !active
                && !rides_start
                && candidates
                    .iter()
                    .any(|candidate| candidate.source == Source::Result)
                && AutomaticInputQueue::idle_result_wait(&store, &state, session, now)?
            {
                for candidate in &mut candidates {
                    if candidate.source == Source::Result {
                        candidate.may_wake = false;
                    }
                }
            }
            let settings = cutex::config::documents::current();
            let budget =
                ReceiverLimit::RawText(TRANSPORT_TEXT_MAX as u64).budget(settings.margin_percent);
            let cutoff = Instant::now() + Duration::from_millis(650);
            let passive = if candidates.is_empty() {
                StartBudget::default()
            } else {
                validate_capacity(
                    &binding,
                    handle.request_with_timeout(
                        CAPACITY_METHOD,
                        capacity_params(&binding),
                        cutoff.saturating_duration_since(Instant::now()),
                    )?,
                )?
            };
            let mut plan = queue.plan_prefix_for_boundary(
                session,
                &candidates,
                cutex::agent_bus::delivery::wake_busy_wait(),
                active,
                true,
                now,
                startup,
                passive,
                |candidate| {
                    if Instant::now() >= cutoff {
                        return Ok(Break(()));
                    }
                    if input_disposal::requested(session, &candidate.id)? {
                        return Ok(Continue(None));
                    }
                    let member = match candidate.source {
                        Source::Result => {
                            let Some((source, submission)) =
                                cutex::asks::returns::prepare_automatic_result(
                                    &store,
                                    &state,
                                    session,
                                    &candidate.id,
                                    &budget,
                                    settings.preview_bytes as usize,
                                    now,
                                )?
                            else {
                                return Ok(Continue(None));
                            };
                            Member {
                                envelope: submission.envelope(&binding, true)?,
                                origin: Origin::Result(source),
                            }
                        }
                        Source::Bus => {
                            let snapshot = bus
                                .snapshot_by_message_id(&candidate.id)?
                                .context("automatic Bus source disappeared")?;
                            let canonical_digest = snapshot
                                .semantic_sha256
                                .context("automatic Bus source is unverified")?;
                            let request = handle.request_with_timeout(
                                "claude/input/status",
                                json!({"threadId":thread, "messageId":candidate.id}),
                                cutoff.saturating_duration_since(Instant::now()),
                            );
                            match probe_legacy_inbox(request)? {
                                Break(()) => return Ok(Break(())),
                                Continue(Some(receipt)) => {
                                    bus.record_local_inbox(session, &candidate.id, &receipt)?;
                                    return Ok(Continue(None));
                                }
                                Continue(None) => {}
                            }
                            let message = bus.canonical_message(&candidate.id)?;
                            let meta = bus.message_meta(&candidate.id)?;
                            let payload = claude_message_params(
                                sessions,
                                thread,
                                session,
                                &message,
                                meta.as_ref(),
                                true,
                            )?;
                            Member {
                                envelope: super::super::claude_ingress::envelope(
                                    session, thread, generation, &payload,
                                )?,
                                origin: Origin::Bus { canonical_digest },
                            }
                        }
                    };
                    Ok(Continue(Some(member)))
                },
            )?;
            if let Some(plan) = plan.as_mut() {
                if !active
                    && plan.trigger.is_some()
                    && admission::owner_defers_delivery(
                        session,
                        &store
                            .queue_repository()?
                            .admission_with_state(session, &state)?,
                    )
                {
                    return Ok(None);
                }
                if !rides_start
                    && plan
                        .members
                        .iter()
                        .any(|member| matches!(member.origin, Origin::Result(_)))
                {
                    let domain = Directory::load()
                        .ok()
                        .map(|directory| {
                            cutex::asks::directory::domain_label(&directory.domain(session))
                        })
                        .unwrap_or_else(|| format!("agent:{session}"));
                    plan.result_budget = Some(ResultBudget {
                        domain,
                        limit: cutex::asks::budget::AskLimits::load().auto_turns_per_hour,
                        at: now,
                    });
                }
                if rides_start {
                    report.admitted = plan
                        .members
                        .iter()
                        .map(|member| member.input_id().to_owned())
                        .collect();
                    report.budget_notes = plan.members.len();
                    report.budget_bytes = plan
                        .members
                        .iter()
                        .map(|member| serde_json::to_vec(&member.envelope).map(|value| value.len()))
                        .sum::<Result<usize, _>>()?;
                }
            }
            Ok(plan)
        },
        |method, params| {
            if method == cutex::app_server::external_input::automatic_batch::SUBMIT_METHOD
                && params["messages"].as_array().is_some_and(|members| {
                    members
                        .iter()
                        .any(|member| member["message"]["delivery"] == "after_turn")
                })
            {
                prepared = Some(admission::prepare_turn(session, "delivery", None));
            }
            let timeout =
                if method == cutex::app_server::external_input::automatic_batch::STATUS_METHOD {
                    Duration::from_millis(250)
                } else {
                    Duration::from_secs(3)
                };
            let response = handle.request_with_timeout(method, params, timeout)?;
            manager().handle_for_generation(session, generation)?;
            Ok(response)
        },
    )?;
    if matches!(outcome, Outcome::Pending | Outcome::Settled) {
        if let Some(turn) = prepared {
            turn.submitted();
        }
    }
    if !matches!(outcome, Outcome::WaitingForStart | Outcome::Settled) {
        report.admitted.clear();
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cutex::app_server::external_input::{
        Delivery, Envelope, Message, Source as Sender, SourceKind,
    };
    use cutex::app_server::protocol::RpcError;
    use cutex::management::v2::automatic_input::Candidate;

    #[test]
    fn slow_legacy_adapter_keeps_the_verified_ordered_prefix() {
        let temp =
            std::env::temp_dir().join(format!("cutex-ordered-probe-{}", uuid::Uuid::new_v4()));
        let queue = AutomaticInputQueue::at_management_root(&temp);
        let now = Utc::now();
        let candidates: Vec<_> = ["verified", "slow", "later"]
            .into_iter()
            .map(|id| Candidate {
                id: id.into(),
                source: Source::Bus,
                received_at: now,
                may_wake: true,
            })
            .collect();
        let mut probes = Vec::new();
        let mut fake_adapter = |id: &str| {
            probes.push(id.to_owned());
            if id == "slow" {
                std::thread::sleep(Duration::from_millis(15));
                Err(AppServerClientError::Timeout {
                    method: "claude/input/status".into(),
                })
            } else {
                Err(AppServerClientError::Rpc(RpcError {
                    code: -32004,
                    message: "input not found".into(),
                    data: None,
                }))
            }
        };
        let plan = queue
            .plan_prefix_for_boundary(
                "receiver",
                &candidates,
                Some(Duration::ZERO),
                true,
                true,
                now,
                None,
                StartBudget::default(),
                |candidate| {
                    match probe_legacy_inbox(fake_adapter(&candidate.id))? {
                        Break(()) => return Ok(Break(())),
                        Continue(Some(_)) => return Ok(Continue(None)),
                        Continue(None) => {}
                    }
                    Ok(Continue(Some(Member {
                        envelope: Envelope {
                            version: 1,
                            owner_id: "receiver".into(),
                            thread_id: "thread".into(),
                            runtime_generation: 7,
                            message: Message {
                                id: candidate.id.clone(),
                                source: Sender {
                                    kind: SourceKind::Agent,
                                    id: "sender".into(),
                                },
                                event_type: "message".into(),
                                delivery: Delivery::Passive,
                                text: "fixture".into(),
                            },
                            view: None,
                            semantic_sha256: String::new(),
                        },
                        origin: Origin::Bus {
                            canonical_digest: "a".repeat(64),
                        },
                    })))
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(probes, ["verified", "slow"]);
        assert_eq!(plan.trigger.as_deref(), Some("verified"));
        assert_eq!(plan.members.len(), 1);
        assert_eq!(plan.members[0].envelope.message.id, "verified");
        std::fs::remove_dir_all(temp).unwrap();
    }
}
