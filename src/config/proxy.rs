//! Proxy configuration normalization and launch environment construction.

use std::net::IpAddr;

use url::Url;

use crate::profiles::model::CodezConfig;
use crate::profiles::model::ProxyConfig;
use crate::profiles::model::RuntimeConfig;
use crate::profiles::model::StoredAccount;

pub const DOCKER_PROXY_HOST_ALIAS: &str = "host.docker.internal";

pub fn proxy_config_from_parts(
    enabled: bool,
    url: Option<String>,
    no_proxy: Option<String>,
) -> anyhow::Result<ProxyConfig> {
    let url = url
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let no_proxy = no_proxy
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if enabled {
        let url = url
            .ok_or_else(|| anyhow::anyhow!("Proxy URL must not be empty when proxy is enabled"))?;
        let scheme = url
            .split("://")
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let supported = matches!(
            scheme.as_str(),
            "http" | "https" | "socks5" | "socks5h" | "socks4" | "socks4a"
        );
        if !supported {
            anyhow::bail!(
                "Unsupported proxy scheme `{scheme}`. Expected http, https, socks5, socks5h, socks4, or socks4a"
            );
        }
        anyhow::ensure!(
            url::Url::parse(&url).is_ok_and(|parsed| parsed.host_str().is_some_and(|host| !host.is_empty())),
            "Proxy URL must look like scheme://[user:password@]host[:port]"
        );
        if let Some(no_proxy) = &no_proxy {
            anyhow::ensure!(
                no_proxy.len() <= 4096 && !no_proxy.chars().any(char::is_control),
                "noProxy must be a comma-separated host list"
            );
        }
        Ok(ProxyConfig {
            enabled: true,
            url: Some(url),
            no_proxy,
        })
    } else {
        Ok(ProxyConfig {
            enabled: false,
            url: None,
            no_proxy: None,
        })
    }
}

pub fn set_global_proxy_config(
    config: &mut CodezConfig,
    url: String,
    no_proxy: Option<String>,
) -> anyhow::Result<bool> {
    Ok(set_proxy_config_value(
        &mut config.proxy,
        proxy_config_from_parts(true, Some(url), no_proxy)?,
    ))
}

pub fn clear_global_proxy_config(config: &mut CodezConfig) -> bool {
    config.proxy.take().is_some()
}

pub fn set_account_proxy_config(
    account: &mut StoredAccount,
    url: String,
    no_proxy: Option<String>,
) -> anyhow::Result<bool> {
    Ok(set_proxy_config_value(
        &mut account.proxy,
        proxy_config_from_parts(true, Some(url), no_proxy)?,
    ))
}

pub fn disable_account_proxy_config(account: &mut StoredAccount) -> anyhow::Result<bool> {
    Ok(set_proxy_config_value(
        &mut account.proxy,
        proxy_config_from_parts(false, None, None)?,
    ))
}

pub fn clear_account_proxy_config(account: &mut StoredAccount) -> bool {
    account.proxy.take().is_some()
}

fn set_proxy_config_value(current: &mut Option<ProxyConfig>, next: ProxyConfig) -> bool {
    if current.as_ref() == Some(&next) {
        return false;
    }
    *current = Some(next);
    true
}

/// A partial proxy object from the Management API; Debug never prints it (the URL may carry
/// credentials).
#[derive(Clone, PartialEq, Eq, Default)]
pub struct ProxyUpdate(pub serde_json::Map<String, serde_json::Value>);

impl std::fmt::Debug for ProxyUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProxyUpdate(<redacted>)")
    }
}

