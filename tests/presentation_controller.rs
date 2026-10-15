//! Explicit private harness entrance; not installed, no model tool or new API.
use cutex::app_server::presentation::{PresentationClient, Receipt};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
#[test]
#[ignore = "requires owned private native fixture"]
fn private_presentation_controller() {
    let home = std::path::PathBuf::from(std::env::var("CUTEX_TEST_PRIVATE_HOME").unwrap());
    assert!(home.is_absolute() && home.join(".cutex-test-private-home").is_file());
    assert_eq!(std::env::var("HOME").unwrap(), home.to_str().unwrap());
    let result = PresentationClient::connect(
        &home.join(".cutex/cutex-sessions.json"),
        &std::env::var("S6_PRIVATE_OWNER").unwrap(),
        std::env::var("S6_PRIVATE_GENERATION")
            .unwrap()
            .parse()
            .unwrap(),
    );
    let client = match result {
        Ok(c) => c,
        Err(e) => {
            println!("P_REPLY {}", json!({"error":e.to_string()}));
            return;
        }
    };
    println!("P_READY");
    std::io::stdout().flush().unwrap();
    for line in std::io::stdin().lock().lines() {
        let result = (|| -> anyhow::Result<Value> {
            let r: Value = serde_json::from_str(&line?)?;
            let expected: Receipt = serde_json::from_value(r["record"].clone())?;
            match r["operation"].as_str() {
                Some("append") => Ok(serde_json::to_value(client.append(&expected)?)?),
                Some("status") => Ok(serde_json::to_value(client.status(&expected)?)?),
                _ => anyhow::bail!("unsupported private operation"),
            }
        })();
        let v = match result {
            Ok(v) => json!({"result":v}),
            Err(e) => json!({"error":e.to_string()}),
        };
        println!("P_REPLY {v}");
        std::io::stdout().flush().unwrap();
    }
}
