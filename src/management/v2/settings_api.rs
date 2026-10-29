//! Root-only global settings subset: `GET|PATCH /v2/settings`.
//!
//! Writes go through `GlobalConfigPatch` / `apply_global_config_patch`, the
//! same typed mutation the TUI Settings page and `cutex settings` use. Secrets
//! (Agent Bus token, Management token, proxy credentials) are never returned;
//! only presence booleans are.

use std::collections::BTreeMap;
use std::net::TcpStream;

use serde_json::{json, Map, Value};

use crate::config::agent_security::{
    AgentSecurityConfig, CrossProjectAccess, DomainLink, ExternalAgentGrant,
};
use crate::config::global_settings::{apply_global_config_patch, ConfigValueUpdate, GlobalConfigPatch};
use crate::config::store::{load_codez_config_checked, save_codez_config};
use crate::http::server::write_json_response;
use crate::http::server::SimpleHttpRequest;
use crate::notify::session::Level;
use crate::config::new_thread::{LaunchPermissions, NewThreadDefaults, ThreadKind};
use crate::profiles::model::{CodezConfig, NewSessionDefaults, ProxyConfig};

use super::profiles_api::{tri_state, visible_string};

pub const PATH: &str = "/v2/settings";

/// Fields whose new value only takes effect after the daemon restarts.
const RESTART_REQUIRED: &[&str] = &["agentBusEnabled", "agentBusPort", "managementListen"];
/// Fields only the loopback listener may change (a remote device token must
/// not open or close network listeners or widen CORS).
const LOOPBACK_ONLY: &[&str] = &["managementListen", "managementCorsOrigins", "peers"];
/// Fields applied when a runtime is next launched (running agents keep theirs).
const NEW_RUNTIMES_ONLY: &[&str] = &["proxy", "claudeCli", "claudeUsageRefreshMinutes", "agentHarness", "promptCacheKey"];

const PATCH_FIELDS: &[&str] = &[
    "defaultProfile",
    "defaultProfileDirectLaunch",
    "quickChatDefaultProfile",
    "newSessionDefaults",
    "newSessionNotification",
    "newThreadDefaults",
    "agentBusEnabled",
    "agentBusPort",
    "proxy",
    "claudeCli",
    "claudeUsageRefreshMinutes",
    "profileQuotaRefreshMinutes",
    "archiveDeleteEmpty",
    "queue",
    "imageInput",
    "autoThreadTitles",
    "threadTitleModel",
    "codexConfigFollowShared",
    // Pre-generalisation name of codexConfigFollowShared (same value).
    "codexPluginsFollowShared",
    "agentSecurity",
    "attachments",
    "documents",
    "managementListen",
    "managementCorsOrigins",
    "ownerNotify",
    "sessionStore",
    "notificationLevels",
    "usage",
    "agentHarness",
    "promptCacheKey",
    "agentBus",
    "artifacts",
    "peers",
    "browser",
    "workflows",
    "services",
];

/// Host proxy (`proxy-modes`): the default every Profile inherits and the proxy of cutex's own
/// outbound requests. `mode` system (nothing stored: daemon environment, then the Windows system
/// proxy; no PAC) | none | manual. Credentials are never returned (`urlHasCredentials`); loopback
/// always bypasses (`loopbackNoProxy`, not editable); `system` = what "follow system" resolves to
/// on this host right now; `editable` = this caller may change it (root credential, never a
/// client / device token).
pub(crate) fn proxy_view(proxy: Option<&ProxyConfig>) -> Value {
    use crate::config::proxy_outbound::{http_proxy_scheme, redacted_proxy_url, system_proxy, OutboundProxy};
    let mode = OutboundProxy::from_config(proxy);
    let (url, has_credentials) = proxy
        .and_then(|proxy| proxy.url.as_deref())
        .map(redacted_proxy_url)
        .map(|(url, creds)| (Some(url), creds))
        .unwrap_or((None, false));
    let warning = match &mode {
        OutboundProxy::Manual { url, .. } if !http_proxy_scheme(url) => Some(
            "cutex's own downloads (upgrades, components, quota readers) need an http:// or https:// proxy; a SOCKS URL only reaches the runtimes",
        ),
        _ => None,
    };
    json!({
        "mode": mode.mode(),
        "enabled": proxy.is_some_and(|proxy| proxy.enabled),
        "url": url,
        "urlHasCredentials": has_credentials,
        "noProxy": proxy.and_then(|proxy| proxy.no_proxy.clone()),
        "loopbackNoProxy": crate::config::proxy::LOOPBACK_NO_PROXY,
        "system": system_proxy().map(|resolved| json!({
            "url": redacted_proxy_url(&resolved.url).0,
            "source": resolved.source.as_str(),
        })),
        "pacSupported": false,
        "editable": proxy_editable(),
        "warning": warning,
    })
}

/// Proxy settings change where traffic goes: only the root credential (this machine, an SSH
/// tunnel, the desktop's host token) may change them, never a client / device token.
pub(crate) fn proxy_editable() -> bool {
    crate::management::listeners::current_client_token().is_none()
}

pub(crate) fn proxy_owner_required(stream: &mut TcpStream) -> anyhow::Result<()> {
    super::server::write_v2_error(
        stream,
        403,
        "Forbidden",
        "proxy_owner_credential_required",
        "proxy settings can only be changed with the host's own credential (not a client or device token)",
        false,
        json!({ "field": "proxy" }),
    )
}

/// Profile a quick chat (`POST /v2/human/sessions`) uses: the request's,
/// else `quick_chat_default_profile`, else the global `default_profile`.
pub fn quick_chat_profile(config: &CodezConfig, requested: Option<&str>) -> Option<String> {
    requested
        .map(str::to_string)
        .or_else(|| config.quick_chat_default_profile.clone())
        .or_else(|| config.default_profile.clone())
}

/// Read-only retention rule of every store this release prunes or keeps by
/// design (`/v2/settings` `retention`, capability `retention-rules`; owner
/// decision 2026-10-23: user data is not deleted on a timer by default).
/// Values come from the code constants and the current settings. Rows use
/// the store ids and `rule` vocabulary of cutex-desktop design 043:
/// `forever`, `age` (`days`), `archive` (moved into the record archive after
/// `days`), `internal` (bookkeeping, pruned automatically). `setting` names
/// the `/v2/settings` key that changes it (null: fixed in this release).
pub fn retention_view(config: &CodezConfig) -> Value {
    let documents = config.documents.effective();
    let row = |id: &str, rule: &str, days: Option<i64>, keep_referenced: bool, setting: Option<&str>, detail: &str| {
        json!({
            "id": id,
            "rule": rule,
            "days": days,
            "keepReferenced": keep_referenced,
            "configurable": setting.is_some(),
            "setting": setting.map(|key| json!({"store": "config", "key": key})),
            "detail": detail,
        })
    };
    let days_or_forever = |days: Option<i64>| if days.is_some() { "age" } else { "forever" };
    let closed = Some(i64::from(documents.closed_retention_days));
    let archive_days = (documents.archive_retention_days > 0).then(|| i64::from(documents.archive_retention_days));
    let workflow_days = crate::workflows::engine::FINISHED_RUN_RETENTION_DAYS;
    let revert_days = crate::files_revert::journal::APPLIED_JOURNAL_RETENTION_DAYS;
    json!([
        row("attachments", "forever", None, true, None,
            "Kept while their session exists (a transcript may show them); removed with the session. attachments.retentionDays is stored but not applied since r252."),
        row("asks", "archive", closed, false, Some("documents.closedRetentionDays"),
            "Closed delegations move into the record archive after this many days."),
        row("bus-messages", "archive", closed, false, Some("documents.closedRetentionDays"),
            "Finished agent messages move into the record archive after this many days."),
        row("owner-notify", "archive", closed, false, Some("documents.closedRetentionDays"),
            &format!(
                "Notifications the owner replied to move into the record archive (kind ownerNotification) after this many days; other finished notifications are deleted after {} days, at most {} records.",
                crate::owner_notify::store::RETENTION_DAYS,
                crate::owner_notify::store::MAX_RECORDS,
            )),
        row("archive", days_or_forever(archive_days), archive_days, false, Some("documents.archiveRetentionDays"),
            "Record archive months (0 = kept for good, the default since r252)."),
        row("documents", "internal", None, false, Some("documents.uploadGraceHours"),
            "Overflow documents go with their source (archived with it); one whose source never existed after uploadGraceHours."),
        row("workflow-runs", days_or_forever(workflow_days), workflow_days, false, None,
            "Finished workflow runs and their step sessions (kept for good since r252; was 30 days)."),
        row("history-reverts", days_or_forever(revert_days), revert_days, false, None,
            &format!(
                "Before-images of applied reverts (kept for good since r252; was 7 days). A rolled-back journal, already restored to disk, is removed after {} days.",
                crate::files_revert::journal::ROLLED_BACK_JOURNAL_RETENTION_DAYS,
            )),
        row("session-store", "internal", Some(i64::from(config.session_store.effective().receipt_retention_days)), false, Some("sessionStore.receiptRetentionDays"),
            "Completed explicit-launch receipts nothing needs."),
        row("user-input", "internal", Some(super::user_input::IDENTITY_RETENTION_DAYS), false, None,
            "Expired input identities only; queued inputs and unresolved native deliveries stay."),
        row("integration-dedupe", "internal", None, false, None,
            "Durable event identities prevent recovery from replaying committed owner events; pending events stay."),
        row("session-activity", "internal", None, false, None,
            "Derived activity projection removed when its session is deleted."),
        row("session-usage", "internal", None, false, None,
            "Derived usage projection removed when its session is deleted; usage ledger is kept forever."),
        row("alarms", "internal", Some(crate::agent_bus::alarms::CANCELLED_RETENTION_DAYS), false, None,
            "Cancelled alarms, this many days after their time. Sent alarms are kept."),
    ])
}

