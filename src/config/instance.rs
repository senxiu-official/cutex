//! Independent instances (`CUTEX_HOME`): selection of the state directory and
//! the guards an instance runs under. Design: `docs/INSTANCES.md`.
//!
//! The default instance lives in `~/.cutex`. `CUTEX_HOME=<dir>` selects an
//! independent instance whose state directory *is* `<dir>`; it must be
//! initialized (`cutex instance init`) and is always isolated: own ports, own
//! credentials, no Bridgeboard, no federation, no machine-wide service
//! controls, no legacy-layout migration.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};

use crate::profiles::model::CodezConfig;

/// Selects an independent instance (absolute path of its state directory).
pub const HOME_ENV: &str = "CUTEX_HOME";

/// Well-known ports of the default instance (management, bus, their
/// Bridgeboard tunnel ports, desktop notify); an instance never uses them.
pub const RESERVED_DEFAULT_PORTS: [u16; 5] = [24270, 24260, 24670, 24660, 24250];

/// Directories an instance home may not live in (relative to the OS home).
const PROTECTED_USER_DIRS: [&str; 5] = [".cutex", ".codex", ".codez-cli", ".codex-codez", ".claude"];

/// Per-agent identity a daemon started from an agent's shell would inherit.
/// Server commands of an instance drop them at start.
const INHERITED_AGENT_ENV_PREFIXES: [&str; 2] = ["CUTEX_AGENT_", "CUTEX_RUNTIME_"];
const INHERITED_AGENT_ENV_KEEP: [&str; 1] = [crate::agent_bus::service::AGENT_BUS_SERVE_TOKEN_ENV];
const INHERITED_AGENT_ENV_EXACT: [&str; 2] = ["CUTEX_MCP_THREAD_ID", "CUTEX_MANAGEMENT_URL"];

/// The `instance` section of `config.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceSettings {
    pub name: String,
    #[serde(default = "default_isolated")]
    pub isolated: bool,
    /// Allow non-loopback management listeners (tailscale, LAN, wildcard).
    #[serde(default)]
    pub remote_listeners: bool,
}

fn default_isolated() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// `~/.cutex`.
    Default,
    /// An independent instance; the path is its state directory.
    Custom(PathBuf),
}

/// Resolve `CUTEX_HOME` (`custom`) against the OS user home. Pure apart from
/// resolving symlinks of existing ancestors.
pub fn select_home(custom: Option<&OsStr>, user_home: Option<&Path>) -> anyhow::Result<Selection> {
    let Some(raw) = custom.filter(|value| !value.is_empty()) else {
        return Ok(Selection::Default);
    };
    let text = raw
        .to_str()
        .ok_or_else(|| anyhow!("{HOME_ENV} must be valid UTF-8"))?;
    let path = Path::new(text);
    if !path.is_absolute() {
        bail!("{HOME_ENV} must be an absolute path: {text}");
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => bail!("{HOME_ENV} must not contain `..`: {text}"),
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    if normalized.parent().is_none() {
        bail!("{HOME_ENV} must not be a filesystem root: {text}");
    }
    let Some(user_home) = user_home else {
        bail!("{HOME_ENV} needs a known user home to check it against the default instance");
    };
    let candidate = comparable(&normalized);
    let default_home = comparable(&user_home.join(".cutex"));
    if candidate == default_home {
        return Ok(Selection::Default);
    }
    if is_within(&candidate, &default_home) {
        bail!("{HOME_ENV} must not be inside the default home {}", user_home.join(".cutex").display());
    }
    if is_within(&default_home, &candidate) {
        bail!("{HOME_ENV} must not contain the default home {}", user_home.join(".cutex").display());
    }
    for protected in PROTECTED_USER_DIRS {
        let protected_path = comparable(&user_home.join(protected));
        if candidate == protected_path || is_within(&candidate, &protected_path) {
            bail!("{HOME_ENV} must not be inside {}", user_home.join(protected).display());
        }
    }
    Ok(Selection::Custom(normalized))
}

/// Path components after resolving the deepest existing ancestor (so an
/// alias of a protected directory compares equal to it); case-folded on
/// Windows.
pub(crate) fn comparable(path: &Path) -> Vec<String> {
    let mut existing = path.to_path_buf();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let full = loop {
        if let Ok(real) = std::fs::canonicalize(&existing) {
            let mut full = strip_verbatim(real);
            for name in rest.iter().rev() {
                full.push(name);
            }
            break full;
        }
        match (existing.file_name().map(|n| n.to_os_string()), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => break path.to_path_buf(),
        }
    };
    full.components()
        .filter(|c| !matches!(c, Component::CurDir))
        .map(|c| {
            let text = c.as_os_str().to_string_lossy().to_string();
            if cfg!(windows) {
                text.replace('/', "\\").to_lowercase()
            } else {
                text
            }
        })
        .collect()
}

fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path
}

