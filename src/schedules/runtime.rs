use super::{Run, Store, SYSTEM_OWNER};
use anyhow::{ensure, Context};
use chrono::{DateTime, Local, Utc};

#[derive(Debug)]
pub struct Paused;
impl std::fmt::Display for Paused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("target is paused; occurrence held")
    }
}
impl std::error::Error for Paused {}
#[derive(Debug)]
pub struct OwnerStopped;
impl std::fmt::Display for OwnerStopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ownerStopped: explicit online is required; occurrence held")
    }
}
impl std::error::Error for OwnerStopped {}
#[derive(Debug)]
pub struct Unavailable(pub String);
impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Unavailable {}

pub trait Runtime {
    /// Fresh eligibility check; errors with Paused hold, Unavailable refuse.
    fn check(&mut self, run: &Run) -> anyhow::Result<()>;
    /// Idempotently verify/reconnect/start; the adapter must recheck pause at spawn.
    fn online(&mut self, run: &Run) -> anyhow::Result<()>;
    fn handoff(&mut self, run: &Run) -> anyhow::Result<String>;
    fn completed_at(&self) -> Option<DateTime<Utc>> {
        None
    }
}

pub fn tick<T: chrono::TimeZone>(
    store: &Store,
    now: DateTime<Utc>,
    zone: &T,
    runtime: &mut impl Runtime,
) -> anyhow::Result<usize> {
    let runs = store.claim(now, zone, 8)?;
    let count = runs.len();
    for run in runs {
        let result = (|| {
            runtime.check(&run)?;
            runtime.online(&run)?;
            runtime.check(&run)?;
            runtime.handoff(&run)
        })();
        let completed = runtime.completed_at().unwrap_or(now);
        match result {
            Ok(message) => {
                store.finish(&run, "handedOff", None, Some(message), completed)?;
            }
            Err(error) => {
                let outcome = if error.is::<Paused>() || error.is::<OwnerStopped>() {
                    "held"
                } else if error.is::<Unavailable>() {
                    "refused"
                } else {
                    "retrying"
                };
                store.finish(&run, outcome, Some(format!("{error:#}")), None, completed)?;
            }
        }
    }
    Ok(count)
}

pub fn validate_target(target: &str) -> anyhow::Result<()> {
    let record = crate::session::store::shared_session_row(target)?
        .ok_or_else(|| Unavailable("target agent is absent".into()))?;
    ensure!(
        !crate::agent_management::agent_execution_blocked(&record),
        Unavailable("target agent is archived".into())
    );
    ensure!(
        crate::runtime::lifecycle::cutex_session_host_is_local(
            &record.host_id,
            &crate::platform::host::current_host_name()
        ),
        Unavailable("target belongs to another host".into())
    );
    Ok(())
}
struct HostRuntime<F> {
    online: F,
}
impl<F: Fn(&str, &str) -> anyhow::Result<()>> Runtime for HostRuntime<F> {
    fn completed_at(&self) -> Option<DateTime<Utc>> {
        Some(Utc::now())
    }
    fn check(&mut self, run: &Run) -> anyhow::Result<()> {
        validate_target(&run.target_session_id)?;
        let asks = crate::asks::store::AskStore::open_default()?;
        if asks.inspect(|state| state.owner_offline.contains(&run.target_session_id))? {
            return Err(OwnerStopped.into());
        }
        if asks.inspect(|state| state.pauses.contains_key(&run.target_session_id))? {
            return Err(Paused.into());
        }
        match crate::asks::pause_admission::gate(&asks, &run.target_session_id)? {
            crate::asks::pause_admission::Gate::Open => Ok(()),
            crate::asks::pause_admission::Gate::Paused => Err(Paused.into()),
            crate::asks::pause_admission::Gate::Deferred => {
                Err(crate::asks::pause_admission::Deferred.into())
            }
        }
    }
    fn online(&mut self, run: &Run) -> anyhow::Result<()> {
        (self.online)(&run.target_session_id, &run.id).map_err(|error| {
            if error.chain().any(|cause| {
                cause.is::<crate::agent_management::AutomaticRuntimeStartOwnerStopped>()
                    || cause.to_string().starts_with("ownerStopped:")
            }) {
                OwnerStopped.into()
            } else {
                error
            }
        })
    }
    fn handoff(&mut self, run: &Run) -> anyhow::Result<String> {
        let alarm = crate::agent_bus::alarms::set_system_notice(
            SYSTEM_OWNER,
            &run.id,
            &run.target_session_id,
            &delivery_message(run),
        )?;
        Ok(alarm.message_id)
    }
}
// Include the slot even for on-time deliveries: retries must use identical
// transport content, including after a handoff receipt was lost.
fn delivery_message(run: &Run) -> String {
    format!(
        "[定时任务] 原定时间：{}（UTC）。\n\n{}",
        run.scheduled_at.to_rfc3339(),
        run.message
    )
}
/// Called in the Agent Bus process. A kernel lock excludes another scheduler
/// across daemon restarts; all runtime and bus work happens after DB commits.
pub fn spawn(
    online: impl Fn(&str, &str) -> anyhow::Result<()> + Send + 'static,
) -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name("cutex-recurring-schedules".into())
        .spawn(move || {
            loop {
                let result = (|| -> anyhow::Result<()> {
                    let store = Store::open_default()?;
                    let _worker = store.worker_lock()?;
                    store.recover_dispatching(Utc::now())?;
                    let mut runtime = HostRuntime { online: &online };
                    loop {
                        tick(&store, Utc::now(), &Local, &mut runtime)?;
                        std::thread::sleep(std::time::Duration::from_secs(2));
                    }
                })();
                if let Err(error) = result {
                    eprintln!("Cutex schedules: {error:#}");
                }
                // An unavailable schedule store must not take down the Agent Bus.
                std::thread::sleep(std::time::Duration::from_secs(30));
            }
        })
        .context("start recurring schedule worker")?;
    Ok(())
}

