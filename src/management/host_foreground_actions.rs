#[cfg(windows)]
use anyhow::Context;

#[cfg(windows)]
use crate::config::env::{env_bool_override, CUTEX_WINDOWS_DESKTOP_LAUNCHER_ENV_VAR};
use crate::session::model::CutexSessionRecord;
#[cfg(windows)]
use crate::session::service::cutex_session_launch_cwd;

pub fn try_start_host_foreground_desktop_terminal(
    record: &CutexSessionRecord,
) -> anyhow::Result<bool> {
    try_start_host_foreground_desktop_terminal_with_profile(record, None)
}

pub fn try_start_host_foreground_desktop_terminal_with_profile(
    record: &CutexSessionRecord,
    launch_profile: Option<&str>,
) -> anyhow::Result<bool> {
    #[cfg(not(windows))]
    {
        let _ = (record, launch_profile);
        Ok(false)
    }

    #[cfg(windows)]
    {
        if env_bool_override(CUTEX_WINDOWS_DESKTOP_LAUNCHER_ENV_VAR) != Some(true) {
            return Ok(false);
        }

        let id = record
            .codex_session_id
            .as_deref()
            .unwrap_or(record.cutex_session_id.as_str());
        let exe = std::env::current_exe()
            .context("failed to resolve current cutex executable for desktop launch")?;
        let cwd = cutex_session_launch_cwd(record);
        let script =
            host_foreground_powershell_script(&exe.display().to_string(), cwd, id, launch_profile);
        let title = format!(
            "cutex {}",
            crate::session::service::cutex_session_display_name(record)
        );
        if try_start_windows_terminal(&title, cwd, &script) {
            return Ok(true);
        }

        std::process::Command::new("cmd")
            .args([
                "/C",
                "start",
                &title,
                "powershell.exe",
                "-NoExit",
                "-NoProfile",
                "-Command",
                &script,
            ])
            .spawn()
            .context("failed to request a visible Windows terminal for cutex session")?;
        Ok(true)
    }
}

#[cfg(any(windows, test))]
fn host_foreground_powershell_script(
    executable: &str,
    cwd: &str,
    id: &str,
    launch_profile: Option<&str>,
) -> String {
    let mut script = format!(
        "Set-Location -LiteralPath {}; & {} session foreground {}",
        powershell_single_quoted(cwd),
        powershell_single_quoted(executable),
        powershell_single_quoted(id),
    );
    if let Some(profile) = launch_profile {
        script.push_str(" --profile ");
        script.push_str(&powershell_single_quoted(profile));
    }
    script
}

#[cfg(windows)]
fn try_start_windows_terminal(title: &str, cwd: &str, script: &str) -> bool {
    let encoded_script = powershell_encoded_command(script);
    std::process::Command::new("wt.exe")
        .args([
            "--title",
            title,
            "-d",
            cwd,
            "powershell.exe",
            "-NoExit",
            "-NoProfile",
            "-EncodedCommand",
            &encoded_script,
        ])
        .spawn()
        .is_ok()
}

#[cfg(windows)]
fn powershell_encoded_command(script: &str) -> String {
    use base64::Engine as _;

    let mut bytes = Vec::with_capacity(script.len() * 2);
    for code_unit in script.encode_utf16() {
        bytes.extend_from_slice(&code_unit.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(any(windows, test))]
fn powershell_single_quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::host_foreground_powershell_script;

    #[test]
    fn desktop_terminal_script_without_profile_is_frozen() {
        assert_eq!(
            host_foreground_powershell_script(
                r"D:\Apps\cutex\cutex.exe",
                r"D:\Work\project",
                "session-1",
                None,
            ),
            "Set-Location -LiteralPath 'D:\\Work\\project'; & 'D:\\Apps\\cutex\\cutex.exe' session foreground 'session-1'"
        );
    }

    #[test]
    fn desktop_terminal_script_quotes_one_launch_profile() {
        assert_eq!(
            host_foreground_powershell_script(
                r"D:\Apps\cutex\cutex.exe",
                r"D:\Owner's Work",
                "session-1",
                Some("owner's profile"),
            ),
            "Set-Location -LiteralPath 'D:\\Owner''s Work'; & 'D:\\Apps\\cutex\\cutex.exe' session foreground 'session-1' --profile 'owner''s profile'"
        );
    }
}
