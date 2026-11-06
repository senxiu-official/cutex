//! Generic component mechanics with a fake handler (no daemon, no systemctl).
use super::*;
use crate::upgrade::archive::writer::{tar_gz, Item};
use crate::upgrade::manifest::{self, Artifact, ComponentPart};
use crate::upgrade::minisign::test_key::TestKey;
use crate::upgrade::service::fake::FakeExec;
use crate::upgrade::stage::fixtures::{installed_layout, temp};
use std::cell::RefCell;

const FAKE: &str = "fake-tool";

/// The fake's world, per test thread: which version "runs", whether the
/// restart gate is closed, pins, an external service.
#[derive(Default)]
struct World {
    calls: Vec<String>,
    running: Option<String>,
    gate_closed: bool,
    pins: BTreeMap<String, usize>,
    pins_unavailable: bool,
    external: bool,
    by_hand: bool,
    before_select: Option<Box<dyn FnOnce()>>,
    needs_user: bool,
}

thread_local! {
    static WORLD: RefCell<World> = RefCell::new(World::default());
}

fn world<T>(f: impl FnOnce(&mut World) -> T) -> T {
    WORLD.with(|w| f(&mut w.borrow_mut()))
}

struct Fake(Metadata);

fn make_fake() -> Box<dyn Handler> {
    Box::new(Fake(Metadata {
        schema: COMPONENT_SCHEMA.into(),
        name: FAKE.into(),
        version: "0.1.0".into(),
        platforms: vec![platform_key()],
    }))
}

impl Handler for Fake {
    fn metadata(&self) -> &Metadata {
        &self.0
    }
    fn required_files(&self) -> &'static [&'static str] {
        &["tool"]
    }
    fn from_source(&self, source: &Path, dest: &Path) -> anyhow::Result<()> {
        std::fs::copy(source.join("tool"), dest.join("tool"))?;
        Ok(())
    }
    fn external(&self, _: &Layout) -> bool {
        world(|w| w.external)
    }
    fn configured_by_hand(&self) -> bool {
        world(|w| w.by_hand)
    }
    fn prepare(&self, ctx: &Context) -> anyhow::Result<Value> {
        if !ctx.dry_run {
            verify_file(&ctx.dir, "tool")?;
        }
        world(|w| {
            w.calls
                .push(format!("prepare {} {}", ctx.version, ctx.dry_run))
        });
        if !ctx.dry_run {
            if let Some(hook) = world(|w| w.before_select.take()) {
                hook();
            }
        }
        if world(|w| w.needs_user) {
            return Ok(json!({"state":states::NEEDS_USER}));
        }
        Ok(json!({}))
    }
    fn select(&self, ctx: &Context) -> anyhow::Result<Value> {
        world(|w| {
            w.calls
                .push(format!("select {} {}", ctx.version, ctx.dry_run));
            if ctx.dry_run {
                return Ok(json!({ "restartPending": false }));
            }
            let restart = w.running.as_deref() != Some(ctx.version.as_str());
            if restart && w.running.is_some() && w.gate_closed {
                return Ok(json!({ "restartPending": true, "restarted": false }));
            }
            w.running = Some(ctx.version.clone());
            Ok(json!({ "restartPending": false, "restarted": restart }))
        })
    }
    fn deselect(&self, ctx: &Context, force: bool) -> anyhow::Result<Vec<String>> {
        world(|w| {
            let pinned: usize = w.pins.values().sum();
            if pinned > 0 && !force {
                return Err(UpgradeError::boxed(
                    409,
                    "component_pinned",
                    "pinned",
                    json!({ "agents": pinned }),
                ));
            }
            w.calls
                .push(format!("deselect {} {}", ctx.version, ctx.dry_run));
            if !ctx.dry_run {
                w.running = None;
            }
            Ok(vec!["stopped".into()])
        })
    }
    fn probe(&self, _: &Layout, entry: &Entry) -> Value {
        world(|w| match &w.running {
            None => json!({ "state": states::DEGRADED }),
            Some(v) if *v != entry.selected => {
                json!({ "state": states::RESTART_PENDING, "running": v })
            }
            Some(_) => json!({ "state": states::READY }),
        })
    }
    fn pins(&self, _: &Layout) -> BTreeMap<String, usize> {
        world(|w| w.pins.clone())
    }
    fn pins_checked(&self, layout: &Layout) -> anyhow::Result<BTreeMap<String, usize>> {
        anyhow::ensure!(!world(|w| w.pins_unavailable), "pin store unavailable");
        Ok(self.pins(layout))
    }
    fn restart_gate(&self, _: &Layout) -> Result<(), String> {
        world(|w| {
            if w.gate_closed {
                Err("1 job running".into())
            } else {
                Ok(())
            }
        })
    }
}

/// A fresh world and the fake registered on this thread.
fn register() {
    WORLD.with(|w| *w.borrow_mut() = World::default());
    TEST_HANDLER.with(|h| *h.borrow_mut() = Some(make_fake));
}

#[test]
fn prune_refuses_unreadable_pins_before_removing_any_version() {
    register();
    let layout = installed_layout("0.1.1", 50);
    let version = version_dir(&layout, FAKE, "0.0.1");
    std::fs::create_dir_all(&version).unwrap();
    std::fs::write(version.join("tool"), b"keep-pinned-runtime").unwrap();
    world(|w| w.pins_unavailable = true);
    for dry_run in [true, false] {
        let error = prune(Some(&layout), &[], FAKE, dry_run).unwrap_err();
        assert!(error.to_string().contains("pin store unavailable"));
        assert_eq!(
            std::fs::read(version.join("tool")).unwrap(),
            b"keep-pinned-runtime"
        );
    }
}

