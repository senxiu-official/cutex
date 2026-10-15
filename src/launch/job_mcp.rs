//! Dedicated Human-reviewed Job adapter. No general MCP configuration surface.
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[cfg(test)]
use super::stock::S6E_CLI_SHA256;
use super::stock::{StockBundle, VerifiedFile};

pub const JOB_SHA256: &str = "ba1a8d4f3e0b5f739e666e3f515d40b0c543e9e75953759ff181f1e71b29c521";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobMcpDescriptor {
    pub version: u32,
    pub adapter: VerifiedFile,
    pub launcher: VerifiedFile,
    pub endpoint: PathBuf,
    pub api_token_file: PathBuf,
    pub grant_key_file: PathBuf,
    /// Root Human's declaration of the daemon/launcher pairing. The endpoint
    /// peer must actually map the exact adapter bytes; this is not a model field.
    pub daemon_pid: u32,
    pub daemon_start_ticks: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateObject {
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    pub changed_seconds: i64,
    pub changed_nanos: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewedJobMcp {
    pub descriptor: JobMcpDescriptor,
    pub endpoint: PrivateObject,
    pub api_token: PrivateObject,
    pub grant_key: PrivateObject,
}

impl JobMcpDescriptor {
    /// A new runtime review binds the saved configuration to the currently
    /// listening daemon. Execution still validates that exact new occurrence.
    pub fn review_current(&self, bundle: &StockBundle) -> anyhow::Result<ReviewedJobMcp> {
        // The Job daemon's sandbox launcher is an independently installed tool.
        // Frontend-only releases must not replace its configured executable.
        let descriptor = self.current_occurrence()?;
        let reviewed = descriptor.review(bundle)?;
        descriptor.check_launcher_capability()?;
        Ok(reviewed)
    }

    #[cfg(unix)]
    fn check_launcher_capability(&self) -> anyhow::Result<()> {
        use std::io::{BufRead, Read, Write};
        let mut socket = std::os::unix::net::UnixStream::connect(&self.endpoint)?;
        let timeout = Some(std::time::Duration::from_secs(3));
        socket.set_read_timeout(timeout)?;
        socket.set_write_timeout(timeout)?;
        let token: String = std::fs::read(&self.api_token_file)?
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let request = serde_json::json!({"token":token,"method":"capabilities","params":{}});
        serde_json::to_writer(&mut socket, &request)?;
        socket.write_all(b"\n")?;
        let mut response = String::new();
        std::io::BufReader::new(socket.take(65537)).read_line(&mut response)?;
        ensure!(
            response.len() <= 65536,
            "Job capabilities response too large"
        );
        let response: serde_json::Value = serde_json::from_str(&response)?;
        ensure!(
            response["ok"] == true
                && response["result"]["allowedLaunchers"]
                    [self.launcher.path.to_string_lossy().as_ref()]
                .as_str()
                    == Some(self.launcher.sha256.as_str()),
            "Job daemon does not accept the selected native launcher; update its --allow-launcher configuration before launching this runtime"
        );
        Ok(())
    }

    #[cfg(not(unix))]
    fn check_launcher_capability(&self) -> anyhow::Result<()> {
        anyhow::bail!("Job launcher preflight currently supports Unix only")
    }

    #[cfg(target_os = "linux")]
    fn current_occurrence(&self) -> anyhow::Result<Self> {
        use std::os::fd::AsRawFd;
        let endpoint = private_object(&self.endpoint, true)?;
        let socket = std::os::unix::net::UnixStream::connect(&self.endpoint)
            .context("configured Job endpoint unavailable")?;
        let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of_val(&peer) as libc::socklen_t;
        ensure!(
            unsafe {
                libc::getsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut peer as *mut libc::ucred).cast(),
                    &mut len,
                )
            } == 0
                && peer.pid > 0
                && peer.uid == unsafe { libc::geteuid() },
            "configured Job peer identity unavailable"
        );
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", peer.pid))?;
        let mut current = self.clone();
        current.daemon_pid = peer.pid as u32;
        current.daemon_start_ticks = stat
            .rsplit_once(')')
            .context("invalid Job process identity")?
            .1
            .split_whitespace()
            .nth(19)
            .context("missing Job birth identity")?
            .parse()?;
        current.validate_peer()?;
        ensure!(
            private_object(&self.endpoint, true)? == endpoint,
            "Job endpoint changed during occurrence resolution"
        );
        Ok(current)
    }

    #[cfg(not(target_os = "linux"))]
    fn current_occurrence(&self) -> anyhow::Result<Self> {
        anyhow::bail!("reviewed Job MCP currently supports Linux only")
    }

    pub fn review(&self, bundle: &StockBundle) -> anyhow::Result<ReviewedJobMcp> {
        ensure!(self.version == 1, "unsupported Job MCP descriptor version");
        ensure!(
            bundle.soon_ingress(),
            "Job MCP requires reviewed coherent native bundle"
        );
        ensure!(
            bundle.version == 4 || self.adapter.sha256.as_str() == JOB_SHA256,
            "unsupported Job adapter bytes"
        );
        ensure!(
            bundle.version == 4 || bundle.cli.as_ref() == Some(&self.launcher),
            "Job launcher must equal reviewed native CLI"
        );
        self.adapter.validate()?;
        self.launcher.validate()?;
        ensure!(
            self.api_token_file != self.grant_key_file,
            "Job API and issuer credentials must be distinct"
        );
        let endpoint = private_object(&self.endpoint, true)?;
        let api_token = private_object(&self.api_token_file, false)?;
        let grant_key = private_object(&self.grant_key_file, false)?;
        ensure!(
            (api_token.device, api_token.inode) != (grant_key.device, grant_key.inode),
            "Job credential objects must be distinct"
        );
        self.validate_peer()?;
        ensure!(
            private_object(&self.endpoint, true)? == endpoint,
            "Job endpoint changed during review"
        );
        Ok(ReviewedJobMcp {
            descriptor: self.clone(),
            endpoint,
            api_token,
            grant_key,
        })
    }

    #[cfg(target_os = "linux")]
    fn validate_peer(&self) -> anyhow::Result<()> {
        use std::os::fd::AsRawFd;
        let socket = std::os::unix::net::UnixStream::connect(&self.endpoint)
            .context("reviewed Job endpoint unavailable")?;
        let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of_val(&peer) as libc::socklen_t;
        ensure!(
            unsafe {
                libc::getsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut peer as *mut libc::ucred).cast(),
                    &mut len,
                )
            } == 0,
            "Job peer identity unavailable"
        );
        ensure!(
            peer.uid == unsafe { libc::geteuid() }
                && peer.pid > 0
                && peer.pid as u32 == self.daemon_pid,
            "Job peer owner/occurrence mismatch"
        );
        let process = PathBuf::from(format!("/proc/{}", self.daemon_pid));
        let birth = || -> anyhow::Result<u64> {
            let stat = std::fs::read_to_string(process.join("stat"))?;
            Ok(stat
                .rsplit_once(')')
                .context("invalid Job process identity")?
                .1
                .split_whitespace()
                .nth(19)
                .context("missing Job birth identity")?
                .parse()?)
        };
        ensure!(
            birth()? == self.daemon_start_ticks,
            "Job daemon occurrence stale"
        );
        ensure!(
            std::fs::read_link(process.join("exe"))? == self.adapter.path,
            "Job daemon executable path mismatch"
        );
        ensure!(
            crate::agent_management::file_sha256(&process.join("exe"))? == self.adapter.sha256,
            "Job daemon mapped bytes mismatch"
        );
        ensure!(
            birth()? == self.daemon_start_ticks,
            "Job daemon changed during review"
        );
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn validate_peer(&self) -> anyhow::Result<()> {
        anyhow::bail!("reviewed Job MCP currently supports Linux only")
    }
}