/// Apply a partial proxy object (`/v2/settings.proxy`, `/v2/profiles` `proxy`) onto `current`.
/// `mode`: `unset_mode` (`system` for the host, `inherit` for a Profile) → `None` (nothing stored),
/// `none` → explicit off, `manual` → URL; without `mode` the legacy `enabled` flag. Omitted `url` /
/// `noProxy` keep the stored values (credentials embedded in the URL survive edits); `null`
/// clears them. `none` keeps a manual address (stored `enabled: false` with its `url` /
/// `noProxy`, never used while off), so switching back to `manual` restores it.
pub fn apply_proxy_update(
    current: Option<&ProxyConfig>,
    update: &serde_json::Map<String, serde_json::Value>,
    unset_mode: &str,
) -> anyhow::Result<Option<ProxyConfig>> {
    use serde_json::Value;
    if let Some(key) = update.keys().find(|key| !["mode", "enabled", "url", "noProxy"].contains(&key.as_str())) {
        anyhow::bail!("proxy: unsupported field {key}");
    }
    let string_field = |key: &str, fallback: Option<String>| -> anyhow::Result<Option<String>> {
        match update.get(key) {
            None => Ok(fallback),
            Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.clone())),
            Some(_) => anyhow::bail!("proxy.{key} must be a string or null"),
        }
    };
    let enabled = match update.get("mode") {
        Some(Value::String(mode)) if mode == unset_mode => return Ok(None),
        Some(Value::String(mode)) if mode == "none" => false,
        Some(Value::String(mode)) if mode == "manual" => true,
        Some(_) => anyhow::bail!("proxy.mode must be {unset_mode}, none or manual"),
        None => match update.get("enabled") {
            None => current.is_some_and(|p| p.enabled),
            Some(Value::Bool(value)) => *value,
            Some(_) => anyhow::bail!("proxy.enabled must be a boolean"),
        },
    };
    let url = string_field("url", current.and_then(|p| p.url.clone()))?;
    let no_proxy = string_field("noProxy", current.and_then(|p| p.no_proxy.clone()))?;
    if enabled {
        return proxy_config_from_parts(true, url, no_proxy).map(Some);
    }
    // Off: remember the manual address (validated like manual). A remembered value that no longer
    // validates is dropped; one given in this update is refused.
    let remembered = match proxy_config_from_parts(true, url.clone(), no_proxy.clone()) {
        Ok(manual) => ProxyConfig { enabled: false, ..manual },
        Err(error) if update.contains_key("url") && url.as_deref().is_some_and(|url| !url.trim().is_empty()) => {
            return Err(error)
        }
        Err(_) => proxy_config_from_parts(false, None, None)?,
    };
    Ok(Some(remembered))
}

pub fn effective_proxy_config<'a>(
    account: &'a StoredAccount,
    global_config: &'a CodezConfig,
) -> Option<&'a ProxyConfig> {
    account.proxy.as_ref().or(global_config.proxy.as_ref())
}

/// NO_PROXY of an explicit "no proxy": every host name (`*`) and every IP (`0.0.0.0/0`, `::/0`),
/// so a runtime that falls back to the Windows system proxy when `HTTP(S)_PROXY` is empty
/// (reqwest / hyper-util in native Codex, httpx) still connects directly.
pub const DIRECT_NO_PROXY: &str = "*,0.0.0.0/0,::/0";

pub fn proxy_envs(
    proxy: Option<&ProxyConfig>,
    runtime: Option<&RuntimeConfig>,
) -> Vec<(String, String)> {
    let Some(proxy) = proxy else {
        return Vec::new();
    };
    if !proxy.enabled {
        let no_proxy = with_loopback_no_proxy(DIRECT_NO_PROXY);
        return vec![
            ("HTTP_PROXY".to_string(), String::new()),
            ("HTTPS_PROXY".to_string(), String::new()),
            ("ALL_PROXY".to_string(), String::new()),
            ("NO_PROXY".to_string(), no_proxy.clone()),
            ("http_proxy".to_string(), String::new()),
            ("https_proxy".to_string(), String::new()),
            ("all_proxy".to_string(), String::new()),
            ("no_proxy".to_string(), no_proxy),
        ];
    };

    let configured_url = proxy.url.clone().unwrap_or_default();
    let url = match runtime {
        Some(RuntimeConfig::Docker { .. }) => {
            rewrite_docker_loopback_proxy_url(&configured_url).unwrap_or(configured_url)
        }
        _ => configured_url,
    };
    let mut envs = vec![
        ("HTTP_PROXY".to_string(), url.clone()),
        ("HTTPS_PROXY".to_string(), url.clone()),
        ("ALL_PROXY".to_string(), url.clone()),
        ("http_proxy".to_string(), url.clone()),
        ("https_proxy".to_string(), url.clone()),
        ("all_proxy".to_string(), url),
    ];

    // Loopback never goes through the proxy: the runtime's own calls to cutex (Claude MCP binding,
    // streamable-HTTP MCP servers, the agent bus) are on 127.0.0.1.
    let no_proxy = with_loopback_no_proxy(proxy.no_proxy.as_deref().unwrap_or_default());
    envs.push(("NO_PROXY".to_string(), no_proxy.clone()));
    envs.push(("no_proxy".to_string(), no_proxy));

    envs
}

