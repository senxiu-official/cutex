//! Host file read for the owner's clients (root credential or a Management
//! client token), e.g. previewing a report an agent linked by absolute path
//! outside its workspace root.
//!
//! `GET /v2/fs/file?path=<absolute path>&maxBytes=<n>` → the workspace
//! `fs/file` body: `{path, size, modifiedAt, encoding: "utf8" | "base64",
//! content, truncated, mime}`.
//!
//! - `path` must be absolute (no `~` or other expansion); a relative path or
//!   one with control characters is 400 `invalid_path`. The response `path`
//!   is the canonical form (Windows without the `\\?\` prefix).
//! - Regular files only (symlinks are followed): a directory is 400
//!   `not_a_file`, a missing path 404 `path_not_found`, an unreadable one 403
//!   `permission_denied`.
//! - At most `maxBytes` (default 2 MiB, 1..=10 MiB, else 400
//!   `invalid_request`) are returned; `truncated: true` when the file is
//!   larger. Binary content (NUL bytes, invalid UTF-8, images, PDF) comes
//!   back base64 with its mime.
//!
//! - The host's Management root credential file (`<cutex home>/config.json`)
//!   and profile-link storage (`<cutex home>/runtime/profile-link/`, including
//!   atomic-write staging files)
//!   are owner-local-only secrets: a paired device's token
//!   or a remote listener gets 403 `owner_local_only`; only owner-local
//!   requests (`users::request_is_owner_local`: the root credential, or an
//!   owner token issued by `client-token issue` on the host, on loopback)
//!   read it.
//!
//! Never reachable with per-agent Agent Bus credentials or the ordinary
//! Management bridge token: only the root check in `server::handle_v2_request`
//! lets a request through.

use std::collections::HashMap;
use std::net::TcpStream;
use std::path::PathBuf;

use serde_json::Value;

use super::workspace::WorkspaceError;
use super::workspace::WorkspaceResult;

pub const PATH: &str = "/v2/fs/file";

pub fn is_host_files_path(path: &str) -> bool {
    path == PATH
}

pub fn read_host_file(params: &HashMap<String, String>) -> WorkspaceResult<Value> {
    let raw = params
        .get("path")
        .map(|raw| raw.trim())
        .filter(|raw| !raw.is_empty())
        .ok_or_else(|| WorkspaceError::new(400, "invalid_path", "path is required"))?;
    if raw.chars().any(char::is_control) {
        return Err(WorkspaceError::new(
            400,
            "invalid_path",
            "path contains control characters",
        ));
    }
    let requested = PathBuf::from(raw);
    validate_path(&requested)?;
    if !requested.is_absolute() {
        return Err(WorkspaceError::new(
            400,
            "invalid_path",
            "path must be absolute",
        ));
    }
    let max_bytes = super::workspace_fs::max_bytes_param(params)?;
    let canonical = std::fs::canonicalize(&requested).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => {
            WorkspaceError::new(404, "path_not_found", "the path does not exist")
        }
        std::io::ErrorKind::PermissionDenied => {
            WorkspaceError::new(403, "permission_denied", "the path cannot be read")
        }
        _ => WorkspaceError::new(
            400,
            "invalid_path",
            format!("the path cannot be resolved: {error}"),
        ),
    })?;
    refuse_root_credential_file(&canonical)?;
    let meta = std::fs::metadata(&canonical).map_err(WorkspaceError::internal)?;
    if !meta.is_file() {
        return Err(WorkspaceError::new(
            400,
            "not_a_file",
            "the path is not a regular file",
        ));
    }
    let display = super::workspace::display_path(&canonical);
    super::workspace_fs::file_content_value(&canonical, &display, &meta, max_bytes)
}

/// 403 `owner_local_only` (`readRootCredential`) when `path` is the root
/// credential or inside profile-link storage and the request is not owner-local. Also used by the
/// workspace file read (`workspace_fs::file_resource`): a workspace rooted
/// at the home directory must not hand it out either.
pub(crate) fn refuse_root_credential_file(path: &std::path::Path) -> WorkspaceResult<()> {
    crate::artifacts::paths::validate_resolved_local_path(path).map_err(|_|WorkspaceError::new(403,"path_forbidden","network, device and alternate-stream paths are unavailable"))?;
    // Owner-local reads are already allowed. Still validate path transport above,
    // but do not classify every file in an owner-local tree/status request.
    if crate::management::users::request_is_owner_local() { return Ok(()); }
    if secret_path_cache::classify_validated(path, false)
    {
        let refused = crate::management::users::owner_local_only("readRootCredential");
        return Err(WorkspaceError::new(403, refused.code, refused.message));
    }
    Ok(())
}