impl ReviewedJobMcp {
    pub fn validate(&self, bundle: &StockBundle) -> anyhow::Result<()> {
        ensure!(
            &self.descriptor.review(bundle)? == self,
            "Job MCP custody changed; fresh Human review required"
        );
        Ok(())
    }

    pub fn config(&self) -> anyhow::Result<serde_json::Value> {
        let d = &self.descriptor;
        Ok(serde_json::json!({
            "command": d.adapter.path,
            "args": ["mcp-stdio", d.endpoint.to_str().context("Job endpoint UTF-8 required")?, d.api_token_file.to_str().context("Job credential path UTF-8 required")?, d.grant_key_file.to_str().context("Job credential path UTF-8 required")?, d.launcher.path.to_str().context("Job launcher UTF-8 required")?],
            "env_vars": ["CUTEX_AGENT_ID", "CUTEX_AGENT_BUS_URL", "CUTEX_AGENT_BUS_TOKEN"]
        }))
    }
}

#[cfg(target_os = "linux")]
fn private_object(path: &Path, socket: bool) -> anyhow::Result<PrivateObject> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
    ensure!(
        path.is_absolute() && path.to_str().is_some() && path.canonicalize()? == path,
        "canonical UTF-8 Job custody path required"
    );
    let parent = path.parent().context("Job path parent missing")?;
    let directory = std::fs::symlink_metadata(parent)?;
    ensure!(
        directory.is_dir()
            && directory.uid() == unsafe { libc::geteuid() }
            && directory.mode() & 0o077 == 0,
        "Job custody directory must be private and owned"
    );
    let before = std::fs::symlink_metadata(path)?;
    let held = if socket {
        None
    } else {
        Some(
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path)?,
        )
    };
    let metadata = match &held {
        Some(file) => file.metadata()?,
        None => before.clone(),
    };
    ensure!(
        (before.dev(), before.ino()) == (metadata.dev(), metadata.ino()),
        "Job custody object replaced while opening"
    );
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
        "Job custody object must be private and owned"
    );
    ensure!(
        if socket {
            metadata.file_type().is_socket()
        } else {
            metadata.is_file()
                && metadata.nlink() == 1
                && metadata.len() > 0
                && metadata.len() <= 4096
        },
        "invalid Job custody object"
    );
    let identity = |m: &std::fs::Metadata| PrivateObject {
        device: m.dev(),
        inode: m.ino(),
        size: m.len(),
        changed_seconds: m.ctime(),
        changed_nanos: m.ctime_nsec(),
    };
    ensure!(
        identity(&std::fs::symlink_metadata(path)?) == identity(&metadata),
        "Job custody object changed during observation"
    );
    Ok(identity(&metadata))
}

