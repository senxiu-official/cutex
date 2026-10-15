//! Client for the authenticated local Human/Management control plane.
//!
//! This module deliberately has no Agent Bus identity dependency. In
//! particular it never reads `CUTEX_AGENT_ID` and never submits an Agent
//! occurrence as a Human TUI caller.

use cutex::agent_management::{CutexProjectWorkspace, ProjectId, ProjectPresentationSettings};
use cutex::management::control_plane::{
    HumanManagementOperatorActionRequest, HumanManagementOperatorReceipt,
    HumanManagementPresentationUpdateRequest, HumanManagementProjectCollection,
    HumanManagementProjectMutationReceipt, HumanManagementProjectMutationRequest,
};
use cutex::management::remote::management_http_json_with_timeout;
use cutex::management::service::{
    management_base_url, management_root_credential,
};
use cutex::profiles::model::CodezConfig;
use std::time::Duration;

#[derive(Clone, Debug)]
pub(super) struct ManagementControlClient {
    base_url: String,
    root_bearer: String,
}

impl ManagementControlClient {
    pub(super) fn native_recovery(
        &self,
        request: &cutex::app_server::external_recovery::RecoveryRequest,
    ) -> anyhow::Result<serde_json::Value> {
        self.request_with_timeout(
            "POST",
            "/v2/agent-management/native-recovery",
            Some(&serde_json::to_vec(request)?),
            Duration::from_secs(120),
        )
    }

    pub(super) fn human_config(
        &self,
        request: &cutex::agent_management::HumanConfigRequest,
    ) -> anyhow::Result<serde_json::Value> {
        self.request_with_timeout(
            "POST",
            "/v2/agent-management/human-config",
            Some(&serde_json::to_vec(request)?),
            Duration::from_secs(120),
        )
    }

    /// `GET /v2/agent-bus/stuck-messages?{query}`.
    pub(super) fn stuck_bus_messages(&self, query: &str) -> anyhow::Result<serde_json::Value> {
        self.request_with_timeout(
            "GET",
            &format!("/v2/agent-bus/stuck-messages?{query}"),
            None,
            Duration::from_secs(30),
        )
    }

    /// `POST /v2/agent-bus/stuck-messages/discard`.
    pub(super) fn discard_bus_message(
        &self,
        body: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        self.request_with_timeout(
            "POST",
            "/v2/agent-bus/stuck-messages/discard",
            Some(&serde_json::to_vec(body)?),
            Duration::from_secs(30),
        )
    }

    pub(super) fn runtime_action_status(
        &self,
        action_id: cutex::agent_management::AgentActionId,
    ) -> anyhow::Result<cutex::agent_management::RuntimeActionStatus> {
        self.request(
            "POST",
            "/v2/agent-management/explicit-launch",
            Some(&serde_json::to_vec(
                &cutex::agent_management::ExplicitLaunchRequest::RuntimeStatus { action_id },
            )?),
        )
    }

    pub(super) fn recover_runtime(
        &self,
        cutex_session_id: cutex::role_revision::CutexSessionId,
        action_id: cutex::agent_management::AgentActionId,
    ) -> anyhow::Result<cutex::agent_management::HumanRuntimeRecovery> {
        self.request_with_timeout(
            "POST",
            "/v2/agent-management/explicit-launch",
            Some(&serde_json::to_vec(
                &cutex::agent_management::ExplicitLaunchRequest::RecoverRuntime {
                    cutex_session_id,
                    action_id,
                },
            )?),
            Duration::from_secs(120),
        )
    }

    pub(super) fn review_stock_runtime(
        &self,
        cutex_session_id: cutex::role_revision::CutexSessionId,
        restart: bool,
    ) -> anyhow::Result<cutex::agent_management::StockRuntimeReview> {
        self.request_with_timeout(
            "POST",
            "/v2/agent-management/explicit-launch",
            Some(&serde_json::to_vec(
                &cutex::agent_management::ExplicitLaunchRequest::ReviewRuntime {
                    cutex_session_id,
                    restart,
                    receiver_canonical_byte_limit: Default::default(),
                    job_mcp: None,
                },
            )?),
            Duration::from_secs(120),
        )
    }

