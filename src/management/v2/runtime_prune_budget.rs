//! Per-inspection I/O memoization and cooperative cancellation; no shared authority cache.
use serde_json::Value;
use std::{
    cell::RefCell,
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
struct Context {
    deadline: Instant,
    cancel: Arc<AtomicBool>,
    paths: HashMap<PathBuf, PathBuf>,
    json: HashMap<PathBuf, Value>,
    json_bytes: usize,
    normalizations: usize,
}
thread_local! {static CURRENT:RefCell<Option<Context>>=const {RefCell::new(None)};static LAST:RefCell<usize>=const {RefCell::new(0)};}
pub struct Scope(bool);
impl Scope {
    pub fn enter(duration: Duration, cancel: Arc<AtomicBool>) -> Self {
        let own = CURRENT.with(|c| {
            let mut c = c.borrow_mut();
            if c.is_some() {
                false
            } else {
                *c = Some(Context {
                    deadline: Instant::now() + duration,
                    cancel,
                    paths: HashMap::new(),
                    json: HashMap::new(),
                    json_bytes: 0,
                    normalizations: 0,
                });
                true
            }
        });
        Self(own)
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        if self.0 {
            CURRENT.with(|c| {
                if let Some(c) = c.borrow_mut().take() {
                    LAST.with(|last| *last.borrow_mut() = c.normalizations);
                }
            });
        }
    }
}
pub fn check() -> anyhow::Result<()> {
    anyhow::ensure!(
        CURRENT.with(|c| c
            .borrow()
            .as_ref()
            .is_none_or(|c| Instant::now() < c.deadline && !c.cancel.load(Ordering::Acquire))),
        "runtime inspection deadline or cancellation"
    );
    Ok(())
}
pub fn path(key: &std::path::Path) -> Option<PathBuf> {
    CURRENT.with(|c| c.borrow().as_ref().and_then(|c| c.paths.get(key).cloned()))
}
pub fn save_path(key: PathBuf, value: PathBuf) {
    CURRENT.with(|c| {
        if let Some(c) = c.borrow_mut().as_mut() {
            c.normalizations += 1;
            if c.paths.len() < 16384 {
                c.paths.insert(key, value);
            }
        }
    });
}
pub fn json(key: &std::path::Path) -> Option<Value> {
    CURRENT.with(|c| c.borrow().as_ref().and_then(|c| c.json.get(key).cloned()))
}
pub fn save_json(key: PathBuf, value: Value, bytes: usize) {
    CURRENT.with(|c| {
        if let Some(c) = c.borrow_mut().as_mut() {
            if c.json_bytes + bytes <= 16 * 1024 * 1024 && c.json.len() < 4096 {
                c.json_bytes += bytes;
                c.json.insert(key, value);
            }
        }
    });
}
#[cfg(test)]
pub fn last_normalizations() -> usize {
    LAST.with(|c| *c.borrow())
}
