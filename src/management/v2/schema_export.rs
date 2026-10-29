//! `cutex management schema export --out <dir>` (capability
//! `schema-export`, cutex-desktop docs/research/cutex-mobile-api.md §8):
//! JSON Schema (draft 2020-12) of the Management v2 resources the desktop and
//! the phone read, plus `index.json`. Clients generate TS types from them
//! (`json-schema-to-typescript`, with `unreachableDefinitions` for `$defs`).
//!
//! The schemas are embedded (`schema/export/*.json`, LF) and checked against
//! real resource values in the tests. Native app-server payloads
//! (`thread/read` results, native messages) keep their own generated schema
//! (`codex app-server generate-json-schema`) and are typed as objects here.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

pub struct ExportedSchema {
    pub name: &'static str,
    pub title: &'static str,
    pub routes: &'static [&'static str],
    pub text: &'static str,
}

pub const SCHEMAS: &[ExportedSchema] = &[
    super::archive::api::EXPORTED_SCHEMA,
    ExportedSchema {
        name: "guides-preferences", title: "Owner-user guide preferences",
        routes: &["GET /v2/human/preferences/guides", "PUT /v2/human/preferences/guides"],
        text: include_str!("schema/export/guides-preferences.json"),
    },
    ExportedSchema {
        name: "image-input", title: "Busy image input policy and steering receipts",
        routes: &["GET|PATCH /v2/settings imageInput", "GET /v2/sessions/{id}", "cutex/userInput/submit", "cutex/userInput/queue/steer"],
        text: include_str!("schema/export/image-input.json"),
    },
    ExportedSchema {
        name: "tool-action-receipt", title: "Bounded tool execution and read-only action receipts",
        routes: &["MCP agent_create", "MCP agent_delegate", "MCP action_status"],
        text: include_str!("schema/export/tool-action-receipt.json"),
    },
    ExportedSchema {
        name: "profile-link", title: "Owner profile account linking",
        routes: &["GET /v2/profile-link/secret", "PUT /v2/profile-link/secret", "DELETE /v2/profile-link/secret", "GET /v2/profiles", "GET /v2/profiles/{profile}", "PATCH /v2/profiles/{profile}"],
        text: include_str!("schema/export/profile-link.json"),
    },
    super::director::EXPORTED_SCHEMA,
    ExportedSchema {
        name: "profile-relay-runtimes", title: "Profile provider interface addresses",
        routes: &["GET /v2/profiles", "GET|PATCH /v2/profiles/{profile}", "POST /v2/profiles", "GET /v2/profile-presets", "GET /v2/capabilities profile-relay-runtimes"],
        text: include_str!("schema/export/profile-relay-runtimes.json"),
    },
    ExportedSchema {
        name: "storage-cleanup", title: "Owner desktop storage cleanup",
        routes: &["POST /v2/storage/cleanups/preview", "POST /v2/storage/cleanups", "GET /v2/storage/cleanups", "GET /v2/storage/cleanups/{runId}"],
        text: include_str!("schema/export/storage-cleanup.json"),
    },
    ExportedSchema {
        name: "fork-preview", title: "Read-only fork preview",
        routes: &["GET /v2/sessions/{id}/fork-preview?afterTurnId=&profile="],
        text: include_str!("schema/export/fork-preview.json"),
    },
    ExportedSchema {
        name: "storage", title: "Host storage",
        routes: &["GET /v2/storage", "POST /v2/storage/refresh", "GET /v2/storage/stores/{id}/items"],
        text: include_str!("schema/export/storage.json"),
    },
    ExportedSchema {
        name: "browser", title: "Browser relay v1",
        routes: &["POST /v2/browser/devices/register", "GET /v2/browser/devices/{deviceId}/commands/stream", "POST /v2/browser/devices/{deviceId}/heartbeat", "POST /v2/browser/devices/{deviceId}/commands/{requestId}/result", "POST /v2/browser/leases/grant", "POST /v2/browser/leases/{leaseId}/activate|revoke", "POST /v2/browser/commands", "GET /v2/browser/commands/{requestId}", "POST /v2/browser/commands/{requestId}/cancel", "GET /v2/browser/agents", "GET /v2/browser/turn-shots?sessionId=&turnId=", "GET /v2/browser/turn-shots/{shotId}", "GET /v2/browser/tool-item?sessionId=&turnId=&itemId=", "GET|PATCH /v2/settings browser", "PATCH /v2/profiles/{id} browserTools"],
        text: include_str!("schema/export/browser.json"),
    },
    ExportedSchema {
        name: "health",
        title: "Host health",
        routes: &["GET /v2/health", "POST /v2/health/fix"],
        text: include_str!("schema/export/health.json"),
    },
    ExportedSchema {
        name: "error",
        title: "Error envelope",
        routes: &[
            "any non-2xx JSON response",
            "SSE cutex_management_stream_error",
        ],
        text: include_str!("schema/export/error.json"),
    },
    ExportedSchema {
        name: "profile-runtime-variants",
        title: "Profile runtime variants and stored credentials",
        routes: &[
            "GET /v2/profiles (items)",
            "GET|PATCH /v2/profiles/{p}",
            "POST /v2/profiles (kind dsh: apiKey, credential; kind kimi: kimiAccount)",
            "POST /v2/profiles/{p}/variants",
            "PATCH|DELETE /v2/profiles/{p}/variants/{v}",
            "POST /v2/human/agents variantId",
            "POST /v2/human/sessions variantId",
            "session resource launchBinding",
        ],
        text: include_str!("schema/export/profile-runtime-variants.json"),
    },
    ExportedSchema {
        name: "sessions",
        title: "Sessions",
        routes: &["GET /v2/sessions", "GET /v2/sessions/{id}"],
        text: include_str!("schema/export/sessions.json"),
    },
    ExportedSchema {
        name: "bootstrap",
        title: "Bootstrap and history",
        routes: &[
            "GET /v2/sessions/{id}/bootstrap",
            "GET /v2/sessions/{id}/history",
        ],
        text: include_str!("schema/export/bootstrap.json"),
    },
    ExportedSchema {
        name: "events",
        title: "Events",
        routes: &["GET /v2/events", "GET /v2/events/stream"],
        text: include_str!("schema/export/events.json"),
    },
    ExportedSchema {
        name: "approvals",
        title: "Owner approvals",
        routes: &[
            "GET /v2/approvals",
            "POST /v2/approvals/{id}/approve",
            "POST /v2/approvals/{id}/deny",
        ],
        text: include_str!("schema/export/approvals.json"),
    },
    ExportedSchema {
        name: "approval-rules",
        title: "Owner automatic approval rules",
        routes: &[
            "GET|POST /v2/approval-rules",
            "PATCH|DELETE /v2/approval-rules/{id}",
            "POST /v2/approval-rules/confirm",
            "GET /v2/approval-rules/audit",
        ],
        text: include_str!("schema/export/approval-rules.json"),
    },
    ExportedSchema {
        name: "asks",
        title: "Asks (delegations)",
        routes: &["GET /v2/asks", "GET /v2/asks/{id}"],
        text: include_str!("schema/export/asks.json"),
    },
    ExportedSchema {
        name: "notifications",
        title: "Notifications",
        routes: &["GET /v2/notifications/states", "POST /v2/notifications/ack"],
        text: include_str!("schema/export/notifications.json"),
    },
    ExportedSchema {
        name: "summary",
        title: "Host summary",
        routes: &[
            "GET /v2/summary",
            "GET /v2/summary/events",
            "POST /v2/summary/read",
        ],
        text: include_str!("schema/export/summary.json"),
    },
    ExportedSchema {
        name: "project-events",
        title: "Project log",
        routes: &[
            "GET /v2/projects/{projectId}/events",
            "GET /v2/projects/{projectId}/events/stream",
            "GET /v2/projects/{projectId}/background",
            "host event cutex/projectEvents/appended",
        ],
        text: include_str!("schema/export/project-events.json"),
    },
    ExportedSchema {
        name: "pair",
        title: "Pairing and client tokens",
        routes: &[
            "POST /v2/pairing-codes",
            "POST /v2/pair",
            "GET /v2/client-tokens",
        ],
        text: include_str!("schema/export/pair.json"),
    },
    ExportedSchema {
        name: "runtime-packages",
        title: "Runtime package upgrade",
        routes: &[
            "GET /v2/runtime-packages",
            "POST /v2/runtime-packages/runs",
            "GET /v2/runtime-packages/runs",
            "GET /v2/runtime-packages/runs/{runId}",
            "POST /v2/runtime-packages/runs/{runId}/revert",
            "host event cutex/runtimePackages/runUpdated",
        ],
        text: include_str!("schema/export/runtime-packages.json"),
    },
    ExportedSchema {
        name: "upgrade",
        title: "Backend upgrade",
        routes: &[
            "GET /v2/upgrade",
            "POST /v2/upgrade/check",
            "POST /v2/upgrade/stage",
            "POST /v2/upgrade/apply",
            "POST /v2/upgrade/cancel",
            "POST /v2/upgrade/rollback",
            "GET /v2/upgrade/runs?limit=",
            "GET /v2/upgrade/runs/{runId}",
            "GET|PATCH /v2/upgrade/settings",
            "host event cutex/upgrade/updated",
        ],
        text: include_str!("schema/export/upgrade.json"),
    },
    ExportedSchema {
        name: "runtimes",
        title: "Runtimes overview",
        routes: &["GET /v2/runtimes"],
        text: include_str!("schema/export/runtimes.json"),
    },
    ExportedSchema {
        name: "claude-accounts",
        title: "Claude accounts",
        routes: &[
            "GET /v2/claude/accounts",
            "POST /v2/claude/accounts",
            "GET /v2/claude/accounts/{id}",
            "PATCH /v2/claude/accounts/{id}",
            "DELETE /v2/claude/accounts/{id}",
            "POST /v2/claude/accounts/{id}/status",
            "POST /v2/claude/accounts/{id}/logout",
            "session resource claudeAccount",
            "GET /v2/profiles/{p}/quota nativeQuota.apiUsage",
        ],
        text: include_str!("schema/export/claude-accounts.json"),
    },
    ExportedSchema {
        name: "kimi-accounts",
        title: "Kimi accounts",
        routes: &[
            "GET /v2/kimi/accounts",
            "POST /v2/kimi/accounts",
            "GET /v2/kimi/accounts/{id}",
            "PATCH /v2/kimi/accounts/{id}",
            "DELETE /v2/kimi/accounts/{id}",
            "POST /v2/kimi/accounts/{id}/login",
            "GET /v2/kimi/accounts/{id}/login/{loginId}",
            "DELETE /v2/kimi/accounts/{id}/login/{loginId}",
            "POST /v2/kimi/accounts/{id}/logout",
            "POST /v2/kimi/accounts/{id}/status",
        ],
        text: include_str!("schema/export/kimi-accounts.json"),
    },
    super::owner_rotation_api::EXPORTED_SCHEMA,
    super::artifact_store::EXPORTED_SCHEMA,
    super::host_info::EXPORTED_SCHEMA,
    super::alarms_api::EXPORTED_SCHEMA,
    super::skills_archive::EXPORTED_SCHEMA,
    super::runtime_restore_api::EXPORTED_SCHEMA,
    super::quota_continue::EXPORTED_SCHEMA,
    super::claude_runtime_api::EXPORTED_SCHEMA,
    super::clients_api::EXPORTED_SCHEMA,
    ExportedSchema {
        name: "services",
        title: "Services, viewing tickets, leases and tiles",
        routes: &[
            "GET /v2/services",
            "GET /v2/services/{slug}",
            "GET /v2/services/{slug}/eligible-owners",
            "POST /v2/services/{slug}/view-ticket",
            "POST /v2/services/{slug}/lease",
            "POST|DELETE /v2/services/{slug}/lease/{leaseId}",
            "GET /v2/services/{slug}/tile",
        ],
        text: include_str!("schema/export/services.json"),
    },
    ExportedSchema {
        name: "users",
        title: "Users, self-service profiles, avatars, own devices, home pages and person identity",
        routes: &[
            "GET /v2/users",
            "GET|PATCH /v2/users/me",
            "GET /v2/users/{id}/avatar",
            "GET /v2/capabilities user",
            "event cutex/userInput/routeAccepted actingUser",
            "GET /v2/identity/challenge",
            "POST /v2/identity/claim",
            "PUT /v2/users/{id}/person-keys",
            "DELETE /v2/users/token-persons/{tokenId}/previous",
            "GET /v2/users/me/devices",
            "DELETE /v2/users/me/devices/{tokenId}",
            "GET|PUT /v2/users/me/home",
            "GET|PUT|DELETE /v2/users/me/home/cover",
        ],
        text: include_str!("schema/export/users.json"),
    },
    ExportedSchema {
        name: "peers",
        title: "Daemon peers and cross-host delegation",
        routes: &[
            "GET|POST /v2/peers",
            "POST /v2/peers/invites",
            "PATCH|DELETE /v2/peers/{peerId}",
            "GET /v2/peers/{peerId}/agents",
            "GET /v2/peers/{peerId}/inflight",
            "GET /v2/peers/{peerId}/audit?limit=",
            "POST /peer/v1/pair",
            "GET /peer/v1/hello",
            "POST /peer/v1/confirm",
            "POST /peer/v1/abort",
            "POST /peer/v1/unpair",
            "GET /peer/v1/agents",
            "PUT|GET /peer/v1/asks/{askId}",
            "POST /peer/v1/asks/{askId}/ops",
            "POST /peer/v1/events",
            "GET|PATCH /v2/settings peers",
        ],
        text: include_str!("schema/export/peers.json"),
    },
];

