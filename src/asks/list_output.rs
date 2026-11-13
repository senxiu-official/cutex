//! Tool-only projections. Communication authority and HTTP/peer rows stay unchanged.
use super::*;

pub(super) fn pending_notices(state: &AskState, authorized: &[&DeliveryRecord]) -> Value {
    let mut counts = json!({"answer":0,"question":0,"state":0});
    for delivery in authorized {
        let kind = match delivery.kind { DeliveryKind::Answer => "answer", DeliveryKind::Question => "question", DeliveryKind::State => "state" };
        counts[kind] = json!(counts[kind].as_u64().unwrap_or(0) + 1);
    }
    let mut ordered = authorized.to_vec();
    ordered.sort_by_key(|d| (&d.created_at, &d.delivery_id));
    let entries: Vec<_> = ordered.into_iter().take(50).map(|delivery| json!({
        "delivery_id":delivery.delivery_id, "delegation":delivery.ask_id, "kind":delivery.kind,
        "ref_id":delivery.ref_id, "passive":delivery.passive, "created_at":delivery.created_at,
        "title":state.asks.get(&delivery.ask_id).map(|ask| ask.title.chars().take(512).collect::<String>()),
        "read_action":{"tool":"delegation_read","arguments":{"delegation":delivery.ask_id}},
        "claim_action":{"tool":"delegation_notices_read","arguments":{"delivery_ids":[delivery.delivery_id]}}
    })).collect();
    json!({"total":authorized.len(), "counts":counts, "entries":entries,
        "more":authorized.len() > 50,
        "detail":"Pending notices include answers, questions and state changes. unclaimed_results is the total of all three kinds. Read a delegation with delegation_read; delegation_notices_read marks pending notices read without waiting."})
}

#[cfg(test)]
pub(super) fn resolve_name(directory: &super::super::directory::Directory, caller: &str,
    name: &str, project: Option<&str>) -> Result<String, AskError> {
    resolve_name_with_notes(directory,caller,name,project).map(|resolution|resolution.cutex_session_id)
}

pub(super) fn resolve_name_with_notes(directory: &super::super::directory::Directory, caller: &str,
    name: &str, project: Option<&str>) -> Result<crate::agent_names::NameResolution, AskError> {
    // A project is a candidate filter, never an authorization grant. Reuse
    // delegate's resolver; its rename-aware implementation also sees this scope.
    let scope=project.map(|project|crate::agent_names::NameScope::for_project(Some(project)));
    let (agent,resolution)=directory.resolve_scoped(Some(caller),name,scope.as_ref())?;
    if !agent.agent_enabled { return Err(AskError::new("id_not_found","That agent does not accept agent input")); }
    Ok(resolution)
}

pub(super) fn compact(row: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for key in ["name", "id", "profile", "model", "status", "queue"] {
        out.insert(key.into(), row.get(key).cloned().unwrap_or(Value::Null));
    }
    if row["is_director"] == true { out.insert("is_director".into(), json!(true)); }
    if row.get("agent_pause").is_some_and(|value| !value.is_null()) {
        out.insert("agent_pause".into(), row["agent_pause"].clone());
    }
    if row["pauseReasons"].as_array().is_some_and(|reasons| !reasons.is_empty()) {
        out.insert("pauseReasons".into(), row["pauseReasons"].clone());
    }
    Value::Object(out)
}

pub(super) fn remote_row(mut row: Value, full: bool) -> Value {
    row["id"] = row["cutexSessionId"].clone();
    if full { row } else { compact(&row) }
}

pub(super) fn pending_reviews(mut reviews: Value) -> Value {
    for (kind, terminal) in [
        ("handover", &["accepted", "discarded", "superseded"][..]),
        ("start", &["adopted", "discarded"][..]),
        ("changes", &["finalized", "superseded", "aborted"][..]),
    ] {
        if let Some(rows) = reviews[kind].as_array_mut() {
            rows.retain(|row| !row["state"].as_str().is_some_and(|state| terminal.contains(&state)));
        }
    }
    reviews
}

pub(super) struct Page {
    pub rows: Vec<Value>,
    pub total: usize,
    pub offline: usize,
    pub next: Option<usize>,
    pub projects: Vec<Value>,
    pub other_projects: Vec<Value>,
    pub other_agents: usize,
}

