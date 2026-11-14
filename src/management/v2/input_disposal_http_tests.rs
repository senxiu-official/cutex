use super::*;
use std::io::Read;
use std::net::TcpListener;

#[test]
fn owner_dispose_http_ignores_busy_operation_lane_and_runtime_progress() {
    let id = format!("cutex.disposal-http-{}", uuid::Uuid::new_v4());
    let home = std::env::var("CUTEX_TEST_PRIVATE_HOME").unwrap();
    let mut record = crate::session::model::CutexSessionRecord::new(
        id.clone(),
        Some("disposal-thread".into()),
        crate::platform::host::current_host_name(),
        home,
        None,
    )
    .unwrap();
    record.registration_class = crate::agent_bus::model::AgentRegistrationClass::Persistent;
    let mut sessions = crate::session::store::with_cutex_session_store(Clone::clone).unwrap();
    sessions.sessions.insert(id.clone(), record);
    crate::session::store::save_cutex_session_store(&sessions).unwrap();
    let lane = cutex_request_lock(&id).unwrap();
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _guard = lane.reserve().unwrap();
        held_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    held_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let request = SimpleHttpRequest {method:"POST".into(),path:format!("/v2/sessions/{id}/cutex/requests"),
        headers:Default::default(),body:serde_json::to_vec(&json!({"requestId":"disposal-http", "method":"cutex/input/dispose","params":{"inputId":"not-projected-yet"}})).unwrap()};
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    let session = id.clone();
    let worker = std::thread::spawn(move || {
        let mut client = TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        let mut context = super::tests::safety_http_context();
        context.load_runtime_status = |_| panic!("disposal cannot wait for runtime status");
        let result = handle_cutex_request(
            &mut stream,
            &request,
            context,
            management_v2_repository().unwrap(),
            &session,
            Some(&crate::management::control_plane::HumanManagementPrincipal::authenticated()),
        );
        drop(stream);
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        reply_tx
            .send((result.map_err(|e| e.to_string()), response))
            .unwrap();
    });
    let timely = reply_rx.recv_timeout(Duration::from_secs(2));
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    worker.join().unwrap();
    let (result, response) = timely.expect("dispose must reply while operation lane remains held");
    result.unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("dispositionId"), "{response}");
    assert!(super::super::input_disposal::requested(&id, "not-projected-yet").unwrap());
}

#[test]
fn owner_disposition_is_self_idempotent_but_not_read_only() {
    for method in ["cutex/input/dispose", "cutex/input/dismiss"] {
        assert!(!super::super::request_idempotency::method_needs_idempotency(method));
        assert!(!super::super::request_idempotency::READ_ONLY_METHODS.contains(&method));
    }
    assert!(super::super::request_idempotency::method_needs_idempotency(
        "cutex/input/retry"
    ));
}
