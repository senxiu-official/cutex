//! macOS process identity: the libproc / sysctl equivalents of the `/proc`
//! reads the Linux paths use (docs/macos/PROBE.md).
//!
//! - `stat(pid)`: parent, process group, session, start time and zombie
//!   state (`/proc/<pid>/stat` on Linux). `start_ticks` is the start time in
//!   microseconds since the Unix epoch: like Linux birth ticks it is fixed
//!   for the life of a process and changes when a PID is reused.
//! - `all_pids()`: every visible PID (`readdir("/proc")`).
//! - `exe(pid)` / `cwd(pid)`: `/proc/<pid>/exe` / `/proc/<pid>/cwd`.
//! - `unix_peer(stream)`: the peer PID and effective UID of a connected Unix
//!   socket (`SO_PEERCRED`).
//! - `open_writers(path)`: PIDs holding `path` open for writing (scans
//!   `/proc/*/fd` on Linux; `lsof` here).
//! - `boot_time()`: `kern.boottime` (`/proc/stat` `btime`).

use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

/// macOS `p_stat` value of a zombie (`SZOMB`, sys/proc.h).
const SZOMB: u32 = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcStat {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
    pub sid: u32,
    /// Start time, microseconds since the Unix epoch.
    pub start_ticks: u64,
    pub zombie: bool,
    pub uid: u32,
}

impl ProcStat {
    pub fn started_at(&self) -> Option<DateTime<Utc>> {
        let secs = i64::try_from(self.start_ticks / 1_000_000).ok()?;
        let nanos = ((self.start_ticks % 1_000_000) * 1_000) as u32;
        DateTime::from_timestamp(secs, nanos)
    }
}

fn bsdinfo(pid: u32) -> io::Result<Option<libc::proc_bsdinfo>> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Ok(None);
    }
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if written == size {
        return Ok(Some(info));
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        // Gone (ESRCH), or never visible to us. A process of another user is
        // readable through PROC_PIDTBSDINFO; EPERM only hides kernel tasks.
        Some(libc::ESRCH) | Some(0) | None => Ok(None),
        _ if !crate::platform::process::process_is_running(pid) => Ok(None),
        _ => Err(error),
    }
}

/// Identity of one live (or zombie) process; `None` when it does not exist.
pub fn stat(pid: u32) -> io::Result<Option<ProcStat>> {
    let Some(info) = bsdinfo(pid)? else {
        return Ok(None);
    };
    let sid = unsafe { libc::getsid(pid as libc::pid_t) };
    // A zombie has no session any more; report its group as the session so
    // scope matching keeps treating it as a member until it is reaped.
    let sid = if sid < 0 { info.pbi_pgid } else { sid as u32 };
    Ok(Some(ProcStat {
        pid: info.pbi_pid,
        ppid: info.pbi_ppid,
        pgid: info.pbi_pgid,
        sid,
        start_ticks: info
            .pbi_start_tvsec
            .saturating_mul(1_000_000)
            .saturating_add(info.pbi_start_tvusec),
        zombie: info.pbi_status == SZOMB,
        uid: info.pbi_uid,
    }))
}

/// Every PID the kernel lists (the list is a snapshot; entries may be gone).
pub fn all_pids() -> io::Result<Vec<u32>> {
    let mut capacity = 4096usize;
    loop {
        let mut buffer = vec![0 as libc::pid_t; capacity];
        let bytes = (buffer.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        let count = unsafe { libc::proc_listallpids(buffer.as_mut_ptr().cast(), bytes) };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        let count = count as usize;
        if count < capacity {
            buffer.truncate(count);
            return Ok(buffer.into_iter().filter(|pid| *pid > 0).map(|pid| pid as u32).collect());
        }
        capacity *= 2;
    }
}

/// Every live process this user may inspect, with its identity. Processes
/// the kernel refuses to describe (EPERM: other users' protected or
/// sandboxed processes) are left out: a process of this user is always
/// readable, so none of ours is missed.
pub fn snapshot() -> io::Result<Vec<ProcStat>> {
    let mut found = Vec::new();
    for pid in all_pids()? {
        match stat(pid) {
            Ok(Some(stat)) => found.push(stat),
            Ok(None) => {}
            Err(error) if matches!(error.raw_os_error(), Some(libc::EPERM) | Some(libc::EACCES)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(found)
}

/// Executable image path; `None` when the process does not exist.
pub fn exe(pid: u32) -> io::Result<Option<PathBuf>> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Ok(None);
    }
    // PROC_PIDPATHINFO_MAXSIZE = 4 * MAXPATHLEN.
    let mut buffer = vec![0u8; 4 * 1024];
    let len = unsafe {
        libc::proc_pidpath(pid as libc::c_int, buffer.as_mut_ptr().cast(), buffer.len() as u32)
    };
    if len <= 0 {
        if !crate::platform::process::process_is_running(pid) {
            return Ok(None);
        }
        return Err(io::Error::last_os_error());
    }
    buffer.truncate(len as usize);
    use std::os::unix::ffi::OsStringExt;
    Ok(Some(PathBuf::from(std::ffi::OsString::from_vec(buffer))))
}

/// Current working directory; `None` when the process does not exist.
pub fn cwd(pid: u32) -> io::Result<Option<PathBuf>> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Ok(None);
    }
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&mut info as *mut libc::proc_vnodepathinfo).cast(),
            size,
        )
    };
    if written != size {
        if !crate::platform::process::process_is_running(pid) {
            return Ok(None);
        }
        return Err(io::Error::last_os_error());
    }
    // `vip_path` is a MAXPATHLEN C string stored as [[c_char; 32]; 32].
    let raw: &[u8] = unsafe {
        std::slice::from_raw_parts(
            info.pvi_cdir.vip_path.as_ptr().cast::<u8>(),
            std::mem::size_of_val(&info.pvi_cdir.vip_path),
        )
    };
    let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
    use std::os::unix::ffi::OsStrExt;
    Ok(Some(PathBuf::from(std::ffi::OsStr::from_bytes(&raw[..end]))))
}

