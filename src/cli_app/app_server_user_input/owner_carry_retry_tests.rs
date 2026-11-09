// Include directly inside app_server_user_input::tests. No runtime, socket,
// capability advertisement or provider is started: the send callback is the
// boundary which must remain unreachable for a previously reserved carrier.

enum OwnerCarryRetrySource {
    None,
    Bus(String),
    Results(String),
}

fn owner_carry_retry_reserve_source(
    kind: &str,
    session: &str,
    client: &str,
) -> OwnerCarryRetrySource {
    use sha2::Digest;

    if kind == "none" {
        return OwnerCarryRetrySource::None;
    }
    if kind == "bus" {
        let bus = cutex::management::v2::agent_bus_state::agent_bus_message_repository().unwrap();
        let sender = format!("cutex.{}", uuid::Uuid::new_v4());
        let id = format!("owner-retry-notice-{}", uuid::Uuid::new_v4());
        let message: cutex::agent_bus::model::AgentBusMessage = serde_json::from_value(json!({
            "id": id, "from": sender, "to": "fixture-runtime",
            "fromCutexSessionId": sender, "toCutexSessionId": session,
            "content": "notice @verbatim-path", "deliveryMode": "after_turn",
            "triggerTurn": true, "createdAtEpochSecs": 1
        }))
        .unwrap();
        bus.record_queued(
            cutex::management::v2::agent_bus_state::AgentBusQueuedMessage {
                owner_cutex_session_id: sender.clone(),
                message_id: id.clone(),
                from_cutex_session_id: Some(sender),
                to_cutex_session_id: session.into(),
                from_runtime_agent_id: Some("fixture-source".into()),
                to_runtime_agent_id: Some("fixture-runtime".into()),
                delivery_mode: "after_turn".into(),
                content: message.content.clone(),
                queued_at: Utc::now(),
                canonical_envelope: message,
                semantic_sha256: "a".repeat(64),
            },
        )
        .unwrap();
        let envelopes = bus
            .reserve_owner_carries(session, client, 1, |message| {
                let mut envelope: cutex::app_server::external_input::Envelope =
                    serde_json::from_value(json!({
                        "version": 2, "ownerId": session, "threadId": "thread-c010",
                        "runtimeGeneration": 7,
                        "message": {"id": message.id,
                            "source": {"kind": "agent", "id": message.from_cutex_session_id},
                            "type": "message", "delivery": "after_turn", "text": message.content},
                        "view": {"schema": "cutex.owner-retry-fixture.v1",
                            "data": {"label": "immutable", "originTurnId": "prior-turn"}},
                        "semanticSha256": ""
                    }))
                    .unwrap();
                envelope.semantic_sha256 = envelope.digest();
                envelope.validate()?;
                Ok(envelope)
            })
            .unwrap();
        assert_eq!(envelopes.len(), 1);
        assert_eq!(envelopes[0].message.id, id);
        return OwnerCarryRetrySource::Bus(id);
    }

    assert_eq!(kind, "results");
    let store = cutex::asks::store::AskStore::open_default().unwrap();
    let now = Utc::now();
    let brief = "owner carry retry fixture";
    store
        .with_state(|state| {
            let (ask, _) = state
                .create_ask(
                    cutex::asks::store::NewAsk {
                        ask_id: None,
                        action_id: format!("owner-retry-{}", uuid::Uuid::new_v4()),
                        requester: cutex::asks::model::Requester::Agent {
                            cutex_session_id: session.into(),
                            name: Some("fixture requester".into()),
                        },
                        target_cutex_session_id: format!("cutex.{}", uuid::Uuid::new_v4()),
                        target_name: None,
                        project_id: None,
                        body: cutex::asks::model::AskBody {
                            text: brief.into(),
                            text_artifact: None,
                            text_bytes: brief.len(),
                            text_sha256: format!("{:x}", sha2::Sha256::digest(brief.as_bytes())),
                            attachments: Vec::new(),
                        },
                        title: None,
                        options: Default::default(),
                        follow_up_of: None,
                        parent_ask: None,
                        gate: None,
                    },
                    now,
                )
                .unwrap();
            state.update_ask(&ask.ask_id, now, |ask| {
                ask.execution = cutex::asks::model::ExecutionState::Answered;
            });
            cutex::asks::delivery::record_owner_stopped_delivery(state, &ask, now).unwrap();
        })
        .unwrap();

    // Match the production owner admission fence, without a running runtime.
    struct ReleaseFence;
    impl Drop for ReleaseFence {
        fn drop(&mut self) {
            cutex::asks::pause_admission::release_worker_fence();
        }
    }
    cutex::asks::pause_admission::acquire_worker_fence().unwrap();
    let _fence = ReleaseFence;
    let bundle = cutex::asks::owner_carries::reserve_owner_results(
        &store,
        session,
        client,
        7,
        cutex::app_server::external_input::size::ReceiverLimit::Off,
        cutex::config::documents::DocumentSettings::default(),
        now,
    )
    .unwrap()
    .expect("one real result reserved for this owner");
    OwnerCarryRetrySource::Results(bundle.event_id)
}

