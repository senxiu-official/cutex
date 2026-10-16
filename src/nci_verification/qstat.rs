use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MAX_OUTPUT: usize = 256 * 1024;
const MAX_STDERR: usize = 16 * 1024;

/// At most this many pinned host keys (e.g. one per Gadi login node, or per key type).
pub(crate) const MAX_HOST_KEYS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Config {
    pub username: String,
    /// Every key is pinned for `gadi.nci.org.au`; any one of them may answer.
    pub host_keys: Vec<String>,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_seconds: u64,
}

fn default_poll_interval() -> u64 {
    21_600
}

fn validate_host_key(host_key: &str) -> Result<&str, &'static str> {
    let (algorithm, encoded) = host_key.split_once(' ').ok_or("qstat_invalid_host_key")?;
    if !matches!(
        algorithm,
        "ssh-ed25519"
            | "ssh-rsa"
            | "ecdsa-sha2-nistp256"
            | "ecdsa-sha2-nistp384"
            | "ecdsa-sha2-nistp521"
    ) || encoded.len() > 16_384
        || encoded.bytes().any(|b| b.is_ascii_whitespace())
    {
        return Err("qstat_invalid_host_key");
    }
    let wire = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| "qstat_invalid_host_key")?;
    let length = wire.get(..4).ok_or("qstat_invalid_host_key")?;
    let length =
        u32::from_be_bytes(length.try_into().map_err(|_| "qstat_invalid_host_key")?) as usize;
    if wire.get(4..4usize.saturating_add(length)) != Some(algorithm.as_bytes())
        || wire.len() <= 4 + length
    {
        return Err("qstat_invalid_host_key");
    }
    Ok(algorithm)
}

impl Config {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        let owner = self.username.as_bytes();
        if owner.is_empty()
            || owner.len() > 64
            || !owner[0].is_ascii_alphanumeric()
            || !owner
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(b))
            || !(1800..=604800).contains(&self.poll_interval_seconds)
        {
            return Err("qstat_invalid_config");
        }
        if self.host_keys.is_empty() || self.host_keys.len() > MAX_HOST_KEYS {
            return Err("qstat_invalid_host_key");
        }
        for (i, key) in self.host_keys.iter().enumerate() {
            validate_host_key(key)?;
            if self.host_keys[..i].contains(key) {
                return Err("qstat_invalid_host_key");
            }
        }
        Ok(())
    }

    /// Signature algorithms limited to the pinned key types, so ssh cannot
    /// negotiate a host key type the owner never confirmed.
    fn host_key_algorithms(&self) -> Result<String, &'static str> {
        let mut algorithms: Vec<&str> = Vec::new();
        for key in &self.host_keys {
            let names: &[&str] = match validate_host_key(key)? {
                // RSA keys sign with SHA-2; the SHA-1 `ssh-rsa` signature stays disabled.
                "ssh-rsa" => &["rsa-sha2-512", "rsa-sha2-256"],
                "ssh-ed25519" => &["ssh-ed25519"],
                "ecdsa-sha2-nistp256" => &["ecdsa-sha2-nistp256"],
                "ecdsa-sha2-nistp384" => &["ecdsa-sha2-nistp384"],
                "ecdsa-sha2-nistp521" => &["ecdsa-sha2-nistp521"],
                _ => return Err("qstat_invalid_host_key"),
            };
            for name in names {
                if !algorithms.contains(name) {
                    algorithms.push(name);
                }
            }
        }
        Ok(algorithms.join(","))
    }

    pub(crate) fn known_hosts(&self) -> String {
        self.host_keys
            .iter()
            .map(|key| format!("gadi.nci.org.au {key}\n"))
            .collect()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Target {
    pub remote_id: String,
    pub correlation_token: String,
    pub remote_name: Option<String>,
    pub project: Option<String>,
    #[serde(default)]
    pub current_state: String,
}

impl Target {
    fn validate(&self) -> Result<(), &'static str> {
        let digits = self
            .remote_id
            .strip_suffix(".gadi-pbs")
            .ok_or("qstat_invalid_target")?;
        if digits.is_empty()
            || digits.len() > 20
            || !digits.bytes().all(|b| b.is_ascii_digit())
            || self.correlation_token.len() != 32
            || !self
                .correlation_token
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
        {
            return Err("qstat_invalid_target");
        }
        Ok(())
    }
}

