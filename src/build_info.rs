//! Build identity of this cutex binary.
//!
//! `Cargo.toml`'s version never moves; a release is named by its label
//! (`r224`) and commit. Release kits build from an LF `git archive` export
//! (no `.git`), so the identity comes from optional build-time environment
//! variables read with `option_env!` (no build script work, no extra
//! rebuilds; rustc tracks them and recompiles when they change):
//!
//! - `CUTEX_RELEASE` — release label, e.g. `r224`;
//! - `CUTEX_COMMIT` — full commit sha (`CUTEX_GIT_SHA`, the older name, is
//!   still honoured);
//! - `CUTEX_BUILD_TIME` — free-form build time, e.g. RFC 3339;
//! - `CUTEX_CHANNEL` — `release` for public (colleague) releases; `main`
//!   (the default when unset, as on our own hosts) otherwise. The older
//!   value `stable` is read as an alias of `release`, `dev` of `main`
//!   (cutex-desktop backend-auto-update RFC §10.2: channel names match the
//!   desktop `release` / `main` branches).
//!
//! Dev builds set none of them; clients then show "dev build".

use serde_json::{json, Value};
use std::sync::OnceLock;

const fn non_empty(value: Option<&'static str>) -> Option<&'static str> {
    match value {
        Some(text) if !text.is_empty() => Some(text),
        _ => None,
    }
}

const fn first(a: Option<&'static str>, b: Option<&'static str>) -> Option<&'static str> {
    match a {
        Some(_) => a,
        None => b,
    }
}

/// `Cargo.toml` package version (not a release id).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const RELEASE: Option<&str> = non_empty(option_env!("CUTEX_RELEASE"));
pub const COMMIT: Option<&str> =
    first(non_empty(option_env!("CUTEX_COMMIT")), non_empty(option_env!("CUTEX_GIT_SHA")));
pub const BUILT_AT: Option<&str> = non_empty(option_env!("CUTEX_BUILD_TIME"));
const CHANNEL_ENV: Option<&str> = non_empty(option_env!("CUTEX_CHANNEL"));
pub const PROFILE: &str = if cfg!(debug_assertions) { "debug" } else { "release" };

pub const CHANNEL_RELEASE: &str = "release";
pub const CHANNEL_MAIN: &str = "main";

/// `release` or `main` (the default).
pub fn channel() -> &'static str {
    channel_for(CHANNEL_ENV)
}

/// A known channel name: `release` (alias `stable`) or `main` (alias
/// `dev`), case-insensitive; `None` for anything else.
pub fn normalize_channel(value: &str) -> Option<&'static str> {
    let value = value.trim();
    if value.eq_ignore_ascii_case(CHANNEL_RELEASE) || value.eq_ignore_ascii_case("stable") {
        Some(CHANNEL_RELEASE)
    } else if value.eq_ignore_ascii_case(CHANNEL_MAIN) || value.eq_ignore_ascii_case("dev") {
        Some(CHANNEL_MAIN)
    } else {
        None
    }
}

fn channel_for(value: Option<&str>) -> &'static str {
    value.and_then(normalize_channel).unwrap_or(CHANNEL_MAIN)
}

fn short_commit(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

fn version_line_for(release: Option<&str>, commit: Option<&str>) -> String {
    let parts: Vec<&str> = release.into_iter().chain(commit.map(short_commit)).collect();
    if parts.is_empty() {
        VERSION.to_string()
    } else {
        format!("{VERSION} ({})", parts.join(", "))
    }
}

/// `cutex --version` text after the binary name: `0.1.0 (r224, 5f30a7c)`,
/// `0.1.0 (5f30a7c)` or plain `0.1.0`.
pub fn version_line() -> &'static str {
    static LINE: OnceLock<String> = OnceLock::new();
    LINE.get_or_init(|| version_line_for(RELEASE, COMMIT))
}

/// `cutexBuild` in `/v2/capabilities` and the schema index, `cutex.build` in
/// `/v2/runtimes`: `{version, release, commit, builtAt, profile, channel}`
/// (release / commit / builtAt null when the build did not set them).
pub fn build_json() -> Value {
    json!({
        "version": VERSION,
        "release": RELEASE,
        "commit": COMMIT,
        "builtAt": BUILT_AT,
        "profile": PROFILE,
        "channel": channel(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_shapes() {
        let sha = "5f30a7cad4dcd8435e797592a380349df509e35d";
        assert_eq!(version_line_for(Some("r224"), Some(sha)), format!("{VERSION} (r224, 5f30a7c)"));
        assert_eq!(version_line_for(None, Some(sha)), format!("{VERSION} (5f30a7c)"));
        assert_eq!(version_line_for(Some("r224"), None), format!("{VERSION} (r224)"));
        assert_eq!(version_line_for(None, None), VERSION);
        assert_eq!(version_line_for(None, Some("abc")), format!("{VERSION} (abc)"));
    }

    #[test]
    fn build_json_shape() {
        let value = build_json();
        assert_eq!(value["version"], VERSION);
        assert_eq!(value["profile"], if cfg!(debug_assertions) { "debug" } else { "release" });
        assert!(value["channel"] == "main" || value["channel"] == "release");
        for key in ["release", "commit", "builtAt"] {
            assert!(value[key].is_null() || value[key].as_str().is_some_and(|s| !s.is_empty()), "{key}");
        }
    }

    #[test]
    fn channel_defaults_to_main_and_reads_old_names() {
        assert_eq!(channel_for(None), "main");
        assert_eq!(channel_for(Some("main")), "main");
        assert_eq!(channel_for(Some("dev")), "main");
        assert_eq!(channel_for(Some("beta")), "main");
        assert_eq!(channel_for(Some("release")), "release");
        assert_eq!(channel_for(Some(" Release ")), "release");
        // Old public name: an alias of release.
        assert_eq!(channel_for(Some("stable")), "release");
        assert_eq!(channel_for(Some(" Stable ")), "release");
        assert_eq!(normalize_channel("beta"), None);
    }

    #[test]
    fn non_empty_and_first() {
        assert_eq!(non_empty(Some("")), None);
        assert_eq!(non_empty(Some("x")), Some("x"));
        assert_eq!(first(None, Some("b")), Some("b"));
        assert_eq!(first(Some("a"), Some("b")), Some("a"));
    }
}
