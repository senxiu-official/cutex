//! Human-selected host routing. Display names and SSH aliases are not identities.
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hosts {
    #[serde(default)]
    pub local_name: Option<String>,
    #[serde(default)]
    pub connections: Vec<Connection>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    pub id: String,
    pub name: String,
    pub host_id: String,
    pub ssh_target: String,
    pub local_port: u16,
    pub remote_port: u16,
    pub token_file: PathBuf,
    #[serde(default = "enabled")]
    pub enabled: bool,
}
fn enabled() -> bool {
    true
}
fn label(s: &str) -> bool {
    !s.trim().is_empty() && s.len() <= 256 && !s.chars().any(char::is_control)
}
pub fn path() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::config_dir()?.join("hosts.json"))
}
impl Hosts {
    pub fn load() -> anyhow::Result<Self> {
        match std::fs::read(path()?) {
            Ok(bytes) => {
                let value: Self = serde_json::from_slice(&bytes)?;
                value.validate()?;
                Ok(value)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.local_name.as_ref().is_none_or(|s| label(s)),
            "Invalid local display name"
        );
        let local = crate::platform::host::current_host_name();
        let mut ids = std::collections::BTreeSet::new();
        let mut hosts = ids.clone();
        let mut ports = std::collections::BTreeSet::new();
        for c in &self.connections {
            ensure!(
                label(&c.id)
                    && c.id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
                "Connection ID must use letters, digits, - or _"
            );
            ensure!(
                label(&c.name) && label(&c.host_id),
                "Invalid host name or identity"
            );
            ensure!(
                !crate::runtime::lifecycle::cutex_session_host_is_local(&c.host_id, &local),
                "Remote connection identifies the local host"
            );
            ensure!(
                label(&c.ssh_target)
                    && !c.ssh_target.starts_with('-')
                    && !c.ssh_target.chars().any(char::is_whitespace),
                "Invalid SSH target"
            );
            ensure!(
                c.local_port != 0 && c.remote_port != 0 && ![24260, 24270].contains(&c.local_port),
                "Choose a distinct nonzero local tunnel port"
            );
            ensure!(
                c.token_file.is_absolute(),
                "Credential file must be an absolute path"
            );
            ensure!(
                ids.insert(c.id.clone())
                    && hosts.insert(c.host_id.to_lowercase())
                    && ports.insert(c.local_port),
                "Duplicate connection ID, host identity or local port"
            );
        }
        Ok(())
    }
    pub fn save(&self) -> anyhow::Result<()> {
        self.validate()?;
        if let Some(cache) = DISPLAY_CACHE.get() {
            if let Ok(mut cache) = cache.lock() {
                *cache = None;
            }
        }
        crate::config::atomic::write_private_pretty_json_atomic(&path()?, self, "host connections")
    }
    pub fn for_host(&self, host: &str) -> anyhow::Result<&Connection> {
        self.connections
            .iter()
            .find(|c| c.host_id.eq_ignore_ascii_case(host) && c.enabled)
            .with_context(|| {
                format!(
                    "Host {host} is not connected/configured; open Settings → Hosts / Connections"
                )
            })
    }
    pub fn display(&self, host: &str) -> String {
        if host.is_empty() || host == "-" {
            return "N/A".into();
        }
        if crate::runtime::lifecycle::cutex_session_host_is_local(
            host,
            &crate::platform::host::current_host_name(),
        ) {
            return format!("Local · {}", self.local_name.as_deref().unwrap_or(host));
        }
        match self
            .connections
            .iter()
            .find(|c| c.host_id.eq_ignore_ascii_case(host))
        {
            Some(c) => format!(
                "Remote · {}{}",
                c.name,
                if c.enabled { "" } else { " (disabled)" }
            ),
            None => format!("Remote · {host} (unconfigured)"),
        }
    }
}
impl Connection {
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.local_port)
    }
    pub fn tunnel_command(&self) -> String {
        // Quoting also makes aliases containing shell punctuation safe to copy.
        let target = format!("'{}'", self.ssh_target.replace('\'', "'\\''"));
        format!(
            "ssh -N -o ExitOnForwardFailure=yes -L 127.0.0.1:{}:127.0.0.1:{} -- {target}",
            self.local_port, self.remote_port
        )
    }
    pub fn verified_endpoint(&self) -> anyhow::Result<(String, String)> {
        ensure!(self.enabled, "Connection disabled");
        let token = read_token(&self.token_file)?;
        let reply = super::remote::management_http_json_with_timeout(
            &self.base_url(),
            "GET",
            "/v2/host",
            Some(&token),
            None,
            std::time::Duration::from_secs(3),
        )
        .with_context(|| {
            format!(
                "Connection {} unavailable. Start its tunnel: {}",
                self.id,
                self.tunnel_command()
            )
        })?;
        ensure!(
            reply["hostId"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(&self.host_id)),
            "Connection {} points to a different host; expected {}",
            self.id,
            self.host_id
        );
        Ok((self.base_url(), token))
    }
}
fn read_token(path: &Path) -> anyhow::Result<String> {
    use std::io::Read;
    let file =
        std::fs::File::open(path).context("Cannot open remote Management credential file")?;
    let mut bytes = Vec::new();
    file.take(8193).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 8192, "Credential file is too large");
    let token = String::from_utf8(bytes)?.trim().to_owned();
    ensure!(
        !token.is_empty() && !token.chars().any(char::is_control),
        "Invalid credential file"
    );
    Ok(token)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn connection(id: &str, host: &str, port: u16) -> Connection {
        Connection {
            id: id.into(),
            name: "Display".into(),
            host_id: host.into(),
            ssh_target: "ssh-alias".into(),
            local_port: port,
            remote_port: 24270,
            token_file: std::env::temp_dir().join("root-token"),
            enabled: true,
        }
    }
    #[test]
    fn identities_aliases_and_ports_are_independent() {
        let mut h = Hosts {
            local_name: Some("Desk".into()),
            connections: vec![
                connection("one", "remote-one", 24671),
                connection("two", "remote-two", 24672),
            ],
        };
        h.validate().unwrap();
        assert_eq!(h.for_host("REMOTE-TWO").unwrap().local_port, 24672);
        h.connections[0].name = "Renamed".into();
        assert_eq!(h.for_host("remote-one").unwrap().ssh_target, "ssh-alias");
        h.connections[1].local_port = 24671;
        assert!(h.validate().is_err());
        h.connections[1].local_port = 24672;
        h.connections[1].host_id = "REMOTE-ONE".into();
        assert!(h.validate().is_err());
    }
    #[test]
    fn unconfigured_hosts_are_not_inferred_as_ssh_targets() {
        assert!(Hosts::default().for_host("unknown").is_err());
    }
}

