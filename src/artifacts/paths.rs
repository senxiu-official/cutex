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

/// Resolve a publish path: relative paths are joined to `base`; the result
/// must be a regular file inside one of `roots` and not forbidden.
pub fn resolve(path: &str, base: Option<&Path>, roots: &[PathBuf]) -> Result<Resolved, PathError> {
    let raw = path.trim();
    if raw.is_empty() || raw.contains('\0') || raw.chars().count() > 4096 {
        return Err(PathError::Invalid("path: a file path is required".into()));
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

/// Media type from the extension; unknown extensions are `text/plain` when
/// `sample` is UTF-8 without NUL, else `application/octet-stream`.
pub fn media_type(name: &str, sample: &[u8]) -> String {
    let known = match extension(name).as_str() {
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
        _ => "",
    };
    if !known.is_empty() {
        return known.to_string();
    }
    if looks_like_text(sample) {
        "text/plain".into()
    } else {
        "application/octet-stream".into()
    }
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
        assert_eq!(media_type("a.md", b""), "text/markdown");
        assert_eq!(media_type("a.unknown", b"hello"), "text/plain");
        assert_eq!(media_type("a.unknown", &[0, 1, 2]), "application/octet-stream");
        assert!(looks_like_text("héllo".as_bytes()));
        assert!(looks_like_text(&"é".as_bytes()[..1]), "cut inside a character");
        assert!(!looks_like_text(&[0xff, 0xfe, 0x41]));
        assert!(is_text_media("application/json"));
        assert!(!is_text_media("image/png"));
    }
}
