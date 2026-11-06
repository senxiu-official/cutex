//! 054 §5 L2: input-ledger E2E on a private debug daemon with fake Codex /
//! Claude runtimes. Run (host-a):
//!
//!   export BLOCK=e2e; . C:/Tools/agent-tmp/temp-Hakka/env.sh
//!   cargo test --locked --test input_ledger_e2e -- --test-threads=1             # harness + the green scenarios
//!   cargo test --locked --test input_ledger_e2e -- --ignored --test-threads=1   # the rest (each ignore says why)
//!   cargo test --release --locked --test input_ledger_e2e -- --ignored --exact s25_load --test-threads=1
//!       # S25 on a release daemon (the marked private home works in release, `config::paths`)
//!
//! `INPUT_LEDGER_E2E_TRACE_DIR=<dir>` also writes every scenario's trace there
//! as `<scenario>.jsonl` (the L3 desktop replay fixtures). Each test gets its
//! own marked home under `$CUTEX_TEST_PRIVATE_HOME/runs/`, its own ports
//! (24370–24399 / 25470–25499) and stops only the PIDs it started.

#[path = "input_ledger_e2e/daemon.rs"]
mod daemon;
#[path = "input_ledger_e2e/fake_runtime.rs"]
mod fake_runtime;
#[path = "input_ledger_e2e/trace.rs"]
mod trace;
#[path = "input_ledger_e2e/world.rs"]
mod world;

use fake_runtime::{Client, FakeRuntime, Kind, NativeStatus, Script};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use trace::{assert_never, assert_path, assert_path_any, assert_surfaces, AssertOptions, Trace};
use world::{World, WorldSpec};

const T: Duration = Duration::from_secs(20);
/// Longest §2.3 deadline (accepted: turn end + 60 s receipt grace) + slack.
const SETTLE: Duration = Duration::from_secs(90);
const STRICT: AssertOptions = AssertOptions { require_settled_at_end: true };
const LOOSE: AssertOptions = AssertOptions { require_settled_at_end: false };

#[test]
fn agent_tell_message_id_matches_sent_event_and_replays() {
    let w = World::start("tell-message-id", WorldSpec::new(&[Kind::Codex, Kind::Codex])
        .bus().persistent(0).persistent(1));
    w.wait_asks_presence(&[0, 1]);
    let check = |args: Value| {
        let first = w.mcp(0, "agent_tell", args.clone()).expect("tell");
        assert_eq!(first["ok"], true, "{first}");
        let id = first["messageId"].as_str().filter(|id| !id.is_empty())
            .unwrap_or_else(|| panic!("missing local bus ID: {first}")).to_string();
        let replay = w.mcp(0, "agent_tell", args).expect("replay tell");
        assert_eq!(replay["messageId"], id, "{replay}");
        assert_eq!(replay["revision"], first["revision"]);
        let until = Instant::now() + T;
        loop {
            let trace = w.recorder.snapshot();
            let events = trace.entries.iter().filter(|entry| entry.source == "session"
                && entry.cutex_session_id.as_deref() == Some(w.sid(0))
                && entry.payload["method"] == "cutex/agentBus/messageSent"
                && entry.payload["params"]["messageId"] == id).collect::<Vec<_>>();
            if !events.is_empty() {
                assert_eq!(events.len(), 1, "replay must not emit a second send event");
                assert_eq!(events[0].payload["params"]["toCutexSessionId"], w.sid(1));
                break;
            }
            assert!(Instant::now() < until, "no messageSent event for result {first}");
            std::thread::sleep(Duration::from_millis(25));
        }
        first
    };
    let plain = check(json!({"to":w.sid(1),"text":"plain tell ID","action_id":"tell-message-id-plain"}));
    let delegated = agent_delegate(&w, 0, 1, "revision target");
    let revision = check(json!({"delegation":delegated,"text":"revised brief",
        "action_id":"tell-message-id-revision"}));
    assert_eq!(revision["revision"]["id"], "v2", "{revision}");
    assert_ne!(plain["messageId"], revision["messageId"]);
    w.finish(LOOSE);
}

#[test]
fn agent_list_compact_private_daemon_lookup_and_offline() {
    let mut spec = WorldSpec::new(&[Kind::Codex,Kind::Codex,Kind::Codex])
        .bus().persistent(0).persistent(1).persistent(2).offline(2);
    spec.options.peers=true;
    let w = World::start("agent-list-compact",spec);
    w.wait_asks_presence(&[0,1]);
    let call = |args|w.mcp(0,"agent_list",args).expect("agent_list");
    let listed = call(json!({}));
    assert_eq!(listed["ok"],true,"{listed}");
    assert_eq!(listed["agents_total"],2,"{listed}");
    assert_eq!(listed["agents_offline"],1,"{listed}");
    assert_eq!(listed["agents"].as_array().unwrap().len(),1,"{listed}");
    let row=&listed["agents"][0];
    for key in ["name","id","profile","model","status","queue"] {assert!(row.get(key).is_some(),"{row}");}
    for key in ["provider_runtime","first_message","note","activityStatus"] {assert!(row.get(key).is_none(),"{row}");}
    let all=call(json!({"include_offline":true}));
    assert_eq!(all["agents"].as_array().unwrap().len(),2,"{all}");
    let detail=call(json!({"name":w.sid(2)}));
    assert_eq!(detail["agents"].as_array().unwrap().len(),1,"{detail}");
    assert_eq!(detail["agents"][0]["status"],"offline","{detail}");
    assert!(detail["agents"][0].get("provider_runtime").is_some(),"{detail}");
    assert!(detail["agents"][0].get("activityStatus").is_some(),"{detail}");
    let (status,sessions)=w.daemon.http("GET","/v2/sessions",None).expect("session HTTP API");
    assert_eq!(status,200,"{sessions}");
    assert!(sessions.to_string().contains(w.sid(2)),"tool filters must not remove HTTP sessions: {sessions}");
    let mut spec=WorldSpec::new(&[Kind::Codex]).bus().persistent(0);
    spec.options.peers=true;
    let sender=World::start("agent-list-peer-lookup",spec);
    let pair=daemon::pair_peers(&sender.daemon,&w.daemon,w.sid(2));
    // Pairing does not synchronously populate the existing directory cache.
    let (status,exported)=sender.daemon.http("GET",&format!("/v2/peers/{}/agents",pair.receiver_on_sender),None).expect("refresh peer directory");
    assert_eq!(status,200,"{exported}");
    assert!(exported.to_string().contains(w.sid(2)),"legacy peer directory still exports the offline session: {exported}");
    let summary=sender.mcp(0,"agent_list",json!({})).expect("peer summary");
    assert_eq!(summary["otherHosts"][0]["agentCount"],1,"{summary}");
    assert_eq!(summary["otherHosts"][0]["offlineCount"],1,"{summary}");
    assert!(summary["otherHosts"][0].get("agents").is_none(),"{summary}");
    let page=sender.mcp(0,"agent_list",json!({"host":pair.receiver_label,"include_offline":true})).expect("peer page");
    assert_eq!(page["otherHosts"][0]["agents"][0]["id"],w.sid(2),"{page}");
    let remote=sender.mcp(0,"agent_list",json!({"name":format!("{}@{}",w.sid(2),pair.receiver_label)})).expect("peer name");
    assert_eq!(remote["agents"][0]["id"],w.sid(2),"{remote}");
    assert_eq!(remote["agents"][0]["cutexSessionId"],w.sid(2),"full exported row: {remote}");
    sender.finish(LOOSE);
    w.finish(LOOSE);
}

