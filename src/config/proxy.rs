//! Proxy configuration normalization and launch environment construction.

use std::net::IpAddr;

use url::Url;

use crate::config::env::CUTE_CODEX_FORCE_HTTP_TRANSPORT_ENV_VAR;
use crate::profiles::model::CodezConfig;
use crate::profiles::model::ProxyConfig;
use crate::profiles::model::RuntimeConfig;
use crate::profiles::model::StoredAccount;

pub const DOCKER_PROXY_HOST_ALIAS: &str = "host.docker.internal";

pub fn proxy_config_from_parts(
    enabled: bool,
    url: Option<String>,
    no_proxy: Option<String>,
    force_http_transport: bool,
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
        Ok(ProxyConfig {
            enabled: true,
            url: Some(url),
            no_proxy,
            force_http_transport,
        })
    } else {
        Ok(ProxyConfig {
            enabled: false,
            url: None,
            no_proxy: None,
            force_http_transport,
        })
    }
}

pub fn set_global_proxy_config(
    config: &mut CodezConfig,
    url: String,
    no_proxy: Option<String>,
    force_http_transport: bool,
) -> anyhow::Result<bool> {
    Ok(set_proxy_config_value(
        &mut config.proxy,
        proxy_config_from_parts(true, Some(url), no_proxy, force_http_transport)?,
    ))
}

pub fn clear_global_proxy_config(config: &mut CodezConfig) -> bool {
    config.proxy.take().is_some()
}

pub fn set_account_proxy_config(
    account: &mut StoredAccount,
    url: String,
    no_proxy: Option<String>,
    force_http_transport: bool,
) -> anyhow::Result<bool> {
    Ok(set_proxy_config_value(
        &mut account.proxy,
        proxy_config_from_parts(true, Some(url), no_proxy, force_http_transport)?,
    ))
}

pub fn disable_account_proxy_config(account: &mut StoredAccount) -> anyhow::Result<bool> {
    Ok(set_proxy_config_value(
        &mut account.proxy,
        proxy_config_from_parts(false, None, None, /*force_http_transport*/ true)?,
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

pub fn effective_proxy_config<'a>(
    account: &'a StoredAccount,
    global_config: &'a CodezConfig,
) -> Option<&'a ProxyConfig> {
    account.proxy.as_ref().or(global_config.proxy.as_ref())
}

pub fn proxy_envs(
    proxy: Option<&ProxyConfig>,
    runtime: Option<&RuntimeConfig>,
) -> Vec<(String, String)> {
    let Some(proxy) = proxy else {
        return Vec::new();
    };
    if !proxy.enabled {
        return vec![
            ("HTTP_PROXY".to_string(), String::new()),
            ("HTTPS_PROXY".to_string(), String::new()),
            ("ALL_PROXY".to_string(), String::new()),
            ("NO_PROXY".to_string(), String::new()),
            ("http_proxy".to_string(), String::new()),
            ("https_proxy".to_string(), String::new()),
            ("all_proxy".to_string(), String::new()),
            ("no_proxy".to_string(), String::new()),
            (
                CUTE_CODEX_FORCE_HTTP_TRANSPORT_ENV_VAR.to_string(),
                "0".to_string(),
            ),
        ];
    };

    let configured_url = proxy.url.clone().unwrap_or_default();
    let url = match runtime {
        Some(RuntimeConfig::Docker { .. }) => {
            rewrite_docker_loopback_proxy_url(&configured_url).unwrap_or(configured_url)
        }
        _ => configured_url,
    };
    let http_proxy_value = url.clone();
    let mut envs = vec![
        ("HTTP_PROXY".to_string(), http_proxy_value.clone()),
        ("HTTPS_PROXY".to_string(), http_proxy_value.clone()),
        ("ALL_PROXY".to_string(), url.clone()),
        ("http_proxy".to_string(), http_proxy_value.clone()),
        ("https_proxy".to_string(), http_proxy_value),
        ("all_proxy".to_string(), url),
        (
            CUTE_CODEX_FORCE_HTTP_TRANSPORT_ENV_VAR.to_string(),
            if proxy.force_http_transport {
                "1".to_string()
            } else {
                "0".to_string()
            },
        ),
    ];

    let no_proxy = proxy.no_proxy.clone().unwrap_or_default();
    envs.push(("NO_PROXY".to_string(), no_proxy.clone()));
    envs.push(("no_proxy".to_string(), no_proxy));

    envs
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
        set_global_proxy_config(&mut config, "http://127.0.0.1:12345".into(), Some("localhost".into()), false).unwrap();
        let inherited: std::collections::BTreeMap<_, _> = native_proxy_envs(&account, &config).into_iter().collect();
        assert_eq!(inherited.get("HTTP_PROXY").map(String::as_str), Some("http://127.0.0.1:12345"));
        assert_eq!(inherited.get("NO_PROXY").map(String::as_str), Some("localhost"));
        disable_account_proxy_config(&mut account).unwrap();
        let disabled = native_proxy_envs(&account, &config);
        assert_eq!(disabled.len(), 8);
        assert!(disabled.iter().all(|(_, value)| value.is_empty()));
        assert!(native_proxy_envs(&sample_account(), &CodezConfig::default()).is_empty());
    }

    #[test]
    fn global_proxy_mutators_report_changed_state() {
        let mut config = CodezConfig::default();

        assert!(set_global_proxy_config(
            &mut config,
            "socks5h://127.0.0.1:7890".to_string(),
            Some("localhost,127.0.0.1".to_string()),
            true,
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
            true,
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
            false,
        )
        .expect("proxy config should be valid"));
        assert_eq!(
            account
                .proxy
                .as_ref()
                .and_then(|proxy| proxy.url.as_deref()),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(
            account
                .proxy
                .as_ref()
                .map(|proxy| proxy.force_http_transport),
            Some(false)
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

/// Standard proxy variables consumed by native Codex, without retired fork switches.
pub fn native_proxy_envs(account: &StoredAccount, config: &CodezConfig) -> Vec<(String, String)> {
    proxy_envs(effective_proxy_config(account, config), None)
        .into_iter()
        .filter(|(key, _)| key != CUTE_CODEX_FORCE_HTTP_TRANSPORT_ENV_VAR)
        .collect()
}
