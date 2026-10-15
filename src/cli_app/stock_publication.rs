//! One gated child, not a supervisor. No native exec before durable publication.
//! An inherited flock proves that an unpublished child cannot outlive its lease.
#[cfg(target_os = "linux")]
mod linux {
    use anyhow::{ensure, Context};
    use cutex::agent_management::StockPublication;
    use cutex::launch::command::LaunchCommand;
    use fs2::FileExt;
    use std::ffi::CString;
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
    use std::os::unix::net::UnixStream;
    use std::path::Path;

    pub fn lease(
        claim: &str,
        prior: Option<&StockPublication>,
    ) -> anyhow::Result<(StockPublication, File)> {
        ensure!(
            uuid::Uuid::parse_str(claim)?.to_string() == claim,
            "invalid publication claim"
        );
        let dir = cutex::session::store::cutex_sessions_path()?
            .parent()
            .context("store parent")?
            .join("runtime/stock-claims");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        let path = dir.join(format!("{claim}.lock"));
        ensure!(
            dir.canonicalize()? == dir,
            "publication root must not be symlinked"
        );
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW);
        if prior.is_none() {
            options.create_new(true);
        }
        let file = options
            .open(&path)
            .context("publication evidence unavailable; no guessed owner")?;
        let meta = file.metadata()?;
        ensure!(
            meta.is_file()
                && meta.uid() == unsafe { libc::geteuid() }
                && meta.nlink() == 1
                && meta.mode() & 0o077 == 0,
            "foreign publication evidence"
        );
        let publication = StockPublication {
            path,
            device: meta.dev(),
            inode: meta.ino(),
        };
        if let Some(prior) = prior {
            ensure!(
                &publication == prior,
                "publication evidence replaced; no recovery"
            );
        }
        file.try_lock_exclusive().context("publication_busy: original creator/child still owns claim; retry exact action after it exits")?;
        Ok((publication, file))
    }

    pub struct GatedChild {
        pid: u32,
        gate: Option<UnixStream>,
        released: bool,
        waited: bool,
    }
    impl GatedChild {
        pub fn id(&self) -> u32 {
            self.pid
        }
        pub fn release(&mut self) -> anyhow::Result<()> {
            if let Some(mut gate) = self.gate.take() {
                gate.write_all(b"G")?;
                self.released = true;
            }
            Ok(())
        }
        pub fn try_wait(&mut self) -> anyhow::Result<bool> {
            if self.waited {
                return Ok(true);
            }
            let mut status = 0;
            let result = unsafe { libc::waitpid(self.pid as i32, &mut status, libc::WNOHANG) };
            if result < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            self.waited = result != 0;
            Ok(self.waited)
        }
        pub fn wait(&mut self) -> anyhow::Result<()> {
            while !self.waited {
                let mut status = 0;
                let result = unsafe { libc::waitpid(self.pid as i32, &mut status, 0) };
                if result < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error.into());
                }
                self.waited = true;
            }
            Ok(())
        }
        pub fn cleanup(&mut self) -> anyhow::Result<()> {
            self.gate.take(); // EOF prevents any unpublished exec, including creator death.
            if !self.try_wait()? {
                let target = if self.released {
                    -(self.pid as i32)
                } else {
                    self.pid as i32
                };
                // This is our unreaped child, never a discovered/guessed process.
                ensure!(
                    unsafe { libc::kill(target, libc::SIGKILL) } == 0,
                    "owned publication child cleanup failed"
                );
            }
            self.wait()
        }
    }
    impl Drop for GatedChild {
        fn drop(&mut self) {
            let _ = self.cleanup();
        }
    }

    pub fn spawn(
        launch: &LaunchCommand,
        cwd: &str,
        log: &Path,
        lease: &File,
    ) -> anyhow::Result<GatedChild> {
        spawn_with_secret(launch, cwd, log, lease, None)
    }
    pub fn spawn_with_secret(
        launch: &LaunchCommand,
        cwd: &str,
        log: &Path,
        lease: &File,
        secret: Option<&cutex::launch::selected_profile::ApiKey>,
    ) -> anyhow::Result<GatedChild> {
        let executable = CString::new(launch.program.as_bytes())?;
        let args: Vec<CString> = std::iter::once(&launch.program)
            .chain(&launch.args)
            .map(|s| CString::new(s.as_bytes()))
            .collect::<Result<_, _>>()?;
        let mut env: Vec<CString> = launch
            .envs
            .iter()
            .map(|(k, v)| CString::new(format!("{k}={v}")))
            .collect::<Result<_, _>>()?;
        if let Some(secret) = secret {
            ensure!(
                !launch.envs.iter().any(|(k, _)| k == "OPENAI_API_KEY"),
                "conflicting API credential source"
            );
            env.push(secret.as_exec_env()?);
        }
        let mut argv: Vec<_> = args.iter().map(|s| s.as_ptr()).collect();
        argv.push(std::ptr::null());
        let mut envp: Vec<_> = env.iter().map(|s| s.as_ptr()).collect();
        envp.push(std::ptr::null());
        let cwd = CString::new(cwd)?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(log)?;
        let null = File::open("/dev/null")?;
        let (mut parent, child) = UnixStream::pair()?;
        parent.set_read_timeout(Some(std::time::Duration::from_secs(15)))?;
        // Preallocate all memory and duplicate above fixed child descriptors before fork.
        let duplicate = |fd| -> anyhow::Result<File> {
            let fd = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 100) };
            ensure!(fd >= 0, "publication descriptor duplication failed");
            Ok(unsafe { File::from_raw_fd(fd) })
        };
        let gate_fd = duplicate(child.as_raw_fd())?;
        let lease_fd = duplicate(lease.as_raw_fd())?;
        let log_fd = duplicate(log.as_raw_fd())?;
        let null_fd = duplicate(null.as_raw_fd())?;
        let pid = unsafe { libc::fork() };
        ensure!(pid >= 0, "stock gated fork failed");
        if pid == 0 {
            // Only async-signal-safe syscalls after fork in the multithreaded owner.
            unsafe {
                if libc::setsid() < 0
                    || libc::chdir(cwd.as_ptr()) != 0
                    || libc::dup2(null_fd.as_raw_fd(), 0) < 0
                    || libc::dup2(log_fd.as_raw_fd(), 1) < 0
                    || libc::dup2(log_fd.as_raw_fd(), 2) < 0
                    || libc::dup2(gate_fd.as_raw_fd(), 3) < 0
                    || libc::dup2(lease_fd.as_raw_fd(), 4) < 0
                    || libc::fcntl(3, libc::F_SETFD, libc::FD_CLOEXEC) < 0
                    || libc::fcntl(4, libc::F_SETFD, libc::FD_CLOEXEC) < 0
                    || libc::syscall(libc::SYS_close_range, 5u32, u32::MAX, 0u32) != 0
                {
                    libc::_exit(126);
                }
                if libc::write(3, b"R".as_ptr().cast(), 1) != 1 {
                    libc::_exit(126);
                }
                let mut go = 0u8;
                if libc::read(3, (&mut go as *mut u8).cast(), 1) != 1 || go != b'G' {
                    libc::_exit(125);
                }
                libc::execve(executable.as_ptr(), argv.as_ptr(), envp.as_ptr());
                libc::_exit(127);
            }
        }
        drop(child);
        drop(gate_fd);
        drop(lease_fd);
        drop(log_fd);
        drop(null_fd);
        let mut owned = GatedChild {
            pid: pid as u32,
            gate: None,
            released: false,
            waited: false,
        };
        let mut ready = [0];
        parent
            .read_exact(&mut ready)
            .context("gated child setup failed before native exec")?;
        ensure!(ready == *b"R", "invalid gated child setup");
        owned.gate = Some(parent);
        Ok(owned)
    }
}
#[cfg(target_os = "linux")]
pub(super) use linux::*;