pub(crate) fn is_within(path: &[String], ancestor: &[String]) -> bool {
    path.len() > ancestor.len() && path.starts_with(ancestor)
}

#[cfg(not(test))]
fn selected_uncached() -> anyhow::Result<Selection> {
    let custom = std::env::var_os(HOME_ENV).filter(|value| !value.is_empty());
    if custom.is_some() && std::env::var_os("CUTEX_TEST_PRIVATE_HOME").is_some() {
        bail!("{HOME_ENV} cannot be combined with CUTEX_TEST_PRIVATE_HOME");
    }
    let user_home = crate::config::paths::home_dir().or_else(dirs::home_dir);
    select_home(custom.as_deref(), user_home.as_deref())
}

/// The instance this process belongs to (from `CUTEX_HOME`). Successful
/// results are cached per raw value.
#[cfg(not(test))]
pub fn selected() -> anyhow::Result<Selection> {
    use std::sync::Mutex;
    static CACHE: Mutex<Option<(Option<std::ffi::OsString>, Selection)>> = Mutex::new(None);
    let raw = std::env::var_os(HOME_ENV).filter(|value| !value.is_empty());
    if let Ok(cache) = CACHE.lock() {
        if let Some((key, selection)) = cache.as_ref() {
            if *key == raw {
                return Ok(selection.clone());
            }
        }
    }
    let selection = selected_uncached()?;
    if let Ok(mut cache) = CACHE.lock() {
        *cache = Some((raw, selection.clone()));
    }
    Ok(selection)
}

#[cfg(test)]
thread_local! {
    static TEST_SELECTION: std::cell::RefCell<Selection> = const { std::cell::RefCell::new(Selection::Default) };
}

/// Unit tests never read `CUTEX_HOME`; they set the selection per thread.
#[cfg(test)]
pub fn selected() -> anyhow::Result<Selection> {
    Ok(TEST_SELECTION.with(|s| s.borrow().clone()))
}

#[cfg(test)]
pub fn set_test_selection(selection: Selection) {
    TEST_SELECTION.with(|s| *s.borrow_mut() = selection);
}

/// The state directory of an independent instance, `None` for the default.
pub fn custom_home() -> anyhow::Result<Option<PathBuf>> {
    Ok(match selected()? {
        Selection::Default => None,
        Selection::Custom(path) => Some(path),
    })
}

/// True in an independent instance (and, fail closed, when `CUTEX_HOME` is
/// set but invalid: every path lookup fails there anyway).
pub fn is_isolated() -> bool {
    !matches!(selected(), Ok(Selection::Default))
}

pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 32
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// Port rule of an instance: 1024–65535 and never a default-instance port.
pub fn validate_isolated_port(port: u16, what: &str) -> anyhow::Result<()> {
    if port < 1024 {
        bail!("{what} {port}: an instance port must be in 1024-65535");
    }
    if RESERVED_DEFAULT_PORTS.contains(&port) {
        bail!("{what} {port} belongs to the default cutex instance; pick another port");
    }
    Ok(())
}

