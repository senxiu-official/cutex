use super::super::{
    model::*,
    repository::{EventRepository, ReplayQuery},
};
use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

fn pending(session: &str, n: usize) -> PendingEvent {
    PendingEvent {
        cutex_session_id: session.into(),
        host_id: "fixture".into(),
        source: EventSource::Cutex,
        schema: None,
        correlation: EventCorrelation::default(),
        native: None,
        cutex: Some(CutexMessage {
            method: "cutex/test/observation".into(),
            params: serde_json::json!({"n":n}),
        }),
    }
}

fn fixture() -> (
    PathBuf,
    Arc<EventRepository>,
    Arc<AtomicBool>,
    RuntimeJournal,
) {
    let root = std::env::temp_dir().join(format!("cx-journal-{}", uuid::Uuid::new_v4()));
    let repository = Arc::new(EventRepository::open(&root, "fixture").unwrap());
    let blocked = Arc::new(AtomicBool::new(false));
    let repo = repository.clone();
    let fail = blocked.clone();
    let journal = RuntimeJournal::new(root.join("events.jsonl"), move |p| {
        if fail.load(Ordering::SeqCst) {
            return Err(std::io::Error::from_raw_os_error(225).into());
        }
        repo.append(p).map(Some)
    });
    (root, repository, blocked, journal)
}

