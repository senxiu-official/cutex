//! macOS service manager backend: two per-user launchd agents
//! (`~/Library/LaunchAgents/<label>.plist`), the counterparts of the systemd
//! `--user` units in [`super::service`].
//!
//! - Labels come from the unit names the install records
//!   (`cutex-agent-bus.service` → `dev.cutex.agent-bus`,
//!   `cutex-management-api.service` → `dev.cutex.management-api`; the debug
//!   drill names `cutex-<label>-…` keep their label part), so `install.json`
//!   needs no new fields.
//! - `RunAtLoad` + `KeepAlive {SuccessfulExit: false}` + `ThrottleInterval 3`
//!   = `WantedBy=default.target` + `Restart=on-failure` + `RestartSec=3`.
//! - The Management API plist sets `AbandonProcessGroup`: launchd then does
//!   not kill what the job started when it stops (`KillMode=process`: native
//!   app-servers survive a restart; reload upgrades re-adopt them).
//! - Start / stop = `launchctl bootstrap gui/<uid> <plist>` /
//!   `launchctl bootout gui/<uid>/<label>` (the per-user GUI domain: the
//!   agents run while the user is logged in, like systemd without linger).
//!
//! Status: generation and the command plans are unit-tested; nothing here
//! has been bootstrapped on a real Mac yet (docs/macos/MVP.md).

use std::path::{Path, PathBuf};

use anyhow::Context;

use super::service::{Exec, ServiceControl, ServiceNames};

pub const LABEL_PREFIX: &str = "dev.cutex.";
/// First line after the XML prolog of every plist `cutex service install`
/// writes; a plist without it is never overwritten or removed.
pub const PLIST_MARKER: &str = "<!-- Managed by `cutex service install`; edits are overwritten. -->";

/// `cutex-agent-bus.service` → `dev.cutex.agent-bus`.
pub fn label_for_unit(unit: &str) -> String {
    let stem = unit.strip_suffix(".service").unwrap_or(unit);
    let stem = stem.strip_prefix("cutex-").unwrap_or(stem);
    format!("{LABEL_PREFIX}{stem}")
}

pub fn plist_file_name(label: &str) -> String {
    format!("{label}.plist")
}

/// `~/Library/LaunchAgents` (with a test private home: under that home).
pub fn default_agents_dir() -> Option<PathBuf> {
    crate::config::paths::home_dir().map(|home| home.join("Library/LaunchAgents"))
}

/// The macOS binary name inside a release (`layout::CUTEX_EXE` on a Mac).
const MACOS_CUTEX_EXE: &str = "cutex";

/// `<base>/<parts…>` joined with `/` whatever the build host: plists only
/// run on macOS, so their paths stay POSIX (identical to `Path::join` there)
/// and the generator tests pass on Windows too.
fn posix_join(base: &Path, parts: &[&str]) -> PathBuf {
    let mut out = base.display().to_string();
    for part in parts {
        if !out.ends_with('/') {
            out.push('/');
        }
        out.push_str(part);
    }
    PathBuf::from(out)
}

/// Where the agents' stdout / stderr go: `<base>/logs/<label>.log`.
pub fn log_path(base: &Path, label: &str) -> PathBuf {
    posix_join(base, &["logs", &format!("{label}.log")])
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn string(value: &str) -> String {
    format!("<string>{}</string>", xml_escape(value))
}

/// One launchd agent definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSpec {
    pub label: String,
    pub program_arguments: Vec<String>,
    pub environment: Vec<(String, String)>,
    pub working_directory: PathBuf,
    pub log: PathBuf,
    /// Leave the job's children running when launchd stops it.
    pub abandon_process_group: bool,
}

