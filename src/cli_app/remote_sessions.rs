//! Remote Human operations stay on the owning host and never import its store.
use anyhow::{ensure, Context};
use cutex::management::{
    connections::{Connection, Hosts},
    remote::management_http_json_with_timeout as request,
    v2::host_sessions::HostSession,
};
use serde_json::{json, Value};
use std::time::Duration;
pub(super) fn connection(id: &str) -> anyhow::Result<Connection> {
    Hosts::load()?
        .connections
        .into_iter()
        .find(|c| c.id == id)
        .context("Connection not found")
}
pub(super) fn list(c: &Connection, query: &str, cursor: Option<&str>) -> anyhow::Result<Value> {
    let (url, token) = c.verified_endpoint()?;
    let value = request(
        &url,
        "POST",
        "/v2/host/sessions",
        Some(&token),
        Some(&serde_json::to_vec(
            &json!({"query":query,"cursor":cursor,"limit":50}),
        )?),
        Duration::from_secs(10),
    )?;
    ensure!(
        value["hostId"]
            .as_str()
            .is_some_and(|id| id.eq_ignore_ascii_case(&c.host_id)),
        "Host catalog identity mismatch"
    );
    Ok(value)
}
pub(super) fn lifecycle(c: &Connection, id: &str, close: bool) -> anyhow::Result<Value> {
    let current=connection(&c.id)?;
    ensure!(current.enabled && current.host_id.eq_ignore_ascii_case(&c.host_id),"Connection disabled or host changed; refresh before retrying");
    let c=&current;
    let value = list(c, id, None)?;
    let row: HostSession = serde_json::from_value(
        value["data"]
            .as_array()
            .context("Invalid catalog")?
            .iter()
            .find(|r| r["id"] == id)
            .context("Exact remote session not found")?
            .clone(),
    )?;
    let (url, token) = c.verified_endpoint()?;
    let request_id = uuid::Uuid::new_v4().to_string();
    let method = if close {
        "cutex/runtime/close"
    } else {
        "cutex/runtime/online"
    };
    let path = format!(
        "/v2/sessions/{}/cutex/requests",
        url::form_urlencoded::byte_serialize(id.as_bytes()).collect::<String>()
    );
    let body = lifecycle_body(&request_id,row.generation,close);
    let result = request(
        &url,
        "POST",
        &path,
        Some(&token),
        Some(&serde_json::to_vec(&body)?),
        Duration::from_secs(120),
    )?;
    ensure!(
        result["requestId"] == request_id
            && result["cutexSessionId"] == id
            && result.pointer("/cutex/method") == Some(&json!(method)),
        "Remote lifecycle response identity mismatch"
    );
    Ok(result)
}
fn lifecycle_body(request_id:&str,generation:u64,close:bool)->Value {
    let mut params=json!({"expectedRuntimeGeneration":generation,"reason":"human_remote"});
    params[if close{"force"}else{"openVisibleTerminal"}]=json!(false);
    json!({"requestId":request_id,"method":if close{"cutex/runtime/close"}else{"cutex/runtime/online"},"params":params})
}
fn posix(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
fn ps(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}
fn command(platform: &str, exe: &str, id: &str) -> anyhow::Result<String> {
    ensure!(
        !exe.is_empty() && !exe.chars().any(char::is_control),
        "Invalid remote executable"
    );
    cutex::role_revision::CutexSessionId::new(id.to_owned())
        .map_err(|_| anyhow::anyhow!("Exact Cutex session ID required"))?;
    if platform == "windows" {
        use base64::Engine;
        let script = format!(
            "& {} session foreground {}; exit $LASTEXITCODE",
            ps(exe),
            ps(id)
        );
        let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        Ok(format!(
            "powershell.exe -NoProfile -EncodedCommand {}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ))
    } else if platform == "linux" {
        Ok(format!(
            "exec {} session foreground {}",
            posix(exe),
            posix(id)
        ))
    } else {
        anyhow::bail!("Unsupported remote platform {platform}")
    }
}
/// A display-only handoff; TUI navigation never starts SSH or a remote runtime.
pub(super) fn ssh_hint(c: &Connection) -> String {
    let target = if cfg!(windows) { ps(&c.ssh_target) } else { posix(&c.ssh_target) };
    format!("Connect: ssh -- {target}\nThen run cutex on {} to start or enter this session.", c.name)
}

pub(super) fn foreground(c: &Connection, id: &str) -> anyhow::Result<std::process::ExitStatus> {
    let current=connection(&c.id)?;
    ensure!(current.enabled && current.host_id.eq_ignore_ascii_case(&c.host_id),"Connection disabled or host changed; refresh before retrying");
    let c=&current;
    let (url, token) = c.verified_endpoint()?;
    let host = request(
        &url,
        "GET",
        "/v2/host",
        Some(&token),
        None,
        Duration::from_secs(3),
    )?;
    ensure!(
        host["hostId"]
            .as_str()
            .is_some_and(|h| h.eq_ignore_ascii_case(&c.host_id)),
        "Foreground host mismatch"
    );
    let cmd = command(
        host["platform"]
            .as_str()
            .context("Upgrade remote Cutex: platform missing")?,
        host["executable"]
            .as_str()
            .context("Remote executable unavailable")?,
        id,
    )?;
    // SSH is the frontend transport only; Cutex on the peer owns lifecycle/cwd/profile.
    let status = std::process::Command::new("ssh")
        .args([
            "-tt",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "--",
            &c.ssh_target,
            &cmd,
        ])
        .status()?;
    ensure!(
        status.success(),
        "Remote foreground returned {status}; running work remains on {}",
        c.name
    );
    Ok(status)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lifecycle_requests_match_the_server_contract() {
        for close in [false,true] {
            let body=lifecycle_body("qa-remote-lifecycle",1,close);
            cutex::management::v2::contract_validation::validate_cutex_request(&body).unwrap();
        }
    }
    #[test]
    fn remote_foreground_uses_peer_paths_and_quotes_shell_metacharacters() {
        let id = "cutex.018e0000-0000-7000-8000-0000f0004fef";
        let linux = command("linux", "/path with spaces/a'$(test)", id).unwrap();
        assert!(linux.contains("'\\''"));
        assert!(linux.ends_with(&format!("'{id}'")));
        let windows = command("windows", r"D:\Apps\Cutex's folder\cutex.exe", id).unwrap();
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(windows.split_whitespace().last().unwrap())
            .unwrap();
        let script = String::from_utf16(
            &bytes
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(script.contains("Cutex''s folder"));
        assert!(script.contains("session foreground"));
    }
}
