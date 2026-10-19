use super::*;
use crate::management::users::{Grant, Projects, Scope};

const METHODS: [&str; 5] = ["GET", "POST", "PUT", "PATCH", "DELETE"];

/// Every `"/v2…"` string literal in the crate's sources (routes, prefixes,
/// test paths). Placeholders become `x`; a trailing `/` gets an `x` segment.
fn v2_literals() -> Vec<String> {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs")
                // This module's own tests name unknown routes on purpose.
                && !path.file_name().is_some_and(|name| name.to_string_lossy().starts_with("access_"))
            {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
    let mut literals = std::collections::BTreeSet::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        let mut rest = text.as_str();
        while let Some(start) = rest.find("\"/v2") {
            let tail = &rest[start + 1..];
            let end = tail
                .find(|c: char| !(c.is_ascii_alphanumeric() || "/_.{}-%".contains(c)))
                .unwrap_or(tail.len());
            let mut literal = tail[..end].to_string();
            rest = &tail[end..];
            while let (Some(open), Some(close)) = (literal.find('{'), literal.find('}')) {
                if close < open {
                    break;
                }
                literal.replace_range(open..=close, "x");
            }
            if literal.ends_with('/') {
                literal.push('x');
            }
            // `"/v2/"` alone is the generic prefix check, not a route.
            if literal == "/v2" || (literal.starts_with("/v2/") && literal != "/v2/x") {
                literals.insert(literal);
            }
        }
    }
    literals.into_iter().collect()
}

#[test]
fn every_v2_route_literal_has_an_explicit_class_for_every_method() {
    let literals = v2_literals();
    assert!(literals.len() > 200, "the source scan found too few routes: {}", literals.len());
    let mut missing = Vec::new();
    for literal in &literals {
        for method in METHODS {
            if classify(method, literal).is_none() {
                missing.push(format!("{method} {literal}"));
            }
        }
    }
    assert!(missing.is_empty(), "unclassified routes (add a rule in access.rs): {missing:#?}");
}

