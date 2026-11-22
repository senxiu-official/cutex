//! Process references are inspected only; pruning never terminates a process.
use anyhow::ensure;
#[cfg(target_os = "linux")]
use anyhow::Context;
use std::path::PathBuf;
#[cfg(windows)]
#[path = "runtime_prune_names.rs"]
mod names_cache;
pub const UNKNOWN_CLAUDE: &str = "__cutex_unresolved_claude_interpreter__";
#[path = "runtime_prune_process_diagnostics.rs"]
mod diagnostics;
pub use diagnostics::{ProcessInspection, UnresolvedClaude};

fn claude_references(
    identity: Option<crate::runtime::claude::process_receipt::ProcessIdentity>,
    args: &[String], out: &mut Vec<PathBuf>,
) {
    claude_references_with(identity, args, out, crate::runtime::claude::process_receipt::references);
}

fn claude_references_with(
    identity: Option<crate::runtime::claude::process_receipt::ProcessIdentity>,
    args: &[String], out: &mut Vec<PathBuf>,
    references: impl FnOnce(&std::path::Path, &crate::runtime::claude::process_receipt::ProcessIdentity) -> anyhow::Result<Vec<PathBuf>>,
) {
    if args.windows(2).any(|pair| pair[0] == "-m" && pair[1] == "cute_claude") {
        let references = identity.and_then(|identity| {
            let state = args.windows(2).find(|pair| pair[0] == "--state-dir")?;
            let state = std::path::Path::new(&state[1]);
            if !state.is_absolute() { return None; }
            references(state, &identity).ok()
        });
        match references {
            Some(paths) => out.extend(paths),
            None => out.push(PathBuf::from(UNKNOWN_CLAUDE)),
        }
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

#[cfg(windows)]
fn windows_process_paths(
    bytes: &[u8], names: &std::collections::HashSet<String>,
    mut references: impl FnMut(Option<crate::runtime::claude::process_receipt::ProcessIdentity>, &[String], &mut Vec<PathBuf>),
) -> anyhow::Result<ProcessInspection> {
    use crate::runtime::claude::process_receipt::ProcessIdentity;
    let processes: Vec<serde_json::Value> = serde_json::from_slice(bytes)?;
    super::runtime_prune_inventory::timing::count("processRows", processes.len());
    let mut out = Vec::new();
    let mut unresolved_claude = UnresolvedClaude::default();
    for process in processes {
        super::runtime_prune_inventory::budget::check()?;
        let name = process["Name"].as_str().unwrap_or("").to_ascii_lowercase();
        let relevant = names.contains(&name) || matches!(name.as_str(),
            "codex-app-server.exe" | "codex.exe" | "cute-codex.exe" | "codex-code-mode-host.exe"
            | "cutex-mcp.exe" | "node.exe" | "python.exe" | "pythonw.exe" | "claude.exe"
            | "kimi.exe" | "rg.exe");
        let executable = process["ExecutablePath"].as_str();
        ensure!(!relevant || executable.is_some(), "runtime process identity unavailable");
        if let Some(path) = executable { out.push(PathBuf::from(path)); }
        if let Some(command) = process["CommandLine"].as_str() {
            let args = windows_arguments(command);
            if args.windows(2).any(|pair| pair[0] == "-m" && pair[1] == "cute_claude") {
                super::runtime_prune_inventory::timing::count("claudeProcesses", 1);
            }
            let identity = (|| {
                Some(ProcessIdentity {
                    pid: u32::try_from(process["ProcessId"].as_u64()?).ok()?,
                    started_at: process["StartedAt"].as_str()?.parse().ok()?,
                    executable: PathBuf::from(executable?),
                })
            })();
            let before = out.len();
            references(identity, &args, &mut out);
            if out[before..].iter().any(|p| p == std::path::Path::new(UNKNOWN_CLAUDE)) {
                unresolved_claude.note(process["ProcessId"].as_u64().and_then(|pid| u32::try_from(pid).ok()), &args);
            }
            argument_paths(args, &mut out);
        } else { ensure!(!relevant, "runtime interpreter arguments unavailable"); }
    }
    Ok(ProcessInspection { paths: out, unresolved_claude })
}

fn executable_names(
    path: &std::path::Path,
    names: &mut std::collections::HashSet<String>,
) -> anyhow::Result<()> {
    #[cfg(windows)]
    names_cache::visit(path.as_os_str().len().saturating_mul(2))?;
    super::runtime_prune_inventory::budget::check()?;
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Ok(());
        }
    }
    if metadata.is_file() {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            names.insert(name.to_lowercase());
        }
    } else {
        directory_names(path, names)?;
    }
    Ok(())
}