/// Sign `release`'s manifest (with a `fake-tool` component of `version`)
/// into the release dir and put the archive into a local source under the
/// component's own tag.
fn ship(
    layout: &Layout,
    source: &Path,
    key: &TestKey,
    release: &str,
    version: Option<&str>,
    payload: &[u8],
) -> Option<Artifact> {
    let mut m = manifest::fixtures::manifest(release, 50);
    let artifact = version.map(|version| {
        // With the license files the component builds put next to their payload.
        let archive = tar_gz(&[
            Item::File {
                path: "tool",
                data: payload,
                mode: 0o755,
            },
            Item::File {
                path: "LICENSE",
                data: b"license",
                mode: 0o644,
            },
            Item::File {
                path: "THIRD_PARTY_NOTICES.md",
                data: b"# Third-party notices\n",
                mode: 0o644,
            },
        ]);
        let name = format!("component-{FAKE}-{version}-{}.tar.gz", platform_key());
        let tag = format!("component-{FAKE}-{version}");
        std::fs::create_dir_all(source.join(&tag)).unwrap();
        std::fs::write(source.join(&tag).join(&name), &archive).unwrap();
        let artifact = Artifact {
            tag: None,
            name,
            sha256: manifest::sha256_hex(&archive),
            size: archive.len() as u64,
        };
        m.components.insert(
            FAKE.into(),
            ComponentPart {
                version: version.into(),
                artifacts: [(platform_key(), artifact.clone())].into(),
            },
        );
        artifact
    });
    let bytes = serde_json::to_vec_pretty(&m).unwrap();
    let dir = layout.release_dir(release);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("manifest.json"), &bytes).unwrap();
    std::fs::write(
        dir.join("manifest.json.minisig"),
        key.sign(&bytes, true, "file:manifest.json"),
    )
    .unwrap();
    artifact
}

/// Make `release` the active one (as an upgrade swap does).
fn activate(layout: &Layout, from: &str, release: &str) {
    std::fs::create_dir_all(layout.release_dir(release)).unwrap();
    std::fs::copy(
        layout.release_executable(from),
        layout.release_executable(release),
    )
    .unwrap();
    #[cfg(unix)]
    crate::upgrade::layout::point_current(layout, release).unwrap();
    #[cfg(windows)]
    {
        let mut record = layout.read_install().unwrap();
        record.active_release = release.into();
        record.previous_release = Some(from.into());
        layout.write_install(&record).unwrap();
    }
}

fn use_source(layout: &Layout, source: &Path) {
    let settings = crate::upgrade::state::Settings {
        source: Some(source.display().to_string()),
        ..Default::default()
    };
    settings.save(layout).unwrap();
}

fn code(error: &anyhow::Error) -> &'static str {
    error
        .downcast_ref::<UpgradeError>()
        .map(|e| e.code)
        .unwrap_or("<other>")
}

fn cleanup(dirs: impl IntoIterator<Item = PathBuf>) {
    for d in dirs {
        let _ = std::fs::remove_dir_all(d);
    }
}

fn status(layout: &Layout, keys: &[PublicKey]) -> Value {
    status_of(Some(layout), keys, handler(FAKE).unwrap().as_ref(), None)
}

#[test]
fn unmanaged_host_reports_a_hand_configured_component_as_external() {
    register();
    let handler = handler(FAKE).unwrap();
    world(|w| w.by_hand = false);
    assert_eq!(
        status_of(None, &[], handler.as_ref(), None)["state"],
        states::MISSING
    );
    world(|w| w.by_hand = true);
    assert_eq!(
        status_of(None, &[], handler.as_ref(), None)["state"],
        states::EXTERNAL
    );
    world(|w| w.by_hand = false);
}

#[test]
fn install_downloads_verifies_unpacks_selects_and_removes() {
    register();
    let key = TestKey::new(7);
    let keys = [key.public()];
    let layout = installed_layout("0.1.1", 50);
    let source = temp("component-source");
    use_source(&layout, &source);
    ship(&layout, &source, &key, "0.1.1", Some("0.1.0"), b"tool-v1");
    let exec = FakeExec::default();
    let before = status(&layout, &keys);
    assert_eq!(before["state"], states::MISSING);
    assert_eq!(before["available"], "0.1.0");
    assert_eq!(list(Some(&layout), &keys, None)["installFrom"], "release");

    let dry = InstallRequest {
        dry_run: true,
        ..Default::default()
    };
    install(Some(&layout), &keys, FAKE, &dry, &exec, Lock::Acquire).unwrap();
    assert!(
        !version_dir(&layout, FAKE, "0.1.0").exists(),
        "a dry run downloads nothing"
    );
    assert!(installed(&layout, FAKE).is_none());

    let out = install(
        Some(&layout),
        &keys,
        FAKE,
        &InstallRequest::default(),
        &exec,
        Lock::Acquire,
    )
    .unwrap();
    let dir = version_dir(&layout, FAKE, "0.1.0");
    assert_eq!(std::fs::read(dir.join("tool")).unwrap(), b"tool-v1");
    let receipt = read_receipt(&dir).unwrap();
    assert_eq!(
        (receipt.origin.as_str(), receipt.version.as_str()),
        (ORIGIN_SIGNED, "0.1.0")
    );
    assert!(receipt.files.contains_key("tool"));
    assert!(
        receipt.files.contains_key("THIRD_PARTY_NOTICES.md"),
        "license files are unpacked and recorded"
    );
    assert_eq!(out["component"]["state"], states::READY, "{out}");
    assert_eq!(out["component"]["selected"], "0.1.0");
    assert_eq!(out["component"]["versions"][0]["version"], "0.1.0");
    // Idempotent: the version dir is reused (the source may be gone).
    std::fs::remove_dir_all(source.join(format!("component-{FAKE}-0.1.0"))).unwrap();
    install(
        Some(&layout),
        &keys,
        FAKE,
        &InstallRequest::default(),
        &exec,
        Lock::Acquire,
    )
    .unwrap();
    // A tampered binary is refused before any start.
    std::fs::write(dir.join("tool"), b"evil").unwrap();
    let error = install(
        Some(&layout),
        &keys,
        FAKE,
        &InstallRequest::default(),
        &exec,
        Lock::Acquire,
    )
    .unwrap_err();
    assert_eq!(code(&error), "component_unverified", "{error:#}");
    assert!(
        installed(&layout, FAKE).unwrap().last_error.is_some(),
        "the failure is recorded, the version stays"
    );
    std::fs::write(dir.join("tool"), b"tool-v1").unwrap();

    // Remove: refused while pinned, then deselected; state cleared, files kept.
    world(|w| w.pins.insert("0.1.0".into(), 2));
    let error = remove(
        Some(&layout),
        &keys,
        FAKE,
        false,
        false,
        &exec,
        Lock::Acquire,
    )
    .unwrap_err();
    assert_eq!(code(&error), "component_pinned");
    assert!(installed(&layout, FAKE).is_some());
    remove(
        Some(&layout),
        &keys,
        FAKE,
        true,
        false,
        &exec,
        Lock::Acquire,
    )
    .unwrap();
    assert!(installed(&layout, FAKE).is_none());
    assert!(dir.join("tool").is_file());
    assert_eq!(
        code(
            &remove(
                Some(&layout),
                &keys,
                FAKE,
                false,
                false,
                &exec,
                Lock::Acquire
            )
            .unwrap_err()
        ),
        "component_not_installed"
    );
    // Prune keeps what the active release names (and what is pinned).
    assert!(prune(Some(&layout), &keys, FAKE, false).unwrap().is_empty());
    cleanup([layout.base.clone(), source]);
}

