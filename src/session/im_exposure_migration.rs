//! One-time migration `im-exposure-retired` (owner decision 2026-10-15): the
//! legacy IM exposure flag (`CutexSessionRecord::exposed_to_backend`,
//! `agent_create` `expose_to_im`) no longer decides whether the owner sees an
//! agent. Management v2 lists every persistent session regardless of it; this
//! migration sets the flag on every active persistent session that has it
//! false, so a rolled-back older binary (which still filters on the flag)
//! lists the same agents. Local-only sessions keep the old flag rule and are
//! not touched.
//!
//! - Runs at the start of `cutex management serve`, under the session store
//!   lock. The record `archive/im-exposure-retired/migration.json` with
//!   `committed: true` marks it done; later starts change nothing, so a flag
//!   cleared afterwards (`cutex session hide`, `cutex im unregister`, both
//!   IM-mirror only now) is never set again.
//! - The record names every changed session (key, name, previous durable
//!   revision and `updated_at`) and is written BEFORE the store commit with
//!   `committed: false`, then rewritten `committed: true`; a failed commit
//!   leaves `committed: false` and the next start plans again.
//! - Convergent: a session that already has the flag is never touched.
//! - Errors never fail startup; the migration is retried on the next start.

use std::fs;
use std::io;
use std::path::Path;

use anyhow::Context;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::agent_bus::model::AgentRegistrationClass;
use crate::session::metadata::cutex_session_presented_name;
use crate::session::model::CutexSessionRecord;
use crate::session::model::CutexSessionStore;

pub const RECORD_SCHEMA: &str = "cutex/im-exposure-retired-migration/v1";
/// Record directory, relative to the cutex dir.
const ARCHIVE_ROOT: &str = "archive/im-exposure-retired";
const RECORD_FILE: &str = "migration.json";

/// One session whose flag was set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExposureChange {
    pub key: String,
    pub cutex_session_id: String,
    pub name: String,
    pub previous_revision: u64,
    pub previous_updated_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MigrationRecord {
    schema: String,
    at: String,
    component: String,
    committed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    sessions: Vec<ExposureChange>,
}

/// `cutex management serve` calls this. Never fails or panics.
pub fn run_on_startup(component: &str) {
    let result = std::panic::catch_unwind(|| {
        let cutex_dir = crate::config::paths::config_dir()?;
        let store = crate::session::store::cutex_sessions_path()?;
        run_at(&cutex_dir, &store, component, Utc::now())
    });
    match result {
        Ok(Ok(Some(changes))) => {
            for change in &changes {
                eprintln!(
                    "cutex im-exposure-retired: session {} ({}): exposed_to_backend false -> true",
                    change.name, change.cutex_session_id
                );
            }
        }
        Ok(Ok(None)) => {}
        Ok(Err(error)) => eprintln!(
            "cutex im-exposure-retired migration: {component}: not run on this start: {error:#}"
        ),
        Err(_) => eprintln!(
            "cutex im-exposure-retired migration: {component}: panicked, not run on this start"
        ),
    }
}

/// The sessions [`run_at`] would change: persistent, active, flag false.
pub fn plan(store: &CutexSessionStore) -> Vec<ExposureChange> {
    let mut changes = store
        .sessions
        .iter()
        .filter(|(_, record)| needs_change(record))
        .map(|(key, record)| ExposureChange {
            key: key.clone(),
            cutex_session_id: record.cutex_session_id.clone(),
            name: cutex_session_presented_name(record),
            previous_revision: record.durable_revision(),
            previous_updated_at: record.updated_at.clone(),
        })
        .collect::<Vec<_>>();
    changes.sort_by(|left, right| left.key.cmp(&right.key));
    changes
}

fn needs_change(record: &CutexSessionRecord) -> bool {
    !record.exposed_to_backend
        && record.is_active()
        && record.registration_class == AgentRegistrationClass::Persistent
}

