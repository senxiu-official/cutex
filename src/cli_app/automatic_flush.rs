// A native event worker must not wait for an input/lifecycle reservation or a
// native RPC: disconnect joins it, and RPC responses share its bounded socket
// event producer. At most one detached
// continuation per session waits instead; manager shutdown never joins it.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Target {
    generation: u64,
    thread: String,
}
struct Pending {
    target: Option<Target>,
    owner: Arc<()>,
}
static PENDING: OnceLock<Mutex<HashMap<String, Pending>>> = OnceLock::new();

fn pending() -> &'static Mutex<HashMap<String, Pending>> {
    PENDING.get_or_init(Default::default)
}

pub(super) fn defer(session: &str, generation: u64, thread: String) -> anyhow::Result<()> {
    let target = Target { generation, thread };
    let mut entries = pending()
        .lock()
        .map_err(|_| anyhow::anyhow!("automatic flush coordination poisoned"))?;
    if let Some(entry) = entries.get_mut(session) {
        if entry
            .target
            .as_ref()
            .is_some_and(|current| current.generation > target.generation)
        {
            return Ok(());
        }
        entry.target = Some(target);
        return Ok(());
    }
    let owner = Arc::new(());
    entries.insert(
        session.into(),
        Pending {
            target: Some(target),
            owner: Arc::clone(&owner),
        },
    );
    let key = session.to_string();
    // Spawn while holding only this tiny metadata lock so a failed spawn can
    // roll back its own entry before any other event coalesces into it.
    if let Err(error) = std::thread::Builder::new()
        .name("input-deferred-flush".into())
        .spawn(move || {
            let _cleanup = WorkerCleanup {
                session: &key,
                owner,
            };
            run(&key);
        })
    {
        entries.remove(session);
        return Err(error.into());
    }
    Ok(())
}

struct WorkerCleanup<'a> {
    session: &'a str,
    owner: Arc<()>,
}
impl Drop for WorkerCleanup<'_> {
    fn drop(&mut self) {
        if let Ok(mut entries) = pending().lock() {
            if entries
                .get(self.session)
                .is_some_and(|entry| Arc::ptr_eq(&entry.owner, &self.owner))
            {
                entries.remove(self.session);
            }
        }
    }
}

fn run(session: &str) {
    loop {
        let target = {
            let Ok(mut entries) = pending().lock() else {
                return;
            };
            match entries
                .get_mut(session)
                .and_then(|entry| entry.target.take())
            {
                Some(target) => target,
                None => {
                    entries.remove(session);
                    return;
                }
            }
        };
        let result = execute(
            session,
            &target,
            || {
                Ok(super::session_manager(session)
                    .status(session)?
                    .filter(|status| status.connected && status.active_turn_id.is_none())
                    .map(|status| Target {
                        generation: status.runtime_generation,
                        thread: status.thread_id,
                    }))
            },
            || super::flush_queued_in_reservation(session, false),
        );
        if let Err(error) = result {
            eprintln!("deferred queued input for {session} was not flushed: {error:#}");
        }
    }
}

fn execute(
    session: &str,
    expected: &Target,
    current: impl FnOnce() -> anyhow::Result<Option<Target>>,
    flush: impl FnOnce() -> anyhow::Result<bool>,
) -> anyhow::Result<bool> {
    let lane = super::session_input_lock(session)?;
    let _reservation = lane.reserve()?;
    // Revalidate *after* admission. An event for a stopped/replaced runtime
    // cannot restart a queue in the next occurrence. No epoch/check/send gap:
    // the execution capability is retained through the native/store result.
    if current()?.as_ref() != Some(expected) {
        return Ok(false);
    }
    flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn event_worker_can_join_while_one_coalesced_continuation_waits_for_lifecycle() {
        let session = format!("cutex.flush-join-{}", uuid::Uuid::new_v4());
        let lane = super::super::session_input_lock(&session).unwrap();
        let lifecycle = lane.reserve().unwrap();
        let key = session.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let event_worker = std::thread::spawn(move || {
            for _ in 0..100 {
                defer(&key, 1, "old-thread".into()).unwrap();
            }
            tx.send(()).unwrap();
        });
        let joined_without_admission = rx.recv_timeout(Duration::from_secs(5));
        let deadline = Instant::now() + Duration::from_secs(5);
        while lane.snapshot().unwrap().waiting == 0 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        let queued = lane.snapshot().unwrap().waiting;
        // Always release before an assertion to avoid stranding background IO.
        drop(lifecycle);
        joined_without_admission.expect("joined event worker waited for lifecycle capability");
        event_worker.join().unwrap();
        assert_eq!(
            queued, 1,
            "events must coalesce rather than spawn a waiter per notification"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while pending().lock().unwrap().contains_key(&session) {
            assert!(
                Instant::now() < deadline,
                "offline continuation failed to drain"
            );
            std::thread::yield_now();
        }
        assert_eq!(lane.snapshot().unwrap().waiting, 0);
    }

    #[test]
    fn event_continuation_cannot_send_into_a_replacement_after_lifecycle_release() {
        let session = format!("cutex.flush-{}", uuid::Uuid::new_v4());
        let lane = super::super::session_input_lock(&session).unwrap();
        let lifecycle = lane.reserve().unwrap();
        let sends = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&sends);
        let key = session.clone();
        let worker = std::thread::spawn(move || {
            execute(
                &key,
                &Target {
                    generation: 1,
                    thread: "old-thread".into(),
                },
                || {
                    Ok(Some(Target {
                        generation: 2,
                        // Revert/restart keeps the native conversation ID.
                        // Generation alone must fence this old callback.
                        thread: "old-thread".into(),
                    }))
                },
                || {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(true)
                },
            )
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while lane.snapshot().unwrap().waiting != 1 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        // Manager's joined event worker has already returned; only this
        // independent continuation waits for lifecycle settlement.
        drop(lifecycle);
        assert!(!worker.join().unwrap().unwrap());
        assert_eq!(sends.load(Ordering::SeqCst), 0);
    }
}
