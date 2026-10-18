//! The proxy of cutex's own outbound HTTP (upgrades and component downloads, quota readers, model
//! discovery, `web_search`, the LiteLLM price catalog, webhooks) and the "follow system" lookup.
//!
//! Three modes, from a stored [`ProxyConfig`] (host `config.json` `proxy`, or a Profile's
//! effective proxy):
//!
//! - **system** (nothing stored): the daemon's environment (`HTTPS_PROXY` / `HTTP_PROXY` /
//!   `ALL_PROXY`, `NO_PROXY`), then on Windows the per-user system proxy (registry
//!   `ProxyEnable` / `ProxyServer` / `ProxyOverride`), on macOS the network proxy settings
//!   (`scutil --proxy`: HTTPS / HTTP proxy, `ExceptionsList`). No PAC / WPAD: PAC users enter a
//!   manual URL.
//! - **none**: direct, whatever the environment says.
//! - **manual**: the stored URL; only Basic credentials inside the URL.
//!
//! Loopback (`127.0.0.1`, `localhost`, `::1`) always bypasses the proxy in every mode. SOCKS URLs
//! are refused for cutex's own requests (its HTTP client is built without SOCKS support and would
//! otherwise connect directly); runtimes still receive them as environment variables.

use crate::config::proxy::{effective_proxy_config, with_loopback_no_proxy};
use crate::profiles::model::{CodezConfig, ProxyConfig, StoredAccount};

/// How one outbound request chooses its proxy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutboundProxy {
    System,
    Direct,
    Manual { url: String, no_proxy: Option<String> },
}

/// Where a resolved proxy came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProxySource {
    Manual,
    Environment,
    WindowsSystem,
    MacosSystem,
}

impl ProxySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Environment => "environment",
            Self::WindowsSystem => "windowsSystem",
            Self::MacosSystem => "macosSystem",
        }
    }
}

/// A concrete proxy: URL (may carry credentials: never log it, use [`redacted_proxy_url`]) and the
/// NO_PROXY list including the loopback names.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedProxy {
    pub url: String,
    pub no_proxy: String,
    pub source: ProxySource,
}

impl std::fmt::Debug for ResolvedProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedProxy")
            .field("url", &redacted_proxy_url(&self.url).0)
            .field("no_proxy", &self.no_proxy)
            .field("source", &self.source)
            .finish()
    }
}

impl OutboundProxy {
    pub fn from_config(proxy: Option<&ProxyConfig>) -> Self {
        match proxy {
            None => Self::System,
            Some(proxy) if !proxy.enabled => Self::Direct,
            Some(proxy) => match proxy.url.as_deref().map(str::trim).filter(|url| !url.is_empty()) {
                Some(url) => Self::Manual { url: url.to_string(), no_proxy: proxy.no_proxy.clone() },
                None => Self::System,
            },
        }
    }

    /// The host proxy (`config.json` `proxy`).
    pub fn host() -> Self {
        Self::from_config(crate::config::store::load_codez_config().proxy.as_ref())
    }

    /// A Profile's effective proxy (its override, else the host proxy).
    pub fn for_profile(account: &StoredAccount, config: &CodezConfig) -> Self {
        Self::from_config(effective_proxy_config(account, config))
    }

    pub fn mode(&self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Direct => "none",
            Self::Manual { .. } => "manual",
        }
    }

    /// The proxy a request would use right now (`None`: direct).
    pub fn resolve(&self) -> Option<ResolvedProxy> {
        match self {
            Self::Direct => None,
            Self::Manual { url, no_proxy } => Some(ResolvedProxy {
                url: url.clone(),
                no_proxy: with_loopback_no_proxy(no_proxy.as_deref().unwrap_or_default()),
                source: ProxySource::Manual,
            }),
            Self::System => system_proxy(),
        }
    }

    /// The ureq proxy of this mode: `Ok(None)` = direct (ureq never reads the environment again).
    pub fn ureq_proxy(&self) -> anyhow::Result<Option<ureq::Proxy>> {
        self.resolve().map(|resolved| ureq_proxy(&resolved.url, &resolved.no_proxy)).transpose()
    }

    /// `builder` with this proxy set.
    pub fn apply(
        &self,
        builder: ureq::config::ConfigBuilder<ureq::typestate::AgentScope>,
    ) -> anyhow::Result<ureq::config::ConfigBuilder<ureq::typestate::AgentScope>> {
        Ok(builder.proxy(self.ureq_proxy()?))
    }
}

