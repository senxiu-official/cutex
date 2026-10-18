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
    ensure!(alarm.to == target, "durable alarm target conflict");
    let expected_message = if legacy_agent_envelope {
        format!("[message from {ALARM_SYSTEM_SENDER}] {}", alarm.message)
    } else {
        alarm.message.clone()
    };
    ensure!(expected_message == message, "durable alarm content conflict");
    ensure!(!alarm.owner.trim().is_empty(), "durable alarm owner absent");
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
        message_id: format!("alarm-{}", uuid::Uuid::new_v4()),
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
/// A cutex system notice to one agent through the alarm scheduler (durable,
/// delivered `after_turn` by the Agent Bus like any alarm, a few seconds from
/// now). `owner` is a reserved non-session owner (e.g.
/// [`APPROVAL_NOTICE_OWNER`]); `notice_id` makes a repeat idempotent. The
/// store lock is contended briefly by the scheduler, so this retries a few
/// times.
pub fn set_system_notice(owner: &str, notice_id: &str, to: &str, message: &str) -> anyhow::Result<Alarm> {
    set_system_notice_at(&path()?, owner, notice_id, to, message, Utc::now())
}

/// Owner of the owner-approval decision notices (cutex `approval-decision-notice`).
pub const APPROVAL_NOTICE_OWNER: &str = "cutex.approvals";

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
    Ok(json!(rows))
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
) -> anyhow::Result<()> {
    locked(path, |data| {
        for a in data
            .values_mut()
            .filter(|a| a.state == "scheduled" && a.next_attempt <= now)
        {
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
        Ok(())
    })
}
pub fn spawn() -> anyhow::Result<()> {
    std::thread::Builder::new().name("cutex-alarms".into()).spawn(||loop {
        let result=(|| {
            let config=crate::config::store::load_codez_config_checked()?;
            tick(&path()?,Utc::now().timestamp(),|a| {
                let response=crate::agent_bus::client::send_local_alarm(&config,&a.to,&a.message,&a.message_id)?;
                ensure!(response.queued,"alarm delivery not acknowledged");
                Ok(json!({"message_id":response.id,"queued":response.queued,"delivery_state":response.delivery_state}))
            })
        })();
        let delay=if let Err(e)=result { eprintln!("Cutex alarm scheduler: {e}"); 30 } else {2};
        std::thread::sleep(std::time::Duration::from_secs(delay));
    })?;
    Ok(())
}
pub fn tools() -> Vec<Value> {
    vec![
        json!({"name":"alarm_set","description":"Schedule a one-shot reminder message to yourself or another agent you may reach. The Cutex service delivers it after_turn at the requested time, including after service restarts.","inputSchema":{"type":"object","properties":{"alarm_id":{"type":"string","description":"Stable ID; reuse for an unchanged retry."},"to":{"type":"string","description":"ID or exact agent name (case insensitive); IDs accept a unique prefix or suffix of at least 6 hex characters."},"message":{"type":"string"},"at":{"type":"string","description":"RFC3339 with timezone, or local YYYY-MM-DD HH:MM. Use at OR delay_seconds."},"delay_seconds":{"type":"integer","minimum":1}},"required":["alarm_id","to","message"],"additionalProperties":false}}),
        json!({"name":"alarm_list","description":"List alarms created by this Agent, including delivery state.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}),
        json!({"name":"alarm_cancel","description":"Cancel a pending alarm created by this Agent.","inputSchema":{"type":"object","properties":{"alarm_id":{"type":"string"}},"required":["alarm_id"],"additionalProperties":false}}),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

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
