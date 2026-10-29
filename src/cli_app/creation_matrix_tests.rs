//! Every session / agent creation entry point × runtime kind (Codex profile,
//! Claude subscription profile). Each cell is either supported (it reaches
//! that kind's creator) or an explicit, documented refusal. A new entry point
//! or a path that silently lacks Claude support must show up here.
//!
//! The test home has a fake Claude deployment (creation only writes records;
//! nothing is spawned) and no native Codex runtime, so a Codex cell that
//! reaches the Codex creator stops at the native runtime / native thread step.
//! Not covered here (no testable entry without a terminal or daemon):
//! the TUI New-agent wizard (`light_new::wizard_form`, explicit Claude branch),
//! `cutex human claude new` (Claude only by design), direct profile launch
//! (`launch_process`: Codex profiles → owned session, Claude profiles →
//! `claude_profile::run`), and owner Director rotation (uses
//! `create_human_agent`, the first row).

use crate::cli_app::test_home::IsolatedTestHome;
use cutex::http::server::SimpleHttpRequest;
use serde_json::{json, Value};
use std::io::Read;

const CODEX: &str = "Codex main";
const CLAUDE: &str = "Claude main";
const CLAUDE_UNSET: &str = "Claude unset";

/// The documented message `create_human_agent` / `create_human_session`
/// refuse Codex-only launch options with for Claude profiles.
const CODEX_ONLY: &str = "sandbox, approvalPolicy, networkAccess and writableRoots are Codex-only";
/// The refusal Claude profiles got from `light_new::human_configuration`
/// before `human-agent-claude`; no owner path may answer it any more.
const OLD_CLAUDE_REFUSAL: &str = "Claude subscription profiles are not supported";

#[derive(Debug)]
enum Expect {
    /// Created a Claude record (ClaudeSubscription backend).
    Claude,
    /// Reached the Codex native creator (fails here only for want of a
    /// native runtime in the test home).
    CodexCreator,
    /// Reached that kind's creator, which failed only for want of a
    /// running runtime; the error contains the text.
    RoutedTo(&'static str),
    /// Explicit refusal whose message contains the text.
    Refused(&'static str),
}

struct Fixture {
    home: IsolatedTestHome,
}

impl Fixture {
    fn new() -> Self {
        let home = IsolatedTestHome::new("creation-matrix").unwrap();
        let root = home.root().to_path_buf();
        let adapter = root.join("adapter");
        std::fs::create_dir_all(adapter.join("src/cute_claude")).unwrap();
        std::fs::create_dir_all(adapter.join("scripts")).unwrap();
        for file in ["src/cute_claude/__main__.py", "scripts/connect_tui.py"] {
            std::fs::write(adapter.join(file), "").unwrap();
        }
        let binary = root.join("binary");
        std::fs::write(&binary, "").unwrap();
        let login = root.join("shared-login");
        std::fs::create_dir(&login).unwrap();
        cutex::runtime::claude::ClaudeDeployment {
            python: binary.clone(),
            adapter_source: adapter,
            claude_cli: binary.clone(),
            claude_home: login,
            tui_binary: binary,
        }
        .install()
        .unwrap();
        super::claude_profile::add(CLAUDE).unwrap();
        // The profile's own Claude rules: writes allowed, commands not.
        let mut store = super::account_store::load_store().unwrap();
        let claude = store.accounts.iter_mut().find(|a| a.name == CLAUDE).unwrap();
        claude.options.claude.allow_write = Some(true);
        claude.options.claude.allow_exec = Some(false);
        cutex::profiles::store::save_store(&store).unwrap();
        // Rules never set: the default (on).
        super::claude_profile::add(CLAUDE_UNSET).unwrap();
        cutex::profiles::lifecycle::create_profile(&cutex::profiles::lifecycle::NewProfile {
            name: CODEX.into(),
            kind: cutex::profiles::lifecycle::NewProfileKind::ApiKey,
            preset: None,
            base_url: Some("http://127.0.0.1:9/v1".into()),
            model: Some("test-model".into()),
            api_key: None,
        })
        .unwrap();
        std::fs::create_dir_all(root.join("work")).unwrap();
        Self { home }
    }

    fn work(&self) -> String {
        self.home.root().join("work").to_string_lossy().into_owned()
    }

    fn set_default_profile(&self, profile: &str) {
        let mut config = cutex::config::store::load_codez_config_checked().unwrap();
        config.default_profile = Some(profile.into());
        cutex::config::store::save_codez_config(&config).unwrap();
    }
}

/// One root-authenticated Management request; returns (status, body).
fn root_request(method: &str, path: &str, body: Value) -> (u16, Value) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let client = std::thread::spawn(move || {
        let mut socket = std::net::TcpStream::connect(address).unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).unwrap();
        response
    });
    let (mut socket, _) = listener.accept().unwrap();
    let request = SimpleHttpRequest {
        method: method.into(),
        path: path.into(),
        headers: [("authorization".to_string(), "Bearer root-test".to_string())]
            .into_iter()
            .collect(),
        body: serde_json::to_vec(&body).unwrap(),
    };
    cutex::management::v2::server::handle_v2_request(
        &mut socket,
        &request,
        Some("bridge-test"),
        Some("root-test"),
        super::management_context::management_request_context(),
    )
    .unwrap();
    drop(socket);
    let response = client.join().unwrap();
    let status = response[9..12].parse().unwrap();
    let body = serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap_or("null"))
        .unwrap_or(Value::Null);
    (status, body)
}

