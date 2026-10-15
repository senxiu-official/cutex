//! Explicit handle inheritance for detached runtimes. Never changes the
//! caller's handles (which may concurrently serve an interactive login).
use anyhow::{ensure, Context};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::ExitStatusExt;
use std::process::{Command, ExitStatus};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::Threading::*;

fn check(ok: i32) -> anyhow::Result<()> {
    if ok == 0 { return Err(std::io::Error::last_os_error().into()); }
    Ok(())
}

pub struct Attributes { storage: Vec<usize>, initialized: bool }
impl Attributes {
    pub fn new(count: u32) -> anyhow::Result<Self> {
        let mut bytes = 0;
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &mut bytes); }
        ensure!(bytes > 0, "process attribute allocation failed");
        let mut this = Self { storage: vec![0; bytes.div_ceil(std::mem::size_of::<usize>())], initialized: false };
        check(unsafe { InitializeProcThreadAttributeList(this.ptr(), count, 0, &mut bytes) })?;
        this.initialized = true;
        Ok(this)
    }
    pub fn ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST { self.storage.as_mut_ptr().cast() }
    pub fn set(&mut self, attribute: usize, handles: &mut [HANDLE]) -> anyhow::Result<()> {
        check(unsafe { UpdateProcThreadAttribute(self.ptr(), 0, attribute,
            handles.as_mut_ptr().cast(), std::mem::size_of_val(handles), std::ptr::null_mut(), std::ptr::null()) })
    }
}
impl Drop for Attributes {
    fn drop(&mut self) { if self.initialized { unsafe { DeleteProcThreadAttributeList(self.ptr()); } } }
}

pub fn inheritable(file: &File) -> anyhow::Result<OwnedHandle> {
    let mut handle = std::ptr::null_mut();
    check(unsafe { DuplicateHandle(GetCurrentProcess(), file.as_raw_handle(), GetCurrentProcess(),
        &mut handle, 0, 1, DUPLICATE_SAME_ACCESS) })?;
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

fn wide(value: &OsStr) -> anyhow::Result<Vec<u16>> {
    let mut value: Vec<u16> = value.encode_wide().collect();
    ensure!(!value.contains(&0), "NUL in process parameter");
    value.push(0);
    Ok(value)
}

// CRT quoting without a lossy UTF-8 round trip, including trailing slashes.
fn quoted(value: &OsStr) -> Vec<u16> {
    let mut result = vec![34];
    let mut slashes = 0;
    for c in value.encode_wide() {
        if c == 92 { slashes += 1; continue; }
        result.extend(std::iter::repeat_n(92, if c == 34 { slashes * 2 + 1 } else { slashes }));
        slashes = 0;
        result.push(c);
    }
    result.extend(std::iter::repeat_n(92, slashes * 2));
    result.push(34);
    result
}

pub struct DetachedChild { process: OwnedHandle }
impl DetachedChild {
    pub fn try_wait(&mut self) -> anyhow::Result<Option<ExitStatus>> {
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 => self.status().map(Some),
            WAIT_TIMEOUT => Ok(None),
            _ => Err(std::io::Error::last_os_error().into()),
        }
    }
    fn status(&self) -> anyhow::Result<ExitStatus> {
        let mut code = 0;
        check(unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) })?;
        Ok(ExitStatus::from_raw(code))
    }
    pub fn wait(&mut self) -> anyhow::Result<ExitStatus> {
        ensure!(unsafe { WaitForSingleObject(self.process.as_raw_handle(), INFINITE) } == WAIT_OBJECT_0,
            "failed to wait for detached runtime");
        self.status()
    }
    /// Process id (for status files; never used to kill another process).
    pub fn id(&self) -> u32 {
        unsafe { GetProcessId(self.process.as_raw_handle()) }
    }
    /// TerminateProcess on this exact process only (its children live on).
    pub fn terminate(&self) -> anyhow::Result<()> {
        check(unsafe { TerminateProcess(self.process.as_raw_handle(), 1) })
    }
}

