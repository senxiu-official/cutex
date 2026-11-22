//! Which files may be published (contract: `artifact_path_forbidden`):
//! the canonical path (symlinks and junctions followed) must be a regular
//! file inside the session's cwd, the project workspace root or a writable
//! root, and never under the cutex home, a `.git` directory or a credential
//! location. Default names, kinds and media types.

use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    /// Missing, unreadable or malformed (400 `invalid_request`).
    Invalid(String),
    /// 403 `artifact_path_forbidden`.
    Forbidden(String),
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) | Self::Forbidden(message) => f.write_str(message),
        }
    }
}

/// A publishable file.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// Canonical, without the Windows `\\?\` prefix.
    pub path: PathBuf,
    pub bytes: u64,
    pub mtime_ms: Option<i64>,
}

impl Resolved {
    pub fn display(&self) -> String {
        self.path.display().to_string()
    }
}

/// Directories (relative to the user's home) that hold credentials.
const CREDENTIAL_DIRS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".config/gcloud",
    ".codex",
    ".claude",
    ".cutex-claude-compatible",
    ".password-store",
    ".local/share/keyrings",
    "AppData/Roaming/Microsoft/Credentials",
    "AppData/Local/Microsoft/Credentials",
    "AppData/Roaming/Microsoft/Protect",
];
/// File names that hold credentials wherever they are.
const CREDENTIAL_FILES: &[&str] = &[
    ".netrc",
    "_netrc",
    ".git-credentials",
    ".pypirc",
    ".npmrc",
    ".pgpass",
    ".credentials.json",
    "credentials.json",
    "auth.json",
    ".env",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
];
/// Directory names refused at any depth (git internals, other cutex homes).
const FORBIDDEN_DIRS: &[&str] = &[".git", ".cutex"];

fn fold(text: &str) -> String {
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text.to_string()
    }
}

pub fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path
}

/// Canonical form (symlinks / junctions followed), `None` when missing.
pub fn canonical(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok().map(strip_verbatim)
}

/// Comparable components of an already canonical path.
pub fn key(path: &Path) -> Vec<String> {
    path.components()
        .filter(|component| !matches!(component, Component::CurDir))
        .map(|component| fold(&component.as_os_str().to_string_lossy().replace('/', "\\")))
        .collect()
}

/// `path` is `root` or beneath it (both canonical).
pub fn within(path: &Path, root: &Path) -> bool {
    let (path, root) = (key(path), key(root));
    path.len() >= root.len() && path.starts_with(&root)
}

/// The relative `/`-joined path of `path` under `root` (both canonical).
pub fn relative(path: &Path, root: &Path) -> Option<String> {
    if !within(path, root) {
        return None;
    }
    let skip = key(root).len();
    let parts: Vec<String> = path
        .components()
        .filter(|component| !matches!(component, Component::CurDir))
        .skip(skip)
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect();
    Some(parts.join("/"))
}

fn homes() -> Vec<PathBuf> {
    let mut homes = Vec::new();
    for home in [crate::config::paths::real_user_home(), crate::config::paths::home_dir()].into_iter().flatten() {
        let home = canonical(&home).unwrap_or(home);
        if !homes.iter().any(|known: &PathBuf| key(known) == key(&home)) {
            homes.push(home);
        }
    }
    homes
}

/// Why a canonical path may never be published or captured, if it may not.
pub fn forbidden_reason(path: &Path) -> Option<String> {
    if let Ok(cutex_home) = crate::config::paths::config_dir() {
        let cutex_home = canonical(&cutex_home).unwrap_or(cutex_home);
        if within(path, &cutex_home) {
            return Some("files under the cutex home cannot be published".into());
        }
    }
    let parts = key(path);
    let directories = &parts[..parts.len().saturating_sub(1)];
    if let Some(name) = directories.iter().find(|part| FORBIDDEN_DIRS.iter().any(|dir| fold(dir) == **part)) {
        return Some(format!("files under a {name} directory cannot be published"));
    }
    if let Some(file) = parts.last() {
        let credential = CREDENTIAL_FILES.iter().any(|name| fold(name) == *file) || file.starts_with(".env.");
        if credential {
            return Some(format!("{file} is a credential file and cannot be published"));
        }
    }
    for home in homes() {
        for dir in CREDENTIAL_DIRS {
            let location = dir.split('/').fold(home.clone(), |path, part| path.join(part));
            if within(path, &location) {
                return Some(format!("files under ~/{dir} (credentials) cannot be published"));
            }
        }
    }
    None
}