pub fn settings_view(config: &CodezConfig) -> Value {
    let session_store_view = config.session_store.effective().view();
    let usage_view = config.usage.view();
    let mut view = json!({
        "contractVersion": 2,
        "defaultProfile": config.default_profile,
        "defaultProfileDirectLaunch": config.default_profile_direct_launch,
        // Profile for quick chats created without one; null = defaultProfile.
        "quickChatDefaultProfile": config.quick_chat_default_profile,
        "quickChatEffectiveProfile": quick_chat_profile(config, None),
        "newSessionDefaults": config.new_session_defaults.iter().map(|(name, d)| {
            (name.clone(), json!({ "model": d.model, "reasoning": d.reasoning }))
        }).collect::<Map<_, _>>(),
        "newSessionNotification": config.new_session_notification,
        "newThreadDefaults": new_thread_defaults_view(&config.new_thread_defaults),
        "newThreadDefaultsBuiltin": {
            "agent": launch_view(&LaunchPermissions::builtin(ThreadKind::Agent)),
            "quickChat": launch_view(&LaunchPermissions::builtin(ThreadKind::QuickChat)),
        },
        "agentBusEnabled": config.agent_bus_enabled,
        "agentBusPort": config.agent_bus_port,
        "agentBusEffectivePort": crate::agent_bus::service::agent_bus_port(config),
        "proxy": proxy_view(config.proxy.as_ref()),
        "claudeUsageRefreshMinutes": config.claude_usage_refresh_minutes,
        "profileQuotaRefreshMinutes": config.profile_quota_refresh_minutes,
        "archiveDeleteEmpty": config.archive_delete_empty,
        "queue": { "ownerMayPassAsks": config.queue.owner_may_pass_asks },
        "queueDefault": { "ownerMayPassAsks": true },
        // Generated titles for unnamed quick chats (profile model when null
        // and no ChatGPT login: see cli_app/thread_titles.rs).
        "autoThreadTitles": config.auto_thread_titles,
        "threadTitleModel": config.thread_title_model,
        // Codex agents with their own native home follow the shared home's
        // plugins / MCP servers / skill toggles / hook trust at launch
        // (launch/shared_config.rs). codexPluginsFollowShared: old name.
        "codexConfigFollowShared": config.codex_config_follow_shared,
        "codexPluginsFollowShared": config.codex_config_follow_shared,
        "agentSecurity": agent_security_view(&config.agent_security),
        // POST /v2/sessions/{id}/attachments: upload cap and retention.
        "attachments": {
            "maxBytes": config.attachments.max_bytes,
            "retentionDays": config.attachments.retention_days,
            "maxBytesRange": [
                crate::config::attachments::MIN_MAX_BYTES,
                crate::config::attachments::MAX_MAX_BYTES,
            ],
        },
        // Input overflow → documents (capability input-overflow-documents).
        "documents": config.documents,
        "documentsDefault": crate::config::documents::DocumentSettings::default(),
        "documentsRange": crate::config::documents::DocumentSettings::default().view()["documentsRange"].clone(),
        // Network listeners (capability listen-settings). null = default
        // (loopback only); applied at the next daemon start.
        "managementListen": config.management_listen,
        "managementListenDefault": crate::management::listeners::DEFAULT_LISTEN,
        "managementListenEntries": listen_entries_view(config),
        "managementListenActiveRemote": crate::management::listeners::active_remote_listeners()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        // Exact Origin allow-list for browser / Capacitor clients; null =
        // the defaults. Applied at once.
        "managementCorsOrigins": config.management_cors_origins,
        "managementCorsOriginsDefault": crate::management::listeners::DEFAULT_CORS_ORIGINS,
        "remoteAccess": remote_access_view(),
        // Agent → owner messages (capability owner-notify).
        "ownerNotify": owner_notify_view(config),
        // Session store maintenance (capability session-store-retention):
        // completed explicit-launch receipts older than receiptRetentionDays
        // that nothing needs are pruned (0 = only receipts still in use).
        "sessionStore": session_store_view["sessionStore"].clone(),
        "sessionStoreDefault": session_store_view["sessionStoreDefault"].clone(),
        "sessionStoreRange": session_store_view["sessionStoreRange"].clone(),
        "secretsPresent": {
            "agentBusToken": config.agent_bus_token.as_deref().is_some_and(|t| !t.is_empty()),
            "managementApiToken": config.management_api_token.as_ref().is_some_and(|t| !t.as_str().is_empty()),
        },
        "restartRequiredFields": RESTART_REQUIRED,
        "newRuntimesOnlyFields": NEW_RUNTIMES_ONLY,
    });
    view["imageInput"] = json!(config.image_input);
    view["imageInputDefault"] = json!(super::user_input::image_steering::ImageInputSettings::default());
    view["claudeCli"] = json!(config.claude_cli);
    view["claudeCliDefault"] = json!(crate::config::claude::ClaudeCliChoice::default());
    // Notification levels (capability notification-levels): stored presets /
    // changed cells + the effective presets. Registry and full tables:
    // GET /v2/notification-levels.
    view["notificationLevels"] = notification_levels_view(config);
    // Read-only retention rules (capability retention-rules; not patchable).
    view["retention"] = retention_view(config);
    // Usage cost estimates (capabilities usage-price-catalog, usage-currency);
    // outside json! (macro recursion limit).
    for key in ["usage", "usageDefault", "usageOptions"] {
        view[key] = usage_view[key].clone();
    }
    // The `# Cutex` launch section (capability agent-harness); preview the
    // rendered text with GET /v2/sessions/{id}/harness.
    view["agentHarness"] = serde_json::to_value(config.agent_harness).unwrap_or_default();
    view["agentHarnessDefault"] =
        serde_json::to_value(crate::config::agent_harness::AgentHarnessSettings::default()).unwrap_or_default();
    // Opt-in stable prompt cache key for native agent launches (design 034);
    // applies to each runtime's next launch, per workspace.
    view["promptCacheKey"] = serde_json::to_value(config.prompt_cache_key).unwrap_or_default();
    view["promptCacheKeyDefault"] =
        serde_json::to_value(crate::config::prompt_cache_key::PromptCacheKeySettings::default()).unwrap_or_default();
    // off | pending (no launch since) | active | unsupported (this native has
    // no prompt cache key) | probe_failed (launches went without the key).
    view["promptCacheKeyStatus"] =
        serde_json::json!(crate::launch::prompt_cache_key::settings_status(config.prompt_cache_key.enabled));
    // Wake / delegation result timing for a busy recipient (capability
    // wake-busy-wait): agentBus {wakeBusyWaitSecs: seconds | null = never
    // early}; read per delivery (applies at once).
    let agent_bus_view = config.agent_bus.view();
    for key in ["agentBus", "agentBusDefault", "agentBusRange"] {
        view[key] = agent_bus_view[key].clone();
    }
    // Artifact store limits and project auto capture (capability artifacts);
    // read per publish / scan (applies at once).
    let artifacts_view = config.artifacts.effective().view();
    for key in ["artifacts", "artifactsDefault", "artifactsRange"] {
        view[key] = artifacts_view[key].clone();
    }
    // Workflow limits / approval (capability workflows); read per submission.
    let workflows_view = config.workflows.view();
    for key in ["workflows", "workflowsDefault", "workflowsRange"] {
        view[key] = workflows_view[key].clone();
    }
    view["peers"] = serde_json::to_value(&config.peers).unwrap_or_default();
    view["browser"] = config.browser.effective().view();
    view["browserDefault"] = crate::config::browser::BrowserSettings::default().view();
    view
}

fn listen_entries_view(config: &CodezConfig) -> Value {
    let entries: Vec<String> = config
        .management_listen
        .clone()
        .filter(|list| !list.is_empty())
        .unwrap_or_else(|| crate::management::listeners::DEFAULT_LISTEN.iter().map(|s| s.to_string()).collect());
    match crate::management::listeners::validate_listen_entries(
        &entries,
        crate::management::listeners::default_listen_port(),
    ) {
        Ok(rows) => Value::Array(rows),
        Err(message) => json!({ "error": message }),
    }
}

fn owner_notify_view(config: &CodezConfig) -> Value {
    use crate::config::owner_notify::{OwnerNotifyDelivery, AUDIENCES, CHANNELS};
    let settings = config.owner_notify.effective();
    let defaults = OwnerNotifyDelivery::default();
    let phone_paired = crate::management::client_tokens::ClientTokenStore::open_default()
        .and_then(|store| store.list())
        .is_ok_and(|tokens| tokens.iter().any(|token| token.kind == "device"));
    json!({
        "delivery": {
            "info": settings.delivery.info,
            "attention": settings.delivery.attention,
            "urgent": settings.delivery.urgent,
        },
        "deliveryDefault": {"info": defaults.info, "attention": defaults.attention, "urgent": defaults.urgent},
        "channels": CHANNELS,
        "bigAlertSound": settings.big_alert_sound,
        "toolAudience": settings.tool_audience,
        "toolAudienceOptions": AUDIENCES,
        "rateLimit": {"perHour": settings.rate_limit.per_hour, "urgentPerHour": settings.rate_limit.urgent_per_hour},
        "agentOverrides": settings.agent_overrides,
        "phonePaired": phone_paired,
    })
}

fn notification_levels_view(config: &CodezConfig) -> Value {
    let registry = crate::notification_levels::registry();
    let owner = config.owner_notify.effective();
    let resolved = config.notification_levels.resolve(&registry, &owner);
    let mut view = config.notification_levels.view();
    view["effectiveEventPreset"] = json!(resolved.event_preset_effective);
    view["effectiveMethodPreset"] = json!(resolved.method_preset_effective);
    view
}

/// Partial `ownerNotify` update applied onto the stored settings.
fn apply_owner_notify(
    current: &crate::config::owner_notify::OwnerNotifySettings,
    value: &Value,
) -> Result<crate::config::owner_notify::OwnerNotifySettings, String> {
    use crate::config::owner_notify::LEVELS;
    let object = value.as_object().ok_or("ownerNotify: must be an object")?;
    if let Some(key) = object.keys().find(|key| {
        !["delivery", "bigAlertSound", "toolAudience", "rateLimit", "agentOverrides"].contains(&key.as_str())
    }) {
        return Err(format!("ownerNotify: unsupported field {key}"));
    }
    let mut next = current.effective();
    if let Some(delivery) = object.get("delivery") {
        let delivery = delivery.as_object().ok_or("ownerNotify.delivery: must be an object")?;
        for (level, channels) in delivery {
            if !LEVELS.contains(&level.as_str()) {
                return Err(format!("ownerNotify.delivery: unknown level {level}"));
            }
            let channels = string_list(channels, &format!("ownerNotify.delivery.{level}"))?;
            next.delivery.set_level(level, channels);
        }
    }
    if let Some(sound) = object.get("bigAlertSound") {
        next.big_alert_sound = sound.as_bool().ok_or("ownerNotify.bigAlertSound: must be a boolean")?;
    }
    if let Some(audience) = object.get("toolAudience") {
        next.tool_audience = audience.as_str().ok_or("ownerNotify.toolAudience: must be a string")?.to_string();
    }
    if let Some(limit) = object.get("rateLimit") {
        let limit = limit.as_object().ok_or("ownerNotify.rateLimit: must be an object")?;
        for (key, value) in limit {
            let number = value
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| format!("ownerNotify.rateLimit.{key}: must be a whole number"))?;
            match key.as_str() {
                "perHour" => next.rate_limit.per_hour = number,
                "urgentPerHour" => next.rate_limit.urgent_per_hour = number,
                _ => return Err(format!("ownerNotify.rateLimit: unsupported field {key}")),
            }
        }
    }
    if let Some(overrides) = object.get("agentOverrides") {
        let overrides = overrides.as_object().ok_or("ownerNotify.agentOverrides: must be an object")?;
        let mut map = BTreeMap::new();
        for (agent, value) in overrides {
            match value {
                Value::Null => {}
                Value::String(mode) => {
                    map.insert(agent.clone(), mode.clone());
                }
                _ => return Err(format!("ownerNotify.agentOverrides.{agent}: must be mute, force or null")),
            }
        }
        next.agent_overrides = map;
    }
    next.validate().map_err(|message| format!("ownerNotify.{message}"))?;
    Ok(next)
}

/// Remote clients that could use a remote listener: the UI warns when there
/// are none (a listener without tokens only refuses requests).
fn remote_access_view() -> Value {
    match crate::management::client_tokens::ClientTokenStore::open_default().and_then(|store| store.list()) {
        Ok(tokens) => json!({
            "clientTokens": tokens.len(),
            "devices": tokens.iter().filter(|token| token.kind == "device").count(),
        }),
        Err(_) => json!({ "clientTokens": null, "devices": null }),
    }
}

fn string_list(value: &Value, field: &str) -> Result<Vec<String>, String> {
    value
        .as_array()
        .ok_or(format!("{field}: must be an array of strings or null"))?
        .iter()
        .map(|item| {
            item.as_str()
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty() && text.len() <= 200)
                .ok_or(format!("{field}: entries must be non-empty strings"))
        })
        .collect()
}

fn launch_view(permissions: &LaunchPermissions) -> Value {
    json!({
        "sandbox": permissions.sandbox,
        "approvalPolicy": permissions.approval,
        "approvalsReviewer": permissions.approvals_reviewer,
        "networkAccess": permissions.network_access,
        "writableRoots": permissions.writable_roots,
    })
}

/// Effective defaults per kind (configured fields over built-ins).
fn new_thread_defaults_view(defaults: &NewThreadDefaults) -> Value {
    json!({
        "agent": launch_view(&defaults.resolve(ThreadKind::Agent)),
        "quickChat": launch_view(&defaults.resolve(ThreadKind::QuickChat)),
    })
}

/// `newThreadDefaults` patch: per kind, `null` resets the whole kind; inside
/// a kind object an absent field is unchanged and `null` resets that field
/// to the built-in default.
#[derive(Debug, Default)]
pub(super) struct NewThreadDefaultsPatch {
    kinds: Vec<(ThreadKind, Option<Map<String, Value>>)>,
}

fn parse_new_thread_defaults(value: &Value) -> Result<NewThreadDefaultsPatch, String> {
    let map = value
        .as_object()
        .ok_or("newThreadDefaults: must be an object with agent and/or quickChat")?;
    let mut patch = NewThreadDefaultsPatch::default();
    for (key, entry) in map {
        let kind = match key.as_str() {
            "agent" => ThreadKind::Agent,
            "quickChat" => ThreadKind::QuickChat,
            other => return Err(format!("newThreadDefaults: unsupported kind {other}")),
        };
        match entry {
            Value::Null => patch.kinds.push((kind, None)),
            Value::Object(fields) => {
                if let Some(field) = fields.keys().find(|k| {
                    !["sandbox", "approvalPolicy", "approvalsReviewer", "networkAccess", "writableRoots"]
                        .contains(&k.as_str())
                }) {
                    return Err(format!("newThreadDefaults.{key}: unsupported field {field}"));
                }
                patch.kinds.push((kind, Some(fields.clone())));
            }
            _ => return Err(format!("newThreadDefaults.{key}: must be an object or null")),
        }
    }
    // Shape-check against empty defaults so bad values fail before any write.
    apply_new_thread_defaults(&NewThreadDefaults::default(), &patch)?;
    Ok(patch)
}