#[test]
fn the_non_owner_surface_is_exactly_the_reviewed_table() {
    use Scoped::*;
    let expected: &[(&str, &str, Class)] = &[
        ("POST", "/v2/pair", Class::Public),
        ("GET", "/v2/capabilities", Class::HostRead),
        ("GET", "/v2/host", Class::HostRead),
        ("GET", "/v2/identity/challenge", Class::AnyPrincipal),
        ("POST", "/v2/identity/claim", Class::AnyPrincipal),
        ("GET", "/v2/sessions", Class::Scoped(SessionList)),
        ("GET", "/v2/sessions/s1", Class::Scoped(SessionRead)),
        ("GET", "/v2/sessions/s1/bootstrap", Class::Scoped(SessionBootstrap)),
        ("GET", "/v2/sessions/s1/history", Class::Scoped(SessionHistory)),
        ("GET", "/v2/sessions/s1/history/turns/t1", Class::Scoped(SessionHistoryTurn)),
        ("POST", "/v2/sessions/s1/app-server/requests", Class::Scoped(NativeRequest)),
        ("POST", "/v2/sessions/s1/app-server/server-request-responses", Class::Scoped(ServerResponse)),
        ("POST", "/v2/sessions/s1/cutex/requests", Class::Scoped(CutexRequest)),
        ("GET", "/v2/events", Class::Scoped(EventPage)),
        ("GET", "/v2/events/stream", Class::Scoped(EventStream)),
        ("GET", "/v2/summary", Class::Scoped(Summary)),
        ("GET", "/v2/summary/events", Class::Scoped(SummaryEvents)),
        ("GET", "/v2/agent-management/projects", Class::Scoped(ProjectList)),
        ("GET", "/v2/agent-management/projects/p1", Class::Scoped(ProjectRead)),
        ("GET", "/v2/asks", Class::Scoped(AskList)),
        ("GET", "/v2/asks/ask-abc", Class::Scoped(AskRead)),
        ("GET", "/v2/asks/ask-abc/answer", Class::Scoped(AskAnswer)),
        ("GET", "/v2/users/me", Class::Scoped(SelfProfile)),
        ("PATCH", "/v2/users/me", Class::Scoped(SelfProfile)),
        ("GET", "/v2/users/owner/avatar", Class::Scoped(UserAvatar)),
        ("GET", "/v2/users/u-0123456789ab/avatar", Class::Scoped(UserAvatar)),
        ("GET", "/v2/users/me/home", Class::Scoped(UserHome)),
        ("PUT", "/v2/users/me/home", Class::Scoped(UserHome)),
        ("GET", "/v2/users/me/home/cover", Class::Scoped(UserHome)),
        ("PUT", "/v2/users/me/home/cover", Class::Scoped(UserHome)),
        ("DELETE", "/v2/users/me/home/cover", Class::Scoped(UserHome)),
        ("GET", "/v2/users/me/devices", Class::AnyPrincipal),
        ("DELETE", "/v2/users/me/devices/mct-0123abcd", Class::AnyPrincipal),
        ("GET", "/v2/projects/p1/fs/dir", Class::Scoped(WorkspaceRead)),
        ("GET", "/v2/projects/p1/fs/file", Class::Scoped(WorkspaceRead)),
        ("GET", "/v2/projects/p1/fs/search", Class::Scoped(WorkspaceRead)),
        ("GET", "/v2/projects/p1/fs/watch", Class::Scoped(WorkspaceRead)),
        ("GET", "/v2/projects/p1/git/status", Class::Scoped(WorkspaceRead)),
        ("GET", "/v2/projects/p1/git/diff", Class::Scoped(WorkspaceRead)),
        ("GET", "/v2/sessions/s1/fs/dir", Class::Scoped(WorkspaceRead)),
        ("GET", "/v2/sessions/s1/fs/file", Class::Scoped(WorkspaceRead)),
        ("GET", "/v2/sessions/s1/git/diff", Class::Scoped(WorkspaceRead)),
        ("POST", "/v2/sessions/s1/attachments", Class::Scoped(AttachmentUpload)),
        ("POST", "/v2/clients/presence", Class::AnyPrincipal),
        ("GET", "/v2/clients", Class::AnyPrincipal),
        ("POST", "/v2/clients/inst-1/close", Class::AnyPrincipal),
        ("POST", "/v2/clients/inst-1/closing", Class::AnyPrincipal),
    ];
    for (method, path, class) in expected {
        assert_eq!(classify(method, path), Some(*class), "{method} {path}");
    }
    // Same paths with other methods, and neighbouring routes: the owner's.
    let owner_only = [
        ("POST", "/v2/sessions"),
        ("DELETE", "/v2/sessions/s1"),
        ("GET", "/v2/sessions/s1/app-server/requests"),
        ("POST", "/v2/summary/read"),
        ("POST", "/v2/summary"),
        ("POST", "/v2/capabilities"),
        ("GET", "/v2/asks/provenance"),
        ("GET", "/v2/artifacts/art-1"),
        ("POST", "/v2/pairing-codes"),
        ("GET", "/v2/pairing-codes"),
        ("GET", "/v2/users"),
        ("PUT", "/v2/users/token-bindings/mct-1"),
        ("POST", "/v2/users/me"),
        ("PUT", "/v2/users/me"),
        ("DELETE", "/v2/users/me"),
        ("POST", "/v2/users/me/devices"),
        ("DELETE", "/v2/users/me/devices"),
        ("GET", "/v2/users/me/devices/mct-0123abcd"),
        ("PATCH", "/v2/users/me/devices/mct-0123abcd"),
        ("DELETE", "/v2/users/me/devices/not-a-token"),
        ("DELETE", "/v2/users/me/devices/mct-1/extra"),
        ("POST", "/v2/users/me/home"),
        ("PATCH", "/v2/users/me/home"),
        ("DELETE", "/v2/users/me/home"),
        ("POST", "/v2/users/me/home/cover"),
        ("PATCH", "/v2/users/me/home/cover"),
        ("GET", "/v2/users/me/home/other"),
        ("GET", "/v2/users/owner/home"),
        ("PUT", "/v2/users/u-0123456789ab/home"),
        ("PATCH", "/v2/users/owner"),
        ("PATCH", "/v2/users/u-0123456789ab/avatar"),
        ("DELETE", "/v2/users/u-0123456789ab/avatar"),
        ("GET", "/v2/users/not-a-user/avatar"),
        ("GET", "/v2/users/settings"),
        ("PUT", "/v2/users/owner/person-keys"),
        ("PUT", "/v2/users/u-0123456789ab/person-keys"),
        ("POST", "/v2/identity/challenge"),
        ("GET", "/v2/identity/claim"),
        ("DELETE", "/v2/identity/claim"),
        ("GET", "/v2/client-tokens"),
        ("GET", "/v2/settings"),
        ("GET", "/v2/profiles"),
        ("GET", "/v2/claude/accounts"),
        ("GET", "/v2/peers"),
        ("POST", "/v2/peers/invites"),
        ("GET", "/v2/fs/file"),
        ("GET", "/v2/fs/dirs"),
        ("POST", "/v2/projects/p1/fs/dir"),
        ("PUT", "/v2/projects/p1/fs/file"),
        ("DELETE", "/v2/sessions/s1/fs/file"),
        ("POST", "/v2/sessions/s1/git/status"),
        ("GET", "/v2/projects/p1/terminals"),
        ("POST", "/v2/projects/p1/terminals"),
        ("GET", "/v2/sessions/s1/terminals"),
        ("POST", "/v2/sessions/s1/terminals"),
        ("GET", "/v2/projects/p1/fs/other"),
        ("GET", "/v2/projects/p1/memory"),
        ("GET", "/v2/sessions/s1/attachments"),
        ("DELETE", "/v2/sessions/s1/attachments"),
        ("PUT", "/v2/users/token-persons/mct-1/previous"),
        ("DELETE", "/v2/users/token-persons/mct-1/previous"),
        ("GET", "/v2/sessions/s1/harness"),
        ("GET", "/v2/sessions/s1/claude-runtime"),
        ("POST", "/v2/sessions/s1/claude-runtime/repin"),
        ("GET", "/v2/sessions/s1/mcp"),
        ("GET", "/v2/sessions/s1/side-chats"),
        ("POST", "/v2/terminals"),
        ("GET", "/v2/terminals/t-1/stream"),
        ("GET", "/v2/approvals"),
        ("POST", "/v2/approvals/apr-1/approve"),
        ("GET", "/v2/alarms"),
        ("GET", "/v2/owner-notifications"),
        ("GET", "/v2/services"),
        ("POST", "/v2/services/web/view-ticket"),
        ("GET", "/v2/browser/agents"),
        ("GET", "/v2/usage"),
        ("GET", "/v2/runtimes"),
        ("GET", "/v2/runtime-packages"),
        ("POST", "/v2/human/agents"),
        ("POST", "/v2/host/sessions"),
        ("POST", "/v2/agent-management/project-mutations"),
        ("POST", "/v2/agent-management/explicit-launch"),
        ("GET", "/v2/agent-management/durable-candidates"),
        ("GET", "/v2/notes"),
        ("GET", "/v2/artifact-store"),
        ("GET", "/v2/health"),
        ("GET", "/v2"),
        ("GET", "/v2/clients/presence"),
        ("POST", "/v2/clients"),
        ("DELETE", "/v2/clients"),
        ("GET", "/v2/clients/inst-1"),
        ("DELETE", "/v2/clients/inst-1"),
        ("GET", "/v2/clients/inst-1/close"),
        ("PATCH", "/v2/clients/inst-1/closing"),
        ("POST", "/v2/clients/inst-1/other"),
        ("POST", "/v2/clients/presence/close"),
        ("POST", "/v2/clients/bad id/close"),
    ];
    for (method, path) in owner_only {
        assert_eq!(classify(method, path), Some(Class::OwnerOnly), "{method} {path}");
    }
    // Default deny for anything unknown.
    assert_eq!(classify("GET", "/v2/brand-new-route"), None);
}

