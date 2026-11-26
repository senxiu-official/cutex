//! Owner-managed, app-side API credentials. The agent Profile store never reads this directory.
use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    net::TcpStream,
    path::{Path, PathBuf},
};

use anyhow::ensure;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::profiles::stored_keys::SecretKey;

pub(super) const PATH: &str = "/v2/feature-profiles";
pub const DIR: &str = "feature-profiles";
const PREFIX: &str = "fp-";
const MAX_BYTES: u64 = 16 * 1024;
const MAX_ENTRIES: usize = 128;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub(super) enum Protocol {
    #[serde(rename = "openaiResponses")]
    Responses,
    #[serde(rename = "anthropicMessages")]
    Messages,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
enum QuotaType {
    SubscriptionWindow,
    PeriodicAllowance,
    Balance,
    PayAsYouGo,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Record {
    id: String,
    name: String,
    protocol: Protocol,
    base_url: String,
    model: String,
    quota_type: QuotaType,
    api_key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Create {
    name: String,
    protocol: Protocol,
    base_url: String,
    model: String,
    quota_type: QuotaType,
    api_key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Patch {
    name: Option<String>,
    protocol: Option<Protocol>,
    base_url: Option<String>,
    model: Option<String>,
    quota_type: Option<QuotaType>,
    api_key: Option<String>,
}

/// No storage lookup. Reserved syntactic IDs receive one non-disclosing refusal
/// even if they were never allocated or have been deleted.
pub(crate) fn is_reserved(id: &str) -> bool {
    id.trim()
        .strip_prefix(PREFIX)
        .is_some_and(|suffix| Uuid::parse_str(suffix).is_ok_and(|uuid| uuid.to_string() == suffix))
}

pub const AGENT_REFUSAL_CODE: &str = "feature_profile_unavailable";
pub const AGENT_REFUSAL_MESSAGE: &str = "The requested Profile is unavailable for agent use.";

#[derive(Debug)]
pub struct AgentRefusal;

impl std::fmt::Display for AgentRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(AGENT_REFUSAL_MESSAGE)
    }
}

impl std::error::Error for AgentRefusal {}

impl AgentRefusal {
    pub fn user_input_error(&self) -> super::user_input::UserInputExecutionError {
        super::user_input::UserInputExecutionError {
            stage: "route".into(),
            code: AGENT_REFUSAL_CODE.into(),
            message: AGENT_REFUSAL_MESSAGE.into(),
            retryable: false,
            outcome_unknown: false,
            details: json!({}),
        }
    }
}

pub fn require_agent_id(id: &str) -> anyhow::Result<()> {
    if is_reserved(id) {
        return Err(AgentRefusal.into());
    }
    Ok(())
}

pub(super) fn is_path(path: &str) -> bool {
    path == PATH
        || path
            .strip_prefix(PATH)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn root() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::config_dir()?.join(DIR))
}

fn write_lock(root: &Path) -> anyhow::Result<File> {
    crate::management::client_tokens::ensure_private_dir(root)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".write.lock"))?;
    fs2::FileExt::lock_exclusive(&file)?;
    Ok(file)
}

fn path(root: &Path, id: &str) -> anyhow::Result<PathBuf> {
    ensure!(
        id == id.trim() && is_reserved(id),
        "invalid feature profile id"
    );
    Ok(root.join(format!("{id}.json")))
}

fn read(root: &Path, id: &str) -> anyhow::Result<Option<Record>> {
    let file = match File::open(path(root, id)?) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "feature profile exceeds its size limit"
    );
    let record: Record = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid feature profile record"))?;
    ensure!(record.id == id, "feature profile binding changed");
    Ok(Some(record))
}

#[derive(Default)]
struct Inventory {
    profiles: Vec<Record>,
    errors: Vec<String>,
}

fn records(root: &Path) -> anyhow::Result<Inventory> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Inventory::default()),
        Err(error) => return Err(error.into()),
    };
    let mut inventory = Inventory::default();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|name| name.strip_suffix(".json"))
            .filter(|id| id == &id.trim() && is_reserved(id))
        else {
            continue;
        };
        match read(root, id) {
            Ok(Some(record)) => {
                ensure!(
                    inventory.profiles.len() < MAX_ENTRIES,
                    "feature profile limit exceeded"
                );
                inventory.profiles.push(record);
            }
            Ok(None) => {}
            Err(_) => inventory.errors.push(id.to_owned()),
        }
    }
    inventory
        .profiles
        .sort_unstable_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    inventory.errors.sort_unstable();
    Ok(inventory)
}

