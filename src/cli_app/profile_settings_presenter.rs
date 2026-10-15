use cutex::agent_bus::identity::account_agent_name;
use cutex::agent_bus::service::DEFAULT_AGENT_BUS_PORT;
use cutex::config::proxy::effective_proxy_config;
use cutex::launch::docker::default_docker_user_name;
use cutex::launch::docker::docker_user_name;
use cutex::notify::service::DEFAULT_DESKTOP_NOTIFY_PORT;
use cutex::platform::command::shell_quote;
use cutex::profiles::inspect::account_model_api_base;
use cutex::profiles::inspect::account_model_provider;
use cutex::profiles::inspect::account_proxy_scope_label;
use cutex::profiles::inspect::account_session_scope_label;
use cutex::profiles::inspect::account_status_line_len;
use cutex::profiles::inspect::session_config_label;
use cutex::profiles::materialize::materialized_account_files;
use cutex::profiles::model::AccountsStore;
use cutex::profiles::model::CodezConfig;
use cutex::profiles::model::RuntimeConfig;
use cutex::profiles::model::StoredAccount;
use cutex::profiles::plan_label::{account_plan, effective_plan_label, resolve_plan_label};
use cutex::ui::format::bool_label;
use cutex::ui::format::optional_label;
use cutex::ui::format::proxy_config_label;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const BLUE: &str = "\x1b[34m";
const MAGENTA: &str = "\x1b[35m";
const CYAN: &str = "\x1b[36m";

pub(super) fn print_profile_list(store: &AccountsStore) {
    if store.accounts.is_empty() {
        println!(
            "No accounts configured. Use `cutex login` or `cutex add --from-auth <path> --name <name>` to add one."
        );
        return;
    }

    struct Row {
        name: String,
        cli: String,
        source: String,
        plan: String,
        runtime: String,
        provider: String,
        email: String,
        active: bool,
    }

    let rows: Vec<Row> = store
        .accounts
        .iter()
        .map(|acc| {
            let active = Some(&acc.id) == store.active_account_id.as_ref();
            let runtime_str = match &acc.runtime {
                RuntimeConfig::Host => "host".to_string(),
                RuntimeConfig::Docker { image, .. } => format!("docker {image}"),
            };
            Row {
                name: acc.name.clone(),
                cli: acc.cli_kind.to_string(),
                source: acc.source.as_deref().unwrap_or("-").to_string(),
                plan: effective_plan_label(acc)
                    .map(|(label, _)| label)
                    .unwrap_or_else(|| "-".to_string()),
                runtime: runtime_str,
                provider: account_model_provider(acc).unwrap_or_else(|| "-".to_string()),
                email: acc.email.as_deref().unwrap_or("-").to_string(),
                active,
            }
        })
        .collect();

    let w_name = rows.iter().map(|r| r.name.len()).max().unwrap_or(4).max(4);
    let w_cli = rows.iter().map(|r| r.cli.len()).max().unwrap_or(3).max(3);
    let w_src = rows
        .iter()
        .map(|r| r.source.len())
        .max()
        .unwrap_or(6)
        .max(6);
    let w_plan = rows.iter().map(|r| r.plan.len()).max().unwrap_or(4).max(4);
    let w_rt = rows
        .iter()
        .map(|r| r.runtime.len())
        .max()
        .unwrap_or(7)
        .max(7);
    let w_prov = rows
        .iter()
        .map(|r| r.provider.len())
        .max()
        .unwrap_or(8)
        .max(8);
    let w_email = rows.iter().map(|r| r.email.len()).max().unwrap_or(5).max(5);

    println!(
        "{DIM}  #  {:<w_name$}  {:<w_cli$}  {:<w_src$}  {:<w_plan$}  {:<w_rt$}  {:<w_prov$}  {:<w_email$}{RESET}",
        "Name", "CLI", "Source", "Plan", "Runtime", "Provider", "Email"
    );

    for (idx, row) in rows.iter().enumerate() {
        let badge = if row.active {
            format!("  {GREEN}● active{RESET}")
        } else {
            String::new()
        };
        let name_color = if row.active { GREEN } else { CYAN };
        println!(
            "  {BOLD}{}{RESET}  {name_color}{:<w_name$}{RESET}  {:<w_cli$}  {BLUE}{:<w_src$}{RESET}  {MAGENTA}{:<w_plan$}{RESET}  {YELLOW}{:<w_rt$}{RESET}  {:<w_prov$}  {DIM}{:<w_email$}{RESET}{badge}",
            idx + 1,
            row.name,
            row.cli,
            row.source,
            row.plan,
            row.runtime,
            row.provider,
            row.email,
        );
    }
}