/// Seed a queued offline-block notice while Management is stopped, then let
/// the real private daemon see its target recover, run the ask and deliver
/// only its final result. No live runtime or provider is involved.
#[test]
fn stale_blocked_notice_private_daemon_recovery() {
    use cutex::asks::{delivery, ledger, store::AskStore, wake};
    use cutex::management::v2::input_ledger::InputLedger;
    let mut w = World::start("stale-blocked-notice-recovery",
        WorldSpec::new(&[Kind::Claude, Kind::Claude]).bus().persistent(0).persistent(1)
            .script(0, auto(300)).script(1, auto(1000)));
    w.daemon.cutex(w.sid(1), "cutex/asks/pause", json!({})).unwrap();
    let short_ask = agent_delegate(&w, 0, 1, "private recovered delegation");
    // Approval is committed before the scheduler reconciles PendingReview
    // to Queued. Wait for that actual source transition before stopping it.
    let until = Instant::now() + T;
    loop {
        let (status, page) = w.daemon.http("GET", &format!("/v2/asks?target={}", w.sid(1)), None).unwrap();
        assert_eq!(status, 200);
        if page["asks"].as_array().unwrap().iter().any(|ask|
            ask["title"] == "private recovered delegation" && ask["execution"] == "queued") { break; }
        assert!(Instant::now() < until, "approved ask never queued: {page}");
        std::thread::sleep(Duration::from_millis(100));
    }
    w.daemon.cutex(w.sid(0), "cutex/asks/pause", json!({})).unwrap();
    w.daemon.stop_management();
    // Construct the canonical host store (shared management DB + asks lock
    // and artifact root). AskStore::at(runtime/asks) is an isolated store,
    // not the host store since 058. The harness runs serially; restore the
    // parent's environment before any source write or scheduler activity.
    let store = {
        struct RestoreEnv(Vec<(&'static str, Option<std::ffi::OsString>)>);
        impl Drop for RestoreEnv {
            fn drop(&mut self) {
                for (key, value) in &self.0 {
                    match value { Some(value) => std::env::set_var(key, value), None => std::env::remove_var(key) }
                }
            }
        }
        let _restore = RestoreEnv(["HOME", "CUTEX_TEST_PRIVATE_HOME", "CUTEX_HOME"].into_iter()
            .map(|key| (key, std::env::var_os(key))).collect());
        std::env::set_var("HOME", &w.daemon.home.root);
        std::env::set_var("CUTEX_TEST_PRIVATE_HOME", &w.daemon.home.root);
        std::env::remove_var("CUTEX_HOME");
        AskStore::open_default().unwrap()
    };
    assert_eq!(store.dir(), w.daemon.home.config_dir.join("runtime/asks"));
    let now = chrono::Utc::now();
    let before = store.snapshot().unwrap();
    let index = cutex::short_ids::Index::new(before.asks.keys().cloned().map(cutex::short_ids::Entry::id));
    let ask = index.resolve(&short_ask, &index, 6)
        .unwrap_or_else(|error| panic!("MCP delegation {short_ask}: {error:?}; source ids {:?}", before.asks.keys().collect::<Vec<_>>()));
    assert_eq!(before.asks[&ask].execution, cutex::asks::model::ExecutionState::Queued);
    let notice = store.with_state(|state| {
        wake::record_owner_action(state, w.sid(1), "ownerStopped", None, now);
        state.deliveries.values().find(|d| d.ask_id == ask && d.ref_id == delivery::BLOCKED_OWNER_ACTION).unwrap().delivery_id.clone()
    }).unwrap();
    let after = store.snapshot().unwrap();
    // Fixture creation is offline. The subsequent source transition, ledger
    // projection, HTTP read and model-facing delivery all run in the daemon.
    let fixture_ledger = InputLedger::open(w.daemon.home.ledger_file());
    let job = ledger::delivery_job(&before, &after, None, &after.deliveries[&notice]).unwrap();
    ledger::apply_job(&fixture_ledger, &job);
    drop(fixture_ledger);
    // Offline fixture writes intentionally have no journal event. Start a
    // fresh observation phase at the daemon's actual checkpoint, rather
    // than treating that offline revision as a missing live SSE delivery.
    w.recorder.stop();
    w.recorder = daemon::Recorder::new();
    w.daemon.start();
    w.daemon.wait_connected(w.sid(0), T);
    w.daemon.wait_connected(w.sid(1), T);
    for index in [0, 1] {
        let sid = w.sid(index).to_string();
        w.recorder.subscribe_latest(&w.daemon, "session", Some(&sid));
        w.recorder.read_input_list(&w.daemon, &sid).unwrap();
    }
    w.recorder.subscribe_latest(&w.daemon, "host", None);
    w.daemon.cutex(w.sid(1), "cutex/asks/resume", json!({})).unwrap();
    let until = Instant::now() + T;
    loop {
        let response = w.recorder.read_input_list(&w.daemon, w.sid(0)).unwrap();
        if response["records"].as_array().unwrap().iter().any(|r|
            r["inputId"] == notice && r["state"] == "discarded" && r["reason"] == "superseded" && r["surface"] == "none") { break; }
        assert!(Instant::now() < until, "obsolete notice not discarded: {response}");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(store.snapshot().unwrap().deliveries[&notice].settled_by.as_deref(), Some(delivery::SUPERSEDED));
    let until = Instant::now() + T;
    let results = loop {
        let response = w.recorder.read_input_list(&w.daemon, w.sid(0)).unwrap();
        let ids: Vec<String> = response["records"].as_array().unwrap().iter()
            .filter(|r| r["askId"] == ask && r["kind"] == "delegationResult")
            .map(|r| r["inputId"].as_str().unwrap().into()).collect();
        if !ids.is_empty() { break ids; }
        assert!(Instant::now() < until, "no result; source ask {:?}; handler calls {:?}",
            store.snapshot().unwrap().asks.get(&ask), w.fake(1).calls().iter().map(|c| &c.method).collect::<Vec<_>>());
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(w.fake(0).inbox().is_empty(), "paused requester received model input");
    w.daemon.cutex(w.sid(0), "cutex/asks/resume", json!({})).unwrap();
    w.wait_settled(&[results[0].as_str()], SETTLE);
    let inbox = w.fake(0).inbox();
    assert!(!inbox.is_empty(), "final result never reached model input");
    assert!(!serde_json::to_string(&inbox.iter().map(|e| &e.params).collect::<Vec<_>>()).unwrap().contains("blockedOwnerAction"));
    assert!(store.snapshot().unwrap().answers.values().any(|a| a.ask_id == ask));
    let trace = w.finish(LOOSE);
    assert_eq!(trace.latest(&notice).unwrap()["surface"], "none");
}

/// Stand-in runtime PID for seeded bindings (see `daemon::Sleeper`).
#[test]
#[ignore = "helper process for the harness, not a test"]
fn harness_sleeper_child() {
    if std::env::var_os(daemon::SLEEPER_ENV).is_some() {
        if let Some(target) = std::env::var(daemon::SLEEPER_FORWARD_ENV).ok().and_then(|port| port.parse().ok()) {
            daemon::forward_loopback(target);
        }
        std::thread::sleep(Duration::from_secs(3 * 3600));
    }
}

#[track_caller]
fn missing_hook(what: &str) -> ! {
    panic!("missing hook: {what}")
}

fn auto(after_ms: u64) -> Script {
    Script { auto_complete_after: Some(Duration::from_millis(after_ms)), ..Script::default() }
}

/// 2.7: submit / steer / flush results carry `input` (the request's record).
#[track_caller]
fn assert_result_input(result: &Result<Value, Value>, id: &str) {
    let value = result.as_ref().unwrap_or_else(|e| panic!("{id}: request failed: {e}"));
    assert_eq!(value["input"]["inputId"], id, "{id}: result.input missing: {value}");
}

/// The path with Claude's `accepted[registered]` → `accepted[offered]`
/// stage steps (§2.4 `accepted.stage`, Claude only; Codex has one stageless
/// `accepted`) collapsed into the first of them, for scenarios whose
/// expectation does not depend on the adapter stages (S4 checks them).
fn path_collapsing_stages(trace: &Trace, id: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for label in trace.path(id) {
        if label.starts_with("accepted") && out.last().is_some_and(|last| last.starts_with("accepted")) {
            continue;
        }
        out.push(label);
    }
    out
}

#[track_caller]
fn assert_path_any_collapsing_stages(trace: &Trace, id: &str, expected: &[&[&str]]) {
    let actual = path_collapsing_stages(trace, id);
    assert!(expected.iter().any(|p| trace::path_matches(p, &actual)),
        "{id}: expected one of {expected:?} (accepted stages collapsed), got {:?}", trace.path(id));
}

fn ids_of_kind(trace: &Trace, sid: &str, kind: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for record in trace.all_records() {
        if record["cutexSessionId"] == sid && record["kind"] == kind {
            let id = record["inputId"].as_str().unwrap_or("").to_string();
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    out
}

fn wait_kind(w: &World, index: usize, kind: &str, count: usize, timeout: Duration) -> Vec<String> {
    let sid = w.sid(index).to_string();
    let ok = w.wait(timeout, |t| ids_of_kind(t, &sid, kind).len() >= count);
    let ids = ids_of_kind(&w.trace(), &sid, kind);
    assert!(ok, "{}: {count} x {kind} records expected on {sid}, got {ids:?}", w.name);
    ids
}

// ---------------------------------------------------------------------------
// Harness (green now)

#[test]
fn fake_codex_speaks_the_native_protocol() {
    let fake = FakeRuntime::start(Kind::Codex, "C:/fixture");
    let mut c = Client::connect(&fake.url);
    assert!(c.request("initialize", json!({})).unwrap()["userAgent"].is_string());
    let thread = c.request("thread/resume", json!({"threadId": fake.thread})).unwrap();
    assert_eq!(thread["thread"]["status"]["type"], "idle");
    let started = c.request("turn/start", json!({"clientUserMessageId": "m1", "input": [{"type": "text", "text": "m1"}]})).unwrap();
    let turn = started["turn"]["id"].as_str().unwrap().to_string();
    assert_eq!(c.wait_notification("turn/started")["params"]["turn"]["id"], turn);
    let steered = c.request("turn/steer", json!({"clientUserMessageId": "m2", "expectedTurnId": turn, "input": [{"type": "text", "text": "m2"}]})).unwrap();
    assert_eq!(steered["turnId"], turn);
    let echo = loop {
        let n = c.wait_notification("item/started");
        if n["params"]["item"]["clientId"] == "m2" {
            break n;
        }
    };
    assert_eq!(echo["params"]["turnId"], turn);
    assert!(c.request("turn/start", json!({"input": [{"type": "text", "text": "x"}]})).is_err(), "busy refuses a second turn");
    c.request("turn/interrupt", json!({"threadId": fake.thread})).unwrap();
    assert_eq!(c.wait_notification("turn/completed")["params"]["turn"]["status"], "interrupted");
    // Missing turn/completed, refusal per marker, delayed answers.
    fake.script(|s| {
        s.drop_turn_completed = true;
        s.refuse.insert("refused".into(), json!({"code": -32009, "message": "fixture refusal"}));
        s.start_delay = Duration::from_millis(300);
    });
    assert!(c.request("turn/start", json!({"input": [{"type": "text", "text": "refused"}]})).is_err());
    let before = Instant::now();
    c.request("turn/start", json!({"clientUserMessageId": "m3", "input": [{"type": "text", "text": "m3"}]})).unwrap();
    assert!(before.elapsed() >= Duration::from_millis(250));
    assert_eq!(fake.complete("completed").is_some(), true);
    assert_eq!(fake.calls_of("turn/start").len(), 4);
    // Native ExternalInput statuses (RC8).
    for (status, delivery, processing) in [
        (NativeStatus::Held("plan_mode"), "pending", "held"),
        (NativeStatus::RetryableError, "retryable_error", "none"),
        (NativeStatus::Conflict, "conflict", "none"),
    ] {
        fake.script(|s| s.external_status = status.clone());
        let r = c.request("thread/externalInput/status", json!({"ownerId": "o", "runtimeGeneration": 1,
            "messages": [{"messageId": "bus-1", "semanticSha256": "x"}]})).unwrap();
        assert_eq!(r["statuses"][0]["deliveryState"], delivery);
        assert_eq!(r["statuses"][0]["processing"]["state"], processing);
    }
    // Disconnect drops the peer; refused reconnects stay refused.
    fake.script(|s| s.refuse_connections = true);
    fake.disconnect_all();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(fake.connected_peers(), 0);
}

#[test]
fn fake_claude_speaks_the_adapter_protocol_with_stages() {
    let fake = FakeRuntime::start_with(Kind::Claude, "C:/fixture", auto(200));
    let mut c = Client::connect(&fake.url);
    let caps = c.request("claude/capabilities", json!({})).unwrap();
    assert_eq!(caps["implementation"], "cute-claude-app-server");
    assert_eq!(caps["input"]["sameTurnSteering"], true);
    assert_eq!(c.request("claude/runtime/status", json!({})).unwrap()["state"], "waiting");
    let turn = fake.begin_turn();
    assert_eq!(c.request("claude/runtime/status", json!({})).unwrap()["state"], "working");
    c.request("turn/steer", json!({"clientUserMessageId": "c1", "expectedTurnId": turn, "input": [{"type": "text", "text": "c1"}]})).unwrap();
    let registered = c.wait_notification("claude/input/stageChanged");
    assert_eq!(registered["params"]["stage"], "registered");
    assert_eq!(registered["params"]["clientId"], "c1");
    assert_eq!(c.wait_notification("claude/input/stageChanged")["params"]["stage"], "offered");
    fake.complete("completed");
    let persisted = c.wait_notification("claude/input/contextPersisted");
    assert_eq!(persisted["params"]["clientId"], "c1");
    assert_eq!(persisted["params"]["turnId"], turn);
    c.wait_notification("turn/completed");
    // auto_complete_after: a started turn completes by itself.
    c.request("turn/start", json!({"clientUserMessageId": "c2", "input": [{"type": "text", "text": "c2"}]})).unwrap();
    assert_eq!(c.wait_notification("turn/completed")["params"]["turn"]["status"], "completed");
    fake.script(|s| s.claude_same_turn_steering = false);
    fake.begin_turn();
    assert!(c.request("turn/steer", json!({"input": [{"type": "text", "text": "c3"}]})).is_err());
}

#[test]
fn harness_daemon_adopts_fake_runtimes_and_serves_ledger_reads() {
    let w = World::start("harness", WorldSpec::new(&[Kind::Codex, Kind::Claude]).script(0, auto(300)));
    for index in 0..2 {
        let sid = w.sid(index).to_string();
        let list = w.recorder.read_input_list(&w.daemon, &sid).expect("cutex/input/list");
        assert!(list["ledgerEpoch"].as_str().is_some_and(|e| !e.is_empty()), "{list}");
        assert_eq!(list["complete"], true);
        assert!(list["records"].is_array());
        let (status, http) = w.daemon.http("GET", &format!("/v2/sessions/{sid}/inputs"), None).unwrap();
        assert_eq!(status, 200, "{http}");
        assert_eq!(http["ledgerEpoch"], list["ledgerEpoch"]);
        let (status, one) = w.daemon.http("GET", &format!("/v2/sessions/{sid}/inputs/absent-id"), None).unwrap();
        assert_eq!(status, 200, "{one}");
        assert_eq!(one["missing"], json!(["absent-id"]));
        let got = w.daemon.cutex(&sid, "cutex/input/get", json!({"inputIds": ["absent-id"]})).unwrap();
        assert_eq!(got["missing"], json!(["absent-id"]));
        w.recorder.read_queue_list(&w.daemon, &sid).expect("queue/list");
    }
    // Owner input reaches the fake runtime through the real daemon path.
    w.submit(0, "harness-m1", "auto", json!({})).expect("submit");
    let start = w.fake(0).wait_call("turn/start", 1, T);
    assert_eq!(start[0].params["clientUserMessageId"], "harness-m1");
    let sid = w.sid(0).to_string();
    assert!(w.wait(T, |t| t.entries.iter().any(|e| e.source == "session" && e.kind == "native"
        && e.cutex_session_id.as_deref() == Some(sid.as_str()) && e.payload["method"] == "turn/completed")),
        "the session stream recorder saw no turn/completed");
    assert!(w.wait(T, |t| t.entries.iter().any(|e| e.source == "host" && e.kind == "legacy")),
        "the host stream recorder saw no cutex event");
    let trace = w.finish(LOOSE);
    assert!(trace.entries.iter().any(|e| e.kind == "inputList"));
}

#[test]
fn harness_ledger_switch_off_refuses_reads() {
    let w = World::start("switch-off", WorldSpec::new(&[Kind::Codex]).ledger(false));
    let sid = w.sid(0).to_string();
    let error = w.daemon.cutex(&sid, "cutex/input/list", json!({})).unwrap_err();
    assert_eq!(error["body"]["error"]["code"], "cutex_method_denied", "{error}");
    let (status, _) = w.daemon.http("GET", &format!("/v2/sessions/{sid}/inputs"), None).unwrap();
    assert_eq!(status, 404);
    assert!(!has_feature(&w, "owner-input-nextup-v1") && !has_feature(&w, "input-ledger-v1"), "switch off: no ledger features");
    w.finish(LOOSE);
}

fn has_feature(w: &World, feature: &str) -> bool {
    let (_, caps) = w.daemon.http("GET", "/v2/capabilities", None).unwrap();
    caps["featureList"].as_array().is_some_and(|list| list.iter().any(|f| f == feature))
}

// ---------------------------------------------------------------------------
// §5.2 scenarios. `green` ones (3/3 on the integration branch) run by default;
// the ignore reason of every other one names what is missing and whose it is
// (C2 owner, C3 bus, C4 asks, C5 settlement / SessionDelivery, or the harness).

macro_rules! both {
    // Green 3/3 on both runtimes: part of the default run.
    ($name:ident, $f:ident, green) => {
        mod $name {
            use super::*;
            #[test]
            fn codex() {
                $f(Kind::Codex)
            }
            #[test]
            fn claude() {
                $f(Kind::Claude)
            }
        }
    };
    ($name:ident, $f:ident, codex: green, claude: $why:literal) => {
        mod $name {
            use super::*;
            #[test]
            fn codex() {
                $f(Kind::Codex)
            }
            #[test]
            #[ignore = $why]
            fn claude() {
                $f(Kind::Claude)
            }
        }
    };
    ($name:ident, $f:ident, codex: $why:literal, claude: green) => {
        mod $name {
            use super::*;
            #[test]
            #[ignore = $why]
            fn codex() {
                $f(Kind::Codex)
            }
            #[test]
            fn claude() {
                $f(Kind::Claude)
            }
        }
    };
    ($name:ident, $f:ident, $why:literal) => {
        mod $name {
            use super::*;
            #[test]
            #[ignore = $why]
            fn codex() {
                $f(Kind::Codex)
            }
            #[test]
            #[ignore = $why]
            fn claude() {
                $f(Kind::Claude)
            }
        }
    };
}

macro_rules! one {
    ($name:ident, $f:ident, $kind:expr, green) => {
        #[test]
        fn $name() {
            $f($kind)
        }
    };
    ($name:ident, $f:ident, $kind:expr, $why:literal) => {
        #[test]
        #[ignore = $why]
        fn $name() {
            $f($kind)
        }
    };
}

fn name(s: &str, kind: Kind) -> String {
    format!("{s}-{}", kind.name())
}

// S1 ★ idle, owner auto: received → sending → inContext; timeline.
fn s01(kind: Kind) {
    let w = World::start(&name("s01", kind), WorldSpec::new(&[kind]).script(0, auto(300)));
    let result = w.submit(0, "s01-m1", "auto", json!({}));
    w.fake(0).wait_call("turn/start", 1, T);
    w.wait_settled(&["s01-m1"], SETTLE);
    let t = w.finish(STRICT);
    assert_result_input(&result, "s01-m1");
    assert_path(&t, "s01-m1", &["received", "sending", "inContext(turnStartInput)"]);
    assert_surfaces(&t, "s01-m1", &["timeline"]);
}
both!(s01_idle_owner_auto, s01, green);

// S2 ★ idle, 0–500 ms after turn/completed: a direct start is on the
// timeline; a steer or a wait cutex chose (waiting{turnFinishing}) is nextUp
// until read (r274.2).
fn s02(kind: Kind) {
    let w = World::start(&name("s02", kind), WorldSpec::new(&[kind]).script(0, auto(300)));
    let sid = w.sid(0).to_string();
    let mut ids = Vec::new();
    for (i, delay) in [0u64, 250, 500].into_iter().enumerate() {
        w.fake(0).begin_turn();
        w.daemon.wait_active_turn(&sid, true, T);
        w.fake(0).complete("completed");
        std::thread::sleep(Duration::from_millis(delay));
        let id = format!("s02-m{i}");
        w.submit(0, &id, "auto", json!({})).expect("submit");
        w.wait_settled(&[&id], SETTLE);
        w.daemon.wait_active_turn(&sid, false, T);
        ids.push(id);
    }
    let t = w.finish(STRICT);
    for id in &ids {
        // r274.2: a direct start is created on the timeline (note a); a steer
        // or a wait the route chose first is nextUp until read. A route that
        // tried a steer while the turn was ending and then started the turn
        // (`sending` written before the steer) shows the same path from nextUp.
        let direct = trace::path_matches(&["received", "sending", "inContext"], &t.path(id));
        let surfaces = t.surfaces(id);
        if direct {
            assert!(surfaces == ["timeline"] || surfaces == ["nextUp", "timeline"], "{id}: {surfaces:?}");
        } else {
            assert_surfaces(&t, id, &["nextUp", "timeline"]);
        }
        assert_path_any(&t, id, &[
            &["received", "sending", "inContext"],
            &["received", "waiting{turnFinishing}", "sending", "inContext"],
            &["received", "sending", "accepted", "inContext"],
        ]);
    }
}
both!(s02_idle_just_after_turn_completed, s02, green);

// S3 ★ busy, Codex insertion: accepted → inContext{clientIdEcho}; nextUp
// until the echo, then the timeline (r274.2).
fn s03(kind: Kind) {
    let w = World::start(&name("s03", kind), WorldSpec::new(&[kind]));
    let turn = w.fake(0).begin_turn();
    w.daemon.wait_active_turn(w.sid(0), true, T);
    let result = w.submit(0, "s03-m1", "auto", json!({}));
    w.fake(0).wait_call("turn/steer", 1, T);
    w.wait_settled(&["s03-m1"], SETTLE);
    w.fake(0).complete("completed");
    let t = w.finish(STRICT);
    assert_result_input(&result, "s03-m1");
    assert_path(&t, "s03-m1", &["received", "sending", "accepted", "inContext(clientIdEcho)"]);
    assert_surfaces(&t, "s03-m1", &["nextUp", "timeline"]);
    let accepted = t.records("session", "s03-m1").into_iter().find(|r| r["state"] == "accepted").unwrap();
    assert_eq!(accepted["turnId"], turn);
}
one!(s03_busy_codex_insertion, s03, Kind::Codex, green);

// S4 ★ busy, Claude insertion: accepted{registered} → {offered} →
// inContext{contextPersisted}; nextUp until offered (r274.2).
fn s04(kind: Kind) {
    let w = World::start(&name("s04", kind), WorldSpec::new(&[kind]));
    w.fake(0).begin_turn();
    w.daemon.wait_active_turn(w.sid(0), true, T);
    let result = w.submit(0, "s04-m1", "auto", json!({}));
    w.fake(0).wait_call("turn/steer", 1, T);
    w.wait_state("s04-m1", "accepted[offered]", T);
    w.fake(0).complete("completed");
    w.wait_settled(&["s04-m1"], SETTLE);
    let t = w.finish(STRICT);
    assert_result_input(&result, "s04-m1");
    assert_path(&t, "s04-m1", &["received", "sending", "accepted[registered]", "accepted[offered]", "inContext(contextPersisted)"]);
    assert_surfaces(&t, "s04-m1", &["nextUp", "timeline"]);
}
one!(s04_busy_claude_insertion, s04, Kind::Claude, green);

// S5 busy, Claude finishing: waiting{turnFinishing} → sending → inContext;
// never accepted (steered).
fn s05(kind: Kind) {
    let mut script = auto(300);
    script.refuse.insert("s05-m1".into(), json!({"code": -32009, "message": "turn is finishing", "data": {"reason": "turnFinishing"}}));
    let w = World::start(&name("s05", kind), WorldSpec::new(&[kind]).script(0, script));
    w.fake(0).begin_turn();
    w.daemon.wait_active_turn(w.sid(0), true, T);
    w.submit(0, "s05-m1", "auto", json!({})).expect("submit");
    std::thread::sleep(Duration::from_millis(300));
    w.fake(0).script(|s| s.refuse.clear());
    w.fake(0).complete("completed");
    w.wait_settled(&["s05-m1"], SETTLE);
    let t = w.finish(STRICT);
    // Director decision 3: the owner record is `sending` before the native
    // call; the adapter's explicit finishing refusal moves it back to
    // waiting{turnFinishing}, then it is sent again. r274.2: a steer is
    // created on nextUp and moves to the timeline when the start reads it.
    assert_path(&t, "s05-m1", &["received", "sending", "waiting{turnFinishing}", "sending", "inContext"]);
    assert_never(&t, "s05-m1", "accepted");
    assert_surfaces(&t, "s05-m1", &["nextUp", "timeline"]);
}
one!(s05_busy_claude_finishing, s05, Kind::Claude, green);

// S6 ★ busy, owner queue: nextUp waiting{explicitQueue} → inContext after the
// turn, moved to the timeline.
fn s06(kind: Kind) {
    let w = World::start(&name("s06", kind), WorldSpec::new(&[kind]).script(0, auto(300)));
    w.fake(0).begin_turn();
    w.daemon.wait_active_turn(w.sid(0), true, T);
    let result = w.submit(0, "s06-m1", "queue", json!({}));
    w.wait_state("s06-m1", "waiting{explicitQueue}", T);
    let queue = w.recorder.read_queue_list(&w.daemon, w.sid(0)).expect("queue/list");
    assert!(queue["inputs"].as_array().is_some_and(|a| a.iter().any(|r| r["inputId"] == "s06-m1" && r["surface"] == "nextUp")), "{queue}");
    w.fake(0).complete("completed");
    w.wait_settled(&["s06-m1"], SETTLE);
    let t = w.finish(STRICT);
    assert_result_input(&result, "s06-m1");
    assert_path(&t, "s06-m1", &["received", "waiting{explicitQueue}", "sending", "inContext"]);
    assert_surfaces(&t, "s06-m1", &["nextUp", "timeline"]);
}
both!(s06_busy_owner_queue, s06, green);

// S7 busy, host lock held 30 s, allowHeld: held → sending → …; 60 s →
// failed{admission_timeout}, retry keeps the id. r274.2: held and the retry
// are nextUp until read.
/// C2 debug-only hook: a daemon thread takes the host authority fence and
/// keeps it for the given seconds (answers once held).
fn hold_host_lock(w: &World, secs: u64) {
    let (status, body) = w.daemon.http("POST", "/v2/test/hostFence/hold", Some(&json!({"seconds": secs}))).expect("hostFence/hold");
    assert_eq!(status, 200, "hostFence/hold: {body}");
}
fn s07(kind: Kind) {
    let w = World::start(&name("s07", kind), WorldSpec::new(&[kind]).script(0, auto(300)));
    w.fake(0).begin_turn();
    w.daemon.wait_active_turn(w.sid(0), true, T);
    hold_host_lock(&w, 30);
    w.submit(0, "s07-m1", "auto", json!({"allowHeld": true})).expect("submit");
    // A steer is accepted into the running turn; Claude persists its context
    // only when that turn completes (S4), so complete it, then be busy again.
    w.wait_state("s07-m1", "accepted", Duration::from_secs(60));
    w.fake(0).complete("completed");
    w.wait_settled(&["s07-m1"], SETTLE);
    w.daemon.wait_active_turn(w.sid(0), false, T);
    w.fake(0).begin_turn();
    w.daemon.wait_active_turn(w.sid(0), true, T);
    hold_host_lock(&w, 65);
    w.submit(0, "s07-m2", "auto", json!({"allowHeld": true})).expect("submit");
    w.wait_state("s07-m2", "failed{admission_timeout}", Duration::from_secs(75));
    let rev = w.trace().latest("s07-m2").unwrap()["rev"].as_u64().unwrap();
    w.daemon.cutex(w.sid(0), "cutex/input/retry", json!({"inputId": "s07-m2", "expectedRev": rev})).expect("retry");
    w.fake(0).complete("completed");
    w.wait_settled(&["s07-m2"], SETTLE);
    let t = w.finish(STRICT);
    // Claude adds accepted[registered] / accepted[offered] (§2.4 stages) where
    // Codex has one accepted: legal extra steps of the same state.
    assert_path_any_collapsing_stages(&t, "s07-m1", &[&["received", "held{hostBusy}", "sending", "accepted", "inContext"], &["received", "held{hostBusy}", "sending", "inContext"]]);
    let m2 = path_collapsing_stages(&t, "s07-m2");
    assert_eq!(&m2[..3.min(m2.len())], &["received", "held{hostBusy}", "failed{admission_timeout}"], "{:?}", t.path("s07-m2"));
    assert!(m2.contains(&"waiting{ownerRetry}".to_string()));
    for id in ["s07-m1", "s07-m2"] {
        assert_surfaces(&t, id, &["nextUp", "timeline"]);
    }
}
both!(s07_host_lock_busy_allow_held, s07, green);

// Owner decision 2026-11-04: an idle send delayed by the host fence
// stays on the timeline, including a second send held behind the first.
fn idle_busy_timeline(kind: Kind) {
    for allow_held in [false, true] {
        let scenario = format!("idle-busy-{}-{allow_held}", if kind == Kind::Codex { "codex" } else { "claude" });
        let w = World::start(&scenario, WorldSpec::new(&[kind]).bus().script(0, auto(400)));
        w.daemon.wait_active_turn(w.sid(0), false, T);
        hold_host_lock(&w, 5);
        for id in ["idle-a", "idle-b"] {
            let reply = w.submit(0, id, "auto", json!({"allowHeld": allow_held})).expect("idle busy submit");
            assert_eq!(reply["input"]["surface"], "timeline", "{scenario} {id}: {reply}");
        }
        w.wait_settled(&["idle-a", "idle-b"], SETTLE);
        let t = w.finish(STRICT);
        for id in ["idle-a", "idle-b"] { assert_surfaces(&t, id, &["timeline"]); }
        let first_send = |id: &str| t.entries.iter().position(|entry| entry.source == "session"
            && entry.kind == "stateChanged" && entry.payload["record"]["inputId"] == id
            && entry.payload["record"]["state"] == "sending").expect("sending event");
        assert!(first_send("idle-a") < first_send("idle-b"), "B cannot pass A");
    }
}
both!(idle_busy_timeline, idle_busy_timeline, green);

fn idle_busy_running_turn(kind: Kind) {
    for allow_held in [false, true] {
        let scenario = format!("running-busy-{}-{allow_held}", kind.name());
        let w = World::start(&scenario, WorldSpec::new(&[kind]).bus().script(0, auto(400)));
        w.fake(0).begin_turn();
        w.daemon.wait_active_turn(w.sid(0), true, T);
        hold_host_lock(&w, 5);
        let reply = w.submit(0, "running-owner", "auto", json!({"allowHeld": allow_held})).expect("running busy submit");
        assert_eq!(reply["input"]["surface"], "nextUp", "{scenario}: {reply}");
        assert_eq!(reply["input"]["state"], if allow_held { "held" } else { "waiting" });
        std::thread::sleep(Duration::from_secs(6));
        w.fake(0).complete("completed");
        w.wait_settled(&["running-owner"], SETTLE);
        let t = w.finish(STRICT);
        assert_surfaces(&t, "running-owner", &["nextUp", "timeline"]);
    }
}
both!(idle_busy_running_turn, idle_busy_running_turn, green);

fn idle_busy_failure_actions(kind: Kind) {
    let w = World::start(&name("idle-busy-failure", kind), WorldSpec::new(&[kind]).bus().script(0, auto(300)));
    w.daemon.wait_active_turn(w.sid(0), false, T);
    hold_host_lock(&w, 65);
    for id in ["idle-retry", "idle-dismiss"] {
        let reply = w.submit(0, id, "auto", json!({"allowHeld": true})).expect("held submit");
        assert_eq!(reply["input"]["surface"], "timeline", "{reply}");
    }
    for id in ["idle-retry", "idle-dismiss"] {
        w.wait_state(id, "failed{admission_timeout}", Duration::from_secs(75));
        // Host-stream projections may omit actions; a point read is the full
        // owner record (the same authority the renderer hydrates).
        let full = w.daemon.cutex(w.sid(0), "cutex/input/get", json!({"inputIds": [id]})).expect("failed point read");
        let row = &full["records"][0];
        assert_eq!(row["surface"], "timeline");
        assert_eq!(row["actions"], json!(["retry", "dismiss"]));
    }
    let failed = w.trace().latest("idle-dismiss").unwrap();
    let dismissed = w.daemon.cutex(w.sid(0), "cutex/input/dismiss", json!({"inputId": "idle-dismiss", "expectedRev": failed["rev"]})).expect("dismiss");
    assert_eq!(dismissed["records"][0]["surface"], "none", "{dismissed}");
    // Fresh reads (the desktop's reload path) must retain the failed bubble
    // and retain the dismissal, independently of the previous event stream.
    let reload = w.daemon.cutex(w.sid(0), "cutex/input/get", json!({"inputIds": ["idle-retry", "idle-dismiss"]})).expect("reload");
    w.recorder.mark("idleFailureReload", reload.clone());
    assert_eq!(reload["records"][0]["surface"], "timeline");
    assert_eq!(reload["records"][1]["surface"], "none");
    // Wait for the debug fence to release; a retry is an explicit owner action.
    std::thread::sleep(Duration::from_secs(6));
    let failed = w.trace().latest("idle-retry").unwrap();
    let retried = w.daemon.cutex(w.sid(0), "cutex/input/retry", json!({"inputId": "idle-retry", "expectedRev": failed["rev"]})).expect("retry");
    assert_eq!(retried["record"]["surface"], "timeline", "{retried}");
    w.wait_settled(&["idle-retry", "idle-dismiss"], SETTLE);
    let t = w.finish(STRICT);
    assert_surfaces(&t, "idle-retry", &["timeline"]);
    assert_surfaces(&t, "idle-dismiss", &["timeline", "none"]);
}
both!(idle_busy_failure_actions, idle_busy_failure_actions, green);

/// Browser acceptance companion: a private daemon with controlled native IO.
/// The external UI driver keeps the bearer in Node and clicks the real renderer
/// actions. Opt-in only; bounded wait, no listener outside loopback.
#[test]
#[ignore = "interactive companion; INPUT_LEDGER_UI_DIR and a browser driver required"]
fn idle_busy_ui_daemon() {
    let dir = std::path::PathBuf::from(std::env::var_os("INPUT_LEDGER_UI_DIR").expect("INPUT_LEDGER_UI_DIR"));
    std::fs::create_dir_all(&dir).unwrap();
    let w = World::start("idle-busy-ui", WorldSpec::new(&[Kind::Codex]).bus().script(0, auto(300)));
    w.daemon.wait_active_turn(w.sid(0), false, T);
    hold_host_lock(&w, 65);
    for id in ["idle-ui-retry", "idle-ui-dismiss"] {
        let reply = w.submit(0, id, "auto", json!({"allowHeld": true})).expect("submit");
        assert_eq!(reply["input"]["surface"], "timeline");
    }
    let ready = dir.join("ready.json");
    std::fs::write(&ready, serde_json::to_vec(&json!({"port": w.daemon.port, "token": w.daemon.token, "session": w.sid(0)})).unwrap()).unwrap();
    assert!(w.wait(Duration::from_secs(180), |_| dir.join("done").exists()), "UI driver did not finish");
    std::fs::remove_file(ready).unwrap();
    w.wait_settled(&["idle-ui-retry", "idle-ui-dismiss"], SETTLE);
    let t = w.finish(STRICT);
    assert!(t.path("idle-ui-retry").iter().any(|label| label == "failed{admission_timeout}"));
    assert_surfaces(&t, "idle-ui-retry", &["timeline"]);
    assert_surfaces(&t, "idle-ui-dismiss", &["timeline", "none"]);
}

// S8 ★ busy with 2 claimed notes, owner interrupt: owner inContext; notes
// inContext or uncertain{turnInterrupted} within 60 s, never back to nextUp.
fn s08(kind: Kind) {
    // Claude: the adapter takes `soon` wakes into a running turn (r8 `externalInputDeliveries`).
    let mut script = auto(500);
    script.claude_soon = true;
    // `--soon` is after-turn on Claude; a busy wake is read at the next step
    // after `agent_bus.wake_busy_wait_secs` (0 here).
    let mut spec = WorldSpec::new(&[kind, Kind::Codex]).bus().script(0, script);
    spec.options.wake_busy_wait_secs = Some(0);
    let w = World::start(&name("s08", kind), spec);
    w.fake(0).begin_turn();
    w.daemon.wait_active_turn(w.sid(0), true, T);
    for i in 0..2 {
        assert!(w.bus_send(1, 0, &format!("s08-note{i}"), &["--soon"]).status.success());
    }
    let notes = wait_kind(&w, 0, "agentMessage", 2, T);
    for id in &notes {
        w.wait_state(id, "accepted", T);
    }
    let interrupted_at = Instant::now();
    w.submit(0, "s08-owner", "interrupt", json!({})).expect("submit");
    let mut ids: Vec<&str> = notes.iter().map(String::as_str).collect();
    ids.push("s08-owner");
    w.wait_settled(&ids, Duration::from_secs(60));
    assert!(interrupted_at.elapsed() <= Duration::from_secs(60));
    let t = w.finish(STRICT);
    assert!(t.path("s08-owner").last().is_some_and(|l| l.starts_with("inContext")));
    // r274.2 §9.4: an explicit interrupt waits in nextUp until the stop is
    // accepted and the new turn starts with it; it moves once, in that write.
    assert_surfaces(&t, "s08-owner", &["nextUp", "timeline"]);
    let moved = first_timeline(&t, "s08-owner");
    assert_eq!((moved["state"].as_str(), moved["evidence"].as_str()), (Some("inContext"), Some("turnStartInput")), "{moved}");
    for id in &notes {
        assert_path_any(&t, id, &[
            &["waiting", "sending", "accepted", "inContext"],
            &["waiting", "sending", "accepted", "uncertain{turnInterrupted}"],
        ]);
        assert_eq!(t.surfaces(id).last().map(String::as_str), Some("timeline"));
    }
}
both!(s08_busy_claimed_notes_owner_interrupt, s08, green);

// S9 idle with an eligible wake, owner auto: owner first (decision 1).
fn s09(kind: Kind) {
    // Both ready at once: the bus acts on a turn end within ~10 ms, the
    // owner's submit lands ~40 ms after it, so without a window the wake has
    // often started its turn first and the owner is steered into it (a
    // race, not decision 1). Claude: the fake answers the bus's pre-start
    // probe 800 ms late, so the owner's message reaches the idle agent while
    // the wake is still eligible and waiting{afterTurn} (daemon admission
    // sees both; decision 1 lets the owner go first). Codex (the C3 debug
    // ExternalInput delivery submits at once, the runtime holds the after-turn
    // wake): the runtime starts it `inbox_turn_delay` (500 ms) after the turn,
    // and an owner turn/start before that takes it along.
    let mut script = auto(300);
    script.probe_delay = Duration::from_millis(800);
    let w = World::start(&name("s09", kind), WorldSpec::new(&[kind, Kind::Codex]).bus().script(0, script));
    w.fake(0).begin_turn();
    w.daemon.wait_active_turn(w.sid(0), true, T);
    assert!(w.bus_send(1, 0, "s09-wake", &[]).status.success());
    let wake = wait_kind(&w, 0, "agentMessage", 1, T).remove(0);
    if kind == Kind::Codex {
        // The runtime holds the after-turn wake before the turn ends (a submit
        // still in flight at the turn end would start a turn by itself).
        w.wait_state(&wake, "accepted", T);
    }
    let starts_before = w.fake(0).calls_of("turn/start").len();
    w.fake(0).complete("completed");
    w.submit(0, "s09-owner", "auto", json!({})).expect("submit");
    w.wait_settled(&["s09-owner", &wake], SETTLE);
    let starts = w.fake(0).calls_of("turn/start");
    assert_eq!(starts.get(starts_before).map(|c| c.params["clientUserMessageId"].clone()), Some(json!("s09-owner")),
        "decision 1: the owner goes first (its own turn/start); s09-owner path {:?}", w.trace().path("s09-owner"));
    let t = w.finish(STRICT);
    assert_path(&t, "s09-owner", &["received", "sending", "inContext"]);
    assert_never(&t, "s09-owner", "waiting{rideAlongAhead}");
    // r274.2 (as in S2): a route that tried a steer while the turn was
    // ending, then started the turn, shows the same path from nextUp.
    let surfaces = t.surfaces("s09-owner");
    assert!(surfaces == ["timeline"] || surfaces == ["nextUp", "timeline"], "s09-owner: {surfaces:?}");
    assert_path(&t, &wake, &["waiting{afterTurn}", "sending", "accepted", "inContext"]);
}
both!(s09_idle_wake_eligible_owner_first, s09, green);

// S10 ★ idle, wake / alarm / Job notice / approval notice: nextUp waiting →
// sending → accepted (timeline) → inContext; alarms also emit stateChanged (RC7).
fn s10(kind: Kind) {
    let w = World::start(&name("s10", kind), WorldSpec::new(&[kind, Kind::Codex]).bus().script(0, auto(300)));
    assert!(w.bus_send(1, 0, "s10-wake", &[]).status.success());
    let wake = wait_kind(&w, 0, "agentMessage", 1, T).remove(0);
    w.wait_settled(&[&wake], SETTLE);
    assert!(w.alarm(0, "s10-alarm", 1).status.success());
    let alarm = wait_kind(&w, 0, "alarm", 1, Duration::from_secs(30)).remove(0);
    w.wait_settled(&[&alarm], SETTLE);
    let t = w.finish(STRICT);
    for id in [&wake, &alarm] {
        assert_path(&t, id, &["waiting", "sending", "accepted", "inContext"]);
        assert_surfaces(&t, id, &["nextUp", "timeline"]);
        // §2.9 bus row: `accepted{turnId?}` (a native Pending / Claimed status names
        // no turn); the turn is required once it is in context.
        let in_context = t.records("session", id).into_iter().find(|r| r["state"] == "inContext").unwrap();
        assert!(in_context["turnId"].is_string(), "{id}: inContext without turnId");
    }
    assert_eq!(t.latest(&alarm).unwrap()["sender"]["kind"], "alarm");
}
both!(s10_idle_wake_and_alarm, s10, green);

// S10 approval notice: agent 0 registers a static service whose root is
// outside its workspace (`service_register`, StaticRoot approval), the owner
// approves (`POST /v2/approvals/{id}/approve`), and the decision notice
// reaches agent 0 as a system notice (sender approval), read at the next step.
fn s10_approval(kind: Kind) {
    let w = World::start(&name("s10a", kind), WorldSpec::new(&[kind]).bus().persistent(0).script(0, auto(300)));
    let root = w.daemon.home.root.join("s10-static");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("index.html"), "<p>s10</p>").unwrap();
    let registered = w.mcp(0, "service_register", json!({"slug": "s10-static", "runtime": "static", "root": root.display().to_string()}))
        .unwrap_or_else(|e| panic!("service_register: {e}"));
    let approval = find_id(&registered, &["apr-"]).unwrap_or_else(|| panic!("service_register asked no approval: {registered}"));
    let (status, body) = w.daemon.http("POST", &format!("/v2/approvals/{approval}/approve"), Some(&json!({"note": "input_ledger_e2e"}))).expect("approve");
    w.recorder.mark("approval", json!({"approval": approval, "status": status, "body": body}));
    assert_eq!(status, 200, "approve {approval}: {body}");
    let notice = wait_kind(&w, 0, "systemNotice", 1, Duration::from_secs(30)).remove(0);
    w.wait_settled(&[&notice], SETTLE);
    let t = w.finish(STRICT);
    assert_eq!(t.latest(&notice).unwrap()["sender"]["kind"], "approval");
    assert_path(&t, &notice, &["waiting", "sending", "accepted", "inContext"]);
    assert_surfaces(&t, &notice, &["nextUp", "timeline"]);
}
both!(s10_idle_approval_notice, s10_approval, green);

// S10 Job Service notice: a completion posted to the Agent Bus the way the
// Job Service does (`World::job_notice`) reaches the idle persistent agent 0
// as a system notice (sender jobService) with its own ledger record (RC7).
fn s10_job(kind: Kind) {
    let w = World::start(&name("s10j", kind), WorldSpec::new(&[kind]).bus().persistent(0).script(0, auto(300)));
    let (status, receipt) = w.job_notice(0, "s10-job");
    assert_eq!(status, 200, "job completion: {receipt}");
    let notice = wait_kind(&w, 0, "systemNotice", 1, Duration::from_secs(30)).remove(0);
    w.wait_settled(&[&notice], SETTLE);
    let t = w.finish(STRICT);
    assert_eq!(t.latest(&notice).unwrap()["sender"]["kind"], "jobService");
    assert_path(&t, &notice, &["waiting", "sending", "accepted", "inContext"]);
    assert_surfaces(&t, &notice, &["nextUp", "timeline"]);
}
both!(s10_idle_job_notice, s10_job, green);

// S10 Claude Task notice (r274.2): the adapter's `claude/task/updated` with
// `notifyParent` for an idle Claude agent queues a "Claude Task" system
// notice, `sending` within 2 s of its first record (twice). With one Claude
// agent this holds even without the wake (the observer's 2 s bus long poll
// of that agent also returns on the enqueue); the wake itself is covered by
// `a_queued_claude_task_notice_wakes_the_parent`. The wake turn it starts is
// a `delivery` turn that lists it, once its native item is in.
fn s10_ctask(kind: Kind) {
    let w = World::start(&name("s10c", kind), WorldSpec::new(&[kind]).bus());
    let sid = w.sid(0).to_string();
    let fake = w.fake(0);
    for round in 1..=2usize {
        let task = format!("s10c{round}a1b2c3d4e5f6");
        fake.emit("claude/task/updated", json!({"threadId": fake.thread, "task": {"background": true,
            "description": "s10c", "status": "completed", "taskId": task, "taskType": "local_agent",
            "toolUseId": "toolu_s10c", "turnId": null,
            "notifyParent": {"description": "s10c", "noticeId": task, "originTurnId": "s10c-origin",
                "status": "completed", "summary": format!("s10c-done-{round}"), "summaryTruncated": false,
                "taskId": task, "taskType": "local_agent", "toolUseId": "toolu_s10c",
                "transcript": format!("claude:fixture:task:{task}")}}}));
        let notice = wait_kind(&w, 0, "systemNotice", round, T).remove(round - 1);
        let turn = wait_active(fake);
        assert!(w.wait(T, |t| t.deliveries("session").iter().any(|d| d["turn"]["turnId"] == turn.as_str()
            && d["turn"]["kind"] == "delivery" && d["turn"]["inputIds"].as_array().is_some_and(|ids| ids.iter().any(|i| i == notice.as_str())))),
            "{notice}: wake turn {turn} never a delivery turn listing it: {:?}",
            w.trace().deliveries("session").iter().map(|d| d["turn"].clone()).collect::<Vec<_>>());
        fake.complete("completed");
        w.wait_settled(&[&notice], SETTLE);
        w.daemon.wait_active_turn(&sid, false, T);
    }
    let t = w.finish(STRICT);
    let notices = ids_of_kind(&t, &sid, "systemNotice");
    assert_eq!(notices.len(), 2, "{notices:?}");
    for notice in &notices {
        assert_eq!(t.latest(notice).unwrap()["sender"]["kind"], "claudeTask");
        assert_path(&t, notice, &["waiting{afterTurn}", "sending", "accepted", "inContext"]);
        let at = |state: &str| t.entries.iter().find(|e| e.source == "session" && e.kind == "stateChanged"
            && e.payload["record"]["inputId"] == notice.as_str() && (state.is_empty() || e.payload["record"]["state"] == state))
            .map(|e| e.at_ms).unwrap_or_else(|| panic!("{notice}: no {state:?} record"));
        let lag = at("sending") - at("");
        assert!(lag <= 2000, "{notice}: sent {lag} ms after it was queued (woken, not the observer pass)");
    }
}
one!(s10_claude_task_notice_wakes_delivery, s10_ctask, Kind::Claude, green);

// S10 Claude Task notice during a slow observer pass (r275 item 1(b)). Three
// idle Claude agents; the pass visits them in session id order: B, A, C.
// A's runtime drops its stream and answers every `initialize` of the
// reconnect only after 4 s, so the pass's visit of A is slow (about 14 s:
// three connections, then its 2 s bus long poll). A notice for B queued
// near the end of that visit is `sending` within 2 s: served right after
// A's visit, not after the rest of the pass (C's visit alone is its 2 s bus
// long poll; before r275 the wake waited for it). The pass timing line
// names A.
fn s10_ctask_pass(kind: Kind) {
    const SLOW: Duration = Duration::from_secs(4);
    let w = World::start(&name("s10p", kind), WorldSpec::new(&[kind, kind, kind]).bus());
    let mut order = (0..3).collect::<Vec<usize>>();
    order.sort_by_key(|&index| w.sid(index).to_string());
    let (b, a) = (order[0], order[1]);
    let (sid_a, sid_b) = (w.sid(a).to_string(), w.sid(b).to_string());
    // Every agent delivered in a pass already (woken deliveries need that).
    std::thread::sleep(Duration::from_secs(8));
    let fake_a = w.fake(a);
    let checked = fake_a.calls_of("initialize").len();
    fake_a.script(|s| s.initialize_delay = SLOW);
    fake_a.disconnect_all();
    w.recorder.mark("slowVisit", json!({"agent": sid_a}));
    // Zero-wait bus polls removed the old final 2s long-poll window. Queue
    // during the last slow initialize instead, with 600ms left in A's visit.
    let initializes=fake_a.wait_call("initialize", checked + 3, Duration::from_secs(60));
    let slow_initialize_ends=initializes[checked+2].at+SLOW;
    std::thread::sleep(SLOW-Duration::from_millis(600));
    assert!(Instant::now()<slow_initialize_ends,"missed the slow initialize fixture window");
    let fake_b = w.fake(b);
    let task = "s10pa1b2c3d4e5f6";
    fake_b.emit("claude/task/updated", json!({"threadId": fake_b.thread, "task": {"background": true,
        "description": "s10p", "status": "completed", "taskId": task, "taskType": "local_agent",
        "toolUseId": "toolu_s10p", "turnId": null,
        "notifyParent": {"description": "s10p", "noticeId": task, "originTurnId": "s10p-origin",
            "status": "completed", "summary": "s10p-done", "summaryTruncated": false,
            "taskId": task, "taskType": "local_agent", "toolUseId": "toolu_s10p",
            "transcript": format!("claude:fixture:task:{task}")}}}));
    let notice = wait_kind(&w, b, "systemNotice", 1, T).remove(0);
    wait_active(fake_b);
    let served_during_slow_initialize=Instant::now()<slow_initialize_ends;
    fake_b.complete("completed");
    w.wait_settled(&[&notice], SETTLE);
    w.daemon.wait_active_turn(&sid_b, false, T);
    w.daemon.wait_connected(&sid_a, Duration::from_secs(60));
    // The pass ends after C's visit: its timing line names A's slow visit
    // (the first pass is logged too; A may be its slowest with a plain poll).
    let log_file = w.daemon.home.root.join("logs").join("management.log");
    let prefix = format!("slowest {sid_a} ");
    let slow_pass = |log: &str| log.lines().find(|line| line.starts_with("Claude observer pass: ")
        && line.split_once(prefix.as_str()).and_then(|(_, ms)| ms.trim_end_matches(" ms").parse::<u64>().ok())
            .is_some_and(|ms| ms >= 2 * SLOW.as_millis() as u64)).map(str::to_string);
    let until = Instant::now() + T;
    let mut log = String::new();
    while Instant::now() < until {
        log = std::fs::read_to_string(&log_file).unwrap_or_default();
        if slow_pass(&log).is_some() { break; }
        std::thread::sleep(Duration::from_millis(200));
    }
    let t = w.finish(STRICT);
    assert_eq!(t.latest(&notice).unwrap()["sender"]["kind"], "claudeTask");
    assert_path(&t, &notice, &["waiting{afterTurn}", "sending", "accepted", "inContext"]);
    let at = |state: &str| t.entries.iter().find(|e| e.source == "session" && e.kind == "stateChanged"
        && e.payload["record"]["inputId"] == notice.as_str() && (state.is_empty() || e.payload["record"]["state"] == state))
        .map(|e| e.at_ms).unwrap_or_else(|| panic!("{notice}: no {state:?} record"));
    let lag = at("sending") - at("");
    // The slow visit was A's, in the pass (not a woken delivery).
    let slow_pass = slow_pass(&log);
    assert!(slow_pass.is_some(), "no slow pass naming {sid_a}:\n{}",
        log.lines().filter(|line| line.contains("Claude observer pass")).collect::<Vec<_>>().join("\n"));
    let slow_pass = slow_pass.unwrap();
    eprintln!("s10p: notice sent {lag} ms after it was queued; {slow_pass}");
    assert!(lag <= 2000, "{notice}: sent {lag} ms after it was queued (not served between sessions of the pass)");
    // A dedicated wake worker may serve B before A even finishes connecting,
    // so the pass counter can stay zero. Prove that earlier native start by
    // the fake's actual initialize deadline; do not accept a post-pass enqueue.
    assert!(!slow_pass.contains(" 0 woken served") || served_during_slow_initialize,
        "the wake waited for the pass to end: {slow_pass}");
    eprintln!("s10p: native delivery before slow initialize ended: {served_during_slow_initialize}");
}
one!(s10_claude_task_notice_during_a_slow_pass, s10_ctask_pass, Kind::Claude, green);

// S11 ★ busy, passive note: pendingNotes → next turn accepted/inContext on the
// timeline; appears once (RC2).
fn s11(kind: Kind) {
    let w = World::start(&name("s11", kind), WorldSpec::new(&[kind, Kind::Codex]).bus().script(0, auto(300)));
    w.fake(0).begin_turn();
    w.daemon.wait_active_turn(w.sid(0), true, T);
    assert!(w.bus_send(1, 0, "s11-note", &["--queue-only"]).status.success());
    let note = wait_kind(&w, 0, "agentMessage", 1, T).remove(0);
    w.wait_state(&note, "waiting{passive}", T);
    // A source waiting on the bus has not yet crossed native admission. The
    // next-turn promise starts at real native acceptance, not its projection.
    w.wait_state(&note, "accepted", T);
    w.fake(0).complete("completed");
    w.daemon.wait_active_turn(w.sid(0), false, T);
    w.submit(0, "s11-owner", "auto", json!({})).expect("submit");
    w.wait_settled(&[&note, "s11-owner"], SETTLE);
    let sid = w.sid(0).to_string();
    let t = w.finish(STRICT);
    assert_surfaces(&t, &note, &["pendingNotes", "timeline"]);
    assert_path_any(&t, &note, &[
        &["waiting{passive}", "accepted", "inContext"],
        &["waiting{passive}", "inContext"],
        &["waiting{passive}", "sending", "accepted", "inContext"],
    ]);
    assert!(t.records("session", &note).iter().rev().find(|r| r["state"] == "inContext" || r["state"] == "accepted")
        .is_some_and(|r| r["turnId"].is_string()));
    assert_eq!(ids_of_kind(&t, &sid, "agentMessage").len(), 1, "the note must appear exactly once");
}
both!(s11_busy_passive_note, s11, green);

// A source may arrive before an owner turn yet reach native admission later.
// It must remain pending and be consumed exactly once by a later owner turn.
fn s11_late(kind: Kind) {
    let mut script=auto(300);
    script.external_status_by_text=vec![("s11-late-note".into(),NativeStatus::RetryableError)];
    let w=World::start(&name("s11-late",kind),WorldSpec::new(&[kind,Kind::Codex]).bus().script(0,script));
    w.fake(0).begin_turn();
    w.daemon.wait_active_turn(w.sid(0),true,T);
    assert!(w.bus_send(1,0,"s11-late-note",&["--queue-only"]).status.success());
    let note=wait_kind(&w,0,"agentMessage",1,T).remove(0);
    w.wait_state(&note,"waiting{nativeRetry}",T);
    w.fake(0).complete("completed");
    w.daemon.wait_active_turn(w.sid(0),false,T);
    w.submit(0,"s11-before-accept","auto",json!({})).expect("first owner");
    w.wait_settled(&["s11-before-accept"],SETTLE);
    w.daemon.wait_active_turn(w.sid(0),false,T);
    assert!(!w.trace().path(&note).iter().any(|state|trace::label_matches("inContext",state)));
    assert!(!w.fake(0).inbox().iter().any(|entry|entry.id==note&&entry.persisted));
    let first=w.input_record(0,"s11-before-accept")["turnId"].as_str().unwrap().to_string();
    w.fake(0).script(|script|script.external_status_by_text.clear());
    w.wait_state(&note,"accepted",T);
    assert!(w.fake(0).active_turn().is_none(),"passive acceptance must not start a turn");
    w.submit(0,"s11-after-accept","auto",json!({})).expect("later owner");
    w.wait_settled(&[&note,"s11-after-accept"],SETTLE);
    let later=w.input_record(0,"s11-after-accept")["turnId"].as_str().unwrap().to_string();
    assert_ne!(first,later);
    assert_eq!(w.input_record(0,&note)["turnId"],later);
    assert_eq!(w.fake(0).inbox().iter().filter(|entry|entry.id==note&&entry.persisted).count(),1);
    let sid=w.sid(0).to_string();
    let trace=w.finish(STRICT);
    assert_surfaces(&trace,&note,&["pendingNotes","timeline"]);
    assert_eq!(ids_of_kind(&trace,&sid,"agentMessage").len(),1);
}
both!(s11_late_acceptance_uses_later_turn, s11_late, green);


// S12 native Held(PlanMode) / RetryableError / Conflict for a wake (RC8):
// one wake each, scripted per text on the recipient's ExternalInput
// (Codex: C3's CUTEX_TEST_FAKE_EXTERNAL_INPUT hook; Claude: r8 native
// ingress). Held and retry settle once the runtime takes them; the
// conflict stays uncertain. Never an "awaiting receipt" accepted.
fn s12(kind: Kind) {
    let mut script = auto(300);
    script.external_status_by_text = vec![
        ("s12-held".into(), NativeStatus::Held("plan_mode")),
        ("s12-retry".into(), NativeStatus::RetryableError),
        ("s12-conflict".into(), NativeStatus::Conflict),
    ];
    let w = World::start(&name("s12", kind), WorldSpec::new(&[kind, Kind::Codex]).bus().script(0, script));
    let mut ids = Vec::new();
    for (i, (text, label)) in [("s12-held", "waiting{nativeHeld}"), ("s12-retry", "waiting{nativeRetry}"), ("s12-conflict", "uncertain{nativeConflict}")]
        .into_iter().enumerate()
    {
        assert!(w.bus_send(1, 0, text, &[]).status.success());
        let id = wait_kind(&w, 0, "agentMessage", i + 1, T).remove(i);
        w.wait_state(&id, label, Duration::from_secs(30));
        ids.push(id);
    }
    assert!(w.trace().records("session", &ids[0]).iter().any(|r| r["reason"] == "nativeHeld" && r["details"]["nativeReason"] == "plan_mode"),
        "waiting{{nativeHeld}} carries details.nativeReason");
    // The runtime takes the held and the retried wake now.
    w.fake(0).script(|s| s.external_status_by_text.retain(|(key, _)| key == "s12-conflict"));
    w.wait_settled(&[&ids[0], &ids[1], &ids[2]], SETTLE);
    let t = w.finish(STRICT);
    assert_path_any(&t, &ids[0], &[&["waiting{afterTurn}", "sending", "waiting{nativeHeld}", "sending", "accepted", "inContext"],
        &["waiting{afterTurn}", "sending", "waiting{nativeHeld}", "accepted", "inContext"]]);
    assert_path_any(&t, &ids[1], &[&["waiting{afterTurn}", "sending", "waiting{nativeRetry}", "sending", "accepted", "inContext"],
        &["waiting{afterTurn}", "sending", "waiting{nativeRetry}", "accepted", "inContext"]]);
    assert_eq!(t.path(&ids[2]).last().map(String::as_str), Some("uncertain{nativeConflict}"));
}
both!(s12_native_held_retryable_conflict, s12, green);

// S13 ★ offline: owner timeline waiting{agentOffline} → online → inContext;
// tell to an offline agent is rejected{targetUnavailable} with a sender-side
// record (decision 2).
fn s13(kind: Kind) {
    let w = World::start(&name("s13", kind), WorldSpec::new(&[kind, Kind::Codex]).bus().offline(0).script(0, auto(300)));
    assert!(!w.daemon.connected(w.sid(0)));
    if kind == Kind::Codex {
        // The auto-start reconnects the seeded binding (its runtime PID is
        // alive); a refused connection would cut over to a real runtime
        // launch. So the fake runtime is "still starting": it accepts the
        // connection but answers `initialize` only after 12 s (the client's
        // request timeout is 30 s). The agent stays offline (not connected,
        // not on the bus) for that window.
        w.fake(0).script(|s| { s.refuse_connections = false; s.initialize_delay = Duration::from_secs(12); });
    }
    // Like the desktop: owner input to an offline agent always starts it (decision 8).
    w.submit(0, "s13-owner", "auto", json!({"startIfOffline": true})).ok();
    w.wait_state("s13-owner", "waiting{agentOffline}", T);
    // Agent 1 tells offline agent 0.
    let mut tell = w.daemon.command(&["agent", "send", w.sid(0), "s13-tell"]);
    tell.env("CUTEX_AGENT_ID", &w.agent(1).runtime_agent_id);
    let out = tell.output().unwrap();
    w.recorder.mark("tell", json!({"status": out.status.code(), "stderr": String::from_utf8_lossy(&out.stderr)}));
    assert!(!out.status.success(), "decision 2: tell to an offline agent is refused");
    assert!(String::from_utf8_lossy(&out.stderr).contains("targetUnavailable"), "the refusal names targetUnavailable");
    let rejected = w.wait(T, |t| t.all_records().iter().any(|r| r["cutexSessionId"] == w.sid(0) && r["sender"]["cutexSessionId"] == w.sid(1) && r["state"] == "rejected" && r["reason"] == "targetUnavailable" && r["surface"] == "none"));
    // Decision 2 record: in the target's ledger (records are per recipient), sender = agent 1, surface none.
    assert!(rejected, "decision 2: a rejected{{targetUnavailable}} record naming the sender");
    w.fake(0).script(|s| s.refuse_connections = false);
    w.daemon.wait_connected(w.sid(0), Duration::from_secs(60));
    w.fake(0).script(|s| s.initialize_delay = Duration::ZERO);
    w.wait_settled(&["s13-owner"], SETTLE);
    let t = w.finish(STRICT);
    assert_path(&t, "s13-owner", &["received", "waiting{agentOffline}", "sending", "inContext"]);
    // Owner decision 8: always auto-start; the message waits in nextUp and moves once, to the timeline.
    assert_surfaces(&t, "s13-owner", &["nextUp", "timeline"]);
}
both!(s13_offline_owner_and_tell, s13, green);

// S14–S18 need a delegation (Ask). The handler runs it in an exclusive turn
// (`claude/input/submit` with `admission: exclusive_turn` on Claude; on
// Codex only through a reviewed stock bundle's ExternalInputClient, which a
// fake cannot provide: asks_runtime.rs `with_client_policy`), and the result
// comes back to the initiator the same way. So handlers are Claude fakes.

/// The first string under any key of `v` (recursively) starting with one of
/// `prefixes`.
fn find_id(v: &Value, prefixes: &[&str]) -> Option<String> {
    match v {
        Value::String(s) if prefixes.iter().any(|p| s.starts_with(p)) => Some(s.clone()),
        Value::Object(map) => {
            for key in ["askId", "ask_id", "delegationId", "delegation_id", "delegation", "id"] {
                if let Some(found) = map.get(key).and_then(|v| find_id(v, prefixes)) {
                    return Some(found);
                }
            }
            map.values().find_map(|v| find_id(v, prefixes))
        }
        Value::Array(values) => values.iter().find_map(|v| find_id(v, prefixes)),
        _ => None,
    }
}

const ASK_PREFIXES: &[&str] = &["ask-", "delegation-", "ask_"];

/// The owner's Ask to `target` (`cutex/ask/create` with the owner
/// credential; C4's S14 route). Returns the ask id.
fn owner_ask(w: &World, target: usize, text: &str) -> String {
    w.wait_asks_presence(&[target]);
    let result = w.daemon.cutex(w.sid(target), "cutex/ask/create", json!({"text": text, "actionId": format!("{text}-{}", uuid::Uuid::new_v4().simple())}));
    w.recorder.mark("askCreate", json!({"target": w.sid(target), "result": result}));
    let value = result.unwrap_or_else(|e| panic!("cutex/ask/create: {e}"));
    find_id(&value, ASK_PREFIXES).unwrap_or_else(|| panic!("cutex/ask/create gave no ask id: {value}"))
}

/// Agent `initiator` delegates to agent `target` (`cutex-mcp` `agent_delegate`
/// as the initiator; C4's S15–S18 route). Returns the delegation id.
fn agent_delegate(w: &World, initiator: usize, target: usize, text: &str) -> String {
    w.wait_asks_presence(&[initiator, target]);
    let mut result = w.mcp(initiator, "agent_delegate", json!({"target": w.sid(target), "text": text, "title": text}))
        .unwrap_or_else(|e| panic!("agent_delegate: {e}"));
    // An async action receipt (`status: running`): poll action_status.
    let until = Instant::now() + T;
    while find_id(&result, ASK_PREFIXES).is_none() && result["status"] == "running" && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(250));
        let action = result["action_id"].clone();
        result = w.mcp(initiator, "action_status", json!({"action_id": action, "tool": "agent_delegate"}))
            .unwrap_or_else(|e| panic!("action_status: {e}"));
        if result["status"] == "running" && result["action_id"].is_null() {
            result["action_id"] = action;
        }
    }
    let delegation = find_id(&result, ASK_PREFIXES).unwrap_or_else(|| panic!("agent_delegate gave no delegation id: {result}"));
    // A handler that "can do more" than the initiator needs the owner's
    // review first (state pendingReview): the owner approves it.
    let inner = if result["result"].is_object() { &result["result"] } else { &result };
    if inner["state"] == "pendingReview" {
        let approval = inner["approval_id"].as_str().unwrap_or_else(|| panic!("pendingReview without approval_id: {result}"));
        let (status, body) = w.daemon.http("POST", &format!("/v2/approvals/{approval}/approve"), Some(&json!({"note": "input_ledger_e2e"})))
            .expect("approve");
        w.recorder.mark("approval", json!({"approval": approval, "status": status, "body": body}));
        assert_eq!(status, 200, "approve {approval}: {body}");
    }
    delegation
}

/// The handler `target` received the delegation (an inbox input with
/// `eventType` ASK) and runs it in its own turn.
fn wait_delegation_turn(w: &World, target: usize) -> Value {
    let until = Instant::now() + T;
    loop {
        // Claude: `claude/input/submit` params; Codex: an ExternalInput envelope.
        let is_ask = |p: &Value| p["eventType"] == "ASK" || p["message"]["type"] == "ASK";
        if let Some(entry) = w.fake(target).inbox().into_iter().find(|e| is_ask(&e.params) && e.turn.is_some()) {
            return entry.params;
        }
        assert!(Instant::now() < until, "{}: the handler never ran the delegation; calls {:?}", w.name,
            w.fake(target).calls().iter().map(|c| c.method.clone()).collect::<Vec<_>>());
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// An Ask from `initiator` to `target` that is running on the target now.
fn delegation_turn(w: &World, initiator: usize, target: usize) -> String {
    let ask = agent_delegate(w, initiator, target, &format!("{}-ask", w.name));
    wait_delegation_turn(w, target);
    ask
}

/// A completed delegation of `initiator` to `target` (the target fake
/// completes its turns by itself): the id of the `delegationResult` record
/// in the initiator's ledger.
fn delegation_result(w: &World, initiator: usize, target: usize) -> String {
    let sid = w.sid(initiator).to_string();
    let before = ids_of_kind(&w.trace(), &sid, "delegationResult").len();
    agent_delegate(w, initiator, target, &format!("{}-ask-{before}", w.name));
    wait_delegation_turn(w, target);
    wait_kind(w, initiator, "delegationResult", before + 1, SETTLE).remove(before)
}

// ---------------------------------------------------------------------------
// r274.1 (054 §3.2): clients join native timeline items and records on
// `timelineItemId`; a record without one is shown again as a tail row.

/// The native timeline items each fake runtime holds, by session: every item
/// of every turn (owner `userMessage` items also by clientId) and every
/// inbox input (ExternalInput / Claude local inbox: the item id is the
/// message id). Read before [`World::finish`] (which stops the fakes).
struct NativeItems {
    sessions: Vec<(String, std::collections::BTreeSet<String>, std::collections::BTreeMap<String, String>)>,
}

fn native_items(w: &World) -> NativeItems {
    let sessions = (0..w.agents.len())
        .map(|index| {
            let mut items = std::collections::BTreeSet::new();
            let mut users = std::collections::BTreeMap::new();
            for turn in w.fake(index).turns() {
                for item in turn["items"].as_array().into_iter().flatten() {
                    let Some(id) = item["id"].as_str() else { continue };
                    items.insert(id.to_string());
                    if item["type"] == "userMessage" {
                        if let Some(client) = item["clientId"].as_str().filter(|client| !client.is_empty()) {
                            users.insert(id.to_string(), client.to_string());
                        }
                    }
                }
            }
            for entry in w.fake(index).inbox() {
                items.insert(entry.id.clone());
            }
            (w.sid(index).to_string(), items, users)
        })
        .collect();
    NativeItems { sessions }
}

/// Every `inContext` timeline record of a delegation request (target side),
/// result / question / status, bus message, alarm or notice names the native
/// item its runtime saw (a request: its attempt's message, never the askId); an owner record (joined on its clientId) names, when it names one,
/// the user item carrying its clientId. Returns how many records were checked.
#[track_caller]
fn assert_bound_to_native_items(t: &Trace, native: &NativeItems) -> usize {
    const BOUND: &[&str] = &[
        "delegationRequest", "delegationResult", "delegationQuestion", "delegationStatus", "agentMessage", "peerMessage", "alarm", "systemNotice",
    ];
    let mut checked = 0;
    for id in t.input_ids() {
        let Some(record) = t.latest(&id) else { continue };
        if record["state"] != "inContext" || record["surface"] != "timeline" {
            continue;
        }
        let session = record["cutexSessionId"].as_str().unwrap_or_default();
        let Some((_, items, users)) = native.sessions.iter().find(|(sid, _, _)| sid == session) else { continue };
        let kind = record["kind"].as_str().unwrap_or_default();
        let item = record["timelineItemId"].as_str();
        if BOUND.contains(&kind) {
            let item = item.unwrap_or_else(|| panic!("{kind} {id} is inContext on the timeline without a timelineItemId: {record}; native items of {session}: {items:?}"));
            assert!(items.contains(item), "{kind} {id}: timelineItemId {item} is no native item of {session}: {items:?}");
            if kind == "delegationRequest" {
                assert!(item.starts_with("cutex-ask-msg-") && item != id, "delegationRequest {id}: {item} is not its attempt's message");
            }
            checked += 1;
        } else if kind == "owner" {
            if let Some(item) = item {
                assert_eq!(users.get(item).map(String::as_str), Some(id.as_str()), "owner {id}: timelineItemId {item} is not its user item");
            }
            checked += 1;
        }
    }
    checked
}

/// `session` holds `n` delegation request records, each bound to a native
/// item (the attempt's message, checked by [`assert_bound_to_native_items`]).
#[track_caller]
fn assert_requests_bound(t: &Trace, session: &str, n: usize) {
    let requests = ids_of_kind(t, session, "delegationRequest");
    assert_eq!(requests.len(), n, "{session}: {requests:?}");
    for id in &requests {
        assert!(t.latest(id).is_some_and(|r| r["timelineItemId"].is_string()), "{:?}", t.latest(id));
    }
}

// S14 ★ in a delegation turn (target side): delivery.turn.kind = ask; owner
// steers or waiting{turnRunning}; a wake waits afterTurn.
fn s14(kind: Kind) {
    let w = World::start(&name("s14", kind), WorldSpec::new(&[Kind::Codex, kind]).bus().script(1, auto(3000)));
    let ask = owner_ask(&w, 1, "s14-ask");
    wait_delegation_turn(&w, 1);
    assert!(w.wait(T, |t| t.deliveries("session").iter().any(|d| d["turn"]["kind"] == "ask" && d["turn"]["askId"] == ask.as_str())),
        "delivery.turn.kind = ask with askId {ask}: {:?}", w.trace().deliveries("session"));
    w.submit(1, "s14-owner", "auto", json!({})).expect("submit");
    assert!(w.bus_send(0, 1, "s14-wake", &[]).status.success());
    let wake = wait_kind(&w, 1, "agentMessage", 1, T).remove(0);
    w.wait_settled(&["s14-owner", &wake], SETTLE);
    let native = native_items(&w);
    let t = w.finish(STRICT);
    assert!(assert_bound_to_native_items(&t, &native) >= 3, "the owner input, the wake and the request are checked");
    assert!(t.latest(&ask).is_some_and(|r| r["kind"] == "delegationRequest" && r["timelineItemId"].is_string()), "{:?}", t.latest(&ask));
    // 054 §5.2 S14: the owner steers into the ask turn or waits
    // `turnRunning`; §2.6 puts a running turn (kind ask) before any lease, and
    // §2.4 keeps `delegationLease` for a real Ask lease with no turn, so a
    // waiting{delegationLease} while the ask turn runs is a product bug (C2).
    // Claude's accepted stages are legal extra steps (§2.4).
    assert_path_any_collapsing_stages(&t, "s14-owner", &[
        &["received", "sending", "accepted", "inContext"],
        &["received", "waiting{turnRunning}", "sending", "inContext"],
        &["received", "waiting{turnRunning}", "sending", "accepted", "inContext"],
    ]);
    assert_eq!(t.path(&wake).first().map(String::as_str), Some("waiting{afterTurn}"));
}
both!(s14_owner_and_wake_during_delegation_turn, s14, green);

// S15 ★ initiator awaits results, 2 within the coalescing window:
// waiting{resultCoalescing} → sending → accepted → inContext; no flapping
// while the host lock is busy (RC5); inputId stable across event ids.
fn s15(kind: Kind) {
    let spec = WorldSpec::new(&[kind, Kind::Claude, Kind::Claude]).bus().persistent(0).persistent(1).persistent(2).script(0, auto(300)).script(1, auto(300)).script(2, auto(300));
    let w = World::start(&name("s15", kind), spec);
    let a = delegation_turn(&w, 0, 1);
    let b = delegation_turn(&w, 0, 2);
    let ids = wait_kind(&w, 0, "delegationResult", 1, SETTLE);
    w.recorder.mark("delegations", json!({"a": a, "b": b}));
    let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    w.wait_settled(&id_refs, SETTLE);
    let native = native_items(&w);
    let targets = [w.sid(1).to_string(), w.sid(2).to_string()];
    let t = w.finish(STRICT);
    assert!(assert_bound_to_native_items(&t, &native) >= ids.len() + 2, "the results and both requests are checked");
    for sid in &targets {
        assert_requests_bound(&t, sid, 1);
    }
    for id in &ids {
        assert_path(&t, id, &["waiting{resultCoalescing}", "sending", "accepted", "inContext"]);
        assert_eq!(t.latest(id).unwrap()["kind"], "delegationResult");
    }
}
both!(s15_initiator_results_coalesce, s15, green);

// S16 initiator agent_wait claims a result: inContext{claimedByWait}, surface none.
fn s16(kind: Kind) {
    let mut spec = WorldSpec::new(&[kind, Kind::Claude]).bus().persistent(0).persistent(1).script(1, auto(1500));
    if kind == Kind::Claude {
        // A Claude profile offers delegation_wait only when the owner turned
        // it on (settings_api.rs `default_delegation_wait`): seed one that has.
        spec = spec.delegation_wait_profile(0);
    }
    let w = World::start(&name("s16", kind), spec);
    w.fake(0).begin_turn();
    w.daemon.wait_active_turn(w.sid(0), true, T);
    let ask = delegation_turn(&w, 0, 1);
    let waited = w.mcp(0, "delegation_wait", json!({"delegations": [ask], "timeout_ms": 60000}));
    w.recorder.mark("delegationWait", json!({"result": waited.as_ref().map_err(|e| e.clone())}));
    let result = wait_kind(&w, 0, "delegationResult", 1, SETTLE).remove(0);
    w.wait_settled(&[&result], SETTLE);
    w.fake(0).complete("completed");
    let native = native_items(&w);
    let target = w.sid(1).to_string();
    let t = w.finish(STRICT);
    assert!(assert_bound_to_native_items(&t, &native) >= 1, "the request is checked");
    assert_requests_bound(&t, &target, 1);
    assert!(t.path(&result).last().is_some_and(|l| l == "inContext(claimedByWait)"), "{:?}", t.path(&result));
    assert!(t.latest(&result).is_some_and(|r| r["timelineItemId"].is_null()), "a result read by a wait is in no native item");
    assert_eq!(t.surfaces(&result).last().map(String::as_str), Some("none"));
}
both!(s16_agent_wait_claims_result, s16, green);

// S17 ★ Event lease, 34 s disconnect around turn end, + 3 owner queued:
// one reconcile releases the lease; queued go inContext in order;
// lease.reconcile visible; no per-second full history reads.
fn s17(kind: Kind) {
    let w = World::start(&name("s17", kind), WorldSpec::new(&[kind, Kind::Claude]).bus().persistent(0).persistent(1).script(0, auto(300)).script(1, auto(300)));
    let result = delegation_result(&w, 0, 1);
    w.wait_state(&result, "accepted", T);
    for i in 0..3 {
        w.submit(0, &format!("s17-q{i}"), "queue", json!({})).expect("submit");
    }
    w.fake(0).script(|s| { s.refuse_connections = true; s.drop_turn_completed = true; });
    w.fake(0).disconnect_all();
    w.fake(0).complete("completed");
    let disconnected_at = Instant::now();
    std::thread::sleep(Duration::from_secs(34));
    w.fake(0).script(|s| { s.refuse_connections = false; s.drop_turn_completed = false; });
    w.daemon.wait_connected(w.sid(0), Duration::from_secs(60));
    w.wait_settled(&[&result, "s17-q0", "s17-q1", "s17-q2"], SETTLE);
    // §5.2 S17: no per-second whole-history reads (read_turn). A whole read is
    // `thread/read` with turns or a `thread/turns/list` page with full items;
    // metadata reads (`includeTurns: false`, `itemsView: notLoaded`) are cheap.
    let full = |c: &fake_runtime::Call| match c.method.as_str() {
        "thread/read" => c.params["includeTurns"] == true,
        "thread/turns/list" => c.params["itemsView"] == "full",
        _ => false,
    };
    let after: Vec<fake_runtime::Call> = w.fake(0).calls().into_iter().filter(|c| c.at >= disconnected_at).collect();
    let reads = after.iter().filter(|c| full(c)).count();
    let read_times: Vec<String> = after.iter().filter(|c| matches!(c.method.as_str(), "thread/turns/list" | "thread/read" | "thread/items/list"))
        .map(|c| format!("{}{}@{}ms", c.method, if full(c) { "(full)" } else { "" }, c.at.duration_since(disconnected_at).as_millis())).collect();
    w.recorder.mark("historyReads", json!({"full": reads, "reads": read_times}));
    assert!(reads <= 3, "{reads} whole-history reads: no per-second full reads (one reconcile): {read_times:?}");
    let order: Vec<_> = w.fake(0).calls_of("turn/start").iter().map(|c| c.params["clientUserMessageId"].clone()).collect();
    let native = native_items(&w);
    let t = w.finish(STRICT);
    assert!(assert_bound_to_native_items(&t, &native) >= 5, "the request, the result and the 3 queued owner inputs are checked");
    assert!(t.latest(&result).is_some_and(|r| r["timelineItemId"].is_string()), "{:?}", t.latest(&result));
    assert!(t.deliveries("session").iter().any(|d| d["lease"]["reconcile"].is_object()), "lease.reconcile is visible");
    assert!(order.ends_with(&[json!("s17-q0"), json!("s17-q1"), json!("s17-q2")]), "{order:?}");
}
both!(s17_event_lease_disconnect_34s, s17, green);

// S18 same as S17 but history unreadable: backoff, stale after 10,
// lease/release → acked inContext, others uncertain{leaseReleased}.
//
// The fake drops turn/completed on a LIVE link, so the bound turn stays the
// runtime's active turn. The stall check (turn_stall.rs, Director decision
// 11-02) reads the quiet turn once per backoff step; history unreadable means
// every read fails (thread/read too), so nothing can prove the turn ended and
// each read is an unproven lease check (054 §2.12 c): stale after 10, then the
// owner releases. With the stall threshold at 2 s (debug override) the 10
// reads take ~100 s (2, 4, 8, 12, 12 … s) instead of ~4 h (5 → 30 min).
// The turn cannot settle before the lease is stale here: S18b covers that.
fn s18(kind: Kind) {
    let w = World::start(&name("s18", kind), WorldSpec::new(&[kind, Kind::Claude]).bus().persistent(0).persistent(1).script(0, auto(5000)).script(1, auto(300)).turn_stall_secs(2));
    let result = delegation_result(&w, 0, 1);
    w.wait_state(&result, "accepted", T);
    w.fake(0).script(|s| { s.history_unreadable = true; s.drop_turn_completed = true; });
    // Agent 0 completes its turns after 5 s by itself: the turn that took
    // the result is still running here, and this completion is the lost one.
    w.daemon.wait_active_turn(w.sid(0), true, T);
    assert!(w.fake(0).complete("completed").is_some(), "the result's turn was running");
    let stale = w.wait(Duration::from_secs(5 * 60), |t| t.deliveries("session").iter().any(|d| d["lease"]["reconcile"]["state"] == "stale"));
    assert!(stale, "lease never became stale");
    let lease = w.trace().deliveries("session").into_iter().rev().find_map(|d| d.get("lease").cloned()).unwrap();
    w.daemon.cutex(w.sid(0), "cutex/input/lease/release", json!({"expectedLease": lease})).expect("lease/release");
    w.wait_settled(&[&result], T);
    let t = w.finish(STRICT);
    assert_path_any(&t, &result, &[&["waiting", "sending", "accepted", "inContext"], &["waiting", "sending", "accepted", "uncertain{leaseReleased}"]]);
}
both!(s18_event_lease_history_unreadable_release, s18, green);

// S18b turn/completed lost on a live link, history readable: the stall check
// finds the thread idle and settles the turn as if turn/completed had arrived
// (same path: lease released, result inContext); the lease never goes stale
// and no whole-history read happens.
fn s18b(kind: Kind) {
    let w = World::start(&name("s18b", kind), WorldSpec::new(&[kind, Kind::Claude]).bus().persistent(0).persistent(1).script(0, auto(5000)).script(1, auto(300)).turn_stall_secs(2));
    let result = delegation_result(&w, 0, 1);
    w.wait_state(&result, "accepted", T);
    w.fake(0).script(|s| s.drop_turn_completed = true);
    let dropped_at = Instant::now();
    w.daemon.wait_active_turn(w.sid(0), true, T);
    assert!(w.fake(0).complete("completed").is_some(), "the result's turn was running");
    w.wait_settled(&[&result], T);
    w.daemon.wait_active_turn(w.sid(0), false, T);
    let full = |c: &fake_runtime::Call| match c.method.as_str() {
        "thread/read" => c.params["includeTurns"] == true,
        "thread/turns/list" => c.params["itemsView"] == "full",
        _ => false,
    };
    let after: Vec<fake_runtime::Call> = w.fake(0).calls().into_iter().filter(|c| c.at >= dropped_at).collect();
    let status_reads = after.iter().filter(|c| c.method == "thread/read" && c.params["includeTurns"] == false).count();
    let reads: Vec<String> = after.iter().filter(|c| matches!(c.method.as_str(), "thread/turns/list" | "thread/read" | "thread/items/list"))
        .map(|c| format!("{}{}@{}ms", c.method, if full(c) { "(full)" } else { "" }, c.at.duration_since(dropped_at).as_millis())).collect();
    w.recorder.mark("historyReads", json!({"statusReads": status_reads, "reads": reads}));
    let t = w.finish(STRICT);
    assert!(status_reads >= 1, "the stall check read the thread status: {reads:?}");
    assert!(!after.iter().any(full), "no whole-history read: {reads:?}");
    assert!(!t.deliveries("session").iter().any(|d| d["lease"]["reconcile"]["state"] == "stale"), "settled before the lease went stale");
    assert_path(&t, &result, &["waiting", "sending", "accepted", "inContext"]);
}
both!(s18b_lost_turn_completed_settles_by_stall_check, s18b, green);

// S19 ★ runtime disconnect mid-turn: delivery disconnected (no turn), records
// keep state + stalled; reconnect settles; the other runtime stays connected.
fn s19_run(kind: Kind, owner_online: bool) {
    let mut busy = Script::default();
    busy.echo_steer_item = false;
    busy.claude_stages = false;
    let w = World::start(&name("s19", kind), WorldSpec::new(&[kind, kind]).script(0, busy));
    w.fake(0).begin_turn();
    w.daemon.wait_active_turn(w.sid(0), true, T);
    w.submit(0, "s19-m1", "auto", json!({})).expect("submit");
    w.wait_state("s19-m1", "accepted", T);
    w.fake(0).script(|s| s.refuse_connections = true);
    w.fake(0).disconnect_all();
    let sid0 = w.sid(0).to_string();
    assert!(w.wait(T, |t| t.deliveries("session").iter().any(|d| d["state"] == "disconnected" && d["turn"].is_null())), "delivery=disconnected with turn null");
    assert!(w.wait(T, |t| t.latest("s19-m1").is_some_and(|r| r["state"] == "accepted" && r["stalled"]["reason"] == "runtimeDisconnected")));
    assert!(w.daemon.connected(w.sid(1)) && w.fake(1).connected_peers() > 0, "the other runtime stays connected");
    w.fake(0).script(|s| s.refuse_connections = false);
    if owner_online {
        // The owner's explicit "start" (desktop / `cutex/runtime/online`,
        // no terminal): with the binding's runtime still alive it reconnects
        // the same binding and generation (management_context.rs
        // `PersistedRuntimeRecoveryAction::Reconnect` → `connect_runtime`).
        let online = w.daemon.cutex(&sid0, "cutex/runtime/online", json!({"openVisibleTerminal": false}));
        w.recorder.mark("ownerOnline", json!({"result": online}));
        online.unwrap_or_else(|e| panic!("cutex/runtime/online: {e}"));
    }
    w.daemon.wait_connected(&sid0, Duration::from_secs(60));
    w.fake(0).complete("completed");
    w.wait_settled(&["s19-m1"], SETTLE);
    let t = w.finish(STRICT);
    assert_path_any(&t, "s19-m1", &[&["received", "sending", "accepted", "inContext"], &["received", "sending", "accepted", "uncertain"]]);
}
fn s19(kind: Kind) {
    s19_run(kind, false)
}
both!(s19_runtime_disconnect_mid_turn, s19, green);

// S19 with the reconnect the owner asks for (`cutex/runtime/online`): the
// same-generation reconnect through the owner path instead of the daemon's
// own (late-adoption observer, r274).
#[test]
fn s19_runtime_disconnect_owner_online_codex() {
    s19_run(Kind::Codex, true)
}

// S20 ★ management restart with unsettled records: stateRevision continues,
// epoch unchanged, unsettled records settle or stay after one reconcile.
fn s20(kind: Kind) {
    let mut script = auto(300);
    script.echo_steer_item = false;
    script.claude_stages = false;
    let mut w = World::start(&name("s20", kind), WorldSpec::new(&[kind]).script(0, script));
    let sid = w.sid(0).to_string();
    w.fake(0).begin_turn();
    w.daemon.wait_active_turn(&sid, true, T);
    w.submit(0, "s20-steer", "auto", json!({})).expect("submit");
    w.submit(0, "s20-queued", "queue", json!({})).expect("submit");
    w.wait_state("s20-steer", "accepted", T);
    w.wait_state("s20-queued", "waiting{explicitQueue}", T);
    let before = w.recorder.read_input_list(&w.daemon, &sid).expect("input/list");
    w.recorder.mark("restart", json!({"stateRevision": before["stateRevision"]}));
    w.daemon.restart();
    w.daemon.wait_connected(&sid, Duration::from_secs(60));
    let after = w.recorder.read_input_list(&w.daemon, &sid).expect("input/list");
    assert_eq!(after["ledgerEpoch"], before["ledgerEpoch"]);
    assert!(after["stateRevision"].as_u64() >= before["stateRevision"].as_u64());
    w.fake(0).complete("completed");
    w.wait_settled(&["s20-steer", "s20-queued"], SETTLE);
    let t = w.finish(STRICT);
    assert!(before["records"].as_array().is_some_and(|r| r.len() == 2), "{before}");
    assert_path(&t, "s20-queued", &["received", "waiting{explicitQueue}", "sending", "inContext"]);
}
both!(s20_management_restart_unsettled, s20, green);

// S20c (r274.2 item 4, host-a 11-03) management restart while the agent's
// turn runs on: a delegation result holds an event lease bound to the turn
// it started; management restarts, the runtime keeps running that turn
// (same connection target, same generation; its `thread/resume` names no
// turn, like the real Claude adapter). After the reconnect management knows
// the running turn again, exactly as if it had never restarted (the
// `control` variants): activeTurnId, delivery `turn` (kind delivery, the
// lease kept), the owner's `auto` message waits or joins the running turn
// (a steer, as in S14 / S20) and is never sent as if the agent were idle (no
// new turn). The turn then completes normally: lease released, result and
// owner message inContext, never stale.
fn s20c_run(kind: Kind, restart: bool) {
    let mut script = Script::default();
    // Codex: no turns in the resume either (the in-progress turn only in
    // `thread/turns/list`), so both runtimes take the recovery path.
    script.resume_without_turns = true;
    let spec = WorldSpec::new(&[kind, Kind::Claude]).bus().persistent(0).persistent(1).script(0, script).script(1, auto(300));
    let mut w = World::start(&name(if restart { "s20c" } else { "s20c-control" }, kind), spec);
    let sid = w.sid(0).to_string();
    let result = delegation_result(&w, 0, 1);
    w.wait_state(&result, "accepted", T);
    w.daemon.wait_active_turn(&sid, true, T);
    let running = w.fake(0).active_turn().and_then(|t| t["id"].as_str().map(str::to_string)).expect("the result's turn runs");
    let delivery_of = |t: &Trace, since: i64| {
        t.entries.iter().rev()
            .filter(|e| e.at_ms >= since && e.source == "session" && e.kind == "deliveryChanged"
                && e.cutex_session_id.as_deref() == Some(sid.as_str()))
            .find_map(|e| e.payload.get("delivery").cloned())
    };
    let before = delivery_of(&w.trace(), 0).expect("a delivery before the restart");
    assert_eq!(before["lease"]["holder"], "event", "{before}");
    assert_eq!(before["lease"]["turnId"], json!(running), "{before}");
    let generation = w.daemon.session(&sid)["runtime"]["runtimeGeneration"].clone();
    if restart {
        w.recorder.mark("restart", json!({"turn": running, "generation": generation}));
        w.daemon.restart();
        w.daemon.wait_connected(&sid, Duration::from_secs(60));
    }
    let since = trace::now_ms();
    assert_eq!(w.daemon.active_turn(&sid).as_deref(), Some(running.as_str()), "activeTurnId is the running turn");
    assert_eq!(w.daemon.session(&sid)["runtime"]["runtimeGeneration"], generation, "same generation");
    assert_eq!(w.fake(0).active_turn().map(|t| t["id"].clone()), Some(json!(running)), "the fake still runs it");
    let turn_delivery = |d: &Value| d["state"] == "turn" && d["turn"]["turnId"] == json!(running)
        && d["turn"]["kind"] == "delivery" && d["lease"]["holder"] == "event" && d["lease"]["turnId"] == json!(running);
    let calls_before = w.fake(0).calls().len();
    w.submit(0, "s20c-owner", "auto", json!({})).expect("submit");
    // Waiting, or joined the running turn; never a turn of its own.
    let held_or_joined = |r: &Value| r["state"] == "waiting"
        || (matches!(r["state"].as_str(), Some("accepted" | "inContext")) && r["turnId"] == json!(running));
    assert!(w.wait(T, |t| delivery_of(t, since).is_some_and(|d| turn_delivery(&d)) && t.latest("s20c-owner").is_some_and(|r| held_or_joined(&r))),
        "delivery turn (the running turn, the lease kept) and the owner message waiting or in that turn: {:?} / {:?}",
        delivery_of(&w.trace(), since), w.trace().latest("s20c-owner"));
    // A few seconds more: the lease reconcile, the stall watch and admission
    // run; nothing takes the agent for idle.
    std::thread::sleep(Duration::from_secs(5));
    let started: Vec<Value> = w.fake(0).calls().into_iter().skip(calls_before)
        .filter(|c| c.method == "turn/start").map(|c| c.params).collect();
    assert!(started.is_empty(), "a new turn was started while the turn ran: {started:?}");
    let owner = w.trace().latest("s20c-owner").expect("owner record");
    assert!(held_or_joined(&owner), "{owner}");
    let now = delivery_of(&w.trace(), 0).expect("delivery");
    assert!(turn_delivery(&now), "the lease is not released, delivery still the running turn: {now}");
    assert_eq!(w.daemon.active_turn(&sid).as_deref(), Some(running.as_str()));
    assert_eq!(w.fake(0).complete("completed").as_deref(), Some(running.as_str()));
    w.wait_settled(&[&result, "s20c-owner"], SETTLE);
    w.daemon.wait_active_turn(&sid, false, T);
    let t = w.finish(STRICT);
    assert_path(&t, &result, &["waiting", "sending", "accepted", "inContext"]);
    assert_path_any_collapsing_stages(&t, "s20c-owner", &[
        &["received", "sending", "inContext"],
        &["received", "sending", "accepted", "inContext"],
        &["received", "waiting", "sending", "inContext"],
        &["received", "waiting", "sending", "accepted", "inContext"],
    ]);
    let last = delivery_of(&t, 0).expect("delivery");
    assert!(last["lease"].is_null(), "the lease is released after the turn: {last}");
    assert!(!t.deliveries("session").iter().any(|d| d["lease"]["reconcile"]["state"] == "stale"), "never stale");
}
fn s20c(kind: Kind) {
    s20c_run(kind, true)
}
both!(s20c_management_restart_keeps_the_running_turn, s20c, green);
fn s20c_control(kind: Kind) {
    s20c_run(kind, false)
}
both!(s20c_control_no_restart, s20c_control, green);

// S21 cutex/stream/reset (ledger lost): new ledgerEpoch, reset notice,
// unsettled records rebuilt from the underlying stores.
fn s21(kind: Kind) {
    let mut w = World::start(&name("s21", kind), WorldSpec::new(&[kind]).script(0, auto(300)));
    let sid = w.sid(0).to_string();
    w.fake(0).begin_turn();
    w.daemon.wait_active_turn(&sid, true, T);
    w.submit(0, "s21-queued", "queue", json!({})).expect("submit");
    w.wait_state("s21-queued", "waiting{explicitQueue}", T);
    let before = w.recorder.read_input_list(&w.daemon, &sid).expect("input/list");
    w.daemon.stop_management();
    std::fs::remove_file(w.daemon.home.ledger_file()).expect("remove the ledger file");
    w.recorder.mark("ledgerLost", json!({}));
    w.daemon.start();
    w.daemon.wait_connected(&sid, Duration::from_secs(60));
    assert!(w.wait(T, |t| t.entries.iter().any(|e| e.kind == "streamReset")), "a cutex/stream/reset-class notice");
    let after = w.recorder.read_input_list(&w.daemon, &sid).expect("input/list");
    assert_ne!(after["ledgerEpoch"], before["ledgerEpoch"]);
    assert!(after["records"].as_array().is_some_and(|r| r.iter().any(|r| r["inputId"] == "s21-queued" && r["state"] == "waiting")), "{after}");
    w.fake(0).complete("completed");
    w.wait_settled(&["s21-queued"], SETTLE);
    w.finish(STRICT);
}
both!(s21_stream_reset_ledger_lost, s21, green);

// S22 queue/list responses out of order, one admissionFresh:false: every
// response carries stateRevision/ledgerEpoch/inputs, stale admission is
// marked, an older read never wins.
fn s22(kind: Kind) {
    let w = World::start(&name("s22", kind), WorldSpec::new(&[kind]).script(0, auto(300)));
    let sid = w.sid(0).to_string();
    w.fake(0).begin_turn();
    w.daemon.wait_active_turn(&sid, true, T);
    for i in 0..4 {
        w.submit(0, &format!("s22-q{i}"), "queue", json!({})).expect("submit");
    }
    let responses = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..16 {
            scope.spawn(|| {
                let r = w.recorder.read_queue_list(&w.daemon, &sid);
                responses.lock().unwrap().push(r);
            });
        }
        scope.spawn(|| { w.submit(0, "s22-q4", "queue", json!({})).ok(); });
    });
    for response in responses.into_inner().unwrap() {
        let r = response.expect("queue/list");
        assert!(r["stateRevision"].is_u64() && r["ledgerEpoch"].is_string() && r["inputs"].is_array(), "{r}");
        if r["admissionFresh"] == false {
            assert_eq!(r["admission"]["stale"], true, "{r}");
        }
    }
    w.fake(0).complete("completed");
    w.wait_settled(&["s22-q0", "s22-q1", "s22-q2", "s22-q3", "s22-q4"], SETTLE);
    w.finish(STRICT);
}
both!(s22_queue_list_out_of_order, s22, green);

// S23 merged insertion: the merged ones superseded{mergedInsert,
// supersededBy}; the merged record follows S3/S4.
fn s23_run(kind: Kind, merge_one_revision: bool) {
    let w = World::start(&name("s23", kind), WorldSpec::new(&[kind]).script(0, auto(300)));
    let sid = w.sid(0).to_string();
    w.fake(0).begin_turn();
    w.daemon.wait_active_turn(&sid, true, T);
    let ids = ["s23-m0", "s23-m1", "s23-m2"];
    for id in ids {
        w.submit(0, id, "queue", json!({})).expect("submit");
    }
    let list = w.recorder.read_queue_list(&w.daemon, &sid).expect("queue/list");
    let items: Vec<Value> = ids.iter().map(|id| {
        let item = list["items"].as_array().into_iter().flatten()
            .find(|item| item["clientUserMessageId"] == *id)
            .unwrap_or_else(|| panic!("{id} not queued: {list}"));
        json!({"queueId": item["queueId"], "expectedRevision": item["revision"]})
    }).collect();
    let merged = w.daemon.cutex(&sid, "cutex/userInput/queue/merge", json!({"items": items}));
    w.recorder.mark("queueMerge", json!({"sid": sid, "result": merged}));
    let merged = merged.expect("queue/merge");
    assert_eq!(merged["item"]["clientUserMessageId"], "s23-m0", "{merged}");
    assert_eq!(merged["inputs"].as_array().map(Vec::len), Some(3), "{merged}");
    for id in &ids[1..] {
        w.wait_state(id, "superseded{mergedInsert}", T);
        let latest = w.trace().latest(id).expect("merged-away record");
        assert_eq!(latest["supersededBy"], "s23-m0", "{latest}");
    }
    // Director decision 4: the merge is ONE batch write (`record_batch`): one
    // transaction, one stateRevision for every record it changed, so clients
    // never see an intermediate (the trace checker accepts the shared revision
    // for distinct inputs only).
    let revisions: std::collections::BTreeSet<u64> = merged["inputs"].as_array().into_iter().flatten()
        .filter_map(|r| r["stateRevision"].as_u64()).collect();
    let superseded: std::collections::BTreeSet<u64> = ids[1..].iter()
        .filter_map(|id| w.trace().records("session", id).into_iter().find(|r| r["state"] == "superseded"))
        .filter_map(|r| r["stateRevision"].as_u64()).collect();
    if merge_one_revision {
        assert!(revisions.len() == 1 && superseded == revisions,
            "decision 4: one stateRevision for the merge; result.inputs {revisions:?}, superseded events {superseded:?}");
    }
    w.fake(0).complete("completed");
    w.wait_settled(&ids, SETTLE);
    let t = w.finish(STRICT);
    assert_eq!(t.latest("s23-m0").map(|r| r["state"].clone()), Some(json!("inContext")), "{:?}", t.path("s23-m0"));
}
fn s23(kind: Kind) {
    s23_run(kind, false)
}
both!(s23_merged_insertion, s23, green);
fn s23_one_revision(kind: Kind) {
    s23_run(kind, true)
}
both!(s23_merge_is_one_revision, s23_one_revision, green);

// S24 cross-host tell / wake between two private daemons (receiver side only,
// decision 5).
// The sender (a second private daemon, a Codex agent) uses `agent_tell
// {to: "<agent>@<host>"}` (`/peer/v1/tells`); the receiver's records are
// `kind = peerMessage` with `remote`: a note follows S11 (pendingNotes →
// the next turn), a wake follows S10 (nextUp → timeline). Only the
// receiver's trace is the fixture.
fn s24(kind: Kind) {
    let mut receiver_spec = WorldSpec::new(&[kind]).bus().persistent(0).script(0, auto(300));
    receiver_spec.options.peers = true;
    let b = World::start(&name("s24", kind), receiver_spec);
    let mut sender_spec = WorldSpec::new(&[Kind::Codex]).bus().persistent(0).script(0, auto(300));
    sender_spec.options.peers = true;
    let a = World::start(&name("s24-sender", kind), sender_spec);
    let pair = daemon::pair_peers(&a.daemon, &b.daemon, b.sid(0));
    b.recorder.mark("peerPair", json!({"pair": format!("{pair:?}")}));
    let to = format!("{}@{}", b.sid(0), pair.receiver_label);
    let tell = |text: &str, wake: bool| {
        let result = a.mcp(0, "agent_tell", json!({"to": to, "text": text, "wake": wake}));
        b.recorder.mark("peerTell", json!({"text": text, "wake": wake, "result": result.as_ref().map_err(|e| e.clone())}));
        let value = result.unwrap_or_else(|e| panic!("agent_tell {to}: {e}"));
        assert_eq!(value["ok"], true, "agent_tell {to}: {value}");
        assert_eq!(value["deliveryKind"], if wake { "wake" } else { "note" }, "{value}");
    };
    // A note while the receiver is busy: a passive note (S11).
    b.fake(0).begin_turn();
    b.daemon.wait_active_turn(b.sid(0), true, T);
    tell("s24-note", false);
    let note = wait_kind(&b, 0, "peerMessage", 1, T).remove(0);
    b.wait_state(&note, "waiting{passive}", T);
    b.fake(0).complete("completed");
    b.daemon.wait_active_turn(b.sid(0), false, T);
    b.submit(0, "s24-owner", "auto", json!({})).expect("submit");
    b.wait_settled(&[&note, "s24-owner"], SETTLE);
    b.daemon.wait_active_turn(b.sid(0), false, T);
    // A wake while it is idle (S10).
    tell("s24-wake", true);
    let wake = wait_kind(&b, 0, "peerMessage", 2, T).remove(1);
    b.wait_settled(&[&wake], SETTLE);
    let t = b.finish(STRICT);
    for id in [&note, &wake] {
        let latest = t.latest(id).unwrap();
        assert_eq!(latest["kind"], "peerMessage", "{latest}");
        assert_eq!(latest["sender"]["kind"], "peer", "{latest}");
        assert_eq!(latest["remote"]["peerId"], pair.sender_on_receiver.as_str(), "{latest}");
        assert_eq!(latest["remote"]["senderId"], a.sid(0), "{latest}");
        assert!(latest["remote"]["host"].is_string(), "{latest}");
    }
    assert_surfaces(&t, &note, &["pendingNotes", "timeline"]);
    assert_path_any(&t, &note, &[
        &["waiting{passive}", "accepted", "inContext"],
        &["waiting{passive}", "inContext"],
        &["waiting{passive}", "sending", "accepted", "inContext"],
    ]);
    assert_path(&t, &wake, &["waiting", "sending", "accepted", "inContext"]);
    assert_surfaces(&t, &wake, &["nextUp", "timeline"]);
}
both!(s24_cross_host_tell_and_wake, s24, green);

// S26 (r274 interrupt-send-contract.md) owner submit during a turn → the steer
// is still unread → cutex/input/interruptSend: the runtime sees ONE
// turn/interrupt of that turn and the SAME clientUserMessageId as the next
// turn's first input, exactly once (never a text copy). A stale CAS first:
// revision_conflict, no side effect.
fn s26(kind: Kind) {
    let mut script = auto(300);
    script.pending_steer = true;
    let w = World::start(&name("s26", kind), WorldSpec::new(&[kind]).script(0, script));
    let sid = w.sid(0).to_string();
    let turn = w.fake(0).begin_turn();
    w.daemon.wait_active_turn(&sid, true, T);
    w.submit(0, "s26-m1", "auto", json!({})).expect("submit");
    w.fake(0).wait_call("turn/steer", 1, T);
    // The session stream's record (the host stream's copy carries no actions).
    let ready = w.wait(T, |t| t.records("session", "s26-m1").last().is_some_and(|r| r["state"] == "accepted"
        && r["actions"].as_array().is_some_and(|a| a.iter().any(|a| a == "interruptSend"))));
    assert!(ready, "s26-m1 never accepted with interruptSend: {:?}", w.trace().path("s26-m1"));
    assert_eq!(w.fake(0).pending_steers().len(), 1, "the steer is not read yet");
    let list = w.recorder.read_input_list(&w.daemon, &sid).expect("input/list");
    let record = list["records"].as_array().unwrap().iter().find(|r| r["inputId"] == "s26-m1").unwrap().clone();
    assert_eq!(list["delivery"]["turn"]["turnId"], turn, "{list}");
    let generation = list["delivery"]["runtimeGeneration"].clone();
    let rev = record["rev"].as_u64().unwrap();
    let params = |rev: u64| json!({"inputId": "s26-m1", "expectedRev": rev, "expectedTurnId": turn,
        "expectedRuntimeGeneration": generation});

    let stale = w.daemon.cutex(&sid, "cutex/input/interruptSend", params(rev - 1)).unwrap_err();
    w.recorder.mark("interruptSendStale", stale.clone());
    assert_eq!(stale["status"], 409, "{stale}");
    assert_eq!(stale["body"]["error"]["code"], "revision_conflict", "{stale}");
    assert_eq!(stale["body"]["error"]["details"]["currentRev"], rev, "{stale}");
    assert_eq!(stale["body"]["error"]["details"]["currentTurnId"], turn, "{stale}");
    assert!(w.fake(0).calls_of("turn/interrupt").is_empty(), "a conflict has no side effect");

    let result = w.daemon.cutex(&sid, "cutex/input/interruptSend", params(rev)).expect("interruptSend");
    w.recorder.mark("interruptSend", result.clone());
    assert_eq!(result["outcome"], "interrupted", "{result}");
    assert_eq!(result["interruptedTurnId"], turn, "{result}");
    assert_eq!(result["record"]["inputId"], "s26-m1", "{result}");
    w.wait_settled(&["s26-m1"], SETTLE);
    w.daemon.wait_active_turn(&sid, false, T);
    std::thread::sleep(Duration::from_millis(500));

    let fake = w.fake(0);
    let interrupts = fake.calls_of("turn/interrupt");
    assert_eq!(interrupts.len(), 1, "one interrupt");
    if let Some(expected) = interrupts[0].params["turnId"].as_str() {
        assert_eq!(expected, turn);
    }
    let starts: Vec<_> = fake.calls_of("turn/start").into_iter()
        .filter(|c| c.params["clientUserMessageId"] == "s26-m1" || first_text(&c.params).contains("s26-m1")).collect();
    assert_eq!(starts.len(), 1, "exactly one turn/start carries s26-m1: {:?}", starts.iter().map(|c| &c.params).collect::<Vec<_>>());
    assert_eq!(starts[0].params["clientUserMessageId"], "s26-m1", "the same id, not a text copy");
    assert_eq!(fake.calls_of("turn/steer").len(), 1);
    assert!(fake.pending_steers().is_empty());
    // In the runtime: the interrupted turn never read it, the next turn's
    // first input is it, nothing else carries it.
    let turns = fake.turns();
    let carrying: Vec<_> = turns.iter().filter(|t| t["items"].as_array().unwrap().iter()
        .any(|i| i["type"] == "userMessage" && i["clientId"] == "s26-m1")).collect();
    assert_eq!(carrying.len(), 1, "{turns:?}");
    let next = carrying[0];
    assert_ne!(next["id"], turn);
    assert_eq!(turns.iter().find(|t| t["id"] == turn).unwrap()["status"], "interrupted");
    assert_eq!(next["items"][0]["clientId"], "s26-m1", "first input of the next turn");

    let t = w.finish(STRICT);
    assert_path_any_collapsing_stages(&t, "s26-m1", &[&["received", "sending", "accepted", "sending{interruptSend}", "inContext(turnStartInput)"]]);
    assert_surfaces(&t, "s26-m1", &["nextUp", "timeline"]);
    let sending = t.records("session", "s26-m1").into_iter().find(|r| r["reason"] == "interruptSend").unwrap();
    assert_eq!(sending["details"]["interruptedTurnId"], turn, "{sending}");
    let done = t.latest("s26-m1").unwrap();
    assert_eq!(done["turnId"], next["id"], "{done}");
    // The interrupt's first transition and its delivery: one revision.
    let delivery = t.entries.iter().find(|e| e.source == "session" && e.kind == "deliveryChanged"
        && e.payload["stateRevision"] == sending["stateRevision"]);
    assert!(delivery.is_some_and(|d| d.payload["delivery"]["state"] == "finishing"),
        "no finishing deliveryChanged at the revision of sending{{interruptSend}} ({})", sending["stateRevision"]);
}
both!(s26_interrupt_send, s26, green);

/// The first version of `id` on the timeline (session stream).
fn first_timeline(t: &Trace, id: &str) -> Value {
    t.records("session", id).into_iter().find(|r| r["surface"] == "timeline")
        .unwrap_or_else(|| panic!("{id} never on the timeline: {:?}", t.path(id)))
}

// S32 ★ (r274.2 §9.4, owner decision 2026-11-03: no auto-redelivery) the
// runtime goes away mid-turn while an owner steer is unread: the record
// stays `accepted` on nextUp, `stalled{runtimeDisconnected}`, with actions
// `retry` + `dismiss`, and cutex never sends it again by itself (the runtime
// sees one `turn/steer` of it until the owner acts).
// a (`dismiss`): `uncertain{runtimeLost}` → `discarded{ownerDiscarded}`.
// b (`retry`): while the runtime is away the resend is refused before any
// native IO → `failed{session_offline}` with retry + dismiss; once it is
// back, the owner's second retry settles it (the runtime had it: history).
fn s32_run(kind: Kind, retry: bool) {
    let mut busy = Script::default();
    busy.echo_steer_item = false;
    busy.claude_stages = false;
    let id = if retry { "s32b-m1" } else { "s32-m1" };
    let w = World::start(&name(if retry { "s32b" } else { "s32" }, kind), WorldSpec::new(&[kind]).script(0, busy));
    let sid = w.sid(0).to_string();
    w.fake(0).begin_turn();
    w.daemon.wait_active_turn(&sid, true, T);
    w.submit(0, id, "auto", json!({})).expect("submit");
    w.wait_state(id, "accepted", T);
    w.fake(0).script(|s| s.refuse_connections = true);
    w.fake(0).disconnect_all();
    let stuck = |t: &Trace| t.records("session", id).last().is_some_and(|r| r["state"] == "accepted"
        && r["stalled"]["reason"] == "runtimeDisconnected" && r["surface"] == "nextUp" && r["actions"] == json!(["retry", "dismiss"]));
    assert!(w.wait(T, stuck), "{id} never stuck with retry + dismiss: {:?}", w.trace().records("session", id).last());
    // Nothing resends it while the owner has not acted.
    std::thread::sleep(Duration::from_millis(1500));
    let sent = |w: &World| ["turn/steer", "turn/start"].iter()
        .map(|method| w.fake(0).calls_of(method).into_iter().filter(|c| c.params["clientUserMessageId"] == id).count())
        .sum::<usize>();
    assert_eq!(sent(&w), 1, "cutex sent {id} again by itself");
    let rev = |w: &World| {
        let list = w.recorder.read_input_list(&w.daemon, &sid).expect("input/list");
        list["records"].as_array().unwrap().iter().find(|r| r["inputId"] == id).expect("listed")["rev"].as_u64().unwrap()
    };
    if retry {
        let away = w.daemon.cutex(&sid, "cutex/input/retry", json!({"inputId": id, "expectedRev": rev(&w)})).expect("retry while away");
        w.recorder.mark("retryAway", away.clone());
        assert_eq!(away["redelivery"]["ok"], false, "{away}");
        assert!(w.wait(T, |t| t.records("session", id).last().is_some_and(|r| r["state"] == "failed" && r["surface"] == "nextUp"
            && r["actions"] == json!(["retry", "dismiss"]))), "{:?}", w.trace().path(id));
        assert_eq!(sent(&w), 1, "refused before native IO");
    } else {
        let dismissed = w.daemon.cutex(&sid, "cutex/input/dismiss", json!({"inputId": id, "expectedRev": rev(&w)})).expect("dismiss");
        w.recorder.mark("dismiss", dismissed.clone());
    }
    w.fake(0).script(|s| s.refuse_connections = false);
    w.daemon.wait_connected(&sid, Duration::from_secs(60));
    if retry {
        let again = w.daemon.cutex(&sid, "cutex/input/retry", json!({"inputId": id, "expectedRev": rev(&w)})).expect("retry");
        w.recorder.mark("retry", again.clone());
        assert_eq!(again["redelivery"]["ok"], true, "{again}");
    }
    w.fake(0).complete("completed");
    w.wait_settled(&[id], SETTLE);
    std::thread::sleep(Duration::from_millis(500));
    // The fake runtime kept the first steer in the turn it was steered into,
    // so the owner's resend finds it in native history: proven in context,
    // not sent a second time (a send again never duplicates what the
    // runtime already has).
    assert_eq!(sent(&w), 1, "nothing goes out twice");
    let t = w.finish(STRICT);
    if retry {
        assert_path(&t, id, &["received", "sending", "accepted", "uncertain{runtimeLost}", "waiting{ownerRetry}", "failed{session_offline}",
            "waiting{ownerRetry}", "sending", "inContext(historyRead)"]);
        assert_eq!(t.surfaces(id), ["nextUp", "timeline"]);
    } else {
        assert_path(&t, id, &["received", "sending", "accepted", "uncertain{runtimeLost}", "discarded{ownerDiscarded}"]);
    }
    // Never on the timeline before it was read.
    let unread_timeline = t.records("session", id).into_iter()
        .any(|r| r["surface"] == "timeline" && r["state"] != "inContext");
    assert!(!unread_timeline, "{id}: on the timeline before it was read: {:?}", t.path(id));
}
fn s32(kind: Kind) {
    s32_run(kind, false)
}
both!(s32_stuck_owner_dismiss, s32, green);
fn s32b(kind: Kind) {
    s32_run(kind, true)
}
both!(s32b_stuck_owner_retry, s32b, green);

// S30 ★ (r274.2 §9.4) a session held after the owner's stop (`stopHold`,
// Esc) with a queued item: the owner's `auto` on the idle agent starts the
// turn at once and is on the timeline only from that start; the held queue
// item (no turn started for it) stays in nextUp.
fn s30(kind: Kind) {
    let w = World::start(&name("s30", kind), WorldSpec::new(&[kind]).script(0, auto(200)));
    let sid = w.sid(0).to_string();
    w.fake(0).begin_turn();
    w.daemon.wait_active_turn(&sid, true, T);
    w.submit(0, "s30-q", "queue", json!({})).expect("queue");
    w.wait_state("s30-q", "waiting{explicitQueue}", T);
    let stop = w.daemon.cutex(&sid, "cutex/turn/stop", json!({})).expect("stop");
    w.recorder.mark("stop", stop.clone());
    w.daemon.wait_active_turn(&sid, false, T);
    // The queue is paused (`stopHold` on the session's delivery); the item
    // keeps its own waiting reason.
    assert_eq!(stop["paused"], true, "{stop}");
    w.submit(0, "s30-m", "auto", json!({})).expect("submit");
    w.wait_settled(&["s30-m"], SETTLE);
    w.daemon.wait_active_turn(&sid, false, T);
    std::thread::sleep(Duration::from_millis(300));
    let t = w.finish(LOOSE);
    let path = t.path("s30-m");
    assert!(path.last().is_some_and(|l| l == "inContext(turnStartInput)"), "{path:?}");
    let moved = first_timeline(&t, "s30-m");
    // Created on nextUp (held session) and moved by the start, or created on
    // the timeline by the direct start itself: never on the timeline unread.
    assert!(matches!(moved["state"].as_str(), Some("inContext" | "sending")), "{moved}");
    if moved["state"] == "sending" {
        assert_surfaces(&t, "s30-m", &["timeline"]);
    } else {
        assert_surfaces(&t, "s30-m", &["nextUp", "timeline"]);
    }
    let q = t.latest("s30-q").unwrap();
    assert_eq!((q["state"].as_str(), q["surface"].as_str()), (Some("waiting"), Some("nextUp")), "{q}");
}
both!(s30_stop_held_owner_send_starts_at_once, s30, green);

// S27 ★ (r274.2, owner decision 2026-11-03) busy owner `auto` insert while
// the agent runs tools: the steer stays in nextUp through the three tool
// calls of the model step it arrived in, then moves to the timeline ONCE, at
// the next step boundary where the agent reads it (Codex: the drained
// steer's echo; Claude: `accepted[offered]`). In native history the user
// item comes right after the tool it was read after, and no later tool
// comes before it.
fn s27(kind: Kind) {
    let script = Script { pending_steer: true, ..Script::default() };
    let w = World::start(&name("s27", kind), WorldSpec::new(&[kind]).script(0, script));
    assert!(has_feature(&w, "owner-input-nextup-v1"), "r274.2 advertises the nextUp-until-read rule");
    let sid = w.sid(0).to_string();
    let fake = w.fake(0);
    let turn = fake.begin_turn();
    w.daemon.wait_active_turn(&sid, true, T);
    let tool1 = fake.step().expect("tool1");
    let result = w.submit(0, "s27-m1", "auto", json!({}));
    fake.wait_call("turn/steer", 1, T);
    let accepted = if kind == Kind::Claude { "accepted[registered]" } else { "accepted" };
    w.wait_state("s27-m1", accepted, T);
    let unread = |t: &Trace| {
        let latest = t.latest("s27-m1").expect("s27-m1 record");
        (latest["state"].clone(), latest["surface"].clone())
    };
    assert_eq!(unread(&w.trace()), (json!("accepted"), json!("nextUp")), "{:?}", w.trace().path("s27-m1"));
    let queue = w.recorder.read_queue_list(&w.daemon, &sid).expect("queue/list");
    assert!(queue["inputs"].as_array().is_some_and(|a| a.iter().any(|r| r["inputId"] == "s27-m1" && r["surface"] == "nextUp")),
        "queue/list inputs hold the unread insert: {queue}");
    // Three tool calls of the same model step: no step boundary, unread.
    let tool2 = fake.tool().expect("tool2");
    let tool3 = fake.tool().expect("tool3");
    let tool4 = fake.tool().expect("tool4");
    let seen = |t: &Trace, item: &str| t.entries.iter().any(|e| e.source == "session" && e.kind == "native"
        && e.payload["itemId"] == item && e.payload["method"] == "item/completed");
    assert!(w.wait(T, |t| seen(t, &tool4)), "tool4 never reached the session stream");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(unread(&w.trace()), (json!("accepted"), json!("nextUp")), "{:?}", w.trace().path("s27-m1"));
    assert_eq!(fake.pending_steers().len(), 1, "the steer is not read yet");
    // The next step reads it at its tool boundary.
    let tool5 = fake.step().expect("tool5");
    assert!(w.wait(T, |t| t.latest("s27-m1").is_some_and(|r| r["surface"] == "timeline") && seen(t, &tool5)),
        "s27-m1 never moved: {:?}", w.trace().path("s27-m1"));
    fake.complete("completed");
    w.wait_settled(&["s27-m1"], SETTLE);

    // Native history: [tool1..tool4, the steer, tool5] (and the final message).
    let turns = fake.turns();
    let items: Vec<String> = turns.iter().find(|t| t["id"] == turn.as_str()).expect("the busy turn")["items"]
        .as_array().unwrap().iter()
        .filter(|i| i["type"] != "agentMessage")
        .map(|i| if i["type"] == "userMessage" { format!("user:{}", i["clientId"].as_str().unwrap_or("")) } else { i["id"].as_str().unwrap_or("").to_string() })
        .collect();
    assert_eq!(items, [tool1.clone(), tool2.clone(), tool3.clone(), tool4.clone(), "user:s27-m1".to_string(), tool5.clone()], "{turns:?}");

    let t = w.finish(STRICT);
    assert_result_input(&result, "s27-m1");
    let expected: &[&str] = match kind {
        Kind::Codex => &["received", "sending", "accepted", "inContext(clientIdEcho)"],
        _ => &["received", "sending", "accepted[registered]", "accepted[offered]", "inContext(contextPersisted)"],
    };
    assert_path(&t, "s27-m1", expected);
    assert_surfaces(&t, "s27-m1", &["nextUp", "timeline"]);
    // Session stream order: every event of tool1-4 before the move, every
    // event of tool5 after it.
    let index = |what: &dyn Fn(&trace::TraceEntry) -> bool| -> Vec<usize> {
        t.entries.iter().enumerate().filter(|(_, e)| e.source == "session" && what(e)).map(|(i, _)| i).collect()
    };
    let moved = index(&|e| e.kind == "stateChanged" && e.payload["record"]["inputId"] == "s27-m1"
        && e.payload["record"]["surface"] == "timeline");
    let moved = moved.first().copied().expect("the move event");
    for (tool, before) in [(&tool1, true), (&tool2, true), (&tool3, true), (&tool4, true), (&tool5, false)] {
        let events = index(&|e| e.kind == "native" && e.payload["itemId"] == tool.as_str());
        assert!(!events.is_empty(), "{tool}: no native events on the session stream");
        assert!(events.iter().all(|i| (*i < moved) == before),
            "{tool}: events {events:?} vs the move at {moved} (expected before: {before})");
    }
    let move_record = &t.entries[moved].payload["record"];
    assert_eq!(move_record["turnId"], turn, "{move_record}");
    if kind == Kind::Codex {
        assert!(move_record["timelineItemId"].is_string(), "Codex binds the echoed item: {move_record}");
        assert_eq!(move_record["state"], "inContext", "{move_record}");
    } else {
        // r274.2 §9.1: Claude moves at the steer echo or `offered`, still
        // `accepted`, bound to the steer item; `inContext` only at the receipt.
        assert_eq!(move_record["state"], "accepted", "{move_record}");
        assert_eq!(move_record["timelineItemId"], "steer-s27-m1", "{move_record}");
        assert!(matches!(move_record["details"]["readSignal"].as_str(), Some("steerEcho" | "offered")), "{move_record}");
    }
}
both!(s27_insert_stays_next_up_until_read, s27, green);

// ---------------------------------------------------------------------------
// S28 ★ (r274.2 §9.1) Claude records leave pendingNotes / nextUp with their
// native row: the record moves to the timeline as `accepted` (turn, item,
// `details.readSignal`) in the batch of that row, before the fake's next
// event, within 500 ms; `inContext` only at the receipt (turn end on r34),
// with no surface change.

/// Session-stream index of the first entry `what` matches.
fn first_at(t: &Trace, what: impl Fn(&trace::TraceEntry) -> bool) -> Option<usize> {
    t.entries.iter().position(|e| e.source == "session" && what(e))
}

fn native_item_entry(e: &trace::TraceEntry, item: &str) -> bool {
    e.kind == "native" && e.payload["itemId"] == item
}

fn change_of(e: &trace::TraceEntry, id: &str, field: &str, value: &str) -> bool {
    e.kind == "stateChanged" && e.payload["record"]["inputId"] == id && e.payload["record"][field] == value
}

fn turn_completed_entry(e: &trace::TraceEntry, turn: &str) -> bool {
    e.kind == "native" && e.payload["method"] == "turn/completed" && e.payload["turnId"] == turn
}

fn seen_native(t: &Trace, item: &str) -> bool {
    first_at(t, |e| native_item_entry(e, item) && e.payload["method"] == "item/completed").is_some()
}

fn wait_active(fake: &FakeRuntime) -> String {
    let until = Instant::now() + T;
    loop {
        if let Some(turn) = fake.active_turn() {
            return turn["id"].as_str().unwrap().to_string();
        }
        assert!(Instant::now() < until, "the fake never started a turn");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The move of note `id` against its native row: after the row's first
/// event, before `sentinel` (the fake's next event, sent after a gap),
/// within 500 ms; `accepted` in `turn`, bound to the message id; then
/// `inContext` only after the turn completed, no further surface.
fn assert_note_moved_at_its_row(t: &Trace, id: &str, turn: &str, sentinel: &str) {
    let row = first_at(t, |e| native_item_entry(e, id)).unwrap_or_else(|| panic!("{id}: no native row"));
    let moved = first_at(t, |e| change_of(e, id, "surface", "timeline")).unwrap_or_else(|| panic!("{id}: never moved"));
    let next = first_at(t, |e| native_item_entry(e, sentinel)).expect("the sentinel event");
    assert!(row < moved && moved < next, "{id}: row {row}, move {moved}, next event {next}");
    let lag = t.entries[moved].at_ms - t.entries[row].at_ms;
    assert!(lag <= 500, "{id}: moved {lag} ms after its native row");
    let record = &t.entries[moved].payload["record"];
    assert_eq!((record["state"].as_str(), record["turnId"].as_str(), record["timelineItemId"].as_str()),
        (Some("accepted"), Some(turn), Some(id)), "{record}");
    assert_eq!(record["details"]["readSignal"], "nativeItem", "{record}");
    let in_context = first_at(t, |e| change_of(e, id, "state", "inContext")).unwrap_or_else(|| panic!("{id}: never inContext"));
    let completed = first_at(t, |e| turn_completed_entry(e, turn)).expect("turn/completed");
    assert!(in_context > completed, "{id}: inContext at {in_context} before the receipt (turn/completed {completed})");
    assert_eq!(t.entries[in_context].payload["record"]["surface"], "timeline");
    assert_surfaces(t, id, &["pendingNotes", "timeline"]);
}

// S28: two notes queued for an idle Claude agent ride along the owner's turn.
fn s28(kind: Kind) {
    let w = World::start(&name("s28", kind), WorldSpec::new(&[kind, Kind::Codex]).bus());
    let sid = w.sid(0).to_string();
    let fake = w.fake(0);
    for text in ["s28-note-a", "s28-note-b"] {
        assert!(w.bus_send(1, 0, text, &["--queue-only"]).status.success());
    }
    let notes = wait_kind(&w, 0, "agentMessage", 2, T);
    fake.wait_inbox(2, T);
    for note in &notes {
        assert!(w.wait(T, |t| t.latest(note).is_some_and(|r| r["surface"] == "pendingNotes" && r["turnId"].is_null()
            && matches!(r["state"].as_str(), Some("sending" | "accepted")))), "{note}: {:?}", w.trace().path(note));
    }
    w.submit(0, "s28-owner", "auto", json!({})).expect("submit");
    let turn = wait_active(fake);
    assert!(w.wait(T, |t| notes.iter().all(|n| t.latest(n).is_some_and(|r| r["surface"] == "timeline"))),
        "notes never moved: {:?}", notes.iter().map(|n| w.trace().path(n)).collect::<Vec<_>>());
    let queue = w.recorder.read_queue_list(&w.daemon, &sid).expect("queue/list");
    assert!(queue["inputs"].as_array().is_some_and(|a| a.iter().all(|r| !notes.iter().any(|n| r["inputId"] == n.as_str()))),
        "queue/list no longer lists moved notes: {queue}");
    std::thread::sleep(Duration::from_secs(1));
    let sentinel = fake.tool().expect("sentinel");
    assert!(w.wait(T, |t| seen_native(t, &sentinel)));
    fake.complete("completed");
    let mut ids: Vec<&str> = notes.iter().map(String::as_str).collect();
    ids.push("s28-owner");
    w.wait_settled(&ids, SETTLE);
    let t = w.finish(STRICT);
    for note in &notes {
        assert_note_moved_at_its_row(&t, note, &turn, &sentinel);
    }
    assert_eq!(ids_of_kind(&t, &sid, "agentMessage").len(), 2, "each note appears exactly once");
}
one!(s28_claude_notes_move_with_their_native_row, s28, Kind::Claude, green);

// S28b: a note queued while the turn runs is read at the next tool boundary.
fn s28b(kind: Kind) {
    let w = World::start(&name("s28b", kind), WorldSpec::new(&[kind, Kind::Codex]).bus());
    let sid = w.sid(0).to_string();
    let fake = w.fake(0);
    let turn = fake.begin_turn();
    w.daemon.wait_active_turn(&sid, true, T);
    let tool1 = fake.step().expect("tool1");
    assert!(w.bus_send(1, 0, "s28b-note", &["--queue-only"]).status.success());
    let note = wait_kind(&w, 0, "agentMessage", 1, T).remove(0);
    fake.wait_inbox(1, T);
    assert!(w.wait(T, |t| t.latest(&note).is_some_and(|r| r["surface"] == "pendingNotes")), "{:?}", w.trace().path(&note));
    std::thread::sleep(Duration::from_millis(300));
    let tool2 = fake.step().expect("tool2: reads the note first");
    assert!(w.wait(T, |t| t.latest(&note).is_some_and(|r| r["surface"] == "timeline")), "{:?}", w.trace().path(&note));
    std::thread::sleep(Duration::from_secs(1));
    let tool3 = fake.tool().expect("tool3");
    assert!(w.wait(T, |t| seen_native(t, &tool3)));
    fake.complete("completed");
    w.wait_settled(&[&note], SETTLE);
    let items: Vec<String> = fake.turns().iter().find(|t| t["id"] == turn.as_str()).expect("the busy turn")["items"]
        .as_array().unwrap().iter().filter(|i| i["type"] != "agentMessage")
        .map(|i| i["id"].as_str().unwrap_or("").to_string()).collect();
    assert_eq!(items, [tool1.clone(), note.clone(), tool2, tool3.clone()]);
    let t = w.finish(STRICT);
    assert!(first_at(&t, |e| native_item_entry(e, &tool1)) < first_at(&t, |e| native_item_entry(e, &note)));
    assert_note_moved_at_its_row(&t, &note, &turn, &tool3);
}
one!(s28b_claude_mid_turn_note_moves_with_its_row, s28b, Kind::Claude, green);

// S28c: a Claude steer moves at its echo / `offered` (still `accepted`),
// not at `contextPersisted`.
fn s28c(kind: Kind) {
    let script = Script { pending_steer: true, ..Script::default() };
    let w = World::start(&name("s28c", kind), WorldSpec::new(&[kind]).script(0, script));
    let sid = w.sid(0).to_string();
    let fake = w.fake(0);
    let turn = fake.begin_turn();
    w.daemon.wait_active_turn(&sid, true, T);
    let tool1 = fake.step().expect("tool1");
    w.submit(0, "s28c-m1", "auto", json!({})).expect("submit");
    w.wait_state("s28c-m1", "accepted[registered]", T);
    assert_eq!(w.trace().latest("s28c-m1").unwrap()["surface"], "nextUp");
    fake.step().expect("tool2: reads the steer first");
    assert!(w.wait(T, |t| t.latest("s28c-m1").is_some_and(|r| r["surface"] == "timeline")), "{:?}", w.trace().path("s28c-m1"));
    std::thread::sleep(Duration::from_secs(1));
    let tool3 = fake.tool().expect("tool3");
    assert!(w.wait(T, |t| seen_native(t, &tool3)));
    fake.complete("completed");
    w.wait_settled(&["s28c-m1"], SETTLE);
    let t = w.finish(STRICT);
    assert_path(&t, "s28c-m1", &["received", "sending", "accepted[registered]", "accepted[offered]", "inContext(contextPersisted)"]);
    assert_surfaces(&t, "s28c-m1", &["nextUp", "timeline"]);
    let echo = first_at(&t, |e| native_item_entry(e, "steer-s28c-m1")).expect("the steer echo on the stream");
    let moved = first_at(&t, |e| change_of(e, "s28c-m1", "surface", "timeline")).unwrap();
    let next = first_at(&t, |e| native_item_entry(e, &tool3)).unwrap();
    assert!(first_at(&t, |e| native_item_entry(e, &tool1)).unwrap() < moved && moved < next, "move {moved}, next {next}");
    assert!((t.entries[moved].at_ms - t.entries[echo].at_ms).abs() <= 500, "moved far from its echo");
    let record = &t.entries[moved].payload["record"];
    assert_eq!((record["state"].as_str(), record["turnId"].as_str(), record["timelineItemId"].as_str()),
        (Some("accepted"), Some(turn.as_str()), Some("steer-s28c-m1")), "{record}");
    assert!(matches!(record["details"]["readSignal"].as_str(), Some("steerEcho" | "offered")), "{record}");
    assert!(!record["actions"].as_array().is_some_and(|a| a.iter().any(|x| x == "interruptSend")), "{record}");
    let in_context = first_at(&t, |e| change_of(e, "s28c-m1", "state", "inContext")).unwrap();
    assert!(in_context > next, "inContext only at contextPersisted (turn end)");
}
one!(s28c_claude_steer_moves_at_its_echo, s28c, Kind::Claude, green);

// S28d: a Claude steer no tool boundary read slips to the next turn: it moves
// at that turn's first user item (its clientId echo), not at its receipt, and
// settles `inContext{turnStartInput}` there (r275: that turn started with it).
fn s28d(kind: Kind) {
    let script = Script { pending_steer: true, claude_slip_steers: true, ..Script::default() };
    let w = World::start(&name("s28d", kind), WorldSpec::new(&[kind]).script(0, script));
    let sid = w.sid(0).to_string();
    let fake = w.fake(0);
    let first = fake.begin_turn();
    w.daemon.wait_active_turn(&sid, true, T);
    fake.step().expect("tool1");
    w.submit(0, "s28d-m1", "auto", json!({})).expect("submit");
    w.wait_state("s28d-m1", "accepted[registered]", T);
    fake.complete("completed");
    let next = wait_active(fake);
    assert_ne!(next, first);
    assert!(w.wait(T, |t| t.latest("s28d-m1").is_some_and(|r| r["surface"] == "timeline")), "{:?}", w.trace().path("s28d-m1"));
    std::thread::sleep(Duration::from_secs(1));
    let sentinel = fake.tool().expect("sentinel");
    assert!(w.wait(T, |t| seen_native(t, &sentinel)));
    fake.complete("completed");
    w.wait_settled(&["s28d-m1"], SETTLE);
    let t = w.finish(STRICT);
    assert_path(&t, "s28d-m1", &["received", "sending", "accepted[registered]", "inContext(turnStartInput)"]);
    assert_surfaces(&t, "s28d-m1", &["nextUp", "timeline"]);
    let first_done = first_at(&t, |e| turn_completed_entry(e, &first)).expect("the first turn ends");
    let echo = first_at(&t, |e| e.kind == "native" && e.payload["clientId"] == "s28d-m1" && e.payload["turnId"] == next.as_str())
        .expect("the next turn's user item");
    let moved = first_at(&t, |e| change_of(e, "s28d-m1", "surface", "timeline")).unwrap();
    let after = first_at(&t, |e| native_item_entry(e, &sentinel)).unwrap();
    assert!(first_done < moved && moved < after, "first turn end {first_done}, move {moved}, next event {after}");
    assert!((t.entries[moved].at_ms - t.entries[echo].at_ms).abs() <= 500, "moved far from its echo");
    let record = &t.entries[moved].payload["record"];
    assert_eq!((record["state"].as_str(), record["turnId"].as_str()), (Some("accepted"), Some(next.as_str())), "{record}");
    assert_eq!(record["timelineItemId"], t.entries[echo].payload["itemId"], "{record}");
    assert_eq!(record["details"]["readSignal"], "steerEcho", "{record}");
    let in_context = first_at(&t, |e| change_of(e, "s28d-m1", "state", "inContext")).unwrap();
    assert!(in_context >= moved && in_context < after, "inContext at the next turn's item, not at its receipt: move {moved}, inContext {in_context}, next event {after}");
}
one!(s28d_claude_slipped_steer_moves_at_the_next_turn_item, s28d, Kind::Claude, green);

// S28e: the turn that showed a note is interrupted before the receipt: the
// note stays on the timeline and becomes uncertain{turnInterrupted}.
fn s28e(kind: Kind) {
    let w = World::start(&name("s28e", kind), WorldSpec::new(&[kind, Kind::Codex]).bus());
    let fake = w.fake(0);
    assert!(w.bus_send(1, 0, "s28e-note", &["--queue-only"]).status.success());
    let note = wait_kind(&w, 0, "agentMessage", 1, T).remove(0);
    fake.wait_inbox(1, T);
    w.submit(0, "s28e-owner", "auto", json!({})).expect("submit");
    wait_active(fake);
    assert!(w.wait(T, |t| t.latest(&note).is_some_and(|r| r["surface"] == "timeline" && r["state"] == "accepted")),
        "{:?}", w.trace().path(&note));
    fake.complete("interrupted");
    // turn_ended keeps an interrupt-held record one INTERRUPTED_RECHECK (10 s).
    w.wait_state(&note, "uncertain{turnInterrupted}", SETTLE);
    w.wait_settled(&["s28e-owner"], SETTLE);
    let t = w.finish(STRICT);
    assert_never(&t, &note, "inContext");
    assert_eq!(t.path(&note).last().map(String::as_str), Some("uncertain{turnInterrupted}"), "{:?}", t.path(&note));
    assert_surfaces(&t, &note, &["pendingNotes", "timeline"]);
}
one!(s28e_claude_note_turn_interrupted_before_receipt, s28e, Kind::Claude, green);

// S31 (r274.2, owner 2026-11-03: only an input certain to reach the agent
// enters nextUp / pendingNotes) a delegation to a paused target: its passive
// status item (`paused` / `held`, read only by delegation_wait) creates no
// ledger record on the requester side, before or after the owner resumes the
// target; the result still arrives and is delivered.
fn s31(kind: Kind) {
    let w = World::start(&name("s31", kind), WorldSpec::new(&[kind, Kind::Claude]).bus().persistent(0).persistent(1).script(0, auto(300)).script(1, auto(300)));
    w.wait_asks_presence(&[0, 1]);
    let paused = w.daemon.cutex(w.sid(1), "cutex/asks/pause", json!({}));
    w.recorder.mark("asksPause", json!({"result": paused.as_ref().map_err(|e| e.clone())}));
    paused.expect("cutex/asks/pause");
    let ask = agent_delegate(&w, 0, 1, "s31-ask");
    // The requester's own view shows the hold (the delegation panel's data).
    let until = Instant::now() + T;
    loop {
        let listed = w.mcp(0, "agent_list", json!({})).unwrap_or_else(|e| panic!("agent_list: {e}"));
        let held = listed["my_delegations"].as_array().into_iter().flatten()
            .any(|row| row["title"] == "s31-ask" && !row["hold"].is_null());
        if held {
            w.recorder.mark("askHeld", json!({"ask": ask}));
            break;
        }
        assert!(Instant::now() < until, "{}: the delegation never showed a hold: {listed}", w.name);
        std::thread::sleep(Duration::from_millis(250));
    }
    // Give the store hook's writer time to have written a record, had it one.
    std::thread::sleep(Duration::from_secs(3));
    assert!(ids_of_kind(&w.trace(), w.sid(0), "delegationStatus").is_empty(), "{}: a passive status notice became a ledger record", w.name);
    let resumed = w.daemon.cutex(w.sid(1), "cutex/asks/resume", json!({}));
    w.recorder.mark("asksResume", json!({"result": resumed.as_ref().map_err(|e| e.clone())}));
    resumed.expect("cutex/asks/resume");
    wait_delegation_turn(&w, 1);
    let result = wait_kind(&w, 0, "delegationResult", 1, SETTLE).remove(0);
    w.wait_settled(&[&result], SETTLE);
    let (sid0, wname) = (w.sid(0).to_string(), w.name.clone());
    let t = w.finish(STRICT);
    assert!(ids_of_kind(&t, &sid0, "delegationStatus").is_empty(), "no status record, ever");
    assert!(t.latest(&result).is_some_and(|r| r["state"] == "inContext"), "the result is delivered: {:?}", t.path(&result));
    // Nothing else on the requester side ever entered nextUp / pendingNotes.
    for record in t.all_records().iter().filter(|r| r["cutexSessionId"] == sid0.as_str() && r["inputId"] != result.as_str()) {
        assert!(!matches!(record["surface"].as_str(), Some("nextUp" | "pendingNotes")), "{wname}: {record}");
    }
}
both!(s31_no_passive_status_notice, s31, green);

// S33 (r274.2 §9.6) history revert: two owner turns in context; the owner
// reverts before turn 2 (`cutex/session/history/revert`). The turn-2
// record becomes `discarded{historyReverted}` (surface none, evidence and
// turnId kept); turn 1's is unchanged; a fresh `/inputs` read ("after a
// reload") shows no timeline record of the removed turn.
fn s33(kind: Kind) {
    let w = World::start(&name("s33", kind), WorldSpec::new(&[kind]).script(0, auto(300)));
    let sid = w.sid(0).to_string();
    if kind == Kind::Codex {
        // cli_app `session_history` backs the rollout up from the host
        // Codex home before `thread/revert`.
        let dir = w.daemon.home.config_dir.join("codex-home").join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        let thread = w.fake(0).thread.clone();
        let path = dir.join(format!("rollout-s33-{thread}.jsonl"));
        std::fs::write(&path, format!("{}\n", json!({"type": "session_meta", "payload": {"id": thread}}))).unwrap();
        w.fake(0).script(|s| s.history_path = Some(path.display().to_string()));
    }
    for (index, id) in ["s33-m1", "s33-m2"].into_iter().enumerate() {
        assert_result_input(&w.submit(0, id, "auto", json!({})), id);
        w.fake(0).wait_call("turn/start", index + 1, T);
        w.wait_state(id, "inContext", SETTLE);
        assert!(w.wait(T, |t| t.entries.iter().filter(|e| e.source == "session" && e.kind == "native"
            && e.cutex_session_id.as_deref() == Some(sid.as_str()) && e.payload["method"] == "turn/completed").count() > index),
            "{}: turn {} never completed", w.name, index + 1);
    }
    let read = |w: &World| -> Vec<Value> {
        let (status, body) = w.daemon.http("GET", &format!("/v2/sessions/{sid}/inputs?includeSettledSecs=604800"), None).unwrap();
        assert_eq!(status, 200, "{body}");
        body["records"].as_array().cloned().unwrap_or_default()
    };
    let find = |records: &[Value], id: &str| records.iter().find(|r| r["inputId"] == id).cloned()
        .unwrap_or_else(|| panic!("{id} missing from {records:?}"));
    let before = read(&w);
    let (m1, m2) = (find(&before, "s33-m1"), find(&before, "s33-m2"));
    let turn2 = m2["turnId"].as_str().expect("turn-2 record is bound to its turn").to_string();
    assert_ne!(m1["turnId"], m2["turnId"]);
    assert_eq!((m2["state"].as_str(), m2["surface"].as_str()), (Some("inContext"), Some("timeline")));
    let reverted = w.daemon.cutex(&sid, "cutex/session/history/revert", json!({"beforeTurnId": turn2}));
    w.recorder.mark("historyRevert", json!({"result": reverted.as_ref().map_err(|e| e.clone())}));
    let reverted = reverted.unwrap_or_else(|e| panic!("{}: history/revert: {e}", w.name));
    assert_eq!(reverted["removedTurnIds"], json!([turn2]), "{reverted}");
    w.wait_state("s33-m2", "discarded{historyReverted}", T);
    // After a reload: a fresh read.
    let after = read(&w);
    let (m1_after, m2_after) = (find(&after, "s33-m1"), find(&after, "s33-m2"));
    assert_eq!(m1_after, m1, "the turn-1 record is unchanged");
    assert_eq!(m2_after["state"], "discarded");
    assert_eq!(m2_after["reason"], "historyReverted");
    assert_eq!(m2_after["surface"], "none");
    assert_eq!(m2_after["evidence"], m2["evidence"], "evidence kept");
    assert_eq!(m2_after["turnId"], json!(turn2), "turnId kept");
    assert_eq!(m2_after["actions"], json!([]));
    assert_eq!(m2_after["details"]["historyRevert"], json!({"actionId": reverted["actionId"], "turnId": turn2}));
    assert!(m2_after["rev"].as_u64() > m2["rev"].as_u64());
    assert!(after.iter().all(|r| !(r["surface"] == "timeline" && r["turnId"] == json!(turn2))),
        "a timeline record of the removed turn: {after:?}");
    let t = w.finish(STRICT);
    assert_path(&t, "s33-m1", &["received", "sending", "inContext(turnStartInput)"]);
    assert_path(&t, "s33-m2", &["received", "sending", "inContext(turnStartInput)", "discarded{historyReverted}(turnStartInput)"]);
    assert_surfaces(&t, "s33-m2", &["timeline", "none"]);
}
both!(s33_history_revert_retracts_ledger_records, s33, green);

fn first_text(params: &Value) -> String {
    params["input"][0]["text"].as_str().unwrap_or("").to_string()
}

// S25 load (050 §6 #5): 10 agents message each other every second, the owner
// sends every 5 s, a new agent every 30 s, for 30 min. Parameters:
// INPUT_LEDGER_E2E_LOAD_AGENTS (10), _LOAD_SECS (1800), _LOAD_OWNER_EVERY (5),
// _LOAD_NEW_AGENT_EVERY (30), _LOAD_RSS_GROWTH_MB (256), _LOAD_RSS_EVERY_S (30:
// Management working-set sample period; the series goes to stderr and to
// `s25-load-rss.csv` in INPUT_LEDGER_E2E_TRACE_DIR, else in the run home).
fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn working_set_kb(pid: u32) -> Option<u64> {
    let out = std::process::Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let field = text.split("\",\"").last()?;
    field.chars().filter(char::is_ascii_digit).collect::<String>().parse().ok()
}

/// One working-set sample: (elapsed s, KiB, agents online).
type RssSample = (f64, u64, usize);

/// (min KiB, max KiB, least-squares slope in KiB/min over the last third of
/// the samples, at least two): growth vs plateau at a glance.
fn rss_stats(series: &[RssSample]) -> (u64, u64, Option<f64>) {
    let min = series.iter().map(|s| s.1).min().unwrap_or(0);
    let max = series.iter().map(|s| s.1).max().unwrap_or(0);
    let tail = &series[series.len() - (series.len() / 3).max(2).min(series.len())..];
    let n = tail.len() as f64;
    let (mx, my) = (tail.iter().map(|s| s.0).sum::<f64>() / n, tail.iter().map(|s| s.1 as f64).sum::<f64>() / n);
    let sxx: f64 = tail.iter().map(|s| (s.0 - mx).powi(2)).sum();
    let sxy: f64 = tail.iter().map(|s| (s.0 - mx) * (s.1 as f64 - my)).sum();
    let slope = (tail.len() >= 2 && sxx > 0.0).then(|| sxy / sxx * 60.0);
    (min, max, slope)
}

#[test]
fn s25_rss_stats_show_min_max_and_the_last_third_slope() {
    let series: Vec<RssSample> = (0..9).map(|i| (i as f64 * 30.0, if i < 6 { 1000 + i * 100 } else { 1500 }, 10)).collect();
    let (min, max, slope) = rss_stats(&series);
    assert_eq!((min, max), (1000, 1500));
    assert!(slope.unwrap().abs() < 1e-9, "plateau: {slope:?}");
    let rising: Vec<RssSample> = (0..6).map(|i| (i as f64 * 60.0, 1000 + i * 10, 10)).collect();
    assert!((rss_stats(&rising).2.unwrap() - 10.0).abs() < 1e-9);
    assert_eq!(rss_stats(&[(0.0, 5, 1)]).2, None);
}

/// Re-check a saved trace and print the deadline-miss report:
/// INPUT_LEDGER_E2E_REPORT_TRACE=<trace.jsonl> [INPUT_LEDGER_E2E_REPORT_LOG=<management.log>].
#[test]
#[ignore = "offline tool: needs INPUT_LEDGER_E2E_REPORT_TRACE"]
fn report_saved_trace() {
    let path = std::env::var_os("INPUT_LEDGER_E2E_REPORT_TRACE").expect("INPUT_LEDGER_E2E_REPORT_TRACE");
    let trace = Trace::from_jsonl(&std::fs::read_to_string(path).unwrap());
    let log = std::env::var_os("INPUT_LEDGER_E2E_REPORT_LOG").and_then(|p| std::fs::read_to_string(p).ok());
    match trace::check_surface_trace(&trace, LOOSE) {
        Ok(summary) => eprintln!("trace passes: {summary:?}"),
        Err(errors) => eprintln!("{}{}", errors.join("
"), trace::deadline_report(&trace, &errors, log.as_deref())),
    }
}

#[test]
#[ignore = "load, 30 min by default; p99 from inputLedger.transition of /v2/diagnostics/control (and the bus's)"]
fn s25_load() {
    let agents = env_u64("INPUT_LEDGER_E2E_LOAD_AGENTS", 10) as usize;
    let secs = env_u64("INPUT_LEDGER_E2E_LOAD_SECS", 1800);
    let owner_every = env_u64("INPUT_LEDGER_E2E_LOAD_OWNER_EVERY", 5);
    let new_every = env_u64("INPUT_LEDGER_E2E_LOAD_NEW_AGENT_EVERY", 30);
    let growth_mb = env_u64("INPUT_LEDGER_E2E_LOAD_RSS_GROWTH_MB", 256);
    let rss_every = Duration::from_secs(env_u64("INPUT_LEDGER_E2E_LOAD_RSS_EVERY_S", 30).max(1));
    let late = ((secs / new_every.max(1)) as usize).min(60);
    // INPUT_LEDGER_E2E_LOAD_LEDGER=0: the same load with the switch off (RSS
    // comparison; no ledger events or timings to check then).
    let ledger_on = env_u64("INPUT_LEDGER_E2E_LOAD_LEDGER", 1) != 0;
    let kinds = vec![Kind::Codex; agents + late];
    let mut spec = WorldSpec::new(&kinds).bus().ledger(ledger_on);
    for index in 0..kinds.len() {
        spec = spec.script(index, auto(200));
    }
    for index in agents..agents + late {
        spec = spec.offline(index);
    }
    let w = World::start("s25-load", spec);
    let pid = w.daemon.management_pid().unwrap();
    let rss_start = working_set_kb(pid).unwrap_or(0);
    let started = Instant::now();
    let mut online = agents;
    let mut tick = 0u64;
    let mut rss: Vec<RssSample> = vec![(0.0, rss_start, online)];
    let mut next_rss = rss_every;
    while started.elapsed() < Duration::from_secs(secs) {
        std::thread::scope(|scope| {
            for i in 0..online {
                let w = &w;
                scope.spawn(move || { w.bus_send(i, (i + 1) % online, &format!("s25-{tick}-{i}"), &["--soon"]); });
            }
        });
        if tick % owner_every == 0 {
            w.submit(tick as usize % online, &format!("s25-owner-{tick}"), "auto", json!({})).ok();
        }
        if tick > 0 && tick % new_every == 0 && online < agents + late {
            w.fake(online).script(|s| s.refuse_connections = false);
            online += 1;
        }
        tick += 1;
        if started.elapsed() >= next_rss {
            if let Some(kib) = working_set_kb(pid) {
                rss.push((started.elapsed().as_secs_f64(), kib, online));
            }
            while next_rss <= started.elapsed() {
                next_rss += rss_every;
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    let rss_end = working_set_kb(pid).unwrap_or(0);
    let elapsed = started.elapsed();
    rss.push((elapsed.as_secs_f64(), rss_end, online));
    // The series (pass/fail stays end - start below): growth vs plateau.
    let csv: String = std::iter::once("elapsed_s,working_set_kib,agents_online".to_string())
        .chain(rss.iter().map(|(at, kib, n)| format!("{at:.0},{kib},{n}")))
        .collect::<Vec<_>>()
        .join("
");
    for (at, kib, n) in &rss {
        eprintln!("s25: rss {at:>6.0} s {kib:>9} KiB {n:>3} agents");
    }
    let (rss_min, rss_max, slope) = rss_stats(&rss);
    let csv_path = std::env::var_os("INPUT_LEDGER_E2E_TRACE_DIR").map(std::path::PathBuf::from)
        .unwrap_or_else(|| w.daemon.home.root.clone()).join("s25-load-rss.csv");
    let written = std::fs::write(&csv_path, csv + "
").map(|_| csv_path.display().to_string()).unwrap_or_else(|e| format!("not written: {e}"));
    eprintln!("s25: rss {} samples every {} s: min {rss_min} KiB, max {rss_max} KiB, last-third slope {} KiB/min; csv {written}",
        rss.len(), rss_every.as_secs(), slope.map_or("n/a".to_string(), |s| format!("{s:+.1}")));
    // Ledger write timing (r274): `inputLedger` of the Management process and
    // of the Agent Bus process (bus-row writes happen in either); present only
    // while the switch is on.
    let (_, control) = w.daemon.http("GET", "/v2/diagnostics/control", None).unwrap();
    let ledger = control.get("inputLedger").cloned();
    let bus_ledger = daemon::http_request(w.daemon.bus_port, &w.daemon.bus_token_from_config(), "GET", "/api/diagnostics/control", None)
        .ok().and_then(|(_, value)| value.get("inputLedger").cloned());
    eprintln!("s25: {:.0} s, {tick} ticks, {online} agents online; management RSS {rss_start} KiB -> {rss_end} KiB ({:+} KiB)",
        elapsed.as_secs_f64(), rss_end as i64 - rss_start as i64);
    eprintln!("s25: management inputLedger {}", ledger.as_ref().map_or("absent".to_string(), Value::to_string));
    eprintln!("s25: agent bus inputLedger {}", bus_ledger.as_ref().map_or("absent".to_string(), Value::to_string));
    let t = w.finish(LOOSE);
    if !ledger_on {
        assert!(rss_end.saturating_sub(rss_start) <= growth_mb * 1024, "management working set grew {} KiB (switch off)", rss_end.saturating_sub(rss_start));
        return;
    }
    assert!(t.entries.iter().any(|e| e.kind == "stateChanged"), "no ledger events under load");
    assert!(rss_end.saturating_sub(rss_start) <= growth_mb * 1024, "management working set grew {} KiB", rss_end.saturating_sub(rss_start));
    let ledger = ledger.unwrap_or_else(|| missing_hook("inputLedger in /v2/diagnostics/control (switch on)"));
    let transition = &ledger["transition"];
    assert!(transition["count"].as_u64().unwrap_or(0) > 0, "no timed ledger writes in Management: {ledger}");
    let p99 = transition["p99Ms"].as_f64().unwrap_or(f64::MAX);
    assert!(p99 < 5.0, "Management ledger write p99 {p99} ms: {ledger}");
    if let Some(bus) = bus_ledger.filter(|bus| bus["transition"]["count"].as_u64().unwrap_or(0) > 0) {
        let p99 = bus["transition"]["p99Ms"].as_f64().unwrap_or(f64::MAX);
        assert!(p99 < 5.0, "Agent Bus ledger write p99 {p99} ms: {bus}");
    }
}

// r274 ungate: S10 job and S24 with the ledger switch OFF (delivery only; no
// ledger reads, the ledger file is never created). The Claude observer takes
// Job Service notices and wakes on a queued bus message / turn end without
// the ledger (before: notices stayed on the bus; tells waited for the 5 s
// observer pass).
#[test]
fn switch_off_s10_job_notice_reaches_claude() {
    let w = World::start("off-s10j-claude", WorldSpec::new(&[Kind::Claude]).bus().ledger(false).persistent(0).script(0, auto(300)));
    let (status, receipt) = w.job_notice(0, "off-s10-job");
    assert_eq!(status, 200, "job completion: {receipt}");
    let inbox = w.fake(0).wait_inbox(1, Duration::from_secs(30));
    assert_eq!(inbox.len(), 1, "{inbox:?}");
    let input = inbox[0].envelope.as_ref().unwrap_or(&inbox[0].params).to_string();
    assert!(input.contains("cutex-job-service") && input.contains("job_completion"), "{input}");
    assert!(!w.daemon.home.ledger_file().exists(), "switch off: no ledger file");
    w.finish(LOOSE);
}

#[test]
fn switch_off_s24_cross_host_tell_reaches_claude_at_once() {
    let mut receiver_spec = WorldSpec::new(&[Kind::Claude]).bus().ledger(false).persistent(0).script(0, auto(300));
    receiver_spec.options.peers = true;
    let b = World::start("off-s24-claude", receiver_spec);
    let mut sender_spec = WorldSpec::new(&[Kind::Codex]).bus().ledger(false).persistent(0).script(0, auto(300));
    sender_spec.options.peers = true;
    let a = World::start("off-s24-sender-claude", sender_spec);
    let pair = daemon::pair_peers(&a.daemon, &b.daemon, b.sid(0));
    let to = format!("{}@{}", b.sid(0), pair.receiver_label);
    let tell = |text: &str, wake: bool| {
        let value = a.mcp(0, "agent_tell", json!({"to": to, "text": text, "wake": wake}))
            .unwrap_or_else(|e| panic!("agent_tell {to}: {e}"));
        assert_eq!(value["ok"], true, "agent_tell {to}: {value}");
    };
    let idle = |b: &World| {
        let until = Instant::now() + T;
        while b.fake(0).active_turn().is_some() {
            assert!(Instant::now() < until, "receiver turn did not end");
            std::thread::sleep(Duration::from_millis(20));
        }
        b.daemon.wait_active_turn(b.sid(0), false, T);
    };
    // Warm-up: the first wake may meet an observer that has not passed yet.
    tell("off-s24-warmup", true);
    b.fake(0).wait_inbox(1, T);
    idle(&b);
    // Wakes to the idle receiver: each one reaches the adapter well before
    // the 5 s observer pass would (three in a row: a pass that happens to be
    // due cannot explain them all).
    for round in 0..3 {
        let before = b.fake(0).inbox().len();
        let told = Instant::now();
        tell(&format!("off-s24-wake-{round}"), true);
        b.fake(0).wait_inbox(before + 1, T);
        let took = told.elapsed();
        assert!(took < Duration::from_millis(1500), "wake {round} reached the Claude adapter after {took:?}");
        idle(&b);
    }
    // A note queued during a busy turn reaches the adapter before the
    // owner's next turn starts.
    b.fake(0).begin_turn();
    b.daemon.wait_active_turn(b.sid(0), true, T);
    let before = b.fake(0).inbox().len();
    let told = Instant::now();
    tell("off-s24-note", false);
    b.fake(0).wait_inbox(before + 1, T);
    let took = told.elapsed();
    assert!(took < Duration::from_millis(1500), "busy note reached the Claude adapter after {took:?}");
    b.fake(0).complete("completed");
    idle(&b);
    assert!(!b.daemon.home.ledger_file().exists() && !a.daemon.home.ledger_file().exists(), "switch off: no ledger file");
    b.finish(LOOSE);
    a.finish(LOOSE);
}


// ---------------------------------------------------------------------------
// Design 057 S27–S30. Actual isolated daemon, opt-in FAKE native owner carry
// protocol. Production runtime/adapter tests are maintained separately.

fn carry_ids_057(params: &Value) -> Vec<String> {
    params["ownerCarries"].as_array().into_iter().flatten()
        .filter_map(|e| e["message"]["id"].as_str().map(str::to_string)).collect()
}

fn assert_owner_and_carry_items_057(w: &World, owner: &str, notice: &str, turn: &str) {
    let turns = w.fake(0).turns();
    let native = turns.iter().find(|value| value["id"] == turn).expect("native consuming turn");
    let items = native["items"].as_array().unwrap();
    let owner_at = items.iter().position(|item| item["type"] == "userMessage" && item["clientId"] == owner).expect("owner native item");
    let notice_at = items.iter().position(|item| item["id"] == notice && item["name"] == "external_event").expect("separate carried native item");
    assert!(owner_at < notice_at, "owner text first, carried service/agent text separate: {native}");
    assert_eq!(notice_at, owner_at + 1, "carry shares the owner's native boundary, with no intervening tool: {native}");
    assert_eq!(items[owner_at]["content"][0]["text"], owner, "carry must not be merged into owner text");
    assert_eq!(items.iter().filter(|item| item["id"] == notice).count(), 1, "one native occurrence");
    let entry = w.fake(0).inbox().into_iter().find(|entry| entry.id == notice).expect("fake carried native identity");
    assert!(entry.persisted, "context only after completing the consuming turn");
    assert_eq!(entry.turn.as_deref(), Some(turn));
}

fn assert_carried_ledger_057(w: &World, owner: &str, notice: &str, turn: &str) {
    let owner_record = w.input_record(0, owner);
    let child = w.input_record(0, notice);
    assert!(owner_record["carries"].as_array().is_some_and(|ids| ids.iter().any(|id| id == notice)), "{owner_record}");
    assert_eq!(child["state"], "inContext", "{child}");
    assert_eq!(child["reason"], "carriedByOwner", "{child}");
    assert_eq!(child["turnId"], turn, "actual consuming native turn: {child}");
    assert!(w.fake(0).calls_of("thread/externalInput/submit").iter().all(|call| call.params["message"]["id"] != notice),
        "reserved carry must never have an independent ExternalInput submit");
    assert_owner_and_carry_items_057(w, owner, notice, turn);
}

fn s057_s27(kind: Kind) {
    let w = World::start(&name("s057-s27", kind), WorldSpec::new(&[kind]).bus().persistent(0).owner_carries(0));
    w.fake(0).begin_turn();
    w.daemon.wait_active_turn(w.sid(0), true, T);
    w.stop_owner(0);
    w.system_notice(0, "s057-s27 stopped background work");
    let notice = wait_kind(&w, 0, "systemNotice", 1, T).remove(0);
    assert!(w.fake(0).inbox().is_empty(), "session stopHold keeps notice in daemon source");
    w.submit(0, "s057-s27-owner", "auto", json!({})).expect("owner sends after stop");
    let calls = w.fake(0).wait_call("turn/start", 1, T);
    let request = calls.iter().find(|call| call.params["clientUserMessageId"] == "s057-s27-owner").unwrap();
    assert_eq!(carry_ids_057(&request.params), [notice.clone()]);
    let turn = w.fake(0).active_turn().unwrap()["id"].as_str().unwrap().to_string();
    assert_ne!(w.input_record(0, &notice)["state"], "inContext", "RPC/item acceptance alone is not a context receipt");
    w.fake(0).complete("completed");
    w.wait_settled(&["s057-s27-owner", &notice], SETTLE);
    assert_carried_ledger_057(&w, "s057-s27-owner", &notice, &turn);
    let trace = w.finish(STRICT);
    assert!(trace.records("session", &notice).iter().all(|record|
        !matches!(record["reason"].as_str(), Some("nextStep" | "waitInsert"))), "no next-step countdown for captured notice: {:?}", trace.path(&notice));
}
both!(s057_s27_paused_system_notice_rides_with_owner, s057_s27, green);

fn s057_s28(kind: Kind) {
    let script = Script { pending_steer: true, claude_soon: true, ..Script::default() };
    let mut spec = WorldSpec::new(&[kind, Kind::Codex]).bus().script(0, script).owner_carries(0);
    spec.options.wake_busy_wait_secs = Some(60);
    let w = World::start(&name("s057-s28", kind), spec);
    let turn = w.fake(0).begin_turn();
    w.daemon.wait_active_turn(w.sid(0), true, T);
    assert!(w.bus_send(1, 0, "s057-s28 notice before owner", &[]).status.success());
    let notice = wait_kind(&w, 0, "agentMessage", 1, T).remove(0);
    assert!(w.fake(0).inbox().is_empty(), "busy after-turn input still safely unsent");
    w.submit(0, "s057-s28-owner", "auto", json!({})).expect("busy owner steer");
    let steers = w.fake(0).wait_call("turn/steer", 1, T);
    assert_eq!(carry_ids_057(&steers[0].params), [notice.clone()]);
    assert_eq!(steers[0].params["expectedTurnId"], turn);
    assert!(!w.fake(0).turns().iter().flat_map(|turn| turn["items"].as_array().unwrap()).any(|item| item["id"] == notice),
        "carry waits with its owner's unread steer");
    w.fake(0).tool().expect("same model step tool");
    assert!(!w.fake(0).inbox().iter().any(|entry| entry.id == notice && entry.turn.is_some()));
    w.fake(0).step().expect("next tool boundary consumes owner and carry");
    assert!(!w.fake(0).inbox().iter().find(|entry| entry.id == notice).unwrap().persisted,
        "native item at the boundary still waits for the fake's context receipt");
    w.fake(0).complete("completed");
    w.wait_settled(&["s057-s28-owner", &notice], SETTLE);
    assert_carried_ledger_057(&w, "s057-s28-owner", &notice, &turn);
    w.finish(STRICT);
}
both!(s057_s28_busy_owner_steer_carries_at_same_boundary, s057_s28, green);

fn s057_s29(kind: Kind) {
    let script = Script { claude_soon: true, ..Script::default() };
    let mut spec = WorldSpec::new(&[kind, Kind::Codex]).bus().script(0, script).owner_carries(0);
    spec.options.wake_busy_wait_secs = Some(0);
    let w = World::start(&name("s057-s29", kind), spec);
    w.submit(0, "s057-s29-owner", "auto", json!({})).expect("owner without pending notices");
    let start = w.fake(0).wait_call("turn/start", 1, T).remove(0);
    assert!(carry_ids_057(&start.params).is_empty());
    assert!(w.bus_send(1, 0, "s057-s29 arrived after capture", &["--soon"]).status.success());
    let notice = wait_kind(&w, 0, "agentMessage", 1, T).remove(0);
    w.fake(0).wait_call("thread/externalInput/submit", 1, T);
    assert!(w.wait(T, |_| w.fake(0).inbox().iter().any(|entry| entry.id == notice && entry.turn.is_some())), "late independent input joins native boundary");
    let owner = w.input_record(0, "s057-s29-owner");
    assert!(!owner["carries"].as_array().is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(notice.as_str()))), "{owner}");
    assert!(!w.source_owner_blocked(0, &notice));
    w.fake(0).complete("completed");
    w.wait_settled(&["s057-s29-owner", &notice], SETTLE);
    assert_ne!(w.input_record(0, &notice)["reason"], "carriedByOwner");
    w.finish(STRICT);
}
both!(s057_s29_later_arrival_is_independent, s057_s29, green);

fn s057_s30(kind: Kind) {
    let mut spec = WorldSpec::new(&[kind, Kind::Codex]).bus().owner_carries(0);
    spec.options.wake_busy_wait_secs = Some(60);
    let w = World::start(&name("s057-s30", kind), spec);
    w.fake(0).begin_turn();
    w.daemon.wait_active_turn(w.sid(0), true, T);
    w.stop_owner(0);
    assert!(w.bus_send(1, 0, "s057-s30 held notice", &[]).status.success());
    let notice = wait_kind(&w, 0, "agentMessage", 1, T).remove(0);
    assert_eq!(w.input_record(0, &notice)["details"]["ownerHold"], false);
    let held = w.hold_input(0, &notice, true);
    assert_eq!(held["reason"], "ownerPause");
    assert!(w.source_owner_blocked(0, &notice));
    // Another explicit stop and resume only clear session admission, not this input.
    w.stop_owner(0);
    w.resume_owner(0);
    assert_eq!(w.input_record(0, &notice)["details"]["ownerHold"], true);
    assert!(w.source_owner_blocked(0, &notice));
    w.submit(0, "s057-s30-first-owner", "auto", json!({})).expect("owner excludes held notice");
    let starts = w.fake(0).wait_call("turn/start", 1, T);
    assert!(carry_ids_057(&starts[0].params).is_empty());
    assert!(w.fake(0).inbox().is_empty());
    let ready = w.hold_input(0, &notice, false);
    assert_eq!(ready["state"], "waiting");
    assert_ne!(ready["reason"], "ownerPause");
    assert!(!w.source_owner_blocked(0, &notice));
    w.submit(0, "s057-s30-second-owner", "auto", json!({})).expect("released notice now rides with owner");
    let steer = w.fake(0).wait_call("turn/steer", 1, T).remove(0);
    assert_eq!(carry_ids_057(&steer.params), [notice.clone()]);
    let turn = w.fake(0).active_turn().unwrap()["id"].as_str().unwrap().to_string();
    if kind == Kind::Claude { w.wait_state("s057-s30-second-owner", "accepted[offered]", T); }
    w.fake(0).complete("completed");
    w.wait_settled(&["s057-s30-first-owner", "s057-s30-second-owner", &notice], SETTLE);
    assert_carried_ledger_057(&w, "s057-s30-second-owner", &notice, &turn);
    w.finish(STRICT);
}
both!(s057_s30_per_input_hold_survives_stop_resume, s057_s30, green);


#[test]
fn s057_fake_carry_capability_opt_in_and_malformed_bundle_is_atomic() {
    for kind in [Kind::Codex, Kind::Claude] {
        let legacy = FakeRuntime::start(kind, "C:/fixture");
        let mut legacy_client = Client::connect(&legacy.url);
        assert!(legacy_client.request("initialize", json!({})).unwrap().get("ownerInputCarries").is_none(),
            "old scenarios must retain legacy capability behavior");
        let fake = FakeRuntime::start_with(kind, "C:/fixture", Script { owner_input_carries: true, ..Script::default() });
        let mut client = Client::connect(&fake.url);
        assert_eq!(client.request("initialize", json!({})).unwrap()["ownerInputCarries"], 1);
        let mut envelope: cutex::app_server::external_input::Envelope = serde_json::from_value(json!({
            "version": 1, "ownerId": "cutex.11111111-1111-4111-8111-111111111111",
            "threadId": fake.thread, "runtimeGeneration": 1,
            "message": {"id": "fake-057-notice", "source": {"kind": "service", "id": "fixture-service"},
                "type": "message", "delivery": "after_turn", "text": "separate service notice"},
            "semanticSha256": ""
        })).unwrap();
        envelope.semantic_sha256 = envelope.digest();
        let valid = serde_json::to_value(envelope).unwrap();
        let mut malformed = valid.clone();
        malformed["message"]["id"] = json!("malformed-057-notice");
        let mut params = json!({"threadId": fake.thread, "clientUserMessageId": "fake-057-owner",
            "input": [{"type":"text", "text":"fake-057-owner"}], "ownerCarries": [valid, malformed]});
        assert!(client.request("turn/start", params.clone()).is_err());
        assert!(fake.turns().is_empty() && fake.inbox().is_empty(), "validate the whole bundle before owner/carry mutation");
        params["ownerCarries"] = json!([valid]);
        let started = client.request("turn/start", params).unwrap();
        let turn = started["turn"]["id"].as_str().unwrap().to_string();
        let items = started["turn"]["items"].as_array().unwrap();
        assert_eq!(items[0]["clientId"], "fake-057-owner");
        assert_eq!(items[0]["content"][0]["text"], "fake-057-owner");
        assert_eq!(items[1]["id"], "fake-057-notice");
        let query = json!({"version":1, "ownerId":valid["ownerId"], "threadId":fake.thread,
            "runtimeGeneration":1, "messages":[{"messageId":valid["message"]["id"], "semanticSha256":valid["semanticSha256"]}]});
        assert!(client.request("thread/externalInput/status", query.clone()).unwrap()["statuses"][0]["receipt"].is_null());
        fake.complete("completed");
        let status = client.request("thread/externalInput/status", query).unwrap();
        assert_eq!(status["statuses"][0]["deliveryState"], "context_persisted");
        assert_eq!(status["statuses"][0]["receipt"]["turnId"], turn);
        assert_eq!(status["statuses"][0]["receipt"]["messageId"], "fake-057-notice");
        assert!(fake.calls_of("thread/externalInput/submit").is_empty());
    }
}


// Exact kit predecessor proof: old daemon creates pending sources; while every
// writer is stopped, current repositories add057 hold/carry metadata and ledger
// projections. Then the unchanged exact-r275 binary must read and deliver them.
fn r276_previous_notice_delivery(kind: Kind) {
    use cutex::app_server::external_input::{Delivery,Envelope,Message,Source,SourceKind};
    use cutex::management::v2::{agent_bus_state::AgentBusMessageRepository,input_ledger::{InputLedger,Expect,Next,Action}};
    let previous=std::path::PathBuf::from(std::env::var_os("R276_PREVIOUS_EXE").expect("exact r275 debug executable"));
    assert!(previous.is_file());
    let mut spec=WorldSpec::new(&[kind,Kind::Codex]).bus().owner_carries(0);
    spec.options.executable=Some(previous);
    // r275 debug Codex ingress admits after-turn while busy. Keep that hook
    // off until all stopped-source metadata is installed; then enable delivery.
    if kind==Kind::Codex {spec.options.fake_external_input=false;}
    let mut w=World::start(&name("r276-previous-notices",kind),spec);
    w.fake(0).begin_turn();
    w.daemon.wait_active_turn(w.sid(0),true,T);
    w.stop_owner(0);
    for marker in ["r276 waiting notice","r276 held notice","r276 carried notice"] {
        let sent=w.bus_send(1,0,marker,&[]);
        assert!(sent.status.success(),"{}",String::from_utf8_lossy(&sent.stderr));
    }
    let ids=wait_kind(&w,0,"agentMessage",3,T);
    let find=|marker:&str|ids.iter().find(|id|w.input_record(0,id)["preview"].as_str().is_some_and(|text|text.contains(marker))).unwrap().clone();
    let waiting=find("r276 waiting");let held=find("r276 held");let carried=find("r276 carried");
    let sid=w.sid(0).to_string();let sender=w.sid(1).to_string();let thread=w.fake(0).thread.clone();
    assert!(w.fake(0).inbox().is_empty());
    w.recorder.stop().save(&w.daemon.home.root.join("preparation-trace.jsonl")).unwrap();
    w.recorder=daemon::Recorder::new();
    w.daemon.stop_all();
    let root=w.daemon.home.config_dir.join("runtime/management-v2");
    {
        let repository=AgentBusMessageRepository::open(&root).unwrap();
        repository.set_owner_hold(&sid,&held,true).unwrap();
        let envelopes=repository.reserve_owner_carries(&sid,"r276-unsubmitted-owner",8,|message| {
            anyhow::ensure!(message.id==carried,"not this source reservation");
            let mut envelope=Envelope {version:1,owner_id:sid.clone(),thread_id:thread.clone(),runtime_generation:1,
                message:Message {id:message.id.clone(),source:Source{kind:SourceKind::Agent,id:sender.clone()},
                    event_type:"message".into(),delivery:Delivery::AfterTurn,text:message.content.clone()},
                view:None,semantic_sha256:String::new()};
            envelope.semantic_sha256=envelope.digest();envelope.validate()?;Ok(envelope)
        }).unwrap();
        assert_eq!(envelopes.len(),1);
        assert!(repository.owner_delivery_blocked(&sid,&held).unwrap());
        assert!(repository.owner_delivery_blocked(&sid,&carried).unwrap());
    }
    {
        let ledger=InputLedger::open(w.daemon.home.ledger_file());
        for (id,owner_hold) in [(&waiting,false),(&held,true),(&carried,false)] {
            let transition=ledger.transition(&sid,id,Expect::ANY,
                Next::waiting(if owner_hold {"ownerPause"} else {"afterTurn"}).details(json!({"ownerHold":owner_hold}))).unwrap();
            assert!(transition.record.actions.contains(&if owner_hold {Action::ReleaseHold} else {Action::Hold}));
        }
    }
    // The rollback policy ends holds, permits duplicate carry delivery, and
    // reports identities. It never invents settled/context evidence.
    eprintln!("r276 rollback notice policy {}",json!({"endedHoldCount":1,"endedHoldIds":[held.clone()],
        "replayEligibleCarryCount":1,"replayEligibleCarryIds":[carried.clone()]}));
    assert!(w.fake(0).inbox().is_empty());
    w.daemon.options.fake_external_input=true;
    w.daemon.start();
    w.daemon.wait_connected(&sid,world::CONNECT_TIMEOUT);
    w.recorder.subscribe_latest(&w.daemon,"session",Some(&sid));
    w.recorder.subscribe_latest(&w.daemon,"host",None);
    for id in [&waiting,&held,&carried] {
        let record=w.input_record(0,id);
        assert_ne!(record["state"],"inContext","no receipt exists before rollback delivery");
    }
    w.resume_owner(0);
    let until=Instant::now()+SETTLE;
    loop {
        if w.fake(0).active_turn().is_some() { w.fake(0).complete("completed"); }
        let inbox=w.fake(0).inbox();
        if ids.iter().all(|id|inbox.iter().any(|entry|&entry.id==id&&entry.persisted)) {break;}
        assert!(Instant::now()<until,"exact previous did not persist every notice: {:?}",ids);
        std::thread::sleep(Duration::from_millis(50));
    }
    w.wait_settled(&[&waiting,&held,&carried],SETTLE);
    for id in [&waiting,&held,&carried] {
        assert_eq!(w.input_record(0,id)["state"],"inContext");
    }
    assert!(!w.source_owner_blocked(0,&held));
    assert!(!w.source_owner_blocked(0,&carried));
    w.finish(LOOSE);
}

#[test]
#[ignore="requires R276_PREVIOUS_EXE pinned to exact r275, isolated kit-predecessor probe"]
fn r276_r275_notice_delivery_claude(){r276_previous_notice_delivery(Kind::Claude);}
#[test]
#[ignore="requires R276_PREVIOUS_EXE pinned to exact r275, isolated kit-predecessor probe"]
fn r276_r275_notice_delivery_codex(){r276_previous_notice_delivery(Kind::Codex);}