#[test]
fn refusals_have_codes() {
    register();
    let key = TestKey::new(7);
    let keys = [key.public()];
    let layout = installed_layout("0.1.1", 50);
    let source = temp("component-source");
    use_source(&layout, &source);
    let exec = FakeExec::default();
    let req = InstallRequest::default();
    // The release does not ship the component (a 0.1.0-style manifest).
    ship(&layout, &source, &key, "0.1.1", None, b"");
    assert_eq!(
        code(&install(Some(&layout), &keys, FAKE, &req, &exec, Lock::Acquire).unwrap_err()),
        "component_not_in_release"
    );
    assert_eq!(status(&layout, &keys)["state"], states::NOT_IN_MANIFEST);
    // A manifest signed by another key.
    ship(
        &layout,
        &source,
        &TestKey::new(9),
        "0.1.1",
        Some("0.1.0"),
        b"x",
    );
    assert_eq!(
        code(&install(Some(&layout), &keys, FAKE, &req, &exec, Lock::Acquire).unwrap_err()),
        "release_unverified"
    );
    // The archive in the source differs from the manifest.
    let artifact = ship(&layout, &source, &key, "0.1.1", Some("0.1.0"), b"good").unwrap();
    std::fs::write(
        source
            .join(format!("component-{FAKE}-0.1.0"))
            .join(&artifact.name),
        tar_gz(&[Item::File {
            path: "tool",
            data: b"evil",
            mode: 0o755,
        }]),
    )
    .unwrap();
    assert!(install(Some(&layout), &keys, FAKE, &req, &exec, Lock::Acquire).is_err());
    assert!(!version_dir(&layout, FAKE, "0.1.0").exists());
    // An external service is never replaced.
    world(|w| w.external = true);
    assert_eq!(
        code(&install(Some(&layout), &keys, FAKE, &req, &exec, Lock::Acquire).unwrap_err()),
        "component_external"
    );
    assert_eq!(status(&layout, &keys)["state"], states::EXTERNAL);
    world(|w| w.external = false);
    // upgrade.lock held by a run or another operation.
    let held = crate::upgrade::state::OperationLock::try_acquire(&layout)
        .unwrap()
        .unwrap();
    assert_eq!(
        code(&install(Some(&layout), &keys, FAKE, &req, &exec, Lock::Acquire).unwrap_err()),
        "upgrade_blocked"
    );
    drop(held);
    // Unmanaged installs and unknown names.
    let unmanaged = Layout::at(temp("unmanaged"));
    assert_eq!(
        code(&install(Some(&unmanaged), &keys, FAKE, &req, &exec, Lock::Acquire).unwrap_err()),
        "unmanaged_install"
    );
    assert_eq!(
        code(&install(None, &keys, FAKE, &req, &exec, Lock::Acquire).unwrap_err()),
        "unmanaged_install"
    );
    assert_eq!(list(None, &keys, None)["installFrom"], "none");
    assert_eq!(
        code(&install(Some(&layout), &keys, "no-such", &req, &exec, Lock::Acquire).unwrap_err()),
        "component_unknown"
    );
    assert!(world(|w| w.calls.is_empty()), "no handler step ran");
    cleanup([layout.base.clone(), source, unmanaged.base]);
}

#[test]
fn from_source_installs_without_a_manifest() {
    register();
    let layout = installed_layout("dev-abc1234", 0);
    let checkout = temp("checkout");
    std::fs::write(checkout.join("tool"), b"built-locally").unwrap();
    let exec = FakeExec::default();
    assert_eq!(list(Some(&layout), &[], None)["installFrom"], "source-only");
    let error = install(
        Some(&layout),
        &[],
        FAKE,
        &InstallRequest::default(),
        &exec,
        Lock::Acquire,
    )
    .unwrap_err();
    assert_eq!(code(&error), "component_source_only");
    let request = InstallRequest {
        from_source: Some(checkout.clone()),
        ..Default::default()
    };
    let out = install(Some(&layout), &[], FAKE, &request, &exec, Lock::Acquire).unwrap();
    let version = out["result"]["version"].as_str().unwrap().to_string();
    assert!(version.starts_with("0.1.0-source-"), "{version}");
    let entry = installed(&layout, FAKE).unwrap();
    assert_eq!(entry.origin, ORIGIN_SOURCE);
    assert_eq!(
        std::fs::read(version_dir(&layout, FAKE, &version).join("tool")).unwrap(),
        b"built-locally"
    );
    let again = install(Some(&layout), &[], FAKE, &request, &exec, Lock::Acquire).unwrap();
    assert_eq!(
        again["result"]["version"],
        version.as_str(),
        "same bytes, same version"
    );
    cleanup([layout.base.clone(), checkout]);
}

