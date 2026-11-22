//! r290 P10 (A1): project action receipts through the real access gate.
//! Included by access_http_tests.rs (shared harness: private instance, ROOT).
use super::*;

const BASE: &str = "/v2/project-actions";

#[test]
fn r290_project_action_receipts_owner_reads_and_settles_others_cannot() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    use crate::management::client_tokens::DeviceScope;
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    let full = tokens.issue("a1-owner-full").unwrap();
    users.bind_token(&full.token_id, Some(OWNER), "root").unwrap();
    let read = tokens.issue_scoped("a1-owner-read", DeviceScope::Read).unwrap();
    users.bind_token(&read.token_id, Some(OWNER), "root").unwrap();
    let member = users.create_user("a1-member", None, Grant::default(), "root").unwrap();
    let member_token = tokens.issue("a1-member").unwrap();
    users.bind_token(&member_token.token_id, Some(&member.id), "root").unwrap();
    std::thread::sleep(Duration::from_millis(30));

    let lookup = |action: &str, query: &str, bearer: Option<&str>| exchange("GET", &format!("{BASE}/{action}?{query}"), bearer, &json!({}));
    let settle = |action: &str, query: &str, bearer: Option<&str>| exchange("POST", &format!("{BASE}/{action}/settle?{query}"), bearer, &json!({}));

    // Control kind: the owner's ordinary receipt namespace.
    let (status, value) = lookup("grant-lost", "kind=grant", Some(ROOT));
    assert_eq!(status, 200, "{value}");
    assert_eq!((value["schema"].as_str(), value["state"].as_str(), value["final"].as_bool()),
        (Some("cutex.project-action-receipt.v1"), Some("notFound"), Some(false)), "{value}");
    // Others never read or settle owner receipts.
    assert_eq!(lookup("grant-lost", "kind=grant", None).0, 401);
    assert!(matches!(lookup("grant-lost", "kind=grant", Some(&member_token.token)).0, 401 | 403));
    assert!(matches!(settle("grant-lost", "kind=grant", Some(&member_token.token)).0, 401 | 403));
    // A narrowed (read) owner device may look, but not settle.
    assert_eq!(lookup("grant-lost", "kind=grant", Some(&read.token)).0, 200);
    assert!(matches!(settle("grant-lost", "kind=grant", Some(&read.token)).0, 401 | 403));
    assert_eq!(lookup("grant-lost", "kind=grant", Some(ROOT)).1["state"], "notFound");
    // Full owner device settles: durable known-none with a receipt the desktop already validates.
    let (status, value) = settle("grant-lost", "kind=grant", Some(&full.token));
    assert_eq!(status, 200, "{value}");
    assert_eq!((value["state"].as_str(), value["settled"].as_bool(), value["final"].as_bool()), (Some("none"), Some(true), Some(true)), "{value}");
    assert_eq!(value["receipt"]["schema"], "cutex.delegation-operation.v1");
    assert_eq!(value["receipt"]["operation"], "grant.create");
    assert_eq!(value["receipt"]["effectState"], "none");
    assert_eq!(lookup("grant-lost", "kind=grant", Some(ROOT)).1["receipt"], value["receipt"]);
    // The same id under a different kind is refused, not reinterpreted.
    assert_eq!(lookup("grant-lost", "kind=stop", Some(ROOT)).0, 409);
    // The old delegation-operations route sees the same known-none receipt.
    let (status, old) = exchange("GET", "/v2/delegation-operations/grant-lost", Some(ROOT), &json!({}));
    assert_eq!((status, old["status"].as_str(), old["outcomeUnknown"].as_bool()), (200, Some("found"), Some(false)), "{old}");

    // Lifecycle kinds need the member entity; settle fences the request id on that agent only.
    assert_eq!(lookup("archive-lost", "kind=archive", Some(ROOT)).0, 400);
    let (status, value) = settle("archive-lost", "kind=archive&entity=cutex.a1-member", Some(&full.token));
    assert_eq!(status, 200, "{value}");
    assert_eq!((value["state"].as_str(), value["settled"].as_bool(), value["final"].as_bool()), (Some("unknown"), Some(true), Some(false)), "{value}");
    assert_eq!(value["entity"], json!({"type":"agent","id":"cutex.a1-member"}));
    assert_eq!(lookup("archive-lost", "kind=archive&entity=cutex.other", Some(ROOT)).1["state"], "notFound");

    // A retained lifecycle response identifies the original method; never
    // reinterpret another method's successful receipt as this action.
    let repository = crate::management::v2::request_idempotency::request_idempotency_repository().unwrap();
    let claim = match repository.begin("cutex.a1-member", "offline-done", &json!({"method":"cutex/runtime/offline"})).unwrap() {
        crate::management::v2::request_idempotency::BeginRequest::Forward(claim) => claim,
        other => panic!("unexpected initial claim: {other:?}"),
    };
    repository.complete(&claim, 200, "OK", json!({"cutex":{"method":"cutex/runtime/offline","result":{}}})).unwrap();
    assert_eq!(lookup("offline-done", "kind=offline&entity=cutex.a1-member", Some(ROOT)).1["state"], "committed");
    assert_eq!(lookup("offline-done", "kind=archive&entity=cutex.a1-member", Some(ROOT)).0, 409);

    // Retirement: no receipt and no review token that could still run it -> none (read-only).
    let (status, value) = settle("retire-lost", "kind=retire&entity=cutex.a1-member", Some(&full.token));
    assert_eq!(status, 200, "{value}");
    assert_eq!((value["state"].as_str(), value["settled"].as_bool(), value["final"].as_bool()), (Some("none"), Some(false), Some(true)), "{value}");

    // The session-store fallback reads the exact receipt row, including after
    // the roster's action receipt was no longer retained there.
    let receipt = serde_json::from_value(json!({
        "request":{"action_id":"retire-row","reason":null,"review":{"cutex_session_id":"cutex.a1-member","formal_name":"Formal","operation":"permanent_retire","revision":1,"runtime_generation":1}},
        "request_sha256":"a".repeat(64),"stage":"committed","result":null,"error":null,"committed_at":"2026-11-17T00:00:00Z"
    })).unwrap();
    crate::session::store::update_receipt_row::<crate::session::store::AgentArchiveReceipts>(
        &crate::session::store::cutex_sessions_path().unwrap(), "retire-row", Some(receipt),
    ).unwrap();
    assert_eq!(lookup("retire-row", "kind=retire&entity=cutex.a1-member", Some(ROOT)).1["state"], "committed");

    // Bad input answers before any state.
    assert_eq!(lookup("x", "kind=delete", Some(ROOT)).0, 400);
    assert_eq!(lookup("x", "kind=grant&kind=stop", Some(ROOT)).0, 400);
    assert_eq!(exchange("POST", &format!("{BASE}/x?kind=grant"), Some(ROOT), &json!({})).0, 400);
    assert_eq!(exchange("GET", &format!("{BASE}/x/settle?kind=grant"), Some(ROOT), &json!({})).0, 400);
    let caps = crate::management::v2::capabilities::capabilities_resource();
    assert!(caps["featureList"].as_array().unwrap().iter().any(|f| f == "project-action-receipts-v1"));
}
