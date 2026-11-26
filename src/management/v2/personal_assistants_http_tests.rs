#[test]
fn personal_assistants_http_owner_scope_creation_recovery_and_revoke() {
    use crate::agent_management::{
        AgentActionId, AgentManagementProvider, ManagedAgentRecord, ManagedAgentSpec, ProjectId,
    };
    use crate::config::instance::{self, Selection};
    use crate::management::client_tokens::{DeviceScope, VerifiedClientToken};
    use crate::management::control_plane::*;
    use crate::management::users::{self, Principal};
    use crate::personal_assistants::{host, Store};
    use crate::role_revision::{CutexSessionId, Rfc3339};
    let _home = crate::config::paths::test_home_env_lock();
    let root = crate::config::paths::home_dir()
        .unwrap()
        .join(format!("pa-http-{}", uuid::Uuid::new_v4().simple()));
    crate::management::client_tokens::ensure_private_dir(&root).unwrap();
    std::fs::write(root.join(crate::config::paths::PRIVATE_HOME_MARKER), b"").unwrap();
    struct Reset(Selection);
    impl Drop for Reset {
        fn drop(&mut self) {
            users::set_current_principal(None);
            crate::management::listeners::set_current_client_token(None);
            instance::set_test_selection(self.0.clone());
        }
    }
    let _reset = Reset(instance::selected().unwrap());
    instance::set_test_selection(Selection::Custom(root.join(".cutex")));
    let mut ctx = safety_http_context();
    ctx.execute_management_project_mutation = |p, req| {
        AgentManagementProvider::open_default()
            .unwrap()
            .execute_project_mutation_for_management(p, req)
    };
    ctx.create_human_agent = |_, req| {
        assert!(!req.start);
        let id = format!("cutex.{}", uuid::Uuid::new_v4());
        let cwd = req.cwd.as_deref().unwrap();
        let sid = CutexSessionId::new(id.clone()).unwrap();
        let mut row = crate::session::creation_identity::with_action(
            &format!("human-agent:{}", req.request_id),
            || {
                crate::session::model::CutexSessionRecord::new(
                    id.clone(),
                    None,
                    crate::platform::host::current_host_name(),
                    cwd.into(),
                    None,
                )
                .unwrap()
            },
        );
        row.profile = req.profile.clone();
        row.agent_enabled = true;
        let mut sessions = crate::session::store::load_cutex_session_store().unwrap();
        sessions.sessions.insert(id.clone(), row);
        crate::session::store::save_cutex_session_store(&sessions).unwrap();
        let member = ManagedAgentRecord {
            project_id: Some(ProjectId::new(req.project_id.as_deref().unwrap()).unwrap()),
            created_by_director_session: None,
            created_by_operator_session: None,
            cutex_session_id: sid.clone(),
            native_session_id: format!("native-{id}"),
            created_at: Rfc3339::new(crate::management::instance::now_utc()).unwrap(),
            retired_at: None,
            spec: ManagedAgentSpec {
                name: req.name.clone(),
                cwd: cwd.into(),
                profile: req.profile.clone(),
                runtime_backend: "app_server".into(),
                model: "fixture".into(),
                reasoning: "medium".into(),
                permissions: "default".into(),
                approval_policy: "never".into(),
                sandbox_mode: "read-only".into(),
                groups: vec![],
                expose_to_im: false,
                pin: false,
                memory_injection: false,
            },
        };
        AgentManagementProvider::open_default()
            .unwrap()
            .store()
            .with_state(true, |mut state| {
                state.agents.insert(sid, member);
                Ok((state, (), true))
            })
            .unwrap();
        Ok(id)
    };
    let project = "fresh-empty-private-project";
    let project_req:HumanManagementProjectMutationRequest=serde_json::from_value(json!({
        "schema":"cutex/human-management-project-mutation/v1","action_id":AgentActionId::new("new-private-project").unwrap(),"project_id":project,
        "operation":{"kind":"create_project","presentation":{"display_name":"Empty private test","badge_label":"T","color":"blue"}}
    })).unwrap();
    (ctx.execute_management_project_mutation)(
        &HumanManagementPrincipal::authenticated(),
        &project_req,
    )
    .unwrap();
    let directory = crate::memory::load_agent_management_directory().unwrap();
    assert!(crate::memory::project_member_session_ids(
        &directory,
        &ProjectId::new(project).unwrap()
    )
    .is_empty());
    let accounts = json!({"version":crate::profiles::model::STORE_VERSION,"active_account_id":null,
        "accounts":[{"id":"fixture-profile","name":"test-profile"}]});
    std::fs::write(
        crate::config::paths::accounts_path().unwrap(),
        serde_json::to_vec(&accounts).unwrap(),
    )
    .unwrap();
    let send_raw = |method: &str, path: &str, bearer: Option<&str>, body: Vec<u8>| {
        let mut request = request(path);
        request.method = method.into();
        request.body = body;
        if let Some(value) = bearer {
            request
                .headers
                .insert("authorization".into(), format!("Bearer {value}"));
        }
        capture_http_response(|stream| {
            handle_v2_request(
                stream,
                &request,
                Some("fixture-agent"),
                Some("fixture-owner"),
                ctx,
            )
        })
    };
    let send = |method: &str, path: &str, bearer: Option<&str>, body: Value| {
        send_raw(method, path, bearer, serde_json::to_vec(&body).unwrap())
    };
    let base = super::super::personal_assistants::PREFIX;
    for token in [None, Some("fixture-agent"), Some("wrong")] {
        assert!(send("GET", base, token, Value::Null).starts_with("HTTP/1.1 403"));
    }
    let owner = Some("fixture-owner");
    users::set_current_principal(Some(Principal::User {
        id: "other".into(),
        name: "Other".into(),
        grant: Default::default(),
    }));
    assert!(send("GET", base, owner, Value::Null).starts_with("HTTP/1.1 403"));
    users::set_current_principal(Some(Principal::Owner));
    let mut read = VerifiedClientToken::test_token("test-read");
    read.scope = DeviceScope::Read;
    crate::management::listeners::set_current_client_token(Some(read));
    assert!(send("POST", base, owner, json!({})).starts_with("HTTP/1.1 403"));
    assert_eq!(
        response_json(&send("GET", base, owner, Value::Null))["items"],
        json!([])
    );
    crate::management::listeners::set_current_client_token(None);
    let repository = crate::management::v2::repository::management_v2_repository().unwrap();
    let checkpoint = repository.checkpoint().unwrap();
    let capabilities = super::super::capabilities::capabilities_resource();
    assert_eq!(
        capabilities["features"]["personalAssistantsQualifiedProjectRefsV1"],
        true
    );
    assert_eq!(
        capabilities["features"]["personalAssistantsCrossHostScopeV1"],
        false
    );
    assert_eq!(
        capabilities["features"]["personalAssistantsOwnerViewsV1"],
        true
    );
    let body = json!({"actionId":"http-first","config":{"name":"Test assistant","profile":"test-profile","homeInstanceId":crate::management::instance::instance_id(),"projectIds":[project]}});
    let mut missing_profile = body.clone();
    missing_profile["config"]["profile"] = json!("missing-fixture-profile");
    assert!(send("POST", base, owner, missing_profile).starts_with("HTTP/1.1 409"));
    assert!(Store::open_default()
        .unwrap()
        .list("", 10)
        .unwrap()
        .is_empty());
    let first = send("POST", base, owner, body.clone());
    assert!(first.starts_with("HTTP/1.1 201"), "{first}");
    assert_eq!(
        response_json(&first),
        response_json(&send("POST", base, owner, body.clone()))
    );
    let row = response_json(&first)["assistant"].clone();
    let id = row["id"].as_str().unwrap();
    let session = row["sessionId"].as_str().unwrap();
    let summary = &response_json(&send("GET", base, owner, Value::Null))["items"][0];
    assert_eq!(summary["id"], id);
    assert_eq!(
        summary["availability"],
        response_json(&first)["availability"]
    );
    assert_eq!(summary["activeProfile"], "test-profile");
    let rejected = send(
        "PATCH",
        &format!("{base}/{id}"),
        owner,
        json!({"actionId":"full-grant",
        "expectedRevision":row["revision"], "change":{"kind":"grants","grants":{"tier":"operational","fullOwner":true}}}),
    );
    assert!(rejected.starts_with("HTTP/1.1 403"), "{rejected}");
    assert_eq!(
        response_json(&rejected)["error"]["code"],
        "full_owner_native_confirmation_required"
    );
    assert_eq!(
        response_json(&send("GET", &format!("{base}/{id}"), owner, Value::Null))["assistant"],
        row
    );
    let record = crate::session::store::shared_session_row(session)
        .unwrap()
        .unwrap();
    assert!(host::dedicated_session(&record));
    assert_eq!(
        Store::open_default()
            .unwrap()
            .for_session(session)
            .unwrap()
            .id,
        id
    );
    assert!(
        host::check_start(&record).is_err(),
        "a runtime without the restricted catalog must stay offline"
    );
    let fresh_director = (ctx.create_human_agent)(
        &HumanManagementPrincipal::authenticated(),
        &serde_json::from_value(json!({
            "requestId":"test-director", "name":"Fixture Director", "profile":"test-profile",
            "projectId":project, "cwd":root, "start":false
        }))
        .unwrap(),
    )
    .unwrap();
    let seat: HumanManagementProjectMutationRequest = serde_json::from_value(json!({
        "schema":"cutex/human-management-project-mutation/v1", "action_id":"seat-fixture-director",
        "project_id":project, "expected_authority_epoch":0, "expected_project_revision":1,
        "operation":{"kind":"assign_director","cutex_session_id":fresh_director}
    }))
    .unwrap();
    (ctx.execute_management_project_mutation)(&HumanManagementPrincipal::authenticated(), &seat)
        .unwrap();
    let store = Store::open_default().unwrap();
    let tool = |caller: &str, name: &str, args: Value| {
        crate::personal_assistants::tools::call(&store, caller, name, &args).unwrap()
    };
    let notes_path = format!("{base}/{id}/notes?q=meeting&limit=1");
    for key in ["alpha", "bravo"] {
        tool(
            session,
            "pa_memory_write",
            json!({"actionId":key, "expectedRevision":0,
            "key":key, "content":"private meeting", "sourceRef":"fixture-note-source"}),
        );
    }
    assert!(
        send("GET", &notes_path, Some("fixture-agent"), Value::Null).starts_with("HTTP/1.1 403")
    );
    let first_note = response_json(&send("GET", &notes_path, owner, Value::Null));
    assert_eq!(first_note["items"][0]["key"], "alpha");
    assert_eq!(first_note["nextCursor"], "alpha");
    let next_note = response_json(&send(
        "GET",
        &format!("{notes_path}&cursor=alpha"),
        owner,
        Value::Null,
    ));
    assert_eq!(next_note["items"][0]["sourceRef"], "fixture-note-source");
    assert_eq!(next_note["items"][0]["key"], "bravo");
    assert!(next_note["nextCursor"].is_null());
    let draft = tool(
        session,
        "pa_request_prepare",
        json!({
            "actionId":"draft", "projectRef":{"instanceId":crate::management::instance::instance_id(),"projectId":project}, "outcome":"Prepare a fixture summary", "sourceRef":"fixture-turn"
        }),
    );
    assert_eq!(draft["projectRef"]["projectId"], project);
    assert_eq!(
        row["coordinationProjectRef"]["instanceId"],
        crate::management::instance::instance_id()
    );
    assert_eq!(
        row["coordinationProjectRef"]["projectId"],
        row["coordinationProjectId"]
    );
    let foreign = crate::personal_assistants::tools::call(
        &store,
        session,
        "pa_project_read",
        &json!({"projectRef":{"instanceId":"other-host","projectId":project}}),
    )
    .unwrap_err();
    assert_eq!(
        crate::personal_assistants::error_status(&foreign).1,
        "destination_scope_required"
    );
    let request_path = format!("{base}/{id}/requests/{}", draft["id"].as_str().unwrap());
    let detail = response_json(&send("GET", &request_path, owner, Value::Null));
    assert_eq!(detail["currentDirector"]["sessionId"], fresh_director);
    assert_eq!(
        tool(&fresh_director, "director_request_list", json!({}))["items"],
        json!([])
    );
    let confirm = json!({"actionId":"submit", "expectedRevision":draft["revision"],
        "expectedAssistantRevision":detail["currentAssistantRevision"], "director":detail["currentDirector"]});
    let path = format!("{request_path}/confirm");
    let receipt = response_json(&send("POST", &path, owner, confirm.clone()));
    assert_eq!(receipt["request"]["state"], "queued");
    assert_eq!(receipt, response_json(&send("POST", &path, owner, confirm)));
    assert_eq!(
        tool(&fresh_director, "director_request_list", json!({}))["items"][0]["outcome"],
        draft["outcome"]
    );
    let mut waiting = Vec::new();
    for i in 0..50 {
        waiting.push(tool(session,"pa_request_prepare",json!({
            "actionId":format!("wait-{i}"),"projectId":project,"outcome":"Fixture draft","sourceRef":"fixture"
        })));
    }
    let listed = tool(&fresh_director, "director_request_list", json!({}));
    let reason = &listed["items"][0]["clarifyUnavailable"];
    assert_eq!(reason["code"], "open_request_limit");
    let refused = crate::personal_assistants::tools::call(&store,&fresh_director,"director_request_respond",&json!({
        "actionId":"clarify-full","requestId":draft["id"],"expectedRevision":2,"response":"clarify","detail":"More detail?"
    })).unwrap_err();
    let (status, code) = crate::personal_assistants::error_status(&refused);
    assert_eq!((status, code), (409, "open_request_limit"));
    assert_eq!(
        reason["message"],
        crate::personal_assistants::error_message(code)
    );
    let release = format!(
        "{base}/{id}/requests/{}/cancel",
        waiting[0]["id"].as_str().unwrap()
    );
    assert_eq!(
        response_json(&send(
            "POST",
            &release,
            owner,
            json!({
                "actionId":"free-slot","expectedRevision":1
            })
        ))["request"]["state"],
        "cancelled"
    );
    let listed = tool(&fresh_director, "director_request_list", json!({}));
    assert!(listed["items"][0].get("clarifyUnavailable").is_none());
    let accepted = tool(
        &fresh_director,
        "director_request_respond",
        json!({
            "actionId":"accept", "requestId":draft["id"], "expectedRevision":2, "response":"accept"
        }),
    );
    assert_eq!(accepted["state"], "accepted");
    assert_eq!(
        response_json(&send("GET", &request_path, owner, Value::Null))["request"]["state"],
        "accepted"
    );
    let templates = format!("{base}/{id}/templates");
    let reference =
        json!({"instanceId":crate::management::instance::instance_id(),"projectId":project});
    let project_list = format!(
        "{base}/requests?instanceId={}&projectId={project}",
        reference["instanceId"].as_str().unwrap()
    );
    let epoch = detail["currentDirector"]["epoch"].as_u64().unwrap();
    let all = format!(
        "{project_list}&directorSessionId={fresh_director}&directorEpoch={epoch}&state=accepted"
    );
    assert!(send("GET", &all, Some("fixture-agent"), Value::Null).starts_with("HTTP/1.1 403"));
    let only = response_json(&send("GET", &all, owner, Value::Null));
    assert_eq!(only["items"].as_array().unwrap().len(), 1);
    assert_eq!(only["items"][0]["id"], draft["id"]);
    assert!(only["nextCursor"].is_null());
    let get = |path: &str| response_json(&send("GET", path, owner, Value::Null));
    assert_eq!(
        get(&format!(
            "{base}/requests?directorSessionId={fresh_director}&directorEpoch=0"
        ))["items"],
        json!([])
    );
    assert_eq!(
        get(&format!("{base}/requests?projectId={project}"))["error"]["code"],
        "invalid_project_ref"
    );
    assert_eq!(
        get(&format!(
            "{base}/requests?instanceId=other-instance&projectId={project}"
        ))["error"]["code"],
        "destination_scope_required"
    );
    assert_eq!(
        get(&format!("{base}/{id}/requests?state=unknown"))["error"]["code"],
        "invalid_request_state"
    );
    let newest = format!("{base}/{id}/requests?state=awaitingOwner&limit=1");
    let page = get(&newest);
    assert_eq!(page["items"][0]["id"], waiting[49]["id"]);
    let second = get(&format!(
        "{newest}&cursor={}",
        page["nextCursor"].as_i64().unwrap()
    ));
    assert_eq!(second["items"][0]["id"], waiting[48]["id"]);
    assert_eq!(row["config"]["projectRefs"], json!([reference]));
    let changes_after = |checkpoint: crate::management::v2::model::EventCheckpoint| {
        repository
            .page(crate::management::v2::repository::ReplayQuery {
                stream_id: Some(checkpoint.stream_id),
                after: checkpoint.cursor,
                limit: 1000,
                cutex_session_id: Some(
                    crate::management::v2::services::HOST_EVENT_SESSION_ID.into(),
                ),
            })
            .unwrap()
            .events
            .into_iter()
            .filter_map(|e| e.cutex)
            .filter(|m| {
                m.method == crate::personal_assistants::CHANGED_EVENT
                    && m.params["assistantId"] == id
            })
            .map(|m| m.params)
            .collect::<Vec<_>>()
    };
    let write_template = |method: &str, path: &str, body: Value| {
        let before = repository.checkpoint().unwrap();
        let response = send(method, path, owner, body.clone());
        assert_eq!(
            changes_after(before),
            [json!({"assistantId":id,"areas":["templates"]})],
            "{method} {path}: {response}"
        );
        let before_replay = repository.checkpoint().unwrap();
        let replay = send(method, path, owner, body);
        assert_eq!(response_json(&response), response_json(&replay));
        assert!(
            changes_after(before_replay).is_empty(),
            "replayed {method} {path}"
        );
        response
    };
    let confirmed = json!({"actionId":"owner-recurring","expectedAssistantRevision":row["revision"],
        "projectRef":reference,"confirmedSeat":detail["currentDirector"],"task":"projectStatus","rule":{"kind":"interval","hours":1}});
    let before_denied = repository.checkpoint().unwrap();
    let denied = send("POST", &templates, Some("fixture-agent"), confirmed.clone());
    assert!(denied.starts_with("HTTP/1.1 403"), "{denied}");
    let mut elsewhere = confirmed.clone();
    elsewhere["projectRef"]["instanceId"] = json!("other-instance");
    assert_eq!(
        response_json(&send("POST", &templates, owner, elsewhere))["error"]["code"],
        "destination_scope_required"
    );
    assert!(changes_after(before_denied).is_empty());
    let made = write_template("POST", &templates, confirmed.clone());
    assert!(made.starts_with("HTTP/1.1 201"), "{made}");
    let schedule = response_json(&made)["schedule"].clone();
    assert_eq!(schedule["assistantTemplate"]["projectRef"], reference);
    let key = schedule["id"].as_str().unwrap();
    let path = format!("{templates}/{key}");
    let db = crate::schedules::Store::open_default().unwrap();
    let due: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(schedule["nextRunAt"].clone()).unwrap();
    let _worker = db.worker_lock().unwrap();
    assert_eq!(
        crate::schedules::runtime::tick_host(&db, due, |_, _| {
            panic!("submitting a confirmed request started an agent")
        })
        .unwrap(),
        1
    );
    let run = db.runs(key, None, 1).unwrap().data.pop().unwrap();
    assert_eq!(run.outcome, "handedOff", "{:?}", run.last_error);
    let r = run.request_id.clone().unwrap();
    assert_eq!(
        response_json(&send("GET", &format!("{path}/runs"), owner, Value::Null))["items"][0]
            ["requestId"],
        r
    );
    let ticket = response_json(&send(
        "GET",
        &format!("{base}/{id}/requests/{r}"),
        owner,
        Value::Null,
    ));
    assert_eq!(ticket["request"]["occurrence"]["occurrenceKey"], run.id);
    assert_eq!(ticket["request"]["projectRef"], reference);
    let paused = response_json(&write_template(
        "POST",
        &format!("{path}/pause"),
        json!({"actionId":"pause-rule","expectedRevision":1}),
    ));
    assert_eq!(paused["schedule"]["enabled"], false);
    let before_stale = repository.checkpoint().unwrap();
    let stale = send(
        "POST",
        &format!("{path}/cancel"),
        owner,
        json!({"actionId":"stale-end-rule","expectedRevision":1}),
    );
    assert!(stale.starts_with("HTTP/1.1 409"), "{stale}");
    assert!(changes_after(before_stale).is_empty());
    let mut renewed = confirmed.clone();
    renewed["actionId"] = json!("new-review");
    renewed["expectedRevision"] = json!(2);
    renewed["task"] = json!("dueRequests");
    let changed = response_json(&write_template("PUT", &path, renewed));
    assert_eq!(
        changed["schedule"]["assistantTemplate"]["templateRevision"],
        3
    );
    let stopped = response_json(&write_template(
        "POST",
        &format!("{path}/cancel"),
        json!({"actionId":"end-rule","expectedRevision":3}),
    ));
    assert!(stopped["schedule"]["deletedAt"].is_string());
    let audit = response_json(&send("GET", &format!("{path}/audit"), owner, Value::Null));
    assert_eq!(audit["items"].as_array().unwrap().len(), 4);
    assert_eq!(audit["items"][0]["actor"], "owner");
    let calendar_path = format!("{base}/{id}/calendar");
    let snap = json!({"actionId":"import-calendar","expectedRevision":0,"snapshot":{"source":"Fixture",
        "observedAt":"2026-11-20T00:00:00Z","events":[{"id":"private","title":"Private event","startsAt":"2026-11-20T11:00:00Z","endsAt":"2026-11-20T12:00:00Z"}]}});
    assert_eq!(
        response_json(&send("PUT", &calendar_path, owner, snap))["status"],
        "ready"
    );
    assert_eq!(
        response_json(&send(
            "DELETE",
            &calendar_path,
            owner,
            json!({"actionId":"remove-calendar","expectedRevision":1})
        ))["status"],
        "disconnected"
    );
    let events: Vec<_> = (0..1000)
        .map(|i| {
            json!({"id":format!("event-{i}"),"title":"会".repeat(160),
        "startsAt":"2026-11-25T10:00:00Z","endsAt":"2026-11-25T10:30:00Z"})
        })
        .collect();
    let mut full = serde_json::to_vec(&json!({"actionId":"calendar-full","expectedRevision":2,
        "snapshot":{"source":"owner","observedAt":"2026-11-25T09:00:00Z","events":events}}))
    .unwrap();
    let cap = super::super::personal_assistants::CALENDAR_IMPORT_MAX_BYTES;
    assert!(full.len() > 512 * 1024 && full.len() < cap);
    full.resize(cap, b' ');
    let imported = send_raw("PUT", &calendar_path, owner, full.clone());
    assert!(imported.starts_with("HTTP/1.1 200"), "{imported}");
    full.push(b' ');
    assert_eq!(
        response_json(&send_raw("PUT", &calendar_path, owner, full))["error"]["code"],
        "body_too_large"
    );
    assert_eq!(
        response_json(&send_raw(
            "PATCH",
            &format!("{base}/{id}"),
            owner,
            vec![b' '; 65537]
        ))["error"]["code"],
        "body_too_large"
    );
    let stale = send(
        "PATCH",
        &format!("{base}/{id}"),
        owner,
        json!({"actionId":"revoke","expectedRevision":1,"change":{"kind":"enabled","enabled":false}}),
    );
    assert!(stale.starts_with("HTTP/1.1 409"), "{stale}");
    let ok = send(
        "PATCH",
        &format!("{base}/{id}"),
        owner,
        json!({"actionId":"revoke","expectedRevision":2,"change":{"kind":"enabled","enabled":false}}),
    );
    assert_eq!(response_json(&ok)["assistant"]["enabled"], false);
    assert_eq!(get(&notes_path)["items"][0]["content"], "private meeting");
    let events = changes_after(checkpoint);
    let count = |area: &str| {
        events
            .iter()
            .filter(|e| e["areas"] == json!([area, "audit"]))
            .count()
    };
    assert_eq!(count("assistant"), 3); // create, bind, disable. Replays and refusals are silent.
    assert_eq!(count("notes"), 2);
    assert_eq!(count("calendar"), 3);
    assert!(events
        .iter()
        .any(|e| e["requestId"] == r && e["areas"] == json!(["requests", "audit"])));
    assert!(events.iter().all(|e| e
        .as_object()
        .unwrap()
        .keys()
        .all(|k| matches!(k.as_str(), "assistantId" | "areas" | "requestId"))));
    let assistant_path = format!("{base}/{id}");
    let patch = |action: &str, change: Value| {
        let current = get(&assistant_path)["assistant"]["revision"].clone();
        let reply = send(
            "PATCH",
            &assistant_path,
            owner,
            json!({"actionId":action,"expectedRevision":current,"change":change}),
        );
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        response_json(&reply)["assistant"]["revision"].clone()
    };
    let enabled = patch(
        "enable-for-review",
        json!({"kind":"enabled","enabled":true}),
    );
    let mut review = confirmed;
    review["actionId"] = json!("review-notice-rule");
    review["expectedAssistantRevision"] = enabled;
    let made = response_json(&send("POST", &templates, owner, review));
    let rule_id = made["schedule"]["id"].as_str().unwrap();
    let due: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(made["schedule"]["nextRunAt"].clone()).unwrap();
    patch(
        "remove-reviewed-project",
        json!({"kind":"scopedSettings","name":"Review","projectRefs":[]}),
    );
    patch(
        "restore-project",
        json!({"kind":"scopedSettings","name":"Review","projectRefs":[reference]}),
    );
    for at in [due, due + chrono::Duration::hours(1)] {
        crate::schedules::runtime::tick_host(&db, at, |_, _| panic!("review started an agent"))
            .unwrap();
    }
    let warnings = get(&format!("{templates}/{rule_id}/audit"));
    assert_eq!(
        warnings["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["verb"] == "reconfirmationRequired")
            .count(),
        1
    );
    let notifications = crate::owner_notify::store::OwnerNotifyStore::open_default()
        .unwrap()
        .snapshot()
        .unwrap();
    let reviews: Vec<_> = notifications
        .notifications
        .values()
        .filter(|r| {
            r.template_review
                .as_ref()
                .is_some_and(|t| t.schedule_id == rule_id)
        })
        .collect();
    assert_eq!(reviews.len(), 1);
    let notice = serde_json::to_value(reviews[0]).unwrap();
    assert_eq!(
        notice["templateReview"],
        json!({"assistantId":id,"scheduleId":rule_id,"templateRevision":1,"reason":"confirmation_stale"})
    );
    assert_eq!(notice["systemKind"], "assistantTemplateNeedsReview");
    assert!(reviews[0].pending());
}