/// True for the URL schemes cutex's own HTTP client can tunnel through.
pub fn http_proxy_scheme(url: &str) -> bool {
    let scheme = url.split("://").next().unwrap_or_default().to_ascii_lowercase();
    !url.contains("://") || matches!(scheme.as_str(), "http" | "https")
}

/// A ureq proxy for `url` bypassing every `no_proxy` entry.
pub fn ureq_proxy(url: &str, no_proxy: &str) -> anyhow::Result<ureq::Proxy> {
    anyhow::ensure!(
        http_proxy_scheme(url),
        "cutex's own requests need an http:// or https:// proxy (SOCKS proxies only reach the runtimes)"
    );
    let parsed = ureq::Proxy::new(url).map_err(|_| anyhow::anyhow!("invalid proxy URL"))?;
    let mut builder = ureq::Proxy::builder(parsed.protocol()).host(parsed.host()).port(parsed.port());
    // Credentials go into the Basic header as written in the URL (not percent-decoded).
    if let Some(user) = parsed.username() {
        builder = builder.username(user);
    }
    if let Some(password) = parsed.password() {
        builder = builder.password(password);
    }
    for entry in no_proxy.split(',').map(str::trim).filter(|entry| !entry.is_empty()) {
        builder = builder.no_proxy(entry);
        // ureq compares hosts as written in the URI: `[::1]`.
        if entry == "::1" {
            builder = builder.no_proxy("[::1]");
        }
    }
    builder.build().map_err(|_| anyhow::anyhow!("invalid proxy URL"))
}

/// Proxy URL with any `user:password@` removed, and whether it had credentials.
pub fn redacted_proxy_url(url: &str) -> (String, bool) {
    match url::Url::parse(url) {
        Ok(mut parsed) if !parsed.username().is_empty() || parsed.password().is_some() => {
            let _ = parsed.set_username("");
            let _ = parsed.set_password(None);
            (parsed.to_string(), true)
        }
        Ok(parsed) if parsed.host_str().is_some() => (url.to_string(), false),
        // Unparseable (or `user:pass@host:port` without a scheme): drop anything before '@' defensively.
        _ => match url.rsplit_once('@') {
            Some((prefix, host)) => {
                let scheme = prefix.split_once("://").map(|(s, _)| s).unwrap_or("");
                (format!("{scheme}://{host}"), true)
            }
            None => (url.to_string(), false),
        },
    }
}

/// A clear message for a proxy failure of a ureq request (407 = credentials), `None` otherwise.
pub fn proxy_error_message(error: &ureq::Error) -> Option<String> {
    match error {
        ureq::Error::StatusCode(407) => Some(PROXY_AUTH_MESSAGE.to_string()),
        ureq::Error::ConnectProxyFailed(reason) if reason.contains("407") => {
            Some(PROXY_AUTH_MESSAGE.to_string())
        }
        ureq::Error::ConnectProxyFailed(reason) => Some(format!("the proxy refused the connection: {reason}")),
        _ => None,
    }
}

pub const PROXY_AUTH_MESSAGE: &str = "the proxy requires authentication (HTTP 407): put user:password@ in the proxy URL (Basic only; NTLM / Kerberos proxies are not supported)";

// ------------------------------------------------------------------ system lookup

/// The environment's proxy for HTTP(S) requests, then the OS proxy setting ([`os_system_proxy`]).
pub fn system_proxy() -> Option<ResolvedProxy> {
    let env_no_proxy = env_value(&["NO_PROXY", "no_proxy"]).unwrap_or_default();
    if let Some(url) = env_proxy_url() {
        return Some(ResolvedProxy {
            url,
            no_proxy: with_loopback_no_proxy(&env_no_proxy),
            source: ProxySource::Environment,
        });
    }
    let os = os_system_proxy()?;
    let mut no_proxy = env_no_proxy;
    if let Some(overrides) = os.overrides {
        if !no_proxy.is_empty() {
            no_proxy.push(',');
        }
        no_proxy.push_str(&overrides);
    }
    Some(ResolvedProxy {
        url: os.url,
        no_proxy: with_loopback_no_proxy(&no_proxy),
        source: os.source,
    })
}

