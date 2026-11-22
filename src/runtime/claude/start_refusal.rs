//! A verified adapter startup refusal closes automatic admission until an
//! explicit retry. Stderr belongs to this spawn and never enters public errors.
use crate::asks::store::AskState;
use chrono::{DateTime, Utc};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

pub const SCHEMA_START_REFUSAL_CODE: &str = "claude_database_version_unsupported";
pub const SCHEMA_START_REFUSAL_DETAIL: &str = "The Claude adapter cannot open this session's database version. Select a compatible Claude runtime, then explicitly bring the agent online or resume it to retry.";

#[derive(Debug)]
pub struct ClaudeStartRefusal;
impl std::fmt::Display for ClaudeStartRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{SCHEMA_START_REFUSAL_CODE}: {SCHEMA_START_REFUSAL_DETAIL}"
        )
    }
}
impl std::error::Error for ClaudeStartRefusal {}

pub fn is_schema_start_refusal(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<ClaudeStartRefusal>())
}

/// Only accept the final exception of this process's fresh bounded stderr.
/// Old log entries, quoted text, and unrelated/transient exits cannot latch.
pub(super) fn schema_refusal_in_stderr(path: &Path, start: u64) -> bool {
    let read = || -> std::io::Result<bool> {
        let mut file = File::open(path)?;
        let end = file.metadata()?.len();
        if end <= start || end - start > 64 * 1024 {
            return Ok(false);
        }
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.take(end - start).read_to_end(&mut bytes)?;
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return Ok(false);
        };
        Ok(text.lines().rev().find(|line| !line.trim().is_empty())
            == Some("RuntimeError: unsupported database version; migration required"))
    };
    read().unwrap_or(false)
}