fn apply_new_thread_defaults(
    current: &NewThreadDefaults,
    patch: &NewThreadDefaultsPatch,
) -> Result<NewThreadDefaults, String> {
    let mut next = current.clone();
    for (kind, fields) in &patch.kinds {
        let name = match kind {
            ThreadKind::Agent => "agent",
            ThreadKind::QuickChat => "quickChat",
        };
        let Some(fields) = fields else {
            *next.configured_mut(*kind) = Default::default();
            continue;
        };
        let target = next.configured_mut(*kind);
        let text = |field: &str, value: &Value| -> Result<Option<String>, String> {
            match value {
                Value::Null => Ok(None),
                Value::String(text) => Ok(Some(text.clone())),
                _ => Err(format!("newThreadDefaults.{name}.{field}: must be a string or null")),
            }
        };
        for (field, value) in fields {
            match field.as_str() {
                "sandbox" => target.sandbox = text(field, value)?,
                "approvalPolicy" => target.approval = text(field, value)?,
                "approvalsReviewer" => {
                    target.approvals_reviewer = text(field, value)?
                        .map(|reviewer| {
                            crate::config::new_thread::canonical_approvals_reviewer(&reviewer)
                                .map(str::to_string)
                                .ok_or(format!(
                                    "newThreadDefaults.{name}.approvalsReviewer: must be one of {}",
                                    crate::config::new_thread::APPROVALS_REVIEWERS.join(", ")
                                ))
                        })
                        .transpose()?
                }
                "networkAccess" => {
                    target.network_access = match value {
                        Value::Null => None,
                        Value::Bool(flag) => Some(*flag),
                        _ => return Err(format!("newThreadDefaults.{name}.networkAccess: must be a boolean or null")),
                    }
                }
                "writableRoots" => {
                    target.writable_roots = match value {
                        Value::Null => None,
                        Value::Array(items) => Some(
                            items
                                .iter()
                                .map(|item| item.as_str().map(str::to_string))
                                .collect::<Option<Vec<_>>>()
                                .ok_or_else(|| {
                                    format!("newThreadDefaults.{name}.writableRoots: must be an array of paths")
                                })?,
                        ),
                        _ => return Err(format!("newThreadDefaults.{name}.writableRoots: must be an array or null")),
                    }
                }
                _ => unreachable!("fields checked by parse"),
            }
        }
    }
    next.validate().map_err(|error| error.to_string())?;
    Ok(next)
}

fn bad_request(stream: &mut TcpStream, message: &str) -> anyhow::Result<()> {
    super::server::write_v2_error(stream, 400, "Bad Request", "invalid_request", message, false, json!({}))
}

fn required_bool(body: &Map<String, Value>, key: &str) -> Result<Option<bool>, String> {
    match body.get(key) {
        None => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("{key}: must be a boolean")),
    }
}

/// `codexConfigFollowShared`, or its old name `codexPluginsFollowShared`.
fn follow_shared(body: &Map<String, Value>) -> Result<Option<bool>, String> {
    match (
        required_bool(body, "codexConfigFollowShared")?,
        required_bool(body, "codexPluginsFollowShared")?,
    ) {
        (Some(new), Some(old)) if new != old => {
            Err("codexConfigFollowShared and codexPluginsFollowShared disagree".to_string())
        }
        (new, old) => Ok(new.or(old)),
    }
}

/// `agentSecurity`: how far agents reach beyond their own project (see
/// cutex `docs/agent-tool-security.md`). Applies to the next Agent Bus
/// request; no restart.
fn agent_security_view(security: &AgentSecurityConfig) -> Value {
    json!({
        "crossProjectAccess": security.cross_project.as_str(),
        "crossProjectAccessOptions": ["isolated", "host"],
        "crossProjectAccessDefault": CrossProjectAccess::default().as_str(),
        "links": security.links,
        "maxLinks": crate::config::agent_security::MAX_DOMAIN_LINKS,
        "externalAgents": security.external_agents,
        "maxExternalAgents": crate::config::agent_security::MAX_EXTERNAL_AGENTS,
    })
}

/// Partial `agentSecurity` update; omitted fields keep their stored value.
#[derive(Debug, Default)]
pub(super) struct AgentSecurityPatch {
    cross_project: Option<CrossProjectAccess>,
    links: Option<Vec<DomainLink>>,
    external_agents: Option<Vec<ExternalAgentGrant>>,
}

fn parse_agent_security(value: &Value) -> Result<AgentSecurityPatch, String> {
    let object = value.as_object().ok_or("agentSecurity: must be an object")?;
    if let Some(key) = object
        .keys()
        .find(|key| {
            !["crossProjectAccess", "links", "externalAgents"].contains(&key.as_str())
        })
    {
        return Err(format!("agentSecurity: unsupported field {key}"));
    }
    let cross_project = object
        .get("crossProjectAccess")
        .map(|value| {
            value
                .as_str()
                .and_then(CrossProjectAccess::parse)
                .ok_or("agentSecurity.crossProjectAccess: must be isolated or host")
        })
        .transpose()?;
    let links = object
        .get("links")
        .map(|value| {
            serde_json::from_value::<Vec<DomainLink>>(value.clone()).map_err(|error| {
                format!(
                    "agentSecurity.links: must be [[\"project:<id>\"|\"agent:<cutex_session_id>\", ...], ...]: {error}"
                )
            })
        })
        .transpose()?;
    let external_agents = object
        .get("externalAgents")
        .map(|value| {
            serde_json::from_value::<Vec<ExternalAgentGrant>>(value.clone()).map_err(|error| {
                format!(
                    "agentSecurity.externalAgents: must be [{{\"project\": \"<project id>\", \"agent\": \"<cutex_session_id>\"}}, ...]: {error}"
                )
            })
        })
        .transpose()?;
    let patch = AgentSecurityPatch {
        cross_project,
        links,
        external_agents,
    };
    // Validate the parts we can check without the stored value (each list
    // is replaced as a whole, so it is validated on its own).
    AgentSecurityConfig {
        cross_project: CrossProjectAccess::default(),
        links: patch.links.clone().unwrap_or_default(),
        external_agents: patch.external_agents.clone().unwrap_or_default(),
    }
    .validate()
    .map_err(|error| error.to_string())?;
    Ok(patch)
}

/// Validated request plus the profile names it refers to (checked against the
/// store at apply time).
#[derive(Debug, Default)]
pub(super) struct SettingsRequest {
    pub(super) patch: GlobalConfigPatch,
    pub(super) touched: Vec<String>,
    pub(super) proxy: Option<Option<Map<String, Value>>>,
    pub(super) new_thread_defaults: Option<NewThreadDefaultsPatch>,
    pub(super) agent_security: Option<AgentSecurityPatch>,
    /// Partial `attachments` update: (maxBytes, retentionDays).
    pub(super) attachments: Option<(Option<u64>, Option<u32>)>,
    /// Partial `documents` update (field → value).
    pub(super) documents: Option<Value>,
    /// `managementListen`: None = unchanged, Some(None) = default.
    pub(super) listen: Option<Option<Vec<String>>>,
    pub(super) cors_origins: Option<Option<Vec<String>>>,
    /// Partial `ownerNotify` update (validated again against the stored value).
    pub(super) owner_notify: Option<Value>,
    /// Partial `sessionStore` update (validated again against the stored value).
    pub(super) session_store: Option<Value>,
    /// Partial `notificationLevels` update (validated again against the stored value).
    pub(super) notification_levels: Option<Value>,
    /// Partial `usage` update (validated again against the stored value).
    pub(super) usage: Option<Value>,
    /// Partial `agentHarness` update (validated again against the stored value).
    pub(super) agent_harness: Option<Value>,
    /// Partial `promptCacheKey` update (validated again against the stored value).
    pub(super) prompt_cache_key: Option<Value>,
    /// Partial `agentBus` update (validated again against the stored value).
    pub(super) agent_bus: Option<Value>,
    /// Partial `artifacts` update (validated again against the stored value).
    pub(super) artifacts: Option<Value>,
    pub(super) peers: Option<Value>,
    pub(super) browser: Option<Value>,
    /// Partial `workflows` update (validated again against the stored value).
    pub(super) workflows: Option<Value>,
    /// Service registry lifecycle settings. Kept in the services store, not
    /// duplicated into CodezConfig.
    pub(super) services: Option<crate::services::store::SettingsPatch>,
}

fn parse_attachments(value: &Value) -> Result<(Option<u64>, Option<u32>), String> {
    use crate::config::attachments::{MAX_MAX_BYTES, MAX_RETENTION_DAYS, MIN_MAX_BYTES};
    let object = value.as_object().ok_or("attachments: must be an object")?;
    if let Some(key) = object
        .keys()
        .find(|key| !["maxBytes", "retentionDays"].contains(&key.as_str()))
    {
        return Err(format!("attachments: unsupported field {key}"));
    }
    let max_bytes = object
        .get("maxBytes")
        .map(|value| {
            value
                .as_u64()
                .filter(|bytes| (MIN_MAX_BYTES..=MAX_MAX_BYTES).contains(bytes))
                .ok_or(format!("attachments.maxBytes: must be {MIN_MAX_BYTES}-{MAX_MAX_BYTES}"))
        })
        .transpose()?;
    let retention_days = object
        .get("retentionDays")
        .map(|value| {
            value
                .as_u64()
                .filter(|days| *days <= u64::from(MAX_RETENTION_DAYS))
                .map(|days| days as u32)
                .ok_or(format!(
                    "attachments.retentionDays: must be 0-{MAX_RETENTION_DAYS} (0 keeps files)"
                ))
        })
        .transpose()?;
    Ok((max_bytes, retention_days))
}