#[test]
fn upgrades_select_new_versions_defer_restarts_and_roll_back() {
    register();
    let key = TestKey::new(7);
    let keys = [key.public()];
    let layout = installed_layout("0.1.1", 50);
    let source = temp("component-source");
    use_source(&layout, &source);
    let exec = FakeExec::default();
    ship(&layout, &source, &key, "0.1.1", Some("0.1.0"), b"tool-v1");

    // Not installed: an update neither downloads nor selects it.
    let mut next = manifest::fixtures::manifest("0.1.2", 51);
    next.components.insert(
        FAKE.into(),
        ComponentPart {
            version: "0.1.1".into(),
            artifacts: Default::default(),
        },
    );
    assert!(stage_installed(&layout, &next, None).is_empty());
    assert_eq!(stage_bytes(&layout, &next), 0);
    let run = temp("run");
    assert!(select_for_release(&layout, &keys, &exec, &run.join("snap.json")).is_empty());

    install(
        Some(&layout),
        &keys,
        FAKE,
        &InstallRequest::default(),
        &exec,
        Lock::Acquire,
    )
    .unwrap();
    assert_eq!(world(|w| w.running.clone()).as_deref(), Some("0.1.0"));

    // Release 0.1.2 ships 0.1.1; stage downloads it ahead of the swap.
    let artifact = ship(&layout, &source, &key, "0.1.2", Some("0.1.1"), b"tool-v2").unwrap();
    let staged = crate::upgrade::stage::staged_manifest(&layout, &keys, "0.1.2").unwrap();
    assert_eq!(stage_bytes(&layout, &staged), artifact.size);
    assert!(stage_installed(&layout, &staged, None).is_empty());
    assert!(version_dir(&layout, FAKE, "0.1.1").join("tool").is_file());

    // Swap while a job runs and an agent pins 0.1.0: selected, restart deferred.
    activate(&layout, "0.1.1", "0.1.2");
    world(|w| {
        w.gate_closed = true;
        w.pins.insert("0.1.0".into(), 1);
    });
    let snapshot = run.join("components-state.before.json");
    let notes = select_for_release(&layout, &keys, &exec, &snapshot);
    assert!(
        notes
            .iter()
            .any(|n| n.contains("restarts when no job is running")),
        "{notes:?}"
    );
    assert_eq!(installed(&layout, FAKE).unwrap().selected, "0.1.1");
    assert_eq!(status(&layout, &keys)["state"], states::RESTART_PENDING);
    assert_eq!(
        world(|w| w.running.clone()).as_deref(),
        Some("0.1.0"),
        "the running service is not touched"
    );
    // The pinned old version survives prune.
    assert!(prune(Some(&layout), &keys, FAKE, false).unwrap().is_empty());
    // Gate still closed: nothing settles. Open: the watcher restarts it.
    settle_pending(&layout, &exec);
    assert_eq!(world(|w| w.running.clone()).as_deref(), Some("0.1.0"));
    world(|w| w.gate_closed = false);
    settle_pending(&layout, &exec);
    assert_eq!(world(|w| w.running.clone()).as_deref(), Some("0.1.1"));
    assert_eq!(status(&layout, &keys)["state"], states::READY);

    // Rollback restores the snapshot's version.
    activate(&layout, "0.1.2", "0.1.1");
    let notes = restore_after_rollback(&layout, &exec, &snapshot);
    assert_eq!(notes, vec![format!("{FAKE} back on 0.1.0")]);
    assert_eq!(installed(&layout, FAKE).unwrap().selected, "0.1.0");
    assert_eq!(world(|w| w.running.clone()).as_deref(), Some("0.1.0"));

    // A release without the component leaves it where it is.
    ship(&layout, &source, &key, "0.1.3", None, b"");
    activate(&layout, "0.1.1", "0.1.3");
    let notes = select_for_release(&layout, &keys, &exec, &run.join("snap2.json"));
    assert!(
        notes
            .iter()
            .any(|n| n.contains("stays on 0.1.0 (notInManifest)")),
        "{notes:?}"
    );
    assert_eq!(installed(&layout, FAKE).unwrap().selected, "0.1.0");
    // Prune: 0.1.1 is unpinned and no longer named by the active release
    // (install.json records no previous release here).
    world(|w| w.pins.clear());
    let removed = prune(Some(&layout), &keys, FAKE, false).unwrap();
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert!(!version_dir(&layout, FAKE, "0.1.1").exists());
    assert!(
        version_dir(&layout, FAKE, "0.1.0").exists(),
        "selected stays"
    );
    cleanup([layout.base.clone(), source, run]);
}