fn command(
    config: &Config,
    target: &Target,
    key_path: &Path,
    known_hosts_path: &Path,
) -> Result<Command, &'static str> {
    config.validate()?;
    target.validate()?;
    let known_hosts = safe_ssh_path(known_hosts_path)?;
    let key = safe_ssh_path(key_path)?;
    let mut cmd = Command::new(ssh_program());
    cmd.args(["-F", "/dev/null", "-p", "22", "-l", &config.username])
        .arg("-i")
        .arg(key);
    for option in [
        "BatchMode=yes",
        "StrictHostKeyChecking=yes",
        "ConnectTimeout=10",
        "ConnectionAttempts=1",
        "ClearAllForwardings=yes",
        "ForwardAgent=no",
        "ForwardX11=no",
        "RequestTTY=no",
        "IdentitiesOnly=yes",
        "IdentityAgent=none",
        "GlobalKnownHostsFile=/dev/null",
        "PasswordAuthentication=no",
        "KbdInteractiveAuthentication=no",
        "NumberOfPasswordPrompts=0",
        "ControlMaster=no",
        "ControlPath=none",
        "ProxyCommand=none",
        "ProxyJump=none",
        "CanonicalizeHostname=no",
        "KnownHostsCommand=none",
        "VerifyHostKeyDNS=no",
        "UpdateHostKeys=no",
        "PermitLocalCommand=no",
    ] {
        cmd.args(["-o", option]);
    }
    cmd.arg("-o").arg(format!(
        "HostKeyAlgorithms={}",
        config.host_key_algorithms()?
    ));
    cmd.arg("-o")
        .arg(format!("UserKnownHostsFile=\"{known_hosts}\""));
    cmd.arg("--")
        .arg("gadi.nci.org.au")
        .args(["/opt/pbs/default/bin/qstat", "-xf", "-F", "json"])
        .arg(&target.remote_id);
    // No inherited agent, askpass, display, proxy or locale settings reach ssh.
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C");
    Ok(cmd)
}

/// The system OpenSSH client. Only a debug build running in a marked private test home
/// (`CUTEX_TEST_PRIVATE_HOME`) may substitute an absolute stub path from `CUTEX_TEST_NCI_SSH`,
/// so the fake-Gadi end-to-end test (scripts/nci-fake-gadi-e2e.py) never opens a network
/// connection; release builds ignore both variables.
fn ssh_program() -> std::ffi::OsString {
    #[cfg(debug_assertions)]
    if std::env::var_os("CUTEX_TEST_PRIVATE_HOME").is_some() {
        if let Some(stub) = std::env::var_os("CUTEX_TEST_NCI_SSH").filter(|p| Path::new(p).is_absolute()) {
            return stub;
        }
    }
    "/usr/bin/ssh".into()
}

/// A failed query as a code the owner can act on. stderr is only classified, never stored
/// or shown (it may contain Gadi's banner or paths).
fn failure_code(status: Option<i32>, stderr: &[u8]) -> &'static str {
    let text = String::from_utf8_lossy(stderr);
    let has = |needle: &str| text.contains(needle);
    match status {
        // ssh's own failure (the remote command never ran).
        Some(255) => {
            if has("Host key verification failed")
                || has("REMOTE HOST IDENTIFICATION HAS CHANGED")
                || has("no matching host key type")
            {
                "qstat_host_key_mismatch"
            } else if has("Permission denied") {
                "qstat_ssh_auth_failed"
            } else if has("Could not resolve hostname")
                || has("Connection timed out")
                || has("Connection refused")
                || has("Network is unreachable")
                || has("No route to host")
                || has("Operation timed out")
                || has("Connection reset")
                || has("Connection closed")
            {
                "qstat_ssh_connect_failed"
            } else {
                "qstat_ssh_failed"
            }
        }
        // The forced-command helper refused the command line.
        _ if has("Only the fixed qstat query is permitted") => "qstat_helper_refused",
        // `command="/usr/bin/python3 -I <path>"` names a file that is not there.
        _ if has("can't open file") => "qstat_helper_missing",
        // PBS has no record of the job (wrong id, or finished longer ago than the history keeps).
        _ if has("Unknown Job Id") => "qstat_job_unknown",
        _ => "qstat_remote_failed",
    }
}

