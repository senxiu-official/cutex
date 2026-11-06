//! Central GET health caching, using no real handler probes or subprocesses.
use super::*;
use crate::upgrade::stage::fixtures::installed_layout;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

struct CountingHandler {
    metadata: Metadata,
    probes: AtomicUsize,
}

impl CountingHandler {
    fn new(name: &str) -> Self {
        Self {
            metadata: Metadata {
                schema: COMPONENT_SCHEMA.into(),
                name: name.into(),
                version: "legacy-r2".into(),
                platforms: vec![platform_key()],
            },
            probes: AtomicUsize::new(0),
        }
    }

    fn detail_key(&self) -> &str {
        if self.metadata.name == "dsh-runtime" {
            "dsh"
        } else {
            "kimi"
        }
    }

    fn calls(&self) -> usize {
        self.probes.load(Ordering::SeqCst)
    }
}

impl Handler for CountingHandler {
    fn metadata(&self) -> &Metadata {
        &self.metadata
    }
    fn required_files(&self) -> &'static [&'static str] {
        &["tool"]
    }
    fn from_source(&self, _: &Path, _: &Path) -> anyhow::Result<()> {
        Ok(())
    }
    fn external(&self, _: &Layout) -> bool {
        false
    }
    fn prepare(&self, _: &Context) -> anyhow::Result<Value> {
        Ok(json!({}))
    }
    fn select(&self, _: &Context) -> anyhow::Result<Value> {
        Ok(json!({}))
    }
    fn deselect(&self, _: &Context, _: bool) -> anyhow::Result<Vec<String>> {
        Ok(vec![])
    }
    fn probe(&self, _: &Layout, _: &Entry) -> Value {
        self.probes.fetch_add(1, Ordering::SeqCst);
        let mut detail = json!({"state":states::NEEDS_USER,"sentinel":"explicit-fake-probe",
            "steps":["fixture-only repair instruction"]});
        detail[self.detail_key()] =
            json!({"version":"fixture-upstream", "node":{"version":"fixture-node","ready":true}});
        detail
    }
    fn pins(&self, _: &Layout) -> BTreeMap<String, usize> {
        BTreeMap::new()
    }
    fn restart_gate(&self, _: &Layout) -> Result<(), String> {
        Ok(())
    }
}

fn installed(name: &str) -> (Layout, CountingHandler) {
    let layout = installed_layout("0.2.0", 0);
    let handler = CountingHandler::new(name);
    let version = "legacy-r2";
    let dir = version_dir(&layout, name, version);
    std::fs::create_dir_all(&dir).unwrap();
    let tool = b"inert fake bundled tool; never executed";
    std::fs::write(dir.join("tool"), tool).unwrap();
    let receipt = Receipt {
        schema: RECEIPT_SCHEMA.into(),
        name: name.into(),
        version: version.into(),
        origin: ORIGIN_SIGNED.into(),
        archive_sha256: None,
        selection_source: None,
        files: BTreeMap::from([("tool".into(), crate::upgrade::manifest::sha256_hex(tool))]),
        unpacked_at: "2026-10-29T00:00:00Z".into(),
    };
    std::fs::write(
        dir.join(RECEIPT_FILE),
        serde_json::to_vec(&receipt).unwrap(),
    )
    .unwrap();
    save_state(
        &layout,
        &BTreeMap::from([(
            name.into(),
            Entry {
                selected: version.into(),
                origin: ORIGIN_SIGNED.into(),
                installed_at: "2026-10-29T00:00:00Z".into(),
                last_error: None,
            },
        )]),
    )
    .unwrap();
    (layout, handler)
}

