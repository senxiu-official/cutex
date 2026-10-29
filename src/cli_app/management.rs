use std::fs;
use std::net::IpAddr;
use std::process::Command;
use std::process::Stdio;

use anyhow::Context;

use cutex::cli::args::{
    ManagementClientTokenCommand, ManagementCommand, ManagementOwnerCommand, ManagementUsersCommand,
};
#[cfg(windows)]
use cutex::config::env::{env_bool_override, CUTEX_WINDOWS_DESKTOP_LAUNCHER_ENV_VAR};
use cutex::config::store::load_codez_config;
use cutex::management::remote::{
    ensure_management_remote_tunnel, management_http_json, raw_management_ssh_tunnel_command,
};
use cutex::management::service::{
    management_api_token, management_base_url, management_health_local_url, management_health_url,
    management_root_credential, validate_management_port,
    DEFAULT_MANAGEMENT_PORT, DEFAULT_MANAGEMENT_REMOTE_TUNNEL_PORT, MANAGEMENT_BRIDGE_ID,
};
use cutex::platform::command::command_exists_in_path;
use cutex::platform::host::current_host_name;
use cutex::profiles::model::CodezConfig;

use super::agent_bus_config;
use super::agent_bus_runtime;
use super::app_server_runtime;
use super::management_context;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const GREEN: &str = "\x1b[32m";
const DIM: &str = "\x1b[2m";

pub(crate) fn run_command(command: ManagementCommand) -> anyhow::Result<()> {
    match command {
        ManagementCommand::Serve { port, bind, token } => {
            arm_windows_hosted_launcher_guard()?;
            cmd_management_serve(port, &bind, token)
        }
        ManagementCommand::Pair { label, user, scope, port, json } => {
            cmd_management_pair(label.as_deref(), user.as_deref(), scope.as_deref(), port, json)
        }
        ManagementCommand::Schema {
            command: cutex::cli::args::ManagementSchemaCommand::Export { out },
        } => {
            for path in cutex::management::v2::schema_export::export(&out)? {
                println!("{}", path.display());
            }
            Ok(())
        }
        ManagementCommand::RemoteUp {
            host,
            service_id,
            local_port,
            remote_port,
            token,
            show_ssh_fallback,
        } => cmd_management_remote_up(
            &host,
            service_id.as_deref(),
            local_port,
            remote_port,
            token.as_deref(),
            show_ssh_fallback,
        ),
        ManagementCommand::ClientToken { command } => cmd_management_client_token(command),
        ManagementCommand::Users { command } => cmd_management_users(command),
        ManagementCommand::Owner { command } => cmd_management_owner(command),
        ManagementCommand::AgentAuthority {
            request_file,
            port,
            token,
        } => cmd_management_agent_authority(&request_file, port, token.as_deref()),
        ManagementCommand::AgentOwnershipImport {
            request_file,
            port,
            token,
        } => cmd_management_agent_ownership_import(&request_file, port, token.as_deref()),
        ManagementCommand::AgentReservationReconcile {
            request_file,
            port,
            token,
        } => cmd_management_agent_reservation_reconcile(&request_file, port, token.as_deref()),
        ManagementCommand::AgentCreateAbandon { action_id, dry_run, port, token } => {
            cmd_management_agent_create_abandon(&action_id, dry_run, port, token.as_deref())
        }
    }
}

/// `cutex management client-token issue|list|revoke`: edits the token store
/// directly (no daemon needed); a running daemon picks changes up on its next
/// request. Output is JSON on stdout.
fn cmd_management_client_token(command: ManagementClientTokenCommand) -> anyhow::Result<()> {
    use cutex::management::client_tokens::ClientTokenStore;
    let store = ClientTokenStore::open_default()?;
    let output = match command {
        ManagementClientTokenCommand::Issue { label, user, pending, scope } => {
            if management_root_credential(&load_codez_config(), None).is_err() {
                eprintln!(
                    "warning: no Management root credential (management_api_token) is configured; \
                     client tokens are refused until one is set"
                );
            }
            let scope = scope
                .as_deref()
                .map(|text| {
                    cutex::management::client_tokens::DeviceScope::parse(text)
                        .ok_or_else(|| anyhow::anyhow!("--scope must be full, chat or read"))
                })
                .transpose()?
                .unwrap_or_default();
            let value = issue_client_token(&store, &label, user, pending, scope)?;
            if value["pending"] == true {
                eprintln!(
                    "note: this token is pending: it connects but sees nothing until the host owner \
                     assigns it (Settings → Users on an owner device, or `cutex management users assign`)"
                );
            }
            serde_json::to_string(&value)?
        }
        ManagementClientTokenCommand::List => {
            serde_json::to_string(&serde_json::json!({ "tokens": store.list()? }))?
        }
        ManagementClientTokenCommand::Revoke { token_id } => {
            if !store.revoke(&token_id)? {
                anyhow::bail!("no client token with id {token_id}");
            }
            serde_json::to_string(&serde_json::json!({ "tokenId": token_id, "revoked": true }))?
        }
    };
    println!("{output}");
    Ok(())
}

