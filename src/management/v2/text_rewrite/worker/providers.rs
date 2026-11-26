use std::{io::Read, path::Path, time::Duration};

use serde_json::{json, Value};

use crate::{
    config::proxy_outbound::OutboundProxy,
    management::v2::profile_quota_native,
    profiles::{
        model::{CliKind, StoredAccount},
        settings_api,
    },
    runtime::{claude::ClaudeDeployment, claude_accounts},
};

use super::super::INSTRUCTIONS;
use super::{Answer, Failure};

mod subscriptions;

#[derive(Clone, Copy)]
enum Wire {
    Responses,
    Messages,
}

pub(super) fn uses_native_subscription(profile: &StoredAccount) -> bool {
    profile.cli_kind == CliKind::Codex
        && profile_quota_native::openai_codex_profile(profile)
        && crate::profiles::inspect::account_uses_chatgpt_auth(profile)
}

pub(super) fn run(profile: &StoredAccount, draft: &str, work: &Path) -> Result<Answer, Failure> {
    let proxy = OutboundProxy::for_profile(profile, &crate::config::store::load_codez_config());
    match profile.cli_kind {
        CliKind::Codex if uses_native_subscription(profile) => {
            subscriptions::native(profile, draft, work, &proxy)
        }
        CliKind::Codex => {
            let base = crate::profiles::inspect::account_model_api_base(profile)
                .ok_or(Failure::ProfileUnavailable)?;
            let endpoint = crate::agent_bus::web_search::responses_url(&base)
                .map_err(|_| Failure::ProfileUnavailable)?;
            let key = crate::profiles::catalog_api::api_key(profile)
                .map_err(|_| Failure::Authentication)?;
            let model = settings_api::codex_snapshot(profile)
                .model
                .filter(|s| !s.is_empty())
                .ok_or(Failure::ProfileUnavailable)?;
            let model = crate::profiles::catalog_api::load(profile)
                .ok()
                .and_then(|state| state.catalog)
                .and_then(|catalog| {
                    catalog["models"]
                        .as_array()?
                        .iter()
                        .find(|row| row["slug"].as_str() == Some(&model))
                        .and_then(|row| row["request_model"].as_str().map(str::to_owned))
                })
                .unwrap_or(model);
            let host = endpoint
                .host_str()
                .filter(|host| !host.is_empty())
                .ok_or(Failure::ProfileUnavailable)?;
            let provider = if host == "api.openai.com" {
                "openai"
            } else {
                "responses"
            };
            api(
                Wire::Responses,
                &endpoint,
                "Authorization",
                &format!("Bearer {key}"),
                &model,
                provider,
                draft,
                &proxy,
            )
        }
        CliKind::Claude => {
            // The parent already resolved this exact profile. Keep its account
            // binding instead of loading every profile again in the worker.
            let id = profile
                .claude_account
                .clone()
                .unwrap_or_else(|| claude_accounts::DEFAULT_ACCOUNT_ID.to_owned());
            let registry = claude_accounts::load().map_err(|_| Failure::ProfileUnavailable)?;
            if let Some(account) = registry
                .find(&id)
                .filter(|account| account.kind == claude_accounts::ClaudeAccountKind::Api)
            {
                let config = account.api.clone().unwrap_or_default();
                config.validate().map_err(|_| Failure::ProfileUnavailable)?;
                let base = config
                    .base_url
                    .as_deref()
                    .unwrap_or("https://api.anthropic.com");
                let mut endpoint =
                    url::Url::parse(base).map_err(|_| Failure::ProfileUnavailable)?;
                let prefix = endpoint.path().trim_end_matches('/');
                let path = if prefix.ends_with("/v1") {
                    format!("{prefix}/messages")
                } else {
                    format!("{prefix}/v1/messages")
                };
                endpoint.set_path(&path);
                let key_path =
                    claude_accounts::key_path(&id).map_err(|_| Failure::Authentication)?;
                let record: Value = serde_json::from_slice(
                    &std::fs::read(key_path).map_err(|_| Failure::Authentication)?,
                )
                .map_err(|_| Failure::Authentication)?;
                let key = crate::profiles::stored_keys::SecretKey::new(
                    record["apiKey"].as_str().ok_or(Failure::Authentication)?,
                )
                .map_err(|_| Failure::Authentication)?;
                let model = if config.base_url.is_none() {
                    "claude-haiku-5-5"
                } else {
                    profile.options.claude.model.as_str()
                };
                let (header, value) = match config.key_env {
                    claude_accounts::ApiKeyEnv::ApiKey => ("x-api-key", key.expose().to_owned()),
                    claude_accounts::ApiKeyEnv::AuthToken => {
                        ("Authorization", format!("Bearer {}", key.expose()))
                    }
                };
                return api(
                    Wire::Messages,
                    &endpoint,
                    header,
                    &value,
                    model,
                    "anthropic",
                    draft,
                    &proxy,
                );
            }
            let deployment =
                ClaudeDeployment::selected().map_err(|_| Failure::ProfileUnavailable)?;
            let (_, home) =
                claude_accounts::home_for_profile(Some(&profile.id), &deployment.claude_home)
                    .map_err(|_| Failure::Authentication)?;
            subscriptions::claude(&deployment, &home, draft, work, &proxy)
        }
        CliKind::Dsh | CliKind::Kimi => Err(Failure::ProfileUnavailable),
    }
}

