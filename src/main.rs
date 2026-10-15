mod cli_app;

fn main() {
    if let Err(err) = cli_app::run() {
        if let Some(error) = cli_app::json_process_error(&err) {
            eprintln!(
                "{}",
                serde_json::to_string(&error).expect("archive JSON error envelope is serializable")
            );
        } else {
            eprintln!("\x1b[31merror:\x1b[0m {err:#}");
        }
        std::process::exit(1);
    }
}
