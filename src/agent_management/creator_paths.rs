//! Creation-path custody across Online/Restart, including deleted creators.
use super::envelope::{resolve_path, PermissionEnvelope, SandboxLevel};
use crate::session::model::{CutexSessionRecord, SessionCreatedBy};
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};

pub(crate) const CHANGED:&str="the folder changed since the agent was created or exceeds its creator's writable paths; the owner can re-approve it with cutex session reapprove-paths";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub cwd: String,
    pub writable_roots: Vec<String>,
    #[serde(default)]
    pub owner_approved: bool,
}
impl Snapshot {
    pub fn capture(record: &CutexSessionRecord, owner_approved: bool) -> anyhow::Result<Self> {
        let resolve = |path: &str| -> anyhow::Result<String> {
            let path = resolve_path(std::path::Path::new(path)).context(CHANGED)?;
            Ok(crate::session::metadata::session_cwd_identity(
                path.to_str().context(CHANGED)?,
            ))
        };
        Ok(Self {
            cwd: resolve(crate::session::service::cutex_session_launch_cwd(record))?,
            writable_roots: record
                .sandbox_writable_roots
                .iter()
                .map(|path| resolve(path))
                .collect::<anyhow::Result<_>>()?,
            owner_approved,
        })
    }
}

/// Call only for an explicit owner permission/path mutation, never an agent edit.
pub fn owner_reapprove(record: &mut CutexSessionRecord) -> anyhow::Result<()> {
    if matches!(record.created_by, Some(SessionCreatedBy::Agent { .. })) {
        record.creator_path_snapshot = Some(Snapshot::capture(record, true)?);
    }
    Ok(())
}

fn check_inner(
    record: &CutexSessionRecord,
    creator: Option<&CutexSessionRecord>,
) -> anyhow::Result<()> {
    if !matches!(record.created_by, Some(SessionCreatedBy::Agent { .. })) {
        return Ok(());
    }
    if let Some(saved) = &record.creator_path_snapshot {
        let current = Snapshot::capture(record, saved.owner_approved)?;
        ensure!(current == *saved, "{CHANGED}");
        if saved.owner_approved {
            return Ok(());
        }
    } else {
        eprintln!(
            "legacy agent-created session {} has no resolved path snapshot; allowing legacy launch",
            record.cutex_session_id
        );
        return Ok(());
    }
    let Some(creator) = creator else {
        return Ok(());
    };
    // Read-only children do not receive writable cwd/root access.
    if PermissionEnvelope::from_record(record)
        .is_ok_and(|envelope| envelope.sandbox == SandboxLevel::ReadOnly)
    {
        return Ok(());
    }
    let envelope =
        PermissionEnvelope::from_record(creator).map_err(|_| anyhow::anyhow!(CHANGED))?;
    if envelope.sandbox == SandboxLevel::FullAccess {
        return Ok(());
    }
    ensure!(
        envelope.may_write(crate::session::service::cutex_session_launch_cwd(record))
            && record
                .sandbox_writable_roots
                .iter()
                .all(|path| envelope.may_write(path)),
        "{CHANGED}"
    );
    Ok(())
}

pub fn changed_error(record: &CutexSessionRecord) -> anyhow::Error {
    super::AgentManagementError::CreatorPathsChanged(record.cutex_session_id.clone()).into()
}

pub fn changed_session_id(error: &anyhow::Error) -> Option<&str> {
    match error.downcast_ref::<super::AgentManagementError>()? {
        super::AgentManagementError::CreatorPathsChanged(id) => Some(id),
        _ => None,
    }
}

pub fn check(
    record: &CutexSessionRecord,
    creator: Option<&CutexSessionRecord>,
) -> anyhow::Result<()> {
    check_inner(record, creator).map_err(|_| changed_error(record))
}

// Backfill only paths that can be resolved sanely within the current creator
// envelope. Legacy children outside it remain allowed, with no pinned snapshot.
fn legacy_snapshot(
    record: &CutexSessionRecord,
    creator: Option<&CutexSessionRecord>,
) -> Option<Snapshot> {
    let cwd = crate::session::service::cutex_session_launch_cwd(record);
    for path in std::iter::once(cwd).chain(record.sandbox_writable_roots.iter().map(String::as_str))
    {
        crate::artifacts::paths::validate_local_path(std::path::Path::new(path)).ok()?;
    }
    let snapshot = Snapshot::capture(record, false).ok()?;
    if let Some(creator) = creator {
        let envelope = PermissionEnvelope::from_record(creator).ok()?;
        if !envelope.may_write(cwd)
            || !record
                .sandbox_writable_roots
                .iter()
                .all(|path| envelope.may_write(path))
        {
            return None;
        }
    }
    Some(snapshot)
}