pub(super) fn print_profile_details(
    store: &AccountsStore,
    account: &StoredAccount,
    global_config: &CodezConfig,
) {
    let files = materialized_account_files(account).ok();
    let active = store.active_account_id.as_deref() == Some(account.id.as_str());
    let provider = account_model_provider(account).unwrap_or_else(|| "-".to_string());
    let api = account_model_api_base(account).unwrap_or_else(|| "-".to_string());
    let status_line_len = account_status_line_len(account);

    println!("{BOLD}{CYAN}Profile{RESET} {}", account.name);
    println!("{DIM}Active{RESET}  {}", bool_label(active));
    println!("{DIM}Id{RESET}      {}", account.id);
    println!(
        "{DIM}Source{RESET}  {}",
        account.source.as_deref().unwrap_or("unknown")
    );
    let live_plan = account_plan(account);
    println!(
        "{DIM}Plan{RESET}    {}",
        resolve_plan_label(
            live_plan.as_deref(),
            account.plan_label.as_deref(),
            cutex::profiles::plan_label::legacy_plan_type(account),
        )
        .map(|(label, _)| label)
        .unwrap_or_else(|| "unknown".to_string())
    );
    println!(
        "{DIM}Account plan{RESET} {}",
        live_plan
            .as_deref()
            .or(account.plan_type.as_deref())
            .unwrap_or("unknown")
    );
    println!(
        "{DIM}Email{RESET}   {}",
        account.email.as_deref().unwrap_or("-")
    );
    println!(
        "{DIM}DefaultArgs{RESET} {}",
        cli_args_label(&account.default_cli_args)
    );
    println!("{DIM}AgentName{RESET} {}", account_agent_name(account));
    println!(
        "{DIM}Runtime{RESET} {}",
        runtime_description(&account.runtime)
    );
    println!("{DIM}Provider{RESET} {}", provider);
    println!("{DIM}ApiBase{RESET} {}", api);
    match status_line_len {
        Some(count) => println!("{DIM}StatusLine{RESET} {} items", count),
        None => println!("{DIM}StatusLine{RESET} -"),
    }
    println!(
        "{DIM}Proxy(profile){RESET}  {}",
        proxy_config_label(account.proxy.as_ref())
    );
    println!(
        "{DIM}Proxy(global){RESET}   {}",
        proxy_config_label(global_config.proxy.as_ref())
    );
    println!(
        "{DIM}Proxy(effective){RESET} {}",
        proxy_config_label(effective_proxy_config(account, global_config))
    );
    println!(
        "{DIM}Proxy(scope){RESET} {}",
        account_proxy_scope_label(account, global_config)
    );
    println!(
        "{DIM}Session(profile){RESET}  {}",
        account
            .session
            .as_ref()
            .map(session_config_label)
            .unwrap_or("inherit")
    );
    println!(
        "{DIM}Session(global){RESET}   {}",
        session_config_label(&global_config.session)
    );
    println!(
        "{DIM}Session(effective){RESET} {}",
        session_config_label(effective_session_config(account, global_config))
    );
    println!(
        "{DIM}Session(scope){RESET} {}",
        account_session_scope_label(account, global_config)
    );
    if let Some(files) = files {
        println!(
            "{DIM}Config File{RESET} {}",
            if files.config_path.exists() {
                "present"
            } else {
                "missing"
            }
        );
        println!(
            "{DIM}Auth File{RESET} {}",
            if files.auth_path.exists() {
                "present"
            } else {
                "missing"
            }
        );
        println!("{DIM}Config{RESET}  {}", files.config_path.display());
        println!("{DIM}Auth{RESET}    {}", files.auth_path.display());
    }
}

