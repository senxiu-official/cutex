//! Root-only Management v2 surface for design 047 approval auto-rules.

use std::net::TcpStream;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::approval_rules::{ApprovalRuleDraft, RuleError};
use crate::approvals::ApprovalStore;
use crate::http::server::{write_json_response, SimpleHttpRequest};

pub const PATH: &str = "/v2/approval-rules";

#[derive(Clone, Debug, PartialEq, Eq)]
enum Route {
    Collection,
    Rule(String),
    Confirm,
    Audit,
}

fn valid_id(id: &str) -> bool {
    id.starts_with("aar-")
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn parse_path(path: &str) -> Option<Route> {
    match path {
        PATH => Some(Route::Collection),
        "/v2/approval-rules/confirm" => Some(Route::Confirm),
        "/v2/approval-rules/audit" => Some(Route::Audit),
        _ => path
            .strip_prefix("/v2/approval-rules/")
            .filter(|id| valid_id(id))
            .map(|id| Route::Rule(id.into())),
    }
}

pub fn is_path(path: &str) -> bool {
    path == PATH || path.starts_with("/v2/approval-rules/")
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WriteBody {
    expected_revision: u64,
    rule: ApprovalRuleDraft,
    #[serde(default)]
    confirmation_token: Option<String>,
    // r290 P10 (B8): the owner resumed an unchanged paused rule (PATCH only).
    #[serde(default)]
    owner_resume: bool,
}

/// r290 P10 (B8): `ownerResume` is an owner gesture: the owner principal
/// (root credential or owner client token) with full device scope. A
/// narrowed device token or a shared-access user never skips the token.
/// Agents never reach this root-only route.
fn owner_gesture_allowed() -> bool {
    crate::management::users::caller_is_owner()
        && crate::management::listeners::current_client_token().is_none_or(|token| token.scope.is_full())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConfirmBody {
    rule: ApprovalRuleDraft,
    typed_phrase: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeleteBody {
    expected_revision: u64,
}

fn body<T: for<'de> Deserialize<'de>>(request: &SimpleHttpRequest) -> Result<T, String> {
    serde_json::from_slice(&request.body).map_err(|error| format!("invalid JSON body: {error}"))
}

fn error(stream: &mut TcpStream, error: RuleError) -> anyhow::Result<()> {
    match error {
        RuleError::Invalid(message) => super::server::write_v2_error(
            stream,
            400,
            "Bad Request",
            "invalid_request",
            &message,
            false,
            json!({}),
        ),
        RuleError::Conflict(revision) => super::server::write_v2_error(
            stream,
            409,
            "Conflict",
            "revision_conflict",
            "approval rules changed; refresh and retry",
            false,
            json!({"revision":revision}),
        ),
        RuleError::NotFound => super::server::write_v2_error(
            stream,
            404,
            "Not Found",
            "approval_rule_not_found",
            "no approval rule has this id",
            false,
            json!({}),
        ),
        RuleError::ConfirmationRequired(phrase) => super::server::write_v2_error(
            stream,
            409,
            "Conflict",
            "confirmation_required",
            "typed confirmation is required for high-risk kinds",
            false,
            json!({"requiredPhrase":phrase}),
        ),
        RuleError::Store(diagnostic) => super::server::write_v2_error(
            stream,
            503,
            "Service Unavailable",
            "approval_rules_unavailable",
            "the approval rule store could not be updated",
            true,
            json!({"diagnostic":diagnostic}),
        ),
    }
}

fn invalid(stream: &mut TcpStream, message: String) -> anyhow::Result<()> {
    super::server::write_v2_error(
        stream,
        400,
        "Bad Request",
        "invalid_request",
        &message,
        false,
        json!({}),
    )
}

fn publish(action: &str, rule_id: Option<&str>, revision: u64) {
    let Ok(repository) = super::repository::management_v2_repository() else {
        return;
    };
    let _ = super::server::append_cutex_event(
        repository,
        super::services::HOST_EVENT_SESSION_ID,
        super::model::EventCorrelation::default(),
        "cutex/approvalRules/changed",
        json!({"action":action,"ruleId":rule_id,"revision":revision}),
    );
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let Some(route) = parse_path(path) else {
        return super::server::write_v2_error(
            stream,
            404,
            "Not Found",
            "not_found",
            "unknown approval rule route",
            false,
            json!({}),
        );
    };
    let store = match ApprovalStore::open_default() {
        Ok(store) => store,
        Err(failure) => return error(stream, RuleError::Store(format!("{failure:#}"))),
    };
    let now = chrono::Utc::now();
    match (request.method.as_str(), route) {
        ("GET", Route::Collection) => match store.list_rules(now) {
            Ok((revision, rules)) => write_json_response(stream, 200, "OK", &json!({
                "contractVersion":1,"hostId":crate::platform::host::current_host_name(),
                "revision":revision,"catalog":ApprovalStore::rule_catalog(),"rules":rules
            })),
            Err(failure) => error(stream, failure),
        },
        ("POST", Route::Confirm) => {
            let input: ConfirmBody = match body(request) { Ok(value) => value, Err(message) => return invalid(stream, message) };
            match store.confirm_rule(input.rule, &input.typed_phrase, now) {
                Ok((confirmation_token, phrase)) => write_json_response(stream, 200, "OK", &json!({
                    "confirmationToken":confirmation_token,"requiredPhrase":phrase,"expiresInSeconds":300
                })),
                Err(failure) => error(stream, failure),
            }
        }
        ("POST", Route::Collection) => {
            let input: WriteBody = match body(request) { Ok(value) => value, Err(message) => return invalid(stream, message) };
            // r290 P10 (B8): a new rule always needs the typed token.
            if input.owner_resume {
                return invalid(stream, "ownerResume applies only to PATCH of an existing rule".into());
            }
            match store.create_rule(input.expected_revision, input.rule, input.confirmation_token.as_deref(), now) {
                Ok((revision, rule)) => { publish("created", Some(&rule.rule_id), revision); write_json_response(stream, 201, "Created", &json!({"revision":revision,"rule":rule})) },
                Err(failure) => error(stream, failure),
            }
        }
        ("PATCH", Route::Rule(id)) => {
            let input: WriteBody = match body(request) { Ok(value) => value, Err(message) => return invalid(stream, message) };
            // r290 P10 (B8): `ownerResume` only counts for a full-scope owner; any
            // other caller (or a changed rule) still needs the typed token.
            let replaced = if input.owner_resume && owner_gesture_allowed() {
                store.replace_rule_owner_resume(&id, input.expected_revision, input.rule, input.confirmation_token.as_deref(), now)
            } else {
                store.replace_rule(&id, input.expected_revision, input.rule, input.confirmation_token.as_deref(), now)
            };
            match replaced {
                Ok((revision, rule)) => { publish("updated", Some(&rule.rule_id), revision); write_json_response(stream, 200, "OK", &json!({"revision":revision,"rule":rule})) },
                Err(failure) => error(stream, failure),
            }
        }
        ("DELETE", Route::Rule(id)) => {
            let input: DeleteBody = match body(request) { Ok(value) => value, Err(message) => return invalid(stream, message) };
            match store.delete_rule(&id, input.expected_revision, now) {
                Ok((revision, changed)) => { if changed { publish("revoked", Some(&id), revision); } write_json_response(stream, 200, "OK", &json!({"revision":revision,"revoked":true,"ruleId":id})) },
                Err(failure) => error(stream, failure),
            }
        }
        ("GET", Route::Audit) => {
            if crate::http::query::query_value(&request.path,"page").as_deref()==Some("keyset") {
                let after=crate::http::query::query_value(&request.path,"after");
                let limit=crate::http::query::query_value(&request.path,"limit").as_deref().unwrap_or("100").parse::<usize>().ok().filter(|n|(1..=200).contains(n));
                let Some(limit)=limit else {return invalid(stream,"limit must be 1..=200".into());};
                return match store.rule_audit_page(after.as_deref(),crate::store::PageBudget{max_rows:limit,max_bytes:256*1024}) {
                    Ok((audit,next))=>write_json_response(stream,200,"OK",&json!({"audit":audit,"nextCursor":next})),
                    Err(failure)=>super::approvals::page_error(stream,&failure),
                };
            }
            let cursor = crate::http::query::query_value(&request.path, "cursor").as_deref().unwrap_or("0").parse::<usize>().ok();
            let limit = crate::http::query::query_value(&request.path, "limit").as_deref().unwrap_or("100").parse::<usize>().ok().filter(|limit| (1..=200).contains(limit));
            let (Some(cursor), Some(limit)) = (cursor, limit) else { return invalid(stream, "cursor must be non-negative and limit must be 1..=200".into()) };
            match store.rule_audit(cursor, limit) {
                Ok((audit, next_cursor)) => write_json_response(stream, 200, "OK", &json!({"audit":audit,"nextCursor":next_cursor})),
                Err(failure) => error(stream, failure),
            }
        }
        _ => super::server::write_v2_error(stream, 405, "Method Not Allowed", "method_not_allowed", "use GET/POST /v2/approval-rules, PATCH/DELETE /v2/approval-rules/{id}, POST /confirm or GET /audit", false, Value::Object(Default::default())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn routes_are_narrow_and_never_overlap_approvals() {
        assert_eq!(parse_path(PATH), Some(Route::Collection));
        assert_eq!(
            parse_path("/v2/approval-rules/confirm"),
            Some(Route::Confirm)
        );
        assert_eq!(parse_path("/v2/approval-rules/audit"), Some(Route::Audit));
        assert_eq!(
            parse_path("/v2/approval-rules/aar-123"),
            Some(Route::Rule("aar-123".into()))
        );
        assert!(parse_path("/v2/approval-rules/bad").is_none());
        assert!(!is_path("/v2/approvals"));
    }

    // r290 P10 (B8)
    #[test]
    fn owner_resume_gesture_counts_only_for_a_full_scope_owner() {
        use crate::management::client_tokens::{DeviceScope, VerifiedClientToken};
        use crate::management::listeners::set_current_client_token;
        use crate::management::users::{set_current_principal, Principal};
        let token = |scope| { let mut token = VerifiedClientToken::test_token("ct-test"); token.scope = scope; token };
        set_current_principal(None);
        set_current_client_token(None);
        assert!(owner_gesture_allowed(), "root credential");
        set_current_client_token(Some(token(DeviceScope::Full)));
        assert!(owner_gesture_allowed(), "full-scope owner device");
        for scope in [DeviceScope::Chat, DeviceScope::Read] {
            set_current_client_token(Some(token(scope)));
            assert!(!owner_gesture_allowed(), "narrowed device token");
        }
        set_current_client_token(Some(token(DeviceScope::Full)));
        set_current_principal(Some(Principal::Pending));
        assert!(!owner_gesture_allowed(), "not the owner");
        set_current_principal(None);
        set_current_client_token(None);
    }

    #[test]
    fn owner_resume_is_a_patch_field_only() {
        let body: WriteBody = serde_json::from_value(json!({
            "expectedRevision": 1, "ownerResume": true,
            "rule": { "subject": { "type": "agent", "cutexSessionId": "cutex.a" }, "kinds": ["managed_service"], "expiry": { "mode": "revoked" } },
        })).unwrap();
        assert!(body.owner_resume);
    }
}
