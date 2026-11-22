//! r290 P1: per-project roles (viewer / collaborator) and the owner's
//! token route for a named user, through the production dispatcher on a
//! private instance.
use super::*;
use crate::management::client_tokens::DeviceScope;

const VIEW: &str = "r290-viewer-project";
const COLLAB: &str = "r290-collab-project";
const HIDDEN: &str = "r290-hidden-project";
const S_VIEW: &str = "cutex.r290-view";
const S_COLLAB: &str = "cutex.r290-collab";
const S_HIDDEN: &str = "cutex.r290-hidden";

/// Three projects with one persistent agent each, working in `work`.
fn seed_projects(instance: &PrivateInstance) -> PathBuf {
    let work = instance.root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("result.md"), "# result\n").unwrap();
    let host = crate::platform::host::current_host_name();
    let mut sessions = crate::session::model::CutexSessionStore::default();
    for sid in [S_VIEW, S_COLLAB, S_HIDDEN] {
        let mut record = crate::session::model::CutexSessionRecord::new(sid.into(), Some(format!("thread-{sid}")), host.clone(),
            work.to_string_lossy().into_owned(), None).unwrap();
        record.registration_class = crate::agent_bus::model::AgentRegistrationClass::Persistent;
        sessions.sessions.insert(sid.into(), record);
    }
    crate::session::store::save_cutex_session_store(&sessions).unwrap();
    let roster = crate::agent_management::AgentManagementStore::open_default().unwrap();
    let _fence = roster.lock_mutations().unwrap();
    roster.with_state(true, |mut state| {
        for (project, sid) in [(VIEW, S_VIEW), (COLLAB, S_COLLAB), (HIDDEN, S_HIDDEN)] {
            state.projects.insert(crate::agent_management::ProjectId::new(project.to_string()).unwrap(),
                serde_json::from_value(json!({"project_id": project, "authorized_director_session": sid,
                    "authority_epoch": 1, "updated_at": "2026-11-17T00:00:00Z"})).unwrap());
            let id = crate::role_revision::CutexSessionId::new(sid.to_string()).unwrap();
            state.current_project_memberships.insert(id.clone(), serde_json::from_value(json!({
                "cutex_session_id": sid, "project_id": project, "revision": 1, "updated_at": "2026-11-17T00:00:00Z"})).unwrap());
            state.agents.insert(id, serde_json::from_value(json!({
                "project_id": project, "created_by_director_session": null, "cutex_session_id": sid,
                "native_session_id": format!("native-{sid}"), "created_at": "2026-11-17T00:00:00Z",
                "spec": {"name": sid, "cwd": work, "profile": "fake", "runtime_backend": "codex", "model": "fake",
                    "reasoning": "medium", "permissions": "full-access", "approval_policy": "never",
                    "sandbox_mode": "danger-full-access", "groups": ["default"]}})).unwrap());
        }
        Ok((state, (), true))
    }).unwrap();
    work
}

/// Publish `work/result.md` into `scope` (as the owner would).
fn publish(work: &std::path::Path, scope: crate::artifacts::Scope, name: &str) -> String {
    use crate::artifacts::publish::{publish, PublishRequest};
    let request = PublishRequest {
        scope, path: work.join("result.md").to_string_lossy().into_owned(), base: None,
        roots: vec![work.to_path_buf()], name_bases: vec![work.to_path_buf()], name: Some(name.into()),
        kind: None, note: None,
        producer: crate::artifacts::Producer { kind: "owner".into(), ..Default::default() },
    };
    publish(&crate::artifacts::host::ManagementHost, &request, chrono::Utc::now()).unwrap().artifact.artifact_id
}

fn submit_body(tag: &str) -> Value {
    json!({"requestId": format!("r290-{tag}"), "method": "cutex/userInput/submit", "params": {
        "clientUserMessageId": format!("r290-{tag}"), "origin": {"kind": "other", "clientId": "private-test"},
        "strategy": "auto", "input": [{"type": "text", "text": "hello"}]}})
}

fn ids(value: &Value) -> Vec<String> {
    let mut ids: Vec<String> = value["items"].as_array().map(|items| items.iter()
        .filter_map(|item| item["artifactId"].as_str().map(str::to_string)).collect()).unwrap_or_default();
    ids.sort();
    ids
}