/// Validate a config against the instance rules (`custom`: running with
/// `CUTEX_HOME`). See `docs/INSTANCES.md` §3.
pub fn validate_config(custom: bool, config: &CodezConfig) -> anyhow::Result<()> {
    if !custom {
        if config.instance.as_ref().is_some_and(|instance| instance.isolated) {
            bail!(
                "the default home has an isolated `instance` section; run this home with {HOME_ENV}=<its path> or remove the section"
            );
        }
        return Ok(());
    }
    let instance = config.instance.as_ref().ok_or_else(|| {
        anyhow!("this {HOME_ENV} is not an initialized cutex instance; run `cutex instance init --name <name> --management-port <port> --agent-bus-port <port>`")
    })?;
    if !instance.isolated {
        bail!("instance.isolated = false is not supported; independent instances are isolated");
    }
    if !valid_name(&instance.name) {
        bail!("instance.name must match [a-z0-9][a-z0-9-]{{0,31}}");
    }
    let management = config
        .management_port
        .ok_or_else(|| anyhow!("instance config needs management_port"))?;
    let bus = config
        .agent_bus_port
        .ok_or_else(|| anyhow!("instance config needs agent_bus_port"))?;
    validate_isolated_port(management, "management_port")?;
    validate_isolated_port(bus, "agent_bus_port")?;
    if management == bus {
        bail!("management_port and agent_bus_port must differ");
    }
    let root = config
        .management_api_token
        .as_ref()
        .map(|token| token.as_str().trim().to_string())
        .filter(|token| !token.is_empty())
        .ok_or_else(|| anyhow!("instance config needs management_api_token (run `cutex instance init`)"))?;
    let bus_token = config
        .agent_bus_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| anyhow!("instance config needs agent_bus_token (run `cutex instance init`)"))?;
    if root == bus_token {
        bail!("management_api_token and agent_bus_token must differ");
    }
    if let Some(entries) = config.management_listen.as_ref().filter(|e| !e.is_empty()) {
        validate_listen_ports(entries, management, Some(bus), instance.remote_listeners)
            .map_err(|message| anyhow!(message))?;
    }
    Ok(())
}

/// Instance rules for `management_listen`: loopback only unless
/// `remote_listeners`, no default-instance port, not the bus port.
pub fn validate_listen_ports(
    entries: &[String],
    default_port: u16,
    bus_port: Option<u16>,
    remote_listeners: bool,
) -> Result<(), String> {
    use crate::management::listeners::ListenSpec;
    for entry in entries {
        let spec = ListenSpec::parse(entry, default_port)
            .map_err(|error| format!("managementListen: {error:#}"))?;
        if !remote_listeners && !spec.is_loopback() {
            return Err(format!(
                "managementListen: {entry}: an instance listens on loopback only (set instance.remote_listeners = true to allow it)"
            ));
        }
        validate_isolated_port(spec.port(), "managementListen port").map_err(|e| format!("{e:#}"))?;
        if Some(spec.port()) == bus_port {
            return Err(format!("managementListen: {entry}: port {} is the instance's Agent Bus port", spec.port()));
        }
    }
    Ok(())
}

/// Listen-entry check for the running process (no-op in the default instance).
pub fn check_listen_entries(entries: &[String], default_port: u16) -> Result<(), String> {
    if !is_isolated() {
        return Ok(());
    }
    let config = crate::config::store::load_codez_config();
    let remote = config.instance.as_ref().is_some_and(|i| i.remote_listeners);
    validate_listen_ports(entries, default_port, config.agent_bus_port, remote)
}

/// The instance home must be private to the current user.
pub fn verify_private_home(home: &Path) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(home)
        .with_context(|| format!("instance home {} is missing; run `cutex instance init`", home.display()))?;
    if !metadata.is_dir() {
        bail!("instance home {} is not a directory", home.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        if metadata.uid() != uid {
            bail!("instance home {} is not owned by the current user", home.display());
        }
        if metadata.mode() & 0o077 != 0 {
            bail!(
                "instance home {} is accessible by group/others (mode {:o}); chmod 700 it",
                home.display(),
                metadata.mode() & 0o777
            );
        }
    }
    #[cfg(windows)]
    crate::platform::private_fs::open_validated_directory(home).map_err(|error| {
        anyhow!(
            "instance home {} is not private to the current user ({error:?}); run `cutex instance init` to secure it",
            home.display()
        )
    })?;
    Ok(())
}