    pub(super) fn run_stock_runtime(
        &self,
        action_id: cutex::agent_management::AgentActionId,
        review: cutex::agent_management::StockRuntimeReview,
    ) -> anyhow::Result<cutex::agent_management::StockRuntimeReceipt> {
        let result = self.request_with_timeout(
            "POST",
            "/v2/agent-management/explicit-launch",
            Some(&serde_json::to_vec(
                &cutex::agent_management::ExplicitLaunchRequest::Run {
                    action_id: action_id.clone(),
                    review: review.clone(),
                },
            )?),
            Duration::from_secs(120),
        );
        match result {
            Ok(receipt) => Ok(receipt),
            Err(error) => self.reconcile_runtime_action(action_id, review, error),
        }
    }

    pub(super) fn review_runtime_package_upgrade(
        &self,
        cutex_session_id: cutex::role_revision::CutexSessionId,
    ) -> anyhow::Result<cutex::agent_management::RuntimePackageUpgradeReview> {
        self.request_with_timeout(
            "POST",
            "/v2/agent-management/explicit-launch",
            Some(&serde_json::to_vec(
                &cutex::agent_management::ExplicitLaunchRequest::ReviewRuntimePackageUpgrade {
                    cutex_session_id,
                },
            )?),
            Duration::from_secs(120),
        )
    }

    pub(super) fn apply_runtime_package_upgrade(
        &self,
        action_id: cutex::agent_management::AgentActionId,
        review: cutex::agent_management::RuntimePackageUpgradeReview,
    ) -> anyhow::Result<cutex::agent_management::RuntimePackageUpgradeReceipt> {
        self.request_with_timeout(
            "POST",
            "/v2/agent-management/explicit-launch",
            Some(&serde_json::to_vec(
                &cutex::agent_management::ExplicitLaunchRequest::ApplyRuntimePackageUpgrade {
                    action_id,
                    review,
                },
            )?),
            Duration::from_secs(120),
        )
    }

    // A lost response does not imply a failed start. Query the original action;
    // never resubmit Run or mint a replacement action from this transport path.
    fn reconcile_runtime_action(
        &self,
        action_id: cutex::agent_management::AgentActionId,
        review: cutex::agent_management::StockRuntimeReview,
        original_error: anyhow::Error,
    ) -> anyhow::Result<cutex::agent_management::StockRuntimeReceipt> {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            match self.runtime_action_status(action_id.clone()) {
                Ok(status) => {
                    anyhow::ensure!(
                        status.action_id == action_id,
                        "action status identity mismatch"
                    );
                    if let Some(receipt) = status.receipt {
                        anyhow::ensure!(
                            receipt.action_id == action_id && receipt.review == review,
                            "action status request mismatch"
                        );
                        if receipt.stage == cutex::agent_management::StockRuntimeStage::Ready {
                            return Ok(receipt);
                        }
                        if let Some(error) = receipt.error {
                            anyhow::bail!("runtime_start_failed: {error}; action {}; inspect with cutex human action {}",
                                action_id.as_str(), action_id.as_str());
                        }
                    }
                    if status.state != "in_progress" || std::time::Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(_) => break,
            }
        }
        Err(original_error.context(format!(
            "runtime_result_unknown: inspect the original action with cutex human action {}; do not start it again until its state is known",
            action_id.as_str()
        )))
    }

    pub(super) fn adopt_saved_native(
        &self,
        request: &cutex::agent_management::HumanAdoptRequest,
    ) -> anyhow::Result<cutex::agent_management::HumanAdoptResult> {
        self.request_with_timeout(
            "POST",
            "/v2/agent-management/adopt-saved-native",
            Some(&serde_json::to_vec(request)?),
            Duration::from_secs(120),
        )
    }
    pub(super) fn review_agent_archive(
        &self,
        request: &cutex::agent_management::AgentArchiveReviewRequest,
    ) -> anyhow::Result<cutex::agent_management::AgentArchiveReview> {
        self.request(
            "POST",
            "/v2/agent-management/archive-review",
            Some(&serde_json::to_vec(request)?),
        )
    }

