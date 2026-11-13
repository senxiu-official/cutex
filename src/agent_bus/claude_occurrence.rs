//! Agent Bus owns Claude presence independently of observer transports.
//! Durable, process and file reads happen outside the Bus state mutex.
use super::{
    groups::agent_from_register_request, model::AgentBusRegisterRequest, store::AgentBusState,
};
use crate::session::model::{CutexSessionRecord, CutexSessionRuntimeBackend};
use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

pub const PATH: &str = "/api/agents/claude-occurrence";
/// Durable MCP authority changes immediately; roster cleanup takes one tick.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Default)]
struct EndChecks {
    pending: std::collections::HashSet<String>,
    last: std::collections::HashMap<String, (Occurrence, Instant)>,
    sender: Option<std::sync::mpsc::SyncSender<Verification>>,
}
struct Verification {
    state: std::sync::Weak<Mutex<AgentBusState>>,
    record: CutexSessionRecord,
    identity: ProcessIdentity,
    expected: Option<Owner>,
}
static END_CHECKS: OnceLock<Mutex<EndChecks>> = OnceLock::new();
fn end_checks() -> &'static Mutex<EndChecks> {
    END_CHECKS.get_or_init(Default::default)
}
struct EndCheckPermit(String);
impl Drop for EndCheckPermit {
    fn drop(&mut self) {
        end_checks()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .remove(&self.0);
    }
}
fn reserve_end_check(checks: &mut EndChecks, occurrence: &Occurrence, now: Instant) -> bool {
    if checks.pending.contains(&occurrence.cutex_session_id)
        || checks
            .last
            .get(&occurrence.cutex_session_id)
            .is_some_and(|(previous, at)| {
                previous == occurrence && now.duration_since(*at) < Duration::from_secs(10)
            })
    {
        return false;
    }
    checks
        .last
        .retain(|_, (_, at)| now.duration_since(*at) < Duration::from_secs(60));
    if checks.last.len() >= 512 {
        return false;
    }
    checks.last.insert(
        occurrence.cutex_session_id.clone(),
        (occurrence.clone(), now),
    );
    checks.pending.insert(occurrence.cutex_session_id.clone());
    true
}
fn schedule_verification(
    state: &Arc<Mutex<AgentBusState>>,
    record: &CutexSessionRecord,
    identity: ProcessIdentity,
) {
    let Some(occurrence) = Occurrence::from_record(record) else {
        return;
    };
    let expected = state.lock().ok().and_then(|state| {
        state
            .claude_occurrences
            .get(&record.cutex_session_id)
            .cloned()
    });
    if expected
        .as_ref()
        .is_some_and(|owner| owner.occurrence == occurrence && !owner.active)
    {
        return;
    }
    let mut checks = end_checks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !reserve_end_check(&mut checks, &occurrence, Instant::now()) {
        return;
    }
    if checks.sender.is_none() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Verification>(64);
        if let Err(error) = std::thread::Builder::new()
            .name("cutex-claude-verify".into())
            .spawn(move || {
                while let Ok(work) = rx.recv() {
                    let _permit = EndCheckPermit(work.record.cutex_session_id.clone());
                    if let Err(error) = verify(work) {
                        eprintln!("Claude occurrence verification unavailable: {error:#}");
                    }
                }
            })
        {
            checks.pending.remove(&occurrence.cutex_session_id);
            eprintln!("Claude verification worker unavailable: {error}");
            return;
        }
        checks.sender = Some(tx);
    }
    let work = Verification {
        state: Arc::downgrade(state),
        record: record.clone(),
        identity,
        expected,
    };
    enqueue_reserved(&mut checks, work);
}

