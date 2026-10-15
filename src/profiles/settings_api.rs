//! Secret-free profile views and a narrow profile settings writer for
//! Management clients (`/v2/profiles`). Writes go through the same primitives
//! as `cutex profile set` / the TUI profile editor: the profile store, the
//! typed `CodexProfileConfigPatch`, `normalize_profile_config_for_account`,
//! `merge_and_write_config_toml` and `refresh_materialized_account_config`, with
//! rollback of already-written files when a later step fails.

use std::path::Path;

use anyhow::{ensure, Context};
use serde_json::{json, Value};

use crate::config::atomic::{write_private_bytes_atomic, write_private_pretty_json_atomic};
use crate::config::global_settings::ConfigValueUpdate;
use crate::config::proxy::effective_proxy_config;
use crate::config::store::load_codez_config;
use crate::config::text::{read_optional_text, write_optional_text_if_changed};
use crate::profiles::appearance::{self, AgentTurnQuestions, TextStyle};
use crate::profiles::codex_profile::{
    apply_codex_profile_config_patch, inspect_codex_profile_config, is_builtin_model_provider,
    reasoning_summary_supported, CodexProfileConfigPatch, CodexProfileConfigSnapshot,
};
use crate::profiles::inspect::{
    account_model_api_base, account_model_provider, account_uses_api_key_auth,
    account_uses_chatgpt_auth, account_auth_payload,
};
use crate::profiles::materialize::{materialized_account_files, refresh_materialized_account_config};
use crate::profiles::model::{AccountsStore, CliKind, CodezConfig, StoredAccount, STORE_VERSION};
use crate::profiles::profile_config::{
    extract_profile_config_toml, merge_and_write_config_toml, normalize_profile_config_for_account,
};

pub const MAX_API_KEY_BYTES: usize = 4096;

/// Load the profile store without migrating it (the CLI/daemon migrates
/// legacy stores on first use; Management refuses rather than half-migrating).
pub fn load_store() -> anyhow::Result<AccountsStore> {
    let path = crate::config::paths::accounts_path()?;
    let Some(raw) = read_optional_text(&path)? else {
        return Ok(AccountsStore::default());
    };
    let store: AccountsStore = serde_json::from_str(&raw)
        .with_context(|| format!("Failed to parse accounts file: {}", path.display()))?;
    ensure!(
        store.version >= STORE_VERSION,
        "accounts file version {} must be migrated by the cutex CLI first",
        store.version
    );
    Ok(store)
}

pub fn find<'a>(store: &'a AccountsStore, profile: &str) -> Option<&'a StoredAccount> {
    store
        .accounts
        .iter()
        .find(|account| account.id == profile || account.name == profile)
}

/// Whether the profile may be switched to/from mid-conversation. An explicit
/// option wins; otherwise API-key Codex profiles default to true and
/// subscription/OAuth profiles (ChatGPT account, Claude) default to false.
pub fn allow_session_switch(account: &StoredAccount) -> bool {
    account
        .options
        .allow_session_switch
        .unwrap_or_else(|| default_allow_session_switch(account))
}

/// Native cute-codex subagents stay enabled unless a profile turns them off
/// (owner decision 2026-10-07: configurable, current default kept).
pub const DEFAULT_NATIVE_SUBAGENTS: bool = true;

/// Effective native-subagent switch for runtimes on a Codex profile. Claude
/// profiles have no cute-codex runtime: `None`.
pub fn native_subagents(account: &StoredAccount) -> Option<bool> {
    (account.cli_kind == CliKind::Codex)
        .then(|| account.options.native_subagents.unwrap_or(DEFAULT_NATIVE_SUBAGENTS))
}

/// Default of the `delegation_wait` tool: Codex models learned to wait for
/// subagents, Claude's pattern is "end the turn, get a notification".
pub fn default_delegation_wait(account: &StoredAccount) -> bool {
    account.cli_kind == CliKind::Codex
}

/// Whether agents on this profile are offered `delegation_wait`.
pub fn delegation_wait(account: &StoredAccount) -> bool {
    account.options.delegation_wait.unwrap_or_else(|| default_delegation_wait(account))
}

pub fn browser_tools(account: &StoredAccount) -> bool { account.browser_tools }

/// Profiles the `web_search` cutex tool can serve: Codex API-key profiles
/// (it calls the profile's own Responses provider with the stored key).
pub fn search_tool_supported(account: &StoredAccount) -> bool {
    crate::profiles::catalog_api::discovery_supported(account)
}

/// Whether agents on this profile are offered the `web_search` cutex tool
/// (profile-search-tool): the stored opt-in on a supported profile.
pub fn search_tool(account: &StoredAccount) -> bool {
    account.search_tool && search_tool_supported(account)
}

/// cute-codex `-c` overrides for the profile's native-subagent switch. Only an
/// explicit choice is passed, so an unset profile keeps the native default
/// (`features.multi_agent` is enabled by default in cute-codex 0.160).
/// Disabling also turns off `multi_agent_v2`, which would otherwise bring
/// the subagent tools back.
pub fn native_subagent_overrides(account: &StoredAccount) -> Vec<(&'static str, bool)> {
    if account.cli_kind != CliKind::Codex {
        return Vec::new();
    }
    match account.options.native_subagents {
        Some(false) => vec![
            ("features.multi_agent", false),
            ("features.multi_agent_v2", false),
        ],
        Some(true) => vec![("features.multi_agent", true)],
        None => Vec::new(),
    }
}

/// Effective AskUserQuestion behaviour for turns a Claude agent did not get
/// from the owner (Agent Bus, services). Codex profiles: `None`.
pub fn agent_turn_questions(account: &StoredAccount) -> Option<AgentTurnQuestions> {
    (account.cli_kind == CliKind::Claude)
        .then(|| account.options.agent_turn_questions.unwrap_or_default())
}

/// Native file snapshots are opt-in for Claude; Codex profiles do not use it.
pub fn claude_file_checkpointing(account: &StoredAccount) -> Option<bool> {
    (account.cli_kind == CliKind::Claude)
        .then(|| account.options.claude_file_checkpointing.unwrap_or(false))
}

/// Whether agents need the owner's approval to launch agents on this profile.
pub fn agent_use_requires_approval(account: &StoredAccount) -> bool {
    account.options.agent_use_requires_approval
}

pub fn default_allow_session_switch(account: &StoredAccount) -> bool {
    account.cli_kind == CliKind::Codex
        && account.source.as_deref() != Some("official")
        && !account_uses_chatgpt_auth(account)
        && account_uses_api_key_auth(account)
}

pub fn codex_snapshot(account: &StoredAccount) -> CodexProfileConfigSnapshot {
    if account.cli_kind != CliKind::Codex {
        return CodexProfileConfigSnapshot::default();
    }
    materialized_account_files(account)
        .ok()
        .and_then(|files| read_optional_text(&files.config_path).ok().flatten())
        .and_then(|raw| extract_profile_config_toml(&raw).ok().flatten())
        .and_then(|profile| inspect_codex_profile_config(Some(&profile)).ok())
        .unwrap_or_default()
}

fn auth_present(account: &StoredAccount) -> bool {
    auth_kind(account) != "none"
}

/// Login/auth status without secrets: chatgpt (OAuth tokens), apiKey,
/// claude (subscription credentials or Claude API key file), or none.
pub fn auth_kind(account: &StoredAccount) -> &'static str {
    match account.cli_kind {
        CliKind::Codex => {
            if account_auth_payload(account)
                .is_some_and(|auth| auth.get("tokens").and_then(Value::as_object).is_some())
            {
                "chatgpt"
            } else if account_uses_api_key_auth(account) {
                "apiKey"
            } else {
                "none"
            }
        }
        CliKind::Claude => {
            let dir = crate::launch::env::materialized_claude_config_dir(account);
            let present = |name: &str| std::fs::metadata(dir.join(name)).is_ok_and(|meta| meta.len() > 0);
            if present(".credentials.json") {
                "claude"
            } else if present("api_key") {
                "apiKey"
            } else {
                "none"
            }
        }
    }
}

/// Explicit provider category, else the one inferred from the login, the
/// runtime and the API base URL (`provider_category::infer`). Codex callers
/// pass the already-read model provider / base URL.
fn effective_provider_category(
    account: &StoredAccount,
    model_provider: Option<&str>,
    base_url: Option<&str>,
) -> crate::profiles::provider_category::ProviderCategory {
    crate::profiles::provider_category::manual(account).unwrap_or_else(|| {
        crate::profiles::provider_category::infer(&account.cli_kind, auth_kind(account), base_url, model_provider)
    })
}

/// [`effective_provider_category`] reading the profile files itself.
pub fn provider_category_of(account: &StoredAccount) -> crate::profiles::provider_category::ProviderCategory {
    if let Some(category) = crate::profiles::provider_category::manual(account) {
        return category;
    }
    let codex = account.cli_kind == CliKind::Codex;
    let model_provider = codex.then(|| account_model_provider(account)).flatten();
    let base_url = codex.then(|| account_model_api_base(account)).flatten();
    effective_provider_category(account, model_provider.as_deref(), base_url.as_deref())
}

/// Full typed Codex profile config (camelCase). `providerEnvKey` is the
/// environment variable NAME only; secrets are never part of the snapshot.
pub fn snapshot_json(snapshot: &CodexProfileConfigSnapshot) -> Value {
    json!({
        "model": snapshot.model,
        "modelProvider": snapshot.model_provider,
        "forcedLoginMethod": snapshot.forced_login_method,
        "modelReasoningEffort": snapshot.model_reasoning_effort,
        "modelReasoningSummary": snapshot.model_reasoning_summary,
        "updatePlanEnabled": snapshot.update_plan_enabled,
        "webSearch": snapshot.web_search,
        "toolSchemaCompat": snapshot.tool_schema_compat,
        "modelCatalogJson": snapshot.model_catalog_json,
        "modelContextWindow": snapshot.model_context_window,
        "modelAutoCompactTokenLimit": snapshot.model_auto_compact_token_limit,
        "providerName": snapshot.provider_name,
        "providerBaseUrl": snapshot.provider_base_url,
        "providerEnvKey": snapshot.provider_env_key,
        "providerWireApi": snapshot.provider_wire_api,
        "requestMaxRetries": snapshot.request_max_retries,
        "streamMaxRetries": snapshot.stream_max_retries,
        "streamIdleTimeoutMs": snapshot.stream_idle_timeout_ms,
        "websocketConnectTimeoutMs": snapshot.websocket_connect_timeout_ms,
        "requiresOpenaiAuth": snapshot.requires_openai_auth,
        "supportsWebsockets": snapshot.supports_websockets,
        // D4 (native r53+): provider `capabilities.instructions_field`.
        "providerInstructionsField": snapshot.provider_instructions_field,
    })
}

