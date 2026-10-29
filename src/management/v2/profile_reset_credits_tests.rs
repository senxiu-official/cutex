//! Fakes only: no test reads or uses a real reset credit.
use std::sync::Mutex;

use super::*;
use crate::profiles::claude_resets::{Claim, Grant, Status};

fn profile(cli_kind: &str) -> StoredAccount {
    serde_json::from_value(json!({ "id": uuid::Uuid::new_v4().to_string(), "name": "rowan", "cli_kind": cli_kind }))
        .unwrap()
}

#[derive(Default)]
struct Fake {
    codex_read: Option<Value>,
    codex_consume: Option<Value>,
    claude_status: Option<Status>,
    claude_claim: Option<Claim>,
    calls: Mutex<Vec<String>>,
}

impl ResetBackend for Fake {
    fn codex_read(&self, _: &StoredAccount) -> Result<Value, String> {
        self.calls.lock().unwrap().push("codex_read".into());
        self.codex_read.clone().ok_or_else(|| "codex app-server unavailable".to_string())
    }
    fn codex_consume(&self, _: &StoredAccount, params: Value) -> Result<Value, String> {
        self.calls.lock().unwrap().push(format!("codex_consume {params}"));
        self.codex_consume.clone().ok_or_else(|| "consume failed".to_string())
    }
    fn claude_read(&self, account: &str) -> anyhow::Result<Option<Status>> {
        self.calls.lock().unwrap().push(format!("claude_read {account}"));
        Ok(self.claude_status.clone())
    }
    fn claude_claim(&self, account: &str, grant: &str, request: &str) -> anyhow::Result<Claim> {
        self.calls.lock().unwrap().push(format!("claude_claim {account} {grant} {request}"));
        self.claude_claim.clone().ok_or_else(|| anyhow::anyhow!("claim failed"))
    }
}

fn grant(id: &str, left: i64, paused: bool) -> Grant {
    Grant {
        id: id.into(),
        label: Some("Gift".into()),
        resets_total: Some(3),
        resets_left: left,
        ends_at: Some(1_795_996_800),
        paused,
        usable_now: !paused,
    }
}

fn status(grants: Vec<Grant>, next: Option<&str>) -> Status {
    Status {
        eligible: true,
        ineligible_reason: None,
        at_limit: true,
        grants,
        next_grant_id: next.map(str::to_string),
        cooldown_until: None,
    }
}

#[test]
fn paths_decode_and_reject_traversal() {
    assert_eq!(parse_path("/v2/profiles/GLM/reset-credits"), Some(("GLM".into(), ResetCreditsRoute::View)));
    assert_eq!(parse_path("/v2/profiles/GLM/reset-credits?refresh=true"), Some(("GLM".into(), ResetCreditsRoute::View)));
    assert_eq!(
        parse_path("/v2/profiles/my%20gpt/reset-credits/consume"),
        Some(("my gpt".into(), ResetCreditsRoute::Consume))
    );
    assert_eq!(parse_path("/v2/profiles/a/b/reset-credits"), None);
    assert_eq!(parse_path("/v2/profiles/a%2Fb/reset-credits"), None);
    assert_eq!(parse_path("/v2/profiles//reset-credits"), None);
    assert_eq!(parse_path("/v2/profiles/a/quota"), None);
}

#[test]
fn targets_follow_login_and_provider() {
    use ProviderCategory as P;
    assert_eq!(target_of(&CliKind::Codex, true, true, None, P::Codex), Target::Codex);
    assert_eq!(
        target_of(&CliKind::Codex, true, false, None, P::Other),
        Target::Unsupported { provider: Some("codex"), reason: "apiKey" }
    );
    for glm in [P::Glm, P::Zai] {
        assert_eq!(
            target_of(&CliKind::Codex, false, false, None, glm),
            Target::Unsupported { provider: Some("glm"), reason: "zcodeOnly" }
        );
    }
    assert_eq!(
        target_of(&CliKind::Codex, false, false, None, P::Kimi),
        Target::Unsupported { provider: None, reason: "provider" }
    );
    assert_eq!(
        target_of(&CliKind::Claude, false, false, Some(("ca-1".into(), ClaudeAccountKind::Subscription)), P::Claude),
        Target::Claude { account: "ca-1".into() }
    );
    assert_eq!(
        target_of(&CliKind::Claude, false, false, Some(("ca-2".into(), ClaudeAccountKind::Api)), P::Claude),
        Target::Unsupported { provider: Some("claude"), reason: "apiKey" }
    );
}