/// Instance guard run before every command (except `cutex instance init|show`).
pub fn preflight() -> anyhow::Result<()> {
    let custom = custom_home()?;
    match custom {
        None => {
            let config = crate::config::store::load_codez_config();
            validate_config(false, &config)
        }
        Some(home) => {
            verify_private_home(&home)?;
            let config = crate::config::store::load_codez_config_checked()?;
            validate_config(true, &config)?;
            management_url_override(&config)?;
            Ok(())
        }
    }
}

/// In an instance, refuse a path outside its home (`what` names the value).
pub fn require_inside_home(path: &Path, what: &str) -> anyhow::Result<()> {
    let Some(home) = custom_home()? else {
        return Ok(());
    };
    require_inside(path, &home, what)
}

pub fn require_inside(path: &Path, home: &Path, what: &str) -> anyhow::Result<()> {
    let target = comparable(path);
    let home_c = comparable(home);
    if !(target == home_c || is_within(&target, &home_c)) {
        bail!(
            "{what} {} is outside this cutex instance ({}); an instance keeps its state inside its home",
            path.display(),
            home.display()
        );
    }
    Ok(())
}

/// Refuse an operation that acts on machine-wide state of the default
/// instance (Bridgeboard tunnels, service units).
pub fn refuse_in_instance(what: &str) -> anyhow::Result<()> {
    if is_isolated() {
        bail!("{what} is not available in an independent cutex instance ({HOME_ENV} is set)");
    }
    Ok(())
}

/// `CUTEX_MANAGEMENT_URL` as the CLI may use it. In an instance it must be a
/// loopback URL on the instance's management port.
pub fn management_url_override(config: &CodezConfig) -> anyhow::Result<Option<String>> {
    let Some(raw) = std::env::var("CUTEX_MANAGEMENT_URL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    if !is_isolated() {
        return Ok(Some(raw));
    }
    check_instance_management_url(&raw, crate::management::service::management_port(config))?;
    Ok(Some(raw))
}

pub fn check_instance_management_url(raw: &str, port: u16) -> anyhow::Result<()> {
    let url = url::Url::parse(raw).with_context(|| format!("CUTEX_MANAGEMENT_URL {raw} is not a URL"))?;
    let loopback = match url.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    if url.scheme() != "http" || !loopback || url.port_or_known_default() != Some(port) {
        bail!(
            "CUTEX_MANAGEMENT_URL {raw} does not point at this instance (http://127.0.0.1:{port}); unset it or fix it"
        );
    }
    Ok(())
}

fn inherited_agent_env(key: &str) -> bool {
    if INHERITED_AGENT_ENV_KEEP.contains(&key) {
        return false;
    }
    INHERITED_AGENT_ENV_EXACT.contains(&key)
        || INHERITED_AGENT_ENV_PREFIXES.iter().any(|prefix| key.starts_with(prefix))
}

/// Server commands of an instance drop per-agent identity inherited from the
/// shell that started them (typically an agent of the main instance). Call
/// before any thread starts.
pub fn scrub_inherited_agent_env() {
    if !is_isolated() {
        return;
    }
    let keys: Vec<String> = std::env::vars_os()
        .filter_map(|(key, _)| key.into_string().ok())
        .filter(|key| inherited_agent_env(key))
        .collect();
    for key in keys {
        std::env::remove_var(key);
    }
}

/// `(CUTEX_HOME, value)` to re-add to a child launched with a scrubbed
/// environment, so a nested `cutex` stays in this instance.
pub fn child_env() -> Option<(&'static str, PathBuf)> {
    custom_home().ok().flatten().map(|home| (HOME_ENV, home))
}

/// Name of the running instance, `None` for the default.
pub fn current_name() -> Option<String> {
    if !is_isolated() {
        return None;
    }
    crate::config::store::load_codez_config().instance.map(|i| i.name)
}