fn session_record(id: &str) -> cutex::session::model::CutexSessionRecord {
    cutex::session::store::load_cutex_session_store()
        .unwrap()
        .sessions
        .get(id)
        .cloned()
        .unwrap_or_else(|| panic!("record {id} missing"))
}

fn in_roster(id: &str) -> bool {
    let snapshot = super::management_context::management_agent_provider()
        .unwrap()
        .store()
        .snapshot()
        .unwrap();
    snapshot
        .agents
        .keys()
        .any(|agent| agent.as_str() == id)
}

/// A `(status, body)` create response as an outcome: the new session ID or
/// the error message.
fn created_id(status: u16, body: &Value) -> Result<String, String> {
    if status == 200 || status == 201 {
        return Ok(body["cutexSessionId"].as_str().unwrap_or_default().to_string());
    }
    Err(format!(
        "{status} {} {}",
        body["error"]["code"].as_str().unwrap_or_default(),
        body["error"]["message"].as_str().unwrap_or_default()
    ))
}

fn check(cell: &str, outcome: Result<String, String>, expect: &Expect) {
    match (expect, &outcome) {
        (Expect::Claude, Ok(id)) => {
            let record = session_record(id);
            assert!(record.claude_session.is_some(), "{cell}: not a Claude record");
            assert_eq!(
                record.runtime_backend,
                cutex::session::model::CutexSessionRuntimeBackend::ClaudeSubscription,
                "{cell}"
            );
            assert!(matches!(record.profile.as_deref(), Some(CLAUDE | CLAUDE_UNSET)), "{cell}");
        }
        (Expect::CodexCreator, Err(message)) => {
            assert!(!message.contains(OLD_CLAUDE_REFUSAL), "{cell}: {message}");
            assert!(
                ["local runtime", "native", "Native", "bundle", "runtime installed"]
                    .iter()
                    .any(|needle| message.contains(needle)),
                "{cell}: did not reach the Codex native creator: {message}"
            );
        }
        (Expect::CodexCreator, Ok(id)) => {
            assert!(session_record(id).claude_session.is_none(), "{cell}");
        }
        (Expect::Refused(text) | Expect::RoutedTo(text), Err(message)) => {
            assert!(message.contains(text), "{cell}: expected refusal {text:?}, got {message}");
        }
        _ => panic!("{cell}: expected {expect:?}, got {outcome:?}"),
    }
}

/// Launch flags a start of `id` would get, and that the rules follow the
/// profile (nothing set for the session).
fn assert_tools(id: &str, expected: (bool, bool), cell: &str) {
    let record = session_record(id);
    let session = record.claude_session.as_ref().unwrap();
    assert!(!session.write_explicit && !session.exec_explicit, "{cell}: follows the profile");
    assert_eq!(cutex::runtime::claude::effective_tools(&record, session).unwrap(), expected, "{cell}");
}

fn set_profile_rules(profile: &str, write: Option<bool>, exec: Option<bool>) {
    let mut store = super::account_store::load_store().unwrap();
    let account = store.accounts.iter_mut().find(|a| a.name == profile).unwrap();
    account.options.claude.allow_write = write;
    account.options.claude.allow_exec = exec;
    cutex::profiles::store::save_store(&store).unwrap();
}

