//! Typed, side-effect-free mutations for persisted global settings.

use anyhow::{ensure, Context};

use crate::agent_bus::service::validate_agent_bus_port;
use crate::notify::service::validate_desktop_notify_port;
use crate::profiles::model::{CodezConfig, ProxyConfig};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GlobalConfigPatch {
    pub appearance: Option<crate::profiles::appearance::Appearance>,
    pub docker_use_sudo: Option<bool>,
    pub terminal_truecolor: Option<bool>,
    pub default_local_host_filter: Option<bool>,
    pub agent_sort: Option<crate::profiles::list_preferences::ListSort>,
    pub session_sort: Option<crate::profiles::list_preferences::ListSort>,
    pub project_sort: Option<crate::profiles::list_preferences::ListSort>,
    pub job_sort: Option<crate::profiles::list_preferences::ListSort>,

    pub session_enabled: Option<bool>,
    pub claude_usage_refresh_minutes: Option<u8>,
    pub profile_quota_refresh_minutes: Option<u8>,
    pub archive_delete_empty: Option<bool>,
    pub queue_owner_may_pass_asks: Option<bool>,
    pub auto_thread_titles: Option<bool>,
    pub codex_config_follow_shared: Option<bool>,
    pub thread_title_model: ConfigValueUpdate<String>,
    /// Cross-project isolation and owner links (`agent_security`).
    pub agent_security: Option<crate::config::agent_security::AgentSecurityConfig>,
    /// Uploaded session attachments (`attachments`).
    pub attachments: Option<crate::config::attachments::AttachmentSettings>,
    /// Input overflow → documents (`documents`).
    pub documents: Option<crate::config::documents::DocumentSettings>,
    pub default_profile: ConfigValueUpdate<String>,
    pub default_profile_direct_launch: Option<bool>,
    pub quick_chat_default_profile: ConfigValueUpdate<String>,
    pub new_session_defaults: Option<std::collections::BTreeMap<String, crate::profiles::model::NewSessionDefaults>>,
    pub new_session_notification: Option<crate::notify::session::Level>,
    pub new_thread_defaults: Option<crate::config::new_thread::NewThreadDefaults>,
    pub proxy: ConfigValueUpdate<ProxyConfig>,
    pub notify_service_url: ConfigValueUpdate<String>,
    pub notify_service_token: ConfigValueUpdate<String>,
    pub notify_service_idle_timeout_secs: ConfigValueUpdate<u64>,
    pub notify_service_composer_idle_timeout_secs: ConfigValueUpdate<u64>,
    pub notify_service_approval_timeout_secs: ConfigValueUpdate<u64>,
    pub notify_service_startup_idle_timeout_secs: ConfigValueUpdate<u64>,
    pub notify_service_events: ConfigValueUpdate<Vec<String>>,
    pub notify_service_user_message_content: ConfigValueUpdate<String>,
    pub notify_service_user_message_preview_chars: ConfigValueUpdate<u64>,
    pub rate_limit_threshold_warning_mode: ConfigValueUpdate<String>,
    pub rate_limit_model_nudge_mode: ConfigValueUpdate<String>,
    pub desktop_notify_enabled: Option<bool>,
    pub desktop_notify_port: ConfigValueUpdate<u16>,
    pub desktop_notify_token: ConfigValueUpdate<String>,
    pub agent_bus_enabled: Option<bool>,
    pub agent_bus_port: ConfigValueUpdate<u16>,
    pub agent_bus_token: ConfigValueUpdate<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConfigValueUpdate<T> {
    #[default]
    Unchanged,
    Set(T),
    Clear,
}

pub fn apply_global_config_patch(
    config: &mut CodezConfig,
    patch: &GlobalConfigPatch,
) -> anyhow::Result<bool> {
    if let ConfigValueUpdate::Set(port) = &patch.desktop_notify_port {
        validate_desktop_notify_port(*port)?;
    }
    if let ConfigValueUpdate::Set(port) = &patch.agent_bus_port {
        validate_agent_bus_port(*port)?;
        if crate::config::instance::is_isolated() && config.management_port == Some(*port) {
            anyhow::bail!("Agent bus port {port} is this instance's management port");
        }
    }
    if matches!(patch.agent_bus_port, ConfigValueUpdate::Clear) && crate::config::instance::is_isolated() {
        anyhow::bail!("an independent instance needs an explicit Agent bus port (clearing it would select the default instance's 24260)");
    }
    if let Some(minutes) = patch.claude_usage_refresh_minutes {
        ensure!(
            (1..=60).contains(&minutes),
            "Claude usage refresh interval must be 1–60 minutes"
        );
    }
    if let Some(minutes) = patch.profile_quota_refresh_minutes {
        ensure!(
            minutes <= 60,
            "Profile quota refresh interval must be 0–60 minutes (0 disables)"
        );
    }
    if let Some(security) = &patch.agent_security {
        security.validate()?;
    }
    if let Some(attachments) = &patch.attachments {
        attachments.validate()?;
    }
    if let Some(documents) = &patch.documents {
        documents.validate()?;
    }
    let mut changed = false;
    if let Some(security) = &patch.agent_security {
        changed |= config.agent_security != *security;
        config.agent_security = security.clone();
    }
    if let Some(attachments) = patch.attachments {
        changed |= config.attachments != attachments;
        config.attachments = attachments;
    }
    if let Some(documents) = patch.documents {
        changed |= config.documents != documents;
        config.documents = documents;
    }
    if let Some(a)=&patch.appearance {
        a.host_suffix.validate()?;
        for style in a.providers.values(){style.validate()?;}
        changed |= config.appearance != *a; config.appearance=a.clone();
    }
    changed |= apply_value_update(&mut config.agent_sort, patch.agent_sort);
    changed |= apply_value_update(&mut config.session_sort, patch.session_sort);
    changed |= apply_value_update(&mut config.project_sort, patch.project_sort);
    changed |= apply_value_update(&mut config.job_sort, patch.job_sort);

    changed |= apply_value_update(&mut config.terminal_truecolor, patch.terminal_truecolor);
    changed |= apply_value_update(&mut config.default_local_host_filter, patch.default_local_host_filter);
    changed |= apply_value_update(&mut config.docker_use_sudo, patch.docker_use_sudo);
    changed |= apply_value_update(&mut config.session.enabled, patch.session_enabled);
    changed |=
        apply_value_update(&mut config.claude_usage_refresh_minutes, patch.claude_usage_refresh_minutes);
    changed |= apply_value_update(
        &mut config.profile_quota_refresh_minutes,
        patch.profile_quota_refresh_minutes,
    );
    changed |= apply_value_update(&mut config.archive_delete_empty, patch.archive_delete_empty);
    changed |= apply_value_update(&mut config.queue.owner_may_pass_asks, patch.queue_owner_may_pass_asks);
    changed |= apply_value_update(&mut config.auto_thread_titles, patch.auto_thread_titles);
    changed |= apply_value_update(
        &mut config.codex_config_follow_shared,
        patch.codex_config_follow_shared,
    );
    changed |= apply_optional_update(&mut config.thread_title_model, &patch.thread_title_model);
    changed |= apply_optional_update(&mut config.default_profile, &patch.default_profile);
    changed |= apply_optional_update(
        &mut config.quick_chat_default_profile,
        &patch.quick_chat_default_profile,
    );
    changed |= apply_value_update(
        &mut config.default_profile_direct_launch,
        patch.default_profile_direct_launch,
    );
    if let Some(defaults) = &patch.new_session_defaults {
        changed |= config.new_session_defaults != *defaults;
        config.new_session_defaults = defaults.clone();
    }
    changed |= apply_value_update(&mut config.new_session_notification, patch.new_session_notification);
    if let Some(defaults) = &patch.new_thread_defaults {
        defaults.validate()?;
        changed |= config.new_thread_defaults != *defaults;
        config.new_thread_defaults = defaults.clone();
    }
    changed |= apply_optional_update(&mut config.proxy, &patch.proxy);
    changed |= apply_optional_update(&mut config.notify_service_url, &patch.notify_service_url);
    changed |= apply_optional_update(
        &mut config.notify_service_token,
        &patch.notify_service_token,
    );
    changed |= apply_optional_update(
        &mut config.notify_service_idle_timeout_secs,
        &patch.notify_service_idle_timeout_secs,
    );
    changed |= apply_optional_update(
        &mut config.notify_service_composer_idle_timeout_secs,
        &patch.notify_service_composer_idle_timeout_secs,
    );
    changed |= apply_optional_update(
        &mut config.notify_service_approval_timeout_secs,
        &patch.notify_service_approval_timeout_secs,
    );
    changed |= apply_optional_update(
        &mut config.notify_service_startup_idle_timeout_secs,
        &patch.notify_service_startup_idle_timeout_secs,
    );
    changed |= apply_optional_update(
        &mut config.notify_service_events,
        &patch.notify_service_events,
    );
    changed |= apply_optional_update(
        &mut config.notify_service_user_message_content,
        &patch.notify_service_user_message_content,
    );
    changed |= apply_optional_update(
        &mut config.notify_service_user_message_preview_chars,
        &patch.notify_service_user_message_preview_chars,
    );
    changed |= apply_optional_update(
        &mut config.rate_limit_threshold_warning_mode,
        &patch.rate_limit_threshold_warning_mode,
    );
    changed |= apply_optional_update(
        &mut config.rate_limit_model_nudge_mode,
        &patch.rate_limit_model_nudge_mode,
    );
    changed |= apply_value_update(
        &mut config.desktop_notify_enabled,
        patch.desktop_notify_enabled,
    );
    changed |= apply_optional_update(&mut config.desktop_notify_port, &patch.desktop_notify_port);
    changed |= apply_optional_update(
        &mut config.desktop_notify_token,
        &patch.desktop_notify_token,
    );
    changed |= apply_value_update(&mut config.agent_bus_enabled, patch.agent_bus_enabled);
    changed |= apply_optional_update(&mut config.agent_bus_port, &patch.agent_bus_port);
    changed |= apply_optional_update(&mut config.agent_bus_token, &patch.agent_bus_token);
    Ok(changed)
}

fn apply_value_update<T: PartialEq>(current: &mut T, update: Option<T>) -> bool {
    let Some(update) = update else {
        return false;
    };
    if *current == update {
        false
    } else {
        *current = update;
        true
    }
}

fn apply_optional_update<T: Clone + PartialEq>(
    current: &mut Option<T>,
    update: &ConfigValueUpdate<T>,
) -> bool {
    match update {
        ConfigValueUpdate::Unchanged => false,
        ConfigValueUpdate::Set(value) if current.as_ref() == Some(value) => false,
        ConfigValueUpdate::Set(value) => {
            *current = Some(value.clone());
            true
        }
        ConfigValueUpdate::Clear if current.is_none() => false,
        ConfigValueUpdate::Clear => {
            *current = None;
            true
        }
    }
}

pub fn parse_optional_u64(value: &str) -> anyhow::Result<Option<u64>> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "-" {
        return Ok(None);
    }
    trimmed
        .parse::<u64>()
        .map(Some)
        .with_context(|| format!("Unsupported integer value: {value}"))
}