pub(super) fn local_page(rows: Vec<(bool, bool, String, Value)>, args: &ListArgs,
    own_project: Option<&str>, directory: &super::super::directory::Directory, caller: &str,
    limit: usize, cursor: usize) -> Result<Page, AskError> {
    let selected = args.project.as_deref().or(own_project);
    let own_domain = own_project.map(|project|crate::config::agent_security::DomainKey::Project(project.to_owned()))
        .unwrap_or_else(||directory.boundary.domain(caller));
    if let Some(project) = &args.project {
        if Some(project.as_str()) != own_project
            && !rows.iter().any(|row| row.3["project"] == *project)
            && !directory.boundary.visible_projects(caller).contains(project) {
            return Err(AskError::new("project_not_found", "No visible project matches this project id"));
        }
    }
    let mut others = std::collections::BTreeMap::<String, (usize, usize)>::new();
    let mut projects = std::collections::BTreeMap::<Option<String>, (usize, usize)>::new();
    let mut other_agents = 0;
    let mut selected_rows = Vec::new();
    for (_, _, _, row) in rows {
        let project = row["project"].as_str();
        if args.name.is_some() {
            if args.project.is_some() && project != selected {
                return Err(AskError::new("id_not_found", "The agent is not in the selected project"));
            }
            selected_rows.push(row);
        } else if if args.project.is_some() { project == selected } else {
            project == own_project || project.is_some_and(|project|directory.boundary.explicitly_linked(
                &own_domain,&crate::config::agent_security::DomainKey::Project(project.to_owned())))
        } {
            selected_rows.push(row);
        } else if let Some(project) = project {
            let count = others.entry(project.to_owned()).or_default();
            count.0 += 1;
            count.1 += usize::from(row["status"] == "offline");
        } else { other_agents += 1; }
    }
    let total = selected_rows.len();
    let offline = selected_rows.iter().filter(|row| row["status"] == "offline").count();
    for row in &selected_rows {
        let count = projects.entry(row["project"].as_str().map(str::to_owned)).or_default();
        count.0 += 1;
        count.1 += usize::from(row["status"] == "offline");
    }
    if args.name.is_none() && !args.include_offline { selected_rows.retain(|row| row["status"] != "offline"); }
    let matching = selected_rows.len();
    let start = if args.name.is_some() { 0 } else { cursor.min(matching) };
    let rows = selected_rows.into_iter().skip(start).take(limit).map(|row| {
        if args.name.is_some() { row } else {
            let mut out = compact(&row);
            if row["project"].as_str().is_some_and(|project|Some(project) != own_project) {
                out["project"] = row["project"].clone();
            }
            out
        }
    }).collect::<Vec<_>>();
    let next = (start + rows.len() < matching).then_some(start + rows.len());
    let mut projects: Vec<_> = projects.into_iter().map(|(project,(total,offline))|
        json!({"project":project,"agents_total":total,"agents_offline":offline})).collect();
    projects.sort_by_key(|row|(row["project"].as_str() != own_project,row["project"].as_str().map(str::to_owned)));
    Ok(Page { rows, total, offline, next, projects,
        other_projects:others.into_iter().map(|(project,(total,offline))|
            json!({"project":project,"agents_total":total,"agents_offline":offline})).collect(), other_agents })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_notice_projection_classifies_and_bounds_without_claiming() {
        let mut state = AskState::default();
        let now = chrono::Utc::now();
        let mut new = crate::asks::store::tests::new_ask("notices", "visible title");
        new.requester = Requester::Agent { cutex_session_id:"requester".into(), name:None };
        let (ask, _) = state.create_ask(new, now).unwrap();
        crate::asks::delivery::record_answer_delivery(&mut state, &ask, "answer", now);
        crate::asks::delivery::record_question_delivery(&mut state, &ask, "question", now);
        for index in 0..53 {
            crate::asks::delivery::record_state_delivery(&mut state, &ask, &format!("state-{index}"), now);
        }
        let deliveries: Vec<_> = state.deliveries.values().collect();
        let projected = pending_notices(&state, &deliveries);
        assert_eq!(projected["total"], 55);
        assert_eq!(projected["counts"], json!({"answer":1,"question":1,"state":53}));
        assert_eq!(projected["entries"].as_array().unwrap().len(), 50);
        assert_eq!(projected["more"], true);
        assert_eq!(projected["entries"][0]["read_action"]["arguments"]["delegation"], ask.ask_id);
        assert!(state.deliveries.values().all(|row| row.state == DeliveryState::Pending));
    }

    #[test]
    fn agent_list_keeps_pending_reviews_and_unknown_future_states() {
        let reviews = pending_reviews(json!({
            "handover":[{"id":"pending","state":"pending"},{"state":"accepted"},{"state":"discarded"},{"state":"superseded"}],
            "start":[{"id":"open","state":"open"},{"id":"uncertain","state":"deliveryUnknown"},{"state":"adopted"},{"state":"discarded"}],
            "changes":[{"id":"prepared","state":"prepared"},{"id":"new","state":"futureState"},
                {"state":"finalized","recipients":["no-longer-needed"]},{"state":"aborted"},{"state":"superseded"}]
        }));
        assert_eq!(reviews["handover"].as_array().unwrap().len(),1);
        assert_eq!(reviews["start"].as_array().unwrap().len(),2);
        assert_eq!(reviews["changes"].as_array().unwrap().len(),2);
        assert!(!reviews.to_string().contains("no-longer-needed"));
    }

    #[test]
    fn agent_list_compact_preserves_pauses_and_omits_unset_flags() {
        let row = json!({"name":"worker","id":"cutex.worker","profile":"p","model":"m","status":"paused","queue":2,
            "is_director":true,"agent_pause":{"reason":"owner","phase":"paused"},"pauseReasons":[{"reason":"projectPause"}],
            "note":"full detail","provider_runtime":"codex","first_message":{"text":"large"},"lastActivityAt":"now"});
        let compacted = compact(&row);
        assert_eq!(compacted.as_object().unwrap().len(),9);
        assert_eq!(compacted["agent_pause"],row["agent_pause"]);
        assert_eq!(compacted["pauseReasons"],row["pauseReasons"]);
        assert_eq!(compacted["is_director"],true);
        assert!(compacted.get("note").is_none());
        let minimal = compact(&json!({"id":"x","is_director":false,"agent_pause":null,"pauseReasons":[]}));
        assert_eq!(minimal.as_object().unwrap().len(),6);
    }
}