pub fn index() -> Value {
    json!({
        "schema": "cutex/management-schema-index/v1",
        "contractVersion": 2,
        "cutexBuild": crate::build_info::build_json(),
        "draft": "https://json-schema.org/draft/2020-12/schema",
        "capabilities": super::capabilities::capabilities_resource()["featureList"].clone(),
        "files": SCHEMAS
            .iter()
            .map(|schema| {
                let value: Value = serde_json::from_str(schema.text).unwrap_or(Value::Null);
                json!({
                    "name": schema.name,
                    "file": format!("{}.json", schema.name),
                    "id": value["$id"],
                    "title": schema.title,
                    "routes": schema.routes,
                    "definitions": value["$defs"]
                        .as_object()
                        .map(|defs| defs.keys().cloned().collect::<Vec<_>>())
                        .unwrap_or_default(),
                })
            })
            .collect::<Vec<_>>(),
    })
}

/// Write every schema and `index.json` into `dir` (created if missing).
pub fn export(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir)?;
    let mut written = Vec::new();
    for schema in SCHEMAS {
        let path = dir.join(format!("{}.json", schema.name));
        std::fs::write(&path, schema.text)?;
        written.push(path);
    }
    let path = dir.join("index.json");
    let mut text = serde_json::to_string_pretty(&index())?;
    text.push('\n');
    std::fs::write(&path, text)?;
    written.push(path);
    Ok(written)
}