/// Sessions follow their profile's current write / exec rules at every
/// start; a rule set for the session (Management session settings) stays.
#[test]
fn claude_sessions_follow_the_profile_rules_at_the_next_start() {
    let fixture = Fixture::new();
    let (status, body) = root_request(
        "POST",
        "/v2/human/sessions",
        json!({"requestId": "follow", "profile": CLAUDE_UNSET, "cwd": fixture.work()}),
    );
    let id = created_id(status, &body).unwrap();
    let args = || {
        let record = session_record(&id);
        cutex::runtime::claude::tool_args(&record, record.claude_session.as_ref().unwrap()).unwrap()
    };
    assert_eq!(args(), ["--allow-write", "--allow-exec"], "default on");
    set_profile_rules(CLAUDE_UNSET, Some(false), Some(false));
    assert!(args().is_empty(), "profile turned both off");
    set_profile_rules(CLAUDE_UNSET, None, Some(true));
    assert_eq!(args(), ["--allow-write", "--allow-exec"]);
    // A session override (cutex/session/defaults/update) wins.
    let mut store = cutex::session::store::load_cutex_session_store().unwrap();
    let claude = store.sessions.get_mut(&id).unwrap().claude_session.as_mut().unwrap();
    claude.allow_write = false;
    claude.write_explicit = true;
    cutex::session::store::save_cutex_session_store(&store).unwrap();
    assert_eq!(args(), ["--allow-exec"]);
    set_profile_rules(CLAUDE_UNSET, Some(true), Some(false));
    assert!(args().is_empty(), "explicit write off stays; exec follows the profile");
    // Records written before r225 carry neither flag: they follow the profile.
    let legacy: cutex::runtime::claude::ClaudeSession = serde_json::from_value(json!({
        "deployment": {"python": "p", "adapter_source": "a", "claude_cli": "c", "claude_home": "h", "tui_binary": "t"},
        "thread_id": "t", "state_dir": "s", "model": "sonnet", "allow_write": false, "allow_exec": false,
    }))
    .unwrap();
    assert!(!legacy.write_explicit && !legacy.exec_explicit);
}

#[test]
fn default_profile_browser_tools_survive_owner_creation_without_project() {
    let fixture=Fixture::new();
    fixture.set_default_profile(CLAUDE);
    let mut profiles=super::account_store::load_store().unwrap();
    profiles.accounts.iter_mut().find(|a|a.name==CLAUDE).unwrap().browser_tools=true;
    cutex::profiles::store::save_store(&profiles).unwrap();
    for (route,body) in [
        ("/v2/human/sessions",json!({"requestId":"default-browser-chat","cwd":fixture.work()})),
        ("/v2/human/agents",json!({"requestId":"default-browser-agent","name":"Default Browser Agent","cwd":fixture.work()})),
    ] {
        let (status,body)=root_request("POST",route,body);
        let id=created_id(status,&body).unwrap();
        let record=session_record(&id);
        assert_eq!(record.profile.as_deref(),Some(CLAUDE),"omitted request profile is materialized at creation");
        assert!(cutex::agent_bus::tool_policy::browser_tools_for_record(&record));
        assert!(cutex::agent_bus::tool_policy::cutex_mcp_enabled(&record));
        let listed=cutex::agent_bus::tool_policy::tools_list_for_session(&id);
        assert!(listed["tools"].as_array().unwrap().iter().any(|tool|tool["name"]=="browser_read"));
    }
    let (status,body)=root_request("POST","/v2/human/sessions",json!({"requestId":"explicit-browser-off","profile":CLAUDE_UNSET,"cwd":fixture.work()}));
    let id=created_id(status,&body).unwrap();let record=session_record(&id);
    assert!(!cutex::agent_bus::tool_policy::browser_tools_for_record(&record));
    assert!(!cutex::agent_bus::tool_policy::cutex_mcp_enabled(&record));
    let listed=cutex::agent_bus::tool_policy::tools_list_for_session(&id);
    assert!(!listed["tools"].as_array().unwrap().iter().any(|tool|tool["name"].as_str().is_some_and(cutex::agent_bus::mcp_browser::is_tool)));
}

