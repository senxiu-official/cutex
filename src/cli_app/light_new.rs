//! Human new-agent bootstrap: persist an empty native thread, then adopt it.
use anyhow::{ensure, Context};
use cutex::app_server::client::{AppServerClient, AppServerClientOptions, AppServerEndpoint};
use cutex::launch::local_deployment::LocalDeployment;
use cutex::launch::stock::StockBundle;

/// Phase timings for thread creation, printed to stderr when
/// `CUTEX_DEBUG_CREATE_TIMING=1` (diagnostics for slow New / quick chat).
pub(super) struct CreateTiming {
    enabled: bool,
    label: &'static str,
    start: std::time::Instant,
    last: std::time::Instant,
}

impl CreateTiming {
    pub(super) fn new(label: &'static str) -> Self {
        let now = std::time::Instant::now();
        Self {
            enabled: std::env::var_os("CUTEX_DEBUG_CREATE_TIMING").is_some_and(|v| v == "1"),
            label,
            start: now,
            last: now,
        }
    }
    pub(super) fn phase(&mut self, name: &str) {
        let now = std::time::Instant::now();
        if self.enabled {
            eprintln!(
                "[create-timing] {} {name}: {}ms (total {}ms)",
                self.label,
                now.duration_since(self.last).as_millis(),
                now.duration_since(self.start).as_millis()
            );
        }
        self.last = now;
    }
}

pub(super) fn create(
    formal_name: &str,
    cwd: &str,
) -> anyhow::Result<cutex::agent_management::HumanAdoptResult> {
    ensure_codex_default_profile()?;
    let configuration = cutex::launch::stock::local_configuration()?.with_permissions(
        &cutex::config::new_thread::resolve_for_create(cutex::config::new_thread::ThreadKind::Agent, None)?,
    )?;
    create_configured(formal_name, cwd, configuration, cutex::config::store::load_codez_config_checked()?.new_session_notification)
}

/// `cutex human new` takes no profile and creates a Codex agent on the
/// default profile; a Claude default profile is refused with the Claude
/// creators named (before any side effect).
pub(super) fn ensure_codex_default_profile() -> anyhow::Result<()> {
    let config = cutex::config::store::load_codez_config_checked()?;
    if let Some(profile) = config.default_profile.as_deref() {
        let profile = cutex::config::new_session::resolve_profile_name(profile)?;
        ensure!(
            super::claude_profile::account(&profile)?.is_none(),
            "cutex human new creates Codex agents, but the default profile {profile} is a Claude subscription profile; use `cutex human claude new --profile {profile}` or New agent in the desktop"
        );
    }
    Ok(())
}

fn create_configured(formal_name: &str, cwd: &str, configuration: cutex::launch::stock::StockConfiguration, notification: cutex::notify::session::Level) -> anyhow::Result<cutex::agent_management::HumanAdoptResult> {
    create_configured_with(formal_name, cwd, configuration, notification, |request| {
        super::management_control_plane::ManagementControlClient::connect()?
            .adopt_saved_native(request)
    })
}

/// Configuration for a Human-created thread of `kind`: the named profile,
/// else the global default, with the kind's launch defaults and the request's
/// overrides applied. Claude subscription profiles use a different creator.
pub(super) fn human_configuration(
    profile: Option<&str>,
    kind: cutex::config::new_thread::ThreadKind,
    launch: Option<&cutex::config::new_thread::LaunchOverrides>,
) -> anyhow::Result<cutex::launch::stock::StockConfiguration> {
    let mut configuration = match profile {
        Some(profile) => {
            ensure!(
                super::claude_profile::account(profile)?.is_none(),
                "Claude subscription profiles are not supported by this creator"
            );
            // The requested model / effort is part of the selection, so a
            // profile without its own `model` can still be created from.
            cutex::launch::stock::local_configuration_with_selection(
                Some(&profile.to_string()),
                launch.and_then(|launch| launch.model.as_ref()),
                launch.and_then(|launch| launch.reasoning_effort.as_ref()).filter(|effort| !effort.is_empty()),
            )?
        }
        None => cutex::launch::stock::local_configuration()?,
    };
    let permissions = cutex::config::new_thread::resolve_for_create(kind, launch)?;
    configuration = configuration.with_permissions(&permissions)?;
    let mut reselected = false;
    if let Some(model) = launch.and_then(|launch| launch.model.as_ref()) {
        configuration.model = model.clone();
        reselected = true;
    }
    if let Some(effort) = launch.and_then(|launch| launch.reasoning_effort.as_ref()) {
        ensure!(valid_reasoning(effort), "unsupported reasoning effort: {effort}");
        configuration.reasoning = Some(effort.clone());
        reselected = true;
    }
    ensure!(!configuration.model.trim().is_empty(), "Model required");
    if reselected {
        // Overridden model / effort must still pass the profile's catalog.
        configuration.validate_runtime_selection()?;
    }
    Ok(configuration)
}