pub(super) fn parse_patch(body: &[u8]) -> Result<SettingsRequest, String> {
    let value: Value =
        serde_json::from_slice(body).map_err(|error| format!("JSON object body required: {error}"))?;
    let body = value.as_object().ok_or("JSON object body required")?;
    if let Some(unknown) = body.keys().find(|key| !PATCH_FIELDS.contains(&key.as_str())) {
        return Err(format!("unsupported field: {unknown}"));
    }
    if body.contains_key("services") && body.len() != 1 {
        return Err("services must be patched on its own".into());
    }
    let mut patch = GlobalConfigPatch {
        default_profile: tri_state(body, "defaultProfile", visible_string(200))?,
        default_profile_direct_launch: required_bool(body, "defaultProfileDirectLaunch")?,
        archive_delete_empty: required_bool(body, "archiveDeleteEmpty")?,
        auto_thread_titles: required_bool(body, "autoThreadTitles")?,
        codex_config_follow_shared: follow_shared(body)?,
        quick_chat_default_profile: tri_state(body, "quickChatDefaultProfile", visible_string(200))?,
        thread_title_model: tri_state(body, "threadTitleModel", visible_string(200))?,
        agent_bus_enabled: required_bool(body, "agentBusEnabled")?,
        agent_bus_port: tri_state(body, "agentBusPort", |value| {
            let port = value
                .as_u64()
                .and_then(|port| u16::try_from(port).ok())
                .ok_or("must be a port number or null")?;
            crate::agent_bus::service::validate_agent_bus_port(port).map_err(|e| e.to_string())?;
            Ok(port)
        })?,
        claude_cli: body
            .get("claudeCli")
            .map(|value| serde_json::from_value(value.clone()).map_err(|_| "claudeCli: must be bundled or system"))
            .transpose()?,
        ..GlobalConfigPatch::default()
    };
    if let Some(value) = body.get("imageInput") {
        let object = value.as_object().ok_or("imageInput: must be an object")?;
        // An empty patch never resets a previously selected steer policy.
        if !object.is_empty() {
            patch.image_input = Some(serde_json::from_value(value.clone())
                .map_err(|_| "imageInput: requires busyBehavior queue or steer, with no unknown fields")?);
        }
    }
    if let Some(value) = body.get("queue") {
        let queue = value.as_object().ok_or("queue: must be an object")?;
        if let Some(unknown) = queue.keys().find(|key| key.as_str() != "ownerMayPassAsks") {
            return Err(format!("queue: unsupported field {unknown}"));
        }
        patch.queue_owner_may_pass_asks = required_bool(queue, "ownerMayPassAsks")?;
    }
    if let Some(value) = body.get("newSessionNotification") {
        patch.new_session_notification = Some(
            serde_json::from_value::<Level>(value.clone())
                .map_err(|_| "newSessionNotification: must be important, normal or off")?,
        );
    }
    if let Some(value) = body.get("claudeUsageRefreshMinutes") {
        let minutes = value
            .as_u64()
            .filter(|m| (1..=60).contains(m))
            .ok_or("claudeUsageRefreshMinutes: must be 1-60")?;
        patch.claude_usage_refresh_minutes = Some(minutes as u8);
    }
    if let Some(value) = body.get("profileQuotaRefreshMinutes") {
        let minutes = value
            .as_u64()
            .filter(|m| *m <= 60)
            .ok_or("profileQuotaRefreshMinutes: must be 0-60 (0 disables)")?;
        patch.profile_quota_refresh_minutes = Some(minutes as u8);
    }
    if let Some(value) = body.get("newSessionDefaults") {
        let map = value
            .as_object()
            .ok_or("newSessionDefaults: must be an object keyed by profile name")?;
        let mut defaults = BTreeMap::new();
        for (name, entry) in map {
            let entry = entry
                .as_object()
                .ok_or_else(|| format!("newSessionDefaults.{name}: must be an object"))?;
            if let Some(key) = entry.keys().find(|k| *k != "model" && *k != "reasoning") {
                return Err(format!("newSessionDefaults.{name}: unsupported field {key}"));
            }
            let field = |key: &str| -> Result<Option<String>, String> {
                match entry.get(key) {
                    None | Some(Value::Null) => Ok(None),
                    Some(value) => visible_string(200)(value)
                        .map(Some)
                        .map_err(|m| format!("newSessionDefaults.{name}.{key}: {m}")),
                }
            };
            let item = NewSessionDefaults { model: field("model")?, reasoning: field("reasoning")? };
            if item != NewSessionDefaults::default() {
                defaults.insert(name.clone(), item);
            }
        }
        patch.new_session_defaults = Some(defaults);
    }
    let proxy = match body.get("proxy") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(Value::Object(map)) => {
            if let Some(key) = map
                .keys()
                .find(|k| !["mode", "enabled", "url", "noProxy"].contains(&k.as_str()))
            {
                return Err(format!("proxy: unsupported field {key}"));
            }
            Some(Some(map.clone()))
        }
        Some(_) => return Err("proxy: must be an object or null".into()),
    };
    let new_thread_defaults = body
        .get("newThreadDefaults")
        .map(parse_new_thread_defaults)
        .transpose()?;
    let agent_security = body
        .get("agentSecurity")
        .map(parse_agent_security)
        .transpose()?;
    let attachments = body.get("attachments").map(parse_attachments).transpose()?;
    let documents = match body.get("documents") {
        None => None,
        Some(value) => {
            crate::config::documents::DocumentSettings::default().patched(value)?;
            Some(value.clone())
        }
    };
    let listen = match body.get("managementListen") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(value) => {
            let entries = string_list(value, "managementListen")?;
            crate::management::listeners::validate_listen_entries(
                &entries,
                crate::management::listeners::default_listen_port(),
            )?;
            Some(Some(entries))
        }
    };
    let cors_origins = match body.get("managementCorsOrigins") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(value) => Some(Some(
            crate::management::listeners::validate_cors_origins(&string_list(value, "managementCorsOrigins")?)
                .map_err(|message| format!("managementCorsOrigins: {message}"))?,
        )),
    };
    let owner_notify = match body.get("ownerNotify") {
        None => None,
        Some(value) => {
            apply_owner_notify(&Default::default(), value)?;
            Some(value.clone())
        }
    };
    let session_store = match body.get("sessionStore") {
        None => None,
        Some(value) => {
            crate::config::session_store::SessionStoreSettings::default().patched(value)?;
            Some(value.clone())
        }
    };
    let notification_levels = match body.get("notificationLevels") {
        None => None,
        Some(value) => {
            crate::config::notification_levels::NotificationLevelSettings::default().patched(
                value,
                &crate::notification_levels::registry(),
                &Default::default(),
            )?;
            Some(value.clone())
        }
    };
    let usage = match body.get("usage") {
        None => None,
        Some(value) => {
            crate::config::usage_pricing::UsageSettings::default().patched(value)?;
            Some(value.clone())
        }
    };
    let agent_harness = match body.get("agentHarness") {
        None => None,
        Some(value) => {
            crate::config::agent_harness::AgentHarnessSettings::default().patched(value)?;
            Some(value.clone())
        }
    };
    let prompt_cache_key = match body.get("promptCacheKey") {
        None => None,
        Some(value) => {
            crate::config::prompt_cache_key::PromptCacheKeySettings::default().patched(value)?;
            Some(value.clone())
        }
    };
    let agent_bus = match body.get("agentBus") {
        None => None,
        Some(value) => {
            crate::config::agent_bus::AgentBusSettings::default().patched(value)?;
            Some(value.clone())
        }
    };
    let artifacts = match body.get("artifacts") {
        None => None,
        Some(value) => {
            crate::config::artifacts::ArtifactSettings::default().patched(value)?;
            Some(value.clone())
        }
    };
    Ok(SettingsRequest {
        services: body
            .get("services")
            .map(|value| {
                serde_json::from_value(value.clone())
                    .map_err(|error| format!("services: invalid settings patch: {error}"))
            })
            .transpose()?,
        workflows: body.get("workflows").map(|value| crate::workflows::WorkflowSettings::default().patched(value).map(|_| value.clone())).transpose()?,
        browser: body.get("browser").map(|value| crate::config::browser::BrowserSettings::default().patched(value).map(|_| value.clone())).transpose()?,
        peers: body.get("peers").map(|value| crate::peers::policy::Settings::default().patched(value).map(|_|value.clone()).map_err(|e|e.to_string())).transpose()?,
        artifacts,
        agent_harness,
        prompt_cache_key,
        agent_bus,
        notification_levels,
        listen,
        cors_origins,
        owner_notify,
        session_store,
        usage,
        patch,
        touched: body.keys().cloned().collect(),
        proxy,
        new_thread_defaults,
        agent_security,
        attachments,
        documents,
    })
}

/// Merge a partial proxy object onto the current proxy; `None` = follow system (nothing stored).
pub(crate) fn merged_proxy(
    current: Option<&ProxyConfig>,
    update: &Map<String, Value>,
) -> anyhow::Result<Option<ProxyConfig>> {
    crate::config::proxy::apply_proxy_update(current, update, "system")
}

fn apply(request: SettingsRequest) -> anyhow::Result<(CodezConfig, bool)> {
    let mut patch = request.patch;
    let store = crate::profiles::settings_api::load_store()?;
    if let ConfigValueUpdate::Set(target) = &patch.default_profile {
        let account = crate::profiles::settings_api::find(&store, target)
            .ok_or_else(|| anyhow::anyhow!("defaultProfile: no profile has this ID or name"))?;
        patch.default_profile = ConfigValueUpdate::Set(account.name.clone());
    }
    if let ConfigValueUpdate::Set(target) = &patch.quick_chat_default_profile {
        let account = crate::profiles::settings_api::find(&store, target)
            .ok_or_else(|| anyhow::anyhow!("quickChatDefaultProfile: no profile has this ID or name"))?;
        patch.quick_chat_default_profile = ConfigValueUpdate::Set(account.name.clone());
    }
    if let Some(defaults) = &patch.new_session_defaults {
        if let Some(missing) = defaults
            .keys()
            .find(|name| !store.accounts.iter().any(|account| &account.name == *name))
        {
            anyhow::bail!("newSessionDefaults: unknown profile name {missing}");
        }
    }
    let mut config = load_codez_config_checked()?;
    patch.proxy = match &request.proxy {
        None => ConfigValueUpdate::Unchanged,
        Some(None) => ConfigValueUpdate::Clear,
        Some(Some(update)) => match merged_proxy(config.proxy.as_ref(), update)? {
            Some(proxy) => ConfigValueUpdate::Set(proxy),
            None => ConfigValueUpdate::Clear,
        },
    };
    if let Some(defaults) = &request.new_thread_defaults {
        patch.new_thread_defaults = Some(
            apply_new_thread_defaults(&config.new_thread_defaults, defaults)
                .map_err(|message| anyhow::anyhow!(message))?,
        );
    }
    if let Some(update) = &request.agent_security {
        let mut security = config.agent_security.clone();
        if let Some(access) = update.cross_project {
            security.cross_project = access;
        }
        if let Some(links) = &update.links {
            security.links = links.clone();
        }
        if let Some(grants) = &update.external_agents {
            security.external_agents = grants.clone();
        }
        patch.agent_security = Some(security);
    }
    if let Some((max_bytes, retention_days)) = request.attachments {
        let mut attachments = config.attachments;
        if let Some(max_bytes) = max_bytes {
            attachments.max_bytes = max_bytes;
        }
        if let Some(retention_days) = retention_days {
            attachments.retention_days = retention_days;
        }
        patch.attachments = Some(attachments);
    }
    if let Some(documents) = &request.documents {
        patch.documents = Some(config.documents.patched(documents).map_err(|message| anyhow::anyhow!(message))?);
    }
    let mut changed = apply_global_config_patch(&mut config, &patch)?;
    if let Some(listen) = request.listen {
        changed |= config.management_listen != listen;
        config.management_listen = listen;
    }
    if let Some(value) = &request.owner_notify {
        let next = apply_owner_notify(&config.owner_notify, value).map_err(|message| anyhow::anyhow!(message))?;
        changed |= config.owner_notify != next;
        config.owner_notify = next;
    }
    if let Some(value) = &request.usage {
        let next = config.usage.patched(value).map_err(|message| anyhow::anyhow!(message))?;
        changed |= config.usage != next;
        config.usage = next;
    }
    if let Some(value) = &request.agent_harness {
        let next = config.agent_harness.patched(value).map_err(|message| anyhow::anyhow!(message))?;
        changed |= config.agent_harness != next;
        config.agent_harness = next;
    }
    if let Some(value) = &request.prompt_cache_key {
        let next = config.prompt_cache_key.patched(value).map_err(|message| anyhow::anyhow!(message))?;
        changed |= config.prompt_cache_key != next;
        config.prompt_cache_key = next;
    }
    if let Some(value) = &request.agent_bus {
        let next = config.agent_bus.patched(value).map_err(|message| anyhow::anyhow!(message))?;
        changed |= config.agent_bus != next;
        config.agent_bus = next;
    }
    if let Some(value) = &request.artifacts {
        let next = config.artifacts.effective().patched(value).map_err(|message| anyhow::anyhow!(message))?;
        changed |= config.artifacts != next;
        config.artifacts = next;
    }
    if let Some(value) = &request.workflows {
        let next = config.workflows.patched(value).map_err(anyhow::Error::msg)?;
        changed |= config.workflows != next;
        config.workflows = next;
    }
    if let Some(value) = &request.browser {
        let next = config.browser.effective().patched(value).map_err(anyhow::Error::msg)?;
        changed |= config.browser != next;
        config.browser = next;
    }
    if let Some(value) = &request.session_store {
        let next = config
            .session_store
            .effective()
            .patched(value)
            .map_err(|message| anyhow::anyhow!(message))?;
        changed |= config.session_store != next;
        config.session_store = next;
    }
    if let Some(value) = &request.notification_levels {
        let next = config
            .notification_levels
            .patched(value, &crate::notification_levels::registry(), &config.owner_notify.effective())
            .map_err(|message| anyhow::anyhow!(message))?;
        changed |= config.notification_levels != next;
        config.notification_levels = next;
    }
    let mut cors_changed = false;
    if let Some(origins) = request.cors_origins {
        cors_changed = config.management_cors_origins != origins;
        changed |= cors_changed;
        config.management_cors_origins = origins;
    }
    if let Some(value) = &request.peers {
        let next = config.peers.patched(value)?;
        changed |= next != config.peers;
        config.peers = next;
    }
    if changed {
        save_codez_config(&config)?;
    }
    if cors_changed {
        crate::management::listeners::configure_cors_origins(config.management_cors_origins.clone());
    }
    if request.peers.is_some() {
        let peers=crate::peers::store::Store::open()?;
        if peers.dir.exists() { crate::peers::worker::maintain(&peers, &crate::asks::store::AskStore::open_default()?)?; }
        crate::peers::worker::wake();
    }
    Ok((config, changed))
}

// Unlike a pure DTO view, this reports the relay's actual fail-closed policy:
// an absent config is permissive for legacy defaults but means `ask` to browser.
fn effective_settings_view(config: &CodezConfig) -> Value {
    let mut view = settings_view(config);
    view["browser"] = crate::config::browser::current().view();
    view["services"] = crate::services::store::ServiceStore::open_default()
        .and_then(|store| store.load().map_err(anyhow::Error::msg))
        .map(|registry| json!(registry.settings))
        .unwrap_or(Value::Null);
    view
}

