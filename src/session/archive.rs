//! Durable archive transitions for `cutex_session` records.
//!
//! This module owns only the durable state transition.  Runtime termination
//! is supplied by the caller so the transition can be tested with disposable
//! fixtures and the live management layer can keep its existing safe-stop
//! implementation.

use std::error::Error;
use std::fmt;

use chrono::DateTime;
use chrono::FixedOffset;

use crate::session::model::CutexSessionArchiveState;
use crate::session::model::CutexSessionRecord;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CutexSessionArchiveError {
    SessionNotFound { cutex_session_id: String },
    StaleRevision { expected: u64, actual: u64 },
    StaleRuntimeFence { expected: u64, actual: u64 },
    AlreadyRetired,
    AlreadyActive,
    RuntimeStillOnline,
    RuntimeStopFailed { detail: String },
    PersistenceUnavailable { detail: String },
    PersistenceUncertain { detail: String },
    UnsupportedMethod { method: String },
    /// Restore refused: a live Agent now holds this record's name.
    AgentNameTaken { name: String, holder: String },
}

impl fmt::Display for CutexSessionArchiveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SessionNotFound { cutex_session_id } => {
                write!(
                    formatter,
                    "cutex session does not exist: {cutex_session_id}"
                )
            }
            Self::StaleRevision { expected, actual } => write!(
                formatter,
                "durable session revision conflict: expected {expected}, current {actual}"
            ),
            Self::StaleRuntimeFence { expected, actual } => write!(
                formatter,
                "runtime generation conflict: expected {expected}, current {actual}"
            ),
            Self::AlreadyRetired => formatter.write_str("cutex session is already retired"),
            Self::AlreadyActive => formatter.write_str("cutex session is already active"),
            Self::RuntimeStillOnline => {
                formatter.write_str("runtime remained online after the stop operation")
            }
            Self::RuntimeStopFailed { detail } => {
                write!(formatter, "safe runtime stop failed: {detail}")
            }
            Self::PersistenceUnavailable { detail } => {
                write!(
                    formatter,
                    "durable session persistence is unavailable: {detail}"
                )
            }
            Self::PersistenceUncertain { detail } => {
                write!(formatter, "durable session outcome is uncertain: {detail}")
            }
            Self::AgentNameTaken { name, holder } => write!(
                formatter,
                "agent_name_taken: the live Agent {holder} now holds the name '{name}'; rename or archive it before restoring this one"
            ),
            Self::UnsupportedMethod { method } => {
                write!(
                    formatter,
                    "unsupported cutex session archive method: {method}"
                )
            }
        }
    }
}

impl Error for CutexSessionArchiveError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CutexSessionArchiveTransition {
    pub cutex_session_id: String,
    pub revision: u64,
    pub runtime_generation: u64,
    pub lifecycle: CutexSessionArchiveState,
    pub retired_at: Option<String>,
}

/// Injectable boundary for the two-phase Retire/Restore transaction.
/// Implementations own runtime observation, stop, and durable commit; this
/// coordinator owns their ordering and shared revision/runtime fences.
pub trait CutexSessionArchiveTransaction {
    fn load_session_including_retired(
        &mut self,
        cutex_session_id: &str,
    ) -> Result<CutexSessionRecord, CutexSessionArchiveError>;

    fn runtime_is_offline(
        &mut self,
        record: &CutexSessionRecord,
    ) -> Result<bool, CutexSessionArchiveError>;

    fn stop_runtime(&mut self, record: &CutexSessionRecord)
        -> Result<(), CutexSessionArchiveError>;

    fn commit_retire(
        &mut self,
        cutex_session_id: &str,
        expected_revision: u64,
        expected_runtime_generation: u64,
        retired_at: String,
    ) -> Result<CutexSessionArchiveTransition, CutexSessionArchiveError>;

