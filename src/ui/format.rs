//! Formatting helpers for human-facing terminal output.

use crate::profiles::model::ProxyConfig;
use serde_json::Value;

pub fn optional_label(value: Option<&str>) -> String {
    value
        .filter(|value| !value.is_empty())
        .unwrap_or("-")
        .to_string()
}

pub fn optional_u64_label(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_string())
}

pub fn truncate_end(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    let mut output = value.chars().take(max_chars - 3).collect::<String>();
    output.push_str("...");
    output
}

pub fn truncate_middle(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }
    if max_chars <= 3 {
        return truncate_end(value, max_chars);
    }
    let left = (max_chars - 3) / 2;
    let right = max_chars - 3 - left;
    let prefix = value.chars().take(left).collect::<String>();
    let suffix = value
        .chars()
        .rev()
        .take(right)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!("{prefix}...{suffix}")
}

pub fn compact_home_path(path: &str) -> String {
    let Some(home) = std::env::var_os("HOME")
        .and_then(|value| value.into_string().ok())
        .filter(|value| !value.is_empty())
    else {
        return path.to_string();
    };
    if path == home {
        return "~".to_string();
    }
    path.strip_prefix(&(home + "/"))
        .map(|suffix| format!("~/{suffix}"))
        .unwrap_or_else(|| path.to_string())
}

pub fn bool_label(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

pub fn proxy_config_label(proxy: Option<&ProxyConfig>) -> String {
    match proxy {
        Some(proxy) if !proxy.enabled => "disabled".to_string(),
        Some(proxy) => {
            let url = proxy.url.as_deref().unwrap_or("<missing-url>");
            let no_proxy = proxy.no_proxy.as_deref().unwrap_or("-");
            format!(
                "enabled url={url} no_proxy={no_proxy} force_http={}",
                bool_label(proxy.force_http_transport)
            )
        }
        None => "inherit/none".to_string(),
    }
}

pub fn compact_json_value(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unprintable>".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_json_value_serializes_on_one_line() {
        let value = serde_json::json!({
            "kind": "runtime",
            "args": ["a", "b"]
        });

        assert_eq!(
            compact_json_value(&value),
            r#"{"args":["a","b"],"kind":"runtime"}"#
        );
    }
}