fn directory_names(
    path: &std::path::Path,
    names: &mut std::collections::HashSet<String>,
) -> anyhow::Result<()> {
    #[cfg(windows)]
    names_cache::visit(0)?;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        #[cfg(windows)]
        names_cache::visit(entry.file_name().len().saturating_mul(2))?;
        super::runtime_prune_inventory::budget::check()?;
        let kind = entry.file_type()?;
        if kind.is_file() {
            // Directory enumeration already provides kind and Windows attributes.
            if let Some(name) = entry.file_name().to_str() { names.insert(name.to_lowercase()); }
        } else if kind.is_dir() && !kind.is_symlink() {
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if entry.metadata()?.file_attributes() & 0x400 != 0 { continue; }
            }
            directory_names(&entry.path(), names)?;
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn proc_command_paths(bytes: &[u8],
    identity: impl FnOnce() -> Option<crate::runtime::claude::process_receipt::ProcessIdentity>,
    out: &mut Vec<PathBuf>,
) {
    let args = bytes.split(|b| *b == 0)
        .map(|b| String::from_utf8_lossy(b).into_owned()).collect::<Vec<_>>();
    if args.windows(2).any(|p| p[0] == "-m" && p[1] == "cute_claude") {
        claude_references(identity(), &args, out);
    }
    argument_paths(args, out);
}

#[cfg(target_os = "linux")]
fn proc_entry_paths(path: &std::path::Path, names: &std::collections::HashSet<String>,
    out: &mut Vec<PathBuf>, unresolved: &mut UnresolvedClaude,
) -> anyhow::Result<()> {
    match std::fs::read_link(path.join("exe")) {
        Ok(executable) => out.push(executable),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            let comm = std::fs::read_to_string(path.join("comm")).context("process name unavailable")?;
            let comm = comm.trim();
            let relevant = names.iter().any(|name| name == comm || (comm.len() == 15 && name.starts_with(comm)))
                || comm.starts_with("python") || comm == "node" || comm.starts_with("codex")
                || comm.starts_with("claude") || comm.starts_with("kimi");
            if relevant { return Err(error).context("runtime process executable unavailable"); }
            return Ok(());
        }
    }
    match std::fs::read(path.join("cmdline")) {
        Ok(bytes) => {
            let before = out.len();
            let pid = path.file_name().and_then(|pid| pid.to_str()).and_then(|pid| pid.parse::<u32>().ok());
            proc_command_paths(&bytes, || pid.and_then(|pid|
                crate::runtime::claude::process_receipt::ProcessIdentity::read(pid).ok()), out);
            if out[before..].iter().any(|p| p == std::path::Path::new(UNKNOWN_CLAUDE)) {
                let args = bytes.split(|b| *b == 0).map(|b| String::from_utf8_lossy(b).into_owned()).collect::<Vec<_>>();
                unresolved.note(pid, &args);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
        Err(error) => return Err(error).context("process arguments unavailable"),
    }
    Ok(())
}

pub fn running_paths(targets: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    inspect_processes(targets).map(|inspection| inspection.paths)
}

/// Called once by the existing inventory worker, never on its startup caller.
pub(super) fn prewarm_discovery() {
    #[cfg(windows)]
    if names_cache::prewarm().is_err() {
        // No paths or underlying IO errors: this is advisory work, never prune authority.
        eprintln!("Runtime filename prewarm incomplete; fresh discovery remains required");
    }
}

#[cfg(test)]
std::thread_local! {
    static TEST_INSPECTION: std::cell::RefCell<Option<Result<Vec<PathBuf>, String>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) struct TestInspection {
    previous: Option<Result<Vec<PathBuf>, String>>,
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}
#[cfg(test)]
impl Drop for TestInspection {
    fn drop(&mut self) {
        TEST_INSPECTION.with(|slot| *slot.borrow_mut() = self.previous.take());
    }
}
/// Policy fixtures control process evidence; native process-inspection tests keep the real scanner.
#[cfg(test)]
pub(super) fn test_inspection(value: Result<Vec<PathBuf>, String>) -> TestInspection {
    TestInspection {
        previous: TEST_INSPECTION.with(|slot| slot.replace(Some(value))),
        _thread: std::marker::PhantomData,
    }
}

pub fn inspect_processes(targets: &[PathBuf]) -> anyhow::Result<ProcessInspection> {
    #[cfg(test)]
    if let Some(value) = TEST_INSPECTION.with(|slot| slot.borrow().clone()) {
        return value
            .map(ProcessInspection::from_paths)
            .map_err(anyhow::Error::msg);
    }

    let mut unresolved_claude = UnresolvedClaude::default();
    let mut out = Vec::new();
    let discovery = super::runtime_prune_inventory::timing::stage("filename_discovery");
    #[cfg(windows)]
    let _initial_names = names_cache::runtime_names(targets)?;
    #[cfg(not(windows))]
    let names = {
        let mut names = std::collections::HashSet::new();
        for target in targets { executable_names(target, &mut names)?; }
        names
    };
    drop(discovery);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // One read-only inventory, with no window, no command interpolation and no logged command
        // lines. Include interpreter arguments: an external Python/Node can run a managed adapter.
        let mut command = std::process::Command::new("powershell.exe");
        command
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command",
                "[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); @(Get-CimInstance Win32_Process -OperationTimeoutSec 1 -ErrorAction Stop | Select-Object ProcessId,Name,ExecutablePath,CommandLine,@{Name='StartedAt';Expression={$_.CreationDate.ToUniversalTime().ToString('o')}}) | ConvertTo-Json -Compress"])
            .creation_flags(0x08000000).stdin(std::process::Stdio::null());
        let cim = super::runtime_prune_inventory::timing::stage("process_cim");
        let result = bounded_output(command, std::time::Duration::from_secs(2))?;
        ensure!(result.status.success(), "process inventory unavailable");
        ensure!(
            result.stdout.len() <= 16 * 1024 * 1024,
            "process inventory exceeded budget"
        );
        drop(cim);
        let validation = super::runtime_prune_inventory::timing::stage("filename_validation");
        let names = names_cache::runtime_names(targets)?;
        drop(validation);
        let _receipts = super::runtime_prune_inventory::timing::stage("process_receipts");
        let inspection = windows_process_paths(&result.stdout, &names, claude_references)?;
        out.extend(inspection.paths);
        unresolved_claude = inspection.unresolved_claude;
    }
    #[cfg(target_os = "linux")]
    {
        for entry in std::fs::read_dir("/proc")? {
            super::runtime_prune_inventory::budget::check()?;
            let path = entry?.path();
            if path.file_name().is_some_and(|n| n.to_string_lossy().bytes().all(|b| b.is_ascii_digit())) {
                proc_entry_paths(&path, &names, &mut out, &mut unresolved_claude)?;
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
    Ok(ProcessInspection { paths: out, unresolved_claude })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn linux_proc_cmdline_fixture_verifies_native_identity_and_receipt() {
        use crate::runtime::claude::process_receipt::{self, ProcessIdentity};
        let root = std::env::temp_dir().join(format!("proc-receipt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let executable = std::env::current_exe().unwrap();
        let deployment = crate::runtime::claude::ClaudeDeployment { python: executable.clone(),
            adapter_source: root.clone(), claude_cli: executable.clone(), claude_home: root.clone(),
            tui_binary: executable.clone() };
        let identity = ProcessIdentity::read(std::process::id()).unwrap();
        let args = [executable.to_str().unwrap(), "-m", "cute_claude", "--state-dir", root.to_str().unwrap()];
        let bytes = args.join("\0").into_bytes();
        let inspect = || { let mut paths = Vec::new();
            proc_command_paths(&bytes, || Some(identity.clone()), &mut paths); paths };
        assert!(inspect().contains(&PathBuf::from(UNKNOWN_CLAUDE)));
        process_receipt::record(&root, &deployment, identity.clone(), identity.pid).unwrap();
        assert!(inspect().contains(&root.canonicalize().unwrap()));
        assert!(!inspect().contains(&PathBuf::from(UNKNOWN_CLAUDE)));
        let mut reused = identity.clone(); reused.started_at += chrono::Duration::seconds(1);
        let mut paths = Vec::new();
        proc_command_paths(&bytes, || Some(reused), &mut paths);
        assert!(paths.contains(&PathBuf::from(UNKNOWN_CLAUDE)));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_proc_real_python_launch_resolves_only_its_receipt_paths() {
        use crate::runtime::claude::process_receipt::{self, ProcessIdentity};
        struct Owned(std::process::Child);
        impl Drop for Owned { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }
        let root = std::env::temp_dir().join(format!("proc-python-{}", uuid::Uuid::new_v4()));
        let adapter = root.join("r47"); let state = root.join("state"); let candidate = root.join("r50");
        std::fs::create_dir_all(adapter.join("cute_claude")).unwrap();
        std::fs::create_dir_all(&state).unwrap(); std::fs::create_dir_all(&candidate).unwrap();
        std::fs::write(adapter.join("cute_claude/__init__.py"), "").unwrap();
        std::fs::write(adapter.join("cute_claude/__main__.py"), "import time; time.sleep(30)").unwrap();
        let child = Owned(std::process::Command::new("python3").args(["-m", "cute_claude", "--state-dir"])
            .arg(&state).env("PYTHONPATH", &adapter).stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap());
        let identity = ProcessIdentity::read(child.0.id()).unwrap();
        let deployment = crate::runtime::claude::ClaudeDeployment { python: identity.executable.clone(),
            adapter_source: adapter.clone(), claude_cli: identity.executable.clone(), claude_home: state.clone(),
            tui_binary: std::env::current_exe().unwrap() };
        let proc_path = PathBuf::from(format!("/proc/{}", child.0.id()));
        let inspect = || { let mut paths = Vec::new(); let mut unresolved_claude = UnresolvedClaude::default();
            proc_entry_paths(&proc_path, &Default::default(), &mut paths, &mut unresolved_claude).unwrap();
            ProcessInspection { paths, unresolved_claude } };
        let unknown = inspect();
        assert!(unknown.paths.contains(&PathBuf::from(UNKNOWN_CLAUDE)));
        assert_eq!(unknown.unresolved_claude.unresolved_process_count, 1);
        assert_eq!(unknown.unresolved_claude.processes[0].pid, Some(child.0.id()));
        process_receipt::record(&state, &deployment, identity, child.0.id()).unwrap();
        let known = inspect();
        assert_eq!(known.unresolved_claude.unresolved_process_count, 0);
        let paths = known.paths;
        assert!(paths.contains(&adapter.canonicalize().unwrap()));
        assert!(!paths.contains(&PathBuf::from(UNKNOWN_CLAUDE)));
        assert!(!paths.iter().any(|p| super::super::runtime_prune_inventory::overlaps(&candidate,p)));
        drop(child); assert!(state.join(process_receipt::FILE).is_file());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn dependency_tree_preserves_all_process_names_and_directory_boundaries() {
        let root = std::env::temp_dir().join(format!("prune-names-{}", uuid::Uuid::new_v4()));
        let nested = root.join("sdk/dependency/bin");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join("CuStOm-HeLpEr.exe"), b"fixture").unwrap();
        std::fs::write(nested.join("Private-Interpreter.exe"), b"fixture").unwrap();
        let mut expected = std::collections::HashSet::from([
            "custom-helper.exe".to_owned(), "private-interpreter.exe".to_owned(),
        ]);
        for index in 0..4096 {
            let name = format!("dependency-{index}.py");
            std::fs::write(nested.join(&name), b"").unwrap();
            expected.insert(name);
        }
        let start = std::time::Instant::now();
        let mut actual = std::collections::HashSet::new();
        executable_names(&root, &mut actual).unwrap();
        assert_eq!(actual, expected);
        println!("PRUNE_NAMES files={} seconds={:.6}", actual.len(), start.elapsed().as_secs_f64());
        // A file target itself must still participate, not only directory targets.
        actual.clear();
        executable_names(&nested.join("Private-Interpreter.exe"), &mut actual).unwrap();
        assert_eq!(actual, std::collections::HashSet::from(["private-interpreter.exe".into()]));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn global_windows_snapshot_resolves_56_foreign_receipts_but_not_unknown_or_reused_pids() {
        use crate::runtime::claude::process_receipt::{self, ProcessIdentity};
        let root = std::env::temp_dir().join(format!("prune-process-proofs-{}", uuid::Uuid::new_v4()));
        let foreign = root.join("r47");
        let candidate = root.join("r50");
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::create_dir_all(&candidate).unwrap();
        let exe = foreign.join("python.exe");
        std::fs::write(&exe, b"fixture identity only; never executed").unwrap();
        let birth: chrono::DateTime<chrono::Utc> = "2026-11-16T02:09:45.546844Z".parse().unwrap();
        let mut snapshot = Vec::new();
        for pid in 1..=56 {
            let state = root.join("sessions").join(format!("cutex.{}", uuid::Uuid::from_u128(pid as u128)));
            std::fs::create_dir_all(&state).unwrap();
            let identity = ProcessIdentity { pid, started_at: birth, executable: exe.canonicalize().unwrap() };
            std::fs::write(state.join(process_receipt::FILE), serde_json::to_vec(&serde_json::json!({
                "schema":"cutex/claude-process-launch/v1", "state_dir":state.canonicalize().unwrap(),
                "processes":[identity], "python":exe, "adapter_source":foreign, "claude_cli":exe,
            })).unwrap()).unwrap();
            snapshot.push(serde_json::json!({"ProcessId":pid,"Name":"python.exe","ExecutablePath":exe,
                "StartedAt":birth.to_rfc3339(), "CommandLine":format!(
                    "\"{}\" \"-u\" \"-m\" \"cute_claude\" --state-dir \"{}\"", exe.display(), state.display())}));
        }
        let bytes = serde_json::to_vec(&snapshot).unwrap();
        let inspect = |bytes: &[u8], reuse: bool| windows_process_paths(bytes, &Default::default(), |identity,args,out| {
            claude_references_with(identity,args,out,|state,identity| process_receipt::references_with(state,identity,|_| {
                let mut current = identity.clone(); current.executable = current.executable.canonicalize()?;
                if reuse { current.started_at += chrono::Duration::seconds(1); }
                Ok(current)
            }));
        }).unwrap();
        let inspection = inspect(&bytes, false);
        assert_eq!(inspection.unresolved_claude.unresolved_process_count, 0);
        let paths = inspection.paths;
        assert_eq!(paths.iter().filter(|p| *p == &foreign).count(), 56);
        assert!(!paths.contains(&PathBuf::from(UNKNOWN_CLAUDE)));
        assert!(!paths.iter().any(|p| super::super::runtime_prune_inventory::overlaps(&candidate,p)));
        let unresolved = inspect(&bytes, true);
        assert!(unresolved.paths.contains(&PathBuf::from(UNKNOWN_CLAUDE)));
        assert_eq!(unresolved.unresolved_claude.unresolved_process_count, 56);
        assert_eq!(unresolved.unresolved_claude.processes.len(), 20);
        assert_eq!(unresolved.unresolved_claude.processes[0].pid, Some(1));
        assert_eq!(unresolved.unresolved_claude.processes[0].session_id,
            Some(format!("cutex.{}", uuid::Uuid::from_u128(1))));
        assert!(!serde_json::to_string(&unresolved.unresolved_claude).unwrap().contains("sessions"));
        // A real receipt pointing into the candidate protects the imported source even
        // when the interpreter lives outside it.
        let state = root.join("sessions").join(format!("cutex.{}", uuid::Uuid::from_u128(1)));
        let file = state.join(process_receipt::FILE);
        let mut receipt: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        receipt["adapter_source"] = serde_json::json!(candidate);
        std::fs::write(&file,serde_json::to_vec(&receipt).unwrap()).unwrap();
        assert!(inspect(&bytes,false).paths.contains(&candidate));
        std::fs::remove_file(&file).unwrap();
        assert!(inspect(&bytes,false).paths.contains(&PathBuf::from(UNKNOWN_CLAUDE)));
        snapshot[0]["StartedAt"] = serde_json::Value::Null;
        assert!(inspect(&serde_json::to_vec(&snapshot).unwrap(),false).paths.contains(&PathBuf::from(UNKNOWN_CLAUDE)));
        std::fs::remove_dir_all(root).unwrap();
    }
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

#[cfg(windows)]
fn bounded_output(
    mut command: std::process::Command,
    duration: std::time::Duration,
) -> anyhow::Result<std::process::Output> {
    use std::{io::Read, process::Stdio, time::Instant};
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("process inventory pipe unavailable"))?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = Instant::now() + duration;
    let status = loop {
        if Instant::now() >= deadline || super::runtime_prune_inventory::budget::check().is_err() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            anyhow::bail!("process inventory deadline exceeded");
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(error.into());
            }
        }
    };
    let stdout = reader
        .join()
        .map_err(|_| anyhow::anyhow!("process inventory reader failed"))??;
    ensure!(
        stdout.len() <= 16 * 1024 * 1024,
        "process inventory exceeded budget"
    );
    Ok(std::process::Output {
        status,
        stdout,
        stderr: Vec::new(),
    })
}

#[cfg(all(test, windows))]
mod timeout_tests {
    #[test]
    fn owned_process_inventory_child_has_a_real_deadline() {
        use std::os::windows::process::CommandExt;
        let mut command = std::process::Command::new("powershell.exe");
        command
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 10",
            ])
            .creation_flags(0x08000000)
            .stdin(std::process::Stdio::null());
        let start = std::time::Instant::now();
        assert!(super::bounded_output(command, std::time::Duration::from_millis(100)).is_err());
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }
}
