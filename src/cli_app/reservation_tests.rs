use super::*;
use cutex::management::v2::user_input::{
    UserInputOrigin, UserInputOriginKind, UserInputRepository, UserInputSubmitParams,
};
use std::sync::{mpsc, Condvar, OnceLock};

#[derive(Default)]
struct NativeHold {
    entered: bool,
    released: bool,
    sends: usize,
    stops: usize,
}
static NATIVE_HOLD: OnceLock<(Mutex<NativeHold>, Condvar)> = OnceLock::new();

#[test]
fn queue_and_focus_reads_do_not_load_runtime_status_or_full_session_projection() {
    let home=crate::cli_app::test_home::IsolatedTestHome::new_private("route-snapshot").unwrap();
    let session=format!("cutex.{}",Uuid::new_v4());
    let mut record=cutex::session::model::CutexSessionRecord::new(session.clone(),None,current_host_name(),home.root().display().to_string(),None).unwrap();
    record.registration_class=cutex::agent_bus::model::AgentRegistrationClass::LocalOnly;
    let mut store=cutex::session::model::CutexSessionStore::default();store.sessions.insert(session.clone(),record);
    cutex::session::store::save_cutex_session_store(&store).unwrap();
    management_v2_repository().unwrap().materialize_recovery_reset(&[],true).unwrap();
    let mut context=super::control_load_tests::http_context();
    context.load_runtime_status=|_|panic!("read-only route must not load a native/full session projection");
    let path=format!("/v2/sessions/{session}/cutex/requests");
    for method in ["cutex/userInput/queue/list","cutex/focus/get"] {
        let response=super::control_load_tests::http_post_with_context(&path,json!({"requestId":method,"method":method,"params":{}}),context);
        assert_eq!(response.0,200,"{method}: {}",response.1);
        if method.ends_with("queue/list") {assert_eq!(response.1["cutex"]["result"]["items"],json!([]),"{}",response.1);}
    }
}

fn ordered_stop(session: &str, thread: &str) -> Result<Option<String>, UserInputExecutionError> {
    assert_eq!(thread, session.trim_start_matches("cutex."));
    let mut hold = NATIVE_HOLD.get().unwrap().0.lock().unwrap();
    assert!(hold.released && hold.sends == 1);
    assert!(
        matches!(
            user_input_repository()
                .unwrap()
                .delivery(session, "reserved-client")
                .unwrap(),
            Some(InputDelivery::Accepted { .. })
        ),
        "stop cannot overtake native/durable delivery settlement"
    );
    hold.stops += 1;
    Ok(Some("busy-turn".into()))
}

fn blocked_submit(
    command: UserInputSubmitCommand,
) -> Result<UserInputSubmitExecution, UserInputExecutionError> {
    let lane = session_input_lock(&command.cutex_session_id).unwrap();
    let _nested = lane.reserve().unwrap();
    submit_v2_with_repository(
        command,
        user_input_repository().unwrap(),
        |_, _| Some(Some("busy-turn".into())),
        |_, _| panic!("new tracked input must not read summary"),
        |_, _| {
            Ok(json!({"thread":{"turns":[{"id":"busy-turn","status":"inProgress","items":[]}]}}))
        },
        |_, method, _, disposition| {
            let (mutex, changed) = NATIVE_HOLD.get_or_init(Default::default);
            let mut hold = mutex.lock().unwrap();
            hold.entered = true;
            hold.sends += 1;
            changed.notify_all();
            let (hold, timeout) = changed
                .wait_timeout_while(hold, Duration::from_secs(15), |hold| !hold.released)
                .unwrap();
            assert!(
                !timeout.timed_out() && hold.released,
                "native release barrier timed out"
            );
            Ok(UserInputSubmitExecution {
                join_refusal: None,
                disposition,
                app_server_accepted: true,
                native_request_id: Some(json!("barrier-native")),
                native_method: Some(method.into()),
                turn_id: Some("busy-turn".into()),
                queue: None,
                queue_paused: false,
            })
        },
        |_, _| panic!("busy input must not flush"),
    )
}