pub(super) fn run_feature(
    profile: crate::management::v2::feature_profiles::Provider,
    draft: &str,
) -> Result<Answer, Failure> {
    use crate::management::v2::feature_profiles::Protocol;
    let config = crate::config::store::load_codez_config();
    let proxy = OutboundProxy::from_config(config.proxy.as_ref());
    let (wire, header, credential, provider) = match profile.protocol {
        Protocol::Responses => (
            Wire::Responses,
            "Authorization",
            format!("Bearer {}", profile.key.expose()),
            if profile.endpoint.host_str() == Some("api.openai.com") {
                "openai"
            } else {
                "responses"
            },
        ),
        Protocol::Messages => (
            Wire::Messages,
            "x-api-key",
            profile.key.expose().to_string(),
            "anthropic",
        ),
    };
    api(
        wire,
        &profile.endpoint,
        header,
        &credential,
        &profile.model,
        provider,
        draft,
        &proxy,
    )
}

fn describe_error(wire: Wire, status: u16, error: &Value) -> Failure {
    fn flag(error: &Value, names: &[&str]) -> bool {
        ["code", "type"].iter().any(|key| {
            error[*key]
                .as_str()
                .is_some_and(|value| names.contains(&value))
        })
    }
    let messages_credit_exhausted = matches!(wire, Wire::Messages)
        && status == 400
        && error["type"] == "invalid_request_error"
        && error["message"]
            .as_str()
            .is_some_and(|message| message.to_ascii_lowercase().contains("credit balance"));
    if flag(
        error,
        &["content_filter", "content_policy_violation", "refusal"],
    ) {
        Failure::ContentRefused
    } else if messages_credit_exhausted
        || (matches!(status, 400 | 402 | 429)
            && flag(
                error,
                &[
                    "insufficient_quota",
                    "billing_error",
                    "billing_hard_limit_reached",
                    "credit_balance_too_low",
                ],
            ))
    {
        Failure::QuotaExhausted
    } else if status == 401
        || (status == 403
            && flag(
                error,
                &["authentication_error", "invalid_api_key", "token_expired"],
            ))
    {
        Failure::Authentication
    } else if status == 429 {
        Failure::RateLimited
    } else {
        Failure::Upstream
    }
}