#[test]
fn codex_summary_keeps_available_rows_and_flags_capped_lists() {
    let read = json!({ "rateLimitResetCredits": { "availableCount": 3, "credits": [
        { "id": "rc_1", "resetType": "codexRateLimits", "status": "available", "grantedAt": 1_790_000_000,
          "expiresAt": 1_796_000_000, "title": "Reset", "description": "Resets 5h and weekly" },
        { "id": "rc_2", "resetType": "codexRateLimits", "status": "redeemed", "grantedAt": 1_790_000_000 }
    ]}});
    let (count, credits, complete) = codex_summary(&read);
    assert_eq!(count, Some(3));
    assert_eq!(credits.len(), 1);
    assert_eq!(credits[0]["expiresAt"], 1_796_000_000);
    assert!(!complete);
    assert_eq!(codex_summary(&json!({ "rateLimitResetCredits": null })), (None, vec![], false));
    let (count, credits, complete) = codex_summary(&json!({ "rateLimitResetCredits": { "availableCount": 0, "credits": [] } }));
    assert_eq!((count, credits.len(), complete), (Some(0), 0, true));
}

#[test]
fn views_are_cached_per_account_and_unsupported_profiles_make_no_call() {
    let fake = Fake {
        codex_read: Some(json!({ "rateLimitResetCredits": { "availableCount": 1, "credits": [
            { "id": "rc_1", "status": "available", "grantedAt": 1, "expiresAt": 2 } ] } })),
        ..Fake::default()
    };
    let codex = profile("codex");
    let first = view_with(&fake, &codex, &Target::Codex, false, 1_000);
    assert_eq!(first["availableCount"], 1);
    assert_eq!(first["supported"], true);
    assert_eq!(first["detailsComplete"], true);
    let again = view_with(&fake, &codex, &Target::Codex, true, 1_005);
    assert_eq!(again["availableCount"], 1);
    assert_eq!(fake.calls.lock().unwrap().len(), 1, "refresh within MIN_REFRESH_SECS uses the cache");
    view_with(&fake, &codex, &Target::Codex, true, 1_000 + MIN_REFRESH_SECS);
    view_with(&fake, &codex, &Target::Codex, false, 1_000 + MIN_REFRESH_SECS + CACHE_SECS);
    assert_eq!(fake.calls.lock().unwrap().len(), 3);

    let glm = view_with(&fake, &profile("codex"), &Target::Unsupported { provider: Some("glm"), reason: "zcodeOnly" }, true, 1_000);
    assert_eq!((glm["supported"].clone(), glm["unsupportedReason"].clone()), (json!(false), json!("zcodeOnly")));
    assert_eq!(fake.calls.lock().unwrap().len(), 3);
}

#[test]
fn read_failures_are_reported_in_the_view() {
    let fake = Fake::default();
    let view = view_with(&fake, &profile("codex"), &Target::Codex, false, 50);
    assert_eq!(view["error"], "codex app-server unavailable");
    assert_eq!(view["availableCount"], Value::Null);
}

