//! The cutex-mcp façade over stdio: `initialize` advertises only tools, the
//! resource / prompt listings answer empty lists (Codex calls them without
//! checking capabilities), unknown methods answer -32601. None of these
//! requests reach the Bus, so the configured port has no listener.
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn facade_answers_listings_without_the_bus() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cutex-mcp"))
        .env("CUTEX_AGENT_ID", "runtime-agent")
        .env("CUTEX_RUNTIME_GENERATION", "1")
        .env("CUTEX_AGENT_BUS_TOKEN", "test-token")
        .env("CUTEX_AGENT_BUS_URL", "http://127.0.0.1:9/")
        .env_remove("CUTEX_MCP_THREAD_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cutex-mcp");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut call = |id: u64, method: &str| -> Value {
        let request = json!({"jsonrpc":"2.0","id":id,"method":method,"params":{}});
        writeln!(stdin, "{request}").unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], id);
        response
    };
    let init = call(1, "initialize");
    assert_eq!(init["result"]["capabilities"], json!({"tools": {}}));
    assert_eq!(call(2, "resources/list")["result"], json!({"resources": []}));
    assert_eq!(call(3, "resources/templates/list")["result"], json!({"resourceTemplates": []}));
    assert_eq!(call(4, "prompts/list")["result"], json!({"prompts": []}));
    assert_eq!(call(5, "resources/read")["error"]["code"], -32601);
    assert_eq!(call(6, "ping")["result"], json!({}));
    drop(call);
    drop(stdin);
    assert!(child.wait().unwrap().success());
}