fn clean(record: &mut Record) -> anyhow::Result<()> {
    record.name = record.name.trim().to_string();
    ensure!(
        !record.name.is_empty()
            && record.name.chars().count() <= 64
            && !record.name.chars().any(char::is_control),
        "invalid profile name"
    );
    record.model = record.model.trim().to_string();
    ensure!(
        !record.model.is_empty()
            && record.model.len() <= 256
            && !record
                .model
                .chars()
                .any(|c| c.is_whitespace() || c.is_control()),
        "invalid model"
    );
    record.base_url = record.base_url.trim().to_string();
    endpoint(record.protocol, &record.base_url)?;
    record.api_key = SecretKey::new(&record.api_key)?.expose().to_string();
    Ok(())
}

fn view(record: &Record) -> Value {
    json!({
        "id":record.id, "name":record.name, "protocol":record.protocol,
        "baseUrl":record.base_url, "model":record.model, "quotaType":record.quota_type,
        "keyPresent":true
    })
}

const CONTROL_PORT_MESSAGE: &str = "Feature Profiles cannot connect to local Cutex control ports";

#[derive(Debug)]
struct ControlPort;

impl fmt::Display for ControlPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(CONTROL_PORT_MESSAGE)
    }
}

impl std::error::Error for ControlPort {}

pub(super) fn endpoint(protocol: Protocol, base: &str) -> anyhow::Result<url::Url> {
    ensure!(base.len() <= 2048, "invalid API base URL");
    let mut url = url::Url::parse(base).map_err(|_| anyhow::anyhow!("invalid API base URL"))?;
    let (loopback, unspecified) = match url.host() {
        Some(url::Host::Domain(name)) => (
            name.trim_end_matches('.').eq_ignore_ascii_case("localhost"),
            false,
        ),
        Some(url::Host::Ipv4(ip)) => (ip.is_loopback(), ip.is_unspecified()),
        Some(url::Host::Ipv6(ip)) => match ip.to_ipv4_mapped() {
            Some(ip) => (ip.is_loopback(), ip.is_unspecified()),
            None => (ip.is_loopback(), ip.is_unspecified()),
        },
        None => (false, false),
    };
    ensure!(
        url.host().is_some()
            && (url.scheme() == "https" || (url.scheme() == "http" && loopback))
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "invalid API base URL"
    );
    if loopback || unspecified {
        let config = crate::config::store::load_codez_config_checked()?;
        let management = crate::management::service::management_port(&config);
        let bus = crate::agent_bus::service::agent_bus_port(&config);
        let port = url.port_or_known_default().expect("validated HTTP scheme");
        let is_listener = config
            .management_listen
            .iter()
            .flatten()
            .filter_map(|spec| {
                crate::management::listeners::ListenSpec::parse(spec, management).ok()
            })
            .any(|spec| spec.port() == port);
        if port == management || port == bus || is_listener {
            return Err(ControlPort.into());
        }
    }
    let prefix = url.path().trim_end_matches('/');
    let path = match protocol {
        Protocol::Responses => format!("{prefix}/responses"),
        Protocol::Messages if prefix.ends_with("/v1") => format!("{prefix}/messages"),
        Protocol::Messages => format!("{prefix}/v1/messages"),
    };
    url.set_path(&path);
    Ok(url)
}

/// Only the isolated rewrite worker loads this protected credential.
pub(super) struct Provider {
    pub protocol: Protocol,
    pub endpoint: url::Url,
    pub model: String,
    pub key: SecretKey,
}

pub(super) fn provider(id: &str) -> anyhow::Result<Option<Provider>> {
    read(&root()?, id)?
        .map(|record| {
            Ok(Provider {
                protocol: record.protocol,
                endpoint: endpoint(record.protocol, &record.base_url)?,
                model: record.model,
                key: SecretKey::new(&record.api_key)?,
            })
        })
        .transpose()
}