fn api(
    wire: Wire,
    endpoint: &url::Url,
    auth_header: &str,
    credential: &str,
    model: &str,
    provider: &str,
    draft: &str,
    proxy: &OutboundProxy,
) -> Result<Answer, Failure> {
    if model.trim().is_empty() {
        return Err(Failure::ProfileUnavailable);
    }
    let body = match wire {
        Wire::Responses => {
            let mut body = json!({"model":model, "instructions":INSTRUCTIONS, "input":draft,
                "tools":[], "stream":false, "store":false, "max_output_tokens":6144});
            if provider == "openai" && model.starts_with("gpt-5") {
                body["reasoning"] = json!({"effort":"low"});
            }
            body
        }
        Wire::Messages => {
            json!({"model":model,"system":INSTRUCTIONS,"messages":[{"role":"user","content":draft}],
            "tools":[],"stream":false,"max_tokens":6144})
        }
    };
    let agent = crate::profiles::model_discovery::provider_agent(Duration::from_secs(10), proxy)
        .map_err(|_| Failure::ProfileUnavailable)?;
    let mut request = agent
        .post(endpoint.as_str())
        .header(auth_header, credential);
    if matches!(wire, Wire::Messages) {
        request = request.header("anthropic-version", "2023-06-01");
    }
    let mut reply = request.send_json(body).map_err(|error| match error {
        ureq::Error::Timeout(_) => Failure::Timeout,
        _ => Failure::Upstream,
    })?;
    let status = reply.status().as_u16();
    let mut bytes = Vec::new();
    reply
        .body_mut()
        .as_reader()
        .take(super::MAX_WIRE + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| describe_error(wire, status, &Value::Null))?;
    if bytes.len() as u64 > super::MAX_WIRE {
        return Err(Failure::InvalidOutput);
    }
    let value = serde_json::from_slice::<Value>(&bytes).ok().or_else(|| {
        std::str::from_utf8(&bytes)
            .ok()?
            .lines()
            .filter_map(|line| {
                let event: Value = serde_json::from_str(line.strip_prefix("data:")?.trim()).ok()?;
                match event["type"].as_str() {
                    Some("response.completed" | "response.failed" | "response.incomplete") => {
                        Some(event["response"].clone())
                    }
                    _ => None,
                }
            })
            .next_back()
    });
    if !(200..300).contains(&status) {
        return Err(describe_error(
            wire,
            status,
            &value.unwrap_or_default()["error"],
        ));
    }
    let value = value.ok_or(Failure::InvalidOutput)?;
    let text = extract(wire, &value)?;
    Ok(Answer {
        text,
        model: value["model"].as_str().map(str::to_owned),
        requested_model: model.into(),
        provider: provider.into(),
        startup_ms: None,
    })
}