fn owner_carry_retry_source_snapshot(source: &OwnerCarryRetrySource) -> Value {
    match source {
        OwnerCarryRetrySource::None => Value::Null,
        OwnerCarryRetrySource::Bus(id) => {
            let bus =
                cutex::management::v2::agent_bus_state::agent_bus_message_repository().unwrap();
            let snapshot = bus.snapshot_by_message_id(id).unwrap().unwrap();
            let envelope = snapshot
                .external_input
                .as_ref()
                .expect("frozen native envelope");
            envelope.validate().unwrap();
            json!({"snapshot": snapshot,
                "carrierIds": bus.owner_carry_ids(&envelope.owner_id, "same-client-message").unwrap()})
        }
        OwnerCarryRetrySource::Results(id) => {
            let store = cutex::asks::store::AskStore::open_default().unwrap();
            let state = store.shared().unwrap();
            let event = state
                .events
                .get(id)
                .expect("reserved result event remains durable");
            let deliveries: Vec<_> = event
                .delivery_ids
                .iter()
                .map(|id| state.deliveries.get(id).unwrap())
                .collect();
            let artifact = store
                .read_artifact(event.event_artifact.as_deref().unwrap(), None)
                .unwrap()
                .unwrap()
                .1;
            json!({"event": event, "deliveries": deliveries, "artifact": artifact})
        }
    }
}

/// A crash between the source reservation and `record_reservation`: nothing
/// was attached to any native request. The owner's resend goes alone; the
/// source reservation stays, whatever the runtime could say about it.
#[test]
fn owner_carry_crash_before_the_ledger_write_stays_reserved_and_sends_alone() {
    if test_runs_in_private_home("owner_carry_crash_before_the_ledger_write_stays_reserved_and_sends_alone") {
        return;
    }
    use cutex::management::v2::input_ledger as il;
    let session = format!("cutex.{}", uuid::Uuid::new_v4());
    let client = "crash-window-owner";
    il::default_ledger().unwrap().transition(&session, client, il::Expect::NEW, il::Next::received().create(il::NewInput {
        kind: il::InputKind::Owner,
        sender: il::InputSender { kind: il::SenderKind::Owner, id: "owner".into(), name: "owner".into(), host: None, cutex_session_id: None },
        intent: il::InputIntent { strategy: Some(il::Strategy::Auto), delivery_mode: None, timing: il::Timing::NextStep },
        preview: "owner".into(),
        ask_id: None,
        remote: None,
    })).unwrap();
    let source = owner_carry_retry_reserve_source("bus", &session, client);
    let before = owner_carry_retry_source_snapshot(&source);
    assert!(super::super::owner_carries::OwnerCarries::has_reservation(&session, client).unwrap());
    assert!(super::super::owner_carries::OwnerCarries::goes_alone(&session, client).unwrap());
    assert_eq!(owner_carry_retry_source_snapshot(&source), before, "a check moves nothing");
}

