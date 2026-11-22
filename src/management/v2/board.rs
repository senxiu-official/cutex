//! r290 P6: one owner read per host, with bounded display examples and shared ETag cache.
use super::{
    repository::EventRepository,
    summary::{BoardGoal, NeedSources},
};
use crate::{
    asks::store::AskState, http::server::SimpleHttpRequest,
    management::server::ManagementRequestContext,
};
use chrono::{DateTime, TimeZone, Utc};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io::Write,
    net::TcpStream,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

pub const PATH: &str = "/v2/board";
pub const CACHE_SECONDS: u64 = 5;
pub const SAFETY_POLL_SECONDS: u64 = 60;
const EXAMPLES: usize = 20;

#[derive(Clone, Debug)]
pub struct Day {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub zone: String,
}
pub fn local_day<T: TimeZone>(now: DateTime<T>) -> anyhow::Result<Day>
where
    T::Offset: std::fmt::Display,
{
    let date = now.date_naive();
    let tomorrow = date
        .succ_opt()
        .ok_or_else(|| anyhow::anyhow!("calendar date overflow"))?;
    let midnight = |date: chrono::NaiveDate| {
        now.timezone()
            .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
            .earliest()
            .map(|v| v.with_timezone(&Utc))
            .ok_or_else(|| anyhow::anyhow!("local calendar midnight unavailable"))
    };
    Ok(Day {
        start: midnight(date)?,
        end: midnight(tomorrow)?,
        zone: now.offset().to_string(),
    })
}

pub fn member_state(session: Option<&Value>) -> &'static str {
    let Some(s) = session else {
        return "unknown";
    };
    let runtime = s
        .pointer("/runtime/status")
        .and_then(Value::as_str)
        .unwrap_or("offline");
    if runtime == "error" {
        return "error";
    }
    if s["pauseReasons"].as_array().is_some_and(|p| !p.is_empty()) {
        return "paused";
    }
    if matches!(runtime, "starting" | "initializing") {
        return "starting";
    }
    if runtime != "online" {
        return "offline";
    }
    if s.pointer("/runtime/activeTurnId")
        .and_then(Value::as_str)
        .is_some()
        || s["activityStatus"] == "busy"
    {
        return "busy";
    }
    "idle"
}

/// Shared store snapshots and owned light inputs; no locks survive projection or plan loading.
pub struct Inputs {
    pub host: String,
    pub instance: String,
    pub projects: Vec<Value>,
    pub sessions: Vec<Value>,
    pub memberships: BTreeMap<String, String>,
    pub asks: Option<Arc<AskState>>,
    pub today: Option<BTreeMap<String, u64>>,
    pub needs: Option<BTreeMap<String, Vec<Value>>>,
    pub plans: BTreeMap<String, Value>,
    pub goals: HashMap<String, BoardGoal>,
    pub day: Day,
}

