//! Local diagnostics remain available when Management or Bus cannot start.
use serde_json::json;

pub(super) fn run(selector: Option<&str>, check: bool, fix: bool, yes: bool, finding_id: Option<&str>) -> anyhow::Result<()> {
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
                finding_ids: Some(selected_findings(&report, finding_id)?),
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

fn selected_findings(report: &serde_json::Value, finding_id: Option<&str>) -> anyhow::Result<Vec<String>> {
    let findings = report["findings"].as_array().ok_or_else(|| anyhow::anyhow!("health report missing findings"))?;
    if let Some(id) = finding_id {
        anyhow::ensure!(findings.iter().any(|finding| finding["id"] == id),
            "finding_changed: exact finding is absent from the selected session; scan again");
        Ok(vec![id.to_owned()])
    } else {
        Ok(findings.iter().filter_map(|finding| finding["id"].as_str().map(str::to_owned)).collect())
    }
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

#[cfg(test)]
mod occurrence_recovery_selection_tests {
    use super::*;
    #[test]
    fn occurrence_exact_finding_selection_cannot_expand_to_new_or_other_session_findings() {
        let report=json!({"findings":[{"id":"health-exact"},{"id":"health-new-unrelated"}]});
        assert_eq!(selected_findings(&report,Some("health-exact")).unwrap(),vec!["health-exact"]);
        assert!(selected_findings(&report,Some("health-stale")).unwrap_err().to_string().starts_with("finding_changed:"));
        let selected_session=json!({"findings":[{"id":"health-target-only"}]});assert!(selected_findings(&selected_session,Some("health-exact")).is_err());
        assert_eq!(selected_findings(&report,None).unwrap().len(),2);
    }
    #[test]
    fn occurrence_doctor_parser_requires_fix_for_exact_finding_and_keeps_dry_run_default() {
        use clap::Parser;
        use cutex::cli::args::Cli;
        assert!(Cli::try_parse_from(["cutex","human","doctor","cutex.private","--finding-id","health-exact"]).is_err());
        for suffix in [vec!["--fix","--finding-id","health-exact"],vec!["--fix","--finding-id","health-exact","--yes"]] {
            let mut args=vec!["cutex","human","doctor","cutex.private"];args.extend(suffix);
            let parsed=Cli::try_parse_from(args).unwrap();let debug=format!("{parsed:?}");assert!(debug.contains("finding_id: Some(\"health-exact\")"));
        }
    }
}