    pub(super) fn execute_agent_archive(
        &self,
        request: &cutex::agent_management::AgentArchiveRequest,
    ) -> anyhow::Result<cutex::agent_management::AgentArchiveReceipt> {
        self.request(
            "POST",
            "/v2/agent-management/archive-actions",
            Some(&serde_json::to_vec(request)?),
        )
    }
    #[cfg(test)]
    pub(super) fn test_endpoint(base_url: String, root_bearer: String) -> Self {
        Self {
            base_url,
            root_bearer,
        }
    }
    pub(super) fn durable_candidates(
        &self,
    ) -> anyhow::Result<Vec<cutex::agent_management::DurableAgentCandidate>> {
        self.request("GET", "/v2/agent-management/durable-candidates", None)
    }

    pub(super) fn import_durable_agent(
        &self,
        request: &cutex::agent_management::DurableImportRequest,
    ) -> anyhow::Result<cutex::agent_management::DurableImportReceipt> {
        self.request(
            "POST",
            "/v2/agent-management/durable-import",
            Some(&serde_json::to_vec(request)?),
        )
    }
    pub(super) fn connect() -> anyhow::Result<Self> {
        let config = cutex::config::store::load_codez_config();
        Self::connect_with_config(&config)
    }

    fn connect_with_config(config: &CodezConfig) -> anyhow::Result<Self> {
        let root_bearer = management_root_credential(config, None)?.to_string();
        if let Some(value) = std::env::var_os("CUTEX_MANAGEMENT_URL") {
            cutex::config::instance::management_url_override(config)?;
            let base_url=value.into_string().map_err(|_|anyhow::anyhow!("CUTEX_MANAGEMENT_URL must be UTF-8"))?;
            let url=url::Url::parse(&base_url)?;
            anyhow::ensure!(url.scheme()=="http" && url.host_str().is_some() && url.username().is_empty() && url.password().is_none(), "CUTEX_MANAGEMENT_URL must be an http endpoint without embedded credentials");
            return Ok(Self {base_url,root_bearer});
        }
        cutex::management::launch::require_management_api_running(config, cutex::management::service::management_port(config))?;
        Ok(Self {
            base_url: management_base_url(cutex::management::service::management_port(config)),
            root_bearer,
        })
    }

    pub(super) fn projects(&self) -> anyhow::Result<HumanManagementProjectCollection> {
        self.request("GET", "/v2/agent-management/projects", None)
    }

    pub(super) fn project(&self, project_id: &ProjectId) -> anyhow::Result<CutexProjectWorkspace> {
        self.request(
            "GET",
            &format!("/v2/agent-management/projects/{project_id}"),
            None,
        )
    }

    pub(super) fn update_presentation(
        &self,
        request: &HumanManagementPresentationUpdateRequest,
    ) -> anyhow::Result<ProjectPresentationSettings> {
        self.request(
            "POST",
            "/v2/agent-management/project-presentation",
            Some(&serde_json::to_vec(request)?),
        )
    }

    pub(super) fn operator_action(
        &self,
        request: &HumanManagementOperatorActionRequest,
    ) -> anyhow::Result<HumanManagementOperatorReceipt> {
        self.request(
            "POST",
            "/v2/agent-management/operator-actions",
            Some(&serde_json::to_vec(request)?),
        )
    }

    pub(super) fn project_mutation(
        &self,
        request: &HumanManagementProjectMutationRequest,
    ) -> anyhow::Result<HumanManagementProjectMutationReceipt> {
        self.request(
            "POST",
            "/v2/agent-management/project-mutations",
            Some(&serde_json::to_vec(request)?),
        )
    }

    pub(super) fn jobs(&self, query: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
        self.request_with_timeout("POST", "/v2/job-service/management-query",
            Some(&serde_json::to_vec(query)?), std::time::Duration::from_secs(6))
    }

