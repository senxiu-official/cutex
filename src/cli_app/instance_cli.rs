//! `cutex instance init|show|use-runtime` (docs/INSTANCES.md).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use cutex::cli::args::InstanceCommand;
use cutex::config::instance::{self, InitOptions};
use cutex::launch::local_deployment::LocalDeployment;
use cutex::launch::stock::{StockBundle, VerifiedFile};

pub(super) fn run(command: InstanceCommand) -> anyhow::Result<()> {
    match command {
        InstanceCommand::Init {
            name,
            management_port,
            agent_bus_port,
            remote_listeners,
        } => init(InitOptions {
            name,
            management_port,
            agent_bus_port,
            remote_listeners,
        }),
        InstanceCommand::Show { json } => show(json),
        InstanceCommand::UseRuntime {
            bundle_manifest,
            registry,
        } => use_runtime(&bundle_manifest, registry.as_deref()),
    }
}

fn require_custom_home() -> anyhow::Result<PathBuf> {
    instance::custom_home()?.with_context(|| {
        format!(
            "set {}=<absolute dir> to select an independent instance (the default ~/.cutex is not one)",
            instance::HOME_ENV
        )
    })
}

fn init(options: InitOptions) -> anyhow::Result<()> {
    let home = require_custom_home()?;
    // Creates the home (0700 / protected current-user DACL) or secures an
    // existing one; then the guard must pass.
    cutex::management::client_tokens::ensure_private_dir(&home)?;
    instance::verify_private_home(&home)?;
    let existing = cutex::config::store::load_codez_config_checked()?;
    let had_section = existing.instance.is_some();
    let config = instance::init_config(existing, &options)?;
    cutex::config::store::save_codez_config(&config)?;
    println!(
        "{} instance {:?} at {}",
        if had_section { "checked" } else { "initialized" },
        options.name,
        home.display()
    );
    show(false)
}

fn show(json: bool) -> anyhow::Result<()> {
    let custom = instance::custom_home()?;
    let home = cutex::config::paths::config_dir()?;
    let config_path = cutex::config::paths::config_path()?;
    let config = cutex::config::store::load_codez_config_checked()?;
    let management_port = cutex::management::service::management_port(&config);
    let bus_port = cutex::agent_bus::service::agent_bus_port(&config);
    let initialized = custom.is_none() || config.instance.is_some();
    let view = serde_json::json!({
        "name": config.instance.as_ref().map(|i| i.name.clone()),
        "isolated": custom.is_some(),
        "initialized": initialized,
        "home": home,
        "configPath": config_path,
        "managementPort": management_port,
        "managementUrl": cutex::management::service::management_base_url(management_port),
        "agentBusPort": bus_port,
        "remoteListeners": config.instance.as_ref().is_some_and(|i| i.remote_listeners),
    });
    if json {
        println!("{}", serde_json::to_string_pretty(&view)?);
        return Ok(());
    }
    match (&custom, &config.instance) {
        (None, _) => println!("default instance"),
        (Some(_), Some(section)) => println!("instance {} (isolated)", section.name),
        (Some(_), None) => println!("uninitialized instance: run `cutex instance init`"),
    }
    println!("home:            {}", home.display());
    println!("config:          {}", config_path.display());
    println!("management:      {}", view["managementUrl"].as_str().unwrap_or_default());
    println!("agent bus port:  {bus_port}");
    Ok(())
}

/// Native home spelling `StockBundle::load_references` accepts (canonical,
/// without the Windows verbatim prefix).
fn canonical_spelling(path: &Path) -> anyhow::Result<PathBuf> {
    let resolved = path
        .canonicalize()
        .with_context(|| format!("resolve {}", path.display()))?;
    let text = resolved.to_string_lossy().to_string();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return Ok(PathBuf::from(format!(r"\\{rest}")));
    }
    Ok(PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text)))
}

fn use_runtime(source: &Path, registry: Option<&Path>) -> anyhow::Result<()> {
    let home = require_custom_home()?;
    let source = canonical_spelling(source)?;
    let bytes = std::fs::read(&source).with_context(|| format!("read {}", source.display()))?;
    let bundle: StockBundle = serde_json::from_slice(&bytes)
        .with_context(|| format!("{} is not a bundle manifest", source.display()))?;
    if bundle.version != 4 {
        bail!("use-runtime needs a local-deployment bundle manifest (version 4), got version {}", bundle.version);
    }
    // Read-only checks of the shared binaries (path + sha256).
    for (what, file) in [
        ("executable", Some(&bundle.executable)),
        ("cli", bundle.cli.as_ref()),
        ("code_mode_host", Some(&bundle.code_mode_host)),
        ("facade", Some(&bundle.facade)),
        ("schema", Some(&bundle.schema)),
    ] {
        if let Some(file) = file {
            file.validate().with_context(|| format!("bundle {what} {}", file.path.display()))?;
        }
    }
    let native_home = home.join("codex-home");
    std::fs::create_dir_all(&native_home)
        .with_context(|| format!("create {}", native_home.display()))?;
    let shared_config = native_home.join("config.toml");
    if !shared_config.exists() {
        cutex::config::atomic::write_private_bytes_atomic(&shared_config, b"")?;
    }
    let native_home = canonical_spelling(&native_home)?;
    let shared_config = canonical_spelling(&shared_config)?;
    let mut derived = bundle.clone();
    derived.shared_config = VerifiedFile {
        sha256: cutex::agent_management::file_sha256(&shared_config)?,
        path: shared_config,
    };
    let digest = cutex::agent_management::file_sha256(&source)?;
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("bundle")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect::<String>();
    let bundles = home.join("runtime").join("bundles");
    std::fs::create_dir_all(&bundles)?;
    let manifest = bundles.join(format!("{stem}-{}.json", &digest.as_str()[..12]));
    cutex::config::atomic::write_private_pretty_json_atomic(&manifest, &derived, "instance bundle manifest")?;
    let deployment = LocalDeployment {
        native_home,
        bundle_manifest: canonical_spelling(&manifest)?,
        job_mcp: None,
    };
    deployment.install()?;
    if let Some(registry) = registry {
        let raw = std::fs::read(registry).with_context(|| format!("read {}", registry.display()))?;
        serde_json::from_slice::<serde_json::Value>(&raw)
            .with_context(|| format!("{} is not JSON", registry.display()))?;
        cutex::config::atomic::write_private_bytes_atomic(
            &home.join("runtime").join("native-version-registry-v1.json"),
            &raw,
        )?;
    }
    println!("runtime selected: {}", deployment.bundle_manifest.display());
    println!("native home:      {}", deployment.native_home.display());
    println!("source (unchanged): {}", source.display());
    Ok(())
}
