//! r271 owner admission: owner inputs held while the host authority fence is
//! busy (`owner-admission-held-v1`, desktop design 050 §5).
//!
//! An HTTP submit tries the fence once (no waiting under a session lane). If it
//! is busy and the client opted in (`allowHeld`), the input is recorded here,
//! durably and keyed by `clientUserMessageId`, and the request returns
//! `disposition: "held"` (`heldBy: "hostBusy"`). The dedicated owner admission
//! worker then waits for the fence holding no lane, tries the session lane, and
//! re-runs the ordinary route under the fence; the fence is dropped before any
//! native IO. Every later owner input of that session is held behind it (per
//! session FIFO). An input not admitted within `HELD_BUDGET` fails visibly
//! (`admission_timeout`); it is never resent in another mode.
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How long an owner input may wait for the host authority fence.
pub const HELD_BUDGET: Duration = Duration::from_secs(60);
pub const HELD_BY_HOST_BUSY: &str = "hostBusy";
pub const ADMISSION_TIMEOUT_CODE: &str = "admission_timeout";
const FILE_NAME: &str = "owner-held-inputs.json";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeldOwnerInput {
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub admission:Option<super::input_ledger_owner::SourceAdmission>,
    pub cutex_session_id: String,
    pub client_user_message_id: String,
    pub management_request_id: String,
    pub thread_id: String,
    /// The validated `cutex/userInput/submit` params (re-parsed on admission).
    pub params: Value,
    pub claude_backend: bool,
    pub held_by: String,
    pub held_since: String,
    pub deadline: String,
    /// Taken by the worker (lane + fence held). An expired claim is never
    /// timed out underneath the worker; a restart re-runs it, and the C-010
    /// delivery record of the identity decides whether it was already sent.
    #[serde(default)]
    pub claimed: bool,
    /// Who held it: `Some(true)` the owner (or root), `Some(false)` a
    /// shared-access user. Always written. A record without it (written
    /// before r289) is not proven owner: when it lands it is delivered as
    /// usual but does not lift the owner's delegation pause or stop hold.
    #[serde(default)]
    pub owner_authorized: Option<bool>,
}

impl HeldOwnerInput {
    pub fn new(
        cutex_session_id: &str,
        client_user_message_id: &str,
        management_request_id: &str,
        thread_id: &str,
        params: Value,
        claude_backend: bool,
        now: DateTime<Utc>,
    ) -> Self {
        let deadline = now + chrono::Duration::from_std(HELD_BUDGET).expect("held budget");
        Self {
            admission:super::input_ledger_owner::source_admission(cutex_session_id,client_user_message_id),
            cutex_session_id: cutex_session_id.into(),
            client_user_message_id: client_user_message_id.into(),
            management_request_id: management_request_id.into(),
            thread_id: thread_id.into(),
            params,
            claude_backend,
            held_by: HELD_BY_HOST_BUSY.into(),
            held_since: now.to_rfc3339(),
            deadline: deadline.to_rfc3339(),
            claimed: false,
            owner_authorized: Some(crate::management::users::caller_is_owner()),
        }
    }

    /// Landing this input may act as the owner's message (fail-closed for
    /// records without provenance).
    pub fn proven_owner(&self) -> bool {
        self.owner_authorized == Some(true)
    }

    fn expired(&self, now: DateTime<Utc>) -> bool {
        DateTime::parse_from_rfc3339(&self.deadline)
            .map(|at| at <= now)
            .unwrap_or(true)
    }
}

#[derive(Default, Serialize, Deserialize)]
struct HeldFile {
    #[serde(default)]
    inputs: Vec<HeldOwnerInput>,
}

struct Store {
    file: Option<HeldFile>,
}

fn store() -> &'static (Mutex<Store>, Condvar) {
    static STORE: OnceLock<(Mutex<Store>, Condvar)> = OnceLock::new();
    STORE.get_or_init(|| (Mutex::new(Store { file: None }), Condvar::new()))
}

fn path() -> anyhow::Result<std::path::PathBuf> {
    Ok(crate::config::paths::runtime_dir()?
        .join("management-v2")
        .join(FILE_NAME))
}

/// One process (Management) owns this file; the process mutex serializes it.
fn with_file<T>(write: bool, action: impl FnOnce(&mut HeldFile) -> T) -> anyhow::Result<T> {
    let (lock, wake) = store();
    let mut guard = lock
        .lock()
        .map_err(|_| anyhow::anyhow!("owner held inputs poisoned"))?;
    if guard.file.is_none() {
        let path = path()?;
        guard.file = Some(match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HeldFile::default(),
            Err(error) => return Err(error.into()),
        });
    }
    let file = guard.file.as_mut().expect("loaded");
    let result = action(file);
    if write {
        let path = path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::config::atomic::write_private_pretty_json_atomic(
            &path,
            &*file,
            "owner held inputs",
        )?;
        wake.notify_all();
    }
    Ok(result)
}

