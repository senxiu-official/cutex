//! Durable one-shot reminders. Delivery uses the existing idempotent Agent Bus.
use anyhow::{ensure, Context};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs::OpenOptions, path::Path};

pub const ALARM_SYSTEM_SENDER: &str = "Cutex Alarm";
pub const ALARM_VIEW_SCHEMA: &str = "cutex.alarm.v1";

/// The Bus can poll while the scheduler is still committing its send receipt.
#[derive(Debug)]
pub struct AlarmAwaitingQueueCommit;
impl std::fmt::Display for AlarmAwaitingQueueCommit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("durable alarm is awaiting its queued receipt commit")
    }
}
impl std::error::Error for AlarmAwaitingQueueCommit {}

#[derive(Debug)]
pub struct InvalidAlarmProvenance(pub String);
impl std::fmt::Display for InvalidAlarmProvenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.0) }
}
impl std::error::Error for InvalidAlarmProvenance {}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Alarm {
    pub alarm_id: String,
    pub owner: String,
    pub to: String,
    pub message: String,
    pub at: String,
    pub delay_seconds: Option<u64>,
    pub message_id: String,
    pub state: String,
    pub attempts: u64,
    pub next_attempt: i64,
    pub receipt: Option<Value>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_attempt: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetArgs {
    pub alarm_id: String,
    pub to: String,
    pub message: String,
    pub at: Option<String>,
    pub delay_seconds: Option<u64>,
}
fn path() -> anyhow::Result<std::path::PathBuf> {
    Ok(crate::config::paths::runtime_dir()?.join("alarms/alarms.json"))
}
/// The alarm store (`runtime/alarms/alarms.json`); Management watches it for
/// `cutex/alarms/changed` (alarms are set / sent from other processes).
pub fn store_path() -> anyhow::Result<std::path::PathBuf> {
    path()
}
/// Every alarm in the store at `path` (missing file = none), unsorted.
pub fn read_all_at(path: &Path) -> anyhow::Result<Vec<Alarm>> {
    let data: BTreeMap<String, Alarm> = match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(e) => return Err(e.into()),
    };
    Ok(data.into_values().collect())
}

/// Why the owner's cancel by message id was refused.
#[derive(Debug)]
pub enum OwnerCancelError {
    NotFound,
    /// Already handed to the Agent Bus: the message cannot be retracted.
    AlreadyQueued(Alarm),
    Store(anyhow::Error),
}

/// The owner cancels one alarm by its unique `message_id` (Management
/// `DELETE /v2/alarms/{id}`). Cancelling a cancelled alarm returns it again.
pub fn cancel_by_message_id_at(path: &Path, message_id: &str) -> Result<Alarm, OwnerCancelError> {
    let mut refusal = None;
    let result = locked(path, |data| {
        let Some(alarm) = data.values_mut().find(|a| a.message_id == message_id) else {
            refusal = Some(OwnerCancelError::NotFound);
            anyhow::bail!("alarm not found");
        };
        if alarm.state == "queued" {
            refusal = Some(OwnerCancelError::AlreadyQueued(alarm.clone()));
            anyhow::bail!("alarm already queued");
        }
        alarm.state = "cancelled".into();
        Ok(alarm.clone())
    });
    match (result, refusal) {
        (Ok(alarm), _) => Ok(alarm),
        (Err(_), Some(refusal)) => Err(refusal),
        (Err(error), None) => Err(OwnerCancelError::Store(error)),
    }
}
fn locked<T>(
    path: &Path,
    f: impl FnOnce(&mut BTreeMap<String, Alarm>) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    std::fs::create_dir_all(path.parent().context("alarm directory missing")?)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("lock"))?;
    lock.try_lock_exclusive()
        .context("Alarm store busy; retry shortly")?;
    let mut data = match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(e) => return Err(e.into()),
    };
    let before = serde_json::to_vec(&data)?;
    let result = f(&mut data)?;
    if serde_json::to_vec(&data)? != before {
        crate::config::atomic::write_private_pretty_json_atomic(path, &data, "scheduled messages")?;
    }
    Ok(result)
}

fn authenticate_queued_message_at(
    path: &Path,
    message_id: &str,
    target: &str,
    message: &str,
    legacy_agent_envelope: bool,
) -> anyhow::Result<Alarm> {
    let data: BTreeMap<String, Alarm> = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(error) => return Err(error.into()),
    };
    // I/O and decode errors stay retryable; only validated provenance conflicts
    // are terminal. In particular, an unavailable store must not lose an alarm.
    (|| -> anyhow::Result<Alarm> {
    ensure!(!message_id.trim().is_empty(), "alarm message identity absent");
    let mut matches = data
        .into_values()
        .filter(|alarm| alarm.message_id == message_id);
    let alarm = matches.next().context("durable alarm absent")?;
    ensure!(matches.next().is_none(), "durable alarm identity ambiguous");
    let role_target=if alarm.to==DIRECTOR_ALARM_PLACEHOLDER {
        crate::director_address::Store::open_default().map_err(|e|anyhow::anyhow!(e.detail))?.snapshot().map_err(|e|anyhow::anyhow!(e.detail))?
            .alarm_roles.get(message_id).filter(|intent|intent.owner==alarm.owner).and_then(|intent|intent.target.clone())
    }else{None};
    ensure!(alarm.to == target || role_target.as_deref()==Some(target), "durable alarm target conflict");
    let expected_message = if legacy_agent_envelope {
        format!("[message from {ALARM_SYSTEM_SENDER}] {}", alarm.message)
    } else {
        alarm.message.clone()
    };
    ensure!(expected_message == message, "durable alarm content conflict");
    ensure!(!alarm.owner.trim().is_empty(), "durable alarm owner absent");
    ensure!(
        !message_id.starts_with(APPROVAL_NOTICE_MESSAGE_PREFIX) || alarm.owner == APPROVAL_NOTICE_OWNER,
        "approval notice id on a non-approval alarm"
    );
    if alarm.state == "scheduled" {
        return Err(AlarmAwaitingQueueCommit.into());
    }
    ensure!(alarm.state == "queued", "durable alarm is not queued");
    Ok(alarm)
    })().map_err(|error| {
        if error.is::<AlarmAwaitingQueueCommit>() { error }
        else { InvalidAlarmProvenance(error.to_string()).into() }
    })
}

pub fn authenticate_queued_message(
    message_id: &str,
    target: &str,
    message: &str,
) -> anyhow::Result<Alarm> {
    authenticate_queued_message_at(&path()?, message_id, target, message, false)
}