impl AgentSpec {
    pub fn plist(&self) -> String {
        let args: String =
            self.program_arguments.iter().map(|arg| format!("\n    {}", string(arg))).collect();
        let env: String = self
            .environment
            .iter()
            .map(|(key, value)| format!("\n    <key>{}</key>\n    {}", xml_escape(key), string(value)))
            .collect();
        let log = self.log.display().to_string();
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
{PLIST_MARKER}
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  {label}
  <key>ProgramArguments</key>
  <array>{args}
  </array>
  <key>EnvironmentVariables</key>
  <dict>{env}
  </dict>
  <key>WorkingDirectory</key>
  {cwd}
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ThrottleInterval</key>
  <integer>3</integer>
  <key>ProcessType</key>
  <string>Background</string>
  <key>AbandonProcessGroup</key>
  {abandon}
  <key>StandardOutPath</key>
  {log}
  <key>StandardErrorPath</key>
  {log}
</dict>
</plist>
"#,
            label = string(&self.label),
            cwd = string(&self.working_directory.display().to_string()),
            abandon = if self.abandon_process_group { "<true/>" } else { "<false/>" },
            log = string(&log),
        )
    }
}

/// Environment every agent carries: the service manager, the install base
/// and, for an independent instance, its `CUTEX_HOME` (the systemd units'
/// `Environment=` lines). launchd starts jobs with a minimal PATH; native
/// tools need the usual system directories.
fn agent_environment(base: &Path, cutex_home: Option<&Path>) -> Vec<(String, String)> {
    let mut env = vec![
        (super::service_manager::SERVICE_MANAGER_ENV.to_string(), super::service_manager::LAUNCHD.to_string()),
        (super::layout::INSTALL_BASE_ENV.to_string(), base.display().to_string()),
        ("PATH".to_string(), "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin".to_string()),
    ];
    if let Some(home) = cutex_home {
        env.push((crate::config::instance::HOME_ENV.to_string(), home.display().to_string()));
    }
    env
}

fn exe(base: &Path) -> String {
    posix_join(base, &["current", MACOS_CUTEX_EXE]).display().to_string()
}

pub fn bus_agent(base: &Path, names: &ServiceNames, cutex_home: Option<&Path>) -> AgentSpec {
    let label = label_for_unit(&names.bus_unit);
    AgentSpec {
        program_arguments: vec![exe(base), "agent".into(), "serve".into()],
        environment: agent_environment(base, cutex_home),
        working_directory: base.to_path_buf(),
        log: log_path(base, &label),
        abandon_process_group: false,
        label,
    }
}

pub fn management_agent(base: &Path, names: &ServiceNames, cutex_home: Option<&Path>) -> AgentSpec {
    let label = label_for_unit(&names.management_unit);
    AgentSpec {
        program_arguments: vec![exe(base), "management".into(), "serve".into()],
        environment: agent_environment(base, cutex_home),
        working_directory: base.to_path_buf(),
        log: log_path(base, &label),
        abandon_process_group: true,
        label,
    }
}

/// `gui/<uid>`: the logged-in user's launchd domain.
pub fn domain(uid: u32) -> String {
    format!("gui/{uid}")
}

pub fn bootstrap_args(uid: u32, plist: &Path) -> Vec<String> {
    vec!["bootstrap".into(), domain(uid), plist.display().to_string()]
}

pub fn bootout_args(uid: u32, label: &str) -> Vec<String> {
    vec!["bootout".into(), format!("{}/{label}", domain(uid))]
}

pub fn print_args(uid: u32, label: &str) -> Vec<String> {
    vec!["print".into(), format!("{}/{label}", domain(uid))]
}

pub fn current_uid() -> u32 {
    #[cfg(unix)]
    {
        unsafe { libc::getuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// Fields of `launchctl print gui/<uid>/<label>` the status shows.
pub fn parse_print(text: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut fields = serde_json::Map::new();
    for line in text.lines() {
        let line = line.trim();
        for key in ["state", "pid", "last exit code", "runs"] {
            if let Some(value) = line.strip_prefix(key).and_then(|rest| rest.trim_start().strip_prefix('=')) {
                fields.entry(key.replace(' ', "_")).or_insert_with(|| serde_json::json!(value.trim()));
            }
        }
    }
    fields
}

pub fn existing_foreign(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|text| !text.lines().take(3).any(|line| line == PLIST_MARKER))
}

/// The runner's stop / start through launchctl (bootout tolerates an agent
/// that is not loaded).
pub struct LaunchdControl<'a> {
    pub exec: &'a dyn Exec,
    pub names: ServiceNames,
    pub agents_dir: PathBuf,
    pub uid: u32,
}

impl LaunchdControl<'_> {
    fn bootout(&self, unit: &str) -> anyhow::Result<()> {
        let label = label_for_unit(unit);
        match self.exec.run("launchctl", &bootout_args(self.uid, &label)) {
            Ok(_) => Ok(()),
            // 3 / 113: "No such process" / "Could not find service": not loaded.
            Err(error) if error.to_string().contains("No such process") || error.to_string().contains("Could not find") => Ok(()),
            Err(error) => Err(error),
        }
    }
    fn bootstrap(&self, unit: &str) -> anyhow::Result<()> {
        let plist = self.agents_dir.join(plist_file_name(&label_for_unit(unit)));
        anyhow::ensure!(plist.is_file(), "{} missing", plist.display());
        self.exec.run("launchctl", &bootstrap_args(self.uid, &plist)).map(|_| ())
    }
}

