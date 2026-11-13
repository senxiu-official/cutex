//! Central credential-location inventory for narrowed Management tokens.
//! See docs/credential-reads.md. Paths describe locations, never file contents.
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum IdentityScan { Full, ToolHome, AccountHomes, Runtime }
fn scan_policy(name: &str) -> IdentityScan {
    match name {
        ".claude" | ".cutex-claude-compatible" | "claude-home" | ".gemini" => IdentityScan::ToolHome,
        "profiles" | "deleted-profiles" | "claude-accounts" | "kimi-accounts" | "runtime/login" => IdentityScan::AccountHomes,
        _ => IdentityScan::Full,
    }
}
pub(super) const TOOL_CREDENTIAL_FILES: &[&str] = &[
    "auth.json", ".credentials.json", "api_key", "rpc-token", "capability-token", "launch.json", "credentials.json", "profile.json", "cutex-shared-config.json", ".claude.json", "settings.json", "settings.local.json",
    "config.json", "config.toml", "accounts.json", "accounts.v2.backup.json", "oauth_creds.json", "token", "token.json", "tokens.json",
    crate::runtime::claude_accounts::BOUND_API_KEY_FILE,
    crate::runtime::claude_accounts::API_ACCOUNT_CONFIG_FILE,
];
pub(super) const TOOL_CREDENTIAL_DIRS: &[&str] = &["credentials", "oauth"];
pub(super) const NESTED_TOOL_HOMES: &[&str] = &[".claude", "claude", ".codex", "codex", ".codex-codez", ".kimi", "kimi", ".gemini", "home", "userhome"];
#[derive(Clone)]
pub(super) struct Location {
    pub path: PathBuf,
    pub tree: bool,
    pub scan: IdentityScan,
}

pub(super) const HOME_TREES: &[&str] = &[
    ".ssh",
    ".gemini",
    ".gnupg",
    ".password-store",
    ".aws",
    ".azure",
    ".config/gcloud",
    ".kube",
    ".local/share/keyrings",
    ".local/share/kwalletd",
    ".local/share/kwalletd5",
    ".kde/share/apps/kwallet",
    ".kde4/share/apps/kwallet",
    ".pki/nssdb",
    ".claude",
    ".cutex-claude-compatible",
    ".kimi/credentials",
    ".kimi/oauth",
    ".config/google-chrome",
    ".config/chromium",
    ".config/microsoft-edge",
    ".config/BraveSoftware/Brave-Browser",
    ".mozilla/firefox",
    "AppData/Local/Google/Chrome/User Data",
    "AppData/Local/Chromium/User Data",
    "AppData/Local/Microsoft/Edge/User Data",
    "AppData/Local/BraveSoftware/Brave-Browser/User Data",
    "AppData/Roaming/Mozilla/Firefox/Profiles",
    "AppData/Local/Microsoft/Credentials",
    "AppData/Roaming/Microsoft/Credentials",
    "AppData/Local/Microsoft/Vault",
    "AppData/Roaming/Microsoft/Vault",
    "AppData/Roaming/Microsoft/Protect",
    "AppData/Roaming/Microsoft/Crypto",
];
pub(super) const HOME_FILES: &[&str] = &[
    ".config/gh/hosts.yml",
    "AppData/Roaming/GitHub CLI/hosts.yml",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".config/git/credentials",
    ".config/rclone/rclone.conf",
    "AppData/Local/runtime-host/token",
    ".netrc",
    "_netrc",
    ".git-credentials",
    ".pypirc",
    ".npmrc",
    ".pgpass",
    ".docker/config.json",
    ".codex/auth.json",
    ".codex/.credentials.json",
    ".codex/config.toml",
    ".codex-codez/auth.json",
    ".codex-codez/.credentials.json",
    ".codex-codez/config.toml",
    ".claude.json",
    ".codez-cli/config.json",
    ".codez-cli/accounts.json",
    ".codez-cli/accounts.v2.backup.json",
];
pub(super) const CUTEX_TREES: &[&str] = &[
    crate::runtime::claude_accounts::HOMES_DIR,
    crate::runtime::claude_accounts::KEYS_DIR,
    crate::profiles::kimi_accounts::HOMES_DIR,
    crate::profiles::stored_keys::KEYS_DIR,
    "management-clients",
    "archive/claude-tool-defaults",
    "archive/profile-generic-switches",
    "profiles",
    "deleted-profiles",
    "claude-home",
    "runtime/login",
    "runtime/profile-link",
    "management-relay",
    "pan",
];
pub(super) const CUTEX_FILES: &[&str] = &[
    "config.json",
    "scoped-mcp.json",
    "notifications/outbound.json",
    "services/registry.json",
    "services/registry.json.pre-services-proxy",
    "accounts.json",
    "accounts.v2.backup.json",
    "codex-home/auth.json",
    "codex-home/.credentials.json",
    "codex-home/config.toml",
    "runtime/docker-home/auth.json",
    "runtime/thirdparty/userhome/auth.json",
];
const DESKTOP_TREES: &[&str] = &[
    "Partitions",
    "Network",
    "Local Storage",
    "Session Storage",
    "IndexedDB",
];
const DESKTOP_FILES: &[&str] = &[
    "host-tokens.json",
    "profile-link/secret-v1.json",
    "Cookies",
    "Cookies-journal",
    "Login Data",
    "Web Data",
    "Local State",
];