/// Persist the existing runtime gate and its actionable explanation atomically.
/// Owner pauses/stops retain priority; neither notifications nor time release it.
pub(crate) fn hold_schema_refusal(
    state: &mut AskState,
    id: &str,
    generation: u64,
    now: DateTime<Utc>,
) {
    crate::asks::control::runtime_gate_pause(state, id, generation, now);
    crate::asks::wake::record_owner_action(
        state,
        id,
        SCHEMA_START_REFUSAL_CODE,
        Some(SCHEMA_START_REFUSAL_DETAIL),
        now,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_this_spawn_final_fixed_exception_is_classified() {
        let dir =
            std::env::temp_dir().join(format!("cutex-schema-refusal-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("stderr");
        let refusal = "RuntimeError: unsupported database version; migration required\n";
        std::fs::write(&log, refusal).unwrap();
        assert!(schema_refusal_in_stderr(&log, 0));
        assert!(!schema_refusal_in_stderr(&log, refusal.len() as u64));
        std::fs::write(&log, format!("{refusal}RuntimeError: connection refused\n")).unwrap();
        assert!(!schema_refusal_in_stderr(&log, 0));
        std::fs::write(&log, format!("quoted: {refusal}")).unwrap();
        assert!(!schema_refusal_in_stderr(&log, 0));
        std::fs::write(&log, format!("{}\r\n", refusal.trim())).unwrap();
        assert!(schema_refusal_in_stderr(&log, 0));
        std::fs::remove_file(log).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    // The parent executes this exact test in a real owned child process.
    #[test]
    fn schema_refusal_child_process() {
        if std::env::var_os("CUTEX_SCHEMA_REFUSAL_FIXTURE").is_none() {
            return;
        }
        eprintln!("RuntimeError: unsupported database version; migration required");
        std::process::exit(23);
    }

    #[test]
    fn verified_child_refusal_blocks_all_automatic_starts_past_120_seconds() {
        use crate::asks::{
            children::AgentFacts,
            scheduler::{
                tests::{world, FakeRuntime},
                tick,
            },
            store::{
                tests::{new_ask, temp_store, REQUESTER, TARGET},
                AskStore,
            },
        };
        for mode in ["wake", "reviewed", "managed"] {
            let (mut world, world_dir) = world(&format!("schema-{mode}"));
            world.facts = vec![
                AgentFacts {
                    cutex_session_id: REQUESTER.into(),
                    project: Some("alpha".into()),
                    ..Default::default()
                },
                AgentFacts {
                    cutex_session_id: TARGET.into(),
                    project: Some("alpha".into()),
                    created_by: Some(REQUESTER.into()),
                    explicit_launch: mode != "wake",
                    reviewed_relaunch: mode == "reviewed",
                    managed_online_by: if mode == "managed" {
                        vec![REQUESTER.into()]
                    } else {
                        vec![]
                    },
                    ..Default::default()
                },
            ];
            let (store, store_dir) = temp_store(&format!("schema-{mode}"));
            let runtime = FakeRuntime::default();
            let now = Utc::now();
            store
                .with_state(|s| {
                    s.create_ask(new_ask("schema-refusal", "keep queued"), now)
                        .unwrap();
                })
                .unwrap();
            tick(&world, &runtime, &store, now).unwrap();
            let starts = || {
                runtime.wakes.borrow().len()
                    + runtime.relaunches.borrow().len()
                    + runtime.managed_wakes.borrow().len()
            };
            assert_eq!(starts(), 1, "fixture must be eligible for {mode}");
            let log = store_dir.join("fixture-stderr");
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "runtime::claude::start_refusal::tests::schema_refusal_child_process",
                    "--nocapture",
                ])
                .env("CUTEX_SCHEMA_REFUSAL_FIXTURE", "1")
                .stdout(std::process::Stdio::null())
                .stderr(File::create(&log).unwrap())
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(23));
            assert!(schema_refusal_in_stderr(&log, 0));
            store
                .with_state(|s| {
                    hold_schema_refusal(s, TARGET, 7, now);
                    crate::asks::wake::finish_wake(s, TARGET, &crate::asks::now_rfc3339(now));
                })
                .unwrap();
            // Reopening the durable store and advancing well past backoff may
            // deliver the explanation, but may never start this target again.
            let reopened = AskStore::at(&store_dir);
            for secs in [120, 121, 600, 86400] {
                tick(
                    &world,
                    &runtime,
                    &reopened,
                    now + chrono::Duration::seconds(secs),
                )
                .unwrap();
                assert_eq!(starts(), 1, "{mode} retried at {secs}");
            }
            reopened
                .with_state(|s| hold_schema_refusal(s, TARGET, 7, now))
                .unwrap();
            let state = reopened.snapshot().unwrap();
            assert_eq!(
                state
                    .notices
                    .iter()
                    .filter(|n| n.method == crate::asks::wake::OWNER_ACTION_EVENT)
                    .count(),
                1
            );
            assert_eq!(
                state
                    .owner_action_told
                    .values()
                    .filter(|r| r.as_str() == SCHEMA_START_REFUSAL_CODE)
                    .count(),
                1
            );
            assert_eq!(state.pauses[TARGET].runtime_generation, Some(7));
            // Explicit Resume admits another attempt, including after a fixed
            // component is selected; time alone cannot do so.
            let later = now + chrono::Duration::days(2);
            reopened
                .with_state(|s| crate::asks::control::owner_resume(s, TARGET, "resumed", later))
                .unwrap();
            tick(&world, &runtime, &reopened, later).unwrap();
            assert_eq!(starts(), 2, "explicit resume should permit {mode}");
            reopened
                .with_state(|s| hold_schema_refusal(s, TARGET, 7, later))
                .unwrap();
            runtime.set(TARGET, |s| {
                s.online = true;
                s.runtime_generation = 8;
                s.exclusive_admission = true;
            });
            tick(
                &world,
                &runtime,
                &reopened,
                later + chrono::Duration::seconds(1),
            )
            .unwrap();
            assert!(
                !reopened.snapshot().unwrap().pauses.contains_key(TARGET),
                "successful explicit Start releases its old runtime gate"
            );
            let _ = std::fs::remove_dir_all(world_dir);
            let _ = std::fs::remove_dir_all(store_dir);
        }
    }

    #[test]
    fn schema_refusal_never_overwrites_owner_stop_or_pause() {
        for stop in [false, true] {
            let mut state = AskState::default();
            let now = Utc::now();
            if stop {
                crate::asks::control::owner_stop(&mut state, "target", now);
            } else {
                crate::asks::control::owner_pause(&mut state, "target", now);
            }
            let before = serde_json::to_value(&state.pauses["target"]).unwrap();
            hold_schema_refusal(&mut state, "target", 7, now);
            assert_eq!(
                serde_json::to_value(&state.pauses["target"]).unwrap(),
                before
            );
            assert!(!crate::asks::control::runtime_gate_opened(
                &mut state,
                "target",
                Some(8),
                "runtimeRestart",
                now
            ));
        }
    }
}