#[test]
fn cutex_and_native_method_tables_are_reviewed_snapshots() {
    let registry = super::super::session::CUTEX_METHOD_REGISTRY_METHODS;
    for method in CUTEX_READ.iter().chain(CUTEX_SEND).chain(CUTEX_ASK) {
        assert!(registry.contains(method), "{method} is not a registered cutex method");
    }
    let owner: Vec<&str> = registry.iter().copied().filter(|m| cutex_need(m) == Need::Owner).collect();
    // A new cutex method lands here first; move it to CUTEX_READ / CUTEX_SEND
    // only after deciding what a non-owner may do with it.
    assert_eq!(
        owner,
        [
            "cutex/session/retire",
            "cutex/session/restore",
            "cutex/session/defaults/update",
            "cutex/session/profile/set",
            "cutex/session/profile/clear",
            "cutex/session/metadata/update",
            "cutex/session/profile/switch",
            "cutex/session/history/revert",
            "cutex/session/files/revert",
            "cutex/session/delete",
            "cutex/session/fork",
            "cutex/session/groups/set",
            "cutex/session/groups/add",
            "cutex/session/groups/remove",
            "cutex/session/visibility/show",
            "cutex/session/visibility/hide",
            "cutex/runtime/online",
            "cutex/runtime/offline",
            "cutex/runtime/close",
            "cutex/focus/set",
            "cutex/focus/clear",
            "cutex/userInput/queue/pause",
            "cutex/userInput/queue/resume",
            "cutex/ask/create",
            "cutex/agentBus/wake",
            "cutex/asks/pause",
            "cutex/asks/resume",
            "cutex/ownerNotify/ack",
            "cutex/ownerNotify/reply",
            "cutex/ownerNotify/dismiss",
            "cutex/ownerNotify/delete",
            "cutex/safetyCheck/grant",
            "cutex/safetyCheck/dismiss",
        ]
    );
    assert_eq!(native_need("turn/start"), Need::Owner);
    assert_eq!(native_need("turn/steer"), Need::Owner);
    assert_eq!(native_need("thread/settings/update"), Need::Owner);
    assert_eq!(native_need("model/list"), Need::Owner);
    assert_eq!(native_need("thread/read"), Need::Read);
    assert_eq!(native_need("turn/interrupt"), Need::Send);
    // Windows sandbox readiness / unelevated setup: the owner's only.
    assert_eq!(native_need("windowsSandbox/readiness"), Need::Owner);
    assert_eq!(native_need("windowsSandbox/setupStart"), Need::Owner);
    assert_eq!(native_need(""), Need::Owner);
    // Native r53 additions stay the owner's (and the gateway denies them).
    for method in crate::management::v2::native_requests::NATIVE_R53_UNFORWARDED_METHODS {
        assert_eq!(native_need(method), Need::Owner, "{method}");
    }
    assert_eq!(cutex_need(""), Need::Owner);
    for method in CUTEX_ASK {
        assert_eq!(cutex_need(method), Need::Ask, "{method}");
        assert!(super::super::asks::is_command(method), "{method}");
    }
}

