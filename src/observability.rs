//! Safe, bounded read-model values derived from typed runtime events.
//!
//! These values are presentation-only. They never become model input and do
//! not carry prompts, raw tool arguments, file contents, command lines, tool
//! output, hidden reasoning, or provider mechanics.

use serde::{Deserialize, Serialize};

pub const OBSERVABILITY_TEXT_LIMIT: usize = 512;
const OBSERVABILITY_LINE_LIMIT: usize = 6;
const OBSERVABILITY_ID_LIMIT: usize = 256;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationAssociation {
    pub cutex_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<crate::agent_management::ProjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_number: Option<u64>,
}

impl ObservationAssociation {
    pub fn session(cutex_session_id: impl Into<String>) -> Self {
        Self {
            cutex_session_id: cutex_session_id.into(),
            project_id: None,
            assignment_id: None,
            attempt_number: None,
        }
    }

    pub fn with_task(mut self, assignment_id: String, attempt_number: Option<u64>) -> Self {
        self.assignment_id = Some(assignment_id);
        self.attempt_number = attempt_number;
        self
    }

    pub fn with_project_task(
        mut self,
        project_id: crate::agent_management::ProjectId,
        assignment_id: String,
        attempt_number: u64,
    ) -> Self {
        self.project_id = Some(project_id);
        self.assignment_id = Some(assignment_id);
        self.attempt_number = Some(attempt_number);
        self
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        validate_identifier(&self.cutex_session_id, "cutex session")?;
        if let Some(assignment_id) = self.assignment_id.as_deref() {
            validate_identifier(assignment_id, "assignment")?;
        }
        if self.attempt_number.is_some() && self.assignment_id.is_none() {
            anyhow::bail!("observability attempt requires an assignment identity");
        }
        if self.project_id.is_some() && self.attempt_number.is_none() {
            anyhow::bail!("observability project requires an exact Task attempt");
        }
        if self.attempt_number.is_some_and(|number| {
            number == 0 || number > crate::management::v2::model::MAX_SAFE_SEQUENCE
        }) {
            anyhow::bail!("observability attempt is outside the positive JSON-safe range");
        }
        Ok(())
    }

