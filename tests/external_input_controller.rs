//! Private composed-probe entrance, not a installed command or MCP tool.
use cutex::app_server::external_input::{Envelope, ExternalInputClient, MessageKey, Retry};
use serde_json::{json, Value};
use std::io::{BufRead, Write};

#[test]
#[ignore = "requires explicit private S6 fixture, never operator state"]
fn private_external_input_controller() {
    let home = std::path::PathBuf::from(std::env::var("CUTEX_TEST_PRIVATE_HOME").unwrap());
    assert!(home.is_absolute() && home.join(".cutex-test-private-home").is_file());
    assert_eq!(std::env::var("HOME").unwrap(), home.to_str().unwrap());
    let path = home.join(".cutex/cutex-sessions.json");
    let owner = std::env::var("S6_PRIVATE_OWNER").unwrap();
    let generation: u64 = std::env::var("S6_PRIVATE_GENERATION")
        .unwrap()
        .parse()
        .unwrap();
    let connection = ExternalInputClient::connect(&path, &owner, generation);
    let client = match connection {
        Ok(client) => client,
        Err(e) => {
            println!("S6_REPLY {}", json!({"error":e.to_string()}));
            return;
        }
    };
    println!("S6_READY");
    std::io::stdout().flush().unwrap();
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        let result = (|| -> anyhow::Result<Value> {
            let request: Value = serde_json::from_str(&line)?;
            match request["operation"].as_str() {
                Some("submit") => Ok(serde_json::to_value(client.submit(
                    &serde_json::from_value::<Envelope>(request["params"].clone())?,
                )?)?),
                Some("status") => Ok(serde_json::to_value(client.status(
                    &serde_json::from_value::<Vec<MessageKey>>(request["params"].clone())?,
                )?)?),
                Some("retry") => Ok(serde_json::to_value(
                    client.retry(&serde_json::from_value::<Retry>(request["params"].clone())?)?,
                )?),
                Some("hint") => Ok(serde_json::to_value(
                    client.next_hint(std::time::Duration::from_millis(100))?,
                )?),
                _ => anyhow::bail!("unknown private test operation"),
            }
        })();
        let value = match result {
            Ok(value) => json!({"result":value}),
            Err(e) => json!({"error":format!("{e:#}")}),
        };
        println!("S6_REPLY {value}");
        std::io::stdout().flush().unwrap();
    }
}
