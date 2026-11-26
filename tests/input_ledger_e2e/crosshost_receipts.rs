//! Two real private daemons, one loopback connection, no provider calls.
use super::{auto, find_id, wait_delegation_turn, ASK_PREFIXES, LOOSE};
use crate::daemon::{http_request_with_headers, pair_peers, Daemon, PeerPair};
use crate::fake_runtime::Kind;
use crate::world::{World, WorldSpec};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(40);

fn world(name: &str, previous: Option<PathBuf>) -> World {
    let mut spec = WorldSpec::new(&[Kind::Codex])
        .bus()
        .persistent(0)
        .script(0, auto(700));
    spec.options.peers = true;
    spec.options.executable = previous;
    World::start(name, spec)
}

#[track_caller]
fn request(daemon: &Daemon, method: &str, path: &str, body: Option<&Value>) -> Value {
    let (status, value) = daemon
        .http(method, path, body)
        .expect("private Management HTTP");
    assert!(
        (200..300).contains(&status),
        "{method} {path}: {status} {value}"
    );
    value
}

fn ask(daemon: &Daemon, id: &str) -> Option<Value> {
    let (status, value) = daemon.http("GET", &format!("/v2/asks/{id}"), None).unwrap();
    match status {
        404 => None,
        200 => Some(value["ask"].clone()),
        status => panic!("read private ask: {status} {value}"),
    }
}