fn env_value(names: &[&str]) -> Option<String> {
    names
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
}

/// First non-empty HTTP(S) proxy of `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` (either case). A SOCKS
/// value is skipped (cutex's client cannot use it).
fn env_proxy_url() -> Option<String> {
    ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty() && http_proxy_scheme(value))
}

/// True when the environment names a proxy (any scheme): runtimes inherit it as is.
pub fn environment_has_proxy() -> bool {
    env_value(&["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]).is_some()
}

/// The OS-level proxy setting "follow system" falls back to when the environment names none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsSystemProxy {
    pub url: String,
    /// Bypass entries as a NO_PROXY list (Windows `ProxyOverride` without `<local>`; macOS
    /// `ExceptionsList`).
    pub overrides: Option<String>,
    pub source: ProxySource,
}

/// The per-user OS proxy: the Windows registry, the macOS network settings, nothing elsewhere.
pub fn os_system_proxy() -> Option<OsSystemProxy> {
    #[cfg(windows)]
    {
        windows_system_proxy()
    }
    #[cfg(target_os = "macos")]
    {
        macos_system_proxy()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        None
    }
}

/// `scutil --proxy` output (a dictionary dump) to one HTTP proxy: the enabled HTTPS proxy, else
/// the enabled HTTP proxy; `ExceptionsList` entries become the NO_PROXY list. PAC and SOCKS
/// settings give `None` (like the Windows lookup).
pub fn parse_scutil_proxy(output: &str) -> Option<OsSystemProxy> {
    let mut values = std::collections::BTreeMap::new();
    let mut exceptions = Vec::new();
    let mut in_exceptions = false;
    for line in output.lines() {
        let line = line.trim();
        if in_exceptions {
            if line.starts_with('}') {
                in_exceptions = false;
            } else if let Some((_, entry)) = line.split_once(" : ") {
                let entry = entry.trim();
                if !entry.is_empty() {
                    exceptions.push(entry.to_string());
                }
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else { continue };
        let (key, value) = (key.trim(), value.trim());
        if key == "ExceptionsList" && value.starts_with("<array>") {
            in_exceptions = true;
        } else if !value.starts_with('<') {
            values.insert(key.to_string(), value.to_string());
        }
    }
    let url = ["HTTPS", "HTTP"].iter().find_map(|kind| {
        if values.get(&format!("{kind}Enable")).map(String::as_str) != Some("1") {
            return None;
        }
        let host = values.get(&format!("{kind}Proxy")).filter(|host| !host.is_empty())?;
        let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.clone() };
        Some(match values.get(&format!("{kind}Port")) {
            Some(port) if !port.is_empty() => format!("http://{host}:{port}"),
            _ => format!("http://{host}"),
        })
    })?;
    Some(OsSystemProxy {
        url,
        overrides: (!exceptions.is_empty()).then(|| exceptions.join(",")),
        source: ProxySource::MacosSystem,
    })
}

/// The macOS network proxy settings via `scutil --proxy`, cached for 30 s (every outbound request
/// in "follow system" mode asks).
#[cfg(target_os = "macos")]
pub fn macos_system_proxy() -> Option<OsSystemProxy> {
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    static CACHE: OnceLock<Mutex<Option<(Instant, Option<OsSystemProxy>)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let mut cached = cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((at, value)) = cached.as_ref() {
        if at.elapsed() < Duration::from_secs(30) {
            return value.clone();
        }
    }
    let value = std::process::Command::new("/usr/sbin/scutil")
        .arg("--proxy")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| parse_scutil_proxy(&String::from_utf8_lossy(&output.stdout)));
    *cached = Some((Instant::now(), value.clone()));
    value
}

/// `ProxyServer` (`host:port` or `http=h:p;https=h:p;…`) to one HTTP proxy URL: the `https=`
/// entry, else `http=`, else a bare `host:port`. SOCKS-only settings give `None`.
pub fn parse_windows_proxy_server(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let with_scheme = |server: &str| -> String {
        let server = server.trim();
        if server.contains("://") { server.to_string() } else { format!("http://{server}") }
    };
    if !value.contains('=') {
        let first = value.split(';').map(str::trim).find(|part| !part.is_empty())?;
        return Some(with_scheme(first)).filter(|url| http_proxy_scheme(url));
    }
    let entries: Vec<(String, &str)> = value
        .split(';')
        .filter_map(|part| part.split_once('='))
        .map(|(scheme, server)| (scheme.trim().to_ascii_lowercase(), server.trim()))
        .filter(|(_, server)| !server.is_empty())
        .collect();
    ["https", "http"]
        .iter()
        .find_map(|wanted| entries.iter().find(|(scheme, _)| scheme == wanted))
        .map(|(_, server)| with_scheme(server))
        .filter(|url| http_proxy_scheme(url))
}

/// `ProxyOverride` (`a;b;<local>`) as a NO_PROXY list.
pub fn parse_windows_proxy_override(value: &str) -> Option<String> {
    let entries: Vec<&str> = value
        .split(';')
        .map(str::trim)
        .filter(|entry| !entry.is_empty() && !entry.eq_ignore_ascii_case("<local>"))
        .collect();
    (!entries.is_empty()).then(|| entries.join(","))
}

#[cfg(windows)]
pub fn windows_system_proxy() -> Option<OsSystemProxy> {
    let enabled = windows_registry::read_dword("ProxyEnable").is_some_and(|value| value == 1);
    if !enabled {
        return None;
    }
    let url = parse_windows_proxy_server(&windows_registry::read_string("ProxyServer")?)?;
    let overrides = windows_registry::read_string("ProxyOverride")
        .as_deref()
        .and_then(parse_windows_proxy_override);
    Some(OsSystemProxy { url, overrides, source: ProxySource::WindowsSystem })
}

#[cfg(windows)]
mod windows_registry {
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
    };

    const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn read_dword(name: &str) -> Option<u32> {
        let key = wide(KEY);
        let name = wide(name);
        let mut value: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        // SAFETY: valid NUL-terminated strings and a u32 buffer of `size` bytes.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                (&mut value as *mut u32).cast(),
                &mut size,
            )
        };
        (status == 0).then_some(value)
    }

    pub fn read_string(name: &str) -> Option<String> {
        let key = wide(KEY);
        let name = wide(name);
        let mut size: u32 = 0;
        // SAFETY: size query with a null buffer.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        };
        if status != 0 || size == 0 || size > 64 * 1024 {
            return None;
        }
        let mut buffer = vec![0u16; (size as usize).div_ceil(2)];
        // SAFETY: buffer holds `size` bytes.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if status != 0 {
            return None;
        }
        let len = buffer.iter().position(|unit| *unit == 0).unwrap_or(buffer.len());
        Some(String::from_utf16_lossy(&buffer[..len]))
    }
}

