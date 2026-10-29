//! Root-only profile settings routes:
//! `GET|POST /v2/profiles`, `GET|PATCH|DELETE /v2/profiles/{profile}`,
//! `POST /v2/profiles/{profile}/duplicate`, `GET /v2/profile-presets`,
//! `POST /v2/profiles/{profile}/export`, `POST /v2/profiles/import`
//! (`profile-transfer`; the export may carry an API key, loopback only),
//! `GET|POST /v2/profile-imports/cc-switch` (`profile-import-cc-switch`,
//! [`super::profile_import_cc_switch`]).
//! `{profile}` is a profile ID or exact name (percent-encoded). Responses are
//! camelCase and secret-free; `apiKey` is write-only.
//!
//! Profile looks (capability `profile-avatar`, `crate::profiles::avatar`):
//!
//! - `GET    /v2/profiles/{profile}/avatar` → the picture (ETag,
//!   `If-None-Match` → 304; 404 `avatar_not_found`)
//! - `PUT    /v2/profiles/{profile}/avatar` `{contentType, data}` (base64 of
//!   a square PNG / JPEG / WebP ≤ 48 KiB, 16..=512 px) → `{contractVersion,
//!   profileId, avatarEtag}`; 400 `invalid_avatar`, 413 `avatar_too_large`
//! - `DELETE /v2/profiles/{profile}/avatar` → the same with `avatarEtag: null`
//! - `GET|PUT|DELETE /v2/profiles/{profile}/intro-portrait`: the same for the
//!   intro portrait (≤ 2 MiB, any aspect, each side ≤ 1024 px; result field
//!   `introPortraitEtag`)
//! - items (list / detail / write results) carry `avatarEtag` and
//!   `introPortraitEtag` (null = none); `appearance` carries `avatarThemes`
//!   (theme ids or null = default) and `introGreeting` (string or null),
//!   both PATCHable (null clears; `avatarThemes` must be a non-empty list of
//!   unique known ids, `detail.avatarThemeOptions`).
//!
//! Images are keyed by the profile id (a rename keeps them) and removed with
//! the profile.
//!
//! Create / duplicate / delete / rename go through
//! `crate::profiles::lifecycle` (the CLI's profile layout).

use std::net::TcpStream;

use serde_json::{json, Map, Value};

use crate::config::global_settings::ConfigValueUpdate;
use crate::config::store::load_codez_config;
use crate::http::server::write_json_response;
use crate::http::server::SimpleHttpRequest;
use crate::profiles::appearance::AgentTurnQuestions;
use crate::profiles::avatar::{self as profile_avatar, Kind as ImageKind};
use crate::profiles::codex_profile::{CodexProfileConfigPatch, MODEL_REASONING_SUMMARY_VALUES, TOOL_SCHEMA_COMPAT_VALUES, WEB_SEARCH_VALUES};
use crate::profiles::lifecycle::{self, LifecycleError, NewProfile, NewProfileKind};
use crate::profiles::settings_api::{self, ApiKey, ProfileApiPatch, MAX_API_KEY_BYTES};
use crate::profiles::transfer;

pub const PRESETS_PATH: &str = "/v2/profile-presets";
pub const PROFILES_CHANGED_EVENT: &str = "cutex/profiles/changed";

/// A host-stream invalidation after durable style persistence. Host events
/// already have owner-only visibility; no style, identity or credential data
/// is copied into this event. Clients re-read the canonical Profile resource.
pub(crate) fn publish_avatar_style_change(profile_id: &str) {
    let result = (|| -> anyhow::Result<()> {
        let repository = super::repository::management_v2_repository()?;
        super::server::append_cutex_event(
            repository,
            super::services::HOST_EVENT_SESSION_ID,
            super::model::EventCorrelation::default(),
            PROFILES_CHANGED_EVENT,
            json!({"profileId":profile_id,"fields":["avatarStyle"],
                "updatedAt":chrono::Utc::now().to_rfc3339()}),
        )
    })();
    if let Err(error) = result {
        // Persistence succeeded. Do not report a failed save or roll it back
        // because the refresh hint could not be appended; GET stays canonical.
        eprintln!("profile avatar style saved, change notification unavailable: {error:#}");
    }
}

pub fn is_list_path(path: &str) -> bool {
    path == "/v2/profiles"
}

fn decode_profile_segment(encoded: &str) -> Option<String> {
    if encoded.is_empty() || encoded.contains('/') {
        return None;
    }
    let decoded = super::profile_quota::percent_decode(encoded)?;
    (!decoded.is_empty() && !decoded.contains('/') && !decoded.chars().any(char::is_control))
        .then_some(decoded)
}

/// `{profile}` for `/v2/profiles/{profile}` (exactly one segment).
pub fn parse_item_path(path: &str) -> Option<String> {
    decode_profile_segment(path.strip_prefix("/v2/profiles/")?)
}

/// `{profile}` for `/v2/profiles/{profile}/duplicate`.
pub fn parse_duplicate_path(path: &str) -> Option<String> {
    decode_profile_segment(path.strip_prefix("/v2/profiles/")?.strip_suffix("/duplicate")?)
}

/// `{profile}` for `/v2/profiles/{profile}/export` (`profile-transfer`).
pub fn parse_export_path(path: &str) -> Option<String> {
    decode_profile_segment(path.strip_prefix("/v2/profiles/")?.strip_suffix("/export")?)
}

/// `{profile}` and the image of `/v2/profiles/{profile}/avatar` and
/// `/v2/profiles/{profile}/intro-portrait` (`profile-avatar`).
pub fn parse_image_path(path: &str) -> Option<(String, ImageKind)> {
    let rest = path.strip_prefix("/v2/profiles/")?;
    if let Some(profile) = rest.strip_suffix("/avatar") {
        return decode_profile_segment(profile).map(|profile| (profile, ImageKind::Avatar));
    }
    let profile = rest.strip_suffix("/intro-portrait")?;
    decode_profile_segment(profile).map(|profile| (profile, ImageKind::IntroPortrait))
}

/// `POST /v2/profiles/import` (`profile-transfer`). The same path with
/// another method still addresses a profile named `import`.
pub const IMPORT_PATH: &str = "/v2/profiles/import";

pub fn is_profiles_path(path: &str) -> bool {
    is_list_path(path)
        || super::profile_variants_api::parse_path(path).is_some()
        || path == PRESETS_PATH
        || parse_item_path(path).is_some()
        || parse_duplicate_path(path).is_some()
        || parse_export_path(path).is_some()
        || parse_image_path(path).is_some()
        || super::profile_import_cc_switch::is_path(path)
}

/// `avatarEtag` / `introPortraitEtag` of a profile item (profile-avatar).
fn with_images(mut item: Value, profile_id: &str, images: &profile_avatar::State) -> Value {
    item["avatarEtag"] = json!(images.etag(ImageKind::Avatar, profile_id));
    item["introPortraitEtag"] = json!(images.etag(ImageKind::IntroPortrait, profile_id));
    item["appearanceRevisions"]["avatar"] = images.revision(ImageKind::Avatar, profile_id);
    item["appearanceRevisions"]["introPortrait"] = images.revision(ImageKind::IntroPortrait, profile_id);
    item
}

pub(crate) fn detail_item(account: &crate::profiles::model::StoredAccount) -> Value {
    let config = load_codez_config();
    let mut links = super::profile_link::owner_full().then(crate::profiles::account_link::LinkContext::new);
    let mut item = with_link(with_images(
        settings_api::profile_item(account, &config, true),
        &account.id,
        &profile_avatar::default_snapshot(),
    ), account, &config, true, &mut links);
    crate::profiles::runtime_variants::augment_item(&mut item, account, &crate::profiles::runtime_variants::Usage::load());
    item
}

fn with_link(mut item: Value, account: &crate::profiles::model::StoredAccount, config: &crate::profiles::model::CodezConfig, detail: bool, links:&mut Option<crate::profiles::account_link::LinkContext>) -> Value {
    if super::profile_link::owner_full() {
        if let Some(link) = links.as_mut().and_then(|context|context.profile_link(account, config)) {
            item["link"] = link;
        }
        item["accountLinkState"] = json!(match account.options.account_link.as_deref() {
            Some("none") => "disabled",
            Some(_) => "manual",
            None => "auto",
        });
        if detail { item["accountLink"] = json!(account.options.account_link); }
    }
    item
}

fn profile_body(account: &crate::profiles::model::StoredAccount) -> Value {
    json!({
        "contractVersion": 2,
        "profile": detail_item(account),
    })
}

/// An error answer not yet written (shared by `POST /v2/profiles` and the
/// profile imports, which report it per item).
#[derive(Debug)]
pub(crate) struct CreateFailure {
    pub status: u16,
    pub reason: &'static str,
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
    pub details: Value,
}

impl CreateFailure {
    fn invalid(message: String) -> Self {
        Self { status: 400, reason: "Bad Request", code: "invalid_request", message, retryable: false, details: json!({}) }
    }

    fn store(error: &anyhow::Error) -> Self {
        Self {
            status: 503,
            reason: "Service Unavailable",
            code: "profile_store_unavailable",
            message: "the profile store could not be read".into(),
            retryable: true,
            details: json!({ "diagnostic": format!("{error:#}") }),
        }
    }

    pub(super) fn write(self, stream: &mut TcpStream) -> anyhow::Result<()> {
        super::server::write_v2_error(stream, self.status, self.reason, self.code, &self.message, self.retryable, self.details)
    }
}

fn lifecycle_failure(profile: &str, failure: LifecycleError) -> CreateFailure {
    let message = failure.to_string();
    let (status, reason, code, retryable, details) = match failure {
        LifecycleError::Invalid(_) => (400, "Bad Request", "invalid_request", false, json!({})),
        LifecycleError::NotFound => {
            let available = crate::profiles::identity::current_profile_names();
            return CreateFailure {
                status: 404,
                reason: "Not Found",
                code: "profile_not_found",
                message: crate::profiles::identity::unknown_profile_message(profile, &available),
                retryable: false,
                details: json!({ "profile": profile, "availableProfiles": available }),
            };
        }
        LifecycleError::NameConflict(name) => {
            (409, "Conflict", "profile_name_conflict", false, json!({ "name": name }))
        }
        LifecycleError::InUse(sessions) => (
            409,
            "Conflict",
            "profile_in_use",
            false,
            json!({ "profile": profile, "sessions": sessions }),
        ),
        LifecycleError::IsDefault(name) => {
            (409, "Conflict", "profile_is_default", false, json!({ "profile": name }))
        }
        LifecycleError::Store(error) => (
            503,
            "Service Unavailable",
            "profile_store_unavailable",
            true,
            json!({ "diagnostic": format!("{error:#}") }),
        ),
    };
    CreateFailure { status, reason, code, message, retryable, details }
}

fn lifecycle_error(stream: &mut TcpStream, profile: &str, failure: LifecycleError) -> anyhow::Result<()> {
    lifecycle_failure(profile, failure).write(stream)
}

/// `POST /v2/profiles`: parse, create, attach the Claude account; the 201
/// body (`ProfileDetail` + created / kind / credentialsPresent).
pub(crate) fn create_from_body(body: &[u8]) -> Result<Value, CreateFailure> {
    let create = parse_create(body).map_err(CreateFailure::invalid)?;
    let claude_account = parse_create_claude_account(body).map_err(CreateFailure::invalid)?;
    let options_body = object_body(body).map_err(CreateFailure::invalid)?;
    let dsh = parse_dsh_options(options_body.get("options")).map_err(CreateFailure::invalid)?;
    if let Some(id) = &claude_account {
        match crate::runtime::claude_accounts::load() {
            Ok(registry) if registry.find(id).is_some() => {}
            Ok(_) => return Err(CreateFailure::invalid(format!("claudeAccount: no Claude account {id} on this host"))),
            Err(error) => return Err(CreateFailure::store(&error)),
        }
    }
    // r265: a DSH key / credential source is applied by the DSH patch below
    // (the key goes to the private key file, never into the store).
    let mut create = create;
    let dsh_key = if create.kind == NewProfileKind::Dsh { create.api_key.take() } else { None };
    let dsh_credential = optional(&options_body, "credential", credential_choice).map_err(CreateFailure::invalid)?;
    let dsh_key_present = dsh_key.is_some();
    let account = lifecycle::create_profile(&create).map_err(|failure| lifecycle_failure(&create.name, failure))?;
    let account = if dsh.is_some() || dsh_key.is_some() || dsh_credential.is_some() {
        let patch = ProfileApiPatch {
            dsh: dsh.map(ConfigValueUpdate::Set).unwrap_or_default(),
            api_key: dsh_key,
            credential: dsh_credential,
            ..Default::default()
        };
        match settings_api::update_profile(&account.id, &patch) {
            Ok((account, _)) => account,
            Err(error) => {
                let _ = lifecycle::delete_profile(&account.id);
                return Err(CreateFailure::invalid(format!("{error:#}")));
            }
        }
    } else { account };
    // r266: the Kimi account of a Kimi-kind profile.
    let kimi_account = object_body(body).ok().and_then(|body| optional(&body, "kimiAccount", kimi_account_id).ok().flatten());
    let account = match &kimi_account {
        Some(id) if create.kind == NewProfileKind::Kimi => {
            let patch = ProfileApiPatch { kimi_account: ConfigValueUpdate::Set(id.clone()), ..Default::default() };
            match settings_api::update_profile(&account.id, &patch) {
                Ok((account, _)) => account,
                Err(error) => {
                    let _ = lifecycle::delete_profile(&account.id);
                    return Err(CreateFailure::invalid(format!("{error:#}")));
                }
            }
        }
        _ => account,
    };
    // profile-relay-runtimes: the per-interface addresses, checked against
    // the created profile (which interfaces it stores, relay acceptance).
    let interfaces = interface_base_urls(&options_body).map_err(CreateFailure::invalid)?;
    let account = if interfaces.is_empty() { account } else {
        let patch = ProfileApiPatch { interface_base_urls: interfaces, ..Default::default() };
        match settings_api::update_profile(&account.id, &patch) {
            Ok((account, _)) => account,
            Err(error) => {
                let _ = lifecycle::delete_profile(&account.id);
                return Err(CreateFailure::invalid(format!("{error:#}")));
            }
        }
    };
    let account = match &claude_account {
        None => account,
        Some(id) => {
            let patch = settings_api::ProfileApiPatch {
                claude_account: ConfigValueUpdate::Set(id.clone()),
                ..Default::default()
            };
            match settings_api::update_profile(&account.id, &patch) {
                Ok((account, _)) => account,
                Err(error) => {
                    let _ = lifecycle::delete_profile(&account.id);
                    return Err(CreateFailure::invalid(format!("{error:#}")));
                }
            }
        }
    };
    let mut body = profile_body(&account);
    body["created"] = json!(true);
    body["kind"] = json!(create.kind.as_str());
    body["credentialsPresent"] = json!(create.api_key.is_some() || dsh_key_present);
    Ok(body)
}

fn bad_request(stream: &mut TcpStream, message: &str) -> anyhow::Result<()> {
    super::server::write_v2_error(
        stream,
        400,
        "Bad Request",
        "invalid_request",
        message,
        false,
        json!({}),
    )
}

fn store_unavailable(stream: &mut TcpStream, error: &anyhow::Error) -> anyhow::Result<()> {
    super::server::write_v2_error(
        stream,
        503,
        "Service Unavailable",
        "profile_store_unavailable",
        "the profile store could not be read",
        true,
        json!({ "diagnostic": format!("{error:#}") }),
    )
}

pub fn list_resource() -> anyhow::Result<Value> {
    let store = settings_api::load_store()?;
    let config = load_codez_config();
    let images = profile_avatar::default_snapshot();
    let mut links = super::profile_link::owner_full().then(crate::profiles::account_link::LinkContext::new);
    let usage = crate::profiles::runtime_variants::Usage::load();
    Ok(json!({
        "contractVersion": 2,
        "defaultProfile": config.default_profile,
        "profiles": store
            .accounts
            .iter()
            .map(|account| {
                let mut item = with_link(with_images(settings_api::profile_item(account, &config, false), &account.id, &images), account, &config, false, &mut links);
                crate::profiles::runtime_variants::augment_item(&mut item, account, &usage);
                item
            })
            .collect::<Vec<_>>(),
    }))
}