fn add(out: &mut Vec<Location>, root: &Path, trees: &[&str], files: &[&str]) {
    out.extend(trees.iter().map(|name| Location {
        path: root.join(name),
        tree: true,
        scan: scan_policy(name),
    }));
    out.extend(files.iter().map(|name| Location {
        path: root.join(name),
        tree: false,
        scan: IdentityScan::Full,
    }));
}
pub(super) fn for_roots(homes: &[PathBuf], cutex: &Path) -> Vec<Location> {
    let mut out = Vec::new();
    for home in homes {
        add(&mut out, home, HOME_TREES, HOME_FILES);
        for app in [
            ".config/Cutex",
            ".config/cutex-desktop",
            "AppData/Roaming/Cutex",
            "AppData/Roaming/cutex-desktop",
        ] {
            add(&mut out, &home.join(app), DESKTOP_TREES, DESKTOP_FILES);
        }
    }
    add(&mut out, cutex, CUTEX_TREES, CUTEX_FILES);
    out.push(Location { path: cutex.join("runtime"), tree: false, scan: IdentityScan::Runtime });
    out
}
fn add_windows_program_data(out: &mut Vec<Location>, root: &Path) {
    add(out, Path::new("C:/ProgramData"), &["ssh", "Tailscale"], &[]);
    if key(root) != key(Path::new("C:/ProgramData")) {
        add(out, root, &["ssh", "Tailscale"], &[]);
    }
}
/// Cache/storage subtrees contain no credential files and can dwarf the
/// credential inventory in real browser profiles. This only prunes identity
/// enumeration; the named credential namespaces keep their read policy.
pub(super) fn skip_identity_subtree(path: &Path) -> bool {
    let parts = key(path).into_iter().map(|part| part.to_lowercase()).collect::<Vec<_>>();
    let browser = parts.iter().any(|part| matches!(part.as_str(),
        "google-chrome" | "chromium" | "google" | "edge" | "microsoft-edge" | "brave-browser" | "firefox" | "cutex" | "cutex-desktop"));
    browser && parts.iter().any(|part| matches!(part.as_str(),
        "cache" | "code cache" | "gpucache" | "cachestorage" | "shadercache" | "grshadercache" |
        "dawncache" | "dawngraphitecache" | "dawnwebgpucache" |
        "crashpad" | "crash reports" | "media cache" | "cache2" | "startupcache" | "shader-cache"))
}