/// Reject network/device/ADS forms before *any* filesystem operation. Also
/// inspect existing reparse/symlink components before canonicalize can follow
/// one onto SMB. A same-user filesystem mutation race remains an OS boundary.
pub fn validate_local_path(path: &Path) -> Result<(), PathError> {
    validate_local(path, false, 0)
}
/// For paths returned by canonicalize, allow only the local drive verbatim
/// prefix that Windows itself adds. Verbatim UNC/device forms stay forbidden
/// (verbatim UNC: unless an [`OwnerShareScope`] covers it, r290 P10 B4/A8).
pub fn validate_resolved_local_path(path: &Path) -> Result<(), PathError> {
    validate_local(path, true, 0)
}
#[cfg(test)]
thread_local! {
    pub(crate) static LOCAL_COMPONENT_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
#[test]
fn round2_network_symlink_is_rejected_without_following_it() {
    let root=std::env::temp_dir().join(format!("relay-link-{}",uuid::Uuid::new_v4().simple()));std::fs::create_dir_all(&root).unwrap();let link=root.join("network");
    #[cfg(unix)]
    std::os::unix::fs::symlink("//localhost/not-a-share",&link).unwrap();
    #[cfg(windows)]
    if let Err(error)=std::os::windows::fs::symlink_dir(r"\\localhost\not-a-share",&link){eprintln!("network symlink creation unavailable: {error}");std::fs::remove_dir(root).unwrap();return;}
    assert!(validate_local_path(&link.join("secret")).is_err());
    #[cfg(unix)]
    std::fs::remove_file(link).unwrap();
    #[cfg(windows)]
    std::fs::remove_dir(link).unwrap();
    std::fs::remove_dir(root).unwrap();
}
fn validate_local(path: &Path, resolved: bool, depth: usize) -> Result<(), PathError> {
    let forbidden=||PathError::Forbidden("network, device and alternate-stream paths are unavailable".into());
    if depth > 32 {return Err(forbidden());}
    let raw=local_text(path, resolved)?;
    let mut prefix=PathBuf::new();
    for component in Path::new(&raw).components() {
        prefix.push(component);
        #[cfg(test)]
        LOCAL_COMPONENT_READS.with(|n| n.set(n.get() + 1));
        if let Ok(target)=std::fs::read_link(&prefix) {
            let target=if target.is_absolute(){target}else{prefix.parent().unwrap_or(Path::new("")).join(target)};
            validate_local(&target,true,depth+1)?;
        }
    }
    Ok(())
}
/// The textual checks of [`validate_local`] (no filesystem access): the text
/// to walk, or 403. r290 P10 (B4/A8): split out so the share allowance is
/// testable without touching SMB.
fn local_text(path: &Path, resolved: bool) -> Result<String, PathError> {
    let forbidden=||PathError::Forbidden("network, device and alternate-stream paths are unavailable".into());
    let text=path.to_string_lossy();
    if text.len()>4096 || text.chars().any(char::is_control){return Err(forbidden());}
    let mut raw=text.as_ref();
    #[cfg(windows)]
    if resolved && raw.starts_with(r"\\?\") {
        let rest=&raw[4..];
        if rest.as_bytes().get(1)==Some(&b':') && rest.as_bytes()[0].is_ascii_alphabetic() {raw=rest;}
    }
    #[cfg(not(windows))]
    let _=resolved;
    // r290 P10 (B4/A8): an owner request (see `OwnerShareScope`) may name
    // `\\wsl$\…`, `\\wsl.localhost\…` or a configured share (also in its
    // `\\?\UNC\` form). The ADS check and the reparse walk below still apply.
    let share;
    if raw.starts_with("\\\\") || raw.starts_with("//") || raw.starts_with("\\/") || raw.starts_with("/\\") || raw.starts_with(r"\??\") {
        share=owner_share_path(raw).ok_or_else(forbidden)?;
        raw=&share;
    }
    #[cfg(windows)]
    if raw.char_indices().any(|(i,c)| c==':' && !(i==1 && raw.as_bytes()[0].is_ascii_alphabetic())) {return Err(forbidden());}
    Ok(raw.to_string())
}

// ---------------------------------------------------------------- r290 P10 (B4/A8) owner shares

/// Most `ownerShares` entries.
pub const MAX_OWNER_SHARES: usize = 32;
const WSL_SERVERS: &[&str] = &["wsl$", "wsl.localhost"];

/// A `\\server\share` root, folded for comparison.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareRoot {
    server: String,
    share: String,
}

#[derive(Clone)]
struct ShareState {
    /// Configured entries (`None`: read `ownerShares` from the settings when first needed).
    entries: Option<std::rc::Rc<[String]>>,
    /// Resolved roots (drive entries mapped), filled on first UNC check.
    roots: Option<std::rc::Rc<[ShareRoot]>>,
    /// The request may read host-control secrets (owner-local, full scope).
    secret_access: bool,
}

thread_local! {
    static OWNER_SHARES: std::cell::RefCell<Option<ShareState>> = const { std::cell::RefCell::new(None) };
}

/// While alive, network paths of the owner allowance pass `validate_local_path`
/// and `validate_resolved_local_path` on this thread: `\\wsl$\…`,
/// `\\wsl.localhost\…` and the `ownerShares` roots (drive entries: whatever the
/// drive maps to). Installed only by owner Management routes
/// (`host_files::owner_share_scope`); everything else keeps refusing every
/// network path. Restores the previous state on drop (nests).
#[must_use]
pub struct OwnerShareScope {
    previous: Option<Option<ShareState>>,
}

impl OwnerShareScope {
    /// No allowance change.
    pub fn inactive() -> Self {
        Self { previous: None }
    }
    /// The allowance with the `ownerShares` setting (read lazily).
    pub fn from_settings(secret_access: bool) -> Self {
        Self::enter(ShareState { entries: None, roots: None, secret_access })
    }
    /// The allowance with these entries (tests, or a caller that has the config).
    pub fn with_entries(entries: Vec<String>, secret_access: bool) -> Self {
        Self::enter(ShareState { entries: Some(entries.into()), roots: None, secret_access })
    }
    fn enter(state: ShareState) -> Self {
        let previous = OWNER_SHARES.with(|cell| cell.replace(Some(state)));
        Self { previous: Some(previous) }
    }
}

impl Drop for OwnerShareScope {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            OWNER_SHARES.with(|cell| *cell.borrow_mut() = previous);
        }
    }
}

/// Whether an owner share allowance is active on this thread.
pub fn owner_shares_active() -> bool {
    OWNER_SHARES.with(|cell| cell.borrow().is_some())
}