#[cfg(not(any(target_os = "linux", windows)))]
mod unsupported {
    use cutex::agent_management::StockPublication;
    pub struct GatedChild;
    impl GatedChild {
        pub fn id(&self) -> u32 {
            0
        }
        pub fn release(&mut self) -> anyhow::Result<()> {
            anyhow::bail!("stock publication requires Linux")
        }
        pub fn try_wait(&mut self) -> anyhow::Result<bool> {
            anyhow::bail!("stock publication requires Linux")
        }
        pub fn wait(&mut self) -> anyhow::Result<()> {
            anyhow::bail!("stock publication requires Linux")
        }
        pub fn cleanup(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
    }
    pub fn lease(
        _: &str,
        _: Option<&StockPublication>,
    ) -> anyhow::Result<(StockPublication, std::fs::File)> {
        anyhow::bail!("stock publication requires Linux")
    }
    pub fn spawn(
        _: &cutex::launch::command::LaunchCommand,
        _: &str,
        _: &std::path::Path,
        _: &std::fs::File,
    ) -> anyhow::Result<GatedChild> {
        anyhow::bail!("stock publication requires Linux")
    }
    pub fn spawn_with_secret(
        _: &cutex::launch::command::LaunchCommand,
        _: &str,
        _: &std::path::Path,
        _: &std::fs::File,
        _: Option<&cutex::launch::selected_profile::ApiKey>,
    ) -> anyhow::Result<GatedChild> {
        anyhow::bail!("stock publication requires Linux")
    }
}
#[cfg(not(any(target_os = "linux", windows)))]
pub(super) use unsupported::*;

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use cutex::launch::command::LaunchCommand;
    use std::io::{BufRead, Read, Write};

