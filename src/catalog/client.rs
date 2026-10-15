use std::collections::HashSet;
use std::fmt;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde_json::json;
use serde_json::Value;

use super::protocol::*;
use super::stdio::OwnedStdioEndpoint;
use super::stdio::StdioAppServerOptions;
use crate::app_server::protocol::RpcError;
use crate::config::paths::host_codex_home_dir;
use crate::launch::program::codex_program;

const MAX_ERROR_TEXT_CHARS: usize = 2_048;

#[derive(Debug, Clone, PartialEq)]
pub enum CatalogError {
    InvalidInput(String),
    Launch(String),
    Transport(String),
    Protocol(String),
    Rpc { method: String, error: RpcError },
    Timeout { method: String },
    ProviderIncompatible(String),
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message)
            | Self::Launch(message)
            | Self::Transport(message)
            | Self::Protocol(message)
            | Self::ProviderIncompatible(message) => formatter.write_str(message),
            Self::Rpc { method, error } => write!(
                formatter,
                "app-server {method} failed with {}: {}",
                error.code,
                bounded_text(&error.message)
            ),
            Self::Timeout { method } => {
                write!(formatter, "timed out waiting for app-server {method}")
            }
        }
    }
}

impl std::error::Error for CatalogError {}

pub trait CatalogEndpoint: Send {
    fn request(&mut self, method: &str, params: Value) -> Result<Value, CatalogError>;
    fn notify(&mut self, method: &str, params: Option<Value>) -> Result<(), CatalogError>;
    /// Wait up to `timeout` for the next server notification `(method,
    /// params)`. Endpoints that do not surface notifications just wait.
    fn next_notification(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<Option<(String, Option<Value>)>, CatalogError> {
        std::thread::sleep(timeout);
        Ok(None)
    }
}

pub struct CatalogClient {
    endpoint: Box<dyn CatalogEndpoint>,
    provider_user_agent: String,
}

impl CatalogClient {
    /// Starts the paired host app-server against Cutex's host Codex home.
    pub fn spawn_local() -> Result<Self, CatalogError> {
        let codex_home = host_codex_home_dir().map_err(|error| {
            CatalogError::Launch(format!(
                "failed to resolve Cutex host Codex home: {error:#}"
            ))
        })?;
        let options = StdioAppServerOptions::new(codex_program(), codex_home.clone());
        Self::from_owned_stdio(options)
    }

    pub fn from_owned_stdio(options: StdioAppServerOptions) -> Result<Self, CatalogError> {
        let expected_codex_home = options.codex_home.clone();
        let endpoint = OwnedStdioEndpoint::spawn(options)?;
        Self::connect(endpoint, Some(&expected_codex_home))
    }

    /// Negotiates the catalog protocol over an injected endpoint. Runtime-host
    /// transports can implement this boundary without taking over local process
    /// ownership.
    pub fn from_endpoint(endpoint: impl CatalogEndpoint + 'static) -> Result<Self, CatalogError> {
        Self::connect(endpoint, None)
    }