/// Peer PID and effective UID of a connected Unix stream socket.
pub fn unix_peer(stream: &std::os::unix::net::UnixStream) -> Option<(u32, u32)> {
    use std::os::fd::AsRawFd;
    let fd = stream.as_raw_fd();
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut len,
        )
    };
    if result != 0 || len as usize != std::mem::size_of::<libc::pid_t>() || pid <= 0 {
        return None;
    }
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
        return None;
    }
    Some((pid as u32, uid))
}

/// PIDs (other than none) that hold `path` open with write access, from
/// `lsof -F pa` (`p<pid>` then `a<r|w|u>` per descriptor). An lsof failure
/// is an error, never "no writer".
pub fn open_writers(path: &Path) -> anyhow::Result<Vec<u32>> {
    use anyhow::Context;
    let output = std::process::Command::new("/usr/sbin/lsof")
        .args(["-n", "-P", "-w", "-F", "pa", "--"])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .context("cannot run lsof for open-writer detection")?;
    // lsof exits 1 when no process has the file open (and prints nothing).
    if !output.status.success() && !output.stdout.is_empty() {
        anyhow::bail!("lsof failed with {}", output.status);
    }
    if !output.status.success() && output.status.code() != Some(1) {
        anyhow::bail!("lsof failed with {}", output.status);
    }
    Ok(parse_lsof_writers(&String::from_utf8_lossy(&output.stdout)))
}

pub(crate) fn parse_lsof_writers(text: &str) -> Vec<u32> {
    let mut current = None;
    let mut writers = Vec::new();
    for line in text.lines() {
        if let Some(pid) = line.strip_prefix('p') {
            current = pid.trim().parse::<u32>().ok();
        } else if let Some(access) = line.strip_prefix('a') {
            if matches!(access.trim(), "w" | "u") {
                if let Some(pid) = current {
                    if !writers.contains(&pid) {
                        writers.push(pid);
                    }
                }
            }
        }
    }
    writers
}

/// Wall-clock time this boot started (`sysctl kern.boottime`).
pub fn boot_time() -> anyhow::Result<DateTime<Utc>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];
    let mut value: libc::timeval = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::timeval>();
    let result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            (&mut value as *mut libc::timeval).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error().into());
    }
    DateTime::from_timestamp(value.tv_sec, (value.tv_usec as u32).saturating_mul(1_000))
        .ok_or_else(|| anyhow::anyhow!("boot time is out of range"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_identity_is_observable() {
        let me = std::process::id();
        let stat = stat(me).unwrap().expect("own process");
        assert_eq!(stat.pid, me);
        assert_eq!(stat.ppid, unsafe { libc::getppid() } as u32);
        assert_eq!(stat.pgid, unsafe { libc::getpgrp() } as u32);
        assert_eq!(stat.sid, unsafe { libc::getsid(0) } as u32);
        assert!(!stat.zombie);
        let started = stat.started_at().unwrap();
        assert!(started <= Utc::now());
        assert!(started >= boot_time().unwrap());
        assert_eq!(exe(me).unwrap().unwrap(), std::env::current_exe().unwrap().canonicalize().unwrap());
        assert_eq!(cwd(me).unwrap().unwrap(), std::env::current_dir().unwrap().canonicalize().unwrap());
        assert!(all_pids().unwrap().contains(&me));
        assert!(snapshot().unwrap().iter().any(|p| p.pid == me));
    }

    #[test]
    fn missing_process_is_none() {
        // PIDs are below 100000 on macOS (PID_MAX 99999).
        assert_eq!(stat(i32::MAX as u32).unwrap(), None);
        assert_eq!(exe(i32::MAX as u32).unwrap(), None);
        assert_eq!(stat(0).unwrap(), None);
    }

    #[test]
    fn unix_peer_reports_this_process() {
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        let (pid, uid) = unix_peer(&a).unwrap();
        assert_eq!(pid, std::process::id());
        assert_eq!(uid, unsafe { libc::geteuid() });
    }

    #[test]
    fn lsof_writer_parse() {
        let text = "p10\nar\np11\naw\nar\np12\nau\np11\naw\n";
        assert_eq!(parse_lsof_writers(text), vec![11, 12]);
    }

    #[test]
    fn open_writer_detection_sees_own_write_handle() {
        let path = std::env::temp_dir().join(format!("cutex-macos-writer-{}", uuid::Uuid::new_v4()));
        let file = std::fs::File::create(&path).unwrap();
        assert!(open_writers(&path).unwrap().contains(&std::process::id()));
        drop(file);
        let reader = std::fs::File::open(&path).unwrap();
        assert!(!open_writers(&path).unwrap().contains(&std::process::id()));
        drop(reader);
        std::fs::remove_file(&path).unwrap();
    }
}