    fn commit_restore(
        &mut self,
        cutex_session_id: &str,
        expected_revision: u64,
        restored_at: String,
    ) -> Result<CutexSessionArchiveTransition, CutexSessionArchiveError>;
}

pub fn retire_cutex_session<T: CutexSessionArchiveTransaction>(
    transaction: &mut T,
    cutex_session_id: &str,
    expected_revision: u64,
    expected_runtime_generation: u64,
    retired_at: String,
) -> Result<CutexSessionArchiveTransition, CutexSessionArchiveError> {
    let before = transaction.load_session_including_retired(cutex_session_id)?;
    validate_retire_preconditions(&before, expected_revision, expected_runtime_generation)?;
    if !transaction.runtime_is_offline(&before)? {
        transaction.stop_runtime(&before)?;
    }

    let after_stop = transaction.load_session_including_retired(cutex_session_id)?;
    validate_retire_preconditions(&after_stop, expected_revision, expected_runtime_generation)?;
    if !transaction.runtime_is_offline(&after_stop)? {
        return Err(CutexSessionArchiveError::RuntimeStillOnline);
    }

    transaction.commit_retire(
        cutex_session_id,
        expected_revision,
        expected_runtime_generation,
        retired_at,
    )
}

pub fn restore_cutex_session<T: CutexSessionArchiveTransaction>(
    transaction: &mut T,
    cutex_session_id: &str,
    expected_revision: u64,
    restored_at: String,
) -> Result<CutexSessionArchiveTransition, CutexSessionArchiveError> {
    let before = transaction.load_session_including_retired(cutex_session_id)?;
    validate_restore_preconditions(&before, expected_revision)?;
    if !transaction.runtime_is_offline(&before)? {
        return Err(CutexSessionArchiveError::RuntimeStillOnline);
    }
    transaction.commit_restore(cutex_session_id, expected_revision, restored_at)
}

pub fn validate_retire_preconditions(
    record: &CutexSessionRecord,
    expected_revision: u64,
    expected_runtime_generation: u64,
) -> Result<(), CutexSessionArchiveError> {
    let actual_revision = record.durable_revision();
    if actual_revision != expected_revision {
        return Err(CutexSessionArchiveError::StaleRevision {
            expected: expected_revision,
            actual: actual_revision,
        });
    }
    if record.is_retired() {
        return Err(CutexSessionArchiveError::AlreadyRetired);
    }
    if record.runtime_generation != expected_runtime_generation {
        return Err(CutexSessionArchiveError::StaleRuntimeFence {
            expected: expected_runtime_generation,
            actual: record.runtime_generation,
        });
    }
    Ok(())
}

pub fn commit_retire(
    record: &mut CutexSessionRecord,
    expected_revision: u64,
    expected_runtime_generation: u64,
    runtime_offline: bool,
    retired_at: String,
) -> Result<CutexSessionArchiveTransition, CutexSessionArchiveError> {
    validate_retire_preconditions(record, expected_revision, expected_runtime_generation)?;
    if !runtime_offline {
        return Err(CutexSessionArchiveError::RuntimeStillOnline);
    }
    validate_utc_timestamp(&retired_at)?;
    record.archive_state = CutexSessionArchiveState::Retired;
    record.retired_at = Some(retired_at);
    record.bump_durable_revision().map_err(|error| {
        CutexSessionArchiveError::PersistenceUncertain {
            detail: error.to_string(),
        }
    })?;
    record.updated_at = record.retired_at.clone().unwrap_or_default();
    Ok(archive_transition(record))
}

pub fn validate_restore_preconditions(
    record: &CutexSessionRecord,
    expected_revision: u64,
) -> Result<(), CutexSessionArchiveError> {
    crate::agent_management::ensure_agent_not_permanently_retired(record)
        .map_err(|error| CutexSessionArchiveError::PersistenceUnavailable { detail: error.to_string() })?;
    let actual_revision = record.durable_revision();
    if actual_revision != expected_revision {
        return Err(CutexSessionArchiveError::StaleRevision {
            expected: expected_revision,
            actual: actual_revision,
        });
    }
    if record.is_active() {
        return Err(CutexSessionArchiveError::AlreadyActive);
    }
    if record_has_runtime_claim(record) {
        return Err(CutexSessionArchiveError::RuntimeStillOnline);
    }
    Ok(())
}

