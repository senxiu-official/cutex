use std::collections::BTreeSet;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::app_server::protocol::RpcNotification;
use crate::app_server::protocol::RpcServerRequest;

fn identity(pid: u32) -> ProcessIdentity {
    ProcessIdentity { pid, started_at: Some(format!("2026-10-15T00:00:0{}Z", pid % 10)) }
}

fn temp_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!("cutex-single-writer-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    root
}

/// Two simulated processes on one home: the Agent Bus service (launcher)
/// and Management (owner). `owner_alive` stands for Management's process.
struct Home {
    root: PathBuf,
    bus: Arc<Ledger>,
    management: Arc<Ledger>,
    owner_alive: Arc<AtomicBool>,
}

fn home() -> Home {
    let root = temp_root();
    let owner_alive = Arc::new(AtomicBool::new(true));
    let alive = owner_alive.clone();
    let liveness = move |process: &ProcessIdentity| process.pid != 2 || alive.load(Ordering::SeqCst);
    Home {
        bus: Arc::new(Ledger::new(&root, identity(1), Role::Launcher, liveness.clone())),
        management: Arc::new(Ledger::new(&root, identity(2), Role::Owner, liveness)),
        root,
        owner_alive,
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn stamped(method: &str, emitted: i64) -> AppServerEvent {
    let params = json!({ "threadId": "t", "id": emitted });
    AppServerEvent::Notification(RpcNotification {
        method: method.to_string(),
        params: Some(params.clone()),
        raw: json!({ "method": method, "params": params, "emittedAtMs": emitted }),
    })
}

fn unstamped(method: &str) -> AppServerEvent {
    AppServerEvent::Notification(RpcNotification {
        method: method.to_string(),
        params: Some(json!({})),
        raw: json!({ "method": method, "params": {} }),
    })
}

fn approval(id: i64) -> AppServerEvent {
    let params = json!({ "threadId": "t" });
    AppServerEvent::ServerRequest(RpcServerRequest {
        id: json!(id),
        method: "item/commandExecution/requestApproval".to_string(),
        params: Some(params.clone()),
        raw: json!({ "id": id, "method": "item/commandExecution/requestApproval", "params": params }),
    })
}

/// The launchers' lease cache is at most `CACHE_TTL` old; real launchers
/// handle an event emitted after the cut only once that much time passed.
#[test]
fn stale_entries_of_one_process_are_checked_once() {
    let root = temp_root();
    // The old Management (pid 2) owned three runtimes, then went away.
    let old = Arc::new(Ledger::new(&root, identity(2), Role::Owner, |_: &ProcessIdentity| true));
    for session in ["a", "b", "c"] {
        let mut connection = old.open(session, 1);
        connection.activate_at(1);
        // The process dies: nothing closes the connection, its lease stays behind.
        std::mem::forget(connection);
    }
    let checks = Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
    let seen = checks.clone();
    let new = Arc::new(Ledger::new(&root, identity(3), Role::Owner, move |process: &ProcessIdentity| {
        seen.lock().unwrap().push(process.pid);
        process.pid != 2
    }));
    let mut connection = new.open("d", 1);
    connection.activate_at(2);
    // Three stale leases of one gone process: one liveness check.
    assert_eq!(*checks.lock().unwrap(), vec![2]);
    let file = new.read();
    assert_eq!(file.leases.keys().cloned().collect::<Vec<_>>(), vec!["d".to_string()]);
    drop(connection);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_reused_pid_is_not_the_process_that_wrote_the_lease() {
    let me = ProcessIdentity::current();
    assert!(me.is_live());
    if me.started_at.is_some() {
        let reused = ProcessIdentity {
            pid: me.pid,
            started_at: Some("2000-01-01T00:00:00+00:00".to_string()),
        };
        assert!(!reused.is_live());
    }
}

fn let_launchers_see_the_lease() {
    std::thread::sleep(CACHE_TTL + Duration::from_millis(20));
}

#[test]
fn a_handoff_journals_every_event_exactly_once() {
    let home = home();
    let start = 1_000_000;
    // Stream: a stamped event every 100 ms for 4 s, an approval after
    // every tenth event.
    let mut stream = Vec::new();
    for index in 0..40i64 {
        stream.push((format!("n{index}"), stamped("item/started", start + index * 100)));
        if index % 10 == 5 {
            stream.push((format!("r{index}"), approval(index)));
        }
    }
    let mut bus = home.bus.open("cutex.a", 3);
    bus.activate();
    // The bus handles the first 12 entries before Management connects.
    let mut bus_handled = BTreeSet::new();
    let mut handled_by_bus_at = |bus: &mut ConnectionOwnership, entries: &[(String, AppServerEvent)]| {
        for (label, event) in entries {
            if bus.handles_at(event, start) {
                assert!(bus_handled.insert(label.clone()));
            }
        }
    };
    handled_by_bus_at(&mut bus, &stream[..12]);
    // Management subscribes at start + 1050 (it receives entries emitted
    // after that) and takes over 1.5 s later.
    let adopted_at = start + 1_050;
    let mut management = home.management.open("cutex.a", 3);
    management.activate_at(adopted_at);
    assert_eq!(management.cut(), Some(adopted_at + HANDOFF_MARGIN_MS));
    let subscribed = stream
        .iter()
        .position(|(label, _)| label == "n11")
        .unwrap();
    let mut management_handled = BTreeSet::new();
    for (label, event) in &stream[subscribed..] {
        if management.handles_at(event, adopted_at) {
            assert!(management_handled.insert(label.clone()));
        }
    }
    let_launchers_see_the_lease();
    assert_eq!(bus.cut(), Some(adopted_at + HANDOFF_MARGIN_MS));
    // The bus lags: it handles the rest only now.
    handled_by_bus_at(&mut bus, &stream[12..]);

    let both = bus_handled.intersection(&management_handled).collect::<Vec<_>>();
    assert!(both.is_empty(), "journaled twice: {both:?}");
    let all = stream.iter().map(|(label, _)| label.clone()).collect::<BTreeSet<_>>();
    let union = bus_handled.union(&management_handled).cloned().collect::<BTreeSet<_>>();
    assert_eq!(union, all, "no event may be lost in the handoff window");
    // The cut sits inside the stream: both sides wrote something.
    assert!(management_handled.contains("n39") && bus_handled.contains("n0"));
    assert!(bus_handled.contains("n25") && management_handled.contains("n26"));
    // The approval after n25 follows n25's side, the one after n35 the owner.
    assert!(bus_handled.contains("r25") && management_handled.contains("r35"));
    // Nobody stopped early: nothing to take over.
    assert!(replayed(&mut bus).is_empty() && replayed(&mut management).is_empty());
}

#[test]
fn an_owner_alone_takes_everything_and_a_later_launcher_writes_nothing() {
    let home = home();
    // Management-launched (or adopted with no other writer): no cut.
    let mut management = home.management.open("cutex.m", 1);
    management.activate_at(5_000);
    assert_eq!(management.cut(), Some(i64::MIN));
    assert_eq!(management.split_after(), None);
    assert!(management.handles_at(&stamped("item/started", 4_000), 5_000));
    assert!(management.handles_at(&unstamped("thread/status/changed"), 5_000));
    // The Agent Bus connects to the same runtime later (Agent Management
    // recovery): it observes but journals nothing.
    let mut bus = home.bus.open("cutex.m", 1);
    bus.activate();
    for emitted in [4_000, 6_000, 9_000] {
        assert!(!bus.handles_at(&stamped("item/started", emitted), emitted));
    }
    assert!(!bus.handles_at(&approval(7), 9_000));
    assert!(!bus.handles_at(&AppServerEvent::Disconnected { reason: "closed".into() }, 9_000));
    // Another generation (a relaunch) is not covered by this lease.
    let mut relaunched = home.bus.open("cutex.m", 2);
    relaunched.activate();
    assert!(relaunched.handles_at(&stamped("item/started", 9_000), 9_000));
}

/// Appends journal records as the owner process would have written them.
fn journal(root: &Path, session: &str, events: &[&AppServerEvent]) {
    use std::io::Write;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("events.jsonl"))
        .unwrap();
    for event in events {
        let message = match event {
            AppServerEvent::Notification(notification) => notification.raw.clone(),
            AppServerEvent::ServerRequest(request) => request.raw.clone(),
            _ => unreachable!(),
        };
        let record = json!({ "cutexSessionId": session, "native": { "kind": "x", "message": message } });
        writeln!(file, "{record}").unwrap();
    }
}

fn delta(text: &str, emitted: i64) -> AppServerEvent {
    let params = json!({ "threadId": "t", "turnId": "u", "itemId": "m", "delta": text });
    AppServerEvent::Notification(RpcNotification {
        method: "item/agentMessage/delta".to_string(),
        params: Some(params.clone()),
        raw: json!({ "method": "item/agentMessage/delta", "params": params, "emittedAtMs": emitted }),
    })
}

fn replayed(connection: &mut ConnectionOwnership) -> Vec<i64> {
    connection
        .take_replay()
        .iter()
        .map(|event| match event {
            AppServerEvent::Notification(notification) => notification.raw["emittedAtMs"].as_i64().unwrap(),
            AppServerEvent::ServerRequest(request) => -request.id.as_i64().unwrap(),
            _ => 0,
        })
        .collect()
}

#[test]
fn live_text_deltas_keep_emission_time_without_takeover_payloads() {
    let home = home();
    let mut bus = home.bus.open("cutex.a", 1);
    bus.activate();
    bus.set_full_text_item_terminal(true);
    let mut management = home.management.open("cutex.a", 1);
    management.activate_at(10_000);
    management.set_full_text_item_terminal(true);
    let_launchers_see_the_lease();
    for index in 0..2000 {
        let event = delta(&"x".repeat(1024), 20_000 + index);
        assert!(!bus.handles_at(&event, 20_000 + index));
    }
    assert!(bus.retained.is_empty());
    assert_eq!(bus.retained_bytes, 0);
    assert_eq!(bus.last_emitted, Some(21_999));
    let request = approval(42); // Untimed: must inherit the last live emission.
    assert!(!bus.handles_at(&request, 30_000));
    assert_eq!(bus.retained.front().unwrap().key, 21_999);
    drop(management);
    let_launchers_see_the_lease();
    bus.poll_at(31_000);
    assert_eq!(replayed(&mut bus), vec![-42]);
}

#[test]
fn a_crashed_owner_leaves_no_gap_the_launcher_journals_what_it_missed() {
    let home = home();
    let mut bus = home.bus.open("cutex.a", 1);
    bus.activate();
    let mut management = home.management.open("cutex.a", 1);
    management.activate_at(10_000);
    let cut = 10_000 + HANDOFF_MARGIN_MS;
    let_launchers_see_the_lease();
    // The owner's part of the stream; the bus keeps it while it follows.
    let owned = [
        stamped("item/started", 20_000),
        delta("a", 20_100),
        delta("b", 20_200),
        stamped("item/completed", 20_300),
        delta("c", 20_400),
        stamped("item/started", 20_400),
        approval(9),
        delta("d", 20_500),
    ];
    for event in &owned {
        assert!(!bus.handles_at(event, 20_000));
        assert!(management.handles_at(event, 20_000));
    }
    assert!(replayed(&mut bus).is_empty() && replayed(&mut management).is_empty());
    // Management journaled the first five and was killed before the rest.
    journal(&home.root, "cutex.a", &owned[..5].iter().collect::<Vec<_>>());
    home.owner_alive.store(false, Ordering::SeqCst);
    let_launchers_see_the_lease();
    // The next event (or an idle poll) notices: the bus journals the
    // missing tail first, in order, then carries on writing.
    let next = stamped("turn/completed", 20_600);
    assert!(bus.handles_at(&next, 20_600));
    assert_eq!(replayed(&mut bus), vec![20_400, -9, 20_500]);
    assert!(bus.handles_at(&delta("e", 20_700), 20_700));
    assert_eq!(bus.cut(), None);
    assert!(cut < 20_000);
    std::mem::forget(management);
}

#[test]
fn a_crashed_owner_is_noticed_while_idle() {
    let home = home();
    let mut bus = home.bus.open("cutex.a", 1);
    bus.activate();
    let mut management = home.management.open("cutex.a", 1);
    management.activate_at(10_000);
    let_launchers_see_the_lease();
    let last = stamped("turn/completed", 20_000);
    assert!(!bus.handles_at(&last, 20_000));
    home.owner_alive.store(false, Ordering::SeqCst);
    let_launchers_see_the_lease();
    bus.poll_at(25_000);
    assert_eq!(replayed(&mut bus), vec![20_000], "the owner never journaled it");
    std::mem::forget(management);
}

#[test]
fn a_new_owner_with_a_later_cut_hands_back_the_range_in_between() {
    let home = home();
    let mut bus = home.bus.open("cutex.a", 1);
    bus.activate();
    let mut first = home.management.open("cutex.a", 1);
    first.activate_at(10_000); // cut 11_500
    let_launchers_see_the_lease();
    let lost = stamped("item/started", 12_000);
    let written = stamped("item/started", 12_100);
    assert!(!bus.handles_at(&written, 12_100));
    assert!(!bus.handles_at(&lost, 12_000));
    journal(&home.root, "cutex.a", &[&written]);
    // Management restarted before the bus noticed: a new lease with a new
    // cut (13_500) replaces the dead one.
    std::mem::forget(first);
    let restarted = Arc::new(Ledger::new(&home.root, identity(3), Role::Owner, |_: &ProcessIdentity| true));
    let mut second = restarted.open("cutex.a", 1);
    second.activate_at(12_000);
    let_launchers_see_the_lease();
    assert_eq!(bus.cut(), Some(13_500));
    // Late copies the old owner had journaled are skipped, the rest of
    // (11_500, 13_500] is the bus's.
    assert!(!bus.handles_at(&stamped("item/started", 12_100), 12_100));
    assert_eq!(replayed(&mut bus), vec![12_000]);
    assert!(bus.handles_at(&stamped("item/completed", 13_000), 13_000));
    assert!(!bus.handles_at(&stamped("item/completed", 14_000), 14_000));
}

#[test]
fn a_launcher_gone_before_the_cut_leaves_no_gap() {
    let home = home();
    let bus_alive = Arc::new(AtomicBool::new(true));
    let alive = bus_alive.clone();
    let liveness = move |process: &ProcessIdentity| process.pid != 1 || alive.load(Ordering::SeqCst);
    let bus_ledger = Arc::new(Ledger::new(&home.root, identity(1), Role::Launcher, liveness.clone()));
    let owner_ledger = Arc::new(Ledger::new(&home.root, identity(2), Role::Owner, liveness));
    let mut bus = bus_ledger.open("cutex.a", 1);
    bus.activate();
    let mut management = owner_ledger.open("cutex.a", 1);
    management.activate_at(10_000); // cut 11_500
    let before = [stamped("item/started", 10_500), delta("x", 10_600), stamped("item/completed", 11_000)];
    for event in &before {
        assert!(bus.handles_at(event, 10_500));
        assert!(!management.handles_at(event, 10_500));
    }
    // The bus journaled only the first one, then its process died.
    journal(&home.root, "cutex.a", &[&before[0]]);
    bus_alive.store(false, Ordering::SeqCst);
    std::mem::forget(bus);
    // At the cut Management journals the rest before its own first event.
    assert!(management.handles_at(&stamped("turn/completed", 12_000), 12_000));
    assert_eq!(replayed(&mut management), vec![10_600, 11_000]);
}

#[test]
fn a_live_launcher_keeps_its_part_at_the_cut() {
    let home = home();
    let mut bus = home.bus.open("cutex.a", 1);
    bus.activate();
    let mut management = home.management.open("cutex.a", 1);
    management.activate_at(10_000);
    assert!(!management.handles_at(&stamped("item/started", 10_500), 10_500));
    // Nothing journaled yet: the bus lags, but it is alive and will.
    management.poll_at(12_000);
    assert!(replayed(&mut management).is_empty());
    assert!(bus.handles_at(&stamped("item/started", 10_500), 10_500));
}

#[test]
fn closing_connections_releases_the_lease_and_the_registration() {
    let home = home();
    let mut bus = home.bus.open("cutex.a", 1);
    bus.activate();
    let mut management = home.management.open("cutex.a", 1);
    management.activate_at(10_000);
    let_launchers_see_the_lease();
    assert!(!bus.handles_at(&stamped("item/started", 20_000), 20_000));
    // Management lets go of the runtime (stop, failed connection): the
    // still connected bus writes again, starting with what Management did
    // not journal.
    drop(management);
    let_launchers_see_the_lease();
    assert!(bus.handles_at(&stamped("item/started", 21_000), 21_000));
    assert_eq!(replayed(&mut bus), vec![20_000]);
    // A new adoption with no writer left takes everything.
    drop(bus);
    let mut again = home.management.open("cutex.a", 1);
    again.activate_at(30_000);
    assert_eq!(again.cut(), Some(i64::MIN));
    drop(again);
    assert!(!home.root.join(FILE_NAME).exists(), "nothing left behind");
}

#[test]
fn a_stale_registration_of_a_gone_process_does_not_delay_takeover() {
    let home = home();
    let gone = Arc::new(Ledger::new(&home.root, identity(7), Role::Launcher, |process: &ProcessIdentity| {
        process.pid != 7
    }));
    let bus = gone.open("cutex.a", 1);
    std::mem::forget(bus);
    let live = Arc::new(Ledger::new(&home.root, identity(2), Role::Owner, |process: &ProcessIdentity| {
        process.pid != 7
    }));
    let mut management = live.open("cutex.a", 1);
    management.activate_at(1_000);
    assert_eq!(management.cut(), Some(i64::MIN));
}

#[test]
fn stampless_runtimes_lean_towards_overlap_near_the_cut() {
    let home = home();
    let mut bus = home.bus.open("cutex.c", 1);
    bus.activate();
    let mut management = home.management.open("cutex.c", 1);
    management.activate_at(100_000);
    let cut = 100_000 + HANDOFF_MARGIN_MS;
    let_launchers_see_the_lease();
    let event = unstamped("item/agentMessage/delta");
    // Received by both within the bias window around the cut: both write.
    assert!(bus.handles_at(&event, cut - 100));
    assert!(management.handles_at(&event, cut - 100));
    // Well before / after: exactly one side.
    assert!(bus.handles_at(&event, cut - 1_000) && !management.handles_at(&event, cut - 1_000));
    assert!(!bus.handles_at(&event, cut + 10) && management.handles_at(&event, cut + 10));
    // Connection-local events go by local time only.
    let disconnected = AppServerEvent::Disconnected { reason: "x".into() };
    assert!(!management.handles_at(&disconnected, cut - 100));
    assert!(!bus.handles_at(&disconnected, cut + 1));
}

#[test]
fn a_damaged_lease_file_never_silences_anyone() {
    let home = home();
    fs::write(home.root.join(FILE_NAME), b"{not json").unwrap();
    let mut bus = home.bus.open("cutex.a", 1);
    bus.activate();
    assert!(bus.handles_at(&stamped("item/started", 1), 1));
    let mut management = home.management.open("cutex.a", 1);
    management.activate_at(10);
    // The damaged file was replaced by the bus registration: a handoff.
    assert_eq!(management.cut(), Some(10 + HANDOFF_MARGIN_MS));
}