fn enqueue_reserved(checks: &mut EndChecks, work: Verification) {
    let session = work.record.cutex_session_id.clone();
    match checks.sender.as_ref().unwrap().try_send(work) {
        Ok(()) => {}
        Err(std::sync::mpsc::TrySendError::Full(_)) => {
            checks.pending.remove(&session);
            checks.last.remove(&session);
        }
        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
            // A lost worker must not leave queued sessions permanently pending.
            checks.sender = None;
            checks.pending.clear();
            checks.last.clear();
        }
    }
}
fn verify(work: Verification) -> anyhow::Result<()> {
    use crate::runtime::claude::ClaudeOccurrenceCheck::*;
    let result = crate::runtime::claude::verify_occurrence(
        &work.record,
        work.identity.started_at,
        &work.identity.adapter_runtime_id,
    )?;
    let Some(state) = work.state.upgrade() else {
        return Ok(());
    };
    if result == Ended {
        return reconcile(&state);
    }
    if result == Unknown {
        return Ok(());
    }
    // Native IO has finished. Re-read durable authority outside the Bus mutex,
    // then CAS the owner observed before the read; delayed results lose.
    let Some(record) = crate::session::store::shared_session_row(&work.record.cutex_session_id)?
    else {
        return Ok(());
    };
    if Occurrence::from_record(&record) != Occurrence::from_record(&work.record) {
        return Ok(());
    }
    let authority = crate::agent_management::archive_authority_snapshot_shared()?;
    let mut checked = checked(
        work.record.cutex_session_id.clone(),
        work.expected.clone(),
        Some(record.clone()),
        &authority,
        &crate::platform::host::current_host_name(),
    );
    if checked.current.is_none() {
        return Ok(());
    }
    if !matches!(&checked.evidence, Evidence::Live(identity) | Evidence::Unverified(identity) if identity == &work.identity)
    {
        return Ok(());
    }
    checked.evidence = Evidence::Live(work.identity.clone());
    let changed = {
        let mut locked = state
            .lock()
            .map_err(|_| anyhow::anyhow!("Bus unavailable"))?;
        if locked.claude_occurrences.get(&work.record.cutex_session_id) != work.expected.as_ref() {
            return Ok(());
        }
        if result == EndedUnverified {
            let occurrence = Occurrence::from_record(&work.record).unwrap();
            locked.claude_occurrences.insert(
                occurrence.cutex_session_id.clone(),
                Owner {
                    occurrence: occurrence.clone(),
                    identity: work.identity,
                    active: false,
                },
            );
            eprintln!(
                "Claude occurrence revoked for {}: ended_unverified; durable binding retained",
                occurrence.cutex_session_id
            );
            remove_roster(&mut locked, &occurrence)
        } else {
            apply(&mut locked, checked, crate::platform::now_epoch_secs()).1
        }
    };
    if changed {
        persist_snapshot(&state)?;
    }
    if result == Live && changed {
        redrive_registered(&state);
    }
    Ok(())
}