#[test]
fn every_creation_path_supports_or_explicitly_refuses_each_runtime_kind() {
    let fixture = Fixture::new();
    let work = fixture.work();
    fixture.set_default_profile(CODEX);
    let mut cells = Vec::new();
    let mut run = |cell: &str, expect: Expect, outcome: Result<String, String>| {
        check(cell, outcome.clone(), &expect);
        cells.push(format!("{cell}: {expect:?} <- {}", match &outcome { Ok(id) => format!("created {id}"), Err(e) => e.chars().take(160).collect() }));
        outcome.ok()
    };

    // 1. Owner New agent: POST /v2/human/agents (create_human_agent; also
    //    owner Director rotation).
    let agent = |name: &str, profile: Option<&str>, launch: Value| {
        let mut body = json!({"requestId": format!("agent-{name}"), "name": name, "cwd": work});
        if let Some(profile) = profile {
            body["profile"] = json!(profile);
        }
        if !launch.is_null() {
            body["launch"] = launch;
        }
        let (status, body) = root_request("POST", "/v2/human/agents", body);
        created_id(status, &body)
    };
    run("human agent × Codex", Expect::CodexCreator, agent("codex-agent", Some(CODEX), Value::Null));
    let id = run(
        "human agent × Claude",
        Expect::Claude,
        agent("claude-agent", Some(CLAUDE), json!({"model": "opus", "reasoningEffort": "high"})),
    )
    .unwrap();
    let record = session_record(&id);
    assert!(record.agent_enabled && !record.is_owned_session());
    assert_eq!(record.created_by, Some(cutex::session::model::SessionCreatedBy::Owner));
    assert_eq!(record.formal_agent_name.as_deref(), Some("claude-agent"));
    assert!(record.exposed_to_backend);
    let claude = record.claude_session.as_ref().unwrap();
    assert_eq!(claude.model, "opus");
    assert_eq!(claude.effort.map(|e| e.as_str()), Some("high"));
    assert!(claude.allow_write && !claude.allow_exec, "the profile's Claude rules");
    assert!(!claude.write_explicit && !claude.exec_explicit, "follows the profile");
    assert!(in_roster(&id), "imported into the Agent Management roster");
    run(
        "human agent × Claude + Codex-only launch fields",
        Expect::Refused(CODEX_ONLY),
        agent("claude-sandbox", Some(CLAUDE), json!({"sandbox": "read-only"})),
    );
    let unset = run("human agent × Claude (rules unset)", Expect::Claude, agent("claude-unset", Some(CLAUDE_UNSET), Value::Null)).unwrap();
    assert_tools(&unset, (true, true), "human agent × Claude (rules unset)");
    fixture.set_default_profile(CLAUDE);
    run("human agent × default Claude profile", Expect::Claude, agent("claude-default", None, Value::Null));
    fixture.set_default_profile(CODEX);

    // 2. Owner quick chat / 新工作: POST /v2/human/sessions.
    let session = |request: &str, profile: &str, launch: Value| {
        let mut body = json!({"requestId": request, "profile": profile, "cwd": work});
        if !launch.is_null() {
            body["launch"] = launch;
        }
        let (status, body) = root_request("POST", "/v2/human/sessions", body);
        created_id(status, &body)
    };
    run("human session × Codex", Expect::CodexCreator, session("chat-codex", CODEX, Value::Null));
    let chat = run("human session × Claude", Expect::Claude, session("chat-claude", CLAUDE, Value::Null)).unwrap();
    assert!(session_record(&chat).is_owned_session());
    assert_eq!(session_record(&chat).created_by, Some(cutex::session::model::SessionCreatedBy::Owner));
    assert_tools(&chat, (true, false), "human session × Claude");
    let chat_unset = run("human session × Claude (rules unset)", Expect::Claude, session("chat-unset", CLAUDE_UNSET, Value::Null)).unwrap();
    assert_tools(&chat_unset, (true, true), "human session × Claude (rules unset)");
    run(
        "human session × Claude + Codex-only launch fields",
        Expect::Refused(CODEX_ONLY),
        session("chat-claude-sandbox", CLAUDE, json!({"writableRoots": [work.clone()]})),
    );

    // 3. Agent-initiated agent_create (configuration check + bootstrap_native).
    let spec = |name: &str, profile: &str| cutex::agent_management::ManagedAgentSpec {
        name: name.into(),
        cwd: work.clone(),
        profile: Some(profile.into()),
        runtime_backend: "host".into(),
        model: if profile == CLAUDE { "sonnet" } else { "test-model" }.into(),
        reasoning: "default".into(),
        permissions: "danger-full-access".into(),
        approval_policy: "never".into(),
        sandbox_mode: "danger-full-access".into(),
        groups: vec!["cutex".into()],
        expose_to_im: false,
        pin: false,
    };
    let bootstrap = |spec: &cutex::agent_management::ManagedAgentSpec| {
        super::agent_management::bootstrap_for_test(spec).map_err(|failure| format!("{} {}", failure.code, failure.detail))
    };
    run("agent_create × Codex", Expect::CodexCreator, bootstrap(&spec("codex-created", CODEX)));
    let thread = bootstrap(&spec("claude-created", CLAUDE));
    let created = thread.map(|thread| {
        cutex::session::store::load_cutex_session_store()
            .unwrap()
            .sessions
            .values()
            .find(|r| r.app_server_thread_id() == Some(thread.as_str()))
            .map(|r| r.cutex_session_id.clone())
            .expect("Claude bootstrap record")
    });
    let created = run("agent_create × Claude", Expect::Claude, created).unwrap();
    // agent_create follows the profile's rules too (it used to force both on).
    assert_tools(&created, (true, false), "agent_create × Claude");
    assert_eq!(session_record(&created).created_by, None, "generic compatibility hook never guesses");
    let actor = cutex::session::model::SessionCreatedBy::Agent {
        cutex_session_id: cutex::role_revision::CutexSessionId::new("cutex.actual-director").unwrap(),
        role: Some(cutex::session::model::SessionCreatorRole::Director),
    };
    let stamped_thread = super::agent_management::bootstrap_with_creator_for_test(&spec("claude-stamped", CLAUDE), Some(&actor)).unwrap();
    let first_store = cutex::session::store::load_cutex_session_store().unwrap();
    let stamped = first_store.sessions.values().find(|r|r.app_server_thread_id()==Some(stamped_thread.as_str())).unwrap();
    assert_eq!(stamped.created_by.as_ref(), Some(&actor), "Claude bootstrap first save has actual actor");
    for (name, profile) in [("owner-cli-plain", None), ("owner-cli-profile", Some(CLAUDE.to_string()))] {
        super::human::run_command(cutex::cli::args::HumanCommand::Claude {
            command: cutex::cli::args::ClaudeCommand::New {
                name: name.into(), cwd: Some(std::path::PathBuf::from(&work)), model: "sonnet".into(),
                allow_write: false, allow_exec: false, profile,
            },
        }).unwrap();
        let saved = cutex::session::store::load_cutex_session_store().unwrap();
        let record = saved.sessions.values().find(|r|r.formal_agent_name.as_deref()==Some(name)).unwrap();
        assert_eq!(record.created_by, Some(cutex::session::model::SessionCreatedBy::Owner), "{name}: first CLI save");
    }
    let mut narrowed = spec("claude-narrow", CLAUDE);
    narrowed.sandbox_mode = "workspace-write".into();
    run(
        "agent_create × Claude + narrowed launch permissions",
        Expect::Refused("unsupported_claude_launch_permissions"),
        bootstrap(&narrowed),
    );

    // 4. Fork: the new session's runtime follows the source session. A Claude
    //    source routes to the Claude adapter's fork (no adapter runs in the
    //    test home), a Codex source to the native fork.
    let fork = |source: &str| {
        let (status, body) = root_request(
            "POST",
            &format!("/v2/sessions/{source}/cutex/requests"),
            json!({"requestId": format!("fork-{source}"), "method": "cutex/session/fork", "params": {}}),
        );
        created_id(status, &body)
    };
    // The Claude fork asks the (not running) adapter: session_fork_failed from
    // claude_structure::fork, i.e. routed to the Claude creator.
    run("fork × Claude source", Expect::RoutedTo("session_fork_failed"), fork(&chat));
    let mut codex_source = cutex::session::model::CutexSessionRecord::new(
        "cutex.codex-source".into(),
        Some("018a0000-0000-7000-8000-0000000000c1".into()),
        cutex::platform::host::current_host_name(),
        work.clone(),
        Some(CODEX.into()),
    )
    .unwrap();
    codex_source.exposed_to_backend = true;
    let mut store = cutex::session::store::load_cutex_session_store().unwrap();
    store.sessions.insert(codex_source.cutex_session_id.clone(), codex_source);
    cutex::session::store::save_cutex_session_store(&store).unwrap();
    let message = fork("cutex.codex-source").unwrap_err();
    assert!(!message.contains("session_fork_failed") || !message.contains("claude"), "{message}");
    run("fork × Codex source", Expect::CodexCreator, Err(message));

    // 5. `cutex human new` (no profile argument: the default profile).
    run(
        "cutex human new × default Codex",
        Expect::CodexCreator,
        super::light_new::create("cli-codex", &work).map(|r| r.adopted.record.cutex_session_id).map_err(|e| format!("{e:#}")),
    );
    fixture.set_default_profile(CLAUDE);
    run(
        "cutex human new × default Claude",
        Expect::Refused("is a Claude subscription profile"),
        super::light_new::create("cli-claude", &work).map(|r| r.adopted.record.cutex_session_id).map_err(|e| format!("{e:#}")),
    );
    eprintln!("creation matrix:\n  {}", cells.join("\n  "));
}

