//! One fair, optional, post-delivery observation phase. Cursors are process-local
//! scheduling hints only; they never bind, submit, replay or close a message.
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

pub(super) const BACKGROUND_BUDGET: Duration = Duration::from_millis(250);
pub(super) const MESSAGE_BUDGET: Duration = Duration::from_millis(75);
pub(super) const MAX_MESSAGES: usize = 4;
const MAX_AGENT_VISITS: usize = 16;

#[derive(Default)]
pub(crate) struct BackgroundObservations {
    after_agent: Option<String>,
    after_message: BTreeMap<String, String>,
}

impl BackgroundObservations {
    pub(crate) fn retain_candidates(&mut self, candidates: &[String]) {
        let candidates: BTreeSet<_> = candidates.iter().map(String::as_str).collect();
        self.after_message
            .retain(|id, _| candidates.contains(id.as_str()));
        // Keep after_agent as a lexical cursor even when that candidate leaves.
        // New/reordered candidates cannot force the scan back to the first ID.
    }

    /// The entire ordinary pass returns before the background deadline starts.
    /// Failures/unavailability in observe cannot abort any ordinary delivery.
    /// The deadline bounds scheduling and native RPC allowances; pending and
    /// observe may use existing blocking repository APIs, so this is not a
    /// hard wall-clock deadline for repository IO, locks or persistence.
    pub(crate) fn after_deliveries(
        &mut self,
        enabled: bool,
        ordinary: impl FnOnce() -> Vec<String>,
        pending: impl FnMut(&str) -> anyhow::Result<Vec<String>>,
        observe: impl FnMut(&str, &str, Instant),
    ) -> usize {
        self.after_deliveries_with_clock(enabled, ordinary, pending, observe, Instant::now)
    }

