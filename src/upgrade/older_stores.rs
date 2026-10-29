//! Rollback swap step (services stopped): rewrite the few records an older
//! cutex cannot parse at all, or would read with more rights than they
//! carry, so it loads the rest of its stores and never widens access. Every
//! record changed here is copied to `displaced/older-stores/` first (never
//! deleted); a roll-forward does not need them back.
//!
//! Each step names the release that introduced its records (`since`) and
//! runs when the rollback target is older. Supported target syntax is a plain
//! `N.N.N` public version, r252..r259, r262, or the exact predecessors r263/r264;
//! older/unknown/dev labels fail closed before any compatibility write.
//!
//! Since 0.1.4:
//! - Approval records of kind `static_root`: an older release rejects the
//!   whole `runtime/approvals/approvals.json` ("approval store is corrupt").
//!   They are removed; a pending one is simply asked again.
//! - `ClaudeSession.initial_permission_mode` (pending until the agent's
//!   first successful start): `ClaudeSession` is `deny_unknown_fields`, so an
//!   older release cannot load any session. The field is cleared; the agent
//!   then starts in the Profile's default mode, as it would have on that
//!   release.
//!
//! Since 0.1.5:
//! - Client / device tokens with a device scope other than `full`
//!   (`management-clients/tokens.json`): an older release ignores `scope`, so
//!   the device would get its user's full rights. Each is held
//!   (`client_tokens::ROLLBACK_HOLD_PREFIX`): the older release refuses it
//!   (401) and keeps the hold when it rewrites the store; 0.1.5 and later read
//!   it again with its scope, so the device works again after a roll-forward
//!   without re-pairing. Kept aside: `client-tokens-held.json` (ids, labels,
//!   scopes; never a secret or hash).
//! - Pending cross-host wakes (`cutex.peer.tell.v2` in
//!   `runtime/management-v2/agent-bus-message-state.json`) never handed to a
//!   runtime: an older release never delivers the unknown control type but
//!   lists the wake as an eligible next step forever (owner input held ~30 s
//!   per message, new delegations to the agent held). Each becomes the
//!   cross-host note it would have been without `wake` (read at the
//!   recipient's next turn; no turn is started).
//! - Unacknowledged new owner device notices (`newOwnerDevices` in
//!   `runtime/users/users-v1.json`): an older release drops them on its next
//!   write of the store. Only copied aside (`new-owner-devices.json`), so the
//!   owner can still review those devices; the store is not changed.

use std::path::Path;

use anyhow::Context;
use serde_json::json;

use crate::director_address::management::ExtensionStateStore;

const APPROVALS: &str = "runtime/approvals/approvals.json";
const SESSIONS: &str = "cutex-sessions.json";
const CLIENT_TOKENS: &str = "management-clients";
const AGENT_BUS: &str = "runtime/management-v2";
const AGENT_BUS_STATE: &str = "runtime/management-v2/agent-bus-message-state.json";
const USERS: &str = "runtime/users/users-v1.json";
const OUT_DIR: &str = "older-stores";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RollbackTarget {
    Release(u64),
    Public(u64, u64, u64),
}

impl RollbackTarget {
    /// Every supported internal rollback target is r252 or newer and reads
    /// both 0.1.4 stored contracts.
    fn supports_0_1_4_state(self) -> bool {
        match self {
            Self::Release(_) => true,
            Self::Public(major, minor, patch) => (major, minor, patch) >= (0, 1, 4),
        }
    }

    /// Every supported internal rollback target also reads the 0.1.5 stored
    /// contracts: narrowed scopes, cross-host wakes and device notices.
    fn supports_0_1_5_state(self) -> bool {
        match self {
            Self::Release(_) => true,
            Self::Public(major, minor, patch) => (major, minor, patch) >= (0, 1, 5),
        }
    }
}

fn rollback_target(target: &str) -> anyhow::Result<RollbackTarget> {
    if let Some(number) = target.strip_prefix('r') {
        let number = number.parse::<u64>().with_context(|| format!("unknown rollback target {target:?}"))?;
        anyhow::ensure!(!matches!(number, 263 | 264) || matches!(target, "r263" | "r264"), "the predecessor target must be spelled exactly r263 or r264");
        anyhow::ensure!(
            number >= 252,
            "internal rollback target {target} is older than minimum supported r252; use only the kit for the build immediately before activation"
        );
        anyhow::ensure!(number <= 259 || matches!(number, 262 | 263 | 264), "target {target} is not an older supported rollback target");
        return Ok(RollbackTarget::Release(number));
    }
    let parts = target
        .split('.')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("unknown rollback target {target:?}"))?;
    anyhow::ensure!(parts.len() == 3, "unknown rollback target {target:?}");
    anyhow::ensure!((parts[0], parts[1], parts[2]) < (0, 2, 0), "target {target} already reads the r257 SQLite hot stores");
    Ok(RollbackTarget::Public(parts[0], parts[1], parts[2]))
}

/// Validate the spelling and supported range without touching rollback data.
pub fn validate_target(target: &str) -> anyhow::Result<()> {
    rollback_target(target).map(|_| ())
}

/// Read-only proof that every compatibility conversion can safely start.
/// Standalone rollback export calls this before creating its backup tree;
/// the stopped-writer prepare/export path repeats it immediately before any
/// compatibility mutation.
pub fn preflight(home: &Path, target: &str) -> anyhow::Result<()> {
    rollback_target(target)?;
    preflight_kimi_profiles(home, target)?;
    preflight_target_stores(home, target)?;
    // After the target's own store checks (which refuse damaged stores first).
    preflight_kimi_sessions(home, target)
}

fn preflight_target_stores(home: &Path, target: &str) -> anyhow::Result<()> {
    match super::r265_rollback::Reader::for_target(target) {
        Some(super::r265_rollback::Reader::R263) => return preflight_preserved_stores(home, super::r265_rollback::Reader::R263),
        Some(super::r265_rollback::Reader::R264) => return super::r265_rollback::preflight_r264(home),
        None => {}
    }
    if rollback_target(target)? == RollbackTarget::Release(262) {
        preflight_owner_result_notes(home, target)?;
        // r262 already reads the same hot stores, cold asks archive, DSH,
        // pause and approval contracts. Validate without legacy conversion.
        return preflight_hot_store_exports_with_archive(home, true);
    }
    preflight_before_hot_stores(home,target)?;
    preflight_hot_store_exports(home)
}

/// The stopped r262 -> r259 archive restoration has not restored asks yet.
/// Keep every other downgrade gate, but validate all asks history instead
/// of recursively demanding that the restoration already happened.
pub fn preflight_asks_archive_restore(home:&Path,target:&str)->anyhow::Result<()> {
    anyhow::ensure!(target=="r259","archive restoration preflight only supports the preceding r259 kit");
    preflight_before_hot_stores(home,target)?;
    preflight_hot_store_exports_with_archive(home,true)
}

fn preflight_before_hot_stores(home:&Path,target:&str)->anyhow::Result<()> {
    preflight_owner_result_notes(home, target)?;
    preflight_r260_state(home, target)?;
    let paused = crate::asks::store::AskStore::at_runtime(&home.join("runtime")).inspect(|state| {
        state.presence.values().filter(|presence| presence.agent_pause.is_some()).count()
    })?;
    anyhow::ensure!(
        paused == 0,
        "target {target} cannot safely preserve {paused} durable agent pause(s); resume every paused agent before rollback"
    );
    crate::upgrade::avatar_compat::preflight(home)?;
    anyhow::ensure!(
        !crate::management::v2::guides_preferences::has_previous_build_rollback_blockers(&home.join("runtime/users"))?,
        "target {target} cannot safely preserve guide preferences; fix forward"
    );
    anyhow::ensure!(
        !crate::services::store::ServiceStore::new(home.join("services"), "rollback-preflight")
            .has_previous_build_rollback_blockers()?,
        "target {target} cannot safely preserve r258 service retention or owner organisation; clear that state or fix forward"
    );
    // Design 047 deliberately has no lossy downgrade. An older binary would
    // drop rules/audit on its next approval-store write, so refuse before any mutation.
    anyhow::ensure!(
        !crate::approval_rules::has_rollback_blockers(&home.join(APPROVALS))?,
        "target {target} cannot safely preserve Cutex approval rules or their audit; revoke/export them and fix forward"
    );
    preflight_legacy_target(home, target)?;
    Ok(())
}

/// Older schedulers mistake the passive owner-context lease for an exclusive
/// result turn. Refuse before conversion; never erase/requeue its native identity.
fn preflight_owner_result_notes(home: &Path, target: &str) -> anyhow::Result<()> {
    // r263 already implements passive owner context and its lease/delivery IDs.
    if rollback_target(target)? == RollbackTarget::Release(263) { return Ok(()); }
    let runtime = home.join("runtime");
    let database = runtime.join("management-v2").join(crate::management::v2::hot_store::DATABASE_FILE);
    // Do not use AskStore::inspect/shared here: legacy reads can migrate JSON.
    let state = if let Some(loaded) = crate::management::v2::hot_store::verified_rows(&database, "asks")? {
        let rows = if crate::asks::archive::persisted_cutoff(&database)?.is_some() {
            crate::asks::archive::history_rows(&database)?
        } else { loaded.rows };
        crate::asks::store::ask_state_from_rows(&rows)?
    } else {
        let path = runtime.join("asks/asks-v1.json");
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("ask rollback preflight: corrupt legacy state")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => crate::asks::store::AskState::default(),
            Err(error) => return Err(error).context("read ask rollback preflight state"),
        }
    };
    let owner_note = |id: &str| id.starts_with("cutex-ask-owner-");
    let owner_events: std::collections::HashSet<_> = state.events.values()
        .filter(|event| owner_note(&event.native_message_id)).map(|event| event.event_id.as_str())
        .chain(state.leases.values().chain(state.next_step_leases.values())
            .filter(|lease| owner_note(&lease.native_message_id)).filter_map(|lease| match &lease.holder {
                crate::asks::model::LeaseHolder::Event { event_id } => Some(event_id.as_str()), _ => None,
            })).collect();
    let unresolved = state.events.values().any(|event| owner_note(&event.native_message_id) && !event.acknowledged)
        || state.leases.values().chain(state.next_step_leases.values()).any(|lease| owner_note(&lease.native_message_id))
        || state.deliveries.values().any(|delivery| delivery.state == crate::asks::model::DeliveryState::QueuedAsEvent
            && delivery.event_id.as_deref().is_some_and(|id| owner_events.contains(id) || owner_note(id)));
    anyhow::ensure!(!unresolved,
        "target {target} cannot preserve unresolved owner-result notes (cutex-ask-owner-*); drain and confirm their delivery on r263 before rollback; no records were changed");
    Ok(())
}