#[test]
fn claude_view_lists_grants_with_expiry() {
    let fake = Fake { claude_status: Some(status(vec![grant("g_a", 2, false), grant("g_b", 1, true)], Some("g_a"))), ..Fake::default() };
    let account = format!("ca-{}", uuid::Uuid::new_v4());
    let view = view_with(&fake, &profile("claude"), &Target::Claude { account: account.clone() }, false, 10);
    assert_eq!(view["availableCount"], 2);
    assert_eq!(view["credits"].as_array().unwrap().len(), 2);
    assert_eq!(view["credits"][1]["status"], "paused");
    assert_eq!(view["account"]["id"], json!(account));
    assert_eq!(view["nextCreditId"], "g_a");
    let none = Fake::default();
    let view = view_with(&none, &profile("claude"), &Target::Claude { account: format!("ca-{}", uuid::Uuid::new_v4()) }, false, 10);
    assert_eq!((view["availableCount"].clone(), view["offered"].clone()), (json!(0), json!(false)));
}

#[test]
fn codex_consume_passes_the_idempotency_key_and_credit() {
    let fake = Fake { codex_consume: Some(json!({ "outcome": "reset" })), ..Fake::default() };
    let codex = profile("codex");
    let request = ConsumeRequest { idempotency_key: "click-0001".into(), credit_id: Some("rc_1".into()) };
    let reply = consume_with(&fake, &codex, &Target::Codex, &request).unwrap();
    assert_eq!(reply["outcome"], "reset");
    assert_eq!(
        fake.calls.lock().unwrap().as_slice(),
        [r#"codex_consume {"creditId":"rc_1","idempotencyKey":"click-0001"}"#]
    );
    let odd = Fake { codex_consume: Some(json!({ "outcome": "somethingNew" })), ..Fake::default() };
    let request = ConsumeRequest { idempotency_key: "click-0002".into(), credit_id: None };
    assert_eq!(consume_with(&odd, &codex, &Target::Codex, &request).unwrap()["outcome"], "unavailable");
}

#[test]
fn claude_consume_uses_the_next_grant_and_maps_results() {
    let fake = Fake {
        claude_status: Some(status(vec![grant("g_a", 2, false)], Some("g_a"))),
        claude_claim: Some(Claim { result: "not_limited".into(), reason: None, resets_left: Some(2) }),
        ..Fake::default()
    };
    let target = Target::Claude { account: "default".into() };
    let request = ConsumeRequest { idempotency_key: "click-0003".into(), credit_id: None };
    let reply = consume_with(&fake, &profile("claude"), &target, &request).unwrap();
    assert_eq!(reply["outcome"], "nothingToReset");
    assert_eq!(reply["resetsLeft"], 2);
    assert_eq!(fake.calls.lock().unwrap().last().unwrap(), "claude_claim default g_a click-0003");

    let empty = Fake { claude_status: Some(status(vec![grant("g_p", 1, true)], None)), ..Fake::default() };
    let reply = consume_with(&empty, &profile("claude"), &target, &request).unwrap();
    assert_eq!(reply["outcome"], "noCredit");
    assert!(!empty.calls.lock().unwrap().iter().any(|call| call.starts_with("claude_claim")));
    assert_eq!(claude_outcome("already_used"), "alreadyRedeemed");
    assert_eq!(claude_outcome("weird"), "unavailable");
}

#[test]
fn consume_refuses_bad_keys_unsupported_targets_and_parallel_use() {
    let fake = Fake::default();
    let codex = profile("codex");
    let short = ConsumeRequest { idempotency_key: "x".into(), credit_id: None };
    assert!(matches!(consume_with(&fake, &codex, &Target::Codex, &short), Err(ConsumeError::Invalid(_))));
    let ok = ConsumeRequest { idempotency_key: "click-0004".into(), credit_id: None };
    let glm = Target::Unsupported { provider: Some("glm"), reason: "zcodeOnly" };
    assert!(matches!(consume_with(&fake, &codex, &glm, &ok), Err(ConsumeError::Unsupported("zcodeOnly"))));
    let _held = Consuming::claim(&format!("codex:{}", codex.id)).unwrap();
    assert!(matches!(consume_with(&fake, &codex, &Target::Codex, &ok), Err(ConsumeError::InProgress)));
    assert!(fake.calls.lock().unwrap().is_empty());
    assert!(serde_json::from_value::<ConsumeRequest>(json!({ "idempotencyKey": "click-0005", "x": 1 })).is_err());
}