#[test]
fn non_owner_input_is_text_and_own_attachments_only() {
    let own = |path: &str| path == "/att/s1/own.png";
    assert_eq!(check_input(&json!({"input": [{"type": "text", "text": "hi"}]}), own), Ok(()));
    assert_eq!(check_input(&json!({"queueId": "q"}), own), Ok(()));
    assert_eq!(
        check_input(&json!({"input": [{"type": "text", "text": "a"}, {"type": "localImage", "path": "/att/s1/own.png"}]}), own),
        Ok(())
    );
    assert_eq!(check_input(&json!({"input": [{"type": "localImage", "path": "/etc/shadow"}]}), own), Err(InputRefusal::NotOwnAttachment));
    assert_eq!(check_input(&json!({"input": [{"type": "localImage"}]}), own), Err(InputRefusal::NotOwnAttachment));
    assert_eq!(check_input(&json!({"input": [{"type": "text", "text": "a"}, {"type": "image", "url": "x"}]}), own), Err(InputRefusal::Type));
    assert_eq!(check_input(&json!({"input": [{"type": "skill", "name": "x", "path": "/att/s1/own.png"}]}), own), Err(InputRefusal::Type));
    assert_eq!(check_input(&json!({"input": [{"type": "mention", "name": "x", "path": "/etc"}]}), own), Err(InputRefusal::Type));
    assert_eq!(check_input(&json!({"input": "text"}), own), Err(InputRefusal::Type));
}

