//! Local diagnostics remain available when Management or Bus cannot start.
use serde_json::json;

pub(super) fn run(selector: Option<&str>, check: bool, fix: bool, yes: bool) -> anyhow::Result<()> {
    if check || fix {
        let selected = selector.map(super::human::resolve_id).transpose()?;
        let mut report = cutex::management::v2::health::scan()?;
        if let Some(id) = selected {
            if let Some(findings) = report["findings"].as_array_mut() {
                findings.retain(|f| f["cutexSessionId"] == id);
            }
        }
        if fix {
            report = cutex::management::v2::health::fix(&cutex::management::v2::health::FixRequest {
                finding_ids: Some(report["findings"].as_array().unwrap().iter().filter_map(|f| f["id"].as_str().map(str::to_owned)).collect()),
                dry_run: !yes,
            }, super::management_context::management_request_context()).map_err(anyhow::Error::new)?;
        }
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    let root = cutex::config::paths::runtime_dir()?;
    let sessions = cutex::session::store::load_cutex_session_store()?;
    let selected = selector.map(super::human::resolve_id).transpose()?;
    let agents = sessions.sessions.values().filter(|r| {
        selected.as_ref().is_none_or(|id| id == &r.cutex_session_id)
    }).map(|r| {
        let receipt = sessions.explicit_launch_receipts.values().filter_map(|r| {
            if let cutex::agent_management::ExplicitLaunchActionReceipt::Runtime(r) = r {
                Some(r)
            } else { None }
        }).filter(|a| a.review.subject.cutex_session_id.as_str() == r.cutex_session_id)
          .max_by(|a, b| a.updated_at.cmp(&b.updated_at));
        json!({
            "id":r.cutex_session_id,"name":r.formal_agent_name,
            "generation":r.runtime_generation,"runtime_agent_id":r.current_runtime_agent_id,
            "pid":r.runtime_pid,"actual_executable":r.runtime_pid.and_then(process_executable),
            "desired_bundle":r.explicit_launch.as_ref().map(|c| &c.bundle_manifest),
            "pending_claim":r.app_server_launch_claim_id,
            "last_start":receipt.map(|a| json!({"action_id":a.action_id,"stage":a.stage,"error":a.error,
                "resume_command":format!("cutex human action {} --resume",a.action_id)})),
            "diagnostic_journal":r.app_server_runtime.as_ref().map(|b| &b.diagnostic_journal_path),
        })
    }).collect::<Vec<_>>();
    let result = json!({
        "cli_executable":std::env::current_exe().ok(),
        "codex_home_env":std::env::var_os("CODEX_HOME").map(|value| value.to_string_lossy().into_owned()),
        "cutex_native_home":cutex::config::paths::host_codex_home_dir()?,
        "runtime_root":root,
        "running_cutex_executables":running_cutex_executables(),
        "agents":agents,
    });
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn process_executable(pid: u32) -> Option<std::path::PathBuf> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        cutex::platform::process::process_executable(pid).ok()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

fn running_cutex_executables() -> Vec<serde_json::Value> {
    let mut result = Vec::new();
    #[cfg(target_os = "macos")]
    for pid in cutex::platform::macos_proc::all_pids().unwrap_or_default() {
        if let Some(executable) = process_executable(pid) {
            if executable.file_name().is_some_and(|name| name == "cutex") {
                result.push(json!({"pid":pid,"executable":executable}));
            }
        }
    }
    #[cfg(target_os = "linux")]
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            if let Some(executable) = process_executable(pid) {
                if executable.file_name().is_some_and(|name| name == "cutex") {
                    result.push(json!({"pid":pid,"executable":executable}));
                }
            }
        }
    }
    result
}