/// `cutex management client-token issue` without the warnings: issue,
/// bind, and the printed JSON `{tokenId, label, token, createdAt, userId
/// (string|null), pending, scope}`. A token bound to the owner is a new
/// owner device (`via: "ssh"`, owner-device-notice): the daemon announces it
/// on its next request.
fn issue_client_token(
    store: &cutex::management::client_tokens::ClientTokenStore,
    label: &str,
    user: Option<String>,
    pending: bool,
    scope: cutex::management::client_tokens::DeviceScope,
) -> anyhow::Result<serde_json::Value> {
    let users = cutex::management::users::UserStore::beside(store);
    // Decide the binding before issuing: the new token must not count
    // as the host's first owner device.
    let binding = match (user, pending) {
        (_, true) => None,
        (Some(user), false) => {
            let known = users.snapshot()?.user(&user).is_some();
            if !known {
                anyhow::bail!("no user with id {user}");
            }
            Some(user)
        }
        (None, false) => users
            .default_cli_binding()
            .map_err(|error| anyhow::anyhow!(error.message))?,
    };
    let issued = store.issue_scoped(label, scope)?;
    if let Some(user) = &binding {
        if let Err(error) = users.bind_token(&issued.token_id, Some(user), "cli") {
            let _ = store.revoke(&issued.token_id);
            anyhow::bail!("could not bind the new token: {}", error.message);
        }
    }
    // Order: tokenId, label, token, createdAt, userId, pending, scope.
    let mut value = serde_json::to_value(&issued)?;
    value["userId"] = serde_json::json!(binding);
    value["pending"] = serde_json::json!(binding.is_none());
    value["scope"] = serde_json::json!(scope);
    Ok(value)
}

/// `POST /v2/pairing-codes` body of `cutex management pair`.
fn pair_request_body(label: Option<&str>, user: Option<&str>, scope: Option<&str>) -> serde_json::Value {
    let mut body = serde_json::json!({ "label": label });
    if let Some(user) = user {
        body["userId"] = serde_json::json!(user);
    }
    if let Some(scope) = scope {
        body["scope"] = serde_json::json!(scope);
    }
    body
}

/// `cutex management owner show|set-person|clear-person`: edits the
/// persons store directly (no daemon needed; local account access is the
/// authority, as for `client-token issue`). Source `local`, actor `cli`.
fn cmd_management_owner(command: ManagementOwnerCommand) -> anyhow::Result<()> {
    use cutex::management::client_tokens::ClientTokenStore;
    use cutex::management::users::UserStore;
    let tokens = ClientTokenStore::open_default()?;
    let users = UserStore::beside(&tokens);
    let (value, json) = owner_command(&users, command)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        print!("{}", owner_text(&value));
    }
    Ok(())
}

/// The work of `cutex management owner …` → (output, print as JSON).
/// Output: `{owner: <GET /v2/host owner>, publicKey, ownerKeys: [{publicKey,
/// fingerprint, addedAt, label, ownerPerson}]}`.
fn owner_command(
    users: &cutex::management::users::UserStore,
    command: ManagementOwnerCommand,
) -> anyhow::Result<(serde_json::Value, bool)> {
    use cutex::management::persons;
    let message = |error: cutex::management::users::UserError| anyhow::anyhow!(error.message);
    let json = match command {
        ManagementOwnerCommand::Show { json } => json,
        ManagementOwnerCommand::SetPerson { key } => {
            let state = users.persons_snapshot()?;
            let Some(public_key) = persons::resolve_owner_key(&state, &key) else {
                anyhow::bail!(
                    "{key} is not one of the owner's person keys (see `cutex management owner show`)"
                );
            };
            users.set_owner_person(&public_key, "cli").map_err(message)?;
            false
        }
        ManagementOwnerCommand::ClearPerson => {
            users.clear_owner_person("cli").map_err(message)?;
            false
        }
    };
    let state = users.persons_snapshot()?;
    let person = users.owner_person().map(|(key, _)| key);
    let keys: Vec<serde_json::Value> = state
        .keys_of(cutex::management::users::OWNER)
        .iter()
        .map(|key| {
            let mut view = key.view();
            view["ownerPerson"] = serde_json::json!(person.as_deref() == Some(key.public_key.as_str()));
            view
        })
        .collect();
    Ok((
        serde_json::json!({ "owner": users.owner_view(), "publicKey": person, "ownerKeys": keys }),
        json,
    ))
}