/// `(server, share, rest)` of a UNC path (`\\server\share\…` or
/// `\\?\UNC\server\share\…`, either slash); `None` for device forms
/// (`\\.\`, `\\?\` other than UNC, `\??\`), an empty server or share, empty
/// inner components, or a `.` / `..` component.
fn unc_parts(raw: &str) -> Option<(String, String, Vec<String>)> {
    let text = raw.replace('/', "\\");
    let body = if text.get(..8).is_some_and(|head| head.eq_ignore_ascii_case(r"\\?\UNC\")) {
        &text[8..]
    } else if let Some(body) = text.strip_prefix(r"\\") {
        body
    } else {
        return None;
    };
    let mut parts: Vec<&str> = body.split('\\').collect();
    if parts.len() > 2 && parts.last() == Some(&"") {
        parts.pop();
    }
    if parts.len() < 2 || parts.iter().any(|part| part.is_empty() || *part == "." || *part == "..") {
        return None;
    }
    let (server, share) = (parts[0], parts[1]);
    if matches!(server, "?" | ".") || server.contains(['?', '*', ':', '"', '<', '>', '|']) || share.contains(['?', '*', ':', '"', '<', '>', '|']) {
        return None;
    }
    Some((server.to_string(), share.to_string(), parts[2..].iter().map(|part| part.to_string()).collect()))
}

fn share_root_of(raw: &str) -> Option<ShareRoot> {
    unc_parts(raw).map(|(server, share, _)| ShareRoot { server: server.to_lowercase(), share: share.to_lowercase() })
}

fn is_loopback_server(server: &str) -> bool {
    let server = server.to_ascii_lowercase();
    server == "localhost"
        || (server.ends_with(".localhost") && !WSL_SERVERS.contains(&server.as_str()))
        || server.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
        || server.ends_with(".ipv6-literal.net")
}

/// Normalise one `ownerShares` entry: `\\server\share` (no further path) or
/// a drive letter with a colon (`Z:`). Loopback servers and administrative
/// shares (`C$`, `ADMIN$`, `IPC$`) are refused: they alias local paths past
/// the local secret rules.
pub fn normalize_owner_share(entry: &str) -> Result<String, String> {
    let bad = |why: &str| Err(format!("ownerShares: {entry:?} {why}"));
    if entry.chars().any(char::is_control) || entry.len() > 260 {
        return bad("is not a share root");
    }
    let bytes = entry.as_bytes();
    if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Ok(entry.to_ascii_uppercase());
    }
    if !entry.starts_with(r"\\") || entry.contains('/') || entry[2..].starts_with(['?', '.']) {
        return bad(r"must be \\server\share or a drive letter like Z:");
    }
    match unc_parts(entry) {
        Some((server, share, rest)) if rest.is_empty() && !entry.ends_with('\\') => {
            if is_loopback_server(&server) {
                return bad("names this machine (loopback); use the local path");
            }
            let upper = share.to_ascii_uppercase();
            if upper == "ADMIN$" || upper == "IPC$" || (upper.len() == 2 && upper.ends_with('$')) {
                return bad("is an administrative share");
            }
            Ok(format!(r"\\{server}\{share}"))
        }
        _ => bad(r"must be exactly \\server\share (no further path, no device form)"),
    }
}

/// Validate and normalise the whole list (≤ 32 entries, duplicates dropped).
pub fn normalize_owner_shares(entries: &[String]) -> Result<Vec<String>, String> {
    if entries.len() > MAX_OWNER_SHARES {
        return Err(format!("ownerShares: at most {MAX_OWNER_SHARES} entries"));
    }
    let mut out: Vec<String> = Vec::new();
    for entry in entries {
        let entry = normalize_owner_share(entry)?;
        if !out.iter().any(|known| known.eq_ignore_ascii_case(&entry)) {
            out.push(entry);
        }
    }
    Ok(out)
}

/// The share roots of normalised entries; a drive entry is mapped through
/// canonicalize (`Z:\` → `\\?\UNC\server\share\`). Non-Windows: none.
fn resolve_share_roots(entries: &[String]) -> Vec<ShareRoot> {
    if !cfg!(windows) {
        return Vec::new();
    }
    entries
        .iter()
        .filter_map(|entry| {
            if entry.len() == 2 && entry.ends_with(':') {
                let mapped = std::fs::canonicalize(format!(r"{entry}\")).ok()?;
                share_root_of(&mapped.to_string_lossy())
            } else {
                share_root_of(entry)
            }
        })
        .collect()
}

/// The `\\server\share\…` form of `raw` when the active owner allowance
/// covers it.
fn owner_share_path(raw: &str) -> Option<String> {
    if !cfg!(windows) {
        return None;
    }
    let state = OWNER_SHARES.with(|cell| cell.borrow().clone())?;
    let (server, share, rest) = unc_parts(raw)?;
    let root = ShareRoot { server: server.to_lowercase(), share: share.to_lowercase() };
    let allowed = WSL_SERVERS.contains(&root.server.as_str()) || {
        let roots = match state.roots {
            Some(roots) => roots,
            None => {
                let entries: Vec<String> = match state.entries {
                    Some(entries) => entries.to_vec(),
                    None => crate::config::store::load_codez_config().owner_shares,
                };
                let entries = normalize_owner_shares(&entries).unwrap_or_default();
                let roots: std::rc::Rc<[ShareRoot]> = resolve_share_roots(&entries).into();
                OWNER_SHARES.with(|cell| {
                    if let Some(active) = cell.borrow_mut().as_mut() {
                        active.roots = Some(roots.clone());
                    }
                });
                roots
            }
        };
        roots.contains(&root)
    };
    if !allowed {
        return None;
    }
    // WSL automounts alias the host disk, including custom CUTEX_HOME paths
    // and links. Remote devices cannot use /mnt to bypass local secret checks.
    // Owner-local requests keep their allowance. Reparse targets take this
    // same path through validate_local, before canonicalize follows them.
    let wsl_mount = WSL_SERVERS.contains(&root.server.as_str())
        && rest.first().is_some_and(|part| part.eq_ignore_ascii_case("mnt"));
    // Another host's (or a WSL distro's) cutex home holds its control credentials.
    if !state.secret_access && (wsl_mount || rest.iter().any(|part| fold(part) == fold(".cutex"))) {
        return None;
    }
    let mut text = format!(r"\\{server}\{share}");
    for part in rest {
        text.push('\\');
        text.push_str(&part);
    }
    Some(text)
}

thread_local! {
    static OWNER_PUBLISH: std::cell::RefCell<Option<std::rc::Rc<OwnerPublishCheck>>> = const { std::cell::RefCell::new(None) };
}
type OwnerPublishCheck = dyn Fn(&Path, u64) -> Result<(), PathError>;

/// r290 P10 (d): while alive, [`resolve`] on this thread applies the owner's
/// publish rules (the owner's `POST /v2/artifact-store`, never an agent's
/// `artifact_publish`): no workspace-root containment and no credential
/// name / directory rules; still a regular file, the network / device / ADS
/// rules (with the owner share allowance) and `check(canonical, bytes)` (the
/// caller's preview-parity and size rules).
#[must_use]
pub struct OwnerPublishScope {
    previous: Option<std::rc::Rc<OwnerPublishCheck>>,
}

impl OwnerPublishScope {
    pub fn enter(check: impl Fn(&Path, u64) -> Result<(), PathError> + 'static) -> Self {
        let check: std::rc::Rc<OwnerPublishCheck> = std::rc::Rc::new(check);
        Self { previous: OWNER_PUBLISH.with(|cell| cell.replace(Some(check))) }
    }
}

impl Drop for OwnerPublishScope {
    fn drop(&mut self) {
        let previous = self.previous.take();
        OWNER_PUBLISH.with(|cell| *cell.borrow_mut() = previous);
    }
}

fn resolve_owner(raw: &str, base: Option<&Path>, check: &OwnerPublishCheck) -> Result<Resolved, PathError> {
    let given = PathBuf::from(raw);
    let joined = if given.is_absolute() || raw.starts_with(r"\\") || raw.starts_with("//") {
        given
    } else {
        let base = base.ok_or_else(|| PathError::Invalid("a relative path needs a working directory; give an absolute path".into()))?;
        base.join(given)
    };
    validate_local_path(&joined)
        .map_err(|_| PathError::Forbidden("network, device and alternate-stream paths cannot be published".into()))?;
    let path = match std::fs::canonicalize(&joined) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(PathError::Invalid(format!("no such file: {}", joined.display())))
        }
        Err(error) => return Err(PathError::Invalid(format!("cannot read {}: {error}", joined.display()))),
    };
    validate_resolved_local_path(&path)
        .map_err(|_| PathError::Forbidden("network, device and alternate-stream paths cannot be published".into()))?;
    let path = strip_verbatim(path);
    let metadata = std::fs::metadata(&path).map_err(|error| PathError::Invalid(format!("cannot read {}: {error}", path.display())))?;
    if !metadata.is_file() {
        return Err(PathError::Forbidden(format!("{} is not a regular file", path.display())));
    }
    check(&path, metadata.len())?;
    Ok(Resolved { path, bytes: metadata.len(), mtime_ms: mtime_ms(&metadata) })
}