    fn connect(
        endpoint: impl CatalogEndpoint + 'static,
        expected_codex_home: Option<&Path>,
    ) -> Result<Self, CatalogError> {
        let mut endpoint: Box<dyn CatalogEndpoint> = Box::new(endpoint);
        let initialized = endpoint
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "cutex_catalog",
                        "title": "Cutex catalog",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "capabilities": {
                        "experimentalApi": true,
                        "optOutNotificationMethods": [],
                    }
                }),
            )
            .map_err(|error| match error {
                CatalogError::Rpc { method, error } if error.code == -32601 => {
                    CatalogError::ProviderIncompatible(format!(
                        "app-server lacks required method {method}"
                    ))
                }
                error => error,
            })?;
        let user_agent = required_string(&initialized, "userAgent", "initialize response")?;
        // A minimum version, not an allowlist: newer app-servers are accepted
        // and missing methods are detected by the probes below.
        if !crate::launch::stock::native_version_from_user_agent(user_agent)
            .is_some_and(crate::launch::stock::native_version_supported)
        {
            let (major, minor) = crate::launch::stock::MIN_NATIVE_VERSION;
            return Err(CatalogError::ProviderIncompatible(format!(
                "paired app-server version is incompatible: expected {major}.{minor} or newer, received {}",
                bounded_text(user_agent)
            )));
        }
        let reported = required_string(&initialized, "codexHome", "initialize response")?;
        if let Some(expected) = expected_codex_home {
            if Path::new(reported) != expected {
                return Err(CatalogError::ProviderIncompatible(format!(
                    "app-server used CODEX_HOME {} instead of Cutex host Codex home {}",
                    Path::new(reported).display(),
                    expected.display()
                )));
            }
        }
        endpoint.notify("initialized", None)?;

        // This read-only probe makes absence of the required experimental
        // project API visible at startup rather than at the first UI action.
        match endpoint.request("project/list", json!({ "limit": 1 })) {
            Ok(result) => {
                decode_project_page(result)?;
            }
            Err(CatalogError::Rpc { method, error }) if error.code == -32601 => {
                return Err(CatalogError::ProviderIncompatible(format!(
                    "paired app-server {user_agent} lacks required method {method}"
                )));
            }
            Err(error) => return Err(error),
        }

        Ok(Self {
            endpoint,
            provider_user_agent: user_agent.to_string(),
        })
    }

    pub fn provider_user_agent(&self) -> &str {
        &self.provider_user_agent
    }

    pub fn thread_list(&mut self, params: ThreadListParams) -> Result<ThreadPage, CatalogError> {
        if params.parent_thread_id.is_some() && params.ancestor_thread_id.is_some() {
            return Err(CatalogError::InvalidInput(
                "thread/list parentThreadId and ancestorThreadId are mutually exclusive"
                    .to_string(),
            ));
        }
        let result = self.request_typed("thread/list", params)?;
        decode_thread_page(result)
    }

    pub fn project_list(&mut self, params: ProjectListParams) -> Result<ProjectPage, CatalogError> {
        let result = self.request_typed("project/list", params)?;
        decode_project_page(result)
    }

    pub fn project_read(&mut self, project_id: &str) -> Result<Project, CatalogError> {
        require_identity(project_id, "project id")?;
        let result = self.request_value("project/read", json!({ "projectId": project_id }))?;
        decode_project_result(result, "project/read")
    }

    pub fn project_create(&mut self, params: ProjectCreateParams) -> Result<Project, CatalogError> {
        require_identity(&params.idempotency_key, "project idempotency key")?;
        self.project_result_typed("project/create", params)
    }

    pub fn project_import(&mut self, params: ProjectImportParams) -> Result<Project, CatalogError> {
        require_identity(&params.idempotency_key, "project idempotency key")?;
        if let Some(thread_ids) = &params.threads {
            for thread_id in thread_ids {
                require_identity(thread_id, "thread id")?;
            }
        }
        self.project_result_typed("project/import", params)
    }

    pub fn project_update(&mut self, params: ProjectUpdateParams) -> Result<Project, CatalogError> {
        require_identity(&params.project_id, "project id")?;
        self.project_result_typed("project/update", params)
    }

    pub fn project_move(&mut self, params: ProjectMoveParams) -> Result<(), CatalogError> {
        require_identity(&params.project_id, "project id")?;
        if let Some(before) = &params.before_project_id {
            require_identity(before, "before project id")?;
        }
        let result = self.request_typed("project/move", params)?;
        require_object(&result, "project/move response")?;
        Ok(())
    }

    pub fn project_delete(&mut self, project_id: &str) -> Result<(), CatalogError> {
        require_identity(project_id, "project id")?;
        let result = self.request_value("project/delete", json!({ "projectId": project_id }))?;
        require_object(&result, "project/delete response")?;
        Ok(())
    }

    fn request_typed(
        &mut self,
        method: &str,
        params: impl serde::Serialize,
    ) -> Result<Value, CatalogError> {
        let params = serde_json::to_value(params).map_err(|error| {
            CatalogError::Protocol(format!("failed to encode {method} request: {error}"))
        })?;
        self.request_value(method, params)
    }

    fn request_value(&mut self, method: &str, params: Value) -> Result<Value, CatalogError> {
        self.endpoint.request(method, params).map_err(|error| {
            if let CatalogError::Rpc { method, error } = &error {
                if error.code == -32601 {
                    return CatalogError::ProviderIncompatible(format!(
                        "paired app-server {} lacks required method {method}",
                        self.provider_user_agent
                    ));
                }
            }
            error
        })
    }

    fn project_result_typed(
        &mut self,
        method: &str,
        params: impl serde::Serialize,
    ) -> Result<Project, CatalogError> {
        let result = self.request_typed(method, params)?;
        decode_project_result(result, method)
    }
}

fn decode_project_page(value: Value) -> Result<ProjectPage, CatalogError> {
    let object = require_object(&value, "project/list response")?;
    let data = required_array(object, "data", "project/list response")?;
    let next_cursor = required_cursor(object, "nextCursor", "project/list response")?;
    let projects = decode_items::<Project>(data, "project/list project")?;
    validate_unique_project_ids(&projects)?;
    Ok(ProjectPage {
        data: projects,
        next_cursor,
    })
}

