//! Clap command-line argument shape for cutex.

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

/// cutex - profile launcher for cute-codex
#[derive(Parser, Debug)]
#[command(
    name = "cutex",
    about = "Interactive session manager and profile launcher for cute-codex",
    version = crate::build_info::version_line(),
    subcommand_required = false,
    arg_required_else_help = false,
    after_help = "CLI selection order: CUTEX_CODEX_BIN override, then cute-codex, then cutex-codex, then codex."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<CommandKind>,

    /// Use the default profile without interactive selection when
    /// running without an explicit subcommand.
    #[arg(short = 'q', long = "quick")]
    pub quick: bool,

    /// Force this invocation to run the selected CLI on the host, even for Docker profiles.
    #[arg(long = "host")]
    pub host: bool,

    /// Enable cutex inter-agent collaboration for this launch.
    #[arg(long = "agent", visible_alias = "collab")]
    pub agent: bool,

    /// Collaboration group(s) for this agent launch. Implies --agent.
    #[arg(long = "group", value_name = "GROUP", num_args = 1.., action = ArgAction::Append)]
    pub groups: Vec<String>,

    /// When no subcommand is provided, any remaining arguments are
    /// passed through to the selected CLI invocation.
    #[arg(last = true, value_name = "CLI_ARGS")]
    pub codex_args: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum CommandKind {
    /// Schedule, list or cancel durable one-shot Agent messages
    Alarm {
        #[command(subcommand)]
        command: AlarmCommand,
    },
    /// Configure or inspect a profile's custom quota endpoint
    Quota {
        profile: String,
        #[arg(long, conflicts_with = "disable")]
        config: Option<std::path::PathBuf>,
        #[arg(long)]
        disable: bool,
        #[arg(long)]
        refresh: bool,
    },
    /// Human management and recovery (also for agents acting at your request)
    Human {
        #[command(subcommand)]
        command: HumanCommand,
    },
    /// List profiles (legacy alias for `profile list`)
    #[command(hide = true)]
    List,

    /// Show active profile details (legacy alias for `profile show`)
    #[command(hide = true)]
    Current,

    /// Switch active profile (legacy alias for `profile use`)
    #[command(hide = true)]
    Use {
        /// Account name or id
        target: String,
    },

    /// Start a new ordinary session, selecting a profile when omitted
    New {
        /// Profile name or id
        profile: Option<String>,
    },

    /// Switch account and then run the selected CLI
    Run {
        /// Account name or id
        profile: String,
        /// Force this invocation to run the selected CLI on the host.
        #[arg(long = "host")]
        host: bool,
        /// Enable cutex inter-agent collaboration for this launch.
        #[arg(long = "agent", visible_alias = "collab")]
        agent: bool,
        /// Collaboration group(s) for this agent launch. Implies --agent.
        #[arg(long = "group", value_name = "GROUP", num_args = 1.., action = ArgAction::Append)]
        groups: Vec<String>,
        /// Override the Docker image only for this invocation.
        #[arg(skip)]
        docker_image: Option<String>,
        /// Override the Docker user name only for this invocation.
        #[arg(skip)]
        docker_user_name: Option<String>,
        /// Arguments to pass to the selected CLI
        #[arg(last = true, value_name = "CLI_ARGS")]
        codex_args: Vec<String>,
    },

    /// Open the agent TUI; list filters retain the compatibility start picker
    Start {
        #[command(flatten)]
        list: SessionListArgs,
    },

    /// Open the continuous agent selector
    Tui,

    /// Add an account from an existing auth file
    #[command(hide = true)]
    Add {
        /// Path to auth.json (codex) or .credentials.json (claude)
        #[arg(long, value_name = "PATH")]
        from_auth: String,
        /// Optional path to config.toml
        #[arg(long, value_name = "PATH")]
        from_config: Option<String>,
        /// Run this profile inside a Docker image
        #[arg(skip)]
        docker_image: Option<String>,
        /// Logical username used for the Docker home path
        #[arg(skip)]
        docker_user_name: Option<String>,
        /// Friendly account name (e.g., "work", "personal")
        #[arg(long)]
        name: String,
        /// CLI type: codex (default) or claude
        #[arg(long, default_value = "codex")]
        cli: String,
    },

    /// Log in and create a new profile (interactive wizard if no arguments)
    Login {
        /// Friendly account name (e.g., "work", "personal")
        #[arg(long)]
        name: Option<String>,
        /// CLI type: codex or claude
        #[arg(long)]
        cli: Option<String>,
        /// API key for third-party provider login (skips OAuth)
        #[arg(long)]
        api_key: Option<String>,
        /// API base URL for third-party provider
        #[arg(long)]
        base_url: Option<String>,
        /// Provider display name (e.g., "deepseek", "anthropic")
        #[arg(long)]
        provider: Option<String>,
    },

    /// Rename an existing account
    #[command(hide = true)]
    Rename {
        /// Existing account name or id
        target: String,
        /// New account name
        #[arg(long)]
        name: String,
    },

    /// Remove an existing account
    #[command(hide = true)]
    Remove {
        /// Existing account name or id
        target: String,
    },

    /// Edit display metadata for a profile (legacy alias for `profile set`)
    #[command(hide = true)]
    Annotate {
        /// Existing account name or id
        target: String,
        /// Override the displayed source/provider label
        #[arg(long, conflicts_with = "clear_source")]
        source: Option<String>,
        /// Clear the displayed source/provider label
        #[arg(long)]
        clear_source: bool,
        /// Override the displayed plan label
        #[arg(long, conflicts_with = "clear_plan")]
        plan: Option<String>,
        /// Clear the displayed plan label
        #[arg(long)]
        clear_plan: bool,
        /// Override the displayed email label
        #[arg(long, conflicts_with = "clear_email")]
        email: Option<String>,
        /// Clear the displayed email label
        #[arg(long)]
        clear_email: bool,
    },

    /// Configure runtime for a profile (legacy alias for `profile set`)
    #[command(hide = true)]
    Runtime {
        /// Account name or id
        target: String,
        /// Run this profile on the host
        #[arg(long)]
        host: bool,
        /// Run this profile inside a Docker image
        #[arg(skip)]
        docker_image: Option<String>,
        /// Logical username used for the Docker home path
        #[arg(skip)]
        docker_user_name: Option<String>,
    },

    /// Unified profile management commands
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },

    /// Unified global settings (proxy, docker sudo, and other defaults)
    Global {
        #[command(subcommand)]
        command: GlobalCommand,
    },

    /// Report objective per-agent token usage observed by Cutex
    Usage {
        /// Time bucket used for report rows
        #[arg(long, value_enum, default_value = "day")]
        period: UsagePeriodArg,
        /// Dimension used within each time bucket
        #[arg(long, value_enum, default_value = "agent")]
        group_by: UsageGroupByArg,
        /// Inclusive lower bound as RFC3339 or YYYY-MM-DD (UTC)
        #[arg(long, value_name = "TIME", conflicts_with = "last")]
        since: Option<String>,
        /// Exclusive upper bound as RFC3339 or YYYY-MM-DD (UTC)
        #[arg(long, value_name = "TIME")]
        until: Option<String>,
        /// Relative range ending at --until or now, for example 24h, 7d, or 8w
        #[arg(long, value_name = "DURATION", conflicts_with = "since")]
        last: Option<String>,
        /// Reset window used when --period reset
        #[arg(long, value_enum, default_value = "primary")]
        reset_window: UsageResetWindowArg,
        /// Emit the exact report as JSON
        #[arg(long)]
        json: bool,
    },

    /// Configure proxy settings (legacy compatibility command)
    #[command(hide = true)]
    Proxy {
        #[command(subcommand)]
        command: ProxyCommand,
    },

    /// Manage durable cutex sessions and cute-alden attachments
    #[command(visible_alias = "ss")]
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },

    /// Manage local notification bridges
    Notify {
        #[command(subcommand)]
        command: NotifyCommand,
    },

    /// Manage IM/workbench registration for coding sessions
    Im {
        #[command(subcommand)]
        command: ImCommand,
    },

    /// Serve backend-facing coding session management APIs
    Management {
        #[command(subcommand)]
        command: ManagementCommand,
    },

    /// Independent cutex instances (CUTEX_HOME; docs/INSTANCES.md)
    Instance {
        #[command(subcommand)]
        command: InstanceCommand,
    },

    /// Backend self-upgrade of a managed install (docs/backend-upgrade.md)
    Upgrade {
        #[command(subcommand)]
        command: UpgradeCommand,
    },

    /// Optional backend components of a managed install (components/README.md)
    Component {
        #[command(subcommand)]
        command: ComponentCommand,
    },

    /// User services of a managed install (systemd --user units on Linux,
    /// the cutex supervisor logon task on Windows)
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },

    /// List and message cutex-launched agents
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },

    /// Open the main interactive configuration wizard
    #[command(visible_alias = "config")]
    Wizard,
}

