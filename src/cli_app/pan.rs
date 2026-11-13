use cutex::cli::args::PanCommand;
use serde_json::{json, Value};

pub(crate) fn run(command: PanCommand, as_json: bool) -> anyhow::Result<()> {
    let body = match command {
        PanCommand::Ls { dir } => json!({"operation":"ls","dir":dir}),
        PanCommand::Search { key, dir } => json!({"operation":"search","dir":dir,"key":key}),
        PanCommand::Meta { remote } => json!({"operation":"meta","remote":remote}),
        PanCommand::Download {
            remote,
            local_dir,
            request_id,
        } => {
            let dir = std::fs::canonicalize(local_dir)?;
            let dir = cutex::agent_management::strip_verbatim_prefix(
                dir.to_str()
                    .ok_or_else(|| anyhow::anyhow!("Target directory must be Unicode"))?,
            );
            json!({"operation":"download","remote":remote,"targetDir":dir,"requestId":request_id.unwrap_or_else(||uuid::Uuid::new_v4().to_string())})
        }
        PanCommand::Status { id } => json!({"operation":"status","id":id}),
        PanCommand::Pause { id } => json!({"operation":"pause","id":id}),
        PanCommand::Resume { id } => json!({"operation":"resume","id":id}),
        PanCommand::Cancel { id } => json!({"operation":"cancel","id":id}),
    };
    // Same owner-private config/roster lookup as the existing local agent CLI.
    // Shells intentionally do not inherit the runtime's bus token. Derive its
    // narrow credential internally; never print either bus or Baidu secrets.
    let (config, agent, _) = super::agent_context::current_live_agent_context()?;
    let session = agent
        .cutex_session_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("A persistent agent context is required"))?;
    let root = config
        .agent_bus_token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("Agent bus authentication is not configured"))?;
    let token = cutex::agent_bus::agent_auth::agent_credential(root, &agent.id, session);
    let url = format!(
        "{}{}",
        cutex::agent_bus::service::agent_bus_base_url(cutex::agent_bus::service::agent_bus_port(
            &config
        )),
        cutex::pan::api::BUS_PATH
    );
    let mut reply=ureq::Agent::config_builder().proxy(None).max_redirects(0).http_status_as_error(false).timeout_global(Some(std::time::Duration::from_secs(90))).build().new_agent()
        .post(&url).header("Authorization",format!("Bearer {token}")).header("x-cutex-agent-id",&agent.id)
        .send_json(&body).map_err(|_|anyhow::anyhow!("Pan daemon request failed; a timed-out download may have started. Check status or retry with the same --request-id"))?;
    let success = reply.status().is_success();
    let result: Value = reply
        .body_mut()
        .with_config()
        .limit(4 * 1024 * 1024)
        .read_json()
        .map_err(|_| anyhow::anyhow!("Pan daemon returned an invalid response"))?;
    if as_json {
        println!("{}", serde_json::to_string(&result)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&result)?);
    }
    if !success {
        anyhow::bail!(
            "Pan operation failed ({})",
            result["error"]["code"].as_str().unwrap_or("daemon_error")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    #[test]
    fn pan_cli_uses_the_authenticated_bus_without_baidu_credentials() {
        let _home = super::super::test_home::IsolatedTestHome::new("pan-cli").unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut config = cutex::config::store::load_codez_config();
        config.agent_bus_port = Some(port);
        config.agent_bus_token = Some("private-test-root".into());
        cutex::config::store::save_codez_config(&config).unwrap();
        let old = std::env::var_os("CUTEX_AGENT_ID");
        std::env::set_var("CUTEX_AGENT_ID", "stock.pan-cli");
        let server = std::thread::spawn(move || {
            for _ in 0..3 {
                let (mut socket, _) = listener.accept().unwrap();
                let request = cutex::http::server::read_simple_http_request(&mut socket).unwrap();
                let result = match request.path.as_str() {
                    "/" => json!("ok"),
                    "/api/agents" => {
                        json!([{"id":"stock.pan-cli","name":"pan-cli","cutex_session_id":"cutex.pan-cli","profile":"test","cwd":".","pid":1,"last_seen_epoch_secs":1}])
                    }
                    cutex::pan::api::BUS_PATH => {
                        let caller = cutex::agent_bus::agent_auth::BusRequest::authenticate(
                            request,
                            Some("private-test-root"),
                        )
                        .unwrap();
                        assert_eq!(caller.agent().unwrap().cutex_session_id, "cutex.pan-cli");
                        assert!(caller.agent_route_permitted());
                        let body: Value = serde_json::from_slice(&caller.body).unwrap();
                        assert_eq!(body["operation"], "status");
                        assert!(!String::from_utf8_lossy(&caller.body).contains("token"));
                        json!({"downloads":[]})
                    }
                    other => panic!("unexpected CLI request {other}"),
                };
                cutex::http::server::write_json_response(&mut socket, 200, "OK", &result).unwrap();
            }
        });
        let parsed =
            cutex::cli::args::Cli::try_parse_from(["cutex", "pan", "status", "--json"]).unwrap();
        if let Some(cutex::cli::args::CommandKind::Pan { command, json }) = parsed.command {
            assert!(json);
            let result = run(command, json);
            match old {
                Some(value) => std::env::set_var("CUTEX_AGENT_ID", value),
                None => std::env::remove_var("CUTEX_AGENT_ID"),
            };
            result.unwrap();
        } else {
            panic!("Pan command not parsed");
        }
        server.join().unwrap();
    }
}