pub fn parse_notify_events(value: &str) -> Option<Vec<String>> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "-" {
        return None;
    }
    Some(
        trimmed
            .split(|character: char| character == ',' || character.is_whitespace())
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(|item| item.replace('-', "_"))
            .collect(),
    )
}

pub fn parse_notify_user_message_content(value: &str) -> anyhow::Result<Option<String>> {
    parse_finite_optional_value(
        value,
        &["none", "preview", "full"],
        "notify user message content mode",
    )
}

pub fn parse_rate_limit_mode(value: &str) -> anyhow::Result<Option<String>> {
    parse_finite_optional_value(
        value,
        &["off", "daily", "always"],
        "rate limit reminder mode",
    )
}

pub fn parse_desktop_notify_port(value: &str) -> anyhow::Result<Option<u16>> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "-" {
        return Ok(None);
    }
    let port = trimmed
        .parse::<u16>()
        .with_context(|| format!("Invalid desktop notify port: {value}"))?;
    validate_desktop_notify_port(port)?;
    Ok(Some(port))
}

pub fn parse_agent_bus_port(value: &str) -> anyhow::Result<Option<u16>> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "-" {
        return Ok(None);
    }
    let port = trimmed
        .parse::<u16>()
        .with_context(|| format!("Invalid Agent Bus port: {value}"))?;
    validate_agent_bus_port(port)?;
    Ok(Some(port))
}

