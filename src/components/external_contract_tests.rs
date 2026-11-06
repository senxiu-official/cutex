//! Manifest-only staged adapter discovery and selected source identity fallback.
//! Every payload is inert. No dependency probe, download or subprocess is used.
use super::*;
use crate::upgrade::manifest::{self, ComponentPart};
use crate::upgrade::minisign::test_key::TestKey;
use crate::upgrade::stage::fixtures::installed_layout;
use std::ffi::OsString;

struct PrivateHome {
    _home: crate::profiles::lifecycle::tests::TestHome,
    previous: Vec<(&'static str, Option<OsString>)>,
}
impl PrivateHome {
    fn new() -> Self {
        let home = crate::profiles::lifecycle::tests::TestHome::new();
        std::fs::write(
            home.root().join(crate::config::paths::PRIVATE_HOME_MARKER),
            b"external contract test",
        )
        .unwrap();
        let tmp = home.root().join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let mut previous = Vec::new();
        for (key, value) in [
            ("CUTEX_HOME", home.root().join(".cutex")),
            ("CUTEX_TEST_PRIVATE_HOME", home.root().to_path_buf()),
            ("CUTEX_INSTALL_BASE", home.root().join("unused-base")),
            ("TEMP", tmp.clone()),
            ("TMP", tmp.clone()),
            ("TMPDIR", tmp),
        ] {
            previous.push((key, std::env::var_os(key)));
            unsafe {
                std::env::set_var(key, value);
            }
        }
        Self {
            _home: home,
            previous,
        }
    }
}
impl Drop for PrivateHome {
    fn drop(&mut self) {
        for (key, value) in self.previous.drain(..).rev() {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

struct Fixture {
    layout: Layout,
    key: TestKey,
    name: String,
    version: String,
    contract: Value,
    archive_sha256: String,
    _home: PrivateHome,
}
impl Fixture {
    fn new(name: &str, old_selected: bool) -> Self {
        let home = PrivateHome::new();
        let layout = installed_layout("0.2.1", 0);
        let key = TestKey::new(117);
        let index =
            index_model::Index::parse(include_bytes!("../../channel-components/index.json"))
                .unwrap();
        let component = index
            .components
            .iter()
            .find(|component| component.name == name)
            .unwrap();
        let release = component
            .releases
            .iter()
            .find(|release| Some(&release.version) == component.recommended.as_ref())
            .unwrap();
        let asset = &release.platforms[&platform_key()];
        let mut manifest = manifest::fixtures::manifest("0.2.1", 11);
        manifest.components.insert(
            name.into(),
            ComponentPart {
                version: release.version.clone(),
                artifacts: BTreeMap::from([(
                    platform_key(),
                    Artifact {
                        tag: Some(release.asset_tag.clone()),
                        name: asset.name.clone(),
                        sha256: asset.sha256.clone(),
                        size: asset.size,
                    },
                )]),
            },
        );
        let bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        std::fs::write(layout.release_dir("0.2.1").join("manifest.json"), &bytes).unwrap();
        std::fs::write(
            layout.release_dir("0.2.1").join("manifest.json.minisig"),
            key.sign(&bytes, true, "manifest-only contract fixture"),
        )
        .unwrap();
        let entries = if old_selected {
            let old = if name == "dsh-runtime" {
                "dsh-r2"
            } else {
                "kimi-r2"
            };
            std::fs::create_dir_all(version_dir(&layout, name, old)).unwrap();
            BTreeMap::from([(
                name.into(),
                Entry {
                    selected: old.into(),
                    origin: ORIGIN_SIGNED.into(),
                    installed_at: "2026-10-28T00:00:00Z".into(),
                    last_error: None,
                },
            )])
        } else {
            BTreeMap::new()
        };
        save_state(&layout, &entries).unwrap();
        crate::session::store::save_cutex_session_store(
            &crate::session::model::CutexSessionStore::default(),
        )
        .unwrap();
        // Defaults are deliberately inert sentinels, never launchable runtimes.
        for path in [
            crate::runtime::dsh::deployment_path().unwrap(),
            crate::runtime::kimi::deployment_path().unwrap(),
        ] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"{\"fixture\":\"unchanged default; never executed\"}").unwrap();
        }
        assert_eq!(
            index::Policy::load(&layout).unwrap().policy,
            "releaseManifest"
        );
        Self {
            layout,
            key,
            name: name.into(),
            version: release.version.clone(),
            contract: release.external_runtime.clone().unwrap(),
            archive_sha256: asset.sha256.clone(),
            _home: home,
        }
    }
    fn keys(&self) -> Vec<PublicKey> {
        vec![self.key.public()]
    }
    fn dir(&self, label: &str) -> PathBuf {
        version_dir(&self.layout, &self.name, label)
    }
    fn payload(&self, label: &str, origin: &str, receipt: bool) -> PathBuf {
        let dir = self.dir(label);
        std::fs::create_dir_all(&dir).unwrap();
        // Write the discriminator before asking the real handler for the
        // payload-specific requirements (legacy requirements are different).
        std::fs::write(
            dir.join("external-runtime.json"),
            serde_json::to_vec_pretty(&self.contract).unwrap(),
        )
        .unwrap();
        let handler = handler(&self.name).unwrap();
        for file in handler.required_files_for(&dir) {
            let path = dir.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            if !path.exists() {
                std::fs::write(path, b"// inert fixture; never executed\n").unwrap();
            }
        }
        let metadata = Metadata {
            schema: COMPONENT_SCHEMA.into(),
            name: self.name.clone(),
            version: self.version.clone(),
            platforms: vec![platform_key()],
        };
        std::fs::write(
            dir.join("component.json"),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();
        if receipt {
            self.write_receipt(&dir, label, origin);
        }
        dir
    }
    fn write_receipt(&self, dir: &Path, label: &str, origin: &str) {
        let receipt = Receipt {
            schema: RECEIPT_SCHEMA.into(),
            name: self.name.clone(),
            version: label.into(),
            origin: origin.into(),
            archive_sha256: if origin == ORIGIN_SIGNED {
                Some(self.archive_sha256.clone())
            } else {
                None
            },
            selection_source: None,
            files: file_hashes(dir).unwrap(),
            unpacked_at: "2026-10-29T00:00:00Z".into(),
        };
        std::fs::write(
            dir.join(RECEIPT_FILE),
            serde_json::to_vec_pretty(&receipt).unwrap(),
        )
        .unwrap();
    }
    fn preserved_state(&self) -> (Vec<u8>, Vec<u8>, Vec<u8>, Value) {
        (
            std::fs::read(state_path(&self.layout)).unwrap(),
            std::fs::read(crate::runtime::dsh::deployment_path().unwrap()).unwrap(),
            std::fs::read(crate::runtime::kimi::deployment_path().unwrap()).unwrap(),
            serde_json::to_value(
                &crate::session::store::load_cutex_session_store_shared()
                    .unwrap()
                    .sessions,
            )
            .unwrap(),
        )
    }
    fn read_status(&self) -> Value {
        let handler = handler(&self.name).unwrap();
        status_of(Some(&self.layout), &self.keys(), handler.as_ref(), None)
    }
    fn rewrite_receipt(&self, dir: &Path, update: impl FnOnce(&mut Receipt)) {
        let mut receipt = read_receipt(dir).unwrap();
        update(&mut receipt);
        std::fs::write(
            dir.join(RECEIPT_FILE),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
    }
}

#[test]
fn manifest_only_staged_verified_candidate_is_exposed_without_selecting_or_probing() {
    for name in ["dsh-runtime", "kimi-runtime"] {
        for old_selected in [false, true] {
            let f = Fixture::new(name, old_selected);
            let before = f.preserved_state();
            assert!(
                external_contract(&f.layout, &f.keys(), name).is_err(),
                "no candidate exists before staging"
            );
            assert!(f.read_status().get("external").is_none());
            let dir = f.payload(&f.version, ORIGIN_SIGNED, false);
            assert!(
                external_contract(&f.layout, &f.keys(), name).is_err(),
                "an arbitrary unreceipted directory is not a staged offer"
            );
            f.write_receipt(&dir, &f.version, ORIGIN_SIGNED);
            let contract = external_contract(&f.layout, &f.keys(), name).unwrap();
            assert_eq!(contract.version, f.version);
            assert_eq!(serde_json::to_value(&contract).unwrap(), f.contract);
            let receipt_before = std::fs::read(dir.join(RECEIPT_FILE)).unwrap();
            for _ in 0..2 {
                let row = f.read_status();
                assert_eq!(row["external"]["version"], f.version);
                assert_eq!(row["external"]["checkedAt"], Value::Null);
                assert_eq!(
                    row["selected"],
                    if old_selected {
                        json!(if name == "dsh-runtime" {
                            "dsh-r2"
                        } else {
                            "kimi-r2"
                        })
                    } else {
                        Value::Null
                    }
                );
                assert_eq!(
                    f.preserved_state(),
                    before,
                    "contract discovery/GET must preserve Entries, defaults and session pins"
                );
                assert_eq!(
                    std::fs::read(dir.join(RECEIPT_FILE)).unwrap(),
                    receipt_before
                );
                assert!(
                    !components_dir(&f.layout).join("external").exists(),
                    "GET must not initialize readiness state"
                );
            }
        }
    }
}

#[test]
fn staged_manifest_candidate_rejects_receipt_and_contract_metadata_tampering() {
    for name in ["dsh-runtime", "kimi-runtime"] {
        for corruption in [
            "archive-sha",
            "receipt-name",
            "receipt-version",
            "contract-bytes",
            "metadata-bytes",
            "contract-identity",
            "metadata-identity",
        ] {
            let f = Fixture::new(name, false);
            let dir = f.payload(&f.version, ORIGIN_SIGNED, true);
            assert!(external_contract(&f.layout, &f.keys(), name).is_ok());
            match corruption {
                "archive-sha" => {
                    f.rewrite_receipt(&dir, |r| r.archive_sha256 = Some("f".repeat(64)))
                }
                "receipt-name" => f.rewrite_receipt(&dir, |r| r.name = "other-runtime".into()),
                "receipt-version" => f.rewrite_receipt(&dir, |r| r.version = "other-r3".into()),
                "contract-bytes" => {
                    std::fs::write(dir.join("external-runtime.json"), b"{}").unwrap();
                }
                "metadata-bytes" => {
                    std::fs::write(dir.join("component.json"), b"{}").unwrap();
                }
                "contract-identity" => {
                    let mut contract = f.contract.clone();
                    contract["version"] = json!("other-r3");
                    std::fs::write(
                        dir.join("external-runtime.json"),
                        serde_json::to_vec(&contract).unwrap(),
                    )
                    .unwrap();
                    f.write_receipt(&dir, &f.version, ORIGIN_SIGNED);
                }
                "metadata-identity" => {
                    let mut metadata: Metadata =
                        serde_json::from_slice(&std::fs::read(dir.join("component.json")).unwrap())
                            .unwrap();
                    metadata.version = "other-r3".into();
                    std::fs::write(
                        dir.join("component.json"),
                        serde_json::to_vec(&metadata).unwrap(),
                    )
                    .unwrap();
                    f.write_receipt(&dir, &f.version, ORIGIN_SIGNED);
                }
                _ => unreachable!(),
            }
            let before = f.preserved_state();
            assert!(
                external_contract(&f.layout, &f.keys(), name).is_err(),
                "accepted {name} {corruption}"
            );
            assert!(
                f.read_status().get("external").is_none(),
                "GET exposed {name} {corruption}"
            );
            assert_eq!(f.preserved_state(), before);
        }
    }
}

#[test]
fn selected_source_label_uses_verified_canonical_metadata_version_without_repinning() {
    for name in ["dsh-runtime", "kimi-runtime"] {
        let f = Fixture::new(name, false);
        let label = format!("{}-source-012345abcdef", f.version);
        let dir = f.payload(&label, ORIGIN_SOURCE, true);
        save_state(
            &f.layout,
            &BTreeMap::from([(
                name.into(),
                Entry {
                    selected: label.clone(),
                    origin: ORIGIN_SOURCE.into(),
                    installed_at: "2026-10-29T00:00:00Z".into(),
                    last_error: None,
                },
            )]),
        )
        .unwrap();
        assert!(
            !f.dir(&f.version).exists(),
            "manifest candidate is intentionally not staged"
        );
        let before = f.preserved_state();
        let receipt_before = std::fs::read(dir.join(RECEIPT_FILE)).unwrap();
        let contract = external_contract(&f.layout, &f.keys(), name).unwrap();
        assert_eq!(contract.version, f.version);
        assert_ne!(contract.version, label);
        let row = f.read_status();
        assert_eq!(row["external"]["version"], f.version);
        assert_eq!(row["external"]["checkedAt"], Value::Null);
        assert_eq!(row["selected"], label);
        assert_eq!(row["origin"], ORIGIN_SOURCE);
        assert_eq!(f.preserved_state(), before);
        assert_eq!(
            std::fs::read(dir.join(RECEIPT_FILE)).unwrap(),
            receipt_before
        );
    }
}

#[test]
fn unselected_source_candidate_is_discovered_from_validated_saved_state_without_resetting_corruption(
) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RefuseExec(AtomicUsize);
    impl Exec for RefuseExec {
        fn run(&self, _program: &str, _args: &[String]) -> anyhow::Result<String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("fixture probe deliberately unavailable; no subprocess executed")
        }
    }

    for name in ["dsh-runtime", "kimi-runtime"] {
        let f = Fixture::new(name, false);
        // This source-preparation scenario has neither a signed component
        // offer nor an Entry; the canonical contract is already known to prepare.
        let manifest = manifest::fixtures::manifest("0.2.1", 11);
        let bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        std::fs::write(f.layout.release_dir("0.2.1").join("manifest.json"), &bytes).unwrap();
        std::fs::write(
            f.layout.release_dir("0.2.1").join("manifest.json.minisig"),
            f.key
                .sign(&bytes, true, "no component offer source fixture"),
        )
        .unwrap();
        assert!(release_manifest(&f.layout, &f.keys())
            .unwrap()
            .unwrap()
            .components
            .is_empty());
        assert!(installed(&f.layout, name).is_none());
        assert!(index::resolve(&f.layout, &f.keys(), name, None).is_err());
        assert!(external_runtime::current_contract(&f.layout, name)
            .unwrap()
            .is_none());
        assert!(external_contract(&f.layout, &f.keys(), name).is_err());
        assert!(!f.dir(&f.version).exists());

        let contract: external_runtime::ExternalRuntime =
            serde_json::from_value(f.contract.clone()).unwrap();
        let node = f._home._home.root().join("fake-node.exe");
        std::fs::write(
            &node,
            b"not an executable; only RefuseExec can observe this path",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let exec = RefuseExec(AtomicUsize::new(0));
        let before = f.preserved_state();
        let status = external_runtime::configure(
            &f.layout,
            name,
            &contract,
            external_runtime::ConfigureRequest {
                external_root: external_runtime::PathUpdate::Set(
                    f._home._home.root().join("missing-source-prefix"),
                ),
                node: external_runtime::PathUpdate::Set(node),
                rg: external_runtime::PathUpdate::Set(
                    f._home
                        ._home
                        .root()
                        .join(if cfg!(windows) { "rg.exe" } else { "rg" }),
                ),
            },
            &exec,
        )
        .unwrap();
        assert!(
            !status.ready(),
            "the simulated source candidate needs user dependency repair"
        );
        assert!(!status.issues.is_empty());
        assert!(
            exec.0.load(Ordering::SeqCst) > 0,
            "configure must exercise the rejecting mock executor"
        );
        assert_eq!(f.preserved_state(), before);
        let state_path = components_dir(&f.layout)
            .join("external")
            .join(format!("{name}.json"));
        let saved = std::fs::read(&state_path).unwrap();
        assert_eq!(
            external_runtime::current_contract(&f.layout, name).unwrap(),
            Some(contract.clone())
        );
        assert_eq!(
            external_contract(&f.layout, &f.keys(), name).unwrap(),
            contract
        );
        let calls = exec.0.load(Ordering::SeqCst);
        for _ in 0..2 {
            let row = f.read_status();
            assert_eq!(row["selected"], Value::Null);
            assert_eq!(row["external"]["version"], f.version);
            assert_eq!(row["external"]["checkedAt"], json!(status.checked_at));
            assert_eq!(row["external"]["issues"].as_array().unwrap().len(), status.issues.len());
            assert_eq!(external_runtime::cached(&f.layout, name, &contract).unwrap(), status);
            assert!(
                !row["external"]["issues"].as_array().unwrap().is_empty(),
                "GET must expose needs-user dependency issues"
            );
            assert_eq!(
                row["state"],
                states::NEEDS_USER,
                "an unselected blocked candidate needs user action"
            );
            assert_eq!(std::fs::read(&state_path).unwrap(), saved);
            assert_eq!(f.preserved_state(), before);
            assert_eq!(exec.0.load(Ordering::SeqCst), calls);
            assert!(
                !f.dir(&f.version).exists(),
                "saved candidate discovery must not stage or scan an archive"
            );
        }

        for malformed in [false, true] {
            let corrupt = if malformed {
                b"{invalid external state".to_vec()
            } else {
                let mut state: Value = serde_json::from_slice(&saved).unwrap();
                state["schema"] = json!("unsupported/state/v99");
                serde_json::to_vec(&state).unwrap()
            };
            std::fs::write(&state_path, &corrupt).unwrap();
            assert!(
                external_runtime::current_contract(&f.layout, name).is_err(),
                "invalid saved state must not be treated as absence"
            );
            assert!(external_contract(&f.layout, &f.keys(), name).is_err());
            assert!(f.read_status().get("external").is_none());
            assert!(
                external_runtime::configure(
                    &f.layout,
                    name,
                    &contract,
                    external_runtime::ConfigureRequest::default(),
                    &exec
                )
                .is_err(),
                "configure must not silently replace corrupt saved state"
            );
            assert_eq!(std::fs::read(&state_path).unwrap(), corrupt);
            assert_eq!(f.preserved_state(), before);
            assert_eq!(exec.0.load(Ordering::SeqCst), calls);
        }
    }
}