// Include directory/file identities, bytes and mtimes, so a GET cannot silently
// create a cache or atomically rewrite an existing file with identical contents.
type Tree = BTreeMap<PathBuf, (String, Vec<u8>, SystemTime)>;
fn snapshot(root: &Path) -> Tree {
    let mut tree = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        let kind = metadata.file_type();
        let (label, bytes) = if kind.is_symlink() {
            (
                "symlink",
                std::fs::read_link(&path)
                    .unwrap()
                    .to_string_lossy()
                    .as_bytes()
                    .to_vec(),
            )
        } else if kind.is_dir() {
            pending.extend(
                std::fs::read_dir(&path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path()),
            );
            ("directory", vec![])
        } else {
            ("file", std::fs::read(&path).unwrap())
        };
        tree.insert(
            path.strip_prefix(root).unwrap().to_path_buf(),
            (label.into(), bytes, metadata.modified().unwrap()),
        );
    }
    tree
}

#[test]
fn full_status_get_without_legacy_health_cache_never_probes_or_writes() {
    for name in ["dsh-runtime", "kimi-runtime"] {
        let (layout, handler) = installed(name);
        let before = snapshot(&layout.base);
        for _ in 0..2 {
            let row = status_of(Some(&layout), &[], &handler, None);
            assert_eq!(row["selected"], "legacy-r2");
            assert_eq!(row["state"], states::READY);
            assert_eq!(row["detail"]["health"], "unchecked");
            assert_eq!(row["detail"]["checkedAt"], Value::Null);
            let requirements = if name == "dsh-runtime" {
                dsh_runtime::requirements()
            } else {
                kimi_runtime::requirements()
            };
            assert_eq!(row[handler.detail_key()], requirements,
                "uncached complete status must use static requirements, not the old DSH Node fallback");
            assert_eq!(row[handler.detail_key()]["node"]["version"], Value::Null);
            assert_eq!(handler.calls(), 0);
            assert!(!health_path(&layout, name).exists());
            assert_eq!(snapshot(&layout.base), before);
        }
    }
}

#[test]
fn explicit_refresh_probes_once_and_subsequent_full_status_uses_persisted_details() {
    for name in ["dsh-runtime", "kimi-runtime"] {
        let (layout, handler) = installed(name);
        let detail = refresh_health(&layout, &handler).unwrap();
        assert_eq!(handler.calls(), 1);
        let cache: Value =
            serde_json::from_slice(&std::fs::read(health_path(&layout, name)).unwrap()).unwrap();
        assert_eq!(cache["version"], "legacy-r2");
        assert!(cache["checkedAt"]
            .as_str()
            .is_some_and(|value| !value.is_empty()));
        assert_eq!(cache["probe"], detail);
        let before = snapshot(&layout.base);
        for _ in 0..3 {
            let row = status_of(Some(&layout), &[], &handler, None);
            assert_eq!(row["state"], states::NEEDS_USER);
            assert_eq!(row["detail"], detail);
            assert_eq!(row["steps"], detail["steps"]);
            assert_eq!(row[handler.detail_key()], detail[handler.detail_key()]);
            assert_eq!(handler.calls(), 1, "GET must not refresh a persisted probe");
            assert_eq!(snapshot(&layout.base), before);
        }
    }
}

#[test]
fn missing_required_tool_overrides_cached_health_without_probing_or_writing() {
    for name in ["dsh-runtime", "kimi-runtime"] {
        let (layout, handler) = installed(name);
        refresh_health(&layout, &handler).unwrap();
        assert_eq!(handler.calls(), 1);
        std::fs::remove_file(version_dir(&layout, name, "legacy-r2").join("tool")).unwrap();
        let before = snapshot(&layout.base);
        for _ in 0..2 {
            let row = status_of(Some(&layout), &[], &handler, None);
            assert_eq!(row["state"], states::DEGRADED);
            assert!(row["detail"]["problem"]
                .as_str()
                .unwrap()
                .contains("lacks tool"));
            assert_eq!(row["detail"].get("sentinel"), None);
            assert_eq!(
                handler.calls(),
                1,
                "incomplete files must not trigger a live probe"
            );
            assert_eq!(snapshot(&layout.base), before);
        }
    }
}