pub(crate) fn authenticate_queued_legacy_message(
    message_id: &str,
    target: &str,
    message: &str,
) -> anyhow::Result<Alarm> {
    authenticate_queued_message_at(&path()?, message_id, target, message, true)
}

fn timestamp(at: &str) -> anyhow::Result<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(at) {
        return Ok(dt.with_timezone(&Utc));
    }
    let dt = NaiveDateTime::parse_from_str(at, "%Y-%m-%d %H:%M")
        .context("at must be RFC3339 with timezone, or local YYYY-MM-DD HH:MM")?;
    Ok(Local
        .from_local_datetime(&dt)
        .single()
        .context("ambiguous/nonexistent local time; specify an RFC3339 timezone")?
        .with_timezone(&Utc))
}
fn create(
    data: &mut BTreeMap<String, Alarm>,
    owner: &str,
    a: SetArgs,
    now: DateTime<Utc>,
) -> anyhow::Result<Alarm> {
    ensure!(
        !a.alarm_id.trim().is_empty() && a.alarm_id.len() <= 200,
        "alarm_id must be 1..200 characters"
    );
    ensure!(
        !a.message.trim().is_empty() && a.message.len() <= 65536,
        "message must be 1..65536 bytes"
    );
    ensure!(
        a.at.is_some() != a.delay_seconds.is_some(),
        "provide exactly one of at or delay_seconds"
    );
    ensure!(
        crate::agent_bus::routing::is_full_durable_cutex_session_id(&a.to),
        "target must resolve to a full Cutex session ID"
    );
    let key = serde_json::to_string(&(owner, &a.alarm_id))?;
    if let Some(old) = data.get(&key) {
        let same_time = if let Some(at) = a.at.as_deref() {
            timestamp(at)?.to_rfc3339() == old.at
        } else {
            a.delay_seconds == old.delay_seconds
        };
        ensure!(
            old.to == a.to && old.message == a.message && same_time,
            "alarm_id already exists with different contents; use another ID"
        );
        return Ok(old.clone());
    }
    let due = if let Some(at) = a.at.as_deref() {
        timestamp(at)?
    } else {
        let seconds = i64::try_from(a.delay_seconds.unwrap())?;
        ensure!(seconds > 0, "delay_seconds must be positive");
        now.checked_add_signed(chrono::Duration::try_seconds(seconds).context("delay too large")?)
            .context("time overflow")?
    };
    ensure!(due > now, "alarm time must be in the future");
    let alarm = Alarm {
        alarm_id: a.alarm_id,
        owner: owner.into(),
        to: a.to,
        message: a.message,
        at: due.to_rfc3339(),
        delay_seconds: a.delay_seconds,
        message_id: if owner == APPROVAL_NOTICE_OWNER {
            format!("{APPROVAL_NOTICE_MESSAGE_PREFIX}{}", uuid::Uuid::new_v4())
        } else {
            format!("alarm-{}", uuid::Uuid::new_v4())
        },
        state: "scheduled".into(),
        attempts: 0,
        next_attempt: due.timestamp(),
        receipt: None,
        last_error: None,
        last_attempt: None,
    };
    data.insert(key, alarm.clone());
    Ok(alarm)
}
pub fn set(owner: &str, args: SetArgs) -> anyhow::Result<Value> {
    locked(&path()?, |d| Ok(json!(create(d, owner, args, Utc::now())?)))
}
// A r253 daemon cannot resolve this reserved non-existent recipient. Keeping
// intent in the extension store makes rollback fail closed until queued.
const DIRECTOR_ALARM_PLACEHOLDER:&str="cutex.00000000-0000-0000-0000-000000000000";
pub fn is_director_alarm(alarm:&Alarm)->bool{alarm.to==DIRECTOR_ALARM_PLACEHOLDER}
pub fn director_alarm_route(alarm:&Alarm)->Option<crate::director_address::AlarmRole>{
    if !is_director_alarm(alarm){return None}
    crate::director_address::Store::open_default().and_then(|s|s.snapshot()).ok()?.alarm_roles.get(&alarm.message_id).cloned()
}
pub fn set_director(owner:&str,args:SetArgs)->anyhow::Result<Value>{
    let raw_digest=crate::asks::sha256_hex(&serde_json::to_vec(&json!({"id":args.alarm_id,"message":args.message,"at":args.at,"delay":args.delay_seconds}))?);
    let store=crate::director_address::Store::open_default().map_err(|e|anyhow::anyhow!(e.detail))?;
    let retry=locked(&path()?,|data|{
        let key=serde_json::to_string(&(owner,&args.alarm_id))?;
        let Some(existing)=data.get(&key) else{return Ok(None)};
        let ext=store.snapshot().map_err(|e|anyhow::anyhow!(e.detail))?;
        let intent=ext.alarm_roles.get(&existing.message_id).context("alarm_id already exists without a Director route")?;
        ensure!(intent.owner==owner && intent.raw_digest==raw_digest,"alarm_id already exists with different contents");
        Ok(Some(role_alarm_view(existing,Some(intent))))
    })?;
    if let Some(retry)=retry{return Ok(retry)}
    let directory=crate::asks::directory::Directory::load().map_err(|e|anyhow::anyhow!(e.detail))?;
    let resolution=directory.resolve_director(owner,None,true).map_err(|e|anyhow::anyhow!("{}: {}",e.code,e.detail))?;
    set_director_at(&path()?,&store,owner,args,resolution.project_id,raw_digest)
}
fn set_director_at(path:&Path,store:&crate::director_address::Store,owner:&str,mut args:SetArgs,project:String,raw_digest:String)->anyhow::Result<Value>{
    args.to=DIRECTOR_ALARM_PLACEHOLDER.into();
    locked(path,|data|{
        let key=serde_json::to_string(&(owner,&args.alarm_id))?;
        if let Some(existing)=data.get(&key){
            let ext=store.snapshot().map_err(|e|anyhow::anyhow!(e.detail))?;
            let intent=ext.alarm_roles.get(&existing.message_id).context("alarm_id already exists without a Director route")?;
            ensure!(intent.owner==owner && intent.raw_digest==raw_digest,"alarm_id already exists with different contents");
            return Ok(role_alarm_view(existing,Some(intent)))
        }
        let alarm=create(data,owner,args,Utc::now())?;
        store.update(|ext|{ext.alarm_roles.insert(alarm.message_id.clone(),crate::director_address::AlarmRole{owner:owner.into(),project_id:project.clone(),raw_digest:raw_digest.clone(),target:None});Ok(())}).map_err(|e|anyhow::anyhow!(e.detail))?;
        let ext=store.snapshot().map_err(|e|anyhow::anyhow!(e.detail))?;
        Ok(role_alarm_view(&alarm,ext.alarm_roles.get(&alarm.message_id)))
    })
}
fn role_alarm_view(alarm:&Alarm,intent:Option<&crate::director_address::AlarmRole>)->Value{
    let mut value=json!(alarm);
    if let Some(intent)=intent{value["to"]=json!("director");value["targetRef"]=json!({"kind":"projectDirector","projectId":intent.project_id});value["resolvedTarget"]=json!(intent.target);}
    value
}
fn resolve_director_alarm(alarm:&Alarm)->anyhow::Result<Option<String>>{
    if alarm.to!=DIRECTOR_ALARM_PLACEHOLDER{return Ok(None)}
    let store=crate::director_address::Store::open_default().map_err(|e|anyhow::anyhow!(e.detail))?;
    let sessions=crate::session::store::load_cutex_session_store_shared()?;
    let directory=crate::asks::directory::Directory::load().map_err(|e|anyhow::anyhow!(e.detail))?;
    let roster=crate::asks::directory::Directory::director_snapshot().map_err(|e|anyhow::anyhow!(e.detail))?;
    resolve_director_alarm_in(alarm,&store,&sessions,&roster,&directory)
}
fn resolve_director_alarm_in(alarm:&Alarm,store:&crate::director_address::Store,sessions:&crate::session::model::CutexSessionStore,roster:&crate::agent_management::AgentManagementSnapshot,directory:&crate::asks::directory::Directory)->anyhow::Result<Option<String>>{
    let ext=store.snapshot().map_err(|e|anyhow::anyhow!(e.detail))?;
    let Some(intent)=ext.alarm_roles.get(&alarm.message_id) else {
        ensure!(alarm.to!=DIRECTOR_ALARM_PLACEHOLDER,"Director alarm intent is unavailable; review before sending");return Ok(None)
    };
    ensure!(intent.owner==alarm.owner,"Director alarm creator changed");
    validate_director_alarm_creator(&alarm.owner,&intent.project_id,sessions,roster)?;
    let current=directory.resolve_director_in(roster,&alarm.owner,Some(&intent.project_id),false).map_err(|e|anyhow::anyhow!(e.detail))?;
    let target=intent.target.as_deref().unwrap_or(&current.target);
    directory.resolve_tell(&alarm.owner,target).map_err(|e|anyhow::anyhow!(e.detail))?;
    let target=target.to_owned();
    let pinned=store.update(|ext|{let saved=ext.alarm_roles.get_mut(&alarm.message_id).ok_or_else(||crate::asks::store::AskError::new("director_state_unavailable","Alarm intent disappeared"))?;if saved.target.is_none(){saved.target=Some(target.clone())}Ok(saved.target.clone())}).map_err(|e|anyhow::anyhow!(e.detail))?;
    if let Some(target)=&pinned{directory.resolve_tell(&alarm.owner,target).map_err(|e|anyhow::anyhow!(e.detail))?;}
    Ok(pinned)
}
fn validate_director_alarm_creator(owner:&str,project:&str,sessions:&crate::session::model::CutexSessionStore,roster:&crate::agent_management::AgentManagementSnapshot)->anyhow::Result<()>{
    ensure!(sessions.sessions.values().any(|r|r.cutex_session_id==owner && !r.is_retired() && r.retired_at.is_none()),"Director alarm creator is retired or missing");
    ensure!(!roster.agents.values().any(|a|a.cutex_session_id.as_str()==owner && a.retired_at.is_some()),"Director alarm creator was retired by management");
    ensure!(crate::asks::directory::Directory::director_projects(roster,owner)==vec![project.to_owned()],"Director alarm creator no longer belongs to its original project");Ok(())
}
/// A cutex system notice to one agent through the alarm scheduler (durable,
/// a few seconds from now; after-turn like any alarm, except approval
/// notices, which a busy agent reads at its next step: 023). `owner` is a reserved non-session owner (e.g.
/// [`APPROVAL_NOTICE_OWNER`]); `notice_id` makes a repeat idempotent. The
/// store lock is contended briefly by the scheduler, so this retries a few
/// times.
pub fn set_system_notice(owner: &str, notice_id: &str, to: &str, message: &str) -> anyhow::Result<Alarm> {
    set_system_notice_at(&path()?, owner, notice_id, to, message, Utc::now())
}