// ------------------------------------------------------------------ API views

/// `/v2/profiles` item `proxy` (`profile-proxy`): the Profile's own setting (`mode` inherit |
/// none | manual, redacted URL) and what it resolves to (`effective`: the host proxy when
/// inheriting). `editable` = this caller may change it (root credential only).
pub fn profile_proxy_view(account: &StoredAccount, config: &CodezConfig) -> serde_json::Value {
    let own = account.proxy.as_ref();
    let mode = match own {
        None => "inherit",
        Some(proxy) if !proxy.enabled => "none",
        Some(_) => "manual",
    };
    let (url, has_credentials) = own
        .and_then(|proxy| proxy.url.as_deref())
        .map(redacted_proxy_url)
        .map(|(url, creds)| (Some(url), creds))
        .unwrap_or((None, false));
    let effective = OutboundProxy::for_profile(account, config);
    let resolved = effective.resolve();
    serde_json::json!({
        "mode": mode,
        "url": url,
        "urlHasCredentials": has_credentials,
        "noProxy": own.and_then(|proxy| proxy.no_proxy.clone()),
        "effective": {
            "mode": effective.mode(),
            "url": resolved.as_ref().map(|resolved| redacted_proxy_url(&resolved.url).0),
            "source": resolved.as_ref().map(|resolved| resolved.source.as_str()),
        },
        "editable": crate::management::listeners::current_client_token().is_none(),
    })
}