pub(super) fn locations() -> Vec<Location> {
    let external = !cfg!(test) && !crate::config::paths::real_home_forbidden();
    let mut homes = crate::config::paths::home_dir()
        .into_iter()
        .collect::<Vec<_>>();
    if external {
        if let Some(real) = crate::config::paths::real_user_home() {
            if !homes.contains(&real) {
                homes.push(real);
            }
        }
    }
    let mut out = for_roots(
        &homes,
        &crate::config::paths::config_dir().unwrap_or_default(),
    );
    out.extend(super::launch_credentials::paths(&crate::config::paths::config_dir().unwrap_or_default()).into_iter().map(|path| Location { path, tree: false, scan: IdentityScan::Full }));
    if external {
        if let Some(state) = std::env::var_os("RUNTIME_HOST_STATE_DIR") { out.extend(runtime_host_locations(Path::new(&state))); }
        if cfg!(windows) {
            if let Some(base) = std::env::var_os("LOCALAPPDATA") { out.extend(runtime_host_locations(&PathBuf::from(base).join("runtime-host"))); }
        }
        out.extend(host_token_locations(&crate::config::paths::config_dir().unwrap_or_default()));
        // Inventory custody paths even when an executable has been removed or
        // a deployment is temporarily invalid. Launch validation must not make
        // an existing credential directory disappear from the read policy.
        if let Some(selected) =
            descriptor::<crate::runtime::claude::ClaudeDeployment>("claude/deployment.json")
        {
            out.push(Location {
                path: selected.claude_home,
                tree: true,
                scan: IdentityScan::ToolHome,
            });
        }
        match descriptor::<crate::launch::local_deployment::LocalDeployment>(
            "light/deployment.json",
        ) {
            Some(selected) => {
                add(&mut out, &selected.native_home, &[], &["auth.json", ".credentials.json", "config.toml", "cutex-shared-config.json"]);
                if let Some(job) = selected.job_mcp {
                    for path in [job.api_token_file, job.grant_key_file] {
                        out.push(Location { path, tree: false, scan: IdentityScan::Full });
                    }
                }
            }
            _ => {
                if let Ok(home) = crate::config::paths::host_codex_home_dir() {
                    add(&mut out, &home, &[], &["auth.json", ".credentials.json", "config.toml", "cutex-shared-config.json"]);
                }
            }
        }
        if let Some(path) = std::env::var_os(crate::config::env::CODEX_AUTH_FILE_ENV_VAR) {
            out.push(Location {
                path: PathBuf::from(path),
                tree: false,
                scan: IdentityScan::Full,
            });
        }
        if let Some(home) = std::env::var_os(crate::config::env::CLAUDE_CONFIG_DIR_ENV_VAR) {
            out.push(Location {
                path: PathBuf::from(home),
                tree: true,
                scan: IdentityScan::ToolHome,
            });
        }
        if let Some(home) = std::env::var_os("KIMI_CODE_HOME") {
            add(&mut out, Path::new(&home), &["credentials", "oauth"], &[]);
        }
        if let Some(home) = std::env::var_os("GNUPGHOME") {
            out.push(Location {
                path: PathBuf::from(home),
                tree: true,
                scan: IdentityScan::Full,
            });
        }
        for name in [
            "AWS_SHARED_CREDENTIALS_FILE",
            "AWS_CONFIG_FILE",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "NETRC",
        ] {
            if let Some(path) = std::env::var_os(name) {
                out.push(Location {
                    path: PathBuf::from(path),
                    tree: false,
                    scan: IdentityScan::Full,
                });
            }
        }
        if let Some(paths) = std::env::var_os("KUBECONFIG") {
            for path in std::env::split_paths(&paths) {
                out.push(Location { path, tree: false, scan: IdentityScan::Full });
            }
        }
        for name in ["CLOUDSDK_CONFIG", "AZURE_CONFIG_DIR"] {
            if let Some(home) = std::env::var_os(name) {
                out.push(Location {
                    path: PathBuf::from(home),
                    tree: true,
                    scan: IdentityScan::Full,
                });
            }
        }
        if let Some(home) = std::env::var_os("DOCKER_CONFIG") {
            add(&mut out, Path::new(&home), &[], &["config.json"]);
        }
        if let Some(home) = std::env::var_os("CODEX_HOME") {
            add(&mut out, Path::new(&home), &[], &["auth.json", ".credentials.json", "config.toml", "cutex-shared-config.json"]);
        }
        if let Some(home) = std::env::var_os("CUTEX_DESKTOP_USER_DATA") {
            add(&mut out, Path::new(&home), DESKTOP_TREES, DESKTOP_FILES);
        }
        if let Some(root) = std::env::var_os("XDG_CONFIG_HOME") {
            add(
                &mut out,
                Path::new(&root),
                &[
                    "gcloud",
                    "google-chrome",
                    "chromium",
                    "microsoft-edge",
                    "BraveSoftware/Brave-Browser",
                ],
                &["gh/hosts.yml", "git/credentials", "rclone/rclone.conf"],
            );
            for app in ["Cutex", "cutex-desktop"] {
                add(
                    &mut out,
                    &PathBuf::from(&root).join(app),
                    DESKTOP_TREES,
                    DESKTOP_FILES,
                );
            }
        }
        if let Some(root) = std::env::var_os("XDG_DATA_HOME") {
            add(
                &mut out,
                Path::new(&root),
                &["keyrings", "kwalletd", "kwalletd5"],
                &[],
            );
        }
        if let Some(root) = std::env::var_os("APPDATA") {
            add(
                &mut out,
                Path::new(&root),
                &[
                    "Microsoft/Credentials",
                    "Microsoft/Vault",
                    "Microsoft/Protect",
                    "Microsoft/Crypto",
                    "Mozilla/Firefox/Profiles",
                ],
                &["GitHub CLI/hosts.yml"],
            );
            for app in ["Cutex", "cutex-desktop"] {
                add(
                    &mut out,
                    &PathBuf::from(&root).join(app),
                    DESKTOP_TREES,
                    DESKTOP_FILES,
                );
            }
        }
        if let Some(root) = std::env::var_os("LOCALAPPDATA") {
            add(
                &mut out,
                Path::new(&root),
                &[
                    "Microsoft/Credentials",
                    "Microsoft/Vault",
                    "Google/Chrome/User Data",
                    "Chromium/User Data",
                    "Microsoft/Edge/User Data",
                    "BraveSoftware/Brave-Browser/User Data",
                ],
                &[],
            );
        }
        if cfg!(unix) {
            add(
                &mut out,
                Path::new("/"),
                &[
                    "var/lib/tailscale",
                    "etc/ssh",
                    "etc/ssl/private",
                    "etc/NetworkManager/system-connections",
                ],
                &["etc/shadow", "etc/gshadow", "etc/krb5.keytab"],
            );
        }
        if cfg!(windows) {
            let root = std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:/ProgramData"));
            add_windows_program_data(&mut out, &root);
            if let Some(root) = std::env::var_os("SystemRoot") {
                add(
                    &mut out,
                    &PathBuf::from(root).join("System32/config"),
                    &[],
                    &["SAM", "SECURITY", "SYSTEM"],
                );
            }
        }
    }
    out
}