/// Owner of the owner-approval decision notices (cutex `approval-decision-notice`).
pub const APPROVAL_NOTICE_OWNER: &str = "cutex.approvals";

/// Bus message id prefix of an approval decision notice (023): the Agent Bus
/// reads it at a busy recipient's next step with no wait, while every other
/// alarm stays after-turn. Still an `alarm-` id, so a release before 023
/// delivers it as an ordinary after-turn alarm. Only notices of
/// [`APPROVAL_NOTICE_OWNER`] get it (checked again at authentication).
pub const APPROVAL_NOTICE_MESSAGE_PREFIX: &str = "alarm-approval-";

fn set_system_notice_at(
    path: &Path,
    owner: &str,
    notice_id: &str,
    to: &str,
    message: &str,
    now: DateTime<Utc>,
) -> anyhow::Result<Alarm> {
    ensure!(!owner.trim().is_empty() && !crate::agent_bus::routing::is_full_durable_cutex_session_id(owner), "system notice owner must be a reserved name");
    let at = (now + chrono::Duration::seconds(1)).to_rfc3339();
    let mut last = None;
    for attempt in 0..20 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let args = SetArgs { alarm_id: notice_id.into(), to: to.into(), message: message.into(), at: Some(at.clone()), delay_seconds: None };
        match locked(path, |data| create(data, owner, args, now)) {
            Ok(alarm) => return Ok(alarm),
            Err(error) if format!("{error:#}").contains("Alarm store busy") => last = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("alarm store busy")))
}

