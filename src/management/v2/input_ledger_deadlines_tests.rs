use super::*;
use crate::management::v2::input_ledger::{
    Evidence, Expect, InputIntent, InputKind, InputSender, InputState, NewInput, Next, SenderKind,
    Timing,
};

fn seed(ledger: &InputLedger, session: &str, deadline: chrono::DateTime<Utc>) {
    let id = "receipt";
    let input = NewInput {
        kind: InputKind::AgentMessage,
        sender: InputSender {
            kind: SenderKind::Agent,
            id: "sender".into(),
            name: "Sender".into(),
            host: None,
            cutex_session_id: None,
        },
        intent: InputIntent {
            strategy: None,
            delivery_mode: None,
            timing: Timing::AfterTurn,
        },
        preview: "test receipt".into(),
        ask_id: None,
        remote: None,
    };
    ledger
        .transition(
            session,
            id,
            Expect::ANY,
            Next::waiting("passive").create(input),
        )
        .unwrap();
    ledger
        .transition(session, id, Expect::ANY, Next::sending())
        .unwrap();
    ledger
        .transition(
            session,
            id,
            Expect::ANY,
            Next::accepted().turn("turn").deadline(Some(deadline)),
        )
        .unwrap();
}

#[test]
fn blocked_session_cannot_starve_due_receipt_or_overwrite_new_revision() {
    let root =
        std::env::temp_dir().join(format!("cutex-deadline-isolation-{}", uuid::Uuid::new_v4()));
    let ledger = Arc::new(InputLedger::open(root.join(il::DATABASE_FILE)));
    let blocked_due = Utc::now() - chrono::Duration::seconds(1);
    seed(&ledger, "blocked", blocked_due);

    let (entered, wait_entered) = mpsc::sync_channel(1);
    let (release, wait_release) = mpsc::sync_channel(1);
    let wait_release = Mutex::new(wait_release);
    let (complete, wait_complete) = mpsc::sync_channel(4);
    let worker_ledger = ledger.clone();
    let mut pool = Pool::start(move |due| {
        let result = il::run_deadline_item(&worker_ledger, due, Utc::now(), |_| {
            if due.cutex_session_id == "blocked" {
                entered.send(()).unwrap();
                wait_release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap();
            }
            Some(Next::uncertain("turnInterrupted"))
        })
        .unwrap();
        complete
            .send((due.cutex_session_id.clone(), result.is_some()))
            .unwrap();
    })
    .unwrap();

    pool.schedule(&ledger).unwrap();
    wait_entered.recv_timeout(Duration::from_secs(2)).unwrap();
    let ready_at = Utc::now();
    seed(&ledger, "healthy", ready_at);
    pool.schedule(&ledger).unwrap();
    assert_eq!(
        wait_complete.recv_timeout(Duration::from_secs(2)).unwrap(),
        ("healthy".to_string(), true)
    );
    let read = |session: &str| {
        ledger
            .get(session, &["receipt".into()])
            .unwrap()
            .records
            .pop()
            .unwrap()
    };
    let healthy = read("healthy");
    assert_eq!(healthy.state, InputState::Uncertain);
    let settled_at = chrono::DateTime::parse_from_rfc3339(&healthy.updated_at)
        .unwrap()
        .with_timezone(&Utc);
    assert!((0..=5_000).contains(&(settled_at.timestamp_millis() - ready_at.timestamp_millis())));

    let blocked = read("blocked");
    assert_eq!(blocked.state, InputState::Accepted);
    let newer = ledger
        .transition(
            "blocked",
            "receipt",
            Expect::rev(blocked.rev),
            Next::in_context(Evidence::ContextPersisted),
        )
        .unwrap()
        .record;
    release.send(()).unwrap();
    assert_eq!(
        wait_complete.recv_timeout(Duration::from_secs(2)).unwrap(),
        ("blocked".to_string(), false)
    );
    let final_row = read("blocked");
    assert_eq!(final_row.state, InputState::InContext);
    assert_eq!(final_row.rev, newer.rev);
    assert_eq!(final_row.evidence, Some(Evidence::ContextPersisted));

    pool.stop();
    drop(ledger);
    std::fs::remove_dir_all(root).unwrap();
}
