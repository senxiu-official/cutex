//! `cutex human project …`: owner-initiated Project operations over the
//! Management control plane (capability `owner-rotation`,
//! `/v2/director-rotations`).

use super::management_control_plane::ManagementControlClient;
use anyhow::Context;
use cutex::cli::args::HumanProjectCommand;
use serde_json::{json, Value};
use std::time::Duration;

const PATH: &str = "/v2/director-rotations";
const TIMEOUT: Duration = Duration::from_secs(60);

pub(super) fn run(command: HumanProjectCommand) -> anyhow::Result<()> {
    let client = ManagementControlClient::connect()?;
    match command {
        HumanProjectCommand::RotateDirector {
            project,
            name,
            profile,
            model,
            effort,
            cwd,
            predecessor,
            handoff_timeout_secs,
            wait,
        } => {
            let projects = client.projects()?;
            let summary = projects
                .projects
                .iter()
                .find(|summary| summary.project_id.as_str() == project)
                .with_context(|| format!("no active Project {project}"))?;
            let director = summary
                .director_cutex_session_id
                .as_ref()
                .with_context(|| format!("Project {project} has no Director"))?;
            let body = start_body(
                &project,
                director.as_str(),
                summary.authority_epoch,
                &predecessor,
                handoff_timeout_secs,
                json!({"name": name, "profile": profile, "model": model, "reasoningEffort": effort, "cwd": cwd}),
            );
            let started: Value =
                client.request_with_timeout("POST", PATH, Some(&serde_json::to_vec(&body)?), TIMEOUT)?;
            print_rotation(&started["rotation"]);
            if wait {
                follow(&client, started["rotation"]["rotationId"].as_str().unwrap_or_default())?;
            }
        }
        HumanProjectCommand::Rotation {
            id,
            project,
            cancel,
            continue_without_handoff,
        } => match (id, project) {
            (Some(id), _) => {
                let path = match (cancel, continue_without_handoff) {
                    (true, _) => format!("{PATH}/{id}/cancel"),
                    (_, true) => format!("{PATH}/{id}/continue"),
                    _ => format!("{PATH}/{id}"),
                };
                let method = if cancel || continue_without_handoff { "POST" } else { "GET" };
                let body = (method == "POST").then(|| b"{}".to_vec());
                let value: Value = client.request_with_timeout(method, &path, body.as_deref(), TIMEOUT)?;
                print_rotation(&value["rotation"]);
            }
            (None, project) => {
                let mut query = url::form_urlencoded::Serializer::new(String::new());
                if let Some(project) = project {
                    query.append_pair("projectId", &project);
                }
                let value: Value =
                    client.request_with_timeout("GET", &format!("{PATH}?{}", query.finish()), None, TIMEOUT)?;
                let rotations = value["rotations"].as_array().cloned().unwrap_or_default();
                if rotations.is_empty() {
                    println!("no Director rotations");
                }
                for rotation in &rotations {
                    println!("{}", one_line(rotation));
                }
            }
        },
    }
    Ok(())
}

fn start_body(
    project: &str,
    director: &str,
    epoch: u64,
    predecessor: &str,
    handoff_timeout_secs: Option<u64>,
    successor: Value,
) -> Value {
    let successor: serde_json::Map<String, Value> = successor
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, value)| !value.is_null())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let mut body = json!({
        "requestId": format!("cli-rotate-{}", uuid::Uuid::new_v4().simple()),
        "projectId": project,
        "expectedDirectorCutexSessionId": director,
        "expectedAuthorityEpoch": epoch,
        "predecessor": predecessor,
        "successor": successor,
    });
    if let Some(secs) = handoff_timeout_secs {
        body["handoffTimeoutSecs"] = json!(secs);
    }
    body
}

fn follow(client: &ManagementControlClient, id: &str) -> anyhow::Result<()> {
    let mut last = String::new();
    loop {
        let value: Value = client.request_with_timeout("GET", &format!("{PATH}/{id}"), None, TIMEOUT)?;
        let rotation = &value["rotation"];
        let line = one_line(rotation);
        if line != last {
            println!("{line}");
            last = line;
        }
        let state = rotation["state"].as_str().unwrap_or_default();
        if matches!(state, "done" | "cancelled" | "failed") {
            print_rotation(rotation);
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn one_line(rotation: &Value) -> String {
    let mut line = format!(
        "{} {} {} -> {} [{}]",
        rotation["rotationId"].as_str().unwrap_or("?"),
        rotation["projectId"].as_str().unwrap_or("?"),
        rotation["predecessor"]["name"].as_str().unwrap_or("?"),
        rotation["successorSpec"]["name"].as_str().unwrap_or("?"),
        rotation["state"].as_str().unwrap_or("?"),
    );
    if rotation["state"] == "timeout" {
        line.push_str(" (handoff note missing: rerun with --continue or --cancel)");
    }
    if let Some(error) = rotation["error"]["message"].as_str() {
        line.push_str(&format!(": {error}"));
    }
    line
}

fn print_rotation(rotation: &Value) {
    println!("{}", serde_json::to_string_pretty(rotation).unwrap_or_default());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_body_drops_unset_successor_fields() {
        let body = start_body(
            "p1",
            "cutex.a",
            3,
            "member",
            None,
            json!({"name": "Eva-r2", "profile": null, "model": null, "reasoningEffort": null, "cwd": null}),
        );
        assert_eq!(body["successor"], json!({"name": "Eva-r2"}));
        assert_eq!(body["expectedAuthorityEpoch"], 3);
        assert!(body.get("handoffTimeoutSecs").is_none());
        let request: cutex::management::v2::owner_rotation::StartRequest =
            serde_json::from_value(body).unwrap();
        assert_eq!(request.successor.name.as_deref(), Some("Eva-r2"));
        let with_timeout = start_body("p1", "cutex.a", 3, "archive", Some(120), json!({}));
        assert_eq!(with_timeout["handoffTimeoutSecs"], 120);
    }

    #[test]
    fn one_line_explains_a_timeout() {
        let line = one_line(&json!({
            "rotationId": "rot-1", "projectId": "p1", "state": "timeout",
            "predecessor": {"name": "Eva"}, "successorSpec": {"name": "Eva-r2"},
        }));
        assert!(line.contains("--continue"), "{line}");
    }
}