#[test]
fn journal_failure_mid_turn_preserves_order_and_defers_durable_effects_until_recovery() {
    let (root, repository, blocked, journal) = fixture();
    let completed = Arc::new(Mutex::new(Vec::new()));
    let offer = |session, n| {
        let completed = completed.clone();
        journal
            .submit(
                pending(session, n),
                1024,
                Box::new(move |event| {
                    let event = event.expect("only durable envelopes reach callback");
                    completed.lock().unwrap().push((
                        event.sequence,
                        event.cutex.unwrap().params["n"].as_u64().unwrap(),
                    ));
                    Ok(())
                }),
            )
            .unwrap()
    };
    assert_eq!(offer("a", 0), Acceptance::Durable);
    let checkpoint = repository.checkpoint().unwrap();
    blocked.store(true, Ordering::SeqCst);
    assert_eq!(offer("a", 1), Acceptance::Buffered);
    assert_eq!(offer("b", 2), Acceptance::Buffered);
    journal.tick();
    assert_eq!(
        repository.checkpoint().unwrap(),
        checkpoint,
        "RAM acceptance cannot advance durable cursor"
    );
    assert_eq!(*completed.lock().unwrap(), vec![(1, 0)]);
    let health = journal.health().unwrap();
    assert_eq!(health["condition"], "event_journal_blocked");
    assert_eq!(health["osError"], 225);
    assert_eq!(health["bufferedEvents"], 2);
    assert_eq!(health["bufferIsDurable"], false);
    assert!(health["file"].as_str().unwrap().ends_with("events.jsonl"));
    blocked.store(false, Ordering::SeqCst);
    journal.tick();
    assert!(journal.health().is_none());
    assert_eq!(*completed.lock().unwrap(), vec![(1, 0), (2, 1), (3, 2)]);
    let page = repository.page(ReplayQuery::default()).unwrap();
    assert_eq!(page.events.len(), 3);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn journal_healthy_io_remains_parallel_and_failed_inflight_entries_keep_admission_order() {
    let root = std::env::temp_dir().join(format!("cx-journal-parallel-{}", uuid::Uuid::new_v4()));
    let repository = Arc::new(EventRepository::open(&root, "fixture").unwrap());
    let blocked = Arc::new(AtomicBool::new(true));
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_a, wait_a) = std::sync::mpsc::channel();
    let (release_b, wait_b) = std::sync::mpsc::channel();
    let gates = [Mutex::new(wait_a), Mutex::new(wait_b)];
    let repo = repository.clone();
    let fail = blocked.clone();
    let journal = Arc::new(RuntimeJournal::new(
        root.join("events.jsonl"),
        move |pending| {
            if fail.load(Ordering::SeqCst) {
                let n = pending.cutex.as_ref().unwrap().params["n"]
                    .as_u64()
                    .unwrap() as usize;
                entered_tx.send(n).unwrap();
                gates[n]
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
                return Err(std::io::Error::from_raw_os_error(225).into());
            }
            repo.append(pending).map(Some)
        },
    ));
    let completed = Arc::new(Mutex::new(Vec::new()));
    let offer = |n| {
        let journal = journal.clone();
        let completed = completed.clone();
        std::thread::spawn(move || {
            journal
                .submit(
                    pending("a", n),
                    1024,
                    Box::new(move |event| {
                        completed
                            .lock()
                            .unwrap()
                            .push(event.unwrap().cutex.unwrap().params["n"].as_u64().unwrap());
                        Ok(())
                    }),
                )
                .unwrap()
        })
    };
    let a = offer(0);
    assert_eq!(entered_rx.recv_timeout(Duration::from_secs(2)).unwrap(), 0);
    let b = offer(1);
    let b_entered = entered_rx.recv_timeout(Duration::from_secs(2));
    // Even a regression must release its first worker rather than hang the suite.
    release_a.send(()).unwrap();
    assert_eq!(a.join().unwrap(), Acceptance::Buffered);
    assert!(
        b_entered.is_ok(),
        "healthy appends must overlap so repository group commit still works"
    );
    let done = completed.clone();
    assert_eq!(
        journal
            .submit(
                pending("a", 2),
                1024,
                Box::new(move |event| {
                    done.lock()
                        .unwrap()
                        .push(event.unwrap().cutex.unwrap().params["n"].as_u64().unwrap());
                    Ok(())
                })
            )
            .unwrap(),
        Acceptance::Buffered
    );
    assert!(
        !journal.tick(),
        "do not replay around an unsettled healthy append"
    );
    release_b.send(()).unwrap();
    assert_eq!(b.join().unwrap(), Acceptance::Buffered);
    blocked.store(false, Ordering::SeqCst);
    journal.tick();
    assert_eq!(
        *completed.lock().unwrap(),
        vec![0, 1, 2],
        "a late failure belongs before newer buffered observations"
    );
    assert!(journal.health().is_none());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn journal_overflow_is_bounded_reported_and_never_acknowledged_as_durable() {
    let (root, _, blocked, journal) = fixture();
    blocked.store(true, Ordering::SeqCst);
    let callbacks = Arc::new(AtomicUsize::new(0));
    for n in 0..MAX_EVENTS + 3 {
        let count = callbacks.clone();
        let result = journal
            .submit(
                pending("a", n),
                1,
                Box::new(move |_| {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }),
            )
            .unwrap();
        assert_eq!(
            result,
            if n < MAX_EVENTS {
                Acceptance::Buffered
            } else {
                Acceptance::Omitted
            }
        );
    }
    let health = journal.health().unwrap();
    assert_eq!(health["bufferedEvents"], MAX_EVENTS);
    assert_eq!(health["omittedEvents"], 3);
    assert_eq!(callbacks.load(Ordering::SeqCst), 0);
    blocked.store(false, Ordering::SeqCst);
    assert!(
        journal.tick(),
        "a recovered backlog schedules another batch immediately"
    );
    for _ in 0..16 {
        journal.tick();
    }
    assert_eq!(callbacks.load(Ordering::SeqCst), MAX_EVENTS);
    assert_eq!(
        journal.health().unwrap()["condition"],
        "event_journal_observations_lost"
    );
    assert!(
        journal.state.lock().unwrap().loss_notice.is_none(),
        "loss/resync notice must be durable before clearing outage"
    );
    // Byte capacity applies independently of the count capacity.
    blocked.store(true, Ordering::SeqCst);
    assert_eq!(
        journal
            .submit(
                pending("a", 999),
                MAX_BYTES + 1,
                Box::new(|_| panic!("omitted callback"))
            )
            .unwrap(),
        Acceptance::Omitted
    );
    assert_eq!(journal.health().unwrap()["bufferedBytes"], 0);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn journal_ram_backlog_is_not_replayed_or_acknowledged_after_process_loss() {
    let (root, repository, blocked, journal) = fixture();
    blocked.store(true, Ordering::SeqCst);
    assert_eq!(
        journal
            .submit(pending("a", 1), 100, Box::new(|_| panic!("not durable")))
            .unwrap(),
        Acceptance::Buffered
    );
    drop(journal);
    let reopened = EventRepository::open(&root, "fixture").unwrap();
    assert_eq!(
        reopened.checkpoint().unwrap(),
        repository.checkpoint().unwrap()
    );
    assert!(reopened
        .page(ReplayQuery::default())
        .unwrap()
        .events
        .is_empty());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn journal_blocked_classification_keeps_io_causes_and_permission_denial_distinct() {
    for io in [
        std::io::Error::from_raw_os_error(225),
        std::io::Error::from_raw_os_error(226),
        std::io::Error::new(std::io::ErrorKind::PermissionDenied, "AV denied"),
    ] {
        let error = anyhow::Error::new(io).context("journal append failed");
        assert!(blocked_io(&error).is_some());
    }
    assert!(blocked_io(&anyhow::anyhow!("generic disk unavailable")).is_none());
}

#[cfg(windows)]
#[test]
fn journal_actual_windows_access_denied_preserves_commit_error_and_retries_without_disconnect() {
    let (root, repository, _, _) = fixture();
    repository.append(pending("a", 0)).unwrap();
    let checkpoint = repository.checkpoint().unwrap();
    let path = root.join("events.jsonl");
    let original = std::fs::metadata(&path).unwrap().permissions();
    struct Restore(PathBuf, std::fs::Permissions);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, self.1.clone());
        }
    }
    let restore = Restore(path.clone(), original.clone());
    let mut readonly = original;
    readonly.set_readonly(true);
    std::fs::set_permissions(&path, readonly).unwrap();
    let error = repository.append(pending("a", 1)).unwrap_err();
    assert_eq!(
        blocked_io(&error),
        Some(5),
        "failed group commit must retain original IO cause"
    );
    assert_eq!(repository.checkpoint().unwrap(), checkpoint);
    let repo = repository.clone();
    let journal = RuntimeJournal::new(path, move |p| repo.append(p).map(Some));
    let count = Arc::new(AtomicUsize::new(0));
    let completed = count.clone();
    assert_eq!(
        journal
            .submit(
                pending("a", 1),
                1024,
                Box::new(move |_| {
                    completed.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            )
            .unwrap(),
        Acceptance::Buffered
    );
    assert_eq!(
        journal.health().unwrap()["condition"],
        "event_journal_blocked"
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    drop(restore);
    journal.tick();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(
        repository.checkpoint().unwrap().sequence,
        checkpoint.sequence + 1
    );
    assert!(journal.health().is_none());
    std::fs::remove_dir_all(root).unwrap();
}
