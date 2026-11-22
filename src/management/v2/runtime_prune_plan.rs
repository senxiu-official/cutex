//! One bounded planner per process. Slow I/O can outlive its caller, never its capacity permit.
use anyhow::ensure;
use serde_json::Value;
use std::{
    net::TcpStream,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};
pub const DEADLINE: Duration = Duration::from_secs(3);
pub struct Job {
    pub trace: Arc<super::inventory::timing::Trace>,
    pub cancel: Arc<AtomicBool>,
    result: Mutex<Option<Result<Value, String>>>,
    started: Instant,
    key: PathBuf,
}
static JOB: OnceLock<Mutex<Option<Arc<Job>>>> = OnceLock::new();
pub fn start(
    key: PathBuf,
    compute: impl FnOnce(Arc<AtomicBool>) -> anyhow::Result<Value> + Send + 'static,
) -> anyhow::Result<Arc<Job>> {
    start_in(JOB.get_or_init(|| Mutex::new(None)), key, compute)
}
fn start_in(
    slot: &Mutex<Option<Arc<Job>>>,
    key: PathBuf,
    compute: impl FnOnce(Arc<AtomicBool>) -> anyhow::Result<Value> + Send + 'static,
) -> anyhow::Result<Arc<Job>> {
    let mut current = slot
        .lock()
        .map_err(|_| anyhow::anyhow!("planner unavailable"))?;
    if let Some(job) = current.as_ref() {
        let done = job
            .result
            .lock()
            .map_err(|_| anyhow::anyhow!("planner unavailable"))?
            .is_some();
        ensure!(done, "runtime inspection already in progress");
        if job.key == key && job.started.elapsed() < Duration::from_secs(1) {
            return Ok(job.clone());
        }
    }
    let job = Arc::new(Job {
        trace: super::inventory::timing::current(),
        cancel: Arc::new(AtomicBool::new(false)),
        result: Mutex::new(None),
        started: Instant::now(),
        key,
    });
    let worker = job.clone();
    std::thread::Builder::new()
        .name("runtime-prune-plan".into())
        .spawn(move || {
            let _timing = super::inventory::timing::Scope::enter(worker.trace.clone());
            let result = compute(worker.cancel.clone())
                .map_err(|_| "runtime inspection unavailable".to_owned());
            let result = if worker.cancel.load(Ordering::Acquire) {
                Err("runtime inspection cancelled".into())
            } else {
                result
            };
            if let Ok(mut slot) = worker.result.lock() {
                *slot = Some(result);
            }
        })?;
    *current = Some(job.clone());
    Ok(job)
}
fn peer_reset(stream: &TcpStream) -> bool {
    if stream.set_nonblocking(true).is_err() {
        return true;
    }
    let result = stream.peek(&mut [0]);
    let _ = stream.set_nonblocking(false);
    // EOF alone may be a legal HTTP write-half close; do not corrupt the response
    // with probe bytes. FIN-only abandonment is bounded by the same deadline.
    matches!(result,Err(error) if matches!(error.kind(),std::io::ErrorKind::ConnectionReset|std::io::ErrorKind::ConnectionAborted|std::io::ErrorKind::BrokenPipe|std::io::ErrorKind::NotConnected))
}
pub fn wait(stream: &TcpStream, job: &Arc<Job>, duration: Duration) -> anyhow::Result<Value> {
    let deadline = Instant::now() + duration;
    loop {
        if peer_reset(stream) || Instant::now() >= deadline {
            job.cancel.store(true, Ordering::Release);
            anyhow::bail!("runtime inspection cancelled or deadline exceeded");
        }
        if let Some(result) = job
            .result
            .lock()
            .map_err(|_| anyhow::anyhow!("planner unavailable"))?
            .clone()
        {
            return result.map_err(anyhow::Error::msg);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn abandoned_request_releases_capacity_while_single_worker_is_still_blocked() {
        let slot = Mutex::new(None);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let job = start_in(&slot, "private-fixture".into(), move |_| {
            release_rx.recv().unwrap();
            done_tx.send(()).unwrap();
            Ok(serde_json::json!({"versions":[]}))
        })
        .unwrap();
        assert!(start_in(&slot, "private-fixture".into(), |_| panic!(
            "must not start a second worker"
        ))
        .is_err());
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let before = crate::management::v2::control_capacity::snapshot()["heavyReadsActive"]
            .as_u64()
            .unwrap();
        let waiting = job.clone();
        let server = std::thread::spawn(move || {
            let permit = crate::management::v2::control_capacity::admit(
                crate::management::v2::control_capacity::WorkClass::HeavyRead,
            )
            .unwrap();
            let _scope = crate::management::v2::control_capacity::response_permit_scope(permit);
            let start = Instant::now();
            let result = wait(&stream, &waiting, Duration::from_millis(80));
            assert!(result.is_err());
            start.elapsed()
        });
        drop(client); // FIN-only abandon: never wait on the filesystem worker.
        let elapsed = server.join().unwrap();
        assert!(elapsed < Duration::from_secs(1));
        assert!(job.cancel.load(Ordering::Acquire));
        assert_eq!(
            crate::management::v2::control_capacity::snapshot()["heavyReadsActive"].as_u64(),
            Some(before)
        );
        assert!(
            done_rx.try_recv().is_err(),
            "capacity must be released BEFORE slow computation finishes"
        );
        release_tx.send(()).unwrap();
        done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        println!("PRUNE_ABANDON_RELEASE seconds={:.6} active_before={} active_after={} worker_still_blocked=true",elapsed.as_secs_f64(),before,crate::management::v2::control_capacity::snapshot()["heavyReadsActive"]);
    }
    #[test]
    fn write_half_closed_client_still_gets_a_valid_json_response() {
        use std::io::Read;
        let slot = Mutex::new(None);
        let job = start_in(&slot, "half-close".into(), |_| {
            Ok(serde_json::json!({"versions":[]}))
        })
        .unwrap();
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let result = wait(&stream, &job, Duration::from_secs(1)).unwrap();
        crate::http::server::write_json_response(&mut stream, 200, "OK", &result).unwrap();
        drop(stream);
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "no probe bytes may corrupt headers"
        );
    }
}
