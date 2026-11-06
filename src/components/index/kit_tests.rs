//! Offline import exercises the real signed kit and index acceptance paths.
use super::*;
use crate::components::index_kit;
use crate::upgrade::archive::writer::{tar_gz, Item};
use crate::upgrade::minisign::test_key::TestKey;
use crate::upgrade::stage::fixtures::{installed_layout, temp};

const CHANNEL: &str = "public-stable";
const COMPONENT: &str = "fake-tool";

struct Fixture {
    root: PathBuf,
    layout: Layout,
    key: TestKey,
    document: Value,
    kit: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = temp("component-kit-import");
        let source = root.join("source");
        let key = TestKey::new(101);
        let release = |name: &str, version: &str, payload: &[u8]| {
            let tag = format!("component-{name}-{version}");
            let filename = format!("{name}-{version}.tar.gz");
            let archive = tar_gz(&[Item::File {
                path: "tool",
                data: payload,
                mode: 0o755,
            }]);
            std::fs::create_dir_all(source.join(&tag)).unwrap();
            std::fs::write(source.join(&tag).join(&filename), &archive).unwrap();
            std::fs::write(
                source.join(&tag).join(format!("{filename}.minisig")),
                key.sign(&archive, true, "canonical import archive fixture"),
            )
            .unwrap();
            json!({"version":version,"payload":"bundled-v1","compatible":{"allCapabilities":[]},
                "assetTag":tag,"platforms":{platform_key():{"name":filename,
                    "sha256":sha256_hex(&archive),"size":archive.len(),"signatureName":format!("{filename}.minisig")}},
                "yanked":false,"yankReason":null,"externalRuntime":null,"externalRuntimeSha256":null})
        };
        let document = json!({"schema":"cutex/components-index/v1","channel":CHANNEL,"sequence":1,
            "publishedAt":"2026-10-29T00:00:00Z","components":[
                {"name":COMPONENT,"recommended":"v1","releases":[
                    release(COMPONENT,"v1",b"working frozen component"),
                    release(COMPONENT,"v2",b"not selected in the kit")]},
                {"name":"other-tool","recommended":"v1","releases":[
                    release("other-tool","v1",b"also not selected in the kit")]}]});
        let bytes = serde_json::to_vec_pretty(&document).unwrap();
        std::fs::create_dir_all(source.join("channel-components")).unwrap();
        std::fs::write(source.join("channel-components/index.json"), &bytes).unwrap();
        std::fs::write(
            source.join("channel-components/index.json.minisig"),
            key.sign(&bytes, true, "canonical import index fixture"),
        )
        .unwrap();
        let kit = root.join("frozen-kit");
        index_kit::freeze(
            &Source::Dir { root: source },
            &[key.public()],
            CHANNEL,
            &kit,
            &[(COMPONENT.into(), "v1".into(), platform_key())],
        )
        .unwrap();
        // No release manifest is needed for these explicit index/kit offers.
        let layout = installed_layout("0.2.0", 0);
        let fixture = Self {
            root,
            layout,
            key,
            document,
            kit,
        };
        fixture.set_default("working-r0");
        fixture
    }

    fn keys(&self) -> Vec<PublicKey> {
        vec![self.key.public()]
    }

    fn set_default(&self, version: &str) {
        let entries = BTreeMap::from([(
            COMPONENT.to_string(),
            crate::components::Entry {
                selected: version.into(),
                origin: "signed".into(),
                installed_at: "2026-10-28T00:00:00Z".into(),
                last_error: None,
            },
        )]);
        crate::components::save_state(&self.layout, &entries).unwrap();
    }

    fn default_bytes(&self) -> Vec<u8> {
        std::fs::read(crate::components::components_dir(&self.layout).join("state.json")).unwrap()
    }

    fn import(&self) -> Value {
        import_kit(&self.layout, &self.keys(), &self.kit).unwrap()
    }

    fn accept_document(&self, document: &Value) {
        let bytes = serde_json::to_vec(document).unwrap();
        accept(
            &self.layout,
            &self.keys(),
            CHANNEL,
            &bytes,
            &self.key.sign(&bytes, true, "newer public index fixture"),
        )
        .unwrap();
    }
}