#[derive(Subcommand, Debug)]
pub enum GlobalCommand {
    /// Show effective global settings
    Show,

    /// Interactively edit global settings
    #[command(visible_alias = "wizard")]
    Edit,

    /// Update global settings in one command
    Set {
        /// Enable or disable managed cute-alden sessions by default
        #[arg(long = "session-enable", value_name = "BOOL")]
        session_enable: Option<bool>,
        /// Profile name or id used as the global default fallback
        #[arg(
            long = "default-profile",
            value_name = "PROFILE",
            conflicts_with = "clear_default_profile"
        )]
        default_profile: Option<String>,
        /// Clear the configured global default profile fallback
        #[arg(long = "clear-default-profile", conflicts_with = "default_profile")]
        clear_default_profile: bool,
        /// Start the configured default profile directly when running plain `cutex`
        #[arg(long = "default-profile-direct-launch", value_name = "BOOL")]
        default_profile_direct_launch: Option<bool>,
        /// Set the global proxy URL
        #[arg(long = "proxy-url", value_name = "URL", conflicts_with = "proxy_clear")]
        proxy_url: Option<String>,
        /// Optional NO_PROXY value for --proxy-url
        #[arg(long = "proxy-no-proxy", value_name = "VALUE", requires = "proxy_url")]
        proxy_no_proxy: Option<String>,
        /// Optional force-http value for --proxy-url (true/false)
        #[arg(long = "proxy-force-http", value_name = "BOOL", requires = "proxy_url")]
        proxy_force_http_transport: Option<bool>,
        /// Clear the global proxy fallback
        #[arg(long = "proxy-clear", conflicts_with = "proxy_url")]
        proxy_clear: bool,
        /// Enable or disable the local inter-agent message bus
        #[arg(long = "agent-bus-enable", value_name = "BOOL")]
        agent_bus_enable: Option<bool>,
        /// Fixed local port for the inter-agent message bus
        #[arg(long = "agent-bus-port", value_name = "PORT")]
        agent_bus_port: Option<u16>,
        /// Shared token for the inter-agent message bus; pass '-' to clear
        #[arg(long = "agent-bus-token", value_name = "TOKEN")]
        agent_bus_token: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ProxyCommand {
    /// Show the global proxy or the effective proxy for one profile
    Show {
        /// Optional account name or id
        profile: Option<String>,
    },

    /// Set the global proxy used when a profile does not override it
    Set {
        /// Proxy URL, for example http://127.0.0.1:7890 or socks5h://127.0.0.1:7890
        url: String,
        /// Optional NO_PROXY value
        #[arg(long = "no-proxy")]
        no_proxy: Option<String>,
        /// Force cute-codex model traffic away from WebSocket transport
        #[arg(long = "force-http", default_value_t = true, action = ArgAction::Set)]
        force_http_transport: bool,
    },

    /// Clear the global proxy
    Clear,

    /// Set a proxy override for one profile
    SetProfile {
        /// Account name or id
        profile: String,
        /// Proxy URL, for example http://127.0.0.1:7890 or socks5h://127.0.0.1:7890
        url: String,
        /// Optional NO_PROXY value
        #[arg(long = "no-proxy")]
        no_proxy: Option<String>,
        /// Force cute-codex model traffic away from WebSocket transport
        #[arg(long = "force-http", default_value_t = true, action = ArgAction::Set)]
        force_http_transport: bool,
    },

    /// Disable proxy inheritance for one profile
    DisableProfile {
        /// Account name or id
        profile: String,
    },

    /// Clear a profile proxy override so it inherits the global proxy again
    ClearProfile {
        /// Account name or id
        profile: String,
    },
}

#[derive(Args, Debug, Clone)]
pub struct SessionListArgs {
    /// Show every durable session, including historical local-only sessions
    #[arg(long)]
    pub all: bool,

    /// Include only offline durable sessions
    #[arg(long)]
    pub offline: bool,

    /// Include only non-persistent local/ephemeral durable sessions
    #[arg(long = "one-shot")]
    pub one_shot: bool,

    /// Include only host-backed durable sessions
    #[arg(long)]
    pub host: bool,

    /// Only show cute-alden runtime sessions
    #[arg(long)]
    pub alden: bool,

    /// Include only attachable durable sessions
    #[arg(long)]
    pub attachable: bool,

    /// Filter by project/cwd/name/id text
    #[arg(long = "project", value_name = "TEXT", action = ArgAction::Append)]
    pub projects: Vec<String>,

    /// Filter by session group
    #[arg(long = "group", value_name = "GROUP", action = ArgAction::Append)]
    pub groups: Vec<String>,

    /// Sort durable sessions
    #[arg(long, value_enum, default_value = "status")]
    pub sort: SessionListSort,
}

