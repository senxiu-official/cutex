//! A suspended native child enters its job atomically at process creation.
//! Until durable publication releases it, creator death closes the job and kills it.
use anyhow::{ensure, Context};
use cutex::launch::command::LaunchCommand;
use cutex::platform::windows_spawn::{Attributes, inheritable};
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::SystemServices::{JOB_OBJECT_QUERY, JOB_OBJECT_TERMINATE};
use windows_sys::Win32::System::Threading::*;

fn wide(value: &OsStr) -> anyhow::Result<Vec<u16>> {
    let mut result: Vec<u16> = value.encode_wide().collect();
    ensure!(!result.contains(&0), "NUL in process parameter");
    result.push(0);
    Ok(result)
}

// Windows CRT quoting, including backslashes immediately before a quote/end.
fn quoted(value: &str) -> String {
    let mut result = String::from("\"");
    let mut slashes = 0;
    for c in value.chars() {
        if c == '\\' { slashes += 1; continue; }
        result.extend(std::iter::repeat_n('\\', if c == '"' { slashes * 2 + 1 } else { slashes }));
        slashes = 0;
        result.push(c);
    }
    result.extend(std::iter::repeat_n('\\', slashes * 2));
    result.push('"');
    result
}

fn check(ok: i32) -> anyhow::Result<()> {
    if ok == 0 { return Err(std::io::Error::last_os_error().into()); }
    Ok(())
}
fn job_limits(job: HANDLE, kill_on_close: bool) -> anyhow::Result<()> {
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    if kill_on_close { limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE; }
    check(unsafe { SetInformationJobObject(job, JobObjectExtendedLimitInformation,
        (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(), std::mem::size_of_val(&limits) as u32) })
}

pub struct GatedChild {
    process: OwnedHandle,
    thread: OwnedHandle,
    job: OwnedHandle,
    pid: u32,
    released: bool,
}
impl GatedChild {
    pub fn id(&self) -> u32 { self.pid }
    pub fn start_ephemeral(&mut self) -> anyhow::Result<()> {
        ensure!(unsafe { ResumeThread(self.thread.as_raw_handle()) } != u32::MAX,
            "failed to resume ephemeral native child");
        Ok(())
    }
    pub fn release(&mut self) -> anyhow::Result<()> {
        if !self.released {
            ensure!(unsafe { ResumeThread(self.thread.as_raw_handle()) } != u32::MAX,
                "failed to resume published native child");
            // Keep a job handle inside the now-running owned process. Windows
            // removes a job's name on last handle close even while members live.
            // Transfer after ResumeThread: creator death before transfer kills
            // the child and is recoverable from the already committed receipt.
            let mut retained = std::ptr::null_mut();
            check(unsafe { DuplicateHandle(GetCurrentProcess(), self.job.as_raw_handle(),
                self.process.as_raw_handle(), &mut retained, 0, 0, DUPLICATE_SAME_ACCESS) })?;
            // This handle belongs to the child; its process teardown closes it.
            self.released = true;
        }
        Ok(())
    }
    pub fn try_wait(&mut self) -> anyhow::Result<bool> {
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 => Ok(true), WAIT_TIMEOUT => Ok(false),
            _ => Err(std::io::Error::last_os_error().into()),
        }
    }
    pub fn wait(&mut self) -> anyhow::Result<()> {
        ensure!(unsafe { WaitForSingleObject(self.process.as_raw_handle(), INFINITE) } == WAIT_OBJECT_0,
            "failed to wait for owned runtime");
        Ok(())
    }
    pub fn cleanup(&mut self) -> anyhow::Result<()> {
        // The job contains only this launch and its descendants; no PID tree guessing.
        check(unsafe { TerminateJobObject(self.job.as_raw_handle(), 125) })?;
        self.wait()
    }
}
impl Drop for GatedChild {
    fn drop(&mut self) { let _ = self.cleanup(); }
}