/// Secret-free list/detail item. Never includes auth.json contents, API keys,
/// email, or proxy credentials.
pub fn profile_item(account: &StoredAccount, config: &CodezConfig, detail: bool) -> Value {
    let provider = appearance::provider(account);
    let effective_style = config.appearance.profile_style(&account.options, provider);
    let is_default = config.default_profile.as_deref() == Some(account.name.as_str());
    let live_plan = crate::profiles::plan_label::account_plan(account);
    let legacy_plan = crate::profiles::plan_label::legacy_plan_type(account);
    let plan = crate::profiles::plan_label::resolve_plan_label(
        live_plan.as_deref(),
        account.plan_label.as_deref(),
        legacy_plan,
    );
    let (provider_family, provider_family_label) = crate::profiles::providers::provider_family(account);
    let codex = account.cli_kind == CliKind::Codex;
    let snapshot = codex.then(|| codex_snapshot(account));
    let (model, model_provider, base_url, reasoning) = match &snapshot {
        Some(snapshot) => (
            snapshot.model.clone(),
            account_model_provider(account),
            account_model_api_base(account),
            snapshot.model_reasoning_effort.clone(),
        ),
        None => (
            Some(account.options.claude.model.clone()),
            None,
            None,
            account.options.claude.effort.map(|effort| effort.as_str().to_string()),
        ),
    };
    let effective_category = effective_provider_category(account, model_provider.as_deref(), base_url.as_deref()).key();
    let mut item = json!({
        "id": account.id,
        "name": account.name,
        "cliKind": account.cli_kind.to_string(),
        "source": account.source,
        "provider": provider.key(),
        // Same value as appearance.effectiveProviderCategory (desktop reads both).
        "effectiveProviderCategory": effective_category,
        "providerFamily": provider_family,
        "providerFamilyLabel": provider_family_label,
        "model": model,
        "modelProvider": model_provider,
        "baseUrl": base_url,
        "modelReasoningEffort": reasoning,
        // Codex only; null = not set in the profile (model default).
        "modelReasoningSummary": snapshot.as_ref().and_then(|s| s.model_reasoning_summary.clone()),
        // Codex only; `[tools.update_plan] enabled` (null = not set: the tool is off).
        "updatePlanEnabled": snapshot.as_ref().and_then(|s| s.update_plan_enabled),
        // Codex only; native `web_search` mode (null = not set: native default).
        "webSearch": snapshot.as_ref().and_then(|s| s.web_search.clone()),
        // Codex only; native `tool_schema_compat` ("off" | "strict"; null =
        // not set: off, tool schemas sent unchanged).
        "toolSchemaCompat": snapshot.as_ref().and_then(|s| s.tool_schema_compat.clone()),
        "modelContextWindow": snapshot.as_ref().and_then(|s| s.model_context_window),
        "modelAutoCompactTokenLimit": snapshot.as_ref().and_then(|s| s.model_auto_compact_token_limit),
        "authPresent": auth_present(account),
        "authKind": auth_kind(account),
        // The owner's label when set ("manual"), else the signed-in ChatGPT
        // account's live plan ("account"), else the legacy plan_type
        // ("manual"); see profiles::plan_label. Claude profiles: the
        // owner's label only.
        "planLabel": plan.as_ref().map(|(label, _)| label),
        "planLabelSource": plan.as_ref().map(|(_, source)| source.as_str()),
        "isDefault": is_default,
        "appearance": {
            "color": account.options.style.as_ref().and_then(|style| style.fg.clone()),
            "effectiveColor": effective_style.fg,
            "providerCategory": crate::profiles::provider_category::manual(account).map(|category| category.key()),
            "effectiveProviderCategory": effective_category,
        },
        "quotaEnabled": account.options.quota.enabled,
        "allowSessionSwitch": allow_session_switch(account),
        "nativeSubagents": native_subagents(account),
        "delegationWait": delegation_wait(account),
        "browserTools": browser_tools(account),
        // profile-search-tool: the `web_search` cutex tool (a separate
        // search-only request to this profile's provider); Codex API-key
        // profiles only (false elsewhere).
        "searchTool": search_tool(account),
        // profile-generic-switches: Codex profiles launch with the host's
        // private native ChatGPT auth.json (explicit agent launch only);
        // null on Claude profiles.
        "privateChatgptAuthFile": codex.then_some(account.private_chatgpt_auth_file),
        "agentUseRequiresApproval": agent_use_requires_approval(account),
        "agentTurnQuestions": agent_turn_questions(account).map(AgentTurnQuestions::as_str),
        "claudeFileCheckpointing": claude_file_checkpointing(account),
        // Claude only: whether new sessions get the file-editing tools
        // (Write/Edit/...) and Bash. Existing sessions keep their own copy.
        "claudeAllowWrite": (!codex).then_some(account.options.claude.write()),
        "claudeAllowExec": (!codex).then_some(account.options.claude.exec()),
        // Claude only: the Claude account (`GET /v2/claude/accounts`) new
        // sessions run on; `default` = the shared login. Existing sessions
        // keep the account they were created on.
        "claudeAccount": (!codex).then(|| account
            .claude_account
            .clone()
            .unwrap_or_else(|| crate::runtime::claude_accounts::DEFAULT_ACCOUNT_ID.to_string())),
        "lastUsedAt": account.last_used_at,
        // profile-model-display-names: model id -> this profile's display name
        // for that entry (display only; ids stay what is selected and sent).
        "modelDisplayNames": account.model_display_names,
    });
    if detail {
        let config_provider = snapshot.as_ref().and_then(|s| s.model_provider.as_deref());
        let custom_provider = config_provider.is_some_and(|provider| !is_builtin_model_provider(provider));
        let api_key_profile = codex && account.source.as_deref() == Some("api-key");
        // API-key profiles always read the key from the profile auth file as
        // OPENAI_API_KEY (normalize_profile_config_for_account rewrites
        // env_key), so the env var name is fixed there.
        let provider_env_key_editable = codex && custom_provider && !api_key_profile;
        item["detail"] = json!({
            "config": snapshot.as_ref().map(snapshot_json),
            "apiKeyPresent": codex && account_uses_api_key_auth(account),
            // The stored label the editor shows: plan_label (overrides the
            // account's plan), else the legacy plan_type while there is no
            // live account plan; null = show the account's plan.
            "planLabelManual": crate::profiles::plan_label::stored_plan_label(
                account.plan_label.as_deref(),
                legacy_plan,
                live_plan.as_deref(),
            ),
            "allowSessionSwitchExplicit": account.options.allow_session_switch,
            "allowSessionSwitchDefault": default_allow_session_switch(account),
            "nativeSubagentsExplicit": account.options.native_subagents,
            "nativeSubagentsDefault": codex.then_some(DEFAULT_NATIVE_SUBAGENTS),
            "delegationWaitExplicit": account.options.delegation_wait,
            "delegationWaitDefault": default_delegation_wait(account),
            "agentTurnQuestionsExplicit": account.options.agent_turn_questions.map(AgentTurnQuestions::as_str),
            "agentTurnQuestionsDefault": (!codex).then_some(AgentTurnQuestions::default().as_str()),
            "agentTurnQuestionsOptions": AgentTurnQuestions::ALL.map(AgentTurnQuestions::as_str),
            "claudeFileCheckpointingExplicit": account.options.claude_file_checkpointing,
            "claudeFileCheckpointingDefault": (!codex).then_some(false),
            "providerCategoryOptions": crate::profiles::provider_category::ProviderCategory::ALL.map(|category| category.key()),
            "catalogEditable": crate::profiles::catalog_api::catalog_supported(account),
            "editable": {
                "name": true,
                "providerEnvKey": provider_env_key_editable,
                "model": codex,
                "modelProvider": codex,
                "forcedLoginMethod": codex,
                "modelReasoningEffort": codex,
                // DeepSeek profiles: the preset removes the key.
                "modelReasoningSummary": codex && reasoning_summary_supported(config_provider),
                "updatePlanEnabled": codex,
                "webSearch": codex,
                "modelContextWindow": codex,
                "modelAutoCompactTokenLimit": codex,
                "providerOptions": codex && custom_provider,
                "color": true,
                "providerCategory": true,
                "allowSessionSwitch": true,
                "nativeSubagents": codex,
                "delegationWait": true,
                "browserTools": true,
                "agentUseRequiresApproval": true,
                "agentTurnQuestions": !codex,
                "claudeFileCheckpointing": !codex,
                "claudeTools": !codex,
                "apiKey": api_key_profile,
                // profile-plan-label-claude: Claude profiles too.
                "planLabel": true,
            },
        });
        // profile-search-tool: Codex API-key profiles (outside json!: its
        // recursion limit).
        item["detail"]["editable"]["searchTool"] = json!(search_tool_supported(account));
        item["detail"]["editable"]["privateChatgptAuthFile"] = json!(codex);
        item["detail"]["editable"]["toolSchemaCompat"] = json!(codex);
        // Claude write / exec rules: explicit value (null = unset) and the
        // default unset means (on; Codex profiles: null).
        let default = (!codex).then_some(crate::profiles::appearance::CLAUDE_TOOLS_DEFAULT);
        item["detail"]["claudeAllowWriteExplicit"] = json!(account.options.claude.allow_write);
        item["detail"]["claudeAllowWriteDefault"] = json!(default);
        item["detail"]["claudeAllowExecExplicit"] = json!(account.options.claude.allow_exec);
        item["detail"]["claudeAllowExecDefault"] = json!(default);
    }
    item
}

