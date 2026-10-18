//! Agent bus service address and port helpers.

use anyhow::bail;

use crate::profiles::model::CodezConfig;

pub const DEFAULT_AGENT_BUS_PORT: u16 = 24260;
pub const AGENT_BUS_BRIDGE_ID: &str = "cutex-agent-bus";
/// Environment variable carrying the bus token to a spawned `cutex agent serve`
/// (never `--token`: a command line is visible to every local user).
pub const AGENT_BUS_SERVE_TOKEN_ENV: &str = "CUTEX_AGENT_BUS_SERVE_TOKEN";

/// Where `cutex agent serve` took its bus token from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeTokenSource {
    /// `--token` (deprecated: visible in process listings).
    CommandLine,
    /// `--token-file PATH`.
    File,
    /// [`AGENT_BUS_SERVE_TOKEN_ENV`] (cutex's own spawning).
    Environment,
    /// `agent_bus_token` in config.json (the default; nothing on argv).
    Config,
    /// No token anywhere.
    None,
}

impl ServeTokenSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::CommandLine => "--token (deprecated)",
            Self::File => "--token-file",
            Self::Environment => AGENT_BUS_SERVE_TOKEN_ENV,
            Self::Config => "config.json agent_bus_token",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServeToken {
    pub token: Option<String>,
    pub source: ServeTokenSource,
    /// Operator warnings (never contain the token).
    pub warnings: Vec<String>,
}

pub const DEPRECATED_TOKEN_FLAG_WARNING: &str = "`cutex agent serve --token` is deprecated and will be removed: \
the token is visible to every local user in process listings. Remove `--token …` from the \
service definition; the bus reads `agent_bus_token` from the owner-private config.json \
(or use --token-file / CUTEX_AGENT_BUS_SERVE_TOKEN).";