pub fn project(inputs: Inputs, now: DateTime<Utc>) -> Value {
    let sessions: BTreeMap<&str, &Value> = inputs
        .sessions
        .iter()
        .filter_map(|s| Some((s["cutexSessionId"].as_str()?, s)))
        .collect();
    let mut rows: BTreeMap<String, Value> = inputs
        .projects
        .into_iter()
        .filter_map(|p| Some((p["projectId"].as_str()?.into(), p)))
        .collect();
    for (id, row) in &mut rows {
        row["projectKey"] = json!(format!("origin:{}:{}", inputs.instance, id));
        row["hosts"] = json!([{"hostId":inputs.host,"instanceId":inputs.instance}]);
        row["members"] = json!({"total":0,"busy":0,"idle":0,"offline":0,"paused":0,"starting":0,"error":0,"unknown":0});
        row["openDelegationsCount"] = if inputs.asks.is_some() {
            json!(0)
        } else {
            Value::Null
        };
        row["finishedToday"] = inputs
            .today
            .as_ref()
            .map(|all| json!(all.get(id).copied().unwrap_or(0)))
            .unwrap_or(Value::Null);
        row["needsYouCount"] = if inputs.needs.is_some() {
            json!(0)
        } else {
            Value::Null
        };
        row["needsYou"] = json!({"count":row["needsYouCount"],"items":[]});
        row["problems"] = json!({"count":if inputs.asks.is_some(){json!(0)}else{Value::Null},"byCode":{},"items":[]});
        row["delegations"] = json!([]);
        row["delegationsTruncated"] = json!(false);
        let director = row["directorSessionId"].as_str().map(str::to_string);
        row["director"]=director.as_ref().map(|id| {
            let session=sessions.get(id.as_str()).copied();
            let observed=inputs.goals.get(id).filter(|goal|session.is_some_and(|s|
                s.pointer("/native/threadId").and_then(Value::as_str)==Some(goal.thread_id.as_str())
                && s.pointer("/runtime/runtimeGeneration").and_then(Value::as_u64)==Some(goal.runtime_generation)));
            json!({"sessionId":id,"state":member_state(session),"plan":session.and_then(|session|inputs.plans.get(id).filter(|plan|session.pointer("/native/threadId").is_some_and(|thread|plan["threadId"]==*thread))).and_then(crate::asks::board_progress::plan_progress),
                "goal":observed.and_then(|g|g.goal.clone()),"goalAvailable":observed.is_some(),
                "goalProvenance":observed.map(|g|json!({"source":"nativeNotification","threadId":g.thread_id,"runtimeGeneration":g.runtime_generation,"updatedAt":g.updated_at}))})
        }).unwrap_or(Value::Null);
    }
    let problem = |row: &mut Value, code: &str, id: &str, target_kind: &str| {
        if let Some(count) = row["problems"]["count"].as_u64() {
            row["problems"]["count"] = json!(count + 1);
        }
        let by_code = &mut row["problems"]["byCode"];
        by_code[code] = json!(by_code[code].as_u64().unwrap_or(0) + 1);
        let items = row["problems"]["items"].as_array_mut().unwrap();
        if items.len() < EXAMPLES {
            items.push(json!({"code":code,"id":id,"targetKind":target_kind}));
        }
    };
    for (id, project) in &inputs.memberships {
        let Some(row) = rows.get_mut(project) else {
            continue;
        };
        let session = sessions.get(id.as_str()).copied();
        if session.is_some_and(|s| matches!(s["lifecycle"].as_str(), Some("retired" | "archived")))
        {
            continue;
        }
        let state = member_state(session);
        let members = &mut row["members"];
        members["total"] = json!(members["total"].as_u64().unwrap() + 1);
        members[state] = json!(members[state].as_u64().unwrap() + 1);
        if state == "error" {
            problem(row, "memberError", id, "agent");
        }
        if state == "idle"
            && session
                .and_then(|s| s.pointer("/activity/lastTurnError"))
                .is_some_and(|v| !v.is_null())
        {
            problem(row, "lastTurnFailed", id, "agent");
        }
        if state == "unknown" {
            problem(row, "memberUnavailable", id, "agent");
        }
    }
    let mut ask_projects = BTreeMap::new();
    if let Some(state) = &inputs.asks {
        let mut asks = state
            .asks
            .values()
            .filter(|a| a.is_open())
            .collect::<Vec<_>>();
        asks.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.ask_id.cmp(&b.ask_id))
        });
        for ask in asks {
            let Some(project) = ask.project_id.as_ref() else {
                continue;
            };
            ask_projects.insert(ask.ask_id.clone(), project.clone());
            let Some(row) = rows.get_mut(project) else {
                continue;
            };
            row["openDelegationsCount"] = json!(row["openDelegationsCount"].as_u64().unwrap() + 1);
            let session = sessions.get(ask.target_cutex_session_id.as_str()).copied();
            let progress = crate::asks::board_progress::delegation(
                ask,
                session,
                inputs.plans.get(&ask.target_cutex_session_id),
                now,
            );
            let execution = serde_json::to_value(ask.execution).unwrap_or(Value::Null);
            if matches!(execution.as_str(), Some("failed" | "rejected")) {
                problem(row, "delegationFailed", &ask.ask_id, "delegation");
            }
            if progress["stalled"] == true {
                problem(row, "delegationStalled", &ask.ask_id, "delegation");
            }
            if ask.current_attempt().is_some_and(|a| a.stop_unconfirmed) {
                problem(row, "stopUnconfirmed", &ask.ask_id, "delegation");
            }
            if matches!(execution.as_str(), Some("running" | "admitted")) {
                if session.is_none() {
                    problem(row, "handlerUnavailable", &ask.ask_id, "delegation");
                } else if member_state(session) == "offline" {
                    problem(row, "handlerOffline", &ask.ask_id, "delegation");
                }
            }
            let list = row["delegations"].as_array_mut().unwrap();
            if list.len() < EXAMPLES {
                let mut value = json!({"askId":ask.ask_id,"title":ask.title.chars().take(160).collect::<String>(),
                    "requester":crate::asks::view::ask_requester(ask),"target":ask.target_cutex_session_id,"execution":execution,"hold":ask.hold,
                    "updatedAt":ask.updated_at,"attempt":ask.current_attempt().map(|a|a.n),
                    "progressNote":ask.current_attempt().and_then(|a|a.progress.last()).map(|n|json!({"text":n.text.chars().take(300).collect::<String>(),"at":n.at}))});
                value
                    .as_object_mut()
                    .unwrap()
                    .extend(progress.as_object().unwrap().clone());
                list.push(value);
            } else {
                row["delegationsTruncated"] = json!(true);
            }
        }
    }
    let mut need_ids: BTreeMap<String, BTreeMap<(String, String, String), Value>> = BTreeMap::new();
    let mut unassigned = BTreeSet::new();
    if let Some(needs) = &inputs.needs {
        for (agent, items) in needs {
            for need in items {
                let kind = need["kind"].as_str().unwrap_or("");
                let id = need["id"].as_str().unwrap_or("");
                let key = (
                    kind.to_string(),
                    id.to_string(),
                    if id.starts_with("native:") {
                        agent.clone()
                    } else {
                        String::new()
                    },
                );
                let project = need["id"]
                    .as_str()
                    .and_then(|id| ask_projects.get(id))
                    .or_else(|| inputs.memberships.get(agent));
                if let Some(id) = project.filter(|id| rows.contains_key(*id)) {
                    need_ids
                        .entry(id.clone())
                        .or_default()
                        .insert(key, json!({"kind":kind,"id":need["id"],"agentId":agent}));
                } else {
                    unassigned.insert(key);
                }
            }
        }
    }
    for (id, needs) in need_ids {
        if let Some(row) = rows.get_mut(&id) {
            row["needsYouCount"] = json!(needs.len());
            row["needsYou"] = json!({"count":needs.len(),"items":needs.into_values().take(EXAMPLES).collect::<Vec<_>>()});
        }
    }
    json!({"contractVersion":1,"hostId":inputs.host,"instanceId":inputs.instance,"generatedAt":now.to_rfc3339(),"refreshAfterSeconds":SAFETY_POLL_SECONDS,"cacheTtlSeconds":CACHE_SECONDS,
        "day":{"start":inputs.day.start.to_rfc3339(),"end":inputs.day.end.to_rfc3339(),"timeZone":inputs.day.zone},
        "policies":{"stalledAfterSeconds":crate::asks::board_progress::STALLED_AFTER_SECONDS,"examplesPerProject":EXAMPLES},
        "availability":{"members":true,"delegations":inputs.asks.is_some(),"finishedToday":inputs.today.is_some(),"needs":inputs.needs.is_some()},
        "unassignedNeedsYouCount":if inputs.needs.is_some(){json!(unassigned.len())}else{Value::Null},"projects":rows.into_values().collect::<Vec<_>>()})
}