/// Background refresh uses the captured config home, never ambient HOME.
pub(super) fn deployment_locations(home: &Path) -> Vec<Location> {
    let mut out = Vec::new();
    let read = |relative: &str| std::fs::read(home.join("runtime").join(relative)).ok();
    if let Some(selected) = read("claude/deployment.json").and_then(|bytes| serde_json::from_slice::<crate::runtime::claude::ClaudeDeployment>(&bytes).ok()) {
        out.push(Location { path: selected.claude_home, tree: true, scan: IdentityScan::ToolHome });
    }
    if let Some(selected) = read("light/deployment.json").and_then(|bytes| serde_json::from_slice::<crate::launch::local_deployment::LocalDeployment>(&bytes).ok()) {
        add(&mut out, &selected.native_home, &[], &["auth.json", ".credentials.json", "config.toml", "cutex-shared-config.json"]);
        if let Some(job) = selected.job_mcp {
            for path in [job.api_token_file, job.grant_key_file] { out.push(Location { path, tree: false, scan: IdentityScan::Full }); }
        }
    }
    out.extend(host_token_locations(home));

    out
}

fn runtime_host_locations(state: &Path) -> Vec<Location> {
    let mut out = Vec::new();
    add(&mut out, state, &[], &["token"]);
    if let Some(path) = std::fs::read(state.join("endpoint.json")).ok().and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok()).and_then(|value| value["tokenFile"].as_str().map(PathBuf::from)) {
        if path.is_absolute() { out.push(Location { path, tree: false, scan: IdentityScan::Full }); }
    }
    out
}

