//! Bounded, path-free planner timings, shared with the waiting request on timeout.
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Instant,
};
pub struct Trace {
    started: Instant,
    state: Mutex<State>,
}
#[derive(Default)]
struct State {
    active: Vec<(&'static str, Instant)>,
    completed: BTreeMap<&'static str, f64>,
    counts: BTreeMap<&'static str, u64>,
}
impl Default for Trace {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            state: Mutex::new(State::default()),
        }
    }
}
impl Trace {
    pub fn snapshot(&self) -> Value {
        let Ok(state) = self.state.lock() else {
            return json!({"unavailable":true});
        };
        json!({"totalMs":self.started.elapsed().as_secs_f64()*1000.,
            "activeStage":state.active.last().map(|(name, _)| name),
            "activeStageMs":state.active.last().map(|(_, start)| start.elapsed().as_secs_f64()*1000.),
            "stagesMs":state.completed,"counts":state.counts})
    }
}
thread_local! { static CURRENT: RefCell<Option<Arc<Trace>>> = const { RefCell::new(None) }; }
pub fn current() -> Arc<Trace> {
    CURRENT.with(|c| c.borrow().clone()).unwrap_or_default()
}
pub struct Scope(Option<Arc<Trace>>);
impl Scope {
    pub fn enter(trace: Arc<Trace>) -> Self {
        Self(CURRENT.with(|c| c.replace(Some(trace))))
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|c| c.replace(self.0.take()));
    }
}
pub struct Stage {
    trace: Option<Arc<Trace>>,
    name: &'static str,
    started: Instant,
}
pub fn count(name: &'static str, value: usize) {
    CURRENT.with(|c| {
        if let Some(trace) = c.borrow().as_ref() {
            if let Ok(mut state) = trace.state.lock() {
                *state.counts.entry(name).or_default() += value as u64;
            }
        }
    });
}
pub fn stage(name: &'static str) -> Stage {
    let trace = CURRENT.with(|c| c.borrow().clone());
    let started = Instant::now();
    if let Some(trace) = &trace {
        if let Ok(mut state) = trace.state.lock() {
            state.active.push((name, started));
        }
    }
    Stage {
        trace,
        name,
        started,
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        if let Some(trace) = &self.trace {
            if let Ok(mut state) = trace.state.lock() {
                if let Some(index) = state
                    .active
                    .iter()
                    .rposition(|(name, start)| *name == self.name && *start == self.started)
                {
                    state.active.remove(index);
                }
                *state.completed.entry(self.name).or_default() +=
                    self.started.elapsed().as_secs_f64() * 1000.;
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timeout_snapshot_preserves_active_and_completed_stages_without_paths() {
        let trace = Arc::new(Trace::default());
        let _scope = Scope::enter(trace.clone());
        drop(stage("authority_references"));
        let _blocked = stage("process_cim");
        let snapshot = trace.snapshot();
        assert_eq!(snapshot["activeStage"], "process_cim");
        assert!(snapshot["stagesMs"]["authority_references"].is_number());
        assert!(snapshot["activeStageMs"].is_number());
    }
}