/// A fresh random secret (64 hex chars, SHA-256 over three v4 UUIDs).
pub fn generate_secret(label: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(b"cutex-instance-secret\0");
    digest.update(label.as_bytes());
    for _ in 0..3 {
        digest.update(uuid::Uuid::new_v4().as_bytes());
    }
    digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Options of `cutex instance init`.
#[derive(Debug, Clone)]
pub struct InitOptions {
    pub name: String,
    pub management_port: u16,
    pub agent_bus_port: u16,
    pub remote_listeners: bool,
}

/// Apply `cutex instance init` to an existing (possibly default) config:
/// add the `instance` section and ports, generate missing tokens. Refuses a
/// conflicting existing section. Pure; the caller persists the result.
pub fn init_config(mut config: CodezConfig, options: &InitOptions) -> anyhow::Result<CodezConfig> {
    if !valid_name(&options.name) {
        bail!("--name must match [a-z0-9][a-z0-9-]{{0,31}}");
    }
    let wanted = InstanceSettings {
        name: options.name.clone(),
        isolated: true,
        remote_listeners: options.remote_listeners,
    };
    if let Some(existing) = config.instance.as_ref() {
        if *existing != wanted {
            bail!("this home already is instance {:?} with other settings; edit config.json to change it", existing.name);
        }
    }
    for (configured, wanted_port, what) in [
        (config.management_port, options.management_port, "management_port"),
        (config.agent_bus_port, options.agent_bus_port, "agent_bus_port"),
    ] {
        if let Some(configured) = configured {
            if configured != wanted_port && config.instance.is_some() {
                bail!("{what} is already {configured}; edit config.json to change it");
            }
        }
    }
    config.instance = Some(wanted);
    config.management_port = Some(options.management_port);
    config.agent_bus_port = Some(options.agent_bus_port);
    if config
        .management_api_token
        .as_ref()
        .is_none_or(|token| token.as_str().trim().is_empty())
    {
        config.management_api_token =
            Some(crate::profiles::model::ManagementApiToken::new(generate_secret("management")));
    }
    if config
        .agent_bus_token
        .as_deref()
        .is_none_or(|token| token.trim().is_empty())
    {
        config.agent_bus_token = Some(format!("cutex-agent-{}", generate_secret("agent-bus")));
    }
    if config.management_listen.as_ref().is_none_or(|e| e.is_empty()) {
        config.management_listen = Some(vec!["127.0.0.1".to_string()]);
    }
    validate_config(true, &config)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::model::ManagementApiToken;

    fn temp_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!("cutex-instance-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn os(path: &Path) -> &OsStr {
        path.as_os_str()
    }

    #[test]
    fn unset_or_empty_selects_default() {
        let user = temp_root();
        assert_eq!(select_home(None, Some(&user)).unwrap(), Selection::Default);
        assert_eq!(select_home(Some(OsStr::new("")), Some(&user)).unwrap(), Selection::Default);
    }

    #[test]
    fn absolute_dir_selects_custom_and_normalizes() {
        let user = temp_root();
        let home = user.join("work").join(".").join("kutex");
        let selection = select_home(Some(os(&home)), Some(&user)).unwrap();
        assert_eq!(selection, Selection::Custom(user.join("work").join("kutex")));
    }

    #[test]
    fn default_home_itself_selects_default() {
        let user = temp_root();
        let home = user.join(".cutex");
        assert_eq!(select_home(Some(os(&home)), Some(&user)).unwrap(), Selection::Default);
    }

    #[test]
    fn relative_parent_and_root_paths_are_refused() {
        let user = temp_root();
        assert!(select_home(Some(OsStr::new("relative/dir")), Some(&user)).is_err());
        let dotted = user.join("a").join("..").join("b");
        assert!(select_home(Some(os(&dotted)), Some(&user)).is_err());
        let root: PathBuf = user.ancestors().last().unwrap().to_path_buf();
        assert!(select_home(Some(os(&root)), Some(&user)).is_err());
    }

    #[test]
    fn protected_directories_are_refused() {
        let user = temp_root();
        for dir in [".cutex", ".codex", ".codez-cli", ".codex-codez", ".claude"] {
            let inside = user.join(dir).join("instance");
            assert!(select_home(Some(os(&inside)), Some(&user)).is_err(), "{dir}");
        }
        // Containing the default home is refused too.
        assert!(select_home(Some(os(&user)), Some(&user)).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn protected_directories_compare_case_insensitively_on_windows() {
        let user = temp_root();
        let inside = user.join(".CUTEX").join("x");
        assert!(select_home(Some(os(&inside)), Some(&user)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_alias_of_the_default_home_is_refused() {
        let user = temp_root();
        std::fs::create_dir_all(user.join(".cutex")).unwrap();
        let alias = user.join("alias");
        std::os::unix::fs::symlink(user.join(".cutex"), &alias).unwrap();
        assert!(select_home(Some(os(&alias.join("inner"))), Some(&user)).is_err());
        // The alias itself resolves to the default home.
        assert_eq!(select_home(Some(os(&alias)), Some(&user)).unwrap(), Selection::Default);
    }

    #[test]
    fn unknown_user_home_is_refused_for_custom() {
        let user = temp_root();
        assert!(select_home(Some(os(&user.join("x"))), None).is_err());
    }

    fn instance_config() -> CodezConfig {
        init_config(
            CodezConfig::default(),
            &InitOptions {
                name: "kutex".into(),
                management_port: 25270,
                agent_bus_port: 25260,
                remote_listeners: false,
            },
        )
        .unwrap()
    }

    #[test]
    fn init_generates_distinct_tokens_and_loopback_listen() {
        let config = instance_config();
        let root = config.management_api_token.as_ref().unwrap().as_str().to_string();
        let bus = config.agent_bus_token.clone().unwrap();
        assert_eq!(root.len(), 64);
        assert_ne!(root, bus);
        assert_eq!(config.management_listen, Some(vec!["127.0.0.1".to_string()]));
        assert_eq!(config.instance.as_ref().unwrap().name, "kutex");
        validate_config(true, &config).unwrap();
        // Re-running init with the same options is idempotent and keeps tokens.
        let again = init_config(
            config.clone(),
            &InitOptions { name: "kutex".into(), management_port: 25270, agent_bus_port: 25260, remote_listeners: false },
        )
        .unwrap();
        assert_eq!(again.management_api_token.unwrap().as_str(), root);
        assert_eq!(again.agent_bus_token.unwrap(), bus);
    }

    #[test]
    fn init_adopts_existing_tokens_and_refuses_conflicts() {
        let existing = CodezConfig {
            management_api_token: Some(ManagementApiToken::new("existing-root")),
            agent_bus_token: Some("existing-bus".into()),
            ..CodezConfig::default()
        };
        let opts = InitOptions { name: "scratch".into(), management_port: 25370, agent_bus_port: 25360, remote_listeners: false };
        let adopted = init_config(existing, &opts).unwrap();
        assert_eq!(adopted.management_api_token.as_ref().unwrap().as_str(), "existing-root");
        let other = InitOptions { name: "other".into(), ..opts.clone() };
        assert!(init_config(adopted.clone(), &other).is_err());
        let moved = InitOptions { management_port: 25371, ..opts };
        assert!(init_config(adopted, &moved).is_err());
    }

    #[test]
    fn init_refuses_bad_names_and_reserved_or_equal_ports() {
        let base = InitOptions { name: "ok".into(), management_port: 25270, agent_bus_port: 25260, remote_listeners: false };
        for bad in ["", "Kutex", "-x", "a_b", &"x".repeat(33)] {
            assert!(init_config(CodezConfig::default(), &InitOptions { name: bad.into(), ..base.clone() }).is_err(), "{bad}");
        }
        for port in RESERVED_DEFAULT_PORTS {
            assert!(init_config(CodezConfig::default(), &InitOptions { management_port: port, ..base.clone() }).is_err());
            assert!(init_config(CodezConfig::default(), &InitOptions { agent_bus_port: port, ..base.clone() }).is_err());
        }
        assert!(init_config(CodezConfig::default(), &InitOptions { management_port: 80, ..base.clone() }).is_err());
        assert!(init_config(CodezConfig::default(), &InitOptions { agent_bus_port: 25270, ..base }).is_err());
    }

    #[test]
    fn instance_config_requires_section_ports_and_tokens() {
        assert!(validate_config(true, &CodezConfig::default()).is_err());
        let good = instance_config();
        let mut missing_port = good.clone();
        missing_port.management_port = None;
        assert!(validate_config(true, &missing_port).is_err());
        let mut missing_bus = good.clone();
        missing_bus.agent_bus_port = None;
        assert!(validate_config(true, &missing_bus).is_err());
        let mut no_token = good.clone();
        no_token.management_api_token = None;
        assert!(validate_config(true, &no_token).is_err());
        let mut same_tokens = good.clone();
        same_tokens.agent_bus_token = Some(good.management_api_token.as_ref().unwrap().as_str().to_string());
        assert!(validate_config(true, &same_tokens).is_err());
        let mut not_isolated = good.clone();
        not_isolated.instance.as_mut().unwrap().isolated = false;
        assert!(validate_config(true, &not_isolated).is_err());
    }

    #[test]
    fn default_home_refuses_an_isolated_section() {
        validate_config(false, &CodezConfig::default()).unwrap();
        assert!(validate_config(false, &instance_config()).is_err());
    }

    #[test]
    fn instance_listeners_are_loopback_only_unless_allowed() {
        let loopback = vec!["127.0.0.1".to_string(), "[::1]:25271".to_string()];
        validate_listen_ports(&loopback, 25270, Some(25260), false).unwrap();
        for entry in ["tailscale", "0.0.0.0", "10.0.0.5"] {
            assert!(validate_listen_ports(&[entry.to_string()], 25270, Some(25260), false).is_err(), "{entry}");
            validate_listen_ports(&[entry.to_string()], 25270, Some(25260), true).unwrap();
        }
        assert!(validate_listen_ports(&["127.0.0.1:24270".into()], 25270, Some(25260), false).is_err());
        assert!(validate_listen_ports(&["127.0.0.1:25260".into()], 25270, Some(25260), false).is_err());
        let mut config = instance_config();
        config.management_listen = Some(vec!["tailscale".into()]);
        assert!(validate_config(true, &config).is_err());
        config.instance.as_mut().unwrap().remote_listeners = true;
        validate_config(true, &config).unwrap();
    }

    #[test]
    fn management_url_must_be_loopback_on_the_instance_port() {
        check_instance_management_url("http://127.0.0.1:25270", 25270).unwrap();
        check_instance_management_url("http://localhost:25270/", 25270).unwrap();
        check_instance_management_url("http://[::1]:25270", 25270).unwrap();
        assert!(check_instance_management_url("http://127.0.0.1:24270", 25270).is_err());
        assert!(check_instance_management_url("http://100.64.0.1:25270", 25270).is_err());
        assert!(check_instance_management_url("https://127.0.0.1:25270", 25270).is_err());
        assert!(check_instance_management_url("not a url", 25270).is_err());
    }

    #[test]
    fn inherited_agent_identity_is_recognized_but_the_serve_token_is_kept() {
        assert!(inherited_agent_env("CUTEX_AGENT_ID"));
        assert!(inherited_agent_env("CUTEX_AGENT_BUS_URL"));
        assert!(inherited_agent_env("CUTEX_RUNTIME_LAUNCH_ID"));
        assert!(inherited_agent_env("CUTEX_MCP_THREAD_ID"));
        assert!(inherited_agent_env("CUTEX_MANAGEMENT_URL"));
        assert!(!inherited_agent_env("CUTEX_AGENT_BUS_SERVE_TOKEN"));
        assert!(!inherited_agent_env("CUTEX_HOME"));
    }

    #[test]
    fn require_inside_accepts_home_and_descendants_only() {
        let root = temp_root();
        let home = root.join("inst");
        require_inside(&home, &home, "x").unwrap();
        require_inside(&home.join("codex-home"), &home, "x").unwrap();
        assert!(require_inside(&root.join("other"), &home, "x").is_err());
        assert!(require_inside(&root.join("inst-2"), &home, "x").is_err());
    }

    #[test]
    fn test_selection_is_per_thread_and_drives_is_isolated() {
        assert!(!is_isolated());
        let root = temp_root();
        set_test_selection(Selection::Custom(root.clone()));
        assert!(is_isolated());
        assert_eq!(custom_home().unwrap(), Some(root));
        assert!(refuse_in_instance("x").is_err());
        set_test_selection(Selection::Default);
        assert!(!is_isolated());
    }

    #[test]
    fn instance_mode_turns_off_machine_wide_side_effects_and_default_fallbacks() {
        use crate::config::global_settings::{apply_global_config_patch, ConfigValueUpdate, GlobalConfigPatch};
        use crate::launch::local_deployment::LocalDeployment;
        let root = temp_root();
        set_test_selection(Selection::Custom(root.clone()));

        // State directory and legacy layouts stay inside the instance.
        assert_eq!(crate::config::paths::config_dir().unwrap(), root);
        assert!(crate::config::paths::legacy_config_dir().unwrap().starts_with(&root));
        assert!(crate::config::paths::legacy_host_codex_home_dir().unwrap().starts_with(&root));
        crate::config::paths::migrate_legacy_runtime_layout().unwrap();
        assert!(!root.join("codex-home").exists());

        // No federation, no Bridgeboard tunnels, no service controls.
        assert!(crate::management::remote::ensure_management_remote_tunnel("peer", "", 25370, 25270, None)
            .unwrap_err()
            .to_string()
            .contains("independent cutex instance"));

        // Ports: never a default-instance port, never a silent default fallback.
        assert!(crate::agent_bus::service::validate_agent_bus_port(24260).is_err());
        crate::agent_bus::service::validate_agent_bus_port(25260).unwrap();
        assert!(crate::management::service::validate_management_port(24270).is_err());
        crate::management::service::validate_management_port(25270).unwrap();
        assert_eq!(crate::agent_bus::service::agent_bus_port(&CodezConfig::default()), 0);
        assert_eq!(crate::management::service::management_port(&CodezConfig::default()), 0);
        let mut config = instance_config();
        assert_eq!(crate::management::service::management_port(&config), 25270);
        let clear = GlobalConfigPatch { agent_bus_port: ConfigValueUpdate::Clear, ..GlobalConfigPatch::default() };
        assert!(apply_global_config_patch(&mut config, &clear).is_err());
        let onto_management = GlobalConfigPatch { agent_bus_port: ConfigValueUpdate::Set(25270), ..GlobalConfigPatch::default() };
        assert!(apply_global_config_patch(&mut config, &onto_management).is_err());
        let moved = GlobalConfigPatch { agent_bus_port: ConfigValueUpdate::Set(25261), ..GlobalConfigPatch::default() };
        apply_global_config_patch(&mut config, &moved).unwrap();
        assert_eq!(config.agent_bus_port, Some(25261));

        // Selected runtime: native home inside the instance.
        let outside = LocalDeployment { native_home: temp_root(), bundle_manifest: root.join("b.json"), job_mcp: None };
        assert!(outside.check_instance_paths().is_err());
        let inside = LocalDeployment { native_home: root.join("codex-home"), bundle_manifest: temp_root().join("b.json"), job_mcp: None };
        inside.check_instance_paths().unwrap();

        set_test_selection(Selection::Default);
        // The default instance is unchanged.
        assert_eq!(crate::agent_bus::service::agent_bus_port(&CodezConfig::default()), 24260);
        assert_eq!(crate::management::service::management_port(&CodezConfig::default()), 24270);
        crate::agent_bus::service::validate_agent_bus_port(24260).unwrap();
        assert!(crate::agent_bus::service::validate_agent_bus_port(25260).is_err());
        outside.check_instance_paths().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn private_home_check_requires_mode_700() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(verify_private_home(&root).is_err());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        verify_private_home(&root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn private_home_check_requires_a_secured_dacl() {
        let root = temp_root();
        let home = root.join("inst");
        std::fs::create_dir_all(&home).unwrap();
        // A plain temp directory inherits non-private ACEs.
        let before = verify_private_home(&home);
        crate::management::client_tokens::ensure_private_dir(&home).unwrap();
        verify_private_home(&home).unwrap();
        let _ = before;
    }
}