pub(super) fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest, path: &str) -> anyhow::Result<()> {
    if super::profile_import_cc_switch::is_path(path) {
        return super::profile_import_cc_switch::handle(stream, request);
    }
    if let Some(route) = super::profile_variants_api::parse_path(path) {
        return super::profile_variants_api::handle(stream, request, route);
    }
    if path == PRESETS_PATH {
        if request.method != "GET" {
            return method_not_allowed(stream, "use GET for /v2/profile-presets");
        }
        return write_json_response(
            stream,
            200,
            "OK",
            &json!({ "contractVersion": 2, "presets": crate::profiles::providers::presets_json() }),
        );
    }
    if is_list_path(path) {
        return match request.method.as_str() {
            "GET" => match list_resource() {
                Ok(value) => write_json_response(stream, 200, "OK", &value),
                Err(error) => store_unavailable(stream, &error),
            },
            "POST" => match create_from_body(&request.body) {
                Ok(body) => write_json_response(stream, 201, "Created", &body),
                Err(failure) => failure.write(stream),
            },
            _ => method_not_allowed(stream, "use GET or POST for /v2/profiles"),
        };
    }
    if let Some(profile) = parse_export_path(path) {
        if request.method != "POST" {
            return method_not_allowed(stream, "use POST for /v2/profiles/{profile}/export");
        }
        return handle_export(stream, &request.body, &profile);
    }
    if path == IMPORT_PATH && request.method == "POST" {
        return handle_import(stream, &request.body);
    }
    if let Some((profile, kind)) = parse_image_path(path) {
        return handle_image(stream, request, &profile, kind);
    }
    if let Some(profile) = parse_duplicate_path(path) {
        if request.method != "POST" {
            return method_not_allowed(stream, "use POST for /v2/profiles/{profile}/duplicate");
        }
        let name = match parse_name_body(&request.body) {
            Ok(name) => name,
            Err(message) => return bad_request(stream, &message),
        };
        return match lifecycle::duplicate_profile(&profile, &name) {
            Ok(account) => {
                let mut body = profile_body(&account);
                body["duplicatedFrom"] = json!(profile);
                // Credentials (auth.json, API key, Claude login) are never copied.
                body["credentialsCopied"] = json!(false);
                write_json_response(stream, 201, "Created", &body)
            }
            Err(failure) => lifecycle_error(stream, &profile, failure),
        };
    }
    let profile = parse_item_path(path).expect("matched profile item path");
    match request.method.as_str() {
        "GET" => {
            let store = match settings_api::load_store() {
                Ok(store) => store,
                Err(error) => return store_unavailable(stream, &error),
            };
            let Some(account) = settings_api::find(&store, &profile) else {
                return not_found(stream, &profile);
            };
            let item = detail_item(account);
            write_json_response(stream, 200, "OK", &json!({ "contractVersion": 2, "profile": item }))
        }
        "PATCH" => {
            let parsed = parse_name_field(&request.body)
                .and_then(|name| Ok((name, parse_patch(&request.body)?)));
            let (rename, patch) = match parsed {
                Ok(parsed) => parsed,
                Err(message) => return bad_request(stream, &message),
            };
            if patch.proxy.is_some() && !super::settings_api::proxy_editable() {
                return super::settings_api::proxy_owner_required(stream);
            }
            // Serialize precondition, rename and settings writes with owner saves.
            let _profile_guard = lifecycle::lock();
            let account_id = match settings_api::load_store() {
                Ok(store) => match settings_api::find(&store, &profile) {
                    Some(account) => {
                        if settings_api::avatar_style_import_conflicts(account, &patch) {
                            // This expected conflict must not write even a diagnostic log.
                            return write_json_response(stream, 409, "Conflict", &json!({
                                "contractVersion": 2,
                                "error": {
                                    "code": "avatar_style_already_set",
                                    "message": "The profile avatar style has already been chosen",
                                    "retryable": false,
                                    "details": {
                                        "avatarStyle": account.options.avatar_style,
                                        "appearanceRevision": settings_api::appearance_revisions(account)["avatarStyle"],
                                    },
                                },
                            }));
                        }
                        account.id.clone()
                    },
                    None => return not_found(stream, &profile),
                },
                Err(error) => return store_unavailable(stream, &error),
            };
            let renamed = match &rename {
                Some(name) => match lifecycle::rename_profile_locked(&account_id, name) {
                    Ok(outcome) => outcome.changed.then(|| (outcome.old_name, outcome.account.name)),
                    Err(failure) => return lifecycle_error(stream, &profile, failure),
                },
                None => None,
            };
            match settings_api::update_profile_locked(&account_id, &patch) {
                Ok((account, changed)) => {
                    let item = detail_item(&account);
                    write_json_response(
                        stream,
                        200,
                        "OK",
                        &json!({
                            "contractVersion": 2,
                            "changed": changed || renamed.is_some(),
                            "renamed": renamed.as_ref().map(|(from, to)| json!({ "from": from, "to": to })),
                            "appliesTo": "newRuntimes",
                            "profile": item,
                        }),
                    )
                }
                Err(error) => {
                    // Keep PATCH all-or-nothing: undo the rename.
                    if let Some((from, _)) = &renamed {
                        if let Err(revert) = lifecycle::rename_profile_locked(&account_id, from) {
                            eprintln!("profile PATCH: could not revert rename of {account_id} to {from}: {revert}");
                        }
                    }
                    bad_request(stream, &format!("{error:#}"))
                }
            }
        }
        "DELETE" => {
            let archived = match archived_references(&request.path) {
                Ok(archived) => archived,
                Err(message) => return bad_request(stream, &message),
            };
            match lifecycle::delete_profile_with(&profile, archived) {
            Ok(outcome) => {
                write_json_response(
                stream,
                200,
                "OK",
                &json!({
                    "contractVersion": 2,
                    "deleted": { "id": outcome.removed.id, "name": outcome.removed.name },
                    "backupPath": outcome.backup_path.map(|path| path.display().to_string()),
                    "backupError": outcome.backup_error,
                    "clearedReferences": {
                        "globalConfig": outcome.references.global_config_changed,
                        "quickRunState": outcome.references.quick_state_changed,
                    },
                    "detachedSessions": outcome.detached_sessions,
                }),
            )
            }
            Err(failure) => lifecycle_error(stream, &profile, failure),
            }
        }
        _ => method_not_allowed(stream, "use GET, PATCH or DELETE for /v2/profiles/{profile}"),
    }
}

/// `GET|PUT|DELETE /v2/profiles/{p}/avatar|intro-portrait` (profile-avatar).
fn handle_image(stream: &mut TcpStream, request: &SimpleHttpRequest, profile: &str, kind: ImageKind) -> anyhow::Result<()> {
    let store = match settings_api::load_store() {
        Ok(store) => store,
        Err(error) => return store_unavailable(stream, &error),
    };
    let Some(profile_id) = settings_api::find(&store, profile).map(|account| account.id.clone()) else {
        return not_found(stream, profile);
    };
    let images = match profile_avatar::ProfileAvatarStore::open_default() {
        Ok(images) => images,
        Err(error) => return store_unavailable(stream, &error),
    };
    let field = match kind {
        ImageKind::Avatar => "avatarEtag",
        ImageKind::IntroPortrait => "introPortraitEtag",
    };
    let upload = match request.method.as_str() {
        "GET" => return write_image(stream, request, &images, kind, &profile_id),
        "PUT" => {
            let value: Value = match serde_json::from_slice(&request.body) {
                Ok(value) => value,
                Err(_) => return bad_request(stream, "JSON object body {contentType, data} required"),
            };
            match crate::management::user_profiles::parse_upload_limited(&value, kind.rules().max_bytes) {
                Ok(upload) => Some(upload),
                Err(failure) => return image_error(stream, failure),
            }
        }
        "DELETE" => None,
        _ => return method_not_allowed(stream, "use GET, PUT or DELETE for this profile image"),
    };
    match images.set(kind, &profile_id, upload.as_ref(), &crate::management::users::current_actor()) {
        Ok(stored) => write_json_response(
            stream,
            200,
            "OK",
            &json!({
                "contractVersion": 2,
                "profileId": profile_id,
                field: stored.map(|avatar| avatar.etag),
            }),
        ),
        Err(failure) => image_error(stream, failure),
    }
}

fn image_error(stream: &mut TcpStream, failure: crate::management::users::UserError) -> anyhow::Result<()> {
    let reason = match failure.status {
        400 => "Bad Request",
        413 => "Payload Too Large",
        _ => "Service Unavailable",
    };
    super::server::write_v2_error(
        stream,
        failure.status,
        reason,
        failure.code,
        &failure.message,
        failure.status >= 500,
        failure.details,
    )
}

fn write_image(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    images: &profile_avatar::ProfileAvatarStore,
    kind: ImageKind,
    profile_id: &str,
) -> anyhow::Result<()> {
    let Some((image, bytes)) = images.image(kind, profile_id) else {
        return super::server::write_v2_error(
            stream,
            404,
            "Not Found",
            "avatar_not_found",
            &format!("this profile has no {}", kind.label()),
            false,
            json!({ "profileId": profile_id }),
        );
    };
    let etag = format!("\"{}\"", image.etag);
    let matches = request
        .headers
        .get("if-none-match")
        .is_some_and(|value| value.split(',').any(|tag| tag.trim().trim_start_matches("W/") == etag));
    let previous = crate::http::server::extra_response_headers();
    crate::http::server::set_extra_response_headers(format!(
        "{previous}ETag: {etag}\r\nCache-Control: private, no-cache\r\nX-Content-Type-Options: nosniff\r\n"
    ));
    let result = if matches {
        crate::http::server::write_http_response(stream, 304, "Not Modified", &image.content_type, &[])
    } else {
        crate::http::server::write_http_response(stream, 200, "OK", &image.content_type, &bytes)
    };
    crate::http::server::set_extra_response_headers(previous);
    result
}

/// `DELETE /v2/profiles/{p}?archived=detach|refuse` (default refuse; see
/// `lifecycle::ArchivedReferences`).
fn archived_references(path: &str) -> Result<lifecycle::ArchivedReferences, String> {
    let Some((_, query)) = path.split_once('?') else {
        return Ok(lifecycle::ArchivedReferences::Refuse);
    };
    let mut archived = lifecycle::ArchivedReferences::Refuse;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        match pair.split_once('=').unwrap_or((pair, "")) {
            ("archived", "detach") => archived = lifecycle::ArchivedReferences::Detach,
            ("archived", "refuse") => archived = lifecycle::ArchivedReferences::Refuse,
            ("archived", _) => return Err("archived must be detach or refuse".into()),
            (key, _) => return Err(format!("unknown query parameter {key}")),
        }
    }
    Ok(archived)
}

fn transfer_error(stream: &mut TcpStream, profile: &str, failure: transfer::TransferError) -> anyhow::Result<()> {
    use transfer::TransferError;
    // Messages are fixed texts or file diagnostics: never bundle values.
    let message = failure.to_string();
    let (status, reason, code, retryable, details) = match failure {
        TransferError::NotFound => return not_found(stream, profile),
        TransferError::Invalid(_) => (400, "Bad Request", "invalid_request", false, json!({})),
        TransferError::Unsupported(_) => (
            400,
            "Bad Request",
            "unsupported_bundle",
            false,
            json!({ "schemaVersion": transfer::BUNDLE_SCHEMA_VERSION }),
        ),
        TransferError::NameConflict(name) => {
            (409, "Conflict", "profile_name_conflict", false, json!({ "name": name }))
        }
        TransferError::Store(error) => (
            503,
            "Service Unavailable",
            "profile_store_unavailable",
            true,
            json!({ "diagnostic": format!("{error:#}") }),
        ),
    };
    super::server::write_v2_error(stream, status, reason, code, &message, retryable, details)
}

/// `POST /v2/profiles/{p}/export {includeCredentials?: bool}`. Credentials
/// only over a loopback connection (an SSH tunnel arrives on loopback; a
/// tailnet / LAN listener is refused with 403 `credentials_loopback_only`).
/// Audited without values. POST only: the bundle must not be cached.
fn handle_export(stream: &mut TcpStream, body: &[u8], profile: &str) -> anyhow::Result<()> {
    let include_credentials = match parse_export_body(body) {
        Ok(include) => include,
        Err(message) => return bad_request(stream, &message),
    };
    let loopback = crate::management::listeners::current_request_is_loopback();
    if include_credentials && !loopback {
        transfer::audit(
            "profileExportRefused",
            json!({
                "profile": profile,
                "includeCredentials": true,
                "reason": "credentials_loopback_only",
                "actor": crate::management::users::current_actor(),
            }),
        );
        return super::server::write_v2_error(
            stream,
            403,
            "Forbidden",
            "credentials_loopback_only",
            "credentials can only be exported over a loopback connection (this machine or an SSH tunnel); export without credentials or connect through SSH",
            false,
            json!({ "profile": profile }),
        );
    }
    match transfer::export_profile(profile, include_credentials) {
        Ok(export) => {
            transfer::audit(
                "profileExport",
                json!({
                    "profileId": export.profile_id,
                    "profileName": export.profile_name,
                    "includeCredentials": include_credentials,
                    "credentialsIncluded": export.credentials_included,
                    "credentialKind": export.credential_kind.as_str(),
                    "loopback": loopback,
                    "actor": crate::management::users::current_actor(),
                }),
            );
            write_json_response(
                stream,
                200,
                "OK",
                &json!({
                    "contractVersion": 2,
                    "credentialsIncluded": export.credentials_included,
                    "credentialKind": export.credential_kind.as_str(),
                    "bundle": export.bundle,
                }),
            )
        }
        Err(failure) => transfer_error(stream, profile, failure),
    }
}

/// `POST /v2/profiles/import {bundle, name, onConflict?: "fail"}` (201).
fn handle_import(stream: &mut TcpStream, body: &[u8]) -> anyhow::Result<()> {
    let (bundle, name) = match parse_import_body(body) {
        Ok(parsed) => parsed,
        Err(message) => return bad_request(stream, &message),
    };
    match transfer::import_profile(&bundle, &name) {
        Ok(outcome) => {
            transfer::audit(
                "profileImport",
                json!({
                    "profileId": outcome.account.id,
                    "profileName": outcome.account.name,
                    "sourceProfileName": outcome.source_name,
                    "credentialsCopied": outcome.credentials_copied,
                    "credentialKind": outcome.credential_kind.as_str(),
                    "loopback": crate::management::listeners::current_request_is_loopback(),
                    "actor": crate::management::users::current_actor(),
                }),
            );
            let mut response = profile_body(&outcome.account);
            response["imported"] = json!(true);
            response["credentialKind"] = json!(outcome.credential_kind.as_str());
            response["credentialsCopied"] = json!(outcome.credentials_copied);
            response["warnings"] = json!(outcome.warnings);
            write_json_response(stream, 201, "Created", &response)
        }
        Err(failure) => transfer_error(stream, &name, failure),
    }
}

pub(super) fn parse_export_body(body: &[u8]) -> Result<bool, String> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(false);
    }
    let body = object_body(body)?;
    if let Some(unknown) = body.keys().find(|key| key.as_str() != "includeCredentials") {
        return Err(format!("unsupported field: {unknown}"));
    }
    Ok(optional_bool(&body, "includeCredentials")?.unwrap_or(false))
}