/// A validator for one `$defs` entry of an exported schema (tests, tools).
pub fn validator(name: &str, definition: &str) -> anyhow::Result<jsonschema::Validator> {
    let schema = SCHEMAS
        .iter()
        .find(|schema| schema.name == name)
        .ok_or_else(|| anyhow::anyhow!("no exported schema {name}"))?;
    let mut value: Value = serde_json::from_str(schema.text)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("schema {name} is not an object"))?;
    object.remove("anyOf");
    object.insert(
        "$ref".into(),
        Value::String(format!("#/$defs/{definition}")),
    );
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .build(&value)
        .map_err(|error| anyhow::anyhow!("schema {name}#{definition}: {error}"))
}

#[cfg(test)]
pub fn assert_valid(name: &str, definition: &str, value: &Value) {
    let validator = validator(name, definition).unwrap();
    let errors: Vec<String> = validator
        .iter_errors(value)
        .map(|e| format!("{} at {}", e, e.instance_path))
        .collect();
    assert!(
        errors.is_empty(),
        "{name}#{definition}: {errors:#?}\n{value:#}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guides_and_creation_provenance_schemas_match_the_additive_contract() {
        for creator in [Value::Null, json!({"kind":"owner"}), json!({"kind":"system"}),
            json!({"kind":"agent","cutexSessionId":"cutex.director","role":"director"}),
            json!({"kind":"agent","cutexSessionId":"cutex.member","role":"member"})] {
            assert_valid("sessions", "sessionCreatedBy", &creator);
        }
        assert!(!validator("sessions", "sessionCreatedBy").unwrap().is_valid(&json!({"kind":"agent","role":"director"})));
        assert!(!validator("sessions", "sessionCreatedBy").unwrap().is_valid(&json!({"kind":"owner","role":"director"})));
        assert_valid("guides-preferences", "guidesPreference", &json!({"schema":1,"skippedThrough":null,"updatedAt":null}));
        assert_valid("guides-preferences", "guidesPreferencePut", &json!({"schema":1,"skippedThrough":"0.2.0","future":true}));
        assert!(!validator("guides-preferences", "guidesPreferencePut").unwrap().is_valid(&json!({"schema":2,"skippedThrough":null})));
    }

    #[test]
    fn full_avatar_style_schema_matches_canonical_profile_values() {
        let contract = validator("profile-link", "avatarStyle").unwrap();
        for value in [
            Value::Null,
            json!({"kind":"portrait","portraitThemes":["uninstalled/theme"]}),
            json!({"kind":"emoji","emojiThemes":null}),
            json!({"kind":"emoji","emojiThemes":["sea","cats"]}),
            json!({"kind":"letter"}),
        ] {
            assert!(contract.is_valid(&value), "{value}");
        }
        for invalid in [
            json!({"kind":"portrait","portraitThemes":["../theme"]}),
            json!({"kind":"portrait","portraitThemes":[]}),
            json!({"kind":"emoji"}), json!({"kind":"emoji","emojiThemes":["unknown"]}),
            json!({"kind":"letter","portraitThemes":["pack/theme"]}),
        ] {
            assert!(!contract.is_valid(&invalid), "{invalid}");
        }
    }

    #[test]
    fn browser_schema_snapshot_and_strict_owner_model_boundaries() {
        use sha2::{Digest,Sha256};
        let schema=SCHEMAS.iter().find(|s|s.name=="browser").unwrap();
        assert_eq!(format!("{:x}",Sha256::digest(schema.text.as_bytes())),"d282fb3671e8fb2d9cc5c8e6b05003e4170c22dd87e03028703885227be91c05");
        assert_valid("browser","heartbeatRequest",&json!({"ownerActivity":true}));
        assert_valid("browser","heartbeatRequest",&json!({}));
        assert!(!validator("browser","heartbeatRequest").unwrap().is_valid(&json!({"ownerActivity":true,"timestamp":1})));
        assert_valid("browser","registration",&json!({"deviceId":"d","hostId":"local","providerId":"browser","executor":"inApp"}));
        assert_valid("browser","agentCommand",&json!({"requestId":"r","tool":"browser_act","arguments":{"title":"Submit","guestId":"g","action":{"kind":"click","ref":"r1"}}}));
        assert!(!validator("browser","agentCommand").unwrap().is_valid(&json!({"requestId":"r","tool":"browser_read","arguments":{"title":"Observe","guestId":"g","leaseId":"spoof"}})));
        assert!(!validator("browser","grant").unwrap().is_valid(&json!({"deviceId":"d","guestId":"g","cutexSessionId":"s","title":"Page","url":"about:blank","source":{"kind":"owner","partition":"spoof"}})));
        assert_valid("browser","deviceResult",&json!({"dispatchSeq":1,"target":{"hostId":"local","deviceId":"d","guestId":"g"},"operationKind":"tab.snapshot","status":"succeeded","metadataOnly":true}));
        assert_valid("browser","settings",&json!({"agentOpenApproval":"off","heartbeatMs":10000,"offlineMs":30000,"leaseIdleSecs":1800,"imageRetainSecs":120,"imageRetainBytes":67108864}));
        assert_valid("browser","profilePatch",&json!({"browserTools":false}));
    }

    #[test]
    fn profile_relay_runtimes_schema_snapshot_matches_real_profiles_and_presets() {
        use sha2::{Digest, Sha256};
        let schema = SCHEMAS.iter().find(|s| s.name == "profile-relay-runtimes").unwrap();
        assert_eq!(format!("{:x}", Sha256::digest(schema.text.as_bytes())), "b91254fa4b9e3890d2d6e94ba4b9c50634b12de3440d333eb1e7b505004be844");
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        for body in [
            json!({"name":"ds","kind":"apiKey","preset":"deepseek"}),
            json!({"name":"ds relay","kind":"apiKey","preset":"deepseek","baseUrl":"https://relay.example/v1"}),
            json!({"name":"gpt","kind":"chatgpt"}),
            json!({"name":"dsh","kind":"dsh","interfaceBaseUrls":{"deepseekNative":"https://api.deepseek.com/anthropic"}}),
            // profile-relay-runtimes wiring: relay addresses for DSH and Kimi Code.
            json!({"name":"dsh relay","kind":"dsh","interfaceBaseUrls":{"deepseekNative":"https://relay.example/anthropic"}}),
            json!({"name":"ds relay dsh","kind":"apiKey","preset":"deepseek","baseUrl":"https://relay.example/v1",
                "interfaceBaseUrls":{"deepseekNative":"https://relay.example/anthropic"}}),
            json!({"name":"relay_profile","kind":"apiKey","preset":"moonshot","baseUrl":"https://relay.example/v1"}),
        ] {
            let created = super::super::profiles_api::create_from_body(&serde_json::to_vec(&body).unwrap()).unwrap();
            assert_valid("profile-relay-runtimes", "profileFields", &created["profile"]);
        }
        for preset in crate::profiles::providers::presets_json().as_array().unwrap() {
            assert_valid("profile-relay-runtimes", "presetFields", preset);
        }
        assert_valid("profile-relay-runtimes", "interfaceBaseUrlsWrite", &json!({"interfaceBaseUrls":{"deepseekNative":null}}));
        assert_valid("profile-runtime-variants", "variantCreate", &json!({"runtime":"dsh","baseUrl":"https://relay.example/anthropic"}));
        assert_valid("profile-runtime-variants", "variantPatch", &json!({"baseUrl":null}));
        assert!(!validator("profile-relay-runtimes", "interfaceBaseUrlsWrite").unwrap()
            .is_valid(&json!({"interfaceBaseUrls":{"chatgptLogin":"https://x.example"}})));
    }

    #[test]
    fn every_schema_compiles_and_every_definition_is_reachable_by_name() {
        for schema in SCHEMAS {
            let value: Value = serde_json::from_str(schema.text).unwrap();
            assert_eq!(
                value["$schema"], "https://json-schema.org/draft/2020-12/schema",
                "{}",
                schema.name
            );
            assert_eq!(
                value["$id"],
                format!("urn:cutex:management-v2:{}", schema.name)
            );
            assert!(!schema.text.contains('\r'), "{} must stay LF", schema.name);
            jsonschema::options()
                .with_draft(jsonschema::Draft::Draft202012)
                .build(&value)
                .unwrap_or_else(|error| panic!("{}: {error}", schema.name));
            for definition in value["$defs"].as_object().unwrap().keys() {
                validator(schema.name, definition).unwrap();
            }
        }
    }

    #[test]
    fn ask_handover_view_fields_are_exported_optional_and_match_real_views(){
        use crate::asks::{model::route::{pin,Pin},store::{AskState,tests::new_ask}};
        let mut state=AskState::default();
        let (ask,_)=state.create_ask(new_ask("handover-schema","brief"),chrono::Utc::now()).unwrap();
        let schema:Value=serde_json::from_str(SCHEMAS.iter().find(|s|s.name=="asks").unwrap().text).unwrap();
        for shape in [&schema["$defs"]["askSummary"],&schema["$defs"]["askDetail"]["properties"]["ask"]]{
            for field in ["controller","originallyRequestedBy","controlError","controlHistorical"]{
                assert!(shape["properties"].get(field).is_some(),"exported property {field}");
                assert!(!shape["required"].as_array().is_some_and(|required|required.iter().any(|value|value==field)),"old records need no {field}");
            }
        }
        for controller in [Ok(Some("new-director".into())),Err("handover_pending_review")]{
            pin(&ask.ask_id,Pin{controller,history:vec![],questions:None});
            let summary=crate::asks::view::summary(&state,&ask);
            assert_valid("asks","askSummary",&summary);
            assert_valid("asks","askDetail",&json!({"ask":crate::asks::view::owner_view(&state,&ask)}));
            assert!(summary.get("controller").is_some() && summary.get("originallyRequestedBy").is_some());
        }
    }

    #[test]
    fn peer_wire_schemas_accept_additive_fields_and_unknown_states() {
        assert_valid(
            "peers",
            "createAsk",
            &json!({
                "schemaVersion":1,"requester":{"cutexSessionId":"remote","extra":true},
                "target":"worker","title":"brief","text":"hello","requestSha256":"digest",
                "futureOption":42,"options":{"ifBusy":"futureMode","futureOption":true}
            }),
        );
        assert_valid(
            "peers",
            "snapshot",
            &json!({
                "schemaVersion":1,"askId":"ask-fixture","revision":2,
                "execution":"futureState","hold":{"reason":"futureHold","extra":true},
                "closed":"futureOutcome","review":"futureReview","interruptReason":"futureReason",
                "extra":true,"answer":{"answerId":"answer-fixture","askId":"ask-fixture",
                    "attempt":1,"revision":1,"status":"futureStatus","bytes":0,"createdAt":"now","extra":true}
            }),
        );
        let value: Value =
            serde_json::from_str(SCHEMAS.iter().find(|s| s.name == "peers").unwrap().text).unwrap();
        assert_eq!(
            value["$defs"]["policy"]["properties"]["inbound"]["properties"]["review"]["default"],
            "always"
        );
        assert_eq!(
            value["$defs"]["settings"]["properties"]["connectTimeoutSecs"]["default"],
            5
        );
        assert_eq!(
            value["$defs"]["settings"]["properties"]["readTimeoutSecs"]["default"],
            30
        );
    }

    #[test]
    fn export_writes_schemas_and_index() {
        let dir =
            std::env::temp_dir().join(format!("cutex-schema-export-{}", uuid::Uuid::new_v4()));
        let written = export(&dir).unwrap();
        assert_eq!(written.len(), SCHEMAS.len() + 1);
        let index: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("index.json")).unwrap())
                .unwrap();
        let names: Vec<&str> = index["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        for expected in [
            "sessions",
            "bootstrap",
            "events",
            "approvals",
            "asks",
            "notifications",
            "summary",
            "pair",
            "runtime-packages",
        ] {
            assert!(names.contains(&expected), "{expected}");
            assert!(dir.join(format!("{expected}.json")).is_file());
        }
        assert!(index["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "schema-export"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn real_resources_match_their_schemas() {
        use crate::management::v2::model::{
            EventCorrelation, EventEnvelope, EventSource, NativeMessage, NativeMessageKind,
        };
        let envelope = EventEnvelope {
            contract_version: 2,
            event_id: "e1".into(),
            cursor: "c1".into(),
            stream_id: "s1".into(),
            sequence: 1,
            received_at: "2026-10-08T00:00:00Z".into(),
            cutex_session_id: "cutex.a".into(),
            host_id: "h".into(),
            source: EventSource::AppServer,
            sensitivity: "owner".into(),
            schema: None,
            correlation: EventCorrelation::default(),
            native: Some(NativeMessage {
                kind: NativeMessageKind::Notification,
                message: json!({"method":"turn/started","params":{}}),
            }),
            cutex: None,
        };
        assert_valid(
            "events",
            "eventEnvelope",
            &serde_json::to_value(&envelope).unwrap(),
        );

        let approval = crate::approvals::ApprovalRecord {
            approval_id: "ap1".into(),
            kind: crate::approvals::ApprovalKind::AgentProfile,
            reason: "profile_requires_approval".into(),
            requester_cutex_session_id: "cutex.a".into(),
            requester_name: Some("Eva".into()),
            project_id: None,
            summary: "Launch".into(),
            details: json!({}),
            request_sha256: "00".into(),
            state: crate::approvals::ApprovalState::Pending,
            created_at: "2026-10-08T00:00:00Z".into(),
            decided_at: None,
            note: None,
            decided_by: Some(crate::approvals::ApprovalDecider {
                by: "owner".into(),
                connection: Some("loopback".into()),
                peer: None,
                client: [("app".to_string(), "cutex-desktop".to_string()), ("input".to_string(), "pointer".to_string())].into(),
            }),
            consumed_at: None,
            wait: Some(crate::approvals::ApprovalWait {
                id: "w".into(),
                started_at: "2026-10-08T00:00:00Z".into(),
                until: "2026-10-08T00:02:00Z".into(),
                thread_id: Some("t".into()),
                turn_id: None,
                ended_at: None,
                end: None,
            }),
            risk: Some(crate::approvals::ApprovalRisk::Medium),
            matched_rule_id: None,
            auto_decision: false,
            rule_revision: None,
        };
        assert_valid(
            "approvals",
            "approvalList",
            &json!({"hostId": "h", "approvals": [approval]}),
        );

        let summary = crate::management::v2::summary::agent_row(
            &json!({
                "cutexSessionId": "cutex.a", "displayName": "Eva", "lifecycle": "active",
                "runtime": {"status": "online", "activeTurnId": null, "runtimeGeneration": 1},
            }),
            None,
            &[
                json!({"kind":"approval","id":"native:1","title":"ls","at":"2026-10-08T00:00:00Z"}),
                json!({"kind":"delegationFailed","id":"ask-1","title":"x","at":"2026-10-08T00:00:00Z","level":"needsYou"}),
            ],
            None,
        )
        .unwrap();
        assert_valid("summary", "agent", &summary);
        let mut bogus = summary.clone();
        bogus["status"] = json!("sleeping");
        assert!(
            !validator("summary", "agent").unwrap().is_valid(&bogus),
            "schemas must reject wrong values"
        );
        assert_valid(
            "summary",
            "summaryEvent",
            &json!({"name": "cutex/summary/agentChanged", "data": {"agent": summary}}),
        );
        assert_valid(
            "pair",
            "pairingCode",
            &json!({
                "code": "ABCD-EFGH", "expiresAt": "2026-10-08T00:10:00.000Z", "label": null, "hostId": "WINBOX", "instanceId": "ci-test-instance",
                "urls": ["http://100.64.0.27:24270"],
                "qr": crate::management::v2::pairing::qr_payload("WINBOX", "ci-test-instance", &["http://100.64.0.27:24270".to_string()], "ABCD-EFGH"),
            }),
        );
    }
}
