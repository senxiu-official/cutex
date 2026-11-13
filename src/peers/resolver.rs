//! Bounded bridge to the OS resolver. A timed-out libc lookup cannot be
//! cancelled, so abandoned callers must not create an unlimited thread fleet.
use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::collections::HashMap;
use std::sync::{mpsc, Arc, Condvar, Mutex, OnceLock};
use std::time::Instant;

type Lookup = dyn Fn(String, u16) -> io::Result<Vec<SocketAddr>> + Send + Sync;
struct Job {
    host: String,
    port: u16,
    deadline: Instant,
    flight: Arc<Flight>,
}
type Outcome = Result<Vec<IpAddr>, (io::ErrorKind, String)>;
#[derive(Default)]
struct Flight { result: Mutex<Option<Outcome>>, ready: Condvar }
type Flights = Arc<Mutex<HashMap<String, Arc<Flight>>>>;
pub(super) struct Resolver {
    jobs: mpsc::SyncSender<Job>,
    flights: Flights,
}
impl Resolver {
    fn new(workers: usize, capacity: usize, lookup: Arc<Lookup>) -> io::Result<Self> {
        let (jobs, receiver) = mpsc::sync_channel::<Job>(capacity);
        let receiver = Arc::new(Mutex::new(receiver));
        let flights: Flights = Arc::default();
        for _ in 0..workers {
            let (receiver, lookup, flights) = (receiver.clone(), lookup.clone(), flights.clone());
            std::thread::Builder::new().name("cutex-peer-dns".into()).spawn(move || loop {
                let job = receiver.lock().unwrap_or_else(|e| e.into_inner()).recv();
                let Ok(job) = job else { break; };
                let result = if Instant::now() >= job.deadline {
                    Err(io::Error::new(io::ErrorKind::TimedOut, "peer DNS deadline expired before lookup"))
                } else { lookup(job.host.clone(), job.port) };
                let result = result.map(|addresses| addresses.into_iter().map(|address| address.ip()).collect())
                    .map_err(|error| (error.kind(), error.to_string()));
                *job.flight.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
                job.flight.ready.notify_all();
                let mut flights = flights.lock().unwrap_or_else(|e| e.into_inner());
                if flights.get(&job.host).is_some_and(|flight| Arc::ptr_eq(flight, &job.flight)) {
                    flights.remove(&job.host);
                }
            })?;
        }
        Ok(Self { jobs, flights })
    }

    pub(super) fn resolve(&self, host: &str, port: u16, deadline: Instant) -> io::Result<Vec<SocketAddr>> {
        deadline.checked_duration_since(Instant::now()).filter(|left| !left.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "peer DNS deadline expired"))?;
        let host = host.to_ascii_lowercase();
        let flight = {
            let mut flights = self.flights.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(flight) = flights.get(&host) { flight.clone() } else {
                let flight = Arc::new(Flight::default());
                self.jobs.try_send(Job { host: host.clone(), port, deadline, flight: flight.clone() })
                    .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "peer DNS capacity exhausted"))?;
                flights.insert(host, flight.clone());
                flight
            }
        };
        let mut result = flight.result.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(result) = result.as_ref() {
                return match result {
                    Ok(addresses) => Ok(addresses.iter().map(|ip| SocketAddr::new(*ip, port)).collect()),
                    Err((kind, message)) => Err(io::Error::new(*kind, message.clone())),
                };
            }
            let left = deadline.checked_duration_since(Instant::now()).filter(|left| !left.is_zero())
                .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "peer DNS deadline expired"))?;
            result = flight.ready.wait_timeout(result, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

pub(super) fn resolve(host: &str, port: u16, deadline: Instant) -> io::Result<Vec<SocketAddr>> {
    static RESOLVER: OnceLock<Option<Resolver>> = OnceLock::new();
    RESOLVER.get_or_init(|| Resolver::new(4, 16, Arc::new(|host, port| (host.as_str(), port).to_socket_addrs().map(|addresses| addresses.collect()))).ok())
        .as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::Other, "peer DNS workers unavailable"))?
        .resolve(host, port, deadline)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn a_hung_hostname_uses_one_worker_and_leaves_other_hosts_usable() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let (release, released) = mpsc::sync_channel(1);
        let released = Mutex::new(released);
        let pool = Arc::new(Resolver::new(2, 4, Arc::new(move |host, port| {
            if host == "hung.invalid" {
                observed.fetch_add(1, Ordering::SeqCst);
                released.lock().unwrap().recv_timeout(Duration::from_secs(3)).unwrap();
            }
            Ok(vec![SocketAddr::from(([127, 0, 0, 1], port))])
        })).unwrap());
        let waiters: Vec<_> = (0..12).map(|n| {
            let pool = pool.clone();
            std::thread::spawn(move || pool.resolve("HUNG.invalid", 8000 + n, Instant::now() + Duration::from_millis(100)))
        }).collect();
        for waiter in waiters { assert!(waiter.join().unwrap().is_err()); }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(pool.resolve("healthy.invalid", 99, Instant::now() + Duration::from_secs(1)).unwrap(),
            vec![SocketAddr::from(([127, 0, 0, 1], 99))]);
        // A later caller to the hung host shares the still-running flight,
        // even after all earlier callers have timed out.
        assert!(pool.resolve("hung.invalid", 100, Instant::now() + Duration::from_millis(20)).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        release.send(()).unwrap();
    }

    #[test]
    fn stuck_dns_has_a_deadline_and_cannot_spawn_unbounded_workers() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (a, p) = (active.clone(), peak.clone());
        let pool = Arc::new(Resolver::new(2, 3, Arc::new(move |_, port| {
            let count = a.fetch_add(1, Ordering::SeqCst) + 1;
            p.fetch_max(count, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(250));
            a.fetch_sub(1, Ordering::SeqCst);
            Ok(vec![SocketAddr::from(([127, 0, 0, 1], port))])
        })).unwrap());
        let workers: Vec<_> = (0..20).map(|_| {
            let pool = pool.clone();
            std::thread::spawn(move || {
                let started = Instant::now();
                let result = pool.resolve("stuck.invalid", 80, started + Duration::from_millis(40));
                assert!(result.is_err());
                assert!(started.elapsed() < Duration::from_secs(1));
            })
        }).collect();
        for worker in workers { worker.join().unwrap(); }
        assert!(peak.load(Ordering::SeqCst) <= 2);
        let deadline = Instant::now() + Duration::from_secs(2);
        while active.load(Ordering::SeqCst) != 0 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        // Expired queued jobs are skipped instead of starting more abandoned lookups.
        assert_eq!(pool.resolve("fresh.invalid", 81, Instant::now() + Duration::from_secs(1)).unwrap(), vec![SocketAddr::from(([127, 0, 0, 1], 81))]);
    }
}