pub(super) fn parse_import_body(body: &[u8]) -> Result<(Value, String), String> {
    // No serde text: a malformed body may hold an API key.
    let value: Value = serde_json::from_slice(body).map_err(|_| "JSON object body required".to_string())?;
    let Value::Object(mut body) = value else {
        return Err("JSON object body required".into());
    };
    if let Some(unknown) = body.keys().find(|key| !["bundle", "name", "onConflict"].contains(&key.as_str())) {
        return Err(format!("unsupported field: {unknown}"));
    }
    match body.get("onConflict") {
        None | Some(Value::Null) => {}
        Some(Value::String(mode)) if mode == "fail" => {}
        Some(_) => return Err("onConflict: only \"fail\" is supported (imports never overwrite)".into()),
    }
    let name = name_value(&body)?.ok_or_else(|| "name: required string".to_string())?;
    let bundle = match body.remove("bundle") {
        Some(bundle @ Value::Object(_)) => bundle,
        _ => return Err("bundle: required object (from POST /v2/profiles/{profile}/export)".into()),
    };
    Ok((bundle, name))
}

const CREATE_FIELDS: &[&str] =&["name", "kind", "preset", "baseUrl", "model", "apiKey", "claudeAccount", "options", "credential", "kimiAccount", "interfaceBaseUrls"];

fn object_body(body: &[u8]) -> Result<Map<String, Value>, String> {
    let value: Value =
        serde_json::from_slice(body).map_err(|error| format!("JSON object body required: {error}"))?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err("JSON object body required".into()),
    }
}

fn optional<T>(
    body: &Map<String, Value>,
    key: &str,
    parse: impl FnOnce(&Value) -> Result<T, String>,
) -> Result<Option<T>, String> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => parse(value).map(Some).map_err(|message| format!("{key}: {message}")),
    }
}

fn api_key_value(value: &Value) -> Result<ApiKey, String> {
    let key = value.as_str().ok_or_else(|| "must be a string".to_string())?.trim();
    if key.is_empty()
        || key.len() > MAX_API_KEY_BYTES
        || key.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(format!("must be 1-{MAX_API_KEY_BYTES} non-whitespace characters"));
    }
    Ok(ApiKey(key.to_string()))
}

/// `POST /v2/profiles` body. `kind` defaults to `apiKey` (with `preset`
/// defaulting to `custom`), so the legacy `{name}` body creates an empty
/// custom API-key profile to fill in afterwards.
pub(crate) fn parse_create(body: &[u8]) -> Result<NewProfile, String> {
    let body = object_body(body)?;
    if let Some(unknown) = body.keys().find(|key| !CREATE_FIELDS.contains(&key.as_str())) {
        return Err(format!("unsupported field: {unknown}"));
    }
    let name = match body.get("name") {
        Some(Value::String(name)) => {
            lifecycle::validate_profile_name(name).map_err(|message| format!("name: {message}"))?
        }
        _ => return Err("name: required string".into()),
    };
    let kind = optional(&body, "kind", |value| {
        value
            .as_str()
            .and_then(NewProfileKind::parse)
            .ok_or_else(|| "must be one of chatgpt, claude, dsh, apiKey, kimi".to_string())
    })?
    .unwrap_or(NewProfileKind::ApiKey);
    let preset = optional(&body, "preset", |value| {
        let id = value.as_str().ok_or("must be a string")?;
        crate::profiles::providers::find_preset(id).ok_or_else(|| {
            format!(
                "unknown preset (known: {})",
                crate::profiles::providers::API_KEY_PRESETS
                    .iter()
                    .map(|preset| preset.id)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
    })?;
    let base_url = optional(&body, "baseUrl", http_url)?;
    let model = optional(&body, "model", visible_string(200))?;
    let api_key = optional(&body, "apiKey", api_key_value)?;
    if kind != NewProfileKind::ApiKey && (preset.is_some() || base_url.is_some() || (api_key.is_some() && kind != NewProfileKind::Dsh)) {
        return Err(format!(
            "preset, baseUrl and apiKey are only valid for kind \"apiKey\" (apiKey also for \"dsh\"; got \"{}\")",
            kind.as_str()
        ));
    }
    let credential = optional(&body, "credential", credential_choice)?;
    if kind != NewProfileKind::Dsh && credential.is_some() {
        return Err("credential applies to DSH profiles only".into());
    }
    if api_key.is_some() && matches!(credential, Some(crate::profiles::runtime_variants::CredentialChoice::Env(_))) {
        return Err("apiKey cannot be stored with credential source env".into());
    }
    if kind != NewProfileKind::Kimi && body.get("kimiAccount").is_some_and(|value| !value.is_null()) {
        return Err(format!("kimiAccount is only valid for kind \"kimi\" (got \"{}\")", kind.as_str()));
    }
    if kind == NewProfileKind::Kimi {
        optional(&body, "kimiAccount", kimi_account_id)?;
        if let Some(model) = &model {
            crate::profiles::kimi::validate_model(model).map_err(|error| format!("model: {error}"))?;
        }
    }
    if kind != NewProfileKind::Claude && body.get("claudeAccount").is_some_and(|value| !value.is_null()) {
        return Err(format!("claudeAccount is only valid for kind \"claude\" (got \"{}\")", kind.as_str()));
    }
    let dsh = parse_dsh_options(body.get("options"))?;
    if kind != NewProfileKind::Dsh && dsh.is_some() {
        return Err("options.dsh applies to DSH profiles only".into());
    }
    if !interface_base_urls(&body)?.is_empty() && !matches!(kind, NewProfileKind::ApiKey | NewProfileKind::Dsh) {
        return Err(format!("interfaceBaseUrls is only valid for kind \"apiKey\" or \"dsh\" (got \"{}\")", kind.as_str()));
    }
    Ok(NewProfile { name, kind, preset, base_url, model, api_key })
}

fn parse_dsh_options(value: Option<&Value>) -> Result<Option<crate::profiles::dsh::DshOptions>, String> {
    let Some(value) = value else { return Ok(None); };
    let options = value.as_object().ok_or("options must be an object")?;
    if options.keys().any(|key| key != "dsh") { return Err("unsupported options field".into()); }
    options.get("dsh").map(|value| {
        let options: crate::profiles::dsh::DshOptions = serde_json::from_value(value.clone())
            .map_err(|error| format!("options.dsh: {error}"))?;
        options.validate().map_err(|error| format!("options.dsh: {error}"))?;
        Ok(options)
    }).transpose()
}

/// `claudeAccount` of a `POST /v2/profiles` body (kind `claude`; checked by
/// [`parse_create`]); `None` / `default` = the shared default account.
pub(super) fn parse_create_claude_account(body: &[u8]) -> Result<Option<String>, String> {
    let body = object_body(body)?;
    Ok(optional(&body, "claudeAccount", claude_account_id)?
        .filter(|id| id != crate::runtime::claude_accounts::DEFAULT_ACCOUNT_ID))
}

fn name_value(body: &Map<String, Value>) -> Result<Option<String>, String> {
    match body.get("name") {
        None => Ok(None),
        Some(Value::String(name)) => lifecycle::validate_profile_name(name)
            .map(Some)
            .map_err(|message| format!("name: {message}")),
        Some(_) => Err("name: must be a string".into()),
    }
}

/// `{name}` body (duplicate).
pub(super) fn parse_name_body(body: &[u8]) -> Result<String, String> {
    let body = object_body(body)?;
    if let Some(unknown) = body.keys().find(|key| key.as_str() != "name") {
        return Err(format!("unsupported field: {unknown}"));
    }
    name_value(&body)?.ok_or_else(|| "name: required string".into())
}

/// Optional `name` (rename) in a PATCH body.
pub(super) fn parse_name_field(body: &[u8]) -> Result<Option<String>, String> {
    name_value(&object_body(body)?)
}

/// 404 `profile_not_found`; the message and `details.availableProfiles` list
/// the current names (an old name no longer resolves after a rename).
pub(super) fn not_found(stream: &mut TcpStream, profile: &str) -> anyhow::Result<()> {
    let available = crate::profiles::identity::current_profile_names();
    super::server::write_v2_error(
        stream,
        404,
        "Not Found",
        "profile_not_found",
        &crate::profiles::identity::unknown_profile_message(profile, &available),
        false,
        json!({ "profile": profile, "availableProfiles": available }),
    )
}

fn method_not_allowed(stream: &mut TcpStream, message: &str) -> anyhow::Result<()> {
    super::server::write_v2_error(
        stream,
        405,
        "Method Not Allowed",
        "method_not_allowed",
        message,
        false,
        json!({}),
    )
}

const PATCH_FIELDS: &[&str] = &[
    "options",
    // Rename (handled by `parse_name_field` + `lifecycle::rename_profile`).
    "name",
    "model",
    "modelProvider",
    "forcedLoginMethod",
    "modelReasoningEffort",
    "modelReasoningSummary",
    "updatePlanEnabled",
    "webSearch",
    "toolSchemaCompat",
    "imageHistoryTrim",
    "modelContextWindow",
    "modelAutoCompactTokenLimit",
    "providerName",
    "providerBaseUrl",
    "providerEnvKey",
    "providerWireApi",
    "requestMaxRetries",
    "streamMaxRetries",
    "streamIdleTimeoutMs",
    "websocketConnectTimeoutMs",
    "requiresOpenaiAuth",
    "supportsWebsockets",
    "providerInstructionsField",
    "color",
    "providerCategory",
    "avatarThemes",
    "avatarStyle",
    "avatarStyleImportIfUnset",
    "introGreeting",
    "accountLink",
    "allowSessionSwitch",
    "nativeSubagents",
    "delegationWait",
    "browserTools",
    "proxy",
    "searchTool",
    "privateChatgptAuthFile",
    "autoPick",
    "agentUseRequiresApproval",
    "agentTurnQuestions",
    "claudeFileCheckpointing",
    "claudeCompaction",
    "claudeCompactionModels",
    "claudeSettingSources",
    "claudeAiConnectors",
    "claudeAllowWrite",
    "claudeAllowExec",
    "planLabel",
    "claudeAccount",
    "modelDisplayNames",
    "interfaceBaseUrls",
    "apiKey",
    // r265 profile-runtime-variants.
    "credential",
    "defaultVariant",
    // r266 kimi-runtime-profiles.
    "kimiAccount",
];

/// Tri-state read: absent = Unchanged, `null` = Clear, value = Set.
/// `claudeCompaction: {window: 100000..1000000, percent: 1..100}`.
fn claude_compaction(value: &Value) -> Result<crate::profiles::claude_compaction::ClaudeCompaction, String> {
    let object = value.as_object().ok_or("must be {window, percent} or null")?;
    if let Some(unknown) = object.keys().find(|key| !matches!(key.as_str(), "window" | "percent")) {
        return Err(format!("unknown field {unknown}"));
    }
    let number = |key: &str| object.get(key).and_then(Value::as_u64).ok_or(format!("{key} must be a whole number"));
    let window = number("window")?;
    let percent = number("percent")?;
    let parsed = crate::profiles::claude_compaction::ClaudeCompaction {
        auto_compact_window: u32::try_from(window).map_err(|_| "window must be 100000..1000000".to_string())?,
        auto_compact_percent: u8::try_from(percent).map_err(|_| "percent must be 1..100".to_string())?,
    };
    if !crate::profiles::claude_compaction::WINDOW_RANGE.contains(&parsed.auto_compact_window) {
        return Err("window must be 100000..1000000".into());
    }
    if !crate::profiles::claude_compaction::PERCENT_RANGE.contains(&parsed.auto_compact_percent) {
        return Err("percent must be 1..100".into());
    }
    Ok(parsed)
}

/// `claudeCompactionModels: {"<model id>": {window, percent}}` (`{}` = none).
fn claude_compaction_models(value: &Value) -> Result<crate::profiles::claude_compaction::ModelTable, String> {
    use crate::profiles::claude_compaction::{valid_model_id, MAX_MODELS};
    let object = value.as_object().ok_or("must be {\"<model id>\": {window, percent}} or null")?;
    if object.len() > MAX_MODELS {
        return Err(format!("at most {MAX_MODELS} models"));
    }
    object.iter().map(|(model, entry)| {
        if !valid_model_id(model) {
            return Err(format!("invalid model ID {model:?}"));
        }
        claude_compaction(entry).map(|parsed| (model.clone(), parsed)).map_err(|message| format!("{model}: {message}"))
    }).collect()
}

/// `imageHistoryTrim: {maxInline: (keep+1)..500, keep: 1..50}`.
fn image_history_trim(value: &Value) -> Result<crate::profiles::codex_profile::ImageHistoryTrim, String> {
    let object = value.as_object().ok_or("must be {maxInline, keep} or null")?;
    if let Some(unknown) = object.keys().find(|key| !matches!(key.as_str(), "maxInline" | "keep")) {
        return Err(format!("unknown field {unknown}"));
    }
    let number = |key: &str| object.get(key).and_then(Value::as_u64).ok_or(format!("{key} must be a whole number"));
    let parsed = crate::profiles::codex_profile::ImageHistoryTrim { max_inline: number("maxInline")?, keep: number("keep")? };
    parsed.validate()?;
    Ok(parsed)
}

pub(super) fn tri_state<T>(
    body: &Map<String, Value>,
    key: &str,
    parse: impl FnOnce(&Value) -> Result<T, String>,
) -> Result<ConfigValueUpdate<T>, String> {
    match body.get(key) {
        None => Ok(ConfigValueUpdate::Unchanged),
        Some(Value::Null) => Ok(ConfigValueUpdate::Clear),
        Some(value) => parse(value)
            .map(ConfigValueUpdate::Set)
            .map_err(|message| format!("{key}: {message}")),
    }
}

pub(super) fn visible_string(max: usize) -> impl Fn(&Value) -> Result<String, String> {
    move |value| {
        let text = value.as_str().ok_or("must be a string or null")?.trim();
        if text.is_empty() || text.len() > max || text.chars().any(char::is_control) {
            return Err(format!("must be 1-{max} visible characters"));
        }
        Ok(text.to_string())
    }
}

fn token(max: usize, extra: &'static str) -> impl Fn(&Value) -> Result<String, String> {
    move |value| {
        let text = value.as_str().ok_or("must be a string or null")?.trim();
        let valid = !text.is_empty()
            && text.len() <= max
            && text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || extra.contains(c));
        valid
            .then(|| text.to_string())
            .ok_or_else(|| format!("must be 1-{max} characters of [A-Za-z0-9{extra}]"))
    }
}

fn one_of(allowed: &'static [&'static str]) -> impl Fn(&Value) -> Result<String, String> {
    move |value| {
        let text = value.as_str().ok_or("must be a string or null")?.trim();
        allowed
            .contains(&text)
            .then(|| text.to_string())
            .ok_or_else(|| format!("must be one of {}", allowed.join(", ")))
    }
}

fn env_name(value: &Value) -> Result<String, String> {
    let text = value.as_str().ok_or("must be a string or null")?.trim();
    let valid = !text.is_empty()
        && text.len() <= 128
        && text.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && !text.starts_with(|c: char| c.is_ascii_digit());
    valid
        .then(|| text.to_string())
        .ok_or_else(|| "must be an environment variable name ([A-Z_][A-Z0-9_]*)".to_string())
}

fn bounded_count(max: u64) -> impl Fn(&Value) -> Result<u64, String> {
    move |value| {
        value
            .as_u64()
            .filter(|count| *count <= max)
            .ok_or_else(|| format!("must be an integer 0-{max} or null"))
    }
}

/// Absent = unchanged; only `true` / `false` are accepted.
fn optional_bool(body: &Map<String, Value>, key: &str) -> Result<Option<bool>, String> {
    match body.get(key) {
        None => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("{key}: must be a boolean")),
    }
}

fn agent_turn_questions(value: &Value) -> Result<AgentTurnQuestions, String> {
    value
        .as_str()
        .and_then(AgentTurnQuestions::parse)
        .ok_or_else(|| "must be decide, wait or null".to_string())
}

/// `default` or an added account id (existence is checked on apply).
fn kimi_account_id(value: &Value) -> Result<String, String> {
    let text = value.as_str().ok_or("must be a Kimi account id (ka-…) or null")?.trim();
    crate::profiles::kimi_accounts::validate_id(text)
        .map(|()| text.to_string())
        .map_err(|_| "must be a Kimi account id (ka-…) or null".to_string())
}