// Cached IDs only add denials: reuse of a retired ID can conservatively deny
// an ordinary file, never allow a private namespace. Do not keep private files
// open in the cache: that prevents Windows from moving their parent directory.
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    volume: u64,
    id: u128,
}
#[cfg(test)]
fn same_file_identity(a: impl AsRef<std::path::Path>, b: impl AsRef<std::path::Path>) -> bool {
    record_identity_read(a.as_ref());
    record_identity_read(b.as_ref());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    { same_file::is_same_file(a, b).unwrap_or(false) }
}

#[cfg(test)]
thread_local! {
    static IDENTITY_READS: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}
#[cfg(test)]
fn record_identity_read(path: &std::path::Path) {
    IDENTITY_READS.with(|n| {
        let (all, relay_root) = n.get();
        n.set((all + 1, relay_root + usize::from(path.file_name().is_some_and(|s| s == "management-relay"))));
    });
}

#[path = "secret_path_cache.rs"]
mod secret_path_cache;

pub(crate) fn prime_secret_path_cache() { secret_path_cache::prime(); }
pub(crate) fn refresh_secret_path_cache() { secret_path_cache::refresh(); }

pub(crate) fn is_relay_private_path(path: &std::path::Path) -> bool {
    if crate::artifacts::paths::validate_resolved_local_path(path).is_err() { return true; }
    secret_path_cache::classify_validated(path, true)
}
pub(crate) fn validate_path(path: &std::path::Path) -> WorkspaceResult<()> {
    crate::artifacts::paths::validate_local_path(path).map_err(|_|WorkspaceError::new(403,"path_forbidden","network, device and alternate-stream paths are unavailable"))
}
pub(crate) fn refuse_private_tree(path: &std::path::Path) -> WorkspaceResult<()> {
    refuse_root_credential_file(path)?;
    if !crate::management::users::request_is_owner_local() {
        let canonical=std::fs::canonicalize(path).unwrap_or_else(|_|path.to_path_buf());
        if crate::config::paths::config_dir().ok().map(|p|p.join("management-relay")).and_then(|p|std::fs::canonicalize(p).ok()).is_some_and(|secret|secret.starts_with(&canonical)) {
            let e=crate::management::users::owner_local_only("readRelayPrivateTree");return Err(WorkspaceError::new(403,e.code,e.message));
        }
    }
    Ok(())
}

/// Single final-handle check against cached private namespaces and file IDs.
/// A1's pre-open network/reparse check is separate and remains unconditional.
pub(crate) fn is_owner_local_secret_file(path: &std::path::Path) -> bool {
    if crate::artifacts::paths::validate_resolved_local_path(path).is_err() { return true; }
    secret_path_cache::classify_validated(path, false)
}

/// A concurrent DELETE (or first PUT) can leave the protected leaf or directory
/// absent after the requested path was resolved. Classify its fixed namespace
/// anyway, resolving existing ancestors so instance-root aliases still work.
fn canonicalize_existing_prefix(path: &std::path::Path) -> Option<PathBuf> {
    let mut prefix = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(&prefix) {
            Ok(mut canonical) => {
                for component in missing.into_iter().rev() {
                    canonical.push(component);
                }
                return Some(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let component = prefix.file_name()?.to_os_string();
                if !prefix.pop() {
                    return None;
                }
                missing.push(component);
            }
            Err(_) => return None,
        }
    }
}

pub fn handle(
    stream: &mut TcpStream,
    request: &crate::http::server::SimpleHttpRequest,
) -> anyhow::Result<()> {
    if request.method != "GET" {
        return super::workspace::write_error(
            stream,
            &WorkspaceError::new(405, "method_not_allowed", "use GET"),
        );
    }
    let params = super::workspace::query_params(request);
    match read_host_file(&params) {
        Ok(value) => crate::http::server::write_json_response(stream, 200, "OK", &value),
        Err(error) => super::workspace::write_error(stream, &error),
    }
}

