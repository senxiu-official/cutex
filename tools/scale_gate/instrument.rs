//! Injected only into the disposable scale-gate build. No shipped dependency.
use std::{cell::Cell, io::Read, sync::atomic::{AtomicU64, Ordering}, time::Instant};
use serde::de::{Deserialize, DeserializeOwned};
static TOTAL: AtomicU64 = AtomicU64::new(0);
thread_local! { static LOCAL: Cell<u64> = const { Cell::new(0) }; }
fn add(n: usize) { TOTAL.fetch_add(n as u64, Ordering::Relaxed); LOCAL.with(|v| v.set(v.get()+n as u64)); }
pub fn from_str<'a,T: Deserialize<'a>>(s: &'a str) -> serde_json::Result<T> { add(s.len()); serde_json::from_str(s) }
pub fn from_slice<'a,T: Deserialize<'a>>(s: &'a [u8]) -> serde_json::Result<T> { add(s.len()); serde_json::from_slice(s) }
struct Counting<R>(R);
impl<R: Read> Read for Counting<R> { fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> { let n=self.0.read(b)?; add(n); Ok(n) } }
pub fn from_reader<R: Read,T: DeserializeOwned>(r:R) -> serde_json::Result<T> { serde_json::from_reader(Counting(r)) }
pub struct Request { path: String, start: Instant, bytes: u64 }
impl Request { pub fn new(path:&str)->Self { Self{path:path.into(),start:Instant::now(),bytes:LOCAL.with(Cell::get)} } }
fn emit(value: serde_json::Value) {
    use std::io::Write;
    static LOG: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _lock = LOG.lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(path)=std::env::var("CUTEX_SCALE_METRICS") {
        if let Ok(mut file)=std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let line = format!("{value}\n");
            let _=file.write_all(line.as_bytes());
        }
    }
}
impl Drop for Request { fn drop(&mut self) { emit(serde_json::json!({"kind":"request","pid":std::process::id(),"path":self.path,"parse_bytes":LOCAL.with(Cell::get)-self.bytes,"elapsed_us":self.start.elapsed().as_micros()})); } }
pub fn start() {
    assert!(cfg!(debug_assertions), "scale instrumentation requires debug build");
    assert!(std::env::var_os("CUTEX_TEST_PRIVATE_HOME").is_some(), "scale instrumentation requires private home");
    std::thread::spawn(|| loop { std::thread::sleep(std::time::Duration::from_millis(100)); emit(serde_json::json!({"kind":"total","pid":std::process::id(),"parse_bytes":TOTAL.load(Ordering::Relaxed)})); });
}