/// Runs the migration on the store at `store_path` of the cutex dir
/// `cutex_dir`. `None` when it was already done; otherwise the committed
/// changes (possibly empty: the record is still written, marking it done).
pub fn run_at(
    cutex_dir: &Path,
    store_path: &Path,
    component: &str,
    now: DateTime<Utc>,
) -> anyhow::Result<Option<Vec<ExposureChange>>> {
    let archive = cutex_dir.join(ARCHIVE_ROOT);
    let record_path = archive.join(RECORD_FILE);
    // Every start after the first: one stat / read, no lock.
    if read_record(&record_path)?.is_some_and(|record| record.committed) {
        return Ok(None);
    }
    crate::management::client_tokens::ensure_private_dir(&cutex_dir.join("archive"))?;
    crate::management::client_tokens::ensure_private_dir(&archive)?;
    let at = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    crate::session::store::with_locked_session_store(store_path, |store| {
        // Re-checked under the store lock: a concurrent start may have done it.
        if read_record(&record_path)?.is_some_and(|record| record.committed) {
            return Ok(None);
        }
        let changes = plan(store);
        let mut record = MigrationRecord {
            schema: RECORD_SCHEMA.to_owned(),
            at: at.clone(),
            component: component.to_owned(),
            committed: changes.is_empty(),
            error: None,
            sessions: changes.clone(),
        };
        write_record(&record_path, &record)?;
        if changes.is_empty() {
            return Ok(Some(changes));
        }
        for change in &changes {
            let session = store
                .sessions
                .get_mut(&change.key)
                .context("session disappeared under the store lock")?;
            session.exposed_to_backend = true;
            session.bump_durable_revision()?;
            session.updated_at = at.clone();
        }
        if let Err(error) = crate::session::store::save_locked_session_store(store_path, store) {
            record.error = Some(format!("{error:#}"));
            let _ = write_record(&record_path, &record);
            return Err(error);
        }
        record.committed = true;
        write_record(&record_path, &record)?;
        Ok(Some(changes))
    })
}

fn read_record(path: &Path) -> anyhow::Result<Option<MigrationRecord>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let record: MigrationRecord =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    anyhow::ensure!(
        record.schema == RECORD_SCHEMA,
        "unexpected schema {:?} in {}",
        record.schema,
        path.display()
    );
    Ok(Some(record))
}