    fn after_deliveries_with_clock(
        &mut self,
        enabled: bool,
        ordinary: impl FnOnce() -> Vec<String>,
        mut pending: impl FnMut(&str) -> anyhow::Result<Vec<String>>,
        mut observe: impl FnMut(&str, &str, Instant),
        mut now: impl FnMut() -> Instant,
    ) -> usize {
        let mut agents = ordinary();
        if !enabled {
            return 0;
        }
        agents.sort();
        agents.dedup();
        if agents.is_empty() {
            return 0;
        }
        let deadline = now() + BACKGROUND_BUDGET;
        let mut index = self
            .after_agent
            .as_ref()
            .map_or(0, |after| agents.partition_point(|id| id <= after))
            % agents.len();
        let mut queues = BTreeMap::<String, Vec<String>>::new();
        let mut exhausted = BTreeSet::new();
        let mut seen = BTreeSet::new();
        let mut messages = 0;
        for _ in 0..MAX_AGENT_VISITS {
            if messages >= MAX_MESSAGES || exhausted.len() == agents.len() || now() >= deadline {
                break;
            }
            let agent = &agents[index];
            index = (index + 1) % agents.len();
            // Advance before reading/collecting: one unavailable adapter must
            // not monopolize the first slot on every subsequent observer pass.
            self.after_agent = Some(agent.clone());
            if exhausted.contains(agent) {
                continue;
            }
            if !queues.contains_key(agent) {
                let mut ids = match pending(agent) {
                    Ok(ids) => ids,
                    Err(_) => {
                        exhausted.insert(agent.clone());
                        continue;
                    }
                };
                ids.sort();
                ids.dedup();
                queues.insert(agent.clone(), ids);
            }
            let at = now();
            if at >= deadline {
                break;
            }
            let ids = &queues[agent];
            let start = self
                .after_message
                .get(agent)
                .map_or(0, |after| ids.partition_point(|id| id <= after));
            let message = ids[start..]
                .iter()
                .chain(&ids[..start])
                .find(|message| !seen.contains(&(agent.clone(), (*message).clone())));
            let Some(message) = message else {
                exhausted.insert(agent.clone());
                continue;
            };
            self.after_message.insert(agent.clone(), message.clone());
            seen.insert((agent.clone(), message.clone()));
            messages += 1;
            observe(agent, message, deadline.min(at + MESSAGE_BUDGET));
        }
        messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[test]
    fn all_delivery_polls_precede_even_slow_background_and_cap_off_does_no_work() {
        let mut schedule = BackgroundObservations::default();
        let trace = RefCell::new(Vec::new());
        let base = Instant::now();
        let clock = Cell::new(base);
        let agents: Vec<_> = (0..100).map(|id| format!("agent-{id:03}")).collect();
        for enabled in [false, true] {
            trace.borrow_mut().clear();
            clock.set(base);
            let attempted = schedule.after_deliveries_with_clock(
                enabled,
                || {
                    for id in &agents {
                        trace.borrow_mut().push(format!("poll-{id}"));
                    }
                    agents.clone()
                },
                |id| {
                    assert_eq!(
                        trace
                            .borrow()
                            .iter()
                            .filter(|entry| entry.starts_with("poll-"))
                            .count(),
                        100
                    );
                    Ok((0..100).map(|n| format!("{id}-message-{n:03}")).collect())
                },
                |id, _, deadline| {
                    trace.borrow_mut().push(format!("collect-{id}"));
                    assert!(deadline <= base + BACKGROUND_BUDGET);
                    assert!(deadline <= clock.get() + MESSAGE_BUDGET);
                    clock.set(deadline); // unavailable/slow read uses its allowance
                },
                || clock.get(),
            );
            assert_eq!(attempted, if enabled { MAX_MESSAGES } else { 0 });
            assert!(clock.get() <= base + BACKGROUND_BUDGET);
            assert!(trace.borrow()[..100]
                .iter()
                .all(|entry| entry.starts_with("poll-")));
        }
    }

    #[test]
    fn fair_agent_and_message_cursors_survive_unavailable_reads_and_changing_order() {
        let mut schedule = BackgroundObservations::default();
        let seen = RefCell::new(BTreeSet::new());
        let base = Instant::now();
        let clock = Cell::new(base);
        let mut agents: Vec<_> = (0..9).map(|n| format!("agent-{n}")).collect();
        for pass in 0..27 {
            agents.reverse();
            clock.set(base);
            schedule.after_deliveries_with_clock(
                true,
                || agents.clone(),
                |id| Ok((0..3).rev().map(|n| format!("{id}-m{n}")).collect()),
                |id, message, deadline| {
                    seen.borrow_mut()
                        .insert((id.to_owned(), message.to_owned()));
                    if pass % 2 == 0 {
                        clock.set(deadline);
                    } // failures still advance both cursors
                },
                || clock.get(),
            );
        }
        assert_eq!(
            seen.borrow().len(),
            27,
            "all nine agents and three messages receive a slot"
        );
    }

    #[test]
    fn empty_or_failed_sources_are_bounded_and_never_starve_later_agents() {
        let mut schedule = BackgroundObservations::default();
        let agents: Vec<_> = (0..40).map(|n| format!("agent-{n:02}")).collect();
        let loads = Cell::new(0);
        let seen = RefCell::new(Vec::new());
        for _ in 0..4 {
            let before = loads.get();
            schedule.after_deliveries(
                true,
                || agents.clone(),
                |id| {
                    loads.set(loads.get() + 1);
                    if id == "agent-39" {
                        Ok(vec!["message".into()])
                    } else {
                        anyhow::bail!("synthetic unavailable")
                    }
                },
                |id, _, _| seen.borrow_mut().push(id.to_owned()),
            );
            assert!(loads.get() - before <= MAX_AGENT_VISITS);
        }
        assert!(!seen.borrow().is_empty());
        assert!(seen.borrow().iter().all(|id| id == "agent-39"));
    }

    #[test]
    fn one_large_agent_is_count_bounded_and_each_message_is_read_once_per_phase() {
        let mut schedule = BackgroundObservations::default();
        let seen = RefCell::new(Vec::new());
        for _ in 0..3 {
            let count = schedule.after_deliveries(
                true,
                || vec!["one".into()],
                |_| Ok((0..30).map(|n| format!("m{n:02}")).collect()),
                |_, message, _| seen.borrow_mut().push(message.to_owned()),
            );
            assert_eq!(count, MAX_MESSAGES);
        }
        assert_eq!(seen.borrow().iter().collect::<BTreeSet<_>>().len(), 12);
        schedule.retain_candidates(&[]);
        assert!(schedule.after_message.is_empty());
    }
}