#[cfg(not(target_os = "linux"))]
fn private_object(_: &Path, _: bool) -> anyhow::Result<PrivateObject> {
    anyhow::bail!("reviewed Job MCP currently supports Linux only")
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn launcher_preflight_uses_wire_credentials_and_exact_accepted_digest() {
        use std::io::{BufRead, Write};
        let root = std::env::temp_dir().join(format!("job-preflight-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let endpoint = root.join("socket");
        let listener = std::os::unix::net::UnixListener::bind(&endpoint).unwrap();
        let api_token_file = root.join("token");
        std::fs::write(&api_token_file, [0, 255, 10]).unwrap();
        let file = VerifiedFile {
            path: "/fixture/codex".into(),
            sha256: crate::role_revision::Sha256::new("a".repeat(64)).unwrap(),
        };
        let descriptor = JobMcpDescriptor {
            version: 1,
            adapter: file.clone(),
            launcher: file,
            endpoint,
            api_token_file,
            grant_key_file: root.join("grant"),
            daemon_pid: 1,
            daemon_start_ticks: 0,
        };
        let server = std::thread::spawn(move || {
            for digest in ["a".repeat(64), "b".repeat(64)] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                std::io::BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(
                    request,
                    serde_json::json!({"token":"00ff0a","method":"capabilities","params":{}})
                );
                writeln!(stream, "{}", serde_json::json!({"ok":true,"result":{"allowedLaunchers":{"/fixture/codex":digest}}})).unwrap();
            }
        });
        descriptor.check_launcher_capability().unwrap();
        assert!(
            descriptor
                .check_launcher_capability()
                .unwrap_err()
                .to_string()
                .contains("--allow-launcher")
        );
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn job_mcp_custody_rejects_replacement_symlink_and_public_files() {
        let root = std::env::temp_dir().join(format!("job-custody-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join("credential");
        std::fs::write(&path, b"private-test-not-a-real-key").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let first = private_object(&path, false).unwrap();
        assert_eq!(first, private_object(&path, false).unwrap());
        std::fs::rename(&path, root.join("retained")).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_ne!(first, private_object(&path, false).unwrap());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(private_object(&path, false).is_err());
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(root.join("retained"), &path).unwrap();
        assert!(private_object(&path, false).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn job_mcp_real_socket_peer_fences_pid_birth_and_executable() {
        let root = std::env::temp_dir().join(format!("job-peer-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let endpoint = root.join("job.sock");
        let listener = std::os::unix::net::UnixListener::bind(&endpoint).unwrap();
        std::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o600)).unwrap();
        let exe = std::fs::read_link("/proc/self/exe").unwrap();
        let file = VerifiedFile {
            path: exe.clone(),
            sha256: crate::agent_management::file_sha256(&exe).unwrap(),
        };
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
        let birth = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .nth(19)
            .unwrap()
            .parse()
            .unwrap();
        let mut d = JobMcpDescriptor {
            version: 1,
            adapter: file.clone(),
            launcher: file,
            endpoint,
            api_token_file: root.join("api"),
            grant_key_file: root.join("grant"),
            daemon_pid: std::process::id(),
            daemon_start_ticks: birth,
        };
        d.validate_peer().unwrap();
        d.daemon_start_ticks += 1;
        assert!(d.validate_peer().is_err());
        d.daemon_start_ticks = birth;
        d.daemon_pid += 1;
        assert!(d.validate_peer().is_err());
        // A new review may rediscover a restarted daemon, but an already-issued
        // descriptor remains stale and must still fail execution validation.
        let refreshed = d.current_occurrence().unwrap();
        assert_eq!(refreshed.daemon_pid, std::process::id());
        assert_eq!(refreshed.daemon_start_ticks, birth);
        assert_eq!(refreshed.adapter, d.adapter);
        assert!(d.validate_peer().is_err());
        refreshed.validate_peer().unwrap();
        let mut changed = d.clone();
        changed.adapter.sha256 = crate::role_revision::Sha256::new("0".repeat(64)).unwrap();
        assert!(
            changed
                .current_occurrence()
                .unwrap_err()
                .to_string()
                .contains("bytes mismatch")
        );
        changed.adapter = d.adapter.clone();
        changed.adapter.path = root.join("unexpected-executable");
        assert!(
            changed
                .current_occurrence()
                .unwrap_err()
                .to_string()
                .contains("path mismatch")
        );
        drop(listener);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn job_mcp_schema_forbids_raw_config_and_authority_fields() {
        let raw = serde_json::json!({"version":1,"adapter":{"path":"/private/job","sha256":JOB_SHA256},"launcher":{"path":"/private/codex","sha256":S6E_CLI_SHA256},"endpoint":"/private/job.sock","api_token_file":"/private/api","grant_key_file":"/private/grant","daemon_pid":123,"daemon_start_ticks":1});
        let d: JobMcpDescriptor = serde_json::from_value(raw.clone()).unwrap();
        for field in [
            "env",
            "env_vars",
            "args",
            "cwd",
            "principal",
            "token",
            "raw_config",
            "default_tools_approval_mode",
        ] {
            let mut bad = raw.clone();
            bad[field] = serde_json::json!("forged");
            assert!(serde_json::from_value::<JobMcpDescriptor>(bad).is_err());
        }
        let identity = PrivateObject {
            device: 1,
            inode: 2,
            size: 3,
            changed_seconds: 4,
            changed_nanos: 5,
        };
        let reviewed = ReviewedJobMcp {
            descriptor: d,
            endpoint: identity.clone(),
            api_token: identity.clone(),
            grant_key: identity,
        };
        let c = reviewed.config().unwrap();
        assert_eq!(c["args"].as_array().unwrap().len(), 5);
        assert_eq!(c["env_vars"].as_array().unwrap().len(), 3);
        assert!(c.get("default_tools_approval_mode").is_none());
        assert!(c.get("cwd").is_none());
    }

    #[test]
    fn job_mcp_default_none_and_native_mcp_config_is_allowed() {
        let request: crate::agent_management::ExplicitLaunchRequest = serde_json::from_value(serde_json::json!({"operation":"review_runtime","cutex_session_id":"cutex.private-test","restart":false})).unwrap();
        assert!(matches!(
            request,
            crate::agent_management::ExplicitLaunchRequest::ReviewRuntime { job_mcp: None, .. }
        ));
        assert!(
            super::super::stock::validate_shared_config("[mcp_servers.cutex_job]\ncommand='job'")
                .is_ok()
        );
    }

    #[test]
    fn job_mcp_pin_refusals_precede_filesystem_or_peer_access() {
        use super::super::stock::*;
        let file = |hash: &str| VerifiedFile {
            path: "/absent-private-test-artifact".into(),
            sha256: crate::role_revision::Sha256::new(hash.to_owned()).unwrap(),
        };
        let bundle = StockBundle {
            version: 3,
            upstream_commit: STOCK_COMMIT.into(),
            native_patch_commit: Some(S6E_COMMIT.into()),
            executable: file(S6E_EXECUTABLE_SHA256),
            cli: Some(file(S6E_CLI_SHA256)),
            code_mode_host: file(STOCK_HOST_SHA256),
            facade: file(STOCK_HOST_SHA256),
            schema: file(S6E_SCHEMA_SHA256),
            launch_config_sha256: None,
            shared_config: file(STOCK_HOST_SHA256),
        };
        let descriptor = JobMcpDescriptor {
            version: 1,
            adapter: file(JOB_SHA256),
            launcher: bundle.cli.clone().unwrap(),
            endpoint: "/absent-private-test-socket".into(),
            api_token_file: "/absent-private-test-api".into(),
            grant_key_file: "/absent-private-test-grant".into(),
            daemon_pid: 1,
            daemon_start_ticks: 0,
        };
        let mut bad = descriptor.clone();
        // Correct current bytes pass eligibility and reach file validation;
        // this is not a claim that the deliberately absent artifact is valid.
        let error = descriptor.review(&bundle).unwrap_err().to_string();
        assert!(!error.contains("adapter bytes"), "{error}");
        bad.adapter.sha256 =
            file("d98d5e33322c7200eb9b149bc39d99da3bb169c994fe14c7f401d26c06d7b1f2").sha256;
        assert!(
            bad.review(&bundle)
                .unwrap_err()
                .to_string()
                .contains("adapter bytes")
        );
        bad = descriptor.clone();
        bad.version = 2;
        assert!(
            bad.review(&bundle)
                .unwrap_err()
                .to_string()
                .contains("descriptor version")
        );
        bad = descriptor.clone();
        bad.adapter.sha256 = file(STOCK_HOST_SHA256).sha256;
        assert!(
            bad.review(&bundle)
                .unwrap_err()
                .to_string()
                .contains("adapter bytes")
        );
        bad = descriptor.clone();
        bad.launcher.sha256 = file(STOCK_HOST_SHA256).sha256;
        assert!(
            bad.review(&bundle)
                .unwrap_err()
                .to_string()
                .contains("equal reviewed native CLI")
        );
        bad = descriptor.clone();
        bad.launcher.path = "/another-private-launcher".into();
        assert!(
            bad.review(&bundle)
                .unwrap_err()
                .to_string()
                .contains("equal reviewed native CLI")
        );
        let mut frontend_update = bundle.clone();
        frontend_update.version = 4;
        frontend_update.cli.as_mut().unwrap().path = "/new/frontend-only-release".into();
        let error = descriptor.review(&frontend_update).unwrap_err().to_string();
        assert!(!error.contains("equal reviewed native CLI"), "{error}");
        assert!(!error.is_empty()); // Still validates the independently pinned files.
        let mut legacy = bundle;
        legacy.version = 1;
        assert!(
            descriptor
                .review(&legacy)
                .unwrap_err()
                .to_string()
                .contains("coherent native bundle")
        );
    }

    #[test]
    #[ignore = "requires the exact external accepted Job build manifest"]
    fn job_manifest_matches_compiled_eligibility() {
        let path = std::env::var("CUTEX_TEST_JOB_BUILD_MANIFEST").unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(
            manifest["source"]["commit"],
            "f7bbe3c42fbf30bfb5fe379c6f6cd5af951991b9"
        );
        let artifacts = manifest["artifacts"].as_array().unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0]["path"], "linux/cutex-job-service");
        assert_eq!(artifacts[0]["sha256"], JOB_SHA256);
    }
}