pub(super) fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    match request.method.as_str() {
        "GET" => match load_codez_config_checked() {
            Ok(config) => write_json_response(stream, 200, "OK", &effective_settings_view(&config)),
            Err(error) => super::server::write_v2_error(
                stream,
                503,
                "Service Unavailable",
                "settings_unavailable",
                "the cutex config could not be read",
                true,
                json!({ "diagnostic": format!("{error:#}") }),
            ),
        },
        "PATCH" => {
            let mut parsed = match parse_patch(&request.body) {
                Ok(parsed) => parsed,
                Err(message) => return bad_request(stream, &message),
            };
            if parsed.proxy.is_some() && !proxy_editable() {
                return proxy_owner_required(stream);
            }
            if let Some(services) = parsed.services.take() {
                let store = crate::services::store::ServiceStore::open_default()?;
                return match store.update_settings(services) {
                    Ok(_) => match load_codez_config_checked() {
                        Ok(config) => {
                            let mut view = effective_settings_view(&config);
                            view["changed"] = json!(true);
                            view["restartRequired"] = json!([]);
                            write_json_response(stream, 200, "OK", &view)
                        }
                        Err(error) => super::server::write_v2_error(
                            stream,
                            503,
                            "Service Unavailable",
                            "settings_unavailable",
                            "the cutex config could not be read",
                            true,
                            json!({ "diagnostic": format!("{error:#}") }),
                        ),
                    },
                    Err(error) => bad_request(stream, &error.to_string()),
                };
            }
            // owner-local-only: turning cross-host peers on is a root-identity
            // action (turning them off stays open to owner principals).
            if parsed.peers.as_ref().is_some_and(|peers| peers.get("enabled") == Some(&Value::Bool(true)))
                && !crate::management::users::request_is_owner_local()
            {
                let error = crate::management::users::owner_local_only("peersEnable");
                return super::server::write_v2_error(stream, 403, "Forbidden", error.code, &error.message, false, error.details);
            }
            if !crate::management::listeners::current_request_is_loopback() {
                // The daemon fetches usage.catalogUrl itself: only from this machine.
                let catalog_url = parsed
                    .usage
                    .as_ref()
                    .is_some_and(|usage| usage.get("catalogUrl").is_some())
                    .then(|| "usage.catalogUrl".to_string());
                if let Some(field) = parsed
                    .touched
                    .iter()
                    .find(|t| LOOPBACK_ONLY.contains(&t.as_str()))
                    .cloned()
                    .or(catalog_url)
                {
                    return super::server::write_v2_error(
                        stream,
                        403,
                        "Forbidden",
                        "loopback_only",
                        &format!("{field} can only be changed on this machine (loopback listener)"),
                        false,
                        json!({ "field": field }),
                    );
                }
            }
            if let Some(choice) = parsed.patch.claude_cli {
                if let Err(error) = crate::components::claude_runtime::validate_cli_choice(
                    super::upgrade_api::layout().as_ref(),
                    choice,
                ) {
                    return bad_request(stream, &format!("claudeCli: {error:#}"));
                }
            }
            let before = load_codez_config_checked().ok().map(|c| effective_settings_view(&c));
            let touched = parsed.touched.clone();
            match apply(parsed) {
                Ok((config, changed)) => {
                    if touched.iter().any(|field| field == "claudeCli") {
                        if let Err(error) = crate::components::claude_runtime::apply_cli_choice(
                            super::upgrade_api::layout().as_ref(),
                            config.claude_cli,
                        ) {
                            return super::server::write_v2_error(
                                stream,
                                503,
                                "Service Unavailable",
                                "claude_cli_select_failed",
                                &format!("the setting was saved, but the selected Claude runtime could not be updated: {error:#}"),
                                true,
                                json!({}),
                            );
                        }
                    }
                    let mut view = effective_settings_view(&config);
                    let restart_required: Vec<&str> = RESTART_REQUIRED
                        .iter()
                        .copied()
                        .filter(|field| {
                            touched.iter().any(|t| t == field)
                                && before.as_ref().map(|b| &b[*field]) != Some(&view[*field])
                        })
                        .collect();
                    view["changed"] = json!(changed);
                    view["restartRequired"] = json!(restart_required);
                    write_json_response(stream, 200, "OK", &view)
                }
                Err(error) => bad_request(stream, &format!("{error:#}")),
            }
        }
        _ => super::server::write_v2_error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "use GET or PATCH for /v2/settings",
            false,
            json!({}),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::model::ManagementApiToken;

    #[test]
    fn claude_cli_choice_is_strict_and_defaults_to_bundled() {
        let defaults = settings_view(&CodezConfig::default());
        assert_eq!(defaults["claudeCli"], "bundled");
        assert_eq!(defaults["claudeCliDefault"], "bundled");
        let parsed = parse_patch(br#"{"claudeCli":"system"}"#).unwrap();
        assert_eq!(parsed.patch.claude_cli, Some(crate::config::claude::ClaudeCliChoice::System));
        assert!(parse_patch(br#"{"claudeCli":"path"}"#).unwrap_err().contains("bundled or system"));
    }

    #[test]
    fn service_lifecycle_settings_are_additive_and_standalone() {
        let parsed = parse_patch(br#"{"services":{"ephemeralIdleMinutes":120}}"#).unwrap();
        assert_eq!(
            parsed.services.unwrap().ephemeral_idle_minutes,
            Some(120)
        );
        assert!(parse_patch(
            br#"{"services":{"ephemeralIdleMinutes":120},"autoThreadTitles":true}"#
        )
        .unwrap_err()
        .contains("on its own"));
        assert!(parse_patch(br#"{"services":{"unknown":1}}"#).is_err());
    }

    #[test]
    fn session_store_view_and_patch_validation() {
        let config = CodezConfig::default();
        let view = settings_view(&config);
        assert_eq!(view["sessionStore"], json!({"receiptRetentionDays": 7}));
        assert_eq!(view["sessionStoreDefault"], json!({"receiptRetentionDays": 7}));
        assert_eq!(view["sessionStoreRange"]["receiptRetentionDays"], json!([0, 365]));
        let request = parse_patch(br#"{"sessionStore":{"receiptRetentionDays":30}}"#).unwrap();
        assert_eq!(request.session_store, Some(json!({"receiptRetentionDays": 30})));
        for bad in [
            br#"{"sessionStore":{"receiptRetentionDays":400}}"#.as_slice(),
            br#"{"sessionStore":{"receiptRetentionDays":-1}}"#,
            br#"{"sessionStore":{"receiptRetentionDays":"7"}}"#,
            br#"{"sessionStore":{"other":1}}"#,
            br#"{"sessionStore":7}"#,
        ] {
            assert!(parse_patch(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        // Hand-edited out-of-range value: the view shows the effective default.
        let mut edited = CodezConfig::default();
        edited.session_store.receipt_retention_days = 9999;
        assert_eq!(settings_view(&edited)["sessionStore"]["receiptRetentionDays"], 7);
        // config.json round trip: default omitted, custom value kept.
        let saved = serde_json::to_value(&config).unwrap();
        assert!(saved.get("session_store").is_none());
        let mut custom = CodezConfig::default();
        custom.session_store.receipt_retention_days = 0;
        let saved = serde_json::to_value(&custom).unwrap();
        assert_eq!(saved["session_store"], json!({"receiptRetentionDays": 0}));
    }

    #[test]
    fn agent_bus_wake_busy_wait_view_and_patch() {
        let config = CodezConfig::default();
        let view = settings_view(&config);
        assert_eq!(view["agentBus"]["wakeBusyWaitSecs"], 120);
        assert_eq!(view["agentBusDefault"]["wakeBusyWaitSecs"], 120);
        assert_eq!(view["agentBus"]["resultCoalescing"]["claudeSubscription"]["quietSecs"], 120);
        let request = parse_patch(br#"{"agentBus":{"resultCoalescing":{"claudeSubscription":{"enabled":false},"codex":{"quietSecs":10}}}}"#).unwrap();
        let updated = config.agent_bus.patched(request.agent_bus.as_ref().unwrap()).unwrap();
        assert!(!updated.result_coalescing.claude_subscription.enabled);
        assert_eq!(updated.result_coalescing.codex.quiet_secs, 10);
        assert_eq!(updated.result_coalescing.claude_other.quiet_secs, 30);
        assert_eq!(view["agentBusRange"]["wakeBusyWaitSecs"], json!([0, 86400]));
        // Read per delivery: neither a restart nor a new runtime is needed.
        assert!(!RESTART_REQUIRED.contains(&"agentBus") && !NEW_RUNTIMES_ONLY.contains(&"agentBus"));
        for (body, stored) in [
            (br#"{"agentBus":{"wakeBusyWaitSecs":0}}"#.as_slice(), json!({"wake_busy_wait_secs": 0})),
            (br#"{"agentBus":{"wakeBusyWaitSecs":45}}"#, json!({"wake_busy_wait_secs": 45})),
            (br#"{"agentBus":{"wakeBusyWaitSecs":null}}"#, json!({"wake_busy_wait_secs": null})),
            (br#"{"agentBus":{"wakeBusyWaitSecs":-1}}"#, json!({"wake_busy_wait_secs": null})),
        ] {
            let request = parse_patch(body).unwrap();
            let mut next = CodezConfig::default();
            next.agent_bus = next.agent_bus.patched(request.agent_bus.as_ref().unwrap()).unwrap();
            assert_eq!(serde_json::to_value(&next).unwrap()["agent_bus"], stored, "{}", String::from_utf8_lossy(body));
        }
        let mut never = CodezConfig::default();
        never.agent_bus = never.agent_bus.patched(&json!({"wakeBusyWaitSecs": null})).unwrap();
        assert_eq!(settings_view(&never)["agentBus"]["wakeBusyWaitSecs"], Value::Null);
        for bad in [
            br#"{"agentBus":{"wakeBusyWaitSecs":86401}}"#.as_slice(),
            br#"{"agentBus":{"wakeBusyWaitSecs":"60"}}"#,
            br#"{"agentBus":{"other":1}}"#,
            br#"{"agentBus":60}"#,
        ] {
            assert!(parse_patch(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        // Default omitted from config.json.
        assert!(serde_json::to_value(&config).unwrap().get("agent_bus").is_none());
    }

    #[test]
    fn prompt_cache_key_view_and_patch() {
        let view = settings_view(&CodezConfig::default());
        assert_eq!(view["promptCacheKey"], json!({"enabled": false}));
        assert_eq!(view["promptCacheKeyStatus"], "off");
        // Applied at each runtime's next launch.
        assert!(PATCH_FIELDS.contains(&"promptCacheKey") && NEW_RUNTIMES_ONLY.contains(&"promptCacheKey"));
        // Accepted by the field list (it was refused as an unsupported field).
        let request = parse_patch(br#"{"promptCacheKey":{"enabled":true}}"#).unwrap();
        let mut config = CodezConfig::default();
        config.prompt_cache_key = config.prompt_cache_key.patched(request.prompt_cache_key.as_ref().unwrap()).unwrap();
        assert!(config.prompt_cache_key.enabled);
        assert_eq!(serde_json::to_value(&config).unwrap()["prompt_cache_key"], json!({"enabled": true}));
        for bad in [br#"{"promptCacheKey":{"enabled":"yes"}}"#.as_slice(), br#"{"promptCacheKey":{"other":1}}"#, br#"{"promptCacheKey":true}"#] {
            assert!(parse_patch(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn browser_settings_view_and_strict_patch() {
        let config=CodezConfig::default();
        assert_eq!(settings_view(&config)["browser"]["agentOpenApproval"],"off");
        assert!(serde_json::to_value(&config).unwrap().get("browser").is_none());
        let request=parse_patch(br#"{"browser":{"agentOpenApproval":"ask","imageRetainSecs":60}}"#).unwrap();
        let next=config.browser.patched(request.browser.as_ref().unwrap()).unwrap();
        assert_eq!(next.view()["agentOpenApproval"],"ask");
        assert_eq!(next.image_retain_secs,60);
        for bad in [br#"{"browser":null}"#.as_slice(),br#"{"browser":{"agentOpenApproval":true}}"#,
            br#"{"browser":{"imageRetainBytes":67108865}}"#,br#"{"browser":{"credential":"bad"}}"#] {assert!(parse_patch(bad).is_err());}
    }

    #[test]
    fn browser_get_reports_missing_config_ask_and_saved_default_off() {
        let _guard = crate::config::paths::test_home_env_lock();
        struct Home(std::path::PathBuf, Option<std::ffi::OsString>);
        impl Drop for Home {
            fn drop(&mut self) {
                unsafe {
                    match &self.1 {
                        Some(previous) => std::env::set_var("HOME", previous),
                        None => std::env::remove_var("HOME"),
                    }
                }
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = std::env::temp_dir().join(format!("browser-settings-get-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join(crate::config::paths::PRIVATE_HOME_MARKER), b"browser test").unwrap();
        let _home = Home(root.clone(), std::env::var_os("HOME"));
        unsafe { std::env::set_var("HOME", &root); }
        let get = || {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let reader = std::thread::spawn(move || {
                use std::io::Read;
                let mut stream = std::net::TcpStream::connect(address).unwrap();
                stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
                let mut response = String::new();
                stream.read_to_string(&mut response).unwrap();
                response
            });
            let (mut stream, _) = listener.accept().unwrap();
            handle(&mut stream, &SimpleHttpRequest {
                method: "GET".into(), path: PATH.into(), headers: Default::default(), body: vec![],
            }).unwrap();
            drop(stream);
            let response = reader.join().unwrap();
            assert!(response.starts_with("HTTP/1.1 200 "));
            serde_json::from_str::<Value>(response.split_once("\r\n\r\n").unwrap().1).unwrap()
        };
        assert!(!crate::config::paths::config_path().unwrap().exists());
        let missing = get();
        assert_eq!(missing["browser"]["agentOpenApproval"], "ask");
        assert_eq!(missing["browserDefault"]["agentOpenApproval"], "off");
        save_codez_config(&CodezConfig::default()).unwrap();
        assert_eq!(get()["browser"]["agentOpenApproval"], "off");
        let mut config = CodezConfig::default();
        config.browser.agent_open_approval = crate::config::browser::AgentOpenApproval::Ask;
        save_codez_config(&config).unwrap();
        assert_eq!(get()["browser"]["agentOpenApproval"], "ask");
    }

    #[test]
    fn artifacts_settings_view_and_patch() {
        let config = CodezConfig::default();
        let view = settings_view(&config);
        assert_eq!(view["artifacts"]["maxBlobBytes"], 64 * 1024 * 1024);
        assert_eq!(view["artifacts"]["hashExternalMaxBytes"], 1024 * 1024 * 1024);
        assert_eq!(view["artifacts"]["quotaBytes"], 10u64 * 1024 * 1024 * 1024);
        assert_eq!(view["artifacts"]["autoCapture"]["enabled"], true);
        assert_eq!(view["artifacts"]["autoCapture"]["patterns"][5], "handoff/**");
        assert_eq!(view["artifactsDefault"], view["artifacts"]);
        assert!(view["artifactsRange"]["quotaBytes"].is_array());
        assert!(!RESTART_REQUIRED.contains(&"artifacts") && !NEW_RUNTIMES_ONLY.contains(&"artifacts"));
        let request = parse_patch(br#"{"artifacts":{"quotaBytes":209715200,"autoCapture":{"patterns":["out/**"]}}}"#).unwrap();
        let mut next = CodezConfig::default();
        next.artifacts = next.artifacts.patched(request.artifacts.as_ref().unwrap()).unwrap();
        let saved = serde_json::to_value(&next).unwrap();
        assert_eq!(saved["artifacts"]["quotaBytes"], 209715200);
        assert_eq!(saved["artifacts"]["autoCapture"], json!({"enabled": true, "patterns": ["out/**"]}));
        for bad in [
            br#"{"artifacts":{"quotaBytes":1}}"#.as_slice(),
            br#"{"artifacts":{"autoCapture":{"enabled":"yes"}}}"#,
            br#"{"artifacts":{"autoCapture":{"patterns":["../x"]}}}"#,
            br#"{"artifacts":{"other":1}}"#,
            br#"{"artifacts":1}"#,
        ] {
            assert!(parse_patch(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        assert!(serde_json::to_value(&config).unwrap().get("artifacts").is_none());
    }

    #[test]
    fn usage_settings_view_and_patch() {
        let config = CodezConfig::default();
        let view = settings_view(&config);
        assert_eq!(view["usage"]["priceSource"], "litellm");
        assert_eq!(view["usage"]["displayCurrency"], "USD");
        assert_eq!(view["usage"]["exchangeRateAutoFetch"], false);
        assert_eq!(view["usageOptions"]["priceSource"], json!(["litellm", "models.dev", "manual"]));
        let request = parse_patch(br#"{"usage":{"displayCurrency":"CNY","exchangeRates":{"CNY":7.2}}}"#).unwrap();
        assert!(request.usage.is_some());
        assert!(parse_patch(br#"{"usage":{"displayCurrency":"EUR"}}"#).is_err());
        assert!(parse_patch(br#"{"usage":{"bogus":1}}"#).is_err());
        // Default omitted from config.json; r216 ignores a present block.
        assert!(serde_json::to_value(&config).unwrap().get("usage").is_none());
        let mut custom = CodezConfig::default();
        custom.usage = custom.usage.patched(&json!({"priceSource": "manual"})).unwrap();
        assert_eq!(serde_json::to_value(&custom).unwrap()["usage"]["priceSource"], "manual");
    }

    #[test]
    fn owner_notify_view_and_partial_patch() {
        let config = CodezConfig::default();
        let view = settings_view(&config);
        assert_eq!(view["ownerNotify"]["delivery"]["urgent"], json!(["desktop", "phone", "bigAlert"]));
        assert_eq!(view["ownerNotify"]["toolAudience"], "ownerFacing");
        assert_eq!(view["ownerNotify"]["rateLimit"], json!({"perHour": 12, "urgentPerHour": 3}));
        let patched = apply_owner_notify(
            &config.owner_notify,
            &json!({"delivery": {"info": []}, "rateLimit": {"urgentPerHour": 1}, "bigAlertSound": false,
                    "agentOverrides": {"cutex.a": "mute", "cutex.b": null}}),
        )
        .unwrap();
        assert!(patched.delivery.info.is_empty());
        assert_eq!(patched.delivery.attention, vec!["desktop", "phone"]);
        assert_eq!(patched.rate_limit.per_hour, 12);
        assert_eq!(patched.rate_limit.urgent_per_hour, 1);
        assert!(!patched.big_alert_sound);
        assert_eq!(patched.agent_overrides.len(), 1);
        for bad in [
            json!({"delivery": {"info": ["fax"]}}),
            json!({"delivery": {"loud": []}}),
            json!({"delivery": {"info": ["desktop", "desktop"]}}),
            json!({"toolAudience": "nobody"}),
            json!({"rateLimit": {"perHour": 0}}),
            json!({"agentOverrides": {"cutex.a": "loud"}}),
            json!({"other": 1}),
        ] {
            assert!(apply_owner_notify(&config.owner_notify, &bad).is_err(), "{bad}");
        }
        let request = parse_patch(br#"{"ownerNotify":{"toolAudience":"everyone"}}"#).unwrap();
        assert!(request.owner_notify.is_some());
        assert!(parse_patch(br#"{"ownerNotify":{"toolAudience":"x"}}"#).is_err());
    }

    #[test]
    fn agent_security_view_and_partial_patch() {
        let mut config = CodezConfig::default();
        let view = settings_view(&config);
        assert_eq!(view["agentSecurity"]["crossProjectAccess"], "isolated");
        assert_eq!(view["agentSecurity"]["links"], json!([]));

        let parsed = parse_patch(
            br#"{"agentSecurity":{"links":[["project:alpha","project:beta"]]}}"#,
        )
        .unwrap();
        let update = parsed.agent_security.unwrap();
        assert!(update.cross_project.is_none());
        let mut patch = GlobalConfigPatch {
            agent_security: Some(AgentSecurityConfig {
                cross_project: config.agent_security.cross_project,
                links: update.links.unwrap(),
                ..AgentSecurityConfig::default()
            }),
            ..GlobalConfigPatch::default()
        };
        assert!(apply_global_config_patch(&mut config, &patch).unwrap());
        assert_eq!(
            settings_view(&config)["agentSecurity"]["links"],
            json!([["project:alpha", "project:beta"]])
        );
        let host = parse_patch(br#"{"agentSecurity":{"crossProjectAccess":"host"}}"#).unwrap();
        assert_eq!(
            host.agent_security.unwrap().cross_project,
            Some(CrossProjectAccess::Host)
        );
        for bad in [
            br#"{"agentSecurity":{"crossProjectAccess":"open"}}"#.as_slice(),
            br#"{"agentSecurity":{"links":[["project:a","project:a"]]}}"#,
            br#"{"agentSecurity":{"links":[["group:a","project:b"]]}}"#,
            br#"{"agentSecurity":{"authentication":"off"}}"#,
            br#"{"agentSecurity":true}"#,
        ] {
            assert!(parse_patch(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        patch.agent_security = Some(AgentSecurityConfig::default());
        assert!(apply_global_config_patch(&mut config, &patch).unwrap());
        assert!(config.agent_security.is_default());
    }

    #[test]
    fn agent_security_external_agents_view_patch_and_revoke() {
        const AGENT: &str = "cutex.00000000-0000-0000-0000-000000000001";
        let mut config = CodezConfig::default();
        let view = settings_view(&config);
        assert_eq!(view["agentSecurity"]["externalAgents"], json!([]));
        assert_eq!(
            view["agentSecurity"]["maxExternalAgents"],
            crate::config::agent_security::MAX_EXTERNAL_AGENTS
        );
        let body = format!(
            r#"{{"agentSecurity":{{"externalAgents":[{{"project":"alpha","agent":"{AGENT}"}}]}}}}"#
        );
        let update = parse_patch(body.as_bytes()).unwrap().agent_security.unwrap();
        assert!(update.cross_project.is_none() && update.links.is_none());
        let grants = update.external_agents.unwrap();
        let mut patch = GlobalConfigPatch {
            agent_security: Some(AgentSecurityConfig {
                external_agents: grants,
                ..config.agent_security.clone()
            }),
            ..GlobalConfigPatch::default()
        };
        assert!(apply_global_config_patch(&mut config, &patch).unwrap());
        assert_eq!(
            settings_view(&config)["agentSecurity"]["externalAgents"],
            json!([{"project": "alpha", "agent": AGENT}])
        );
        assert_eq!(
            serde_json::to_value(&config.agent_security).unwrap(),
            json!({"external_agents": [{"project": "alpha", "agent": AGENT}]})
        );
        let duplicate = format!(
            r#"{{"agentSecurity":{{"externalAgents":[{{"project":"alpha","agent":"{AGENT}"}},{{"project":"alpha","agent":"{AGENT}"}}]}}}}"#
        );
        for bad in [
            duplicate.as_bytes(),
            br#"{"agentSecurity":{"externalAgents":[{"project":"alpha","agent":"agent:x"}]}}"#,
            br#"{"agentSecurity":{"externalAgents":[{"project":"bad id","agent":"cutex.00000000-0000-0000-0000-000000000001"}]}}"#,
            br#"{"agentSecurity":{"externalAgents":[{"project":"alpha"}]}}"#,
            br#"{"agentSecurity":{"externalAgents":{"project":"alpha"}}}"#,
            br#"{"agentSecurity":{"external_agents":[]}}"#,
        ] {
            assert!(parse_patch(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        // An empty list revokes every grant.
        let revoke = parse_patch(br#"{"agentSecurity":{"externalAgents":[]}}"#)
            .unwrap()
            .agent_security
            .unwrap();
        patch.agent_security = Some(AgentSecurityConfig {
            external_agents: revoke.external_agents.unwrap(),
            ..config.agent_security.clone()
        });
        assert!(apply_global_config_patch(&mut config, &patch).unwrap());
        assert!(config.agent_security.is_default());
    }

    #[test]
    fn view_hides_secrets_and_proxy_credentials() {
        let config = CodezConfig {
            agent_bus_token: Some("bus-secret-token".into()),
            management_api_token: Some(ManagementApiToken::new("mgmt-secret-token")),
            proxy: Some(ProxyConfig {
                enabled: true,
                url: Some("http://alice:pw-secret@127.0.0.1:7890".into()),
                no_proxy: None,
            }),
            ..CodezConfig::default()
        };
        let view = settings_view(&config);
        let text = view.to_string();
        for secret in ["bus-secret-token", "mgmt-secret-token", "pw-secret", "alice"] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        assert_eq!(view["secretsPresent"]["agentBusToken"], true);
        assert_eq!(view["proxy"]["urlHasCredentials"], true);
        assert_eq!(view["proxy"]["url"], "http://127.0.0.1:7890/");
        assert_eq!(view["newSessionNotification"], "off");
    }

    #[test]
    fn listen_settings_parse_validate_and_view() {
        // Default-instance rules: a bare address takes the default port 24270
        // (nothing here connects or binds).
        crate::config::instance::with_default_ports_for_test(listen_settings_parse_validate_and_view_inner);
    }

    fn listen_settings_parse_validate_and_view_inner() {
        let parsed = parse_patch(br#"{"managementListen":["127.0.0.1","tailscale","10.0.0.5:24270"],"managementCorsOrigins":["capacitor://localhost"]}"#).unwrap();
        assert_eq!(
            parsed.listen,
            Some(Some(vec!["127.0.0.1".to_string(), "tailscale".to_string(), "10.0.0.5:24270".to_string()]))
        );
        assert_eq!(parsed.cors_origins, Some(Some(vec!["capacitor://localhost".to_string()])));
        let cleared = parse_patch(br#"{"managementListen":null,"managementCorsOrigins":null}"#).unwrap();
        assert_eq!(cleared.listen, Some(None));
        assert_eq!(cleared.cors_origins, Some(None));
        assert!(parse_patch(br#"{"managementListen":["tailscale"]}"#).unwrap_err().contains("loopback"));
        assert!(parse_patch(br#"{"managementListen":"127.0.0.1"}"#).is_err());
        assert!(parse_patch(br#"{"managementCorsOrigins":["http://x/y"]}"#).is_err());

        let mut config = CodezConfig::default();
        let view = settings_view(&config);
        assert!(view["managementListen"].is_null());
        assert_eq!(view["managementListenEntries"][0]["kind"], "loopback");
        assert_eq!(view["managementCorsOriginsDefault"][2], "capacitor://localhost");
        assert!(view["restartRequiredFields"].as_array().unwrap().iter().any(|f| f == "managementListen"));
        config.management_listen = Some(vec!["127.0.0.1".into(), "0.0.0.0:24280".into()]);
        let view = settings_view(&config);
        assert_eq!(view["managementListenEntries"][1]["kind"], "wildcard");
        assert!(view["managementListenEntries"][1]["warning"].is_string());
    }

    #[test]
    fn listen_settings_writes_are_loopback_only() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            use std::io::Read;
            let mut stream = std::net::TcpStream::connect(address).unwrap();
            let mut text = String::new();
            stream.read_to_string(&mut text).unwrap();
            text
        });
        let (mut stream, peer) = listener.accept().unwrap();
        crate::management::listeners::set_current_connection(Some(
            crate::management::listeners::ConnectionInfo { remote: true, peer },
        ));
        let request = SimpleHttpRequest {
            method: "PATCH".into(),
            path: PATH.into(),
            headers: Default::default(),
            body: br#"{"managementListen":["127.0.0.1","0.0.0.0:24280"]}"#.to_vec(),
        };
        crate::config::instance::with_default_ports_for_test(|| handle(&mut stream, &request)).unwrap();
        crate::management::listeners::set_current_connection(None);
        drop(stream);
        let response = client.join().unwrap();
        assert!(response.starts_with("HTTP/1.1 403 "), "{response}");
        assert!(response.contains("\"loopback_only\""), "{response}");
    }

    #[test]
    fn patch_parses_tri_state_and_rejects_unknown() {
        let parsed = parse_patch(
            br#"{"defaultProfile":null,"agentBusPort":24361,"newSessionNotification":"normal",
                 "newSessionDefaults":{"GLM":{"model":"glm-5","reasoning":null}},
                 "proxy":{"enabled":false}}"#,
        )
        .unwrap();
        assert_eq!(parsed.patch.default_profile, ConfigValueUpdate::Clear);
        assert_eq!(parsed.patch.agent_bus_port, ConfigValueUpdate::Set(24361));
        assert_eq!(parsed.patch.new_session_notification, Some(Level::Normal));
        assert_eq!(
            parsed.patch.new_session_defaults.as_ref().unwrap()["GLM"].model.as_deref(),
            Some("glm-5")
        );
        assert!(matches!(parsed.proxy, Some(Some(_))));
        for bad in [
            r#"{"dockerUseSudo":true}"#,
            r#"{"agentBusEnabled":null}"#,
            r#"{"agentBusPort":70000}"#,
            r#"{"newSessionNotification":"loud"}"#,
            r#"{"claudeUsageRefreshMinutes":0}"#,
            r#"{"profileQuotaRefreshMinutes":61}"#,
            r#"{"proxy":{"password":"x"}}"#,
            r#"{"newSessionDefaults":{"GLM":{"effort":"x"}}}"#,
        ] {
            assert!(parse_patch(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn thread_title_settings_view_and_patch() {
        let config = CodezConfig::default();
        let view = settings_view(&config);
        assert_eq!(view["autoThreadTitles"], true);
        assert_eq!(view["threadTitleModel"], Value::Null);
        let parsed = parse_patch(br#"{"autoThreadTitles":false,"threadTitleModel":"gpt-5.6-luna"}"#).unwrap();
        assert_eq!(parsed.patch.auto_thread_titles, Some(false));
        assert_eq!(parsed.patch.thread_title_model, ConfigValueUpdate::Set("gpt-5.6-luna".into()));
        let parsed = parse_patch(br#"{"threadTitleModel":null}"#).unwrap();
        assert_eq!(parsed.patch.thread_title_model, ConfigValueUpdate::Clear);
        assert!(parse_patch(br#"{"autoThreadTitles":"yes"}"#).is_err());
    }

    #[test]
    fn codex_config_follow_shared_defaults_on_and_patches_as_boolean() {
        assert_eq!(settings_view(&CodezConfig::default())["codexConfigFollowShared"], true);
        let legacy: CodezConfig = serde_json::from_value(json!({})).unwrap();
        assert!(legacy.codex_config_follow_shared, "absent field keeps the default");
        // Daemon data written under the old field name is still read.
        let old: CodezConfig =
            serde_json::from_value(json!({"codex_plugins_follow_shared": false})).unwrap();
        assert!(!old.codex_config_follow_shared);
        let saved = serde_json::to_value(&old).unwrap();
        assert_eq!(saved["codex_config_follow_shared"], false);
        let parsed = parse_patch(br#"{"codexConfigFollowShared":false}"#).unwrap();
        assert_eq!(parsed.patch.codex_config_follow_shared, Some(false));
        let mut config = CodezConfig::default();
        assert!(apply_global_config_patch(&mut config, &parsed.patch).unwrap());
        let view = settings_view(&config);
        assert_eq!(view["codexConfigFollowShared"], false);
        assert_eq!(view["codexPluginsFollowShared"], false);
        // The old name still patches; both names must agree.
        let parsed = parse_patch(br#"{"codexPluginsFollowShared":true}"#).unwrap();
        assert_eq!(parsed.patch.codex_config_follow_shared, Some(true));
        assert!(parse_patch(
            br#"{"codexConfigFollowShared":true,"codexPluginsFollowShared":true}"#
        )
        .is_ok());
        assert!(parse_patch(
            br#"{"codexConfigFollowShared":true,"codexPluginsFollowShared":false}"#
        )
        .is_err());
        assert!(parse_patch(br#"{"codexConfigFollowShared":null}"#).is_err());
    }

    #[test]
    fn image_input_settings_default_patch_noop_and_persistence() {
        use super::super::user_input::image_steering::ImageBusyBehavior;
        let mut config: CodezConfig = serde_json::from_value(json!({})).unwrap();
        assert_eq!(config.image_input.busy_behavior, ImageBusyBehavior::Queue);
        assert_eq!(settings_view(&config)["imageInput"], json!({"busyBehavior":"queue"}));
        let patch = parse_patch(br#"{"imageInput":{"busyBehavior":"steer"}}"#).unwrap();
        assert!(apply_global_config_patch(&mut config, &patch.patch).unwrap());
        assert!(!apply_global_config_patch(&mut config, &patch.patch).unwrap());
        let empty = parse_patch(br#"{"imageInput":{}}"#).unwrap();
        assert_eq!(empty.patch.image_input, None);
        assert!(!apply_global_config_patch(&mut config, &empty.patch).unwrap());
        assert_eq!(settings_view(&config)["imageInput"], json!({"busyBehavior":"steer"}));
        assert_eq!(settings_view(&config)["imageInputDefault"], json!({"busyBehavior":"queue"}));
        let root = std::env::temp_dir().join(format!("image-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("config.json");
        crate::config::atomic::write_private_pretty_json_atomic(&file, &config, "image settings test").unwrap();
        let reopened: CodezConfig = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(reopened.image_input, config.image_input);
        assert_eq!(serde_json::to_value(&reopened).unwrap()["image_input"]["busyBehavior"], "steer");
        std::fs::remove_dir_all(root).unwrap();
        assert!(!RESTART_REQUIRED.contains(&"imageInput") && !NEW_RUNTIMES_ONLY.contains(&"imageInput"));
        for bad in [r#"{"imageInput":null}"#, r#"{"imageInput":[]}"#,
            r#"{"imageInput":{"busyBehavior":null}}"#, r#"{"imageInput":{"busyBehavior":true}}"#,
            r#"{"imageInput":{"busyBehavior":"discard"}}"#, r#"{"imageInput":{"unknown":true}}"#,
            r#"{"imageInput":{"busy_behavior":"steer"}}"#] {
            assert!(parse_patch(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn queue_owner_may_pass_asks_defaults_on_and_patches_as_boolean() {
        let mut config: CodezConfig = serde_json::from_value(json!({})).unwrap();
        assert!(config.queue.owner_may_pass_asks);
        assert_eq!(settings_view(&config)["queue"]["ownerMayPassAsks"], true);
        assert!(serde_json::to_value(&config).unwrap().get("queue").is_none());
        let parsed = parse_patch(br#"{"queue":{"ownerMayPassAsks":false}}"#).unwrap();
        assert_eq!(parsed.patch.queue_owner_may_pass_asks, Some(false));
        assert!(apply_global_config_patch(&mut config, &parsed.patch).unwrap());
        assert!(!apply_global_config_patch(&mut config, &parsed.patch).unwrap());
        assert_eq!(settings_view(&config)["queue"]["ownerMayPassAsks"], false);
        assert_eq!(settings_view(&config)["queueDefault"]["ownerMayPassAsks"], true);
        let saved = serde_json::to_value(&config).unwrap();
        assert_eq!(saved["queue"]["owner_may_pass_asks"], false);
        assert!(!serde_json::from_value::<CodezConfig>(saved).unwrap().queue.owner_may_pass_asks);
        assert_eq!(parse_patch(br#"{"queue":{}}"#).unwrap().patch.queue_owner_may_pass_asks, None);
        for bad in [
            r#"{"queue":null}"#,
            r#"{"queue":{"ownerMayPassAsks":null}}"#,
            r#"{"queue":{"ownerMayPassAsks":"yes"}}"#,
            r#"{"queue":{"owner_may_pass_asks":false}}"#,
        ] {
            assert!(parse_patch(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn archive_delete_empty_defaults_on_and_patches_as_boolean() {
        let view = settings_view(&CodezConfig::default());
        assert_eq!(view["archiveDeleteEmpty"], true);
        let legacy: CodezConfig = serde_json::from_value(json!({})).unwrap();
        assert!(legacy.archive_delete_empty, "absent field keeps the default");
        let parsed = parse_patch(br#"{"archiveDeleteEmpty":false}"#).unwrap();
        assert_eq!(parsed.patch.archive_delete_empty, Some(false));
        let mut config = CodezConfig::default();
        assert!(apply_global_config_patch(&mut config, &parsed.patch).unwrap());
        assert!(!config.archive_delete_empty);
        assert_eq!(settings_view(&config)["archiveDeleteEmpty"], false);
        assert!(parse_patch(br#"{"archiveDeleteEmpty":null}"#).is_err());
        assert!(parse_patch(br#"{"archiveDeleteEmpty":"yes"}"#).is_err());
    }

    #[test]
    fn quick_chat_default_profile_view_patch_and_fallback() {
        let view = settings_view(&CodezConfig::default());
        assert!(view["quickChatDefaultProfile"].is_null());
        assert!(view["quickChatEffectiveProfile"].is_null());
        let config = CodezConfig { default_profile: Some("main".into()), ..CodezConfig::default() };
        assert_eq!(settings_view(&config)["quickChatEffectiveProfile"], "main");
        let config = CodezConfig { quick_chat_default_profile: Some("kimi".into()), ..config };
        let view = settings_view(&config);
        assert_eq!(view["quickChatDefaultProfile"], "kimi");
        assert_eq!(view["quickChatEffectiveProfile"], "kimi");
        assert_eq!(quick_chat_profile(&config, Some("GLM")).as_deref(), Some("GLM"));
        assert_eq!(quick_chat_profile(&config, None).as_deref(), Some("kimi"));

        let parsed = parse_patch(br#"{"quickChatDefaultProfile":"kimi"}"#).unwrap();
        assert_eq!(parsed.patch.quick_chat_default_profile, ConfigValueUpdate::Set("kimi".into()));
        let parsed = parse_patch(br#"{"quickChatDefaultProfile":null}"#).unwrap();
        assert_eq!(parsed.patch.quick_chat_default_profile, ConfigValueUpdate::Clear);
        assert!(parse_patch(br#"{"quickChatDefaultProfile":""}"#).is_err());
        assert!(parse_patch(br#"{"quickChatDefaultProfile":3}"#).is_err());
        let mut saved = CodezConfig::default();
        let patch = GlobalConfigPatch {
            quick_chat_default_profile: ConfigValueUpdate::Set("kimi".into()),
            ..GlobalConfigPatch::default()
        };
        assert!(apply_global_config_patch(&mut saved, &patch).unwrap());
        assert_eq!(saved.quick_chat_default_profile.as_deref(), Some("kimi"));
        assert_eq!(serde_json::to_value(&saved).unwrap()["quick_chat_default_profile"], "kimi");
        assert!(serde_json::to_value(CodezConfig::default()).unwrap().get("quick_chat_default_profile").is_none());
    }

    #[test]
    fn profile_quota_refresh_minutes_view_and_patch() {
        let view = settings_view(&CodezConfig::default());
        assert_eq!(view["profileQuotaRefreshMinutes"], 5);
        let parsed = parse_patch(br#"{"profileQuotaRefreshMinutes":0}"#).unwrap();
        assert_eq!(parsed.patch.profile_quota_refresh_minutes, Some(0));
        let parsed = parse_patch(br#"{"profileQuotaRefreshMinutes":15}"#).unwrap();
        assert_eq!(parsed.patch.profile_quota_refresh_minutes, Some(15));
        assert!(parse_patch(br#"{"profileQuotaRefreshMinutes":-1}"#).is_err());
        assert!(parse_patch(br#"{"profileQuotaRefreshMinutes":"5"}"#).is_err());
    }

    #[test]
    fn attachments_view_and_partial_patch() {
        let view = settings_view(&CodezConfig::default());
        assert_eq!(view["attachments"]["maxBytes"], 8 * 1024 * 1024);
        assert_eq!(view["attachments"]["retentionDays"], 0, "kept by default since r252");
        let parsed = parse_patch(br#"{"attachments":{"maxBytes":1048576}}"#).unwrap();
        assert_eq!(parsed.attachments, Some((Some(1_048_576), None)));
        let parsed = parse_patch(br#"{"attachments":{"retentionDays":0}}"#).unwrap();
        assert_eq!(parsed.attachments, Some((None, Some(0))));
        for body in [
            br#"{"attachments":{"maxBytes":1}}"#.as_slice(),
            br#"{"attachments":{"maxBytes":16777217}}"#,
            br#"{"attachments":{"retentionDays":-1}}"#,
            br#"{"attachments":{"dir":"x"}}"#,
            br#"{"attachments":8}"#,
        ] {
            assert!(parse_patch(body).is_err(), "{}", String::from_utf8_lossy(body));
        }
        let mut config = CodezConfig::default();
        let patch = GlobalConfigPatch {
            attachments: Some(crate::config::attachments::AttachmentSettings {
                max_bytes: 1_048_576,
                retention_days: 7,
            }),
            ..GlobalConfigPatch::default()
        };
        assert!(apply_global_config_patch(&mut config, &patch).unwrap());
        assert_eq!(settings_view(&config)["attachments"]["maxBytes"], 1_048_576);
        // Defaults are not written to config.json.
        let json = serde_json::to_value(CodezConfig::default()).unwrap();
        assert!(json.get("attachments").is_none());
    }

    /// Every retention is visible (owner decision 2026-10-23), read-only.
    #[test]
    fn retention_rules_are_listed_and_follow_settings() {
        let view = settings_view(&CodezConfig::default());
        let rows = view["retention"].as_array().unwrap();
        let rule = |id: &str| rows.iter().find(|row| row["id"] == id).unwrap_or_else(|| panic!("{id} missing")).clone();
        for id in ["attachments", "asks", "bus-messages", "owner-notify", "archive", "documents", "workflow-runs", "history-reverts", "session-store", "alarms"] {
            let row = rule(id);
            assert!(row["rule"].is_string() && row["detail"].is_string(), "{row}");
            assert_eq!(row["configurable"], row["setting"].is_object(), "{row}");
        }
        assert_eq!(rule("attachments")["rule"], "forever");
        assert_eq!(rule("attachments")["keepReferenced"], true);
        assert_eq!((rule("archive")["rule"].clone(), rule("archive")["days"].clone()), (json!("forever"), Value::Null));
        assert_eq!(rule("workflow-runs")["rule"], "forever");
        assert_eq!(rule("history-reverts")["rule"], "forever");
        assert_eq!((rule("owner-notify")["rule"].clone(), rule("owner-notify")["days"].clone()), (json!("archive"), json!(30)));
        assert_eq!(rule("archive")["setting"], json!({"store": "config", "key": "documents.archiveRetentionDays"}));
        let mut config = CodezConfig::default();
        config.documents.archive_retention_days = 365;
        config.documents.closed_retention_days = 10;
        let view = settings_view(&config);
        let rows = view["retention"].as_array().unwrap();
        let archive = rows.iter().find(|row| row["id"] == "archive").unwrap();
        assert_eq!((archive["rule"].clone(), archive["days"].clone()), (json!("age"), json!(365)));
        assert_eq!(rows.iter().find(|row| row["id"] == "asks").unwrap()["days"], 10);
        assert_eq!(parse_patch(br#"{"retention":[]}"#).err().as_deref(), Some("unsupported field: retention"));
    }

    #[test]
    fn new_thread_defaults_view_and_patch_round_trip() {
        let view = settings_view(&CodezConfig::default());
        assert_eq!(view["newThreadDefaults"]["agent"]["sandbox"], "workspace-write");
        assert_eq!(view["newThreadDefaults"]["agent"]["approvalPolicy"], "on-request");
        assert_eq!(view["newThreadDefaults"]["agent"]["networkAccess"], true);
        assert_eq!(view["newThreadDefaults"]["quickChat"]["sandbox"], "workspace-write");
        assert_eq!(view["newThreadDefaults"]["quickChat"]["approvalPolicy"], "on-request");
        assert_eq!(view["newThreadDefaults"]["quickChat"]["networkAccess"], false);
        assert_eq!(view["newThreadDefaults"]["agent"]["approvalsReviewer"], "auto_review");
        assert_eq!(view["newThreadDefaults"]["quickChat"]["approvalsReviewer"], "auto_review");
        assert_eq!(view["newThreadDefaultsBuiltin"], view["newThreadDefaults"]);
        // The reviewer is a patchable field; the legacy alias is stored canonically.
        let reviewer = parse_patch(br#"{"newThreadDefaults":{"quickChat":{"approvalsReviewer":"user"},"agent":{"approvalsReviewer":"guardian_subagent"}}}"#).unwrap();
        let with_reviewer = apply_new_thread_defaults(&NewThreadDefaults::default(), reviewer.new_thread_defaults.as_ref().unwrap()).unwrap();
        assert_eq!(with_reviewer.quick_chat.approvals_reviewer.as_deref(), Some("user"));
        assert_eq!(with_reviewer.agent.approvals_reviewer.as_deref(), Some("auto_review"));
        let view = settings_view(&CodezConfig { new_thread_defaults: with_reviewer.clone(), ..CodezConfig::default() });
        assert_eq!(view["newThreadDefaults"]["quickChat"]["approvalsReviewer"], "user");
        let reset = parse_patch(br#"{"newThreadDefaults":{"quickChat":{"approvalsReviewer":null}}}"#).unwrap();
        let after = apply_new_thread_defaults(&with_reviewer, reset.new_thread_defaults.as_ref().unwrap()).unwrap();
        assert_eq!(after.quick_chat.approvals_reviewer, None);

        let parsed = parse_patch(
            br#"{"newThreadDefaults":{"quickChat":{"networkAccess":true,"approvalPolicy":"never"}}}"#,
        )
        .unwrap();
        let next = apply_new_thread_defaults(
            &NewThreadDefaults::default(),
            parsed.new_thread_defaults.as_ref().unwrap(),
        )
        .unwrap();
        assert_eq!(next.quick_chat.network_access, Some(true));
        assert_eq!(next.quick_chat.approval.as_deref(), Some("never"));
        assert!(next.agent.is_empty());
        let config = CodezConfig { new_thread_defaults: next.clone(), ..CodezConfig::default() };
        let view = settings_view(&config);
        assert_eq!(view["newThreadDefaults"]["quickChat"]["networkAccess"], true);
        assert_eq!(view["newThreadDefaults"]["quickChat"]["sandbox"], "workspace-write");
        // Field null resets that field; kind null resets the kind.
        let reset = parse_patch(br#"{"newThreadDefaults":{"quickChat":{"approvalPolicy":null}}}"#).unwrap();
        let after = apply_new_thread_defaults(&next, reset.new_thread_defaults.as_ref().unwrap()).unwrap();
        assert_eq!(after.quick_chat.approval, None);
        assert_eq!(after.quick_chat.network_access, Some(true));
        let clear = parse_patch(br#"{"newThreadDefaults":{"quickChat":null}}"#).unwrap();
        let cleared = apply_new_thread_defaults(&after, clear.new_thread_defaults.as_ref().unwrap()).unwrap();
        assert!(cleared.is_empty());
        // Persisted spelling is snake_case and omitted when empty.
        let saved = serde_json::to_value(&config).unwrap();
        assert_eq!(saved["new_thread_defaults"]["quick_chat"]["network_access"], true);
        assert!(serde_json::to_value(CodezConfig::default()).unwrap().get("new_thread_defaults").is_none());
        for bad in [
            r#"{"newThreadDefaults":{"worker":{}}}"#,
            r#"{"newThreadDefaults":{"agent":{"sandbox":"full"}}}"#,
            r#"{"newThreadDefaults":{"agent":{"approvalPolicy":"untrusted"}}}"#,
            r#"{"newThreadDefaults":{"agent":{"approvalsReviewer":"owner"}}}"#,
            r#"{"newThreadDefaults":{"agent":{"approvalsReviewer":true}}}"#,
            r#"{"newThreadDefaults":{"agent":{"networkAccess":"yes"}}}"#,
            r#"{"newThreadDefaults":{"agent":{"writableRoots":["relative"]}}}"#,
            r#"{"newThreadDefaults":{"agent":{"model":"x"}}}"#,
            r#"{"newThreadDefaults":[]}"#,
        ] {
            assert!(parse_patch(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn proxy_merge_keeps_stored_url_when_omitted() {
        let current = ProxyConfig {
            enabled: true,
            url: Some("http://u:p@127.0.0.1:1".into()),
            no_proxy: None,
        };
        let update = json!({ "noProxy": "corp.example" });
        let merged = merged_proxy(Some(&current), update.as_object().unwrap()).unwrap().unwrap();
        assert_eq!(merged.url.as_deref(), Some("http://u:p@127.0.0.1:1"));
        assert_eq!(merged.no_proxy.as_deref(), Some("corp.example"));
        let enable_without_url = json!({ "enabled": true });
        assert!(merged_proxy(None, enable_without_url.as_object().unwrap()).is_err());
        let retired = parse_patch(br#"{"proxy":{"forceHttpTransport":false}}"#).unwrap_err();
        assert!(retired.contains("forceHttpTransport"), "{retired}");
    }

    #[test]
    fn proxy_modes_round_trip() {
        let manual = ProxyConfig { enabled: true, url: Some("http://u:p@127.0.0.1:1".into()), no_proxy: None };
        let set = |current: Option<&ProxyConfig>, body: Value| merged_proxy(current, body.as_object().unwrap());
        assert_eq!(set(Some(&manual), json!({"mode": "system"})).unwrap(), None);
        // Off keeps the manual address (never used while off); manual restores it.
        let none = set(Some(&manual), json!({"mode": "none"})).unwrap().unwrap();
        assert!(!none.enabled);
        assert_eq!(none.url.as_deref(), Some("http://u:p@127.0.0.1:1"));
        assert!(matches!(
            crate::config::proxy_outbound::OutboundProxy::from_config(Some(&none)),
            crate::config::proxy_outbound::OutboundProxy::Direct
        ));
        assert!(crate::config::proxy::proxy_envs(Some(&none), None)
            .iter()
            .all(|(key, value)| key.eq_ignore_ascii_case("no_proxy") || value.is_empty()));
        let again = set(Some(&none), json!({"mode": "manual"})).unwrap().unwrap();
        assert!(again.enabled);
        assert_eq!(again.url.as_deref(), Some("http://u:p@127.0.0.1:1"));
        // An edit while off is remembered; `url: null` forgets; an invalid one is refused.
        let edited = set(Some(&none), json!({"url": "http://127.0.0.1:2"})).unwrap().unwrap();
        assert!(!edited.enabled && edited.url.as_deref() == Some("http://127.0.0.1:2"));
        assert!(set(Some(&none), json!({"mode": "none", "url": "not a url"})).is_err());
        let forgotten = set(Some(&none), json!({"mode": "none", "url": null})).unwrap().unwrap();
        assert!(!forgotten.enabled && forgotten.url.is_none());
        assert!(set(Some(&forgotten), json!({"mode": "manual"})).is_err(), "manual needs a URL");
        assert!(set(None, json!({"mode": "none"})).unwrap().unwrap().url.is_none());
        assert!(set(None, json!({"mode": "pac"})).is_err());
        assert!(set(None, json!({"mode": "manual", "url": "not a url"})).is_err());
        assert!(set(None, json!({"mode": "manual", "url": "ftp://proxy:21"})).is_err());

        let view = |proxy: Option<ProxyConfig>| settings_view(&CodezConfig { proxy, ..CodezConfig::default() })["proxy"].clone();
        assert_eq!(view(None)["mode"], "system");
        assert_eq!(view(None)["editable"], true);
        assert_eq!(view(None)["loopbackNoProxy"], json!(["127.0.0.1", "localhost", "::1"]));
        let none_view = view(Some(none));
        assert_eq!(none_view["mode"], "none");
        assert_eq!(none_view["url"], "http://127.0.0.1:1/");
        assert_eq!(none_view["urlHasCredentials"], true);
        let manual_view = view(Some(manual));
        assert_eq!(manual_view["mode"], "manual");
        assert_eq!(manual_view["url"], "http://127.0.0.1:1/");
        assert!(manual_view.get("forceHttpTransport").is_none());
        let socks = view(Some(ProxyConfig { enabled: true, url: Some("socks5h://127.0.0.1:1080".into()), no_proxy: None }));
        assert!(socks["warning"].as_str().unwrap().contains("SOCKS"));
    }

    #[test]
    fn proxy_writes_need_the_root_credential() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            use std::io::Read;
            let mut stream = std::net::TcpStream::connect(address).unwrap();
            let mut text = String::new();
            stream.read_to_string(&mut text).unwrap();
            text
        });
        let (mut stream, _) = listener.accept().unwrap();
        crate::management::listeners::set_current_client_token(Some(
            crate::management::client_tokens::VerifiedClientToken::test_token("mct-proxy-test"),
        ));
        let request = SimpleHttpRequest {
            method: "PATCH".into(),
            path: PATH.into(),
            headers: Default::default(),
            body: br#"{"proxy":{"mode":"none"}}"#.to_vec(),
        };
        let editable = settings_view(&CodezConfig::default())["proxy"]["editable"].clone();
        handle(&mut stream, &request).unwrap();
        crate::management::listeners::set_current_client_token(None);
        drop(stream);
        let response = client.join().unwrap();
        assert_eq!(editable, false);
        assert!(response.starts_with("HTTP/1.1 403 "), "{response}");
        assert!(response.contains("\"proxy_owner_credential_required\""), "{response}");
    }
}