/// Write-only API key: never Debug-printed.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(pub String);

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ApiKey(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProfileApiPatch {
    pub codex_config: CodexProfileConfigPatch,
    pub color: ConfigValueUpdate<String>,
    /// `Clear` returns to the inferred category.
    pub provider_category: ConfigValueUpdate<crate::profiles::provider_category::ProviderCategory>,
    pub allow_session_switch: ConfigValueUpdate<bool>,
    /// `Clear` restores the native default (enabled).
    pub native_subagents: ConfigValueUpdate<bool>,
    /// `Clear` restores the per-runtime default.
    pub delegation_wait: ConfigValueUpdate<bool>,
    pub browser_tools: Option<bool>,
    /// Codex API-key profiles only (`search_tool_supported`).
    pub search_tool: Option<bool>,
    /// Codex profiles only (`profile-generic-switches`).
    pub private_chatgpt_auth_file: Option<bool>,
    pub agent_use_requires_approval: Option<bool>,
    /// Claude profiles only; `Clear` restores the default (decide).
    pub agent_turn_questions: ConfigValueUpdate<AgentTurnQuestions>,
    /// Claude profiles only; effective on the next runtime launch. Clear = off.
    pub claude_file_checkpointing: ConfigValueUpdate<bool>,
    /// Claude profiles only: tools of sessions created from now on.
    pub claude_allow_write: ConfigValueUpdate<bool>,
    pub claude_allow_exec: ConfigValueUpdate<bool>,
    /// Manual plan label (validated by the caller), Codex and Claude
    /// profiles. `Clear` drops it; on Codex profiles also the legacy
    /// `plan_type` annotation (Claude profiles have none: left as is).
    pub plan_label: ConfigValueUpdate<String>,
    pub api_key: Option<ApiKey>,
    /// Claude profiles only: the Claude account of new sessions
    /// (`runtime::claude_accounts`); `Clear` (or `default`) = the shared
    /// default account.
    pub claude_account: ConfigValueUpdate<String>,
    /// profile-model-display-names: merge into the per-entry display names;
    /// `Some(name)` sets (trimmed), `None` removes that entry's name.
    pub model_display_names: std::collections::BTreeMap<String, Option<String>>,
}

/// Longest per-entry model display name (characters).
pub const MAX_MODEL_DISPLAY_NAME_CHARS: usize = 80;
/// Most per-entry model display names one profile keeps.
pub const MAX_MODEL_DISPLAY_NAMES: usize = 256;

/// Trimmed display name, or why it is refused. Empty = remove.
pub fn normalize_model_display_name(name: &str) -> Result<Option<String>, String> {
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        return Ok(None);
    }
    if name.chars().count() > MAX_MODEL_DISPLAY_NAME_CHARS {
        return Err(format!("at most {MAX_MODEL_DISPLAY_NAME_CHARS} characters"));
    }
    if name.chars().any(|c| {
        c.is_control()
            || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    }) {
        return Err("control or bidi characters are not allowed".into());
    }
    Ok(Some(name))
}

fn apply_model_display_names(
    account: &mut StoredAccount,
    patch: &std::collections::BTreeMap<String, Option<String>>,
) -> anyhow::Result<()> {
    for (id, name) in patch {
        crate::profiles::catalog_api::validate_model_id(id)
            .with_context(|| format!("modelDisplayNames: invalid model id {id:?}"))?;
        let name = match name.as_deref() {
            Some(name) => normalize_model_display_name(name)
                .map_err(|message| anyhow::anyhow!("modelDisplayNames[{id}]: {message}"))?,
            None => None,
        };
        // A name equal to the id adds nothing; store it as "no name".
        match name.filter(|name| name != id) {
            Some(name) => {
                account.model_display_names.insert(id.clone(), name);
            }
            None => {
                account.model_display_names.remove(id);
            }
        }
    }
    ensure!(
        account.model_display_names.len() <= MAX_MODEL_DISPLAY_NAMES,
        "modelDisplayNames: at most {MAX_MODEL_DISPLAY_NAMES} entries per profile"
    );
    Ok(())
}

/// auth.json after setting the API key: the existing object with
/// `OPENAI_API_KEY` replaced (a legacy lowercase `openai_api_key` is folded
/// into it) and every other field (`auth_mode`, `last_refresh`, ...) kept.
/// An unchanged key yields the current payload unchanged (no write).
fn api_key_auth_payload(current: Option<&Value>, key: &str) -> Value {
    let mut next = current
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let unchanged = next.get("OPENAI_API_KEY").and_then(Value::as_str) == Some(key)
        || (next.get("OPENAI_API_KEY").is_none()
            && next.get("openai_api_key").and_then(Value::as_str) == Some(key));
    if unchanged {
        return Value::Object(next);
    }
    next.remove("openai_api_key");
    next.insert("OPENAI_API_KEY".into(), Value::String(key.to_string()));
    if next.get("auth_mode").and_then(Value::as_str).is_some_and(|mode| mode != "apikey") {
        next.insert("auth_mode".into(), Value::String("apikey".into()));
    }
    Value::Object(next)
}

/// Store-level options of a profile patch (no files touched).
fn apply_store_options(account: &mut StoredAccount, patch: &ProfileApiPatch) -> anyhow::Result<()> {
    match &patch.color {
        ConfigValueUpdate::Unchanged => {}
        ConfigValueUpdate::Set(color) => {
            let mut style = account.options.style.clone().unwrap_or_default();
            style.fg = Some(color.trim().to_ascii_uppercase());
            style.validate()?;
            account.options.style = Some(style);
        }
        ConfigValueUpdate::Clear => {
            if let Some(style) = account.options.style.as_mut() {
                style.fg = None;
            }
            if account.options.style.as_ref() == Some(&TextStyle::default()) {
                account.options.style = None;
            }
        }
    }
    match patch.provider_category {
        ConfigValueUpdate::Unchanged => {}
        ConfigValueUpdate::Set(category) => {
            crate::profiles::provider_category::set_manual(account, Some(category))
        }
        ConfigValueUpdate::Clear => crate::profiles::provider_category::set_manual(account, None),
    }
    match patch.allow_session_switch {
        ConfigValueUpdate::Unchanged => {}
        ConfigValueUpdate::Set(value) => account.options.allow_session_switch = Some(value),
        ConfigValueUpdate::Clear => account.options.allow_session_switch = None,
    }
    let codex = account.cli_kind == CliKind::Codex;
    match patch.native_subagents {
        ConfigValueUpdate::Unchanged => {}
        ConfigValueUpdate::Set(_) | ConfigValueUpdate::Clear if !codex => {
            anyhow::bail!("nativeSubagents applies to Codex profiles only")
        }
        ConfigValueUpdate::Set(value) => account.options.native_subagents = Some(value),
        ConfigValueUpdate::Clear => account.options.native_subagents = None,
    }
    match patch.delegation_wait {
        ConfigValueUpdate::Unchanged => {}
        ConfigValueUpdate::Set(value) => account.options.delegation_wait = Some(value),
        ConfigValueUpdate::Clear => account.options.delegation_wait = None,
    }
    if let Some(required) = patch.agent_use_requires_approval {
        account.options.agent_use_requires_approval = required;
    }
    if let Some(enabled) = patch.browser_tools { account.browser_tools = enabled; }
    if let Some(enabled) = patch.search_tool {
        anyhow::ensure!(
            !enabled || search_tool_supported(account),
            "searchTool applies to Codex API-key profiles only"
        );
        account.search_tool = enabled;
    }
    if let Some(enabled) = patch.private_chatgpt_auth_file {
        anyhow::ensure!(!enabled || codex, "privateChatgptAuthFile applies to Codex profiles only");
        account.private_chatgpt_auth_file = enabled;
    }
    match patch.agent_turn_questions {
        ConfigValueUpdate::Unchanged => {}
        ConfigValueUpdate::Set(_) | ConfigValueUpdate::Clear if codex => {
            anyhow::bail!("agentTurnQuestions applies to Claude profiles only")
        }
        ConfigValueUpdate::Set(mode) => account.options.agent_turn_questions = Some(mode),
        ConfigValueUpdate::Clear => account.options.agent_turn_questions = None,
    }
    match patch.claude_file_checkpointing {
        ConfigValueUpdate::Unchanged => {}
        ConfigValueUpdate::Set(_) | ConfigValueUpdate::Clear if codex => {
            anyhow::bail!("claudeFileCheckpointing applies to Claude profiles only")
        }
        ConfigValueUpdate::Set(value) => account.options.claude_file_checkpointing = Some(value),
        ConfigValueUpdate::Clear => account.options.claude_file_checkpointing = None,
    }
    for (update, field) in [
        (&patch.claude_allow_write, &mut account.options.claude.allow_write),
        (&patch.claude_allow_exec, &mut account.options.claude.allow_exec),
    ] {
        match update {
            ConfigValueUpdate::Unchanged => {}
            ConfigValueUpdate::Set(_) | ConfigValueUpdate::Clear if codex => {
                anyhow::bail!("claudeAllowWrite / claudeAllowExec apply to Claude profiles only")
            }
            // null = unset: the default (on).
            ConfigValueUpdate::Set(value) => *field = Some(*value),
            ConfigValueUpdate::Clear => *field = None,
        }
    }
    match &patch.plan_label {
        ConfigValueUpdate::Unchanged => {}
        ConfigValueUpdate::Set(label) => {
            let label = crate::profiles::plan_label::normalize_manual_plan_label(label)
                .map_err(|message| anyhow::anyhow!("planLabel: {message}"))?;
            account.plan_label = Some(label);
        }
        ConfigValueUpdate::Clear => {
            account.plan_label = None;
            // Codex: the legacy last-login / preset annotation goes too, so
            // the account's plan (or nothing) shows. Claude profiles never
            // display plan_type (plan_label::legacy_plan_type).
            if codex {
                account.plan_type = None;
            }
        }
    }
    match &patch.claude_account {
        ConfigValueUpdate::Unchanged => {}
        ConfigValueUpdate::Set(_) | ConfigValueUpdate::Clear if codex => {
            anyhow::bail!("claudeAccount applies to Claude profiles only")
        }
        ConfigValueUpdate::Set(id) if id != crate::runtime::claude_accounts::DEFAULT_ACCOUNT_ID => {
            let registry = crate::runtime::claude_accounts::load()?;
            ensure!(
                registry.find(id).is_some(),
                "claudeAccount: no Claude account {id} on this host"
            );
            account.claude_account = Some(id.clone());
        }
        ConfigValueUpdate::Set(_) | ConfigValueUpdate::Clear => account.claude_account = None,
    }
    apply_model_display_names(account, &patch.model_display_names)?;
    Ok(())
}

fn store_fields_changed(account: &StoredAccount, original: &StoredAccount) -> bool {
    account.options != original.options
        || account.provider_category != original.provider_category
        || account.plan_label != original.plan_label
        || account.plan_type != original.plan_type
        || account.claude_account != original.claude_account
        || account.model_display_names != original.model_display_names
        || account.browser_tools != original.browser_tools
        || account.search_tool != original.search_tool
        || account.private_chatgpt_auth_file != original.private_chatgpt_auth_file
}