fn collect(
    context: ManagementRequestContext,
    repository: &EventRepository,
) -> anyhow::Result<Value> {
    let registry = (context.load_registry)()?;
    let list = super::session::session_list_resource_with_hidden(
        &registry,
        context.load_runtime_status,
        repository,
        true,
    )?;
    let sessions = list["sessions"].as_array().cloned().unwrap_or_default();
    let principal = crate::management::control_plane::HumanManagementPrincipal::authenticated();
    let collection = (context.list_management_projects)(&principal)
        .map_err(|e| anyhow::anyhow!("project roster unavailable: {e:?}"))?;
    let projects:Vec<Value>=collection.projects.into_iter().map(|p|json!({"projectId":p.project_id.as_str(),"name":p.presentation.display_name,"color":p.presentation.color,
        "directorSessionId":p.director_cutex_session_id.as_ref().map(|id|id.as_str())})).collect();
    let directory = crate::agent_management::AgentManagementStore::open_default()?
        .directory_snapshot_shared()?;
    let memberships = directory
        .agents
        .iter()
        .filter(|(_, a)| a.retired_at.is_none())
        .filter_map(|(id, a)| {
            Some((
                id.as_str().into(),
                crate::agent_management::current_project_id(&directory, a)?
                    .as_str()
                    .into(),
            ))
        })
        .collect::<BTreeMap<String, String>>();
    let now = Utc::now();
    let day = local_day(chrono::Local::now())?;
    let store = crate::asks::store::AskStore::open_default().ok();
    let asks = store.as_ref().and_then(|s| s.shared().ok());
    let today = store
        .as_ref()
        .filter(|_| asks.is_some())
        .and_then(|s| s.board_closed_counts(day.start, day.end).ok());
    let pending = super::server_requests::pending_all();
    let approvals =
        crate::approvals::ApprovalStore::open_default().and_then(|s| s.list(false, now));
    let notifications = crate::owner_notify::store::OwnerNotifyStore::open_default()
        .and_then(|s| s.list("pending", None, usize::MAX));
    let needs = match (&asks, pending, approvals, notifications) {
        (Some(asks), Ok(pending), Ok(approvals), Ok(notifications)) => {
            let sources = NeedSources {
                native: super::summary::answerable_native_requests(&sessions, pending),
                approvals: approvals
                    .into_iter()
                    .filter(|a| a.state == crate::approvals::ApprovalState::Pending)
                    .filter_map(|a| serde_json::to_value(a).ok())
                    .collect(),
                asks: asks
                    .asks
                    .values()
                    .filter(|a| a.is_open())
                    .map(|a| crate::asks::view::summary(asks, a))
                    .collect(),
                owner_notifications: notifications,
                evaluator: crate::notification_levels::Evaluator::current(),
                agent_projects: memberships
                    .iter()
                    .map(|(a, p)| (a.clone(), p.clone()))
                    .collect(),
                projects_known: true,
            };
            Some(super::summary::needs_by_agent(&sources))
        }
        _ => None,
    };
    let project_ids = projects
        .iter()
        .filter_map(|p| p["projectId"].as_str())
        .collect::<BTreeSet<_>>();
    let session_ids = sessions
        .iter()
        .filter_map(|s| s["cutexSessionId"].as_str())
        .collect::<BTreeSet<_>>();
    let mut wanted = BTreeSet::new();
    if let Some(asks) = &asks {
        wanted.extend(
            asks.asks
                .values()
                .filter(|a| {
                    a.is_open()
                        && a.project_id
                            .as_deref()
                            .is_some_and(|p| project_ids.contains(p))
                        && session_ids.contains(a.target_cutex_session_id.as_str())
                        && matches!(
                            a.execution,
                            crate::asks::model::ExecutionState::Running
                                | crate::asks::model::ExecutionState::Admitted
                        )
                })
                .map(|a| a.target_cutex_session_id.clone()),
        );
    }
    for p in &projects {
        if let Some(id) = p["directorSessionId"]
            .as_str()
            .filter(|id| session_ids.contains(*id))
        {
            wanted.insert(id.into());
        }
    }
    let observations = super::summary::board_projections()
        .ok()
        .flatten()
        .unwrap_or_default();
    let mut plans = BTreeMap::new();
    let mut goals = HashMap::new();
    for (id, projection) in observations {
        if let Some(goal) = projection.board_goal {
            goals.insert(id.clone(), goal);
        }
        if let Some(plan) = projection.board_plan.filter(|p| {
            sessions.iter().any(|s| {
                s["cutexSessionId"] == id
                    && s.pointer("/native/threadId").and_then(Value::as_str)
                        == Some(p.thread_id.as_str())
                    && s.pointer("/runtime/runtimeGeneration")
                        .and_then(Value::as_u64)
                        == Some(p.runtime_generation)
            })
        }) {
            plans.insert(id,json!({"threadId":plan.thread_id,"turnId":plan.turn_id,"boardProgress":plan.progress,"updatedAt":plan.updated_at}));
        }
    }
    for id in wanted {
        if !plans.contains_key(&id) {
            let value = (context.load_board_plan)(&id);
            plans.insert(id, value);
        }
    }
    Ok(project(
        Inputs {
            host: crate::platform::host::current_host_name(),
            instance: crate::management::instance::instance_id(),
            projects,
            sessions,
            memberships,
            asks,
            today,
            needs,
            plans,
            goals,
            day,
        },
        now,
    ))
}

