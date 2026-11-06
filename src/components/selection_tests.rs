//! Full source-install regressions for central selection results and Exec fencing.
use super::*;
use crate::upgrade::stage::fixtures::{installed_layout, temp};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy)]
enum Mode {
    Ready,
    PrepareBlocked,
    SelectBlocked,
    SelectedWithHint,
}

struct SelectionHandler {
    metadata: Metadata,
    mode: Mode,
    prepares: AtomicUsize,
    selects: AtomicUsize,
}

impl SelectionHandler {
    fn new(name: &str, mode: Mode) -> Self {
        Self {
            metadata: Metadata {
                schema: COMPONENT_SCHEMA.into(),
                name: name.into(),
                version: "candidate-r2".into(),
                platforms: vec![platform_key()],
            },
            mode,
            prepares: AtomicUsize::new(0),
            selects: AtomicUsize::new(0),
        }
    }

    fn node_version(ctx: &Context) -> anyhow::Result<String> {
        ctx.exec.run("fixture-node", &["--version".into()])
    }
}

impl Handler for SelectionHandler {
    fn metadata(&self) -> &Metadata {
        &self.metadata
    }
    fn required_files(&self) -> &'static [&'static str] {
        &["tool"]
    }
    fn from_source(&self, source: &Path, dest: &Path) -> anyhow::Result<()> {
        std::fs::copy(source.join("tool"), dest.join("tool"))?;
        Ok(())
    }
    fn external(&self, _: &Layout) -> bool {
        false
    }
    fn prepare(&self, ctx: &Context) -> anyhow::Result<Value> {
        self.prepares.fetch_add(1, Ordering::SeqCst);
        verify_file(&ctx.dir, "tool")?;
        let version = Self::node_version(ctx)?;
        // A non-version command must pass through in both phases. This Exec is
        // entirely fake; no subprocess or runtime configuration is involved.
        ctx.exec
            .run("fixture-node", &["--fixture-mutation".into()])?;
        Ok(match self.mode {
            Mode::PrepareBlocked => {
                json!({"prepared":false,"needsUser":{"steps":["fix fixture dependency"]}})
            }
            Mode::SelectedWithHint => json!({"prepared":true,"nodeVersion":version,
                "needsUser":{"steps":["login after selection"]}}),
            _ => json!({"prepared":true,"nodeVersion":version}),
        })
    }
    fn select(&self, ctx: &Context) -> anyhow::Result<Value> {
        self.selects.fetch_add(1, Ordering::SeqCst);
        let version = Self::node_version(ctx)?;
        ctx.exec
            .run("fixture-node", &["--fixture-mutation".into()])?;
        Ok(match self.mode {
            Mode::SelectBlocked => {
                json!({"selected":false,"needsUser":{"steps":["fix fixture selection"]}})
            }
            Mode::SelectedWithHint => json!({"selected":true,"nodeVersion":version,
                "needsUser":{"steps":["login after selection"]}}),
            _ => json!({"selected":true,"nodeVersion":version}),
        })
    }
    fn deselect(&self, _: &Context, _: bool) -> anyhow::Result<Vec<String>> {
        Ok(vec![])
    }
    fn probe(&self, _: &Layout, _: &Entry) -> Value {
        json!({"state":states::READY})
    }
    fn pins(&self, _: &Layout) -> BTreeMap<String, usize> {
        BTreeMap::new()
    }
    fn restart_gate(&self, _: &Layout) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Default)]
struct CountingExec {
    versions: AtomicUsize,
    mutations: AtomicUsize,
}
impl Exec for CountingExec {
    fn run(&self, program: &str, args: &[String]) -> anyhow::Result<String> {
        assert_eq!(program, "fixture-node", "unexpected real-program request");
        if args == ["--version"] {
            self.versions.fetch_add(1, Ordering::SeqCst);
            Ok("v24.21.0\n".into())
        } else {
            assert_eq!(args, ["--fixture-mutation"]);
            self.mutations.fetch_add(1, Ordering::SeqCst);
            Ok(String::new())
        }
    }
}

struct Fixture {
    layout: Layout,
    checkout: PathBuf,
    old: Entry,
    old_receipt: Vec<u8>,
}
impl Fixture {
    fn new(name: &str) -> Self {
        let layout = installed_layout("dev-selection-fixture", 0);
        let checkout = temp("component-selection-checkout");
        std::fs::write(checkout.join("tool"), b"inert candidate source tool").unwrap();
        let old = Entry {
            selected: "bundled-old".into(),
            origin: ORIGIN_SIGNED.into(),
            installed_at: "2026-10-28T00:00:00Z".into(),
            last_error: Some("previous diagnostic".into()),
        };
        save_state(&layout, &BTreeMap::from([(name.into(), old.clone())])).unwrap();
        let dir = version_dir(&layout, name, &old.selected);
        std::fs::create_dir_all(&dir).unwrap();
        let tool = b"inert previous tool";
        std::fs::write(dir.join("tool"), tool).unwrap();
        let receipt = Receipt {
            schema: RECEIPT_SCHEMA.into(),
            name: name.into(),
            version: old.selected.clone(),
            origin: ORIGIN_SIGNED.into(),
            archive_sha256: Some("a".repeat(64)),
            selection_source: Some(
                json!({"kind":"componentIndex","channel":"public-stable","sequence":1,
                "indexSha256":"b".repeat(64),"keyId":"fixture-key","archiveSha256":"a".repeat(64)}),
            ),
            files: BTreeMap::from([("tool".into(), crate::upgrade::manifest::sha256_hex(tool))]),
            unpacked_at: "2026-10-28T00:00:00Z".into(),
        };
        let old_receipt = serde_json::to_vec_pretty(&receipt).unwrap();
        std::fs::write(dir.join(RECEIPT_FILE), &old_receipt).unwrap();
        Self {
            layout,
            checkout,
            old,
            old_receipt,
        }
    }