#[cfg(test)]
#[path = "relay_path_hot_tests.rs"]
mod cost_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn relay_path_hot_static_reads_root_identity_at_most_once() {
        let home = crate::profiles::lifecycle::tests::TestHome::new();
        let relay = crate::config::paths::config_dir().unwrap().join("management-relay");
        std::fs::create_dir_all(&relay).unwrap();
        std::fs::write(relay.join("identity.json"), "synthetic-key").unwrap();
        let root = home.root().join("public");
        let nested = root.join("a/b/c/d/e/f/g/h");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("ok.txt"), "public").unwrap();
        let root = root.canonicalize().unwrap();
        IDENTITY_READS.with(|n| n.set((0, 0)));
        assert!(crate::services::static_files::resolve(&root, "/a/b/c/d/e/f/g/h/ok.txt").is_some());
        let (all, relay_root) = IDENTITY_READS.with(|n| n.get());
        eprintln!("relay_path_hot static identity_reads={all} relay_root_reads={relay_root}");
        assert!(relay_root <= 1, "one classification must read the relay root identity once, got {relay_root}");
    }

    #[cfg(unix)]
    #[test]
    fn secret_identity_checks_preserve_hardlinks_without_opening_fifos() {
        let dir = std::env::temp_dir().join(format!("cutex-file-identity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let original = dir.join("original");
        let alias = dir.join("alias");
        let pipe = dir.join("pipe");
        std::fs::write(&original, b"synthetic secret").unwrap();
        std::fs::hard_link(&original, &alias).unwrap();
        let name = std::ffi::CString::new(pipe.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(same_file_identity(&original, &alias));
        assert!(!same_file_identity(&original, &pipe));
        assert!(same_file_identity(&pipe, &pipe));
        assert!(!same_file_identity(&original, dir.join("absent")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg(windows)]
    fn round2_all_filesystem_entry_points_reject_aliases_before_io() {
        use super::super::{host_dirs,workspace::{ResolvedRoot,Scope}};
        let root=crate::config::paths::home_dir().unwrap();
        for alias in [r"\\localhost\C$\cutex-review-fixture",r"\\127.0.0.1\C$\cutex-review-fixture",r"\\?\UNC\localhost\C$\fixture",r"\\?\C:\fixture",r"\\.\C:\fixture",r"C:\fixture::$DATA"] {
            assert_eq!(read_host_file(&params(alias,None)).unwrap_err().status,403,"host file {alias}");
            if !alias.starts_with(r"\\?\C:") {
                assert!(refuse_root_credential_file(std::path::Path::new(alias)).is_err(),"guard {alias}");
                assert!(is_relay_private_path(std::path::Path::new(alias)),"private check must fail closed {alias}");
            }
            assert_eq!(host_dirs::list_dirs(Some(alias),Some(&root),10).unwrap_err().status,403,"host dirs {alias}");
            assert_eq!(ResolvedRoot::from_dir(Scope {kind:super::super::workspace::ScopeKind::Session,id:"fixture".into()},std::path::Path::new(alias)).unwrap_err().status,403,"workspace root {alias}");
            assert_eq!(crate::services::static_files::check_root(alias,&root.join(".cutex/services"),&crate::services::store::Caller::Owner).unwrap_err().status,403,"static root {alias}");
        }
    }

    struct RequestContextGuard {
        connection: Option<crate::management::listeners::ConnectionInfo>,
        token: Option<crate::management::client_tokens::VerifiedClientToken>,
        principal: Option<crate::management::users::Principal>,
    }

    impl RequestContextGuard {
        fn new() -> Self {
            Self {
                connection: crate::management::listeners::current_connection(),
                token: crate::management::listeners::current_client_token(),
                principal: crate::management::users::current_principal(),
            }
        }

        fn request(
            &self,
            remote: bool,
            issued: Option<crate::management::client_tokens::IssuedVia>,
        ) {
            use crate::management::{client_tokens::VerifiedClientToken, listeners, users};
            listeners::set_current_connection(Some(listeners::ConnectionInfo {
                remote,
                peer: if remote {
                    "192.0.2.1:12345"
                } else {
                    "127.0.0.1:12345"
                }
                .parse()
                .unwrap(),
            }));
            let token = issued.map(|issued_via| {
                let mut token = VerifiedClientToken::test_token("synthetic-owner-full");
                token.issued_via = issued_via;
                token
            });
            listeners::set_current_client_token(token);
            users::set_current_principal(Some(users::Principal::Owner));
        }
    }

    impl Drop for RequestContextGuard {
        fn drop(&mut self) {
            crate::management::listeners::set_current_connection(self.connection.take());
            crate::management::listeners::set_current_client_token(self.token.take());
            crate::management::users::set_current_principal(self.principal.take());
        }
    }

    const SYNTHETIC_LINK_SECRET: &str = "synthetic-profile-link-secret-fixture";

    fn secret_fixture() -> (
        crate::profiles::lifecycle::tests::TestHome,
        super::super::workspace::ResolvedRoot,
    ) {
        use super::super::workspace::{ResolvedRoot, Scope, ScopeKind};
        let home = crate::profiles::lifecycle::tests::TestHome::new();
        std::fs::write(
            home.root().join(crate::config::paths::PRIVATE_HOME_MARKER),
            b"",
        )
        .unwrap();
        let config = crate::config::paths::config_dir().unwrap();
        assert!(
            config.starts_with(home.root()),
            "fixture must resolve to its own private home"
        );
        std::fs::create_dir_all(config.join("runtime/profile-link")).unwrap();
        std::fs::write(
            config.join("runtime/profile-link/secret-v1"),
            SYNTHETIC_LINK_SECRET,
        )
        .unwrap();
        std::fs::write(config.join("ordinary-report.txt"), "synthetic report").unwrap();
        let root = ResolvedRoot::from_dir(
            Scope {
                kind: ScopeKind::Project,
                id: "synthetic-workspace".into(),
            },
            &config,
        )
        .unwrap();
        (home, root)
    }

    fn assert_secret_reads(
        root: &super::super::workspace::ResolvedRoot,
        relative: &str,
        allowed: bool,
    ) {
        let file = root.canonical.join(relative);
        let host = read_host_file(&params(&super::super::workspace::display_path(&file), None));
        let workspace = super::super::workspace_fs::file_resource(root, &params(relative, None));
        for result in [host, workspace] {
            if allowed {
                assert_eq!(result.unwrap()["content"], SYNTHETIC_LINK_SECRET);
            } else {
                let error = result.unwrap_err();
                assert_eq!((error.status, error.code), (403, "owner_local_only"));
                assert!(!error.message.contains(SYNTHETIC_LINK_SECRET));
            }
        }
    }

    #[test]
    fn profile_link_secret_refuses_paired_owner_full_and_remote_reads_on_both_routes() {
        use crate::management::client_tokens::IssuedVia;
        let (_home, root) = secret_fixture();
        let context = RequestContextGuard::new();
        for (remote, issued) in [
            (false, Some(IssuedVia::Pairing)),
            (true, Some(IssuedVia::Pairing)),
            (true, Some(IssuedVia::HostCli)),
            (true, None),
        ] {
            context.request(remote, issued);
            assert_secret_reads(&root, "runtime/profile-link/secret-v1", false);
            assert_secret_reads(&root, "runtime/profile-link/./secret-v1", false);
            assert!(read_host_file(&params(
                &super::super::workspace::display_path(&root.canonical.join("ordinary-report.txt")),
                None
            ))
            .is_ok());
            assert!(super::super::workspace_fs::file_resource(
                &root,
                &params("ordinary-report.txt", None)
            )
            .is_ok());
        }
    }

    #[test]
    fn profile_link_secret_keeps_local_root_and_host_cli_reads_on_both_routes() {
        use crate::management::client_tokens::IssuedVia;
        let (_home, root) = secret_fixture();
        let context = RequestContextGuard::new();
        for issued in [None, Some(IssuedVia::HostCli)] {
            context.request(false, issued);
            assert_secret_reads(&root, "runtime/profile-link/secret-v1", true);
            assert_secret_reads(&root, "runtime/profile-link/./secret-v1", true);
        }
    }

    #[test]
    fn profile_link_staging_files_are_owner_local_on_both_routes_without_prefix_overreach() {
        use crate::management::client_tokens::IssuedVia;
        let (_home, root) = secret_fixture();
        let context = RequestContextGuard::new();
        let staging = "runtime/profile-link/.secret-v1.tmp-12345-synthetic";
        std::fs::write(root.canonical.join(staging), SYNTHETIC_LINK_SECRET).unwrap();
        let sibling = "runtime/profile-link-backup/report.txt";
        std::fs::create_dir_all(root.canonical.join("runtime/profile-link-backup")).unwrap();
        std::fs::write(root.canonical.join(sibling), SYNTHETIC_LINK_SECRET).unwrap();
        for (remote, issued) in [
            (false, Some(IssuedVia::Pairing)),
            (true, Some(IssuedVia::HostCli)),
        ] {
            context.request(remote, issued);
            assert_secret_reads(&root, staging, false);
            assert_secret_reads(
                &root,
                "runtime/profile-link/./.secret-v1.tmp-12345-synthetic",
                false,
            );
            // Path::starts_with is component-aware; a similar sibling name is
            // an ordinary readable directory, not part of secret storage.
            assert_secret_reads(&root, sibling, true);
        }
        context.request(false, None);
        assert_secret_reads(&root, staging, true);
    }

    #[test]
    fn profile_link_classification_survives_a_missing_leaf_and_missing_directory() {
        use crate::management::client_tokens::IssuedVia;
        let (_home, root) = secret_fixture();
        let context = RequestContextGuard::new();
        let captured =
            std::fs::canonicalize(root.canonical.join("runtime/profile-link/secret-v1")).unwrap();
        std::fs::remove_file(&captured).unwrap();
        context.request(false, Some(IssuedVia::Pairing));
        assert_eq!(
            refuse_root_credential_file(&captured).unwrap_err().code,
            "owner_local_only"
        );
        std::fs::remove_dir(root.canonical.join("runtime/profile-link")).unwrap();
        // The directory is temporarily gone too: resolve its existing prefix
        // and append its fixed missing component instead of dropping protection.
        assert_eq!(
            refuse_root_credential_file(&captured).unwrap_err().code,
            "owner_local_only"
        );
        std::fs::create_dir(root.canonical.join("runtime/profile-link")).unwrap();
        std::fs::write(&captured, SYNTHETIC_LINK_SECRET).unwrap();
        assert_secret_reads(&root, "runtime/profile-link/secret-v1", false);
        context.request(false, None);
        assert_secret_reads(&root, "runtime/profile-link/secret-v1", true);
    }

    #[cfg(unix)]
    #[test]
    fn profile_link_staging_file_and_directory_aliases_are_owner_local() {
        use crate::management::client_tokens::IssuedVia;
        let (_home, root) = secret_fixture();
        let context = RequestContextGuard::new();
        let staging = root
            .canonical
            .join("runtime/profile-link/.secret-v1.tmp-12345-synthetic");
        std::fs::write(&staging, SYNTHETIC_LINK_SECRET).unwrap();
        std::os::unix::fs::symlink(&staging, root.canonical.join("staging-alias")).unwrap();
        std::os::unix::fs::symlink(
            root.canonical.join("runtime/profile-link"),
            root.canonical.join("storage-alias"),
        )
        .unwrap();
        context.request(false, Some(IssuedVia::Pairing));
        assert_secret_reads(&root, "staging-alias", false);
        assert_secret_reads(&root, "storage-alias/.secret-v1.tmp-12345-synthetic", false);
        context.request(false, None);
        assert_secret_reads(&root, "staging-alias", true);
        assert_secret_reads(&root, "storage-alias/.secret-v1.tmp-12345-synthetic", true);
    }

    #[cfg(unix)]
    #[test]
    fn profile_link_secret_symlink_alias_uses_the_same_owner_local_guard() {
        use crate::management::client_tokens::IssuedVia;
        let (_home, root) = secret_fixture();
        let context = RequestContextGuard::new();
        std::os::unix::fs::symlink(
            root.canonical.join("runtime/profile-link/secret-v1"),
            root.canonical.join("secret-alias"),
        )
        .unwrap();
        context.request(false, Some(IssuedVia::Pairing));
        assert_secret_reads(&root, "secret-alias", false);
        context.request(false, None);
        assert_secret_reads(&root, "secret-alias", true);
        let final_path = root.canonical.join("runtime/profile-link/secret-v1");
        std::fs::remove_file(&final_path).unwrap();
        let outside = root.canonical.join("synthetic-linked-target");
        std::fs::write(&outside, SYNTHETIC_LINK_SECRET).unwrap();
        std::os::unix::fs::symlink(&outside, final_path).unwrap();
        context.request(false, Some(IssuedVia::Pairing));
        assert_secret_reads(&root, "synthetic-linked-target", false);
        context.request(false, None);
        assert_secret_reads(&root, "synthetic-linked-target", true);
    }

    #[test]
    fn a1_secret_guards_work_without_relay_setup_on_ordinary_listeners() {
        use crate::management::client_tokens::IssuedVia;
        let (_home,root)=secret_fixture();let context=RequestContextGuard::new();
        let dir=root.canonical.join("management-relay");assert!(!dir.exists());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("identity.json"),"synthetic inactive relay key fixture").unwrap();
        std::fs::write(root.canonical.join("config.json"),"synthetic root credential fixture").unwrap();
        // No setup, valid relay identity or TLS listener is involved. The path
        // guard also protects ordinary loopback/tailnet device access.
        let previous=crate::management::relay::current_peer();crate::management::relay::set_peer(None);
        for remote in [false,true] {
            context.request(remote,Some(IssuedVia::Pairing));
            for name in ["config.json","management-relay/identity.json"] {
                let path=super::super::workspace::display_path(&root.canonical.join(name));
                let error=read_host_file(&params(&path,None)).unwrap_err();
                assert_eq!((error.status,error.code),(403,"owner_local_only"));
            }
            let ordinary=super::super::workspace::display_path(&root.canonical.join("ordinary-report.txt"));
            assert_eq!(read_host_file(&params(&ordinary,None)).unwrap()["content"],"synthetic report");
        }
        crate::management::relay::set_peer(previous);
    }

    fn params(path: &str, max: Option<&str>) -> HashMap<String, String> {
        let mut params = HashMap::from([("path".to_string(), path.to_string())]);
        if let Some(max) = max {
            params.insert("maxBytes".to_string(), max.to_string());
        }
        params
    }

    fn tree() -> PathBuf {
        let root = std::env::temp_dir().join(format!("cutex-host-files-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("dir")).unwrap();
        std::fs::write(root.join("report.md"), "# Issue\nline 2\n").unwrap();
        std::fs::write(root.join("blob.bin"), [0u8, 1, 2, 3, 0, 255]).unwrap();
        std::fs::write(root.join("pic.png"), [0x89u8, b'P', b'N', b'G']).unwrap();
        root
    }

    #[test]
    fn reads_text_with_canonical_path_and_truncation() {
        let root = tree();
        let file = root.join("report.md");
        let value = read_host_file(&params(file.to_str().unwrap(), None)).unwrap();
        assert_eq!(value["encoding"], "utf8");
        assert_eq!(value["content"], "# Issue\nline 2\n");
        assert_eq!(value["truncated"], false);
        assert_eq!(value["mime"], "text/markdown");
        assert_eq!(value["size"], 15);
        let canonical =
            super::super::workspace::display_path(&std::fs::canonicalize(&file).unwrap());
        assert_eq!(value["path"], canonical);
        assert!(value["modifiedAt"].is_string());
        let value = read_host_file(&params(file.to_str().unwrap(), Some("7"))).unwrap();
        assert_eq!(value["content"], "# Issue");
        assert_eq!(value["truncated"], true);
        assert_eq!(value["size"], 15);
        // A non-canonical spelling resolves to the same file.
        let dotted = root.join("dir").join("..").join("report.md");
        let value = read_host_file(&params(dotted.to_str().unwrap(), None)).unwrap();
        assert_eq!(value["path"], canonical);
    }

    #[test]
    fn binary_and_images_come_back_base64() {
        use base64::Engine;
        let root = tree();
        let value = read_host_file(&params(root.join("blob.bin").to_str().unwrap(), None)).unwrap();
        assert_eq!(value["encoding"], "base64");
        assert_eq!(value["mime"], "application/octet-stream");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(value["content"].as_str().unwrap())
            .unwrap();
        assert_eq!(bytes, [0u8, 1, 2, 3, 0, 255]);
        let value = read_host_file(&params(root.join("pic.png").to_str().unwrap(), None)).unwrap();
        assert_eq!(
            (value["encoding"].as_str(), value["mime"].as_str()),
            (Some("base64"), Some("image/png"))
        );
    }

    /// The route sits behind the root gate in `handle_v2_request`: the root
    /// credential and Management client tokens pass; per-agent Agent Bus
    /// credentials, the ordinary bridge token and anonymous callers do not,
    /// and the Agent Bus never routes it for an agent.
    #[test]
    fn only_the_root_credential_or_a_client_token_passes_the_gate() {
        use crate::agent_bus::agent_auth::{agent_credential, agent_route_allowed};
        use crate::http::server::SimpleHttpRequest;
        use crate::management::client_tokens::{
            authenticate_management_request_with, ClientTokenStore,
        };
        use crate::management::v2::server::{
            authenticated_root_request, is_remote_client_root_path,
        };

        assert!(is_remote_client_root_path(PATH));
        assert!(!super::super::workspace::is_workspace_path(PATH));
        let root = "management-root-secret";
        let store = ClientTokenStore::at(
            std::env::temp_dir()
                .join(format!("cutex-host-files-tokens-{}", uuid::Uuid::new_v4()))
                .join("management-clients"),
        );
        let issued = store.issue("desktop").unwrap();
        let allowed = |bearer: Option<&str>, agent_id: Option<&str>| {
            let mut headers = HashMap::new();
            if let Some(bearer) = bearer {
                headers.insert("authorization".to_string(), format!("Bearer {bearer}"));
            }
            if let Some(agent_id) = agent_id {
                headers.insert("x-cutex-agent-id".to_string(), agent_id.to_string());
            }
            let mut request = SimpleHttpRequest {
                method: "GET".into(),
                path: format!("{PATH}?path=%2Fetc%2Fhosts"),
                headers,
                body: Vec::new(),
            };
            authenticate_management_request_with(&mut request, Some(root), &store);
            authenticated_root_request(&request, Some(root))
        };
        assert!(allowed(Some(root), None));
        assert!(allowed(Some(&issued.token), None));
        // Per-agent credentials, even one derived from the same secret.
        let agent = agent_credential(root, "agent-1", "cutex.s1");
        assert!(!allowed(Some(&agent), Some("agent-1")));
        let agent = agent_credential("agent-bus-root", "agent-1", "cutex.s1");
        assert!(!allowed(Some(&agent), Some("agent-1")));
        assert!(!allowed(Some("management-bridge-token"), None));
        assert!(!allowed(Some("cxm1.forged.token"), None));
        assert!(!allowed(None, None));
        store.revoke(&issued.token_id).unwrap();
        assert!(!allowed(Some(&issued.token), None));
        let fenced = HashMap::from([
            ("x-cutex-agent-id".to_string(), "agent-1".to_string()),
            (
                crate::agent_bus::mcp::THREAD_HEADER.to_string(),
                "t".to_string(),
            ),
            (
                crate::agent_bus::mcp::GENERATION_HEADER.to_string(),
                "1".to_string(),
            ),
        ]);
        assert!(!agent_route_allowed("GET", PATH, &fenced));
        assert!(!agent_route_allowed("POST", PATH, &fenced));
    }

    #[test]
    fn refuses_relative_missing_directory_and_bad_limits() {
        let root = tree();
        let code = |path: &str, max: Option<&str>| {
            let error = read_host_file(&params(path, max)).unwrap_err();
            (error.status, error.code)
        };
        assert_eq!(code("report.md", None), (400, "invalid_path"));
        assert_eq!(code("~/report.md", None), (400, "invalid_path"));
        assert_eq!(code("", None), (400, "invalid_path"));
        assert_eq!(code("/tmp/a\nb", None), (400, "invalid_path"));
        assert_eq!(
            code(root.join("missing.md").to_str().unwrap(), None),
            (404, "path_not_found")
        );
        assert_eq!(
            code(root.join("dir").to_str().unwrap(), None),
            (400, "not_a_file")
        );
        let file = root.join("report.md");
        assert_eq!(
            code(file.to_str().unwrap(), Some("0")),
            (400, "invalid_request")
        );
        assert_eq!(
            code(file.to_str().unwrap(), Some("99999999999")),
            (400, "invalid_request")
        );
        assert!(read_host_file(&HashMap::new()).is_err());
    }
}