// ------------------------------------------------------------------ runtime environment

/// Environment for a managed Claude runtime (adapter + Claude CLI; both honour `HTTPS_PROXY` /
/// `NO_PROXY`) of `profile` (its effective proxy; no profile: the host proxy). A configured proxy
/// goes in as is (explicit off: empty values + NO_PROXY `*`). In "follow system" mode the daemon
/// environment is inherited; on Windows and macOS, when it names no proxy, the OS proxy is handed
/// over explicitly (the Node-based Claude CLI reads neither the registry nor `scutil`).
pub fn claude_runtime_proxy_envs(profile: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
    let config = crate::config::store::load_codez_config();
    let account_proxy = match profile {
        Some(name) => {
            let store = crate::profiles::settings_api::load_store()?;
            crate::profiles::settings_api::find(&store, name).and_then(|account| account.proxy.clone())
        }
        None => None,
    };
    Ok(runtime_proxy_envs(account_proxy.as_ref().or(config.proxy.as_ref())))
}

/// [`claude_runtime_proxy_envs`] for an already resolved effective proxy.
pub fn runtime_proxy_envs(effective: Option<&ProxyConfig>) -> Vec<(String, String)> {
    if let Some(proxy) = effective {
        return crate::config::proxy::native_proxy_envs_for(Some(proxy));
    }
    if environment_has_proxy() {
        return Vec::new();
    }
    let Some(os) = os_system_proxy() else { return Vec::new() };
    system_envs(&os)
}