#[test]
fn project_roles_grant_reads_sends_and_artifacts_per_role_and_revoke_at_once() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let instance = PrivateInstance::new();
    let work = seed_projects(&instance);
    let users = UserStore::open_default().unwrap();
    let tokens = ClientTokenStore::open_default().unwrap();
    let ema = users.create_user("Ema", None, Grant::default(), "root").unwrap();
    let device = tokens.issue("ema-laptop").unwrap();
    users.bind_token(&device.token_id, Some(&ema.id), "root").unwrap();
    let owner_chat = tokens.issue_scoped("owner-chat", DeviceScope::Chat).unwrap();
    users.bind_token(&owner_chat.token_id, Some(OWNER), "root").unwrap();
    let role_path = |project: &str| format!("/v2/users/{}/projects/{project}", ema.id);

    // Owner only: a non-owner, and an owner device without the full scope, cannot grant.
    let (status, value) = p2_exchange("PUT", &role_path(VIEW), &device.token, &json!({"role": "viewer"}));
    assert_eq!((status, code(&value)), (403, "owner_required"), "{value}");
    let (status, value) = p2_exchange("PUT", &role_path(VIEW), &owner_chat.token, &json!({"role": "viewer"}));
    assert_eq!((status, code(&value)), (403, "device_scope_denied"), "{value}");
    let (status, value) = p2_exchange("PUT", &role_path(VIEW), ROOT, &json!({"role": "owner"}));
    assert_eq!((status, code(&value)), (400, "invalid_role"), "{value}");
    let (status, value) = p2_exchange("PUT", &format!("/v2/users/{OWNER}/projects/{VIEW}"), ROOT, &json!({"role": "viewer"}));
    assert_eq!((status, code(&value)), (400, "owner_immutable"), "{value}");
    for (project, role) in [(VIEW, "viewer"), (COLLAB, "collaborator")] {
        let (status, value) = p2_exchange("PUT", &role_path(project), ROOT, &json!({"role": role}));
        assert_eq!((status, &value["role"], &value["changed"]), (200, &json!(role), &json!(true)), "{value}");
    }
    let (status, value) = p2_exchange("PUT", &role_path(VIEW), ROOT, &json!({"role": "viewer"}));
    assert_eq!((status, &value["changed"]), (200, &json!(false)), "idempotent: {value}");
    crate::management::v2::schema_export::assert_valid("users", "userProjectRoleResult", &value);
    // Listed on the user.
    let (_, listing) = p2_exchange("GET", "/v2/users", ROOT, &json!({}));
    crate::management::v2::schema_export::assert_valid("users", "userList", &listing);
    let user = listing["users"].as_array().unwrap().iter().find(|u| u["id"] == ema.id.as_str()).unwrap().clone();
    assert_eq!(user["grant"]["projectRoles"], json!({COLLAB: "collaborator", VIEW: "viewer"}), "{user}");
    let (_, caps) = p2_exchange("GET", "/v2/capabilities", &device.token, &json!({}));
    let features = caps["featureList"].as_array().unwrap();
    assert!(features.iter().any(|f| f == "user-project-roles") && features.iter().any(|f| f == "user-token-issue"));

    // Files of the project's agents: both roles read, the ungranted project's agent is hidden.
    for (sid, expected) in [(S_VIEW, 200), (S_COLLAB, 200), (S_HIDDEN, 404)] {
        let (status, value) = p2_exchange("GET", &format!("/v2/sessions/{sid}/fs/file?path=result.md"), &device.token, &json!({}));
        assert_eq!(status, expected, "{sid}: {value}");
    }
    // An ungranted project stays hidden (the granted ones reach the
    // project handler, which this fixture does not serve).
    let (status, value) = p2_exchange("GET", &format!("/v2/agent-management/projects/{HIDDEN}"), &device.token, &json!({}));
    assert_eq!((status, code(&value)), (404, "project_not_found"), "{value}");
    // Send: collaborator yes (the gate lets it through to the fixture
    // runtime), viewer no, ungranted not found.
    let send = |sid: &str, tag: &str| p2_exchange("POST", &format!("/v2/sessions/{sid}/cutex/requests"), &device.token, &submit_body(tag));
    let (status, value) = send(S_VIEW, "view");
    assert_eq!((status, code(&value)), (403, "send_not_granted"), "{value}");
    let (_, value) = send(S_COLLAB, "collab");
    assert_eq!(any_code(&value), "test_user_input_reached", "a collaborator may send: {value}");
    let (status, value) = send(S_HIDDEN, "hidden");
    assert_eq!((status, any_code(&value)), (404, "session_not_found"), "{value}");

    // Artifacts.
    let a_view = publish(&work, crate::artifacts::Scope::project(VIEW), "view.md");
    let a_collab = publish(&work, crate::artifacts::Scope::project(COLLAB), "collab.md");
    let a_hidden = publish(&work, crate::artifacts::Scope::project(HIDDEN), "hidden.md");
    let a_session = publish(&work, crate::artifacts::Scope::session(S_VIEW), "session.md");
    let (status, list) = p2_exchange("GET", "/v2/artifact-store", &device.token, &json!({}));
    assert_eq!(status, 200, "{list}");
    let mut expected = vec![a_view.clone(), a_collab.clone(), a_session.clone()];
    expected.sort();
    assert_eq!(ids(&list), expected, "only visible scopes: {list}");
    let (status, list) = p2_exchange("GET", &format!("/v2/artifact-store?project={VIEW}"), &device.token, &json!({}));
    assert_eq!((status, ids(&list)), (200, vec![a_view.clone()]), "{list}");
    let (status, value) = p2_exchange("GET", &format!("/v2/artifact-store?project={HIDDEN}"), &device.token, &json!({}));
    assert_eq!((status, code(&value)), (404, "project_not_found"), "{value}");
    let (status, value) = p2_exchange("GET", &format!("/v2/artifact-store?forSession={S_HIDDEN}"), &device.token, &json!({}));
    assert_eq!((status, any_code(&value)), (404, "session_not_found"), "{value}");
    for id in [&a_view, &a_collab, &a_session] {
        let (status, value) = p2_exchange("GET", &format!("/v2/artifact-store/{id}"), &device.token, &json!({}));
        assert_eq!(status, 200, "{value}");
        let (status, value) = p2_exchange("GET", &format!("/v2/artifact-store/{id}/versions/latest/content"), &device.token, &json!({}));
        assert_eq!((status, &value["content"]), (200, &json!("# result\n")), "{value}");
        let (status, _, body) = exchange_with(p2_context(), "GET", &format!("/v2/artifact-store/{id}/versions/latest/raw"),
            Some(&device.token), vec![], "application/json", "");
        assert_eq!((status, body.as_slice()), (200, b"# result\n".as_slice()));
    }
    for path in [format!("/v2/artifact-store/{a_hidden}"), format!("/v2/artifact-store/{a_hidden}/versions/latest/content"),
        format!("/v2/artifact-store/{a_hidden}/versions/latest/raw"), format!("/v2/artifact-store/{a_hidden}/diff?from=1&to=1")] {
        let (status, value) = p2_exchange("GET", &path, &device.token, &json!({}));
        assert_eq!((status, code(&value)), (404, "artifact_not_found"), "{path}: {value}");
    }
    // Artifact writes and host-wide routes stay the owner's.
    for (method, path) in [("PATCH", format!("/v2/artifact-store/{a_view}")), ("DELETE", format!("/v2/artifact-store/{a_view}")),
        ("POST", "/v2/artifact-store".to_string()), ("POST", "/v2/artifact-store/resolve".to_string()),
        ("GET", "/v2/artifact-store/usage".to_string())] {
        let (status, value) = p2_exchange(method, &path, &device.token, &json!({}));
        assert_eq!((status, code(&value)), (403, "owner_required"), "{method} {path}: {value}");
    }
    // Cross-review P1: the gate reads the handler's own decoded parameters
    // (a repeated name is refused for everyone), and a requested scope never
    // widens the grant.
    for query in [format!("project={VIEW}&project={HIDDEN}"), format!("project={HIDDEN}&project={VIEW}"),
        format!("session={S_VIEW}&session={S_HIDDEN}"), format!("project={VIEW}&%70roject={HIDDEN}")] {
        let (status, value) = p2_exchange("GET", &format!("/v2/artifact-store?{query}"), &device.token, &json!({}));
        assert_eq!((status, code(&value)), (400, "invalid_request"), "{query}: {value}");
        assert!(!value.to_string().contains(&a_hidden), "{value}");
    }
    let (status, value) = p2_exchange("GET", &format!("/v2/artifact-store?project={VIEW}&project={HIDDEN}"), ROOT, &json!({}));
    assert_eq!((status, code(&value)), (400, "invalid_request"), "one rule for the owner too: {value}");
    let (status, value) = p2_exchange("GET", &format!("/v2/artifact-store?%70roject={HIDDEN}"), &device.token, &json!({}));
    assert_eq!((status, code(&value)), (404, "project_not_found"), "encoded name: {value}");
    let (status, value) = p2_exchange("GET", &format!("/v2/artifact-store?for%53ession={S_HIDDEN}"), &device.token, &json!({}));
    assert_eq!((status, any_code(&value)), (404, "session_not_found"), "encoded name: {value}");
    let (status, list) = p2_exchange("GET", &format!("/v2/artifact-store?%70roject={VIEW}"), &device.token, &json!({}));
    assert_eq!((status, ids(&list)), (200, vec![a_view.clone()]), "{list}");
    let (status, list) = p2_exchange("GET", &format!("/v2/artifact-store?forSession={S_VIEW}"), &device.token, &json!({}));
    assert_eq!((status, ids(&list)), (200, vec![a_view.clone()]), "the agent's project: {list}");
    // The same rule on the other scoped routes' gate decisions.
    for (path, expected) in [
        (format!("/v2/sessions/{S_VIEW}?%6Cifecycle=retired"), (403, "owner_required")),
        (format!("/v2/sessions/{S_VIEW}?lifecycle=active&lifecycle=retired"), (400, "invalid_request")),
        ("/v2/sessions?%69ncludeHidden=true".to_string(), (403, "owner_required")),
        ("/v2/sessions?includeHidden=false&includeHidden=true".to_string(), (400, "invalid_request")),
        ("/v2/asks?%63ursor=x".to_string(), (403, "forbidden")),
    ] {
        let (status, value) = p2_exchange("GET", &path, &device.token, &json!({}));
        assert_eq!((status, code(&value)), expected, "{path}: {value}");
    }

    // The owner keeps every scope.
    let (status, list) = p2_exchange("GET", "/v2/artifact-store", ROOT, &json!({}));
    assert_eq!((status, ids(&list).len()), (200, 4), "{list}");
    let (status, _) = p2_exchange("GET", &format!("/v2/artifact-store/{a_hidden}"), &owner_chat.token, &json!({}));
    assert_eq!(status, 200);

    // A viewer project stays read-only with the grant-wide send too.
    let (status, value) = p2_exchange("PATCH", &format!("/v2/users/{}", ema.id), ROOT, &json!({"grant": {
        "send": true, "projectRoles": {VIEW: "viewer", COLLAB: "collaborator"}}}));
    assert_eq!(status, 200, "{value}");
    let (status, value) = send(S_VIEW, "view-2");
    assert_eq!((status, code(&value)), (403, "send_not_granted"), "{value}");

    // Revocation takes effect on the next request.
    let (status, value) = p2_exchange("DELETE", &role_path(COLLAB), ROOT, &json!({}));
    assert_eq!((status, &value["role"], &value["changed"]), (200, &Value::Null, &json!(true)), "{value}");
    assert!(value["user"]["grant"]["projectRoles"].get(COLLAB).is_none(), "{value}");
    crate::management::v2::schema_export::assert_valid("users", "userProjectRoleResult", &value);
    let (status, value) = send(S_COLLAB, "collab-2");
    assert_eq!((status, any_code(&value)), (404, "session_not_found"), "{value}");
    let (status, value) = p2_exchange("GET", &format!("/v2/sessions/{S_COLLAB}/fs/file?path=result.md"), &device.token, &json!({}));
    assert_eq!(status, 404, "{value}");
    let (status, value) = p2_exchange("GET", &format!("/v2/artifact-store/{a_collab}"), &device.token, &json!({}));
    assert_eq!((status, code(&value)), (404, "artifact_not_found"), "{value}");
    let (_, list) = p2_exchange("GET", "/v2/artifact-store", &device.token, &json!({}));
    let mut expected = vec![a_view, a_session];
    expected.sort();
    assert_eq!(ids(&list), expected, "{list}");
}