fn same_paths(left: &CutexSessionRecord, right: &CutexSessionRecord) -> bool {
    left.revision == right.revision
        && left.created_by == right.created_by
        && crate::session::service::cutex_session_launch_cwd(left)
            == crate::session::service::cutex_session_launch_cwd(right)
        && left.sandbox_writable_roots == right.sandbox_writable_roots
        && crate::runtime::args::effective_runtime_permission_defaults(left)
            == crate::runtime::args::effective_runtime_permission_defaults(right)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::model::CutexSessionStore;
    fn persist(parent: Option<&CutexSessionRecord>, child: &CutexSessionRecord) {
        let mut store = CutexSessionStore::default();
        if let Some(parent) = parent {
            store
                .sessions
                .insert(parent.cutex_session_id.clone(), parent.clone());
        }
        store
            .sessions
            .insert(child.cutex_session_id.clone(), child.clone());
        crate::session::store::save_cutex_session_store(&store).unwrap();
    }

    #[test]
    fn legacy_first_start_saves_snapshot_without_changing_launch_revision() {
        let home = crate::profiles::lifecycle::tests::TestHome::new();
        let (parent, mut child) = fixture(home.root());
        child.creator_path_snapshot = None;
        persist(Some(&parent), &child);
        before_start(&child).unwrap();
        let path = crate::session::store::cutex_sessions_path().unwrap();
        let (_, saved) = crate::session::store::load_session_row(&path, &child.cutex_session_id)
            .unwrap()
            .unwrap();
        assert_eq!(saved.revision, child.revision);
        assert_eq!(saved.updated_at, child.updated_at);
        assert!(!saved.creator_path_snapshot.as_ref().unwrap().owner_approved);
        assert_eq!(
            saved.creator_path_snapshot,
            Some(Snapshot::capture(&child, false).unwrap())
        );
        let store_revision = crate::session::store::load_cutex_session_store().unwrap().store_revision.get();
        before_start(&child).unwrap();
        assert_eq!(crate::session::store::load_cutex_session_store().unwrap().store_revision.get(), store_revision);
    }

    #[test]
    fn legacy_deleted_creator_is_backfilled_when_paths_are_sane() {
        let home = crate::profiles::lifecycle::tests::TestHome::new();
        let (_, mut child) = fixture(home.root());
        child.creator_path_snapshot = None;
        persist(None, &child);
        before_start(&child).unwrap();
        let path = crate::session::store::cutex_sessions_path().unwrap();
        assert!(
            crate::session::store::load_session_row(&path, &child.cutex_session_id)
                .unwrap()
                .unwrap()
                .1
                .creator_path_snapshot
                .is_some()
        );
    }

    #[test]
    fn legacy_outside_creator_still_starts_with_a_warning_and_no_snapshot() {
        const FLAG: &str = "CUTEX_CREATOR_PATH_WARNING_PROBE";
        if std::env::var_os(FLAG).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "agent_management::creator_paths::tests::legacy_outside_creator_still_starts_with_a_warning_and_no_snapshot", "--nocapture", "--test-threads=1"])
                .env(FLAG, "1").output().unwrap();
            assert!(
                output.status.success(),
                "{} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stderr).contains("allowing legacy launch"));
            return;
        }
        let home = crate::profiles::lifecycle::tests::TestHome::new();
        let (parent, mut child) = fixture(home.root());
        child.cwd = home.root().join("outside").to_str().unwrap().into();
        child.creator_path_snapshot = None;
        persist(Some(&parent), &child);
        before_start(&child).unwrap();
        let path = crate::session::store::cutex_sessions_path().unwrap();
        assert!(
            crate::session::store::load_session_row(&path, &child.cutex_session_id)
                .unwrap()
                .unwrap()
                .1
                .creator_path_snapshot
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn backfilled_snapshot_blocks_swaps_even_with_a_stale_legacy_clone() {
        let home = crate::profiles::lifecycle::tests::TestHome::new();
        let (parent, mut child) = fixture(home.root());
        child.creator_path_snapshot = None;
        persist(Some(&parent), &child);
        before_start(&child).unwrap();
        let path = std::path::PathBuf::from(&child.cwd);
        std::fs::rename(&path, home.root().join("original-child")).unwrap();
        std::os::unix::fs::symlink(home.root().join("outside"), &path).unwrap();
        let error = before_start(&child).unwrap_err();
        assert_eq!(
            changed_session_id(&error),
            Some(child.cutex_session_id.as_str())
        );
        let error = error
            .downcast_ref::<super::super::AgentManagementError>()
            .unwrap();
        assert_eq!(error.code(), "creator_paths_changed");
        let approved = reapprove_at(
            &crate::session::store::cutex_sessions_path().unwrap(),
            &child.cutex_session_id,
        )
        .unwrap();
        assert!(
            approved
                .creator_path_snapshot
                .as_ref()
                .unwrap()
                .owner_approved
        );
        assert_eq!(approved.revision, child.revision + 1);
        before_start(&approved).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(home.root().join("original-child"), &path).unwrap();
        assert!(before_start(&approved).is_err());
    }
    fn fixture(root: &std::path::Path) -> (CutexSessionRecord, CutexSessionRecord) {
        let work = root.join("workspace");
        let child = work.join("child");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();
        let mut parent = CutexSessionRecord::new(
            "cutex.path-parent".into(),
            None,
            "test".into(),
            work.to_str().unwrap().into(),
            None,
        )
        .unwrap();
        parent.sandbox_mode = Some("workspace-write".into());
        parent.approval_policy = Some("on-request".into());
        let mut record = CutexSessionRecord::new(
            "cutex.path-child".into(),
            None,
            "test".into(),
            child.to_str().unwrap().into(),
            None,
        )
        .unwrap();
        record.sandbox_mode = Some("workspace-write".into());
        record.approval_policy = Some("on-request".into());
        record
            .stamp_created_by_at_creation(Some(&SessionCreatedBy::Agent {
                cutex_session_id: crate::role_revision::CutexSessionId::new(
                    parent.cutex_session_id.clone(),
                )
                .unwrap(),
                role: None,
            }))
            .unwrap();
        (parent, record)
    }
    #[cfg(unix)]
    #[test]
    fn start_refuses_swapped_cwd_with_existing_or_deleted_creator_and_owner_can_reapprove() {
        let home = crate::profiles::lifecycle::tests::TestHome::new();
        let (parent, mut child) = fixture(home.root());
        assert!(check(&child, Some(&parent)).is_ok());
        assert!(check(&child, None).is_ok());
        let path = std::path::PathBuf::from(&child.cwd);
        std::fs::rename(&path, home.root().join("original-child")).unwrap();
        std::os::unix::fs::symlink(home.root().join("outside"), &path).unwrap();
        for creator in [Some(&parent), None] {
            let error = check(&child, creator).unwrap_err().to_string();
            assert!(error.contains("folder changed since the agent was created"));
            assert!(error.contains("owner can re-approve"));
        }
        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert(parent.cutex_session_id.clone(), parent.clone());
        store
            .sessions
            .insert(child.cutex_session_id.clone(), child.clone());
        crate::session::store::save_cutex_session_store(&store).unwrap();
        let account = serde_json::from_value(
            serde_json::json!({"id":"test","name":"test","cli_kind":"codex"}),
        )
        .unwrap();
        let plan = crate::runtime::lifecycle::SessionOnlineResumePlan {
            effective_args: vec![],
            groups: vec![],
            codex_session_id: "thread".into(),
            launch_cwd: child.cwd.clone(),
        };
        assert!(crate::runtime::lifecycle::finalize_session_online_launch(
            &child,
            &account,
            crate::launch::command::LaunchCommand::new("unused"),
            &plan
        )
        .unwrap_err()
        .to_string()
        .contains("folder changed"));
        owner_reapprove(&mut child).unwrap();
        assert!(
            check(&child, Some(&parent)).is_ok(),
            "explicit owner approval overrides creator roots"
        );
        assert!(check(&child, None).is_ok());
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(home.root().join("original-child"), &path).unwrap();
        assert!(
            check(&child, None).is_err(),
            "owner approval still refuses a later swap"
        );
    }
    #[cfg(unix)]
    #[test]
    fn writable_root_swaps_are_refused_and_unchanged_paths_follow_current_creator_roots() {
        let home = crate::profiles::lifecycle::tests::TestHome::new();
        let (mut parent, mut child) = fixture(home.root());
        let extra = home.root().join("workspace/extra");
        std::fs::create_dir_all(&extra).unwrap();
        child.sandbox_writable_roots = vec![extra.to_str().unwrap().into()];
        child.creator_path_snapshot = Some(Snapshot::capture(&child, false).unwrap());
        assert!(check(&child, Some(&parent)).is_ok());
        parent.cwd = home.root().join("outside").to_str().unwrap().into();
        assert!(check(&child, Some(&parent)).is_err());
        assert!(check(&child, None).is_ok());
        std::fs::rename(&extra, home.root().join("original-extra")).unwrap();
        std::os::unix::fs::symlink(home.root().join("outside"), &extra).unwrap();
        assert!(check(&child, None).is_err());
    }
    #[test]
    fn legacy_records_are_allowed_and_owner_default_edits_refresh_the_snapshot() {
        let home = crate::profiles::lifecycle::tests::TestHome::new();
        let (parent, mut child) = fixture(home.root());
        let encoded = serde_json::to_value(&child).unwrap();
        assert!(encoded.get("creator_path_snapshot").is_some());
        let roundtrip: CutexSessionRecord = serde_json::from_value(encoded).unwrap();
        assert_eq!(roundtrip.creator_path_snapshot, child.creator_path_snapshot);
        child.creator_path_snapshot = None;
        assert!(check(&child, Some(&parent)).is_ok());
        assert!(check(&child, None).is_ok());
        let mut store = CutexSessionStore::default();
        let id = child.cutex_session_id.clone();
        store.sessions.insert(id.clone(), child);
        crate::session::service::update_cutex_session_runtime_defaults(
            &mut store,
            &id,
            crate::session::service::CutexSessionRuntimeDefaultsPatch {
                managed_cwd: crate::session::service::CutexSessionValueUpdate::Set(
                    home.root().join("outside").to_str().unwrap().into(),
                ),
                ..Default::default()
            },
        )
        .unwrap();
        let child = &store.sessions[&id];
        assert!(child.creator_path_snapshot.as_ref().unwrap().owner_approved);
        assert!(check(child, Some(&parent)).is_ok());
    }
}

