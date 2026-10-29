use super::*;

/// The shared HOME lock plus private instance / temp / install paths: no test
/// may discover the owner's selection or default component directory.
struct PrivateHome {
    home: crate::profiles::lifecycle::tests::TestHome,
    previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
}
impl PrivateHome {
    fn new() -> Self {
        let home = crate::profiles::lifecycle::tests::TestHome::new();
        let root = home.root().to_path_buf();
        std::fs::write(root.join(crate::config::paths::PRIVATE_HOME_MARKER), "test").unwrap();
        std::fs::create_dir_all(root.join("tmp")).unwrap();
        let mut previous = Vec::new();
        for (key, value) in [
            ("CUTEX_HOME", root.join(".cutex")),
            ("CUTEX_TEST_PRIVATE_HOME", root.clone()),
            ("TEMP", root.join("tmp")),
            ("TMP", root.join("tmp")),
            ("TMPDIR", root.join("tmp")),
            ("CUTEX_INSTALL_BASE", root.join("unmanaged-base")),
        ] {
            previous.push((key, std::env::var_os(key)));
            unsafe { std::env::set_var(key, value) };
        }
        Self { home, previous }
    }
    fn root(&self) -> &Path {
        self.home.root()
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

fn fixture_payload(dir: &Path, kimi_version: &str) {
    for file in REQUIRED_FILES {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "// fixture\n").unwrap();
    }
    std::fs::write(dir.join("component.json"), include_str!("../../components/kimi-runtime/component.json")).unwrap();
    for (file, name, version) in [
        ("runtime/node_modules/@moonshot-ai/kimi-code/package.json", "@moonshot-ai/kimi-code", kimi_version),
        ("runtime/node_modules/ws/package.json", "ws", "8.22.0"),
        ("node_modules/ws/package.json", "ws", "8.22.0"),
    ] {
        std::fs::write(dir.join(file), json!({"name": name, "version": version}).to_string()).unwrap();
    }
}

struct FakeNode(&'static str);
impl Exec for FakeNode {
    fn run(&self, _program: &str, args: &[String]) -> anyhow::Result<String> {
        assert_eq!(args, ["--version"]);
        Ok(self.0.into())
    }
}

#[test]
fn metadata_pins_kimi_and_platforms() {
    let runtime = KimiRuntime::default();
    assert_eq!(runtime.metadata().name, NAME);
    assert!(runtime.metadata().platforms.iter().any(|platform| platform == "linux-x64"));
    assert_eq!(requirements()["version"], PINNED_KIMI_VERSION);
    assert_eq!(requirements()["node"]["minimum"], NODE_MINIMUM);
    assert!(crate::components::handlers().iter().any(|handler| handler.metadata().name == NAME));
}

#[test]
fn layout_requires_the_pinned_kimi_and_every_adapter_file() {
    let home = PrivateHome::new();
    let good = home.root().join("good");
    fixture_payload(&good, PINNED_KIMI_VERSION);
    check_layout(&good).unwrap();
    let wrong = home.root().join("wrong");
    fixture_payload(&wrong, "2.4.0");
    assert!(check_layout(&wrong).unwrap_err().to_string().contains("@moonshot-ai/kimi-code@2.3.3"));
    std::fs::remove_file(good.join("src/session-ops.mjs")).unwrap();
    assert!(check_layout(&good).is_err());
}

#[test]
fn probe_reports_old_node_as_needs_user_without_selecting() {
    let home = PrivateHome::new();
    let dir = home.root().join("payload");
    fixture_payload(&dir, PINNED_KIMI_VERSION);
    let node = home.root().join("node");
    std::fs::write(&node, "").unwrap();
    let options = Options { node: Some(node.clone()) };
    let old = probe_dir(&dir, &options, &FakeNode("v22.18.0"));
    assert_eq!(old["state"], states::NEEDS_USER);
    assert_eq!(old["needsUser"]["code"], "kimi_node_required");
    let ready = probe_dir(&dir, &options, &FakeNode("v22.19.0"));
    assert_eq!(ready["state"], states::DEGRADED, "ready Node but not selected: {ready}");
    assert_eq!(ready["kimi"]["node"]["ready"], true);
}

#[test]
fn source_install_rejects_links_and_needs_staged_packages() {
    let home = PrivateHome::new();
    let source = home.root().join("source");
    std::fs::create_dir_all(source.join("src")).unwrap();
    for file in ["package.json", "package-lock.json"] {
        std::fs::write(source.join(file), "{}").unwrap();
    }
    // r267: a tree of another component version (an older kimi-r1 source) is refused first.
    let current = include_str!("../../components/kimi-runtime/component.json");
    std::fs::write(source.join("component.json"), current.replace("\"kimi-r2\"", "\"kimi-r1\"")).unwrap();
    let error = KimiRuntime::default().from_source(&source, &home.root().join("out")).unwrap_err().to_string();
    assert!(error.contains("kimi-r1") && error.contains("kimi-r2"), "{error}");
    std::fs::write(source.join("component.json"), current).unwrap();
    let error = KimiRuntime::default().from_source(&source, &home.root().join("out")).unwrap_err().to_string();
    assert!(error.contains("no packages are downloaded"), "{error}");
    #[cfg(unix)]
    {
        fixture_payload(&source, PINNED_KIMI_VERSION);
        std::os::unix::fs::symlink("/etc/hostname", source.join("src/link.mjs")).unwrap();
        let error = KimiRuntime::default().from_source(&source, &home.root().join("out2")).unwrap_err().to_string();
        assert!(error.contains("links"), "{error}");
    }
}

/// The real pinned Kimi Code tree (`CUTEX_KIMI_TEST_INSTALL` = an offline
/// `npm install @moonshot-ai/kimi-code@2.3.3` prefix) with the repository's
/// adapter: staged like `cutex component install --from-source`, selected
/// with the host Node, then answering the Profile layer's hooks.
#[test]
fn real_payload_stages_selects_and_answers_profile_hooks() {
    let Some(install) = std::env::var_os("CUTEX_KIMI_TEST_INSTALL").map(PathBuf::from) else {
        eprintln!("skipped: CUTEX_KIMI_TEST_INSTALL not set");
        return;
    };
    let node = std::env::var_os("CUTEX_KIMI_TEST_NODE").map(PathBuf::from).or_else(|| node_path(Path::new("/nonexistent"), &Options::default()));
    let Some(node) = node else {
        eprintln!("skipped: no Node");
        return;
    };
    let home = PrivateHome::new();
    // An explicit source: the adapter checkout plus a staged runtime/.
    let source = home.root().join("source");
    let adapter = Path::new(env!("CARGO_MANIFEST_DIR")).join("components").join(NAME);
    std::fs::create_dir_all(&source).unwrap();
    for file in ["component.json", "package.json", "package-lock.json"] {
        std::fs::copy(adapter.join(file), source.join(file)).unwrap();
    }
    copy_tree(&adapter.join("src"), &source.join("src")).unwrap();
    std::fs::create_dir_all(source.join("runtime")).unwrap();
    for file in ["package.json", "package-lock.json"] {
        std::fs::copy(install.join(file), source.join("runtime").join(file)).unwrap();
    }
    copy_tree(&install.join("node_modules"), &source.join("runtime/node_modules")).unwrap();
    let dest = home.root().join("components").join(NAME).join("kimi-r1");
    KimiRuntime::default().from_source(&source, &dest).unwrap();
    check_layout(&dest).unwrap();
    assert!(!dest.join("runtime/node_modules/.bin").exists());
    let deployment = deployment_for(&dest, node);
    deployment.install().unwrap();
    assert!(KimiDeployment::selected().is_ok());
    assert!(advertised_with(None), "a hand-selected deployment is advertised");
    assert_eq!(component_probe(), (crate::profiles::runtime_variants::Status::Supported, None));
    let cli = cli_locator().unwrap();
    assert_eq!(cli.args, vec![dest.join(KIMI_MAIN).to_string_lossy().into_owned()]);
    assert!(cli.env.iter().any(|(key, value)| key == "KIMI_CODE_WATCH" && value == "0"));
    let version = std::process::Command::new(&cli.program).args(&cli.args).arg("--version")
        .env("HOME", home.root()).env("KIMI_CODE_HOME", home.root().join("kimi-version")).envs(cli.env.iter().cloned()).output().unwrap();
    assert!(String::from_utf8_lossy(&version.stdout).contains(PINNED_KIMI_VERSION), "{version:?}");
}

#[test]
fn pins_count_kimi_agents_by_their_component_version() {
    let root = PathBuf::from("/c/components/kimi-runtime");
    let sessions = vec![
        json!({"install_root": "/c/components/kimi-runtime/kimi-r1/runtime", "flavor": "kimi"}),
        json!({"install_root": "/c/components/kimi-runtime/kimi-r1/runtime", "flavor": "kimi"}),
        json!({"install_root": "/c/components/dsh-runtime/dsh-r9/runtime"}),
    ];
    let pins = crate::components::dsh_runtime::pins_for(&root, sessions);
    assert_eq!(pins.get("kimi-r1"), Some(&2));
    assert_eq!(pins.len(), 1);
}