static DISPLAY_CACHE: std::sync::OnceLock<std::sync::Mutex<Option<(std::time::Instant, Hosts)>>> =
    std::sync::OnceLock::new();
pub fn display(host: &str) -> String {
    let cache = DISPLAY_CACHE.get_or_init(|| std::sync::Mutex::new(None));
    if let Ok(mut cache) = cache.lock() {
        if cache
            .as_ref()
            .is_none_or(|(time, _)| time.elapsed() > std::time::Duration::from_secs(5))
        {
            *cache = Some((std::time::Instant::now(), Hosts::load().unwrap_or_default()));
        }
        return cache.as_ref().unwrap().1.display(host);
    }
    Hosts::default().display(host)
}

#[cfg(test)]
mod endpoint_tests {
    use super::*;
    use std::io::{Read,Write};
    #[test]
    fn wrong_peer_identity_is_rejected_before_any_runtime_request() {
        let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port=listener.local_addr().unwrap().port();
        let file=std::env::temp_dir().join(format!("cutex-host-token-{}",uuid::Uuid::new_v4()));
        std::fs::write(&file,"test-root-secret").unwrap();
        let c=Connection{id:"test".into(),name:"test".into(),host_id:"expected-host".into(),ssh_target:"alias".into(),local_port:port,remote_port:24270,token_file:file.clone(),enabled:true};
        let worker=std::thread::spawn(move|| {
            for host in ["wrong-host","expected-host"] {
                let (mut stream,_)=listener.accept().unwrap();
                stream.set_read_timeout(Some(std::time::Duration::from_secs(3))).unwrap();
                let mut received=Vec::new();let mut buf=[0;1024];
                while !received.windows(4).any(|w|w==b"\r\n\r\n") {let n=stream.read(&mut buf).unwrap();assert!(n>0);received.extend_from_slice(&buf[..n]);}
                let request=String::from_utf8(received).unwrap();assert!(request.starts_with("GET /v2/host "));assert!(request.contains("test-root-secret"));
                let body=serde_json::json!({"hostId":host}).to_string();
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
            }
        });
        assert!(c.verified_endpoint().unwrap_err().to_string().contains("different host"));
        assert_eq!(c.verified_endpoint().unwrap().1,"test-root-secret");
        worker.join().unwrap();std::fs::remove_file(file).unwrap();
    }
}

/// Compact table label; full routing detail remains available through display().
pub fn short_display(host:&str)->String {
    let full=display(host);
    full.strip_prefix("Local · ").or_else(||full.strip_prefix("Remote · ")).unwrap_or(&full).trim_end_matches(" (disabled)").trim_end_matches(" (unconfigured)").to_owned()
}