/// Resolve a publish path: relative paths are joined to `base`; the result
/// must be a regular file inside one of `roots` and not forbidden.
pub fn resolve(path: &str, base: Option<&Path>, roots: &[PathBuf]) -> Result<Resolved, PathError> {
    let raw = path.trim();
    if raw.is_empty() || raw.contains('\0') || raw.chars().count() > 4096 {
        return Err(PathError::Invalid("path: a file path is required".into()));
    }
    // r290 P10 (d): the owner's own publish.
    if let Some(check) = OWNER_PUBLISH.with(|cell| cell.borrow().clone()) {
        return resolve_owner(raw, base, &*check);
    }
    // Checked before anything touches the path: on Windows a UNC or device
    // path (`\\host\share`, `\\.\pipe\…`, `\\?\…`) would be opened by
    // `canonicalize` itself (SMB / NTLM to another host, a named pipe), and
    // a `:` after the drive names an alternate data stream (`.env::$DATA`)
    // that the file-name checks would not see.
    #[cfg(windows)]
    {
        let unc = raw.starts_with("\\\\") || raw.starts_with("//") || raw.starts_with("\\/") || raw.starts_with("/\\");
        let stream = raw.char_indices().any(|(index, ch)| ch == ':' && !(index == 1 && raw.as_bytes()[0].is_ascii_alphabetic()));
        if unc || stream {
            return Err(PathError::Forbidden("network, device and alternate-stream paths cannot be published".into()));
        }
    }
    let given = PathBuf::from(raw);
    let joined = if given.is_absolute() {
        given
    } else {
        let base = base.ok_or_else(|| PathError::Invalid("a relative path needs a working directory; give an absolute path".into()))?;
        base.join(given)
    };
    validate_local_path(&joined)?;
    let path = match std::fs::canonicalize(&joined) {
        Ok(path) => strip_verbatim(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(PathError::Invalid(format!("no such file: {}", joined.display())))
        }
        Err(error) => return Err(PathError::Invalid(format!("cannot read {}: {error}", joined.display()))),
    };
    let metadata = std::fs::metadata(&path).map_err(|error| PathError::Invalid(format!("cannot read {}: {error}", path.display())))?;
    if !metadata.is_file() {
        return Err(PathError::Forbidden(format!("{} is not a regular file", path.display())));
    }
    if let Some(reason) = forbidden_reason(&path) {
        return Err(PathError::Forbidden(reason));
    }
    let inside = roots.iter().filter_map(|root| canonical(root)).any(|root| within(&path, &root));
    if !inside {
        return Err(PathError::Forbidden(format!(
            "{} is outside the working directory, the project workspace and the writable roots",
            path.display()
        )));
    }
    Ok(Resolved { path, bytes: metadata.len(), mtime_ms: mtime_ms(&metadata) })
}

pub fn mtime_ms(metadata: &std::fs::Metadata) -> Option<i64> {
    let modified = metadata.modified().ok()?;
    let since = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    i64::try_from(since.as_millis()).ok()
}

/// Default name: the path relative to the first of `bases` that contains
/// it, else the file name.
pub fn default_name(path: &Path, bases: &[PathBuf]) -> String {
    for base in bases {
        let Some(base) = canonical(base) else { continue };
        if let Some(relative) = relative(path, &base).filter(|relative| !relative.is_empty()) {
            return relative;
        }
    }
    path.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_else(|| path.display().to_string())
}

fn extension(name: &str) -> String {
    let file = name.rsplit(['/', '\\']).next().unwrap_or(name);
    file.rsplit_once('.').map(|(_, extension)| extension.to_ascii_lowercase()).unwrap_or_default()
}

