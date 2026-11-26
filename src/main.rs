#[macro_use]
mod diagnostic_stderr;

mod cli_app;

fn main() {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--internal-text-rewrite-worker")) {
        std::process::exit(cutex::management::v2::text_rewrite::worker_entry());
    }
    cutex::config::proxy::ensure_process_loopback_no_proxy();
    // r266: the Kimi runtime component answers the Profile layer's probes.
    cutex::components::kimi_runtime::register_profile_hooks();
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