fn wait_ask(daemon: &Daemon, id: &str, predicate: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let view = ask(daemon, id);
        if let Some(view) = &view {
            if predicate(view) {
                return view.clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "ask {id} on {} did not settle: {view:?}",
            daemon.port
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn wait_receipt(daemon: &Daemon, id: &str, expected: &str) -> Value {
    wait_ask(daemon, id, |view| {
        view["crossHost"]["receipt"]["state"] == expected
    })
}

fn source_ask(target: &World, pair: &PeerPair, action: &str) -> Value {
    json!({
        "target": format!("{}@{}", target.sid(0), pair.receiver_label),
        "title": action,
        "text": "Private coordination fixture",
        "notify": "passive",
        "action_id": action,
    })
}

fn delegate(source: &World, args: &Value) -> Value {
    let mut result = source
        .mcp(0, "agent_delegate", args.clone())
        .expect("source delegate tool");
    let deadline = Instant::now() + DEADLINE;
    while result["status"] == "running" {
        assert!(
            Instant::now() < deadline,
            "delegate tool still running: {result}"
        );
        std::thread::sleep(Duration::from_millis(100));
        result = source
            .mcp(
                0,
                "action_status",
                json!({
                    "tool": "agent_delegate", "action_id": args["action_id"],
                }),
            )
            .expect("source action status");
    }
    result.get("result").cloned().unwrap_or(result)
}

fn ask_id(source: &World, result: &Value) -> String {
    assert_eq!(result["ok"], true, "{result}");
    let id = find_id(result, ASK_PREFIXES).unwrap_or_else(|| panic!("missing ask id: {result}"));
    let prefix = match id.strip_prefix("delegation-") {
        Some(suffix) => format!("ask-{suffix}"),
        None => id,
    };
    let page = request(&source.daemon, "GET", "/v2/asks", None);
    let mut matching = page["asks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|ask| ask["askId"].as_str())
        .filter(|id| id.starts_with(&prefix));
    let id = matching
        .next()
        .unwrap_or_else(|| panic!("no owner record for {prefix}: {page}"));
    assert!(matching.next().is_none(), "ambiguous fixture id {prefix}");
    id.to_string()
}

fn pause(target: &World) {
    target
        .daemon
        .cutex(target.sid(0), "cutex/asks/pause", json!({}))
        .unwrap();
}

fn resume(target: &World) {
    target
        .daemon
        .cutex(target.sid(0), "cutex/asks/resume", json!({}))
        .unwrap();
}

fn project_member(world: &World, project: &str) {
    // The generic fake runtime fixture seeds a persistent session without a
    // roster row. Import through the same reviewed Human endpoint as desktop.
    let candidates = request(
        &world.daemon,
        "GET",
        "/v2/agent-management/durable-candidates",
        None,
    );
    let candidate = candidates
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| candidate["cutex_session_id"] == world.sid(0))
        .expect("fixture agent candidate");
    let formal_name = candidate["formal_name"].as_str().unwrap_or(project);
    let imported = request(
        &world.daemon,
        "POST",
        "/v2/agent-management/durable-import",
        Some(&json!({
            "action_id": format!("{project}-import"), "candidate": candidate,
            "confirmed_formal_name": formal_name, "assignment": null, "detach": null,
        })),
    );
    assert_eq!(imported["complete"], true, "{imported}");
    let path = "/v2/agent-management/project-mutations";
    let schema = "cutex/human-management-project-mutation/v1";
    request(
        &world.daemon,
        "POST",
        path,
        Some(&json!({
            "schema": schema,
            "action_id": format!("{project}-create"),
            "project_id": project,
            "operation": {"kind": "create_project", "presentation": {
                "display_name": project, "badge_label": "XH", "color": "blue",
            }},
        })),
    );
    request(
        &world.daemon,
        "POST",
        path,
        Some(&json!({
            "schema": schema,
            "action_id": format!("{project}-add"),
            "project_id": project,
            "operation": {"kind": "add_member", "cutex_session_id": world.sid(0)},
        })),
    );
}

fn open_routes(world: &World, project: &str) {
    let path = format!("/v2/projects/{project}/delegation-policy");
    let current = request(&world.daemon, "GET", &path, None);
    request(
        &world.daemon,
        "PUT",
        &path,
        Some(&json!({
            "actionId": format!("{project}-open"),
            "expectedRevision": current["revision"],
            "memberMode": "open",
        })),
    );
}

#[test]
fn crosshost_phase1_distinguishes_source_and_destination_routing() {
    let a = world("xh-policy-a", None);
    let b = world("xh-policy-b", None);
    project_member(&a, "project-xh-a");
    let pair = pair_peers(&a.daemon, &b.daemon, b.sid(0));
    let source_refused = delegate(&a, &source_ask(&b, &pair, "xh-source-refused"));
    assert_eq!(
        source_refused["code"], "delegation_cross_host_source_routing_refused",
        "{source_refused}"
    );
    assert_eq!(source_refused["status"], "no_write", "{source_refused}");
    assert_eq!(
        source_refused["crossHost"]["receipt"]["reason"],
        "sourceRoutingRefused"
    );
    assert_eq!(
        source_refused["crossHost"]["peer"]["instanceId"],
        pair.receiver_on_sender
    );
    assert!(request(&a.daemon, "GET", "/v2/asks", None)["asks"]
        .as_array()
        .unwrap()
        .is_empty());

    open_routes(&a, "project-xh-a");
    project_member(&b, "project-xh-b");
    let refused = ask_id(
        &a,
        &delegate(&a, &source_ask(&b, &pair, "xh-destination-refused")),
    );
    let view = wait_receipt(&a.daemon, &refused, "failed");
    assert_eq!(
        view["crossHost"]["receipt"]["reason"],
        "destinationRoutingRefused"
    );
    assert_eq!(
        view["remote"]["lastError"],
        "delegation_cross_host_destination_routing_refused"
    );
    assert!(
        ask(&b.daemon, &refused).is_none(),
        "destination must leave no ask on refusal"
    );

    open_routes(&b, "project-xh-b");
    pause(&b);
    let accepted = ask_id(
        &a,
        &delegate(&a, &source_ask(&b, &pair, "xh-after-refusal")),
    );
    wait_receipt(&a.daemon, &accepted, "deliveredToPeer");

    // The paused target already has the earlier delegation in queue. The
    // destination's ordinary "busy" refusal crosses the generic error path.
    let mut busy_args = source_ask(&b, &pair, "xh-target-busy");
    busy_args["if_busy"] = json!("fail");
    let busy = ask_id(&a, &delegate(&a, &busy_args));
    let busy_view = wait_receipt(&a.daemon, &busy, "failed");
    assert_eq!(busy_view["remote"]["lastError"], "peer_request_failed");
    assert_eq!(
        busy_view["crossHost"]["receipt"]["reason"],
        "peerRejectedRequest"
    );
    assert!(ask(&b.daemon, &busy).is_none());

    b.finish(LOOSE);
    a.finish(LOOSE);
}

#[test]
fn crosshost_phase1_disconnect_reconnect_and_replay() {
    let mut a = world("xh-send", None);
    let mut b = world("xh-recv", None);
    let pair = pair_peers(&a.daemon, &b.daemon, b.sid(0));
    pause(&b);
    let args = source_ask(&b, &pair, "xh-replay");
    let id = ask_id(&a, &delegate(&a, &args));
    let outgoing = wait_receipt(&a.daemon, &id, "deliveredToPeer");
    assert_eq!(
        outgoing["crossHost"]["peer"]["instanceId"],
        pair.receiver_on_sender
    );
    assert_eq!(outgoing["crossHost"]["peer"]["label"], pair.receiver_label);
    assert!(outgoing["crossHost"]["receipt"]["confirmedAt"].is_string());
    let inbound = wait_receipt(&b.daemon, &id, "deliveredToPeer");
    assert_eq!(inbound["crossHost"]["direction"], "inbound");
    assert_eq!(
        inbound["crossHost"]["peer"]["instanceId"],
        pair.sender_on_receiver
    );
    assert_ne!(pair.sender_on_receiver, pair.receiver_on_sender);

    b.daemon.stop_management();
    assert!(
        b.daemon.http("GET", "/v2/host", None).is_err(),
        "fixture destination must be disconnected"
    );
    let held = ask(&a.daemon, &id).unwrap();
    assert_eq!(
        held["crossHost"]["receipt"]["state"], "deliveredToPeer",
        "disconnect must preserve the last positive receipt"
    );
    assert_eq!(
        ask_id(&a, &delegate(&a, &args)),
        id,
        "replay must return the original source id while disconnected"
    );
    a.daemon.restart();
    a.daemon.wait_connected(a.sid(0), DEADLINE);
    assert_eq!(
        ask_id(&a, &delegate(&a, &args)),
        id,
        "source restart must retain the action binding"
    );

    b.daemon.restart();
    b.daemon.wait_connected(b.sid(0), DEADLINE);
    resume(&b);
    wait_delegation_turn(&b, 0);
    let accepted = wait_receipt(&a.daemon, &id, "accepted");
    assert_eq!(
        accepted["crossHost"]["peer"]["instanceId"],
        pair.receiver_on_sender
    );
    assert_eq!(ask_id(&a, &delegate(&a, &args)), id);
    let rows = request(
        &b.daemon,
        "GET",
        &format!("/v2/asks?peer={}", pair.sender_on_receiver),
        None,
    );
    let rows = rows["asks"].as_array().expect("destination ask list");
    assert_eq!(
        rows.iter()
            .filter(|row| row["title"] == "xh-replay")
            .count(),
        1,
        "{rows:?}"
    );
    assert_eq!(
        rows.iter().find(|row| row["title"] == "xh-replay").unwrap()["askId"],
        id
    );
    b.finish(LOOSE);
    a.finish(LOOSE);
}

fn peer_secret(source: &Daemon, destination: &str) -> String {
    std::fs::read_to_string(
        source
            .home
            .root
            .join(".cutex/runtime/peers/secrets")
            .join(format!("{destination}.token")),
    )
    .expect("fixture peer credential")
    .trim()
    .to_string()
}

fn hello(target: &Daemon, caller: &str, secret: &str) -> (u16, Value) {
    http_request_with_headers(
        target.port,
        secret,
        "GET",
        "/peer/v1/hello",
        &[("X-Cutex-Peer-Instance", caller.to_string())],
        None,
    )
    .expect("private peer hello")
}

#[test]
fn crosshost_phase1_remove_and_repair_replaces_credentials() {
    let a = world("xh-rkey-a", None);
    let b = world("xh-rkey-b", None);
    let first = pair_peers(&a.daemon, &b.daemon, b.sid(0));
    let old_secret = peer_secret(&a.daemon, &first.receiver_on_sender);
    assert_eq!(
        hello(&b.daemon, &first.sender_on_receiver, &old_secret).0,
        200
    );

    pause(&b);
    let id = ask_id(
        &a,
        &delegate(&a, &source_ask(&b, &first, "xh-before-rotation")),
    );
    wait_receipt(&a.daemon, &id, "deliveredToPeer");
    request(
        &a.daemon,
        "DELETE",
        &format!("/v2/peers/{}", first.receiver_on_sender),
        None,
    );
    let failed = wait_receipt(&a.daemon, &id, "failed");
    assert_eq!(failed["crossHost"]["receipt"]["reason"], "peerRemoved");
    assert_eq!(
        hello(&b.daemon, &first.sender_on_receiver, &old_secret).0,
        401
    );
    request(
        &b.daemon,
        "DELETE",
        &format!("/v2/peers/{}", first.sender_on_receiver),
        None,
    );

    let replacement = pair_peers(&a.daemon, &b.daemon, b.sid(0));
    assert_eq!(replacement.receiver_on_sender, first.receiver_on_sender);
    assert_eq!(replacement.sender_on_receiver, first.sender_on_receiver);
    let new_secret = peer_secret(&a.daemon, &replacement.receiver_on_sender);
    assert!(
        old_secret != new_secret,
        "pairing must issue a new fixture credential"
    );
    assert_eq!(
        hello(&b.daemon, &replacement.sender_on_receiver, &old_secret).0,
        401
    );
    assert_eq!(
        hello(&b.daemon, &replacement.sender_on_receiver, &new_secret).0,
        200
    );
    let next = ask_id(
        &a,
        &delegate(&a, &source_ask(&b, &replacement, "xh-after-rotation")),
    );
    assert_ne!(next, id);
    wait_receipt(&a.daemon, &next, "deliveredToPeer");
    assert_eq!(
        ask(&a.daemon, &id).unwrap()["crossHost"]["receipt"]["reason"],
        "peerRemoved"
    );
    b.finish(LOOSE);
    a.finish(LOOSE);
}

fn older_executable() -> PathBuf {
    let executable = PathBuf::from(
        std::env::var_os("CUTEX_CROSSHOST_OLDER_DAEMON")
            .expect("set the absolute path of a verified older cutex executable"),
    );
    assert!(executable.is_absolute() && executable.is_file());
    executable
}

#[test]
#[ignore = "requires a separately verified pre-feature cutex executable via CUTEX_CROSSHOST_OLDER_DAEMON"]
fn crosshost_phase1_new_source_to_older_destination() {
    let a = world("xh-new", None);
    let b = world("xh-prev", Some(older_executable()));
    let older_build = request(&b.daemon, "GET", "/v2/host", None);
    eprintln!("older peer build: {}", older_build["cutexBuild"]);
    let capabilities = request(&b.daemon, "GET", "/v2/capabilities", None);
    assert_ne!(
        capabilities["features"]["peerDeliveryReceiptsV1"], true,
        "fixture must predate these receipts"
    );
    let pair = pair_peers(&a.daemon, &b.daemon, b.sid(0));
    pause(&b);
    let args = source_ask(&b, &pair, "xh-older-compatible");
    let id = ask_id(&a, &delegate(&a, &args));
    let view = wait_receipt(&a.daemon, &id, "deliveredToPeer");
    assert_eq!(
        view["crossHost"]["peer"]["instanceId"],
        pair.receiver_on_sender
    );
    assert!(ask(&b.daemon, &id).unwrap().get("crossHost").is_none());
    assert_eq!(ask_id(&a, &delegate(&a, &args)), id);
    project_member(&b, "project-xh-older");
    let denied = ask_id(
        &a,
        &delegate(&a, &source_ask(&b, &pair, "xh-older-refused")),
    );
    let view = wait_receipt(&a.daemon, &denied, "failed");
    assert_eq!(
        view["crossHost"]["receipt"]["reason"],
        "peerRefusedUnspecified"
    );
    assert_eq!(
        view["remote"]["lastError"],
        "peer_create_refused_unspecified"
    );
    assert!(ask(&b.daemon, &denied).is_none());
    b.finish(LOOSE);
    a.finish(LOOSE);
}

#[test]
#[ignore = "requires a separately verified pre-feature daemon and its matching cutex-mcp executable"]
fn crosshost_phase1_older_source_to_new_destination() {
    let executable = older_executable();
    let facade = executable.with_file_name(format!("cutex-mcp{}", std::env::consts::EXE_SUFFIX));
    assert!(
        facade.is_file(),
        "matching older MCP facade: {}",
        facade.display()
    );
    let mut a = world("xh-prev-src", Some(executable));
    a.daemon.options.mcp_executable = Some(facade);
    let b = world("xh-new-dst", None);
    assert_ne!(
        request(&a.daemon, "GET", "/v2/capabilities", None)["features"]["peerDeliveryReceiptsV1"],
        true
    );

    let pair = pair_peers(&a.daemon, &b.daemon, b.sid(0));
    pause(&b);
    let args = source_ask(&b, &pair, "xh-old-source");
    let id = ask_id(&a, &delegate(&a, &args));
    let delivered = wait_receipt(&b.daemon, &id, "deliveredToPeer");
    assert_eq!(delivered["crossHost"]["direction"], "inbound");
    assert_eq!(
        delivered["crossHost"]["peer"]["instanceId"],
        pair.sender_on_receiver
    );
    let old_mirror = wait_ask(&a.daemon, &id, |ask| ask["remote"]["syncState"] == "synced");
    assert_eq!(old_mirror["remote"]["peerId"], pair.receiver_on_sender);
    assert!(old_mirror.get("crossHost").is_none());
    assert_eq!(ask_id(&a, &delegate(&a, &args)), id);

    resume(&b);
    wait_receipt(&b.daemon, &id, "accepted");
    b.finish(LOOSE);
    a.finish(LOOSE);
}
