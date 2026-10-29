// Real loopback HTTP/parser -> production connection/bulk admission -> V2
// resource/history/submit handlers. Native history I/O alone is synthetic.
use super::*;
use cutex::management::server::{ManagementNativeForwardError, ManagementRequestContext};
use cutex::management::v2::control_capacity;
use std::cell::Cell;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Barrier, Condvar, OnceLock};

#[derive(Default)]
struct NativeGate {
    held: bool,
    entered: usize,
    expired: bool,
    entry_us: Vec<u128>,
}
static GATE: OnceLock<(Mutex<NativeGate>, Condvar)> = OnceLock::new();
thread_local! { static REQUEST_START: Cell<Option<Instant>> = const { Cell::new(None) }; }

fn gate() -> &'static (Mutex<NativeGate>, Condvar) {
    GATE.get_or_init(Default::default)
}
fn release() {
    let (state, ready) = gate();
    state.lock().unwrap().held = false;
    ready.notify_all();
}
struct ReleaseOnDrop;
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        release();
    }
}

fn history_forward(id: &str, request: Value) -> Result<Value, ManagementNativeForwardError> {
    // Produce real bounded turn/item data, not a sleeping replacement for the gate.
    let turns: Vec<Value> = (0..12)
        .map(|n| {
            json!({"id":format!("turn-{n}"),"status":"completed",
        "itemsView":"full","items":[{"type":"agentMessage","id":format!("item-{n}"),
            "text":"bounded native history fixture ".repeat(64)}]})
        })
        .collect();
    let thread = id.trim_start_matches("cutex.");
    let method = request["method"].as_str().unwrap();
    if method == "thread/read" {
        let (state, ready) = gate();
        let mut state = state.lock().unwrap();
        state.entered += 1;
        REQUEST_START.with(|start| {
            if let Some(start) = start.take() {
                state.entry_us.push(start.elapsed().as_micros());
            }
        });
        ready.notify_all();
        let (mut state, timeout) = ready
            .wait_timeout_while(state, Duration::from_millis(2500), |state| state.held)
            .unwrap();
        if timeout.timed_out() && state.held {
            state.expired = true;
        }
    }
    let result = match method {
        "thread/read" => json!({"thread":{"id":thread,"turns":
            if request["params"]["includeTurns"] == true { turns } else { vec![] }}}),
        "thread/turns/list" => {
            let offset = request["params"]["cursor"]
                .as_str()
                .map(|s| s.parse::<usize>().unwrap())
                .unwrap_or(0);
            let limit = request["params"]["limit"].as_u64().unwrap().min(100) as usize;
            let data: Vec<_> = turns.into_iter().rev().skip(offset).take(limit).collect();
            json!({"nextCursor":(offset+data.len()<12).then(||(offset+data.len()).to_string()),"data":data})
        }
        "thread/items/list" => json!({"data":[{"turnId":request["params"]["turnId"],
            "item":{"id":"item-paged","type":"agentMessage","text":"bounded item".repeat(64)}}],"nextCursor":null}),
        other => panic!("unexpected history method {other}"),
    };
    // Native JSON framing/decoding remains part of the fixture cost.
    Ok(serde_json::from_slice(
        &serde_json::to_vec(&json!({"id":request["id"],"result":result})).unwrap(),
    )
    .unwrap())
}

fn context() -> ManagementRequestContext {
    let mut context = super::control_load_tests::http_context();
    context.forward_native_request = history_forward;
    context.load_bootstrap_state =
        |_, _| Ok(json!({"userInputQueue":[],"pendingServerRequests":[],"agentBusMessages":[]}));
    context.load_latest_plan = |_| Value::Null;
    context.load_latest_token_usage = |_| Value::Null;
    context
}

fn exchange(method: &str, path: &str, body: Value) -> (u16, Value, u128) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let started = Instant::now();
    std::thread::scope(|scope| {
        let server = scope.spawn(move || {
            let (mut stream, peer) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(15)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(15)))
                .unwrap();
            cutex::http::server::set_keep_alive(cutex::http::server::KeepAlive::Off);
            cutex::http::server::set_accept_gzip(false);
            let request = cutex::http::server::read_simple_http_request(&mut stream).unwrap();
            let mut slot = cutex::management::listeners::ConnectionSlot::acquire().unwrap();
            REQUEST_START.with(|start| start.set(Some(Instant::now())));
            cutex::management::server::handle_admitted_management_request(
                &mut stream,
                request,
                Some("isolated-fixture-root"),
                Some("isolated-fixture-root"),
                context(),
                Some(peer),
                Some(&mut slot),
            )
            .unwrap();
            REQUEST_START.with(|start| start.set(None));
        });
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        let payload = if method == "GET" {
            String::new()
        } else {
            body.to_string()
        };
        write!(stream,"{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer isolated-fixture-root\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",payload.len()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        server.join().unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        (
            head.split_whitespace().nth(1).unwrap().parse().unwrap(),
            serde_json::from_str(body).unwrap(),
            started.elapsed().as_micros(),
        )
    })
}

fn count(value: &Value, key: &str) -> u64 {
    value[key].as_u64().unwrap()
}
fn percentile(values: &[u128], percent: usize) -> u128 {
    values[(values.len() * percent).div_ceil(100).saturating_sub(1)]
}