/// Spawn an absolute executable with ordinary CRT arguments, inherited
/// environment plus Command overrides, stdin/stdout=NUL and stderr=log.
/// Callers must not use env_clear, raw_arg, or Command stdio/creation flags;
/// this helper deliberately owns those launch policies.
///
/// The child gets its own hidden console (CREATE_NO_WINDOW), not none
/// (DETACHED_PROCESS): a console-subsystem grandchild of a console-less
/// process allocates a brand-new, visible console window (handed to Windows
/// Terminal when it is the default terminal). The Claude adapter hit this:
/// the venv `python.exe` launcher re-execs the base interpreter, which then
/// runs `claude.exe` in a window titled "Claude". With a hidden console every
/// descendant inherits it and no window appears. Like a detached process, the
/// child still survives the caller's console closing and receives none of
/// its Ctrl-C / Ctrl-Break events (own console, own process group).
pub const DETACHED_CREATION_FLAGS: PROCESS_CREATION_FLAGS =
    CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT;

pub fn spawn_detached(command: &Command, log: &File) -> anyhow::Result<DetachedChild> {
    ensure!(std::path::Path::new(command.get_program()).is_absolute(), "detached executable must be absolute");
    let program = wide(command.get_program())?;
    let mut commandline = Vec::new();
    for arg in std::iter::once(command.get_program()).chain(command.get_args()) {
        if !commandline.is_empty() { commandline.push(32); }
        commandline.extend(quoted(arg));
    }
    ensure!(!commandline.contains(&0), "NUL in process argument");
    commandline.push(0);
    let cwd = command.get_current_dir().map(|p| wide(p.as_os_str())).transpose()?;
    // Windows environment names are case-insensitive. Explicit removals and
    // profile overrides must replace the inherited spelling as well.
    let mut env: BTreeMap<String, (OsString, OsString)> = std::env::vars_os()
        .map(|(k, v)| (k.to_string_lossy().to_uppercase(), (k, v))).collect();
    for (key, value) in command.get_envs() {
        let normalized = key.to_string_lossy().to_uppercase();
        if let Some(value) = value { env.insert(normalized, (key.into(), value.into())); }
        else { env.remove(&normalized); }
    }
    let mut environment = Vec::new();
    for (_, (mut key, value)) in env {
        key.push("="); key.push(value);
        environment.extend(wide(&key)?);
    }
    environment.push(0);
    if environment.len() == 1 { environment.push(0); }
    let null = inheritable(&OpenOptions::new().read(true).write(true).open("NUL")?)?;
    let log = inheritable(log)?;
    let mut handles = [null.as_raw_handle(), log.as_raw_handle()];
    let mut attributes = Attributes::new(1)?;
    attributes.set(PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize, &mut handles)?;
    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = std::mem::size_of_val(&startup) as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = null.as_raw_handle();
    startup.StartupInfo.hStdOutput = null.as_raw_handle();
    startup.StartupInfo.hStdError = log.as_raw_handle();
    startup.lpAttributeList = attributes.ptr();
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    check(unsafe { CreateProcessW(program.as_ptr(), commandline.as_mut_ptr(),
        std::ptr::null(), std::ptr::null(), 1,
        DETACHED_CREATION_FLAGS,
        environment.as_ptr().cast(), cwd.as_ref().map_or(std::ptr::null(), |v| v.as_ptr()),
        &startup.StartupInfo, &mut process) }).context("failed to create detached runtime")?;
    let process_handle = unsafe { OwnedHandle::from_raw_handle(process.hProcess) };
    let _thread = unsafe { OwnedHandle::from_raw_handle(process.hThread) };
    Ok(DetachedChild { process: process_handle })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;

    #[test]
    fn spawn_probe_child() {
        let Ok(path) = std::env::var("CUTEX_SPAWN_PROBE") else { return; };
        let handle = std::env::var("CUTEX_SPAWN_EXTRA_HANDLE").unwrap().parse::<usize>().unwrap() as HANDLE;
        let mut flags = 0;
        let accessible = unsafe { GetHandleInformation(handle, &mut flags) } != 0;
        std::fs::write(path, serde_json::to_vec(&serde_json::json!({
            "extraHandleAccessible": accessible,
            "cwd": std::env::current_dir().unwrap(),
            "value": std::env::var("CUTEX_SPAWN_VALUE").unwrap(),
            "removed": std::env::var_os("CUTEX_SPAWN_REMOVED").is_none(),
        })).unwrap()).unwrap();
        eprintln!("spawn fixture stderr");
    }

    #[test]
    fn whitelist_excludes_inheritable_pipe_and_preserves_environment_and_logs() {
        #[link(name = "kernel32")]
        extern "system" {
            fn CreatePipe(read: *mut HANDLE, write: *mut HANDLE,
                attrs: *const windows_sys::Win32::Security::SECURITY_ATTRIBUTES, size: u32) -> i32;
        }
        let mut read = std::ptr::null_mut();
        let mut write = std::ptr::null_mut();
        check(unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) }).unwrap();
        let _read = unsafe { OwnedHandle::from_raw_handle(read) };
        let write = unsafe { OwnedHandle::from_raw_handle(write) };
        check(unsafe { SetHandleInformation(write.as_raw_handle(), HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) }).unwrap();
        let root = std::env::temp_dir().join(format!("cutex-spawn-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        for whitelist in [false, true] {
            let path = root.join(if whitelist { "after.json" } else { "before.json" });
            let log_path = root.join(if whitelist { "after.log" } else { "before.log" });
            let log = File::create(&log_path).unwrap();
            let mut command = Command::new(std::env::current_exe().unwrap());
            command.args(["--exact", "platform::windows_spawn::tests::spawn_probe_child", "--nocapture"])
                .env("CUTEX_SPAWN_PROBE", &path)
                .env("CUTEX_SPAWN_EXTRA_HANDLE", (write.as_raw_handle() as usize).to_string())
                .env("CUTEX_SPAWN_VALUE", "unicode 测试 with spaces \" and trailing \\")
                .env_remove("CUTEX_SPAWN_REMOVED").current_dir(&root);
            let status = if whitelist {
                spawn_detached(&command, &log).unwrap().wait().unwrap()
            } else {
                command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(log)
                    .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP).spawn().unwrap().wait().unwrap()
            };
            assert!(status.success());
            let evidence: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            assert_eq!(evidence["extraHandleAccessible"], !whitelist, "{evidence}");
            assert_eq!(evidence["cwd"], serde_json::json!(root));
            assert_eq!(evidence["value"], "unicode 测试 with spaces \" and trailing \\");
            assert_eq!(evidence["removed"], true);
            assert!(std::fs::read_to_string(log_path).unwrap().contains("spawn fixture stderr"));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetConsoleWindow() -> HWND;
        fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
    }
    #[link(name = "user32")]
    extern "system" {
        fn IsWindowVisible(window: HWND) -> i32;
    }

    fn console_evidence() -> serde_json::Value {
        let mut pids = [0u32; 64];
        let attached = unsafe { GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) };
        let window = unsafe { GetConsoleWindow() };
        serde_json::json!({
            "pid": std::process::id(),
            "hasConsole": attached > 0,
            "consolePids": pids[..(attached as usize).min(pids.len())].to_vec(),
            "windowVisible": !window.is_null() && unsafe { IsWindowVisible(window) } != 0,
        })
    }

    #[test]
    fn console_probe_grandchild() {
        let Ok(path) = std::env::var("CUTEX_CONSOLE_GRANDCHILD") else { return; };
        std::fs::write(path, serde_json::to_vec(&console_evidence()).unwrap()).unwrap();
    }

    // Plays the venv launcher: a console program that starts another console
    // program with default flags (it inherits our console or, without one,
    // gets a brand-new visible console).
    #[test]
    fn console_probe_child() {
        let Ok(path) = std::env::var("CUTEX_CONSOLE_CHILD") else { return; };
        let grandchild_path = format!("{path}.grandchild");
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "platform::windows_spawn::tests::console_probe_grandchild", "--nocapture"])
            .env_remove("CUTEX_CONSOLE_CHILD")
            .env("CUTEX_CONSOLE_GRANDCHILD", &grandchild_path)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .status().unwrap();
        let grandchild: serde_json::Value = serde_json::from_slice(&std::fs::read(&grandchild_path).unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(&serde_json::json!({
            "status": status.code(), "child": console_evidence(), "grandchild": grandchild,
        })).unwrap()).unwrap();
    }

    #[test]
    fn detached_child_and_console_grandchild_share_one_hidden_console() {
        let root = std::env::temp_dir().join(format!("cutex-spawn-console-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("evidence.json");
        let log = File::create(root.join("child.log")).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "platform::windows_spawn::tests::console_probe_child", "--nocapture"])
            .env("CUTEX_CONSOLE_CHILD", &path).env_remove("CUTEX_CONSOLE_GRANDCHILD");
        assert!(spawn_detached(&command, &log).unwrap().wait().unwrap().success());
        let evidence: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(evidence["status"], 0, "{evidence}");
        let (child, grandchild) = (&evidence["child"], &evidence["grandchild"]);
        // DETACHED_PROCESS would leave the child without a console and give the
        // grandchild a fresh visible one.
        assert_eq!(child["hasConsole"], true, "{evidence}");
        assert_eq!(child["windowVisible"], false, "{evidence}");
        assert_eq!(grandchild["hasConsole"], true, "{evidence}");
        assert_eq!(grandchild["windowVisible"], false, "{evidence}");
        let shared = grandchild["consolePids"].as_array().unwrap();
        assert!(shared.contains(&child["pid"]), "grandchild inherited the child's console: {evidence}");
        // Own console: the test runner's console (if any) is not shared.
        assert!(!shared.contains(&serde_json::json!(std::process::id())), "{evidence}");
        std::fs::remove_dir_all(root).unwrap();
    }

    // Opt-in: CUTEX_CONSOLE_VENV_PYTHON = a venv's Scripts\python.exe (the
    // launcher that re-execs the base interpreter, as the Claude adapter does).
    #[test]
    fn venv_python_launcher_gets_hidden_console() {
        let Ok(python) = std::env::var("CUTEX_CONSOLE_VENV_PYTHON") else { return; };
        let root = std::env::temp_dir().join(format!("cutex-spawn-venv-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("evidence.json");
        let log = File::create(root.join("python.log")).unwrap();
        let script = "import ctypes,json,os,sys\n\
            k=ctypes.windll.kernel32;u=ctypes.windll.user32\n\
            k.GetConsoleWindow.restype=ctypes.c_void_p\n\
            w=k.GetConsoleWindow();a=(ctypes.c_uint*64)()\n\
            json.dump({'exe':sys.executable,'pid':os.getpid(),'hasConsole':k.GetConsoleProcessList(a,64)>0,\
            'windowVisible':bool(w) and u.IsWindowVisible(ctypes.c_void_p(w))!=0},open(sys.argv[1],'w'))";
        let mut command = Command::new(python);
        command.args(["-u", "-c", script]).arg(&path);
        assert!(spawn_detached(&command, &log).unwrap().wait().unwrap().success(),
            "{}", std::fs::read_to_string(root.join("python.log")).unwrap_or_default());
        let evidence: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        eprintln!("venv evidence: {evidence}");
        assert_eq!(evidence["hasConsole"], true, "{evidence}");
        assert_eq!(evidence["windowVisible"], false, "{evidence}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn command_line_quotes_empty_unicode_and_trailing_backslashes() {
        let text = |s: &str| String::from_utf16(&quoted(OsStr::new(s))).unwrap();
        assert_eq!(text(""), "\"\"");
        assert_eq!(text("测试 a\\"), "\"测试 a\\\\\"");
        assert_eq!(text("a\\\"b"), "\"a\\\\\\\"b\"");
    }
}