fn parse_finite_optional_value(
    value: &str,
    supported: &[&str],
    label: &str,
) -> anyhow::Result<Option<String>> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "-" {
        return Ok(None);
    }
    let normalized = trimmed.replace('-', "_");
    if supported.contains(&normalized.as_str()) {
        Ok(Some(normalized))
    } else {
        anyhow::bail!("Unsupported {label}: {value}. Use {}", supported.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_changes_only_requested_general_and_proxy_values() {
        let mut config = CodezConfig::default();
        config.notify_service_user_message_content = Some("legacy-mode".to_string());
        config.agent_bus_token = Some("keep-secret".to_string());
        let proxy = ProxyConfig {
            enabled: true,
            url: Some("socks5h://127.0.0.1:7890".to_string()),
            no_proxy: Some("localhost".to_string()),
            force_http_transport: true,
        };
        let patch = GlobalConfigPatch {
            docker_use_sudo: Some(true),
            session_enabled: Some(false),
            proxy: ConfigValueUpdate::Set(proxy.clone()),
            ..GlobalConfigPatch::default()
        };

        assert!(apply_global_config_patch(&mut config, &patch).expect("apply patch"));
        assert!(config.docker_use_sudo);
        assert!(!config.session.enabled);
        assert_eq!(config.proxy.as_ref(), Some(&proxy));
        assert_eq!(
            config.notify_service_user_message_content.as_deref(),
            Some("legacy-mode")
        );
        assert_eq!(config.agent_bus_token.as_deref(), Some("keep-secret"));
        assert!(!apply_global_config_patch(&mut config, &patch).expect("reapply patch"));
    }

    #[test]
    fn profile_defaults_use_the_shared_typed_patch() {
        let mut config = CodezConfig {
            default_profile: Some("alpha".to_string()),
            ..CodezConfig::default()
        };
        let patch = GlobalConfigPatch {
            default_profile: ConfigValueUpdate::Set("beta".to_string()),
            default_profile_direct_launch: Some(true),
            ..GlobalConfigPatch::default()
        };

        assert!(apply_global_config_patch(&mut config, &patch).expect("apply profile defaults"));
        assert_eq!(config.default_profile.as_deref(), Some("beta"));
        assert!(config.default_profile_direct_launch);
        assert!(!apply_global_config_patch(&mut config, &patch).expect("reapply profile defaults"));

        let clear = GlobalConfigPatch {
            default_profile: ConfigValueUpdate::Clear,
            ..GlobalConfigPatch::default()
        };
        assert!(apply_global_config_patch(&mut config, &clear).expect("clear default profile"));
        assert_eq!(config.default_profile, None);
        assert!(config.default_profile_direct_launch);
    }

    #[test]
    fn claude_usage_refresh_patch_is_bounded_and_idempotent() {
        let mut config = CodezConfig::default();
        assert_eq!(config.claude_usage_refresh_minutes, 5);
        let patch = GlobalConfigPatch {
            claude_usage_refresh_minutes: Some(2),
            ..GlobalConfigPatch::default()
        };

        assert!(apply_global_config_patch(&mut config, &patch).expect("apply refresh interval"));
        assert_eq!(config.claude_usage_refresh_minutes, 2);
        assert!(!apply_global_config_patch(&mut config, &patch).expect("reapply refresh interval"));

        let invalid = GlobalConfigPatch {
            claude_usage_refresh_minutes: Some(0),
            ..GlobalConfigPatch::default()
        };
        assert!(apply_global_config_patch(&mut config, &invalid).is_err());
    }

    #[test]
    fn clear_proxy_is_idempotent_and_leaves_other_values_untouched() {
        let mut config = CodezConfig::default();
        config.session.enabled = true;
        config.proxy = Some(ProxyConfig {
            enabled: true,
            url: Some("http://127.0.0.1:7890".to_string()),
            no_proxy: None,
            force_http_transport: false,
        });
        let patch = GlobalConfigPatch {
            proxy: ConfigValueUpdate::Clear,
            ..GlobalConfigPatch::default()
        };

        assert!(apply_global_config_patch(&mut config, &patch).expect("clear proxy"));
        assert_eq!(config.proxy, None);
        assert!(!apply_global_config_patch(&mut config, &patch).expect("reclear proxy"));
        assert!(config.session.enabled);
    }

    #[test]
    fn notification_patch_updates_typed_values_and_secret_actions() {
        let mut config = CodezConfig::default();
        config.notify_service_token = Some("old-notify".to_string());
        config.desktop_notify_token = Some("old-desktop".to_string());
        let patch = GlobalConfigPatch {
            notify_service_url: ConfigValueUpdate::Set("https://notify.test/push".to_string()),
            notify_service_token: ConfigValueUpdate::Set("new-notify".to_string()),
            notify_service_idle_timeout_secs: ConfigValueUpdate::Set(90),
            notify_service_events: ConfigValueUpdate::Set(vec![
                "turn_completed".to_string(),
                "approval_requested".to_string(),
            ]),
            notify_service_user_message_content: ConfigValueUpdate::Set("preview".to_string()),
            rate_limit_threshold_warning_mode: ConfigValueUpdate::Set("daily".to_string()),
            desktop_notify_enabled: Some(true),
            desktop_notify_port: ConfigValueUpdate::Set(24251),
            desktop_notify_token: ConfigValueUpdate::Clear,
            ..GlobalConfigPatch::default()
        };

        assert!(apply_global_config_patch(&mut config, &patch).expect("apply notification patch"));
        assert_eq!(
            config.notify_service_url.as_deref(),
            Some("https://notify.test/push")
        );
        assert_eq!(config.notify_service_token.as_deref(), Some("new-notify"));
        assert_eq!(config.notify_service_idle_timeout_secs, Some(90));
        assert_eq!(
            config.notify_service_events,
            Some(vec![
                "turn_completed".to_string(),
                "approval_requested".to_string()
            ])
        );
        assert_eq!(
            config.notify_service_user_message_content.as_deref(),
            Some("preview")
        );
        assert_eq!(
            config.rate_limit_threshold_warning_mode.as_deref(),
            Some("daily")
        );
        assert!(config.desktop_notify_enabled);
        assert_eq!(config.desktop_notify_port, Some(24251));
        assert_eq!(config.desktop_notify_token, None);
    }

    #[test]
    fn notification_value_parsers_normalize_and_validate() {
        assert_eq!(parse_optional_u64(" 42 ").expect("integer"), Some(42));
        assert_eq!(parse_optional_u64("-").expect("clear integer"), None);
        assert!(parse_optional_u64("forty").is_err());
        assert_eq!(
            parse_notify_events("turn-completed, approval_requested waiting-approval"),
            Some(vec![
                "turn_completed".to_string(),
                "approval_requested".to_string(),
                "waiting_approval".to_string()
            ])
        );
        assert_eq!(
            parse_notify_user_message_content("preview").expect("message content"),
            Some("preview".to_string())
        );
        assert!(parse_notify_user_message_content("legacy").is_err());
        assert_eq!(
            parse_rate_limit_mode("daily").expect("rate mode"),
            Some("daily".to_string())
        );
        assert!(parse_rate_limit_mode("sometimes").is_err());
        assert_eq!(
            parse_desktop_notify_port("24251").expect("desktop port"),
            Some(24251)
        );
        assert!(parse_desktop_notify_port("8080").is_err());
        assert_eq!(
            parse_agent_bus_port("24261").expect("Agent Bus port"),
            Some(24261)
        );
        assert!(parse_agent_bus_port("59996").is_err());
    }

    #[test]
    fn invalid_desktop_port_rejects_the_entire_patch_before_mutation() {
        let mut config = CodezConfig::default();
        let patch = GlobalConfigPatch {
            docker_use_sudo: Some(true),
            desktop_notify_port: ConfigValueUpdate::Set(8080),
            ..GlobalConfigPatch::default()
        };

        assert!(apply_global_config_patch(&mut config, &patch).is_err());
        assert!(!config.docker_use_sudo);
        assert_eq!(config.desktop_notify_port, None);
    }

    #[test]
    fn agent_bus_patch_is_typed_and_invalid_port_is_atomic() {
        let mut config = CodezConfig::default();
        config.agent_bus_enabled = false;
        config.agent_bus_token = Some("old-bus-secret".to_string());
        let patch = GlobalConfigPatch {
            agent_bus_enabled: Some(true),
            agent_bus_port: ConfigValueUpdate::Set(24261),
            agent_bus_token: ConfigValueUpdate::Set("new-bus-secret".to_string()),
            ..GlobalConfigPatch::default()
        };

        assert!(apply_global_config_patch(&mut config, &patch).expect("apply Agent Bus patch"));
        assert!(config.agent_bus_enabled);
        assert_eq!(config.agent_bus_port, Some(24261));
        assert_eq!(config.agent_bus_token.as_deref(), Some("new-bus-secret"));

        let mut unchanged = CodezConfig::default();
        let invalid = GlobalConfigPatch {
            docker_use_sudo: Some(true),
            agent_bus_port: ConfigValueUpdate::Set(59996),
            ..GlobalConfigPatch::default()
        };
        assert!(apply_global_config_patch(&mut unchanged, &invalid).is_err());
        assert!(!unchanged.docker_use_sudo);
        assert_eq!(unchanged.agent_bus_port, None);
    }
}