/// ssh expands `%` tokens in `-i` and `UserKnownHostsFile`; such paths and
/// characters that would break the quoted option value are refused.
fn safe_ssh_path(path: &Path) -> Result<&str, &'static str> {
    let text = path.to_str().ok_or("qstat_invalid_path")?;
    if !path.is_absolute()
        || text.contains(['%', '\n', '\r', '\0', '"', '\\'])
        || text.chars().any(char::is_control)
    {
        return Err("qstat_invalid_path");
    }
    Ok(text)
}

pub(crate) fn query(
    config: &Config,
    target: &Target,
    key_path: &Path,
    known_hosts_path: &Path,
) -> Result<Option<Value>, &'static str> {
    let output = run(
        command(config, target, key_path, known_hosts_path)?,
        Duration::from_secs(30),
    )?;
    parse(config, target, &output)
}

fn nonblocking(fd: i32) -> Result<(), &'static str> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err("qstat_io");
    }
    Ok(())
}

fn drain(reader: &mut impl Read, output: &mut Vec<u8>, limit: usize) -> Result<bool, &'static str> {
    let mut buf = [0; 8192];
    // Bound each polling pass even if the peer continuously writes.
    for _ in 0..32 {
        match reader.read(&mut buf) {
            Ok(0) => return Ok(true),
            Ok(n) => {
                if output.len() + n > limit {
                    return Err("qstat_output_limit");
                }
                output.extend_from_slice(&buf[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err("qstat_io"),
        }
    }
    Ok(false)
}

/// Exit status of a child that has exited (`Some(None)` = killed by a signal), observed
/// without reaping it, so its pid (and the group id it leads) cannot be reused before the
/// group kill.
fn exited_unreaped(pid: i32) -> Result<Option<Option<i32>>, &'static str> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if rc < 0 {
        return Err("qstat_io");
    }
    if unsafe { info.si_pid() } == 0 {
        return Ok(None);
    }
    Ok(Some(
        (info.si_code == libc::CLD_EXITED).then(|| unsafe { info.si_status() }),
    ))
}