/// Runtime entry points call this outside session-store write locks.
pub fn before_start(record: &CutexSessionRecord) -> anyhow::Result<()> {
    let Some(SessionCreatedBy::Agent {
        cutex_session_id, ..
    }) = &record.created_by
    else {
        return Ok(());
    };
    if record
        .creator_path_snapshot
        .as_ref()
        .is_some_and(|snapshot| snapshot.owner_approved)
    {
        return check(record, None);
    }
    let path = crate::session::store::cutex_sessions_path()?;
    let creator = crate::session::store::load_session_row(&path, cutex_session_id.as_str())?;
    let creator_key = creator.as_ref().map(|(key, _)| key.clone());
    let creator = creator.as_ref().map(|(_, record)| record);
    if record.creator_path_snapshot.is_some() {
        return check(record, creator);
    }
    // A second start hook may still hold the pre-backfill record. Never let
    // that stale clone bypass a snapshot written by the first hook.
    let stored = crate::session::store::load_session_row(&path, &record.cutex_session_id)?;
    if let Some(saved) = stored
        .as_ref()
        .and_then(|(_, record)| record.creator_path_snapshot.as_ref())
    {
        let mut current = record.clone();
        current.creator_path_snapshot = Some(saved.clone());
        return check(&current, creator);
    }
    let Some((key, _)) = stored.as_ref() else {
        return check(record, creator);
    };
    let Some(snapshot) = legacy_snapshot(record, creator) else {
        return check(record, creator);
    };
    let saved = crate::session::store::with_locked_session_rows(&path, |rows| {
        let Some(current) = rows.session(key) else {
            return Ok(None);
        };
        if let Some(saved) = &current.creator_path_snapshot {
            return Ok(Some(saved.clone()));
        }
        if !same_paths(current, record) {
            return Ok(None);
        }
        if let Some(creator) = creator {
            if rows
                .session(creator_key.as_deref().expect("loaded creator key"))
                .is_none_or(|current| !same_paths(current, creator))
            {
                return Ok(None);
            }
        } else if rows.session(cutex_session_id.as_str()).is_some() {
            return Ok(None);
        }
        rows.session_mut(key)
            .expect("checked session")
            .creator_path_snapshot = Some(snapshot.clone());
        // Internal path metadata, not a new owner configuration: keep the
        // revision/observation fields stable for the in-flight launch CAS.
        rows.commit()?;
        Ok(Some(snapshot))
    })?;
    let mut checked = record.clone();
    checked.creator_path_snapshot = saved;
    check(&checked, creator)
}

/// Explicit owner action; callers must authenticate the full-scope owner.
pub fn reapprove_at(path: &std::path::Path, id: &str) -> anyhow::Result<CutexSessionRecord> {
    let (key, record) =
        crate::session::store::load_session_row(path, id)?.context("session_not_found")?;
    ensure!(
        matches!(record.created_by, Some(SessionCreatedBy::Agent { .. })),
        "session_not_agent_created"
    );
    ensure!(!record.is_retired(), "session_retired");
    let snapshot = Snapshot::capture(&record, true).map_err(|_| changed_error(&record))?;
    crate::session::store::with_locked_session_rows(path, |rows| {
        let current = rows.session_mut(&key).context("session_not_found")?;
        ensure!(
            same_paths(current, &record),
            "session_configuration_changed"
        );
        current.creator_path_snapshot = Some(snapshot);
        current.bump_durable_revision()?;
        current.updated_at = chrono::Utc::now().to_rfc3339();
        let result = current.clone();
        rows.commit()?;
        Ok(result)
    })
}