impl Default for SessionListArgs {
    fn default() -> Self {
        Self {
            all: false,
            offline: false,
            one_shot: false,
            host: false,
            alden: false,
            attachable: false,
            projects: Vec::new(),
            groups: Vec::new(),
            sort: SessionListSort::Status,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SessionListSort {
    Status,
    Recent,
    Name,
    Project,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum UsagePeriodArg {
    Total,
    Hour,
    Day,
    Week,
    Reset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum UsageGroupByArg {
    Agent,
    Profile,
    Model,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum UsageResetWindowArg {
    Primary,
    Secondary,
}

#[derive(Subcommand, Debug)]
pub enum SessionCommand {
    /// Private Linux stock-U opt-in: submit an explicit root-Human review/action JSON file
    Stock {
        #[arg(long)]
        request: std::path::PathBuf,
        /// Existing local Management endpoint; never starts a service
        #[arg(long)]
        management_url: String,
    },
    /// Attach stock native CLI to the exact already-ready owner (no new writer)
    StockAttach { id: String },
    /// Close and archive a native session and its spawned descendants; retain history
    CloseNative {
        id: String,
        /// Existing daemon Unix socket (defaults to the native home's control socket)
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// Restore a native archived session without starting its runtime
    RestoreNative {
        id: String,
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// Open the interactive session management wizard
    #[command(visible_alias = "edit")]
    Wizard {
        #[command(flatten)]
        list: SessionListArgs,
    },

    /// List durable cutex sessions and known cute-alden sessions
    List {
        #[command(flatten)]
        list: SessionListArgs,
    },

    /// Show a durable cutex session by cutex_session_id or Codex session id
    Show {
        /// cutex_session_id or Codex session id
        id: String,
    },

    /// List reversibly archived durable Agents (not permanent Management retirement)
    #[command(alias = "archived")]
    Retired {
        /// Print machine-readable JSON
        #[arg(long)]
        json: bool,
    },

    /// Reversibly archive an Agent with guarded whole-runtime stop proof
    #[command(alias = "archive")]
    Retire {
        /// Exact durable cutex_session_id
        id: String,
        /// Optional audit reason
        #[arg(long)]
        reason: Option<String>,
        /// Print machine-readable JSON
        #[arg(long)]
        json: bool,
    },

    /// Restore a reversibly archived Agent Offline, retaining current membership
    Restore {
        /// Exact durable cutex_session_id
        id: String,
        /// Print machine-readable JSON
        #[arg(long)]
        json: bool,
    },

    /// Back up and close orphaned turns in an offline native rollout
    RepairHistory {
        /// cutex_session_id or Codex session id
        id: String,
        /// Print machine-readable JSON
        #[arg(long)]
        json: bool,
    },

    /// Adopt a recent/local Codex session into durable cutex management
    Adopt {
        /// cutex_session_id, Codex session id, or unique thread/display name
        id: String,
        /// Display name hint for cutex/IM surfaces
        #[arg(long)]
        name: Option<String>,
        /// Managed launch cwd override
        #[arg(long = "cwd", conflicts_with = "current_cwd")]
        cwd: Option<String>,
        /// Set managed launch cwd to the shell's current directory
        #[arg(long = "current-cwd")]
        current_cwd: bool,
        /// Collaboration group(s) to persist
        #[arg(long = "group", value_name = "GROUP", num_args = 1.., action = ArgAction::Append)]
        groups: Vec<String>,
        /// Legacy, ignored: every managed session is listed for the owner and
        /// mirrored to the IM registry (im-exposure-retired)
        #[arg(long = "im")]
        expose_to_im: bool,
        /// Pin this session on the start screen
        #[arg(long)]
        pin: bool,
    },

    /// Expose a Codex session to the management/IM backend
    Expose {
        /// cutex_session_id or Codex session id
        id: String,
        /// Display name hint for frontends
        #[arg(long)]
        name: Option<String>,
        /// Replace groups on the exposed session
        #[arg(long = "group", value_name = "GROUP", num_args = 1.., action = ArgAction::Append)]
        groups: Vec<String>,
    },

    /// Clear a session's legacy IM-registry flag (a durable session stays in the owner's list)
    Hide {
        /// cutex_session_id or Codex session id
        id: String,
    },

    /// Remove cutex management metadata without deleting history or killing runtime
    Unmanage {
        /// cutex_session_id, Codex session id, or unique thread/display name
        id: String,
    },

    /// Control whether a session appears on the start quick-action screen
    Quick {
        #[command(subcommand)]
        command: SessionQuickCommand,
    },

    /// Edit durable session groups
    Groups {
        #[command(subcommand)]
        command: SessionGroupsCommand,
    },

    /// Set or clear the durable profile used for future managed launches
    Profile {
        #[command(subcommand)]
        command: SessionProfileCommand,
    },

    /// Edit durable session runtime defaults
    Defaults {
        #[command(subcommand)]
        command: SessionDefaultsCommand,
    },

    /// Edit the cutex-managed launch cwd for a session
    Cwd {
        #[command(subcommand)]
        command: SessionCwdCommand,
    },

    /// Bring a managed session online
    Online {
        /// cutex_session_id or Codex session id
        id: String,
        /// Use a profile for processes launched by this action only
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
    },

    /// Resume a managed session in this visible terminal
    Foreground {
        /// cutex_session_id or Codex session id
        id: String,
        /// Use a profile for this visible TUI launch only
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
    },

    /// Bring a managed session offline
    Offline {
        /// cutex_session_id or Codex session id
        id: String,
        /// Force stop where supported
        #[arg(long)]
        force: bool,
    },

    /// Close a managed session runtime
    Close {
        /// cutex_session_id or Codex session id
        id: String,
        /// Force close where supported
        #[arg(long)]
        force: bool,
    },

    /// Attach to an existing named cute-alden session
    Attach {
        /// Session name
        #[arg(long)]
        name: String,
        /// Detach any active cute-alden client first, then attach here
        #[arg(long)]
        takeover: bool,
    },

    /// Take over an attachable cute-alden runtime by cutex/Codex session id
    Takeover {
        /// cutex_session_id, Codex session id, or live cute-alden session name
        id: String,
    },

    /// Check whether resuming a Codex session would duplicate a live runtime
    DuplicateCheck {
        /// cutex_session_id or Codex session id
        id: String,
        /// Print machine-readable JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum SessionQuickCommand {
    /// Always recommend this session on the start screen
    Pin {
        /// cutex_session_id or Codex session id
        id: String,
    },

    /// Never recommend this session on the start screen
    Hide {
        /// cutex_session_id or Codex session id
        id: String,
    },

    /// Let cutex recommend this session only from cwd or explicit user history
    Auto {
        /// cutex_session_id or Codex session id
        id: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum SessionDefaultsCommand {
    /// Show runtime defaults for one session
    Show {
        /// cutex_session_id or Codex session id
        id: String,
    },

    /// Update runtime defaults for one session
    Set {
        /// cutex_session_id or Codex session id
        id: String,
        /// Runtime backend: host, native, docker, cute-alden, future
        #[arg(long = "runtime-backend")]
        runtime_backend: Option<String>,
        /// Permission preset such as read-only, workspace, or full-access
        #[arg(long = "permission")]
        permission_defaults: Option<String>,
        /// Approval policy such as on-request or never
        #[arg(long = "approval-policy")]
        approval_policy: Option<String>,
        /// Sandbox mode such as workspace-write or danger-full-access
        #[arg(long = "sandbox")]
        sandbox_mode: Option<String>,
        /// Default model override
        #[arg(long)]
        model: Option<String>,
        /// Default reasoning effort
        #[arg(long)]
        reasoning: Option<String>,
        /// Replace extra cute-codex CLI args; repeat for multiple args
        #[arg(long = "cli-arg", value_name = "ARG", action = ArgAction::Append, allow_hyphen_values = true)]
        cli_args: Vec<String>,
        /// Clear all extra cute-codex CLI args
        #[arg(long = "clear-cli-args")]
        clear_cli_args: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum SessionCwdCommand {
    /// Show session cwd, current cwd, and managed cwd
    Show {
        /// cutex_session_id or Codex session id
        id: String,
    },

    /// Set the managed launch cwd
    Set {
        /// cutex_session_id or Codex session id
        id: String,
        /// Path to use for future managed launches
        path: String,
    },

    /// Set the managed launch cwd to the shell's current directory
    Current {
        /// cutex_session_id or Codex session id
        id: String,
    },

    /// Clear the managed cwd and fall back to the session cwd
    Clear {
        /// cutex_session_id or Codex session id
        id: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum SessionGroupsCommand {
    /// Replace all groups
    Set {
        /// cutex_session_id or Codex session id
        id: String,
        /// Group to set
        #[arg(long = "group", value_name = "GROUP", num_args = 1.., action = ArgAction::Append)]
        groups: Vec<String>,
    },

    /// Add groups
    Add {
        /// cutex_session_id or Codex session id
        id: String,
        /// Group to add
        #[arg(long = "group", value_name = "GROUP", num_args = 1.., action = ArgAction::Append)]
        groups: Vec<String>,
    },

    /// Remove groups
    Remove {
        /// cutex_session_id or Codex session id
        id: String,
        /// Group to remove
        #[arg(long = "group", value_name = "GROUP", num_args = 1.., action = ArgAction::Append)]
        groups: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum SessionProfileCommand {
    /// Set the named profile as durable configured intent for a session
    Set {
        /// cutex_session_id or Codex session id
        id: String,
        /// Existing profile name or id on the target session's host
        profile: String,
    },
    /// Clear durable configured intent and follow the global default next launch
    Clear {
        /// cutex_session_id or Codex session id
        id: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum AgentCommand {
    /// List currently registered cutex agents
    List {
        /// Only show agents in this group
        #[arg(long = "group")]
        group: Vec<String>,
        /// Show all groups even when running inside an agent
        #[arg(long)]
        all_groups: bool,
    },

    /// Send a message to another cutex agent
    Send {
        /// Runtime endpoint ID, durable cutex session ID, or unique display/thread name
        target: String,
        /// Message to deliver
        message: String,
        /// Stable external identifier for idempotent message delivery
        #[arg(
            long,
            value_name = "NONEMPTY_ID",
            value_parser = parse_external_message_id
        )]
        external_message_id: Option<String>,
        /// Resolve the target across all registered groups
        #[arg(long)]
        all_groups: bool,
        /// Deliver passively without asking the target to process it
        #[arg(long = "queue-only")]
        queue_only: bool,
        /// Ask the target to process the message as soon as possible
        #[arg(long)]
        soon: bool,
        /// Request an explicit interruption of the target's current work
        #[arg(long)]
        interrupt: bool,
        /// Override the sender label
        #[arg(long)]
        from: Option<String>,
    },

    /// Call the project-scoped Agent Management service as the current Agent
    Manage {
        #[command(subcommand)]
        command: AgentManagementCliCommand,
    },

    /// Show agent bus config and health
    Status,

    /// Show recent local agent-bus audit events
    Log {
        /// Only show records related to this agent id/name
        #[arg(long)]
        agent: Option<String>,
        /// Maximum records to print
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Print raw JSONL records
        #[arg(long)]
        json: bool,
    },

    /// Change live agent collaboration groups
    Groups {
        #[command(subcommand)]
        command: AgentGroupsCommand,
    },

    /// Legacy: point this machine at another host's bus through Bridgeboard
    RemoteUp {
        /// Bus owner host, such as devbox
        host: String,
        /// Bridgeboard service id to use; defaults to cutex-agent-bus
        #[arg(long)]
        service_id: Option<String>,
        /// Local forwarded port to configure cutex against
        #[arg(long)]
        local_port: Option<u16>,
        /// Remote bus port on the owner host
        #[arg(long)]
        remote_port: Option<u16>,
        /// Shared bus token; omit to keep the current configured token
        #[arg(long)]
        token: Option<String>,
        /// Show fallback SSH command even if Bridgeboard succeeds
        #[arg(long)]
        show_ssh_fallback: bool,
        /// Do not update local cutex config
        #[arg(long)]
        no_config: bool,
    },

    /// Run the shared agent bus in the foreground
    #[command(hide = true)]
    Serve {
        /// Port to bind on 127.0.0.1
        #[arg(long)]
        port: Option<u16>,
        /// DEPRECATED (visible in process listings): bearer token accepted
        /// by the bus. Omit it: the bus reads `agent_bus_token` from the
        /// owner-private config.json (or --token-file / the
        /// CUTEX_AGENT_BUS_SERVE_TOKEN environment variable).
        #[arg(long)]
        token: Option<String>,
        /// Read the bus token from this owner-private file (first line,
        /// trimmed) instead of config.json, e.g. a systemd credential.
        #[arg(long, value_name = "PATH")]
        token_file: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
pub enum AgentManagementCliCommand {
    Create {
        #[arg(long)]
        request_file: String,
    },
    QueryManaged {
        #[arg(long)]
        request_file: String,
    },
    Online {
        #[arg(long)]
        request_file: String,
    },
    Offline {
        #[arg(long)]
        request_file: String,
    },
    Restart {
        #[arg(long)]
        request_file: String,
    },
    /// Permanently retire managed Agent ownership (not reversible Archive or runtime Stop)
    #[command(alias = "permanent-retire")]
    Close {
        #[arg(long)]
        request_file: String,
    },
    Replace {
        #[arg(long)]
        request_file: String,
    },
    GrantOperator {
        #[arg(long)]
        request_file: String,
    },
    RevokeOperator {
        #[arg(long)]
        request_file: String,
    },
}

fn parse_external_message_id(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("external message ID cannot be empty or whitespace-only".to_string());
    }
    Ok(value.to_string())
}

#[derive(Subcommand, Debug)]
pub enum AgentGroupsCommand {
    /// Replace groups for a live agent by id, name, or session id
    Set {
        /// Agent id/name or session id
        target: String,
        /// New collaboration groups
        #[arg(value_name = "GROUP", required = true)]
        groups: Vec<String>,
    },

    /// Add groups to a live agent by id, name, or session id
    Add {
        /// Agent id/name or session id
        target: String,
        /// Groups to add
        #[arg(value_name = "GROUP", required = true)]
        groups: Vec<String>,
    },

    /// Remove groups from a live agent by id, name, or session id
    Remove {
        /// Agent id/name or session id
        target: String,
        /// Groups to remove
        #[arg(value_name = "GROUP", required = true)]
        groups: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ImCommand {
    /// Register or update a coding session in the IM/workbench registry
    Register {
        /// Codex/cute-codex session id
        session_id: String,
        /// Display name shown by IM/workbench
        #[arg(long)]
        name: Option<String>,
        /// Host where this session belongs
        #[arg(long)]
        host: Option<String>,
        /// Working directory for this session
        #[arg(long)]
        cwd: Option<String>,
        /// cutex profile for this session
        #[arg(long)]
        profile: Option<String>,
        /// Group(s) assigned to this session registration
        #[arg(long = "group", value_name = "GROUP", num_args = 1.., action = ArgAction::Append)]
        groups: Vec<String>,
        /// Register as temporary/ephemeral instead of persistent
        #[arg(long)]
        temporary: bool,
    },

    /// Register the current cutex-launched agent session in the IM/workbench registry
    RegisterCurrent {
        /// Display name shown by IM/workbench; defaults to the live agent base name
        #[arg(long)]
        name: Option<String>,
        /// Group(s) assigned to this session registration
        #[arg(long = "group", value_name = "GROUP", num_args = 1.., action = ArgAction::Append)]
        groups: Vec<String>,
        /// Register as temporary/ephemeral instead of persistent
        #[arg(long)]
        temporary: bool,
    },

    /// Hide/unmanage a coding session in the IM/workbench registry
    Unregister {
        /// Codex/cute-codex session id
        session_id: String,
    },

    /// Hide/unmanage the current cutex-launched agent session in the IM/workbench registry
    UnregisterCurrent,

    /// List registered IM/workbench coding sessions
    List,

    /// Show one registered IM/workbench coding session
    Show {
        /// Codex/cute-codex session id
        session_id: String,
    },

    /// Show current live agent/session registration diagnostics
    StatusCurrent,

    /// Change registered session groups
    Groups {
        #[command(subcommand)]
        command: ImGroupsCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum ImGroupsCommand {
    /// Replace registered groups for a coding session
    Set {
        /// Codex/cute-codex session id
        session_id: String,
        /// New groups
        #[arg(value_name = "GROUP", required = true)]
        groups: Vec<String>,
    },

    /// Add registered groups for a coding session
    Add {
        /// Codex/cute-codex session id
        session_id: String,
        /// Groups to add
        #[arg(value_name = "GROUP", required = true)]
        groups: Vec<String>,
    },

    /// Remove registered groups for a coding session
    Remove {
        /// Codex/cute-codex session id
        session_id: String,
        /// Groups to remove
        #[arg(value_name = "GROUP", required = true)]
        groups: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ManagementCommand {
    /// Run the local HTTP management API in the foreground
    Serve {
        /// Port to bind on 127.0.0.1
        #[arg(long)]
        port: Option<u16>,
        /// Address to listen on; repeatable: `127.0.0.1`, `<ip>:<port>`, a
        /// Tailscale IPv4 address or `tailscale` (`tailscale ip -4`, resolved
        /// in the background). Default: config `management_listen`, else
        /// 127.0.0.1. Non-loopback listeners accept only client / device tokens.
        #[arg(long)]
        bind: Vec<String>,
        /// Management bearer override; a value distinct from Agent Bus is also the root credential
        #[arg(long)]
        token: Option<String>,
    },

    /// Connect this machine to another host's management API through Bridgeboard
    RemoteUp {
        /// Management API owner host, such as WINBOX
        host: String,
        /// Bridgeboard service id to use; defaults to cutex-management-api
        #[arg(long)]
        service_id: Option<String>,
        /// Local forwarded port used by this host
        #[arg(long)]
        local_port: Option<u16>,
        /// Remote management API port on the owner host
        #[arg(long)]
        remote_port: Option<u16>,
        /// Management bearer override; omit to use configured credential selection
        #[arg(long)]
        token: Option<String>,
        /// Show fallback SSH command even if Bridgeboard succeeds
        #[arg(long)]
        show_ssh_fallback: bool,
    },

    /// Create a one-time pairing code for a phone (prints the code and a QR)
    Pair {
        /// Name shown for the device until it reports its own
        #[arg(long)]
        label: Option<String>,
        /// Local Management API port
        #[arg(long)]
        port: Option<u16>,
        /// Print the JSON response only
        #[arg(long)]
        json: bool,
    },

    /// Export the Management v2 JSON Schemas (for client type generation)
    Schema {
        #[command(subcommand)]
        command: ManagementSchemaCommand,
    },

    /// Issue, list or revoke Management client tokens for remote clients
    ClientToken {
        #[command(subcommand)]
        command: ManagementClientTokenCommand,
    },

    /// Local users and token bindings (P1 users and grants)
    Users {
        #[command(subcommand)]
        command: ManagementUsersCommand,
    },

    /// Initialize or explicitly correct one project's authorized Director
    AgentAuthority {
        /// Strict project authority JSON request
        #[arg(long)]
        request_file: String,
        /// Local Management API port
        #[arg(long)]
        port: Option<u16>,
        /// Dedicated Management bearer override
        #[arg(long)]
        token: Option<String>,
    },

    /// Import one missing ownership record for the already-authorized legacy Director
    AgentOwnershipImport {
        /// Strict legacy Director ownership import JSON request
        #[arg(long)]
        request_file: String,
        /// Local Management API port
        #[arg(long)]
        port: Option<u16>,
        /// Dedicated Management bearer override
        #[arg(long)]
        token: Option<String>,
    },

    /// Reconcile one legacy pre-effect Agent reservation using root-owned evidence
    AgentReservationReconcile {
        /// Strict reservation reconciliation JSON request
        #[arg(long)]
        request_file: String,
        /// Local Management API port
        #[arg(long)]
        port: Option<u16>,
        /// Dedicated Management root bearer override
        #[arg(long)]
        token: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ManagementSchemaCommand {
    /// Write sessions/bootstrap/events/approvals/asks/notifications/summary/
    /// pair/error .json (JSON Schema 2020-12) and index.json into a directory
    Export {
        /// Output directory (created if missing; files are overwritten)
        #[arg(long)]
        out: std::path::PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum ManagementClientTokenCommand {
    /// Create a token; prints {"tokenId","label","token","createdAt"} once
    /// (the secret is not stored and cannot be shown again)
    Issue {
        /// Human-readable client name, for example desktop-<machine>
        #[arg(long)]
        label: String,
        /// Bind the token to this local user (`owner` or `u-…`). Without it
        /// the token is the owner's only for the host's first device or when
        /// `ownerRequiresApproval` is off; otherwise it is pending until the
        /// owner assigns it (Settings → Users, or `cutex management users assign`)
        #[arg(long, conflicts_with = "pending")]
        user: Option<String>,
        /// Leave the token pending (unassigned) even for a first device
        #[arg(long)]
        pending: bool,
    },
    /// List tokens (no secrets): tokenId, label, createdAt, lastUsedAt
    List,
    /// Revoke a token; requests using it are refused at once
    Revoke {
        /// Token id (mct-…) from `list`
        token_id: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum ManagementUsersCommand {
    /// Users, token bindings (no secrets), peers and settings as JSON
    List,
    /// Bind a client / device token to a user (`owner`, `u-…`) or `pending`
    Assign {
        /// Token id (mct-…)
        token_id: String,
        /// `owner`, a user id, or `pending`
        user: String,
    },
    /// Before rolling back to a cutex without users: list non-owner and
    /// pending tokens and non-owner peers (they would become root / unreadable
    /// there). Exit code 1 while any remain. `--revoke` revokes those tokens.
    RollbackCheck {
        #[arg(long)]
        revoke: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum NotifyCommand {
    /// Full per-session notification state snapshot for external adapters
    States,
    /// Acknowledge only the exact reminder previously displayed to the user
    Ack {
        thread_id: String,
        reminder_id: String,
    },
    /// Run the independent event-to-webhook adapter
    Run,
    /// Inspect worker health and bounded outbox
    Status,
    /// Apply outbound JSON configuration from a local file (token stays out of argv)
    Configure { file: std::path::PathBuf },
    /// Enqueue a clearly marked synthetic webhook test
    Test,
    /// List the latest deliveries, optionally filtered by state
    Deliveries {
        #[arg(long)]
        state: Option<String>,
    },
    /// Retry a failed notification against the currently configured destination
    Retry { event_id: String },
    /// Read or change a native session's external notification priority
    Session {
        /// Native session UUID (shown by cute-codex /status)
        thread_id: String,
        #[arg(long, value_enum, conflicts_with = "cycle")]
        level: Option<crate::notify::session::Level>,
        /// Cycle OFF -> CIAO! -> ON -> OFF
        #[arg(long)]
        cycle: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ProfileCommand {
    /// Discover or inspect models offered by an OpenAI-compatible API profile
    Models {
        target: String,
        /// Refresh from the provider's /models endpoint
        #[arg(long)]
        refresh: bool,
        /// Apply the generated model catalog to future launches
        #[arg(long)]
        apply: bool,
    },
    /// List profiles with full runtime/proxy/provider context
    List,

    /// Show one profile in detail (defaults to active profile)
    Show {
        /// Optional account name or id; defaults to the active profile
        target: Option<String>,
    },

    /// Interactively edit one profile (defaults to active profile)
    #[command(visible_alias = "wizard")]
    Edit {
        /// Optional account name or id; defaults to the active profile
        target: Option<String>,
    },

    /// Switch the active profile
    Use {
        /// Account name or id
        target: String,
    },

    /// Rename a profile
    Rename {
        /// Existing profile name or id
        target: String,
        /// New profile name
        #[arg(long)]
        name: String,
    },

    /// Remove a profile
    Remove {
        /// Existing profile name or id
        target: String,
    },

    /// Copy one profile's settings into a new profile, optionally changing
    /// provider settings. Credentials are not copied: log the copy in or set
    /// its API key afterwards (a ChatGPT login is never copied)
    Copy {
        /// Source profile name or id
        source: String,
        /// New profile name
        #[arg(long)]
        name: String,
        /// Optional provider id for the copied profile
        #[arg(long)]
        provider: Option<String>,
        /// Optional base_url override for the copied provider
        #[arg(long = "provider-base-url")]
        provider_base_url: Option<String>,
        /// Also copy the source's API key (API-key profiles only)
        #[arg(long = "with-api-key")]
        with_api_key: bool,
    },

    /// Copy one profile's status_line to every profile
    CloneStatusLine {
        /// Source profile name or id; defaults to the active profile
        #[arg(long)]
        from: Option<String>,
    },

    /// Move a profile to the top of the list order
    PinTop {
        /// Account name or id
        target: String,
    },

    /// Move a profile to the bottom of the list order
    PinBottom {
        /// Account name or id
        target: String,
    },

    /// Update profile metadata, runtime, and proxy override in one command
    Set {
        /// Account name or id
        target: String,
        /// Rename the profile
        #[arg(long)]
        name: Option<String>,
        /// Override the displayed source/provider label
        #[arg(long, conflicts_with = "clear_source")]
        source: Option<String>,
        /// Clear the displayed source/provider label
        #[arg(long)]
        clear_source: bool,
        /// Override the displayed plan label
        #[arg(long, conflicts_with = "clear_plan")]
        plan: Option<String>,
        /// Clear the displayed plan label
        #[arg(long)]
        clear_plan: bool,
        /// Override the displayed email label
        #[arg(long, conflicts_with = "clear_email")]
        email: Option<String>,
        /// Clear the displayed email label
        #[arg(long)]
        clear_email: bool,
        /// Default CLI args prepended for this profile, parsed like a shell command line
        #[arg(
            long = "default-cli-args",
            value_name = "SHELL",
            conflicts_with = "clear_default_cli_args"
        )]
        default_cli_args: Option<String>,
        /// Clear the stored default CLI args for this profile
        #[arg(long = "clear-default-cli-args", conflicts_with = "default_cli_args")]
        clear_default_cli_args: bool,
        /// Name exposed through `cutex agent list`
        #[arg(long = "agent-name", conflicts_with = "clear_agent_name")]
        agent_name: Option<String>,
        /// Clear the stored agent name and use the profile name
        #[arg(long = "clear-agent-name", conflicts_with = "agent_name")]
        clear_agent_name: bool,
        /// Run this profile on the host
        #[arg(long = "host")]
        host: bool,
        /// Run this profile inside a Docker image
        #[arg(skip)]
        docker_image: Option<String>,
        /// Logical username used for the Docker home path
        #[arg(skip)]
        docker_user_name: Option<String>,
        /// Set a profile proxy override URL (enables override)
        #[arg(
            long = "proxy-url",
            value_name = "URL",
            conflicts_with_all = ["proxy_disable", "proxy_inherit"]
        )]
        proxy_url: Option<String>,
        /// Optional NO_PROXY value for --proxy-url
        #[arg(long = "proxy-no-proxy", value_name = "VALUE", requires = "proxy_url")]
        proxy_no_proxy: Option<String>,
        /// Optional force-http value for --proxy-url (true/false)
        #[arg(long = "proxy-force-http", value_name = "BOOL", requires = "proxy_url")]
        proxy_force_http_transport: Option<bool>,
        /// Disable proxy inheritance for this profile
        #[arg(long = "proxy-disable", conflicts_with = "proxy_inherit")]
        proxy_disable: bool,
        /// Clear profile proxy override and inherit global proxy
        #[arg(long = "proxy-inherit")]
        proxy_inherit: bool,
        /// Force managed sessions on for this profile
        #[arg(
            long = "session-enable",
            conflicts_with_all = ["session_disable", "session_inherit"]
        )]
        session_enable: bool,
        /// Disable managed sessions for this profile
        #[arg(long = "session-disable", conflicts_with = "session_inherit")]
        session_disable: bool,
        /// Clear the profile session override so it inherits the global default
        #[arg(long = "session-inherit")]
        session_inherit: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum HumanCommand {
    /// Pair daemon instances for cross-host delegation.
    #[command(alias = "peers")]
    Peer {
        #[command(subcommand)]
        command: PeerCommand,
    },
    /// Claude subscription adapter (limited backend support)
    Claude {
        #[command(subcommand)]
        command: ClaudeCommand,
    },
    /// Compile a welcome animation; optionally save its editable source in Cutex settings.
    StatusAnimation {
        file: std::path::PathBuf,
        #[arg(long)]
        install: bool,
    },
    /// Host connections and read-only connection testing
    Hosts {
        #[command(subcommand)]
        command: HostsCommand,
    },
    /// Inspect local runtime, failed actions and storage without starting services
    Doctor {
        id: Option<String>,
        #[arg(long)]
        check: bool,
        #[arg(long)]
        fix: bool,
        #[arg(long, requires = "fix")]
        yes: bool,
    },
    /// Create a persistent native agent without sending a model prompt
    New {
        name: String,
        #[arg(long)]
        cwd: Option<std::path::PathBuf>,
    },
    /// Select an installed native runtime for new and adopted agents
    InstallRuntime {
        bundle_manifest: std::path::PathBuf,
        #[arg(long)]
        source_home: Option<std::path::PathBuf>,
        #[arg(long)]
        job_descriptor: Option<std::path::PathBuf>,
    },
    /// Inspect, change, or undo desired configuration
    Config {
        #[command(subcommand)]
        command: HumanConfigCommand,
    },
    /// Inspect the result of an existing runtime action; never starts another
    Action {
        action_id: String,
        #[arg(long)]
        resume: bool,
    },
    /// Clear an interrupted start after proving its process no longer exists
    Recover {
        id: String,
        #[arg(long)]
        action_id: Option<String>,
    },
    /// Explicitly inspect or retry one held External Input message
    ExternalInput {
        #[command(subcommand)]
        command: HumanExternalInputCommand,
    },
    /// Start a managed native agent using its current configuration
    Start { id: String },
    /// Stop an agent; --force sends a forced close first
    Stop {
        id: String,
        #[arg(long)]
        force: bool,
    },
    /// Restart an agent; --force does a forced stop and start instead of a live restart
    Restart {
        id: String,
        #[arg(long)]
        force: bool,
    },
    /// List or discard Agent Bus messages that cannot be delivered
    BusMessages {
        #[command(subcommand)]
        command: HumanBusMessagesCommand,
    },
    /// Owner-initiated Project operations (Director rotation)
    Project {
        #[command(subcommand)]
        command: HumanProjectCommand,
    },
    /// Attach to a running agent without restarting it
    Attach { id: String },
    /// Existing session configuration and lifecycle commands
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Management API and project authority administration
    Management {
        #[command(subcommand)]
        command: ManagementCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum PeerCommand {
    /// Export peer records and strip the ask store for rollback (both daemons must be stopped).
    ExportAndStrip {
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    Invite {
        #[arg(long)]
        label: Option<String>,
    },
    Add {
        url: String,
        code: String,
        #[arg(long)]
        label: Option<String>,
    },
    List,
    Remove {
        peer_id: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum HumanExternalInputCommand {
    /// List durable External Input candidates for one receiver
    List {
        id: String,
        /// Include delivered/output-observed messages instead of actionable ones only
        #[arg(long)]
        all: bool,
    },
    /// Review one message against its live native runtime
    Review { id: String, message_id: String },
    /// Explicitly retry one held message after accepting repeat-risk
    Retry {
        id: String,
        message_id: String,
        #[arg(long)]
        action_id: Option<String>,
        #[arg(long)]
        retry_id: Option<String>,
        #[arg(long, required = true)]
        confirm_repeat: bool,
    },
    /// Inspect an earlier explicit recovery action
    Status { action_id: String },
    /// Analyze undelivered backlog without changing any message
    Analyze {
        id: String,
        /// Stale threshold such as 30m, 6h, 7d, or seconds
        #[arg(long, default_value = "1h")]
        older_than: String,
    },
    /// Release one exact stale-held message for delivery
    Release {
        id: String,
        message_id: String,
        #[arg(long, required = true)]
        semantic_sha256: String,
        #[arg(long, required = true)]
        confirm_release: bool,
    },
    /// Discard one exact stale-held message while preserving an audit tombstone
    Discard {
        id: String,
        message_id: String,
        #[arg(long, required = true)]
        semantic_sha256: String,
        #[arg(long, required = true)]
        reason: String,
        #[arg(long, required = true)]
        confirm_discard: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ClaudeCommand {
    /// Select an installed adapter, official Claude CLI/login home, and TUI
    Install {
        file: std::path::PathBuf,
    },
    /// Save a Claude agent identity; Start opens the runtime without a model prompt
    New {
        name: String,
        #[arg(long)]
        cwd: Option<std::path::PathBuf>,
        #[arg(long)]
        model: String,
        #[arg(long)]
        allow_write: bool,
        #[arg(long)]
        allow_exec: bool,
        /// Claude profile (name or id); its Claude account decides the login
        /// home the agent keeps for life (default: the shared account)
        #[arg(long)]
        profile: Option<String>,
    },
    Start {
        id: String,
    },
    Status {
        id: String,
    },
    Close {
        id: String,
    },
    Attach {
        id: String,
    },
    /// Move Claude sessions to another adapter runtime (default: the selected
    /// one). Offline sessions change now; online ones are not interrupted and
    /// change at their next start. Keeps the thread, adapter state and Claude
    /// config home; prints the previous pin (saved as previous-deployment.json
    /// in the session state dir, usable as --deployment to undo).
    Repin {
        /// Session id or agent name; omit with --all
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        id: Option<String>,
        /// Every Claude session on this host
        #[arg(long)]
        all: bool,
        /// Target deployment JSON (same shape as `install`); default: the
        /// selected Claude runtime
        #[arg(long)]
        deployment: Option<std::path::PathBuf>,
        /// Report what would change without changing anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Claude accounts: several Claude logins / API keys on this host. A
    /// Claude profile names its account; an agent keeps the account it was
    /// created on.
    Account {
        #[command(subcommand)]
        command: ClaudeAccountCommand,
    },
}

/// `ACCOUNT` = `default`, an account id (`ca-…`) or its label.
#[derive(Debug, Subcommand)]
pub enum ClaudeAccountCommand {
    /// Accounts (default first) with status, profiles and agents
    List,
    /// Add a subscription account (then `login` it), or with --api an API
    /// account (key from stdin, optional Anthropic-compatible base URL)
    Add {
        label: String,
        #[arg(long)]
        api: bool,
        /// ANTHROPIC_BASE_URL, e.g. https://api.deepseek.com/anthropic
        #[arg(long, requires = "api")]
        base_url: Option<String>,
        /// ANTHROPIC_API_KEY (default, x-api-key) or ANTHROPIC_AUTH_TOKEN (Bearer)
        #[arg(long, requires = "api")]
        key_env: Option<String>,
        /// Extra Claude Code setting (repeatable), e.g.
        /// ANTHROPIC_DEFAULT_SONNET_MODEL=deepseek-v4-pro
        #[arg(long = "env", value_name = "KEY=VALUE", requires = "api")]
        env: Vec<String>,
        /// Read the API key from stdin (never pass it as an argument)
        #[arg(long, requires = "api")]
        api_key_stdin: bool,
    },
    /// Log a subscription account in: runs the official `claude auth login`
    /// in this terminal with the account's CLAUDE_CONFIG_DIR
    Login {
        account: String,
        #[arg(long)]
        email: Option<String>,
        /// Only print the commands (cutex / PowerShell / POSIX shell)
        #[arg(long)]
        print: bool,
    },
    /// `claude auth logout` for a subscription account
    Logout { account: String },
    /// Check now (`claude auth status`; API accounts: key stored)
    Status { account: String },
    /// Change the label or API settings (new values apply when an agent's
    /// adapter starts next)
    Set {
        account: String,
        #[arg(long)]
        label: Option<String>,
        #[arg(long, conflicts_with = "clear_base_url")]
        base_url: Option<String>,
        #[arg(long)]
        clear_base_url: bool,
        #[arg(long)]
        key_env: Option<String>,
        /// Replaces all extra settings (repeatable)
        #[arg(long = "env", value_name = "KEY=VALUE", conflicts_with = "clear_env")]
        env: Vec<String>,
        #[arg(long)]
        clear_env: bool,
        /// Replace the API key, read from stdin
        #[arg(long)]
        api_key_stdin: bool,
    },
    /// Bind a Claude profile to an account (`default` = the shared one);
    /// new agents on the profile use it
    Bind { profile: String, account: String },
    /// Deregister an account (refused while a profile or an active agent
    /// uses it). Keeps its home (login, Claude history) unless --delete-home
    Remove {
        account: String,
        #[arg(long)]
        delete_home: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum HumanBusMessagesCommand {
    /// Undelivered messages older than --min-age (default 1h), oldest first
    List {
        /// Minimum age, e.g. 30m, 1h, 2d, or seconds (0 = all)
        #[arg(long, default_value = "1h")]
        min_age: String,
        /// Only messages to or from this agent
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value_t = 200)]
        limit: usize,
        /// Print the raw JSON response
        #[arg(long)]
        json: bool,
    },
    /// Make one message terminal (never redriven) and drop it from live queues
    Discard {
        message_id: String,
        /// Why (kept in the record and the audit log)
        #[arg(long)]
        reason: String,
        /// Refuse unless the message still has this semantic digest
        #[arg(long)]
        expected_sha256: Option<String>,
        /// Allow Job / control messages (check the job first)
        #[arg(long)]
        allow_protected: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum HumanProjectCommand {
    /// Rotate a Project's Director: handoff note, successor, rebind, dispose
    /// (capability owner-rotation; omitted successor fields follow the Director)
    RotateDirector {
        /// Project id
        project: String,
        /// Successor name (default: the Director's name with -rN incremented)
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
        /// What the old Director becomes: member, operator or archive
        #[arg(long, default_value = "member")]
        predecessor: String,
        /// Seconds to wait for the handoff note (60-86400, default 600)
        #[arg(long)]
        handoff_timeout_secs: Option<u64>,
        /// Follow the rotation until it finishes
        #[arg(long)]
        wait: bool,
    },
    /// Show one rotation, or every rotation of a Project with --project
    Rotation {
        id: Option<String>,
        #[arg(long)]
        project: Option<String>,
        /// Cancel the rotation (before the successor is created)
        #[arg(long, conflicts_with = "continue_without_handoff")]
        cancel: bool,
        /// Stop waiting for the handoff note and continue
        #[arg(long = "continue")]
        continue_without_handoff: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum HumanConfigCommand {
    Show {
        id: String,
    },
    /// Set fields using KEY=VALUE; KEY=null clears an override
    Set {
        id: String,
        #[arg(required = true)]
        fields: Vec<String>,
        #[arg(long)]
        action_id: Option<String>,
    },
    Undo {
        id: String,
        original_action_id: String,
        #[arg(long)]
        action_id: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum HostsCommand {
    /// List a page of sessions on a configured host
    Sessions {
        id: String,
        #[arg(long, default_value = "")]
        query: String,
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Start or reconnect an existing session on its host
    Online { id: String, session: String },
    /// Close the remote runtime without archiving its session
    Close { id: String, session: String },
    /// Enter the remote host's own Cutex frontend over SSH
    Foreground { id: String, session: String },
    /// Show configured identities and connection details (no credential values)
    List,
    /// Replace hosts configuration from a JSON file
    Configure { file: std::path::PathBuf },
    /// Test an existing tunnel and verify its remote host identity
    Test { id: String },
}

#[derive(Debug, clap::Subcommand)]
pub enum AlarmCommand {
    Set {
        #[arg(long)]
        to: String,
        #[arg(long)]
        message: String,
        #[arg(long, conflicts_with = "after", required_unless_present = "after")]
        at: Option<String>,
        #[arg(long, conflicts_with = "at", help = "Delay in seconds")]
        after: Option<u64>,
        #[arg(long)]
        id: Option<String>,
    },
    List,
    Cancel {
        id: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum InstanceCommand {
    /// Initialize (or adopt) the instance selected by CUTEX_HOME: private
    /// home, `instance` section, ports, fresh tokens
    Init {
        /// Instance name ([a-z0-9][a-z0-9-]{0,31})
        #[arg(long)]
        name: String,
        /// Management API port (not a default-instance port)
        #[arg(long)]
        management_port: u16,
        /// Agent Bus port (not a default-instance port)
        #[arg(long)]
        agent_bus_port: u16,
        /// Allow non-loopback management listeners (tailscale, LAN)
        #[arg(long)]
        remote_listeners: bool,
    },
    /// Show the selected instance (no secrets)
    Show {
        #[arg(long)]
        json: bool,
    },
    /// Select a runtime from another deployment's bundle manifest (read-only:
    /// derives an instance-local manifest whose native home is inside the instance)
    UseRuntime {
        /// Source bundle manifest (version 4)
        bundle_manifest: std::path::PathBuf,
        /// Native version registry to copy (display metadata)
        #[arg(long)]
        registry: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ComponentCommand {
    /// Every component: state, selected / available version
    List {
        #[arg(long)]
        json: bool,
    },
    /// One component (or all) in detail (JSON)
    Status { name: Option<String> },
    /// Install or update to the version the running cutex release names
    Install {
        name: String,
        /// Build / take the files from a checkout instead (origin: source)
        #[arg(long = "from-source", value_name = "DIR")]
        from_source: Option<std::path::PathBuf>,
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Stop and unselect it for new agents (files and data stay; see prune)
    Remove {
        name: String,
        /// Even while jobs run or agents were launched with it
        #[arg(long)]
        force: bool,
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Delete versions nothing selects, pins or names
    Prune {
        name: String,
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum UpgradeCommand {
    /// Installed / available / staged release and the current run
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Read the channel now (signed channel pointer + manifest)
    Check,
    /// Download, verify and preflight a release (default: the available one)
    Stage {
        release: Option<String>,
    },
    /// Activate the staged release (waits for idle agents; never interrupts a turn)
    Apply {
        release: Option<String>,
        /// reload | full (only raises the computed mode)
        #[arg(long)]
        mode: Option<String>,
        /// Give up (deferred, nothing changed) when not idle within this
        #[arg(long = "max-wait")]
        max_wait_secs: Option<u64>,
        /// Required for a one-way release: repeat its name
        #[arg(long = "confirm-one-way", value_name = "RELEASE")]
        confirm_one_way: Option<String>,
        /// after | during | none (full mode)
        #[arg(long = "package-run")]
        package_run: Option<String>,
        /// Return right after starting the runner
        #[arg(long = "no-follow")]
        no_follow: bool,
    },
    /// Cancel a run that is still waiting
    Cancel,
    /// Back to the previous release (binaries + runtime selection; never session data)
    Rollback {
        #[arg(long)]
        to: Option<String>,
        #[arg(long = "dry-run")]
        dry_run: bool,
        #[arg(long = "max-wait")]
        max_wait_secs: Option<u64>,
        #[arg(long = "no-follow")]
        no_follow: bool,
    },
    /// Finished runs, newest first
    History {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Print `{runs}` (the full run records, as `GET /v2/upgrade/runs` shows them)
        #[arg(long)]
        json: bool,
    },
    /// Show or change upgrade settings
    Config {
        /// release | main
        #[arg(long)]
        channel: Option<String>,
        /// Release source: https URL, loopback http URL, or an absolute directory
        #[arg(long)]
        source: Option<String>,
        #[arg(long = "clear-source")]
        clear_source: bool,
        #[arg(long = "check-interval-hours")]
        check_interval_hours: Option<u32>,
        #[arg(long = "auto-stage")]
        auto_stage: Option<bool>,
        #[arg(long = "auto-apply")]
        auto_apply: Option<bool>,
        #[arg(long = "package-run")]
        package_run: Option<String>,
        #[arg(long = "package-run-batch")]
        package_run_batch: Option<usize>,
        #[arg(long = "max-wait")]
        max_wait_secs: Option<u64>,
    },
    /// Read-only self test of this binary against the live home (preflight)
    Selftest {
        #[arg(long)]
        json: bool,
    },
    /// Execute a prepared run (started by apply / rollback)
    #[command(hide = true)]
    Run {
        run_id: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum ServiceCommand {
    /// Install this build as a managed install and register the services
    Install {
        /// Install base (default: ~/.local/share/cutex, %LOCALAPPDATA%\cutex)
        #[arg(long)]
        base: Option<std::path::PathBuf>,
        /// Linux: write the units here instead of ~/.config/systemd/user (not started)
        #[arg(long = "unit-dir")]
        unit_dir: Option<std::path::PathBuf>,
        /// release | main (default: this build's channel)
        #[arg(long)]
        channel: Option<String>,
        #[arg(long = "no-start")]
        no_start: bool,
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Overwrite units / a task this command did not write
        #[arg(long)]
        force: bool,
    },
    /// Remove the units / task (releases, state and ~/.cutex stay)
    Uninstall {
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Install record, units / supervisor state
    Status,
    /// Windows: the supervisor the logon task starts (also usable on Linux for tests)
    #[command(hide = true)]
    Run,
}