/// Loopback names every NO_PROXY cutex hands out (or runs with) carries.
pub const LOOPBACK_NO_PROXY: [&str; 3] = ["127.0.0.1", "localhost", "::1"];

/// `value` (a NO_PROXY list) with the loopback names appended where missing. Also after `*`:
/// several clients (reqwest, hyper-util) match `*` against host names only, never IP literals.
pub fn with_loopback_no_proxy(value: &str) -> String {
    let mut entries: Vec<String> = value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect();
    for name in LOOPBACK_NO_PROXY {
        if !entries.iter().any(|entry| entry.eq_ignore_ascii_case(name)) {
            entries.push(name.to_string());
        }
    }
    entries.join(",")
}

/// Make this process (and every child it spawns: natives, the Claude CLI, tools) bypass any
/// environment proxy for loopback. ureq's default agent and reqwest read `HTTP(S)_PROXY` /
/// `ALL_PROXY` with no built-in loopback exception, so an inherited user proxy would otherwise
/// carry cutex's own 127.0.0.1 traffic. Call at the very start of `main`, before threads exist.
pub fn ensure_process_loopback_no_proxy() {
    let current = std::env::var("NO_PROXY")
        .ok()
        .or_else(|| std::env::var("no_proxy").ok())
        .unwrap_or_default();
    let next = with_loopback_no_proxy(&current);
    if std::env::var("NO_PROXY").ok().as_deref() != Some(next.as_str()) {
        std::env::set_var("NO_PROXY", &next);
    }
    if !cfg!(windows) {
        let lower = with_loopback_no_proxy(&std::env::var("no_proxy").unwrap_or_else(|_| next.clone()));
        if std::env::var("no_proxy").ok().as_deref() != Some(lower.as_str()) {
            std::env::set_var("no_proxy", lower);
        }
    }
}