/// An incomplete version dir no longer blocks the install: the state a user step recorded into a
/// dir that was never unpacked left (only `prepared/needs-user.json`; fetch refused it with "exists
/// but was not unpacked … move it away"), and an unpacked dir that lost files (receipt present,
/// a required file gone; prepare refused it). Install moves it aside to a dated
/// `<version>.incomplete-<ts>` and unpacks the version again; a complete dir from another archive is
/// still refused.
#[test]
fn an_incomplete_version_dir_is_moved_aside_and_installed_again() {
    register();
    let key = TestKey::new(7);
    let keys = [key.public()];
    let layout = installed_layout("0.1.1", 50);
    let source = temp("component-source");
    use_source(&layout, &source);
    ship(&layout, &source, &key, "0.1.1", Some("0.1.0"), b"tool-v1");
    let exec = FakeExec::default();
    let handler = handler(FAKE).unwrap();
    let dir = version_dir(&layout, FAKE, "0.1.0");
    let parent = dir.parent().unwrap().to_path_buf();
    let aside = || -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = std::fs::read_dir(&parent)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(&format!("0.1.0{INCOMPLETE_MARK}"))
            })
            .collect();
        found.sort();
        found
    };

    // 1. Only a needs-user marker in a dir that was never unpacked.
    std::fs::create_dir_all(dir.join("prepared")).unwrap();
    std::fs::write(
        dir.join("prepared/needs-user.json"),
        br#"{"code":"python_missing"}"#,
    )
    .unwrap();
    assert!(incomplete_reason(handler.as_ref(), &dir)
        .unwrap()
        .contains("receipt.json"));
    // A dry run says what it would move and moves nothing.
    let dry = install(
        Some(&layout),
        &keys,
        FAKE,
        &InstallRequest {
            dry_run: true,
            ..Default::default()
        },
        &exec,
        Lock::Acquire,
    )
    .unwrap();
    assert_eq!(dry["result"]["repaired"]["dryRun"], true, "{dry}");
    assert!(dir.join("prepared/needs-user.json").is_file() && aside().is_empty());
    let out = install(
        Some(&layout),
        &keys,
        FAKE,
        &InstallRequest::default(),
        &exec,
        Lock::Acquire,
    )
    .unwrap();
    assert_eq!(out["component"]["state"], states::READY, "{out}");
    assert!(
        out["result"]["repaired"]["reason"]
            .as_str()
            .unwrap()
            .contains("receipt.json"),
        "{out}"
    );
    assert_eq!(std::fs::read(dir.join("tool")).unwrap(), b"tool-v1");
    assert!(read_receipt(&dir).is_some());
    let moved = aside();
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert!(
        moved[0].join("prepared/needs-user.json").is_file(),
        "kept for inspection, not deleted"
    );
    assert_eq!(
        PathBuf::from(out["result"]["repaired"]["movedAside"].as_str().unwrap()),
        moved[0]
    );

    // 2. Unpacked, then a required file was lost: fetch would reuse it and prepare refuse it.
    std::fs::remove_file(dir.join("tool")).unwrap();
    assert!(incomplete_reason(handler.as_ref(), &dir)
        .unwrap()
        .contains("lacks tool"));
    let out = install(
        Some(&layout),
        &keys,
        FAKE,
        &InstallRequest::default(),
        &exec,
        Lock::Acquire,
    )
    .unwrap();
    assert!(out["result"]["repaired"].is_object(), "{out}");
    assert_eq!(std::fs::read(dir.join("tool")).unwrap(), b"tool-v1");
    assert_eq!(
        aside().len(),
        2,
        "a second dated dir, the first one untouched"
    );
    // Moved-aside dirs are not versions (status, prune).
    let versions: Vec<Value> = status(&layout, &keys)["versions"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(
        versions
            .iter()
            .map(|v| v["version"].clone())
            .collect::<Vec<_>>(),
        vec![json!("0.1.0")]
    );

    // 3. Complete, nothing to repair: reused as before.
    assert!(incomplete_reason(handler.as_ref(), &dir).is_none());
    let out = install(
        Some(&layout),
        &keys,
        FAKE,
        &InstallRequest::default(),
        &exec,
        Lock::Acquire,
    )
    .unwrap();
    assert!(out["result"].get("repaired").is_none(), "{out}");
    assert_eq!(aside().len(), 2);

    // 4. Complete but from another archive: still refused, never moved.
    ship(
        &layout,
        &source,
        &key,
        "0.1.1",
        Some("0.1.0"),
        b"tool-other",
    );
    let error = install(
        Some(&layout),
        &keys,
        FAKE,
        &InstallRequest::default(),
        &exec,
        Lock::Acquire,
    )
    .unwrap_err();
    assert_eq!(code(&error), "component_identity_conflict", "{error:#}");
    assert_eq!(std::fs::read(dir.join("tool")).unwrap(), b"tool-v1");
    assert_eq!(aside().len(), 2);
    cleanup([layout.base.clone(), source]);
}

