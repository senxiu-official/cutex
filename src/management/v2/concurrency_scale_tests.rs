//! Explicit, loopback-only management latency experiment with a stalled reader.
use super::tests::safety_http_context;
use crate::catalog::{CatalogEndpoint, CatalogError};
use crate::management::listeners::{ConnectionInfo, ConnectionSlot};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Default)]
struct Fixture {
    release: Mutex<bool>,
    ready: Condvar,
}
fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(Default::default)
}
struct FakeRuntime(String);
impl CatalogEndpoint for FakeRuntime {
    fn request(&mut self, method: &str, _: Value) -> Result<Value, CatalogError> {
        if self.0 == "cutex.conc-stuck" {
            let state = fixture().release.lock().unwrap();
            let (_state, expired) = fixture().ready.wait_timeout_while(state, Duration::from_secs(60), |release| !*release).unwrap();
            if expired.timed_out() {
                return Err(CatalogError::Timeout { method: method.into() });
            }
        }
        match method {
            "thread/read" => Ok(json!({"thread":{"id":self.0,"turns":[]}})),
            "thread/turns/list" => Ok(json!({"data":[],"nextCursor":null})),
            _ => panic!("unexpected fake runtime method: {method}"),
        }
    }
    fn notify(&mut self, _: &str, _: Option<Value>) -> Result<(), CatalogError> { Ok(()) }
}
fn serve(mut stream: TcpStream, info: ConnectionInfo, mut slot: ConnectionSlot) {
    let request = crate::http::server::read_simple_http_request(&mut stream).unwrap();
    let mut context = safety_http_context();
    context.open_offline_history = |session| Ok((Box::new(FakeRuntime(session.into())), std::env::var_os("HOME").unwrap().into()));
    context.load_latest_plan = |_| Value::Null;
    context.load_latest_token_usage = |_| Value::Null;
    crate::management::server::handle_admitted_management_request(
        &mut stream, request, Some("fixture"), Some("fixture"), context, Some(info.peer), Some(&mut slot),
    ).unwrap();
}
fn get(address: std::net::SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(address).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(25))).unwrap();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer fixture\r\nConnection: close\r\n\r\n").unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}
fn control_latencies(address: std::net::SocketAddr) -> Value {
    let mut samples = Vec::new();
    for _ in 0..200 {
        let started = Instant::now();
        let response = get(address, "/v2/diagnostics/control");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(f64::total_cmp);
    json!({"requests":samples.len(),"p50Ms":samples[99],"p99Ms":samples[197],"maxMs":samples[199]})
}

#[test]
#[ignore = "explicit management latency experiment; run alone with a marked private HOME"]
fn one_stalled_session_management_latency() {
    let home = std::path::PathBuf::from(std::env::var_os("CUTEX_TEST_PRIVATE_HOME").unwrap());
    assert!(home.join(".cutex-test-private-home").is_file());
    assert_eq!(std::env::var_os("HOME").unwrap(), home.as_os_str());
    let mut sessions = crate::session::store::load_cutex_session_store().unwrap();
    for id in ["cutex.conc-stuck", "cutex.conc-healthy"] {
        let record = crate::session::model::CutexSessionRecord::new(
            id.into(), Some(id.into()), crate::platform::host::current_host_name(), home.to_string_lossy().into_owned(), None,
        ).unwrap();
        sessions.sessions.insert(id.into(), record);
    }
    crate::session::store::save_cutex_session_store(&sessions).unwrap();
    let descriptor = crate::management::v2::event_blobs::EventBlobStore::open_default().unwrap()
        .put("cutex.conc-stuck", "image/png", b"fixture-image").unwrap();
    let sha = descriptor["sha256"].as_str().unwrap();
    let media_path = format!("/v2/sessions/cutex.conc-stuck/media/{sha}");
    let blob_path = format!("/v2/sessions/cutex.conc-stuck/blobs/{sha}");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    // Match the ordinary loopback listener's blocking accept. The stoppable
    // Tailscale listener polls at 100 ms and would bias sequential latency.
    let listener = std::thread::spawn(move || {
        for _ in 0..406 {
            let (stream, peer) = listener.accept().unwrap();
            let slot = ConnectionSlot::acquire().unwrap();
            std::thread::spawn(move || serve(stream, ConnectionInfo { remote: false, peer }, slot));
        }
    });
    let baseline = control_latencies(address);
    let blocked: Vec<_> = (0..2).map(|_| std::thread::spawn(move || get(address, "/v2/sessions/cutex.conc-stuck/history?media=inline"))).collect();
    let deadline = Instant::now() + Duration::from_secs(5);
    while {
        let capacity = crate::management::v2::control_capacity::snapshot();
        capacity["heavyReadsActive"] != 1 || capacity["heavyReadsWaiting"] != 1
    } {
        assert!(Instant::now() < deadline, "stalled reads were not admitted");
        std::thread::sleep(Duration::from_millis(5));
    }
    let stalled = control_latencies(address);
    let mut same_session_media = Vec::new();
    for path in [&media_path, &blob_path] {
        let started = Instant::now();
        let response = get(address, path);
        same_session_media.push(json!({"path":path,"status":response.lines().next(),"latencyMs":started.elapsed().as_secs_f64()*1000.0}));
        assert!(response.starts_with("HTTP/1.1 200"), "stored same-session media was refused behind native history: {response}");
        assert!(response.ends_with("fixture-image"));
    }
    let started = Instant::now();
    let healthy = get(address, "/v2/sessions/cutex.conc-healthy/history?media=inline");
    let healthy_ms = started.elapsed().as_secs_f64() * 1000.0;
    *fixture().release.lock().unwrap() = true;
    fixture().ready.notify_all();
    let blocked_responses: Vec<_> = blocked.into_iter().map(|worker| worker.join().unwrap()).collect();
    for response in &blocked_responses { assert!(response.starts_with("HTTP/1.1 200"), "{response}"); }
    let recovered = get(address, "/v2/sessions/cutex.conc-healthy/history?media=inline");
    assert!(recovered.starts_with("HTTP/1.1 200"), "{recovered}");
    listener.join().unwrap();
    for id in ["cutex.conc-stuck", "cutex.conc-healthy"] { crate::management::v2::history::evict_offline_reader(id); }
    let report = json!({"baselineControl":baseline,"stalledControl":stalled,
        "healthyHistoryWhileStalled":{"latencyMs":healthy_ms,"status":healthy.lines().next(),"capacityRefusal":healthy.contains("heavy_read_wait_timeout")},
        "sameSessionMediaWhileStalled":same_session_media,
        "recoveredHistoryStatus":recovered.lines().next(),"stalledSessionRequests":2,"fakeRuntime":"CatalogEndpoint blocks metadata read; no external process or live daemon"});
    println!("{report}");
    if let Some(path) = std::env::var_os("CUTEX_CONCURRENCY_REPORT") { std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap(); }
    assert!(healthy.starts_with("HTTP/1.1 200"), "unrelated session must retain capacity: {healthy}");
}