#[test]
fn owner_carry_retry_retains_unresolved_without_capability_after_replacement() {
    if test_runs_in_private_home(
        "owner_carry_retry_retains_unresolved_without_capability_after_replacement",
    ) {
        return;
    }

    // Genuine source stores, both unresolved states and both binding shapes.
    // No-reservation controls prove this is not the image/queue short circuit
    // or a blanket ban on legacy idle-thread recovery.
    for source_kind in ["bus", "results", "none"] {
        for state_name in ["sending", "unknown"] {
            for replaced_thread in [false, true] {
                let case =
                    format!("owner-carry-retry-{source_kind}-{state_name}-{replaced_thread}");
                let (root, repository) = queue_repository(&case);
                let session = format!("cutex.{}", uuid::Uuid::new_v4());
                let command = retry_command(&session, "initial-request");
                let client = command.params.client_user_message_id.clone();
                repository
                    .register_identity(&session, &command.params)
                    .unwrap();
                let since = (Utc::now() - chrono::Duration::days(1)).to_rfc3339();
                let prior = if state_name == "sending" {
                    InputDelivery::Sending {
                        thread_id: command.thread_id.clone(),
                        since,
                    }
                } else {
                    InputDelivery::Unknown {
                        thread_id: command.thread_id.clone(),
                        since,
                    }
                };
                assert!(repository
                    .set_delivery(&session, &client, None, Some(prior.clone()))
                    .unwrap());
                let source = owner_carry_retry_reserve_source(source_kind, &session, &client);
                let before = owner_carry_retry_source_snapshot(&source);
                let reserved = source_kind != "none";
                assert_eq!(
                    super::super::owner_carries::OwnerCarries::has_reservation(&session, &client)
                        .unwrap(),
                    reserved
                );

                drop(repository);
                let repository = UserInputRepository::open(&root).unwrap();
                assert!(repository.list(&session).unwrap().1.is_empty());
                let mut retry = retry_command(&session, "replacement-request");
                if replaced_thread {
                    retry.thread_id = "replacement-thread".into();
                }
                assert!(!image_steering::contains_images(&retry.params.input));
                // No manager entry means no ownerInputCarries capability,
                // irrespective of which generation the caller asks about.
                for generation in [7, 8] {
                    assert!(
                        !session_manager(&session).owner_carries_supported(&session, generation)
                    );
                }

                if reserved {
                    for attempt in ["replacement-request", "second-retry"] {
                        retry.management_request_id = attempt.into();
                        let error = submit_v2_with_repository(
                            retry.clone(),
                            &repository,
                            |_, _| Some(None),
                            thread_with(json!([])),
                            never_sent,
                            no_flush,
                        )
                        .expect_err(
                            "reserved carrier must stop before the sender/capability fallback",
                        );
                        assert_eq!(error.code, "user_input_in_progress", "{case}");
                        assert_eq!(error.details["delivery"], state_name, "{case}");
                        assert_eq!(
                            repository.delivery(&session, &client).unwrap(),
                            Some(prior.clone()),
                            "{case}"
                        );
                        assert_eq!(owner_carry_retry_source_snapshot(&source), before, "{case}");
                        assert!(super::super::owner_carries::OwnerCarries::has_reservation(
                            &session, &client
                        )
                        .unwrap());
                    }
                } else {
                    let sent = std::cell::RefCell::new(Vec::new());
                    let execution = submit_v2_with_repository(
                        retry,
                        &repository,
                        |_, _| Some(None),
                        thread_with(json!([])),
                        recording_sender(&sent),
                        no_flush,
                    )
                    .expect("legacy non-carried text still recovers from an idle old send");
                    assert_eq!(
                        execution.disposition,
                        UserInputDisposition::Started,
                        "{case}"
                    );
                    assert_eq!(
                        *sent.borrow(),
                        vec![format!("turn/start {client}")],
                        "{case}"
                    );
                    assert!(matches!(repository.delivery(&session, &client).unwrap(),
                        Some(InputDelivery::Accepted { turn_id: Some(turn), .. }) if turn == "turn-new"));
                }
                drop(repository);
                std::fs::remove_dir_all(root).unwrap();
            }
        }
    }
}