/// Pick the bus token for `cutex agent serve`, in order: `--token`
/// (deprecated, warns), `--token-file`, the environment variable, config.
/// Clients (agents, management, CLI) always use config.json's token, so a
/// different explicit token is reported as a warning.
pub fn resolve_serve_token(
    cli_token: Option<String>,
    token_file: Option<&std::path::Path>,
    env_token: Option<String>,
    config_token: Option<&str>,
) -> anyhow::Result<ServeToken> {
    let mut warnings = Vec::new();
    let non_empty = |value: Option<String>| {
        value
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    let cli_token = non_empty(cli_token);
    if cli_token.is_some() {
        warnings.push(DEPRECATED_TOKEN_FLAG_WARNING.to_string());
    }
    let (token, source) = if let Some(token) = cli_token {
        (Some(token), ServeTokenSource::CommandLine)
    } else if let Some(path) = token_file {
        let (token, file_warnings) = read_serve_token_file(path)?;
        warnings.extend(file_warnings);
        (Some(token), ServeTokenSource::File)
    } else if let Some(token) = non_empty(env_token) {
        (Some(token), ServeTokenSource::Environment)
    } else if let Some(token) = non_empty(config_token.map(str::to_string)) {
        (Some(token), ServeTokenSource::Config)
    } else {
        (None, ServeTokenSource::None)
    };
    if let (Some(token), Some(config)) = (token.as_deref(), config_token.map(str::trim)) {
        if source != ServeTokenSource::Config && !config.is_empty() && config != token {
            warnings.push(format!(
                "the bus token from {} differs from config.json agent_bus_token; local agents and \
                 cutex clients authenticate with the config.json token",
                source.label()
            ));
        }
    }
    Ok(ServeToken {
        token,
        source,
        warnings,
    })
}

/// Read a token file: a regular file (no symlink), first non-empty line,
/// at least 16 characters. Unix refuses group / other permission bits;
/// Windows warns unless the file has cutex's owner-only protected DACL.
pub fn read_serve_token_file(path: &std::path::Path) -> anyhow::Result<(String, Vec<String>)> {
    use anyhow::Context;
    let mut warnings = Vec::new();
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("agent bus token file {} is not readable", path.display()))?;
    if metadata.file_type().is_symlink() {
        // systemd credentials and Kubernetes-style secrets are symlinks to
        // a private target; validate the target instead.
        let target = std::fs::metadata(path)
            .with_context(|| format!("agent bus token file {} is a dangling link", path.display()))?;
        if !target.is_file() {
            bail!("agent bus token file {} is not a regular file", path.display());
        }
    } else if !metadata.is_file() {
        bail!("agent bus token file {} is not a regular file", path.display());
    }
    let file = std::fs::File::open(path)
        .with_context(|| format!("agent bus token file {} is not readable", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = file.metadata()?.permissions().mode();
        if mode & 0o077 != 0 {
            bail!(
                "agent bus token file {} is readable by group or others (mode {:o}); chmod 600 it",
                path.display(),
                mode & 0o777
            );
        }
    }
    #[cfg(windows)]
    if crate::platform::private_fs::validate_private_file(&file).is_err() {
        warnings.push(format!(
            "agent bus token file {} does not have an owner-only protected ACL; other accounts \
             with access to it can read the token",
            path.display()
        ));
    }
    use std::io::Read;
    let mut text = String::new();
    file.take(64 * 1024)
        .read_to_string(&mut text)
        .with_context(|| format!("agent bus token file {} is not UTF-8 text", path.display()))?;
    let token = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string();
    if token.len() < 16 {
        bail!(
            "agent bus token file {} holds no token (need at least 16 characters)",
            path.display()
        );
    }
    Ok((token, warnings))
}

pub fn validate_agent_bus_port(port: u16) -> anyhow::Result<()> {
    if crate::config::instance::uses_isolated_ports() {
        return crate::config::instance::validate_isolated_port(port, "Agent bus port");
    }
    if !(24000..=24999).contains(&port) {
        bail!("Agent bus port must be in the Bridgeboard 24xxx range");
    }
    Ok(())
}

pub fn agent_bus_port(config: &CodezConfig) -> u16 {
    // An instance never falls back to the default bus (0 fails closed; the
    // instance preflight requires the port). A test process never resolves a
    // default-instance port, not even a stored one.
    if crate::config::instance::default_ports_forbidden() {
        return config
            .agent_bus_port
            .filter(|port| !crate::config::instance::RESERVED_DEFAULT_PORTS.contains(port))
            .unwrap_or(0);
    }
    config.agent_bus_port.unwrap_or(if crate::config::instance::is_isolated() {
        0
    } else {
        DEFAULT_AGENT_BUS_PORT
    })
}

pub fn agent_bus_base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

pub fn agent_bus_health_url(port: u16) -> String {
    format!("{}/", agent_bus_base_url(port))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cutex-serve-token-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_private(path: &std::path::Path, text: &str) {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write;
        options.open(path).unwrap().write_all(text.as_bytes()).unwrap();
    }

    #[test]
    fn config_token_is_the_default_and_needs_no_argv() {
        let resolved = resolve_serve_token(None, None, None, Some("config-token-0123456789")).unwrap();
        assert_eq!(resolved.source, ServeTokenSource::Config);
        assert_eq!(resolved.token.as_deref(), Some("config-token-0123456789"));
        assert!(resolved.warnings.is_empty());
        let none = resolve_serve_token(Some("  ".into()), None, Some(String::new()), None).unwrap();
        assert_eq!(none.source, ServeTokenSource::None);
        assert!(none.token.is_none());
        assert!(none.warnings.is_empty());
    }

    #[test]
    fn environment_beats_config_and_matching_token_is_quiet() {
        let resolved = resolve_serve_token(
            None,
            None,
            Some("same-token-0123456789".into()),
            Some("same-token-0123456789"),
        )
        .unwrap();
        assert_eq!(resolved.source, ServeTokenSource::Environment);
        assert!(resolved.warnings.is_empty());
    }

    #[test]
    fn deprecated_flag_still_works_but_warns_without_leaking_the_token() {
        let secret = "cli-secret-token-0123456789";
        let resolved = resolve_serve_token(
            Some(secret.into()),
            None,
            Some("env-token-0123456789".into()),
            Some("config-token-0123456789"),
        )
        .unwrap();
        assert_eq!(resolved.source, ServeTokenSource::CommandLine);
        assert_eq!(resolved.token.as_deref(), Some(secret));
        assert_eq!(resolved.warnings.len(), 2, "{:?}", resolved.warnings);
        assert!(resolved.warnings[0].contains("deprecated"));
        assert!(resolved.warnings[1].contains("differs from config.json"));
        assert!(resolved.warnings.iter().all(|w| !w.contains(secret)));
    }

    #[test]
    fn token_file_is_read_trimmed_and_validated() {
        let dir = temp_dir("file");
        let path = dir.join("agent-bus-token");
        write_private(&path, "\n  file-token-0123456789  \nignored\n");
        let resolved = resolve_serve_token(
            None,
            Some(&path),
            Some("env-token-0123456789".into()),
            Some("file-token-0123456789"),
        )
        .unwrap();
        assert_eq!(resolved.source, ServeTokenSource::File);
        assert_eq!(resolved.token.as_deref(), Some("file-token-0123456789"));
        #[cfg(unix)]
        assert!(resolved.warnings.is_empty(), "{:?}", resolved.warnings);

        let short = dir.join("short");
        write_private(&short, "abc\n");
        assert!(read_serve_token_file(&short).unwrap_err().to_string().contains("no token"));
        assert!(read_serve_token_file(&dir.join("missing")).is_err());
        assert!(read_serve_token_file(&dir).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let open = dir.join("open");
            write_private(&open, "open-token-0123456789");
            std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o644)).unwrap();
            let error = read_serve_token_file(&open).unwrap_err().to_string();
            assert!(error.contains("chmod 600"), "{error}");
            assert!(!error.contains("open-token"));
            let link = dir.join("link");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert_eq!(read_serve_token_file(&link).unwrap().0, "file-token-0123456789");
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