fn write_record(path: &Path, record: &MigrationRecord) -> anyhow::Result<()> {
    crate::config::atomic::write_private_pretty_json_atomic(
        path,
        record,
        "im-exposure-retired migration record",
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    struct TempHome {
        root: PathBuf,
        dir: PathBuf,
    }

    impl TempHome {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "cutex-im-exposure-migration-{}",
                uuid::Uuid::new_v4().simple()
            ));
            let dir = root.join(".cutex");
            fs::create_dir_all(&dir).unwrap();
            Self { root, dir }
        }

        fn store(&self) -> PathBuf {
            self.dir.join("cutex-sessions.json")
        }

        fn seed(&self, records: Vec<CutexSessionRecord>) {
            let path = self.store();
            crate::session::store::with_locked_session_store(&path, |store| {
                for record in records {
                    store.sessions.insert(record.cutex_session_id.clone(), record);
                }
                crate::session::store::save_locked_session_store(&path, store)
            })
            .unwrap();
        }

        fn load(&self) -> CutexSessionStore {
            crate::session::store::load_cutex_session_store_from_path(&self.store()).unwrap()
        }

        fn record(&self) -> serde_json::Value {
            serde_json::from_slice(
                &fs::read(self.dir.join(ARCHIVE_ROOT).join(RECORD_FILE)).unwrap(),
            )
            .unwrap()
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn session(
        name: &str,
        class: AgentRegistrationClass,
        exposed: bool,
        retired: bool,
    ) -> CutexSessionRecord {
        let mut record = CutexSessionRecord::new_at(
            format!("cutex.{name}"),
            None,
            "host".to_string(),
            "/tmp/work".to_string(),
            None,
            "2026-10-01T00:00:00Z".to_string(),
        )
        .unwrap();
        record.thread_name = Some(name.to_string());
        record.display_name_hint = Some(name.to_string());
        record.registration_class = class;
        record.exposed_to_backend = exposed;
        if retired {
            record.archive_state = crate::session::model::CutexSessionArchiveState::Retired;
        }
        record
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-15T01:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn sets_the_flag_on_active_persistent_sessions_once_and_records_them() {
        let home = TempHome::new();
        home.seed(vec![
            session("worker-a", AgentRegistrationClass::Persistent, false, false),
            session("worker-b", AgentRegistrationClass::LocalOnly, false, false),
            session("worker-c", AgentRegistrationClass::Persistent, true, false),
            session("one-shot", AgentRegistrationClass::Ephemeral, false, false),
            session("archived", AgentRegistrationClass::Persistent, false, true),
        ]);
        let before = home.load();
        let changes = run_at(&home.dir, &home.store(), "test", now()).unwrap().unwrap();
        let ids = changes.iter().map(|c| c.cutex_session_id.as_str()).collect::<Vec<_>>();
        assert_eq!(ids, ["cutex.worker-a"]);

        let after = home.load();
        assert_eq!(after.sessions["cutex.worker-b"], before.sessions["cutex.worker-b"], "local-only untouched");
        for id in ["cutex.worker-a"] {
            assert!(after.sessions[id].exposed_to_backend, "{id}");
            assert!(
                after.sessions[id].durable_revision() > before.sessions[id].durable_revision(),
                "{id}: revision bumped"
            );
        }
        assert_eq!(after.sessions["cutex.worker-c"], before.sessions["cutex.worker-c"]);
        assert!(!after.sessions["cutex.one-shot"].exposed_to_backend);
        assert!(!after.sessions["cutex.archived"].exposed_to_backend);

        let record = home.record();
        assert_eq!(record["schema"], RECORD_SCHEMA);
        assert_eq!(record["committed"], true);
        assert_eq!(record["sessions"][0]["cutexSessionId"], "cutex.worker-a");
        assert_eq!(record["sessions"][0]["name"], "worker-a");

        // Done: a flag cleared afterwards (IM mirror only) is not set again.
        let mut store = home.load();
        store.sessions.get_mut("cutex.worker-a").unwrap().exposed_to_backend = false;
        let path = home.store();
        crate::session::store::with_locked_session_store(&path, |locked| {
            *locked = store.clone();
            crate::session::store::save_locked_session_store(&path, locked)
        })
        .unwrap();
        assert!(run_at(&home.dir, &home.store(), "test", now()).unwrap().is_none());
        assert!(!home.load().sessions["cutex.worker-a"].exposed_to_backend);
        assert_eq!(home.record()["sessions"].as_array().unwrap().len(), 1, "record kept");
    }

    #[test]
    fn an_uncommitted_record_reruns_and_converges() {
        let home = TempHome::new();
        home.seed(vec![session("worker-a", AgentRegistrationClass::Persistent, false, false)]);
        let archive = home.dir.join(ARCHIVE_ROOT);
        fs::create_dir_all(&archive).unwrap();
        write_record(
            &archive.join(RECORD_FILE),
            &MigrationRecord {
                schema: RECORD_SCHEMA.into(),
                at: "2026-10-15T00:00:00Z".into(),
                component: "test".into(),
                committed: false,
                error: Some("simulated commit failure".into()),
                sessions: Vec::new(),
            },
        )
        .unwrap();
        let changes = run_at(&home.dir, &home.store(), "test", now()).unwrap().unwrap();
        assert_eq!(changes.len(), 1);
        assert!(home.load().sessions["cutex.worker-a"].exposed_to_backend);
        assert_eq!(home.record()["committed"], true);
        assert!(run_at(&home.dir, &home.store(), "test", now()).unwrap().is_none());
    }

    #[test]
    fn a_home_with_nothing_to_change_is_marked_done() {
        let home = TempHome::new();
        home.seed(vec![session("worker-c", AgentRegistrationClass::Persistent, true, false)]);
        let changes = run_at(&home.dir, &home.store(), "test", now()).unwrap().unwrap();
        assert!(changes.is_empty());
        let record = home.record();
        assert_eq!((record["committed"].clone(), record["sessions"].clone()), (serde_json::json!(true), serde_json::json!([])));
        assert!(run_at(&home.dir, &home.store(), "test", now()).unwrap().is_none());
    }
}