pub(super) fn print_global_settings(config: &CodezConfig) {
    println!("{BOLD}{CYAN}Global Settings{RESET}");
    println!(
        "{DIM}docker_use_sudo{RESET} {}",
        bool_label(config.docker_use_sudo)
    );
    println!(
        "{DIM}custom_status_items{RESET} {}",
        config.custom_status_items.len()
    );
    println!(
        "{DIM}session{RESET} {}",
        session_config_label(&config.session)
    );
    println!(
        "{DIM}default_profile{RESET} {}",
        config.default_profile.as_deref().unwrap_or("-")
    );
    println!(
        "{DIM}default_profile_direct_launch{RESET} {}",
        bool_label(config.default_profile_direct_launch)
    );
    println!(
        "{DIM}proxy{RESET} {}",
        proxy_config_label(config.proxy.as_ref())
    );
    println!(
        "{DIM}notify_service_url{RESET} {}",
        config.notify_service_url.as_deref().unwrap_or("-")
    );
    println!(
        "{DIM}notify_service_token{RESET} {}",
        if config
            .notify_service_token
            .as_ref()
            .is_some_and(|t| !t.is_empty())
        {
            "(set)"
        } else {
            "-"
        }
    );
    println!(
        "{DIM}notify_service_idle_timeout_secs{RESET} {}",
        config
            .notify_service_idle_timeout_secs
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string())
    );
    println!(
        "{DIM}notify_service_composer_idle_timeout_secs{RESET} {}",
        config
            .notify_service_composer_idle_timeout_secs
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string())
    );
    println!(
        "{DIM}notify_service_approval_timeout_secs{RESET} {}",
        config
            .notify_service_approval_timeout_secs
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string())
    );
    println!(
        "{DIM}notify_service_startup_idle_timeout_secs{RESET} {}",
        config
            .notify_service_startup_idle_timeout_secs
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string())
    );
    println!(
        "{DIM}notify_service_events{RESET} {}",
        config
            .notify_service_events
            .as_ref()
            .map(|events| events.join(","))
            .unwrap_or_else(|| "-".to_string())
    );
    println!(
        "{DIM}notify_service_user_message_content{RESET} {}",
        config
            .notify_service_user_message_content
            .as_deref()
            .unwrap_or("-")
    );
    println!(
        "{DIM}notify_service_user_message_preview_chars{RESET} {}",
        config
            .notify_service_user_message_preview_chars
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string())
    );
    println!(
        "{DIM}rate_limit_threshold_warning_mode{RESET} {}",
        optional_label(config.rate_limit_threshold_warning_mode.as_deref())
    );
    println!(
        "{DIM}rate_limit_model_nudge_mode{RESET} {}",
        optional_label(config.rate_limit_model_nudge_mode.as_deref())
    );
    println!(
        "{DIM}desktop_notify_enabled{RESET} {}",
        bool_label(config.desktop_notify_enabled)
    );
    println!(
        "{DIM}desktop_notify_port{RESET} {}",
        config
            .desktop_notify_port
            .unwrap_or(DEFAULT_DESKTOP_NOTIFY_PORT)
    );
    println!(
        "{DIM}desktop_notify_token{RESET} {}",
        if config
            .desktop_notify_token
            .as_ref()
            .is_some_and(|token| !token.is_empty())
        {
            "(set)"
        } else {
            "-"
        }
    );
    println!(
        "{DIM}agent_bus_enabled{RESET} {}",
        bool_label(config.agent_bus_enabled)
    );
    println!(
        "{DIM}agent_bus_port{RESET} {}",
        config.agent_bus_port.unwrap_or(DEFAULT_AGENT_BUS_PORT)
    );
    println!(
        "{DIM}agent_bus_token{RESET} {}",
        if config
            .agent_bus_token
            .as_ref()
            .is_some_and(|token| !token.is_empty())
        {
            "(set)"
        } else {
            "-"
        }
    );
}

fn cli_args_label(args: &[String]) -> String {
    if args.is_empty() {
        "-".to_string()
    } else {
        args.iter()
            .map(|arg| shell_quote(arg))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn runtime_description(runtime: &RuntimeConfig) -> String {
    match runtime {
        RuntimeConfig::Host => "host".to_string(),
        RuntimeConfig::Docker { image, user_name } => format!(
            "docker image={} user={}",
            image,
            docker_user_name(user_name.as_deref()).unwrap_or_else(|_| default_docker_user_name())
        ),
    }
}

fn effective_session_config<'a>(
    account: &'a StoredAccount,
    global_config: &'a CodezConfig,
) -> &'a cutex::profiles::model::SessionConfig {
    account.session.as_ref().unwrap_or(&global_config.session)
}