fn validate_utc_timestamp(timestamp: &str) -> Result<(), CutexSessionArchiveError> {
    let parsed = DateTime::<FixedOffset>::parse_from_rfc3339(timestamp).map_err(|error| {
        CutexSessionArchiveError::PersistenceUncertain {
            detail: format!("invalid archive timestamp: {error}"),
        }
    })?;
    if parsed.offset().local_minus_utc() != 0 {
        return Err(CutexSessionArchiveError::PersistenceUncertain {
            detail: "archive timestamp must use UTC".to_string(),
        });
    }
    Ok(())
}

pub fn commit_restore(
    record: &mut CutexSessionRecord,
    expected_revision: u64,
    restored_at: String,
) -> Result<CutexSessionArchiveTransition, CutexSessionArchiveError> {
    validate_restore_preconditions(record, expected_revision)?;
    record.archive_state = CutexSessionArchiveState::Active;
    record.retired_at = None;
    record.bump_durable_revision().map_err(|error| {
        CutexSessionArchiveError::PersistenceUncertain {
            detail: error.to_string(),
        }
    })?;
    record.updated_at = restored_at;
    Ok(archive_transition(record))
}

pub fn record_has_runtime_claim(record: &CutexSessionRecord) -> bool {
    record.app_server_launch_claim_id.is_some()
        || record.app_server_runtime.is_some()
        || record.current_runtime_agent_id.is_some()
        || record.alden_pid.is_some()
        || record.runtime_pid.is_some()
}