#[test]
fn owner_issues_and_revokes_a_named_users_token_over_http() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let users = UserStore::open_default().unwrap();
    let tokens = ClientTokenStore::open_default().unwrap();
    let ema = users.create_user("Ema", None, Grant::default(), "root").unwrap();
    let other = users.create_user("Other", None, Grant::default(), "root").unwrap();
    let owner_chat = tokens.issue_scoped("owner-chat", DeviceScope::Chat).unwrap();
    users.bind_token(&owner_chat.token_id, Some(OWNER), "root").unwrap();
    let path = format!("/v2/users/{}/tokens", ema.id);

    for (bearer, body, expected) in [
        (owner_chat.token.as_str(), json!({"label": "x"}), (403, "device_scope_denied")),
        (ROOT, json!({}), (400, "invalid_label")),
        (ROOT, json!({"label": "  "}), (400, "invalid_label")),
        (ROOT, json!({"label": "x", "scope": "admin"}), (400, "invalid_scope")),
        (ROOT, json!({"label": "x", "extra": 1}), (400, "invalid_request")),
    ] {
        let (status, value) = p2_exchange("POST", &path, bearer, &body);
        assert_eq!(status, expected.0, "{body}: {value}");
        if expected.1 != "invalid_request" { assert_eq!(code(&value), expected.1, "{body}: {value}"); }
    }
    let (status, value) = p2_exchange("POST", &format!("/v2/users/{OWNER}/tokens"), ROOT, &json!({"label": "x"}));
    assert_eq!((status, code(&value)), (400, "owner_token_not_issued"), "{value}");
    let (status, value) = p2_exchange("POST", "/v2/users/u-000000000000/tokens", ROOT, &json!({"label": "x"}));
    assert_eq!((status, code(&value)), (404, "user_not_found"), "{value}");

    let (status, issued) = p2_exchange("POST", &path, ROOT, &json!({"label": "Ema laptop", "scope": "chat"}));
    assert_eq!(status, 201, "{issued}");
    crate::management::v2::schema_export::assert_valid("users", "userTokenIssued", &issued);
    assert_eq!((&issued["userId"], &issued["pending"], &issued["scope"], &issued["label"]),
        (&json!(ema.id), &json!(false), &json!("chat"), &json!("Ema laptop")), "{issued}");
    let secret = issued["token"].as_str().unwrap().to_string();
    let token_id = issued["tokenId"].as_str().unwrap().to_string();
    // The new token acts as Ema (not the owner) and cannot issue tokens itself.
    let (status, caps) = p2_exchange("GET", "/v2/capabilities", &secret, &json!({}));
    assert_eq!((status, &caps["user"]["isOwner"], &caps["user"]["id"]), (200, &json!(false), &json!(ema.id)), "{caps}");
    let (status, value) = p2_exchange("POST", &path, &secret, &json!({"label": "more"}));
    assert_eq!((status, code(&value)), (403, "device_scope_denied"), "{value}");
    let full = tokens.issue("ema-full").unwrap();
    users.bind_token(&full.token_id, Some(&ema.id), "root").unwrap();
    let (status, value) = p2_exchange("POST", &path, &full.token, &json!({"label": "more"}));
    assert_eq!((status, code(&value)), (403, "owner_required"), "a full-scope user device: {value}");
    let (_, listing) = p2_exchange("GET", "/v2/users", ROOT, &json!({}));
    let row = listing["tokens"].as_array().unwrap().iter().find(|t| t["tokenId"] == token_id.as_str()).unwrap().clone();
    assert_eq!((&row["userId"], &row["pending"]), (&json!(ema.id), &json!(false)), "{row}");
    assert!(!listing.to_string().contains(&secret), "the secret is returned only once");

    // Revoke: only under its own user; then the token is refused at once.
    let (status, value) = p2_exchange("DELETE", &format!("/v2/users/{}/tokens/{token_id}", other.id), ROOT, &json!({}));
    assert_eq!((status, code(&value)), (404, "client_token_not_found"), "{value}");
    let (status, value) = p2_exchange("DELETE", &format!("{path}/{token_id}"), &full.token, &json!({}));
    assert_eq!((status, code(&value)), (403, "owner_required"), "{value}");
    let (status, value) = p2_exchange("DELETE", &format!("{path}/{token_id}"), ROOT, &json!({}));
    assert_eq!((status, &value["revoked"]), (200, &json!(true)), "{value}");
    crate::management::v2::schema_export::assert_valid("users", "userTokenRevoked", &value);
    let (status, _) = p2_exchange("GET", "/v2/users/me", &secret, &json!({}));
    assert_eq!(status, 401, "a revoked token is refused on the next request");
    let (status, value) = p2_exchange("DELETE", &format!("{path}/{token_id}"), ROOT, &json!({}));
    assert_eq!((status, code(&value)), (404, "client_token_not_found"), "{value}");
}