#[test]
fn same_session_snapshot_reads_finish_during_native_rpc_and_duplicate_sends_once() {
    let home =
        crate::cli_app::test_home::IsolatedTestHome::new_private("reservation-http").unwrap();
    let session = format!("cutex.{}", Uuid::new_v4());
    let mut record = cutex::session::model::CutexSessionRecord::new(
        session.clone(),
        Some(session.trim_start_matches("cutex.").into()),
        current_host_name(),
        home.root().display().to_string(),
        None,
    )
    .unwrap();
    record.registration_class = cutex::agent_bus::model::AgentRegistrationClass::LocalOnly;
    record.runtime_generation = 1;
    record.exposed_to_backend = false;
    let mut store = cutex::session::model::CutexSessionStore::default();
    store.sessions.insert(session.clone(), record);
    cutex::session::store::save_cutex_session_store(&store).unwrap();
    management_v2_repository()
        .unwrap()
        .materialize_recovery_reset(&[], true)
        .unwrap();
    let path = format!("/v2/sessions/{session}/cutex/requests");
    let body = json!({"requestId":"reserved-submit", "method":"cutex/userInput/submit", "params":{
        "clientUserMessageId":"reserved-client", "origin":{"kind":"other","clientId":"reservation-test"},
        "strategy":"auto", "input":[{"type":"text","text":"fixture","text_elements":[]}]}});
    let mut context = super::control_load_tests::http_context();
    context.handle_user_input = blocked_submit;
    context.interrupt_active_turn = ordered_stop;
    let (mutex, changed) = NATIVE_HOLD.get_or_init(Default::default);
    *mutex.lock().unwrap() = NativeHold::default();
    std::thread::scope(|scope| {
        let submit = scope.spawn(|| {
            super::control_load_tests::http_post_with_context(&path, body.clone(), context)
        });
        let (hold, timeout) = changed
            .wait_timeout_while(mutex.lock().unwrap(), Duration::from_secs(5), |hold| {
                !hold.entered
            })
            .unwrap();
        assert!(
            !timeout.timed_out() && hold.entered,
            "submit did not reach native barrier"
        );
        drop(hold);
        let lane = session_input_lock(&session).unwrap();
        assert!(
            lane.snapshot().unwrap().active.is_some(),
            "coordinator is observable during RPC"
        );
        let (tx, rx) = mpsc::channel();
        for method in [
            "cutex/session/get",
            "cutex/session/groups/get",
            "cutex/focus/get",
            "cutex/userInput/queue/list",
        ] {
            let tx = tx.clone();
            let path = &path;
            scope.spawn(move || {
                let result = super::control_load_tests::http_post_with_context(
                    path,
                    json!({"requestId":format!("read-{method}"),"method":method,"params":{}}),
                    super::control_load_tests::http_context(),
                );
                tx.send((method, result)).unwrap();
            });
        }
        let duplicate = scope.spawn(|| {
            super::control_load_tests::http_post_with_context(&path, body.clone(), context)
        });
        let stop = scope.spawn(|| {
            super::control_load_tests::http_post_with_context(
                &path,
                json!({"requestId":"ordered-stop","method":"cutex/turn/stop","params":{}}),
                context,
            )
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while lane.snapshot().unwrap().waiting < 2 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        let queued = lane.snapshot().unwrap().waiting;
        let results: Vec<_> = (0..4)
            .map(|_| rx.recv_timeout(Duration::from_secs(5)))
            .collect();
        // Release before asserting, so a regression does not strand fixture IO.
        mutex.lock().unwrap().released = true;
        changed.notify_all();
        assert!(
            queued >= 2,
            "duplicate and stop must reach the admission barrier"
        );
        for result in results {
            let (method, response) = result.expect("snapshot read blocked behind native RPC");
            assert_eq!(response.0, 200, "{method}: {}", response.1);
        }
        let first = submit.join().unwrap();
        let replay = duplicate.join().unwrap();
        assert_eq!(first.0, 200, "{}", first.1);
        assert_eq!(
            replay, first,
            "same request must replay its durable receipt"
        );
        let stopped = stop.join().unwrap();
        assert_eq!(stopped.0, 200, "{}", stopped.1);
    });
    let mut conflict = body.clone();
    conflict["params"]["input"][0]["text"] = json!("changed input");
    assert_eq!(
        super::control_load_tests::http_post_with_context(&path, conflict.clone(), context).0,
        409
    );
    conflict["requestId"] = json!("different-request-conflicting-client");
    assert_eq!(
        super::control_load_tests::http_post_with_context(&path, conflict, context).0,
        409
    );
    let mut retry = body;
    retry["requestId"] = json!("new-request-same-client");
    let dedup = super::control_load_tests::http_post_with_context(&path, retry, context);
    assert_eq!(dedup.0, 200, "{}", dedup.1);
    assert_eq!(mutex.lock().unwrap().sends, 1);
    assert_eq!(mutex.lock().unwrap().stops, 1);
    let snapshot = session_input_lock(&session).unwrap().snapshot().unwrap();
    assert!(snapshot.active.is_none() && snapshot.waiting == 0);
}

fn crash_command(session: &str) -> UserInputSubmitCommand {
    UserInputSubmitCommand {
        management_request_id: "crash-request".into(),
        cutex_session_id: session.into(),
        thread_id: "crash-thread".into(),
        params: UserInputSubmitParams {
            join_delegation: None,
            client_user_message_id: "crash-client".into(),
            origin: UserInputOrigin {
                kind: UserInputOriginKind::Other,
                client_id: "crash-fixture".into(),
            },
            strategy: UserInputStrategy::Auto,
            input: vec![json!({"type":"text","text":"fixture","text_elements":[]})],
            expected_turn_id: None,
        },
    }
}

fn blocked_raw_forward(
    session: &str,
    message: Value,
) -> Result<Value, cutex::management::server::ManagementNativeForwardError> {
    crate::cli_app::management_context::with_native_mutation_reservation(
        session,
        message,
        |message| {
            if message["method"] != "thread/read" {
                let (mutex, changed) = NATIVE_HOLD.get_or_init(Default::default);
                let mut hold = mutex.lock().unwrap();
                hold.entered = true;
                hold.sends += 1;
                changed.notify_all();
                let (hold, timeout) = changed
                    .wait_timeout_while(hold, Duration::from_secs(15), |hold| !hold.released)
                    .unwrap();
                assert!(
                    !timeout.timed_out() && hold.released,
                    "raw native release barrier timed out"
                );
            }
            Ok(json!({"id":message["id"], "result":{}}))
        },
    )
}

fn ordered_raw_stop(
    session: &str,
    thread: &str,
) -> Result<Option<String>, UserInputExecutionError> {
    assert_eq!(thread, session.trim_start_matches("cutex."));
    let mut hold = NATIVE_HOLD.get().unwrap().0.lock().unwrap();
    assert!(hold.released && hold.sends == 1);
    hold.stops += 1;
    Ok(Some("busy-turn".into()))
}

#[test]
fn raw_native_mutation_serializes_with_stop_but_thread_reads_progress() {
    let home =
        crate::cli_app::test_home::IsolatedTestHome::new_private("reservation-raw-http").unwrap();
    let session = format!("cutex.{}", Uuid::new_v4());
    let thread = session.trim_start_matches("cutex.");
    let mut record = cutex::session::model::CutexSessionRecord::new(
        session.clone(),
        Some(thread.into()),
        current_host_name(),
        home.root().display().to_string(),
        None,
    )
    .unwrap();
    record.registration_class = cutex::agent_bus::model::AgentRegistrationClass::LocalOnly;
    record.runtime_generation = 1;
    record.exposed_to_backend = false;
    let mut store = cutex::session::model::CutexSessionStore::default();
    store.sessions.insert(session.clone(), record);
    cutex::session::store::save_cutex_session_store(&store).unwrap();
    management_v2_repository()
        .unwrap()
        .materialize_recovery_reset(&[], true)
        .unwrap();
    let path = format!("/v2/sessions/{session}/app-server/requests");
    let control_path = format!("/v2/sessions/{session}/cutex/requests");
    let body = json!({"requestId":"raw-steer","native":{"message":{"id":1,"method":"turn/steer","params":{
        "threadId":thread,"expectedTurnId":"busy-turn","input":[{"type":"text","text":"fixture","text_elements":[]}]}}}});
    let mut context = super::control_load_tests::http_context();
    context.forward_native_request = blocked_raw_forward;
    context.interrupt_active_turn = ordered_raw_stop;
    let (mutex, changed) = NATIVE_HOLD.get_or_init(Default::default);
    *mutex.lock().unwrap() = NativeHold::default();
    std::thread::scope(|scope| {
        let native = scope.spawn(|| {
            super::control_load_tests::http_post_with_context(&path, body.clone(), context)
        });
        let (hold, timeout) = changed
            .wait_timeout_while(mutex.lock().unwrap(), Duration::from_secs(5), |hold| {
                !hold.entered
            })
            .unwrap();
        assert!(!timeout.timed_out() && hold.entered);
        drop(hold);
        let stop = scope.spawn(|| {
            super::control_load_tests::http_post_with_context(
                &control_path,
                json!({"requestId":"raw-ordered-stop","method":"cutex/turn/stop","params":{}}),
                context,
            )
        });
        let lane = session_input_lock(&session).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while lane.snapshot().unwrap().waiting == 0 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        let waiting = lane.snapshot().unwrap().waiting;
        let (tx, rx) = mpsc::channel();
        let read_path = &path;
        scope.spawn(move || {
            tx.send(super::control_load_tests::http_post_with_context(read_path,
                json!({"requestId":"raw-read","native":{"message":{"id":2,"method":"thread/read","params":{"threadId":thread}}}}), context)).unwrap();
        });
        let read = rx.recv_timeout(Duration::from_secs(5));
        mutex.lock().unwrap().released = true;
        changed.notify_all();
        assert_eq!(waiting, 1, "stop must queue behind the raw native mutation");
        assert_eq!(
            read.expect("native read blocked behind mutating RPC").0,
            200
        );
        assert_eq!(native.join().unwrap().0, 200);
        assert_eq!(stop.join().unwrap().0, 200);
    });
    assert_eq!(
        super::control_load_tests::http_post_with_context(&path, body, context).0,
        200
    );
    assert_eq!(
        mutex.lock().unwrap().sends,
        1,
        "raw request replay must not forward twice"
    );
    assert_eq!(mutex.lock().unwrap().stops, 1);
}

fn recover_input_from_disk(
    source: &std::path::Path,
    fresh: &std::path::Path,
) -> UserInputRepository {
    // A fresh path defeats the process-local file-stamp cache, so this checks
    // the durable delivery record rather than its surviving in-memory Arc.
    std::fs::create_dir_all(fresh).unwrap();
    std::fs::copy(
        source.join("user-input-state.json"),
        fresh.join("user-input-state.json"),
    )
    .unwrap();
    UserInputRepository::open(fresh).unwrap()
}

#[test]
fn restart_after_native_acceptance_before_receipt_never_authorizes_a_second_send() {
    use cutex::management::v2::request_idempotency::{BeginRequest, RequestIdempotencyRepository};
    let home =
        crate::cli_app::test_home::IsolatedTestHome::new_private("reservation-crash").unwrap();
    let root = home.root().join("operation");
    let session = format!("cutex.{}", Uuid::new_v4());
    let request_body = json!({"requestId":"crash-request","method":"cutex/userInput/submit"});
    let receipt =
        RequestIdempotencyRepository::open(root.join("receipts"), "old-process".into()).unwrap();
    assert!(matches!(
        receipt
            .begin(&session, "crash-request", &request_body)
            .unwrap(),
        BeginRequest::Forward(_)
    ));
    let repository = UserInputRepository::open(root.join("input")).unwrap();
    let sends = std::cell::Cell::new(0);
    let result = submit_v2_with_repository(
        crash_command(&session),
        &repository,
        |_, _| Some(None),
        |_, _| panic!("new idle input needs no summary"),
        |_, _| panic!("new idle input needs no history"),
        |_, _, _, _| {
            sends.set(sends.get() + 1);
            // Simulate acceptance with a lost response: no safe retry proof.
            Err(execution_error_message(
                "native_submit",
                "transport_lost",
                "injected lost response",
                false,
                true,
            ))
        },
        |_, _| Ok(false),
    );
    assert!(result.unwrap_err().outcome_unknown);
    drop(repository);
    drop(receipt);
    let restarted =
        RequestIdempotencyRepository::open(root.join("receipts"), "new-process".into()).unwrap();
    assert!(matches!(
        restarted
            .begin(&session, "crash-request", &request_body)
            .unwrap(),
        BeginRequest::OutcomeUnknown
    ));
    let recovered = recover_input_from_disk(&root.join("input"), &root.join("restarted-input"));
    assert!(matches!(
        recovered.delivery(&session, "crash-client").unwrap(),
        Some(InputDelivery::Unknown { .. })
    ));
    let lane = session_input_lock(&session).unwrap();
    let _operation = lane.reserve().unwrap();
    let mut retry = crash_command(&session);
    retry.management_request_id = "new-request".into();
    let pending = submit_v2_with_repository(
        retry,
        &recovered,
        |_, _| Some(None),
        |_, _| Ok(None),
        |_, _| Ok(json!({"thread":{"turns":[]}})),
        |_, _, _, _| panic!("recent unknown delivery must block resend after restart"),
        |_, _| Ok(false),
    )
    .unwrap_err();
    assert_eq!(pending.code, "user_input_in_progress");
    assert_eq!(sends.get(), 1);
}

#[test]
fn reopening_sending_reservation_refuses_retry_before_history_proves_absence() {
    let home =
        crate::cli_app::test_home::IsolatedTestHome::new_private("reservation-sending-crash")
            .unwrap();
    let root = home.root().join("input");
    let session = format!("cutex.{}", Uuid::new_v4());
    let command = crash_command(&session);
    let repository = UserInputRepository::open(&root).unwrap();
    assert_eq!(
        repository
            .register_identity(&session, &command.params)
            .unwrap(),
        ClientIdentityDecision::New
    );
    let sending = InputDelivery::Sending {
        thread_id: command.thread_id.clone(),
        since: Utc::now().to_rfc3339(),
    };
    assert!(repository
        .set_delivery(&session, "crash-client", None, Some(sending.clone()))
        .unwrap());
    drop(repository); // crash before forwarding, or before recording its result.
    let recovered = recover_input_from_disk(&root, &home.root().join("restarted-input"));
    let lane = session_input_lock(&session).unwrap();
    let _reservation = lane.reserve().unwrap();
    let pending = submit_v2_with_repository(
        command,
        &recovered,
        |_, _| Some(None),
        |_, _| Ok(None),
        |_, _| Ok(json!({"thread":{"turns":[]}})),
        |_, _, _, _| panic!("unresolved durable Sending is not permission to resend"),
        |_, _| Ok(false),
    )
    .unwrap_err();
    assert_eq!(pending.code, "user_input_in_progress");
    assert_eq!(
        recovered.delivery(&session, "crash-client").unwrap(),
        Some(sending)
    );
}