fn run(mut command: Command, timeout: Duration) -> Result<Vec<u8>, &'static str> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let start = Instant::now();
    let mut child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "qstat_ssh_missing"
        } else {
            "qstat_spawn"
        }
    })?;
    let pid = child.id() as i32;
    let result = (|| {
        let mut stdout = child.stdout.take().ok_or("qstat_io")?;
        let mut stderr = child.stderr.take().ok_or("qstat_io")?;
        nonblocking(stdout.as_raw_fd())?;
        nonblocking(stderr.as_raw_fd())?;
        let mut output = Vec::new();
        let mut errors = Vec::new();
        let mut failed_at: Option<Instant> = None;
        loop {
            if start.elapsed() >= timeout {
                return Err("qstat_timeout");
            }
            let out_done = drain(&mut stdout, &mut output, MAX_OUTPUT)?;
            let err_done = drain(&mut stderr, &mut errors, MAX_STDERR)?;
            if let Some(status) = exited_unreaped(pid)? {
                if status != Some(0) {
                    // Read the rest of stderr to classify the failure, but never wait on a
                    // descendant that keeps the pipes open.
                    let since = *failed_at.get_or_insert_with(Instant::now);
                    if (out_done && err_done) || since.elapsed() >= Duration::from_secs(1) {
                        return Err(failure_code(status, &errors));
                    }
                } else if out_done && err_done {
                    return Ok(output);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    // The child is not reaped yet (only observed with WNOWAIT), so `pid` still
    // names its group: kill every descendant that may hold a pipe open, then reap.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    let _ = child.wait();
    result
}

/// `Ok(None)` is a confirmed nonfinal observation with no state to deliver.
fn parse(config: &Config, target: &Target, raw: &[u8]) -> Result<Option<Value>, &'static str> {
    config.validate()?;
    target.validate()?;
    if raw.len() > MAX_OUTPUT {
        return Err("qstat_output_limit");
    }
    let data: Value = serde_json::from_slice(raw).map_err(|_| "qstat_invalid_response")?;
    let jobs = data
        .get("Jobs")
        .and_then(Value::as_object)
        .ok_or("qstat_invalid_response")?;
    if jobs.len() != 1 {
        return Err("qstat_job_mismatch");
    }
    let job = jobs
        .get(&target.remote_id)
        .and_then(Value::as_object)
        .ok_or("qstat_job_mismatch")?;
    let owner = job
        .get("Job_Owner")
        .and_then(Value::as_str)
        .ok_or("qstat_owner_mismatch")?;
    let (username, host) = owner.split_once('@').ok_or("qstat_owner_mismatch")?;
    if username != config.username || host.is_empty() || host.contains('@') {
        return Err("qstat_owner_mismatch");
    }
    let name = job
        .get("Job_Name")
        .and_then(Value::as_str)
        .ok_or("qstat_name_mismatch")?;
    if !name.ends_with(&format!("cx{}", target.correlation_token))
        || target
            .remote_name
            .as_deref()
            .is_some_and(|expected| expected != name)
    {
        return Err("qstat_name_mismatch");
    }
    let project = job
        .get("project")
        .map(|value| value.as_str().ok_or("qstat_project_mismatch"))
        .transpose()?;
    let account = job
        .get("Account_Name")
        .map(|value| value.as_str().ok_or("qstat_project_mismatch"))
        .transpose()?;
    if matches!((project, account), (Some(project), Some(account)) if project != account)
        || target
            .project
            .as_deref()
            .is_some_and(|expected| project.or(account) != Some(expected))
    {
        return Err("qstat_project_mismatch");
    }
    let state = job
        .get("job_state")
        .and_then(Value::as_str)
        .ok_or("qstat_unknown_state")?;
    let mut event = json!({"source":"nci", "remoteId":target.remote_id,
        "correlationToken":target.correlation_token});
    match state {
        // E (exiting) is still running: the end is not yet recorded.
        "R" | "E" => event["state"] = json!("running"),
        // Waiting to run (held, waiting for its start time, in transit).
        "Q" | "H" | "W" | "T" => event["state"] = json!("queued"),
        "F" => match job.get("Exit_status") {
            // PBS records no exit status for a job deleted before it ran
            // (owner/admin qdel while queued or held).
            None => {
                event["exitCode"] = Value::Null;
                event["state"] = json!("cancelled");
                event["reason"] = json!("cancelled");
            }
            Some(value) => {
                let exit = value
                    .as_i64()
                    .and_then(|v| i32::try_from(v).ok())
                    .ok_or("qstat_invalid_exit_status")?;
                event["exitCode"] = if exit < 0 { Value::Null } else { json!(exit) };
                event["state"] = json!(if exit == 0 { "exited" } else { "failed" });
                if exit != 0 {
                    event["reason"] = json!(match exit {
                        // PBS JOB_EXEC_KILL_WALLTIME.
                        -29 => "walltime_exceeded",
                        // Other PBS-internal (negative) codes stay a generic failure.
                        exit if exit < 0 => "aborted",
                        // Includes 271 (256 + SIGTERM), observed for an owner qdel of a
                        // running job but not specific to it: not mapped to cancelled.
                        _ => "nonzero_exit",
                    });
                }
            }
        },
        // Other single-letter PBS states (suspended S/U, moved M, array B/X, ...)
        // are not final: an ordinary observation with nothing to report.
        other if other.len() == 1 && other.bytes().all(|b| b.is_ascii_uppercase()) => {
            return Ok(None)
        }
        _ => return Err("qstat_unknown_state"),
    }
    // Hash only authoritative event facts, not changing qstat metadata or email content.
    event["eventId"] = json!(format!(
        "qstat:{:x}",
        Sha256::digest(serde_json::to_vec(&event).map_err(|_| "qstat_invalid_response")?)
    ));
    Ok(Some(event))
}

#[cfg(test)]
mod tests {
    use super::*;
    const TOKEN: &str = "0123456789abcdef0123456789abcdef";
    fn key(algorithm: &str, fill: u8) -> String {
        let mut wire = (algorithm.len() as u32).to_be_bytes().to_vec();
        wire.extend_from_slice(algorithm.as_bytes());
        wire.extend_from_slice(&[0, 0, 0, 32]);
        wire.extend_from_slice(&[fill; 32]);
        format!(
            "{algorithm} {}",
            base64::engine::general_purpose::STANDARD.encode(wire)
        )
    }
    fn config() -> Config {
        Config {
            username: "owner".into(),
            host_keys: vec![key("ssh-ed25519", 0)],
            poll_interval_seconds: 21_600,
        }
    }
    fn target() -> Target {
        Target {
            remote_id: "123.gadi-pbs".into(),
            correlation_token: TOKEN.into(),
            remote_name: None,
            project: Some("zz00".into()),
            current_state: String::new(),
        }
    }
    fn response(state: &str) -> Value {
        json!({"Jobs":{"123.gadi-pbs":{"Job_Owner":"owner@gadi", "Job_Name":format!("cx{TOKEN}"), "Account_Name":"zz00", "job_state":state, "Exit_status":0}}})
    }
    fn parse_value(v: Value) -> Result<Value, &'static str> {
        parse(&config(), &target(), &serde_json::to_vec(&v).unwrap())
            .map(|event| event.expect("reportable state"))
    }

    #[test]
    fn fixed_command_and_injection_rejection() {
        let cmd = command(
            &config(),
            &target(),
            Path::new("/private/key"),
            Path::new("/private/known_hosts"),
        )
        .unwrap();
        assert_eq!(cmd.get_program(), "/usr/bin/ssh");
        let args: Vec<_> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
        assert!(args.contains(&"StrictHostKeyChecking=yes"));
        assert!(args.contains(&"BatchMode=yes"));
        assert_eq!(
            &args[args.len() - 7..],
            &[
                "--",
                "gadi.nci.org.au",
                "/opt/pbs/default/bin/qstat",
                "-xf",
                "-F",
                "json",
                "123.gadi-pbs"
            ]
        );
        assert!(args.contains(&"IdentityAgent=none"));
        assert!(args.contains(&"IdentitiesOnly=yes"));
        assert!(args.contains(&"UserKnownHostsFile=\"/private/known_hosts\""));
        assert_eq!(
            &args[..8],
            &[
                "-F",
                "/dev/null",
                "-p",
                "22",
                "-l",
                "owner",
                "-i",
                "/private/key"
            ]
        );
        for username in ["-evil", "a;b", "owner@gadi", "a b", ""] {
            let mut c = config();
            c.username = username.into();
            assert!(c.validate().is_err());
        }
        for host_key in [
            "ssh-ed25519 AAAA",
            "ssh-ed25519 a\nb",
            "ssh-rsa invalid",
            "bad base64",
        ] {
            let mut c = config();
            c.host_keys = vec![host_key.into()];
            assert!(c.validate().is_err());
        }
        let mut c = config();
        c.host_keys = Vec::new();
        assert!(c.validate().is_err());
        c.host_keys = vec![key("ssh-ed25519", 0), key("ssh-ed25519", 0)];
        assert!(c.validate().is_err(), "duplicate pinned key");
        c.host_keys = (0..=MAX_HOST_KEYS as u8)
            .map(|i| key("ssh-ed25519", i))
            .collect();
        assert!(c.validate().is_err(), "too many pinned keys");
        let mut c = config();
        c.poll_interval_seconds = 1799;
        assert!(c.validate().is_err());
        let mut v = serde_json::to_value(config()).unwrap();
        v.as_object_mut().unwrap().remove("pollIntervalSeconds");
        assert_eq!(
            serde_json::from_value::<Config>(v)
                .unwrap()
                .poll_interval_seconds,
            21_600
        );

        let mut t = target();
        t.remote_id = "123;true.gadi-pbs".into();
        assert!(command(
            &config(),
            &t,
            Path::new("/private/key"),
            Path::new("/private/known_hosts")
        )
        .is_err());
    }

    #[test]
    fn states_and_stable_events() {
        for (state, expected) in [
            ("R", "running"),
            ("E", "running"),
            ("Q", "queued"),
            ("H", "queued"),
            ("W", "queued"),
            ("T", "queued"),
            ("F", "exited"),
        ] {
            assert_eq!(parse_value(response(state)).unwrap()["state"], expected);
        }
        let mut v = response("F");
        let first = parse_value(v.clone()).unwrap();
        assert!(first.get("resources").is_none());
        v["timestamp"] = json!(99);
        v["Jobs"]["123.gadi-pbs"]["comment"] = json!("irrelevant");
        assert_eq!(first, parse_value(v).unwrap());
    }

    #[test]
    fn nonfinal_states_are_observations_not_errors() {
        for state in ["S", "U", "M", "B", "X"] {
            let raw = serde_json::to_vec(&response(state)).unwrap();
            assert_eq!(parse(&config(), &target(), &raw), Ok(None), "{state}");
        }
        for state in ["", "RR", "r", "?"] {
            assert_eq!(
                parse_value(response(state)),
                Err("qstat_unknown_state"),
                "{state:?}"
            );
        }
    }

    #[test]
    fn finished_jobs_settle_with_typed_reasons() {
        // (Exit_status, state, reason, exitCode)
        for (exit, state, reason, code) in [
            (json!(0), "exited", Value::Null, json!(0)),
            (json!(1), "failed", json!("nonzero_exit"), json!(1)),
            (json!(271), "failed", json!("nonzero_exit"), json!(271)),
            (
                json!(-29),
                "failed",
                json!("walltime_exceeded"),
                Value::Null,
            ),
            (json!(-9), "failed", json!("aborted"), Value::Null),
            (json!(-1), "failed", json!("aborted"), Value::Null),
        ] {
            let mut v = response("F");
            v["Jobs"]["123.gadi-pbs"]["Exit_status"] = exit.clone();
            let parsed = parse_value(v).unwrap();
            assert_eq!(parsed["state"], state, "{exit}");
            assert_eq!(parsed.get("reason").cloned().unwrap_or(Value::Null), reason);
            assert_eq!(parsed["exitCode"], code, "{exit}");
        }
        // Deleted while queued: finished without an exit status.
        let mut v = response("F");
        v["Jobs"]["123.gadi-pbs"]
            .as_object_mut()
            .unwrap()
            .remove("Exit_status");
        let parsed = parse_value(v).unwrap();
        assert_eq!(parsed["state"], "cancelled");
        assert_eq!(parsed["reason"], "cancelled");
        assert_eq!(parsed["exitCode"], Value::Null);
        for bad in [Value::Null, json!("0"), json!(1.5), json!(1u64 << 40)] {
            let mut v = response("F");
            v["Jobs"]["123.gadi-pbs"]["Exit_status"] = bad;
            assert_eq!(parse_value(v), Err("qstat_invalid_exit_status"));
        }
    }

    #[test]
    fn ssh_environment_host_key_types_and_token_paths() {
        let cmd = command(
            &config(),
            &target(),
            Path::new("/private/key"),
            Path::new("/private/known_hosts"),
        )
        .unwrap();
        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(envs.contains(&(
            std::ffi::OsStr::new("PATH"),
            Some(std::ffi::OsStr::new("/usr/bin:/bin"))
        )));
        assert!(envs.contains(&(
            std::ffi::OsStr::new("LANG"),
            Some(std::ffi::OsStr::new("C"))
        )));
        assert_eq!(envs.len(), 2, "environment is cleared: {envs:?}");
        let args: Vec<_> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
        assert!(args.contains(&"HostKeyAlgorithms=ssh-ed25519"));

        let mut c = config();
        c.host_keys = vec![
            key("ssh-rsa", 1),
            key("ecdsa-sha2-nistp256", 2),
            key("ssh-rsa", 3),
        ];
        c.validate().unwrap();
        assert_eq!(
            c.host_key_algorithms().unwrap(),
            "rsa-sha2-512,rsa-sha2-256,ecdsa-sha2-nistp256"
        );
        assert_eq!(c.known_hosts().lines().count(), 3);
        assert!(c
            .known_hosts()
            .lines()
            .all(|line| line.starts_with("gadi.nci.org.au ")));

        for (key_path, hosts_path) in [
            ("/private/%d/key", "/private/known_hosts"),
            ("/private/key", "/private/%h_known_hosts"),
            ("relative/key", "/private/known_hosts"),
            ("/private/key", "/private/known\"hosts"),
            ("/private/k\x07ey", "/private/known_hosts"),
        ] {
            assert_eq!(
                command(
                    &config(),
                    &target(),
                    Path::new(key_path),
                    Path::new(hosts_path)
                )
                .unwrap_err(),
                "qstat_invalid_path",
                "{key_path} {hosts_path}"
            );
        }
    }

    #[test]
    fn project_and_account_fields_must_agree() {
        let mut value = response("F");
        value["Jobs"]["123.gadi-pbs"]["project"] = json!("zz00");
        assert!(parse_value(value.clone()).is_ok());
        value["Jobs"]["123.gadi-pbs"]
            .as_object_mut()
            .unwrap()
            .remove("Account_Name");
        assert!(parse_value(value.clone()).is_ok());
        value["Jobs"]["123.gadi-pbs"]["Account_Name"] = json!("other");
        assert_eq!(parse_value(value.clone()), Err("qstat_project_mismatch"));
        value["Jobs"]["123.gadi-pbs"]["project"] = json!("other");
        assert_eq!(parse_value(value.clone()), Err("qstat_project_mismatch"));
        value["Jobs"]["123.gadi-pbs"]["project"] = Value::Null;
        assert_eq!(parse_value(value), Err("qstat_project_mismatch"));
    }

    #[test]
    fn identity_and_incomplete_results_retry() {
        for (key, value) in [
            ("Job_Owner", json!("other@gadi")),
            ("Job_Name", json!("other")),
            ("Account_Name", json!("wrong")),
            ("Exit_status", Value::Null),
        ] {
            let mut v = response("F");
            v["Jobs"]["123.gadi-pbs"][key] = value;
            assert!(parse_value(v).is_err());
        }
        assert!(parse_value(json!({"Jobs":{}})).is_err());
        let mut wrong_id = response("F");
        let record = wrong_id["Jobs"]
            .as_object_mut()
            .unwrap()
            .remove("123.gadi-pbs")
            .unwrap();
        wrong_id["Jobs"]["124.gadi-pbs"] = record;
        assert_eq!(parse_value(wrong_id), Err("qstat_job_mismatch"));
        let mut v = response("F");
        v["Jobs"]["124.gadi-pbs"] = v["Jobs"]["123.gadi-pbs"].clone();
        assert!(parse_value(v).is_err());
        let mut t = target();
        t.remote_name = Some("prefix-different".into());
        assert!(parse(&config(), &t, &serde_json::to_vec(&response("F")).unwrap()).is_err());
        assert_eq!(
            parse(&config(), &target(), &vec![b' '; MAX_OUTPUT + 1]),
            Err("qstat_output_limit")
        );
    }

    #[test]
    fn process_limits_and_failure() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "exit 1"]);
        assert_eq!(run(cmd, Duration::from_secs(1)), Err("qstat_remote_failed"));
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "echo 'user@gadi.nci.org.au: Permission denied (publickey).' >&2; exit 255"]);
        assert_eq!(run(cmd, Duration::from_secs(2)), Err("qstat_ssh_auth_failed"));
        // A descendant holding the pipes open does not delay the failure past the grace.
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "sleep 5 & echo 'Host key verification failed.' >&2; exit 255"]);
        let start = Instant::now();
        assert_eq!(run(cmd, Duration::from_secs(4)), Err("qstat_host_key_mismatch"));
        assert!(start.elapsed() < Duration::from_secs(3));
        assert_eq!(
            run(Command::new("/nonexistent/ssh"), Duration::from_secs(1)),
            Err("qstat_ssh_missing")
        );
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "sleep 5 & wait"]);
        let start = Instant::now();
        assert_eq!(run(cmd, Duration::from_millis(50)), Err("qstat_timeout"));
        assert!(start.elapsed() < Duration::from_secs(2));
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "sleep 5 & exit 0"]);
        assert_eq!(run(cmd, Duration::from_millis(50)), Err("qstat_timeout"));
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "head -c 300000 /dev/zero"]);
        assert_eq!(run(cmd, Duration::from_secs(2)), Err("qstat_output_limit"));
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "head -c 20000 /dev/zero >&2"]);
        assert_eq!(run(cmd, Duration::from_secs(2)), Err("qstat_output_limit"));
    }

    #[test]
    fn exit_is_observed_without_reaping_the_group_leader() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 3"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let start = Instant::now();
        let observed = loop {
            if let Some(status) = exited_unreaped(pid).unwrap() {
                break status;
            }
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(observed, Some(3));
        // Still a zombie: the pid (and its group id) cannot have been reused.
        assert!(Path::new(&format!("/proc/{pid}")).exists());
        assert_eq!(exited_unreaped(pid).unwrap(), Some(Some(3)));
        assert_eq!(child.wait().unwrap().code(), Some(3));
    }

    #[test]
    fn failures_are_classified_for_the_owner() {
        let cases: &[(Option<i32>, &str, &str)] = &[
            (Some(255), "Host key verification failed.", "qstat_host_key_mismatch"),
            (Some(255), "@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@", "qstat_host_key_mismatch"),
            (Some(255), "Unable to negotiate with 1.2.3.4 port 22: no matching host key type found.", "qstat_host_key_mismatch"),
            (Some(255), "u@gadi.nci.org.au: Permission denied (publickey).", "qstat_ssh_auth_failed"),
            (Some(255), "ssh: Could not resolve hostname gadi.nci.org.au: Name or service not known", "qstat_ssh_connect_failed"),
            (Some(255), "ssh: connect to host gadi.nci.org.au port 22: Connection timed out", "qstat_ssh_connect_failed"),
            (Some(255), "something else", "qstat_ssh_failed"),
            (Some(126), "Only the fixed qstat query is permitted.", "qstat_helper_refused"),
            (Some(2), "/usr/bin/python3: can't open file '/home/x/.cutex/nci-qstat-only.py': [Errno 2] No such file or directory", "qstat_helper_missing"),
            (Some(153), "qstat: Unknown Job Id 123.gadi-pbs", "qstat_job_unknown"),
            (Some(1), "", "qstat_remote_failed"),
            (None, "", "qstat_remote_failed"),
        ];
        for (status, stderr, code) in cases {
            assert_eq!(failure_code(*status, stderr.as_bytes()), *code, "{stderr}");
        }
    }

    #[test]
    fn release_semantics_use_the_system_ssh_without_the_test_variable() {
        // The lib tests never set CUTEX_TEST_NCI_SSH; the stub needs it explicitly.
        if std::env::var_os("CUTEX_TEST_NCI_SSH").is_none() {
            assert_eq!(ssh_program(), "/usr/bin/ssh");
        }
    }
}
