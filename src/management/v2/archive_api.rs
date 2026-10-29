//! Owner-only routes. The Management access gate runs before this dispatcher.
use crate::agent_management::*;
use crate::management::control_plane::HumanManagementPrincipal;
use crate::http::server::{SimpleHttpRequest, write_json_response};
use serde_json::{json, Value};
use std::net::TcpStream;

pub const REVIEW: &str = "/v2/agent-management/permanent-retirement/review";
pub const EXECUTE: &str = "/v2/agent-management/permanent-retirement/execute";
pub enum OwnerRequest {
    Review(PermanentAgentRetirementReviewRequest),
    Execute(PermanentAgentRetirementRequest),
}
pub type Handler = fn(&HumanManagementPrincipal, OwnerRequest) -> Result<Value, AgentManagementError>;

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest, handler: Handler) -> anyhow::Result<()> {
    let payload = if request.path.split('?').next() == Some(REVIEW) {
        serde_json::from_slice(&request.body).map(OwnerRequest::Review)
    } else { serde_json::from_slice(&request.body).map(OwnerRequest::Execute) };
    let payload = match payload {
        Ok(payload) => payload,
        Err(_) => return super::super::server::write_v2_error(stream,400,"Bad Request","invalid_request",
            "strict owner archive request required",false,json!({})),
    };
    match handler(&HumanManagementPrincipal::authenticated(), payload) {
        Ok(result) => write_json_response(stream,200,"OK",&result),
        Err(error) => super::super::server::write_agent_management_control_error(stream,error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn both_routes_are_owner_only_and_execute_requires_credential() {
        for path in [REVIEW, EXECUTE] {
            assert_eq!(crate::management::v2::access::classify("POST",path), Some(crate::management::v2::access::Class::OwnerOnly));
        }
        assert!(serde_json::from_value::<PermanentAgentRetirementRequest>(json!({
            "action_id":"x","cutex_session_id":"cutex.fixture","confirmed_name":"pasted"
        })).is_err());
    }
}

pub const EXPORTED_SCHEMA: super::super::schema_export::ExportedSchema = super::super::schema_export::ExportedSchema {
    name: "agent-archive-kind-v1",
    title: "Recoverable archive and owner permanent retirement",
    routes: &["POST /v2/agent-management/permanent-retirement/review", "POST /v2/agent-management/permanent-retirement/execute"],
    text: r#"{
      "$schema":"https://json-schema.org/draft/2020-12/schema",
      "$id":"urn:cutex:management-v2:agent-archive-kind-v1",
      "title":"Recoverable archive and owner permanent retirement",
      "$defs":{
        "PermanentRetirementReviewRequest":{"type":"object","additionalProperties":false,"required":["cutex_session_id"],"properties":{"cutex_session_id":{"type":"string"}}},
        "PermanentRetirementReview":{"type":"object","required":["confirmation_token","cutex_session_id","formal_name","revision","runtime_generation","expires_at_unix"],"properties":{"confirmation_token":{"type":"string"},"cutex_session_id":{"type":"string"},"formal_name":{"type":"string"},"revision":{"type":"integer","minimum":0},"runtime_generation":{"type":"integer","minimum":0},"expires_at_unix":{"type":"integer"}}},
        "PermanentRetirementExecuteRequest":{"type":"object","additionalProperties":false,"required":["action_id","cutex_session_id","confirmed_name","confirmation_token","expected_revision"],"properties":{"action_id":{"type":"string"},"cutex_session_id":{"type":"string"},"confirmed_name":{"type":"string","description":"Exact current formal name; paste is permitted"},"confirmation_token":{"type":"string","description":"Owner review credential, expires after 300 seconds; one distinct action"},"expected_revision":{"type":"integer","minimum":0}}},
        "ArchiveProjection":{"type":"object","required":["archiveKind","restorable","restoreBlockers","allowedActions"],"properties":{"archiveKind":{"enum":["active","soft","permanent","unknown"]},"restorable":{"type":"boolean"},"restoreBlockers":{"type":"array","items":{"type":"string"}},"allowedActions":{"type":"array","items":{"enum":["archive","restore","permanentRetire","start","send"]}}}}
      }
    }"#,
};