    pub fn matches_task(&self, assignment_id: &str, attempt_number: u64) -> bool {
        self.assignment_id.as_deref() == Some(assignment_id)
            && self.attempt_number == Some(attempt_number)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SafeOutputClass {
    Progress,
    FinalVisible,
    Unclassified,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SafeOutputProjection {
    pub association: ObservationAssociation,
    pub class: SafeOutputClass,
    pub display_text: String,
    pub updated_at: String,
    pub runtime_generation: u64,
}

impl SafeOutputProjection {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.association.validate()?;
        validate_projection_common(
            &self.display_text,
            &self.updated_at,
            self.runtime_generation,
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SafeToolCallClass {
    Command,
    McpTool,
    DynamicTool,
    CollaborationTool,
    FileChange,
    ImageView,
}

impl SafeToolCallClass {
    pub fn display_text(self) -> &'static str {
        match self {
            Self::Command => "Command",
            Self::McpTool => "MCP tool",
            Self::DynamicTool => "Tool",
            Self::CollaborationTool => "Agent tool",
            Self::FileChange => "File change",
            Self::ImageView => "Image view",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SafeToolCallStatus {
    Started,
    Progress,
    Finished,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SafeToolCallProjection {
    pub association: ObservationAssociation,
    pub class: SafeToolCallClass,
    pub status: SafeToolCallStatus,
    pub display_text: String,
    pub updated_at: String,
    pub runtime_generation: u64,
}

impl SafeToolCallProjection {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.association.validate()?;
        validate_projection_common(
            &self.display_text,
            &self.updated_at,
            self.runtime_generation,
        )
    }
}

fn validate_projection_common(
    display_text: &str,
    updated_at: &str,
    runtime_generation: u64,
) -> anyhow::Result<()> {
    if display_text.is_empty()
        || display_text.chars().count() > OBSERVABILITY_TEXT_LIMIT
        || display_text.lines().count() > OBSERVABILITY_LINE_LIMIT
        || display_text
            .chars()
            .any(|character| character.is_control() && character != '\n')
    {
        anyhow::bail!("observability display text is empty, oversized, or unsafe");
    }
    chrono::DateTime::parse_from_rfc3339(updated_at)
        .map_err(anyhow::Error::from)
        .map(|_| ())?;
    if runtime_generation == 0
        || runtime_generation > crate::management::v2::model::MAX_SAFE_SEQUENCE
    {
        anyhow::bail!("observability runtime generation is outside the positive JSON-safe range");
    }
    Ok(())
}

fn validate_identifier(value: &str, label: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.chars().count() > OBSERVABILITY_ID_LIMIT
        || value.chars().any(char::is_control)
    {
        anyhow::bail!("observability {label} identity is empty, oversized, or unsafe");
    }
    Ok(())
}

/// Bounds visible assistant text and redacts the entire preview when common
/// credential material is present. The caller must still select only an
/// authoritative visible-output field; this helper never makes arbitrary raw
/// event content safe.
pub fn sanitize_visible_output(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if looks_sensitive(value) {
        return Some("[redacted sensitive output]".to_string());
    }
    let mut output = String::new();
    let mut lines = 0_usize;
    for character in value.chars() {
        if output.chars().count() >= OBSERVABILITY_TEXT_LIMIT {
            break;
        }
        if character == '\n' {
            lines += 1;
            if lines >= OBSERVABILITY_LINE_LIMIT {
                break;
            }
            output.push(character);
        } else if character.is_control() {
            output.push('\u{fffd}');
        } else {
            output.push(character);
        }
    }
    let output = output.trim().to_string();
    (!output.is_empty()).then_some(output)
}

fn looks_sensitive(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if [
        "authorization:",
        "bearer ",
        "api_key",
        "api key",
        "access_token",
        "access token",
        "refresh_token",
        "refresh token",
        "client_secret",
        "client secret",
        "password=",
        "password:",
        "private key",
        "begin openssh",
        "begin rsa",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return true;
    }
    value.split_whitespace().any(|word| {
        let word = word.trim_matches(|character: char| {
            matches!(character, ',' | ';' | ':' | ')' | ']' | '}' | '"' | '\'')
        });
        word.starts_with("sk-")
            || word.starts_with("ghp_")
            || word.starts_with("github_pat_")
            || word.starts_with("xoxb-")
            || word.starts_with("xoxp-")
            || (word.starts_with("AKIA") && word.len() >= 16)
            || looks_like_jwt(word)
            || looks_like_credential_url(word)
    })
}

fn looks_like_jwt(value: &str) -> bool {
    let parts = value.split('.').collect::<Vec<_>>();
    parts.len() == 3
        && parts.iter().all(|part| {
            part.len() >= 8
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
}

fn looks_like_credential_url(value: &str) -> bool {
    value
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('@').map(|(authority, _)| authority))
        .is_some_and(|authority| authority.contains(':'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_output_is_bounded_sanitized_and_secret_redacting() {
        assert_eq!(
            sanitize_visible_output(" final\u{0007} reply ").as_deref(),
            Some("final� reply")
        );
        assert_eq!(
            sanitize_visible_output("Authorization: Bearer secret").as_deref(),
            Some("[redacted sensitive output]")
        );
        assert_eq!(
            sanitize_visible_output(&"x".repeat(OBSERVABILITY_TEXT_LIMIT + 20))
                .expect("bounded")
                .chars()
                .count(),
            OBSERVABILITY_TEXT_LIMIT
        );
    }

    #[test]
    fn association_requires_exact_task_identity() {
        let association = ObservationAssociation::session("cutex.worker")
            .with_task("assignment-1".to_string(), Some(2));
        association.validate().unwrap();
        assert!(association.matches_task("assignment-1", 2));
        assert!(!association.matches_task("assignment-1", 1));
        assert!(!association.matches_task("assignment-2", 2));
    }
}