pub fn spawn(launch: &LaunchCommand, cwd: &str, log: &Path, lease: &File) -> anyhow::Result<GatedChild> {
    spawn_with_secret(launch, cwd, log, lease, None)
}
pub fn spawn_with_secret(launch: &LaunchCommand, cwd: &str, log: &Path, _lease: &File,
    secret: Option<&cutex::launch::selected_profile::ApiKey>) -> anyhow::Result<GatedChild> {
    let program = wide(OsStr::new(&launch.program))?;
    let cwd = wide(OsStr::new(cwd))?;
    let mut commandline = wide(OsStr::new(&std::iter::once(&launch.program).chain(&launch.args)
        .map(|arg| quoted(arg)).collect::<Vec<_>>().join(" ")))?;
    let mut command = launch.to_command();
    if let Some(secret) = secret { secret.apply(&mut command); }
    // Launch plans supply the complete environment, like the Linux execve path.
    let mut env: Vec<_> = command.get_envs().filter_map(|(k,v)| v.map(|v| (k,v))).collect();
    env.sort_by_key(|(key,_)| key.to_string_lossy().to_uppercase());
    let mut environment = Vec::<u16>::new();
    for (key,value) in env {
        let mut pair = key.to_os_string(); pair.push("="); pair.push(value);
        environment.extend(wide(&pair)?);
    }
    environment.push(0);
    if environment.len() == 1 { environment.push(0); }
    let (device, inode) = cutex::platform::private_fs::identity(_lease)?.publication_key();
    let name = job_name(device, inode)?;
    let job = unsafe { CreateJobObjectW(std::ptr::null(), name.as_ptr()) };
    let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    ensure!(!job.is_null(), "failed to create runtime job: {}", std::io::Error::last_os_error());
    let job = unsafe { OwnedHandle::from_raw_handle(job) };
    ensure!(!existed, "publication job still exists; recover its existing runtime");
    cutex::platform::private_fs::secure_runtime_object(job.as_raw_handle())?;
    job_limits(job.as_raw_handle(), true)?;
    let log = OpenOptions::new().create(true).append(true).open(log)?;
    let log = inheritable(&log)?;
    let null = inheritable(&File::open("NUL")?)?;
    let mut jobs = [job.as_raw_handle()];
    let mut handles = [log.as_raw_handle(), null.as_raw_handle()];
    let mut attributes = Attributes::new(2)?;
    attributes.set(PROC_THREAD_ATTRIBUTE_JOB_LIST as usize, &mut jobs)?;
    attributes.set(PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize, &mut handles)?;
    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = std::mem::size_of_val(&startup) as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = null.as_raw_handle();
    startup.StartupInfo.hStdOutput = log.as_raw_handle();
    startup.StartupInfo.hStdError = log.as_raw_handle();
    startup.lpAttributeList = attributes.ptr();
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    check(unsafe { CreateProcessW(program.as_ptr(), commandline.as_mut_ptr(),
        std::ptr::null(), std::ptr::null(), 1,
        CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
        environment.as_ptr().cast(), cwd.as_ptr(), &startup.StartupInfo, &mut process) })
        .context("failed to create suspended runtime in owned job")?;
    let child = GatedChild { process: unsafe { OwnedHandle::from_raw_handle(process.hProcess) },
        thread: unsafe { OwnedHandle::from_raw_handle(process.hThread) }, job,
        pid: process.dwProcessId, released: false };
    // SSH services may run elevated with a default DACL that omits TokenUser.
    // Let the same user's normal TUI inspect its owner without elevation.
    // Keep the child gated; any ACL failure drops it before publication.
    cutex::platform::private_fs::secure_runtime_object(child.process.as_raw_handle())
        .context("Set native process access for the current user")?;
    Ok(child)
}