    fn request<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
    ) -> anyhow::Result<T> {
        self.request_with_timeout(method, path, body, Duration::from_secs(5))
    }

    pub(super) fn request_with_timeout<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        timeout: Duration,
    ) -> anyhow::Result<T> {
        let value = management_http_json_with_timeout(
            &self.base_url,
            method,
            path,
            Some(&self.root_bearer),
            body,
            timeout,
        )?;
        serde_json::from_value(value)
            .map_err(|error| anyhow::anyhow!("invalid Management control response: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cutex::profiles::model::ManagementApiToken;

    #[test]
    fn lost_run_response_queries_same_action_without_a_second_start() {
        use cutex::agent_management::*;
        use std::io::{Read, Write};
        let review: StockRuntimeReview = serde_json::from_value(serde_json::json!({
            "subject":{"cutex_session_id":"cutex.test","formal_name":"Test","durable_sha256":"a".repeat(64),"authority_sha256":"b".repeat(64),"current_project_id":null,"revision":0,"runtime_generation":0},
            "contract":{"version":2,"native_id":"00000000-0000-4000-8000-000000000001","native_home":"/private","bundle_manifest":"/private/manifest","bundle_sha256":"c".repeat(64)},
            "configuration":{"profile_name":"alpha","profile_id":"private-profile","inherited":false,"profile_sha256":"c".repeat(64),"account_sha256":"d".repeat(64),"model":"private-model","reasoning":null,"model_provider":"private","provider":{"name":"private","base_url":"http://127.0.0.1:1/v1","wire_api":"responses","requires_openai_auth":false,"supports_websockets":false},"sandbox":"read-only","approval":"on-request"},"restart":false
        })).unwrap();
        let action = AgentActionId::new("lost-response-action").unwrap();
        let receipt = StockRuntimeReceipt {
            launch_cwd: None,
            action_id: action.clone(),
            review: review.clone(),
            stage: StockRuntimeStage::Ready,
            claim_id: "claim".into(),
            runtime_agent_id: "runtime".into(),
            expected_generation: 1,
            binding: None,
            publication: None,
            error: None,
            updated_at: "now".into(),
        };
        let response = serde_json::to_vec(&RuntimeActionStatus {
            action_id: action.clone(),
            state: "succeeded".into(),
            receipt: Some(receipt.clone()),
            recovery: None,
        })
        .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = ManagementControlClient::test_endpoint(
            format!("http://{}", listener.local_addr().unwrap()),
            "test-root".into(),
        );
        let server = std::thread::spawn(move || {
            for expected in ["run", "runtime_status"] {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let body = loop {
                    let mut buf = [0u8; 4096];
                    let n = socket.read(&mut buf).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(split) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = std::str::from_utf8(&bytes[..split]).unwrap();
                        let length: usize = headers
                            .lines()
                            .find_map(|l| l.strip_prefix("Content-Length: "))
                            .unwrap()
                            .parse()
                            .unwrap();
                        if bytes.len() >= split + 4 + length {
                            break serde_json::from_slice::<serde_json::Value>(
                                &bytes[split + 4..split + 4 + length],
                            )
                            .unwrap();
                        }
                    }
                };
                assert_eq!(body["operation"], expected);
                assert_eq!(body["action_id"], "lost-response-action");
                if expected == "runtime_status" {
                    write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        response.len()
                    )
                    .unwrap();
                    socket.write_all(&response).unwrap();
                }
                // First connection intentionally closes after the server committed.
            }
        });
        assert_eq!(client.run_stock_runtime(action, review).unwrap(), receipt);
        server.join().unwrap();
    }

    #[test]
    fn runtime_request_waits_for_response_beyond_old_five_second_limit() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = ManagementControlClient::test_endpoint(
            format!("http://{}", listener.local_addr().unwrap()),
            "test-root".into(),
        );
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            socket.read(&mut request).unwrap();
            std::thread::sleep(Duration::from_millis(5500));
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 17\r\nConnection: close\r\n\r\n{\"stage\":\"ready\"}").unwrap();
        });
        let result: serde_json::Value = client
            .request_with_timeout(
                "POST",
                "/v2/agent-management/explicit-launch",
                None,
                Duration::from_secs(120),
            )
            .unwrap();
        assert_eq!(result["stage"], "ready");
        server.join().unwrap();
    }

    #[test]
    fn configured_client_uses_only_dedicated_management_root() {
        let config = CodezConfig {
            management_api_token: Some(ManagementApiToken::new("management-root")),
            agent_bus_token: Some("agent-bus-root".to_string()),
            ..Default::default()
        };
        // Construction of the authenticated context is pure and has no Agent
        // environment lookup. Avoid starting a server in this focused test by
        // asserting the credential resolver used by the constructor.
        assert_eq!(
            management_root_credential(&config, None).unwrap(),
            "management-root"
        );
        assert_ne!(
            management_root_credential(&config, None).unwrap(),
            config.agent_bus_token.as_deref().unwrap()
        );
    }
}