pub fn list(owner: Option<&str>) -> anyhow::Result<Value> {
    let mut rows: Vec<_> = read_all_at(&path()?)?
        .into_iter()
        .filter(|a| owner.is_none_or(|o| o == a.owner))
        .collect();
    rows.sort_by(|a, b| a.at.cmp(&b.at));
    let ext=crate::director_address::Store::open_default().and_then(|s|s.snapshot()).ok();
    Ok(json!(rows.iter().map(|alarm|role_alarm_view(alarm,ext.as_ref().and_then(|s|s.alarm_roles.get(&alarm.message_id)))).collect::<Vec<_>>()))
}
pub fn cancel(owner: Option<&str>, id: &str) -> anyhow::Result<Value> {
    locked(&path()?, |d| {
        let keys: Vec<_> = d
            .iter()
            .filter(|(_, a)| a.alarm_id == id && owner.is_none_or(|o| o == a.owner))
            .map(|(k, _)| k.clone())
            .collect();
        ensure!(
            keys.len() == 1,
            "alarm missing or ambiguous; use its creator to disambiguate"
        );
        let a = d.get_mut(&keys[0]).unwrap();
        ensure!(
            a.state != "queued",
            "alarm already queued; cannot retract the message"
        );
        a.state = "cancelled".into();
        Ok(json!(a))
    })
}
pub fn resolve_local(target: &str) -> anyhow::Result<String> {
    ensure!(!target.trim().eq_ignore_ascii_case("director"),"director_alias_unsupported: Director alarms require an authenticated agent project; use alarm_set");
    let store = crate::session::store::load_cutex_session_store_shared()?;
    let host = crate::platform::host::current_host_name();
    let rows: Vec<_> = store
        .sessions
        .values()
        .filter(|r| {
            !r.is_retired()
                && crate::runtime::lifecycle::cutex_session_host_is_local(&r.host_id, &host)
        })
        .collect();
    let index = crate::short_ids::Index::new(rows.iter().map(|r| crate::short_ids::Entry {id:r.cutex_session_id.clone(), name:r.formal_agent_name.clone()}));
    index.resolve(target, &index, crate::short_ids::settings().minimum())
        .map_err(|error| anyhow::anyhow!("{}: {}", error.code, error.to_json()))
}
pub fn caller_owner(runtime: &str) -> anyhow::Result<String> {
    let store = crate::session::store::load_cutex_session_store_shared()?;
    caller_owner_in(&store, runtime, crate::platform::process::process_is_running)
}

fn caller_owner_in(
    store: &crate::session::model::CutexSessionStore,
    runtime: &str,
    alive: impl Fn(u32) -> bool,
) -> anyhow::Result<String> {
    let rows: Vec<_> = store
        .sessions
        .values()
        .filter(|r| {
            r.current_runtime_agent_id.as_deref() == Some(runtime)
                && !r.is_retired()
                && r.retired_at.is_none()
                && r.runtime_pid.is_none_or(&alive)
        })
        .collect();
    let mut candidates = rows.iter().map(|r| r.cutex_session_id.as_str()).collect::<Vec<_>>();
    candidates.sort_unstable();
    ensure!(
        rows.len() == 1,
        "alarm creator has no unique durable mapping; candidates: [{}]; run `cutex human doctor --check`",
        candidates.join(", ")
    );
    Ok(rows[0].cutex_session_id.clone())
}
fn tick(
    path: &Path,
    now: i64,
    mut send: impl FnMut(&Alarm) -> anyhow::Result<Value>,
) -> anyhow::Result<Vec<Alarm>> {
    locked(path, |data| {
        // Due alarms in the order they fell due (`at`; for approval notices
        // the decision time), not in store-key order (023).
        let mut due: Vec<&mut Alarm> = data
            .values_mut()
            .filter(|a| a.state == "scheduled" && a.next_attempt <= now)
            .collect();
        due.sort_by_cached_key(|a| (timestamp(&a.at).map(|t| t.timestamp_nanos_opt().unwrap_or(i64::MAX)).unwrap_or(i64::MAX), a.message_id.clone()));
        for a in due {
            a.attempts += 1;
            a.last_attempt=DateTime::from_timestamp(now,0).map(|t|t.to_rfc3339());
            match send(a) {
                Ok(receipt) => {
                    a.state = "queued".into();
                    a.receipt = Some(receipt);
                    a.last_error = None;
                }
                Err(error) => {
                    a.next_attempt = now.saturating_add(30);
                    a.last_error = Some(format!("{error:#}").chars().take(2048).collect());
                },
            }
        }
        Ok(prune_cancelled(data, now))
    })
}
/// Cancelled alarms are dropped this long after their time (bookkeeping;
/// since r252). Queued alarms stay: the Agent Bus authenticates a queued
/// alarm message against this store until it is delivered.
pub const CANCELLED_RETENTION_DAYS: i64 = 7;
fn prune_cancelled(data: &mut BTreeMap<String, Alarm>, now: i64) -> Vec<Alarm> {
    let cutoff = now.saturating_sub(CANCELLED_RETENTION_DAYS * 86_400);
    let mut removed = Vec::new();
    data.retain(|_, alarm| {
        let expired = alarm.state == "cancelled" && timestamp(&alarm.at).is_ok_and(|at| at.timestamp() < cutoff);
        if expired { removed.push(alarm.clone()); }
        !expired
    });
    removed
}