fn system_envs(windows: &OsSystemProxy) -> Vec<(String, String)> {
    let no_proxy = with_loopback_no_proxy(&{
        let mut list = std::env::var("NO_PROXY").unwrap_or_default();
        if let Some(overrides) = &windows.overrides {
            if !list.trim().is_empty() {
                list.push(',');
            }
            list.push_str(overrides);
        }
        list
    });
    let mut envs = Vec::new();
    for key in ["HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"] {
        envs.push((key.to_string(), windows.url.clone()));
    }
    envs.push(("NO_PROXY".to_string(), no_proxy.clone()));
    envs.push(("no_proxy".to_string(), no_proxy));
    envs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_from_stored_config() {
        assert_eq!(OutboundProxy::from_config(None), OutboundProxy::System);
        let off = ProxyConfig { enabled: false, url: None, no_proxy: None };
        assert_eq!(OutboundProxy::from_config(Some(&off)), OutboundProxy::Direct);
        assert!(OutboundProxy::Direct.resolve().is_none());
        assert!(OutboundProxy::Direct.ureq_proxy().unwrap().is_none());
        let manual = ProxyConfig {
            enabled: true,
            url: Some("http://u:pass-word@proxy.example:3128".into()),
            no_proxy: Some("corp.example".into()),
        };
        let mode = OutboundProxy::from_config(Some(&manual));
        let resolved = mode.resolve().unwrap();
        assert_eq!(resolved.source, ProxySource::Manual);
        assert_eq!(resolved.no_proxy, "corp.example,127.0.0.1,localhost,::1");
        let proxy = mode.ureq_proxy().unwrap().unwrap();
        assert_eq!(proxy.host(), "proxy.example");
        assert_eq!(proxy.port(), 3128);
        assert_eq!(proxy.password(), Some("pass-word"));
        for host in ["127.0.0.1", "localhost", "corp.example"] {
            let uri: ureq::http::Uri = format!("http://{host}/x").parse().unwrap();
            assert!(proxy.is_no_proxy(&uri), "{host}");
        }
        let uri: ureq::http::Uri = "http://[::1]:80/x".parse().unwrap();
        assert!(proxy.is_no_proxy(&uri));
        let uri: ureq::http::Uri = "https://github.com/x".parse().unwrap();
        assert!(!proxy.is_no_proxy(&uri));
        assert!(!format!("{resolved:?}").contains("pass-word"));
    }

    #[test]
    fn socks_is_refused_for_own_requests() {
        let mode = OutboundProxy::Manual { url: "socks5://127.0.0.1:1080".into(), no_proxy: None };
        let error = mode.ureq_proxy().unwrap_err().to_string();
        assert!(error.contains("SOCKS"), "{error}");
        assert!(http_proxy_scheme("proxy.example:8080"));
        assert!(http_proxy_scheme("HTTPS://proxy.example"));
    }

    #[test]
    fn windows_proxy_server_and_override_parsing() {
        assert_eq!(parse_windows_proxy_server("127.0.0.1:7890").as_deref(), Some("http://127.0.0.1:7890"));
        assert_eq!(
            parse_windows_proxy_server("http=10.0.0.1:80;https=10.0.0.2:443;ftp=x:1").as_deref(),
            Some("http://10.0.0.2:443")
        );
        assert_eq!(parse_windows_proxy_server("http=10.0.0.1:80").as_deref(), Some("http://10.0.0.1:80"));
        assert_eq!(parse_windows_proxy_server("socks=127.0.0.1:1080"), None);
        assert_eq!(parse_windows_proxy_server(""), None);
        assert_eq!(
            parse_windows_proxy_override("localhost;127.*;<local>; *.corp ").as_deref(),
            Some("localhost,127.*,*.corp")
        );
        assert_eq!(parse_windows_proxy_override("<local>"), None);
        let envs: std::collections::BTreeMap<_, _> = system_envs(&OsSystemProxy {
            url: "http://127.0.0.1:7890".into(),
            overrides: Some("*.corp".into()),
            source: ProxySource::WindowsSystem,
        })
        .into_iter()
        .collect();
        assert_eq!(envs["HTTPS_PROXY"], "http://127.0.0.1:7890");
        assert!(envs["NO_PROXY"].contains("*.corp") && envs["NO_PROXY"].contains("127.0.0.1"));
    }

    #[test]
    fn scutil_proxy_parsing() {
        let off = "<dictionary> {\n  HTTPEnable : 0\n  HTTPSEnable : 0\n  ProxyAutoConfigEnable : 0\n  SOCKSEnable : 0\n}\n";
        assert_eq!(parse_scutil_proxy(off), None);
        let on = "<dictionary> {\n  ExceptionsList : <array> {\n    0 : *.local\n    1 : 169.254/16\n  }\n  \
                  FTPPassive : 1\n  HTTPEnable : 1\n  HTTPPort : 8080\n  HTTPProxy : proxy.corp\n  \
                  HTTPSEnable : 1\n  HTTPSPort : 3128\n  HTTPSProxy : secure.corp\n  ProxyAutoConfigEnable : 0\n}\n";
        let parsed = parse_scutil_proxy(on).unwrap();
        assert_eq!(parsed.url, "http://secure.corp:3128");
        assert_eq!(parsed.overrides.as_deref(), Some("*.local,169.254/16"));
        assert_eq!(parsed.source, ProxySource::MacosSystem);
        let http_only = "<dictionary> {\n  HTTPEnable : 1\n  HTTPPort : 7890\n  HTTPProxy : 127.0.0.1\n  HTTPSEnable : 0\n}\n";
        let parsed = parse_scutil_proxy(http_only).unwrap();
        assert_eq!(parsed.url, "http://127.0.0.1:7890");
        assert_eq!(parsed.overrides, None);
        // PAC / SOCKS only: nothing for cutex's own client.
        let socks = "<dictionary> {\n  ProxyAutoConfigEnable : 1\n  ProxyAutoConfigURLString : http://wpad/x.pac\n  SOCKSEnable : 1\n  SOCKSPort : 1080\n  SOCKSProxy : 127.0.0.1\n}\n";
        assert_eq!(parse_scutil_proxy(socks), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_scutil_lookup_runs() {
        // Whatever this Mac is set to, the lookup must not panic and must report the macOS source.
        if let Some(found) = macos_system_proxy() {
            assert_eq!(found.source, ProxySource::MacosSystem);
        }
    }

    #[test]
    fn proxy_errors_are_explained() {
        let auth = proxy_error_message(&ureq::Error::ConnectProxyFailed("proxy server responded 407/407".into()));
        assert_eq!(auth.as_deref(), Some(PROXY_AUTH_MESSAGE));
        assert_eq!(proxy_error_message(&ureq::Error::StatusCode(407)).as_deref(), Some(PROXY_AUTH_MESSAGE));
        assert!(proxy_error_message(&ureq::Error::StatusCode(404)).is_none());
    }

    /// A one-shot fake proxy on 127.0.0.1: returns its port and the request head it received.
    fn fake_proxy(answer: &'static str) -> (u16, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).unwrap() == 0 {
                    break;
                }
                head.push(byte[0]);
            }
            stream.write_all(answer.as_bytes()).unwrap();
            String::from_utf8_lossy(&head).into_owned()
        });
        (port, handle)
    }

    #[test]
    fn manual_proxy_carries_requests_and_basic_credentials() {
        use std::io::{Read, Write};
        fn read_head(stream: &mut std::net::TcpStream) -> String {
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
                head.push(byte[0]);
            }
            String::from_utf8_lossy(&head).into_owned()
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let proxy = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let connect = read_head(&mut stream);
            stream.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").unwrap();
            let inner = read_head(&mut stream);
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").unwrap();
            (connect, inner)
        });
        let mode = OutboundProxy::Manual { url: format!("http://alice:s3cret@127.0.0.1:{port}"), no_proxy: None };
        let agent = mode.apply(ureq::Agent::config_builder()).unwrap().build().new_agent();
        let body = agent.get("http://origin.invalid/file").call().unwrap().body_mut().read_to_string().unwrap();
        let (connect, inner) = proxy.join().unwrap();
        assert_eq!(body, "ok");
        assert!(connect.starts_with("CONNECT origin.invalid:80 HTTP/1.1"), "{connect}");
        // alice:s3cret
        assert!(connect.contains("Proxy-Authorization: Basic YWxpY2U6czNjcmV0"), "{connect}");
        assert!(inner.starts_with("GET /file HTTP/1.1"), "{inner}");
    }

    #[test]
    fn proxy_407_is_a_clear_error() {
        let (port, proxy) = fake_proxy("HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let mode = OutboundProxy::Manual { url: format!("http://127.0.0.1:{port}"), no_proxy: None };
        let agent = mode.apply(ureq::Agent::config_builder()).unwrap().build().new_agent();
        let error = agent.get("https://origin.invalid/file").call().unwrap_err();
        let head = proxy.join().unwrap();
        assert!(head.starts_with("CONNECT origin.invalid:443"), "{head}");
        assert_eq!(proxy_error_message(&error).as_deref(), Some(PROXY_AUTH_MESSAGE));
    }

    #[test]
    fn loopback_never_goes_through_the_proxy() {
        // Origin on loopback answers directly; the "proxy" port is closed.
        let (origin_port, origin) = fake_proxy("HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\ndirect");
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mode = OutboundProxy::Manual { url: format!("http://127.0.0.1:{closed}"), no_proxy: None };
        let agent = mode.apply(ureq::Agent::config_builder()).unwrap().build().new_agent();
        for host in ["127.0.0.1"] {
            let body = agent.get(&format!("http://{host}:{origin_port}/x")).call().unwrap().body_mut().read_to_string().unwrap();
            assert_eq!(body, "direct");
        }
        assert!(origin.join().unwrap().starts_with("GET /x HTTP/1.1"));
    }

    #[test]
    fn redaction_drops_credentials() {
        assert_eq!(redacted_proxy_url("http://u:secret@127.0.0.1:7890"), ("http://127.0.0.1:7890/".into(), true));
        assert_eq!(redacted_proxy_url("http://127.0.0.1:7890").1, false);
        let (url, creds) = redacted_proxy_url("u:secret@host:1");
        assert!(creds && !url.contains("secret"));
    }
}
