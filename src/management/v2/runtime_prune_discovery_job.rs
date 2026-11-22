//! Coalesced resident-worker discovery with a wall-clock bound even without a request budget.
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
type Names = Arc<HashSet<String>>;
type Task = Box<dyn FnOnce() + Send>;
type Discover = Arc<dyn Fn(&[PathBuf]) -> anyhow::Result<Names> + Send + Sync>;
struct Job {
    targets: Vec<PathBuf>,
    expires: Instant,
    result: Mutex<Option<Result<Names, String>>>,
}
static SLOT: Mutex<Option<Arc<Job>>> = Mutex::new(None);
pub(super) fn run(targets: &[PathBuf]) -> anyhow::Result<Names> {
    run_in(
        &SLOT,
        targets,
        super::ready_names,
        crate::management::v2::runtimes_api::maintain_runtime_inventory,
        Arc::new(|targets| {
            let _priority = super::BackgroundPriority::enter()?;
            super::collect(targets, super::super::executable_names).map(Arc::new)
        }),
        Duration::from_secs(8),
        Duration::from_secs(1),
    )
}
fn run_in(
    slot: &Mutex<Option<Arc<Job>>>,
    targets: &[PathBuf],
    ready: impl Fn(&[PathBuf]) -> anyhow::Result<Option<Names>>,
    queue: impl Fn(Task) -> bool,
    discover: Discover,
    run_limit: Duration,
    margin: Duration,
) -> anyhow::Result<Names> {
    let caller_expires = Instant::now() + run_limit + margin;
    let mut key = targets.to_vec();
    key.sort();
    key.dedup();
    loop {
        anyhow::ensure!(
            Instant::now() < caller_expires,
            "filename discovery wait deadline"
        );
        if let Some(names) = ready(&key)? {
            return Ok(names);
        }
        let job = {
            let mut current = super::lock(slot)?;
            let active = current
                .as_ref()
                .filter(|job| {
                    Instant::now() < job.expires
                        && job.result.lock().is_ok_and(|result| result.is_none())
                })
                .cloned();
            if let Some(active) = active {
                active
            } else {
                let deadline = Instant::now() + run_limit;
                let job = Arc::new(Job {
                    targets: key.clone(),
                    expires: deadline + margin,
                    result: Mutex::new(None),
                });
                let worker = job.clone();
                let discover = discover.clone();
                let discover = discover.clone();
                let queued = queue(Box::new(move || {
                    // A task panic must not kill the resident worker or leave waiters pending.
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        anyhow::ensure!(!remaining.is_zero(), "discovery queue deadline");
                        let _budget =
                            crate::management::v2::runtime_prune_inventory::budget::Scope::enter(
                                remaining,
                                Default::default(),
                            );
                        let names = discover(&worker.targets)?;
                        crate::management::v2::runtime_prune_inventory::budget::check()?;
                        Ok(names)
                    }))
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("discovery worker task panicked")))
                    .map_err(|_| "filename discovery unavailable".to_owned());
                    *worker
                        .result
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) = Some(result);
                }));
                anyhow::ensure!(queued, "filename discovery worker busy");
                *current = Some(job.clone());
                job
            }
        };
        let _waiting =
            crate::management::v2::runtime_prune_inventory::timing::stage("filename_wait");
        loop {
            if Instant::now() >= caller_expires || Instant::now() >= job.expires {
                if Instant::now() >= job.expires {
                    let mut result = job.result.lock().unwrap_or_else(|error| error.into_inner());
                    if result.is_none() {
                        *result = Some(Err("filename discovery wait deadline".into()));
                    }
                }
                anyhow::bail!("filename discovery wait deadline");
            }
            crate::management::v2::runtime_prune_inventory::budget::check()?;
            if let Some(result) = super::lock(&job.result)?.clone() {
                if job.targets == key {
                    return result.map_err(anyhow::Error::msg);
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn panic_and_recover(with_request_budget: bool) {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (sender, receiver) = std::sync::mpsc::channel::<Task>();
        let thread = std::thread::spawn(move || {
            while let Ok(task) = receiver.recv() {
                task();
            }
        });
        let slot = Mutex::new(None);
        let panic_once = Arc::new(AtomicBool::new(true));
        let discover: Discover = Arc::new(move |_| {
            if panic_once.swap(false, Ordering::SeqCst) {
                panic!("fixture task panic");
            }
            Ok(Arc::new(HashSet::from(["known.exe".into()])))
        });
        let budget = with_request_budget.then(|| {
            crate::management::v2::runtime_prune_inventory::budget::Scope::enter(
                Duration::from_secs(2),
                Default::default(),
            )
        });
        let run = || {
            run_in(
                &slot,
                &["fixture-root".into()],
                |_| Ok(None),
                |task| sender.send(task).is_ok(),
                discover.clone(),
                Duration::from_millis(100),
                Duration::from_millis(20),
            )
        };
        assert!(run().is_err());
        assert!(run().unwrap().contains("known.exe"));
        drop(budget);
        drop(sender);
        thread.join().unwrap();
    }
    #[test]
    fn task_panic_during_get_fills_result_and_slot_recovers() {
        panic_and_recover(true);
    }
    #[test]
    fn task_panic_during_post_without_budget_fills_result_and_slot_recovers() {
        panic_and_recover(false);
    }
    #[test]
    fn lost_worker_is_bounded_without_budget_and_expired_slot_is_replaceable() {
        let slot = Mutex::new(None);
        let began = Instant::now();
        assert!(run_in(
            &slot,
            &["fixture-root".into()],
            |_| Ok(None),
            |_task| true,
            Arc::new(|_| unreachable!()),
            Duration::from_millis(20),
            Duration::from_millis(10)
        )
        .is_err());
        assert!(began.elapsed() < Duration::from_millis(500));
        // Model a worker that died outside its task before ever publishing a result.
        *slot.lock().unwrap() = Some(Arc::new(Job {
            targets: vec!["fixture-root".into()],
            expires: Instant::now() - Duration::from_secs(1),
            result: Mutex::new(None),
        }));
        let result = run_in(
            &slot,
            &["fixture-root".into()],
            |_| Ok(None),
            |task| {
                task();
                true
            },
            Arc::new(|_| Ok(Arc::new(HashSet::from(["recovered.exe".into()])))),
            Duration::from_millis(100),
            Duration::from_millis(20),
        )
        .unwrap();
        assert!(result.contains("recovered.exe"));
    }
}