/// `human-agent-scratch-cwd`: POST /v2/human/agents without cwd and
/// projectId creates the agent in a fresh owner-private
/// `<config dir>/agents-scratch/<id>`; a retry of the same requestId replays
/// the answer without a second directory; refusals and failed creations leave
/// no directory; an explicit cwd is used as before.
#[test]
fn human_agent_without_cwd_or_project_gets_a_private_scratch_workspace() {
    let fixture = Fixture::new();
    fixture.set_default_profile(CODEX);
    let area = cutex::config::paths::config_dir()
        .unwrap()
        .join(cutex::management::v2::human_agents::AGENT_SCRATCH_DIR_NAME);
    let entries = || -> Vec<std::path::PathBuf> {
        match std::fs::read_dir(&area) {
            Ok(dir) => dir.map(|entry| entry.unwrap().path()).collect(),
            Err(_) => Vec::new(),
        }
    };
    let create = |body: Value| {
        let (status, body) = root_request("POST", "/v2/human/agents", body);
        created_id(status, &body)
    };

    let body = json!({"requestId": "scratch-claude", "name": "scratch-claude", "profile": CLAUDE});
    let id = create(body.clone()).expect("created without cwd");
    let record = session_record(&id);
    let cwd = std::path::PathBuf::from(&record.cwd);
    assert!(cwd.is_dir(), "scratch cwd exists: {}", cwd.display());
    let canonical_area = area.canonicalize().unwrap();
    // The stored cwd is plain (no `\\?\` on Windows); compare canonical forms.
    assert_eq!(cwd.parent().unwrap().canonicalize().unwrap(), canonical_area, "under agents-scratch");
    assert_eq!(entries().len(), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [cwd.as_path(), area.as_path()] {
            assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o700, "{}", path.display());
        }
    }
    #[cfg(windows)]
    cutex::platform::private_fs::open_validated_directory(&cwd).expect("owner-only DACL");
    assert!(record.agent_enabled && record.exposed_to_backend);
    assert_eq!(record.formal_agent_name.as_deref(), Some("scratch-claude"));

    // Same requestId: the stored answer, same agent and directory.
    assert_eq!(create(body).expect("retry"), id);
    assert_eq!(entries().len(), 1, "a retry allocates no second directory");

    // A validation refusal happens before allocation.
    let refused = create(json!({"requestId": "scratch-sandbox", "name": "scratch-sandbox",
        "profile": CLAUDE, "launch": {"sandbox": "read-only"}}));
    assert!(refused.unwrap_err().contains(CODEX_ONLY));
    // A creation that fails (no native runtime in the test home) removes its
    // untouched scratch directory.
    let failed = create(json!({"requestId": "scratch-codex", "name": "scratch-codex", "profile": CODEX}));
    check("scratch × Codex", failed, &Expect::CodexCreator);
    assert_eq!(entries().len(), 1, "no orphan directory");

    // An explicit cwd is used exactly as before.
    let explicit = create(json!({"requestId": "explicit", "name": "explicit-cwd", "profile": CLAUDE, "cwd": fixture.work()}))
        .expect("explicit cwd");
    // The stored cwd is plain (no `\\?\` on Windows); compare canonical forms.
    assert_eq!(
        std::path::PathBuf::from(session_record(&explicit).cwd).canonicalize().unwrap(),
        std::path::Path::new(&fixture.work()).canonicalize().unwrap()
    );
    assert_eq!(entries().len(), 1);
}