fn owner_text(value: &serde_json::Value) -> String {
    let owner = &value["owner"];
    let mut text = format!(
        "Owner:        {}\n",
        owner["displayName"].as_str().unwrap_or("(no name)")
    );
    if owner["confirmed"] == true {
        text.push_str(&format!(
            "Owner person: {} (source {})\n",
            owner["fingerprint"].as_str().unwrap_or_default(),
            owner["source"].as_str().unwrap_or_default()
        ));
    } else {
        text.push_str("Owner person: unconfirmed\n");
    }
    let keys = value["ownerKeys"].as_array().cloned().unwrap_or_default();
    if keys.is_empty() {
        text.push_str("Owner keys:   none (a desktop adds its key when it connects)\n");
    } else {
        text.push_str("Owner keys:\n");
        for key in keys {
            text.push_str(&format!(
                "  {} {}  added {}{}\n",
                if key["ownerPerson"] == true { "*" } else { " " },
                key["fingerprint"].as_str().unwrap_or_default(),
                key["addedAt"].as_str().unwrap_or_default(),
                key["label"].as_str().map(|label| format!("  {label}")).unwrap_or_default()
            ));
        }
    }
    text
}

/// `cutex management users list|assign|rollback-check`: edits the users
/// store directly (no daemon needed). Output is JSON on stdout.
fn cmd_management_users(command: ManagementUsersCommand) -> anyhow::Result<()> {
    use cutex::management::client_tokens::ClientTokenStore;
    use cutex::management::users::{UserStore, OWNER};
    let tokens = ClientTokenStore::open_default()?;
    let users = UserStore::beside(&tokens);
    let message = |error: cutex::management::users::UserError| anyhow::anyhow!(error.message);
    match command {
        ManagementUsersCommand::List => {
            let value = cutex::management::v2::users_api::listing(&users).map_err(message)?;
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        ManagementUsersCommand::Assign { token_id, user } => {
            let target = (user != "pending").then_some(user.as_str());
            users.bind_token(&token_id, target, "cli").map_err(message)?;
            println!("{}", serde_json::json!({ "tokenId": token_id, "userId": target, "pending": target.is_none() }));
        }
        ManagementUsersCommand::RollbackCheck { revoke } => {
            let state = users.snapshot()?;
            let risky: Vec<_> = tokens
                .list()?
                .into_iter()
                .filter(|token| state.bindings.get(&token.token_id).map(String::as_str) != Some(OWNER))
                .collect();
            let peers: Vec<String> = cutex::peers::store::Store::open()
                .and_then(|store| store.snapshot())
                .map(|state| {
                    state
                        .peers
                        .values()
                        .filter(|peer| peer.user_id != OWNER)
                        .map(|peer| peer.peer_id.clone())
                        .collect()
                })
                .unwrap_or_default();
            let mut revoked = Vec::new();
            if revoke {
                for token in &risky {
                    if tokens.revoke(&token.token_id)? {
                        revoked.push(token.token_id.clone());
                    }
                }
            }
            let remaining_tokens = risky.len() - revoked.len();
            println!(
                "{}",
                serde_json::json!({
                    "nonOwnerTokens": risky.iter().map(|t| serde_json::json!({
                        "tokenId": t.token_id, "label": t.label,
                        "userId": state.bindings.get(&t.token_id),
                    })).collect::<Vec<_>>(),
                    "revoked": revoked,
                    "nonOwnerPeers": peers,
                    "readyForRollback": remaining_tokens == 0 && peers.is_empty(),
                })
            );
            if remaining_tokens != 0 || !peers.is_empty() {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}

fn arm_windows_hosted_launcher_guard() -> anyhow::Result<()> {
    #[cfg(windows)]
    if env_bool_override(CUTEX_WINDOWS_DESKTOP_LAUNCHER_ENV_VAR) == Some(true) {
        cutex::platform::windows_parent_guard::arm_launcher_exit_guard()?;
    }
    Ok(())
}

/// `cutex [human] management agent-create-abandon <action_id> [--dry-run]`:
/// POST /v2/agent-management/create-abandon (root), prints the JSON reply.
fn cmd_management_agent_create_abandon(
    action_id: &str,
    dry_run: bool,
    port: Option<u16>,
    explicit_root_token: Option<&str>,
) -> anyhow::Result<()> {
    let request = cutex::agent_management::CreateAbandonRequest { action_id: action_id.to_string(), dry_run };
    let body = serde_json::to_vec(&request)?;
    let config = load_codez_config();
    let port = port.unwrap_or_else(|| cutex::management::service::management_port(&load_codez_config()));
    validate_management_port(port)?;
    let token = management_root_credential(&config, explicit_root_token)?;
    let response = management_http_json(
        &management_base_url(port),
        "POST",
        "/v2/agent-management/create-abandon",
        Some(token),
        Some(&body),
    )?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

fn cmd_management_agent_reservation_reconcile(
    request_file: &str,
    port: Option<u16>,
    explicit_root_token: Option<&str>,
) -> anyhow::Result<()> {
    let bytes = fs::read(request_file).with_context(|| {
        format!("Failed to read Agent reservation reconciliation request: {request_file}")
    })?;
    let request: cutex::agent_management::AgentReservationReconciliationRequest =
        serde_json::from_slice(&bytes)
            .context("Failed to parse strict Agent reservation reconciliation request")?;
    let body = serde_json::to_vec(&request)?;
    let config = load_codez_config();
    let port = port.unwrap_or_else(|| cutex::management::service::management_port(&load_codez_config()));
    validate_management_port(port)?;
    let token = management_root_credential(&config, explicit_root_token)?;
    let response = management_http_json(
        &management_base_url(port),
        "POST",
        "/v2/agent-management/reservation-reconciliation",
        Some(token),
        Some(&body),
    )?;
    println!("{}", serde_json::to_string(&response)?);
    Ok(())
}

fn cmd_management_agent_ownership_import(
    request_file: &str,
    port: Option<u16>,
    explicit_root_token: Option<&str>,
) -> anyhow::Result<()> {
    let bytes = fs::read(request_file).with_context(|| {
        format!("Failed to read legacy Director ownership import request: {request_file}")
    })?;
    let request: cutex::agent_management::LegacyDirectorOwnershipImportRequest =
        serde_json::from_slice(&bytes)
            .context("Failed to parse strict legacy Director ownership import request")?;
    let body = serde_json::to_vec(&request)?;
    let config = load_codez_config();
    let port = port.unwrap_or_else(|| cutex::management::service::management_port(&load_codez_config()));
    validate_management_port(port)?;
    let token = management_root_credential(&config, explicit_root_token)?;
    let response = management_http_json(
        &management_base_url(port),
        "POST",
        "/v2/agent-management/legacy-director-ownership-import",
        Some(token),
        Some(&body),
    )?;
    println!("{}", serde_json::to_string(&response)?);
    Ok(())
}

fn cmd_management_agent_authority(
    request_file: &str,
    port: Option<u16>,
    explicit_root_token: Option<&str>,
) -> anyhow::Result<()> {
    let bytes = fs::read(request_file)
        .with_context(|| format!("Failed to read project authority request: {request_file}"))?;
    let request: cutex::agent_management::ProjectAuthorityRequest = serde_json::from_slice(&bytes)
        .context("Failed to parse strict project authority request")?;
    let body = serde_json::to_vec(&request)?;
    let config = load_codez_config();
    let port = port.unwrap_or_else(|| cutex::management::service::management_port(&load_codez_config()));
    validate_management_port(port)?;
    let token = management_root_credential(&config, explicit_root_token)?;
    let response = management_http_json(
        &management_base_url(port),
        "POST",
        "/v2/agent-management/authority",
        Some(token),
        Some(&body),
    )?;
    println!("{}", serde_json::to_string(&response)?);
    Ok(())
}

/// `cutex management pair`: ask the local daemon (loopback, root credential)
/// for a pairing code and print it with a terminal QR code.
fn cmd_management_pair(
    label: Option<&str>,
    user: Option<&str>,
    scope: Option<&str>,
    port: Option<u16>,
    json: bool,
) -> anyhow::Result<()> {
    let config = load_codez_config();
    let port = port.unwrap_or_else(|| cutex::management::service::management_port(&load_codez_config()));
    validate_management_port(port)?;
    let root = management_root_credential(&config, None)
        .map_err(|error| anyhow::anyhow!("{error}; pairing needs management_api_token"))?;
    let body = serde_json::to_vec(&pair_request_body(label, user, scope))?;
    let response = management_http_json(
        &management_base_url(port),
        "POST",
        "/v2/pairing-codes",
        Some(root),
        Some(&body),
    )?;
    if json {
        println!("{}", serde_json::to_string(&response)?);
        return Ok(());
    }
    let code = response["code"].as_str().unwrap_or_default();
    let qr = response["qr"].as_str().unwrap_or_default();
    let urls: Vec<&str> = response["urls"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect();
    println!("{}", super::terminal_qr::render(qr)?);
    println!("Pairing code: {BOLD}{code}{RESET}");
    println!("Expires:      {}", response["expiresAt"].as_str().unwrap_or_default());
    if urls.is_empty() {
        eprintln!(
            "warning: no remote listener is up (set management_listen to include \"tailscale\" and restart the management service); the phone cannot reach this host yet"
        );
    } else {
        println!("Addresses:    {}", urls.join("  "));
    }
    println!("{DIM}Scan the QR code in the app, or enter an address and the code.{RESET}");
    Ok(())
}

/// Listen specs: explicit `--bind` values, else config `management_listen`,
/// else 127.0.0.1 (all on `port` unless an entry names its own).
pub(crate) fn management_listen_specs(
    bind: &[String],
    configured: Option<&[String]>,
    port: u16,
) -> anyhow::Result<Vec<cutex::management::listeners::ListenSpec>> {
    use cutex::management::listeners::ListenSpec;
    let from_config = bind.is_empty();
    let entries: Vec<String> = if !bind.is_empty() {
        bind.to_vec()
    } else if let Some(configured) = configured.filter(|list| !list.is_empty()) {
        configured.to_vec()
    } else {
        vec!["127.0.0.1".to_string()]
    };
    cutex::config::instance::check_listen_entries(&entries, port).map_err(anyhow::Error::msg)?;
    let mut specs = Vec::new();
    for entry in &entries {
        let spec = ListenSpec::parse(entry, port)?;
        validate_management_port(spec.port())?;
        if !specs.contains(&spec) {
            specs.push(spec);
        }
    }
    // The desktop and CLI reach the daemon over loopback: a configured list
    // without a loopback (or wildcard) entry still gets 127.0.0.1.
    if from_config
        && !specs.iter().any(|spec| {
            matches!(
                cutex::management::listeners::listen_entry_kind(spec),
                "loopback" | "wildcard"
            )
        })
    {
        eprintln!("\x1b[33mwarning:\x1b[0m management_listen has no loopback entry; also listening on 127.0.0.1:{port}");
        specs.insert(0, ListenSpec::parse("127.0.0.1", port)?);
    }
    Ok(specs)
}

pub(crate) fn cmd_management_serve(
    port: Option<u16>,
    bind: &[String],
    token: Option<String>,
) -> anyhow::Result<()> {
    #[cfg(windows)]
    cutex::platform::private_fs::secure_current_runtime_process()
        .context("Set service process access for the current user")?;
    // r219: archive state of the removed Task Service / Release rotation / seats
    // (docs/r219-removals.md §4) before anything opens it. Never fatal.
    let _migration_guard = cutex::peers::strip::daemon_guard()?;
    // Native events of runtimes other processes connected too are journaled
    // once: this process takes them over (app_server::single_writer).
    cutex::app_server::single_writer::claim_owner_role();
    cutex::r219_migration::run_on_startup("management serve");
    // Escaped Windows session cwds (`E:\\x\\y`) -> normalized. Never fatal.
    cutex::session::cwd_migration::run_on_startup("management serve");
    // r225: Claude profile write / exec rules default on (clears stored values once).
    cutex::profiles::claude_tool_defaults::run_on_startup("management serve");
    // im-exposure-retired: set the legacy IM flag on durable sessions once, so
    // older binaries after a rollback list the same agents. Never fatal.
    cutex::session::im_exposure_migration::run_on_startup("management serve");
    // profile-generic-switches: legacy private ChatGPT profile -> switch,
    // quota_preset auto -> explicit reader (converges every start).
    cutex::profiles::generic_switches::run_on_startup("management serve");
    // Design 028 (e): known-model catalog rows lose the values their provider
    // rejects (e.g. glm-5.3 `medium`); owner edits stay. Converges every start.
    cutex::profiles::catalog_api::correct_known_rows_on_startup("management serve");
    // 2026-10-15: notification presets `legacy` removed; absent / legacy
    // tables are stored as `default` once (record in archive/). Never fatal.
    cutex::config::notification_levels::run_legacy_migration_on_startup("management serve");
    let configured = load_codez_config();
    let config = agent_bus_config::ensure_agent_bus_config(true, configured.agent_bus_port)?;
    agent_bus_runtime::ensure_agent_bus_running(&config, true)?;
    let port = port.unwrap_or_else(|| cutex::management::service::management_port(&load_codez_config()));
    validate_management_port(port)?;
    cutex::management::listeners::set_default_listen_port(port);
    let listen = management_listen_specs(bind, config.management_listen.as_deref(), port)?;
    cutex::management::listeners::configure_cors_origins(config.management_cors_origins.clone());
    let agent_management_admin_token = management_root_credential(&config, token.as_deref())
        .ok()
        .map(str::to_string);
    let token = management_api_token(&config, token.as_deref()).map(str::to_string);
    // Cap malloc arenas and return freed arena memory to the OS (glibc keeps
    // per-thread peaks); before the worker threads below start.
    cutex::platform::heap::start_daemon_heap_maintenance();
    // r219: director_rotate actions an older release left in flight.
    super::agent_management::finish_removed_director_rotations_at_startup();
    // Before anything serves a client: clear the durable claims of runtimes
    // that are proven gone (a previous boot, exited, PID reused), so no
    // endpoint reports them from a stale record. Cheap evidence only; live
    // bindings are reconnected in the background below.
    match app_server_runtime::clear_dead_runtimes_at_startup(
        &current_host_name(),
        std::time::Duration::from_secs(10),
    ) {
        Ok(summary) => {
            if summary.cleared_stale > 0 || !summary.failures.is_empty() {
                println!(
                    "startup runtime reconciliation: cleared={} deferred={} failed={}",
                    summary.cleared_stale,
                    summary.skipped,
                    summary.failures.len()
                );
                for failure in summary.failures.iter().take(3) {
                    eprintln!("startup runtime reconciliation warning: {failure}");
                }
            }
        }
        Err(error) => eprintln!("startup runtime reconciliation warning: {error:#}"),
    }
    // Runtimes that were intended online but did not survive the previous
    // stop (reboot, crash): offer them for restore (never started here).
    cutex::session::runtime_intent::detect_unclean_stop_at_startup();
    // Owner-stop marks older releases left behind (their HTTP online did not
    // clear them): drop the ones the owner has since overridden.
    cutex::asks::wake::prune_stale_owner_marks_at_startup();
    // Recover existing runtime connections in the background. A slow native
    // peer must not keep HTTP health unavailable and trigger repeated auto-start.
    let recovery_config = config.clone();
    std::thread::Builder::new()
        .name("cutex-runtime-recovery".into())
        .spawn(move || {
            if let Err(error) = recover_runtime_connections(&recovery_config) {
                eprintln!("app-server runtime adoption warning: {error:#}");
            }
            // Runtimes other processes start from now on (Agent Management
            // runs in the Agent Bus service) are adopted as they appear.
            app_server_runtime::observe_late_runtimes(&current_host_name());
        })?;
    super::claude_runtime::spawn_observer()?;
    if let Err(error) = cutex::management::v2::journal_watch::start_watcher() {
        eprintln!("shared journal watcher warning: {error:#}");
    }
    cutex::management::v2::health::start_scan();
    // Asks (委托): admission scheduler and Management v2 ask events.
    if let Err(error) = super::asks_runtime::spawn_scheduler() {
        eprintln!("asks scheduler warning: {error:#}");
    }
    if let Err(error) = cutex::management::v2::services::start_supervisor() {
        eprintln!("services supervisor warning: {error:#}");
    }
    if let Err(error) = cutex::management::v2::alarms_api::start_watcher() {
        eprintln!("alarms watcher warning: {error:#}");
    }
    // Approvals requested in the Agent Bus process (services, agent
    // profiles) as live host events.
    if let Err(error) = cutex::management::v2::approvals::start_watcher() {
        eprintln!("approvals watcher warning: {error:#}");
    }
    // Session / project writes of other processes (agent_create runs in the
    // Agent Bus service) as a live host event.
    if let Err(error) = cutex::management::v2::sessions_watch::start_watcher() {
        eprintln!("sessions watcher warning: {error:#}");
    }
    if let Err(error) = cutex::management::v2::upgrade_api::start_watcher() {
        eprintln!("upgrade watcher warning: {error:#}");
    }
    #[cfg(target_os = "linux")]
    cutex::inbound_mail::start_worker();
    #[cfg(all(target_os = "linux", feature = "nci-qstat"))]
    cutex::nci_verification::start_worker();
    // Notification worker (state collector + webhook outbox): embedded for a
    // managed install, whose services start no separate `cutex notify run`.
    // The worker lock keeps a concurrently running `cutex notify run` and
    // this one from both delivering.
    if cutex::notify::outbound::embedded_worker_wanted(
        std::env::var(cutex::notify::outbound::EMBEDDED_WORKER_ENV).ok().as_deref(),
        cutex::upgrade::layout::Layout::detect().is_some(),
    ) {
        if let Err(error) = cutex::notify::outbound::spawn_embedded_worker() {
            eprintln!("notification worker warning: {error:#}");
        }
    }
    cutex::management::v2::profile_quota_native::start_refresher();
    cutex::management::v2::usage::catalog::start_refresher();
    cutex::management::v2::usage::external::start_scanner();
    cutex::management::v2::owner_rotation_api::start_driver(management_context::management_request_context());
    // Workflow runs (approvals, Agent Bus submissions, drivers, retention).
    cutex::management::v2::workflows_api::start(management_context::management_request_context());
    cutex::management::v2::prime_job_capabilities();
    run_management_server(&listen, token, agent_management_admin_token)
}

fn recover_runtime_connections(config: &CodezConfig) -> anyhow::Result<()> {
    let adoption = app_server_runtime::adopt_persisted_runtimes(config, &current_host_name())?;
    if adoption.adopted > 0
        || adoption.cleared_stale > 0
        || adoption.skipped > 0
        || !adoption.failures.is_empty()
    {
        println!(
            "app-server runtime adoption: adopted={} cleared_stale={} skipped={} failed={}",
            adoption.adopted,
            adoption.cleared_stale,
            adoption.skipped,
            adoption.failures.len()
        );
        for failure in adoption.failures.iter().take(3) {
            eprintln!("app-server adoption warning: {failure}");
        }
    }
    Ok(())
}

fn run_management_server(
    listen: &[cutex::management::listeners::ListenSpec],
    token: Option<String>,
    agent_management_admin_token: Option<String>,
) -> anyhow::Result<()> {
    cutex::management::server::run_management_server_on(
        listen,
        token,
        agent_management_admin_token,
        register_management_handoff,
        management_context::management_request_context(),
    )
}

fn register_management_handoff(bind_addr: IpAddr, port: u16) {
    // Never from an instance or a test process (test port guard).
    if cutex::config::instance::uses_isolated_ports() {
        return;
    }
    if !command_exists_in_path("bridgeboard") {
        return;
    }
    let owner_host = current_host_name().to_ascii_lowercase();
    let url = management_health_url(bind_addr, port);
    let _ = Command::new("bridgeboard")
        .arg("handoff")
        .arg("--id")
        .arg(MANAGEMENT_BRIDGE_ID)
        .arg("--title")
        .arg("cutex management API")
        .arg("--port")
        .arg(port.to_string())
        .arg("--owner-host")
        .arg(owner_host)
        .arg("--pid-from-port")
        .arg("--local-url")
        .arg(&url)
        .arg("--open-url")
        .arg(&url)
        .arg("--health-url")
        .arg(&url)
        .arg("--tunnel-mode")
        .arg("local_forward")
        .arg("--require-healthy")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub(crate) fn cmd_management_remote_up(
    host: &str,
    service_id: Option<&str>,
    local_port: Option<u16>,
    remote_port: Option<u16>,
    token: Option<&str>,
    show_ssh_fallback: bool,
) -> anyhow::Result<()> {
    cutex::config::instance::refuse_in_instance("`cutex management remote-up` (Bridgeboard tunnel)")?;
    let config = load_codez_config();
    let local_port = local_port.unwrap_or(DEFAULT_MANAGEMENT_REMOTE_TUNNEL_PORT);
    let remote_port = remote_port.unwrap_or(DEFAULT_MANAGEMENT_PORT);
    let service_id = service_id.unwrap_or(MANAGEMENT_BRIDGE_ID);
    let token = management_api_token(&config, token);
    ensure_management_remote_tunnel(host, service_id, local_port, remote_port, token)?;
    println!(
        "{GREEN}management tunnel ready{RESET}: service={BOLD}{service_id}{RESET} host={BOLD}{host}{RESET} local_url={BOLD}{}{RESET}",
        management_health_local_url(local_port)
    );
    if show_ssh_fallback {
        println!(
            "{DIM}SSH fallback:{RESET} {}",
            raw_management_ssh_tunnel_command(host, local_port, remote_port)
        );
    }
    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use clap::Parser;

    use cutex::cli::args::Cli;

    /// r219: seat administration went with the Task Service; the Director is
    /// the Agent Management project authority (`agent-authority`).
    #[test]
    fn seat_administration_command_was_removed() {
        for args in [
            &["cutex", "management", "seat", "query"][..],
            &[
                "cutex",
                "management",
                "seat",
                "bind",
                "--action-id",
                "bind-director-1",
                "--seat-id",
                "cutex-director",
                "--occupant-cutex-session",
                "cutex-session-director",
            ][..],
        ] {
            assert!(Cli::try_parse_from(args).is_err(), "{args:?}");
        }
        assert!(Cli::try_parse_from([
            "cutex",
            "management",
            "agent-authority",
            "--request-file",
            "authority.json",
        ])
        .is_ok());
    }

    fn temp_stores() -> (
        std::path::PathBuf,
        cutex::management::client_tokens::ClientTokenStore,
        cutex::management::users::UserStore,
    ) {
        let root = std::env::temp_dir().join(format!("cutex-cli-owner-{}", uuid::Uuid::new_v4()));
        let tokens =
            cutex::management::client_tokens::ClientTokenStore::at(root.join("management-clients"));
        let users = cutex::management::users::UserStore::beside(&tokens);
        (root, tokens, users)
    }

    fn key(seed: u8) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([seed; 32])
    }

    fn root_claim(users: &cutex::management::users::UserStore, key: &str) {
        let claim = cutex::management::persons::VerifiedClaim {
            token_id: None,
            public_key: key.to_string(),
            display_name: None,
        };
        users.apply_claim(&claim, "root").unwrap();
    }

    #[test]
    fn client_token_issue_prints_user_and_pending() {
        let (root, tokens, _) = temp_stores();
        // The host's first device: the owner's.
        let first = super::issue_client_token(&tokens, "desktop-a", None, false, Default::default()).unwrap();
        let fields: Vec<&String> = first.as_object().unwrap().keys().collect();
        for field in ["tokenId", "label", "token", "createdAt", "userId", "pending"] {
            assert!(fields.iter().any(|f| *f == field), "{field}: {first}");
        }
        assert_eq!(first["userId"], "owner");
        assert_eq!(first["pending"], false);
        assert_eq!(first["label"], "desktop-a");
        assert!(first["token"].as_str().unwrap().starts_with("cxm1."));
        // --pending: no user.
        let pending = super::issue_client_token(&tokens, "ssh-b", None, true, Default::default()).unwrap();
        assert_eq!(pending["userId"], serde_json::Value::Null);
        assert_eq!(pending["pending"], true);
        // Later device without --user: pending (owner approval required by default).
        let later = super::issue_client_token(&tokens, "ssh-c", None, false, Default::default()).unwrap();
        assert_eq!((later["userId"].clone(), later["pending"].clone()), (serde_json::Value::Null, serde_json::json!(true)));
        // --user owner.
        let owner = super::issue_client_token(
            &tokens,
            "ssh-d",
            Some("owner".into()),
            false,
            cutex::management::client_tokens::DeviceScope::Chat,
        )
        .unwrap();
        assert_eq!((owner["userId"].clone(), owner["pending"].clone()), (serde_json::json!("owner"), serde_json::json!(false)));
        assert_eq!(owner["scope"], "chat");
        assert_eq!(first["scope"], "full");
        assert!(super::issue_client_token(&tokens, "ssh-e", Some("u-000000000000".into()), false, Default::default()).is_err());
        // Owner tokens from the CLI are new owner devices (via ssh); the
        // first device too (it was bound, not migrated).
        let users = cutex::management::users::UserStore::beside(&tokens);
        let notices = users.snapshot().unwrap().new_owner_devices.clone();
        let ids: Vec<(&str, &str)> = notices.iter().map(|n| (n.token_id.as_str(), n.via.as_str())).collect();
        assert!(ids.contains(&(owner["tokenId"].as_str().unwrap(), "ssh")), "{ids:?}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn pair_passes_the_user() {
        let cli = Cli::try_parse_from(["cutex", "management", "pair", "--user", "u-0123456789ab", "--json"]).unwrap();
        let cutex::cli::args::CommandKind::Management {
            command: cutex::cli::args::ManagementCommand::Pair { user, json, .. },
        } = cli.command.unwrap()
        else {
            panic!("pair should parse");
        };
        assert_eq!((user.as_deref(), json), (Some("u-0123456789ab"), true));
        assert_eq!(
            super::pair_request_body(Some("phone"), Some("u-0123456789ab"), None),
            serde_json::json!({ "label": "phone", "userId": "u-0123456789ab" })
        );
        assert_eq!(super::pair_request_body(None, None, None), serde_json::json!({ "label": null }));
        assert_eq!(
            super::pair_request_body(None, None, Some("read")),
            serde_json::json!({ "label": null, "scope": "read" })
        );
        let cli = Cli::try_parse_from(["cutex", "management", "pair", "--scope", "chat"]).unwrap();
        let cutex::cli::args::CommandKind::Management {
            command: cutex::cli::args::ManagementCommand::Pair { scope, .. },
        } = cli.command.unwrap()
        else {
            panic!("pair should parse");
        };
        assert_eq!(scope.as_deref(), Some("chat"));
        assert!(Cli::try_parse_from(["cutex", "management", "pair", "--scope", "admin"]).is_err());
    }

    #[test]
    fn owner_commands_show_set_and_clear_the_owner_person() {
        use cutex::cli::args::ManagementOwnerCommand;
        for args in [
            &["cutex", "management", "owner", "show", "--json"][..],
            &["cutex", "management", "owner", "set-person", "0123456789abcdef"][..],
            &["cutex", "management", "owner", "clear-person"][..],
            &["cutex", "human", "management", "owner", "show"][..],
        ] {
            assert!(Cli::try_parse_from(args).is_ok(), "{args:?}");
        }
        let (root, _, users) = temp_stores();
        let (value, json) = super::owner_command(&users, ManagementOwnerCommand::Show { json: true }).unwrap();
        assert!(json);
        assert_eq!(value["owner"]["confirmed"], false);
        assert_eq!(value["ownerKeys"], serde_json::json!([]));
        root_claim(&users, &key(1));
        root_claim(&users, &key(2));
        let (value, _) = super::owner_command(&users, ManagementOwnerCommand::Show { json: false }).unwrap();
        let first = cutex::management::persons::fingerprint(&key(1));
        let second = cutex::management::persons::fingerprint(&key(2));
        assert_eq!(value["owner"]["fingerprint"], first.as_str());
        assert_eq!(value["owner"]["source"], "firstRootClaim");
        assert_eq!(value["publicKey"], key(1).as_str());
        assert_eq!(value["ownerKeys"][0]["ownerPerson"], true);
        assert_eq!(value["ownerKeys"][1]["ownerPerson"], false);
        assert!(super::owner_text(&value).contains(&first));
        // set-person by fingerprint.
        let (value, _) =
            super::owner_command(&users, ManagementOwnerCommand::SetPerson { key: second.clone() }).unwrap();
        assert_eq!(value["owner"]["fingerprint"], second.as_str());
        assert_eq!(value["owner"]["source"], "local");
        // Not an owner key.
        assert!(super::owner_command(&users, ManagementOwnerCommand::SetPerson { key: key(9) }).is_err());
        // clear-person, then set by public key.
        let (value, _) = super::owner_command(&users, ManagementOwnerCommand::ClearPerson).unwrap();
        assert_eq!(value["owner"]["confirmed"], false);
        assert_eq!(value["owner"]["fingerprint"], serde_json::Value::Null);
        assert!(super::owner_text(&value).contains("unconfirmed"));
        let (value, _) = super::owner_command(&users, ManagementOwnerCommand::SetPerson { key: key(1) }).unwrap();
        assert_eq!(value["owner"]["fingerprint"], first.as_str());
        let audit = std::fs::read_to_string(users.dir().join("audit.jsonl")).unwrap();
        assert!(audit.contains("\"ownerPersonSet\"") && audit.contains("\"actor\":\"cli\""), "{audit}");
        let _ = std::fs::remove_dir_all(root);
    }
}