/// Default kind from the name.
pub fn default_kind(name: &str) -> &'static str {
    let normalized = name.replace('\\', "/").to_ascii_lowercase();
    if normalized.split('/').next() == Some("handoff") || normalized.contains("/handoff/") {
        return "handoff";
    }
    let file = normalized.rsplit('/').next().unwrap_or(&normalized);
    if (file.starts_with("plan") || file.ends_with("-plan.md") || file.ends_with("_plan.md")) && file.ends_with(".md") {
        return "plan";
    }
    match extension(name).as_str() {
        "md" | "markdown" | "txt" | "pdf" | "html" | "htm" | "docx" | "doc" | "tex" | "rst" | "odt" | "rtf" => "report",
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "bmp" | "tif" | "tiff" | "eps" => "figure",
        "csv" | "tsv" | "xlsx" | "xls" | "ods" | "parquet" | "feather" => "table",
        "json" | "jsonl" | "ndjson" | "npz" | "npy" | "h5" | "hdf5" | "nc" | "pkl" | "pickle" | "zip" | "tar" | "gz" | "zst" | "sqlite"
        | "sqlite3" | "db" | "arrow" | "mat" | "rds" => "data",
        _ => "other",
    }
}

/// Default kind of a version: from the name, else (a name without a known
/// extension, e.g. an owner-chosen label) from the source file's name.
pub fn default_kind_of(name: &str, source: &str) -> &'static str {
    match default_kind(name) {
        "other" => default_kind(file_part(source)),
        kind => kind,
    }
}

/// Last segment of a `/` or `\` separated path (both on every platform: a
/// Windows source path reads the same on a Linux host and vice versa).
fn file_part(path: &str) -> &str {
    path.rsplit(['/', '\\']).find(|segment| !segment.is_empty()).unwrap_or(path)
}

/// Media type of a version, independent of the platform: the source file's
/// extension first (the file that was read), then the artifact name's
/// extension (a name may be a label without one), then the content's magic
/// bytes; otherwise `text/plain` when `sample` is UTF-8 without NUL, else
/// `application/octet-stream`.
pub fn media_type(source: &str, name: &str, sample: &[u8]) -> String {
    if let Some(known) = media_type_by_extension(&extension(file_part(source))).or_else(|| media_type_by_extension(&extension(name))) {
        return known.to_string();
    }
    if let Some(sniffed) = sniff(sample) {
        return sniffed.to_string();
    }
    if looks_like_text(sample) {
        "text/plain".into()
    } else {
        "application/octet-stream".into()
    }
}

/// Media type of well-known file signatures (and an HTML document start).
fn sniff(sample: &[u8]) -> Option<&'static str> {
    const SIGNATURES: &[(&[u8], &str)] = &[
        (b"%PDF-", "application/pdf"),
        (b"\x89PNG\r\n\x1a\n", "image/png"),
        (b"\xff\xd8\xff", "image/jpeg"),
        (b"GIF87a", "image/gif"),
        (b"GIF89a", "image/gif"),
    ];
    if let Some((_, media)) = SIGNATURES.iter().find(|(magic, _)| sample.starts_with(magic)) {
        return Some(media);
    }
    if sample.len() >= 12 && &sample[..4] == b"RIFF" && &sample[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    let text = sample.strip_prefix(b"\xef\xbb\xbf").unwrap_or(sample);
    let start = text.iter().position(|byte| !byte.is_ascii_whitespace()).unwrap_or(text.len());
    let head: Vec<u8> = text[start..].iter().take(15).map(u8::to_ascii_lowercase).collect();
    if head.starts_with(b"<!doctype html") || head.starts_with(b"<html") {
        return Some("text/html");
    }
    None
}

fn media_type_by_extension(extension: &str) -> Option<&'static str> {
    let known = match extension {
        "apk" => "application/vnd.android.package-archive",
        "md" | "markdown" => "text/markdown",
        "txt" | "log" | "text" => "text/plain",
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "tex" => "text/x-tex",
        "rst" => "text/x-rst",
        "py" => "text/x-python",
        "rs" => "text/x-rust",
        "js" | "mjs" => "text/javascript",
        "ts" => "text/x-typescript",
        "json" => "application/json",
        "jsonl" | "ndjson" => "application/x-ndjson",
        "yaml" | "yml" => "application/yaml",
        "toml" => "application/toml",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "eps" => "application/postscript",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "parquet" => "application/vnd.apache.parquet",
        _ => return None,
    };
    Some(known)
}

/// UTF-8 (a sample may end inside a character) without NUL.
pub fn looks_like_text(sample: &[u8]) -> bool {
    if sample.contains(&0) {
        return false;
    }
    match std::str::from_utf8(sample) {
        Ok(_) => true,
        Err(error) => error.error_len().is_none() && sample.len() - error.valid_up_to() < 4,
    }
}

