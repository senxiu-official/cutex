//! Process references are inspected only; pruning never terminates a process.
#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::ensure;
use std::path::PathBuf;
pub const UNKNOWN_CLAUDE: &str = "__cutex_unresolved_claude_interpreter__";

fn legacy_claude(
    executable: Option<&std::path::Path>,
    args: &[String],
    targets: &[PathBuf],
    out: &mut Vec<PathBuf>,
) {
    if args
        .windows(2)
        .any(|pair| pair[0] == "-m" && pair[1] == "cute_claude")
        && !executable.is_some_and(|exe| {
            targets.iter().any(|target| {
                target.is_dir() && super::runtime_prune_inventory::overlaps(target, exe)
            })
        })
    {
        // Legacy external Python loads its module through PYTHONPATH. A missing durable binding
        // cannot prove which adapter it has imported; refuse Claude cleanup, not unrelated kinds.
        out.push(PathBuf::from(UNKNOWN_CLAUDE));
    }
}

fn argument_paths(arguments: impl IntoIterator<Item = String>, out: &mut Vec<PathBuf>) {
    for argument in arguments {
        let value = argument
            .split_once('=')
            .map_or(argument.as_str(), |(_, value)| value);
        let path = PathBuf::from(value);
        if path.is_absolute() {
            out.push(path);
        }
    }
}

#[cfg(windows)]
fn windows_arguments(text: &str) -> Vec<String> {
    let mut quoted = false;
    let mut current = String::new();
    let mut out = Vec::new();
    for ch in text.chars() {
        if ch == '"' {
            quoted = !quoted;
        } else if ch.is_whitespace() && !quoted {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn executable_names(path: &std::path::Path, names: &mut std::collections::HashSet<String>) {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    if metadata.file_type().is_symlink() {
        return;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return;
        }
    }
    if path.is_file() {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            names.insert(name.to_lowercase());
        }
    } else if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if let Ok(kind) = entry.file_type() {
                if !kind.is_symlink() {
                    executable_names(&entry.path(), names);
                }
            }
        }
    }
}

pub fn running_paths(targets: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut names = std::collections::HashSet::new();
    for target in targets {
        executable_names(target, &mut names);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // One read-only inventory, with no window, no command interpolation and no logged command
        // lines. Include interpreter arguments: an external Python/Node can run a managed adapter.
        let result = std::process::Command::new("powershell.exe")
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command",
                "[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); @(Get-CimInstance Win32_Process -ErrorAction Stop | Select-Object Name,ExecutablePath,CommandLine) | ConvertTo-Json -Compress"])
            .creation_flags(0x08000000).stdin(std::process::Stdio::null()).output()?;
        ensure!(result.status.success(), "process inventory unavailable");
        ensure!(
            result.stdout.len() <= 16 * 1024 * 1024,
            "process inventory exceeded budget"
        );
        let processes: Vec<serde_json::Value> = serde_json::from_slice(&result.stdout)?;
        for process in processes {
            let name = process["Name"].as_str().unwrap_or("").to_ascii_lowercase();
            let relevant = names.contains(&name)
                || matches!(
                    name.as_str(),
                    "codex-app-server.exe"
                        | "codex.exe"
                        | "cute-codex.exe"
                        | "codex-code-mode-host.exe"
                        | "cutex-mcp.exe"
                        | "node.exe"
                        | "python.exe"
                        | "pythonw.exe"
                        | "claude.exe"
                        | "kimi.exe"
                        | "rg.exe"
                );
            let executable = process["ExecutablePath"].as_str();
            ensure!(
                !relevant || executable.is_some(),
                "runtime process identity unavailable"
            );
            if let Some(path) = executable {
                out.push(PathBuf::from(path));
            }
            if let Some(command) = process["CommandLine"].as_str() {
                let args = windows_arguments(command);
                legacy_claude(
                    executable.map(std::path::Path::new),
                    &args,
                    targets,
                    &mut out,
                );
                argument_paths(args, &mut out);
            } else {
                ensure!(!relevant, "runtime interpreter arguments unavailable");
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        for entry in std::fs::read_dir("/proc")? {
            let path = entry?.path();
            if !path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
            {
                continue;
            }
            let executable = match std::fs::read_link(path.join("exe")) {
                Ok(executable) => {
                    out.push(executable.clone());
                    executable
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    let comm = std::fs::read_to_string(path.join("comm"))
                        .context("process name unavailable")?;
                    let comm = comm.trim();
                    let relevant = names
                        .iter()
                        .any(|name| name == comm || (comm.len() == 15 && name.starts_with(comm)))
                        || comm.starts_with("python")
                        || comm == "node"
                        || comm.starts_with("codex")
                        || comm.starts_with("claude")
                        || comm.starts_with("kimi");
                    if relevant {
                        return Err(error).context("runtime process executable unavailable");
                    }
                    continue;
                }
            };
            match std::fs::read(path.join("cmdline")) {
                Ok(bytes) => {
                    let args = bytes
                        .split(|b| *b == 0)
                        .map(|b| String::from_utf8_lossy(b).into_owned())
                        .collect::<Vec<_>>();
                    legacy_claude(Some(&executable), &args, targets, &mut out);
                    argument_paths(args, &mut out);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("process arguments unavailable"),
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        for pid in crate::platform::macos_proc::all_pids()? {
            if let Some(path) = crate::platform::macos_proc::exe(pid)? {
                out.push(path);
            }
            // Interpreter arguments require an equally authoritative platform implementation.
        }
        anyhow::bail!("runtime argument inventory not supported on macOS");
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    anyhow::bail!("runtime process inventory unsupported");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    #[test]
    fn quoted_interpreters_and_flag_paths_are_protected() {
        let mut paths = Vec::new();
        argument_paths(
            windows_arguments(
                r#""C:\Program Files\node\node.exe" "D:\runtime files\adapter.js" --root="D:\runtime files\root""#,
            ),
            &mut paths,
        );
        assert!(paths.contains(&PathBuf::from(r"D:\runtime files\adapter.js")));
        assert!(paths.contains(&PathBuf::from(r"D:\runtime files\root")));
    }
}