fn redrive_registered(state: &Arc<Mutex<AgentBusState>>) {
    let callback = state.lock().ok().and_then(|state| state.claude_registered);
    if let Some(callback) = callback {
        if let Err(error) = callback(state) {
            eprintln!("Claude registration redrive unavailable: {error:#}");
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Occurrence {
    pub cutex_session_id: String,
    pub thread_id: String,
    pub generation: u64,
    pub connection_id: String,
    pub runtime_agent_id: String,
    pub pid: u32,
}
impl Occurrence {
    pub fn from_record(record: &CutexSessionRecord) -> Option<Self> {
        if record.runtime_backend != CutexSessionRuntimeBackend::ClaudeSubscription
            || record.runtime_generation == 0
        {
            return None;
        }
        let session = record.claude_session.as_ref()?;
        let connection_id = session.connection_id.clone()?;
        let runtime_agent_id = record.current_runtime_agent_id.clone()?;
        let pid = record.runtime_pid.filter(|pid| *pid > 0)?;
        if runtime_agent_id != format!("claude.{connection_id}") {
            return None;
        }
        Some(Self {
            cutex_session_id: record.cutex_session_id.clone(),
            thread_id: session.thread_id.clone(),
            generation: record.runtime_generation,
            connection_id,
            runtime_agent_id,
            pid,
        })
    }
}
/// Confirm and offline are only recheck hints, never trusted grants/revocations.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub occurrence: Occurrence,
    pub confirmed_offline: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct ProcessIdentity {
    started_at: DateTime<Utc>,
    adapter_runtime_id: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owner {
    occurrence: Occurrence,
    identity: ProcessIdentity,
    active: bool,
}
enum Evidence {
    Live(ProcessIdentity),
    Unverified(ProcessIdentity),
    Dead(&'static str),
    Unavailable,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor {
    pid: u32,
    runtime_id: String,
}
// Do not deserialize or retain the credential fields.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Binding {
    thread_id: String,
    runtime_agent_id: String,
    generation: u64,
}

fn read_stamped<T: serde::de::DeserializeOwned>(
    path: &std::path::Path,
) -> anyhow::Result<(T, DateTime<Utc>)> {
    read_stamped_with(path, || {})
}
fn read_stamped_with<T: serde::de::DeserializeOwned>(
    path: &std::path::Path,
    after_read: impl FnOnce(),
) -> anyhow::Result<(T, DateTime<Utc>)> {
    let before = std::fs::metadata(path)?.modified()?;
    let bytes = std::fs::read(path)?;
    after_read();
    let after = std::fs::metadata(path)?.modified()?;
    anyhow::ensure!(
        before == after && before <= std::time::SystemTime::now() && bytes == std::fs::read(path)?,
        "Claude evidence timestamp changed/invalid"
    );
    Ok((serde_json::from_slice(&bytes)?, before.into()))
}
fn inspect(
    record: &CutexSessionRecord,
    occurrence: &Occurrence,
    previous: Option<&Owner>,
) -> Evidence {
    inspect_with(
        record,
        occurrence,
        previous,
        crate::platform::process::process_liveness,
        crate::platform::process::process_started_at,
    )
}

fn inspect_with(
    record: &CutexSessionRecord,
    occurrence: &Occurrence,
    previous: Option<&Owner>,
    liveness: impl FnOnce(u32) -> Option<bool>,
    birth: impl FnOnce(u32) -> anyhow::Result<DateTime<Utc>>,
) -> Evidence {
    match liveness(occurrence.pid) {
        Some(false) => return Evidence::Dead("pid_dead"),
        None => return Evidence::Unavailable,
        Some(true) => {}
    }
    let Ok(started_at) = birth(occurrence.pid) else {
        return Evidence::Unavailable;
    };
    if previous.is_some_and(|owner| {
        owner.occurrence == *occurrence && owner.identity.started_at != started_at
    }) {
        return Evidence::Dead("pid_identity_mismatch");
    }
    if let Some(owner) = previous.filter(|owner| owner.occurrence == *occurrence && owner.active) {
        // Birth identity was anchored at adoption. A transport or binding file
        // rewrite is not a new process, and need not be read every second.
        return Evidence::Live(owner.identity.clone());
    }
    let session = record.claude_session.as_ref().unwrap();
    let evidence = (|| -> anyhow::Result<ProcessIdentity> {
        let (descriptor, descriptor_at) =
            read_stamped::<Descriptor>(&session.state_dir.join("server.json"))?;
        let (binding, _binding_at) =
            read_stamped::<Binding>(&session.state_dir.join("cutex-mcp-binding.json"))?;
        let (descriptor_again, descriptor_again_at) =
            read_stamped::<Descriptor>(&session.state_dir.join("server.json"))?;
        // Stable launch documents identify a candidate. Initial/startup and
        // old-mtime candidates also require authenticated logical-owner proof;
        // file timestamps alone never grant a reused PID an occurrence.
        anyhow::ensure!(
            descriptor.pid == occurrence.pid
                && !descriptor.runtime_id.is_empty()
                && descriptor_again.pid == descriptor.pid
                && descriptor_again.runtime_id == descriptor.runtime_id
                && descriptor_again_at == descriptor_at
                && binding.thread_id == occurrence.thread_id
                && binding.runtime_agent_id == occurrence.runtime_agent_id
                && binding.generation == occurrence.generation,
            "Claude launch evidence mismatch"
        );
        Ok(ProcessIdentity {
            started_at,
            adapter_runtime_id: descriptor.runtime_id,
        })
    })();
    match evidence {
        Ok(identity)
            if previous.is_some_and(|owner| {
                owner.occurrence == *occurrence && owner.identity != identity
            }) =>
        {
            Evidence::Dead("adapter_identity_mismatch")
        }
        Ok(identity) => Evidence::Unverified(identity),
        Err(_) => Evidence::Unavailable,
    }
}
fn registration(
    record: &CutexSessionRecord,
    occurrence: &Occurrence,
    now: u64,
) -> super::model::AgentBusAgent {
    let name = crate::session::service::cutex_session_display_name(record);
    let mut agent = agent_from_register_request(
        AgentBusRegisterRequest {
            id: occurrence.runtime_agent_id.clone(),
            name: name.clone(),
            base_name: Some(name),
            thread_name: record.thread_name.clone(),
            path_key: None,
            session_id: Some(occurrence.thread_id.clone()),
            profile: record.profile.clone().unwrap_or_else(|| "Claude".into()),
            cwd: record.cwd.clone(),
            pid: occurrence.pid,
            host_id: Some(record.host_id.clone()),
            groups: record.agent_groups.clone(),
            registration_class: record.registration_class,
        },
        now,
    );
    agent.cutex_session_id = Some(record.cutex_session_id.clone());
    agent
}
fn remove_roster(state: &mut AgentBusState, occurrence: &Occurrence) -> bool {
    if state
        .agents
        .get(&occurrence.runtime_agent_id)
        .is_some_and(|agent| {
            agent.pid == occurrence.pid
                && agent.session_id.as_deref() == Some(&occurrence.thread_id)
        })
    {
        state.agents.remove(&occurrence.runtime_agent_id);
        state.messages.remove(&occurrence.runtime_agent_id);
        true
    } else {
        false
    }
}
struct Checked {
    session: String,
    expected: Option<Owner>,
    current: Option<(Occurrence, Arc<CutexSessionRecord>)>,
    evidence: Evidence,
}
fn revoke(state: &mut AgentBusState, session: &str, reason: &str) -> (&'static str, bool) {
    let Some(owner) = state.claude_occurrences.get_mut(session) else {
        return ("stale", false);
    };
    if !owner.active {
        return ("unchanged", false);
    }
    owner.active = false;
    let occurrence = owner.occurrence.clone();
    eprintln!("Claude occurrence revoked for {session}: {reason}");
    ("revoked", remove_roster(state, &occurrence))
}
fn apply(state: &mut AgentBusState, checked: Checked, now: u64) -> (&'static str, bool) {
    // A slow read never overwrites a newer CLI/controller result.
    if state.claude_occurrences.get(&checked.session) != checked.expected.as_ref() {
        return ("stale", false);
    }
    let Some((occurrence, record)) = checked.current else {
        let result = revoke(state, &checked.session, "durable_or_lifecycle_mismatch");
        state.claude_occurrences.remove(&checked.session);
        return result;
    };
    match checked.evidence {
        Evidence::Unavailable | Evidence::Unverified(_) => {
            if checked
                .expected
                .as_ref()
                .is_some_and(|owner| owner.occurrence != occurrence)
            {
                let result = revoke(state, &checked.session, "durable_occurrence_replaced");
                state.claude_occurrences.remove(&checked.session);
                result
            } else {
                (
                    if checked.expected.as_ref().is_some_and(|owner| owner.active) {
                        "unchanged"
                    } else {
                        "stale"
                    },
                    false,
                )
            }
        }
        Evidence::Dead(reason) => revoke(state, &checked.session, reason),
        Evidence::Live(identity) => {
            if let Some(previous) = &checked.expected {
                if previous.occurrence == occurrence && !previous.active {
                    return ("stale", false);
                }
                if previous.occurrence != occurrence {
                    remove_roster(state, &previous.occurrence);
                }
            }
            let created = checked
                .expected
                .as_ref()
                .is_none_or(|owner| owner.occurrence != occurrence);
            state.claude_occurrences.insert(
                checked.session,
                Owner {
                    occurrence: occurrence.clone(),
                    identity,
                    active: true,
                },
            );
            let missing = !state.agents.contains_key(&occurrence.runtime_agent_id);
            if created || missing {
                state.agents.insert(
                    occurrence.runtime_agent_id.clone(),
                    registration(&record, &occurrence, now),
                );
            } else if let Some(agent) = state.agents.get_mut(&occurrence.runtime_agent_id) {
                agent.last_seen_epoch_secs = now;
            }
            (
                if created || missing {
                    "registered"
                } else {
                    "unchanged"
                },
                created || missing,
            )
        }
    }
}
fn checked(
    session: String,
    previous: Option<Owner>,
    record: Option<Arc<CutexSessionRecord>>,
    authority: &crate::agent_management::AgentManagementSnapshot,
    host: &str,
) -> Checked {
    let current = record
        .filter(|record| {
            crate::runtime::lifecycle::cutex_session_host_is_local(&record.host_id, host)
                && !crate::agent_management::agent_execution_blocked_in(authority, record)
        })
        .and_then(|record| Occurrence::from_record(&record).map(|occurrence| (occurrence, record)));
    let evidence = current
        .as_ref()
        .map_or(Evidence::Unavailable, |(occurrence, record)| {
            inspect(record, occurrence, previous.as_ref())
        });
    Checked {
        session,
        expected: previous,
        current,
        evidence,
    }
}
fn persist_snapshot(state: &Arc<Mutex<AgentBusState>>) -> anyhow::Result<()> {
    // Preserve the existing serialized registry-write contract. Cloning here
    // and later writing outside that lock could overwrite a newer ordinary
    // register/unregister. Authority/process/native reads never hold this lock.
    super::store::persist_agent_bus_registry(state)
}
pub fn update(
    state: &Arc<Mutex<AgentBusState>>,
    request: &Request,
) -> anyhow::Result<&'static str> {
    let occurrence = &request.occurrence;
    let previous = state
        .lock()
        .map_err(|_| anyhow::anyhow!("Bus unavailable"))?
        .claude_occurrences
        .get(&occurrence.cutex_session_id)
        .cloned();
    let record = crate::session::store::shared_session_row(&occurrence.cutex_session_id)?;
    if record.as_deref().and_then(Occurrence::from_record).as_ref() != Some(occurrence) {
        return Ok("stale");
    }
    let authority = crate::agent_management::archive_authority_snapshot_shared()?;
    let checked = checked(
        occurrence.cutex_session_id.clone(),
        previous,
        record.clone(),
        &authority,
        &crate::platform::host::current_host_name(),
    );
    let pending = match &checked.evidence {
        Evidence::Unverified(identity) => Some(identity.clone()),
        Evidence::Live(identity) if request.confirmed_offline => Some(identity.clone()),
        _ => None,
    };
    let (outcome, changed) = {
        let mut locked = state
            .lock()
            .map_err(|_| anyhow::anyhow!("Bus unavailable"))?;
        apply(&mut locked, checked, crate::platform::now_epoch_secs())
    };
    if changed {
        persist_snapshot(state)?;
    }
    if let (Some(identity), Some(record)) = (pending, record.as_deref()) {
        schedule_verification(state, record, identity);
    }
    Ok(outcome)
}
pub fn owned(state: &AgentBusState, runtime_agent_id: &str) -> bool {
    state
        .claude_occurrences
        .values()
        .any(|owner| owner.active && owner.occurrence.runtime_agent_id == runtime_agent_id)
}
fn current_claim(record: &CutexSessionRecord, host: &str) -> bool {
    record.runtime_pid.is_some_and(|pid| pid > 0)
        && record.runtime_generation > 0
        && record.runtime_backend == CutexSessionRuntimeBackend::ClaudeSubscription
        && !record.is_retired()
        && crate::runtime::lifecycle::cutex_session_host_is_local(&record.host_id, host)
        && record.claude_session.as_ref().is_some_and(|session| {
            session.connection_id.as_deref().is_some_and(|connection| {
                !connection.is_empty()
                    && record
                        .current_runtime_agent_id
                        .as_deref()
                        .and_then(|id| id.strip_prefix("claude."))
                        == Some(connection)
            })
        })
}
fn collect_checks(
    previous: &std::collections::HashMap<String, Owner>,
    records: &crate::session::model::CutexSessionStore,
    host: &str,
    authority: impl FnOnce() -> Result<
        Arc<crate::agent_management::AgentManagementSnapshot>,
        crate::agent_management::AgentManagementError,
    >,
) -> anyhow::Result<Vec<Checked>> {
    let candidates = records
        .sessions
        .values()
        .filter(|record| current_claim(record, host))
        .map(|record| record.cutex_session_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let authority = if candidates.is_empty() {
        None
    } else {
        Some(authority()?)
    };
    let mut sessions = candidates.clone();
    sessions.extend(previous.keys().cloned());
    Ok(sessions
        .into_iter()
        .map(|session| {
            let expected = previous.get(&session).cloned();
            if candidates.contains(&session) {
                checked(
                    session.clone(),
                    expected,
                    records.sessions.shared(&session),
                    authority.as_deref().unwrap(),
                    host,
                )
            } else {
                Checked {
                    session,
                    expected,
                    current: None,
                    evidence: Evidence::Unavailable,
                }
            }
        })
        .collect())
}
pub fn reconcile(state: &Arc<Mutex<AgentBusState>>) -> anyhow::Result<()> {
    let previous = state
        .lock()
        .map_err(|_| anyhow::anyhow!("Bus unavailable"))?
        .claude_occurrences
        .clone();
    let records = crate::session::store::load_cutex_session_store_shared()?;
    let host = crate::platform::host::current_host_name();
    let checked = collect_checks(
        &previous,
        &records,
        &host,
        crate::agent_management::archive_authority_snapshot_shared,
    )?;
    let pending = checked
        .iter()
        .filter_map(|checked| match (&checked.evidence, &checked.current) {
            (Evidence::Unverified(identity), Some((_, record))) => {
                Some((record.clone(), identity.clone()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let (changed, registered) = {
        let mut locked = state
            .lock()
            .map_err(|_| anyhow::anyhow!("Bus unavailable"))?;
        let mut changed = false;
        let mut registered = false;
        for checked in checked {
            let (outcome, did_change) =
                apply(&mut locked, checked, crate::platform::now_epoch_secs());
            changed |= did_change;
            registered |= outcome == "registered";
        }
        (changed, registered)
    };
    if changed {
        persist_snapshot(state)?;
    }
    if registered {
        redrive_registered(state);
    }
    for (record, identity) in pending {
        schedule_verification(state, &record, identity);
    }
    Ok(())
}
pub fn spawn(
    state: &Arc<Mutex<AgentBusState>>,
    registered: fn(&Arc<Mutex<AgentBusState>>) -> anyhow::Result<usize>,
) -> anyhow::Result<()> {
    state
        .lock()
        .map_err(|_| anyhow::anyhow!("Bus unavailable"))?
        .claude_registered = Some(registered);
    // Begin re-adoption before serving, with no observer confirmation, native start,
    // generation/binding write or input flush.
    if let Err(error) = readopt_startup(state) {
        eprintln!("Claude occurrence startup unavailable: {error:#}");
    }
    let state = Arc::downgrade(state);
    std::thread::Builder::new()
        .name("cutex-claude-occurrences".into())
        .spawn(move || loop {
            std::thread::sleep(CHECK_INTERVAL);
            let Some(state) = state.upgrade() else { break };
            if let Err(error) = reconcile(&state) {
                eprintln!("Claude occurrence renewal unavailable: {error:#}");
            }
        })
        .context("spawn Claude occurrence controller")?;
    Ok(())
}

pub fn readopt_startup(state: &Arc<Mutex<AgentBusState>>) -> anyhow::Result<()> {
    // Restored heartbeats are not proof of a live original process. Discard
    // Claude presence before any request is served, then re-adopt only exact,
    // identity-checked claims. This also closes the first-adoption PID reuse gap.
    {
        let mut state = state
            .lock()
            .map_err(|_| anyhow::anyhow!("Bus unavailable"))?;
        state.agents.retain(|id, _| !id.starts_with("claude."));
        state.claude_occurrences.clear();
    }
    reconcile(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    pub(super) fn record(
        root: &std::path::Path,
        generation: u64,
        connection: &str,
    ) -> CutexSessionRecord {
        let mut record = CutexSessionRecord::new(
            "cutex.occurrence".into(),
            None,
            crate::platform::host::current_host_name(),
            root.to_string_lossy().into_owned(),
            None,
        )
        .unwrap();
        record.runtime_backend = CutexSessionRuntimeBackend::ClaudeSubscription;
        record.agent_enabled = true;
        record.registration_class = super::super::model::AgentRegistrationClass::Persistent;
        record.runtime_generation = generation;
        record.runtime_pid = Some(std::process::id());
        record.current_runtime_agent_id = Some(format!("claude.{connection}"));
        record.claude_session = Some(serde_json::from_value(json!({
            "deployment":{"python":"unused", "adapter_source":"unused", "claude_cli":"unused", "claude_home":"unused", "tui_binary":"unused"},
            "thread_id":"018e0000-0000-7000-8000-0000b0009ae4", "state_dir":root, "model":"sonnet", "instructions":"", "connection_id":connection,
            "allow_write":false, "allow_exec":false
        })).unwrap());
        record
    }
    fn identity() -> ProcessIdentity {
        ProcessIdentity {
            started_at: crate::platform::process::process_started_at(std::process::id()).unwrap(),
            adapter_runtime_id: "adapter".into(),
        }
    }
    fn observation(
        state: &AgentBusState,
        record: &CutexSessionRecord,
        evidence: Evidence,
    ) -> Checked {
        Checked {
            session: record.cutex_session_id.clone(),
            expected: state
                .claude_occurrences
                .get(&record.cutex_session_id)
                .cloned(),
            current: Some((
                Occurrence::from_record(record).unwrap(),
                Arc::new(record.clone()),
            )),
            evidence,
        }
    }
    fn acquire(state: &mut AgentBusState, record: &CutexSessionRecord, now: u64) {
        let checked = observation(state, record, Evidence::Live(identity()));
        apply(state, checked, now);
    }
    #[test]
    fn transport_loss_does_not_release_occurrence_and_uncertainty_does_not_renew() {
        let record = record(std::path::Path::new("unused"), 1, "one");
        let mut state = AgentBusState::default();
        acquire(&mut state, &record, 10);
        // Two independent callers confirming the same exact occurrence share
        // the Bus owner. Neither owns a registration-only managed bridge.
        acquire(&mut state, &record, 11);
        let before = state.claude_occurrences.clone();
        let failed_probe = observation(&state, &record, Evidence::Unavailable);
        assert_eq!(apply(&mut state, failed_probe, 50), ("unchanged", false));
        assert_eq!(state.agents["claude.one"].last_seen_epoch_secs, 11);
        assert_eq!(state.claude_occurrences, before);
        // Native evidence remains valid despite a real observer socket loss.
        acquire(&mut state, &record, 200);
        assert_eq!(state.agents["claude.one"].last_seen_epoch_secs, 200);
        assert!(owned(&state, "claude.one"));
    }
    #[test]
    fn delayed_old_scan_cannot_revoke_or_revive_replacement() {
        let old = record(std::path::Path::new("unused"), 1, "one");
        let new = record(std::path::Path::new("unused"), 2, "two");
        let mut state = AgentBusState::default();
        acquire(&mut state, &old, 1);
        let delayed = observation(&state, &old, Evidence::Dead("pid_dead"));
        let delayed_refresh = observation(&state, &old, Evidence::Live(identity()));
        acquire(&mut state, &new, 2);
        assert_eq!(apply(&mut state, delayed, 3), ("stale", false));
        assert_eq!(apply(&mut state, delayed_refresh, 3), ("stale", false));
        assert!(!state.agents.contains_key("claude.one"));
        assert!(state.agents.contains_key("claude.two"));
    }
    #[test]
    fn confirmed_death_and_durable_close_revoke_and_cannot_resurrect_same_tuple() {
        for reason in [
            "pid_dead",
            "pid_identity_mismatch",
            "adapter_identity_mismatch",
            "lifecycle_blocked",
        ] {
            let record = record(std::path::Path::new("unused"), 1, "one");
            let mut state = AgentBusState::default();
            acquire(&mut state, &record, 1);
            let mut check = observation(&state, &record, Evidence::Dead(reason));
            if reason == "lifecycle_blocked" {
                check.current = None;
            }
            assert_eq!(apply(&mut state, check, 2), ("revoked", true));
            assert!(!owned(&state, "claude.one"));
            assert!(state.agents.is_empty());
            if reason == "lifecycle_blocked" {
                continue;
            }
            let check = observation(&state, &record, Evidence::Live(identity()));
            assert_eq!(apply(&mut state, check, 3), ("stale", false));
        }
    }
    #[test]
    fn every_tuple_field_is_part_of_the_occurrence_fence() {
        let record = record(std::path::Path::new("unused"), 1, "one");
        let occurrence = Occurrence::from_record(&record).unwrap();
        for field in [
            "cutex_session_id",
            "thread_id",
            "generation",
            "connection_id",
            "runtime_agent_id",
            "pid",
        ] {
            let mut changed = occurrence.clone();
            match field {
                "cutex_session_id" => changed.cutex_session_id.push('x'),
                "thread_id" => changed.thread_id.push('x'),
                "generation" => changed.generation += 1,
                "connection_id" => changed.connection_id.push('x'),
                "runtime_agent_id" => changed.runtime_agent_id.push('x'),
                _ => changed.pid += 1,
            }
            assert_ne!(occurrence, changed, "{field}");
        }
        let mut bad = record.clone();
        bad.current_runtime_agent_id = Some("claude.other".into());
        assert!(Occurrence::from_record(&bad).is_none());
    }
    #[test]
    fn first_adoption_rejects_reused_pid_and_unstable_or_future_launch_documents() {
        let root = std::env::temp_dir().join(format!("claude-evidence-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let record = record(&root, 1, "one");
        let occurrence = Occurrence::from_record(&record).unwrap();
        let server = root.join("server.json");
        let binding = root.join("cutex-mcp-binding.json");
        std::fs::write(
            &server,
            json!({"pid":std::process::id(),"runtimeId":"adapter"}).to_string(),
        )
        .unwrap();
        std::fs::write(&binding, json!({"threadId":occurrence.thread_id,"runtimeAgentId":occurrence.runtime_agent_id,"generation":1,"token":"never retain"}).to_string()).unwrap();
        assert!(matches!(
            inspect(&record, &occurrence, None),
            Evidence::Unverified(_)
        ));
        let mut previous = Owner {
            occurrence: occurrence.clone(),
            identity: identity(),
            active: true,
        };
        previous.identity.started_at -= chrono::Duration::seconds(1);
        assert!(matches!(
            inspect(&record, &occurrence, Some(&previous)),
            Evidence::Dead("pid_identity_mismatch")
        ));
        let born = identity().started_at;
        let past: std::time::SystemTime = (born - chrono::Duration::seconds(1)).into();
        std::fs::File::options()
            .write(true)
            .open(&server)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(past))
            .unwrap();
        let pending = observation(
            &AgentBusState::default(),
            &record,
            inspect(&record, &occurrence, None),
        );
        let mut not_adopted = AgentBusState::default();
        assert_eq!(apply(&mut not_adopted, pending, 1), ("stale", false));
        assert!(not_adopted.agents.is_empty());
        let future = std::time::SystemTime::now() + Duration::from_secs(100);
        std::fs::File::options()
            .write(true)
            .open(&server)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(future))
            .unwrap();
        assert!(read_stamped::<Descriptor>(&server).is_err());
        std::fs::write(
            &server,
            json!({"pid":std::process::id(),"runtimeId":"adapter"}).to_string(),
        )
        .unwrap();
        assert!(
            read_stamped_with::<Descriptor>(&server, || std::fs::write(&server, b"{}").unwrap())
                .is_err()
        );
        assert!(matches!(
            inspect(&record, &occurrence, None),
            Evidence::Unavailable
        ));
        std::fs::remove_file(&server).unwrap();
        assert!(matches!(
            inspect(&record, &occurrence, None),
            Evidence::Unavailable
        ));
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod correction_tests {
    use super::*;
    fn identity() -> ProcessIdentity {
        ProcessIdentity {
            started_at: crate::platform::process::process_started_at(std::process::id()).unwrap(),
            adapter_runtime_id: "adapter".into(),
        }
    }
    #[test]
    fn occurrence_unknown_pid_keeps_owner_and_later_recovers_without_a_new_generation() {
        let record = super::tests::record(std::path::Path::new("unused"), 1, "one");
        let occurrence = Occurrence::from_record(&record).unwrap();
        let mut state = AgentBusState::default();
        apply(
            &mut state,
            Checked {
                session: record.cutex_session_id.clone(),
                expected: None,
                current: Some((occurrence.clone(), Arc::new(record.clone()))),
                evidence: Evidence::Live(identity()),
            },
            10,
        );
        let previous = state.claude_occurrences[&record.cutex_session_id].clone();
        let evidence = inspect_with(
            &record,
            &occurrence,
            Some(&previous),
            |_| None,
            |_| panic!("UNKNOWN must not launch a PID fallback/helper"),
        );
        assert!(matches!(evidence, Evidence::Unavailable));
        assert_eq!(
            apply(
                &mut state,
                Checked {
                    session: record.cutex_session_id.clone(),
                    expected: Some(previous.clone()),
                    current: Some((occurrence.clone(), Arc::new(record.clone()))),
                    evidence
                },
                50
            ),
            ("unchanged", false)
        );
        assert!(owned(&state, "claude.one"));
        assert_eq!(state.agents["claude.one"].last_seen_epoch_secs, 10);
        let evidence = inspect_with(
            &record,
            &occurrence,
            Some(&previous),
            |_| Some(true),
            crate::platform::process::process_started_at,
        );
        assert!(matches!(evidence, Evidence::Live(_)));
        apply(
            &mut state,
            Checked {
                session: record.cutex_session_id.clone(),
                expected: Some(previous),
                current: Some((occurrence, Arc::new(record.clone()))),
                evidence,
            },
            100,
        );
        assert!(owned(&state, "claude.one"));
        assert_eq!(state.agents["claude.one"].last_seen_epoch_secs, 100);
        assert_eq!(record.runtime_generation, 1);
    }
    #[test]
    fn occurrence_offline_catalog_does_not_read_authority_and_active_rows_stay_shared() {
        let mut records = crate::session::model::CutexSessionStore::default();
        for i in 0..1024 {
            let mut record = super::tests::record(std::path::Path::new("unused"), 1, "one");
            record.cutex_session_id = format!("cutex.offline-{i}");
            record.runtime_pid = None;
            records
                .sessions
                .insert(record.cutex_session_id.clone(), record);
        }
        assert!(collect_checks(
            &Default::default(),
            &records,
            &crate::platform::host::current_host_name(),
            || panic!("offline-only host must not open archive authority")
        )
        .unwrap()
        .is_empty());
        let record = super::tests::record(std::path::Path::new("unused"), 1, "one");
        let occurrence = Occurrence::from_record(&record).unwrap();
        records
            .sessions
            .insert(record.cutex_session_id.clone(), record.clone());
        let previous = std::collections::HashMap::from([(
            record.cutex_session_id.clone(),
            Owner {
                occurrence,
                identity: identity(),
                active: true,
            },
        )]);
        let authority = Arc::new(crate::agent_management::AgentManagementSnapshot::empty());
        let checked = collect_checks(
            &previous,
            &records,
            &crate::platform::host::current_host_name(),
            || Ok(authority.clone()),
        )
        .unwrap();
        assert_eq!(checked.len(), 1);
        assert!(Arc::ptr_eq(
            &checked[0].current.as_ref().unwrap().1,
            &records.sessions.shared(&record.cutex_session_id).unwrap()
        ));
    }
    #[test]
    fn occurrence_full_or_disconnected_queue_does_not_leave_sticky_pending_owners() {
        let state = Arc::new(Mutex::new(AgentBusState::default()));
        let base = super::tests::record(std::path::Path::new("unused"), 1, "one");
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut checks = EndChecks {
            sender: Some(tx),
            ..Default::default()
        };
        let now = Instant::now();
        let work = |record: CutexSessionRecord| Verification {
            state: Arc::downgrade(&state),
            record,
            identity: identity(),
            expected: None,
        };
        let first = Occurrence::from_record(&base).unwrap();
        assert!(reserve_end_check(&mut checks, &first, now));
        enqueue_reserved(&mut checks, work(base.clone()));
        assert!(!reserve_end_check(&mut checks, &first, now));
        let mut second = base.clone();
        second.cutex_session_id = "cutex.second".into();
        let second_identity = Occurrence::from_record(&second).unwrap();
        assert!(reserve_end_check(&mut checks, &second_identity, now));
        enqueue_reserved(&mut checks, work(second.clone()));
        assert!(!checks.pending.contains(&second.cutex_session_id));
        assert!(!checks.last.contains_key(&second.cutex_session_id));
        assert!(reserve_end_check(&mut checks, &second_identity, now));
        drop(rx);
        enqueue_reserved(&mut checks, work(second));
        assert!(checks.sender.is_none() && checks.pending.is_empty() && checks.last.is_empty());
    }
}