/// r266 Kimi-kind profiles (`cli_kind: "kimi"`): every supported rollback
/// target predates them and its closed profile enum would reject the whole
/// accounts store. Refuse before any output (like DSH before r260); the owner
/// deletes the Kimi profiles (Kimi accounts and `kimi` variants on API-key
/// profiles are ignored by older releases) or keeps r266.
fn preflight_kimi_profiles(home: &Path, target: &str) -> anyhow::Result<()> {
    let accounts_path = home.join("accounts.json");
    if !accounts_path.is_file() {
        return Ok(());
    }
    let accounts: crate::profiles::model::AccountsStore = serde_json::from_slice(
        &std::fs::read(&accounts_path).with_context(|| format!("read {}", accounts_path.display()))?,
    )
    .with_context(|| format!("decode {}", accounts_path.display()))?;
    let kimi = accounts
        .accounts
        .iter()
        .filter(|account| account.cli_kind == crate::profiles::model::CliKind::Kimi)
        .count();
    anyhow::ensure!(
        kimi == 0,
        "target {target} cannot read {kimi} Kimi profile(s); keep r266 or delete the Kimi profiles before rollback"
    );
    Ok(())
}

/// r266 Kimi sessions (`runtime_backend: "kimi"`, also on API-key profiles'
/// `kimi` variant): every supported target's closed backend enum rejects the
/// record, so the whole session store would not decode. Refuse before output.
/// The store is read from a private disposable copy (opening it in place may
/// import JSON into SQLite or touch WAL files; rollback inputs stay untouched).
fn preflight_kimi_sessions(home: &Path, target: &str) -> anyhow::Result<()> {
    let names = ["cutex-sessions.json", "cutex-sessions.sqlite3", "cutex-sessions.sqlite3-wal", "cutex-sessions.sqlite3-shm"];
    let present = names.iter().filter(|name| home.join(name).is_file()).collect::<Vec<_>>();
    if present.is_empty() {
        return Ok(());
    }
    let temp = std::fs::canonicalize(std::env::temp_dir())?;
    anyhow::ensure!(!temp.starts_with(std::fs::canonicalize(home)?), "rollback validation TEMP must be outside the source home");
    let scratch = temp.join(format!("cutex-rollback-kimi-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&scratch)?;
    let counted = (|| -> anyhow::Result<usize> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o700))?;
        }
        #[cfg(windows)]
        crate::platform::private_fs::secure_directory(&scratch)?;
        for name in &present {
            std::fs::copy(home.join(name), scratch.join(name)).with_context(|| format!("copy {name} for validation"))?;
        }
        crate::session::store::with_cutex_session_store_at(&scratch.join(SESSIONS), |store| {
            store
                .sessions
                .values()
                .filter(|record| record.runtime_backend == crate::session::model::CutexSessionRuntimeBackend::Kimi)
                .count()
        })
    })();
    std::fs::remove_dir_all(&scratch).context("remove private rollback validation copy")?;
    let kimi = counted?;
    anyhow::ensure!(
        kimi == 0,
        "target {target} cannot read {kimi} Kimi session(s); keep r266 or delete the Kimi agents before rollback"
    );
    Ok(())
}

/// r259 predates the DSH profile/session contracts. Its profile enum rejects a
/// DSH profile, and its session backend enum rejects a DSH record. Do not
/// rewrite either contract into a weaker shape: refuse before any rollback
/// output so the owner can keep r260 running or explicitly retire the DSH
/// state first. Default DSH options are omitted from non-DSH profiles and are
/// therefore compatible with r259.
fn preflight_r260_state(home: &Path, target: &str) -> anyhow::Result<()> {
    let accounts_path = home.join("accounts.json");
    if accounts_path.is_file() {
        let accounts: crate::profiles::model::AccountsStore = serde_json::from_slice(
            &std::fs::read(&accounts_path)
                .with_context(|| format!("read {}", accounts_path.display()))?,
        )
        .with_context(|| format!("decode {}", accounts_path.display()))?;
        let dsh_profiles = accounts
            .accounts
            .iter()
            .filter(|account| {
                account.cli_kind == crate::profiles::model::CliKind::Dsh
                    || account.options.dsh != crate::profiles::dsh::DshOptions::default()
            })
            .count();
        anyhow::ensure!(
            dsh_profiles == 0,
            "target {target} cannot safely preserve {dsh_profiles} DSH profile(s); keep r260 or retire the DSH profiles before rollback"
        );
    }

    let sessions_path = home.join(SESSIONS);
    let dsh_sessions = crate::session::store::with_cutex_session_store_at(&sessions_path, |store| {
        store
            .sessions
            .values()
            .filter(|record| {
                record.runtime_backend.is_acp() || record.dsh_session.is_some()
            })
            .count()
    })?;
    anyhow::ensure!(
        dsh_sessions == 0,
        "target {target} cannot safely preserve {dsh_sessions} DSH session(s); keep r260 or archive/retire the DSH sessions before rollback"
    );

    let management_path = home
        .join("runtime/agent-management/v1/agent-management-v1.json");
    if management_path.is_file() {
        let management: crate::agent_management::AgentManagementSnapshot = serde_json::from_slice(
            &std::fs::read(&management_path)
                .with_context(|| format!("read {}", management_path.display()))?,
        )
        .with_context(|| format!("decode {}", management_path.display()))?;
        let dsh_agents = management
            .agents
            .values()
            .filter(|agent| matches!(agent.spec.runtime_backend.as_str(), "dsh" | "kimi"))
            .count();
        anyhow::ensure!(
            dsh_agents == 0,
            "target {target} cannot safely preserve {dsh_agents} managed DSH agent(s); keep r260 or retire the DSH agents before rollback"
        );
    }
    Ok(())
}

/// `target`: the release label the rollback switches to.
pub fn prepare(home: &Path, displaced: &Path, target: &str) -> anyhow::Result<Vec<String>> {
    let target_capabilities = rollback_target(target)?;
    preflight(home, target)?;
    let mut notes = Vec::new();
    if !target_capabilities.supports_0_1_4_state() {
        since_0_1_4(home, displaced, &mut notes)?;
    }
    if !target_capabilities.supports_0_1_5_state() {
        since_0_1_5(home, displaced, &mut notes)?;
    }
    notes.extend(export_hot_stores(home, target, true)?);
    Ok(notes)
}

/// Rebuild the pre-r257 JSON views from verified SQLite rows. The normal
/// rollback runner calls this only after it has stopped the services; the
/// standalone CLI requires the same condition to be acknowledged explicitly.
pub fn export_hot_stores(home: &Path, target: &str, daemon_stopped: bool) -> anyhow::Result<Vec<String>> {
    anyhow::ensure!(daemon_stopped, "stop cutex services, then repeat with --daemon-stopped");
    preflight(home, target)?;
    if matches!(rollback_target(target)?, RollbackTarget::Release(262 | 263 | 264)) {
        return Ok(Vec::new());
    }
    let runtime = home.join("runtime");
    let management = runtime.join("management-v2");
    let database = management.join(crate::management::v2::hot_store::DATABASE_FILE);
    let asks = crate::asks::store::AskStore::at_runtime(&runtime);
    let has_asks = crate::management::v2::hot_store::generation(&database, "asks")?.is_some();
    let has_bus = crate::management::v2::hot_store::generation(&database, "agent_bus")?.is_some();
    let bus = has_bus.then(|| crate::management::v2::agent_bus_state::AgentBusMessageRepository::open(&management)).transpose()?;

    // Validate every source before publishing or handing off any one of them.
    // A failed decode must leave all r257 stores authoritative.
    let export_asks = has_asks && asks.verify_legacy_export()?;
    let export_bus = match &bus {
        Some(bus) => bus.verify_legacy_export()?,
        None => false,
    };
    let integration_database = management.join("integration-event-dedupe-v2.sqlite");
    let export_integration = integration_database.is_file();

    let mut notes = Vec::new();
    if export_integration {
        let output = management.join("integration-event-dedupe-v1.json");
        let count = crate::management::v2::integration_events::export_integration_dedupe_v1(&management, &output, daemon_stopped)?;
        notes.push(format!("runtime/management-v2/integration-event-dedupe-v1.json: exported {count} exact identity record(s)"));
    }
    if export_asks {
        if asks.export_legacy_json()? {
            notes.push("runtime/asks/asks-v1.json: exported from verified r257 SQLite rows".into());
        }
    }
    if export_bus {
        if bus.as_ref().expect("validated agent-bus export").export_legacy_json()? {
            notes.push("runtime/management-v2/agent-bus-message-state.json: exported from verified r257 SQLite rows".into());
        }
    }
    Ok(notes)
}

/// Validate every r257 source before any older-store compatibility rewrite.
/// The exporter repeats these checks under each store's lock, but this full
/// read-only pass prevents an unrelated corrupt hot row from leaving an
/// earlier compatibility mutation live after a refused rollback.
fn preflight_hot_store_exports(home: &Path) -> anyhow::Result<()> {
    preflight_hot_store_exports_with_archive(home,false)
}