impl ServiceControl for LaunchdControl<'_> {
    fn stop_all(&self) -> anyhow::Result<()> {
        self.bootout(&self.names.management_unit)?;
        self.bootout(&self.names.bus_unit)
    }
    fn start_all(&self) -> anyhow::Result<()> {
        self.bootstrap(&self.names.bus_unit)?;
        self.bootstrap(&self.names.management_unit)
    }
}

/// Write both plists (refusing one this command did not write unless
/// `force`). Returns their paths (bus, management).
pub fn write_agents(
    agents_dir: &Path,
    base: &Path,
    names: &ServiceNames,
    cutex_home: Option<&Path>,
    force: bool,
) -> anyhow::Result<(PathBuf, PathBuf)> {
    let bus = bus_agent(base, names, cutex_home);
    let management = management_agent(base, names, cutex_home);
    let bus_path = agents_dir.join(plist_file_name(&bus.label));
    let management_path = agents_dir.join(plist_file_name(&management.label));
    for path in [&bus_path, &management_path] {
        anyhow::ensure!(
            force || !existing_foreign(path),
            "{} exists and was not written by `cutex service install`; refusing (use --force to replace it)",
            path.display()
        );
    }
    std::fs::create_dir_all(agents_dir).with_context(|| format!("create {}", agents_dir.display()))?;
    std::fs::create_dir_all(base.join("logs"))?;
    crate::config::atomic::write_bytes_atomic(&bus_path, bus.plist().as_bytes())?;
    crate::config::atomic::write_bytes_atomic(&management_path, management.plist().as_bytes())?;
    Ok((bus_path, management_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upgrade::service::fake::FakeExec;

    #[test]
    fn labels_follow_the_recorded_unit_names() {
        let names = ServiceNames::default();
        assert_eq!(label_for_unit(&names.bus_unit), "dev.cutex.agent-bus");
        assert_eq!(label_for_unit(&names.management_unit), "dev.cutex.management-api");
        let drill = ServiceNames::for_test_label("drill");
        assert_eq!(label_for_unit(&drill.bus_unit), "dev.cutex.drill-agent-bus");
        assert_eq!(label_for_unit(&drill.management_unit), "dev.cutex.drill-management-api");
    }

    #[test]
    fn management_plist_runs_the_current_release_and_keeps_natives_alive() {
        let base = Path::new("/Users/me/Applications/cutex & co");
        let spec = management_agent(base, &ServiceNames::default(), Some(Path::new("/Users/me/.cutex-kutex")));
        let plist = spec.plist();
        assert!(plist.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"));
        assert_eq!(plist.lines().nth(1), Some(PLIST_MARKER));
        assert!(plist.contains("<key>Label</key>\n  <string>dev.cutex.management-api</string>"));
        assert!(plist.contains("<string>/Users/me/Applications/cutex &amp; co/current/cutex</string>\n    <string>management</string>\n    <string>serve</string>"));
        assert!(plist.contains("<key>CUTEX_SERVICE_MANAGER</key>\n    <string>launchd</string>"));
        assert!(plist.contains(&format!("<key>{}</key>\n    <string>/Users/me/Applications/cutex &amp; co</string>", crate::upgrade::layout::INSTALL_BASE_ENV)));
        assert!(plist.contains(&format!("<key>{}</key>\n    <string>/Users/me/.cutex-kutex</string>", crate::config::instance::HOME_ENV)));
        assert!(plist.contains("<key>AbandonProcessGroup</key>\n  <true/>"));
        assert!(plist.contains("<key>SuccessfulExit</key>\n    <false/>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(plist.contains("<string>/Users/me/Applications/cutex &amp; co/logs/dev.cutex.management-api.log</string>"));
        // Balanced, well-formed enough for plutil: every opened dict/array closes.
        assert_eq!(plist.matches("<dict>").count(), plist.matches("</dict>").count());
        assert_eq!(plist.matches("<array>").count(), plist.matches("</array>").count());
    }

    #[test]
    fn bus_plist_lets_launchd_stop_its_group() {
        let spec = bus_agent(Path::new("/opt/cutex"), &ServiceNames::default(), None);
        let plist = spec.plist();
        assert!(plist.contains("<string>/opt/cutex/current/cutex</string>\n    <string>agent</string>\n    <string>serve</string>"));
        assert!(plist.contains("<key>AbandonProcessGroup</key>\n  <false/>"));
        assert!(!plist.contains(crate::config::instance::HOME_ENV));
    }

    /// `plutil -lint` accepts what we write (macOS only: plutil ships with it).
    #[cfg(target_os = "macos")]
    #[test]
    fn plutil_accepts_the_generated_plists() {
        let dir = std::env::temp_dir().join(format!("cutex-launchd-{}", uuid::Uuid::new_v4()));
        let base = dir.join("base dir");
        let (bus, management) = write_agents(&dir.join("agents"), &base, &ServiceNames::default(), None, false).unwrap();
        for path in [bus, management] {
            let out = std::process::Command::new("/usr/bin/plutil").arg("-lint").arg(&path).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn foreign_plists_are_not_overwritten() {
        let dir = std::env::temp_dir().join(format!("cutex-launchd-{}", uuid::Uuid::new_v4()));
        let agents = dir.join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let names = ServiceNames::default();
        let path = agents.join("dev.cutex.agent-bus.plist");
        std::fs::write(&path, "<?xml version=\"1.0\"?>\n<plist/>").unwrap();
        assert!(write_agents(&agents, &dir, &names, None, false).unwrap_err().to_string().contains("refusing"));
        write_agents(&agents, &dir, &names, None, true).unwrap();
        assert!(!existing_foreign(&path));
        // Ours can be rewritten without force.
        write_agents(&agents, &dir, &names, None, false).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn control_boots_out_management_first_and_bootstraps_bus_first() {
        let dir = std::env::temp_dir().join(format!("cutex-launchd-{}", uuid::Uuid::new_v4()));
        let agents = dir.join("agents");
        let names = ServiceNames::default();
        write_agents(&agents, &dir, &names, None, false).unwrap();
        let exec = FakeExec::default();
        let control = LaunchdControl { exec: &exec, names, agents_dir: agents.clone(), uid: 501 };
        control.stop_all().unwrap();
        control.start_all().unwrap();
        let calls = exec.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![
                "launchctl bootout gui/501/dev.cutex.management-api".to_string(),
                "launchctl bootout gui/501/dev.cutex.agent-bus".to_string(),
                format!("launchctl bootstrap gui/501 {}", agents.join("dev.cutex.agent-bus.plist").display()),
                format!("launchctl bootstrap gui/501 {}", agents.join("dev.cutex.management-api.plist").display()),
            ]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bootout_of_an_unloaded_agent_is_not_an_error() {
        let exec = FakeExec::default();
        *exec.fail.lock().unwrap() = Some("bootout".into());
        let control = LaunchdControl { exec: &exec, names: ServiceNames::default(), agents_dir: PathBuf::from("/nonexistent"), uid: 501 };
        // FakeExec's failure text is "fake failure: …": a real error, surfaced.
        assert!(control.stop_all().is_err());
        assert!(control.start_all().is_err(), "missing plists refuse to bootstrap");
    }

    #[test]
    fn print_output_fields() {
        let text = "gui/501/dev.cutex.agent-bus = {\n\tactive count = 1\n\tstate = running\n\tpid = 4242\n\truns = 3\n\tlast exit code = 0\n}";
        let fields = parse_print(text);
        assert_eq!(fields["state"], "running");
        assert_eq!(fields["pid"], "4242");
        assert_eq!(fields["runs"], "3");
        assert_eq!(fields["last_exit_code"], "0");
    }
}
