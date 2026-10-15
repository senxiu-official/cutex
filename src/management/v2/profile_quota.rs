//! Root-only per-profile quota routes:
//! `GET /v2/profiles/{profile}/quota[?refresh=true]`,
//! `GET|PUT /v2/profiles/{profile}/quota-config`.
//! `{profile}` is a profile ID or exact name (percent-encoded).
//!
//! The gauges come from the profile's quota-config (HTTP reader) when one is
//! enabled, else, for ChatGPT-login profiles, from the native rate limits
//! (`profile_quota_native`), for Claude profiles from the subscription usage
//! (`profile_quota_claude`); `source` says which (`quotaConfig` | `native` |
//! `claude` | null) and `nativeQuota` carries the native snapshot metadata.

use std::net::TcpStream;

use serde::Deserialize;
use serde_json::json;

use crate::http::server::write_json_response;
use crate::http::server::SimpleHttpRequest;
use crate::profiles::quota;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileQuotaRoute {
    Gauges,
    Config,
}

/// `(profile, route)` for `/v2/profiles/{profile}/quota[-config]`.
pub fn parse_path(path: &str) -> Option<(String, ProfileQuotaRoute)> {
    let rest = path.strip_prefix("/v2/profiles/")?;
    let (encoded, route) = if let Some(encoded) = rest.strip_suffix("/quota-config") {
        (encoded, ProfileQuotaRoute::Config)
    } else {
        (rest.strip_suffix("/quota")?, ProfileQuotaRoute::Gauges)
    };
    if encoded.is_empty() || encoded.contains('/') {
        return None;
    }
    let decoded = percent_decode(encoded)?;
    (!decoded.is_empty() && !decoded.contains('/') && !decoded.chars().any(char::is_control))
        .then_some((decoded, route))
}

pub(super) fn percent_decode(encoded: &str) -> Option<String> {
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = encoded.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QuotaConfigUpdate {
    /// `custom` or a reader name (`quota::Reader`); `auto` is refused.
    #[serde(default)]
    preset: Option<String>,
    #[serde(default)]
    primary: Option<quota::Config>,
    #[serde(default)]
    extras: Option<Vec<quota::Config>>,
}

impl QuotaConfigUpdate {
    fn unknown_field(&self) -> Option<String> {
        self.primary
            .iter()
            .chain(self.extras.iter().flatten())
            .find_map(|config| config.extra.keys().next().cloned())
    }
}

fn refresh_requested(path: &str) -> Result<bool, String> {
    let Some((_, query)) = path.split_once('?') else {
        return Ok(false);
    };
    let mut refresh = false;
    for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match (name.as_ref(), value.as_ref()) {
            ("refresh", "true") => refresh = true,
            ("refresh", "false") => refresh = false,
            ("refresh", other) => return Err(format!("refresh must be true or false, got {other}")),
            (other, _) => return Err(format!("unsupported query parameter: {other}")),
        }
    }
    Ok(refresh)
}