fn host_is_loopback(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

pub fn rewrite_docker_loopback_proxy_url(url: &str) -> Option<String> {
    if url.trim().is_empty() {
        return None;
    }
    let mut parsed = Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if !host_is_loopback(host) {
        return None;
    }
    parsed.set_host(Some(DOCKER_PROXY_HOST_ALIAS)).ok()?;
    Some(parsed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::model::CliKind;

    fn sample_account() -> StoredAccount {
        StoredAccount {
            options: Default::default(),
            browser_tools: false,
            search_tool: false,
            quota_reader: None,
            private_chatgpt_auth_file: false,
            claude_code: Default::default(),
            id: "acct-proxy".to_string(),
            name: "proxy".to_string(),
            email: None,
            plan_type: None,
            plan_label: None,
            quota_preset: Default::default(),
            source: None,
            runtime: RuntimeConfig::Host,
            proxy: None,
            session: None,
            cli_kind: CliKind::Codex,
            default_cli_args: Vec::new(),
            agent_name: None,
            last_used_at: None,
            provider_category: None,
            claude_account: None,
            model_display_names: Default::default(),
        }
    }

    #[test]
    fn native_proxy_inherits_global_and_explicit_profile_disable_clears_variables() {
        let mut account = sample_account();
        let mut config = CodezConfig::default();
        set_global_proxy_config(&mut config, "http://127.0.0.1:12345".into(), Some("localhost".into())).unwrap();
        let inherited: std::collections::BTreeMap<_, _> = native_proxy_envs(&account, &config).into_iter().collect();
        assert_eq!(inherited.get("HTTP_PROXY").map(String::as_str), Some("http://127.0.0.1:12345"));
        assert_eq!(inherited.get("NO_PROXY").map(String::as_str), Some("localhost,127.0.0.1,::1"));
        disable_account_proxy_config(&mut account).unwrap();
        let disabled: std::collections::BTreeMap<_, _> = native_proxy_envs(&account, &config).into_iter().collect();
        assert_eq!(disabled.len(), 8);
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"] {
            assert_eq!(disabled[key], "", "{key}");
        }
        // Explicit off: every host and IP skip any proxy (Windows system proxy fallback included).
        assert_eq!(disabled["NO_PROXY"], "*,0.0.0.0/0,::/0,127.0.0.1,localhost,::1");
        assert_eq!(disabled["no_proxy"], disabled["NO_PROXY"]);
        assert!(native_proxy_envs(&sample_account(), &CodezConfig::default()).is_empty());
    }

    #[test]
    fn loopback_is_always_in_no_proxy() {
        assert_eq!(with_loopback_no_proxy(""), "127.0.0.1,localhost,::1");
        assert_eq!(with_loopback_no_proxy(" example.com , LOCALHOST "), "example.com,LOCALHOST,127.0.0.1,::1");
        assert_eq!(with_loopback_no_proxy("127.0.0.1,localhost,::1"), "127.0.0.1,localhost,::1");
        assert_eq!(with_loopback_no_proxy("*"), "*,127.0.0.1,localhost,::1");
        let mut config = CodezConfig::default();
        set_global_proxy_config(&mut config, "http://proxy.example:3128".into(), None).unwrap();
        let envs: std::collections::BTreeMap<_, _> = native_proxy_envs(&sample_account(), &config).into_iter().collect();
        assert_eq!(envs.get("NO_PROXY").map(String::as_str), Some("127.0.0.1,localhost,::1"));
        assert_eq!(envs.get("no_proxy").map(String::as_str), Some("127.0.0.1,localhost,::1"));
    }

    #[test]
    fn global_proxy_mutators_report_changed_state() {
        let mut config = CodezConfig::default();

        assert!(set_global_proxy_config(
            &mut config,
            "socks5h://127.0.0.1:7890".to_string(),
            Some("localhost,127.0.0.1".to_string()),
        )
        .expect("proxy config should be valid"));
        assert_eq!(
            config.proxy.as_ref().and_then(|proxy| proxy.url.as_deref()),
            Some("socks5h://127.0.0.1:7890")
        );

        assert!(!set_global_proxy_config(
            &mut config,
            "socks5h://127.0.0.1:7890".to_string(),
            Some("localhost,127.0.0.1".to_string()),
        )
        .expect("matching proxy config should be valid"));

        assert!(clear_global_proxy_config(&mut config));
        assert!(config.proxy.is_none());
        assert!(!clear_global_proxy_config(&mut config));
    }

    #[test]
    fn account_proxy_mutators_set_disable_and_clear() {
        let mut account = sample_account();

        assert!(set_account_proxy_config(
            &mut account,
            "http://127.0.0.1:8080".to_string(),
            None,
        )
        .expect("proxy config should be valid"));
        assert_eq!(
            account
                .proxy
                .as_ref()
                .and_then(|proxy| proxy.url.as_deref()),
            Some("http://127.0.0.1:8080")
        );

        assert!(disable_account_proxy_config(&mut account).expect("disable should be valid"));
        assert_eq!(
            account.proxy.as_ref().map(|proxy| proxy.enabled),
            Some(false)
        );
        assert!(
            !disable_account_proxy_config(&mut account).expect("matching disable should be valid")
        );

        assert!(clear_account_proxy_config(&mut account));
        assert!(account.proxy.is_none());
        assert!(!clear_account_proxy_config(&mut account));
    }
}

/// Standard proxy variables of a Profile's effective proxy (native Codex, Claude runtime).
pub fn native_proxy_envs(account: &StoredAccount, config: &CodezConfig) -> Vec<(String, String)> {
    native_proxy_envs_for(effective_proxy_config(account, config))
}

pub fn native_proxy_envs_for(proxy: Option<&ProxyConfig>) -> Vec<(String, String)> {
    proxy_envs(proxy, None)
}
