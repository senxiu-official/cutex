fn main() {
    cutex::config::proxy::ensure_process_loopback_no_proxy();
    if cutex::agent_bus::mcp::run().is_err() {
        eprintln!("Cutex MCP configuration or protocol failure");
        std::process::exit(2);
    }
}