fn run_read_burst(readers: usize, rounds: usize) {
    let home =
        crate::cli_app::test_home::IsolatedTestHome::new_private("desktop-read-queue").unwrap();
    let sessions: Vec<_> = (0..readers)
        .map(|_| format!("cutex.{}", Uuid::new_v4()))
        .collect();
    let mut store = cutex::session::model::CutexSessionStore::default();
    for id in &sessions {
        let mut record = cutex::session::model::CutexSessionRecord::new(
            id.clone(),
            Some(id.trim_start_matches("cutex.").into()),
            current_host_name(),
            home.root().display().to_string(),
            None,
        )
        .unwrap();
        record.registration_class = cutex::agent_bus::model::AgentRegistrationClass::LocalOnly;
        record.runtime_generation = 1;
        record.exposed_to_backend = false;
        store.sessions.insert(id.clone(), record);
    }
    cutex::session::store::save_cutex_session_store(&store).unwrap();
    management_v2_repository()
        .unwrap()
        .materialize_recovery_reset(&[], true)
        .unwrap();
    let mut samples = Vec::new();
    let mut control_samples = Vec::new();
    let mut entry_samples = Vec::new();
    let before = control_capacity::snapshot();
    for round in 0..rounds {
        *gate().0.lock().unwrap() = NativeGate {
            held: true,
            ..Default::default()
        };
        let barrier = Arc::new(Barrier::new(readers + 1));
        std::thread::scope(|scope| {
            let mut requests = Vec::new();
            for (n, id) in sessions.iter().enumerate() {
                let barrier = Arc::clone(&barrier);
                requests.push(scope.spawn(move || {
                    barrier.wait();
                    let endpoint = if n % 2 == 0 {
                        "bootstrap?turnLimit=8"
                    } else {
                        "history?limit=8"
                    };
                    exchange("GET", &format!("/v2/sessions/{id}/{endpoint}"), Value::Null)
                }));
            }
            let release_on_failure = ReleaseOnDrop;
            barrier.wait();
            let deadline = Instant::now() + Duration::from_millis(1500);
            loop {
                let snapshot = control_capacity::snapshot();
                if count(&snapshot, "heavyReadsActive") == 2
                    && count(&snapshot, "heavyReadsWaiting") >= (readers - 2) as u64
                    && gate().0.lock().unwrap().entered >= 2
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "readers did not reach real FIFO queue: {snapshot}"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            let queued = control_capacity::snapshot();
            let path = format!("/v2/sessions/{}/cutex/requests", sessions[0]);
            for (name, params) in [
                ("cutex/userInput/queue/list", json!({})),
                (
                    "cutex/userInput/submit",
                    json!({"clientUserMessageId":format!("queue-control-{}-{round}",sessions[0]),
                    "origin":{"kind":"other","clientId":"read-queue-fixture"},"strategy":"auto",
                    "input":[{"type":"text","text":"control while heavy reads are held","text_elements":[]}]}),
                ),
            ] {
                let response = exchange(
                    "POST",
                    &path,
                    json!({"requestId":Uuid::new_v4().to_string(),"method":name,"params":params}),
                );
                assert_eq!(response.0, 200, "control failed: {}", response.1);
                control_samples.push(response.2);
                assert!(
                    gate().0.lock().unwrap().held,
                    "control completed only after native reads were released"
                );
            }
            assert_eq!(
                count(&control_capacity::snapshot(), "heavyReadsQueuedTotal"),
                count(&queued, "heavyReadsQueuedTotal"),
                "control entered heavy queue"
            );
            release();
            drop(release_on_failure);
            for request in requests {
                let response = request.join().unwrap();
                assert_eq!(response.0, 200, "desktop read failed: {}", response.1);
                let turns = response
                    .1
                    .pointer("/thread/turns")
                    .or_else(|| response.1.pointer("/native/result/thread/turns"));
                assert!(
                    turns
                        .and_then(Value::as_array)
                        .is_some_and(|turns| !turns.is_empty()),
                    "missing real history data: {}",
                    response.1
                );
                samples.push(response.2);
            }
        });
        let state = gate().0.lock().unwrap();
        assert!(
            !state.expired,
            "native handshake expired before controls completed"
        );
        entry_samples.extend_from_slice(&state.entry_us);
    }
    let after = control_capacity::snapshot();
    assert!(count(&after, "heavyReadsQueuedTotal") > count(&before, "heavyReadsQueuedTotal"));
    for key in ["heavyReadWaitTimeouts", "heavyReadQueueFull", "refused"] {
        assert_eq!(count(&after, key), count(&before, key), "{key} changed");
    }
    assert_eq!(count(&after, "heavyReadsActive"), 0);
    assert_eq!(count(&after, "heavyReadsWaiting"), 0);
    samples.sort_unstable();
    control_samples.sort_unstable();
    entry_samples.sort_unstable();
    eprintln!(
        "cutex-control-load {}",
        json!({"schema":1,"kind":"heavy_read_queue","operation":"bootstrap+history+control",
        "sessions":readers,"rounds":rounds,"http200":samples.len(),"http503":0,
        "control200":control_samples.len(),"p50Us":percentile(&samples,50),"p95Us":percentile(&samples,95),
        "controlP50Us":percentile(&control_samples,50),"controlP95Us":percentile(&control_samples,95),
        "nativeEntryP50Us":percentile(&entry_samples,50),"nativeEntryP95Us":percentile(&entry_samples,95),
        "nativeEntryIncludesResourcePreparation":true,
        "queued":count(&after,"heavyReadsQueuedTotal")-count(&before,"heavyReadsQueuedTotal"),
        "poolWaitNs":count(&after,"heavyReadWaitNsTotal")-count(&before,"heavyReadWaitNsTotal"),
        "listenerAdmissionCovered":true})
    );
}

#[test]
fn queued_desktop_reads_leave_control_requests_runnable() {
    run_read_burst(4, 1);
}

#[test]
#[ignore = "isolated real HTTP burst; run explicitly with --nocapture --test-threads=1"]
fn sixteen_desktop_bootstrap_history_reads_wait_without_503() {
    run_read_burst(16, 4);
}