fn decode_thread_page(value: Value) -> Result<ThreadPage, CatalogError> {
    let object = require_object(&value, "thread/list response")?;
    let data = required_array(object, "data", "thread/list response")?;
    for item in data {
        let item = require_object(item, "thread/list thread")?;
        required_nonempty_string(item, "id", "thread/list thread")?;
        required_nonempty_string(item, "sessionId", "thread/list thread")?;
        if !item.contains_key("projectId") {
            return Err(CatalogError::Protocol(
                "thread/list thread omitted required projectId".to_string(),
            ));
        }
        match item.get("projectId") {
            Some(Value::Null) => {}
            Some(Value::String(project_id)) if !project_id.trim().is_empty() => {}
            Some(Value::String(_)) => {
                return Err(CatalogError::Protocol(
                    "thread/list thread projectId must not be empty".to_string(),
                ));
            }
            _ => {
                return Err(CatalogError::Protocol(
                    "thread/list thread projectId must be a string or null".to_string(),
                ));
            }
        }
    }
    let next_cursor = required_cursor(object, "nextCursor", "thread/list response")?;
    let backwards_cursor = required_cursor(object, "backwardsCursor", "thread/list response")?;
    let threads = decode_items::<CatalogThread>(data, "thread/list thread")?;
    let mut ids = HashSet::new();
    if let Some(duplicate) = threads
        .iter()
        .find(|thread| !ids.insert(thread.id.as_str()))
    {
        return Err(CatalogError::Protocol(format!(
            "thread/list response repeated thread id {}",
            duplicate.id
        )));
    }
    Ok(ThreadPage {
        data: threads,
        next_cursor,
        backwards_cursor,
    })
}

fn decode_project_result(value: Value, method: &str) -> Result<Project, CatalogError> {
    let object = require_object(&value, &format!("{method} response"))?;
    let project = object.get("project").ok_or_else(|| {
        CatalogError::Protocol(format!("{method} response omitted required project"))
    })?;
    let project: Project = decode_value(project.clone(), &format!("{method} project"))?;
    require_response_identity(&project.id, "project id")?;
    Ok(project)
}

fn validate_unique_project_ids(projects: &[Project]) -> Result<(), CatalogError> {
    let mut ids = HashSet::new();
    for project in projects {
        require_response_identity(&project.id, "project id")?;
        if !ids.insert(project.id.as_str()) {
            return Err(CatalogError::Protocol(format!(
                "project/list response repeated project id {}",
                project.id
            )));
        }
    }
    Ok(())
}

fn decode_items<T: DeserializeOwned>(
    items: &[Value],
    description: &str,
) -> Result<Vec<T>, CatalogError> {
    items
        .iter()
        .cloned()
        .map(|item| decode_value(item, description))
        .collect()
}

fn decode_value<T: DeserializeOwned>(value: Value, description: &str) -> Result<T, CatalogError> {
    serde_json::from_value(value).map_err(|error| {
        CatalogError::Protocol(format!(
            "invalid {description}: {}",
            bounded_text(&error.to_string())
        ))
    })
}

fn require_object<'a>(
    value: &'a Value,
    description: &str,
) -> Result<&'a serde_json::Map<String, Value>, CatalogError> {
    value
        .as_object()
        .ok_or_else(|| CatalogError::Protocol(format!("{description} must be a JSON object")))
}

fn required_array<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
    description: &str,
) -> Result<&'a Vec<Value>, CatalogError> {
    object.get(field).and_then(Value::as_array).ok_or_else(|| {
        CatalogError::Protocol(format!("{description} omitted required array {field}"))
    })
}

fn required_cursor(
    object: &serde_json::Map<String, Value>,
    field: &str,
    description: &str,
) -> Result<Option<String>, CatalogError> {
    match object.get(field) {
        Some(Value::Null) => Ok(None),
        Some(Value::String(cursor)) if !cursor.is_empty() => Ok(Some(cursor.clone())),
        Some(Value::String(_)) => Err(CatalogError::Protocol(format!(
            "{description} {field} must not be empty"
        ))),
        Some(_) => Err(CatalogError::Protocol(format!(
            "{description} {field} must be a string or null"
        ))),
        None => Err(CatalogError::Protocol(format!(
            "{description} omitted required pagination field {field}"
        ))),
    }
}

fn required_string<'a>(
    value: &'a Value,
    field: &str,
    description: &str,
) -> Result<&'a str, CatalogError> {
    let object = require_object(value, description)?;
    required_nonempty_string(object, field, description)
}