    fn install(&self, handler: &SelectionHandler, exec: &CountingExec) -> Value {
        install_with(
            Some(&self.layout),
            &[],
            handler,
            &InstallRequest {
                from_source: Some(self.checkout.clone()),
                ..Default::default()
            },
            exec,
            Lock::Acquire,
        )
        .unwrap()
    }

    fn assert_old_selection(&self, name: &str, before_state: &[u8]) {
        assert_eq!(installed(&self.layout, name).unwrap(), self.old);
        assert_eq!(
            std::fs::read(state_path(&self.layout)).unwrap(),
            before_state
        );
        assert_eq!(
            std::fs::read(version_dir(&self.layout, name, &self.old.selected).join(RECEIPT_FILE))
                .unwrap(),
            self.old_receipt,
            "staging must retain the selected version's provenance"
        );
    }
}

#[test]
fn full_install_replays_prepared_node_version_but_never_caches_mutating_commands() {
    for name in ["dsh-runtime", "kimi-runtime"] {
        let fixture = Fixture::new(name);
        let handler = SelectionHandler::new(name, Mode::Ready);
        let exec = CountingExec::default();
        let out = fixture.install(&handler, &exec);
        assert_eq!(handler.prepares.load(Ordering::SeqCst), 1);
        assert_eq!(handler.selects.load(Ordering::SeqCst), 1);
        assert_eq!(
            exec.versions.load(Ordering::SeqCst),
            1,
            "select must reuse prepare's Node probe"
        );
        assert_eq!(
            exec.mutations.load(Ordering::SeqCst),
            2,
            "non-version requests must always reach Exec"
        );
        assert_eq!(out["result"]["prepare"]["nodeVersion"], "v24.21.0\n");
        assert_eq!(out["result"]["select"]["nodeVersion"], "v24.21.0\n");
        assert_eq!(out["result"]["outcome"], "selected");
        assert_eq!(out["result"]["selected"], true);
        assert_eq!(
            installed(&fixture.layout, name).unwrap().selected,
            out["result"]["version"].as_str().unwrap()
        );
        assert_ne!(
            installed(&fixture.layout, name).unwrap().selected,
            fixture.old.selected
        );
    }
}

#[test]
fn prepare_false_with_user_step_stages_without_select_or_changing_prior_entry() {
    for name in ["dsh-runtime", "kimi-runtime"] {
        let fixture = Fixture::new(name);
        let before = std::fs::read(state_path(&fixture.layout)).unwrap();
        let handler = SelectionHandler::new(name, Mode::PrepareBlocked);
        let exec = CountingExec::default();
        let out = fixture.install(&handler, &exec);
        assert_eq!(out["result"]["outcome"], "needsUser");
        assert_eq!(out["result"]["selected"], false);
        assert_eq!(out["component"]["selected"], fixture.old.selected);
        assert_eq!(handler.selects.load(Ordering::SeqCst), 0);
        assert_eq!(exec.versions.load(Ordering::SeqCst), 1);
        assert_eq!(exec.mutations.load(Ordering::SeqCst), 1);
        let candidate = out["result"]["version"].as_str().unwrap();
        assert!(version_dir(&fixture.layout, name, candidate)
            .join(RECEIPT_FILE)
            .is_file());
        fixture.assert_old_selection(name, &before);
    }
}

#[test]
fn select_false_with_user_step_keeps_prior_entry_and_selection_provenance() {
    for name in ["dsh-runtime", "kimi-runtime"] {
        let fixture = Fixture::new(name);
        let before = std::fs::read(state_path(&fixture.layout)).unwrap();
        let handler = SelectionHandler::new(name, Mode::SelectBlocked);
        let exec = CountingExec::default();
        let out = fixture.install(&handler, &exec);
        assert_eq!(out["result"]["outcome"], "needsUser");
        assert_eq!(out["result"]["selected"], false);
        assert_eq!(out["result"]["select"]["selected"], false);
        assert_eq!(out["component"]["selected"], fixture.old.selected);
        assert_eq!(handler.selects.load(Ordering::SeqCst), 1);
        assert_eq!(exec.versions.load(Ordering::SeqCst), 1);
        fixture.assert_old_selection(name, &before);
    }
}

#[test]
fn successful_selection_with_login_hint_is_not_misclassified_as_blocked() {
    for name in ["dsh-runtime", "kimi-runtime"] {
        let fixture = Fixture::new(name);
        let handler = SelectionHandler::new(name, Mode::SelectedWithHint);
        let exec = CountingExec::default();
        let out = fixture.install(&handler, &exec);
        assert_eq!(out["result"]["prepare"]["prepared"], true);
        assert!(out["result"]["prepare"].get("needsUser").is_some());
        assert_eq!(handler.selects.load(Ordering::SeqCst), 1);
        assert_eq!(out["result"]["select"]["selected"], true);
        assert!(out["result"]["select"].get("needsUser").is_some());
        assert_eq!(out["result"]["outcome"], "selected");
        assert_eq!(out["result"]["selected"], true);
        let entry = installed(&fixture.layout, name).unwrap();
        assert_eq!(entry.selected, out["result"]["version"].as_str().unwrap());
        assert_eq!(entry.origin, ORIGIN_SOURCE);
        assert_eq!(entry.last_error, None);
        assert_ne!(entry.selected, fixture.old.selected);
        assert_eq!(exec.versions.load(Ordering::SeqCst), 1);
    }
}