/// component.json may list a platform the release builds do not package (claude-runtime:
/// darwin-arm64, source installs only). A release that ships the component for other platforms
/// only answers `component_no_platform_package` naming this platform, the packaged ones and the
/// source install, not "update cutex".
#[test]
fn a_release_without_this_platforms_package_says_so() {
    register();
    let key = TestKey::new(7);
    let keys = [key.public()];
    let layout = installed_layout("0.1.1", 50);
    let source = temp("component-source");
    use_source(&layout, &source);
    ship(&layout, &source, &key, "0.1.1", Some("0.1.0"), b"tool-v1");
    // Re-sign the manifest with the archive listed for another platform only.
    let dir = layout.release_dir("0.1.1");
    let mut m = ReleaseManifest::parse(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    let part = m.components.get_mut(FAKE).unwrap();
    let artifact = part.artifacts.remove(&platform_key()).unwrap();
    let other = if platform_key() == "linux-x64" {
        "windows-x64"
    } else {
        "linux-x64"
    };
    part.artifacts.insert(other.into(), artifact);
    let bytes = serde_json::to_vec_pretty(&m).unwrap();
    std::fs::write(dir.join("manifest.json"), &bytes).unwrap();
    std::fs::write(
        dir.join("manifest.json.minisig"),
        key.sign(&bytes, true, "file:manifest.json"),
    )
    .unwrap();

    let error = install(
        Some(&layout),
        &keys,
        FAKE,
        &InstallRequest::default(),
        &FakeExec::default(),
        Lock::Acquire,
    )
    .unwrap_err();
    assert_eq!(code(&error), "component_no_platform_package", "{error:#}");
    let text = format!("{error:#}");
    assert!(
        text.contains(&format!("no {FAKE} 0.1.0 package for {}", platform_key())),
        "{text}"
    );
    assert!(
        text.contains(other) && text.contains("--from-source"),
        "{text}"
    );
    let details = &error.downcast_ref::<UpgradeError>().unwrap().details;
    assert_eq!(details["platform"], platform_key());
    assert_eq!(details["packaged"], json!([other]));
    assert!(!version_dir(&layout, FAKE, "0.1.0").exists());
    assert_eq!(status(&layout, &keys)["state"], states::NOT_IN_MANIFEST);
    assert!(world(|w| w.calls.is_empty()));
    cleanup([layout.base.clone(), source]);
}

#[test]
fn the_registry_lists_job_service_from_its_component_json() {
    let list = list(None, &[], None);
    let job = list["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "job-service")
        .unwrap()
        .clone();
    assert_eq!(job["platforms"], json!(["linux-x64"]));
    assert_eq!(job["state"], states::MISSING);
    assert_eq!(list["managed"], false);
    // component.json and the crate agree on the version (build.sh checks the same).
    let cargo = include_str!("../../components/job-service/Cargo.toml");
    let handler = job_service::JobService::default();
    let version = &Handler::metadata(&handler).version;
    assert!(
        cargo.contains(&format!("\nversion = \"{version}\"")),
        "component.json {version} vs Cargo.toml"
    );
}

fn index_tool(layout: &Layout, source: &Path, key: &TestKey, version: &str) -> Value {
    let archive = tar_gz(&[Item::File {
        path: "tool",
        data: b"independent-tool",
        mode: 0o755,
    }]);
    let tag = format!("component-{FAKE}-{version}");
    let name = format!("{FAKE}-{version}.tar.gz");
    std::fs::create_dir_all(source.join(&tag)).unwrap();
    std::fs::write(source.join(&tag).join(&name), &archive).unwrap();
    std::fs::write(
        source.join(&tag).join(format!("{name}.minisig")),
        key.sign(&archive, true, "component fixture"),
    )
    .unwrap();
    let doc = json!({"schema":"cutex/components-index/v1","channel":"public-stable","sequence":1,"publishedAt":"2026-10-29T00:00:00Z",
        "components":[{"name":FAKE,"recommended":version,"releases":[{"version":version,"payload":"bundled-v1","compatible":{"allCapabilities":[]},
        "assetTag":tag,"platforms":{platform_key():{"name":name,"sha256":manifest::sha256_hex(&archive),"size":archive.len(),"signatureName":format!("{name}.minisig")}},
        "yanked":false,"yankReason":null,"externalRuntime":null,"externalRuntimeSha256":null}]}]});
    index::Policy {
        policy: "componentIndex".into(),
        source: Some(source.display().to_string()),
        ..Default::default()
    }
    .save(layout)
    .unwrap();
    accept_index(layout, key, &doc);
    doc
}
fn accept_index(layout: &Layout, key: &TestKey, doc: &Value) {
    let bytes = serde_json::to_vec(doc).unwrap();
    index::accept(
        layout,
        &[key.public()],
        "public-stable",
        &bytes,
        &key.sign(&bytes, true, "index fixture"),
    )
    .unwrap();
}

#[test]
fn independent_index_install_keeps_manifest_pin_and_default_across_backend_update() {
    register();
    let key = TestKey::new(91);
    let keys = [key.public()];
    let layout = installed_layout("0.1.1", 50);
    let source = temp("independent-component");
    use_source(&layout, &source);
    ship(&layout, &source, &key, "0.1.1", Some("r2"), b"bundled-tool");
    let exec = FakeExec::default();
    install(
        Some(&layout),
        &keys,
        FAKE,
        &InstallRequest::default(),
        &exec,
        Lock::Acquire,
    )
    .unwrap();
    world(|w| {
        w.pins.insert("r2".into(), 1);
    });
    index_tool(&layout, &source, &key, "r3");
    let request = InstallRequest {
        version: Some("r3".into()),
        ..Default::default()
    };
    let out = install(Some(&layout), &keys, FAKE, &request, &exec, Lock::Acquire).unwrap();
    assert_eq!(out["result"]["outcome"], "selected");
    assert_eq!(out["component"]["selected"], "r3");
    assert_eq!(out["component"]["available"], "r2");
    assert_eq!(
        read_receipt(&version_dir(&layout, FAKE, "r3"))
            .unwrap()
            .selection_source
            .unwrap()["kind"],
        "componentIndex"
    );
    assert_eq!(
        code(
            &install(
                Some(&layout),
                &keys,
                FAKE,
                &InstallRequest::default(),
                &exec,
                Lock::Acquire
            )
            .unwrap_err()
        ),
        "component_version_required"
    );
    ship(
        &layout,
        &source,
        &key,
        "0.1.2",
        Some("r4"),
        b"new-manifest-tool",
    );
    activate(&layout, "0.1.1", "0.1.2");
    let notes = select_for_release(
        &layout,
        &keys,
        &exec,
        &layout.base.join("before-components.json"),
    );
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(installed(&layout, FAKE).unwrap().selected, "r3");
    assert_eq!(world(|w| w.pins.get("r2").copied()), Some(1));
    assert!(!version_dir(&layout, FAKE, "r4").exists());
}

#[test]
fn newly_accepted_yank_prevents_selection_after_download_and_prepare() {
    register();
    let key = TestKey::new(92);
    let keys = [key.public()];
    let layout = installed_layout("0.1.1", 50);
    let source = temp("yank-selection-fence");
    use_source(&layout, &source);
    ship(&layout, &source, &key, "0.1.1", Some("r2"), b"working-tool");
    let exec = FakeExec::default();
    install(
        Some(&layout),
        &keys,
        FAKE,
        &InstallRequest::default(),
        &exec,
        Lock::Acquire,
    )
    .unwrap();
    let mut doc = index_tool(&layout, &source, &key, "r3");
    doc["sequence"] = json!(2);
    doc["components"][0]["recommended"] = Value::Null;
    doc["components"][0]["releases"][0]["yanked"] = json!(true);
    let hook_layout = layout.clone();
    world(|w| {
        w.before_select = Some(Box::new(move || {
            accept_index(&hook_layout, &TestKey::new(92), &doc)
        }))
    });
    let error = install(
        Some(&layout),
        &keys,
        FAKE,
        &InstallRequest {
            version: Some("r3".into()),
            ..Default::default()
        },
        &exec,
        Lock::Acquire,
    )
    .unwrap_err();
    assert_eq!(code(&error), "component_yanked");
    assert_eq!(installed(&layout, FAKE).unwrap().selected, "r2");
    assert_eq!(world(|w| w.running.clone()).as_deref(), Some("r2"));
    assert!(!world(|w| w
        .calls
        .iter()
        .any(|call| call == "select r3 false")));
    assert!(
        version_dir(&layout, FAKE, "r3")
            .join(RECEIPT_FILE)
            .is_file(),
        "verified staged bytes are retained"
    );
}

#[test]
fn staged_needs_user_retains_working_bundled_default_and_no_selection_claim() {
    register();
    let key = TestKey::new(93);
    let keys = [key.public()];
    let layout = installed_layout("0.1.1", 50);
    let source = temp("needs-user-keeps-default");
    use_source(&layout, &source);
    ship(&layout, &source, &key, "0.1.1", Some("r2"), b"working-tool");
    let exec = FakeExec::default();
    install(
        Some(&layout),
        &keys,
        FAKE,
        &InstallRequest::default(),
        &exec,
        Lock::Acquire,
    )
    .unwrap();
    index_tool(&layout, &source, &key, "r3");
    world(|w| w.needs_user = true);
    let out = install(
        Some(&layout),
        &keys,
        FAKE,
        &InstallRequest {
            version: Some("r3".into()),
            ..Default::default()
        },
        &exec,
        Lock::Acquire,
    )
    .unwrap();
    assert_eq!(out["result"]["outcome"], "needsUser");
    assert_eq!(out["result"]["selected"], false);
    assert_eq!(out["component"]["selected"], "r2");
    assert_eq!(world(|w| w.running.clone()).as_deref(), Some("r2"));
    assert!(!world(|w| w
        .calls
        .iter()
        .any(|call| call == "select r3 false")));
}

#[test]
fn incompatible_index_recommendation_falls_back_to_manifest_but_null_does_not() {
    register();
    let key = TestKey::new(94);
    let keys = [key.public()];
    let layout = installed_layout("0.1.1", 50);
    let source = temp("manifest-fallback");
    use_source(&layout, &source);
    ship(&layout, &source, &key, "0.1.1", Some("r2"), b"working-tool");
    let mut doc = index_tool(&layout, &source, &key, "r3");
    doc["sequence"] = json!(2);
    doc["components"][0]["releases"][0]["compatible"]["allCapabilities"] =
        json!(["future-daemon-capability"]);
    accept_index(&layout, &key, &doc);
    let row = index::view(&layout, &keys, FAKE, None);
    assert_eq!(row["source"], "releaseManifest");
    assert_eq!(row["recommended"], "r2");
    assert_eq!(row["releases"][0]["compatible"], false);
    assert_eq!(
        index::resolve(&layout, &keys, FAKE, Some("r2"))
            .unwrap()
            .offer
            .version,
        "r2"
    );
    assert_eq!(
        code(&index::resolve(&layout, &keys, FAKE, Some("r3")).unwrap_err()),
        "component_incompatible"
    );
    doc["sequence"] = json!(3);
    doc["components"][0]["recommended"] = Value::Null;
    accept_index(&layout, &key, &doc);
    assert_eq!(
        index::view(&layout, &keys, FAKE, None)["recommended"],
        Value::Null
    );
    assert_eq!(
        code(&index::resolve(&layout, &keys, FAKE, Some("r2")).unwrap_err()),
        "component_offer_changed"
    );
}

#[test]
fn same_version_manifest_repair_preserves_independent_selection_provenance() {
    register();
    let key = TestKey::new(95);
    let keys = [key.public()];
    let layout = installed_layout("0.1.1", 50);
    let source = temp("same-version-provenance");
    use_source(&layout, &source);
    ship(&layout, &source, &key, "0.1.1", Some("r2"), b"working-tool");
    let exec = FakeExec::default();
    let doc = index_tool(&layout, &source, &key, "r3");
    install(
        Some(&layout),
        &keys,
        FAKE,
        &InstallRequest {
            version: Some("r3".into()),
            ..Default::default()
        },
        &exec,
        Lock::Acquire,
    )
    .unwrap();
    let release = &doc["components"][0]["releases"][0];
    let asset = &release["platforms"][platform_key()];
    let mut m = manifest::fixtures::manifest("0.1.1", 50);
    m.components.insert(
        FAKE.into(),
        ComponentPart {
            version: "r3".into(),
            artifacts: BTreeMap::from([(
                platform_key(),
                Artifact {
                    tag: Some(release["assetTag"].as_str().unwrap().into()),
                    name: asset["name"].as_str().unwrap().into(),
                    sha256: asset["sha256"].as_str().unwrap().into(),
                    size: asset["size"].as_u64().unwrap(),
                },
            )]),
        },
    );
    let bytes = serde_json::to_vec(&m).unwrap();
    let path = layout.release_dir("0.1.1");
    std::fs::write(path.join("manifest.json"), &bytes).unwrap();
    std::fs::write(
        path.join("manifest.json.minisig"),
        key.sign(&bytes, true, "manifest fixture"),
    )
    .unwrap();
    index::Policy::default().save(&layout).unwrap();
    install(
        Some(&layout),
        &keys,
        FAKE,
        &InstallRequest::default(),
        &exec,
        Lock::Acquire,
    )
    .unwrap();
    assert_eq!(
        read_receipt(&version_dir(&layout, FAKE, "r3"))
            .unwrap()
            .selection_source
            .unwrap()["kind"],
        "componentIndex"
    );
    ship(&layout, &source, &key, "0.1.2", Some("r4"), b"new-tool");
    activate(&layout, "0.1.1", "0.1.2");
    select_for_release(&layout, &keys, &exec, &layout.base.join("snapshot.json"));
    assert_eq!(installed(&layout, FAKE).unwrap().selected, "r3");
}

#[test]
fn same_version_other_platform_index_offer_uses_effective_manifest_fallback() {
    register();
    let key = TestKey::new(96);
    let keys = [key.public()];
    let layout = installed_layout("0.1.1", 50);
    let source = temp("same-version-fallback");
    use_source(&layout, &source);
    ship(&layout, &source, &key, "0.1.1", Some("r2"), b"working-tool");
    let mut doc = index_tool(&layout, &source, &key, "r3");
    let other = if platform_key() == "linux-x64" {
        "windows-x64"
    } else {
        "linux-x64"
    };
    let asset = doc["components"][0]["releases"][0]["platforms"][platform_key()].clone();
    doc["sequence"] = json!(2);
    doc["components"][0]["recommended"] = json!("r2");
    doc["components"][0]["releases"][0]["version"] = json!("r2");
    doc["components"][0]["releases"][0]["platforms"] = json!({other:asset});
    accept_index(&layout, &key, &doc);
    assert_eq!(
        index::view(&layout, &keys, FAKE, None)["source"],
        "releaseManifest"
    );
    assert_eq!(
        index::resolve(&layout, &keys, FAKE, Some("r2"))
            .unwrap()
            .provenance["kind"],
        "releaseManifest"
    );
    install(
        Some(&layout),
        &keys,
        FAKE,
        &InstallRequest {
            version: Some("r2".into()),
            ..Default::default()
        },
        &FakeExec::default(),
        Lock::Acquire,
    )
    .unwrap();
    assert_eq!(installed(&layout, FAKE).unwrap().selected, "r2");
}


#[test]
fn source_only_refusal_suggests_verified_explicit_version_without_selecting_it() {
    register();
    let key = TestKey::new(106);
    let keys = [key.public()];
    let layout = installed_layout("dev-source-only-ui", 0);
    let source = temp("source-only-index-ui");
    let mut document = index_tool(&layout, &source, &key, "r3");
    let exec = FakeExec::default();
    let error = install(Some(&layout), &keys, FAKE, &InstallRequest::default(), &exec, Lock::Acquire).unwrap_err();
    let refusal = error.downcast_ref::<UpgradeError>().unwrap();
    assert_eq!(refusal.code, "component_source_only");
    assert_eq!(refusal.details["offeredVersion"], "r3");
    assert_eq!(refusal.details["suggestedRequest"], json!({"version":"r3"}));
    assert!(installed(&layout, FAKE).is_none());
    assert!(!version_dir(&layout, FAKE, "r3").exists());
    assert!(world(|w| w.calls.is_empty()));
    let selected = install(Some(&layout), &keys, FAKE,
        &InstallRequest { version: Some("r3".into()), ..Default::default() }, &exec, Lock::Acquire).unwrap();
    assert_eq!(selected["result"]["selected"], true);
    assert_eq!(installed(&layout, FAKE).unwrap().selected, "r3");
    // Adopt the same canonical offer as a kit, then model the actual owner
    // bases: component-managed, with no backend install record.
    let index_dir = source.join("channel-components");
    std::fs::create_dir_all(&index_dir).unwrap();
    let bytes = serde_json::to_vec(&document).unwrap();
    std::fs::write(index_dir.join("index.json"), &bytes).unwrap();
    std::fs::write(index_dir.join("index.json.minisig"), key.sign(&bytes, true, "manual kit fixture")).unwrap();
    let kit = temp("manual-kit-parent").join("kit");
    index::freeze_kit(&layout, &keys, &kit, &[(FAKE.into(), "r3".into(), platform_key())]).unwrap();
    index::import_kit(&layout, &keys, &kit).unwrap();
    std::fs::remove_file(layout.install_record_path()).unwrap();
    assert!(release_manifest(&layout, &keys).unwrap().is_none());
    assert!(fetch_release_manifest(&layout, &keys).unwrap().is_none());
    assert_eq!(install_from(Some(&layout), &keys), "source-only");
    assert_eq!(list(Some(&layout), &keys, None)["managed"], true);
    let before = std::fs::read(state_path(&layout)).unwrap();
    let refusal = install(Some(&layout), &keys, FAKE, &InstallRequest::default(), &exec, Lock::Acquire).unwrap_err();
    let refusal = refusal.downcast_ref::<UpgradeError>().unwrap();
    assert_eq!((refusal.status, refusal.code), (409, "component_source_only"));
    assert_eq!(refusal.details["suggestedRequest"], json!({"version":"r3"}));
    assert_eq!(std::fs::read(state_path(&layout)).unwrap(), before);
    let selected = install(Some(&layout), &keys, FAKE,
        &InstallRequest { version: Some("r3".into()), ..Default::default() }, &exec, Lock::Acquire).unwrap();
    assert_eq!(selected["result"]["selected"], true);
    assert!(!layout.install_record_path().exists());
    document["sequence"] = json!(2);
    document["components"][0]["recommended"] = Value::Null;
    document["components"][0]["releases"][0]["yanked"] = json!(true);
    accept_index(&layout, &key, &document);
    let before = std::fs::read(state_path(&layout)).unwrap();
    let error = install(Some(&layout), &keys, FAKE, &InstallRequest::default(), &exec, Lock::Acquire).unwrap_err();
    let refusal = error.downcast_ref::<UpgradeError>().unwrap();
    assert_eq!(refusal.code, "component_source_only");
    assert!(refusal.details.get("offeredVersion").is_none(), "never suggest a withdrawn identity");
    assert_eq!(std::fs::read(state_path(&layout)).unwrap(), before);
}
