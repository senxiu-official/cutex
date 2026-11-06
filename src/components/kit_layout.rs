//! Component-only adoption of an unmanaged base by a verified signed kit.
//! This deliberately does not change backend upgrade Layout detection/records.
use crate::upgrade::layout::Layout;
use crate::upgrade::minisign::PublicKey;

/// Locate the explicitly supplied or executable-derived base. Never choose a
/// platform default, create a directory or infer backend updater ownership.
pub fn candidate() -> Option<Layout> {
    if let Some(base) = std::env::var_os(crate::upgrade::layout::INSTALL_BASE_ENV) {
        let base = std::path::PathBuf::from(base);
        return base.is_absolute().then(|| Layout::at(base));
    }
    let executable = std::env::current_exe().ok()?;
    Layout::from_executable(&executable).filter(|layout| layout.base.is_absolute())
}

/// Keep existing backend-managed installs unchanged, otherwise recognize only
/// an absolute candidate with a currently trusted imported kit snapshot.
pub fn detect() -> Option<Layout> {
    Layout::detect().or_else(|| {
        candidate().filter(|layout| managed(layout, &crate::upgrade::keys::trusted_keys()))
    })
}

pub fn managed(layout: &Layout, keys: &[PublicKey]) -> bool {
    layout.is_managed() || super::index::kit_managed(layout, keys)
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::components::{components_dir, index, index_kit};
    use crate::upgrade::keys::TEST_PUBLIC_KEY_ENV;
    use crate::upgrade::manifest::{platform_key, sha256_hex};
    use crate::upgrade::minisign::test_key::TestKey;
    use crate::upgrade::source::Source;
    use crate::upgrade::stage::fixtures::installed_layout;
    use serde_json::{Value, json};
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    fn assert_component_only_source_contract(f: &Fixture) {
        use crate::components;
        let keys = [f.key.public()];
        f.assert_backend_unmanaged();
        assert!(managed(&f.layout, &keys));
        assert_eq!(
            components::install_from(Some(&f.layout), &keys),
            "source-only"
        );
        assert!(
            components::release_manifest(&f.layout, &keys)
                .unwrap()
                .is_none()
        );
        assert!(
            components::fetch_release_manifest(&f.layout, &keys)
                .unwrap()
                .is_none()
        );
        let list = components::list(Some(&f.layout), &keys, None);
        assert_eq!(list["managed"], true);
        assert_eq!(list["installFrom"], "source-only");
        assert!(components::load_state(&f.layout).is_empty());
        assert!(!components::version_dir(&f.layout, "fake-tool", "v1").exists());
        f.assert_backend_unmanaged();
    }

    struct Environment {
        _home: crate::profiles::lifecycle::tests::TestHome,
        previous: Vec<(&'static str, Option<OsString>)>,
    }
    impl Environment {
        fn new() -> Self {
            let home = crate::profiles::lifecycle::tests::TestHome::new();
            std::fs::write(
                home.root().join(crate::config::paths::PRIVATE_HOME_MARKER),
                b"component kit layout tests",
            )
            .unwrap();
            let tmp = home.root().join("tmp");
            std::fs::create_dir_all(&tmp).unwrap();
            let mut previous = Vec::new();
            for (name, value) in [
                ("CUTEX_HOME", home.root().join(".cutex")),
                ("CUTEX_TEST_PRIVATE_HOME", home.root().to_path_buf()),
                ("TEMP", tmp.clone()),
                ("TMP", tmp.clone()),
                ("TMPDIR", tmp),
            ] {
                previous.push((name, std::env::var_os(name)));
                unsafe {
                    std::env::set_var(name, value);
                }
            }
            // These two variables are changed repeatedly by the tests below;
            // capture their original values only once while holding HOME's lock.
            for name in [
                crate::upgrade::layout::INSTALL_BASE_ENV,
                TEST_PUBLIC_KEY_ENV,
            ] {
                previous.push((name, std::env::var_os(name)));
                unsafe {
                    std::env::remove_var(name);
                }
            }
            Self {
                _home: home,
                previous,
            }
        }
        fn root(&self) -> &Path {
            self._home.root()
        }
        fn base(&self, path: &Path) {
            unsafe {
                std::env::set_var(crate::upgrade::layout::INSTALL_BASE_ENV, path);
            }
        }
        fn key(&self, key: &TestKey) {
            unsafe {
                std::env::set_var(TEST_PUBLIC_KEY_ENV, key.public().to_base64());
            }
        }
    }
    impl Drop for Environment {
        fn drop(&mut self) {
            for (name, value) in self.previous.drain(..).rev() {
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }
    }

    struct Fixture {
        environment: Environment,
        layout: Layout,
        key: TestKey,
        kit: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let environment = Environment::new();
            let base = environment.root().join("unmanaged-owner-base");
            std::fs::create_dir(&base).unwrap();
            let layout = Layout::at(base);
            environment.base(&layout.base);
            let key = TestKey::new(126);
            environment.key(&key);
            let source_root = environment.root().join("source");
            let tag = "component-fake-tool-v1";
            let name = "component-fake-tool-v1.tar.gz";
            let payload = b"opaque canonical fixture archive; never extracted";
            let index = json!({"schema":"cutex/components-index/v1","channel":"private-layout-test","sequence":1,
                "publishedAt":"2026-10-29T00:00:00Z","components":[{"name":"fake-tool","recommended":"v1",
                "releases":[{"version":"v1","payload":"bundled-v1","compatible":{"allCapabilities":[]},
                "assetTag":tag,"platforms":{platform_key():{"name":name,"size":payload.len(),"sha256":sha256_hex(payload),
                    "signatureName":format!("{name}.minisig")}},"yanked":false,"yankReason":null,
                "externalRuntime":null,"externalRuntimeSha256":null}]}]});
            let bytes = serde_json::to_vec_pretty(&index).unwrap();
            std::fs::create_dir_all(source_root.join("channel-components")).unwrap();
            std::fs::create_dir_all(source_root.join(tag)).unwrap();
            std::fs::write(source_root.join("channel-components/index.json"), &bytes).unwrap();
            std::fs::write(
                source_root.join("channel-components/index.json.minisig"),
                key.sign(&bytes, true, "kit layout index fixture"),
            )
            .unwrap();
            std::fs::write(source_root.join(tag).join(name), payload).unwrap();
            std::fs::write(
                source_root.join(tag).join(format!("{name}.minisig")),
                key.sign(payload, true, "kit layout archive fixture"),
            )
            .unwrap();
            let kit = environment.root().join("frozen-kit");
            index_kit::freeze(
                &Source::Dir { root: source_root },
                &[key.public()],
                "private-layout-test",
                &kit,
                &[("fake-tool".into(), "v1".into(), platform_key())],
            )
            .unwrap();
            Self {
                environment,
                layout,
                key,
                kit,
            }
        }
        fn import(&self) {
            index::import_kit(&self.layout, &[self.key.public()], &self.kit).unwrap();
        }
        fn assert_backend_unmanaged(&self) {
            assert!(!self.layout.install_record_path().exists());
            assert!(!self.layout.is_managed());
            assert!(
                Layout::detect().is_none(),
                "component adoption must not enable backend Layout detection"
            );
            assert!(
                crate::management::v2::upgrade_api::layout().is_none(),
                "component adoption must not enable the backend upgrade API"
            );
        }
    }

    #[test]
    fn signed_kit_adopts_only_components_without_creating_backend_install_record() {
        let f = Fixture::new();
        assert_eq!(candidate().unwrap().base, f.layout.base);
        assert!(!managed(&f.layout, &[f.key.public()]));
        assert!(detect().is_none());
        f.assert_backend_unmanaged();
        f.import();
        assert!(managed(&f.layout, &[f.key.public()]));
        assert_eq!(detect().unwrap().base, f.layout.base);
        assert_eq!(index::Policy::load(&f.layout).unwrap().policy, "privateKit");
        assert_eq!(
            index::resolve(&f.layout, &[f.key.public()], "fake-tool", Some("v1"))
                .unwrap()
                .offer
                .version,
            "v1"
        );
        f.assert_backend_unmanaged();
    }

    #[test]
    fn signed_kit_without_current_or_install_record_has_source_only_legacy_contract() {
        let f = Fixture::new();
        f.import();
        assert!(!f.layout.base.join("current").exists());
        assert_component_only_source_contract(&f);
    }

    #[cfg(unix)]
    #[test]
    fn signed_kit_with_current_but_no_install_record_still_has_source_only_legacy_contract() {
        let f = Fixture::new();
        f.import();
        let release = f.layout.release_dir("0.2.1");
        std::fs::create_dir_all(&release).unwrap();
        // A valid signed manifest reachable through current does not confer
        // backend ownership when the private manual base has no install.json.
        let manifest = crate::upgrade::manifest::fixtures::manifest("0.2.1", 11);
        let bytes = serde_json::to_vec(&manifest).unwrap();
        std::fs::write(release.join("manifest.json"), &bytes).unwrap();
        std::fs::write(
            release.join("manifest.json.minisig"),
            f.key
                .sign(&bytes, true, "unmanaged attached fixture manifest"),
        )
        .unwrap();
        std::os::unix::fs::symlink(&release, f.layout.base.join("current")).unwrap();
        assert!(f.layout.base.join("current/manifest.json").is_file());
        assert_component_only_source_contract(&f);
    }

    #[test]
    fn backend_managed_attached_signed_manifest_retains_release_install_source() {
        let environment = Environment::new();
        let layout = installed_layout("dev-r270-attached", 0);
        environment.base(&layout.base);
        let key = TestKey::new(125);
        let manifest = crate::upgrade::manifest::fixtures::manifest("dev-r270-attached", 12);
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let release = layout.release_dir("dev-r270-attached");
        std::fs::write(release.join("manifest.json"), &bytes).unwrap();
        std::fs::write(
            release.join("manifest.json.minisig"),
            key.sign(&bytes, true, "managed attached fixture manifest"),
        )
        .unwrap();
        assert!(layout.is_managed());
        assert_eq!(
            crate::components::install_from(Some(&layout), &[key.public()]),
            "release"
        );
        assert_eq!(
            crate::components::release_manifest(&layout, &[key.public()]).unwrap(),
            Some(manifest.clone())
        );
        assert_eq!(
            crate::components::fetch_release_manifest(&layout, &[key.public()]).unwrap(),
            Some(manifest)
        );
        let list = crate::components::list(Some(&layout), &[key.public()], None);
        assert_eq!(list["managed"], true);
        assert_eq!(list["installFrom"], "release");
    }

    #[test]
    fn corrupt_snapshot_signature_and_foreign_keys_do_not_adopt_a_base() {
        let f = Fixture::new();
        f.import();
        let other = TestKey::new(127);
        assert!(!managed(&f.layout, &[other.public()]));
        f.environment.key(&other);
        assert!(
            detect().is_none(),
            "foreign global trust must not accept the imported test signature"
        );
        f.environment.key(&f.key);
        assert!(detect().is_some());
        let state_path = components_dir(&f.layout).join("index-state.json");
        let original = std::fs::read(&state_path).unwrap();
        for corrupt in [b"{broken state".as_slice(), b"{}".as_slice()] {
            std::fs::write(&state_path, corrupt).unwrap();
            assert!(!managed(&f.layout, &[f.key.public()]));
            assert!(detect().is_none());
            assert_eq!(
                std::fs::read(&state_path).unwrap(),
                corrupt,
                "detection must not repair/reset invalid kit state"
            );
            f.assert_backend_unmanaged();
        }
        std::fs::write(&state_path, &original).unwrap();
        let state: Value = serde_json::from_slice(&original).unwrap();
        let digest = state["kit"]["record"]["signatureDigest"].as_str().unwrap();
        let signature_path = components_dir(&f.layout)
            .join("index-blobs")
            .join(format!("{digest}.minisig"));
        std::fs::write(&signature_path, b"not a minisign signature").unwrap();
        assert!(!managed(&f.layout, &[f.key.public()]));
        assert!(detect().is_none());
        assert_eq!(
            std::fs::read(signature_path).unwrap(),
            b"not a minisign signature"
        );
        f.assert_backend_unmanaged();
    }

    #[test]
    fn ordinary_managed_layout_detection_is_preserved_without_kit_trust() {
        let environment = Environment::new();
        let layout = installed_layout("0.2.1", 0);
        environment.base(&layout.base);
        assert!(managed(&layout, &[]));
        assert_eq!(candidate().unwrap().base, layout.base);
        assert_eq!(detect().unwrap().base, layout.base);
        assert_eq!(Layout::detect().unwrap().base, layout.base);
        assert_eq!(
            crate::management::v2::upgrade_api::layout().unwrap().base,
            layout.base
        );
        assert!(!components_dir(&layout).join("index-state.json").exists());
    }

    #[test]
    fn candidate_uses_only_absolute_explicit_or_executable_root_without_creation() {
        let environment = Environment::new();
        environment.base(Path::new("relative-owner-base"));
        assert!(candidate().is_none());
        assert!(detect().is_none());
        environment.base(Path::new(""));
        assert!(
            candidate().is_none(),
            "an explicitly empty base is not an absolute candidate"
        );
        let missing = environment.root().join("does-not-exist");
        environment.base(&missing);
        assert_eq!(candidate().unwrap().base, missing);
        assert!(detect().is_none());
        assert!(
            !missing.exists(),
            "detection must not create the candidate base"
        );
        unsafe {
            std::env::remove_var(crate::upgrade::layout::INSTALL_BASE_ENV);
        }
        let expected = Layout::from_executable(&std::env::current_exe().unwrap())
            .filter(|layout| layout.base.is_absolute());
        assert_eq!(
            candidate().map(|layout| layout.base),
            expected.map(|layout| layout.base)
        );
    }
}