pub fn lease(claim: &str, prior: Option<&cutex::agent_management::StockPublication>)
    -> anyhow::Result<(cutex::agent_management::StockPublication, File)> {
    use cutex::platform::private_fs;
    use fs2::FileExt;
    ensure!(uuid::Uuid::parse_str(claim)?.to_string() == claim, "invalid publication claim");
    let root = cutex::session::store::cutex_sessions_path()?.parent().context("store parent")?
        .join("runtime/stock-claims");
    std::fs::create_dir_all(&root)?;
    let (_, identity) = private_fs::secure_directory(&root)?;
    let flags = libc::O_RDWR | if prior.is_none() { libc::O_CREAT | libc::O_EXCL } else { 0 };
    let path = root.join(format!("{claim}.lock"));
    let file = private_fs::open_child(&root, identity, &format!("{claim}.lock"), flags, false)?;
    let (device, inode) = private_fs::identity(&file)?.publication_key();
    let publication = cutex::agent_management::StockPublication { path, device, inode };
    if let Some(prior) = prior { ensure!(&publication == prior, "publication evidence replaced"); }
    file.try_lock_exclusive().context("publication_busy: original creator still owns claim")?;
    Ok((publication, file))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_publication_parent_fixture() {
        let Ok(root) = std::env::var("CUTEX_WINDOWS_PARENT_FIXTURE") else { return; };
        let root = std::path::PathBuf::from(root);
        let lease = File::create(root.join("lease")).unwrap();
        let executable = format!("{}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe", std::env::var("SystemRoot").unwrap());
        let launch = LaunchCommand::new(&executable).args(["-NoProfile", "-Command", "Start-Sleep -Seconds 60"])
            .env("SystemRoot", std::env::var("SystemRoot").unwrap());
        let mut child = spawn(&launch, root.to_str().unwrap(), &root.join("log"), &lease).unwrap();
        let (device, inode) = cutex::platform::private_fs::identity(&lease).unwrap().publication_key();
        let published = std::env::var("CUTEX_WINDOWS_PARENT_PUBLISH").as_deref() == Ok("1");
        if published { child.release().unwrap(); }
        let evidence = serde_json::json!({"pid":child.id(), "started":cutex::platform::process::process_started_at(child.id()).unwrap().to_rfc3339(),
            "executable":executable, "device":device, "inode":inode});
        std::fs::write(root.join("ready"), serde_json::to_vec(&evidence).unwrap()).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(60));
    }

    #[test]
    fn windows_publication_parent_death_cleans_pending_and_preserves_published() {
        for published in [false, true] {
            let root = std::env::temp_dir().join(format!("cutex-parent-death-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            let mut parent = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "cli_app::stock_publication::windows::tests::windows_publication_parent_fixture", "--nocapture"])
                .env("CUTEX_WINDOWS_PARENT_FIXTURE", &root)
                .env("CUTEX_WINDOWS_PARENT_PUBLISH", if published { "1" } else { "0" })
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            let evidence: serde_json::Value = loop {
                if let Ok(bytes) = std::fs::read(root.join("ready")) {
                    if let Ok(value) = serde_json::from_slice(&bytes) { break value; }
                }
                if std::time::Instant::now() >= deadline {
                    let _ = parent.kill(); let _ = parent.wait(); panic!("fixture did not publish evidence");
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            };
            let pid = evidence["pid"].as_u64().unwrap() as u32;
            let started = evidence["started"].as_str().unwrap();
            let executable = Path::new(evidence["executable"].as_str().unwrap());
            let process = verified_process(pid, started, executable).unwrap().unwrap();
            parent.kill().unwrap(); parent.wait().unwrap();
            if published {
                assert_eq!(unsafe { WaitForSingleObject(process.as_raw_handle(), 100) }, WAIT_TIMEOUT);
                let publication = cutex::agent_management::StockPublication { path:root.join("lease"),
                    device:evidence["device"].as_u64().unwrap(), inode:evidence["inode"].as_u64().unwrap() };
                stop_published_job(&publication, Some(&process)).unwrap();
            } else {
                assert_eq!(unsafe { WaitForSingleObject(process.as_raw_handle(), 5000) }, WAIT_OBJECT_0);
            }
            drop(process);
            std::fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn windows_publication_suspends_until_release_and_cleans_unreleased_child() {
        let root = std::env::temp_dir().join(format!("cutex-gate-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let lease = File::create(root.join("lease")).unwrap();
        let launch = LaunchCommand::new(format!("{}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe", std::env::var("SystemRoot").unwrap()))
            .args(["-NoProfile", "-Command", "Set-Content -LiteralPath proof -Value launched"])
            .env("SystemRoot", std::env::var("SystemRoot").unwrap());
        let mut child = spawn(&launch, root.to_str().unwrap(), &root.join("log"), &lease).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!root.join("proof").exists());
        assert!(!child.try_wait().unwrap());
        child.cleanup().unwrap();
        drop(child);
        assert!(!root.join("proof").exists());
        let mut child = spawn(&launch, root.to_str().unwrap(), &root.join("log"), &lease).unwrap();
        child.release().unwrap();
        child.wait().unwrap();
        assert!(root.join("proof").exists());
        drop(child); drop(lease);
        std::fs::remove_dir_all(root).unwrap();
    }
}

fn job_name(device: u64, inode: u64) -> anyhow::Result<Vec<u16>> {
    wide(OsStr::new(&format!("Global\\CutexRuntime-{device:016x}{inode:016x}")))
}

pub fn verified_process(pid: u32, started_at: &str, executable: &Path) -> anyhow::Result<Option<OwnedHandle>> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) { return Ok(None); }
        return Err(error.into());
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    if unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } == WAIT_OBJECT_0 { return Ok(None); }
    let mut creation: FILETIME = unsafe { std::mem::zeroed() };
    let mut exit = creation; let mut kernel = creation; let mut user = creation;
    check(unsafe { GetProcessTimes(handle.as_raw_handle(), &mut creation, &mut exit, &mut kernel, &mut user) })?;
    let ticks = ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64;
    let unix = ticks.checked_sub(116_444_736_000_000_000).context("invalid process time")?;
    let expected = chrono::DateTime::parse_from_rfc3339(started_at)?;
    ensure!(expected.timestamp() == (unix / 10_000_000) as i64 && expected.timestamp_subsec_nanos() == ((unix % 10_000_000) * 100) as u32,
        "runtime PID was reused");
    let mut image = vec![0u16; 32768]; let mut len = image.len() as u32;
    check(unsafe { QueryFullProcessImageNameW(handle.as_raw_handle(), 0, image.as_mut_ptr(), &mut len) })?;
    let actual = std::path::PathBuf::from(String::from_utf16(&image[..len as usize])?).canonicalize()?;
    ensure!(actual == executable.canonicalize()?, "runtime executable mismatch");
    Ok(Some(handle))
}

pub fn stop_published_job(publication: &cutex::agent_management::StockPublication,
    process: Option<&OwnedHandle>) -> anyhow::Result<()> {
    let name = job_name(publication.device, publication.inode)?;
    let job = unsafe { OpenJobObjectW(JOB_OBJECT_QUERY | JOB_OBJECT_TERMINATE, 0, name.as_ptr()) };
    if job.is_null() {
        let error = std::io::Error::last_os_error();
        if process.is_none() && error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) { return Ok(()); }
        return Err(error.into());
    }
    let job = unsafe { OwnedHandle::from_raw_handle(job) };
    if let Some(process) = process {
        let mut belongs = 0;
        check(unsafe { IsProcessInJob(process.as_raw_handle(), job.as_raw_handle(), &mut belongs) })?;
        ensure!(belongs != 0, "runtime is outside its published job");
    }
    check(unsafe { TerminateJobObject(job.as_raw_handle(), 0) })?;
    if let Some(process) = process {
        ensure!(unsafe { WaitForSingleObject(process.as_raw_handle(), 5000) } == WAIT_OBJECT_0,
            "runtime stop timeout");
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        check(unsafe { QueryInformationJobObject(job.as_raw_handle(), JobObjectBasicAccountingInformation,
            (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
            std::mem::size_of_val(&info) as u32, std::ptr::null_mut()) })?;
        if info.ActiveProcesses == 0 { break; }
        ensure!(std::time::Instant::now() < deadline, "runtime descendants stop timeout");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Ok(())
}
