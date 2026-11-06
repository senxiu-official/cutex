//! Central external-runtime wire regressions. Inert payloads and fake probes only.
//! The lead declares this module and wires view() into status_of/check_external.
use super::*;
use crate::upgrade::minisign::test_key::TestKey;
use crate::upgrade::stage::fixtures::installed_layout;
use std::cell::Cell;
use std::ffi::OsString;

struct PrivateHome {
    home: crate::profiles::lifecycle::tests::TestHome,
    previous: Vec<(&'static str, Option<OsString>)>,
}
impl PrivateHome {
    fn new() -> Self {
        let home = crate::profiles::lifecycle::tests::TestHome::new();
        put(
            &home.root().join(crate::config::paths::PRIVATE_HOME_MARKER),
            b"r270 wire tests",
        );
        let mut guard = Self {
            home,
            previous: Vec::new(),
        };
        for (name, value) in [
            ("CUTEX_HOME", guard.home.root().join(".cutex")),
            ("CUTEX_TEST_PRIVATE_HOME", guard.home.root().to_path_buf()),
            ("CUTEX_INSTALL_BASE", guard.home.root().join("unused-base")),
        ] {
            guard.set(name, Some(value.into_os_string()));
        }
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
            "PATH",
        ] {
            guard.set(name, None);
        }
        guard
    }
    fn set(&mut self, name: &'static str, value: Option<OsString>) {
        self.previous.push((name, std::env::var_os(name)));
        unsafe {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}
impl Drop for PrivateHome {
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

fn put(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

// Include every file's bytes and every directory/link identity, not just state.json.
type Tree = BTreeMap<PathBuf, (String, Vec<u8>)>;
fn tree(root: &Path) -> Tree {
    let mut result = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        if !path.exists() && !path.is_symlink() {
            continue;
        }
        let kind = std::fs::symlink_metadata(&path).unwrap().file_type();
        let (label, bytes) = if kind.is_symlink() {
            (
                "link",
                std::fs::read_link(&path)
                    .unwrap()
                    .to_string_lossy()
                    .as_bytes()
                    .to_vec(),
            )
        } else if kind.is_dir() {
            pending.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
            ("dir", Vec::new())
        } else {
            ("file", std::fs::read(&path).unwrap())
        };
        result.insert(path, (label.into(), bytes));
    }
    result
}

fn binary(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.into()
    });
    // Unix tripwire: accidental RealExec on GET creates a new file and fails
    // the complete-tree assertion. Windows gates may supply their native sentinel.
    if let Some(sentinel) = std::env::var_os("CUTEX_TEST_PROBE_SENTINEL") {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::copy(sentinel, &path).unwrap();
    } else {
        put(
            &path,
            b"#!/bin/sh\nprintf 'unexpected GET exec' > \"$0.log\"\nexit 1\n",
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::canonicalize(path).unwrap()
}

#[derive(Default)]
struct Probe {
    calls: Cell<usize>,
    multiline_failure: bool,
}
const STACK_TAIL: &str = "r270-diagnostic-tail";
impl Exec for Probe {
    fn run(&self, program: &str, args: &[String]) -> anyhow::Result<String> {
        self.calls.set(self.calls.get() + 1);
        if Path::new(program).file_stem().unwrap() == "rg" {
            assert_eq!(args, ["--version"]);
            return Ok("ripgrep 14.1.1\n".into());
        }
        if args == ["--version"] {
            return Ok("v24.21.0\n".into());
        }
        assert!(args.len() >= 3);
        assert_eq!(&args[..2], ["--input-type=module", "-e"]);
        assert!(!args[2].contains("pty.spawn("));
        assert!(!args[2].contains("binding.startProcess("));
        if args[2].contains("cutex-entries-v1") {
            if self.multiline_failure {
                anyhow::bail!(
                    "Error: entry resolution failed {}\n{}\n{}",
                    "x".repeat(300),
                    "stack-frame\r\n".repeat(60),
                    STACK_TAIL
                );
            }
            return Ok(json!({"marker":"cutex-entries-v1", "issues":[]}).to_string());
        }
        if args[2].contains("node:sqlite") {
            return Ok("cutex-sqlite-ok\n".into());
        }
        assert!(args[2].contains("createRequire"));
        Ok("cutex-pty-ok\n".into())
    }
}

struct Fixture {
    home: PrivateHome,
    layout: Layout,
    key: TestKey,
    name: String,
    version: String,
    contract: external_runtime::ExternalRuntime,
    prefix: PathBuf,
    node: PathBuf,
    rg: PathBuf,
}
impl Fixture {
    fn new(name: &str) -> Self {
        let home = PrivateHome::new();
        let layout = installed_layout("dev-r270-wire", 0);
        let key = TestKey::new(137);
        let bytes = include_bytes!("../../channel-components/index.json");
        let index = index_model::Index::parse(bytes).unwrap();
        let component = index.components.iter().find(|c| c.name == name).unwrap();
        let release = component
            .releases
            .iter()
            .find(|r| Some(&r.version) == component.recommended.as_ref())
            .unwrap();
        let contract: external_runtime::ExternalRuntime =
            serde_json::from_value(release.external_runtime.clone().unwrap()).unwrap();
        let source = home.home.root().join("offline-mirror");
        std::fs::create_dir_all(&source).unwrap();
        index::Policy {
            policy: "componentIndex".into(),
            channel: index.channel.clone(),
            source: Some(source.display().to_string()),
            ..Default::default()
        }
        .save(&layout)
        .unwrap();
        index::accept(
            &layout,
            &[key.public()],
            &index.channel,
            bytes,
            &key.sign(bytes, true, "r270 saved candidate fixture"),
        )
        .unwrap();
        save_state(&layout, &State::new()).unwrap();
        crate::session::store::save_cutex_session_store(
            &crate::session::model::CutexSessionStore::default(),
        )
        .unwrap();
        for path in [
            crate::runtime::dsh::deployment_path().unwrap(),
            crate::runtime::kimi::deployment_path().unwrap(),
        ] {
            put(&path, br#"{"inertDefault":"must remain byte-identical"}"#);
        }
        let prefix = home.home.root().join("prefix");
        for (package, version) in &contract.external_root.packages {
            put(
                &prefix
                    .join("node_modules")
                    .join(package)
                    .join("package.json"),
                &serde_json::to_vec(&json!({"name":package,"version":version})).unwrap(),
            );
        }
        for entry in std::iter::once(&contract.external_root.cli_entry)
            .chain(contract.external_root.catalog_entry.iter())
        {
            put(
                &prefix.join(entry),
                b"throw Error('vendor code must never execute');\n",
            );
        }
        let node = binary(&home.home.root().join("bin"), "node");
        let rg = binary(&home.home.root().join("bin"), "rg");
        Self {
            home,
            layout,
            key,
            name: name.into(),
            version: release.version.clone(),
            contract,
            prefix: std::fs::canonicalize(prefix).unwrap(),
            node,
            rg,
        }
    }
    fn keys(&self) -> Vec<PublicKey> {
        vec![self.key.public()]
    }
    fn status(&self) -> Value {
        status_of(
            Some(&self.layout),
            &self.keys(),
            handler(&self.name).unwrap().as_ref(),
            None,
        )
    }
    fn checked(&self, request: Option<Value>, exec: &Probe) -> Value {
        check_external(
            Some(&self.layout),
            &self.keys(),
            &self.name,
            request.map(|v| serde_json::from_value(v).unwrap()),
            exec,
            Lock::Acquire,
        )
        .unwrap()
    }
    fn paths(&self) -> Value {
        json!({"externalRoot":self.prefix, "node":self.node,
            "rg":if self.name == "kimi-runtime" {json!(self.rg)} else {Value::Null}})
    }
    fn display_paths(&self) -> Value {
        // Fixture paths are local-drive paths, never device/UNC targets.
        let display = |path: &Path| {
            let raw = path.to_string_lossy();
            raw.strip_prefix(r"\\?\").unwrap_or(&raw).to_string()
        };
        json!({"externalRoot":display(&self.prefix), "node":display(&self.node),
            "rg":if self.name == "kimi-runtime" {json!(display(&self.rg))} else {Value::Null}})
    }
    fn protected(&self) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        (
            std::fs::read(state_path(&self.layout)).unwrap(),
            std::fs::read(crate::runtime::dsh::deployment_path().unwrap()).unwrap(),
            std::fs::read(crate::runtime::kimi::deployment_path().unwrap()).unwrap(),
        )
    }
    fn readonly_get(&self, exec: &Probe) -> Value {
        let before = (tree(self.home.home.root()), tree(&self.layout.base));
        let calls = exec.calls.get();
        let first = self.status();
        assert_eq!(
            first["external"],
            external_runtime::view(&self.layout, &self.name, &self.contract).unwrap()
        );
        for _ in 0..2 {
            assert_eq!(self.status(), first);
        }
        assert_eq!(exec.calls.get(), calls, "GET must not probe");
        assert_eq!(
            (tree(self.home.home.root()), tree(&self.layout.base)),
            before,
            "GET changed file bytes/identities or launched a dependency tripwire"
        );
        assert!(!self.node.with_extension("log").exists());
        assert!(!self.rg.with_extension("log").exists());
        first
    }
}

#[test]
fn saved_signed_candidate_has_external_view_before_download_but_no_local_versions() {
    for name in ["dsh-runtime", "kimi-runtime"] {
        let f = Fixture::new(name);
        let exec = Probe::default();
        let protected = f.protected();
        let row = f.readonly_get(&exec);
        assert_eq!(row["external"]["version"], f.version);
        assert_eq!(
            row["external"]["storedConfig"],
            json!({"externalRoot":null,"node":null,"rg":null})
        );
        assert_eq!(
            row["external"]["resolved"],
            json!({"externalRoot":null,"node":null,"rg":null})
        );
        assert_eq!(row["versions"], json!([]));
        assert!(row["selected"].is_null());
        assert!(!version_dir(&f.layout, name, &f.version).exists());
        let checked = f.checked(Some(f.paths()), &exec);
        assert_eq!(checked["result"]["outcome"], "ready");
        assert_eq!(checked["result"]["selected"], false);
        assert_eq!(checked["result"]["external"]["storedConfig"], f.paths());
        assert_eq!(checked["result"]["external"]["resolved"], f.display_paths());
        assert!(exec.calls.get() > 0);
        let row = f.readonly_get(&exec);
        assert_eq!(row["external"], checked["result"]["external"]);
        assert_eq!(row["versions"], json!([]));
        assert!(row["selected"].is_null());
        assert_eq!(
            f.protected(),
            protected,
            "configure selected a candidate or changed defaults"
        );
        let rechecked = f.checked(None, &exec);
        assert_eq!(rechecked["result"]["external"]["storedConfig"], f.paths());
        assert_eq!(f.protected(), protected);
    }
}

#[test]
fn raw_overrides_are_distinct_from_resolved_paths_and_null_clear_restores_discovery() {
    for name in ["dsh-runtime", "kimi-runtime"] {
        let mut f = Fixture::new(name);
        let exec = Probe::default();
        let protected = f.protected();
        // Keep lexical .. in the raw input while resolution canonicalizes it.
        let raw_root = f.prefix.join("../prefix");
        let raw_node = f
            .node
            .parent()
            .unwrap()
            .join("../bin")
            .join(f.node.file_name().unwrap());
        let raw_rg =
            f.rg.parent()
                .unwrap()
                .join("../bin")
                .join(f.rg.file_name().unwrap());
        let raw = json!({"externalRoot":raw_root,"node":raw_node,
            "rg":if name == "kimi-runtime" {json!(raw_rg)} else {Value::Null}});
        let checked = f.checked(Some(raw.clone()), &exec);
        assert_eq!(checked["result"]["external"]["storedConfig"], raw);
        assert_eq!(
            checked["result"]["external"]["resolved"]["externalRoot"],
            f.display_paths()["externalRoot"]
        );
        assert_eq!(
            checked["result"]["external"]["resolved"]["node"],
            f.display_paths()["node"]
        );
        f.home.set(
            if name == "dsh-runtime" {
                "DSH_INSTALL_ROOT"
            } else {
                "KIMI_INSTALL_ROOT"
            },
            Some(f.prefix.clone().into_os_string()),
        );
        f.home.set(
            "PATH",
            Some(std::env::join_paths([f.node.parent().unwrap()]).unwrap()),
        );
        let cleared = f.checked(
            Some(json!({"externalRoot":null,"node":null,"rg":null})),
            &exec,
        );
        assert_eq!(cleared["result"]["outcome"], "ready");
        assert_eq!(cleared["result"]["selected"], false);
        assert_eq!(
            cleared["result"]["external"]["storedConfig"],
            json!({"externalRoot":null,"node":null,"rg":null})
        );
        assert_eq!(cleared["result"]["external"]["resolved"], f.display_paths());
        assert_eq!(
            f.readonly_get(&exec)["external"],
            cleared["result"]["external"]
        );
        f.home.set(
            if name == "dsh-runtime" {
                "DSH_INSTALL_ROOT"
            } else {
                "KIMI_INSTALL_ROOT"
            },
            None,
        );
        f.home.set("PATH", None);
        let undiscovered = f.checked(None, &exec);
        assert_eq!(undiscovered["result"]["outcome"], "needsUser");
        assert_eq!(
            undiscovered["result"]["external"]["storedConfig"],
            json!({"externalRoot":null,"node":null,"rg":null})
        );
        assert_eq!(
            undiscovered["result"]["external"]["resolved"],
            json!({"externalRoot":null,"node":null,"rg":null})
        );
        f.readonly_get(&exec);
        assert_eq!(f.protected(), protected);
    }
}

#[test]
fn multiline_probe_failure_is_short_on_wire_with_full_diagnostics_and_unchanged_internal_cache() {
    for name in ["dsh-runtime", "kimi-runtime"] {
        let f = Fixture::new(name);
        let exec = Probe {
            multiline_failure: true,
            ..Default::default()
        };
        let protected = f.protected();
        let checked = f.checked(Some(f.paths()), &exec);
        assert_eq!(checked["result"]["outcome"], "needsUser");
        assert_eq!(checked["result"]["selected"], false);
        let external = &checked["result"]["external"];
        let issue = external["issues"]
            .as_array()
            .unwrap()
            .iter()
            .find(|issue| issue["code"] == "external_root_invalid")
            .unwrap();
        let actual = issue["actual"].as_str().unwrap();
        assert!(!actual.is_empty() && actual.chars().count() <= 240);
        assert!(!actual.contains(['\n', '\r']));
        let diagnostics = serde_json::to_string(external).unwrap();
        assert!(
            diagnostics.contains(STACK_TAIL),
            "full bounded error unavailable in diagnostics"
        );
        assert!(
            issue.get("diagnostics").is_some() || external.get("diagnostics").is_some(),
            "full errors need an explicit diagnostics field"
        );
        let persisted = external_runtime::cached(&f.layout, name, &f.contract).unwrap();
        let persisted_issue = persisted
            .issues
            .iter()
            .find(|issue| issue.code == "external_root_invalid")
            .unwrap();
        assert!(persisted_issue.actual.as_deref().unwrap().contains('\n'));
        assert!(
            persisted_issue
                .actual
                .as_deref()
                .unwrap()
                .contains(STACK_TAIL)
        );
        assert!(
            !serde_json::to_value(&persisted)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("storedConfig")
        );
        assert_eq!(f.readonly_get(&exec)["external"], *external);
        assert_eq!(f.protected(), protected);
    }
}