/// This is a storage-format check, NOT permission for binary-only rollback.
/// The stopped-writer kit must still enforce archive reader compatibility,
/// pending terminal events and retry settlement obligations, and back them up.
/// Do not call normal repository open/history APIs here: they create locks,
/// secure directories or import legacy state even during an apparent read.
pub(super) fn preflight_preserved_stores(home: &Path, reader: super::r265_rollback::Reader) -> anyhow::Result<()> {
    let management = home.join("runtime/management-v2");
    let database = management.join(crate::management::v2::hot_store::DATABASE_FILE);
    let mut paths = vec![database.clone(), home.join("runtime/asks/asks-v1.json"),
        management.join("agent-bus-message-state.json"),
        management.join("integration-event-dedupe-v2.sqlite"),
        management.join("integration-event-dedupe-v1.json"),
        management.join("integration-event-dedupe-v2.warning")];
    for db in [&database, &management.join("integration-event-dedupe-v2.sqlite")] {
        for suffix in ["-wal", "-shm", "-journal"] {
            paths.push(std::path::PathBuf::from(format!("{}{suffix}", db.display())));
        }
    }
    let guard_paths = super::r264_rollback::paths(home)?;
    paths.extend(guard_paths.iter().cloned());
    let mut sources = Vec::new();
    for path in &paths {
        // exists() hides access errors; only a proven missing file is absence.
        for ancestor in path.ancestors().skip(1) {
            match std::fs::symlink_metadata(ancestor) {
                Ok(meta) => {
                    anyhow::ensure!(meta.is_dir() && !meta.file_type().is_symlink(), "non-directory/link rollback ancestor: {}", ancestor.display());
                    #[cfg(windows)] {
                        use std::os::windows::fs::MetadataExt;
                        anyhow::ensure!(meta.file_attributes() & 0x400 == 0, "reparse rollback ancestor: {}", ancestor.display());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).context("inspect rollback ancestor"),
            }
        }
        match std::fs::symlink_metadata(path) {
            Ok(meta) => {
                anyhow::ensure!(meta.is_file() && !meta.file_type().is_symlink(), "not a regular rollback input: {}", path.display());
                #[cfg(windows)] {
                    use std::os::windows::fs::MetadataExt;
                    anyhow::ensure!(meta.file_attributes() & 0x400 == 0, "reparse rollback input: {}", path.display());
                }
                std::fs::File::open(path).with_context(|| format!("read rollback input {}", path.display()))?;
                sources.push(path.clone());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("inspect rollback input"),
        }
    }
    if sources.is_empty() { return Ok(()); }
    let before = paths.iter().map(|p| super::r264_rollback::fingerprint(p)).collect::<anyhow::Result<Vec<_>>>()?;
    // SQLite READ_ONLY can still create/change WAL shared-memory files. Never
    // point a SQLite connection at the original home. Preserve WAL in a private
    // disposable validation copy; immutable=1 would silently ignore its rows.
    // Services/CLI writers must already be fenced by the stopped-writer kit.
    let temp = std::fs::canonicalize(std::env::temp_dir())?;
    let original = std::fs::canonicalize(home)?;
    anyhow::ensure!(!temp.starts_with(&original), "rollback validation TEMP must be outside the source home");
    let scratch = temp.join(format!("cutex-rollback-verify-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&scratch)?;
    struct Scratch(std::path::PathBuf);
    impl Drop for Scratch { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
    let scratch = Scratch(scratch);
    let result = (|| {
        #[cfg(windows)]
        crate::platform::private_fs::secure_directory(&scratch.0)?;
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&scratch.0, std::fs::Permissions::from_mode(0o700))?;
        }
        for path in &sources {
            let dest = scratch.0.join(path.strip_prefix(home)?);
            std::fs::create_dir_all(dest.parent().context("snapshot parent")?)?;
            std::fs::copy(path, &dest)?;
            let copied = super::r264_rollback::fingerprint(&dest)?.context("missing validation copy")?;
            let index = paths.iter().position(|p| p == path).context("missing validation input")?;
            let initial = before[index].as_ref().context("input disappeared before copy")?;
            anyhow::ensure!(copied.0 == initial.0 && copied.2 == initial.2, "rollback input changed while copying");
        }
        validate_preserved_snapshot(&scratch.0, reader)?;
        let after = paths.iter().map(|p| super::r264_rollback::fingerprint(p)).collect::<anyhow::Result<Vec<_>>>()?;
        anyhow::ensure!(before == after, "rollback input changed during validation");
        let mut current = super::r264_rollback::paths(home)?;
        let mut prior = guard_paths.clone();
        current.sort(); prior.sort();
        anyhow::ensure!(current == prior, "rollback input set changed during validation");
        Ok(())
    })();
    // Never leave a validation tree even on a refused/corrupt input.
    std::fs::remove_dir_all(&scratch.0).context("remove private rollback validation copy")?;
    result
}

fn validate_preserved_snapshot(home: &Path, reader: super::r265_rollback::Reader) -> anyhow::Result<()> {
    let management = home.join("runtime/management-v2");
    let database = management.join(crate::management::v2::hot_store::DATABASE_FILE);
    for db in [&database, &management.join("integration-event-dedupe-v2.sqlite"),
        &management.join("pending-stops.sqlite"), &home.join("cutex-sessions.sqlite3")] {
        for suffix in ["-wal", "-shm", "-journal"] {
            let sidecar = std::path::PathBuf::from(format!("{}{suffix}", db.display()));
            anyhow::ensure!(db.try_exists()? || !sidecar.try_exists()?, "orphan SQLite sidecar: {}", sidecar.display());
        }
        if db.try_exists()? {
            let connection = rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let check: String = connection.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
            anyhow::ensure!(check == "ok", "rollback SQLite integrity check failed");
        }
    }
    if database.try_exists()? {
        let db=rusqlite::Connection::open_with_flags(&database,rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        // A blank/foreign database must not masquerade as absent stores, nor
        // may orphan rows vanish behind the optional metadata lookup.
        let mut tables=db.prepare("SELECT name FROM sqlite_master WHERE type='table'")?;
        let names=tables.query_map([],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        anyhow::ensure!(names.iter().any(|s|s=="hot_store_meta") && names.iter().any(|s|s=="hot_store_rows")
            && names.iter().all(|s|matches!(s.as_str(),"hot_store_meta"|"hot_store_rows"|"asks_archive_meta"|"asks_archive_rows")),"unknown management hot-store schema");
        let invalid:i64=db.query_row("SELECT count(*) FROM hot_store_meta WHERE store NOT IN ('asks','agent_bus') OR verified NOT IN (0,1)",[],|r|r.get(0))?;
        let orphan:i64=db.query_row("SELECT count(*) FROM hot_store_rows r LEFT JOIN hot_store_meta m ON r.store=m.store WHERE m.store IS NULL",[],|r|r.get(0))?;
        anyhow::ensure!(invalid==0 && orphan==0,"unknown/orphan hot-store metadata or rows");
    }
    if let Some(loaded) = crate::management::v2::hot_store::verified_rows(&database, "asks")? {
        let rows = if crate::asks::archive::persisted_cutoff(&database)?.is_some() {
            crate::asks::archive::history_rows(&database)?
        } else { loaded.rows };
        crate::asks::store::ask_state_from_rows(&rows)?;
    } else {
        anyhow::ensure!(!crate::management::v2::hot_store::has_store(&database, "asks")?,
            "asks migration/handoff is unverified; reconcile before rollback");
        // Check even without hot rows: unmarked cold data must not disappear.
        anyhow::ensure!(crate::asks::archive::persisted_cutoff(&database)?.is_none(),
            "cold asks without verified hot metadata");
        match std::fs::read(home.join("runtime/asks/asks-v1.json")) {
            Ok(bytes) => { serde_json::from_slice::<crate::asks::store::AskState>(&bytes)
                .context("corrupt legacy asks rollback state")?; }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("read legacy asks rollback state"),
        }
    }
    crate::management::v2::agent_bus_state::AgentBusMessageRepository::validate_persisted_read_only(&management)?;
    // Current decoding accepts v8, but the pinned r263/r264 readers do not.
    // Keep their rollback contract independent of the current writer version.
    crate::management::v2::agent_bus_state::AgentBusMessageRepository::validate_settlement_rollback_read_only(&management)?;
    crate::management::v2::integration_events::integration_dedupe_health(&management)?;
    super::r264_rollback::validate_snapshot(home, reader)
}

fn preflight_hot_store_exports_with_archive(home:&Path,allow_asks_archive:bool)->anyhow::Result<()> {
    let runtime = home.join("runtime");
    let management = runtime.join("management-v2");
    let database = management.join(crate::management::v2::hot_store::DATABASE_FILE);
    if crate::management::v2::hot_store::generation(&database, "asks")?.is_some() {
        let asks=crate::asks::store::AskStore::at_runtime(&runtime);
        if allow_asks_archive {
            asks.history_snapshot().context("ask history restoration preflight failed")?;
        } else {
            asks.verify_legacy_export().context("ask hot-store rollback preflight failed")?;
        }
    }
    if crate::management::v2::hot_store::generation(&database, "agent_bus")?.is_some() {
        crate::management::v2::agent_bus_state::AgentBusMessageRepository::open(&management)?
            .verify_legacy_export()
            .context("agent-bus hot-store rollback preflight failed")?;
    }
    if management.join("integration-event-dedupe-v2.sqlite").is_file() {
        crate::management::v2::integration_events::integration_dedupe_health(&management).context("integration dedupe rollback preflight failed")?;
    }
    Ok(())
}

fn preflight_legacy_target(home: &Path, target: &str) -> anyhow::Result<()> {
    let needs_legacy_blockers = match rollback_target(target)? {
        RollbackTarget::Release(number) => number < 256,
        RollbackTarget::Public(..) => true,
    };
    if !needs_legacy_blockers {
        return Ok(());
    }

    let profile = crate::profiles::account_link::legacy_downgrade_blockers(home).context("legacy profile downgrade preflight failed")?;
    anyhow::ensure!(
        profile.is_empty(),
        "target {target} cannot safely read current profile state; reconciliation required: {}",
        profile.join(", ")
    );

    let director_root = home.join("runtime/director-address");
    let ownership = crate::director_address::Store::at(&director_root).snapshot().map_err(|error| anyhow::anyhow!(error.detail))?;
    let mut blockers = Vec::new();
    if ownership.reviews.values().any(|review| review.state == "pending") {
        blockers.push("pending_handover_review");
    }
    if ownership.asks.values().any(|ask| ask.pending_reconcile) {
        blockers.push("pending_handover_reconcile");
    }
    if ownership.asks.values().any(|ask| ask.owner_hold) {
        blockers.push("handover_owner_hold");
    }
    if ownership.asks.values().any(|ask| ask.summary_pending) {
        blockers.push("handover_summary_pending");
    }
    if !ownership.alarm_roles.is_empty() {
        blockers.push("director_targeted_alarm_role");
    }

    let management = crate::director_address::management::ManagementStore::at(&director_root).read()?;
    use crate::director_address::management::{AdoptionState, NoticeState, StartReviewState, TransitionState};
    if management
        .transitions
        .values()
        .any(|transition| transition.state == TransitionState::Prepared || (transition.state == TransitionState::Finalized && (!transition.handover_dispatched || !transition.event_emitted)))
    {
        blockers.push("pending_director_transition");
    }
    if management.notices.values().any(|notice| notice.state == NoticeState::Pending) {
        blockers.push("pending_director_notice");
    }
    if management
        .start_reviews
        .values()
        .any(|review| matches!(review.state, StartReviewState::Open | StartReviewState::DeliveryUnknown))
    {
        blockers.push("pending_start_review");
    }
    if management
        .adoptions
        .values()
        .any(|adoption| matches!(adoption.state, AdoptionState::Approved | AdoptionState::Submitted | AdoptionState::SubmissionUnknown))
    {
        blockers.push("pending_start_adoption");
    }
    anyhow::ensure!(
        blockers.is_empty(),
        "target {target} cannot preserve active Director work; reconciliation required: {}",
        blockers.join(", ")
    );
    Ok(())
}

fn since_0_1_4(home: &Path, displaced: &Path, notes: &mut Vec<String>) -> anyhow::Result<()> {
    let approvals = home.join(APPROVALS);
    if approvals.is_file() {
        let removed = crate::approvals::ApprovalStore::at(&approvals).remove_kind(crate::approvals::ApprovalKind::StaticRoot)?;
        if !removed.is_empty() {
            save(displaced, "approvals-static-root.json", &json!({ "approvals": removed }))?;
            notes.push(format!("{APPROVALS}: {} static_root approval record(s) removed", removed.len()));
        }
    }
    let sessions = home.join(SESSIONS);
    if crate::session::store::session_database_path(&sessions).is_file() {
        let cleared = crate::session::store::with_locked_session_rows(&sessions, |rows| {
            let keys: Vec<String> = rows
                .sessions()
                .filter(|(_, record)| record.claude_session.as_ref().is_some_and(|claude| claude.initial_permission_mode.is_some()))
                .map(|(key, _)| key.clone())
                .collect();
            let mut cleared = Vec::new();
            for key in keys {
                let record = rows.session_mut(&key).expect("key listed above");
                let mode = record.claude_session.as_mut().and_then(|claude| claude.initial_permission_mode.take());
                record.bump_durable_revision()?;
                record.updated_at = chrono::Utc::now().to_rfc3339();
                cleared.push(json!({ "key": key, "name": record.formal_agent_name, "initialPermissionMode": mode }));
            }
            if !cleared.is_empty() {
                rows.commit()?;
            }
            Ok(cleared)
        })?;
        if !cleared.is_empty() {
            save(displaced, "claude-initial-permission-modes.json", &json!({ "sessions": cleared }))?;
            notes.push(format!("{SESSIONS}: pending Claude permission mode cleared on {} agent(s)", cleared.len()));
        }
    }
    Ok(())
}

fn since_0_1_5(home: &Path, displaced: &Path, notes: &mut Vec<String>) -> anyhow::Result<()> {
    let held = crate::management::client_tokens::ClientTokenStore::at(home.join(CLIENT_TOKENS)).hold_narrowed_for_rollback()?;
    if !held.is_empty() {
        save(displaced, "client-tokens-held.json", &json!({ "tokens": held }))?;
        notes.push(format!(
            "{CLIENT_TOKENS}/tokens.json: {} device token(s) with a narrowed scope held (refused until the roll-forward)",
            held.len()
        ));
    }
    let bus_database = home.join(AGENT_BUS).join(crate::management::v2::hot_store::DATABASE_FILE);
    if home.join(AGENT_BUS_STATE).is_file() || crate::management::v2::hot_store::generation(&bus_database, "agent_bus")?.is_some() {
        let downgraded = crate::management::v2::agent_bus_state::AgentBusMessageRepository::open(home.join(AGENT_BUS))?.downgrade_pending_peer_wakes()?;
        if !downgraded.is_empty() {
            save(displaced, "peer-wakes.json", &json!({ "messages": downgraded }))?;
            notes.push(format!("{AGENT_BUS_STATE}: {} pending cross-host wake(s) turned into notes", downgraded.len()));
        }
    }
    let users = home.join(USERS);
    if users.is_file() {
        let state: serde_json::Value = serde_json::from_slice(&std::fs::read(&users)?).map_err(|error| anyhow::anyhow!("read {}: {error}", users.display()))?;
        let notices = state.get("newOwnerDevices").and_then(|v| v.as_array()).filter(|v| !v.is_empty());
        if let Some(notices) = notices {
            save(displaced, "new-owner-devices.json", &json!({ "newOwnerDevices": notices }))?;
            notes.push(format!(
                "{USERS}: {} unacknowledged new owner device notice(s) kept aside (the older release drops them)",
                notices.len()
            ));
        }
    }
    Ok(())
}

fn save(displaced: &Path, name: &str, value: &serde_json::Value) -> anyhow::Result<()> {
    let path = displaced.join(OUT_DIR).join(name);
    anyhow::ensure!(!path.exists(), "{} already exists", path.display());
    crate::config::atomic::write_private_pretty_json_atomic(&path, value, "rollback record")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approvals::{ApprovalKind, ApprovalRequest, ApprovalStore, GateOutcome};

    fn temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cutex-older-stores-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn request(kind: ApprovalKind, sha: &str) -> ApprovalRequest {
        ApprovalRequest {
            kind,
            reason: "test".into(),
            requester_cutex_session_id: "cutex.a".into(),
            requester_name: None,
            project_id: None,
            summary: "s".into(),
            details: json!({}),
            request_sha256: sha.into(),
        }
    }

    fn file_tree(root: &Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
        fn visit(root: &Path, path: &Path, out: &mut std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>) {
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() { out.insert(path.strip_prefix(root).unwrap().into(), vec![]); visit(root, &path, out); }
                else { out.insert(path.strip_prefix(root).unwrap().into(), std::fs::read(&path).unwrap()); }
            }
        }
        let mut out = std::collections::BTreeMap::new(); visit(root, root, &mut out); out
    }

    #[test]
    fn r263_preserves_owner_notes_and_legacy_files_without_migration() {
        for kind in ["pending", "lease", "next-step", "queued", "orphan-queued", "settled"] {
            let home = temp("r263-legacy");
            let path = home.join("runtime/asks/asks-v1.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, serde_json::to_vec(&owner_note_fixture(kind)).unwrap()).unwrap();
            let bus = home.join(AGENT_BUS_STATE);
            std::fs::create_dir_all(bus.parent().unwrap()).unwrap();
            std::fs::write(&bus, br#"{"version":7,"messages":{}}"#).unwrap();
            let before = file_tree(&home);
            preflight(&home, "r263").unwrap();
            assert!(prepare(&home, &home.join("unused"), "r263").unwrap().is_empty());
            assert!(export_hot_stores(&home, "r263", true).unwrap().is_empty());
            assert_eq!(file_tree(&home), before);
            if kind != "settled" { assert!(preflight(&home, "r262").is_err()); }
        }
    }

    #[test]
    fn r263_absence_corruption_and_unreadable_inputs_never_create_outputs() {
        let home = temp("r263-no-write");
        let absent = home.join("absent");
        preflight(&absent, "r263").unwrap();
        assert!(!absent.exists());
        for (relative, bytes) in [("runtime/asks/asks-v1.json", b"{".as_slice()),
            (AGENT_BUS_STATE, br#"{"version":99,"messages":{}}"#.as_slice()),
            ("runtime/management-v2/management-state.sqlite3", b"not sqlite".as_slice()),
            ("runtime/management-v2/integration-event-dedupe-v1.json", b"{}".as_slice())] {
            let root = temp("r263-invalid"); let path = root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap(); std::fs::write(&path, bytes).unwrap();
            let before = file_tree(&root);
            assert!(prepare(&root, &root.join("unused"), "r263").is_err(), "{relative}");
            assert_eq!(file_tree(&root), before);
        }
        let path = home.join("runtime/asks/asks-v1.json");
        std::fs::create_dir_all(&path).unwrap(); // wrong-kind is not absence
        let before = file_tree(&home);
        assert!(preflight(&home, "r263").is_err()); assert_eq!(file_tree(&home), before);
    }

    #[cfg(windows)]
    #[test]
    fn r263_access_denied_is_not_absence() {
        use std::os::windows::fs::OpenOptionsExt;
        let home = temp("r263-denied"); let path = home.join("runtime/asks/asks-v1.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bytes = serde_json::to_vec(&owner_note_fixture("pending")).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(&path).unwrap();
        assert!(preflight(&home, "r263").is_err());
        assert!(!home.join("runtime/management-v2").exists());
        drop(held); assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    fn owner_note_fixture(kind: &str) -> crate::asks::store::AskState {
        let mut state = crate::asks::store::AskState::default();
        let event = json!({"eventId":"event-owner", "cutexSessionId":"cutex.owner",
            "deliveryIds":["delivery-owner"], "nativeMessageId":"cutex-ask-owner-fixture",
            "acknowledged":kind == "settled" || kind == "queued", "createdAt":"2026-10-26T00:00:00Z"});
        if matches!(kind, "pending" | "settled" | "queued") {
            state.events.insert("event-owner".into(), serde_json::from_value(event).unwrap());
        }
        if matches!(kind, "lease" | "next-step") {
            let lease = serde_json::from_value(json!({"cutexSessionId":"cutex.owner",
                "holder":{"kind":"event","event_id":"event-owner"}, "nativeMessageId":"cutex-ask-owner-fixture",
                "runtimeGeneration":7,"since":"2026-10-26T00:00:00Z"})).unwrap();
            if kind == "lease" { state.leases.insert("cutex.owner".into(), lease); }
            else { state.next_step_leases.insert("cutex.owner".into(), lease); }
        }
        if matches!(kind, "queued" | "settled" | "orphan-queued") {
            state.deliveries.insert("delivery-owner".into(), serde_json::from_value(json!({
                "deliveryId":"delivery-owner", "kind":"answer", "askId":"ask-fixture", "refId":"answer-fixture",
                "requesterCutexSessionId":"cutex.owner", "state":if kind == "settled" {"acknowledged"} else {"queuedAsEvent"},
                "eventId":if kind == "orphan-queued" {"cutex-ask-owner-orphan"} else {"event-owner"},
                "createdAt":"2026-10-26T00:00:00Z", "updatedAt":"2026-10-26T00:00:00Z"
            })).unwrap());
        }
        state
    }

    #[test]
    fn owner_result_rollback_blocker_preserves_legacy_records_before_any_mutation() {
        for kind in ["pending", "lease", "next-step", "queued", "orphan-queued"] {
            let home = temp("owner-note-block");
            let path = home.join("runtime/asks/asks-v1.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let before = serde_json::to_vec_pretty(&owner_note_fixture(kind)).unwrap();
            std::fs::write(&path, &before).unwrap();
            for target in ["r262", "r259", "r252", "0.1.3", "0.1.5"] {
                let displaced = home.join("must-not-be-created");
                for result in [preflight(&home, target).map(|_| vec![]),
                    prepare(&home, &displaced, target), export_hot_stores(&home, target, true)] {
                    assert!(format!("{:#}", result.unwrap_err()).contains("drain and confirm"), "{kind} -> {target}");
                }
                assert_eq!(std::fs::read(&path).unwrap(), before);
                assert!(!displaced.exists());
                assert!(!home.join("runtime/management-v2").exists(), "must not migrate legacy records");
            }
            assert!(preflight_asks_archive_restore(&home, "r259").unwrap_err().to_string().contains("drain and confirm"));
            assert_eq!(std::fs::read(&path).unwrap(), before);
            std::fs::remove_dir_all(home).unwrap();
        }
    }

    #[test]
    fn owner_result_rollback_allows_settled_and_absent_notes_without_creating_stores() {
        let home = temp("owner-note-clear");
        preflight(&home, "r262").unwrap();
        assert!(std::fs::read_dir(&home).unwrap().next().is_none());
        let path = home.join("runtime/asks/asks-v1.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for state in [owner_note_fixture("settled"), crate::asks::store::AskState::default()] {
            let before = serde_json::to_vec(&state).unwrap();
            std::fs::write(&path, &before).unwrap();
            preflight(&home, "r262").unwrap();
            assert!(export_hot_stores(&home, "r262", true).unwrap().is_empty());
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert!(!home.join("runtime/management-v2").exists());
        }
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn owner_result_rollback_checks_authoritative_hot_rows_without_writing() {
        let home = temp("owner-note-hot");
        let asks = crate::asks::store::AskStore::at_runtime(home.join("runtime"));
        asks.with_state(|state| *state = owner_note_fixture("pending")).unwrap();
        let database = home.join("runtime/management-v2/management-state.sqlite3");
        let dump = || {
            let db = rusqlite::Connection::open_with_flags(&database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
            let mut query = db.prepare("SELECT store,collection,row_key,value_json FROM hot_store_rows ORDER BY store,collection,row_key").unwrap();
            let rows = query.query_map([], |r| Ok((r.get::<_, String>(0)?,r.get::<_, String>(1)?,r.get::<_, String>(2)?,r.get::<_, Vec<u8>>(3)?))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
            let generation = crate::management::v2::hot_store::generation(&database, "asks").unwrap();
            (rows, generation)
        };
        let before = dump();
        assert!(preflight(&home, "r262").unwrap_err().to_string().contains("drain and confirm"));
        assert_eq!(dump(), before);
        // Acknowledged history is compatible; neither stale JSON nor old events
        // should override the authoritative settled hot rows.
        asks.with_state(|state| *state = owner_note_fixture("settled")).unwrap();
        let before = dump();
        preflight(&home, "r262").unwrap();
        assert_eq!(dump(), before);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn static_root_approvals_are_removed_and_kept_aside() {
        let home = temp("approvals");
        let displaced = temp("displaced");
        let store = ApprovalStore::at(home.join(APPROVALS));
        let now = chrono::Utc::now();
        for (kind, sha) in [(ApprovalKind::StaticRoot, "a"), (ApprovalKind::ManagedService, "b")] {
            assert!(matches!(store.gate(request(kind, sha), now).unwrap(), GateOutcome::Pending { .. }));
        }
        let notes = prepare(&home, &displaced, "0.1.3").unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        let left = crate::approvals::read_all_at(&home.join(APPROVALS)).unwrap();
        assert_eq!(left.iter().map(|r| r.kind).collect::<Vec<_>>(), vec![ApprovalKind::ManagedService]);
        let kept: serde_json::Value = serde_json::from_slice(&std::fs::read(displaced.join(OUT_DIR).join("approvals-static-root.json")).unwrap()).unwrap();
        assert_eq!(kept["approvals"].as_array().unwrap().len(), 1);
        // Nothing left to change: a second pass is a no-op.
        assert!(prepare(&home, &temp("displaced2"), "0.1.3").unwrap().is_empty());
    }

    #[test]
    fn pending_claude_permission_modes_are_cleared_and_kept_aside() {
        let home = temp("sessions");
        let displaced = temp("displaced");
        let path = home.join(SESSIONS);
        let claude = |mode: Option<&str>| -> crate::runtime::claude::ClaudeSession {
            serde_json::from_value(json!({
                "deployment": {"python": "p", "adapter_source": "a", "claude_cli": "c", "claude_home": "h", "tui_binary": "t"},
                "thread_id": "t", "state_dir": "s", "model": "m", "initial_permission_mode": mode
            }))
            .unwrap()
        };
        crate::session::store::with_locked_session_rows(&path, |rows| {
            for (name, mode) in [("a", Some("auto")), ("b", None)] {
                let mut record = crate::session::model::CutexSessionRecord::new_at(
                    format!("cutex.{name}"),
                    Some(format!("thread-{name}")),
                    "devbox".into(),
                    format!("/tmp/{name}"),
                    None,
                    "2026-10-18T00:00:00Z".into(),
                )?;
                record.claude_session = Some(claude(mode));
                rows.insert_session(format!("cutex.{name}"), record);
            }
            rows.commit().map(|_| ())
        })
        .unwrap();
        let notes = prepare(&home, &displaced, "0.1.3").unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        crate::session::store::with_cutex_session_store_at(&path, |store| {
            for record in store.sessions.values() {
                let claude = record.claude_session.as_ref().unwrap();
                assert!(claude.initial_permission_mode.is_none());
                // What an older cutex sees: no unknown field in the row.
                assert!(serde_json::to_value(claude).unwrap().get("initial_permission_mode").is_none());
            }
        })
        .unwrap();
        let kept: serde_json::Value = serde_json::from_slice(&std::fs::read(displaced.join(OUT_DIR).join("claude-initial-permission-modes.json")).unwrap()).unwrap();
        assert_eq!(kept["sessions"][0]["initialPermissionMode"], "auto");
    }

    #[test]
    fn an_empty_home_needs_nothing() {
        for target in ["0.1.3", "0.1.4", "r252", "r256", "r257", "r258", "r259", "r262", "r263"] {
            let home = temp("empty");
            assert!(prepare(&home, &temp("displaced"), target).unwrap().is_empty());
            assert_eq!(std::fs::read_dir(&home).unwrap().count(), 0, "nothing created for {target}");
        }
        for target in ["dev-4add630", "unknown", "", "0.1.4-rc1", "0.1", "r260", "r261", "r0263", "r0264", "r265"] {
            let home = temp("unknown");
            assert!(prepare(&home, &temp("displaced"), target).is_err(), "{target}");
            assert_eq!(std::fs::read_dir(&home).unwrap().count(), 0, "nothing created for {target}");
        }
    }

    #[test]
    fn r262_predecessor_preserves_hot_cold_archive_pause_and_rejects_corruption() {
        use crate::asks::model::{CloseOutcome, ExecutionState};
        let home = temp("r262-preserve");
        let asks = crate::asks::store::AskStore::at_runtime(home.join("runtime"));
        let old = chrono::DateTime::parse_from_rfc3339("2026-10-23T00:00:00Z").unwrap().with_timezone(&chrono::Utc);
        let cutoff = old + chrono::Duration::days(2);
        let cold_id = asks.with_state(|state| {
            let closed = state.create_ask(crate::asks::store::tests::new_ask("cold-predecessor", "retained"), old).unwrap().0;
            let saved = state.asks.get_mut(&closed.ask_id).unwrap();
            saved.execution = ExecutionState::Cancelled;
            saved.closed = Some(CloseOutcome::Abandoned);
            saved.closed_at = Some(crate::asks::now_rfc3339(old));
            state.notices.clear();
            state.create_ask(crate::asks::store::tests::new_ask("hot-predecessor", "active"), cutoff).unwrap();
            crate::asks::agent_pause::pause(state, "cutex.paused", "owner", None, cutoff);
            closed.ask_id
        }).unwrap();
        assert_eq!(asks.archive_before(Some(cutoff), cutoff).unwrap().asks, 1);
        let path = home.join("runtime/management-v2/management-state.sqlite3");
        let dump = || {
            let db = rusqlite::Connection::open(&path).unwrap();
            ["SELECT * FROM hot_store_meta ORDER BY store", "SELECT * FROM hot_store_rows ORDER BY store,collection,row_key",
             "SELECT * FROM asks_archive_meta ORDER BY singleton", "SELECT * FROM asks_archive_rows ORDER BY collection,row_key"]
                .iter().map(|sql| {
                    db.prepare(sql).unwrap().query_map([], |row| {
                        (0..row.as_ref().column_count()).map(|column| row.get_ref(column).map(|value| format!("{value:?}"))).collect::<rusqlite::Result<Vec<_>>>()
                    }).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap()
                }).collect::<Vec<_>>()
        };
        let before = dump();
        let legacy_before = std::fs::read(home.join("runtime/asks/asks-v1.json")).ok();
        for target in ["r262", "r263"] {
            preflight(&home, target).unwrap();
            assert!(prepare(&home, &home.join("unused-displaced"), target).unwrap().is_empty());
            assert!(export_hot_stores(&home, target, true).unwrap().is_empty());
            assert!(!home.join("unused-displaced").exists());
        }
        assert_eq!(dump(), before, "no metadata/generation/hot/cold bytes may change");
        assert!(!asks.snapshot().unwrap().asks.contains_key(&cold_id), "no cold promotion");
        assert!(asks.history_snapshot().unwrap().asks.contains_key(&cold_id));
        assert_eq!(std::fs::read(home.join("runtime/asks/asks-v1.json")).ok(), legacy_before, "no legacy JSON export");
        assert!(export_hot_stores(&home, "r262", false).is_err(), "writer fence stays required");
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute("UPDATE asks_archive_rows SET sha256='invalid'", []).unwrap();
        assert!(preflight(&home, "r262").is_err(), "corrupt cold history must refuse");
        assert!(preflight(&home, "r263").is_err(), "r263 also refuses corrupt cold history");
        drop(db);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn r259_archive_restore_preflight_does_not_require_its_own_output() {
        use crate::asks::model::{CloseOutcome,ExecutionState};
        let home=temp("r259-archive-restore");
        let asks=crate::asks::store::AskStore::at_runtime(home.join("runtime"));
        let old=chrono::DateTime::parse_from_rfc3339("2026-10-23T00:00:00Z").unwrap().with_timezone(&chrono::Utc);
        let cutoff=old+chrono::Duration::days(2);
        let id=asks.with_state(|state| {
            let ask=state.create_ask(crate::asks::store::tests::new_ask("r259-restore","retained"),old).unwrap().0;
            let saved=state.asks.get_mut(&ask.ask_id).unwrap();
            saved.execution=ExecutionState::Cancelled;
            saved.closed=Some(CloseOutcome::Abandoned);
            saved.closed_at=Some(crate::asks::now_rfc3339(old));
            state.notices.clear();
            ask.ask_id
        }).unwrap();
        asks.archive_before(Some(cutoff),cutoff).unwrap();
        assert!(!asks.snapshot().unwrap().asks.contains_key(&id));
        assert!(preflight(&home,"r259").is_err(),"ordinary export must still refuse cold omission");
        preflight_asks_archive_restore(&home,"r259").unwrap();
        assert!(!asks.snapshot().unwrap().asks.contains_key(&id),"preflight must not promote or restore");
        asks.restore_archived_for_rollback(true).unwrap();
        preflight(&home,"r259").unwrap();
        assert!(asks.snapshot().unwrap().asks.contains_key(&id));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn rollback_refuses_kimi_profiles_for_every_target() {
        let home = temp("r266-kimi-profile");
        let mut accounts = crate::profiles::model::AccountsStore::default();
        accounts.accounts.push(serde_json::from_value(json!({"id":"kimi-profile","name":"Kimi","cli_kind":"kimi","kimi_account":"ka-a"})).unwrap());
        std::fs::write(home.join("accounts.json"), serde_json::to_vec_pretty(&accounts).unwrap()).unwrap();
        for target in ["r264", "r263", "r262", "r259", "0.1.5"] {
            let error = format!("{:#}", preflight(&home, target).unwrap_err());
            assert!(error.contains("Kimi profile"), "{target}: {error}");
        }
        // A kimi variant on an API-key profile is no obstacle (older releases ignore it).
        let mut accounts = crate::profiles::model::AccountsStore::default();
        accounts.accounts.push(serde_json::from_value(json!({"id":"k","name":"K","cli_kind":"codex",
            "runtime_variants":[{"id":"kimi","runtime":"kimi","defaults":{}}]})).unwrap());
        std::fs::write(home.join("accounts.json"), serde_json::to_vec_pretty(&accounts).unwrap()).unwrap();
        preflight_kimi_profiles(&home, "r263").unwrap();
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn rollback_refuses_kimi_sessions_for_every_target() {
        // A Kimi agent on an API-key profile's `kimi` variant: no Kimi-kind
        // profile, but its record's backend no older release decodes.
        let home = temp("r266-kimi-session");
        let path = home.join(SESSIONS);
        crate::session::store::with_locked_session_rows(&path, |rows| {
            let mut record = crate::session::model::CutexSessionRecord::new_at(
                "cutex.kimi".into(), Some("thread-kimi".into()), "devbox".into(), "/tmp/kimi".into(), None, "2026-10-27T00:00:00Z".into(),
            )?;
            record.runtime_backend = crate::session::model::CutexSessionRuntimeBackend::Kimi;
            rows.insert_session("cutex.kimi", record);
            rows.commit().map(|_| ())
        })
        .unwrap();
        let before = std::fs::read_dir(&home).unwrap().map(|entry| entry.unwrap().file_name()).collect::<std::collections::BTreeSet<_>>();
        for target in ["r264", "r263", "r262", "r259", "0.1.5"] {
            let error = format!("{:#}", preflight(&home, target).unwrap_err());
            // Below r260 the DSH/ACP session gate refuses the same record first.
            assert!(error.contains("Kimi session") || error.contains("DSH session"), "{target}: {error}");
        }
        let after = std::fs::read_dir(&home).unwrap().map(|entry| entry.unwrap().file_name()).collect::<std::collections::BTreeSet<_>>();
        assert_eq!(before, after, "the check reads a copy");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn r259_rollback_refuses_dsh_profiles_before_any_write() {
        let home = temp("r259-dsh-profile");
        let mut accounts = crate::profiles::model::AccountsStore::default();
        accounts.accounts.push(
            serde_json::from_value(json!({
                "id":"dsh-profile", "name":"DSH", "cli_kind":"dsh"
            }))
            .unwrap(),
        );
        let original = serde_json::to_vec_pretty(&accounts).unwrap();
        std::fs::write(home.join("accounts.json"), &original).unwrap();
        let displaced = home.join("displaced");

        for operation in [
            preflight(&home, "r259").map(|_| Vec::new()),
            preflight_asks_archive_restore(&home,"r259").map(|_|Vec::new()),
            prepare(&home, &displaced, "r259"),
            export_hot_stores(&home, "r259", true),
        ] {
            let error = operation.unwrap_err();
            assert!(error.to_string().contains("DSH profile"), "{error:#}");
        }
        assert_eq!(std::fs::read(home.join("accounts.json")).unwrap(), original);
        assert!(!displaced.exists());
        assert!(!home.join("cutex-sessions.sqlite3").exists());
    }

    #[test]
    fn r259_rollback_refuses_dsh_sessions_before_any_write() {
        let home = temp("r259-dsh-session");
        let sessions = home.join(SESSIONS);
        let mut record = crate::session::model::CutexSessionRecord::new_at(
            "cutex.dsh-rollback".into(),
            None,
            "devbox".into(),
            home.to_string_lossy().into_owned(),
            None,
            "2026-10-25T00:00:00Z".into(),
        )
        .unwrap();
        record.runtime_backend = crate::session::model::CutexSessionRuntimeBackend::Dsh;
        record.dsh_session = Some(
            serde_json::from_value(json!({
                "thread_id":"018e0000-0000-7000-8000-000000000260",
                "state_dir":home.join("dsh-state"), "node":"node", "adapter":"adapter",
                "dsh_cli":"dsh", "patch":"patch", "install_root":"install",
                "native_session_id":null, "spike":false, "model":"deepseek-v4-flash",
                "api_key_env":"DEEPSEEK_API_KEY", "deployment_version":"0.2.1-rc.3"
            }))
            .unwrap(),
        );
        crate::session::store::with_locked_session_rows(&sessions, |rows| {
            rows.insert_session(record.cutex_session_id.clone(), record.clone());
            rows.commit().map(|_| ())
        })
        .unwrap();
        let displaced = home.join("displaced");

        let error = prepare(&home, &displaced, "r259").unwrap_err();
        assert!(error.to_string().contains("DSH session"), "{error:#}");
        let after = crate::session::store::with_cutex_session_store_at(&sessions, |store| {
            store.sessions["cutex.dsh-rollback"].clone()
        })
        .unwrap();
        assert_eq!(after.runtime_backend, crate::session::model::CutexSessionRuntimeBackend::Dsh);
        assert!(after.dsh_session.is_some());
        assert!(!displaced.exists());
    }

    #[test]
    fn avatar_contract_blocks_every_old_target_before_export_or_displacement() {
        let home = temp("avatar-preflight");
        let displaced = home.join("displaced");
        let mut accounts = crate::profiles::model::AccountsStore::default();
        accounts.accounts.push(serde_json::from_value(json!({
            "id":"avatar-fixture","name":"Avatar fixture","email":null,"plan_type":null,"last_used_at":null,
            "options":{"avatar_style":{"kind":"letter"}}
        })).unwrap());
        let bytes = serde_json::to_vec(&accounts).unwrap();
        std::fs::write(home.join("accounts.json"), &bytes).unwrap();
        for target in ["r256", "r252", "0.1.5"] {
            assert!(preflight(&home, target).unwrap_err().to_string().contains("avatar"));
            assert!(prepare(&home, &displaced, target).is_err());
            assert!(export_hot_stores(&home, target, true).is_err());
        }
        assert_eq!(std::fs::read(home.join("accounts.json")).unwrap(), bytes);
        assert!(!displaced.exists());
        assert!(!home.join("runtime").exists());
        assert!(!home.join("cutex-sessions.sqlite3").exists());
    }

    #[test]
    fn r252_preflight_refuses_active_director_work_before_any_export() {
        let home = temp("director-preflight");
        let state = json!({
            "version": 1,
            "revision": 1,
            "alarmRoles": {
                "alarm-1": {
                    "owner": "cutex.owner",
                    "projectId": "project",
                    "rawDigest": "digest",
                    "target": "cutex.director"
                }
            }
        });
        crate::config::atomic::write_private_pretty_json_atomic(&home.join("runtime/director-address/ownership-v1.json"), &state, "Director downgrade preflight fixture").unwrap();
        for target in ["r252", "0.1.5", "0.1.4"] {
            let error = prepare(&home, &temp("displaced"), target).unwrap_err();
            assert!(error.to_string().contains("director_targeted_alarm_role"), "{target}: {error:#}");
        }
        assert!(!home.join("runtime/asks/asks-v1.json").exists());
        assert!(preflight_legacy_target(&home, "r256").is_ok());
    }

    #[test]
    fn agent_pause_blocks_every_older_target_before_export_or_displacement() {
        let home = temp("agent-pause-preflight");
        let runtime = home.join("runtime");
        let store = crate::asks::store::AskStore::at_runtime(&runtime);
        store
            .with_state(|state| {
                crate::asks::agent_pause::pause(state, "cutex.paused", "owner", None, chrono::Utc::now());
            })
            .unwrap();
        let before = store.snapshot().unwrap();
        let displaced = home.join("rollback-output");

        for target in ["r258", "r257", "r252", "0.1.5"] {
            let error = preflight(&home, target).unwrap_err();
            assert!(error.to_string().contains("durable agent pause"), "{target}: {error:#}");
            assert!(prepare(&home, &displaced, target).is_err(), "{target}");
            assert!(export_hot_stores(&home, target, true).is_err(), "{target}");
        }

        assert_eq!(store.snapshot().unwrap(), before);
        assert!(!displaced.exists(), "preflight refusal must precede rollback output");
        assert!(!runtime.join("asks/asks-v1.json").exists());
    }

    #[test]
    fn pre_r257_rollback_exports_latest_sqlite_ask_rows() {
        let home = temp("sqlite-hot-export");
        let runtime = home.join("runtime");
        let asks = crate::asks::store::AskStore::at_runtime(&runtime);
        asks.with_state(|state| {
            state.wait_cancel.insert("cutex.target".into(), 7);
        })
        .unwrap();
        assert!(!runtime.join("asks/asks-v1.json").exists());
        let notes = prepare(&home, &temp("displaced"), "r256").unwrap();
        assert!(notes.iter().any(|note| note.contains("asks-v1.json")), "{notes:?}");
        let exported: crate::asks::store::AskState = serde_json::from_slice(&std::fs::read(runtime.join("asks/asks-v1.json")).unwrap()).unwrap();
        assert_eq!(exported.wait_cancel["cutex.target"], 7);
    }

    #[test]
    fn corrupt_ask_preflight_leaves_pending_peer_wake_unchanged() {
        let home = temp("preflight-before-wake");
        let runtime = home.join("runtime");
        crate::asks::store::AskStore::at_runtime(&runtime)
            .with_state(|state| {
                state.wait_cancel.insert("cutex.target".into(), 7);
            })
            .unwrap();
        let management = runtime.join("management-v2");
        let bus = crate::management::v2::agent_bus_state::AgentBusMessageRepository::open(&management).unwrap();
        bus.record_queued_isolated(peer_message("wake-before-failure", true)).unwrap();
        let database = management.join(crate::management::v2::hot_store::DATABASE_FILE);
        rusqlite::Connection::open(&database)
            .unwrap()
            .execute("UPDATE hot_store_rows SET value_json = x'7b' WHERE store = 'asks'", [])
            .unwrap();

        let displaced = temp("preflight-before-wake-displaced");
        assert!(prepare(&home, &displaced, "0.1.4").is_err());
        let wake = bus.canonical_message("wake-before-failure").unwrap();
        assert_eq!(
            wake.control_type.as_deref(),
            Some(crate::agent_bus::peer_tell::CONTROL_TYPE_V2),
            "failed preflight must not turn the pending wake into a passive note"
        );
        assert!(!displaced.join(OUT_DIR).join("peer-wakes.json").exists());
    }

    #[test]
    fn steps_run_for_older_targets_and_target_parser_is_fail_closed() {
        for (target, supports_0_1_4, supports_0_1_5) in [
            ("0.1.3", false, false), ("0.1.4", true, false), ("0.1.5", true, true),
            ("r252", true, true), ("r256", true, true), ("r258", true, true), ("r259", true, true), ("r262", true, true), ("r263", true, true), ("r264", true, true),
        ] {
            let parsed = rollback_target(target).unwrap();
            assert_eq!(parsed.supports_0_1_4_state(), supports_0_1_4, "{target}");
            assert_eq!(parsed.supports_0_1_5_state(), supports_0_1_5, "{target}");
        }
        for target in ["r240", "r241", "r245", "r246", "r251", "dev-4add630", "unknown", "", "0.1.4-rc1", "0.1", "r260", "r261", "r265"] {
            assert!(rollback_target(target).is_err(), "{target}");
        }
    }

    #[test]
    fn approval_rules_refuse_previous_build_before_any_rollback_write() {
        let home = temp("approval-rules-rollback-blocker");
        let approvals = home.join(APPROVALS);
        std::fs::create_dir_all(approvals.parent().unwrap()).unwrap();
        let original = serde_json::to_vec_pretty(&json!({"version":1,"approvals":[],"rulesRevision":1,"approvalRules":[{"ruleId":"aar-keep"}],"approvalRuleAudit":[],"approvalRuleConfirmations":[]})).unwrap();
        std::fs::write(&approvals, &original).unwrap();
        let displaced = temp("approval-rules-rollback-displaced");
        let error = prepare(&home, &displaced, "r257").unwrap_err();
        assert!(error.to_string().contains("cannot safely preserve Cutex approval rules"), "{error:#}");
        assert_eq!(std::fs::read(&approvals).unwrap(), original);
        assert!(!displaced.join(OUT_DIR).exists(), "preflight refusal must precede compatibility output");
    }

    #[test]
    fn guides_and_service_intent_refuse_previous_build_before_any_write() {
        let home = temp("guides-rollback-blocker");
        let guides = home.join("runtime/users/guides-preferences-v1.json");
        std::fs::create_dir_all(guides.parent().unwrap()).unwrap();
        std::fs::write(&guides, serde_json::to_vec(&json!({
            "schema":"cutex/guides-preferences/v1",
            "users":{"owner":{"schema":1,"skippedThrough":"0.2.0","updatedAt":"2026-10-24T00:00:00Z"}}
        })).unwrap()).unwrap();
        let displaced = home.join("displaced");
        let error = prepare(&home, &displaced, "r257").unwrap_err();
        assert!(error.to_string().contains("guide preferences"), "{error:#}");
        assert!(!displaced.exists());

        std::fs::remove_file(guides).unwrap();
        let services = home.join("services/registry.json");
        std::fs::create_dir_all(services.parent().unwrap()).unwrap();
        std::fs::write(&services, serde_json::to_vec(&json!({
            "schemaVersion":3,"settings":{"ephemeralIdleMinutes":0},"services":{},"tombstones":{}
        })).unwrap()).unwrap();
        let error = prepare(&home, &displaced, "r257").unwrap_err();
        assert!(error.to_string().contains("service retention"), "{error:#}");
        assert!(!displaced.exists());
    }

    #[test]
    fn internal_targets_before_r252_are_refused_before_any_write() {
        for target in ["r241", "r251"] {
            let home = temp(&format!("unsupported-{target}"));
            let error = prepare(&home, &home.join("displaced"), target).unwrap_err();
            assert!(error.to_string().contains("older than minimum supported r252"), "{target}: {error:#}");
            assert_eq!(std::fs::read_dir(&home).unwrap().count(), 0, "{target} must be refused before any write");
        }
    }

    #[test]
    fn a_0_1_4_target_keeps_what_0_1_4_reads() {
        let home = temp("keep-014");
        let store = ApprovalStore::at(home.join(APPROVALS));
        assert!(matches!(store.gate(request(ApprovalKind::StaticRoot, "a"), chrono::Utc::now()).unwrap(), GateOutcome::Pending { .. }));
        assert!(prepare(&home, &temp("displaced"), "0.1.4").unwrap().is_empty());
        assert_eq!(crate::approvals::read_all_at(&home.join(APPROVALS)).unwrap().len(), 1);
        // A 0.1.3 target still removes it.
        assert_eq!(prepare(&home, &temp("displaced"), "0.1.3").unwrap().len(), 1);
    }

    #[test]
    fn narrowed_device_tokens_are_held_for_releases_that_ignore_scopes() {
        use crate::management::client_tokens::{ClientTokenStore, DeviceScope};
        let home = temp("tokens");
        let tokens = ClientTokenStore::at(home.join(CLIENT_TOKENS));
        let full = tokens.issue("desktop").unwrap();
        let (chat, _) = tokens.issue_device_scoped("phone", "Pixel", "android", DeviceScope::Chat).unwrap();
        // A 0.1.5 target reads scopes: nothing to do.
        assert!(prepare(&home, &temp("displaced"), "0.1.5").unwrap().is_empty());
        let displaced = temp("displaced");
        let notes = prepare(&home, &displaced, "0.1.4").unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(tokens.tokens_path()).unwrap()).unwrap();
        let hash = crate::management::client_tokens::token_hash_hex;
        assert_eq!(raw["tokens"][0]["tokenSha256"], hash(&full.token).as_str(), "a full token stays usable");
        assert_ne!(raw["tokens"][1]["tokenSha256"], hash(&chat.token).as_str(), "0.1.4 cannot match the held one");
        let kept: serde_json::Value = serde_json::from_slice(&std::fs::read(displaced.join(OUT_DIR).join("client-tokens-held.json")).unwrap()).unwrap();
        assert_eq!(kept["tokens"][0]["tokenId"], chat.token_id.as_str());
        assert_eq!(kept["tokens"][0]["scope"], "chat");
        assert!(!kept.to_string().contains(&hash(&chat.token)), "no hash kept aside");
        assert!(prepare(&home, &temp("displaced2"), "0.1.4").unwrap().is_empty(), "a second pass is a no-op");
        // After a roll-forward the device is back with its scope.
        assert_eq!(tokens.list().unwrap()[1].scope, DeviceScope::Chat);
        assert_eq!(tokens.verify(&chat.token).unwrap(), Some(chat.token_id));
    }

    fn peer_message(id: &str, wake: bool) -> crate::management::v2::agent_bus_state::AgentBusQueuedMessage {
        use crate::agent_bus::peer_tell::{CONTROL_TYPE, CONTROL_TYPE_V2};
        const TO: &str = "cutex.11111111-1111-4111-8111-111111111111";
        let envelope: crate::agent_bus::model::AgentBusMessage = serde_json::from_value(json!({
            "id": id, "kind": "message", "from": "worker@alpha", "to": "helper",
            "toCutexSessionId": TO, "content": "see /tmp/x",
            "deliveryMode": if wake { "after_turn" } else { "passive" }, "triggerTurn": wake,
            "createdAtEpochSecs": 1, "senderKind": "agent",
            "controlType": if wake { CONTROL_TYPE_V2 } else { CONTROL_TYPE },
            "controlPayload": {"peerId": "ci-0123456789abcdef0123456789abcdef", "host": "alpha",
                "senderId": "cutex.remote", "senderName": "worker", "tellId": format!("ptl-{id}")},
            "externalMessageId": format!("ptell-{id}")
        }))
        .unwrap();
        let params = crate::app_server::bus_bridge::inter_agent_params("", "", TO, &envelope).unwrap();
        crate::management::v2::agent_bus_state::AgentBusQueuedMessage {
            owner_cutex_session_id: TO.into(),
            message_id: id.into(),
            from_cutex_session_id: None,
            to_cutex_session_id: TO.into(),
            from_runtime_agent_id: None,
            to_runtime_agent_id: Some("rt-to".into()),
            delivery_mode: envelope.delivery_mode.event_label().into(),
            content: envelope.content.clone(),
            queued_at: chrono::Utc::now(),
            semantic_sha256: crate::app_server::bus_bridge::inter_agent_semantic_sha256(&params),
            canonical_envelope: envelope,
        }
    }

    #[test]
    fn internal_targets_keep_permission_modes_and_peer_wakes_they_support() {
        use crate::agent_bus::delivery::AgentDeliveryMode;
        use crate::agent_bus::peer_tell::CONTROL_TYPE_V2;
        use crate::management::v2::agent_bus_state::AgentBusMessageRepository;

        for target in ["r252", "r256"] {
            let home = temp(&format!("keep-internal-{target}"));
            let sessions = home.join(SESSIONS);
            let claude: crate::runtime::claude::ClaudeSession = serde_json::from_value(json!({
                "deployment": {"python": "p", "adapter_source": "a", "claude_cli": "c", "claude_home": "h", "tui_binary": "t"},
                "thread_id": "t", "state_dir": "s", "model": "m", "initial_permission_mode": "auto"
            })).unwrap();
            crate::session::store::with_locked_session_rows(&sessions, |rows| {
                let mut record = crate::session::model::CutexSessionRecord::new_at(
                    "cutex.keep".into(), Some("thread-keep".into()), "devbox".into(), "/tmp/keep".into(), None,
                    "2026-10-24T00:00:00Z".into(),
                )?;
                record.claude_session = Some(claude);
                rows.insert_session("cutex.keep", record);
                rows.commit().map(|_| ())
            }).unwrap();
            let repository = AgentBusMessageRepository::open(home.join(AGENT_BUS)).unwrap();
            repository.record_queued_isolated(peer_message("wake-keep", true)).unwrap();

            prepare(&home, &temp(&format!("keep-internal-{target}-displaced")), target).unwrap();

            crate::session::store::with_cutex_session_store_at(&sessions, |store| {
                let mode = store.sessions["cutex.keep"].claude_session.as_ref().and_then(|session| session.initial_permission_mode.as_deref());
                assert_eq!(mode, Some("auto"), "{target}");
            }).unwrap();
            let wake = AgentBusMessageRepository::open(home.join(AGENT_BUS)).unwrap().canonical_message("wake-keep").unwrap();
            assert_eq!(wake.control_type.as_deref(), Some(CONTROL_TYPE_V2), "{target}");
            assert_eq!((wake.delivery_mode, wake.trigger_turn), (AgentDeliveryMode::AfterTurn, true), "{target}");
        }
    }

    #[test]
    fn pending_cross_host_wakes_become_notes_for_releases_without_wakes() {
        use crate::agent_bus::delivery::AgentDeliveryMode;
        use crate::agent_bus::peer_tell::CONTROL_TYPE;
        use crate::management::v2::agent_bus_state::AgentBusMessageRepository;
        let home = temp("wakes");
        let repository = AgentBusMessageRepository::open(home.join(AGENT_BUS)).unwrap();
        repository.record_queued_isolated(peer_message("wake-1", true)).unwrap();
        repository.record_queued_isolated(peer_message("note-1", false)).unwrap();
        let note_before = repository.snapshot_by_message_id("note-1").unwrap().unwrap();
        let current_target = prepare(&home, &temp("displaced"), "0.1.5").unwrap();
        assert_eq!(current_target.len(), 1, "SQLite export only: {current_target:?}");
        assert_eq!(
            repository.canonical_message("wake-1").unwrap().control_type.as_deref(),
            Some(crate::agent_bus::peer_tell::CONTROL_TYPE_V2),
        );
        let displaced = temp("displaced");
        let notes = prepare(&home, &displaced, "0.1.4").unwrap();
        assert_eq!(notes.len(), 2, "wake downgrade plus SQLite export: {notes:?}");
        // Read fresh, as the older release would.
        let repository = AgentBusMessageRepository::open(home.join(AGENT_BUS)).unwrap();
        let wake = repository.canonical_message("wake-1").unwrap();
        assert_eq!(wake.control_type.as_deref(), Some(CONTROL_TYPE));
        assert_eq!((wake.delivery_mode.clone(), wake.trigger_turn), (AgentDeliveryMode::Passive, false));
        assert_eq!(wake.external_message_id.as_deref(), Some("ptell-wake-1"));
        let snapshot = repository.snapshot_by_message_id("wake-1").unwrap().unwrap();
        assert_eq!((snapshot.state.as_str(), snapshot.delivery_mode.as_str()), ("pending", "passive"));
        // The digest is the note's: what a v1-only release recomputes.
        let (_, text) = crate::agent_bus::peer_tell::model_text(&wake).unwrap();
        assert!(!text.contains("Wake:"), "{text}");
        let params = crate::app_server::bus_bridge::inter_agent_params("", "", &snapshot.to_cutex_session_id, &wake).unwrap();
        assert_eq!(snapshot.semantic_sha256.as_deref(), Some(crate::app_server::bus_bridge::inter_agent_semantic_sha256(&params).as_str()));
        assert_eq!(repository.snapshot_by_message_id("note-1").unwrap().unwrap(), note_before, "a note stays");
        let kept: serde_json::Value = serde_json::from_slice(&std::fs::read(displaced.join(OUT_DIR).join("peer-wakes.json")).unwrap()).unwrap();
        assert_eq!(kept["messages"][0]["messageId"], "wake-1");
        assert_eq!(kept["messages"][0]["canonicalEnvelope"]["controlType"], "cutex.peer.tell.v2");
        let second = prepare(&home, &temp("displaced2"), "0.1.4").unwrap();
        assert_eq!(second.len(), 1, "a second pass only refreshes the rollback handoff: {second:?}");
    }

    #[test]
    fn unacknowledged_new_owner_device_notices_are_kept_aside() {
        let home = temp("notices");
        let users = home.join(USERS);
        std::fs::create_dir_all(users.parent().unwrap()).unwrap();
        let state = json!({"schema": "cutex/users/v1", "newOwnerDevices": [
            {"tokenId": "mct-1", "at": "2026-10-20T00:00:00Z", "via": "pairing", "announced": true}]});
        std::fs::write(&users, serde_json::to_vec(&state).unwrap()).unwrap();
        let displaced = temp("displaced");
        assert!(prepare(&home, &temp("displaced"), "0.1.5").unwrap().is_empty());
        assert_eq!(prepare(&home, &displaced, "0.1.4").unwrap().len(), 1);
        let kept: serde_json::Value = serde_json::from_slice(&std::fs::read(displaced.join(OUT_DIR).join("new-owner-devices.json")).unwrap()).unwrap();
        assert_eq!(kept["newOwnerDevices"][0]["tokenId"], "mct-1");
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&std::fs::read(&users).unwrap()).unwrap(), state, "store unchanged");
    }
}