pub fn agent_tools() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({"name":"schedule_list","description":"List owner-created recurring schedules targeting you. Read-only; only the owner creates, changes or deletes these schedules.","inputSchema":{"type":"object","properties":{"cursor":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":100}},"additionalProperties":false}}),
    ]
}
pub fn agent_list(session: &str, args: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Args {
        cursor: Option<String>,
        limit: Option<usize>,
    }
    let args: Args = serde_json::from_value(args.clone())?;
    ensure!(
        crate::agent_bus::routing::is_full_durable_cutex_session_id(session),
        "authenticated durable agent required"
    );
    ensure!(
        args.cursor.as_ref().is_none_or(|c| c.len() <= 100),
        "invalid cursor"
    );
    Ok(serde_json::to_value(own_list(
        &Store::open_default()?,
        session,
        args.cursor.as_deref(),
        args.limit.unwrap_or(50),
    )?)?)
}

pub(crate) fn own_list(
    store: &Store,
    session: &str,
    cursor: Option<&str>,
    limit: usize,
) -> anyhow::Result<super::Page<super::Schedule>> {
    let mut page = store.list(Some(session), cursor, limit)?;
    for row in &mut page.data {
        if row
            .last_run
            .as_ref()
            .is_some_and(|run| run.target_session_id != session)
        {
            row.last_run = None;
        }
        if let Some(run) = &mut row.last_run {
            run.lease_token = None;
        }
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedules::{CreateArgs, Rule};

    fn owner_stop_holds(explicit: bool, late: bool) {
        let target = format!("cutex.{}", uuid::Uuid::new_v4());
        let root =
            std::env::temp_dir().join(format!("schedule-owner-stop-{}", uuid::Uuid::new_v4()));
        let mut record = crate::session::model::CutexSessionRecord::new(
            target.clone(),
            None,
            crate::platform::host::current_host_name(),
            root.to_string_lossy().into_owned(),
            None,
        )
        .unwrap();
        record.agent_enabled = true;
        if explicit {
            record.explicit_launch = Some(crate::agent_management::ExplicitLaunchContract {
                version: 4,
                migration_action_id: None,
                native_id: uuid::Uuid::new_v4().to_string(),
                native_home: root.join("native"),
                bundle_manifest: root.join("bundle.json"),
                bundle_sha256: crate::role_revision::Sha256::new("a".repeat(64)).unwrap(),
            });
        }
        let path = crate::session::store::cutex_sessions_path().unwrap();
        crate::session::store::with_locked_session_rows(&path, |rows| {
            rows.insert_session(&target, record);
            rows.commit().map(drop)
        })
        .unwrap();
        let asks = crate::asks::store::AskStore::open_default().unwrap();
        if !late {
            asks.with_state(|state| {
                state.owner_offline.insert(target.clone());
            })
            .unwrap();
        }
        let store = Store::open(&root).unwrap();
        let now = Utc::now();
        let row = store
            .create(
                CreateArgs {
                    action_id: format!("owner-stop-{}", uuid::Uuid::new_v4()),
                    target_session_id: target.clone(),
                    message: "report".into(),
                    rule: Rule::Interval { hours: 1 },
                    enabled: true,
                },
                now,
                &Utc,
                |_| Ok(()),
            )
            .unwrap();
        let mut runtime = HostRuntime {
            online: |_: &str, _: &str| -> anyhow::Result<()> {
                if late {
                    // The HostRuntime check passed before this owner stop.
                    asks.with_state(|state| {
                        state.owner_offline.insert(target.clone());
                    })?;
                    anyhow::bail!("ownerStopped: explicit online is required");
                }
                panic!("schedule must not invoke either launch adapter for an owner-stopped agent")
            },
        };
        tick(&store, now + chrono::Duration::hours(1), &Utc, &mut runtime).unwrap();
        let run = store.get(&row.id).unwrap().last_run.unwrap();
        assert_eq!(run.outcome, "held");
        assert!(run
            .last_error
            .as_deref()
            .unwrap()
            .starts_with("ownerStopped:"));
        assert!(run.message_id.is_none());
        asks.with_state(|state| {
            state.owner_offline.remove(&target);
        })
        .unwrap();
        // The owner's start clears this marker; the same held occurrence then
        // becomes eligible, with its original message/slot intact.
        runtime.check(&run).unwrap();
        assert_eq!(run.message, "report");
        assert_eq!(
            run.scheduled_at.timestamp_millis(),
            row.next_run_at.unwrap().timestamp_millis()
        );
        crate::session::store::with_locked_session_rows(&path, |rows| {
            rows.remove_session(&target);
            rows.commit().map(drop)
        })
        .unwrap();
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn owner_stopped_automatic_target_is_held_until_owner_starts_it() {
        owner_stop_holds(false, false);
    }
    #[test]
    fn owner_stopped_explicit_launch_target_is_held_until_owner_starts_it() {
        owner_stop_holds(true, false);
    }
    #[test]
    fn schedule_automatic_stop_between_check_and_online_is_held() {
        owner_stop_holds(false, true);
    }
    #[test]
    fn schedule_explicit_stop_between_check_and_online_is_held() {
        owner_stop_holds(true, true);
    }
    #[test]
    fn delayed_delivery_keeps_original_slot_and_stable_text_across_retries() {
        let root = std::env::temp_dir().join(format!("schedule-slot-{}", uuid::Uuid::new_v4()));
        let store = Store::open(&root).unwrap();
        let now = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        store
            .create(
                CreateArgs {
                    action_id: "slot-time".into(),
                    target_session_id: format!("cutex.{}", uuid::Uuid::new_v4()),
                    message: "x".repeat(65408),
                    rule: Rule::Interval { hours: 1 },
                    enabled: true,
                },
                now,
                &Utc,
                |_| Ok(()),
            )
            .unwrap();
        let first = store
            .claim(now + chrono::Duration::hours(1), &Utc, 1)
            .unwrap()
            .remove(0);
        let text = delivery_message(&first);
        store
            .finish(
                &first,
                "held",
                Some("ownerStopped".into()),
                None,
                now + chrono::Duration::hours(1),
            )
            .unwrap();
        let retry = store
            .claim(now + chrono::Duration::days(7), &Utc, 1)
            .unwrap()
            .remove(0);
        assert_eq!(delivery_message(&retry), text);
        assert!(text.contains("2026-01-01T01:00:00+00:00"));
        assert!(text.ends_with(&format!("\n\n{}", "x".repeat(65408))));
        assert!(text.len() <= 65536);
        assert_eq!(retry.id, first.id);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