// Thread-local opt-in of the current submit (`allowHeld`): only clients that
// understand `disposition: "held"` get it; others keep `queuedReason: hostBusy`.
// Carries the session's `claude_backend` for the held record.
thread_local! {
    static ALLOW_HELD: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

pub fn with_allow_held<T>(allow: bool, claude_backend: bool, run: impl FnOnce() -> T) -> T {
    let previous = ALLOW_HELD.with(|flag| flag.replace(allow.then_some(claude_backend)));
    let result = run();
    ALLOW_HELD.with(|flag| flag.set(previous));
    result
}

pub fn allow_held() -> bool {
    ALLOW_HELD.with(|flag| flag.get().is_some())
}

pub fn held_claude_backend() -> bool {
    ALLOW_HELD.with(|flag| flag.get().unwrap_or(false))
}

/// Record `input` (idempotent by session + clientUserMessageId). Returns the
/// stored record, which is the earlier one for a repeated identity.
pub fn hold(input: HeldOwnerInput) -> anyhow::Result<HeldOwnerInput> {
    super::input_disposal::check(&input.cutex_session_id,[input.client_user_message_id.as_str()])?;
    with_file(true, |file| {
        if let Some(existing) = file.inputs.iter().find(|held| {
            held.cutex_session_id == input.cutex_session_id
                && held.client_user_message_id == input.client_user_message_id
        }) {
            return existing.clone();
        }
        file.inputs.push(input.clone());
        input
    })
}

pub fn find(session: &str, client_user_message_id: &str) -> anyhow::Result<Option<HeldOwnerInput>> {
    with_file(false, |file| {
        file.inputs
            .iter()
            .find(|held| {
                held.cutex_session_id == session
                    && held.client_user_message_id == client_user_message_id
            })
            .cloned()
    })
}

/// Whether `session` has held owner input (later owner input queues behind it).
pub fn has_session(session: &str) -> anyhow::Result<bool> {
    with_file(false, |file| {
        file.inputs
            .iter()
            .any(|held| held.cutex_session_id == session)
    })
}

pub fn held_for(session: &str) -> anyhow::Result<Vec<HeldOwnerInput>> {
    with_file(false, |file| {
        file.inputs
            .iter()
            .filter(|held| held.cutex_session_id == session)
            .cloned()
            .collect()
    })
}

/// Sessions whose head input is waiting (not claimed), oldest head first.
pub fn sessions_in_order() -> anyhow::Result<Vec<String>> {
    with_file(false, |file| {
        let mut heads: Vec<(String, String)> = Vec::new();
        for held in &file.inputs {
            if heads
                .iter()
                .any(|(session, _)| session == &held.cutex_session_id)
            {
                continue;
            }
            // The first record of a session is its head (FIFO append order).
            if !held.claimed {
                heads.push((held.cutex_session_id.clone(), held.held_since.clone()));
            } else {
                heads.push((held.cutex_session_id.clone(), String::new()));
            }
        }
        let mut waiting: Vec<_> = heads
            .into_iter()
            .filter(|(_, since)| !since.is_empty())
            .collect();
        waiting.sort_by(|a, b| a.1.cmp(&b.1));
        waiting.into_iter().map(|(session, _)| session).collect()
    })
}

/// Claim the head input of `session` (the caller holds its lane and the fence).
/// The head blocks the session: later inputs wait until it is admitted or expires.
pub fn claim_head(session: &str) -> anyhow::Result<Option<HeldOwnerInput>> {
    with_file(true, |file| {
        let head = file
            .inputs
            .iter_mut()
            .find(|held| held.cutex_session_id == session)?;
        // An expired head belongs to the expiry thread (visible timeout).
        if head.claimed || head.expired(Utc::now()) {
            return None;
        }
        head.claimed = true;
        Some(head.clone())
    })
}

pub(crate) fn remember_admission(session:&str,id:&str,facts:&super::input_ledger_owner::SourceAdmission)->anyhow::Result<()> {
    let needs=find(session,id)?.is_some_and(|held|held.admission.as_ref()!=Some(facts));
    if !needs{return Ok(());}
    with_file(true,|file|if let Some(held)=file.inputs.iter_mut().find(|held|held.cutex_session_id==session&&held.client_user_message_id==id){held.admission=Some(facts.clone());})
}

/// Replace the params of a held input (`cutex/input/interruptSend` while
/// the host fence is busy: re-run it with strategy `interrupt`). Keeps its
/// place, deadline and claim.
pub fn update_params(session: &str, client_user_message_id: &str, params: Value) -> anyhow::Result<()> {
    with_file(true, |file| {
        if let Some(held) = file.inputs.iter_mut().find(|held| {
            held.cutex_session_id == session && held.client_user_message_id == client_user_message_id
        }) {
            held.params = params;
        }
    })
}

/// Remove a finished (admitted, queued, failed or timed out) input.
pub fn finish(session: &str, client_user_message_id: &str) -> anyhow::Result<()> {
    with_file(true, |file| {
        file.inputs.retain(|held| {
            !(held.cutex_session_id == session
                && held.client_user_message_id == client_user_message_id)
        })
    })
}

/// Restart: claims of a previous process are re-run (C-010 decides delivery).
pub fn unclaim_all() -> anyhow::Result<()> {
    with_file(true, |file| {
        for held in &mut file.inputs {
            held.claimed = false;
        }
    })
}

/// A definitely-unsent group is still held after one worker pass. Preserve
/// its original FIFO position/deadline while allowing a later admission pass.
pub fn unclaim(session:&str,id:&str)->anyhow::Result<()> {
    with_file(true,|file| {
        if let Some(held)=file.inputs.iter_mut().find(|h|h.cutex_session_id==session && h.client_user_message_id==id) {
            held.claimed=false;
        }
    })
}

/// Unclaimed inputs past their own deadline, removed from the store. Later
/// inputs of the same session keep their place (the next one becomes head).
pub fn take_expired(now: DateTime<Utc>) -> anyhow::Result<Vec<HeldOwnerInput>> {
    with_file(true, |file| {
        let (expired, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut file.inputs)
            .into_iter()
            .partition(|held| !held.claimed && held.expired(now));
        file.inputs = kept;
        expired
    })
}

/// Worker: wait until held input may be waiting (or `timeout`).
pub fn wait_for_work(timeout: Duration) {
    let (lock, wake) = store();
    if let Ok(guard) = lock.lock() {
        let pending = guard
            .file
            .as_ref()
            .is_some_and(|file| file.inputs.iter().any(|held| !held.claimed));
        if !pending {
            let _ = wake.wait_timeout(guard, timeout);
        }
    }
}

pub fn any_waiting() -> anyhow::Result<bool> {
    with_file(false, |file| file.inputs.iter().any(|held| !held.claimed))
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    if let Ok(mut guard) = store().0.lock() {
        guard.file = None;
    }
}

#[cfg(test)]
mod provenance_tests {
    use super::*;
    use crate::config::instance::{self, Selection};

    /// The owner flag is always written; a record without it (r288) reads
    /// back as not proven owner after a cold load.
    #[test]
    fn held_input_provenance_is_written_and_missing_reads_as_not_owner() {
        let _home_lock = crate::config::paths::test_home_env_lock();
        let root = crate::config::paths::home_dir().unwrap().join(format!("oh-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(crate::config::paths::PRIVATE_HOME_MARKER), b"").unwrap();
        let previous = instance::selected().unwrap();
        instance::set_test_selection(Selection::Custom(root.join(".cutex")));
        reset_for_test();
        let now = Utc::now();
        crate::management::users::set_current_principal(None);
        let owner = HeldOwnerInput::new("cutex.held-prov", "owner-1", "req-1", "thread-1", serde_json::json!({}), false, now);
        crate::management::users::set_current_principal(Some(crate::management::users::Principal::User {
            id: "user-1".into(), name: "Shared".into(), grant: Default::default() }));
        let shared = HeldOwnerInput::new("cutex.held-prov", "shared-1", "req-2", "thread-1", serde_json::json!({}), false, now);
        crate::management::users::set_current_principal(None);
        assert_eq!((owner.owner_authorized, shared.owner_authorized), (Some(true), Some(false)));
        hold(owner).unwrap();
        hold(shared).unwrap();
        // Round trip: both flags are on disk, and a cold load reads them back.
        let raw: Value = serde_json::from_slice(&std::fs::read(path().unwrap()).unwrap()).unwrap();
        let flags: Vec<Value> = raw["inputs"].as_array().unwrap().iter().map(|input| input["ownerAuthorized"].clone()).collect();
        assert_eq!(flags, [Value::Bool(true), Value::Bool(false)], "{raw}");
        reset_for_test();
        let loaded = held_for("cutex.held-prov").unwrap();
        assert_eq!(loaded.iter().map(HeldOwnerInput::proven_owner).collect::<Vec<_>>(), [true, false]);
        // A legacy record (no field) cold-loads as not proven owner.
        let mut legacy = raw.clone();
        for input in legacy["inputs"].as_array_mut().unwrap() { input.as_object_mut().unwrap().remove("ownerAuthorized"); }
        std::fs::write(path().unwrap(), serde_json::to_vec(&legacy).unwrap()).unwrap();
        reset_for_test();
        let loaded = held_for("cutex.held-prov").unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.iter().all(|held| held.owner_authorized.is_none() && !held.proven_owner()));
        reset_for_test();
        instance::set_test_selection(previous);
        let _ = std::fs::remove_dir_all(&root);
    }
}