fn claude_account_id(value: &Value) -> Result<String, String> {
    let text = value.as_str().ok_or("must be a Claude account id, \"default\" or null")?.trim();
    if text == crate::runtime::claude_accounts::DEFAULT_ACCOUNT_ID {
        return Ok(text.to_string());
    }
    crate::runtime::claude_accounts::validate_id(text)
        .map(|()| text.to_string())
        .map_err(|_| "must be a Claude account id (ca-…), \"default\" or null".to_string())
}

/// `modelDisplayNames`: `{modelId: name | null}`, merged into the stored map
/// (`null` or "" removes that entry's name). Absent = unchanged.
fn model_display_names(
    body: &Map<String, Value>,
) -> Result<std::collections::BTreeMap<String, Option<String>>, String> {
    let Some(value) = body.get("modelDisplayNames") else {
        return Ok(Default::default());
    };
    let object = value
        .as_object()
        .ok_or("modelDisplayNames: must be an object {modelId: name | null}")?;
    if object.len() > crate::profiles::settings_api::MAX_MODEL_DISPLAY_NAMES {
        return Err("modelDisplayNames: too many entries".into());
    }
    object
        .iter()
        .map(|(id, name)| {
            crate::profiles::catalog_api::validate_model_id(id)
                .map_err(|error| format!("modelDisplayNames: {error}"))?;
            let name = match name {
                Value::Null => None,
                Value::String(text) => crate::profiles::settings_api::normalize_model_display_name(text)
                    .map_err(|message| format!("modelDisplayNames[{id}]: {message}"))?,
                _ => return Err(format!("modelDisplayNames[{id}]: must be a string or null")),
            };
            Ok((id.clone(), name))
        })
        .collect()
}

/// profile-relay-runtimes: `interfaceBaseUrls {kind: url | null}`; which
/// kinds a profile stores is checked when the patch is applied.
fn interface_base_urls(
    body: &Map<String, Value>,
) -> Result<std::collections::BTreeMap<String, Option<String>>, String> {
    use crate::profiles::interface_urls::{normalize_base_url, InterfaceKind};
    let Some(value) = body.get("interfaceBaseUrls") else {
        return Ok(Default::default());
    };
    let object = value
        .as_object()
        .ok_or("interfaceBaseUrls: must be an object {interfaceKind: url | null}")?;
    object
        .iter()
        .map(|(kind, url)| {
            if !InterfaceKind::parse(kind).is_some_and(InterfaceKind::has_address) {
                return Err(format!("interfaceBaseUrls: unknown interface kind {kind:?}"));
            }
            let url = match url {
                Value::Null => None,
                Value::String(text) => Some(
                    normalize_base_url(text).map_err(|message| format!("interfaceBaseUrls.{kind}: {message}"))?,
                ),
                _ => return Err(format!("interfaceBaseUrls.{kind}: must be a string or null")),
            };
            Ok((kind.clone(), url))
        })
        .collect()
}

fn plan_label(value: &Value) -> Result<String, String> {
    let text = value.as_str().ok_or("must be a string or null")?;
    crate::profiles::plan_label::normalize_manual_plan_label(text)
}