pub(super) fn exists(id: &str) -> anyhow::Result<bool> {
    match fs::metadata(path(&root()?, id)?) {
        Ok(metadata) => Ok(metadata.is_file() && metadata.len() <= MAX_BYTES),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

enum Failure {
    Invalid,
    ControlPort,
    Missing,
    Full,
    Storage,
}

fn storage<T>(result: anyhow::Result<T>) -> Result<T, Failure> {
    result.map_err(|_| Failure::Storage)
}

fn apply(method: &str, id: Option<&str>, body: &[u8]) -> Result<(u16, Value), Failure> {
    let root = storage(root())?;
    if method == "GET" {
        return if let Some(id) = id {
            let record = storage(read(&root, id))?.ok_or(Failure::Missing)?;
            Ok((200, json!({"contractVersion":2,"profile":view(&record)})))
        } else {
            let inventory = storage(records(&root))?;
            let profiles = inventory.profiles.iter().map(view).collect::<Vec<_>>();
            let errors = inventory
                .errors
                .iter()
                .map(|id| json!({"id":id,"code":"invalid_feature_profile_record"}))
                .collect::<Vec<_>>();
            Ok((
                200,
                json!({"contractVersion":2,"profiles":profiles,"errors":errors}),
            ))
        };
    }
    if body.len() as u64 > MAX_BYTES {
        return Err(Failure::Invalid);
    }
    let _lock = storage(write_lock(&root))?;
    let mut record = match (method, id) {
        ("POST", None) => {
            let input: Create = serde_json::from_slice(body).map_err(|_| Failure::Invalid)?;
            if storage(records(&root))?.profiles.len() >= MAX_ENTRIES {
                return Err(Failure::Full);
            }
            Record {
                id: format!("{PREFIX}{}", Uuid::new_v4()),
                name: input.name,
                protocol: input.protocol,
                base_url: input.base_url,
                model: input.model,
                quota_type: input.quota_type,
                api_key: input.api_key,
            }
        }
        ("PATCH", Some(id)) => {
            let value: Value = serde_json::from_slice(body).map_err(|_| Failure::Invalid)?;
            let fields = value.as_object().ok_or(Failure::Invalid)?;
            if fields.is_empty() || fields.values().any(Value::is_null) {
                return Err(Failure::Invalid);
            }
            let patch: Patch = serde_json::from_value(value).map_err(|_| Failure::Invalid)?;
            let mut record = storage(read(&root, id))?.ok_or(Failure::Missing)?;
            if let Some(name) = patch.name {
                record.name = name;
            }
            if let Some(protocol) = patch.protocol {
                record.protocol = protocol;
            }
            if let Some(base) = patch.base_url {
                record.base_url = base;
            }
            if let Some(model) = patch.model {
                record.model = model;
            }
            if let Some(quota) = patch.quota_type {
                record.quota_type = quota;
            }
            if let Some(key) = patch.api_key {
                record.api_key = key;
            }
            record
        }
        ("DELETE", Some(id)) => {
            let path = storage(path(&root, id))?;
            return match fs::remove_file(path) {
                Ok(()) => Ok((200, json!({"contractVersion":2,"deleted":true}))),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Err(Failure::Missing),
                Err(_) => Err(Failure::Storage),
            };
        }
        _ => return Err(Failure::Invalid),
    };
    clean(&mut record).map_err(|error| {
        if error.is::<ControlPort>() {
            Failure::ControlPort
        } else {
            Failure::Invalid
        }
    })?;
    let path = storage(path(&root, &record.id))?;
    storage(crate::config::atomic::write_private_pretty_json_atomic(
        &path,
        &record,
        "feature profile",
    ))?;
    Ok((
        if id.is_none() { 201 } else { 200 },
        json!({"contractVersion":2,"profile":view(&record)}),
    ))
}

pub(super) fn handle(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    request_path: &str,
) -> anyhow::Result<()> {
    let id = request_path.strip_prefix(&format!("{PATH}/"));
    let (status, code, message) = if id.is_some_and(|id| !is_reserved(id) || id.trim() != id) {
        (
            404,
            "feature_profile_not_found",
            "Feature Profile not found",
        )
    } else if !matches!(
        (request.method.as_str(), id),
        ("GET", _) | ("POST", None) | ("PATCH" | "DELETE", Some(_))
    ) {
        (
            405,
            "method_not_allowed",
            "Use the Feature Profile list or item methods",
        )
    } else {
        match apply(&request.method, id, &request.body) {
            Ok((status, value)) => {
                return write_json_response(
                    stream,
                    status,
                    if status == 201 { "Created" } else { "OK" },
                    &value,
                )
            }
            Err(Failure::Invalid) => (
                400,
                "invalid_feature_profile",
                "Invalid Feature Profile settings",
            ),
            Err(Failure::ControlPort) => (400, "feature_profile_control_port", CONTROL_PORT_MESSAGE),
            Err(Failure::Missing) => (
                404,
                "feature_profile_not_found",
                "Feature Profile not found",
            ),
            Err(Failure::Full) => (
                409,
                "feature_profile_limit",
                "Feature Profile limit reached",
            ),
            Err(Failure::Storage) => (
                503,
                "feature_profiles_unavailable",
                "Feature Profile storage unavailable",
            ),
        }
    };
    let reason = if status == 405 {
        "Method Not Allowed"
    } else {
        super::server::http_reason_for(status)
    };
    super::server::write_v2_error(
        stream,
        status,
        reason,
        code,
        message,
        status == 503,
        json!({}),
    )
}

#[cfg(test)]
pub(super) fn create_rewrite_fixture() -> String {
    let body = serde_json::to_vec(
        &json!({"name":"Rewrite fixture","protocol":"openaiResponses",
        "baseUrl":"http://127.0.0.1:9/v1","model":"fixture","quotaType":"payAsYouGo",
        "apiKey":"fixture-key"}),
    )
    .unwrap();
    let (_, result) = apply("POST", None, &body)
        .ok()
        .expect("create private Feature Profile");
    result["profile"]["id"].as_str().unwrap().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_profiles_reserved_agent_ids_do_not_consult_or_disclose_the_store() {
        use crate::workflows::{engine::Facts, facts::LibraryFacts};
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let id = "fp-00000000-0000-4000-8000-000000000000";
        let refusal = require_agent_id(id)
            .unwrap_err()
            .downcast::<AgentRefusal>()
            .unwrap();
        let api = refusal.user_input_error();
        assert_eq!(api.code, AGENT_REFUSAL_CODE);
        assert_eq!(api.details, json!({}));
        assert_eq!(api.message, AGENT_REFUSAL_MESSAGE);
        let placement = super::super::profile_auto_pick::resolve_live(
            id,
            &super::super::profile_auto_pick::Spend::default(),
            false,
        )
        .unwrap_err();
        assert_eq!(placement.code, AGENT_REFUSAL_CODE);
        assert_eq!(
            LibraryFacts::new()
                .profile_facts(&[id.into()])
                .err()
                .unwrap()
                .code,
            AGENT_REFUSAL_CODE
        );
        assert!(crate::config::new_session::resolve_profile_name(id)
            .unwrap_err()
            .is::<AgentRefusal>());
        assert!(!root().unwrap().exists());
    }

    #[test]
    fn feature_profiles_reject_configured_local_control_ports_on_save_and_use() {
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        const ID: &str = "fp-00000000-0000-4000-8000-000000000000";
        let root = root().unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let record = json!({"id":ID, "name":"Earlier profile", "protocol":"openaiResponses",
            "baseUrl":"http://127.0.0.1:35470/v1", "model":"fixture", "quotaType":"balance",
            "apiKey":"synthetic-port-fixture"});
        std::fs::write(
            path(&root, ID).unwrap(),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        let config = crate::profiles::model::CodezConfig {
            management_port: Some(35470),
            agent_bus_port: Some(35460),
            management_listen: Some(vec!["127.0.0.1:35471".into()]),
            ..Default::default()
        };
        crate::config::store::save_codez_config(&config).unwrap();
        for address in [
            "http://127.0.0.1:35470",
            "https://localhost.:35460",
            "http://[::1]:35471",
            "http://[::ffff:127.0.0.1]:35470",
        ] {
            assert!(endpoint(Protocol::Responses, address).is_err(), "{address}");
        }
        assert!(endpoint(Protocol::Responses, "https://example.invalid:35470").is_ok());
        assert!(endpoint(Protocol::Responses, "http://127.0.0.1:35472").is_ok());
        assert!(
            provider(ID).is_err(),
            "existing profiles recheck the current host ports"
        );
        let mut input = record;
        input.as_object_mut().unwrap().remove("id");
        let body = serde_json::to_vec(&input).unwrap();
        assert!(matches!(
            apply("POST", None, &body),
            Err(Failure::ControlPort)
        ));
    }

    #[test]
    fn feature_profiles_validate_provider_endpoints_without_accepting_embedded_secrets() {
        assert_eq!(
            endpoint(Protocol::Responses, "https://api.openai.com/v1/")
                .unwrap()
                .path(),
            "/v1/responses"
        );
        assert_eq!(
            endpoint(Protocol::Messages, "https://api.anthropic.com")
                .unwrap()
                .path(),
            "/v1/messages"
        );
        assert_eq!(
            endpoint(Protocol::Messages, "http://[::1]:9999/prefix/v1")
                .unwrap()
                .path(),
            "/prefix/v1/messages"
        );
        for bad in [
            "https://user:password@example.invalid",
            "https://example.invalid?api_key=test",
            "https://example.invalid/#secret",
            "http://example.invalid",
            "file:///tmp/profile",
        ] {
            assert!(endpoint(Protocol::Responses, bad).is_err(), "{bad}");
        }
    }
}