/// Owner report 2026-11-08: the owner's resend of a carrier whose delivery never
/// resolved. Same proof as a non-carried send: the thread is idle, the grace is
/// over and the history this request read does not show it. It then goes alone:
/// the delivery is cleared, `carries` emptied, the source reservation untouched
/// (057 section 6). Anything short of that proof stays refused, moving nothing.
#[test]
fn owner_carry_resend_goes_alone_only_on_the_absence_proof() {
    if test_runs_in_private_home("owner_carry_resend_goes_alone_only_on_the_absence_proof") {
        return;
    }
    use cutex::management::v2::input_ledger as il;
    let old = || (Utc::now() - chrono::Duration::days(1)).to_rfc3339();
    let idle = || json!([]);
    let busy = || json!([{"id": "turn-running", "status": "inProgress", "items": []}]);
    // (name, resend, thread replaced, age, turns, goes out)
    let cases: Vec<(&str, bool, bool, String, Value, bool)> = vec![
        ("proven", true, false, old(), idle(), true),
        ("within-grace", true, false, Utc::now().to_rfc3339(), idle(), false),
        ("busy", true, false, old(), busy(), false),
        // That thread's history was not read, so nothing proves it absent.
        ("other-thread", true, true, old(), idle(), false),
        // Not the owner's resend: a blind repeat of the same id.
        ("not-a-resend", false, false, old(), idle(), false),
    ];
    for state_name in ["sending", "unknown"] {
        for (name, resending, replaced_thread, since, turns, goes_out) in &cases {
            let case = format!("owner-carry-alone-{state_name}-{name}");
            let (root, repository) = queue_repository(&case);
            let session = format!("cutex.{}", uuid::Uuid::new_v4());
            let mut command = retry_command(&session, "initial-request");
            let client = command.params.client_user_message_id.clone();
            repository.register_identity(&session, &command.params).unwrap();
            let prior = if state_name == "sending" {
                InputDelivery::Sending { thread_id: command.thread_id.clone(), since: since.clone() }
            } else {
                InputDelivery::Unknown { thread_id: command.thread_id.clone(), since: since.clone() }
            };
            assert!(repository.set_delivery(&session, &client, None, Some(prior.clone())).unwrap());
            let ledger = il::default_ledger().unwrap();
            ledger.transition(&session, &client, il::Expect::NEW, il::Next::received().create(il::NewInput {
                kind: il::InputKind::Owner,
                sender: il::InputSender { kind: il::SenderKind::Owner, id: "owner".into(), name: "owner".into(), host: None, cutex_session_id: None },
                intent: il::InputIntent { strategy: Some(il::Strategy::Auto), delivery_mode: None, timing: il::Timing::NextStep },
                preview: "owner".into(),
                ask_id: None,
                remote: None,
            })).unwrap();
            let mut carrying = il::Next::sending();
            carrying.carries = Some(vec!["the-note".into()]);
            ledger.transition(&session, &client, il::Expect::ANY, carrying).unwrap();
            ledger.transition(&session, &client, il::Expect::ANY, il::Next::uncertain("outcomeUnknown")).unwrap();
            if *resending {
                ledger.transition(&session, &client, il::Expect::ANY, il::Next::waiting("ownerRetry")).unwrap();
            }
            let source = owner_carry_retry_reserve_source("bus", &session, &client);
            let before = owner_carry_retry_source_snapshot(&source);
            if *replaced_thread {
                command.thread_id = "replacement-thread".into();
            }
            command.management_request_id = "the-resend".into();
            let sent = std::cell::RefCell::new(Vec::new());
            let result = submit_v2_with_repository(command, &repository, |_, _| Some(None), thread_with(turns.clone()), recording_sender(&sent), no_flush);
            let record = ledger.get(&session, std::slice::from_ref(&client)).unwrap().records.pop().unwrap();
            if *goes_out {
                let execution = result.unwrap_or_else(|error| panic!("{case}: {error:?}"));
                assert_eq!(execution.disposition, UserInputDisposition::Started, "{case}");
                assert_eq!(*sent.borrow(), vec![format!("turn/start {client}")], "{case}");
                assert!(record.carries.is_empty(), "{case}: {:?}", record.carries);
                assert!(super::super::owner_carries::OwnerCarries::goes_alone(&session, &client).unwrap(), "{case}");
                assert!(matches!(repository.delivery(&session, &client).unwrap(), Some(InputDelivery::Accepted { .. })), "{case}");
            } else {
                let error = result.expect_err(&case);
                assert_eq!(error.code, "user_input_in_progress", "{case}");
                assert_eq!(error.details["delivery"], state_name, "{case}");
                assert_eq!(error.details.get("retryCannotHelp").is_some(), *replaced_thread, "{case}: {}", error.details);
                assert!(sent.borrow().is_empty(), "{case}");
                assert_eq!(record.carries, vec!["the-note".to_string()], "{case}");
                assert_eq!(repository.delivery(&session, &client).unwrap(), Some(prior), "{case}");
            }
            assert_eq!(owner_carry_retry_source_snapshot(&source), before, "{case}: the reservation is untouched");
            drop(repository);
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}