/// Move `sid` into `project` (its membership and agent row), as a project
/// move does; the Agent Management store revision changes.
fn move_agent(sid: &str, project: &str) {
    let roster = crate::agent_management::AgentManagementStore::open_default().unwrap();
    let _fence = roster.lock_mutations().unwrap();
    roster.with_state(true, |mut state| {
        let id = crate::role_revision::CutexSessionId::new(sid.to_string()).unwrap();
        let mut membership = serde_json::to_value(&state.current_project_memberships[&id]).unwrap();
        membership["project_id"] = json!(project);
        membership["revision"] = json!(membership["revision"].as_u64().unwrap() + 1);
        state.current_project_memberships.insert(id.clone(), serde_json::from_value(membership).unwrap());
        let mut agent = serde_json::to_value(&state.agents[&id]).unwrap();
        agent["project_id"] = json!(project);
        state.agents.insert(id, serde_json::from_value(agent).unwrap());
        Ok((state, (), true))
    }).unwrap();
}

/// Cross-review P2: an agent moved from a collaborator project into a viewer
/// project is read-only at once, for the cached scope (peer directory, event
/// streams) and for peer delivery, not after the cache expires.
#[test]
fn a_project_move_changes_send_rights_at_once() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let instance = PrivateInstance::new();
    seed_projects(&instance);
    let users = UserStore::open_default().unwrap();
    let tokens = ClientTokenStore::open_default().unwrap();
    let mut grant = Grant::default();
    grant.project_roles.insert(VIEW.into(), crate::management::users::ProjectRole::Viewer);
    grant.project_roles.insert(COLLAB.into(), crate::management::users::ProjectRole::Collaborator);
    let ema = users.create_user("Ema", None, grant.clone(), "root").unwrap();
    let device = tokens.issue("ema-laptop").unwrap();
    users.bind_token(&device.token_id, Some(&ema.id), "root").unwrap();
    let mut peer = crate::peers::tests::peer();
    peer.user_id = ema.id.clone();
    let agent = crate::asks::directory::tests::agent(S_COLLAB, "collab");

    let before = Scope::cached(&grant);
    assert!(before.sees_session(S_COLLAB) && before.may_send(S_COLLAB));
    assert!(crate::peers::policy::visible(&peer, &agent));
    assert!(crate::peers::policy::can_deliver(&peer, &agent), "a collaborator's agent");

    move_agent(S_COLLAB, VIEW);
    let after = Scope::cached(&grant);
    assert!(after.sees_session(S_COLLAB), "still visible in the viewer project");
    assert!(!after.may_send(S_COLLAB), "the cached scope follows the move at once");
    assert!(!crate::peers::policy::can_deliver(&peer, &agent), "no peer send into a viewer project");
    let (status, value) = p2_exchange("POST", &format!("/v2/sessions/{S_COLLAB}/cutex/requests"), &device.token, &submit_body("moved"));
    assert_eq!((status, code(&value)), (403, "send_not_granted"), "{value}");

    move_agent(S_COLLAB, HIDDEN);
    assert!(!Scope::cached(&grant).sees_session(S_COLLAB), "moved out of the grant: hidden at once");
    assert!(!crate::peers::policy::visible(&peer, &agent));
}