/// `thread/start` of the bootstrap native: the reviewer travels with the
/// approval policy so the persisted thread starts with it (the record keeps
/// it for every later start / resume).
fn thread_start_body(cwd: &std::path::Path, configuration: &cutex::launch::stock::StockConfiguration) -> serde_json::Value {
    let mut body = serde_json::json!({"cwd":cwd,"approvalPolicy":configuration.approval,"ephemeral":false,"historyMode":"paginated"});
    if let Some(reviewer) = &configuration.approvals_reviewer {
        body["approvalsReviewer"] = serde_json::Value::String(reviewer.clone());
    }
    body
}

/// Resolved permissions a configuration launches with (persisted on the record).
pub(super) fn launch_permissions(
    configuration: &cutex::launch::stock::StockConfiguration,
) -> cutex::config::new_thread::LaunchPermissions {
    cutex::config::new_thread::LaunchPermissions {
        sandbox: configuration.sandbox.clone(),
        approval: configuration.approval.clone(),
        approvals_reviewer: configuration.approvals_reviewer.clone(),
        network_access: configuration.network_access,
        writable_roots: configuration.writable_roots.clone(),
    }
}

/// Shared by `cutex human new` and Management `POST /v2/human/agents`:
/// persist an empty native thread in `cwd` (exactly, no per-agent
/// subdirectory), then adopt it through `adopt` (Management client from the
/// CLI, the in-process Human handler inside the daemon).
pub(super) fn create_configured_with(
    formal_name: &str,
    cwd: &str,
    configuration: cutex::launch::stock::StockConfiguration,
    notification: cutex::notify::session::Level,
    adopt: impl FnOnce(
        &cutex::agent_management::HumanAdoptRequest,
    ) -> anyhow::Result<cutex::agent_management::HumanAdoptResult>,
) -> anyhow::Result<cutex::agent_management::HumanAdoptResult> {
    ensure!(
        !formal_name.trim().is_empty()
            && formal_name.trim() == formal_name
            && formal_name.chars().count() <= 128
            && !formal_name.chars().any(char::is_control),
        "valid agent name required"
    );
    let cwd = std::path::Path::new(cwd).canonicalize()?;
    ensure!(cwd.is_dir(), "agent cwd must be a directory");
    // Store the ordinary spelling (no Windows verbatim `\\?\` prefix) so agent cwds
    // compare equal to Project workspace roots and user-entered paths.
    let cwd = std::path::PathBuf::from(cutex::agent_management::strip_verbatim_prefix(
        cwd.to_str().context("agent cwd must be UTF-8")?,
    ));
    // The roster import refuses a root cwd; refuse it before the native thread exists.
    ensure!(cwd.parent().is_some(), "agent cwd must not be a filesystem root");
    let native = create_native(&cwd, &configuration, |_| {})?;
    cutex::notify::session::session(&native, cutex::notify::session::Change::Set(notification))?;
    adopt(&cutex::agent_management::HumanAdoptRequest {
            session_only: false,
            creation_defaults: Some(cutex::agent_management::HumanCreationDefaults {
                profile: configuration.profile_name.clone(),
                model: configuration.model.clone(),
                reasoning: configuration.reasoning.clone(),
                permissions: Some(launch_permissions(&configuration)),
            }),
            action_id: cutex::agent_management::AgentActionId::new(format!("human-new-{native}"))?,
            native_id: native.clone(),
            cwd: cwd.to_string_lossy().into_owned(),
            formal_name: formal_name.into(),
        })
        .with_context(|| format!("Saved native thread {native} exists; retry Adopt for this ID"))
}