#[test]
fn delegation_commands_need_a_visible_related_ask() {
    use crate::asks::model::Requester;
    let mut state = crate::asks::store::AskState::default();
    let (template, _) = state
        .create_ask(crate::asks::store::tests::new_ask("template", "hello"), chrono::Utc::now())
        .unwrap();
    state.asks.clear();
    let ask = |id: &str, requester: Requester, target: &str| {
        let mut record = template.clone();
        record.ask_id = id.into();
        record.requester = requester;
        record.target_cutex_session_id = target.into();
        record
    };
    let agent = |id: &str| Requester::Agent { cutex_session_id: id.into(), name: None };
    state.asks.insert("ask-mine".into(), ask("ask-mine", agent("a1"), "a2"));
    state.asks.insert("ask-owner".into(), ask("ask-owner", Requester::Owner, "a1"));
    state.asks.insert("ask-other".into(), ask("ask-other", agent("a1"), "hidden"));
    let question = |id: &str, ask_id: &str| -> crate::asks::model::QuestionRecord {
        serde_json::from_value(json!({
            "questionId": id, "askId": ask_id, "attempt": 1, "runtimeGeneration": 1,
            "native": {"threadId": "t", "turnId": "u", "itemId": "i", "requestId": 1},
            "questions": [], "route": "owner", "state": "open", "createdAt": "2026-10-15T00:00:00Z",
        }))
        .unwrap()
    };
    state.questions.insert("q-1".into(), question("q-1", "ask-mine"));
    state.questions.insert("q-2".into(), question("q-2", "ask-other"));
    let scope = Scope::for_tests(Grant { send: true, ..Grant::default() }, &["a1", "a2"]);
    let check = |params: Value, session: &str| match ask_command_target(&state, &params, session, &scope) {
        AskTarget::Allowed => "allowed",
        AskTarget::Invalid => "invalid",
        AskTarget::Hidden(code) => code,
    };
    assert_eq!(check(json!({"askId": "ask-mine"}), "a2"), "allowed");
    assert_eq!(check(json!({"askId": "ask-mine"}), "a1"), "allowed", "the requester's path");
    assert_eq!(check(json!({"askId": "ask-owner"}), "a1"), "allowed", "an owner ask to a visible agent");
    assert_eq!(check(json!({"askId": "ask-mine"}), "a3"), "ask_not_found", "unrelated path session");
    assert_eq!(check(json!({"askId": "ask-other"}), "a1"), "ask_not_found", "the target is invisible");
    assert_eq!(check(json!({"askId": "ask-missing"}), "a1"), "ask_not_found");
    assert_eq!(check(json!({"questionId": "q-1", "answers": {}}), "a2"), "allowed");
    assert_eq!(check(json!({"questionId": "q-1"}), "a3"), "question_not_found");
    assert_eq!(check(json!({"questionId": "q-2"}), "a1"), "question_not_found");
    assert_eq!(check(json!({"questionId": "q-missing"}), "a1"), "question_not_found");
    assert_eq!(check(json!({}), "a1"), "invalid");
    assert_eq!(check(json!({"questionId": 5}), "a1"), "invalid");
}

#[test]
fn summary_and_project_filters_keep_only_the_grant() {
    let scope = Scope::for_tests(
        Grant { projects: Projects::Only(vec!["p1".into()]), standalone_agents: vec!["solo".into()], send: false },
        &["a1", "solo"],
    );
    let mut summary = json!({
        "agents": [
            {"id": "a1", "projectId": "p1", "needs": [{"kind": "approval"}]},
            {"id": "a2", "projectId": "p2", "needs": [{"kind": "approval"}]},
            {"id": "solo", "needs": []},
        ],
        "projects": [{"id": "p1"}, {"id": "p2"}],
        "needsYouCount": 2,
    });
    filter_summary(&mut summary, &scope);
    assert_eq!(summary["agents"].as_array().unwrap().len(), 2);
    assert_eq!(summary["projects"], json!([{"id": "p1"}]));
    assert_eq!(summary["needsYouCount"], 1);

    assert!(summary_event_visible("cutex/summary/agentChanged", &json!({"agent": {"id": "a1"}}), &scope));
    assert!(!summary_event_visible("cutex/summary/agentChanged", &json!({"agent": {"id": "a2"}}), &scope));
    assert!(summary_event_visible("cutex/summary/turnEnded", &json!({"id": "solo"}), &scope));
    assert!(!summary_event_visible("cutex/summary/agentRemoved", &json!({"id": "a2"}), &scope));
    assert!(!summary_event_visible("cutex/summary/approvalsChanged", &json!({"count": 1}), &scope));
    assert!(!summary_event_visible("cutex/summary/asksChanged", &json!({"count": 1}), &scope));
    assert!(!summary_event_visible("cutex/summary/messageToOwner", &json!({"id": "n"}), &scope));
    assert!(summary_event_visible("cutex/summary/resync", &json!({}), &scope));

    let mut projects = json!({
        "schema": "cutex/human-management-projects/v1",
        "projects": [{"project_id": "p1"}, {"project_id": "p2"}],
        "archived_projects": [{"project_id": "p1"}],
        "available_agents": [{"cutex_session_id": "x"}],
    });
    filter_projects(&mut projects, &scope);
    assert_eq!(projects["projects"], json!([{"project_id": "p1"}]));
    assert!(projects.get("archived_projects").is_none() && projects.get("available_agents").is_none());
}

