//! Linux opt-in acceptance with owner-prepared prefixes and canonical archives.
//! Seeds r2 bindings; never launches a vendor CLI, agent, model or PTY.
use super::*;
use crate::runtime::dsh::{AcpFlavor, DshDeployment, DshSession};
use crate::runtime::kimi::{KimiCredentialSource, KimiDeployment, KimiOptions};
use crate::session::model::{CutexSessionRecord, CutexSessionStore};
use crate::upgrade::minisign::test_key::TestKey;
use crate::upgrade::source::Source;
use crate::upgrade::stage::fixtures::installed_layout;
use anyhow::{ensure, Context as _};
use std::ffi::OsString;

const ORIGINAL_INDEX: &[u8] = include_bytes!("../../channel-components/index.json");

struct AcceptanceEnvironment {
    previous: Vec<(&'static str, Option<OsString>)>,
    scratch: PathBuf,
}
impl AcceptanceEnvironment {
    fn new() -> anyhow::Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let scratch =
            std::env::temp_dir().join(format!("cutex-r269-real-prefix-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&scratch)?;
        std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o700))?;
        let home = scratch.join("home");
        std::fs::create_dir(&home)?;
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))?;
        std::fs::write(
            home.join(crate::config::paths::PRIVATE_HOME_MARKER),
            b"r269 acceptance private home",
        )?;
        let tmp = scratch.join("tmp");
        std::fs::create_dir(&tmp)?;
        let mut guard = Self {
            previous: Vec::new(),
            scratch,
        };
        for (name, value) in [
            ("HOME", home.clone()),
            ("CUTEX_TEST_PRIVATE_HOME", home.clone()),
            ("CUTEX_HOME", home.join(".cutex")),
            ("CUTEX_INSTALL_BASE", home.join("unused-install")),
            ("TEMP", tmp.clone()),
            ("TMP", tmp.clone()),
            ("TMPDIR", tmp),
        ] {
            guard.change(name, Some(value.into_os_string()));
        }
        // Readiness receives explicit paths. Do not inherit a prefix, Node
        // preload or module override from the harness/owner's environment.
        for name in [
            "DSH_INSTALL_ROOT",
            "KIMI_INSTALL_ROOT",
            "CUTEX_DSH_INSTALL_ROOT",
            "CUTEX_KIMI_INSTALL_ROOT",
            "CUTEX_DSH_NODE",
            "CUTEX_KIMI_NODE",
            "CUTEX_KIMI_RG",
            "NODE_OPTIONS",
            "NODE_PATH",
        ] {
            guard.change(name, None);
        }
        Ok(guard)
    }
    fn change(&mut self, name: &'static str, value: Option<OsString>) {
        self.previous.push((name, std::env::var_os(name)));
        unsafe {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}
impl Drop for AcceptanceEnvironment {
    fn drop(&mut self) {
        for (name, value) in self.previous.drain(..).rev() {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
        // Retain only our own scratch for diagnostics; never clean the supplied
        // real prefixes, canonical archive directory or evidence destination.
    }
}

struct Evidence {
    path: Option<PathBuf>,
    document: Value,
}
impl Evidence {
    fn record(&mut self, stage: &str, data: Value) -> anyhow::Result<()> {
        self.document["stages"]
            .as_array_mut()
            .context("evidence stages")?
            .push(json!({"stage":stage,"data":data}));
        if let Some(path) = &self.path {
            ensure!(
                path.is_absolute(),
                "acceptance evidence path must be absolute"
            );
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, serde_json::to_vec_pretty(&self.document)?)?;
        }
        Ok(())
    }
}

fn default_bytes(paths: &[PathBuf; 2]) -> anyhow::Result<[Vec<u8>; 2]> {
    Ok([std::fs::read(&paths[0])?, std::fs::read(&paths[1])?])
}
fn session_records() -> anyhow::Result<Value> {
    Ok(serde_json::to_value(
        &crate::session::store::load_cutex_session_store_shared()?.sessions,
    )?)
}
fn assert_pins(layout: &Layout, records: &Value) -> anyhow::Result<()> {
    ensure!(
        session_records()? == *records,
        "an existing seeded session binding changed"
    );
    let dsh = dsh_runtime::DshRuntime::default().pins_checked(layout)?;
    let kimi = kimi_runtime::KimiRuntime::default().pins_checked(layout)?;
    ensure!(
        dsh == BTreeMap::from([("dsh-r2".into(), 1)]),
        "seeded DSH r2 pin changed"
    );
    ensure!(
        kimi == BTreeMap::from([("kimi-r2".into(), 1)]),
        "seeded Kimi r2 pin changed"
    );
    Ok(())
}

fn seed_old_bindings(layout: &Layout, scratch: &Path) -> anyhow::Result<[PathBuf; 2]> {
    let mut sessions = CutexSessionStore::default();
    let mut components = BTreeMap::new();
    let mut defaults = Vec::new();
    for (name, version, flavor, upstream) in [
        ("dsh-runtime", "dsh-r2", AcpFlavor::Dsh, "0.2.1-rc.3"),
        ("kimi-runtime", "kimi-r2", AcpFlavor::Kimi, "2.3.3"),
    ] {
        let dir = version_dir(layout, name, version);
        std::fs::create_dir_all(dir.join("src"))?;
        std::fs::write(
            dir.join("src/server.mjs"),
            b"// seeded r2 pin; never executed\n",
        )?;
        let node = dir.join("seeded-unexecuted-node");
        let root = dir.join("runtime");
        let cli = root.join(if flavor == AcpFlavor::Dsh {
            "node_modules/@deepseek-ai/dsh/lib/bin.js"
        } else {
            "node_modules/@moonshot-ai/kimi-code/dist/main.mjs"
        });
        let path = if flavor == AcpFlavor::Dsh {
            crate::runtime::dsh::deployment_path()?
        } else {
            crate::runtime::kimi::deployment_path()?
        };
        let deployment = if flavor == AcpFlavor::Dsh {
            serde_json::to_value(DshDeployment {
                node: node.clone(),
                adapter: dir.join("src/server.mjs"),
                dsh_cli: cli.clone(),
                install_root: root.clone(),
                version: upstream.into(),
            })?
        } else {
            serde_json::to_value(KimiDeployment {
                node: node.clone(),
                adapter: dir.join("src/server.mjs"),
                kimi_cli: cli.clone(),
                install_root: root.clone(),
                version: upstream.into(),
                rg: None,
            })?
        };
        std::fs::create_dir_all(path.parent().context("default deployment parent")?)?;
        std::fs::write(&path, serde_json::to_vec_pretty(&deployment)?)?;
        defaults.push(path);
        components.insert(
            name.into(),
            Entry {
                selected: version.into(),
                origin: ORIGIN_SIGNED.into(),
                installed_at: "2026-10-28T00:00:00Z".into(),
                last_error: None,
            },
        );
        let id = format!("cutex.r269-acceptance-{}", flavor.runtime_name());
        let mut record = CutexSessionRecord::new(
            id.clone(),
            None,
            "acceptance-fixture".into(),
            scratch.display().to_string(),
            None,
        )?;
        record.runtime_backend = flavor.backend();
        record.dsh_session = Some(DshSession {
            flavor,
            kimi: (flavor == AcpFlavor::Kimi).then(|| KimiOptions {
                rg: None,
                credential: KimiCredentialSource::Profile,
                models: vec![],
            }),
            thread_id: uuid::Uuid::new_v4().to_string(),
            state_dir: scratch.join("seeded-sessions").join(&id),
            node,
            adapter: dir.join("src/server.mjs"),
            dsh_cli: cli,
            patch: scratch.join(&id).join("unused-config"),
            install_root: root,
            native_session_id: None,
            deployment_version: upstream.into(),
            model: if flavor == AcpFlavor::Dsh {
                "deepseek-v4-pro".into()
            } else {
                "kimi-code/kimi-for-coding".into()
            },
            reasoning_effort: None,
            api_key_env: "ACCEPTANCE_UNUSED_KEY".into(),
            spike: false,
        });
        sessions.sessions.insert(id, record);
    }
    save_state(layout, &components)?;
    crate::session::store::save_cutex_session_store(&sessions)?;
    Ok(defaults
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected two default deployments"))?)
}

fn probe_summary(out: &Value) -> Value {
    json!({"outcome":out["result"]["outcome"],"selected":out["result"]["selected"],
        "version":out["result"]["version"],"checkedAt":out["result"]["external"]["checkedAt"],
        "issues":out["result"]["external"]["issues"].as_array().map(|issues|
            issues.iter().map(|issue|issue["code"].clone()).collect::<Vec<_>>()).unwrap_or_default()})
}

fn broken_ws_prefix(scratch: &Path, real: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let root = scratch.join(format!("broken-ws-{name}"));
    std::fs::create_dir(&root)?;
    if real.join("package.json").is_file() {
        std::fs::copy(real.join("package.json"), root.join("package.json"))?;
    }
    let modules = root.join("node_modules");
    std::fs::create_dir(&modules)?;
    // Retain each complete top-level name (including scoped directories), not
    // a reconstructed basename. All links are created only in owned scratch.
    for child in std::fs::read_dir(real.join("node_modules"))? {
        let child = child?;
        if child.file_name() != "ws" {
            std::os::unix::fs::symlink(child.path(), modules.join(child.file_name()))?;
        }
    }
    let ws = modules.join("ws");
    std::fs::create_dir(&ws)?;
    // Correct real package metadata/version, deliberately no main/exports file.
    std::fs::copy(real.join("node_modules/ws/package.json"), ws.join("package.json"))?;
    Ok(root)
}

#[test]
fn linux_real_prefix_canonical_install_kit_and_pinned_r2_acceptance() -> anyhow::Result<()> {
    let (Some(prefix_root), Some(canonical_root)) = (
        std::env::var_os("CUTEX_R269_ACCEPTANCE_ROOT"),
        std::env::var_os("CUTEX_R269_CANONICAL_ROOT"),
    ) else {
        return Ok(());
    };
    let evidence_path = std::env::var_os("CUTEX_R269_ACCEPTANCE_EVIDENCE").map(PathBuf::from);
    let _home_lock = crate::config::paths::test_home_env_lock();
    let prefix_root = std::fs::canonicalize(prefix_root)?;
    let canonical_root = std::fs::canonicalize(canonical_root)?;
    ensure!(
        platform_key() == "linux-x64",
        "canonical acceptance requires linux-x64"
    );
    let mut environment = AcceptanceEnvironment::new()?;
    let node = prefix_root.join("node-v24.21.0-linux-x64/bin/node");
    let rg = PathBuf::from("/usr/bin/rg");
    ensure!(
        node.is_file() && rg.is_file(),
        "owner-prepared Node/ripgrep paths are missing"
    );
    let layout = installed_layout("dev-r269-external-acceptance", 0);
    let key = TestKey::new(123);
    let keys = [key.public()];
    let original = index_model::Index::parse(ORIGINAL_INDEX)?;
    let mirror = environment.scratch.join("source-mirror");
    std::fs::create_dir_all(mirror.join("channel-components"))?;
    let index_signature = key.sign(ORIGINAL_INDEX, true, "canonical acceptance test index");
    std::fs::write(mirror.join("channel-components/index.json"), ORIGINAL_INDEX)?;
    std::fs::write(
        mirror.join("channel-components/index.json.minisig"),
        &index_signature,
    )?;
    let mut assets = Vec::new();
    for component in &original.components {
        for release in &component.releases {
            for (platform, asset) in &release.platforms {
                let path = canonical_root.join(&asset.name);
                ensure!(
                    std::fs::metadata(&path)?.len() == asset.size
                        && crate::upgrade::manifest::file_sha256_hex(&path)? == asset.sha256,
                    "canonical archive identity mismatch for {}",
                    asset.name
                );
                let bytes = std::fs::read(&path)?;
                let dir = mirror.join(&release.asset_tag);
                std::fs::create_dir_all(&dir)?;
                std::fs::write(dir.join(&asset.name), &bytes)?;
                std::fs::write(
                    dir.join(&asset.signature_name),
                    key.sign(&bytes, true, "canonical acceptance test archive"),
                )?;
                assets.push(
                    json!({"component":component.name,"version":release.version,"platform":platform,
                    "name":asset.name,"sha256":asset.sha256,"size":asset.size}),
                );
            }
        }
    }
    let mut evidence = Evidence {
        path: evidence_path,
        document: json!({"schema":"cutex/r269-external-acceptance/v1",
        "scope":"seeded r2 default/session-pin preservation; real r3 bounded dependency readiness; no vendor CLI/model/PTY execution",
        "prefixRoot":prefix_root,"canonicalRoot":canonical_root,"scratch":environment.scratch,
        "indexSha256":crate::upgrade::manifest::sha256_hex(ORIGINAL_INDEX),"assets":assets,"stages":[]}),
    };
    evidence.record(
        "canonical-inputs-verified",
        json!({"archiveCount":assets.len()}),
    )?;
    index::Policy {
        policy: "componentIndex".into(),
        channel: original.channel.clone(),
        source: Some(
            Source::Dir {
                root: mirror.clone(),
            }
            .display(),
        ),
        ..Default::default()
    }
    .save(&layout)?;
    index::accept(
        &layout,
        &keys,
        &original.channel,
        ORIGINAL_INDEX,
        &index_signature,
    )?;
    let defaults = seed_old_bindings(&layout, &environment.scratch)?;
    let old_defaults = default_bytes(&defaults)?;
    let old_entries = load_state(&layout);
    let old_records = session_records()?;
    assert_pins(&layout, &old_records)?;
    evidence.record("seeded-r2-preservation-baseline", json!({"sessionCount":2,
        "sessionRecordsSha256":crate::upgrade::manifest::sha256_hex(&serde_json::to_vec(&old_records)?),
        "dshDefaultSha256":crate::upgrade::manifest::sha256_hex(&old_defaults[0]),
        "kimiDefaultSha256":crate::upgrade::manifest::sha256_hex(&old_defaults[1]),
        "oldVendorExecution":false}))?;
    let exec = external_runtime::RealExec;

    // Configure the signed candidate BEFORE its archive is downloaded. A
    // deliberately absent prefix is a real readiness failure, not a fake probe.
    for (name, version) in [("dsh-runtime", "dsh-r3"), ("kimi-runtime", "kimi-r3")] {
        ensure!(
            !version_dir(&layout, name, version).exists(),
            "candidate was already downloaded"
        );
        let request = external_runtime::ConfigureRequest {
            external_root: external_runtime::PathUpdate::Set(
                environment.scratch.join(format!("missing-{name}")),
            ),
            node: external_runtime::PathUpdate::Set(node.clone()),
            rg: if name == "kimi-runtime" {
                external_runtime::PathUpdate::Set(rg.clone())
            } else {
                external_runtime::PathUpdate::Unchanged
            },
        };
        let checked = check_external(
            Some(&layout),
            &keys,
            name,
            Some(request),
            &exec,
            Lock::Acquire,
        )?;
        ensure!(
            checked["result"]["outcome"] == "needsUser" && checked["result"]["selected"] == false,
            "missing prefix incorrectly reported ready/selected"
        );
        ensure!(
            !version_dir(&layout, name, version).exists(),
            "configure downloaded a candidate"
        );
        ensure!(
            default_bytes(&defaults)? == old_defaults && load_state(&layout) == old_entries,
            "missing-prefix configure changed defaults"
        );
        assert_pins(&layout, &old_records)?;
        evidence.record(
            &format!("{name}-missing-prefix-configure"),
            probe_summary(&checked),
        )?;
        let handler = handler(name)?;
        let staged = install_with(
            Some(&layout),
            &keys,
            handler.as_ref(),
            &InstallRequest {
                version: Some(version.into()),
                ..Default::default()
            },
            &exec,
            Lock::Acquire,
        )?;
        ensure!(
            staged["result"]["outcome"] == "needsUser" && staged["result"]["selected"] == false,
            "dependency-blocked installation changed selection"
        );
        let release = original
            .components
            .iter()
            .find(|c| c.name == name)
            .context("canonical component")?
            .releases
            .iter()
            .find(|r| r.version == version)
            .context("canonical release")?;
        let dir = version_dir(&layout, name, version);
        index::verify_external(&dir, release)?;
        let receipt = read_receipt(&dir).context("staged canonical receipt")?;
        ensure!(
            receipt.archive_sha256.as_deref()
                == Some(release.platforms[&platform_key()].sha256.as_str()),
            "receipt archive identity mismatch"
        );
        ensure!(
            default_bytes(&defaults)? == old_defaults && load_state(&layout) == old_entries,
            "staging changed old defaults"
        );
        assert_pins(&layout, &old_records)?;
        evidence.record(&format!("{name}-blocked-install"), json!({"outcome":"needsUser","selected":false,
            "externalContractSha256":release.external_runtime_sha256,"defaultBytesPreserved":true,"sessionPinsPreserved":true}))?;
    }

    for (name, prefix) in [("dsh-runtime", "dsh"), ("kimi-runtime", "kimi")] {
        let checked = check_external(
            Some(&layout),
            &keys,
            name,
            Some(external_runtime::ConfigureRequest {
                external_root: external_runtime::PathUpdate::Set(prefix_root.join(prefix)),
                node: external_runtime::PathUpdate::Set(node.clone()),
                rg: if name == "kimi-runtime" {
                    external_runtime::PathUpdate::Set(rg.clone())
                } else {
                    external_runtime::PathUpdate::Unchanged
                },
            }),
            &exec,
            Lock::Acquire,
        )?;
        ensure!(
            checked["result"]["outcome"] == "ready" && checked["result"]["selected"] == false,
            "prepared real prefix failed readiness or changed selection: {}",
            checked["result"]
        );
        ensure!(
            default_bytes(&defaults)? == old_defaults && load_state(&layout) == old_entries,
            "configure changed an old default"
        );
        assert_pins(&layout, &old_records)?;
        evidence.record(
            &format!("{name}-real-prefix-ready"),
            probe_summary(&checked),
        )?;
    }

    // Exercise real package-entry resolution while BOTH defaults/Entries/pins
    // are still the r2 baseline. Never edit the retained owner/Moth prefixes.
    for (name, prefix, version) in [
        ("dsh-runtime", "dsh", "dsh-r3"),
        ("kimi-runtime", "kimi", "kimi-r3"),
    ] {
        let real = prefix_root.join(prefix);
        let broken = broken_ws_prefix(&environment.scratch, &real, name)?;
        let checked = check_external(
            Some(&layout),
            &keys,
            name,
            Some(external_runtime::ConfigureRequest {
                external_root: external_runtime::PathUpdate::Set(broken.clone()),
                node: external_runtime::PathUpdate::Set(node.clone()),
                rg: if name == "kimi-runtime" {
                    external_runtime::PathUpdate::Set(rg.clone())
                } else {
                    external_runtime::PathUpdate::Unchanged
                },
            }),
            &exec,
            Lock::Acquire,
        )?;
        ensure!(
            checked["result"]["outcome"] == "needsUser"
                && checked["result"]["selected"] == false,
            "broken real ws entry incorrectly ready/selected"
        );
        // The wire view adds raw-config/display/diagnostic fields; internal
        // cached status remains the strict persisted execution representation.
        let contract = external_contract(&layout, &keys, name)?;
        let status = external_runtime::cached(&layout, name, &contract)?;
        ensure!(
            status.root.as_ref() == Some(&broken)
                && status.issues.iter().any(|issue| issue.code == "external_root_invalid"
                && issue.path.as_ref() == Some(&broken.join("node_modules/ws"))
                && !issue.steps.is_empty()),
            "broken ws diagnostic lacks package path/repair steps"
        );
        let contract = external_contract(&layout, &keys, name)?;
        ensure!(
            external_runtime::cached(&layout, name, &contract)? == status && !status.ready(),
            "broken ws needsUser status was not persisted"
        );
        ensure!(
            default_bytes(&defaults)? == old_defaults && load_state(&layout) == old_entries,
            "broken ws configure changed r2 defaults/Entries"
        );
        assert_pins(&layout, &old_records)?;
        let handler = handler(name)?;
        let staged = install_with(
            Some(&layout),
            &keys,
            handler.as_ref(),
            &InstallRequest {
                version: Some(version.into()),
                ..Default::default()
            },
            &exec,
            Lock::Acquire,
        )?;
        ensure!(
            staged["result"]["outcome"] == "needsUser"
                && staged["result"]["selected"] == false,
            "broken ws installation selected r3"
        );
        ensure!(
            default_bytes(&defaults)? == old_defaults && load_state(&layout) == old_entries,
            "broken ws installation changed r2 defaults/Entries"
        );
        assert_pins(&layout, &old_records)?;
        let restored = check_external(
            Some(&layout),
            &keys,
            name,
            Some(external_runtime::ConfigureRequest {
                external_root: external_runtime::PathUpdate::Set(real),
                node: external_runtime::PathUpdate::Unchanged,
                rg: external_runtime::PathUpdate::Unchanged,
            }),
            &exec,
            Lock::Acquire,
        )?;
        ensure!(
            restored["result"]["outcome"] == "ready"
                && restored["result"]["selected"] == false,
            "restored real prefix is not ready before normal r3 selection"
        );
        ensure!(
            default_bytes(&defaults)? == old_defaults && load_state(&layout) == old_entries,
            "restoring real prefix changed r2 defaults/Entries"
        );
        assert_pins(&layout, &old_records)?;
        evidence.record(&format!("{name}-real-prefix-broken-ws"), json!({
            "configure":probe_summary(&checked),"brokenRoot":broken,
            "externalIssues":status.issues,
            "issuePathAndSteps":true,"needsUserCachePersisted":true,
            "installOutcome":staged["result"]["outcome"],"selected":false,
            "r2DefaultBytesPreserved":true,"r2EntriesPreserved":true,"r2PinsPreserved":true,
            "correctRootRestoredReady":true
        }))?;
    }

    for (name, version) in [("dsh-runtime", "dsh-r3"), ("kimi-runtime", "kimi-r3")] {
        let handler = handler(name)?;
        let selected = install_with(
            Some(&layout),
            &keys,
            handler.as_ref(),
            &InstallRequest {
                version: Some(version.into()),
                ..Default::default()
            },
            &exec,
            Lock::Acquire,
        )?;
        ensure!(
            selected["result"]["selected"] == true && selected["result"]["outcome"] == "selected",
            "r3 was not selected"
        );
        ensure!(
            installed(&layout, name).context("selected Entry")?.selected == version,
            "r3 Entry not committed"
        );
        if name == "dsh-runtime" {
            ensure!(
                std::fs::read(&defaults[1])? == old_defaults[1]
                    && installed(&layout, "kimi-runtime")
                        .context("old Kimi Entry")?
                        .selected
                        == "kimi-r2",
                "DSH installation changed Kimi's default"
            );
        }
        assert_pins(&layout, &old_records)?;
        evidence.record(
            &format!("{name}-r3-selected-for-new-agents"),
            json!({"selected":true,"version":version,"seededR2PinsPreserved":true}),
        )?;
    }
    let dsh: DshDeployment = serde_json::from_slice(&std::fs::read(&defaults[0])?)?;
    let kimi: KimiDeployment = serde_json::from_slice(&std::fs::read(&defaults[1])?)?;
    ensure!(
        dsh.node == node
            && dsh.install_root == prefix_root.join("dsh")
            && dsh.adapter == version_dir(&layout, "dsh-runtime", "dsh-r3").join("src/server.mjs"),
        "DSH default is not the real r3 prefix"
    );
    ensure!(
        kimi.node == node
            && kimi.install_root == prefix_root.join("kimi")
            && kimi.rg == Some(rg.clone())
            && kimi.adapter
                == version_dir(&layout, "kimi-runtime", "kimi-r3").join("src/server.mjs"),
        "Kimi default is not the real r3 prefix"
    );
    let selected_defaults = default_bytes(&defaults)?;
    let selected_entries = load_state(&layout);

    // Offline refresh uses a deliberately absent local mirror, never a network.
    let working_policy = index::Policy::load(&layout)?;
    let mut offline = working_policy.clone();
    offline.source = Some(
        Source::Dir {
            root: environment.scratch.join("offline-mirror"),
        }
        .display(),
    );
    offline.save(&layout)?;
    ensure!(
        index::refresh(&layout, &keys).is_err(),
        "offline refresh unexpectedly succeeded"
    );
    for (name, version) in [("dsh-runtime", "dsh-r3"), ("kimi-runtime", "kimi-r3")] {
        ensure!(
            index::resolve(&layout, &keys, name, Some(version))?
                .offer
                .version
                == version,
            "last-good offer lost offline"
        );
        ensure!(
            index::view(&layout, &keys, name, Some(version))["stale"] == true,
            "offline diagnostics missing"
        );
    }
    working_policy.save(&layout)?;
    ensure!(
        default_bytes(&defaults)? == selected_defaults && load_state(&layout) == selected_entries,
        "offline refresh changed selections"
    );
    assert_pins(&layout, &old_records)?;
    evidence.record(
        "offline-last-good-cache",
        json!({"offersRetained":true,"defaultsPreserved":true,"sessionPinsPreserved":true}),
    )?;

    let kit = environment.scratch.join("canonical-kit");
    index::freeze_kit(
        &layout,
        &keys,
        &kit,
        &[
            ("dsh-runtime".into(), "dsh-r3".into(), platform_key()),
            ("kimi-runtime".into(), "kimi-r3".into(), platform_key()),
        ],
    )?;
    let lock = index_kit::verify(&kit, &keys)?;
    ensure!(
        lock.assets.len() == 2,
        "kit must contain both current-platform components"
    );
    ensure!(
        std::fs::read(kit.join("channel-components/index.json"))? == ORIGINAL_INDEX
            && std::fs::read(kit.join("channel-components/index.json.minisig"))?
                == index_signature.as_bytes(),
        "kit changed original signed index pair"
    );
    for asset in &lock.assets {
        ensure!(
            std::fs::read(kit.join(&asset.tag).join(&asset.asset.name))?
                == std::fs::read(canonical_root.join(&asset.asset.name))?,
            "kit repacked a canonical archive"
        );
        ensure!(
            std::fs::read(kit.join(&asset.tag).join(&asset.asset.signature_name))?
                == std::fs::read(mirror.join(&asset.tag).join(&asset.asset.signature_name))?,
            "kit changed canonical test signature bytes"
        );
    }
    // The owner bases have no backend install record. Exercise that shape with
    // the same real canonical payloads/prefixes and existing r2 session pins.
    std::fs::remove_file(layout.install_record_path())?;
    environment.change(
        "CUTEX_INSTALL_BASE",
        Some(layout.base.clone().into_os_string()),
    );
    environment.change(
        crate::upgrade::keys::TEST_PUBLIC_KEY_ENV,
        Some(key.public().to_base64().into()),
    );
    ensure!(
        !layout.is_managed() && crate::management::v2::upgrade_api::layout().is_none(),
        "backend unexpectedly managed before kit adoption"
    );
    index::import_kit(&layout, &keys, &kit)?;
    ensure!(
        kit_layout::detect().is_some_and(|candidate| candidate.base == layout.base),
        "component-only kit adoption not detected"
    );
    ensure!(
        crate::management::v2::upgrade_api::layout().is_none()
            && !layout.install_record_path().exists(),
        "component adoption enabled backend updater"
    );
    ensure!(
        index::Policy::load(&layout)?.policy == "privateKit",
        "kit policy not selected"
    );
    ensure!(
        default_bytes(&defaults)? == selected_defaults && load_state(&layout) == selected_entries,
        "kit import changed defaults"
    );
    assert_pins(&layout, &old_records)?;
    evidence.record("canonical-kit-roundtrip", json!({"archiveCount":2,"originalArchiveAndSignatureBytes":true,"defaultsPreserved":true,"sessionPinsPreserved":true}))?;

    for (name, version) in [("dsh-runtime", "dsh-r3"), ("kimi-runtime", "kimi-r3")] {
        let handler = handler(name)?;
        let selected = install_with(
            Some(&layout),
            &keys,
            handler.as_ref(),
            &InstallRequest {
                version: Some(version.into()),
                ..Default::default()
            },
            &exec,
            Lock::Acquire,
        )?;
        ensure!(
            selected["result"]["selected"] == true,
            "component-only kit selection failed"
        );
        ensure!(
            !layout.install_record_path().exists()
                && crate::management::v2::upgrade_api::layout().is_none(),
            "kit selection enabled backend updater"
        );
        assert_pins(&layout, &old_records)?;
    }
    evidence.record("component-only-kit-adoption", json!({"realPrefixesReady":true,"bothExplicitSelections":true,"backendUpdaterEnabled":false,"installRecordCreated":false,"sessionPinsPreserved":true}))?;
    let selected_defaults = default_bytes(&defaults)?;
    let selected_entries = load_state(&layout);

    let mut yank: Value = serde_json::from_slice(ORIGINAL_INDEX)?;
    yank["sequence"] = json!(original.sequence + 1);
    let component = yank["components"]
        .as_array_mut()
        .context("canonical components")?
        .iter_mut()
        .find(|c| c["name"] == "kimi-runtime")
        .context("canonical Kimi")?;
    component["recommended"] = Value::Null;
    let release = component["releases"]
        .as_array_mut()
        .context("canonical releases")?
        .iter_mut()
        .find(|r| r["version"] == "kimi-r3")
        .context("canonical Kimi r3")?;
    release["yanked"] = json!(true);
    release["yankReason"] = json!("acceptance test withdrawal");
    let bytes = serde_json::to_vec(&yank)?;
    index::accept(
        &layout,
        &keys,
        &original.channel,
        &bytes,
        &key.sign(&bytes, true, "acceptance test withdrawal"),
    )?;
    let error = index::resolve(&layout, &keys, "kimi-runtime", Some("kimi-r3")).unwrap_err();
    ensure!(
        error
            .downcast_ref::<UpgradeError>()
            .is_some_and(|e| e.code == "component_yanked"),
        "withdrawal did not block frozen offer"
    );
    ensure!(
        index::view(&layout, &keys, "kimi-runtime", Some("kimi-r3"))["selectedYanked"] == true,
        "selected withdrawal warning missing"
    );
    index::refresh(&layout, &keys)?;
    ensure!(
        index::resolve(&layout, &keys, "kimi-runtime", Some("kimi-r3")).is_err(),
        "frozen refresh resurrected withdrawn offer"
    );
    ensure!(
        default_bytes(&defaults)? == selected_defaults && load_state(&layout) == selected_entries,
        "withdrawal changed defaults"
    );
    assert_pins(&layout, &old_records)?;
    evidence.record(
        "withdrawal-preserves-existing-defaults-and-pins",
        json!({"newSelectionBlocked":true,"selectedYanked":true,
        "defaultsPreserved":true,"seededR2PinsPreserved":true,
        "dshDefaultSha256":crate::upgrade::manifest::sha256_hex(&selected_defaults[0]),
        "kimiDefaultSha256":crate::upgrade::manifest::sha256_hex(&selected_defaults[1])}),
    )?;
    Ok(())
}