    #[test]
    fn stock_publication_creator_fixture() {
        let Ok(root) = std::env::var("CUTEX_STOCK_CREATOR_FIXTURE") else {
            return;
        };
        let claim = std::env::var("CUTEX_STOCK_CREATOR_CLAIM").unwrap();
        let (publication, file) = lease(&claim, None).unwrap();
        let launch = LaunchCommand::new("/bin/sh").args(["-c", "printf launched >> native-proof"]);
        let child = spawn(
            &launch,
            &root,
            &std::path::Path::new(&root).join("child.log"),
            &file,
        )
        .unwrap();
        println!(
            "PUBLICATION {}",
            serde_json::json!({"publication": publication, "pid":child.id()})
        );
        std::io::stdout().flush().unwrap();
        let mut input = [0];
        let _ = std::io::stdin().read(&mut input); // parent kills us, never releases native exec
        panic!("creator should be killed at the precommit boundary");
    }

    #[test]
    fn stock_publication_real_creator_death_same_lease_replay_and_negative_evidence() {
        let home = crate::cli_app::test_home::IsolatedTestHome::new("p").unwrap();
        let root = home.root();
        let claim = uuid::Uuid::new_v4().to_string();
        let mut creator = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cli_app::stock_publication::tests::stock_publication_creator_fixture",
                "--nocapture",
            ])
            .env("CUTEX_STOCK_CREATOR_FIXTURE", root)
            .env("CUTEX_STOCK_CREATOR_CLAIM", &claim)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = std::io::BufReader::new(creator.stdout.take().unwrap());
        let evidence: serde_json::Value = loop {
            let mut line = String::new();
            assert!(output.read_line(&mut line).unwrap() > 0);
            if let Some(json) = line.strip_prefix("PUBLICATION ") {
                break serde_json::from_str(json).unwrap();
            }
        };
        let publication = serde_json::from_value(evidence["publication"].clone()).unwrap();
        let unpublished = evidence["pid"].as_u64().unwrap() as u32;
        assert!(!root.join("native-proof").exists());
        assert!(lease(&claim, Some(&publication))
            .unwrap_err()
            .to_string()
            .contains("publication_busy"));
        // Actual SIGKILL, not Drop cleanup. Only the owned creator is targeted.
        creator.kill().unwrap();
        creator.wait().unwrap();
        // Blocking on the exact flock is an event oracle: its inherited holder
        // must exit on gate EOF before absence is established, no sleep proof.
        let witness = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&publication.path)
            .unwrap();
        fs2::FileExt::lock_exclusive(&witness).unwrap();
        drop(witness);
        let (same, file) = lease(&claim, Some(&publication)).unwrap();
        assert_eq!(same, publication);
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{unpublished}/stat")) {
            assert_eq!(
                stat.rsplit(')').next().unwrap().split_whitespace().next(),
                Some("Z")
            );
        }
        assert!(!root.join("native-proof").exists());
        let launch = LaunchCommand::new("/bin/sh").args(["-c", "printf launched >> native-proof"]);
        let mut child = spawn(
            &launch,
            root.to_str().unwrap(),
            &root.join("child.log"),
            &file,
        )
        .unwrap();
        assert_ne!(child.id(), unpublished);
        assert!(!root.join("native-proof").exists());
        child.release().unwrap();
        drop(file);
        child.wait().unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("native-proof")).unwrap(),
            "launched"
        );
        let old = publication.path.with_extension("old");
        std::fs::rename(&publication.path, &old).unwrap();
        assert!(lease(&claim, Some(&publication)).is_err());
        std::fs::write(&publication.path, b"foreign inode").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&publication.path, std::fs::Permissions::from_mode(0o600))
            .unwrap();
        assert!(lease(&claim, Some(&publication))
            .unwrap_err()
            .to_string()
            .contains("replaced"));
    }
}

#[cfg(windows)]
#[path = "stock_publication_windows.rs"]
mod windows;
#[cfg(windows)]
pub(super) use windows::*;