/// claude-code-sources: a list of `project` / `user` (empty = none).
fn setting_sources(value: &Value) -> Result<Vec<crate::profiles::claude_code::SettingSource>, String> {
    let items = value.as_array().ok_or("must be a list of project / user, or null")?;
    items
        .iter()
        .map(|item| {
            item.as_str()
                .and_then(crate::profiles::claude_code::SettingSource::parse)
                .ok_or_else(|| format!("unknown source {item}; known: project, user"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(crate::profiles::claude_code::normalize)
}

fn boolean(value: &Value) -> Result<bool, String> {
    value.as_bool().ok_or_else(|| "must be a boolean or null".to_string())
}

fn positive_count(value: &Value) -> Result<u64, String> {
    value
        .as_u64()
        .filter(|count| *count > 0 && *count <= i64::MAX as u64)
        .ok_or_else(|| "must be a positive integer or null".to_string())
}

fn http_url(value: &Value) -> Result<String, String> {
    let text = value.as_str().ok_or("must be a string or null")?.trim();
    let url = url::Url::parse(text).map_err(|error| format!("invalid URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("must be an http(s) URL with a host".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("must not embed credentials".into());
    }
    Ok(text.to_string())
}

/// v2 keys; the legacy `chatgpt` is accepted as codex.
fn provider_category(value: &Value) -> Result<crate::profiles::provider_category::ProviderCategory, String> {
    value
        .as_str()
        .and_then(crate::profiles::provider_category::ProviderCategory::parse)
        .ok_or_else(|| "must be kimi, glm, zai, claude, deepseek, codex, other or null".to_string())
}

/// profile-avatar: a non-empty list of unique known theme ids (canonical
/// order on return).
fn avatar_themes(value: &Value) -> Result<Vec<String>, String> {
    let items = value.as_array().ok_or("must be a list of theme ids or null")?;
    let ids = items
        .iter()
        .map(|item| item.as_str().map(str::to_string).ok_or_else(|| "must be a list of theme ids or null".to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    profile_avatar::normalize_avatar_themes(&ids)
}

/// profile-avatar: the greeting text ("" = clear on apply).
fn intro_greeting(value: &Value) -> Result<String, String> {
    let text = value.as_str().ok_or("must be a string or null")?;
    profile_avatar::normalize_intro_greeting(text).map(|text| text.unwrap_or_default())
}

fn color(value: &Value) -> Result<String, String> {
    let text = value.as_str().ok_or("must be a string or null")?.trim();
    let valid = text.len() == 7
        && text.starts_with('#')
        && text[1..].bytes().all(|b| b.is_ascii_hexdigit());
    valid
        .then(|| text.to_ascii_uppercase())
        .ok_or_else(|| "must be #RRGGBB or null".to_string())
}

pub(crate) fn parse_patch(body: &[u8]) -> Result<ProfileApiPatch, String> {
    let value: Value =
        serde_json::from_slice(body).map_err(|error| format!("JSON object body required: {error}"))?;
    let body = value.as_object().ok_or("JSON object body required")?;
    if let Some(unknown) = body.keys().find(|key| !PATCH_FIELDS.contains(&key.as_str())) {
        return Err(format!("unsupported field: {unknown}"));
    }
    let api_key = match body.get("apiKey") {
        None => None,
        Some(Value::String(key)) => {
            let key = key.trim();
            if key.is_empty()
                || key.len() > MAX_API_KEY_BYTES
                || key.chars().any(|c| c.is_control() || c.is_whitespace())
            {
                return Err(format!(
                    "apiKey: must be 1-{MAX_API_KEY_BYTES} non-whitespace characters"
                ));
            }
            Some(ApiKey(key.to_string()))
        }
        // r265: DSH stored keys only (checked by the update).
        Some(Value::Null) => None,
        Some(_) => return Err("apiKey: must be a string or null".into()),
    };
    let api_key_clear = body.get("apiKey") == Some(&Value::Null);
    let credential = optional(body, "credential", credential_choice)?;
    let default_variant = tri_state(body, "defaultVariant", variant_id_value)?;
    let avatar_style_import_if_unset = match body.get("avatarStyleImportIfUnset") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err("avatarStyleImportIfUnset: must be a boolean".into()),
    };
    if avatar_style_import_if_unset && !body.contains_key("avatarStyle") {
        return Err("avatarStyleImportIfUnset: avatarStyle is required".into());
    }
    Ok(ProfileApiPatch {
        dsh: match body.get("options") {
            None => ConfigValueUpdate::Unchanged,
            Some(value) => {
                let object = value.as_object().ok_or("options must be an object")?;
                if object.get("dsh") == Some(&Value::Null) {
                    if object.len() != 1 { return Err("unsupported options field".into()); }
                    ConfigValueUpdate::Clear
                } else {
                    parse_dsh_options(Some(value))?.map(ConfigValueUpdate::Set).unwrap_or_default()
                }
            }
        },
        avatar_style_import_if_unset,
        codex_config: CodexProfileConfigPatch {
            model: tri_state(body, "model", visible_string(200))?,
            model_provider: tri_state(body, "modelProvider", token(64, "_.-"))?,
            forced_login_method: tri_state(body, "forcedLoginMethod", one_of(&["chatgpt", "api"]))?,
            model_reasoning_effort: tri_state(body, "modelReasoningEffort", token(32, "_-"))?,
            model_reasoning_summary: tri_state(
                body,
                "modelReasoningSummary",
                one_of(MODEL_REASONING_SUMMARY_VALUES),
            )?,
            model_context_window: tri_state(body, "modelContextWindow", positive_count)?,
            model_auto_compact_token_limit: tri_state(body, "modelAutoCompactTokenLimit", positive_count)?,
            provider_name: tri_state(body, "providerName", visible_string(100))?,
            provider_base_url: tri_state(body, "providerBaseUrl", http_url)?,
            provider_env_key: tri_state(body, "providerEnvKey", env_name)?,
            provider_wire_api: tri_state(body, "providerWireApi", one_of(&["responses"]))?,
            request_max_retries: tri_state(body, "requestMaxRetries", bounded_count(100))?,
            stream_max_retries: tri_state(body, "streamMaxRetries", bounded_count(100))?,
            stream_idle_timeout_ms: tri_state(body, "streamIdleTimeoutMs", bounded_count(86_400_000))?,
            websocket_connect_timeout_ms: tri_state(body, "websocketConnectTimeoutMs", bounded_count(600_000))?,
            requires_openai_auth: tri_state(body, "requiresOpenaiAuth", boolean)?,
            update_plan_enabled: tri_state(body, "updatePlanEnabled", boolean)?,
            web_search: tri_state(body, "webSearch", one_of(WEB_SEARCH_VALUES))?,
            tool_schema_compat: tri_state(body, "toolSchemaCompat", one_of(TOOL_SCHEMA_COMPAT_VALUES))?,
            image_history_trim: tri_state(body, "imageHistoryTrim", image_history_trim)?,
            supports_websockets: tri_state(body, "supportsWebsockets", boolean)?,
            provider_instructions_field: tri_state(body, "providerInstructionsField", boolean)?,
            ..CodexProfileConfigPatch::default()
        },
        color: tri_state(body, "color", color)?,
        provider_category: tri_state(body, "providerCategory", provider_category)?,
        avatar_themes: tri_state(body, "avatarThemes", avatar_themes)?,
        avatar_style: tri_state(body, "avatarStyle", crate::profiles::avatar::parse_avatar_style)?,
        intro_greeting: tri_state(body, "introGreeting", intro_greeting)?,
        account_link: tri_state(body, "accountLink", |value| {
            crate::profiles::account_link::validate_manual(value.as_str().ok_or("must be a string or null")?)
                .map_err(|_| "must be none or m- followed by a base64url 16-byte group id".to_string())
        })?,
        allow_session_switch: tri_state(body, "allowSessionSwitch", boolean)?,
        native_subagents: tri_state(body, "nativeSubagents", boolean)?,
        delegation_wait: tri_state(body, "delegationWait", boolean)?,
        browser_tools: match body.get("browserTools") {
            None => None,
            Some(Value::Bool(value)) => Some(*value),
            Some(_) => return Err("browserTools: must be a boolean".into()),
        },
        proxy: match body.get("proxy") {
            None => None,
            Some(Value::Null) => Some(crate::config::proxy::ProxyUpdate(Map::from_iter([("mode".to_string(), json!("inherit"))]))),
            Some(Value::Object(update)) => Some(crate::config::proxy::ProxyUpdate(update.clone())),
            Some(_) => return Err("proxy: must be an object or null".into()),
        },
        search_tool: match body.get("searchTool") {
            None => None,
            Some(Value::Bool(value)) => Some(*value),
            Some(_) => return Err("searchTool: must be a boolean".into()),
        },
        private_chatgpt_auth_file: match body.get("privateChatgptAuthFile") {
            None => None,
            Some(Value::Bool(value)) => Some(*value),
            Some(_) => return Err("privateChatgptAuthFile: must be a boolean".into()),
        },
        auto_pick: match body.get("autoPick") {
            None => None,
            Some(Value::Bool(value)) => Some(*value),
            Some(_) => return Err("autoPick: must be a boolean".into()),
        },
        agent_use_requires_approval: match body.get("agentUseRequiresApproval") {
            None => None,
            Some(Value::Bool(value)) => Some(*value),
            Some(_) => return Err("agentUseRequiresApproval: must be a boolean".into()),
        },
        agent_turn_questions: tri_state(body, "agentTurnQuestions", agent_turn_questions)?,
        claude_file_checkpointing: tri_state(body, "claudeFileCheckpointing", boolean)?,
        claude_compaction: tri_state(body, "claudeCompaction", claude_compaction)?,
        claude_compaction_models: tri_state(body, "claudeCompactionModels", claude_compaction_models)?,
        claude_setting_sources: tri_state(body, "claudeSettingSources", setting_sources)?,
        claude_ai_connectors: match body.get("claudeAiConnectors") {
            None => None,
            Some(Value::Bool(value)) => Some(*value),
            Some(_) => return Err("claudeAiConnectors: must be a boolean".into()),
        },
        claude_allow_write: tri_state(body, "claudeAllowWrite", boolean)?,
        claude_allow_exec: tri_state(body, "claudeAllowExec", boolean)?,
        plan_label: tri_state(body, "planLabel", plan_label)?,
        claude_account: tri_state(body, "claudeAccount", claude_account_id)?,
        kimi_account: tri_state(body, "kimiAccount", kimi_account_id)?,
        model_display_names: model_display_names(body)?,
        interface_base_urls: interface_base_urls(body)?,
        api_key,
        api_key_clear,
        credential,
        default_variant,
    })
}

/// r265 `credential {source: stored|env, envName?}` (DSH profiles).
fn credential_choice(value: &Value) -> Result<crate::profiles::runtime_variants::CredentialChoice, String> {
    use crate::profiles::runtime_variants::CredentialChoice;
    let object = value.as_object().ok_or("must be an object")?;
    if let Some(unknown) = object.keys().find(|key| !["source", "envName"].contains(&key.as_str())) {
        return Err(format!("unsupported field: {unknown}"));
    }
    match (object.get("source").and_then(Value::as_str), object.get("envName")) {
        (Some("stored"), None | Some(Value::Null)) => Ok(CredentialChoice::Stored),
        (Some("env"), Some(Value::String(name))) => {
            crate::profiles::stored_keys::validate_env_source_name(name).map_err(|error| format!("envName: {error:#}"))?;
            Ok(CredentialChoice::Env(name.clone()))
        }
        (Some("env"), _) => Err("envName: required string for source env (no implicit default)".into()),
        (Some("stored"), _) => Err("envName applies to source env only".into()),
        _ => Err("source must be stored or env".into()),
    }
}

fn variant_id_value(value: &Value) -> Result<String, String> {
    let id = value.as_str().ok_or("must be a string")?;
    crate::profiles::runtime_variants::validate_variant_id(id).map_err(|error| error.to_string())?;
    Ok(id.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn dsh_profile_create_patch_roundtrip_and_boundaries() {
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let created = create_from_body(br#"{"name":"DSH test","kind":"dsh","options":{"dsh":{"reasoningEffort":"max","apiKeyEnv":"DSH_TEST_KEY"}}}"#).unwrap();
        assert_eq!(created["contractVersion"], 2);
        assert_eq!(created["created"], true);
        assert_eq!(created["kind"], "dsh");
        assert_eq!(created["credentialsPresent"], false);
        let profile = &created["profile"];
        assert_eq!(profile["cliKind"], "dsh");
        assert_eq!(profile["model"], crate::profiles::dsh::DEFAULT_MODEL);
        assert_eq!(profile["options"]["dsh"]["apiKeyEnv"], "DSH_TEST_KEY");
        assert_eq!(profile["modelReasoningEffort"], "max");
        let id = profile["id"].as_str().unwrap();
        let patch = parse_patch(br#"{"options":{"dsh":{"model":"deepseek-custom","reasoningEffort":"off"}}}"#).unwrap();
        let (account, changed) = settings_api::update_profile(id, &patch).unwrap();
        assert!(changed);
        assert_eq!(account.cli_kind, crate::profiles::model::CliKind::Dsh);
        assert_eq!(account.options.dsh.model, "deepseek-custom");
        let files = crate::profiles::materialize::materialized_account_files(&account).unwrap();
        assert!(!files.auth_path.exists());
        assert!(!files.config_path.exists());
        let models = crate::profiles::catalog_api::models_resource(&account).unwrap();
        assert_eq!(models["modelsSource"]["kind"], "dsh", "DSH models resource: {models}");
        assert_eq!(models["models"][0]["supportedReasoningEfforts"], json!(["off","low","high","max"]));
        assert_eq!(models["selectedModel"], "deepseek-custom");
        for body in [
            br#"{"privateChatgptAuthFile":false}"#.as_slice(),
            br#"{"claudeAccount":"default"}"#,
            br#"{"claudeAllowWrite":true}"#,
            br#"{"claudeAiConnectors":false}"#,
            br#"{"providerEnvKey":"OTHER_KEY"}"#,
        ] {
            let patch = parse_patch(body).unwrap();
            assert!(settings_api::update_profile(id, &patch).is_err());
        }
        let copy = lifecycle::duplicate_profile(id, "DSH copy").unwrap();
        assert_eq!(copy.cli_kind, crate::profiles::model::CliKind::Dsh);
        assert_eq!(copy.options.dsh, account.options.dsh);
        let exported = crate::profiles::transfer::export_profile(id, false).unwrap();
        assert!(!exported.credentials_included);
        let imported = crate::profiles::transfer::import_profile(&exported.bundle, "DSH imported").unwrap();
        assert_eq!(imported.account.cli_kind, crate::profiles::model::CliKind::Dsh);
        assert_eq!(imported.account.options.dsh, account.options.dsh);
        let renamed = lifecycle::rename_profile(&copy.id, "DSH renamed").unwrap();
        assert_eq!(renamed.account.cli_kind, crate::profiles::model::CliKind::Dsh);
        lifecycle::delete_profile(&copy.id).unwrap();

        let wire = serde_json::to_value(&account).unwrap();
        assert_eq!(wire["cli_kind"], "dsh");
        let decoded: crate::profiles::model::StoredAccount = serde_json::from_value(wire).unwrap();
        assert_eq!(decoded.cli_kind, crate::profiles::model::CliKind::Dsh);
    }

    #[test]
    fn interface_base_urls_create_patch_and_views() {
        use crate::profiles::interface_urls::{BASE_URL_MISSING, DEEPSEEK_ANTHROPIC_BASE_URL};
        // Body checks: known address-bearing kinds, plain http(s) URLs,
        // only for apiKey / dsh creates.
        assert!(parse_patch(br#"{}"#).unwrap().interface_base_urls.is_empty());
        let patch = parse_patch(br#"{"interfaceBaseUrls":{"deepseekNative":" https://relay.example/anthropic ","kimiNative":null}}"#).unwrap();
        assert_eq!(patch.interface_base_urls.get("deepseekNative"), Some(&Some("https://relay.example/anthropic".into())));
        assert_eq!(patch.interface_base_urls.get("kimiNative"), Some(&None));
        for body in [
            r#"{"interfaceBaseUrls":[]}"#,
            r#"{"interfaceBaseUrls":{"chatgptLogin":null}}"#,
            r#"{"interfaceBaseUrls":{"deepseekAnthropic":null}}"#,
            r#"{"interfaceBaseUrls":{"deepseekNative":1}}"#,
            r#"{"interfaceBaseUrls":{"deepseekNative":"https://k:s@relay.example"}}"#,
        ] {
            assert!(parse_patch(body.as_bytes()).is_err(), "{body}");
        }
        for kind in ["chatgpt", "claude"] {
            let body = json!({"name":"x","kind":kind,"interfaceBaseUrls":{"deepseekNative":DEEPSEEK_ANTHROPIC_BASE_URL}});
            assert!(parse_create(&serde_json::to_vec(&body).unwrap()).is_err(), "{kind}");
        }

        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let (status, presets) = call("GET", PRESETS_PATH, "", false);
        assert_eq!(status, 200);
        let deepseek = presets["presets"].as_array().unwrap().iter().find(|row| row["id"] == "deepseek").unwrap();
        assert_eq!(deepseek["providerInterfaces"][1]["baseUrlDefault"], DEEPSEEK_ANTHROPIC_BASE_URL);

        // A relay DSH address is accepted at create (DSH reads it: B4).
        let relay = json!({"name":"ds relay addr","kind":"apiKey","preset":"deepseek","baseUrl":"https://relay.example/v1",
            "interfaceBaseUrls":{"deepseekNative":"https://relay.example/anthropic"}});
        let (status, created) = call("POST", "/v2/profiles", &relay.to_string(), false);
        assert_eq!(status, 201, "{created}");
        let interfaces = &created["profile"]["providerInterfaces"];
        assert_eq!(interfaces[1]["baseUrl"], "https://relay.example/anthropic");
        assert_eq!((interfaces[1]["official"].clone(), interfaces[1]["relayAccepted"].clone()), (json!(false), json!(true)));
        // ... and on a DSH-kind profile.
        let (status, dsh) = call("POST", "/v2/profiles",
            r#"{"name":"dsh relay","kind":"dsh","interfaceBaseUrls":{"deepseekNative":"http://127.0.0.1:9/anthropic"}}"#, false);
        assert_eq!(status, 201, "{dsh}");
        assert_eq!(dsh["profile"]["providerInterfaces"][0]["baseUrl"], "http://127.0.0.1:9/anthropic");
        for name in ["ds relay addr", "dsh relay"] {
            let (status, _) = call("DELETE", &format!("/v2/profiles/{}", name.replace(' ', "%20")), "", false);
            assert!(status == 200 || status == 204, "{name}: {status}");
        }

        let (status, created) = call("POST", "/v2/profiles", r#"{"name":"ds relay","kind":"apiKey","preset":"deepseek","baseUrl":"https://relay.example/v1"}"#, false);
        assert_eq!(status, 201, "{created}");
        let interfaces = &created["profile"]["providerInterfaces"];
        assert_eq!(interfaces[0]["official"], false);
        assert_eq!(interfaces[1]["kind"], "deepseekNative");
        assert_eq!(interfaces[1]["baseUrl"], Value::Null);
        let id = created["profile"]["id"].as_str().unwrap().to_string();
        let path = format!("/v2/profiles/{id}");
        // Variants API (B5): adding DSH without its address is refused; with
        // `baseUrl` it is stored as the profile's deepseekNative address.
        let (status, refused) = call("POST", &format!("{path}/variants"), r#"{"runtime":"dsh"}"#, false);
        assert_eq!(status, 409, "{refused}");
        assert!(refused.to_string().contains(BASE_URL_MISSING), "{refused}");
        let (status, added) = call("POST", &format!("{path}/variants"), r#"{"runtime":"dsh","baseUrl":"https://relay.example/anthropic"}"#, false);
        assert_eq!(status, 201, "{added}");
        assert_eq!(added["profile"]["providerInterfaces"][1]["baseUrl"], "https://relay.example/anthropic");
        let (status, patched) = call("PATCH", &format!("{path}/variants/dsh"), r#"{"baseUrl":"https://relay2.example/anthropic"}"#, false);
        assert_eq!(status, 200, "{patched}");
        assert_eq!(patched["profile"]["providerInterfaces"][1]["baseUrl"], "https://relay2.example/anthropic");
        let (status, refused) = call("PATCH", &format!("{path}/variants/codex"), r#"{"baseUrl":"https://relay2.example/anthropic"}"#, false);
        assert_eq!(status, 400, "the cute-codex address is providerBaseUrl: {refused}");
        let (status, _) = call("PATCH", &format!("{path}/variants/dsh"), r#"{"baseUrl":"ftp://x"}"#, false);
        assert_eq!(status, 400);
        let (status, _) = call("DELETE", &format!("{path}/variants/dsh"), "", false);
        assert_eq!(status, 200);
        let (status, patched) = call("PATCH", &path, r#"{"interfaceBaseUrls":{"deepseekNative":"https://relay.example/anthropic"}}"#, false);
        assert_eq!(status, 200, "{patched}");
        assert_eq!(patched["profile"]["providerInterfaces"][1]["official"], false);
        let (status, patched) = call("PATCH", &path, &json!({"interfaceBaseUrls":{"deepseekNative":DEEPSEEK_ANTHROPIC_BASE_URL}}).to_string(), false);
        assert_eq!(status, 200, "{patched}");
        assert_eq!(patched["changed"], true);
        assert_eq!(patched["profile"]["providerInterfaces"][1]["baseUrl"], DEEPSEEK_ANTHROPIC_BASE_URL);
        assert_eq!(patched["profile"]["providerInterfaces"][1]["official"], true);
        // The main address back to official keeps the stored DSH address.
        let (status, patched) = call("PATCH", &path, r#"{"providerBaseUrl":"https://api.deepseek.com/"}"#, false);
        assert_eq!(status, 200, "{patched}");
        assert_eq!(patched["profile"]["providerInterfaces"][0]["official"], true);
        let (_, patched) = call("PATCH", &path, r#"{"interfaceBaseUrls":{"deepseekNative":null}}"#, false);
        assert_eq!(patched["profile"]["providerInterfaces"][1]["baseUrl"], DEEPSEEK_ANTHROPIC_BASE_URL);
        assert!(settings_api::load_store().unwrap().accounts.iter().all(|account| account.interface_base_urls.is_empty()));

        // Profiles without a stored interface refuse the field.
        let (_, gpt) = call("POST", "/v2/profiles", r#"{"name":"gpt","kind":"chatgpt"}"#, false);
        let (status, _) = call("PATCH", &format!("/v2/profiles/{}", gpt["profile"]["id"].as_str().unwrap()),
            &json!({"interfaceBaseUrls":{"deepseekNative":DEEPSEEK_ANTHROPIC_BASE_URL}}).to_string(), false);
        assert_eq!(status, 400);
    }

    #[test]
    fn dsh_profile_create_rejects_foreign_options_and_secrets() {
        assert_eq!(parse_create(br#"{"name":"d","kind":"dsh"}"#).unwrap().kind, NewProfileKind::Dsh);
        for body in [
            // r265: a top-level apiKey is the write-only stored key (accepted);
            // keys inside options stay refused.
            json!({"name":"d","kind":"dsh","claudeAccount":"default"}),
            json!({"name":"d","kind":"dsh","options":{"dsh":{"apiKey":"never"}}}),
            json!({"name":"d","kind":"dsh","options":{"dsh":{"reasoningEffort":"medium"}}}),
            json!({"name":"d","kind":"dsh","options":{"dsh":{"apiKeyEnv":"sk-not-env"}}}),
            json!({"name":"d","kind":"claude","options":{"dsh":{}}}),
        ] { assert!(parse_create(&serde_json::to_vec(&body).unwrap()).is_err(), "{body}"); }
    }

    use super::*;

    #[test]
    fn browser_profile_patch_is_boolean_and_default_off() {
        assert!(parse_patch(br#"{"proxy":{"mode":"none"}}"#).unwrap().proxy.is_some());
        assert_eq!(parse_patch(br#"{"proxy":null}"#).unwrap().proxy.unwrap().0["mode"], "inherit");
        assert!(parse_patch(br#"{"proxy":"http://x"}"#).is_err());
        assert_eq!(parse_patch(br#"{"browserTools":true}"#).unwrap().browser_tools,Some(true));
        assert_eq!(parse_patch(br#"{"browserTools":false}"#).unwrap().browser_tools,Some(false));
        assert_eq!(parse_patch(br#"{}"#).unwrap().browser_tools,None);
        for bad in [br#"{"browserTools":null}"#.as_slice(),br#"{"browserTools":"true"}"#,br#"{"browserTools":1}"#] {
            assert!(parse_patch(bad).is_err());
        }
    }

    #[test]
    fn claude_code_sources_patch() {
        use crate::profiles::claude_code::SettingSource::*;
        assert_eq!(parse_patch(br#"{}"#).unwrap().claude_setting_sources, ConfigValueUpdate::Unchanged);
        assert_eq!(parse_patch(br#"{"claudeSettingSources":null}"#).unwrap().claude_setting_sources, ConfigValueUpdate::Clear);
        assert_eq!(parse_patch(br#"{"claudeSettingSources":[]}"#).unwrap().claude_setting_sources, ConfigValueUpdate::Set(vec![]));
        assert_eq!(
            parse_patch(br#"{"claudeSettingSources":["user","project","user"]}"#).unwrap().claude_setting_sources,
            ConfigValueUpdate::Set(vec![Project, User])
        );
        for bad in [br#"{"claudeSettingSources":["local"]}"#.as_slice(), br#"{"claudeSettingSources":"project"}"#] {
            assert!(parse_patch(bad).unwrap_err().starts_with("claudeSettingSources: "));
        }
        assert_eq!(parse_patch(br#"{"claudeAiConnectors":true}"#).unwrap().claude_ai_connectors, Some(true));
        assert_eq!(parse_patch(br#"{}"#).unwrap().claude_ai_connectors, None);
        assert!(parse_patch(br#"{"claudeAiConnectors":null}"#).unwrap_err().starts_with("claudeAiConnectors: "));
    }

    #[test]
    fn private_chatgpt_auth_file_patch_is_boolean() {
        assert_eq!(parse_patch(br#"{"privateChatgptAuthFile":true}"#).unwrap().private_chatgpt_auth_file, Some(true));
        assert_eq!(parse_patch(br#"{}"#).unwrap().private_chatgpt_auth_file, None);
        for bad in [br#"{"privateChatgptAuthFile":null}"#.as_slice(), br#"{"privateChatgptAuthFile":"on"}"#] {
            assert!(parse_patch(bad).unwrap_err().starts_with("privateChatgptAuthFile: "));
        }
    }

    #[test]
    fn search_tool_profile_patch_is_boolean() {
        assert_eq!(parse_patch(br#"{"searchTool":true}"#).unwrap().search_tool, Some(true));
        assert_eq!(parse_patch(br#"{"searchTool":false}"#).unwrap().search_tool, Some(false));
        assert_eq!(parse_patch(br#"{}"#).unwrap().search_tool, None);
        for bad in [br#"{"searchTool":null}"#.as_slice(), br#"{"searchTool":"on"}"#, br#"{"searchTool":1}"#] {
            assert!(parse_patch(bad).unwrap_err().starts_with("searchTool: "));
        }
    }

    #[test]
    fn item_paths_decode_and_reject_nested() {
        assert!(is_list_path("/v2/profiles"));
        assert_eq!(parse_item_path("/v2/profiles/GLM"), Some("GLM".into()));
        assert_eq!(parse_item_path("/v2/profiles/my%20glm"), Some("my glm".into()));
        assert_eq!(parse_item_path("/v2/profiles/GLM/quota"), None);
        assert_eq!(parse_item_path("/v2/profiles/a%2Fb"), None);
        assert_eq!(parse_item_path("/v2/profiles/"), None);
        assert_eq!(parse_duplicate_path("/v2/profiles/my%20glm/duplicate"), Some("my glm".into()));
        assert_eq!(parse_duplicate_path("/v2/profiles/duplicate"), None);
        assert_eq!(parse_duplicate_path("/v2/profiles/a/b/duplicate"), None);
        assert!(is_profiles_path("/v2/profiles/GLM/duplicate"));
        assert!(is_profiles_path(PRESETS_PATH));
        assert!(!is_profiles_path("/v2/profiles/GLM/quota"));
    }

    #[test]
    fn create_body_defaults_and_validation() {
        let legacy = parse_create(br#"{"name":" GLM "}"#).unwrap();
        assert_eq!(legacy.name, "GLM");
        assert_eq!(legacy.kind, NewProfileKind::ApiKey);
        assert!(legacy.preset.is_none() && legacy.api_key.is_none());

        let full = parse_create(
            br#"{"name":"kimi","kind":"apiKey","preset":"kimi","baseUrl":"https://api.moonshot.cn/v1",
                 "model":"kimi-k2","apiKey":"sk-test-only"}"#,
        )
        .unwrap();
        assert_eq!(full.preset.unwrap().id, "moonshot");
        assert_eq!(full.base_url.as_deref(), Some("https://api.moonshot.cn/v1"));
        assert_eq!(full.model.as_deref(), Some("kimi-k2"));
        assert!(!format!("{full:?}").contains("sk-test-only"));
        assert_eq!(parse_create(br#"{"name":"c","kind":"claude"}"#).unwrap().kind, NewProfileKind::Claude);
        assert_eq!(parse_create(br#"{"name":"w","kind":"chatgpt","model":"gpt-5.5"}"#).unwrap().kind, NewProfileKind::Chatgpt);
        for bad in [
            r#"{}"#,
            r#"{"name":""}"#,
            r#"{"name":"a/b"}"#,
            r#"{"name":"x","kind":"api-key"}"#,
            r#"{"name":"x","preset":"nope"}"#,
            r#"{"name":"x","baseUrl":"ftp://x"}"#,
            r#"{"name":"x","kind":"chatgpt","preset":"zai"}"#,
            r#"{"name":"x","kind":"claude","apiKey":"k"}"#,
            r#"{"name":"x","apiKey":"has space"}"#,
            r#"{"name":"x","extra":1}"#,
            r#"[]"#,
        ] {
            assert!(parse_create(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn name_bodies_for_duplicate_and_rename() {
        assert_eq!(parse_name_body(br#"{"name":"copy"}"#).unwrap(), "copy");
        assert!(parse_name_body(br#"{}"#).is_err());
        assert!(parse_name_body(br#"{"name":"copy","model":"x"}"#).is_err());
        assert_eq!(parse_name_field(br#"{"model":"x"}"#).unwrap(), None);
        assert_eq!(parse_name_field(br#"{"name":"new","model":"x"}"#).unwrap().as_deref(), Some("new"));
        assert!(parse_name_field(br#"{"name":1}"#).is_err());
        // PATCH accepts name next to config fields.
        let patch = parse_patch(br#"{"name":"new","model":"glm-5"}"#).unwrap();
        assert_eq!(patch.codex_config.model, ConfigValueUpdate::Set("glm-5".into()));
    }

    #[test]
    fn patch_is_tri_state_and_strict() {
        let patch = parse_patch(
            br##"{"model":" glm-5 ","modelReasoningEffort":null,"modelContextWindow":200000,
                 "color":"#aabbcc","allowSessionSwitch":false,"providerEnvKey":"GLM_KEY",
                 "requestMaxRetries":0,"supportsWebsockets":null,"forcedLoginMethod":"api",
                 "modelReasoningSummary":"concise"}"##,
        )
        .unwrap();
        assert_eq!(patch.codex_config.model, ConfigValueUpdate::Set("glm-5".into()));
        assert_eq!(patch.codex_config.model_reasoning_effort, ConfigValueUpdate::Clear);
        assert_eq!(patch.codex_config.model_reasoning_summary, ConfigValueUpdate::Set("concise".into()));
        assert_eq!(patch.codex_config.model_context_window, ConfigValueUpdate::Set(200000));
        assert_eq!(patch.codex_config.model_provider, ConfigValueUpdate::Unchanged);
        assert_eq!(patch.color, ConfigValueUpdate::Set("#AABBCC".into()));
        assert_eq!(patch.allow_session_switch, ConfigValueUpdate::Set(false));
        assert!(patch.api_key.is_none());
        assert_eq!(patch.codex_config.provider_env_key, ConfigValueUpdate::Set("GLM_KEY".into()));
        assert_eq!(patch.codex_config.request_max_retries, ConfigValueUpdate::Set(0));
        assert_eq!(patch.codex_config.supports_websockets, ConfigValueUpdate::Clear);
        assert_eq!(patch.codex_config.provider_instructions_field, ConfigValueUpdate::Unchanged);
        assert_eq!(
            parse_patch(br#"{"providerInstructionsField":true}"#).unwrap().codex_config.provider_instructions_field,
            ConfigValueUpdate::Set(true)
        );
        assert!(parse_patch(br#"{"providerInstructionsField":"yes"}"#).is_err());
        assert_eq!(patch.codex_config.forced_login_method, ConfigValueUpdate::Set("api".into()));
        assert_eq!(patch.provider_category, ConfigValueUpdate::Unchanged);
        use crate::profiles::provider_category::ProviderCategory;
        assert_eq!(
            parse_patch(br#"{"providerCategory":"zai"}"#).unwrap().provider_category,
            ConfigValueUpdate::Set(ProviderCategory::Zai)
        );
        assert_eq!(
            parse_patch(br#"{"providerCategory":"chatgpt"}"#).unwrap().provider_category,
            ConfigValueUpdate::Set(ProviderCategory::Codex)
        );
        assert_eq!(parse_patch(br#"{"providerCategory":null}"#).unwrap().provider_category, ConfigValueUpdate::Clear);

        for bad in [
            r#"{"providerCategory":"openai"}"#,
            r#"{"providerCategory":1}"#,
            r#"{"unknown":1}"#,
            r#"{"modelContextWindow":0}"#,
            r#"{"modelContextWindow":-5}"#,
            r#"{"contextWindow":1000}"#,
            r#"{"providerBaseUrl":"ftp://x"}"#,
            r#"{"providerBaseUrl":"https://user:pw@example.com"}"#,
            r#"{"providerEnvKey":"lower"}"#,
            r#"{"providerWireApi":"chat"}"#,
            r#"{"requestMaxRetries":101}"#,
            r#"{"forcedLoginMethod":"oauth"}"#,
            r#"{"color":"red"}"#,
            r#"{"modelProvider":"bad provider"}"#,
            // r265: `apiKey: null` parses (DSH stored-key clear; refused by the update elsewhere).
            r#"{"apiKey":1}"#,
            r#"{"apiKey":"has space"}"#,
            r#"{"allowSessionSwitch":"yes"}"#,
            r#"{"modelReasoningSummary":"verbose"}"#,
            r#"{"modelReasoningSummary":1}"#,
            r#"[]"#,
        ] {
            assert!(parse_patch(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn reasoning_summary_patch() {
        for value in ["auto", "concise", "detailed", "none"] {
            let body = format!(r#"{{"modelReasoningSummary":" {value} "}}"#);
            assert_eq!(
                parse_patch(body.as_bytes()).unwrap().codex_config.model_reasoning_summary,
                ConfigValueUpdate::Set(value.to_string())
            );
        }
        assert_eq!(
            parse_patch(br#"{"modelReasoningSummary":null}"#).unwrap().codex_config.model_reasoning_summary,
            ConfigValueUpdate::Clear
        );
        let unchanged = parse_patch(br#"{"modelReasoningEffort":"high"}"#).unwrap();
        assert_eq!(unchanged.codex_config.model_reasoning_summary, ConfigValueUpdate::Unchanged);
        for bad in [
            r#"{"modelReasoningSummary":"verbose"}"#,
            r#"{"modelReasoningSummary":"Auto"}"#,
            r#"{"modelReasoningSummary":""}"#,
            r#"{"modelReasoningSummary":1}"#,
            r#"{"modelReasoningSummary":true}"#,
            r#"{"modelReasoningSummary":["auto"]}"#,
        ] {
            let error = parse_patch(bad.as_bytes()).unwrap_err();
            assert!(error.starts_with("modelReasoningSummary: "), "{bad}: {error}");
        }
        assert_eq!(
            parse_patch(br#"{"reasoningSummary":"auto"}"#).unwrap_err(),
            "unsupported field: reasoningSummary"
        );
    }

    #[test]
    fn web_search_patch() {
        for value in ["disabled", "cached", "indexed", "live"] {
            let body = format!(r#"{{"webSearch":"{value}"}}"#);
            assert_eq!(
                parse_patch(body.as_bytes()).unwrap().codex_config.web_search,
                ConfigValueUpdate::Set(value.to_string())
            );
        }
        assert_eq!(
            parse_patch(br#"{"webSearch":null}"#).unwrap().codex_config.web_search,
            ConfigValueUpdate::Clear
        );
        let unchanged = parse_patch(br#"{"modelReasoningEffort":"high"}"#).unwrap();
        assert_eq!(unchanged.codex_config.web_search, ConfigValueUpdate::Unchanged);
        for bad in [r#"{"webSearch":"off"}"#, r#"{"webSearch":false}"#, r#"{"webSearch":""}"#] {
            let error = parse_patch(bad.as_bytes()).unwrap_err();
            assert!(error.starts_with("webSearch: "), "{bad}: {error}");
        }
    }

    #[test]
    fn tool_schema_compat_patch() {
        for value in ["off", "strict"] {
            let body = format!(r#"{{"toolSchemaCompat":"{value}"}}"#);
            assert_eq!(
                parse_patch(body.as_bytes()).unwrap().codex_config.tool_schema_compat,
                ConfigValueUpdate::Set(value.to_string())
            );
        }
        assert_eq!(
            parse_patch(br#"{"toolSchemaCompat":null}"#).unwrap().codex_config.tool_schema_compat,
            ConfigValueUpdate::Clear
        );
        let unchanged = parse_patch(br#"{"webSearch":"disabled"}"#).unwrap();
        assert_eq!(unchanged.codex_config.tool_schema_compat, ConfigValueUpdate::Unchanged);
        for bad in [r#"{"toolSchemaCompat":"gemini"}"#, r#"{"toolSchemaCompat":true}"#, r#"{"toolSchemaCompat":""}"#] {
            let error = parse_patch(bad.as_bytes()).unwrap_err();
            assert!(error.starts_with("toolSchemaCompat: "), "{bad}: {error}");
        }
    }

    #[test]
    fn image_history_trim_patch() {
        use crate::profiles::codex_profile::ImageHistoryTrim;
        assert_eq!(parse_patch(br#"{}"#).unwrap().codex_config.image_history_trim, ConfigValueUpdate::Unchanged);
        assert_eq!(
            parse_patch(br#"{"imageHistoryTrim":null}"#).unwrap().codex_config.image_history_trim,
            ConfigValueUpdate::Clear
        );
        for (max_inline, keep) in [(12, 4), (2, 1), (500, 50), (51, 50)] {
            let body = format!(r#"{{"imageHistoryTrim":{{"maxInline":{max_inline},"keep":{keep}}}}}"#);
            assert_eq!(
                parse_patch(body.as_bytes()).unwrap().codex_config.image_history_trim,
                ConfigValueUpdate::Set(ImageHistoryTrim { max_inline, keep })
            );
        }
        for bad in [
            r#"{"imageHistoryTrim":{"maxInline":12,"keep":0}}"#,
            r#"{"imageHistoryTrim":{"maxInline":100,"keep":51}}"#,
            r#"{"imageHistoryTrim":{"maxInline":4,"keep":4}}"#,
            r#"{"imageHistoryTrim":{"maxInline":3,"keep":4}}"#,
            r#"{"imageHistoryTrim":{"maxInline":501,"keep":4}}"#,
            r#"{"imageHistoryTrim":{"maxInline":12}}"#,
            r#"{"imageHistoryTrim":{"keep":4}}"#,
            r#"{"imageHistoryTrim":{"maxInline":12.5,"keep":4}}"#,
            r#"{"imageHistoryTrim":{"maxInline":-12,"keep":4}}"#,
            r#"{"imageHistoryTrim":{"maxInline":"12","keep":4}}"#,
            r#"{"imageHistoryTrim":{"maxInline":12,"keep":4,"extra":1}}"#,
            r#"{"imageHistoryTrim":12}"#,
            r#"{"imageHistoryTrim":true}"#,
            r#"{"imageHistoryTrim":[12,4]}"#,
        ] {
            let error = parse_patch(bad.as_bytes()).unwrap_err();
            assert!(error.starts_with("imageHistoryTrim: "), "{bad}: {error}");
        }
    }

    #[test]
    fn update_plan_enabled_patch() {
        for value in [true, false] {
            let body = format!(r#"{{"updatePlanEnabled":{value}}}"#);
            assert_eq!(
                parse_patch(body.as_bytes()).unwrap().codex_config.update_plan_enabled,
                ConfigValueUpdate::Set(value)
            );
        }
        assert_eq!(
            parse_patch(br#"{"updatePlanEnabled":null}"#).unwrap().codex_config.update_plan_enabled,
            ConfigValueUpdate::Clear
        );
        let unchanged = parse_patch(br#"{"modelReasoningEffort":"high"}"#).unwrap();
        assert_eq!(unchanged.codex_config.update_plan_enabled, ConfigValueUpdate::Unchanged);
        for bad in [
            r#"{"updatePlanEnabled":"true"}"#,
            r#"{"updatePlanEnabled":1}"#,
            r#"{"updatePlanEnabled":{"enabled":true}}"#,
        ] {
            let error = parse_patch(bad.as_bytes()).unwrap_err();
            assert!(error.starts_with("updatePlanEnabled: "), "{bad}: {error}");
        }
    }

    #[test]
    fn claude_compaction_models_patch() {
        use crate::profiles::claude_compaction::ClaudeCompaction;
        assert_eq!(parse_patch(br#"{}"#).unwrap().claude_compaction_models, ConfigValueUpdate::Unchanged);
        assert_eq!(parse_patch(br#"{"claudeCompactionModels":null}"#).unwrap().claude_compaction_models, ConfigValueUpdate::Clear);
        assert_eq!(
            parse_patch(br#"{"claudeCompactionModels":{"claude-sonnet-5-5":{"window":233000,"percent":100}}}"#).unwrap().claude_compaction_models,
            ConfigValueUpdate::Set([("claude-sonnet-5-5".to_string(), ClaudeCompaction { auto_compact_window: 233_000, auto_compact_percent: 100 })].into())
        );
        assert_eq!(parse_patch(br#"{"claudeCompactionModels":{}}"#).unwrap().claude_compaction_models, ConfigValueUpdate::Set(Default::default()));
        for bad in [
            r#"{"claudeCompactionModels":{"claude-sonnet-5-5":{"window":99999,"percent":100}}}"#,
            r#"{"claudeCompactionModels":{"has space":{"window":233000,"percent":100}}}"#,
            r#"{"claudeCompactionModels":{"":{"window":233000,"percent":100}}}"#,
            r#"{"claudeCompactionModels":{"m":{"window":233000}}}"#,
            r#"{"claudeCompactionModels":[]}"#,
        ] {
            assert!(parse_patch(bad.as_bytes()).is_err(), "{bad}");
        }
        let many: serde_json::Map<String, Value> = (0..33).map(|i| (format!("m{i}"), json!({"window":233000,"percent":100}))).collect();
        assert!(parse_patch(json!({"claudeCompactionModels": many}).to_string().as_bytes()).is_err());
    }

    #[test]
    fn claude_compaction_patch() {
        use crate::profiles::claude_compaction::ClaudeCompaction;
        assert_eq!(parse_patch(br#"{}"#).unwrap().claude_compaction, ConfigValueUpdate::Unchanged);
        assert_eq!(parse_patch(br#"{"claudeCompaction":null}"#).unwrap().claude_compaction, ConfigValueUpdate::Clear);
        assert_eq!(
            parse_patch(br#"{"claudeCompaction":{"window":533000,"percent":100}}"#).unwrap().claude_compaction,
            ConfigValueUpdate::Set(ClaudeCompaction { auto_compact_window: 533_000, auto_compact_percent: 100 })
        );
        for bad in [
            r#"{"claudeCompaction":{"window":99999,"percent":100}}"#,
            r#"{"claudeCompaction":{"window":1000001,"percent":100}}"#,
            r#"{"claudeCompaction":{"window":500000,"percent":0}}"#,
            r#"{"claudeCompaction":{"window":500000,"percent":101}}"#,
            r#"{"claudeCompaction":{"window":500000}}"#,
            r#"{"claudeCompaction":{"window":500000.5,"percent":90}}"#,
            r#"{"claudeCompaction":{"window":500000,"percent":90,"models":{}}}"#,
            r#"{"claudeCompaction":500000}"#,
        ] {
            assert!(parse_patch(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn claude_file_checkpointing_patch() {
        assert_eq!(parse_patch(br#"{}"#).unwrap().claude_file_checkpointing, ConfigValueUpdate::Unchanged);
        assert_eq!(parse_patch(br#"{"claudeFileCheckpointing":null}"#).unwrap().claude_file_checkpointing, ConfigValueUpdate::Clear);
        assert_eq!(parse_patch(br#"{"claudeFileCheckpointing":true}"#).unwrap().claude_file_checkpointing, ConfigValueUpdate::Set(true));
        assert_eq!(parse_patch(br#"{"claudeFileCheckpointing":false}"#).unwrap().claude_file_checkpointing, ConfigValueUpdate::Set(false));
        for bad in [r#"{"claudeFileCheckpointing":"on"}"#, r#"{"claudeFileCheckpointing":1}"#] {
            assert!(parse_patch(bad.as_bytes()).is_err());
        }
    }

    #[test]
    fn claude_tools_patch() {
        let patch = parse_patch(br#"{"claudeAllowWrite":true,"claudeAllowExec":false}"#).unwrap();
        assert_eq!((patch.claude_allow_write, patch.claude_allow_exec), (ConfigValueUpdate::Set(true), ConfigValueUpdate::Set(false)));
        assert_eq!(parse_patch(br#"{}"#).unwrap().claude_allow_write, ConfigValueUpdate::Unchanged);
        assert_eq!(parse_patch(br#"{"claudeAllowWrite":null}"#).unwrap().claude_allow_write, ConfigValueUpdate::Clear);
        for bad in [r#"{"claudeAllowExec":"on"}"#] {
            assert!(parse_patch(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn model_display_names_patch() {
        let parsed = parse_patch(br#"{"modelDisplayNames":{"gpt-6-astra":" Astra 6 ","Astra [400K]":null,"x":""}}"#)
            .unwrap()
            .model_display_names;
        assert_eq!(
            parsed,
            [
                ("Astra [400K]".to_string(), None),
                ("gpt-6-astra".to_string(), Some("Astra 6".to_string())),
                ("x".to_string(), None),
            ]
            .into()
        );
        assert!(parse_patch(br#"{}"#).unwrap().model_display_names.is_empty());
        let bidi = json!({"modelDisplayNames": {"a": format!("a{}b", char::from_u32(0x202e).unwrap())}}).to_string();
        for bad in [
            r#"{"modelDisplayNames":["a"]}"#,
            r#"{"modelDisplayNames":{"a":1}}"#,
            r#"{"modelDisplayNames":{" a":"x"}}"#,
            bidi.as_str(),
        ] {
            assert!(parse_patch(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn agent_turn_questions_patch() {
        assert_eq!(
            parse_patch(br#"{"agentTurnQuestions":"wait"}"#).unwrap().agent_turn_questions,
            ConfigValueUpdate::Set(AgentTurnQuestions::Wait)
        );
        assert_eq!(
            parse_patch(br#"{"agentTurnQuestions":"decide"}"#).unwrap().agent_turn_questions,
            ConfigValueUpdate::Set(AgentTurnQuestions::Decide)
        );
        assert_eq!(
            parse_patch(br#"{"agentTurnQuestions":null}"#).unwrap().agent_turn_questions,
            ConfigValueUpdate::Clear
        );
        assert_eq!(
            parse_patch(br#"{}"#).unwrap().agent_turn_questions,
            ConfigValueUpdate::Unchanged
        );
        for bad in [r#"{"agentTurnQuestions":"ask"}"#, r#"{"agentTurnQuestions":true}"#] {
            assert!(parse_patch(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn plan_label_patch() {
        assert_eq!(
            parse_patch(br#"{"planLabel":" GLM Pro "}"#).unwrap().plan_label,
            ConfigValueUpdate::Set("GLM Pro".into())
        );
        assert_eq!(parse_patch(br#"{"planLabel":null}"#).unwrap().plan_label, ConfigValueUpdate::Clear);
        assert_eq!(parse_patch(br#"{}"#).unwrap().plan_label, ConfigValueUpdate::Unchanged);
        let forty = format!(r#"{{"planLabel":"{}"}}"#, "套".repeat(40));
        assert!(parse_patch(forty.as_bytes()).is_ok());
        let long = format!(r#"{{"planLabel":"{}"}}"#, "a".repeat(41));
        for bad in [long.as_str(), r#"{"planLabel":""}"#, r#"{"planLabel":" "}"#, r#"{"planLabel":1}"#,
                    r#"{"planLabel":"a\nb"}"#] {
            assert!(parse_patch(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn api_key_patch_error_and_debug_never_echo_secret() {
        let patch = parse_patch(br#"{"apiKey":"sk-very-secret"}"#).unwrap();
        assert!(!format!("{patch:?}").contains("sk-very-secret"));
        let error = parse_patch(br#"{"apiKey":"sk-very secret"}"#).unwrap_err();
        assert!(!error.contains("sk-very"));
    }

    /// One request through `handle` on a real socket; `remote` = accepted on
    /// a non-loopback listener. Returns (status line + headers + body).
    fn call(method: &str, path: &str, body: &str, remote: bool) -> (u16, Value) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            use std::io::Read;
            let mut stream = std::net::TcpStream::connect(address).unwrap();
            let mut bytes = Vec::new();
            stream.read_to_end(&mut bytes).unwrap();
            // Image bodies are binary.
            String::from_utf8_lossy(&bytes).into_owned()
        });
        let (mut stream, peer) = listener.accept().unwrap();
        crate::management::listeners::set_current_connection(Some(
            crate::management::listeners::ConnectionInfo { remote, peer },
        ));
        let request = SimpleHttpRequest {
            method: method.into(),
            path: path.into(),
            headers: Default::default(),
            body: body.as_bytes().to_vec(),
        };
        handle(&mut stream, &request, path).unwrap();
        crate::management::listeners::set_current_connection(None);
        drop(stream);
        let response = client.join().unwrap();
        let status = response[9..12].parse().unwrap();
        let json = response.split_once("\r\n\r\n").map(|(_, body)| body).unwrap_or_default();
        (status, serde_json::from_str(json).unwrap_or(Value::Null))
    }

    fn audit_text() -> String {
        let path = crate::config::paths::runtime_dir().unwrap().join(transfer::AUDIT_DIR).join("audit.jsonl");
        std::fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn transfer_routes_loopback_gate_round_trip_and_audit_without_values() {
        const KEY: &str = "route-test-placeholder-key";
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let created = call(
            "POST",
            "/v2/profiles",
            &json!({ "name": "GLM", "kind": "apiKey", "preset": "glm", "apiKey": KEY }).to_string(),
            false,
        );
        assert_eq!(created.0, 201, "{created:?}");

        // Credentials over a remote (tailnet / LAN) listener: refused.
        let (status, body) = call("POST", "/v2/profiles/GLM/export", r#"{"includeCredentials":true}"#, true);
        assert_eq!(status, 403, "{body}");
        assert_eq!(body["error"]["code"], "credentials_loopback_only", "{body}");
        // Settings only over the same listener: allowed, no key.
        let (status, remote) = call("POST", "/v2/profiles/GLM/export", "", true);
        assert_eq!(status, 200, "{remote}");
        assert_eq!(remote["credentialsIncluded"], false);
        assert!(!remote.to_string().contains(KEY));
        // GET is not a thing (the bundle must not be cached or logged).
        assert_eq!(call("GET", "/v2/profiles/GLM/export", "", false).0, 405);

        // Loopback (this machine / SSH tunnel): the key travels.
        let (status, local) = call("POST", "/v2/profiles/GLM/export", r#"{"includeCredentials":true}"#, false);
        assert_eq!(status, 200, "{local}");
        assert_eq!(local["credentialsIncluded"], true);
        assert_eq!(local["bundle"]["credentials"]["apiKey"], KEY);

        // Import: clash refused, new name created with warnings.
        let import = |name: &str, bundle: &Value| {
            call("POST", IMPORT_PATH, &json!({ "bundle": bundle, "name": name }).to_string(), false)
        };
        let (status, clash) = import("GLM", &local["bundle"]);
        assert_eq!(status, 409, "{clash}");
        assert_eq!(clash["error"]["code"], "profile_name_conflict");
        assert!(!clash.to_string().contains(KEY));
        let (status, copied) = import("GLM (other host)", &local["bundle"]);
        assert_eq!(status, 201, "{copied}");
        assert_eq!((copied["imported"].as_bool(), copied["credentialsCopied"].as_bool()), (Some(true), Some(true)));
        assert_eq!(copied["warnings"], json!([]));
        assert_eq!(copied["profile"]["name"], "GLM (other host)");
        assert!(!copied.to_string().contains(KEY), "import response is secret-free");
        let (status, plain) = import("GLM 3", &remote["bundle"]);
        assert_eq!(status, 201);
        assert_eq!(plain["warnings"][0]["code"], "needsLogin");
        let mut newer = local["bundle"].clone();
        newer["schemaVersion"] = json!(9);
        let (status, refused) = import("GLM 4", &newer);
        assert_eq!((status, refused["error"]["code"].as_str()), (400, Some("unsupported_bundle")));
        assert!(!refused.to_string().contains(KEY));
        // A profile literally named "import" stays addressable by GET.
        assert_eq!(call("GET", IMPORT_PATH, "", false).0, 404);

        let audit = audit_text();
        assert!(!audit.contains(KEY), "audit holds no values: {audit}");
        assert_eq!(audit.matches("\"profileExport\"").count(), 2, "{audit}");
        assert_eq!(audit.matches("\"profileExportRefused\"").count(), 1, "{audit}");
        assert_eq!(audit.matches("\"profileImport\"").count(), 2, "{audit}");
        assert!(audit.contains("\"credentialsIncluded\":true"), "{audit}");
        // Nor the diagnostic error log.
        let errors = crate::config::paths::runtime_dir().unwrap().join("service-logs/management-errors.jsonl");
        assert!(!std::fs::read_to_string(errors).unwrap_or_default().contains(KEY));
    }

    fn png_data(width: u32, height: u32) -> String {
        use base64::Engine;
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(width, height).write_to(&mut out, image::ImageFormat::Png).unwrap();
        base64::engine::general_purpose::STANDARD.encode(out.into_inner())
    }

    #[test]
    fn avatar_style_conditional_import_requires_boolean_and_explicit_value() {
        for body in [
            r#"{"avatarStyleImportIfUnset":true}"#,
            r#"{"avatarStyleImportIfUnset":null,"avatarStyle":null}"#,
            r#"{"avatarStyleImportIfUnset":"true","avatarStyle":null}"#,
            r#"{"avatarStyleImportIfUnset":1,"avatarStyle":null}"#,
        ] {
            assert!(parse_patch(body.as_bytes()).is_err(), "{body}");
        }
        assert!(!parse_patch(br#"{"avatarStyleImportIfUnset":false}"#).unwrap().avatar_style_import_if_unset);
        assert!(parse_patch(br#"{"avatarStyleImportIfUnset":true,"avatarStyle":null}"#).unwrap().avatar_style_import_if_unset);
    }

    #[test]
    fn stale_avatar_import_cannot_overwrite_owner_choice_or_rename() {
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        for (name, owner_choice) in [("OwnerSave", json!({"kind":"letter"})), ("OwnerClear", Value::Null)] {
            let (status, _) = call("POST", "/v2/profiles", &json!({"name":name,"kind":"apiKey","preset":"glm"}).to_string(), false);
            assert_eq!(status, 201);
            let path = format!("/v2/profiles/{name}");
            let (_, before) = call("GET", &path, "", false);
            assert!(before["profile"]["appearance"]["avatarStyle"].is_null());
            let (status, owner) = call("PATCH", &path, &json!({"avatarStyle":owner_choice}).to_string(), false);
            assert_eq!(status, 200, "{owner}");
            let accounts_path = crate::config::paths::accounts_path().unwrap();
            let diagnostic_path = crate::config::paths::runtime_dir().unwrap().join("service-logs/management-errors.jsonl");
            let diagnostic_before = std::fs::read(&diagnostic_path).ok();
            let bytes = std::fs::read(&accounts_path).unwrap();
            let modified = std::fs::metadata(&accounts_path).unwrap().modified().unwrap();
            let (status, rejected) = call("PATCH", &path, &json!({
                "name":format!("{name}Renamed"), "color":"#abcdef",
                "avatarStyle":{"kind":"emoji","emojiThemes":null}, "avatarStyleImportIfUnset":true,
            }).to_string(), false);
            assert_eq!(status, 409, "{rejected}");
            assert_eq!(rejected["error"]["code"], "avatar_style_already_set");
            assert_eq!(rejected["error"]["details"]["avatarStyle"], owner_choice);
            assert_eq!(rejected["error"]["details"]["appearanceRevision"], owner["profile"]["appearanceRevisions"]["avatarStyle"]);
            assert_eq!(std::fs::read(&accounts_path).unwrap(), bytes);
            assert_eq!(std::fs::metadata(&accounts_path).unwrap().modified().unwrap(), modified);
            assert_eq!(std::fs::read(&diagnostic_path).ok(), diagnostic_before);
            let (status, current) = call("GET", &path, "", false);
            assert_eq!(status, 200);
            assert_eq!(current["profile"]["name"], name);
        }
    }

    #[test]
    fn null_avatar_import_claims_clock_and_combines_with_other_patch_fields() {
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let (status, _) = call("POST", "/v2/profiles", r#"{"name":"ImportDefault","kind":"apiKey","preset":"glm"}"#, false);
        assert_eq!(status, 201);
        let (status, saved) = call("PATCH", "/v2/profiles/ImportDefault", r##"{"name":"ImportedDefault","color":"#abcdef","avatarStyle":null,"avatarStyleImportIfUnset":true}"##, false);
        assert_eq!(status, 200, "{saved}");
        assert_eq!(saved["profile"]["name"], "ImportedDefault");
        assert_eq!(saved["profile"]["appearance"]["color"], "#ABCDEF");
        assert!(saved["profile"]["appearanceRevisions"]["avatarStyle"]["updatedAt"].is_string());
        let (status, rejected) = call("PATCH", "/v2/profiles/ImportedDefault", r#"{"avatarStyle":null,"avatarStyleImportIfUnset":true}"#, false);
        assert_eq!(status, 409, "{rejected}");
    }

    #[test]
    fn avatar_style_patch_is_strict_and_tri_state() {
        use crate::profiles::avatar::AvatarStyle;
        assert_eq!(parse_patch(br#"{}"#).unwrap().avatar_style, ConfigValueUpdate::Unchanged);
        assert_eq!(parse_patch(br#"{"avatarStyle":null}"#).unwrap().avatar_style, ConfigValueUpdate::Clear);
        assert_eq!(parse_patch(br#"{"avatarStyle":{"kind":"letter"}}"#).unwrap().avatar_style, ConfigValueUpdate::Set(AvatarStyle::Letter));
        assert_eq!(parse_patch(br#"{"avatarStyle":{"kind":"emoji","emojiThemes":null}}"#).unwrap().avatar_style, ConfigValueUpdate::Set(AvatarStyle::Emoji { emoji_themes: None }));
        for style in [json!({"kind":"emoji"}), json!({"kind":"portrait","portraitThemes":[]}), json!({"kind":"letter","extra":1}), json!("letter"), json!({"kind":"unknown"})] {
            let body = json!({"avatarStyle":style}).to_string();
            assert!(parse_patch(body.as_bytes()).unwrap_err().starts_with("avatarStyle:"), "{body}");
        }
    }

    #[test]
    fn profile_avatar_style_routes_round_trip_keep_sessions_and_legacy_choices() {
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let create = |name: &str| call("POST", "/v2/profiles", &json!({"name":name,"kind":"apiKey","preset":"glm","apiKey":"synthetic-style-fixture"}).to_string(), false);
        let (status, created) = create("Style");
        assert_eq!(status, 201, "{created}");
        let id = created["profile"]["id"].as_str().unwrap().to_string();
        assert!(created["profile"]["appearance"]["avatarStyle"].is_null());
        assert_eq!(created["profile"]["detail"]["editable"]["avatarStyle"], true);

        // An actual fixed session must survive every Profile style save unchanged.
        let mut sessions = crate::session::model::CutexSessionStore::default();
        let session_id = format!("cutex.{}", uuid::Uuid::new_v4());
        let mut session = crate::session::model::CutexSessionRecord::new(session_id.clone(), None, "synthetic-host".into(), crate::config::paths::config_dir().unwrap().to_string_lossy().into_owned(), Some("Style".into())).unwrap();
        session.avatar = Some(serde_json::from_value(json!({"kind":"emoji","value":"🐱"})).unwrap());
        sessions.sessions.insert(session_id, session);
        crate::session::store::save_cutex_session_store(&sessions).unwrap();
        let before_sessions = serde_json::to_value(crate::session::store::load_cutex_session_store().unwrap()).unwrap();
        for (requested, expected) in [
            (json!({"kind":"portrait","portraitThemes":["uninstalled/theme"]}), json!({"kind":"portrait","portraitThemes":["uninstalled/theme"]})),
            (json!({"kind":"emoji","emojiThemes":["cats","sea"]}), json!({"kind":"emoji","emojiThemes":["sea","cats"]})),
            (json!({"kind":"emoji","emojiThemes":null}), json!({"kind":"emoji","emojiThemes":null})),
            (json!({"kind":"letter"}), json!({"kind":"letter"})),
        ] {
            let body = json!({"avatarStyle":requested}).to_string();
            let (status, saved) = call("PATCH", "/v2/profiles/Style", &body, false);
            assert_eq!(status, 200, "{saved}");
            assert_eq!(saved["profile"]["appearance"]["avatarStyle"], expected);
            assert!(saved["profile"]["appearance"]["avatarThemes"].is_null(), "no automatic emoji mirror");
            let (status, detail) = call("GET", &format!("/v2/profiles/{id}"), "", true);
            assert_eq!(status, 200, "{detail}");
            assert_eq!(detail["profile"]["appearance"]["avatarStyle"], expected);
            let (status, list) = call("GET", "/v2/profiles", "", true);
            assert_eq!(status, 200);
            let row = list["profiles"].as_array().unwrap().iter().find(|row| row["id"] == id).unwrap();
            assert_eq!(row["appearance"]["avatarStyle"], expected);
            let (_, unchanged) = call("PATCH", "/v2/profiles/Style", "{}", false);
            assert_eq!(unchanged["changed"], false);
            assert_eq!(unchanged["profile"]["appearance"]["avatarStyle"], expected);
            assert_eq!(serde_json::to_value(crate::session::store::load_cutex_session_store().unwrap()).unwrap(), before_sessions);
        }
        let (_, legacy) = call("PATCH", "/v2/profiles/Style", r#"{"avatarThemes":["cats"]}"#, false);
        assert_eq!(legacy["profile"]["appearance"]["avatarStyle"], json!({"kind":"letter"}));
        assert_eq!(legacy["profile"]["appearance"]["avatarThemes"], json!(["cats"]));
        let (_, cleared) = call("PATCH", "/v2/profiles/Style", r#"{"avatarStyle":null}"#, false);
        assert!(cleared["profile"]["appearance"]["avatarStyle"].is_null());
        assert_eq!(cleared["profile"]["appearance"]["avatarThemes"], json!(["cats"]));
        assert_eq!(serde_json::to_value(crate::session::store::load_cutex_session_store().unwrap()).unwrap(), before_sessions);
        let (status, failed) = call("PATCH", "/v2/profiles/Style", r#"{"name":"BadRename","avatarStyle":{"kind":"letter","extra":true}}"#, false);
        assert_eq!(status, 400, "{failed}");
        assert_eq!(call("GET", "/v2/profiles/Style", "", false).0, 200);
        assert_eq!(call("GET", "/v2/profiles/BadRename", "", false).0, 404);
        let (_, renamed) = call("PATCH", "/v2/profiles/Style", r#"{"name":"Renamed","avatarStyle":{"kind":"portrait","portraitThemes":["future-pack/theme"]}}"#, false);
        assert_eq!(renamed["profile"]["id"], id);
        assert_eq!(renamed["profile"]["appearance"]["avatarStyle"]["kind"], "portrait");
        assert_eq!(call("DELETE", "/v2/profiles/Renamed", "", false).0, 200);
        let (status, recreated) = create("Renamed");
        assert_eq!(status, 201, "{recreated}");
        assert_ne!(recreated["profile"]["id"], id);
        assert!(recreated["profile"]["appearance"]["avatarStyle"].is_null());
    }

    #[test]
    fn profile_avatar_routes_round_trip_rename_and_delete() {
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let created = call(
            "POST",
            "/v2/profiles",
            &json!({ "name": "Pic", "kind": "apiKey", "preset": "glm", "apiKey": "placeholder" }).to_string(),
            false,
        );
        assert_eq!(created.0, 201, "{created:?}");
        let id = created.1["profile"]["id"].as_str().unwrap().to_string();
        assert_eq!(created.1["profile"]["avatarEtag"], Value::Null);
        assert_eq!(created.1["profile"]["introPortraitEtag"], Value::Null);
        assert_eq!(created.1["profile"]["appearance"]["avatarThemes"], Value::Null);
        assert_eq!(created.1["profile"]["detail"]["avatarThemeOptions"][0], "animals");

        assert_eq!(call("GET", "/v2/profiles/Pic/avatar", "", false).0, 404);
        let avatar = json!({ "contentType": "image/png", "data": png_data(64, 64) }).to_string();
        let (status, put) = call("PUT", "/v2/profiles/Pic/avatar", &avatar, false);
        assert_eq!(status, 200, "{put}");
        assert_eq!(put["profileId"], id.as_str());
        let etag = put["avatarEtag"].as_str().unwrap().to_string();
        // Refusals: not square, wrong type, not JSON, unknown profile.
        let (status, bad) = call("PUT", "/v2/profiles/Pic/avatar", &json!({ "contentType": "image/png", "data": png_data(64, 32) }).to_string(), false);
        assert_eq!((status, bad["error"]["code"].as_str()), (400, Some("invalid_avatar")), "{bad}");
        let (status, bad) = call("PUT", "/v2/profiles/Pic/avatar", &json!({ "contentType": "image/gif", "data": png_data(64, 64) }).to_string(), false);
        assert_eq!((status, bad["error"]["code"].as_str()), (400, Some("invalid_avatar")), "{bad}");
        assert_eq!(call("PUT", "/v2/profiles/Pic/avatar", "nope", false).0, 400);
        assert_eq!(call("PUT", "/v2/profiles/Nobody/avatar", &avatar, false).0, 404);
        assert_eq!(call("POST", "/v2/profiles/Pic/avatar", &avatar, false).0, 405);

        // The intro portrait: any aspect, its own ETag.
        let portrait = json!({ "contentType": "image/png", "data": png_data(300, 500) }).to_string();
        let (status, put) = call("PUT", "/v2/profiles/Pic/intro-portrait", &portrait, false);
        assert_eq!(status, 200, "{put}");
        let portrait_etag = put["introPortraitEtag"].as_str().unwrap().to_string();
        assert_eq!(call("GET", &format!("/v2/profiles/{id}/intro-portrait"), "", false).0, 200);

        // Themes + greeting.
        let (status, patched) = call("PATCH", "/v2/profiles/Pic", r#"{"avatarThemes":["cats","sea"],"introGreeting":"  Hi {{owner}} "}"#, false);
        assert_eq!(status, 200, "{patched}");
        assert_eq!(patched["profile"]["appearance"]["avatarThemes"], json!(["sea", "cats"]));
        assert_eq!(patched["profile"]["appearance"]["introGreeting"], "Hi {{owner}}");
        for bad in [r#"{"avatarThemes":[]}"#, r#"{"avatarThemes":["all"]}"#, r#"{"avatarThemes":["sea","sea"]}"#, r#"{"avatarThemes":"sea"}"#] {
            assert_eq!(call("PATCH", "/v2/profiles/Pic", bad, false).0, 400, "{bad}");
        }

        // Rename keeps everything (keyed by id).
        let (status, renamed) = call("PATCH", "/v2/profiles/Pic", r#"{"name":"Picture"}"#, false);
        assert_eq!(status, 200, "{renamed}");
        assert_eq!(renamed["profile"]["avatarEtag"], etag.as_str());
        assert_eq!(renamed["profile"]["introPortraitEtag"], portrait_etag.as_str());
        let (status, list) = call("GET", "/v2/profiles", "", false);
        assert_eq!(status, 200);
        let row = list["profiles"].as_array().unwrap().iter().find(|p| p["id"] == id.as_str()).unwrap().clone();
        assert_eq!(row["avatarEtag"], etag.as_str());
        assert_eq!(row["appearance"]["avatarThemes"], json!(["sea", "cats"]));
        assert_eq!(call("GET", "/v2/profiles/Picture/avatar", "", false).0, 200);

        // Clears.
        let (status, cleared) = call("PATCH", "/v2/profiles/Picture", r#"{"avatarThemes":null,"introGreeting":""}"#, false);
        assert_eq!(status, 200, "{cleared}");
        assert_eq!(cleared["profile"]["appearance"]["avatarThemes"], Value::Null);
        assert_eq!(cleared["profile"]["appearance"]["introGreeting"], Value::Null);
        let (status, deleted) = call("DELETE", "/v2/profiles/Picture/avatar", "", false);
        assert_eq!((status, &deleted["avatarEtag"]), (200, &Value::Null), "{deleted}");
        assert_eq!(call("GET", "/v2/profiles/Picture/avatar", "", false).0, 404);

        // Profile delete removes the intro portrait too.
        assert_eq!(call("DELETE", "/v2/profiles/Picture", "", false).0, 200);
        let images = profile_avatar::ProfileAvatarStore::open_default().unwrap().snapshot().unwrap();
        assert!(images.avatars.is_empty() && images.intro_portraits.is_empty());
    }

    #[test]
    fn account_links_round_trip_manual_undo_and_are_hidden_from_narrowed_devices() {
        use base64::Engine;
        use crate::management::{client_tokens::{DeviceScope,VerifiedClientToken},listeners};
        struct Reset;
        impl Drop for Reset {fn drop(&mut self){listeners::set_current_client_token(None);}}
        let _reset=Reset;
        let _home=crate::profiles::lifecycle::tests::TestHome::new();
        let (status,created)=call("POST","/v2/profiles", &json!({"name":"Linked","kind":"apiKey","preset":"glm","apiKey":"synthetic-profile-link-key"}).to_string(),false);
        assert_eq!(status,201,"{created}");
        assert!(created["profile"].get("link").is_none());
        let home=crate::config::paths::config_dir().unwrap();
        use sha2::Digest;
        crate::profiles::account_link::put_secret(&home,&base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(b"synthetic-profile-link-API-secret"))).unwrap();
        let automatic=call("GET","/v2/profiles/Linked","",false).1["profile"].clone();
        assert_eq!(automatic["link"]["basis"],"api-key");
        assert_eq!(automatic["accountLinkState"],"auto");
        assert_eq!(automatic["accountLink"],Value::Null);
        super::super::schema_export::assert_valid("profile-link","profileMetadata",&automatic);
        let manual=format!("m-{}",base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([42u8;16]));
        let (status,patched)=call("PATCH","/v2/profiles/Linked",&json!({"accountLink":manual}).to_string(),false);
        assert_eq!(status,200,"{patched}");assert_eq!(patched["profile"]["link"]["basis"],"manual");
        assert_eq!(patched["profile"]["accountLink"],manual);
        let mut token=VerifiedClientToken::test_token("narrowed-owner");
        for scope in [DeviceScope::Read,DeviceScope::Chat] {
            token.scope=scope;listeners::set_current_client_token(Some(token.clone()));
            let list=call("GET","/v2/profiles","",false).1;
            for item in list["profiles"].as_array().unwrap() {
                assert!(item.get("link").is_none());assert!(item.get("accountLinkState").is_none());assert!(item.get("accountLink").is_none());
            }
            let detail=call("GET","/v2/profiles/Linked","",false).1;
            assert!(detail["profile"].get("link").is_none());assert!(detail["profile"].get("accountLink").is_none());
        }
        listeners::set_current_client_token(None);
        let disabled=call("PATCH","/v2/profiles/Linked",r#"{"accountLink":"none"}"#,false).1;
        assert!(disabled["profile"].get("link").is_none());assert_eq!(disabled["profile"]["accountLinkState"],"disabled");
        let cleared=call("PATCH","/v2/profiles/Linked",r#"{"accountLink":null}"#,false).1;
        assert_eq!(cleared["profile"]["link"],automatic["link"]);
        assert_eq!(call("PATCH","/v2/profiles/Linked",r#"{"accountLink":"invalid"}"#,false).0,400);
        let restored=call("PATCH","/v2/profiles/Linked",&json!({"accountLink":manual}).to_string(),false).1;
        assert_eq!(restored["profile"]["link"],patched["profile"]["link"]);
        let account=settings_api::load_store().unwrap().accounts.into_iter().find(|a|a.name=="Linked").unwrap();
        let generic=settings_api::profile_item(&account,&load_codez_config(),true);
        assert!(generic.get("link").is_none());assert!(generic.get("accountLink").is_none());
    }

    #[test]
    fn image_paths_parse() {
        assert_eq!(parse_image_path("/v2/profiles/a%20b/avatar"), Some(("a b".into(), ImageKind::Avatar)));
        assert_eq!(parse_image_path("/v2/profiles/x/intro-portrait"), Some(("x".into(), ImageKind::IntroPortrait)));
        assert_eq!(parse_image_path("/v2/profiles/avatar"), None);
        assert_eq!(parse_image_path("/v2/profiles/a/b/avatar"), None);
        assert!(is_profiles_path("/v2/profiles/x/avatar"));
    }

    #[test]
    fn transfer_bodies_are_strict() {
        assert!(!parse_export_body(b"").unwrap());
        assert!(!parse_export_body(b"{}").unwrap());
        assert!(parse_export_body(br#"{"includeCredentials":true}"#).unwrap());
        assert!(parse_export_body(br#"{"includeCredentials":"yes"}"#).is_err());
        assert!(parse_export_body(br#"{"other":1}"#).is_err());
        assert_eq!(parse_export_path("/v2/profiles/my%20glm/export"), Some("my glm".into()));
        assert!(is_profiles_path("/v2/profiles/GLM/export") && is_profiles_path(IMPORT_PATH));

        let (_, name) = parse_import_body(br#"{"bundle":{},"name":" new ","onConflict":"fail"}"#).unwrap();
        assert_eq!(name, "new");
        for bad in [
            r#"{"bundle":{}}"#,
            r#"{"name":"x"}"#,
            r#"{"bundle":[],"name":"x"}"#,
            r#"{"bundle":{},"name":"x","onConflict":"overwrite"}"#,
            r#"{"bundle":{},"name":"x","extra":1}"#,
        ] {
            assert!(parse_import_body(bad.as_bytes()).is_err(), "{bad}");
        }
        let error = parse_import_body(br#"{"bundle":{"credentials":{"apiKey":"sk-leak"}},"name":"x",}"#).unwrap_err();
        assert!(!error.contains("sk-leak"), "{error}");
    }
}