#[test]
fn capabilities_carry_the_callers_user() {
    let pending = Principal::Pending.capability_view(None);
    assert_eq!(pending["pending"], true);
    assert_eq!(pending["isOwner"], false);
    let owner = Principal::Owner.capability_view(Some("Pat"));
    assert_eq!(owner, json!({"id": "owner", "name": "Pat", "isOwner": true, "pending": false, "grant": null}));
    let user = Principal::User { id: "u-0123456789ab".into(), name: "C".into(), grant: Grant::default() }
        .capability_view(None);
    assert_eq!(user["grant"], json!({"projects": [], "standaloneAgents": [], "send": false}));
    assert!(Principal::Owner.access_stamp() != Principal::Pending.access_stamp());
    let read = Principal::User { id: "u-0123456789ab".into(), name: "C".into(), grant: Grant::default() };
    let send = Principal::User { id: "u-0123456789ab".into(), name: "C".into(), grant: Grant { send: true, ..Grant::default() } };
    assert_ne!(read.access_stamp(), send.access_stamp(), "a grant change must end open streams");
}

#[test]
fn rewrite_query_replaces_one_parameter() {
    assert_eq!(rewrite_query("/v2/asks?limit=5&state=open", "limit", "1000"), "/v2/asks?state=open&limit=1000");
    assert_eq!(rewrite_query("/v2/asks", "limit", "1000"), "/v2/asks?limit=1000");
}

#[test]
fn device_scope_needs_per_route_class() {
    let none = |_: &str, _: &Value| None;
    let need = |method: &str, path: &str, body: Value| scope_need(method, path, &body, Some("mct-self"), none);
    // Reads: any GET (owner routes included), public / host routes, identity.
    for path in ["/v2/settings", "/v2/users", "/v2/events/stream", "/v2/sessions/s/bootstrap", "/v2/client-tokens"] {
        assert_eq!(need("GET", path, Value::Null), ScopeNeed::Read, "{path}");
    }
    assert_eq!(need("POST", "/v2/pair", Value::Null), ScopeNeed::Read);
    assert_eq!(need("POST", "/v2/identity/claim", Value::Null), ScopeNeed::Read);
    assert_eq!(need("DELETE", "/v2/users/me/devices/mct-self", Value::Null), ScopeNeed::Read, "sign itself out");
    assert_eq!(need("DELETE", "/v2/users/me/devices/mct-other", Value::Null), ScopeNeed::Full);
    // Native / cutex methods.
    let native = |method: &str| json!({"native": {"message": {"method": method}}});
    let path = "/v2/sessions/s/app-server/requests";
    assert_eq!(need("POST", path, native("thread/read")), ScopeNeed::Read);
    assert_eq!(need("POST", path, native("turn/interrupt")), ScopeNeed::Chat);
    assert_eq!(need("POST", path, native("turn/start")), ScopeNeed::Full);
    let path = "/v2/sessions/s/cutex/requests";
    for (method, expected) in [
        ("cutex/session/get", ScopeNeed::Read),
        ("cutex/userInput/submit", ScopeNeed::Chat),
        ("cutex/turn/stop", ScopeNeed::Chat),
        ("cutex/ask/respond", ScopeNeed::Chat),
        ("cutex/ask/close", ScopeNeed::Full),
        ("cutex/session/retire", ScopeNeed::Full),
    ] {
        assert_eq!(need("POST", path, json!({"method": method})), expected, "{method}");
    }
    // Server responses: questions are chat, approvals (and unknown ids) full.
    let pending = |_: &str, id: &Value| match id.as_i64() {
        Some(1) => Some("item/tool/requestUserInput".to_string()),
        Some(2) => Some("item/fileChange/requestApproval".to_string()),
        _ => None,
    };
    let path = "/v2/sessions/s/app-server/server-request-responses";
    let respond = |id: i64| json!({"native": {"message": {"id": id, "result": {}}}});
    assert_eq!(scope_need("POST", path, &respond(1), None, pending), ScopeNeed::Chat);
    assert_eq!(scope_need("POST", path, &respond(2), None, pending), ScopeNeed::Full);
    assert_eq!(scope_need("POST", path, &respond(3), None, pending), ScopeNeed::Full);
    assert_eq!(need("POST", "/v2/sessions/s/attachments", Value::Null), ScopeNeed::Chat);
    // user-home: reading needs read, editing the own home page chat.
    assert_eq!(need("GET", "/v2/users/me/home", Value::Null), ScopeNeed::Read);
    assert_eq!(need("GET", "/v2/users/me/home/cover", Value::Null), ScopeNeed::Read);
    for (method, path) in [("PUT", "/v2/users/me/home"), ("PUT", "/v2/users/me/home/cover"), ("DELETE", "/v2/users/me/home/cover")] {
        assert_eq!(need(method, path, Value::Null), ScopeNeed::Chat, "{method} {path}");
    }
    // client-remote-close: presence and the own outcome for every scope; the
    // close handler itself refuses narrowed scopes (client_close_forbidden).
    for path in ["/v2/clients/presence", "/v2/clients/i-1/close", "/v2/clients/i-1/closing"] {
        assert_eq!(need("POST", path, Value::Null), ScopeNeed::Read, "{path}");
    }
    // Every other write.
    for (method, path) in [
        ("PATCH", "/v2/settings"),
        ("POST", "/v2/approvals/x"),
        ("POST", "/v2/human/agents"),
        ("POST", "/v2/pairing-codes"),
        ("PUT", "/v2/users/token-bindings/mct-x"),
        ("POST", "/v2/terminals"),
        ("POST", "/v2/peers"),
    ] {
        assert_eq!(need(method, path, Value::Null), ScopeNeed::Full, "{method} {path}");
    }
    use crate::management::client_tokens::DeviceScope;
    assert!(scope_allows(DeviceScope::Read, ScopeNeed::Read) && !scope_allows(DeviceScope::Read, ScopeNeed::Chat));
    assert!(scope_allows(DeviceScope::Chat, ScopeNeed::Chat) && !scope_allows(DeviceScope::Chat, ScopeNeed::Full));
    assert!(scope_allows(DeviceScope::Full, ScopeNeed::Full));
}