/// Only a successful business-store commit authorizes removal of its redundant
/// Director route. Cancelled rows that still exist retain retry/list metadata;
/// queued routes remain the Bus's provenance and must NEVER be removed here.
/// Lock order matches set_director_at: alarm -> Director; never the inverse.
fn prune_removed_role_routes_at(path: &Path, store: &crate::director_address::Store, removed: &[Alarm]) -> anyhow::Result<usize> {
    if removed.is_empty() { return Ok(0) }
    locked(path, |data| {
        let absent: Vec<_> = removed.iter().filter(|alarm|
            alarm.state == "cancelled" && alarm.to == DIRECTOR_ALARM_PLACEHOLDER
            && !alarm.message_id.is_empty()
            && !data.values().any(|current| current.message_id == alarm.message_id)
        ).collect();
        if absent.is_empty() { return Ok(0) }
        store.update(|state| {
            let mut count = 0;
            for alarm in absent {
                if state.alarm_roles.get(&alarm.message_id).is_some_and(|role| role.owner == alarm.owner) {
                    state.alarm_roles.remove(&alarm.message_id);
                    count += 1;
                }
            }
            Ok(count)
        }).map_err(|e| anyhow::anyhow!(e.detail))
    })
}
pub fn spawn() -> anyhow::Result<()> {
    std::thread::Builder::new().name("cutex-alarms".into()).spawn(||loop {
        let result=(|| -> anyhow::Result<()> {
            let config=crate::config::store::load_codez_config_checked()?;
            let alarm_path = path()?;
            let removed = tick(&alarm_path,Utc::now().timestamp(),|a| {
                let mut outgoing=a.clone();
                if let Some(target)=resolve_director_alarm(a)?{outgoing.to=target;}
                let response=crate::agent_bus::client::send_local_alarm(&config,&outgoing.to,&a.message,&a.message_id)?;
                ensure!(response.queued,"alarm delivery not acknowledged");
                Ok(json!({"message_id":response.id,"queued":response.queued,"delivery_state":response.delivery_state}))
            })?;
            if !removed.is_empty() {
                // Failure/crash here retains redundant rows. Missing rows alone
                // are never proof: do not guess that legacy orphans were closed.
                let cleanup = crate::director_address::Store::open_default()
                    .map_err(|e| anyhow::anyhow!(e.detail))
                    .and_then(|store| prune_removed_role_routes_at(&alarm_path, &store, &removed));
                if cleanup.is_err() { eprintln!("Director alarm bookkeeping cleanup deferred; routes retained"); }
            }
            Ok(())
        })();
        let delay=if let Err(e)=result { eprintln!("Cutex alarm scheduler: {e}"); 30 } else {2};
        std::thread::sleep(std::time::Duration::from_secs(delay));
    })?;
    Ok(())
}
pub fn tools() -> Vec<Value> {
    vec![
        json!({"name":"alarm_set","description":"Schedule a one-shot reminder message to yourself or another agent you may reach. The Cutex service delivers it after_turn at the requested time, including after service restarts.","inputSchema":{"type":"object","properties":{"alarm_id":{"type":"string","description":"Stable ID; reuse for an unchanged retry."},"to":{"type":"string","description":"ID, exact agent name, or director. A director reminder captures your project and resolves its current Director when due; retired creators cannot send. IDs accept a unique prefix or suffix of at least 6 hex characters."},"message":{"type":"string"},"at":{"type":"string","description":"RFC3339 with timezone, or local YYYY-MM-DD HH:MM. Use at OR delay_seconds."},"delay_seconds":{"type":"integer","minimum":1}},"required":["alarm_id","to","message"],"additionalProperties":false}}),
        json!({"name":"alarm_list","description":"List alarms created by this Agent, including delivery state.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}),
        json!({"name":"alarm_cancel","description":"Cancel a pending alarm created by this Agent.","inputSchema":{"type":"object","properties":{"alarm_id":{"type":"string"}},"required":["alarm_id"],"additionalProperties":false}}),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn director_alarm_captures_project_without_pinning_the_director_until_due(){
        let root=std::env::temp_dir().join(format!("director-alarm-{}",uuid::Uuid::new_v4()));
        let store=crate::director_address::Store::at(root.join("extension"));let path=root.join("alarms.json");
        let first=set_director_at(&path,&store,"creator",args(),"original".into(),"digest".into()).unwrap();
        assert_eq!(first["to"],"director");assert_eq!(first["targetRef"]["projectId"],"original");assert!(first["resolvedTarget"].is_null());
        let id=first["message_id"].as_str().unwrap();
        assert_eq!(store.snapshot().unwrap().alarm_roles[id].target,None);
        // An unchanged retry after a membership change preserves the captured project.
        let retry=set_director_at(&path,&store,"creator",args(),"later".into(),"digest".into()).unwrap();
        assert_eq!(retry,first);
        assert!(set_director_at(&path,&store,"creator",args(),"original".into(),"changed payload".into()).is_err());
        let rollback=read_all_at(&path).unwrap();assert_eq!(rollback[0].to,DIRECTOR_ALARM_PLACEHOLDER);
        assert_ne!(rollback[0].to,"director");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn director_alarm_creator_must_still_be_active_in_the_original_project(){
        use crate::{agent_management::AgentManagementSnapshot,session::model::{CutexSessionRecord,CutexSessionStore,CutexSessionArchiveState}};
        let owner="cutex.00000000-0000-0000-0000-000000000001";
        let mut sessions=CutexSessionStore::default();let mut roster=AgentManagementSnapshot::empty();
        sessions.sessions.insert(owner.into(),CutexSessionRecord::new(owner.into(),None,"local".into(),".".into(),None).unwrap());
        let authority=serde_json::from_value(json!({"project_id":"original","authorized_director_session":owner,"authority_epoch":1,"updated_at":"2026-10-24T00:00:00Z"})).unwrap();
        roster.projects.insert(crate::agent_management::ProjectId::new("original").unwrap(),authority);
        assert!(validate_director_alarm_creator(owner,"original",&sessions,&roster).is_ok());
        assert!(validate_director_alarm_creator(owner,"another",&sessions,&roster).is_err());
        sessions.sessions.get_mut(owner).unwrap().archive_state=CutexSessionArchiveState::Retired;
        assert!(validate_director_alarm_creator(owner,"original",&sessions,&roster).is_err());
        sessions.sessions.clear();assert!(validate_director_alarm_creator(owner,"original",&sessions,&roster).is_err());
    }
    #[test]
    fn director_alarm_resolves_when_due_then_keeps_the_first_send_target_on_retry(){
        use crate::{agent_management::{AgentManagementSnapshot,ProjectId},session::model::{CutexSessionRecord,CutexSessionStore}};
        let root=std::env::temp_dir().join(format!("director-alarm-fire-{}",uuid::Uuid::new_v4()));
        let store=crate::director_address::Store::at(root.join("extension"));let path=root.join("alarms.json");
        let owner="cutex.00000000-0000-0000-0000-000000000001";
        let old="cutex.00000000-0000-0000-0000-000000000002";
        let new="cutex.00000000-0000-0000-0000-000000000003";
        set_director_at(&path,&store,owner,args(),"alpha".into(),"digest".into()).unwrap();
        let alarm=read_all_at(&path).unwrap().remove(0);
        let mut sessions=CutexSessionStore::default();sessions.sessions.insert(owner.into(),CutexSessionRecord::new(owner.into(),None,"local".into(),".".into(),None).unwrap());
        let mut roster=AgentManagementSnapshot::empty();
        let mut directory=crate::asks::directory::tests::directory(&[(owner,"member",Some("alpha")),(old,"old",Some("alpha")),(new,"new",Some("alpha"))]);
        roster.current_project_memberships.insert(crate::role_revision::CutexSessionId::new(owner).unwrap(),serde_json::from_value(json!({"cutex_session_id":owner,"project_id":"alpha","revision":1,"updated_at":"2026-10-24T00:00:00Z"})).unwrap());
        // Created before this assignment, but first fired after it: choose new.
        roster.projects.insert(ProjectId::new("alpha").unwrap(),serde_json::from_value(json!({"project_id":"alpha","authorized_director_session":new,"authority_epoch":2,"updated_at":"2026-10-24T00:00:00Z"})).unwrap());
        assert_eq!(resolve_director_alarm_in(&alarm,&store,&sessions,&roster,&directory).unwrap().as_deref(),Some(new));
        roster.projects.get_mut(&ProjectId::new("alpha").unwrap()).unwrap().authorized_director_session=crate::role_revision::CutexSessionId::new(old).unwrap();
        assert_eq!(resolve_director_alarm_in(&alarm,&store,&sessions,&roster,&directory).unwrap().as_deref(),Some(new),"uncertain retry preserves its first message recipient");
        directory.agents.get_mut(new).unwrap().archived=true;
        assert!(resolve_director_alarm_in(&alarm,&store,&sessions,&roster,&directory).is_err(),"a now-unreachable pinned target is not silently forwarded");
        let empty=crate::director_address::Store::at(root.join("empty"));
        assert!(resolve_director_alarm_in(&alarm,&empty,&sessions,&roster,&directory).is_err(),"missing extension intent never falls back to a guessed target");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn caller_owner_excludes_dead_and_retired_duplicates_and_reports_ambiguity() {
        use crate::session::model::{CutexSessionRecord, CutexSessionStore, CutexSessionArchiveState};
        let mut store = CutexSessionStore::default();
        for (n, pid, retired) in [(1, 11, false), (2, 22, false), (3, 11, true)] {
            let id = format!("cutex.00000000-0000-0000-0000-{n:012}");
            let mut row = CutexSessionRecord::new(id.clone(), None, "local".into(), ".".into(), None).unwrap();
            row.current_runtime_agent_id = Some("runtime".into());
            row.runtime_pid = Some(pid);
            if retired { row.archive_state = CutexSessionArchiveState::Retired; }
            store.sessions.insert(id, row);
        }
        assert_eq!(caller_owner_in(&store, "runtime", |pid| pid == 11).unwrap(), "cutex.00000000-0000-0000-0000-000000000001");
        let error = caller_owner_in(&store, "runtime", |_| true).unwrap_err().to_string();
        assert!(error.contains("000000000001") && error.contains("000000000002"));
        assert!(!error.contains("000000000003"));
        assert!(error.contains("cutex human doctor --check"));
        assert!(caller_owner_in(&store, "runtime", |_| false).is_err());
    }
    fn args() -> SetArgs {
        SetArgs {
            alarm_id: "remind".into(),
            to: "cutex.00000000-0000-0000-0000-000000000001".into(),
            message: "continue".into(),
            at: None,
            delay_seconds: Some(60),
        }
    }
    #[test]
    fn system_notice_is_scheduled_once_for_a_reserved_owner() {
        let dir = std::env::temp_dir().join(format!("alarm-notice-{}", uuid::Uuid::new_v4()));
        let path = dir.join("alarms.json");
        let now = Utc::now();
        let to = "cutex.00000000-0000-0000-0000-000000000001";
        let first = set_system_notice_at(&path, APPROVAL_NOTICE_OWNER, "apr-1:approved", to, "retry now", now).unwrap();
        assert_eq!((first.owner.as_str(), first.state.as_str()), (APPROVAL_NOTICE_OWNER, "scheduled"));
        assert!(first.next_attempt <= now.timestamp() + 1);
        let again = set_system_notice_at(&path, APPROVAL_NOTICE_OWNER, "apr-1:approved", to, "retry now", now).unwrap();
        assert_eq!(again.message_id, first.message_id, "idempotent");
        assert_eq!(read_all_at(&path).unwrap().len(), 1);
        assert!(set_system_notice_at(&path, to, "x", to, "m", now).is_err(), "a session id is not a system owner");
        std::fs::remove_dir_all(dir).ok();
    }

    /// 023: approval notices get the `alarm-approval-` id (next step on the
    /// Bus); every other alarm keeps `alarm-`; the prefix is only valid on an
    /// approval owner; one notice per approval and outcome, sent in decision order.
    #[test]
    fn approval_notices_are_marked_authenticated_and_sent_in_decision_order() {
        let dir = std::env::temp_dir().join(format!("alarm-approval-{}", uuid::Uuid::new_v4()));
        let path = dir.join("alarms.json");
        let now = Utc::now();
        let to = "cutex.00000000-0000-0000-0000-000000000001";
        // Decided later but with a store key that sorts first ("apr-a" < "apr-z").
        let later = set_system_notice_at(&path, APPROVAL_NOTICE_OWNER, "apr-a:denied", to, "do not retry", now + chrono::Duration::milliseconds(5)).unwrap();
        let first = set_system_notice_at(&path, APPROVAL_NOTICE_OWNER, "apr-z:approved", to, "retry now", now).unwrap();
        let revoked = set_system_notice_at(&path, APPROVAL_NOTICE_OWNER, "apr-z:denied", to, "revoked", now + chrono::Duration::milliseconds(9)).unwrap();
        for notice in [&later, &first, &revoked] {
            assert!(notice.message_id.starts_with(APPROVAL_NOTICE_MESSAGE_PREFIX), "{}", notice.message_id);
        }
        assert_ne!(first.message_id, revoked.message_id, "approved, then revoked: two notices");
        let reminder = locked(&path, |data| create(data, "cutex.owner", args(), now)).unwrap();
        assert!(reminder.message_id.starts_with("alarm-") && !reminder.message_id.starts_with(APPROVAL_NOTICE_MESSAGE_PREFIX));
        let mut sent = Vec::new();
        tick(&path, now.timestamp() + 5, |a| { sent.push(a.alarm_id.clone()); Ok(json!({"id": a.message_id})) }).unwrap();
        assert_eq!(sent, ["apr-z:approved", "apr-a:denied", "apr-z:denied"], "decision order, not key order");
        // Repeat of a queued notice is a no-op: nothing is sent twice.
        set_system_notice_at(&path, APPROVAL_NOTICE_OWNER, "apr-z:approved", to, "retry now", now).unwrap();
        tick(&path, now.timestamp() + 30, |a| panic!("sent twice: {}", a.alarm_id)).unwrap();
        // Queued notices authenticate; the prefix on another owner's alarm does not.
        assert_eq!(authenticate_queued_message_at(&path, &first.message_id, to, "retry now", false).unwrap().owner, APPROVAL_NOTICE_OWNER);
        locked(&path, |data| {
            let r = data.values_mut().find(|a| a.owner == "cutex.owner").unwrap();
            r.message_id = format!("{APPROVAL_NOTICE_MESSAGE_PREFIX}forged");
            r.state = "queued".into();
            Ok(())
        }).unwrap();
        let forged = format!("{APPROVAL_NOTICE_MESSAGE_PREFIX}forged");
        assert!(authenticate_queued_message_at(&path, &forged, &reminder.to, &reminder.message, false)
            .unwrap_err().is::<InvalidAlarmProvenance>());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn queued_alarm_authentication_is_exact_and_fail_closed() {
        let dir = std::env::temp_dir().join(format!("alarm-auth-{}", uuid::Uuid::new_v4()));
        let path = dir.join("alarms.json");
        let now = Utc::now();
        let alarm = locked(&path, |data| create(data, "cutex.owner", args(), now)).unwrap();
        assert!(authenticate_queued_message_at(&path, &alarm.message_id, &alarm.to, &alarm.message, false)
            .unwrap_err().is::<AlarmAwaitingQueueCommit>());
        assert!(!authenticate_queued_message_at(&path, &alarm.message_id, &alarm.to, "forged", false)
            .unwrap_err().is::<AlarmAwaitingQueueCommit>());
        locked(&path, |data| {
            data.values_mut().next().unwrap().state = "queued".into();
            Ok(())
        })
        .unwrap();
        let authenticated = authenticate_queued_message_at(
            &path,
            &alarm.message_id,
            &alarm.to,
            &alarm.message,
            false,
        )
        .unwrap();
        assert_eq!(authenticated.owner, "cutex.owner");
        assert!(authenticate_queued_message_at(
            &path,
            &alarm.message_id,
            "cutex.00000000-0000-0000-0000-000000000002",
            &alarm.message,
            false,
        )
        .is_err());
        assert!(authenticate_queued_message_at(
            &path,
            &alarm.message_id,
            &alarm.to,
            "forged",
            false,
        )
        .is_err());
        assert!(authenticate_queued_message_at(
            &path,
            "alarm-missing",
            &alarm.to,
            &alarm.message,
            false,
        )
        .is_err());
        let legacy = format!("[message from {ALARM_SYSTEM_SENDER}] {}", alarm.message);
        assert!(authenticate_queued_message_at(
            &path,
            &alarm.message_id,
            &alarm.to,
            &legacy,
            true,
        )
        .is_ok());
        assert!(authenticate_queued_message_at(
            &path,
            &alarm.message_id,
            &alarm.to,
            &legacy,
            false,
        )
        .is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn scheduler_sends_local_http_without_runtime_sender_and_persists_receipt() {
        use crate::http::server::{read_simple_http_request,write_json_response};
        let listener=std::net::TcpListener::bind(("127.0.0.1",0)).unwrap();
        let config=crate::profiles::model::CodezConfig{agent_bus_port:Some(listener.local_addr().unwrap().port()),..Default::default()};
        let server=std::thread::spawn(move||{
            let (mut stream,_)=listener.accept().unwrap();
            let req=read_simple_http_request(&mut stream).unwrap();
            let body:Value=serde_json::from_slice(&req.body).unwrap();
            assert!(body.get("all_hosts").is_none());assert!(body["from_agent_id"].is_null());
            assert_eq!(body["from"],"Cutex Alarm");assert_eq!(body["delivery_mode"],"after_turn");
            assert_eq!(body["sender_kind"],"alarm_system");
            assert!(body["external_message_id"].as_str().unwrap().starts_with("alarm-"));
            write_json_response(&mut stream,200,"OK",&json!({"id":"fixture-delivery","to":"fixture","trigger_turn":true,"queued":true})).unwrap();
        });
        let dir=std::env::temp_dir().join(format!("alarm-test-{}",uuid::Uuid::new_v4()));let p=dir.join("alarms.json");let now=Utc::now();
        locked(&p,|d|create(d,"owner",args(),now)).unwrap();
        tick(&p,now.timestamp()+61,|a|{
            let receipt=crate::agent_bus::client::send_local_alarm(&config,&a.to,&a.message,&a.message_id)?;
            ensure!(receipt.queued,"not queued");Ok(json!({"id":receipt.id}))
        }).unwrap();
        server.join().unwrap();
        let data:BTreeMap<String,Alarm>=serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        let a=data.values().next().unwrap();assert_eq!(a.state,"queued");assert_eq!(a.receipt.as_ref().unwrap()["id"],"fixture-delivery");
        tick(&p,now.timestamp()+120,|_|panic!("duplicate")).unwrap();std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn timezone_and_validation() {
        assert_eq!(
            timestamp("2026-09-22T12:00:00+10:00").unwrap(),
            timestamp("2026-09-22T02:00:00Z").unwrap()
        );
        let mut d = BTreeMap::new();
        let now = Utc::now();
        let mut a = args();
        a.delay_seconds = Some(0);
        assert!(create(&mut d, "owner", a, now).is_err());
        let mut a = args();
        a.delay_seconds = None;
        a.at = Some((now - chrono::Duration::seconds(1)).to_rfc3339());
        assert!(create(&mut d, "owner", a, now).is_err());
        assert!(d.is_empty());
    }
    #[test]
    fn future_alarm_is_not_sent_and_idle_tick_does_not_rewrite() {
        let dir = std::env::temp_dir().join(format!("alarm-test-{}", uuid::Uuid::new_v4()));
        let p = dir.join("alarms.json");
        let now = Utc::now();
        locked(&p, |d| create(d, "owner", args(), now)).unwrap();
        let before = std::fs::metadata(&p).unwrap().modified().unwrap();
        tick(&p, now.timestamp(), |_| panic!("early send")).unwrap();
        assert_eq!(before, std::fs::metadata(&p).unwrap().modified().unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn idempotent_delay_does_not_move_on_retry() {
        let now = Utc::now();
        let mut data = BTreeMap::new();
        let a = create(&mut data, "owner", args(), now).unwrap();
        let b = create(
            &mut data,
            "owner",
            args(),
            now + chrono::Duration::seconds(30),
        )
        .unwrap();
        assert_eq!(a.at, b.at);
        assert_eq!(a.message_id, b.message_id);
        let mut changed = args();
        changed.message = "different".into();
        assert!(create(&mut data, "owner", changed, now).is_err());
        let mut invalid = args();
        invalid.at = Some(now.to_rfc3339());
        assert!(create(&mut data, "owner", invalid, now).is_err());
    }
    #[test]
    fn durable_retry_same_message_then_no_duplicate_and_cancelled_skipped() {
        let dir = std::env::temp_dir().join(format!("alarm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("alarms.json");
        let now = Utc::now();
        let a = locked(&path, |d| create(d, "owner", args(), now)).unwrap();
        let mut ids = vec![];
        tick(&path, now.timestamp() + 61, |a| {
            ids.push(a.message_id.clone());
            anyhow::bail!("timeout")
        })
        .unwrap();
        let failed:BTreeMap<String,Alarm>=serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(failed.values().next().unwrap().last_error.as_deref().unwrap().contains("timeout"));
        assert!(failed.values().next().unwrap().last_attempt.is_some());
        tick(&path, now.timestamp() + 92, |a| {
            ids.push(a.message_id.clone());
            Ok(json!({"queued":true}))
        })
        .unwrap();
        tick(&path, now.timestamp() + 200, |_| panic!("duplicate send")).unwrap();
        assert_eq!(ids, vec![a.message_id.clone(), a.message_id]);
        locked(&path, |d| {
            let mut b = args();
            b.alarm_id = "cancelled".into();
            let a = create(d, "owner", b, now)?;
            for v in d.values_mut() {
                if v.message_id == a.message_id {
                    v.state = "cancelled".into();
                }
            }
            Ok(())
        })
        .unwrap();
        tick(&path, now.timestamp() + 300, |_| panic!("cancelled sent")).unwrap();
        // Bookkeeping: the cancelled alarm goes 7 days after its time; the
        // queued one stays (the Agent Bus may still authenticate it).
        assert_eq!(read_all_at(&path).unwrap().len(), 2);
        tick(&path, now.timestamp() + 8 * 86_400, |_| panic!("cancelled sent")).unwrap();
        let left = read_all_at(&path).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].state, "queued");
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn owner_cancel_by_message_id_refuses_queued_and_unknown() {
        let dir = std::env::temp_dir().join(format!("alarm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("alarms.json");
        let now = Utc::now();
        assert!(read_all_at(&path).unwrap().is_empty());
        let a = locked(&path, |d| create(d, "owner", args(), now)).unwrap();
        let mut b = args();
        b.alarm_id = "second".into();
        let b = locked(&path, |d| create(d, "owner", b, now)).unwrap();
        assert!(matches!(cancel_by_message_id_at(&path, "alarm-missing"), Err(OwnerCancelError::NotFound)));
        let cancelled = cancel_by_message_id_at(&path, &a.message_id).unwrap();
        assert_eq!(cancelled.state, "cancelled");
        // Idempotent.
        assert_eq!(cancel_by_message_id_at(&path, &a.message_id).unwrap().state, "cancelled");
        tick(&path, now.timestamp() + 61, |_| Ok(json!({"queued": true}))).unwrap();
        assert!(matches!(cancel_by_message_id_at(&path, &b.message_id), Err(OwnerCancelError::AlreadyQueued(_))));
        let states: BTreeMap<String, String> = read_all_at(&path).unwrap().into_iter().map(|a| (a.alarm_id, a.state)).collect();
        assert_eq!(states["remind"], "cancelled");
        assert_eq!(states["second"], "queued");
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod role_pruning_tests {
    use super::*;

    fn intent(owner: &str) -> crate::director_address::AlarmRole {
        crate::director_address::AlarmRole { owner:owner.into(),project_id:"project".into(),raw_digest:"digest".into(),target:None }
    }
    fn args(now: DateTime<Utc>) -> SetArgs {
        SetArgs { alarm_id:"retry".into(),to:"director".into(),message:"reminder".into(),
            at:Some((now+chrono::Duration::seconds(60)).to_rfc3339()),delay_seconds:None }
    }

    #[test]
    fn cancelled_route_retains_exact_same_alarm_id_retry_and_list_metadata_until_business_purge() {
        let root=std::env::temp_dir().join(format!("alarm-role-prune-{}",uuid::Uuid::new_v4()));
        let path=root.join("alarms.json");
        let store=crate::director_address::Store::at(root.join("director"));
        let now=Utc::now();
        let first=set_director_at(&path,&store,"owner",args(now),"project".into(),"digest".into()).unwrap();
        let message_id=first["message_id"].as_str().unwrap();
        let cancelled=cancel_by_message_id_at(&path,message_id).unwrap();
        assert_eq!(prune_removed_role_routes_at(&path,&store,&[cancelled.clone()]).unwrap(),0,"still-visible cancellation is not redundant");
        let retry=set_director_at(&path,&store,"owner",args(now),"different-project".into(),"digest".into()).unwrap();
        assert_eq!(retry["message_id"],first["message_id"]);
        assert_eq!(retry["targetRef"],first["targetRef"],"retry keeps captured project, not today's authority");
        assert_eq!(retry["to"],"director");assert_eq!(retry["state"],"cancelled");
        let before=store.snapshot().unwrap();
        assert_eq!(role_alarm_view(&cancelled,before.alarm_roles.get(message_id))["targetRef"],first["targetRef"]);
        assert!(set_director_at(&path,&store,"owner",args(now),"project".into(),"changed-digest".into()).is_err());
        let early=tick(&path,now.timestamp()+120,|_|panic!("cancelled alarm sent")).unwrap();
        assert!(early.is_empty());
        let removed=tick(&path,now.timestamp()+8*86400,|_|panic!("cancelled alarm sent")).unwrap();
        assert_eq!(removed.len(),1);assert!(read_all_at(&path).unwrap().is_empty());
        assert!(store.snapshot().unwrap().alarm_roles.contains_key(message_id),"business commit happens first");
        assert_eq!(prune_removed_role_routes_at(&path,&store,&removed).unwrap(),1);
        assert!(!store.snapshot().unwrap().alarm_roles.contains_key(message_id));
        assert_eq!(prune_removed_role_routes_at(&path,&store,&removed).unwrap(),0,"cleanup retry is idempotent");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn queued_scheduled_unknown_or_mismatched_routes_are_not_pruned_even_with_missing_business_rows() {
        let root=std::env::temp_dir().join(format!("alarm-role-prune-{}",uuid::Uuid::new_v4()));
        let path=root.join("alarms.json");
        let store=crate::director_address::Store::at(root.join("director"));
        let now=Utc::now();
        let first=set_director_at(&path,&store,"owner",args(now),"project".into(),"digest".into()).unwrap();
        let mut removed=read_all_at(&path).unwrap().remove(0);
        let id=removed.message_id.clone();
        locked(&path,|data|{data.clear();Ok(())}).unwrap();
        for state in ["scheduled","queued","unknown"] {
            removed.state=state.into();
            assert_eq!(prune_removed_role_routes_at(&path,&store,&[removed.clone()]).unwrap(),0);
        }
        removed.state="cancelled".into();removed.owner="wrong-owner".into();
        assert_eq!(prune_removed_role_routes_at(&path,&store,&[removed.clone()]).unwrap(),0);
        assert!(store.snapshot().unwrap().alarm_roles.contains_key(&id));
        assert_eq!(prune_removed_role_routes_at(&path,&store,&[]).unwrap(),0,"absence alone is not terminal proof");
        assert_eq!(first["to"],"director");
        // A failed decode/commit cannot yield business-purge proof. Retry keeps
        // original route and captured metadata; never infer closure from I/O.
        std::fs::write(&path,b"broken-json").unwrap();
        assert!(tick(&path,now.timestamp()+8*86400,|_|panic!("no send")).is_err());
        assert!(prune_removed_role_routes_at(&path,&store,&[removed]).is_err());
        assert_eq!(store.snapshot().unwrap().alarm_roles[&id],intent("owner"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