fn error_code(error: anyhow::Error) -> String {
    error
        .downcast_ref::<crate::upgrade::stage::UpgradeError>()
        .map(|error| error.code.to_string())
        .unwrap_or_else(|| format!("{error:#}"))
}

#[test]
fn imported_kit_offers_only_locked_assets_without_changing_component_defaults() {
    let f = Fixture::new();
    let defaults = f.default_bytes();
    let result = f.import();
    assert_eq!(result["selected"], false);
    assert_eq!(f.default_bytes(), defaults);
    let policy = Policy::load(&f.layout).unwrap();
    assert_eq!(policy.policy, "privateKit");
    assert_eq!(policy.channel, CHANNEL);
    assert_eq!(
        policy.source(&f.layout).unwrap(),
        Source::Dir {
            root: std::fs::canonicalize(&f.kit).unwrap()
        }
    );
    let resolved = resolve(&f.layout, &f.keys(), COMPONENT, Some("v1")).unwrap();
    assert_eq!(resolved.offer.version, "v1");
    assert_eq!(resolved.provenance["kind"], "privateKit");
    assert_eq!(resolved.provenance["sequence"], 1);
    assert_eq!(
        resolve(&f.layout, &f.keys(), COMPONENT, None)
            .unwrap()
            .offer,
        resolved.offer
    );
    let state = read_state(&f.layout).unwrap();
    assert_eq!(
        state.kit.as_ref().unwrap().assets,
        BTreeSet::from([identity_key(COMPONENT, "v1", &platform_key())])
    );

    // Both offers are present and correctly signed in the snapshot, but neither
    // was explicitly frozen. Presence in the document is not kit authorization.
    for (name, version) in [(COMPONENT, "v2"), ("other-tool", "v1")] {
        assert_eq!(
            error_code(resolve(&f.layout, &f.keys(), name, Some(version)).unwrap_err()),
            "component_offer_changed"
        );
        let row = view(&f.layout, &f.keys(), name, None);
        let release = row["releases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["version"] == version)
            .unwrap();
        assert_eq!(release["compatible"], false);
        assert_eq!(release["incompatibleReason"], "privateKit:notLocked");
    }
    assert!(!f.kit.join("component-fake-tool-v2").exists());
    assert!(!f.kit.join("component-other-tool-v1").exists());
    assert_eq!(f.default_bytes(), defaults);
}

#[test]
fn newer_same_channel_does_not_retarget_kit_or_lower_watermark_and_refresh_recovers() {
    let f = Fixture::new();
    f.import();
    let defaults = f.default_bytes();
    let frozen = resolve(&f.layout, &f.keys(), COMPONENT, Some("v1")).unwrap();
    let mut newer = f.document.clone();
    newer["sequence"] = json!(2);
    newer["components"][0]["recommended"] = json!("v2");
    f.accept_document(&newer);
    let state = read_state(&f.layout).unwrap();
    assert_eq!(state.channels[CHANNEL].sequence, 2);
    assert_eq!(
        cached(&f.layout, &f.keys(), &state, CHANNEL)
            .unwrap()
            .unwrap()
            .components[0]
            .recommended
            .as_deref(),
        Some("v2")
    );
    assert_eq!(state.kit.as_ref().unwrap().record.sequence, 1);
    let effective = resolve(&f.layout, &f.keys(), COMPONENT, None).unwrap();
    assert_eq!(effective.offer, frozen.offer);
    assert_eq!(effective.source, frozen.source);
    assert_eq!(effective.provenance["sequence"], 1);
    assert_eq!(
        error_code(resolve(&f.layout, &f.keys(), COMPONENT, Some("v2")).unwrap_err()),
        "component_offer_changed"
    );

    // A real failed paired refresh marks diagnostics stale. Restore exact source
    // bytes, then refresh the frozen sequence without replaying public acceptance.
    let index_path = f.kit.join("channel-components/index.json");
    let original = std::fs::read(&index_path).unwrap();
    let mut changed = original.clone();
    changed.push(b' ');
    std::fs::write(&index_path, changed).unwrap();
    assert!(refresh(&f.layout, &f.keys()).is_err());
    assert_eq!(view(&f.layout, &f.keys(), COMPONENT, None)["stale"], true);
    assert!(read_state(&f.layout).unwrap().errors.contains_key(CHANNEL));
    std::fs::write(index_path, original).unwrap();
    let refreshed = refresh(&f.layout, &f.keys()).unwrap();
    assert_eq!(refreshed["sequence"], 1);
    assert_eq!(refreshed["stale"], false);
    let state = read_state(&f.layout).unwrap();
    assert_eq!(state.channels[CHANNEL].sequence, 2);
    assert_eq!(state.kit.as_ref().unwrap().record.sequence, 1);
    assert!(state.kit.as_ref().unwrap().record.stale_reason.is_none());
    assert!(!state.errors.contains_key(CHANNEL));
    let row = view(&f.layout, &f.keys(), COMPONENT, None);
    assert_eq!(row["source"], "privateKit");
    assert_eq!(row["sequence"], 1);
    assert_eq!(row["recommended"], "v1");
    assert_eq!(row["stale"], false);
    assert_eq!(row["staleReason"], Value::Null);
    assert_eq!(f.default_bytes(), defaults);
}

#[test]
fn higher_sequence_public_withdrawal_blocks_frozen_offer_but_keeps_selected_default() {
    let f = Fixture::new();
    f.set_default("v1");
    let defaults = f.default_bytes();
    f.import();
    let mut withdrawal = f.document.clone();
    withdrawal["sequence"] = json!(3);
    withdrawal["components"][0]["recommended"] = json!("v2");
    withdrawal["components"][0]["releases"][0]["yanked"] = json!(true);
    withdrawal["components"][0]["releases"][0]["yankReason"] = json!("withdrawal fixture");
    f.accept_document(&withdrawal);
    assert_eq!(
        error_code(resolve(&f.layout, &f.keys(), COMPONENT, Some("v1")).unwrap_err()),
        "component_yanked"
    );
    assert_eq!(
        error_code(resolve(&f.layout, &f.keys(), COMPONENT, None).unwrap_err()),
        "component_yanked"
    );
    let row = view(&f.layout, &f.keys(), COMPONENT, Some("v1"));
    assert_eq!(row["selectedYanked"], true);
    assert_eq!(row["recommended"], Value::Null);
    // Reading the original frozen non-yanked snapshot cannot unyank its identity.
    refresh(&f.layout, &f.keys()).unwrap();
    assert_eq!(read_state(&f.layout).unwrap().channels[CHANNEL].sequence, 3);
    assert_eq!(
        error_code(resolve(&f.layout, &f.keys(), COMPONENT, Some("v1")).unwrap_err()),
        "component_yanked"
    );
    assert_eq!(
        crate::components::installed(&f.layout, COMPONENT)
            .unwrap()
            .selected,
        "v1"
    );
    assert_eq!(f.default_bytes(), defaults);
}

#[test]
fn corrupt_archive_import_preserves_existing_policy_cache_and_defaults() {
    let f = Fixture::new();
    // Keep a real accepted public cache and explicit policy as the before-state,
    // so a failed import cannot pass merely by leaving an empty install empty.
    Policy {
        policy: "componentIndex".into(),
        source: Some(f.root.join("source").display().to_string()),
        ..Default::default()
    }
    .save(&f.layout)
    .unwrap();
    f.accept_document(&f.document);
    let policy_path = components_dir(&f.layout).join("index-policy.json");
    let policy_before = std::fs::read(&policy_path).unwrap();
    let state_before = std::fs::read(state_path(&f.layout)).unwrap();
    let defaults_before = f.default_bytes();
    let lock = index_kit::verify(&f.kit, &f.keys()).unwrap();
    let asset = &lock.assets[0];
    let archive = f.kit.join(&asset.tag).join(&asset.asset.name);
    let mut bytes = std::fs::read(&archive).unwrap();
    bytes[0] ^= 1; // Preserve size: refusal must validate the actual canonical bytes.
    std::fs::write(archive, bytes).unwrap();
    assert!(import_kit(&f.layout, &f.keys(), &f.kit).is_err());
    assert_eq!(std::fs::read(policy_path).unwrap(), policy_before);
    assert_eq!(std::fs::read(state_path(&f.layout)).unwrap(), state_before);
    assert_eq!(f.default_bytes(), defaults_before);
    assert_eq!(Policy::load(&f.layout).unwrap().policy, "componentIndex");
    assert!(read_state(&f.layout).unwrap().kit.is_none());
}