fn archive_transition(record: &CutexSessionRecord) -> CutexSessionArchiveTransition {
    CutexSessionArchiveTransition {
        cutex_session_id: record.cutex_session_id.clone(),
        revision: record.durable_revision(),
        runtime_generation: record.runtime_generation,
        lifecycle: record.archive_state,
        retired_at: record.retired_at.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_bus::model::AgentRegistrationClass;
    use crate::session::model::CutexAppServerRuntimeBinding;
    use crate::session::model::CutexAppServerTransport;
    use crate::session::model::CutexSessionQuickActionMode;
    use crate::session::model::CutexSessionRuntimeBackend;
    use crate::session::model::CutexSessionUserAction;

    #[derive(Debug)]
    struct FakeArchiveTransaction {
        record: CutexSessionRecord,
        runtime_online: bool,
        stop_fails: bool,
        steps: Vec<&'static str>,
    }

    impl CutexSessionArchiveTransaction for FakeArchiveTransaction {
        fn load_session_including_retired(
            &mut self,
            _cutex_session_id: &str,
        ) -> Result<CutexSessionRecord, CutexSessionArchiveError> {
            self.steps.push("load");
            Ok(self.record.clone())
        }

        fn runtime_is_offline(
            &mut self,
            record: &CutexSessionRecord,
        ) -> Result<bool, CutexSessionArchiveError> {
            self.steps.push("observe");
            Ok(!self.runtime_online && !record_has_runtime_claim(record))
        }

        fn stop_runtime(
            &mut self,
            _record: &CutexSessionRecord,
        ) -> Result<(), CutexSessionArchiveError> {
            self.steps.push("stop");
            if self.stop_fails {
                return Err(CutexSessionArchiveError::RuntimeStopFailed {
                    detail: "injected stop failure".to_string(),
                });
            }
            self.runtime_online = false;
            self.record.app_server_launch_claim_id = None;
            self.record.app_server_runtime = None;
            self.record.current_runtime_agent_id = None;
            self.record.alden_pid = None;
            self.record.runtime_pid = None;
            Ok(())
        }

        fn commit_retire(
            &mut self,
            _cutex_session_id: &str,
            expected_revision: u64,
            expected_runtime_generation: u64,
            retired_at: String,
        ) -> Result<CutexSessionArchiveTransition, CutexSessionArchiveError> {
            self.steps.push("commit_retire");
            commit_retire(
                &mut self.record,
                expected_revision,
                expected_runtime_generation,
                true,
                retired_at,
            )
        }

        fn commit_restore(
            &mut self,
            _cutex_session_id: &str,
            expected_revision: u64,
            restored_at: String,
        ) -> Result<CutexSessionArchiveTransition, CutexSessionArchiveError> {
            self.steps.push("commit_restore");
            commit_restore(&mut self.record, expected_revision, restored_at)
        }
    }

    fn record() -> CutexSessionRecord {
        CutexSessionRecord::new_at(
            "cutex.archive-test".to_string(),
            Some("thread.archive-test".to_string()),
            "devbox".to_string(),
            "/tmp/archive-test".to_string(),
            Some("default".to_string()),
            "2026-08-10T00:00:00Z".to_string(),
        )
        .expect("record")
    }

    #[test]
    fn retire_requires_the_current_revision_and_runtime_fence() {
        let mut value = record();
        value.runtime_generation = 4;
        assert!(matches!(
            commit_retire(&mut value, 1, 3, true, "2026-08-10T00:01:00Z".to_string()),
            Err(CutexSessionArchiveError::StaleRuntimeFence {
                expected: 3,
                actual: 4
            })
        ));
        assert!(matches!(
            commit_retire(&mut value, 2, 4, true, "2026-08-10T00:01:00Z".to_string()),
            Err(CutexSessionArchiveError::StaleRevision {
                expected: 2,
                actual: 1
            })
        ));
    }

    #[test]
    fn retire_marks_archive_only_after_offline_confirmation() {
        let mut value = record();
        value.runtime_generation = 4;
        let original = value.clone();
        assert!(matches!(
            commit_retire(&mut value, 1, 4, false, "2026-08-10T00:01:00Z".to_string()),
            Err(CutexSessionArchiveError::RuntimeStillOnline)
        ));
        assert_eq!(value, original);

        let result = commit_retire(&mut value, 1, 4, true, "2026-08-10T00:01:00Z".to_string())
            .expect("retire");
        assert_eq!(result.lifecycle, CutexSessionArchiveState::Retired);
        assert_eq!(result.revision, 2);
        assert_eq!(value.retired_at.as_deref(), Some("2026-08-10T00:01:00Z"));
    }

    #[test]
    fn restore_preserves_all_durable_fields_and_never_starts_a_runtime() {
        let mut value = record();
        value.pending_launch_id = Some("legacy-launch-correlation".to_string());
        value.app_server_launch_claim_id = Some("launch-claim-9".to_string());
        value.thread_name = Some("archive-thread".to_string());
        value.display_name_hint = Some("Archive Test".to_string());
        value.host_id = "host-preserved".to_string();
        value.cwd = "/worktree/archive-test".to_string();
        value.managed_cwd = Some("/managed/archive-test".to_string());
        value.profile = Some("deepseek".to_string());
        value.runtime_backend = CutexSessionRuntimeBackend::CuteAlden;
        value.agent_enabled = true;
        value.agent_groups = vec!["cutex".to_string(), "management".to_string()];
        value.registration_class = AgentRegistrationClass::Persistent;
        value.exposed_to_backend = true;
        value.quick_action = CutexSessionQuickActionMode::Pinned;
        value.default_cli_args = vec![
            "--search".to_string(),
            "--config".to_string(),
            "model_provider=deepseek".to_string(),
        ];
        value.permission_defaults = Some("workspace-write".to_string());
        value.approval_policy = Some("on-request".to_string());
        value.sandbox_mode = Some("workspace-write".to_string());
        value.model_defaults = Some("deepseek-chat".to_string());
        value.reasoning_defaults = Some("high".to_string());
        value.alden_session_name = Some("cute-alden-archive-test".to_string());
        value.alden_pid = Some(9101);
        value.runtime_pid = Some(9102);
        value.app_server_runtime = Some(CutexAppServerRuntimeBinding {
            transport: CutexAppServerTransport::UnixSocket,
            endpoint: "/tmp/cutex-archive-test.sock".to_string(),
            pid: 9102,
            runtime_dir: "/tmp/cutex-archive-test".to_string(),
            launched_profile: Some("deepseek".to_string()),
            launched_profile_id: None,
            launch_profile_source: None,
            auth_token_path: Some("/tmp/cutex-archive-test/token".to_string()),
            diagnostic_journal_path: "/tmp/cutex-archive-test/journal.jsonl".to_string(),
            schema_version: "v2".to_string(),
            schema_sha256: "schema-sha256".to_string(),
            started_at: "2026-08-10T00:00:10Z".to_string(),
        });
        value.current_runtime_agent_id = Some("runtime-agent-9".to_string());
        value.runtime_generation = 9;
        value.last_runtime_agent_id = Some("runtime-agent-8".to_string());
        value.last_seen_at = Some("2026-08-10T00:00:20Z".to_string());
        value.last_user_selected_at = Some("2026-08-10T00:00:30Z".to_string());
        value.last_user_action = Some(CutexSessionUserAction::Takeover);

        let mut expected = value.clone();
        expected.revision = 3;
        expected.archive_state = CutexSessionArchiveState::Active;
        expected.retired_at = None;
        expected.app_server_launch_claim_id = None;
        expected.alden_pid = None;
        expected.runtime_pid = None;
        expected.app_server_runtime = None;
        expected.current_runtime_agent_id = None;
        expected.updated_at = "2026-08-10T00:02:00Z".to_string();

        let mut transaction = FakeArchiveTransaction {
            record: value,
            runtime_online: true,
            stop_fails: false,
            steps: Vec::new(),
        };
        retire_cutex_session(
            &mut transaction,
            "cutex.archive-test",
            1,
            9,
            "2026-08-10T00:01:00Z".to_string(),
        )
        .expect("retire");
        let result = restore_cutex_session(
            &mut transaction,
            "cutex.archive-test",
            2,
            "2026-08-10T00:02:00Z".to_string(),
        )
        .expect("restore");

        assert_eq!(result.lifecycle, CutexSessionArchiveState::Active);
        assert_eq!(result.revision, 3);
        assert_eq!(result.runtime_generation, 9);
        assert_eq!(result.retired_at, None);
        assert_eq!(transaction.record, expected);
        assert!(!record_has_runtime_claim(&transaction.record));
        assert_eq!(
            transaction.steps,
            [
                "load",
                "observe",
                "stop",
                "load",
                "observe",
                "commit_retire",
                "load",
                "observe",
                "commit_restore"
            ]
        );
    }

    #[test]
    fn repeated_transitions_have_typed_idempotency_errors() {
        let mut value = record();
        commit_retire(&mut value, 1, 0, true, "2026-08-10T00:01:00Z".to_string()).expect("retire");
        assert!(matches!(
            commit_retire(&mut value, 2, 0, true, "2026-08-10T00:02:00Z".to_string()),
            Err(CutexSessionArchiveError::AlreadyRetired)
        ));
        assert!(matches!(
            commit_restore(&mut value, 1, "2026-08-10T00:03:00Z".to_string()),
            Err(CutexSessionArchiveError::StaleRevision { .. })
        ));
        commit_restore(&mut value, 2, "2026-08-10T00:03:00Z".to_string()).expect("restore");
        assert!(matches!(
            commit_restore(&mut value, 3, "2026-08-10T00:04:00Z".to_string()),
            Err(CutexSessionArchiveError::AlreadyActive)
        ));
    }

    #[test]
    fn transaction_coordinates_offline_and_online_retire_before_commit() {
        let mut offline = FakeArchiveTransaction {
            record: record(),
            runtime_online: false,
            stop_fails: false,
            steps: Vec::new(),
        };
        retire_cutex_session(
            &mut offline,
            "cutex.archive-test",
            1,
            0,
            "2026-08-10T00:01:00Z".to_string(),
        )
        .expect("offline retire");
        assert_eq!(
            offline.steps,
            ["load", "observe", "load", "observe", "commit_retire"]
        );

        let mut online_record = record();
        online_record.runtime_generation = 4;
        online_record.current_runtime_agent_id = Some("runtime-4".to_string());
        let mut online = FakeArchiveTransaction {
            record: online_record,
            runtime_online: true,
            stop_fails: false,
            steps: Vec::new(),
        };
        retire_cutex_session(
            &mut online,
            "cutex.archive-test",
            1,
            4,
            "2026-08-10T00:01:00Z".to_string(),
        )
        .expect("online retire");
        assert_eq!(
            online.steps,
            [
                "load",
                "observe",
                "stop",
                "load",
                "observe",
                "commit_retire"
            ]
        );
    }

    #[test]
    fn transaction_stop_failure_leaves_the_record_active() {
        let mut online_record = record();
        online_record.current_runtime_agent_id = Some("runtime-1".to_string());
        let original = online_record.clone();
        let mut transaction = FakeArchiveTransaction {
            record: online_record,
            runtime_online: true,
            stop_fails: true,
            steps: Vec::new(),
        };

        let error = retire_cutex_session(
            &mut transaction,
            "cutex.archive-test",
            1,
            0,
            "2026-08-10T00:01:00Z".to_string(),
        )
        .expect_err("failed stop must block retire");

        assert!(matches!(
            error,
            CutexSessionArchiveError::RuntimeStopFailed { .. }
        ));
        assert_eq!(transaction.steps, ["load", "observe", "stop"]);
        assert_eq!(transaction.record, original);
    }

    #[test]
    fn restore_rejects_connected_runtime_and_never_invokes_a_launch() {
        let mut retired = record();
        commit_retire(&mut retired, 1, 0, true, "2026-08-10T00:01:00Z".to_string())
            .expect("retire fixture");
        let mut transaction = FakeArchiveTransaction {
            record: retired,
            runtime_online: true,
            stop_fails: false,
            steps: Vec::new(),
        };

        let error = restore_cutex_session(
            &mut transaction,
            "cutex.archive-test",
            2,
            "2026-08-10T00:02:00Z".to_string(),
        )
        .expect_err("connected restore must fail");

        assert_eq!(error, CutexSessionArchiveError::RuntimeStillOnline);
        assert_eq!(transaction.steps, ["load", "observe"]);
        assert!(transaction.record.is_retired());
    }

    #[test]
    fn historical_pending_launch_id_is_preserved_across_retire_restore() {
        let mut value = record();
        value.pending_launch_id = Some("legacy-launch-correlation".to_string());
        assert!(!record_has_runtime_claim(&value));
        let mut transaction = FakeArchiveTransaction {
            record: value,
            runtime_online: false,
            stop_fails: false,
            steps: Vec::new(),
        };

        retire_cutex_session(
            &mut transaction,
            "cutex.archive-test",
            1,
            0,
            "2026-08-10T00:01:00Z".to_string(),
        )
        .expect("retire with historical correlation");
        restore_cutex_session(
            &mut transaction,
            "cutex.archive-test",
            2,
            "2026-08-10T00:02:00Z".to_string(),
        )
        .expect("restore with historical correlation");

        assert_eq!(
            transaction.record.pending_launch_id.as_deref(),
            Some("legacy-launch-correlation")
        );
        assert!(transaction.record.is_active());
    }
}