/// Media types shown as text.
pub fn is_text_media(media_type: &str) -> bool {
    media_type.starts_with("text/")
        || matches!(
            media_type,
            "application/json" | "application/x-ndjson" | "application/yaml" | "application/toml" | "application/xml" | "image/svg+xml"
        )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn temp(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cutex-artifact-paths-{label}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        canonical(&dir).unwrap()
    }

    #[test]
    fn resolves_inside_roots_and_refuses_the_rest() {
        let root = temp("roots");
        let workspace = root.join("ws");
        let outside = root.join("outside");
        std::fs::create_dir_all(workspace.join("reports")).unwrap();
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(workspace.join("reports/a.md"), "# A").unwrap();
        std::fs::write(workspace.join(".git/config"), "[core]").unwrap();
        std::fs::write(workspace.join(".env"), "KEY=1").unwrap();
        std::fs::write(outside.join("b.md"), "# B").unwrap();
        let roots = vec![workspace.clone()];
        let resolved = resolve("reports/a.md", Some(&workspace), &roots).unwrap();
        assert_eq!(resolved.bytes, 3);
        assert!(resolved.mtime_ms.is_some());
        assert_eq!(default_name(&resolved.path, &[workspace.clone()]), "reports/a.md");
        assert_eq!(default_name(&resolved.path, &[outside.clone()]), "a.md");
        let absolute = workspace.join("reports").join("a.md");
        assert!(resolve(&absolute.display().to_string(), None, &roots).is_ok());
        assert!(matches!(resolve("reports/a.md", None, &roots), Err(PathError::Invalid(_))), "relative without a base");
        assert!(matches!(resolve("reports/missing.md", Some(&workspace), &roots), Err(PathError::Invalid(_))));
        assert!(matches!(resolve("reports", Some(&workspace), &roots), Err(PathError::Forbidden(_))), "a directory");
        assert!(matches!(resolve(".git/config", Some(&workspace), &roots), Err(PathError::Forbidden(_))));
        assert!(matches!(resolve(".env", Some(&workspace), &roots), Err(PathError::Forbidden(_))));
        assert!(matches!(resolve("../outside/b.md", Some(&workspace), &roots), Err(PathError::Forbidden(_))), "dot-dot leaves the root");
        assert!(resolve("../outside/b.md", Some(&workspace), &[workspace.clone(), outside.clone()]).is_ok(), "a writable root");
        assert!(matches!(resolve("", Some(&workspace), &roots), Err(PathError::Invalid(_))));
        #[cfg(windows)]
        for path in [r"\\127.0.0.1\c$\x.md", "//127.0.0.1/c$/x.md", r"\\.\pipe\x", r"\\?\C:\x.md", ".env::$DATA", "reports/a.md:s"] {
            assert!(matches!(resolve(path, Some(&workspace), &roots), Err(PathError::Forbidden(_))), "{path}");
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn refuses_the_cutex_home_and_credential_dirs() {
        let cutex_home = crate::config::paths::config_dir().unwrap();
        std::fs::create_dir_all(&cutex_home).unwrap();
        let file = cutex_home.join(format!("artifact-path-probe-{}.txt", uuid::Uuid::new_v4().simple()));
        std::fs::write(&file, "x").unwrap();
        let resolved = resolve(&file.display().to_string(), None, &[cutex_home.clone()]);
        assert!(matches!(resolved, Err(PathError::Forbidden(ref message)) if message.contains("cutex home")), "{resolved:?}");
        let _ = std::fs::remove_file(&file);
        let home = crate::config::paths::home_dir().unwrap();
        assert!(forbidden_reason(&home.join(".ssh").join("config")).is_some());
        assert!(forbidden_reason(&home.join(".codex").join("auth.json")).is_some());
        assert!(forbidden_reason(&home.join("project").join(".cutex").join("config.json")).is_some());
        assert!(forbidden_reason(&home.join("project").join("reports").join("a.md")).is_none());
        assert!(forbidden_reason(&home.join("project").join(".env.local")).is_some());
    }

    /// A symlink (Unix) or junction (Windows) inside the workspace that
    /// leads out of it is refused; one that stays inside is fine.
    #[test]
    fn links_leaving_the_root_are_refused() {
        let root = temp("links");
        let workspace = root.join("ws");
        let outside = root.join("outside");
        std::fs::create_dir_all(workspace.join("inner")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.md"), "s").unwrap();
        std::fs::write(workspace.join("inner/ok.md"), "o").unwrap();
        let link_out = workspace.join("out-link");
        let link_in = workspace.join("in-link");
        let made = make_dir_link(&outside, &link_out) && make_dir_link(&workspace.join("inner"), &link_in);
        assert!(made, "could not create directory links");
        let roots = vec![workspace.clone()];
        assert!(matches!(resolve("out-link/secret.md", Some(&workspace), &roots), Err(PathError::Forbidden(_))));
        let inside = resolve("in-link/ok.md", Some(&workspace), &roots).unwrap();
        assert_eq!(default_name(&inside.path, &[workspace.clone()]), "inner/ok.md", "named by its real path");
        #[cfg(windows)]
        {
            let _ = std::fs::remove_dir(&link_out);
            let _ = std::fs::remove_dir(&link_in);
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    pub(crate) fn make_dir_link(target: &Path, link: &Path) -> bool {
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(unix)]
    pub(crate) fn make_dir_link(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }

    #[test]
    fn kinds_and_media_types() {
        assert_eq!(default_kind("reports/regression.md"), "report");
        assert_eq!(default_kind("figures/fig1.PNG"), "figure");
        assert_eq!(default_kind("results/table.csv"), "table");
        assert_eq!(default_kind("results/run.jsonl"), "data");
        assert_eq!(default_kind("handoff/next.md"), "handoff");
        assert_eq!(default_kind("plan.md"), "plan");
        assert_eq!(default_kind("weird.xyz"), "other");
        assert_eq!(media_type("", "a.md", b""), "text/markdown");
        assert_eq!(media_type("", "a.unknown", b"hello"), "text/plain");
        assert_eq!(media_type("", "a.unknown", &[0, 1, 2]), "application/octet-stream");
    }

    /// A name chosen by the owner (a label without an extension) must not
    /// turn an HTML / PDF / JPEG source into text or octet-stream; Windows
    /// and POSIX source paths read the same on every platform.
    #[test]
    fn media_type_prefers_the_source_extension_on_any_platform() {
        let html = b"<!DOCTYPE html>\n<html><body>hi</body></html>";
        for source in [r"C:\Users\someone\work\out\report.HTML", "/home/someone/work/out/report.html", r"\\?\D:\w\report.htm", "out/report.html"] {
            assert_eq!(media_type(source, "round2-html-artifact", html), "text/html", "{source}");
        }
        assert_eq!(media_type(r"D:\w\paper.pdf", "round2-pdf-artifact", b"%PDF-1.7\n%\xe2\xe3"), "application/pdf");
        assert_eq!(media_type(r"D:\w\photo.JPG", "Holiday photo", &[0xff, 0xd8, 0xff, 0xe0, 0, 0x10]), "image/jpeg");
        assert_eq!(media_type(r"D:\w\photo.jpeg", "photo", &[0xff, 0xd8, 0xff]), "image/jpeg");
        // The name's extension still counts when the source has none.
        assert_eq!(media_type(r"D:\w\README", "notes.md", b"# hi"), "text/markdown");
        // The source wins over a misleading name.
        assert_eq!(media_type("/w/fig.png", "fig.txt", b"\x89PNG\r\n\x1a\n"), "image/png");
        // Neither has a known extension: the content's signature decides.
        assert_eq!(media_type(r"D:\w\blob", "Blob", b"%PDF-1.4"), "application/pdf");
        assert_eq!(media_type("/w/blob", "Blob", b"\x89PNG\r\n\x1a\n\0\0"), "image/png");
        assert_eq!(media_type("/w/blob", "Blob", &[0xff, 0xd8, 0xff, 0xdb]), "image/jpeg");
        assert_eq!(media_type("/w/blob", "Blob", b"GIF89a\x01\0"), "image/gif");
        assert_eq!(media_type("/w/blob", "Blob", b"RIFF\x10\0\0\0WEBPVP8 "), "image/webp");
        assert_eq!(media_type("/w/page", "Page", b"\xef\xbb\xbf\r\n  <!doctype HTML><html>"), "text/html");
        assert_eq!(media_type("/w/page", "Page", b"<html lang=\"en\">"), "text/html");
        assert_eq!(media_type("/w/notes", "Notes", b"plain words"), "text/plain");
        assert_eq!(default_kind_of("round2-html-artifact", r"C:\w\out\report.html"), "report");
        assert_eq!(default_kind_of("Holiday photo", "/w/photo.jpg"), "figure");
        assert_eq!(default_kind_of("results/table.csv", r"C:\w\x.png"), "table");
        assert_eq!(default_kind_of("Label", "/w/blob"), "other");
        assert!(looks_like_text("héllo".as_bytes()));
        assert!(looks_like_text(&"é".as_bytes()[..1]), "cut inside a character");
        assert!(!looks_like_text(&[0xff, 0xfe, 0x41]));
        assert!(is_text_media("application/json"));
        assert!(!is_text_media("image/png"));
    }


    // r290 P10 (B4/A8): `ownerShares` entries are share roots or drive letters, nothing else.
    #[test]
    fn owner_share_entries_are_strict() {
        assert_eq!(normalize_owner_share(r"\\nas\docs").unwrap(), r"\\nas\docs");
        assert_eq!(normalize_owner_share("z:").unwrap(), "Z:");
        for bad in [
            "", "Z", "ZZ:", "Z:\\", r"C:\x", r"\\nas", r"\\nas\docs\sub", r"\\nas\docs\", "//nas/docs", r"\\nas/docs",
            r"\\?\UNC\nas\docs", r"\\.\pipe\x", r"\??\C:\x", r"\\localhost\docs", r"\\127.0.0.1\docs", r"\\[::1]\docs",
            r"\\nas\C$", r"\\nas\ADMIN$", r"\\nas\IPC$", r"\\nas\do:cs", r"\\nas\..", "\\\\nas\\docs\u{7}",
        ] {
            assert!(normalize_owner_share(bad).is_err(), "{bad:?}");
        }
        let many: Vec<String> = (0..=MAX_OWNER_SHARES).map(|i| format!(r"\\nas\s{i}")).collect();
        assert!(normalize_owner_shares(&many).is_err());
        assert!(normalize_owner_shares(&many[..MAX_OWNER_SHARES]).is_ok());
        assert_eq!(
            normalize_owner_shares(&[r"\\NAS\Docs".into(), r"\\nas\docs".into(), "z:".into()]).unwrap(),
            vec![r"\\NAS\Docs".to_string(), "Z:".into()]
        );
    }

    // r290 P10 (B4/A8): pure text checks (`local_text`), no SMB / WSL access.
    #[cfg(windows)]
    #[test]
    fn owner_share_scope_opens_wsl_and_configured_shares_only() {
        let check = |p: &str| local_text(Path::new(p), false).is_ok();
        let resolved = |p: &str| local_text(Path::new(p), true).is_ok();
        // No scope (agents and every other caller): every network path is refused.
        for p in [r"\\wsl$\Ubuntu\home\a.md", r"\\wsl.localhost\Ubuntu\x", r"\\nas\docs\a.md", r"\\?\UNC\nas\docs\a.md"] {
            assert!(!check(p), "{p}");
            assert!(!resolved(p), "{p}");
        }
        {
            let _scope = OwnerShareScope::with_entries(vec![r"\\NAS\Docs".into()], false);
            assert!(owner_shares_active());
            for p in [r"\\wsl$\Ubuntu\home\a.md", r"\\WSL.localhost\Ubuntu\x", "//wsl$/Ubuntu/x", r"\\nas\docs\a.md", r"\\nas\docs", r"\\?\UNC\nas\docs\a.md"] {
                assert!(check(p), "{p}");
            }
            assert!(resolved(r"\\?\UNC\nas\docs\a.md"));
            assert_eq!(local_text(Path::new(r"\\?\UNC\nas\docs\a.md"), true).unwrap(), r"\\nas\docs\a.md");
            assert!(!check(r"\\nas\other\a.md"), "another share on the same server");
            assert!(!check(r"\\evil\docs\a.md"), "the same share name elsewhere");
            // Device, ADS, dot and control forms stay refused even for the owner.
            for p in [
                r"\\.\pipe\x", r"\??\C:\x", r"\\?\GLOBALROOT\Device\x", r"\\.\UNC\nas\docs\x",
                r"\\nas\docs\a.md:secret", r"\\nas\docs\a.md::$DATA", r"\\wsl$\Ubuntu\a:b",
                r"\\nas\docs\..\other\x", r"\\nas\docs\.\x", r"\\nas\docs\\x", "\\\\wsl$\\Ubuntu\\a\u{1}b", r"C:\x.md:s",
            ] {
                assert!(!check(p), "{p}");
                assert!(!resolved(p), "{p}");
            }
            assert!(!check(r"\\?\C:\x"), "caller-supplied verbatim drive path");
            assert!(resolved(r"\\?\C:\x"), "canonical local drive prefix remains allowed");
            // Another host's (a distro's) cutex home: only with owner-local secret access.
            assert!(!check(r"\\wsl$\Ubuntu\home\me\.cutex\config.json"));
            let _inner = OwnerShareScope::with_entries(Vec::new(), true);
            assert!(check(r"\\wsl$\Ubuntu\home\me\.cutex\config.json"));
            assert!(!check(r"\\nas\docs\a.md"), "the inner scope replaces the outer entries");
        }
        assert!(!owner_shares_active());
        assert!(!check(r"\\wsl$\Ubuntu\home\a.md"), "the allowance ends with its guard");
        assert!(check(r"C:\x\a.md") && resolved(r"\\?\C:\x\a.md"), "local paths unchanged");
        let _inactive = OwnerShareScope::inactive();
        assert!(!check(r"\\wsl$\Ubuntu\home\a.md"));
    }

    // r290 P10 (B4/A8): a link inside the allowance that leaves it is still refused.
    #[cfg(windows)]
    #[test]
    fn owner_share_scope_still_validates_link_targets() {
        let root = temp("share-link");
        let link = root.join("elsewhere");
        if let Err(error) = std::os::windows::fs::symlink_dir(r"\\nas\other", &link) {
            eprintln!("network symlink creation unavailable: {error}");
            return;
        }
        let _scope = OwnerShareScope::with_entries(vec![r"\\nas\docs".into()], true);
        assert!(validate_local_path(&link.join("secret.md")).is_err());
        std::fs::remove_dir(link).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn remote_owner_wsl_mount_aliases_need_owner_local_secret_access() {
        let paths = [
            r"\\wsl$\Ubuntu\mnt\c\Users\owner\custom-home\config.json",
            r"\\WSL.LOCALHOST\Ubuntu\MNT\D\control-via-link\secret",
            r"\\?\UNC\wsl$\Ubuntu\mnt\c\custom-home\config.json",
            "//wsl.localhost/Ubuntu/mnt/c/custom-home/config.json",
            r"\\wsl$\Ubuntu\mnt",
        ];
        {
            let _scope = OwnerShareScope::with_entries(vec![r"\\nas\docs".into()], false);
            for path in paths {
                // Textual refusal means no WSL/SMB I/O, including link targets.
                LOCAL_COMPONENT_READS.with(|reads| reads.set(0));
                assert!(validate_local_path(Path::new(path)).is_err(), "{path}");
                assert!(validate_resolved_local_path(Path::new(path)).is_err(), "{path}");
                assert_eq!(LOCAL_COMPONENT_READS.with(|reads| reads.get()), 0, "{path}");
            }
            assert!(local_text(Path::new(r"\\wsl$\Ubuntu\home\owner\notes.md"), false).is_ok());
            assert!(local_text(Path::new(r"\\nas\docs\mnt\notes.md"), false).is_ok());
            let root = temp("wsl-host-link");
            let link = root.join("host-control");
            match std::os::windows::fs::symlink_dir(r"\\wsl$\Ubuntu\mnt\c\custom-home", &link) {
                Ok(()) => {
                    assert!(validate_local_path(&link.join("config.json")).is_err());
                    std::fs::remove_dir(&link).unwrap();
                }
                Err(error) => eprintln!("WSL mount symlink fixture unavailable: {error}"),
            }
            std::fs::remove_dir(root).unwrap();
        }
        let _scope = OwnerShareScope::with_entries(Vec::new(), true);
        for path in paths {
            assert!(local_text(Path::new(path), false).is_ok(), "owner-local {path}");
            assert!(local_text(Path::new(path), true).is_ok(), "owner-local resolved {path}");
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn owner_shares_are_inert_off_windows() {
        let _scope = OwnerShareScope::with_entries(vec![r"\\nas\docs".into()], true);
        assert!(local_text(Path::new("//wsl$/Ubuntu/x"), false).is_err());
        assert!(local_text(Path::new("//nas/docs/x"), false).is_err());
    }

    // r290 P10 (d): the owner's publish drops containment and the credential-name rules only.
    #[test]
    fn owner_publish_scope_drops_the_agent_rules_but_keeps_file_and_network_rules() {
        let root = temp("owner-publish");
        let workspace = root.join("ws");
        let outside = root.join(".claude");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(workspace.join(".env.example"), "A=1").unwrap();
        std::fs::write(outside.join("CLAUDE.md"), "notes").unwrap();
        let roots = vec![workspace.clone()];
        // Agent rules (no scope): refused.
        assert!(matches!(resolve(".env.example", Some(&workspace), &roots), Err(PathError::Forbidden(_))));
        let elsewhere = outside.join("CLAUDE.md").display().to_string();
        assert!(matches!(resolve(&elsewhere, Some(&workspace), &roots), Err(PathError::Forbidden(_))));
        let checked = std::rc::Rc::new(std::cell::Cell::new(0));
        {
            let seen = checked.clone();
            let _owner = OwnerPublishScope::enter(move |_, _| {
                seen.set(seen.get() + 1);
                Ok(())
            });
            assert_eq!(resolve(".env.example", Some(&workspace), &roots).unwrap().bytes, 3);
            assert_eq!(resolve(&elsewhere, Some(&workspace), &roots).unwrap().bytes, 5);
            assert_eq!(checked.get(), 2);
            assert!(matches!(resolve(&workspace.display().to_string(), None, &roots), Err(PathError::Forbidden(_))), "a directory");
            assert!(matches!(resolve("missing.md", Some(&workspace), &roots), Err(PathError::Invalid(_))));
            assert!(resolve("relative.md", None, &roots).is_err());
            for p in [r"\\127.0.0.1\c$\x.md", "//127.0.0.1/c$/x.md", r"\\.\pipe\x", r"\\?\C:\x.md", ".env.example::$DATA", "a.md:s"] {
                let refused = resolve(p, Some(&workspace), &roots);
                if cfg!(windows) {
                    assert!(matches!(refused, Err(PathError::Forbidden(_))), "{p}: {refused:?}");
                } else {
                    assert!(refused.is_err(), "{p}");
                }
            }
            let _deny = OwnerPublishScope::enter(|_, _| Err(PathError::Forbidden("caller rule".into())));
            assert_eq!(resolve(".env.example", Some(&workspace), &roots).unwrap_err(), PathError::Forbidden("caller rule".into()));
        }
        assert!(resolve(".env.example", Some(&workspace), &roots).is_err(), "the scope ends with its guard");
    }
}