#[path = "access_http_tests.rs"]
mod http;

#[test]
fn route_accepted_events_may_name_the_acting_user() {
    let event = |params: Value| json!({"method": "cutex/userInput/routeAccepted", "params": params});
    let mut params = json!({
        "managementRequestId": "req-1",
        "clientUserMessageId": "m-1",
        "origin": {"kind": "backend", "clientId": "c"},
        "input": [{"type": "text", "text": "hi", "text_elements": []}],
        "strategy": "queue",
        "state": "routed_to_cutex",
        "acceptedAt": "2026-10-14T00:00:00Z",
    });
    super::super::contract_validation::validate_cutex_event_message(&event(params.clone())).unwrap();
    params["actingUser"] = json!({"id": "u-0123456789ab", "name": "User 2"});
    super::super::contract_validation::validate_cutex_event_message(&event(params.clone())).unwrap();
    // users-self-profile decorates actingUser (current_acting_user): a
    // non-owner send must not fail event validation (503) because of it.
    params["actingUser"] = json!({"id": "u-0123456789ab", "name": "User 2", "displayName": null, "avatarEtag": null});
    super::super::contract_validation::validate_cutex_event_message(&event(params.clone())).unwrap();
    params["actingUser"] = json!({"id": "u-0123456789ab", "name": "User 2", "displayName": "Two", "avatarEtag": "0123456789abcdef"});
    super::super::contract_validation::validate_cutex_event_message(&event(params.clone())).unwrap();
    params["actingUser"] = json!({"id": "u-0123456789ab", "name": "User 2", "token": "x"});
    assert!(super::super::contract_validation::validate_cutex_event_message(&event(params)).is_err());
}

#[test]
fn workflow_events_stay_with_the_owner() {
    assert!(owner_only_event("cutex/workflow/run/changed"));
    assert!(owner_only_event("cutex/workflow/step/changed"));
    assert!(owner_only_event("cutex/workflow/log"));
    assert!(!owner_only_event("cutex/userInput/routeAccepted"));
    assert!(!owner_only_event("cutex/ask/changed"));
}