pub(super) fn handle(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    profile: &str,
    route: ProfileQuotaRoute,
) -> anyhow::Result<()> {
    let bad_request = |stream: &mut TcpStream, message: &str| {
        super::server::write_v2_error(
            stream,
            400,
            "Bad Request",
            "invalid_request",
            message,
            false,
            json!({}),
        )
    };
    let account = match quota::find_profile(profile) {
        Ok(Some(account)) => account,
        Ok(None) => return super::profiles_api::not_found(stream, profile),
        Err(error) => {
            return super::server::write_v2_error(
                stream,
                503,
                "Service Unavailable",
                "profile_store_unavailable",
                "the profile store could not be read",
                true,
                json!({ "diagnostic": format!("{error:#}") }),
            )
        }
    };
    match (request.method.as_str(), route) {
        ("GET", ProfileQuotaRoute::Gauges) => {
            let refresh = match refresh_requested(&request.path) {
                Ok(refresh) => refresh,
                Err(message) => return bad_request(stream, &message),
            };
            // Precedence: an enabled quota-config (HTTP reader) wins; native
            // rate limits serve ChatGPT-login profiles without one.
            let native = super::profile_quota_native::native_quota_applies(&account);
            let claude = super::profile_quota_claude::claude_quota_applies(&account);
            // Claude: the profile's Claude account decides (subscription
            // windows = "claude", API usage totals = "claudeApi").
            let mut claude_source = "claude";
            let read = if native {
                super::profile_quota_native::quota_view(&account, refresh)
                    .map(|(gauges, meta)| (gauges, Some(meta)))
            } else if claude {
                super::profile_quota_claude::quota_view(&account, refresh).map(|(gauges, meta, source)| {
                    claude_source = source;
                    (gauges, Some(meta))
                })
            } else {
                quota::gauges(&account, refresh).map(|gauges| (gauges, None))
            };
            let source = if native {
                Some("native")
            } else if claude {
                Some(claude_source)
            } else if super::profile_quota_native::has_explicit_quota_config(&account) {
                Some("quotaConfig")
            } else {
                None
            };
            match read {
                Ok((gauges, native_quota)) => write_json_response(
                    stream,
                    200,
                    "OK",
                    &json!({
                        "contractVersion": 2,
                        "profile": { "id": account.id, "name": account.name },
                        "gauges": gauges,
                        "source": source,
                        "nativeQuota": native_quota,
                    }),
                ),
                Err(error) => super::server::write_v2_error(
                    stream,
                    500,
                    "Internal Server Error",
                    "quota_read_failed",
                    &format!("{error:#}"),
                    true,
                    json!({}),
                ),
            }
        }
        ("GET", ProfileQuotaRoute::Config) => {
            write_json_response(stream, 200, "OK", &quota::config_view(&account))
        }
        ("PUT", ProfileQuotaRoute::Config) => {
            let update: QuotaConfigUpdate = match serde_json::from_slice(&request.body) {
                Ok(update) => update,
                Err(error) => {
                    return bad_request(stream, &format!("strict quota-config body required: {error}"))
                }
            };
            // The stored config tolerates unknown fields (persistence audit);
            // this API input stays strict.
            if let Some(field) = update.unknown_field() {
                return bad_request(stream, &format!("strict quota-config body required: unknown field `{field}`"));
            }
            let preset = match update.preset.as_deref().map(quota::Choice::parse_settable).transpose() {
                Ok(preset) => preset.unwrap_or_else(|| quota::Choice::of(&account)),
                Err(message) => return bad_request(stream, &message),
            };
            match quota::update_config(
                &account.id,
                preset,
                update.primary.unwrap_or_else(|| account.options.quota.clone()),
                update.extras.unwrap_or_else(|| account.options.quotas.clone()),
            ) {
                Ok(updated) => write_json_response(stream, 200, "OK", &quota::config_view(&updated)),
                Err(error) => bad_request(stream, &format!("{error:#}")),
            }
        }
        _ => super::server::write_v2_error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "use GET for quota, GET or PUT for quota-config",
            false,
            json!({}),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_paths_decode_profile_and_reject_traversal() {
        assert_eq!(
            parse_path("/v2/profiles/GLM/quota"),
            Some(("GLM".to_string(), ProfileQuotaRoute::Gauges))
        );
        assert_eq!(
            parse_path("/v2/profiles/my%20glm/quota-config"),
            Some(("my glm".to_string(), ProfileQuotaRoute::Config))
        );
        assert_eq!(parse_path("/v2/profiles//quota"), None);
        assert_eq!(parse_path("/v2/profiles/a/b/quota"), None);
        assert_eq!(parse_path("/v2/profiles/a%2Fb/quota"), None);
        assert_eq!(parse_path("/v2/profiles/a/other"), None);
        assert_eq!(parse_path("/v2/profiles/%zz/quota"), None);
        assert_eq!(refresh_requested("/x?refresh=true"), Ok(true));
        assert_eq!(refresh_requested("/x"), Ok(false));
        assert!(refresh_requested("/x?refresh=1").is_err());
        assert!(refresh_requested("/x?other=1").is_err());
    }

    #[test]
    fn quota_config_update_is_strict() {
        let update: QuotaConfigUpdate = serde_json::from_value(json!({
            "preset": "usage_endpoint",
            "extras": [{ "enabled": true, "label": "GLMw", "window_label": "week",
                         "value_pointer": "/rate_limits/1/remaining",
                         "limit_pointer": "/rate_limits/1/limit",
                         "reset_pointer": "/rate_limits/1/reset_at" }]
        }))
        .unwrap();
        assert_eq!(update.preset.as_deref(), Some("usage_endpoint"));
        assert_eq!(
            quota::Choice::parse_settable(update.preset.as_deref().unwrap()),
            Ok(quota::Choice::Reader(quota::Reader::UsageEndpoint))
        );
        assert!(quota::Choice::parse_settable("auto").is_err());
        assert_eq!(update.extras.unwrap()[0].window_label.as_deref(), Some("week"));
        assert!(serde_json::from_value::<QuotaConfigUpdate>(json!({ "secret": 1 })).is_err());
        let nested: QuotaConfigUpdate = serde_json::from_value(json!({ "primary": { "secret": 1 } })).unwrap();
        assert_eq!(nested.unknown_field().as_deref(), Some("secret"));
        let extras: QuotaConfigUpdate = serde_json::from_value(json!({ "extras": [{}, { "x": 1 }] })).unwrap();
        assert_eq!(extras.unknown_field().as_deref(), Some("x"));
    }
}