fn required_nonempty_string<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
    description: &str,
) -> Result<&'a str, CatalogError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            CatalogError::Protocol(format!(
                "{description} omitted required non-empty string {field}"
            ))
        })
}

fn require_identity(value: &str, description: &str) -> Result<(), CatalogError> {
    if value.trim().is_empty() {
        Err(CatalogError::InvalidInput(format!(
            "{description} must not be empty"
        )))
    } else {
        Ok(())
    }
}

fn require_response_identity(value: &str, description: &str) -> Result<(), CatalogError> {
    if value.trim().is_empty() {
        Err(CatalogError::Protocol(format!(
            "app-server response {description} must not be empty"
        )))
    } else {
        Ok(())
    }
}

pub(crate) fn bounded_text(value: &str) -> String {
    if value.chars().count() <= MAX_ERROR_TEXT_CHARS {
        return value.to_string();
    }
    let mut output = value.chars().take(MAX_ERROR_TEXT_CHARS).collect::<String>();
    output.push_str("…[truncated]");
    output
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;

    use super::*;

    struct FakeEndpoint {
        responses: VecDeque<Result<Value, CatalogError>>,
        sent: std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>,
    }

    impl CatalogEndpoint for FakeEndpoint {
        fn request(&mut self, method: &str, params: Value) -> Result<Value, CatalogError> {
            self.sent
                .lock()
                .expect("sent lock")
                .push((method.to_string(), params));
            self.responses.pop_front().expect("fake response")
        }

        fn notify(&mut self, method: &str, params: Option<Value>) -> Result<(), CatalogError> {
            self.sent
                .lock()
                .expect("sent lock")
                .push((method.to_string(), params.unwrap_or(Value::Null)));
            Ok(())
        }
    }

    fn project(id: &str) -> Value {
        json!({
            "id": id,
            "name": "Project",
            "roots": [{ "path": "/work" }],
            "metadata": {},
            "position": 0,
            "createdAt": 1,
            "updatedAt": 2,
            "futureField": true
        })
    }

    fn connected(
        responses: Vec<Result<Value, CatalogError>>,
    ) -> (
        CatalogClient,
        std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>,
    ) {
        let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut initial = vec![
            Ok(json!({
                "userAgent": "cutex_catalog/0.153.2 (test)",
                "codexHome": "/cutex/codex-home",
                "platformFamily": "unix",
                "platformOs": "linux",
                "additive": true
            })),
            Ok(json!({ "data": [], "nextCursor": null, "additive": true })),
        ];
        initial.extend(responses);
        let client = CatalogClient::from_endpoint(FakeEndpoint {
            responses: initial.into(),
            sent: sent.clone(),
        })
        .expect("connect fake catalog");
        (client, sent)
    }

    #[test]
    fn negotiates_experimental_api_and_probes_project_method() {
        let (_client, sent) = connected(Vec::new());
        let sent = sent.lock().expect("sent lock");
        assert_eq!(sent[0].0, "initialize");
        assert_eq!(
            sent[0].1.pointer("/capabilities/experimentalApi"),
            Some(&Value::Bool(true))
        );
        assert_eq!(sent[1], ("initialized".to_string(), Value::Null));
        assert_eq!(sent[2], ("project/list".to_string(), json!({ "limit": 1 })));
    }

    #[test]
    fn supports_minimum_catalog_version_and_newer() {
        for (version, supported) in [
            ("0.152.9", false),
            ("0.153.4", true),
            ("0.155.0", true),
            ("0.157.1", true),
            ("0.160.0-r47", true),
            ("0.161.2", true),
            ("1.0.0", true),
        ] {
            let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let result = CatalogClient::from_endpoint(FakeEndpoint {
                responses: vec![
                    Ok(json!({"userAgent": format!("cutex_catalog/{version} (test)"), "codexHome": "/tmp"})),
                    Ok(json!({"data": [], "nextCursor": null})),
                ]
                .into(),
                sent,
            });
            assert_eq!(result.is_ok(), supported, "version {version}");
        }
    }

    #[test]
    fn rejects_wrong_provider_version_and_missing_project_api() {
        let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let error = CatalogClient::from_endpoint(FakeEndpoint {
            responses: vec![Ok(json!({
                "userAgent": "cutex_catalog/0.150.1 (test)",
                "codexHome": "/tmp",
            }))]
            .into(),
            sent: sent.clone(),
        })
        .err()
        .expect("version mismatch");
        assert!(matches!(error, CatalogError::ProviderIncompatible(_)));

        let error = CatalogClient::from_endpoint(FakeEndpoint {
            responses: vec![
                Ok(json!({
                    "userAgent": "cutex_catalog/0.153.2 (test)",
                    "codexHome": "/tmp",
                })),
                Err(CatalogError::Rpc {
                    method: "project/list".to_string(),
                    error: RpcError {
                        code: -32601,
                        message: "method not found".to_string(),
                        data: None,
                    },
                }),
            ]
            .into(),
            sent,
        })
        .err()
        .expect("missing method");
        assert!(error
            .to_string()
            .contains("lacks required method project/list"));
    }

    #[test]
    fn decodes_additive_project_and_thread_pages_and_preserves_thread_additions() {
        let (mut client, _) = connected(vec![
            Ok(json!({ "data": [project("project-1")], "nextCursor": "next", "future": 1 })),
            Ok(json!({
                "data": [{
                    "id": "thread-1",
                    "sessionId": "session-1",
                    "projectId": "project-1",
                    "preview": "hello",
                    "modelProvider": "openai",
                    "newProviderField": { "kept": true }
                }],
                "nextCursor": null,
                "backwardsCursor": "back",
                "future": 1
            })),
        ]);
        let projects = client
            .project_list(ProjectListParams::default())
            .expect("project page");
        assert_eq!(projects.data[0].id, "project-1");
        assert_eq!(projects.next_cursor.as_deref(), Some("next"));

        let threads = client
            .thread_list(ThreadListParams {
                project_id: Some(Some("project-1".to_string())),
                ..ThreadListParams::default()
            })
            .expect("thread page");
        assert_eq!(threads.data[0].id, "thread-1");
        assert_eq!(
            threads.data[0]
                .additional_fields
                .get("newProviderField")
                .and_then(|value| value.get("kept")),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn validates_required_identities_and_pagination() {
        let (mut client, _) = connected(vec![Ok(json!({
            "data": [{ "sessionId": "session-1", "projectId": null }],
            "nextCursor": null,
            "backwardsCursor": null
        }))]);
        let error = client
            .thread_list(ThreadListParams::default())
            .expect_err("missing id");
        assert!(error.to_string().contains("non-empty string id"));

        let (mut client, _) = connected(vec![Ok(json!({
            "data": [],
            "nextCursor": 7
        }))]);
        let error = client
            .project_list(ProjectListParams::default())
            .expect_err("invalid cursor");
        assert!(error.to_string().contains("must be a string or null"));
    }

    #[test]
    fn serializes_explicit_null_project_filter() {
        let (mut client, sent) = connected(vec![Ok(json!({
            "data": [],
            "nextCursor": null,
            "backwardsCursor": null
        }))]);
        client
            .thread_list(ThreadListParams {
                project_id: Some(None),
                ..ThreadListParams::default()
            })
            .expect("thread list");
        let sent = sent.lock().expect("sent lock");
        assert_eq!(sent[3].1.get("projectId"), Some(&Value::Null));
    }

    #[test]
    fn exposes_every_required_project_operation_with_paired_wire_names() {
        let project_response = || Ok(json!({ "project": project("project-1") }));
        let (mut client, sent) = connected(vec![
            project_response(),
            project_response(),
            project_response(),
            project_response(),
            Ok(json!({ "future": true })),
            Ok(json!({})),
        ]);
        client.project_read("project-1").expect("read");
        client
            .project_create(ProjectCreateParams {
                name: "Project".to_string(),
                roots: vec![ProjectRoot {
                    path: PathBuf::from("/work"),
                }],
                metadata: None,
                idempotency_key: "create-1".to_string(),
            })
            .expect("create");
        client
            .project_import(ProjectImportParams {
                name: "Project".to_string(),
                roots: vec![ProjectRoot {
                    path: PathBuf::from("/work"),
                }],
                metadata: None,
                threads: Some(vec!["thread-1".to_string()]),
                idempotency_key: "import-1".to_string(),
            })
            .expect("import");
        client
            .project_update(ProjectUpdateParams {
                project_id: "project-1".to_string(),
                name: Some("Renamed".to_string()),
                roots: None,
                metadata: None,
            })
            .expect("update");
        client
            .project_move(ProjectMoveParams {
                project_id: "project-1".to_string(),
                before_project_id: None,
            })
            .expect("move");
        client.project_delete("project-1").expect("delete");

        let methods = sent
            .lock()
            .expect("sent lock")
            .iter()
            .map(|(method, _)| method.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            &methods[3..],
            &[
                "project/read".to_string(),
                "project/create".to_string(),
                "project/import".to_string(),
                "project/update".to_string(),
                "project/move".to_string(),
                "project/delete".to_string(),
            ]
        );
    }
}