/// Apply a validated patch to the stored profile and its materialized files.
/// Returns the updated account and whether anything changed.
pub fn update_profile(profile: &str, patch: &ProfileApiPatch) -> anyhow::Result<(StoredAccount, bool)> {
    let mut store = load_store()?;
    let index = store
        .accounts
        .iter()
        .position(|account| account.id == profile || account.name == profile)
        .context("Profile not found")?;
    let original = store.accounts[index].clone();
    let mut account = original.clone();

    // Store-level options.
    apply_store_options(&mut account, patch)?;
    let account_changed = store_fields_changed(&account, &original);
    let codex = account.cli_kind == CliKind::Codex;

    if patch.api_key.is_some() {
        ensure!(
            codex && account.source.as_deref() == Some("api-key"),
            "API key editing is only available for Codex API-key profiles"
        );
    }
    let files = materialized_account_files(&account)?;
    let original_auth_file = read_optional_text(&files.auth_path)?;
    let original_config_file = read_optional_text(&files.config_path)?;
    let original_model_catalog_file = read_optional_text(&files.model_catalog_path)?;
    let current_profile_config = original_config_file
        .as_deref()
        .map(extract_profile_config_toml)
        .transpose()?
        .flatten();
    let (next_profile_config, config_changed) = if patch.codex_config.is_empty() {
        (current_profile_config, false)
    } else {
        ensure!(codex, "Codex model settings are unavailable for a Claude profile");
        let (next, changed) =
            apply_codex_profile_config_patch(current_profile_config.as_deref(), &patch.codex_config)?;
        let snapshot = inspect_codex_profile_config(next.as_deref())?;
        if let (Some(window), Some(limit)) =
            (snapshot.model_context_window, snapshot.model_auto_compact_token_limit)
        {
            ensure!(
                limit < window,
                "modelAutoCompactTokenLimit ({limit}) must be smaller than modelContextWindow ({window})"
            );
        }
        (next, changed)
    };
    let current_auth = original_auth_file
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok());
    let next_auth = patch
        .api_key
        .as_ref()
        .map(|key| api_key_auth_payload(current_auth.as_ref(), &key.0));
    let auth_changed = next_auth
        .as_ref()
        .is_some_and(|next| current_auth.as_ref() != Some(next));

    if !(account_changed || config_changed || auth_changed) {
        return Ok((account, false));
    }

    let normalized_profile_config = config_changed
        .then(|| normalize_profile_config_for_account(&account, next_profile_config))
        .transpose()?;

    if let Some(next_auth) = next_auth.as_ref().filter(|_| auth_changed) {
        crate::profiles::materialize::ensure_materialized_account_dir(&files)?;
        write_private_pretty_json_atomic(&files.auth_path, next_auth, "profile auth.json")?;
    }
    let restore_auth = |path: &Path| -> anyhow::Result<()> {
        if !auth_changed {
            return Ok(());
        }
        match original_auth_file.as_deref() {
            Some(original) => write_private_bytes_atomic(path, original.as_bytes()),
            None if path.exists() => std::fs::remove_file(path)
                .with_context(|| format!("Failed to remove restored auth file: {}", path.display())),
            None => Ok(()),
        }
    };
    let restore_config = || -> anyhow::Result<()> {
        write_optional_text_if_changed(&files.config_path, original_config_file.as_deref())?;
        write_optional_text_if_changed(
            &files.model_catalog_path,
            original_model_catalog_file.as_deref(),
        )
    };

    if config_changed {
        let global_config = load_codez_config();
        let written = (|| -> anyhow::Result<()> {
            merge_and_write_config_toml(
                &files.config_path,
                normalized_profile_config
                    .as_ref()
                    .and_then(|config| config.as_deref()),
                effective_proxy_config(&account, &global_config)
                    .map(|proxy| proxy.enabled)
                    .unwrap_or(false),
            )?;
            refresh_materialized_account_config(&account)?;
            Ok(())
        })();
        if let Err(error) = written {
            restore_config()?;
            restore_auth(&files.auth_path)?;
            return Err(error);
        }
    }

    if account_changed {
        store.accounts[index] = account.clone();
        if let Err(error) = crate::profiles::store::save_store(&store) {
            if config_changed {
                restore_config()?;
            }
            restore_auth(&files.auth_path)?;
            return Err(error);
        }
    }
    Ok((account, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(cli_kind: CliKind, source: &str) -> StoredAccount {
        StoredAccount {
            options: Default::default(),
            browser_tools: false,
            search_tool: false,
            quota_reader: None,
            private_chatgpt_auth_file: false,
            id: format!("settings-api-test-{}", uuid::Uuid::new_v4()),
            name: "alpha".into(),
            email: Some("owner-private@example.com".into()),
            plan_type: Some("pro".into()),
            plan_label: None,
            quota_preset: Default::default(),
            source: Some(source.into()),
            runtime: Default::default(),
            proxy: Some(crate::profiles::model::ProxyConfig {
                enabled: true,
                url: Some("http://bob:proxy-secret@127.0.0.1:1".into()),
                no_proxy: None,
                force_http_transport: true,
            }),
            session: None,
            cli_kind,
            default_cli_args: Vec::new(),
            agent_name: None,
            last_used_at: None,
            provider_category: None,
            claude_account: None,
            model_display_names: Default::default(),
        }
    }

    #[test]
    fn profile_item_is_secret_free_and_camel_case() {
        let config = CodezConfig {
            default_profile: Some("alpha".into()),
            agent_bus_token: Some("bus-secret".into()),
            ..CodezConfig::default()
        };
        for account in [account(CliKind::Codex, "api-key"), account(CliKind::Claude, "official")] {
            let item = profile_item(&account, &config, true);
            let text = item.to_string();
            for secret in ["owner-private", "proxy-secret", "bob", "bus-secret"] {
                assert!(!text.contains(secret), "{secret} leaked: {text}");
            }
            for key in ["cliKind", "modelProvider", "baseUrl", "modelReasoningEffort", "modelReasoningSummary", "updatePlanEnabled", "webSearch", "toolSchemaCompat",
                        "authPresent", "authKind", "isDefault", "quotaEnabled", "allowSessionSwitch", "lastUsedAt"] {
                assert!(item.get(key).is_some(), "{key}");
            }
            assert_eq!(item["isDefault"], true);
            // The fixture's plan_type: a Codex legacy label; meaningless (hidden) on Claude.
            let legacy = (account.cli_kind == CliKind::Codex).then_some("pro");
            assert_eq!(item["planLabel"], json!(legacy));
        }
    }

    #[test]
    fn provider_category_patch_keeps_an_r216_readable_legacy_value() {
        use crate::profiles::provider_category::ProviderCategory;
        let config = CodezConfig::default();
        let mut claude = account(CliKind::Claude, "official");
        let item = profile_item(&claude, &config, true);
        assert_eq!(item["appearance"]["providerCategory"], Value::Null);
        assert_eq!(item["appearance"]["effectiveProviderCategory"], "claude");
        assert_eq!(item["effectiveProviderCategory"], "claude");
        assert_eq!(item["detail"]["editable"]["providerCategory"], true);

        for (category, legacy) in [(ProviderCategory::Zai, "glm"), (ProviderCategory::Kimi, "other"), (ProviderCategory::Codex, "chatgpt")] {
            let patch = ProfileApiPatch { provider_category: ConfigValueUpdate::Set(category), ..Default::default() };
            apply_store_options(&mut claude, &patch).unwrap();
            let stored = serde_json::to_value(&claude).unwrap();
            assert_eq!(stored["options"]["provider"], legacy);
            assert_eq!(stored["provider_category"], category.key());
            let item = profile_item(&claude, &config, false);
            assert_eq!(item["appearance"]["providerCategory"], category.key());
            assert_eq!(item["effectiveProviderCategory"], category.key());
            // r216 ignores the extra account field (no deny_unknown_fields).
            let reread: StoredAccount = serde_json::from_value(stored).unwrap();
            assert_eq!(crate::profiles::provider_category::manual(&reread), Some(category));
        }
        let patch = ProfileApiPatch { provider_category: ConfigValueUpdate::Clear, ..Default::default() };
        apply_store_options(&mut claude, &patch).unwrap();
        assert_eq!((claude.options.provider, claude.provider_category.as_deref()), (None, None));
    }

    #[test]
    fn native_subagents_default_on_and_only_explicit_choices_reach_the_launch() {
        let mut codex = account(CliKind::Codex, "api-key");
        assert_eq!(native_subagents(&codex), Some(true));
        assert!(native_subagent_overrides(&codex).is_empty());
        codex.options.native_subagents = Some(false);
        assert_eq!(native_subagents(&codex), Some(false));
        assert_eq!(
            native_subagent_overrides(&codex),
            vec![("features.multi_agent", false), ("features.multi_agent_v2", false)]
        );
        codex.options.native_subagents = Some(true);
        assert_eq!(native_subagent_overrides(&codex), vec![("features.multi_agent", true)]);
        let detail = profile_item(&codex, &CodezConfig::default(), true);
        assert_eq!(detail["nativeSubagents"], true);
        assert_eq!(detail["detail"]["nativeSubagentsExplicit"], true);
        assert_eq!(detail["detail"]["editable"]["nativeSubagents"], true);
        assert_eq!(detail["agentUseRequiresApproval"], false);

        let mut claude = account(CliKind::Claude, "official");
        claude.options.native_subagents = Some(false);
        assert_eq!(native_subagents(&claude), None);
        assert!(native_subagent_overrides(&claude).is_empty());
        let detail = profile_item(&claude, &CodezConfig::default(), true);
        assert_eq!(detail["detail"]["editable"]["nativeSubagents"], false);
    }

    #[test]
    fn browser_tools_default_off_and_editable_on_both_runtimes() {
        for kind in [CliKind::Codex, CliKind::Claude] {
            let mut profile=account(kind,"api-key");
            assert!(!browser_tools(&profile));
            let view=profile_item(&profile,&CodezConfig::default(),true);
            assert_eq!(view["browserTools"],false);
            assert_eq!(view["detail"]["editable"]["browserTools"],true);
            let original = profile.clone();
            apply_store_options(&mut profile,&ProfileApiPatch {browser_tools:Some(true),..Default::default()}).unwrap();
            assert!(browser_tools(&profile));
            // A browserTools-only PATCH must count as a change, or update_profile skips the save.
            assert!(store_fields_changed(&profile, &original));
            let saved = serde_json::to_value(&profile).unwrap();
            assert_eq!(saved["browser_tools"], true);
            assert!(saved["options"].get("browser_tools").is_none());
            assert_eq!(profile_item(&profile, &CodezConfig::default(), true)["browserTools"], true);
            apply_store_options(&mut profile,&ProfileApiPatch {browser_tools:Some(false),..Default::default()}).unwrap();
            assert!(!browser_tools(&profile));
            assert!(serde_json::to_value(&profile).unwrap().get("browser_tools").is_none());
        }
    }

    #[test]
    fn private_chatgpt_auth_file_is_a_codex_switch_default_off() {
        let mut profile = account(CliKind::Codex, "official");
        let view = profile_item(&profile, &CodezConfig::default(), true);
        assert_eq!(view["privateChatgptAuthFile"], false);
        assert_eq!(view["detail"]["editable"]["privateChatgptAuthFile"], true);
        let original = profile.clone();
        let on = ProfileApiPatch { private_chatgpt_auth_file: Some(true), ..Default::default() };
        apply_store_options(&mut profile, &on).unwrap();
        assert!(store_fields_changed(&profile, &original));
        let saved = serde_json::to_value(&profile).unwrap();
        assert_eq!(saved["private_chatgpt_auth_file"], true, "top level, outside options");
        assert_eq!(profile_item(&profile, &CodezConfig::default(), false)["privateChatgptAuthFile"], true);
        apply_store_options(&mut profile, &ProfileApiPatch { private_chatgpt_auth_file: Some(false), ..Default::default() }).unwrap();
        assert!(serde_json::to_value(&profile).unwrap().get("private_chatgpt_auth_file").is_none());
        let mut claude = account(CliKind::Claude, "official");
        assert!(apply_store_options(&mut claude, &on).is_err());
        let view = profile_item(&claude, &CodezConfig::default(), true);
        assert_eq!(view["privateChatgptAuthFile"], Value::Null);
        assert_eq!(view["detail"]["editable"]["privateChatgptAuthFile"], false);
    }

    #[test]
    fn agent_turn_questions_default_decide_for_claude_and_refused_for_codex() {
        let mut claude = account(CliKind::Claude, "official");
        assert_eq!(agent_turn_questions(&claude), Some(AgentTurnQuestions::Decide));
        let detail = profile_item(&claude, &CodezConfig::default(), true);
        assert_eq!(detail["agentTurnQuestions"], "decide");
        assert_eq!(detail["detail"]["agentTurnQuestionsExplicit"], Value::Null);
        assert_eq!(detail["detail"]["agentTurnQuestionsDefault"], "decide");
        assert_eq!(detail["detail"]["agentTurnQuestionsOptions"], json!(["decide", "wait"]));
        assert_eq!(detail["detail"]["editable"]["agentTurnQuestions"], true);

        let wait = ProfileApiPatch {
            agent_turn_questions: ConfigValueUpdate::Set(AgentTurnQuestions::Wait),
            ..ProfileApiPatch::default()
        };
        apply_store_options(&mut claude, &wait).unwrap();
        assert_eq!(agent_turn_questions(&claude), Some(AgentTurnQuestions::Wait));
        assert_eq!(
            serde_json::to_value(&claude.options).unwrap()["agent_turn_questions"],
            "wait"
        );
        let detail = profile_item(&claude, &CodezConfig::default(), true);
        assert_eq!(detail["agentTurnQuestions"], "wait");
        assert_eq!(detail["detail"]["agentTurnQuestionsExplicit"], "wait");
        let clear = ProfileApiPatch {
            agent_turn_questions: ConfigValueUpdate::Clear,
            ..ProfileApiPatch::default()
        };
        apply_store_options(&mut claude, &clear).unwrap();
        assert_eq!(claude.options.agent_turn_questions, None);
        assert!(serde_json::to_value(&claude.options)
            .unwrap()
            .get("agent_turn_questions")
            .is_none());

        let mut codex = account(CliKind::Codex, "api-key");
        assert_eq!(agent_turn_questions(&codex), None);
        let detail = profile_item(&codex, &CodezConfig::default(), true);
        assert_eq!(detail["agentTurnQuestions"], Value::Null);
        assert_eq!(detail["detail"]["agentTurnQuestionsDefault"], Value::Null);
        assert_eq!(detail["detail"]["editable"]["agentTurnQuestions"], false);
        assert!(apply_store_options(&mut codex, &wait).is_err());
        assert!(apply_store_options(&mut codex, &clear).is_err());
    }

    #[test]
    fn file_checkpointing_is_opt_in_and_claude_only() {
        let mut claude = account(CliKind::Claude, "official");
        assert_eq!(claude_file_checkpointing(&claude), Some(false));
        let detail = profile_item(&claude, &CodezConfig::default(), true);
        assert_eq!(detail["claudeFileCheckpointing"], false);
        assert_eq!(detail["detail"]["claudeFileCheckpointingExplicit"], Value::Null);
        assert_eq!(detail["detail"]["claudeFileCheckpointingDefault"], false);
        assert_eq!(detail["detail"]["editable"]["claudeFileCheckpointing"], true);
        for value in [true, false] {
            let patch = ProfileApiPatch {
                claude_file_checkpointing: ConfigValueUpdate::Set(value),
                ..ProfileApiPatch::default()
            };
            apply_store_options(&mut claude, &patch).unwrap();
            assert_eq!(claude_file_checkpointing(&claude), Some(value));
            let raw = serde_json::to_value(&claude.options).unwrap();
            assert_eq!(raw["claude_file_checkpointing"], value);
            let restored: appearance::ProfileOptions = serde_json::from_value(raw).unwrap();
            assert_eq!(restored.claude_file_checkpointing, Some(value));
            let detail = profile_item(&claude, &CodezConfig::default(), true);
            assert_eq!(detail["detail"]["claudeFileCheckpointingExplicit"], value);
            let mut codex = account(CliKind::Codex, "api-key");
            assert!(apply_store_options(&mut codex, &patch).is_err());
        }
        let clear = ProfileApiPatch {
            claude_file_checkpointing: ConfigValueUpdate::Clear,
            ..ProfileApiPatch::default()
        };
        apply_store_options(&mut claude, &clear).unwrap();
        assert_eq!(claude_file_checkpointing(&claude), Some(false));
        assert!(serde_json::to_value(&claude.options).unwrap().get("claude_file_checkpointing").is_none());
        let mut codex = account(CliKind::Codex, "api-key");
        let detail = profile_item(&codex, &CodezConfig::default(), true);
        assert_eq!(detail["claudeFileCheckpointing"], Value::Null);
        assert_eq!(detail["detail"]["claudeFileCheckpointingDefault"], Value::Null);
        assert_eq!(detail["detail"]["editable"]["claudeFileCheckpointing"], false);
        assert!(apply_store_options(&mut codex, &clear).is_err());
    }

    #[test]
    fn claude_tools_editable_for_claude_only() {
        let mut claude = account(CliKind::Claude, "official");
        let detail = profile_item(&claude, &CodezConfig::default(), true);
        // Unset = the default (on).
        assert_eq!(detail["claudeAllowWrite"], true);
        assert_eq!(detail["claudeAllowExec"], true);
        assert_eq!(detail["detail"]["claudeAllowWriteExplicit"], Value::Null);
        assert_eq!(detail["detail"]["claudeAllowWriteDefault"], true);
        assert_eq!(detail["detail"]["claudeAllowExecExplicit"], Value::Null);
        assert_eq!(detail["detail"]["claudeAllowExecDefault"], true);
        assert_eq!(detail["detail"]["editable"]["claudeTools"], true);
        let off = ProfileApiPatch {
            claude_allow_write: ConfigValueUpdate::Set(false),
            claude_allow_exec: ConfigValueUpdate::Set(false),
            ..ProfileApiPatch::default()
        };
        apply_store_options(&mut claude, &off).unwrap();
        assert_eq!((claude.options.claude.allow_write, claude.options.claude.allow_exec), (Some(false), Some(false)));
        let detail = profile_item(&claude, &CodezConfig::default(), true);
        assert_eq!(detail["claudeAllowWrite"], false);
        assert_eq!(detail["claudeAllowExec"], false);
        assert_eq!(detail["detail"]["claudeAllowWriteExplicit"], false);
        let clear = ProfileApiPatch { claude_allow_exec: ConfigValueUpdate::Clear, ..ProfileApiPatch::default() };
        apply_store_options(&mut claude, &clear).unwrap();
        assert_eq!((claude.options.claude.allow_write, claude.options.claude.allow_exec), (Some(false), None));
        assert!(!claude.options.claude.write() && claude.options.claude.exec());

        let mut codex = account(CliKind::Codex, "api-key");
        let detail = profile_item(&codex, &CodezConfig::default(), true);
        assert_eq!(detail["claudeAllowWrite"], Value::Null);
        assert_eq!(detail["detail"]["claudeAllowWriteDefault"], Value::Null);
        assert_eq!(detail["detail"]["editable"]["claudeTools"], false);
        assert!(apply_store_options(&mut codex, &off).is_err());
        assert!(apply_store_options(&mut codex, &clear).is_err());
    }

    #[test]
    fn session_switch_defaults_off_for_subscriptions_and_explicit_wins() {
        let mut official = account(CliKind::Codex, "official");
        assert!(!allow_session_switch(&official));
        official.options.allow_session_switch = Some(true);
        assert!(allow_session_switch(&official));
        assert!(!allow_session_switch(&account(CliKind::Claude, "official")));
    }

    #[test]
    fn api_key_debug_is_redacted() {
        let patch = ProfileApiPatch {
            api_key: Some(ApiKey("sk-secret-value".into())),
            ..ProfileApiPatch::default()
        };
        let debug = format!("{patch:?}");
        assert!(!debug.contains("sk-secret-value"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn api_key_payload_preserves_fields_and_is_noop_when_unchanged() {
        let current = json!({ "auth_mode": "apikey", "OPENAI_API_KEY": "k1",
                              "last_refresh": "2026-10-06T00:00:00Z", "tokens": null });
        assert_eq!(api_key_auth_payload(Some(&current), "k1"), current);
        let next = api_key_auth_payload(Some(&current), "k2");
        assert_eq!(next["OPENAI_API_KEY"], "k2");
        assert_eq!(next["auth_mode"], "apikey");
        assert_eq!(next["last_refresh"], "2026-10-06T00:00:00Z");
        assert!(next.get("tokens").is_some());

        let lowercase = json!({ "openai_api_key": "k1" });
        assert_eq!(api_key_auth_payload(Some(&lowercase), "k1"), lowercase);
        let folded = api_key_auth_payload(Some(&lowercase), "k2");
        assert_eq!(folded, json!({ "OPENAI_API_KEY": "k2" }));

        assert_eq!(api_key_auth_payload(None, "k"), json!({ "OPENAI_API_KEY": "k" }));
        let chatgpt_mode = json!({ "auth_mode": "chatgpt", "OPENAI_API_KEY": null });
        assert_eq!(api_key_auth_payload(Some(&chatgpt_mode), "k")["auth_mode"], "apikey");
    }

    #[test]
    fn model_display_names_merge_per_entry_for_every_profile_kind() {
        let patch = |pairs: &[(&str, Option<&str>)]| ProfileApiPatch {
            model_display_names: pairs.iter().map(|(id, name)| (id.to_string(), name.map(str::to_string))).collect(),
            ..ProfileApiPatch::default()
        };
        for (kind, source) in [(CliKind::Codex, "official"), (CliKind::Codex, "api-key"), (CliKind::Claude, "official")] {
            let mut profile = account(kind, source);
            let original = profile.clone();
            apply_store_options(&mut profile, &patch(&[("gpt-6-astra", Some("  Astra   6 ")), ("Astra [400K]", Some("Astra 400K"))])).unwrap();
            assert!(store_fields_changed(&profile, &original));
            assert_eq!(
                profile.model_display_names,
                [("Astra [400K]".to_string(), "Astra 400K".to_string()), ("gpt-6-astra".to_string(), "Astra 6".to_string())].into()
            );
            // Merge: other entries stay; null, "" and a name equal to the id remove.
            apply_store_options(&mut profile, &patch(&[("gpt-6-astra", None), ("sonnet", Some("sonnet"))])).unwrap();
            assert_eq!(profile.model_display_names, [("Astra [400K]".to_string(), "Astra 400K".to_string())].into());
            apply_store_options(&mut profile, &patch(&[("Astra [400K]", Some("   "))])).unwrap();
            assert!(profile.model_display_names.is_empty());
            for bad in [patch(&[(" padded", Some("x"))]), patch(&[("m", Some(&"x".repeat(81)))]), patch(&[("m", Some("a\u{202e}b"))])] {
                assert!(apply_store_options(&mut profile, &bad).is_err());
            }
            let item = profile_item(&profile, &CodezConfig::default(), false);
            assert_eq!(item["modelDisplayNames"], json!({}));
        }
        let mut profile = account(CliKind::Codex, "official");
        apply_store_options(&mut profile, &patch(&[("gpt-5.5", Some("Five"))])).unwrap();
        assert_eq!(profile_item(&profile, &CodezConfig::default(), false)["modelDisplayNames"], json!({"gpt-5.5": "Five"}));
        // Stored at the top level; an empty map is not written (older binaries ignore the key).
        let stored = serde_json::to_value(&profile).unwrap();
        assert_eq!(stored["model_display_names"], json!({"gpt-5.5": "Five"}));
        assert!(serde_json::to_value(account(CliKind::Codex, "official")).unwrap().get("model_display_names").is_none());
    }

    #[test]
    fn plan_label_store_options_and_clear_drops_codex_legacy_annotation() {
        let mut codex = account(CliKind::Codex, "api-key");
        let set = ProfileApiPatch {
            plan_label: ConfigValueUpdate::Set(" GLM Pro ".into()),
            ..ProfileApiPatch::default()
        };
        let original = codex.clone();
        apply_store_options(&mut codex, &set).unwrap();
        assert_eq!(codex.plan_label.as_deref(), Some("GLM Pro"));
        assert_eq!(codex.plan_type.as_deref(), Some("pro"));
        assert!(store_fields_changed(&codex, &original));
        let clear = ProfileApiPatch { plan_label: ConfigValueUpdate::Clear, ..ProfileApiPatch::default() };
        apply_store_options(&mut codex, &clear).unwrap();
        assert_eq!((codex.plan_label.as_deref(), codex.plan_type.as_deref()), (None, None));
        let too_long = ProfileApiPatch {
            plan_label: ConfigValueUpdate::Set("x".repeat(41)),
            ..ProfileApiPatch::default()
        };
        assert!(apply_store_options(&mut codex, &too_long).is_err());
        // Codex API-key profile: the legacy plan_type still shows without a label.
        let mut legacy = account(CliKind::Codex, "api-key");
        legacy.plan_type = Some("DeepSeek".into());
        let item = profile_item(&legacy, &CodezConfig::default(), true);
        assert_eq!((item["planLabel"].clone(), item["planLabelSource"].clone()), (json!("DeepSeek"), json!("manual")));
        assert_eq!(item["detail"]["planLabelManual"], "DeepSeek");
        assert_eq!(item["detail"]["editable"]["planLabel"], true);
    }

    #[test]
    fn claude_profile_plan_label_is_manual_only() {
        // The fixture carries a stray plan_type: Claude profiles never show it.
        let mut claude = account(CliKind::Claude, "official");
        assert_eq!(claude.plan_type.as_deref(), Some("pro"));
        assert_eq!(crate::profiles::plan_label::account_plan(&claude), None);
        assert_eq!(crate::profiles::plan_label::effective_plan_label(&claude), None);
        let item = profile_item(&claude, &CodezConfig::default(), true);
        assert_eq!((item["planLabel"].clone(), item["planLabelSource"].clone()), (Value::Null, Value::Null));
        assert_eq!(item["detail"]["planLabelManual"], Value::Null);
        assert_eq!(item["detail"]["editable"]["planLabel"], true);

        let original = claude.clone();
        let set = ProfileApiPatch {
            plan_label: ConfigValueUpdate::Set(" Max 20x ".into()),
            ..ProfileApiPatch::default()
        };
        apply_store_options(&mut claude, &set).unwrap();
        assert_eq!(claude.plan_label.as_deref(), Some("Max 20x"));
        assert!(store_fields_changed(&claude, &original));
        let item = profile_item(&claude, &CodezConfig::default(), true);
        assert_eq!((item["planLabel"].clone(), item["planLabelSource"].clone()), (json!("Max 20x"), json!("manual")));
        assert_eq!(item["detail"]["planLabelManual"], "Max 20x");
        assert_eq!(
            crate::profiles::plan_label::effective_plan_label(&claude),
            Some(("Max 20x".to_string(), crate::profiles::plan_label::PlanLabelSource::Manual))
        );
        let too_long = ProfileApiPatch {
            plan_label: ConfigValueUpdate::Set("x".repeat(41)),
            ..ProfileApiPatch::default()
        };
        assert!(apply_store_options(&mut claude, &too_long).is_err());
        assert_eq!(claude.plan_label.as_deref(), Some("Max 20x"));

        // Clear drops the label only; plan_type is left as it was.
        let clear = ProfileApiPatch { plan_label: ConfigValueUpdate::Clear, ..ProfileApiPatch::default() };
        apply_store_options(&mut claude, &clear).unwrap();
        assert_eq!((claude.plan_label.as_deref(), claude.plan_type.as_deref()), (None, Some("pro")));
        assert!(!store_fields_changed(&claude, &original));
        let item = profile_item(&claude, &CodezConfig::default(), false);
        assert_eq!((item["planLabel"].clone(), item["planLabelSource"].clone()), (Value::Null, Value::Null));
    }

    #[test]
    fn chatgpt_profile_manual_label_overrides_the_live_account_plan() {
        use crate::management::v2::profile_login::profile_account_view;
        use crate::management::v2::profile_quota_native as native;
        use crate::profiles::lifecycle::{create_profile, NewProfile, NewProfileKind};
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let created = create_profile(&NewProfile {
            name: "work".into(),
            kind: NewProfileKind::Chatgpt,
            preset: None,
            base_url: None,
            model: None,
            api_key: None,
        })
        .expect("chatgpt profile");
        // Not signed in: the stored label is shown.
        let (labelled, changed) = update_profile(
            &created.id,
            &ProfileApiPatch { plan_label: ConfigValueUpdate::Set("free".into()), ..ProfileApiPatch::default() },
        )
        .unwrap();
        assert!(changed);
        let item = profile_item(&labelled, &CodezConfig::default(), true);
        assert_eq!(item["planLabel"], "free");
        assert_eq!(item["planLabelSource"], "manual");
        assert_eq!(item["detail"]["planLabelManual"], "free");
        assert_eq!(item["detail"]["editable"]["planLabel"], true);

        // Signed in (token says pro): the owner's label still wins.
        let issued = 1_791_000_000;
        let files = materialized_account_files(&labelled).unwrap();
        std::fs::create_dir_all(files.auth_path.parent().unwrap()).unwrap();
        std::fs::write(&files.auth_path, crate::profiles::plan_label::test_chatgpt_auth_at("pro", Some(issued)))
            .unwrap();
        let item = profile_item(&labelled, &CodezConfig::default(), true);
        assert_eq!(item["planLabel"], "free");
        assert_eq!(item["planLabelSource"], "manual");
        assert_eq!(item["detail"]["planLabelManual"], "free");
        assert!(!item.to_string().contains("plan-test@example.com"));
        assert_eq!(profile_account_view(&files.auth_path, &labelled.id)["planType"], "pro");

        // Clearing drops the stored label: the account's plan shows.
        let (cleared, changed) = update_profile(
            &created.id,
            &ProfileApiPatch { plan_label: ConfigValueUpdate::Clear, ..ProfileApiPatch::default() },
        )
        .unwrap();
        assert!(changed);
        assert_eq!((cleared.plan_label.as_deref(), cleared.plan_type.as_deref()), (None, None));
        let item = profile_item(&cleared, &CodezConfig::default(), true);
        assert_eq!((item["planLabel"].clone(), item["planLabelSource"].clone()), (json!("pro"), json!("account")));
        assert_eq!(item["detail"]["planLabelManual"], Value::Null);
        let (_, changed) = update_profile(
            &created.id,
            &ProfileApiPatch { plan_label: ConfigValueUpdate::Clear, ..ProfileApiPatch::default() },
        )
        .unwrap();
        assert!(!changed);

        // The native rate-limit read observed a downgrade after the token was
        // minted: the live plan (label and login view) follows it.
        let read = json!({ "rateLimits": { "planType": "free",
            "primary": { "usedPercent": 1, "windowDurationMins": 300 } } });
        let dir = native::store_dir().unwrap();
        native::apply_read_at(&dir, &cleared, Ok(read.clone()), issued - 10).unwrap();
        assert_eq!(profile_item(&cleared, &CodezConfig::default(), false)["planLabel"], "pro");
        native::apply_read_at(&dir, &cleared, Ok(read), issued + 60).unwrap();
        let item = profile_item(&cleared, &CodezConfig::default(), true);
        assert_eq!((item["planLabel"].clone(), item["planLabelSource"].clone()), (json!("free"), json!("account")));
        assert_eq!(profile_account_view(&files.auth_path, &cleared.id)["planType"], "free");
        // The plain auth-file view knows only the token.
        assert_eq!(crate::management::v2::profile_login::account_view(&files.auth_path)["planType"], "pro");

        // A legacy last-login plan_type stays behind the live plan and out of
        // the editable field.
        let mut legacy = cleared.clone();
        legacy.plan_type = Some("plus".into());
        let item = profile_item(&legacy, &CodezConfig::default(), true);
        assert_eq!(item["planLabel"], "free");
        assert_eq!(item["detail"]["planLabelManual"], Value::Null);

        // Signed out: the old snapshot is ignored; only stored labels remain.
        std::fs::remove_file(&files.auth_path).unwrap();
        let item = profile_item(&legacy, &CodezConfig::default(), true);
        assert_eq!((item["planLabel"].clone(), item["planLabelSource"].clone()), (json!("plus"), json!("manual")));
        assert_eq!(item["detail"]["planLabelManual"], "plus");
        let item = profile_item(&cleared, &CodezConfig::default(), false);
        assert_eq!((item["planLabel"].clone(), item["planLabelSource"].clone()), (Value::Null, Value::Null));
    }

    #[test]
    fn reasoning_summary_round_trips_through_profile_views() {
        use crate::profiles::lifecycle::{create_profile, NewProfile, NewProfileKind};
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let config = CodezConfig::default();
        let summary = |update: ConfigValueUpdate<String>| ProfileApiPatch {
            codex_config: CodexProfileConfigPatch {
                model_reasoning_summary: update,
                ..CodexProfileConfigPatch::default()
            },
            ..ProfileApiPatch::default()
        };
        let created = create_profile(&NewProfile {
            name: "work".into(),
            kind: NewProfileKind::Chatgpt,
            preset: None,
            base_url: None,
            model: Some("gpt-5.5".into()),
            api_key: None,
        })
        .expect("chatgpt profile");
        let item = profile_item(&created, &config, true);
        assert_eq!(item["modelReasoningSummary"], Value::Null);
        assert_eq!(item["detail"]["config"]["modelReasoningSummary"], Value::Null);
        assert_eq!(item["detail"]["editable"]["modelReasoningSummary"], true);

        let (updated, changed) =
            update_profile(&created.id, &summary(ConfigValueUpdate::Set("detailed".into()))).unwrap();
        assert!(changed);
        let item = profile_item(&updated, &config, true);
        assert_eq!(item["modelReasoningSummary"], "detailed");
        assert_eq!(item["detail"]["config"]["modelReasoningSummary"], "detailed");
        assert_eq!(item["detail"]["config"]["model"], "gpt-5.5");
        assert_eq!(profile_item(&updated, &config, false)["modelReasoningSummary"], "detailed");
        // Native launches read the materialized profile config.
        let files = materialized_account_files(&updated).unwrap();
        let written = std::fs::read_to_string(&files.config_path).unwrap();
        assert!(written.contains("model_reasoning_summary = \"detailed\""), "{written}");
        let (_, changed) =
            update_profile(&created.id, &summary(ConfigValueUpdate::Set("detailed".into()))).unwrap();
        assert!(!changed);

        let (cleared, changed) = update_profile(&created.id, &summary(ConfigValueUpdate::Clear)).unwrap();
        assert!(changed);
        let item = profile_item(&cleared, &config, true);
        assert_eq!(item["modelReasoningSummary"], Value::Null);
        assert_eq!(item["detail"]["config"]["modelReasoningSummary"], Value::Null);
        assert!(!std::fs::read_to_string(&files.config_path).unwrap().contains("model_reasoning_summary"));
        assert!(update_profile(&created.id, &summary(ConfigValueUpdate::Set("verbose".into()))).is_err());

        // DeepSeek: not offered; a set is refused before anything is written.
        let deepseek = create_profile(&NewProfile {
            name: "ds".into(),
            kind: NewProfileKind::ApiKey,
            preset: crate::profiles::providers::find_preset("deepseek"),
            base_url: None,
            model: None,
            api_key: None,
        })
        .expect("deepseek profile");
        let item = profile_item(&deepseek, &config, true);
        assert_eq!(item["detail"]["editable"]["modelReasoningSummary"], false);
        assert_eq!(item["detail"]["editable"]["modelReasoningEffort"], true);
        assert_eq!(item["detail"]["config"]["modelReasoningSummary"], Value::Null);
        let deepseek_config = materialized_account_files(&deepseek).unwrap().config_path;
        let before = std::fs::read_to_string(&deepseek_config).unwrap();
        let error = update_profile(&deepseek.id, &summary(ConfigValueUpdate::Set("auto".into()))).unwrap_err();
        assert!(format!("{error:#}").contains("DeepSeek"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&deepseek_config).unwrap(), before);
        let (_, changed) = update_profile(&deepseek.id, &summary(ConfigValueUpdate::Clear)).unwrap();
        assert!(!changed);

        let claude = profile_item(&account(CliKind::Claude, "official"), &config, true);
        assert_eq!(claude["modelReasoningSummary"], Value::Null);
        assert_eq!(claude["detail"]["editable"]["modelReasoningSummary"], false);
    }

    #[test]
    fn tool_schema_compat_round_trips_and_the_gemini_preset_sets_strict() {
        use crate::profiles::lifecycle::{create_profile, NewProfile, NewProfileKind};
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let config = CodezConfig::default();
        let compat = |update: ConfigValueUpdate<String>| ProfileApiPatch {
            codex_config: CodexProfileConfigPatch { tool_schema_compat: update, ..CodexProfileConfigPatch::default() },
            ..ProfileApiPatch::default()
        };
        let gemini = create_profile(&NewProfile {
            name: "gem".into(),
            kind: NewProfileKind::ApiKey,
            preset: crate::profiles::providers::find_preset("gemini"),
            base_url: Some("https://relay.example.com/v1".into()),
            model: Some("gemini-3-flash".into()),
            api_key: None,
        })
        .expect("gemini profile");
        let item = profile_item(&gemini, &config, true);
        assert_eq!(item["toolSchemaCompat"], "strict");
        assert_eq!(item["detail"]["config"]["toolSchemaCompat"], "strict");
        assert_eq!(item["detail"]["editable"]["toolSchemaCompat"], true);
        let files = materialized_account_files(&gemini).unwrap();
        let written = std::fs::read_to_string(&files.config_path).unwrap();
        let mut table: toml::Value = toml::from_str(&written).unwrap();
        assert_eq!(table["tool_schema_compat"].as_str(), Some("strict"), "{written}");
        // The selected-profile launch projection accepts and forwards it.
        table.as_table_mut().unwrap().insert("cutex_provider_mode".into(), "selected_profile_v2".into());
        let parsed = crate::launch::selected_profile::Config::parse(&toml::to_string(&table).unwrap())
            .expect("selected projection accepts tool_schema_compat");
        assert_eq!(parsed.tool_schema_compat.as_deref(), Some("strict"));
        // The explicit-launch merge carries the profile-scoped key into the
        // agent's native config and drops it again when the profile clears it.
        let agent_config = files.config_path.with_file_name("agent-config.toml");
        std::fs::write(&agent_config, "model = \"x\"\ntool_schema_compat = \"strict\"\n").unwrap();
        crate::profiles::profile_config::merge_and_write_config_toml(&agent_config, Some(&written), false).unwrap();
        let merged: toml::Value = toml::from_str(&std::fs::read_to_string(&agent_config).unwrap()).unwrap();
        assert_eq!(merged["tool_schema_compat"].as_str(), Some("strict"));
        crate::profiles::profile_config::merge_and_write_config_toml(&agent_config, Some("model = \"y\"\n"), false).unwrap();
        let merged: toml::Value = toml::from_str(&std::fs::read_to_string(&agent_config).unwrap()).unwrap();
        assert!(merged.get("tool_schema_compat").is_none(), "{merged}");

        // Reversible: null clears (native default "off"), values set it.
        let (cleared, changed) = update_profile(&gemini.id, &compat(ConfigValueUpdate::Clear)).unwrap();
        assert!(changed);
        assert_eq!(profile_item(&cleared, &config, true)["toolSchemaCompat"], Value::Null);
        assert!(!std::fs::read_to_string(&files.config_path).unwrap().contains("tool_schema_compat"));
        assert!(update_profile(&gemini.id, &compat(ConfigValueUpdate::Set("gemini".into()))).is_err());
        let (off, changed) = update_profile(&gemini.id, &compat(ConfigValueUpdate::Set("off".into()))).unwrap();
        assert!(changed);
        assert_eq!(profile_item(&off, &config, false)["toolSchemaCompat"], "off");

        // An existing custom profile adopts it with one PATCH.
        let custom = create_profile(&NewProfile {
            name: "relay".into(),
            kind: NewProfileKind::ApiKey,
            preset: crate::profiles::providers::find_preset("custom"),
            base_url: Some("https://relay.example.com/v1".into()),
            model: None,
            api_key: None,
        })
        .expect("custom profile");
        assert_eq!(profile_item(&custom, &config, true)["toolSchemaCompat"], Value::Null);
        let (adopted, changed) =
            update_profile(&custom.id, &compat(ConfigValueUpdate::Set("strict".into()))).unwrap();
        assert!(changed);
        assert_eq!(profile_item(&adopted, &config, true)["toolSchemaCompat"], "strict");
        let (_, changed) = update_profile(&custom.id, &compat(ConfigValueUpdate::Set("strict".into()))).unwrap();
        assert!(!changed);
    }

    #[test]
    fn web_search_round_trips_and_the_gemini_preset_disables_it() {
        use crate::profiles::lifecycle::{create_profile, NewProfile, NewProfileKind};
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let config = CodezConfig::default();
        let web_search = |update: ConfigValueUpdate<String>| ProfileApiPatch {
            codex_config: CodexProfileConfigPatch { web_search: update, ..CodexProfileConfigPatch::default() },
            ..ProfileApiPatch::default()
        };
        let gemini = create_profile(&NewProfile {
            name: "gem".into(),
            kind: NewProfileKind::ApiKey,
            preset: crate::profiles::providers::find_preset("gemini"),
            base_url: Some("https://relay.example.com/v1".into()),
            model: Some("gemini-3-flash".into()),
            api_key: None,
        })
        .expect("gemini profile");
        let item = profile_item(&gemini, &config, true);
        assert_eq!(item["webSearch"], "disabled");
        assert_eq!(item["detail"]["config"]["webSearch"], "disabled");
        assert_eq!(item["detail"]["editable"]["webSearch"], true);
        assert_eq!(item["providerFamily"], "custom:relay.example.com");
        assert_eq!(item["detail"]["config"]["model"], "gemini-3-flash", "the request's model wins");
        let files = materialized_account_files(&gemini).unwrap();
        let written = std::fs::read_to_string(&files.config_path).unwrap();
        let mut table: toml::Value = toml::from_str(&written).unwrap();
        assert_eq!(table["web_search"].as_str(), Some("disabled"), "{written}");
        // The selected-profile launch projection accepts and forwards it.
        table.as_table_mut().unwrap().insert("cutex_provider_mode".into(), "selected_profile_v2".into());
        let parsed = crate::launch::selected_profile::Config::parse(&toml::to_string(&table).unwrap())
            .expect("selected projection accepts web_search");
        assert_eq!(parsed.web_search.as_deref(), Some("disabled"));
        // A re-materialization keeps it.
        crate::profiles::materialize::refresh_materialized_account_config(&gemini).unwrap();
        assert_eq!(profile_item(&gemini, &config, false)["webSearch"], "disabled");

        // Reversible: null clears (native default), a value sets it again.
        let (cleared, changed) = update_profile(&gemini.id, &web_search(ConfigValueUpdate::Clear)).unwrap();
        assert!(changed);
        assert_eq!(profile_item(&cleared, &config, true)["webSearch"], Value::Null);
        assert!(!std::fs::read_to_string(&files.config_path).unwrap().contains("web_search"));
        assert!(update_profile(&gemini.id, &web_search(ConfigValueUpdate::Set("off".into()))).is_err());

        // An existing custom profile adopts it with one PATCH.
        let custom = create_profile(&NewProfile {
            name: "relay".into(),
            kind: NewProfileKind::ApiKey,
            preset: crate::profiles::providers::find_preset("custom"),
            base_url: Some("https://relay.example.com/v1".into()),
            model: None,
            api_key: None,
        })
        .expect("custom profile");
        assert_eq!(profile_item(&custom, &config, true)["webSearch"], Value::Null);
        assert_eq!(profile_item(&custom, &config, true)["detail"]["config"]["model"], Value::Null);
        let defaulted = create_profile(&NewProfile {
            name: "gem2".into(),
            kind: NewProfileKind::ApiKey,
            preset: crate::profiles::providers::find_preset("gemini"),
            base_url: Some("https://relay.example.com/v1".into()),
            model: None,
            api_key: None,
        })
        .expect("gemini profile without a model");
        assert_eq!(
            profile_item(&defaulted, &config, true)["detail"]["config"]["model"],
            crate::profiles::gemini::GEMINI_DEFAULT_MODEL
        );
        let (adopted, changed) =
            update_profile(&custom.id, &web_search(ConfigValueUpdate::Set("disabled".into()))).unwrap();
        assert!(changed);
        assert_eq!(profile_item(&adopted, &config, true)["webSearch"], "disabled");
        let (_, changed) =
            update_profile(&custom.id, &web_search(ConfigValueUpdate::Set("disabled".into()))).unwrap();
        assert!(!changed);

        // profile-search-tool: a per-profile owner switch, default off for
        // every preset (Gemini included); PATCH toggles it on API-key
        // profiles only.
        assert_eq!(profile_item(&gemini, &config, true)["searchTool"], false);
        assert_eq!(profile_item(&gemini, &config, true)["detail"]["editable"]["searchTool"], true);
        assert_eq!(profile_item(&custom, &config, true)["searchTool"], false);
        let search = |enabled| ProfileApiPatch { search_tool: Some(enabled), ..ProfileApiPatch::default() };
        let (custom_on, changed) = update_profile(&custom.id, &search(true)).unwrap();
        assert!(changed, "a searchTool-only PATCH is saved");
        assert!(crate::profiles::settings_api::search_tool(&custom_on));
        assert_eq!(serde_json::to_value(&custom_on).unwrap()["search_tool"], true);
        let (custom_off, changed) = update_profile(&custom.id, &search(false)).unwrap();
        assert!(changed);
        assert_eq!(profile_item(&custom_off, &config, true)["searchTool"], false);
        let chatgpt = create_profile(&NewProfile {
            name: "chatgpt-search".into(),
            kind: NewProfileKind::Chatgpt,
            preset: None,
            base_url: None,
            model: None,
            api_key: None,
        })
        .expect("chatgpt profile");
        assert_eq!(profile_item(&chatgpt, &config, true)["detail"]["editable"]["searchTool"], false);
        assert!(update_profile(&chatgpt.id, &search(true)).is_err(), "official profiles cannot opt in");
        assert!(update_profile(&chatgpt.id, &search(false)).is_ok());
    }

    #[test]
    fn update_plan_enabled_round_trips_and_reaches_the_selected_launch_projection() {
        use crate::profiles::lifecycle::{create_profile, NewProfile, NewProfileKind};
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let config = CodezConfig::default();
        let update_plan = |update: ConfigValueUpdate<bool>| ProfileApiPatch {
            codex_config: CodexProfileConfigPatch {
                update_plan_enabled: update,
                ..CodexProfileConfigPatch::default()
            },
            ..ProfileApiPatch::default()
        };
        let created = create_profile(&NewProfile {
            name: "work".into(),
            kind: NewProfileKind::Chatgpt,
            preset: None,
            base_url: None,
            model: Some("gpt-5.5".into()),
            api_key: None,
        })
        .expect("chatgpt profile");
        let item = profile_item(&created, &config, true);
        assert_eq!(item["updatePlanEnabled"], Value::Null);
        assert_eq!(item["detail"]["config"]["updatePlanEnabled"], Value::Null);
        assert_eq!(item["detail"]["editable"]["updatePlanEnabled"], true);

        let (updated, changed) = update_profile(&created.id, &update_plan(ConfigValueUpdate::Set(true))).unwrap();
        assert!(changed);
        let item = profile_item(&updated, &config, true);
        assert_eq!(item["updatePlanEnabled"], true);
        assert_eq!(item["detail"]["config"]["updatePlanEnabled"], true);
        assert_eq!(item["detail"]["config"]["model"], "gpt-5.5");
        assert_eq!(profile_item(&updated, &config, false)["updatePlanEnabled"], true);
        // The materialized profile config carries the nested table, and the
        // selected-profile launch projection accepts and forwards it.
        let files = materialized_account_files(&updated).unwrap();
        let written = std::fs::read_to_string(&files.config_path).unwrap();
        let table: toml::Value = toml::from_str(&written).unwrap();
        assert_eq!(table["tools"]["update_plan"]["enabled"].as_bool(), Some(true), "{written}");
        let mut selected = table.clone();
        selected
            .as_table_mut()
            .unwrap()
            .insert("cutex_provider_mode".into(), "selected_profile_v2".into());
        let parsed = crate::launch::selected_profile::Config::parse(&toml::to_string(&selected).unwrap())
            .expect("selected projection accepts the profile tools switch");
        assert_eq!(
            crate::launch::selected_profile::Tools::projected(parsed.tools)
                .and_then(|tools| tools.update_plan)
                .and_then(|switch| switch.enabled),
            Some(true)
        );
        // A re-materialization (e.g. a later unrelated edit) keeps it.
        crate::profiles::materialize::refresh_materialized_account_config(&updated).unwrap();
        assert_eq!(profile_item(&updated, &config, false)["updatePlanEnabled"], true);
        let (_, changed) = update_profile(&created.id, &update_plan(ConfigValueUpdate::Set(true))).unwrap();
        assert!(!changed);

        let (disabled, changed) = update_profile(&created.id, &update_plan(ConfigValueUpdate::Set(false))).unwrap();
        assert!(changed);
        assert_eq!(profile_item(&disabled, &config, true)["detail"]["config"]["updatePlanEnabled"], false);

        let (cleared, changed) = update_profile(&created.id, &update_plan(ConfigValueUpdate::Clear)).unwrap();
        assert!(changed);
        let item = profile_item(&cleared, &config, true);
        assert_eq!(item["updatePlanEnabled"], Value::Null);
        assert_eq!(item["detail"]["config"]["updatePlanEnabled"], Value::Null);
        let written = std::fs::read_to_string(&files.config_path).unwrap();
        assert!(!written.contains("update_plan"), "{written}");
        assert!(toml::from_str::<toml::Value>(&written).unwrap().get("tools").is_none(), "{written}");

        // DeepSeek keeps it (its preset does not touch `tools`).
        let deepseek = create_profile(&NewProfile {
            name: "ds".into(),
            kind: NewProfileKind::ApiKey,
            preset: crate::profiles::providers::find_preset("deepseek"),
            base_url: None,
            model: None,
            api_key: None,
        })
        .expect("deepseek profile");
        assert_eq!(profile_item(&deepseek, &config, true)["detail"]["editable"]["updatePlanEnabled"], true);
        let (deepseek, changed) = update_profile(&deepseek.id, &update_plan(ConfigValueUpdate::Set(true))).unwrap();
        assert!(changed);
        assert_eq!(profile_item(&deepseek, &config, true)["updatePlanEnabled"], true);

        let claude = profile_item(&account(CliKind::Claude, "official"), &config, true);
        assert_eq!(claude["updatePlanEnabled"], Value::Null);
        assert_eq!(claude["detail"]["editable"]["updatePlanEnabled"], false);
    }

    #[test]
    fn profile_item_reports_family_and_editability() {
        let config = CodezConfig::default();
        let item = profile_item(&account(CliKind::Claude, "official"), &config, true);
        assert_eq!(item["providerFamily"], "anthropic");
        assert_eq!(item["providerFamilyLabel"], "Anthropic (Claude)");
        assert_eq!(item["detail"]["editable"]["name"], true);
        assert_eq!(item["detail"]["editable"]["providerEnvKey"], false);
        let item = profile_item(&account(CliKind::Codex, "api-key"), &config, true);
        assert_eq!(item["detail"]["editable"]["providerEnvKey"], false);
        assert!(item["providerFamily"].is_string());
    }

    #[test]
    fn preset_base_url_edit_keeps_the_explicit_reader_and_moves_the_family() {
        use crate::profiles::lifecycle::{create_profile, NewProfile, NewProfileKind};
        use crate::profiles::quota::{config_view, Choice, Reader};
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let config = CodezConfig::default();
        let base_url = |url: &str| ProfileApiPatch {
            codex_config: CodexProfileConfigPatch {
                provider_base_url: ConfigValueUpdate::Set(url.into()),
                ..CodexProfileConfigPatch::default()
            },
            ..ProfileApiPatch::default()
        };
        let created = create_profile(&NewProfile {
            name: "kimi".into(),
            kind: NewProfileKind::ApiKey,
            preset: crate::profiles::providers::find_preset("moonshot"),
            base_url: None,
            model: None,
            api_key: None,
        })
        .expect("moonshot profile");
        let item = profile_item(&created, &config, true);
        assert_eq!(item["detail"]["editable"]["providerOptions"], true);
        assert_eq!(item["providerFamily"], "moonshot");
        assert_eq!(config_view(&created)["resolvedPrimary"]["url"], "https://api.moonshot.ai/v1/users/me/balance");

        // The reader is the owner's explicit choice: a base URL edit never
        // picks another one by host (the family still follows the host).
        for (url, family) in [
            ("https://api.moonshot.cn/v1", "moonshot"),
            ("https://api.deepseek.com/v1", "deepseek"),
            ("https://relay.example/v1", "moonshot"),
        ] {
            let (updated, changed) = update_profile(&created.id, &base_url(url)).unwrap();
            assert!(changed, "{url}");
            assert_eq!(updated.quota_reader, Some(Reader::MoonshotBalance), "{url}");
            let item = profile_item(&updated, &config, true);
            assert_eq!(item["baseUrl"], url);
            assert_eq!(item["providerFamily"], family, "{url}");
            let view = config_view(&updated);
            assert_eq!(view["resolvedPrimary"]["url"], "https://api.moonshot.ai/v1/users/me/balance", "{url}");
            assert_eq!(view["preset"], "moonshot_balance");
        }

        // The generic usage endpoint follows the current base URL.
        let mut store = load_store().unwrap();
        let account = store.accounts.iter_mut().find(|a| a.id == created.id).unwrap();
        crate::profiles::quota::apply_choice(account, Choice::Reader(Reader::UsageEndpoint)).unwrap();
        assert_eq!(account.options.quota.url, "https://relay.example/v1/usage", "resolved endpoint stored");
        crate::profiles::store::save_store(&store).unwrap();
        let (updated, _) = update_profile(&created.id, &base_url("https://relay.example/v2/")).unwrap();
        let view = config_view(&updated);
        assert_eq!(view["preset"], "usage_endpoint");
        assert_eq!(view["resolvedPrimary"]["url"], "https://relay.example/v2/usage");
        assert_eq!(view["presetError"], Value::Null);
    }
}