/// Persist an empty native thread without a model turn or management recursion.
/// Report its identity immediately so typed callers can journal late failures.
#[cfg(any(unix, windows))]
pub(super) fn create_native(
    cwd: &std::path::Path,
    configuration: &cutex::launch::stock::StockConfiguration,
    mut captured: impl FnMut(&str),
) -> anyhow::Result<String> {
    let mut timing = CreateTiming::new("create_native");
    let deployment = LocalDeployment::selected()?.context("No local runtime installed")?;
    let bundle: StockBundle = serde_json::from_slice(&std::fs::read(&deployment.bundle_manifest)?)?;
    StockBundle::load_references(
        bundle
            .shared_config
            .path
            .parent()
            .context("bundle home missing")?,
        &deployment.bundle_manifest,
        &cutex::agent_management::file_sha256(&deployment.bundle_manifest)?,
    )?;
    configuration.validate_auth_home(&deployment.native_home)?;
    timing.phase("bundle+auth checks");
    // Not `std::env::temp_dir()`: an inherited TEMP may be another home's stock-tmp.
    let directory = super::stock_lifecycle::launch_tmpdir(std::env::var("TMPDIR").ok().as_deref())?.join(format!(
        "cn-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    ));
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
    }
    #[cfg(windows)]
    let layout = cutex::app_server::runtime::AppServerRuntimeLayout::prepare_stock(
        &format!("bootstrap-{}", uuid::Uuid::new_v4()),
    )?;
    #[cfg(windows)]
    {
        std::fs::create_dir(&directory)?;
        cutex::platform::private_fs::secure_directory(&directory)?;
    }
    #[cfg(unix)]
    let socket = directory.join("native.sock");
    use super::stock_lifecycle::{clean_launch, configured, permission_profile};
    let launch = configured(
        clean_launch(&bundle.executable.path, &deployment.native_home)?,
        &configuration,
        true,
        None,
        &bundle,
        &deployment.native_home,
    )?;
    let launch = permission_profile(launch, configuration)?;
    #[cfg(unix)]
    let launch = launch.arg("--listen").arg(format!("unix://{}", socket.display()));
    #[cfg(windows)]
    let launch = layout.app_server_args().into_iter().skip(1)
        .fold(launch, |launch, arg| launch.arg(arg));
    let mut command = launch.to_command();
    if let Some(projection) = &configuration.selected_projection {
        if let Some(secret) = projection.secret()? {
            secret.apply(&mut command);
        }
    }
    command
        .current_dir(&cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(directory.join("native.stderr.log"))?);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let parent = std::process::id();
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() as u32 != parent {
                    return Err(std::io::Error::other("native creator exited"));
                }
                Ok(())
            });
        }
    }
    struct Owned(std::process::Child);
    impl Drop for Owned {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    #[cfg(unix)]
    let owned = Owned(command.spawn()?);
    #[cfg(windows)]
    let owned = {
        let lease = std::fs::File::create(directory.join("bootstrap.lock"))?;
        let secret = configuration.selected_projection.as_ref()
            .map(|projection| projection.secret()).transpose()?.flatten();
        let mut child = super::stock_publication::spawn_with_secret(&launch, cwd.to_str().context("cwd must be UTF-8")?,
            &directory.join("native.stderr.log"), &lease, secret.as_ref())?;
        child.start_ephemeral()?;
        child
    };
    timing.phase("spawn");
    #[cfg(unix)]
    let endpoint = AppServerEndpoint::UnixSocket { socket_path: socket };
    #[cfg(windows)]
    let endpoint = layout.endpoint();
    let client = AppServerClient::connect(AppServerClientOptions::new(endpoint))?;
    let handle = client.handle();
    timing.phase("connect");
    let response = handle.request("thread/start", thread_start_body(cwd, &configuration)).context("Native creation response unconfirmed; inspect Recent before creating another agent")?;
    let native = response["thread"]["id"]
        .as_str()
        .context("native thread ID missing")?
        .to_string();
    ensure!(
        uuid::Uuid::parse_str(&native)?.to_string() == native,
        "invalid native ID"
    );
    timing.phase("thread/start");
    captured(&native);
    cutex::notify::session::session(&native, cutex::notify::session::Change::Set(
        cutex::config::store::load_codez_config_checked()?.new_session_notification,
    )).with_context(|| format!("Native thread {native} exists; notification initialization failed"))?;
    // Once the native ID is known, always report it on later failure so that a
    // saved thread can be adopted without generating a second identity.
    let operation = || -> anyhow::Result<_> {
        let ack = handle.request(
            "thread/read",
            serde_json::json!({"threadId":native,"includeTurns":true}),
        )?;
        ensure!(
            ack["thread"]["id"].as_str() == Some(&native)
                && ack["thread"]["historyMode"].as_str() == Some("paginated")
                && ack["thread"]["turns"].as_array().is_some_and(Vec::is_empty),
            "empty native thread was not persisted"
        );
        // In-process adoption relies on this read instead of re-reading the
        // thread from a second native (see management_context::adopt_created_native).
        ensure!(
            ack["thread"]["cwd"].as_str()
                .and_then(|thread_cwd| std::path::Path::new(thread_cwd).canonicalize().ok())
                == Some(cwd.canonicalize()?),
            "native thread cwd differs from the requested cwd"
        );
        Ok(())
    };
    operation().with_context(|| {
        format!("Native thread {native} created; recover/adopt this ID before trying new again")
    })?;
    timing.phase("notify+thread/read");
    drop(client);
    drop(owned);
    timing.phase("shutdown");
    Ok(native)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn create_native(
    _cwd: &std::path::Path,
    _configuration: &cutex::launch::stock::StockConfiguration,
    _captured: impl FnMut(&str),
) -> anyhow::Result<String> {
    anyhow::bail!("Managed native creation requires the Linux runtime adapter; no session was created")
}

/// Foreground form shared by the main selector's New action.
pub(super) fn wizard() -> anyhow::Result<Option<cutex::session::model::CutexSessionRecord>> {
    let result = wizard_form();
    match &result {
        Ok(Some(created)) => println!("Created Cutex Agent: {}", created.formal_agent_name.as_deref().unwrap_or("")),
        Ok(None) => println!("Creation cancelled."),
        Err(error) => println!("Creation did not complete: {error:#}"),
    }
    super::prompt::prompt_line("Press Enter to return to Cutex", "")?;
    result
}

fn selected_profile(input: &str, profiles: &[String]) -> anyhow::Result<String> {
    if let Some(name) = profiles.iter().find(|name| name.as_str() == input) {
        return Ok(name.clone());
    }
    if let Some(name) = input.parse::<usize>().ok().and_then(|n| n.checked_sub(1)).and_then(|i| profiles.get(i)) {
        return Ok(name.clone());
    }
    anyhow::bail!("Unknown profile; choose a listed name or number")
}

fn valid_reasoning(value: &str) -> bool {
    value.is_empty() || ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra", "persistent"].contains(&value)
}

#[derive(Debug, Eq, PartialEq)]
struct ReasoningChoices {
    supported: Vec<String>,
    default_effort: Option<String>,
}

fn model_reasoning_choices(
    catalog: &serde_json::Value,
    model: &str,
) -> Option<ReasoningChoices> {
    let row = catalog["models"].as_array()?
        .iter()
        .find(|row| row["slug"].as_str() == Some(model))?;
    let supported = row["supported_reasoning_levels"].as_array()?
        .iter()
        .filter_map(|level| level["effort"].as_str())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let default_effort = row["default_reasoning_level"].as_str()
        .filter(|value| supported.iter().any(|supported| supported == value))
        .map(str::to_owned)
        .or_else(|| supported.first().cloned());
    Some(ReasoningChoices { supported, default_effort })
}

fn effective_reasoning_default(
    choices: Option<&ReasoningChoices>,
    configured: Option<&str>,
) -> Option<String> {
    let Some(choices) = choices else {
        return configured.map(str::to_owned);
    };
    if choices.supported.is_empty() {
        return None;
    }
    choices.default_effort.clone()
        .or_else(|| configured.and_then(|value| choices.supported.iter().find(|supported| supported.as_str() == value)).cloned())
        .or_else(|| choices.supported.first().cloned())
}

fn reasoning_supported(value: &str, choices: Option<&ReasoningChoices>) -> bool {
    match choices {
        None => true,
        Some(choices) if choices.supported.is_empty() => value.is_empty(),
        Some(choices) => {
            choices
                .supported
                .iter()
                .any(|supported| supported.as_str() == value)
        }
    }
}

fn reasoning_label(choices: Option<&ReasoningChoices>) -> String {
    match choices {
        None => "none/minimal/low/medium/high/xhigh/max/ultra/persistent".to_owned(),
        Some(choices) if choices.supported.is_empty() => "empty to omit".to_owned(),
        Some(choices) => choices.supported.join("/"),
    }
}

fn selected_model_catalog(
    configuration: &cutex::launch::stock::StockConfiguration,
) -> anyhow::Result<Option<serde_json::Value>> {
    let Some(catalog) = configuration
        .selected_projection
        .as_ref()
        .and_then(|projection| projection.catalog.as_ref())
    else {
        return Ok(None);
    };
    let bytes = std::fs::read(&catalog.path)?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn wizard_form() -> anyhow::Result<Option<cutex::session::model::CutexSessionRecord>> {
    let name = super::prompt::prompt_line("New agent name (empty cancels)", "")?;
    if name.is_empty() {
        return Ok(None);
    }
    let cwd = super::prompt::prompt_line(
        "Working directory",
        &std::env::current_dir()?.to_string_lossy(),
    )?;
    let defaults = cutex::config::store::load_codez_config_checked()?;
    let profiles = super::account_store::load_profile_names_read_only()?;
    ensure!(!profiles.is_empty(), "No profiles configured; add a profile in Settings first");
    println!("Available profiles:");
    for (index, name) in profiles.iter().enumerate() { println!("  {}. {}", index + 1, name); }
    let profile = loop {
        let input = super::prompt::prompt_line("Profile (name or number)", defaults.default_profile.as_deref().unwrap_or(""))?;
        match selected_profile(&input, &profiles) {
            Ok(profile) => break profile,
            Err(error) => println!("{error}"),
        }
    };
    if let Some(account)=super::claude_profile::account(&profile)? {
        let mut settings=account.options.claude.clone();
        if let Some(model)=defaults.new_session_defaults.get(&profile).and_then(|d|d.model.clone()){settings.model=model;}
        settings.model=super::prompt::prompt_line("Model",&settings.model)?;
        settings.effort=cutex::profiles::appearance::ClaudeEffort::parse(&super::prompt::prompt_line("Effort (default/low/medium/high/xhigh/max)",settings.effort.map(|e|e.as_str()).unwrap_or("default"))?)?;
        // A rule kept at the profile's value keeps following the profile.
        let write:bool=super::prompt::prompt_line("Allow file writes (true/false)",&settings.write().to_string())?.parse()?;
        let exec:bool=super::prompt::prompt_line("Allow commands (true/false)",&settings.exec().to_string())?.parse()?;
        let tools=((write!=settings.write()).then_some(write),(exec!=settings.exec()).then_some(exec));
        let level=super::prompt::prompt_line("Notification (important/normal/off)",match defaults.new_session_notification{cutex::notify::session::Level::Important=>"important",cutex::notify::session::Level::Normal=>"normal",cutex::notify::session::Level::Off=>"off"})?;
        let level=match level.as_str(){"important"=>cutex::notify::session::Level::Important,"normal"=>cutex::notify::session::Level::Normal,"off"=>cutex::notify::session::Level::Off,_=>anyhow::bail!("Unknown notification level")};
        let record=super::claude_profile::create(&account,&name,std::path::Path::new(&cwd),true,settings,tools)?;
        cutex::notify::session::session(record.app_server_thread_id().context("Claude thread missing")?,cutex::notify::session::Change::Set(level)).with_context(||format!("Agent {} exists; notification initialization failed",record.cutex_session_id))?;
        return Ok(Some(record));
    }
    let mut configuration = cutex::launch::stock::local_configuration_with_profile(Some(&profile))?
        .with_permissions(&cutex::config::new_thread::resolve_for_create(
            cutex::config::new_thread::ThreadKind::Agent,
            None,
        )?)?;
    configuration.model = super::prompt::prompt_line("Model", &configuration.model)?;
    let catalog = selected_model_catalog(&configuration)?;
    let choices = catalog.as_ref()
        .and_then(|catalog| model_reasoning_choices(catalog, &configuration.model));
    let reasoning_default = effective_reasoning_default(
        choices.as_ref(),
        configuration.reasoning.as_deref(),
    ).unwrap_or_default();
    let reasoning_prompt = format!("Reasoning effort ({})", reasoning_label(choices.as_ref()));
    let reasoning = loop {
        let value = super::prompt::prompt_line(&reasoning_prompt, &reasoning_default)?;
        if valid_reasoning(&value) && reasoning_supported(&value, choices.as_ref()) { break value; }
        println!("Reasoning effort not supported by this model; choose a listed value.");
    };
    configuration.reasoning = (!reasoning.is_empty()).then_some(reasoning);
    let notification = super::prompt::prompt_line("Notification (important/normal/off)", match defaults.new_session_notification {
        cutex::notify::session::Level::Important => "important",
        cutex::notify::session::Level::Normal => "normal",
        cutex::notify::session::Level::Off => "off",
    })?;
    let notification = match notification.as_str() {
        "important" => cutex::notify::session::Level::Important,
        "normal" => cutex::notify::session::Level::Normal,
        "off" => cutex::notify::session::Level::Off,
        _ => anyhow::bail!("Unknown notification level"),
    };
    ensure!(!configuration.model.trim().is_empty(), "Model required");
    configuration.validate_runtime_selection()?;
    let result = create_configured(&name, &cwd, configuration, notification)?;
    if let Some(error) = &result.error {
        anyhow::bail!("Agent created, but import incomplete: {error}");
    }
    Ok(Some(result.adopted.record))
}

pub(super) fn adopt_saved(
    native: &str,
    name: &str,
    cwd: &str,
) -> anyhow::Result<cutex::agent_management::HumanAdoptResult> {
    use cutex::catalog::CatalogEndpoint;
    let launch = super::session_native_workflow::NativeLaunch {
        cwd: cwd.into(),
        native_home: LocalDeployment::source_home()?,
        profile: None,
        model: None,
    };
    let source = launch.endpoint()?.request(
        "thread/read",
        serde_json::json!({"threadId":native,"includeTurns":false}),
    )?;
    let cwd = source
        .pointer("/thread/cwd")
        .and_then(serde_json::Value::as_str)
        .context("saved thread cwd missing")?;
    let result = super::management_control_plane::ManagementControlClient::connect()?
        .adopt_saved_native(&cutex::agent_management::HumanAdoptRequest {
            session_only: false,
            creation_defaults: None,
            action_id: cutex::agent_management::AgentActionId::new(format!(
                "human-adopt-{native}"
            ))?,
            native_id: native.into(),
            cwd: cwd.into(),
            formal_name: name.into(),
        })?;
    if let Some(error) = &result.error {
        anyhow::bail!("Agent adopted, but roster import incomplete: {error}. Retry the same Adopt");
    }
    Ok(result)
}

/// New ordinary Cutex session (quick chat): native history + dedicated
/// owner, no Agent import. Uses the quick-chat launch defaults.
pub(super) fn create_owned_session(profile: &str, cwd: &std::path::Path) -> anyhow::Result<String> {
    let configuration = human_configuration(Some(profile), cutex::config::new_thread::ThreadKind::QuickChat, None)?;
    create_owned_session_with(profile, cwd, configuration, |id| eprintln!("Session: {id}"), |request| {
        super::management_control_plane::ManagementControlClient::connect()?.adopt_saved_native(request)
    })
}

/// Shared by `cutex new` / TUI New session and Management
/// `POST /v2/human/sessions` (which adopts in-process).
pub(super) fn create_owned_session_with(
    profile: &str,
    cwd: &std::path::Path,
    mut configuration: cutex::launch::stock::StockConfiguration,
    captured: impl FnMut(&str),
    adopt: impl FnOnce(
        &cutex::agent_management::HumanAdoptRequest,
    ) -> anyhow::Result<cutex::agent_management::HumanAdoptResult>,
) -> anyhow::Result<String> {
    if let Some(projection) = configuration.selected_projection.as_mut() { projection.requires_job = false; }
    let cwd = canonical_cwd(cwd)?;
    let mut timing = CreateTiming::new("owned_session");
    let native = create_native(&cwd, &configuration, captured)?;
    timing.phase("create_native");
    let permissions = launch_permissions(&configuration);
    let result = adopt(&cutex::agent_management::HumanAdoptRequest {
        session_only: true,
        action_id: cutex::agent_management::AgentActionId::new(format!("session-owner-{native}"))?,
        native_id: native.clone(),
        cwd: cwd.to_string_lossy().into_owned(),
        formal_name: format!("Session {}", &native[..8]),
        creation_defaults: Some(cutex::agent_management::HumanCreationDefaults {
            profile: profile.into(),
            model: configuration.model,
            reasoning: configuration.reasoning,
            permissions: Some(permissions),
        }),
    })
    .with_context(|| format!("Session {native} exists; runtime registration incomplete, retain this ID"))?;
    timing.phase("adopt");
    Ok(result.adopted.record.cutex_session_id)
}

/// Canonical directory without the Windows verbatim prefix, so session cwds
/// compare equal to Project roots and user-entered paths.
pub(super) fn canonical_cwd(cwd: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    let cwd = cwd.canonicalize()?;
    ensure!(cwd.is_dir(), "session cwd must be a directory");
    Ok(std::path::PathBuf::from(cutex::agent_management::strip_verbatim_prefix(
        cwd.to_str().context("session cwd must be UTF-8")?,
    )))
}

#[cfg(test)]
mod wizard_tests {
    use super::*;
    #[test]
    fn owned_session_cwd_has_no_verbatim_prefix() {
        let dir = std::env::temp_dir();
        let cwd = canonical_cwd(&dir).unwrap();
        assert!(!cwd.to_string_lossy().starts_with(r"\\?\"), "{}", cwd.display());
        assert_eq!(cwd.canonicalize().unwrap(), dir.canonicalize().unwrap());
        assert!(canonical_cwd(&dir.join("cutex-no-such-dir-quick-chat")).is_err());
    }
    #[test]
    fn profiles_accept_names_or_numbers_without_silent_fallback() {
        let profiles = vec!["orchid".into(), "GLM".into()];
        assert_eq!(selected_profile("GLM", &profiles).unwrap(), "GLM");
        assert_eq!(selected_profile("2", &profiles).unwrap(), "GLM");
        for invalid in ["0", "3", "unknown", ""] { assert!(selected_profile(invalid, &profiles).is_err()); }
    }
    #[test]
    fn native_reasoning_defaults_are_accepted() {
        for value in ["", "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra", "persistent"] { assert!(valid_reasoning(value)); }
        assert!(!valid_reasoning("maximum"));
    }

    #[test]
    fn model_catalog_replaces_unsupported_reasoning_default() {
        let catalog = serde_json::json!({"models":[{"slug":"k3-256k",
            "default_reasoning_level":"max",
            "supported_reasoning_levels":[{"effort":"low"},{"effort":"high"},{"effort":"max"}]}]});
        let choices = model_reasoning_choices(&catalog, "k3-256k").unwrap();
        assert_eq!(choices.supported, ["low", "high", "max"]);
        assert_eq!(effective_reasoning_default(Some(&choices), Some("medium")).as_deref(), Some("max"));
        assert!(reasoning_supported("max", Some(&choices)));
        assert!(!reasoning_supported("medium", Some(&choices)));
        assert_eq!(reasoning_label(Some(&choices)), "low/high/max");
    }

    #[test]
    fn model_catalog_without_levels_defaults_to_omitted_reasoning() {
        let catalog = serde_json::json!({"models":[{"slug":"generic",
            "default_reasoning_level":null,"supported_reasoning_levels":[]}]});
        let choices = model_reasoning_choices(&catalog, "generic").unwrap();
        assert!(choices.supported.is_empty());
        assert_eq!(effective_reasoning_default(Some(&choices), Some("medium")), None);
        assert!(reasoning_supported("", Some(&choices)));
        assert!(!reasoning_supported("medium", Some(&choices)));
        assert_eq!(reasoning_label(Some(&choices)), "empty to omit");
    }

    #[test]
    fn models_outside_catalog_preserve_the_profile_default_until_full_review() {
        let catalog = serde_json::json!({"models":[{"slug":"other","supported_reasoning_levels":[{"effort":"low"}]}]});
        assert!(model_reasoning_choices(&catalog, "missing").is_none());
        assert_eq!(effective_reasoning_default(None, Some("medium")).as_deref(), Some("medium"));
        assert!(reasoning_supported("medium", None));
        assert_eq!(reasoning_label(None), "none/minimal/low/medium/high/xhigh/max/ultra/persistent");
    }
}

#[cfg(test)]
mod tests {
    use crate::cli_app::test_home::IsolatedTestHome;
    use cutex::config::new_thread::{LaunchOverrides, ThreadKind};
    use cutex::profiles::model::{AccountsStore, CliKind, RuntimeConfig, StoredAccount};

    /// A pre-`cutex_provider_mode` ChatGPT profile:
    /// `auth.json` + `config.toml` with only model / effort.
    fn legacy_chatgpt_profile(home: &IsolatedTestHome) -> std::path::PathBuf {
        // The Linux auth custody review refuses group/other-writable
        // ancestors. A real home is initialized privately by the daemon; this
        // fixture writes the stores directly, so create the tree owner-only
        // itself instead of inheriting the caller's umask.
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(cutex::config::paths::config_dir().unwrap().join("profiles"))
                .unwrap();
        }
        let account = StoredAccount {
            options: Default::default(),
            browser_tools: false,
            search_tool: false,
            quota_reader: None,
            private_chatgpt_auth_file: false,
            id: "00000000-0000-4000-8000-0000000000a1".into(),
            name: "orchid".into(),
            email: None,
            plan_type: None,
            plan_label: None,
            quota_preset: Default::default(),
            source: Some("test".into()),
            runtime: RuntimeConfig::Host,
            proxy: None,
            session: None,
            cli_kind: CliKind::Codex,
            default_cli_args: Vec::new(),
            agent_name: None,
            last_used_at: None,
            provider_category: None,
            claude_account: None,
            model_display_names: Default::default(),
        };
        cutex::profiles::store::save_store(&AccountsStore {
            version: cutex::profiles::model::STORE_VERSION,
            accounts: vec![account.clone()],
            active_account_id: Some(account.id.clone()),
        })
        .unwrap();
        let files = cutex::profiles::materialize::materialized_account_files(&account).unwrap();
        std::fs::create_dir_all(files.auth_path.parent().unwrap()).unwrap();
        use base64::Engine;
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"https://api.openai.com/auth":{"chatgpt_user_id":"user-test"}}"#);
        let auth = serde_json::json!({"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{
            "id_token":format!("header.{claims}.signature"),"access_token":"fake-access",
            "refresh_token":"fake-refresh","account_id":"account-test"},"last_refresh":null});
        std::fs::write(&files.auth_path, serde_json::to_vec(&auth).unwrap()).unwrap();
        let profile = "model = \"gpt-5.6-sol\"\nmodel_reasoning_effort = \"medium\"\n";
        std::fs::write(&files.config_path, profile).unwrap();
        let profile_dir = files.auth_path.parent().unwrap();
        #[cfg(windows)]
        cutex::platform::private_fs::secure_tree(profile_dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(profile_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::set_permissions(&files.auth_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let native_home = home.root().join("native-home");
        std::fs::create_dir_all(&native_home).unwrap();
        std::fs::write(native_home.join("config.toml"), "").unwrap();
        let deployment = cutex::config::paths::runtime_dir().unwrap().join("light/deployment.json");
        std::fs::create_dir_all(deployment.parent().unwrap()).unwrap();
        std::fs::write(
            &deployment,
            serde_json::to_vec(&serde_json::json!({
                "native_home": native_home,
                "bundle_manifest": home.root().join("bundle.json"),
            }))
            .unwrap(),
        )
        .unwrap();
        let mut config = cutex::config::store::load_codez_config();
        config.default_profile = Some("orchid".into());
        cutex::config::store::save_codez_config(&config).unwrap();
        files.config_path
    }

    #[test]
    fn legacy_chatgpt_profile_accepts_reselected_model_for_quick_chat_and_agent() {
        let home = IsolatedTestHome::new_private("legacy-chatgpt-profile").unwrap();
        let config_path = legacy_chatgpt_profile(&home);
        let before = std::fs::read(&config_path).unwrap();
        let launch = LaunchOverrides {
            model: Some("gpt-5.6-luna".into()),
            reasoning_effort: Some("high".into()),
            ..Default::default()
        };
        for kind in [ThreadKind::QuickChat, ThreadKind::Agent] {
            for launch in [None, Some(&launch)] {
                let configuration = super::human_configuration(Some("orchid"), kind, launch)
                    .unwrap_or_else(|error| panic!("{kind:?} {launch:?}: {error:#}"));
                assert!(configuration.selected_projection.is_some());
                let (model, effort) = match launch {
                    Some(_) => ("gpt-5.6-luna", "high"),
                    None => ("gpt-5.6-sol", "medium"),
                };
                assert_eq!(configuration.model, model);
                assert_eq!(configuration.reasoning.as_deref(), Some(effort));
            }
        }
        // Default-profile path (no explicit profile) also reselects.
        super::human_configuration(None, ThreadKind::QuickChat, Some(&launch)).unwrap();
        // The projection is in memory only: the profile file is untouched.
        assert_eq!(std::fs::read(&config_path).unwrap(), before);
    }
}
