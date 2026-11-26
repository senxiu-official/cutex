//! Receipt rechecks are independent of event delivery and maintenance.
use std::collections::BTreeSet;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;

use super::{deadline_decision, il, DeadlineRuntime, InputLedger, LiveOwnerStores, LiveRuntime};
use il::DueInput;

const WORKERS: usize = 4;
const POLL: Duration = Duration::from_millis(200);
const NATIVE_BUDGET: Duration = Duration::from_secs(2);
// Reserve time for the revision CAS and event publication within §5.2.
const LAST_NATIVE_MS: i64 = 4_000;

struct Pool {
    jobs: mpsc::SyncSender<DueInput>,
    finished: mpsc::Receiver<String>,
    active: BTreeSet<String>,
    #[cfg(test)]
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Pool {
    fn start(run: impl Fn(&DueInput) + Send + Sync + 'static) -> std::io::Result<Self> {
        let (jobs, requests) = mpsc::sync_channel::<DueInput>(WORKERS);
        let requests = Arc::new(Mutex::new(requests));
        let (completed, finished) = mpsc::sync_channel(WORKERS);
        let run = Arc::new(run);
        #[cfg(test)]
        let mut threads = Vec::new();
        for _ in 0..WORKERS {
            let (requests, completed, run) = (requests.clone(), completed.clone(), run.clone());
            let _thread = std::thread::Builder::new()
                .name("cutex-input-recheck".into())
                .spawn(move || loop {
                    let job = {
                        let receiver = requests
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        receiver.recv()
                    };
                    let Ok(job) = job else { break };
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&job))).is_err()
                    {
                        eprintln!("cutex input ledger: deadline worker panicked");
                    }
                    if completed.send(job.cutex_session_id).is_err() {
                        break;
                    }
                })?;
            #[cfg(test)]
            threads.push(_thread);
        }
        Ok(Self {
            jobs,
            finished,
            active: BTreeSet::new(),
            #[cfg(test)]
            threads,
        })
    }

    fn schedule(&mut self, ledger: &InputLedger) -> Result<(), il::LedgerError> {
        self.finish_ready();
        while self.active.len() < WORKERS {
            let exclude: Vec<_> = self.active.iter().map(String::as_str).collect();
            let Some(job) = ledger.due_excluding(Utc::now(), 1, &exclude)?.pop() else {
                break;
            };
            let session = job.cutex_session_id.clone();
            if self.jobs.try_send(job).is_err() {
                break;
            }
            self.active.insert(session);
        }
        Ok(())
    }

    fn finish_ready(&mut self) {
        for session in self.finished.try_iter() {
            self.active.remove(&session);
        }
    }

    fn wait(&mut self) {
        if let Ok(session) = self.finished.recv_timeout(POLL) {
            self.active.remove(&session);
        }
    }

    #[cfg(test)]
    fn stop(self) {
        drop(self.jobs);
        drop(self.finished);
        for thread in self.threads {
            thread.join().unwrap();
        }
    }
}

fn settle(ledger: &InputLedger, runtime: LiveRuntime, due: &DueInput) {
    let start = Instant::now();
    let now = Utc::now();
    let overdue = now
        .timestamp_millis()
        .saturating_sub(due.deadline_ms)
        .max(0);
    let remaining = Duration::from_millis(LAST_NATIVE_MS.saturating_sub(overdue).max(0) as u64);
    let bounded = DeadlineRuntime {
        runtime,
        until: start + remaining.min(NATIVE_BUDGET),
    };
    let result = il::run_deadline_item(ledger, due, now, |record| {
        deadline_decision(record, &bounded, &LiveOwnerStores, now)
    });
    let decision_ms = start.elapsed().as_millis();
    match result {
        Ok(Some(transition)) => {
            if let Err(error) = il::publish(&transition) {
                eprintln!("cutex input ledger (settle): publish: {error:#}");
            }
        }
        Ok(None) => {}
        Err(error) => eprintln!("cutex input ledger (settle): deadlines: {error}"),
    }
    let total_ms = start.elapsed().as_millis();
    if overdue >= 500 || total_ms >= 500 {
        eprintln!("cutex input ledger (settle): deadline overdue_ms={overdue} apply_ms={decision_ms} total_ms={total_ms}");
    }
}

pub(super) fn start(runtime: LiveRuntime) -> std::io::Result<()> {
    let mut pool = Pool::start(move |due| {
        if il::enabled() {
            if let Ok(ledger) = il::default_ledger() {
                settle(ledger, runtime, due);
            }
        }
    })?;
    std::thread::Builder::new()
        .name("cutex-input-deadlines".into())
        .spawn(move || loop {
            if il::enabled() {
                if let Ok(ledger) = il::default_ledger() {
                    if let Err(error) = pool.schedule(ledger) {
                        eprintln!("cutex input ledger (settle): schedule: {error}");
                    }
                }
            }
            pool.wait();
        })
        .map(|_| ())
}

#[cfg(test)]
#[path = "input_ledger_deadlines_tests.rs"]
mod tests;