fn extract(wire: Wire, value: &Value) -> Result<String, Failure> {
    if !value["error"].is_null() {
        return Err(describe_error(wire, 200, &value["error"]));
    }
    let mut text = String::new();
    let mut append = |rows: &Value, text_kind: &str| -> Result<(), Failure> {
        for row in rows.as_array().ok_or(Failure::InvalidOutput)? {
            match row["type"].as_str() {
                Some("refusal") => return Err(Failure::ContentRefused),
                Some(kind) if kind == text_kind => {
                    text.push_str(row["text"].as_str().ok_or(Failure::InvalidOutput)?)
                }
                _ => return Err(Failure::InvalidOutput),
            }
        }
        Ok(())
    };
    match wire {
        Wire::Responses => {
            if value["status"] != "completed" {
                return Err(
                    if value["incomplete_details"]["reason"] == "content_filter" {
                        Failure::ContentRefused
                    } else {
                        Failure::InvalidOutput
                    },
                );
            }
            for item in value["output"].as_array().ok_or(Failure::InvalidOutput)? {
                match item["type"].as_str() {
                    Some("reasoning") => {}
                    Some("message") if item["role"] == "assistant" => {
                        append(&item["content"], "output_text")?
                    }
                    _ => return Err(Failure::InvalidOutput),
                }
            }
        }
        Wire::Messages => {
            if value["stop_reason"] == "refusal" {
                return Err(Failure::ContentRefused);
            }
            if value["stop_reason"] != "end_turn" {
                return Err(Failure::InvalidOutput);
            }
            append(&value["content"], "text")?;
        }
    }
    if text.is_empty() {
        Err(Failure::InvalidOutput)
    } else {
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn feature_profiles_use_both_api_protocols_without_a_runtime() {
        use crate::management::v2::feature_profiles::{endpoint, Protocol, Provider};
        use std::net::TcpListener;

        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        for (protocol, expected_path, auth_header, authorization, answer) in [
            (
                Protocol::Responses,
                "/v1/responses",
                "authorization",
                "Bearer fixture-key",
                json!({"status":"completed","model":"fixture","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"result"}]}]}),
            ),
            (
                Protocol::Messages,
                "/v1/messages",
                "x-api-key",
                "fixture-key",
                json!({"stop_reason":"end_turn","model":"fixture","content":[{"type":"text","text":"result"}]}),
            ),
        ] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let base = format!("http://{}/v1", listener.local_addr().unwrap());
            listener.set_nonblocking(true).unwrap();
            let server = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "provider worker did not connect"
                            );
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("fixture accept: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let request = crate::http::server::read_simple_http_request(&mut stream).unwrap();
                assert_eq!(request.method, "POST");
                assert_eq!(request.path, expected_path);
                assert!(request
                    .headers
                    .get(auth_header)
                    .is_some_and(|value| value == authorization));
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(body["model"], "fixture");
                assert_eq!(body["tools"], json!([]));
                assert!(body.to_string().contains("masked-draft"));
                crate::http::server::write_json_response(&mut stream, 200, "OK", &answer).unwrap();
            });
            let result = run_feature(
                Provider {
                    protocol,
                    endpoint: endpoint(protocol, &base).unwrap(),
                    model: "fixture".into(),
                    key: crate::profiles::stored_keys::SecretKey::new("fixture-key")
                        .unwrap(),
                },
                "masked-draft",
            );
            server.join().unwrap();
            let answer = result.unwrap();
            assert_eq!(answer.text, "result");
            assert_eq!(answer.requested_model, "fixture");
        }
    }

    #[test]
    fn statuses_are_allow_listed_and_content_wins() {
        for (code, body, expected) in [
            (429, json!({}), Failure::RateLimited),
            (
                429,
                json!({"type":"insufficient_quota"}),
                Failure::QuotaExhausted,
            ),
            (
                429,
                json!({"type":"content_policy_violation"}),
                Failure::ContentRefused,
            ),
            (401, json!({}), Failure::Authentication),
            (403, json!({"code":"model_denied"}), Failure::Upstream),
            (500, json!({"code":"insufficient_quota"}), Failure::Upstream),
        ] {
            assert_eq!(describe_error(Wire::Responses, code, &body), expected);
        }
    }
    #[test]
    fn messages_credit_balance_error_allows_fallback() {
        let exhausted = json!({"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API."});
        let category = describe_error(Wire::Messages, 400, &exhausted);
        assert_eq!(category, Failure::QuotaExhausted);
        assert!(category.may_fallback());
        for (wire, status, error) in [
            (Wire::Responses, 400, exhausted.clone()),
            (Wire::Messages, 500, exhausted),
            (
                Wire::Messages,
                400,
                json!({"type":"invalid_request_error","message":"Invalid model."}),
            ),
            (
                Wire::Messages,
                400,
                json!({"type":"other_error","message":"credit balance is low"}),
            ),
        ] {
            assert_eq!(describe_error(wire, status, &error), Failure::Upstream);
        }
    }

    #[test]
    fn protocol_rejects_tool_results_and_content_refusal() {
        assert!(matches!(
            extract(
                Wire::Responses,
                &json!({"status":"completed","output":[{"type":"function_call"}]})
            ),
            Err(Failure::InvalidOutput)
        ));
        assert!(matches!(
            extract(
                Wire::Messages,
                &json!({"stop_reason":"refusal","content":[]})
            ),
            Err(Failure::ContentRefused)
        ));
    }
}