fn host_token_locations(home: &Path) -> Vec<Location> {
    let mut out = Vec::new();
    if let Some(hosts) = std::fs::read(home.join("hosts.json")).ok().and_then(|bytes| serde_json::from_slice::<crate::management::connections::Hosts>(&bytes).ok()) {
        for connection in hosts.connections { out.push(Location { path: connection.token_file, tree: false, scan: IdentityScan::Full }); }
    }

    out
}

fn descriptor<T: serde::de::DeserializeOwned>(relative: &str) -> Option<T> {
    let path = crate::config::paths::runtime_dir().ok()?.join(relative);
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

// Runtime contains owner content (artifacts, attachments and documents), so
// protect concrete custody namespaces/names rather than denying all runtime/.
pub(super) const RUNTIME_IDENTITY_TREES: &[(&str, IdentityScan)] = &[
    ("claude/sessions", IdentityScan::AccountHomes), ("light/agents", IdentityScan::AccountHomes), ("dsh", IdentityScan::AccountHomes),
    ("kimi", IdentityScan::AccountHomes), ("kimi/redaction", IdentityScan::Full),
    ("app-server", IdentityScan::Full), ("job-service", IdentityScan::Full),
    ("nci-verification", IdentityScan::Full), ("peers/secrets", IdentityScan::Full),
    ("users", IdentityScan::AccountHomes), ("docker-home", IdentityScan::ToolHome),
    ("thirdparty/userhome", IdentityScan::ToolHome),
];
pub(super) const RUNTIME_IDENTITY_FILES: &[&str] = &[
    "task-service/job-service-completion.token", "inbound-mail/config.json", "peers/peers.json",
];
pub(super) fn same_file_family(root: &[String], path: &[String]) -> bool {
    if root == path { return true; }
    root.len() == path.len() && root.len() > 0 && root[..root.len()-1] == path[..path.len()-1]
        && path.last().is_some_and(|name| name.starts_with(&format!(".{}.tmp-", root.last().unwrap())))
}
pub(super) fn runtime_secret_key(root: &[String], path: &[String]) -> bool {
    let Some(relative) = path.strip_prefix(root) else { return false; };
    let Some(first) = relative.first() else { return false; };
    // These are user-authored content stores, including owner-private blobs.
    if matches!(first.as_str(), "artifacts" | "attachments" | "documents" | "file-reverts" | "workflows") { return false; }
    if matches!(first.as_str(), "app-server" | "job-service" | "nci-verification") { return true; }
    if relative.starts_with(&["peers".into(), "secrets".into()]) || relative.starts_with(&["kimi".into(), "redaction".into()]) { return true; }
    for file in RUNTIME_IDENTITY_FILES {
        if same_file_family(&key(Path::new(file)), relative) { return true; }
    }
    let Some(name) = relative.last() else { return false; };
    let mut base = name.as_str();
    if base.starts_with('.') && base.contains(".tmp-") { base = base[1..].split(".tmp-").next().unwrap_or(base); }
    TOOL_CREDENTIAL_FILES.contains(&base) || base.ends_with(".sock") || base.ends_with(".socket")
        || relative.iter().any(|part| matches!(part.as_str(), "credentials" | "oauth"))
}

// This also supports Windows spellings in platform-independent inventory tests.
pub(super) fn key(path: &Path) -> Vec<String> {
    let text = path.to_string_lossy().replace('\\', "/");
    let text = text.strip_prefix("//?/").unwrap_or(&text);
    let windows = text.as_bytes().get(1) == Some(&b':') || text.starts_with("UNC/");
    text.split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .map(|part| {
            if windows {
                part.to_lowercase()
            } else {
                part.to_string()
            }
        })
        .collect()
}
pub(super) fn contains(location: &Location, path: &Path) -> bool {
    let root = key(&location.path);
    let path = key(path);
    if location.scan == IdentityScan::Runtime { runtime_secret_key(&root, &path) }
    else if location.tree { path.starts_with(&root) }
    else { same_file_family(&root, &path) }
}

#[cfg(test)]
pub(super) const PRIVATE_WRITER_CASES: &[(&str, &str)] = &[
    ("materialized Claude OAuth", "profiles/p/claude/.credentials.json"),
    ("legacy materialized Claude key", "profiles/p/claude/api_key"),
    ("materialized Codex auth", "profiles/p/auth.json"),
    ("materialized Kimi credentials", "profiles/p/kimi/credentials/kimi-code.json"),
    ("deleted profile credential", "deleted-profiles/old-p/claude/.credentials.json"),
    ("deleted profile private metadata", "deleted-profiles/old-p/profile.json"),
    ("Claude bound launch key", "runtime/claude/sessions/session/claude-api-key.bound.json"),
    ("Claude API launch config", "runtime/claude/sessions/session/claude-api-account.json"),
    ("Claude RPC token", "runtime/claude/sessions/session/rpc-token"),
    ("native loopback capability", "runtime/app-server/native/capability-token"),
    ("DSH endpoint token", "runtime/app-server/dsh-session/rpc-token"),
    ("DSH token-bearing launch", "runtime/dsh/session/launch.json"),
    ("Kimi token-bearing launch", "runtime/kimi/session/launch.json"),
    ("Kimi linked home credentials", "runtime/kimi/session/home/kimi/credentials/kimi-code.json"),
    ("native agent MCP OAuth", "runtime/light/agents/native/.credentials.json"),
    ("native secret-bearing config", "runtime/light/agents/native/config.toml"),
    ("native MCP copied baseline", "runtime/light/agents/native/cutex-shared-config.json"),
    ("host Codex MCP OAuth", "codex-home/.credentials.json"),
    ("host Codex MCP config", "codex-home/config.toml"),
    ("Job Service API token", "runtime/job-service/v1/api.token"),
    ("Job Service grant key", "runtime/job-service/v1/grant.key"),
    ("Job Service completion token", "runtime/task-service/job-service-completion.token"),
    ("NCI generated private key", "runtime/nci-verification/pending-generated.key"),
    ("NCI installed private key", "runtime/nci-verification/credential.key"),
    ("inbound IMAP password", "runtime/inbound-mail/config.json"),
    ("peer bearer token", "runtime/peers/secrets/peer.token"),
    ("peer credential verifiers", "runtime/peers/peers.json"),
    ("Kimi credential fingerprints", "runtime/kimi/redaction/account.json"),
    ("client credential verifiers", "management-clients/tokens.json"),
    ("client verifier migration copy", "runtime/users/backup-timestamp/tokens.json"),
    ("scoped MCP secrets", "scoped-mcp.json"),
    ("notification webhook bearer", "notifications/outbound.json"),
    ("managed service environment", "services/registry.json"),
    ("managed service migration copy", "services/registry.json.pre-services-proxy"),
    ("legacy credential migration copy", "archive/claude-tool-defaults/accounts.pre-stamp.json"),
    ("legacy profile migration copy", "archive/profile-generic-switches/accounts.pre-stamp.json"),
];

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runtime_host_endpoint_declares_its_selected_token_path() {
        let home = crate::profiles::lifecycle::tests::TestHome::new();
        let state = home.root().join("runtime-host"); std::fs::create_dir_all(&state).unwrap();
        let selected = home.root().join("custom-control-token.txt");
        std::fs::write(state.join("endpoint.json"), serde_json::to_vec(&serde_json::json!({"schemaVersion":1,"tokenFile":selected})).unwrap()).unwrap();
        let roots = runtime_host_locations(&state);
        assert!(roots.iter().any(|root| contains(root, &state.join("token"))));
        assert!(roots.iter().any(|root| contains(root, &selected)));
        assert!(!roots.iter().any(|root| contains(root, &home.root().join("ordinary.txt"))));
    }

    #[test]
    fn explicit_host_token_custody_does_not_depend_on_launch_validation() {
        let home = crate::profiles::lifecycle::tests::TestHome::new();
        let token = home.root().join("custom-host-token.txt");
        let hosts = serde_json::json!({"connections":[{"id":"other", "name":"Other", "host_id":"other-id", "ssh_target":"unused", "local_port":12345, "remote_port":12346, "token_file":token}]});
        std::fs::write(home.root().join("hosts.json"), serde_json::to_vec(&hosts).unwrap()).unwrap();
        let locations = host_token_locations(home.root());
        assert!(locations.iter().any(|location| contains(location, &token)));
        assert!(!locations.iter().any(|location| contains(location, &home.root().join("ordinary.txt"))));
    }

    #[test]
    fn browser_cache_pruning_keeps_credential_files_and_program_data_ssh() {
        for cache in ["Cache", "Code Cache", "GPUCache", "Service Worker/CacheStorage", "ShaderCache", "GrShaderCache"] {
            assert!(skip_identity_subtree(&PathBuf::from("C:/Users/Owner/AppData/Local/Google/Chrome/User Data/Default").join(cache)));
        }
        for file in ["Cookies", "Network/Cookies", "Login Data", "Web Data", "Local State", "IndexedDB/session.leveldb", "blob_storage/token"] {
            assert!(!skip_identity_subtree(&PathBuf::from("C:/Users/Owner/AppData/Local/Microsoft/Edge/User Data/Default").join(file)));
        }
        assert!(!skip_identity_subtree(Path::new("/home/owner/.ssh/Cache")));
        let mut roots = Vec::new();
        add_windows_program_data(&mut roots, Path::new("D:/SystemData"));
        for file in ["C:/ProgramData/ssh/ssh_host_ed25519_key", "C:/ProgramData/Tailscale/tailscaled.state", "D:/SystemData/ssh/key"] {
            assert!(roots.iter().any(|root| contains(root, Path::new(file))), "{file}");
        }
    }

    #[test]
    fn windows_forms_and_ordinary_previews() {
        let roots = for_roots(
            &[PathBuf::from("C:/Users/Owner")],
            Path::new("C:/Users/Owner/.cutex"),
        );
        for path in [
            r"C:\Users\Owner\.ssh\id_ed25519",
            r"C:\Users\Owner\.config\gh\hosts.yml",
            r"C:\Users\Owner\AppData\Roaming\GitHub CLI\hosts.yml",
            r"C:\Users\Owner\.cargo\credentials",
            r"C:\Users\Owner\.cargo\credentials.toml",
            r"C:\Users\Owner\.gemini\settings.json",
            r"C:\Users\Owner\.config\git\credentials",
            r"C:\Users\Owner\.config\rclone\rclone.conf",
            r"C:\Users\Owner\_netrc",
            r"C:\Users\Owner\AppData\Roaming\Cutex\host-tokens.json",
            r"C:\Users\Owner\AppData\Roaming\cutex-desktop\profile-link\secret-v1.json",
            r"C:\Users\Owner\AppData\Local\Google\Chrome\User Data\Default\Network\Cookies",
            r"\\?\C:\Users\OWNER\.cutex\kimi-accounts\.login\a\credentials\x",
            r"c:\users\owner\AppData\Local\Microsoft\Edge\User Data\Default\Network\Cookies",
            r"C:\Users\Owner\AppData\Roaming\Microsoft\Protect\key",
        ] {
            assert!(
                roots.iter().any(|root| contains(root, Path::new(path))),
                "{path}"
            );
        }
        for path in [
            "C:/Users/Owner/.cutex/projects/p/memory/MEMORY.md",
            "C:/Users/Owner/.cutex/artifacts/a.html",
            "C:/Users/Owner/.ssh-backup/report.md",
        ] {
            assert!(
                !roots.iter().any(|root| contains(root, Path::new(path))),
                "{path}"
            );
        }
    }
}