#[derive(Default)]
struct Cache {
    entry: Option<(String, Instant, Arc<Value>, String)>,
}
impl Cache {
    fn read(
        &mut self,
        key: &str,
        load: impl FnOnce() -> anyhow::Result<Value>,
    ) -> anyhow::Result<(Arc<Value>, String)> {
        if let Some((old, at, body, etag)) = &self.entry {
            if old == key && at.elapsed() < Duration::from_secs(CACHE_SECONDS) {
                return Ok((body.clone(), etag.clone()));
            }
        }
        let mut body = load()?;
        let mut semantic = body.clone();
        semantic.as_object_mut().unwrap().remove("generatedAt");
        semantic.as_object_mut().unwrap().remove("etag");
        let etag = format!("\"{:x}\"", Sha256::digest(serde_json::to_vec(&semantic)?));
        body["etag"] = json!(etag);
        let body = Arc::new(body);
        self.entry = Some((key.into(), Instant::now(), body.clone(), etag.clone()));
        Ok((body, etag))
    }
}
fn matches_etag(header: Option<&str>, etag: &str) -> bool {
    header.is_some_and(|s| {
        s.split(',')
            .any(|v| v.trim() == "*" || v.trim().trim_start_matches("W/") == etag)
    })
}

pub fn handle(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
    repository: &EventRepository,
) -> anyhow::Result<()> {
    if request.method != "GET" {
        return super::server::write_v2_error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "use GET",
            false,
            json!({}),
        );
    }
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let key = crate::config::paths::runtime_dir()?
        .to_string_lossy()
        .into_owned();
    let result = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .read(&key, || collect(context, repository));
    let (body, etag) = match result {
        Ok(value) => value,
        Err(error) => {
            return super::server::write_v2_error(
                stream,
                503,
                "Service Unavailable",
                "board_unavailable",
                &error.to_string(),
                true,
                json!({}),
            )
        }
    };
    let header = request
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("if-none-match"))
        .map(|(_, v)| v.as_str());
    if matches_etag(header, &etag) {
        write!(stream,"HTTP/1.1 304 Not Modified\r\nETag: {etag}\r\nCache-Control: private, max-age=0\r\nConnection: close\r\n\r\n")?;
        return Ok(());
    }
    let bytes = serde_json::to_vec(body.as_ref())?;
    write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nETag: {etag}\r\nCache-Control: private, max-age=0\r\nConnection: close\r\n\r\n",bytes.len())?;
    stream.write_all(&bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Inputs, DateTime<Utc>) {
        let (ask, session, now) = crate::asks::board_progress::tests::fixture();
        let target = ask.target_cutex_session_id.clone();
        let mut state = AskState::default();
        state.asks.insert(ask.ask_id.clone(), ask.into());
        let inputs = Inputs {
            host: "eva".into(),
            instance: "instance-a".into(),
            projects: vec![
                json!({"projectId":"alpha","name":"Project A","color":"blue","directorSessionId":target}),
                json!({"projectId":"empty","name":"Empty"}),
            ],
            sessions: vec![session],
            memberships: BTreeMap::from([(target.clone(), "alpha".into())]),
            asks: Some(Arc::new(state)),
            today: Some(BTreeMap::from([("alpha".into(), 3)])),
            needs: Some(BTreeMap::new()),
            plans: BTreeMap::new(),
            goals: HashMap::new(),
            day: local_day(now).unwrap(),
        };
        (inputs, now)
    }
    #[test]
    fn board_needs_have_capped_jump_targets_and_native_ids_are_per_agent() {
        let (mut inputs, now) = fixture();
        let first = inputs.memberships.keys().next().unwrap().clone();
        inputs
            .memberships
            .insert("second-agent".into(), "alpha".into());
        inputs.needs = Some(BTreeMap::from([
            (
                first.clone(),
                vec![json!({"kind":"question","id":"native:1"})],
            ),
            (
                "second-agent".into(),
                vec![json!({"kind":"question","id":"native:1"})],
            ),
        ]));
        let body = project(inputs, now);
        let row = &body["projects"][0];
        assert_eq!(row["needsYouCount"], 2);
        assert_eq!(row["needsYou"]["items"].as_array().unwrap().len(), 2);
        assert_ne!(
            row["needsYou"]["items"][0]["agentId"],
            row["needsYou"]["items"][1]["agentId"]
        );
        let (mut inputs, _) = fixture();
        inputs.needs = Some(BTreeMap::from([(
            first,
            (0..30)
                .map(|n| json!({"kind":"ownerReply","id":format!("n-{n}")}))
                .collect(),
        )]));
        let body = project(inputs, now);
        let row = &body["projects"][0];
        assert_eq!(row["needsYouCount"], 30);
        assert_eq!(row["needsYou"]["count"], 30);
        assert_eq!(row["needsYou"]["items"].as_array().unwrap().len(), EXAMPLES);
        assert!(row["needsYou"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["kind"].is_string() && v["id"].is_string() && v["agentId"].is_string()));
    }
    #[test]
    fn board_last_turn_failure_is_soft_and_problem_targets_are_typed() {
        let (mut inputs, now) = fixture();
        inputs.sessions[0]["runtime"]["activeTurnId"] = Value::Null;
        inputs.sessions[0]["activity"] = json!({"lastTurnError":{"message":"previous failure"}});
        let body = project(inputs, now);
        let row = &body["projects"][0];
        assert_eq!(row["members"]["idle"], 1);
        assert_eq!(row["members"]["error"], 0);
        assert_eq!(row["problems"]["items"][0]["code"], "lastTurnFailed");
        assert_eq!(row["problems"]["items"][0]["targetKind"], "agent");
        let (inputs, now) = fixture();
        let body = project(inputs, now);
        assert_eq!(
            body["projects"][0]["problems"]["items"][0]["targetKind"],
            "delegation"
        );
        assert_eq!(body["refreshAfterSeconds"], 60);
        assert_eq!(CACHE_SECONDS, 5);
    }
    #[test]
    fn board_problem_codes_count_all_soft_problems_beyond_the_example_cap() {
        let (mut inputs, now) = fixture();
        inputs.asks = Some(Arc::new(AskState::default()));
        inputs.sessions.clear();
        inputs.memberships.clear();
        for n in 0..21 {
            let id = format!("member-{n:02}");
            inputs.memberships.insert(id.clone(), "alpha".into());
            inputs
                .sessions
                .push(json!({"cutexSessionId":id,"runtime":{"status":"online"},
                "activity":{"lastTurnError":{"message":"previous failure"}}}));
        }
        let body = project(inputs, now);
        let problems = &body["projects"][0]["problems"];
        assert_eq!(problems["count"], 21);
        assert_eq!(problems["items"].as_array().unwrap().len(), EXAMPLES);
        assert_eq!(problems["byCode"], json!({"lastTurnFailed":21}));
        assert_eq!(body["projects"][1]["problems"]["byCode"], json!({}));
        let (body, _) = Cache::default().read("soft-codes", || Ok(body)).unwrap();
        super::super::schema_export::assert_valid("board", "snapshot", body.as_ref());
    }
    #[test]
    fn board_aggregates_empty_projects_exact_counts_and_bounded_examples() {
        let (mut inputs, now) = fixture();
        let template = inputs
            .asks
            .as_ref()
            .unwrap()
            .asks
            .values()
            .next()
            .unwrap()
            .clone();
        let state = Arc::make_mut(inputs.asks.as_mut().unwrap());
        for n in 0..51 {
            let mut ask = (*template).clone();
            ask.ask_id = format!("ask-extra-{n}");
            state.asks.insert(ask.ask_id.clone(), ask.into());
        }
        let target = template.target_cutex_session_id.clone();
        inputs.needs = Some(BTreeMap::from([(
            target,
            vec![
                json!({"kind":"ownerMessage","id":"n"}),
                json!({"kind":"ownerMessage","id":"n"}),
            ],
        )]));
        let body = project(inputs, now);
        let alpha = &body["projects"][0];
        let empty = &body["projects"][1];
        assert_eq!(alpha["members"]["busy"], 1);
        assert_eq!(alpha["openDelegationsCount"], 52);
        assert_eq!(alpha["finishedToday"], 3);
        assert_eq!(alpha["needsYouCount"], 1);
        assert_eq!(alpha["delegations"].as_array().unwrap().len(), EXAMPLES);
        assert_eq!(alpha["delegationsTruncated"], true);
        assert_eq!(alpha["problems"]["count"], 52);
        assert_eq!(alpha["problems"]["byCode"], json!({"delegationStalled":52}));
        assert_eq!(empty["members"]["total"], 0);
        assert_eq!(empty["openDelegationsCount"], 0);
        assert_eq!(alpha["projectKey"], "origin:instance-a:alpha");
        assert_eq!(alpha["hosts"][0]["hostId"], "eva");
        let (body, _) = Cache::default().read("schema", || Ok(body)).unwrap();
        super::super::schema_export::assert_valid("board", "snapshot", body.as_ref());
    }
    #[test]
    fn board_unavailable_sources_are_null_and_goals_require_current_binding() {
        let (mut inputs, now) = fixture();
        let id = inputs.memberships.keys().next().unwrap().clone();
        inputs.asks = None;
        inputs.today = None;
        inputs.needs = None;
        inputs.goals.insert(
            id.clone(),
            BoardGoal {
                thread_id: "foreign".into(),
                runtime_generation: 1,
                goal: Some(json!({"objective":"wrong"})),
                updated_at: now.to_rfc3339(),
            },
        );
        let body = project(inputs, now);
        let p = &body["projects"][0];
        assert!(p["openDelegationsCount"].is_null());
        assert!(p["finishedToday"].is_null());
        assert!(p["needsYouCount"].is_null());
        assert_eq!(p["director"]["goalAvailable"], false);
        let (mut inputs, _) = fixture();
        inputs.goals.insert(
            id,
            BoardGoal {
                thread_id: "thread".into(),
                runtime_generation: 1,
                goal: None,
                updated_at: now.to_rfc3339(),
            },
        );
        let body = project(inputs, now);
        assert_eq!(body["projects"][0]["director"]["goalAvailable"], true);
        assert!(body["projects"][0]["director"]["goal"].is_null());
    }
    #[test]
    fn board_missing_running_handler_is_a_problem_without_guessing_stalled() {
        let (mut inputs, now) = fixture();
        inputs.sessions.clear();
        let body = project(inputs, now);
        let row = &body["projects"][0];
        assert_eq!(row["members"]["unknown"], 1);
        assert!(row["problems"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["code"] == "handlerUnavailable"));
        assert_eq!(row["delegations"][0]["stalled"], false);
    }
    #[test]
    fn board_etag_cache_coalesces_and_excludes_wall_clock_but_not_content() {
        let mut cache = Cache::default();
        let (inputs, now) = fixture();
        let body = project(inputs, now);
        let (first, etag) = cache.read("a", || Ok(body.clone())).unwrap();
        let (same, same_etag) = cache
            .read("a", || panic!("fresh cache must not touch stores"))
            .unwrap();
        assert!(Arc::ptr_eq(&first, &same));
        assert_eq!(etag, same_etag);
        assert!(matches_etag(Some(&format!("W/{etag}, \"other\"")), &etag));
        assert!(matches_etag(Some("*"), &etag));
        cache.entry.as_mut().unwrap().1 = Instant::now() - Duration::from_secs(CACHE_SECONDS + 1);
        let mut refreshed = body.clone();
        refreshed["generatedAt"] = json!("later");
        assert_eq!(cache.read("a", || Ok(refreshed)).unwrap().1, etag);
        let mut changed = body;
        changed["projects"][0]["members"]["busy"] = json!(2);
        assert_ne!(cache.read("b", || Ok(changed)).unwrap().1, etag);
    }
    #[test]
    fn board_local_day_uses_calendar_midnight_and_half_open_bounds() {
        let offset = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        let now = offset.with_ymd_and_hms(2026, 11, 17, 0, 1, 0).unwrap();
        let day = local_day(now).unwrap();
        assert_eq!(day.start.to_rfc3339(), "2026-11-16T16:00:00+00:00");
        assert_eq!(day.end.to_rfc3339(), "2026-11-17T16:00:00+00:00");
        assert_ne!(
            day.start,
            (now.with_timezone(&Utc) - chrono::Duration::days(1))
        );
    }
}
