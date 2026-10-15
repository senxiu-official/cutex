fn main() {
    if cutex::agent_bus::mcp::run().is_err() {
        eprintln!("Cutex MCP configuration or protocol failure");
        std::process::exit(2);
    }
}
