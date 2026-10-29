use std::collections::HashMap;
use std::io::{self, IsTerminal, Stdout};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use chrono::DateTime;
use chrono::Utc;
use crossterm::cursor::Show;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use cutex::agent_bus::model::{AgentBusAgent, AgentGroupUpdateMode};
use cutex::agent_management::{
    effective_presentation, AgentManagementSnapshot, AgentManagementStore, ProjectPaletteColor,
};
use cutex::config::global_settings::apply_global_config_patch;
use cutex::config::store::load_codez_config;
use cutex::config::store::load_codez_config_checked;
use cutex::config::store::save_codez_config;
use cutex::management::v2::activity::load_session_activity_states;
use cutex::management::v2::activity::SessionActivityState;
use cutex::observability::{SafeToolCallClass, SafeToolCallStatus};
use cutex::profiles::model::CodezConfig;
use cutex::runtime::alden::{cute_alden_sessions, CuteAldenSession};
use cutex::session::model::{CutexSessionQuickActionMode, CutexSessionRecord, CutexSessionStore};
use cutex::session::projection::{
    cutex_session_is_attachable, cutex_session_lifecycle_state_with_agents,
    runtime_backend_short_label, CutexSessionLifecycleState,
};
use cutex::session::service::{
    adopt_cutex_session, cutex_session_display_name, cutex_session_is_managed,
    set_cutex_session_display_name_by_key, set_cutex_session_profile_by_key,
    unmanage_cutex_session, CutexSessionAdoptOptions, CutexSessionEnsureSeed,
};
use cutex::session::service::{
    persist_cutex_session_store_and_im_record, update_cutex_session_routing_by_key,
    update_cutex_session_runtime_defaults_by_key, CutexSessionValueUpdate,
};
use cutex::session::store::load_cutex_session_store;
use cutex::ui::format::compact_home_path;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Cell, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph, Row, Table,
    TableState, Wrap,
};
use ratatui::Frame;
use super::session_tui_terminal::Terminal;
use tui_input::{Input, InputRequest};

use super::account_store::{
    load_profile_catalog_read_only, load_profile_names_read_only, ProfileCatalogEntry,
};
use super::agent_bus_runtime;
use super::profile::{activate_account, remove_profile, rename_profile, update_profile_settings};
use super::profile_settings::ProfileSettingsPatch;
use super::session_tui_actions::{
    session_tui_actions_for_record, SessionTuiAction, SessionTuiActionItem,
};
use super::session_tui_input::{self as input_policy, Command, Gate, Help, LeaveReview};
use super::session_tui_profile_settings::{
    ProfileSettingsDraft, ProfileSettingsField, ProfileSettingsSnapshot,
};
use super::session_tui_recent::{
    RecentAdoptionRequest, RecentCatalog, RecentCommand, RecentLoadState, RecentSessionsWorkspace,
};
use super::session_tui_settings::{
    GlobalSettingsDraft, GlobalSettingsField, GlobalSettingsSnapshot, SecretSettingsAction,
    SessionSettingsChoice, SessionSettingsCommand, SessionSettingsDraft, SessionSettingsEditorKind,
    SessionSettingsField, SessionSettingsSnapshot, SessionTuiSettingCategory,
    SessionTuiSettingOption,
};
use super::session_tui_view::{self as views, AgentSessionView, ListKind, Observation, SubjectRef};
use super::session_tui_workspace::{
    PrimaryPanel, PrimaryPanelOutcome, SessionTuiWorkspace, WorkspaceSelection,
};
use super::session_tui_workspace_events::{workspace_event_from_key, WorkspaceEvent};
use super::session_tui_workspace_loading::{WorkspaceLoad, WorkspaceLoadPoll};
use super::session_tui_workspace_render::{render_workspace, WorkspaceRenderer};

const WIDE_LAYOUT_MIN_WIDTH: u16 = 96;
const SETTINGS_TWO_PANE_MIN_WIDTH: u16 = 64;
const PROFILE_TWO_PANE_MIN_WIDTH: u16 = 104;
pub(super) const INSPECTOR_SPLIT_MIN_WIDTH: u16 = 115;
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const ACTIVITY_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

type CutexTerminal = Terminal<CrosstermBackend<Stdout>>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum SelectorTarget {
    Agent(String),
    RemoteAgent(String,String),
    RetiredAgent(String),
    RecentSessions,
    RetiredSessions,
    CutexProjects,
    Projects,
    Profiles,
    GlobalSettings,
}

impl SelectorTarget {
    fn workspace(&self) -> SessionTuiWorkspace {
        match self {
            Self::Agent(_) | Self::RemoteAgent(..) => SessionTuiWorkspace::Agents,
            Self::RecentSessions => SessionTuiWorkspace::RecentSessions,
            Self::RetiredAgent(_) | Self::RetiredSessions => SessionTuiWorkspace::RetiredSessions,
            Self::CutexProjects => SessionTuiWorkspace::CutexProjects,
            Self::Projects => SessionTuiWorkspace::Projects,
            Self::Profiles => SessionTuiWorkspace::Profiles,
            Self::GlobalSettings => SessionTuiWorkspace::GlobalSettings,
        }
    }

    fn agent_key(&self) -> Option<&str> {
        match self {
            Self::Agent(key) | Self::RetiredAgent(key) => Some(key),
            Self::RemoteAgent(..) | Self::RecentSessions
            | Self::RetiredSessions
            | Self::CutexProjects
            | Self::Projects
            | Self::Profiles
            | Self::GlobalSettings => None,
        }
    }

    fn is_profiles(&self) -> bool {
        matches!(self, Self::Profiles)
    }

    fn is_projects(&self) -> bool {
        matches!(self, Self::Projects)
    }

    fn is_cutex_projects(&self) -> bool {
        matches!(self, Self::CutexProjects)
    }

    #[cfg(test)]
    fn is_retired_sessions(&self) -> bool {
        matches!(self, Self::RetiredSessions)
    }

    fn is_global_settings(&self) -> bool {
        matches!(self, Self::GlobalSettings)
    }

    #[cfg(test)]
    fn is_system(&self) -> bool {
        matches!(
            self,
            Self::RecentSessions
                | Self::RetiredSessions
                | Self::CutexProjects
                | Self::Projects
                | Self::Profiles
                | Self::GlobalSettings
        )
    }

    fn uses_global_settings(&self) -> bool {
        matches!(self, Self::Profiles | Self::GlobalSettings)
    }
}

#[derive(Debug, Clone)]
struct SelectorRow {
    view: Option<AgentSessionView>,
    target: SelectorTarget,
    agent: String,
    /// Mutable native conversation title. It is presentation-only and never
    /// participates in the managed Agent's primary identity.
    thread_title: Option<String>,
    project: Option<SelectorProjectContext>,
    configured_profile: Option<String>,
    lifecycle: Option<CutexSessionLifecycleState>,
    host: String,
    backend: String,
    managed_path: String,
    retired_at: Option<String>,
    revision: u64,
    activity_session_id: Option<String>,
    activity: Option<SelectorActivity>,
    actions: Vec<SessionTuiActionItem>,
    settings: Vec<SessionTuiSettingCategory>,
    settings_snapshot: Option<SessionSettingsSnapshot>,
    global_settings_snapshot: Option<GlobalSettingsSnapshot>,
    attachable: bool,
    pinned: bool,
    managed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectorProjectContext {
    agent_name: String,
    project_id: String,
    display_name: String,
    badge_label: String,
    color: ProjectPaletteColor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectorActivityClass {
    Output,
    Command,
    Mcp,
    Tool,
    Agent,
    Edit,
    Image,
}

impl SelectorActivityClass {
    fn label(self) -> &'static str {
        match self {
            Self::Output => "OUT",
            Self::Command => "CMD",
            Self::Mcp => "MCP",
            Self::Tool => "TOOL",
            Self::Agent => "AGT",
            Self::Edit => "EDIT",
            Self::Image => "IMG",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SelectorActivity {
    class: SelectorActivityClass,
    updated_at: String,
    failed: bool,
}

impl SelectorRow {
    fn launch_profile_control_available(&self) -> bool {
        let Some(lifecycle) = self.lifecycle else {
            return false;
        };
        let Some(snapshot) = self.settings_snapshot.as_ref() else {
            return false;
        };
        snapshot.uses_codex_profiles() && !snapshot.profile_names().is_empty()
            && self.actions.iter().any(|item| {
                item.action
                    .supports_launch_profile(lifecycle, self.attachable)
            })
    }

    fn action_control_count(&self) -> usize {
        self.actions.len() + usize::from(self.launch_profile_control_available())
    }

    fn action_for_control_index(&self, index: usize) -> Option<SessionTuiAction> {
        let action_index =
            index.checked_sub(usize::from(self.launch_profile_control_available()))?;
        self.actions.get(action_index).map(|item| item.action)
    }

    fn control_index_for_action(&self, action: SessionTuiAction) -> Option<usize> {
        self.actions
            .iter()
            .position(|item| item.action == action)
            .map(|index| index + usize::from(self.launch_profile_control_available()))
    }

    fn action_supports_launch_profile(&self, action: SessionTuiAction) -> bool {
        self.settings_snapshot.as_ref().is_none_or(|snapshot| snapshot.uses_codex_profiles()) && self.lifecycle
            .is_some_and(|lifecycle| action.supports_launch_profile(lifecycle, self.attachable))
    }

    fn launch_profile_choices(
        &self,
        global_default_profile: Option<&str>,
    ) -> Vec<SessionSettingsChoice> {
        let Some(snapshot) = self.settings_snapshot.as_ref() else {
            return Vec::new();
        };
        let default = match self.session_profile_override() {
            Some(profile) => profile.to_string(),
            None => match global_default_profile {
                Some(profile) => format!("global: {profile}"),
                None => "global: not configured".to_string(),
            },
        };
        std::iter::once(SessionSettingsChoice {
            label: format!("Session default ({default})"),
            value: None,
        })
        .chain(
            snapshot
                .profile_names()
                .iter()
                .map(|name| SessionSettingsChoice {
                    label: name.clone(),
                    value: Some(name.clone()),
                }),
        )
        .collect()
    }

    fn launch_profile_detail(
        &self,
        launch_profile: Option<&str>,
        global_default_profile: Option<&str>,
    ) -> String {
        match launch_profile {
            Some(profile) => format!("{profile} (this launch only)"),
            None => format!(
                "Session default: {}",
                self.session_profile_detail(global_default_profile)
            ),
        }
    }

    fn session_profile_detail(&self, global_default_profile: Option<&str>) -> String {
        match self.session_profile_override() {
            Some(profile) => profile.to_string(),
            None => match global_default_profile {
                Some(profile) => format!("{profile} (global)"),
                None => "global default not configured".to_string(),
            },
        }
    }

    fn session_profile_override(&self) -> Option<&str> {
        self.settings_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.value(SessionSettingsField::Profile))
            .filter(|profile| *profile != "-")
    }
}

#[derive(Debug)]
struct SelectorSnapshot {
    rows: Vec<SelectorRow>,
    warning: Option<String>,
}

#[derive(Debug)]
enum RuntimeCloseWorkerResult {
    Closed(SelectorSnapshot),
    ClosedRefreshFailed(String),
    Failed(String),
}

pub(super) type SelectorEvent = WorkspaceEvent;

#[derive(Debug, Clone, PartialEq, Eq)]
enum SelectorControl {
    NewSession,
    NewAgent,
    NativeResume {
        catalog: String,
        thread: String,
        cwd: String,
    },
    ExecuteArchive(cutex::agent_management::AgentArchiveRequest),
    Continue,
    Exit,
    Selected(SessionTuiIntent),
    OpenRetiredSessions,
    OpenRecentSessions,
    ArchiveNative(String),
    CloseNative(String),
    CloseOwnedSession { native: String, owner: String },
    RevertHistory(String),
    Recent(RecentCommand),
    AdoptRecent(RecentAdoptionRequest),
    OpenProfileManager,
    OpenHosts,
    OpenServices,
    RemoteBrowse(cutex::management::connections::Connection,String),
    OpenCutexProjects,
    OpenProjects,
    ApplySettings(SessionSettingsApplyRequest),
    ApplyGlobalSettings(GlobalSettingsApplyRequest),
    ApplyProfileSettings(ProfileSettingsApplyRequest),
    ManageSession(SessionManagementRequest),
    ManageProfile(ProfileManagementRequest),
    LoginProfile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SessionTuiIntent {
    pub(super) key: String,
    pub(super) action: SessionTuiAction,
    pub(super) launch_profile: Option<String>,
    pub(super) stock_runtime: Option<super::stock_lifecycle::ReviewedStockRuntimeAction>,
    pub(super) runtime_package: Option<super::stock_lifecycle::ReviewedRuntimePackageUpgrade>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionSettingsApplyRequest {
    key: String,
    draft: SessionSettingsDraft,
    profile_names: Vec<String>,
    changed_count: usize,
}

#[derive(Debug)]
struct SessionSettingsApplyResult {
    record: CutexSessionRecord,
    profile_names: Vec<String>,
    warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GlobalSettingsApplyRequest {
    draft: GlobalSettingsDraft,
    profile_names: Vec<String>,
    changed_count: usize,
}

#[derive(Debug)]
struct GlobalSettingsApplyResult {
    config: CodezConfig,
    profile_names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProfileSettingsApplyRequest {
    profile_id: String,
    patch: ProfileSettingsPatch,
    changed_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionManagementRequest {
    key: String,
    command: SessionSettingsCommand,
    profile_names: Vec<String>,
}

#[derive(Debug)]
struct SessionManagementResult {
    record: CutexSessionRecord,
    profile_names: Vec<String>,
    warning: Option<String>,
}

#[derive(Debug)]
struct RecentAdoptionResult {
    store: CutexSessionStore,
    snapshot: Result<SelectorSnapshot, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProfileManagementCommand {
    Activate,
    Rename { new_name: String },
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProfileManagementRequest {
    profile_id: String,
    profile_name: String,
    command: ProfileManagementCommand,
}

#[derive(Debug)]
struct ProfileManagerStartup {
    notice: Option<String>,
    warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionTuiCycleOutcome {
    NewSession,
    NewAgent,
    NativeResume {
        catalog: String,
        thread: String,
        cwd: String,
    },
    Exit,
    Selected(SessionTuiIntent),
    LoginProfile,
    CutexProjects,
    Projects,
    Switch(PrimaryPanel),
}

#[derive(Debug)]
struct ProfileMutationReceipt {
    preferred_profile_id: Option<String>,
    notice: String,
}

#[derive(Debug, Clone)]
struct ProfileProjectionSnapshot {
    records: HashMap<String, CutexSessionRecord>,
    config: CodezConfig,
    profile_names: Vec<String>,
}

#[derive(Debug)]
struct ProfileManagementResult {
    profiles: Vec<ProfileCatalogEntry>,
    projection: ProfileProjectionSnapshot,
    preferred_profile_id: Option<String>,
    notice: String,
}

#[derive(Debug, Clone)]
struct PendingSettingsRefreshOverride {
    target: SelectorTarget,
    snapshot: SessionSettingsSnapshot,
    agent: Option<String>,
    configured_profile: Option<String>,
    backend: String,
    pinned: bool,
    managed: bool,
    actions: Option<Vec<SessionTuiActionItem>>,
    warning: Option<String>,
}

#[derive(Debug, Clone)]
struct PendingGlobalSettingsRefreshOverride {
    snapshot: GlobalSettingsSnapshot,
}

#[derive(Debug, Clone)]
struct PendingProfileRefreshOverride {
    projection: ProfileProjectionSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SelectorMode {
    Agents,
    RecentSessions,
    RetiredSessions {
        selected: usize,
    },
    Actions {
        agent_key: String,
        selected: usize,
        launch_profile: Option<String>,
    },
    Settings {
        target: SelectorTarget,
        category: usize,
        option: usize,
        focus: SettingsFocus,
        view: SettingsView,
    },
    ProfileManager {
        profiles: Vec<ProfileCatalogEntry>,
        selected: usize,
        focus: ProfileWorkspaceFocus,
        editor_selected: usize,
    },
    ConfirmRuntimeAction {
        agent_key: String,
        action: SessionTuiAction,
        launch_profile: Option<String>,
        confirmed: bool,
    },
    ClosingRuntime {
        agent_key: String,
        agent_name: String,
        action: SessionTuiAction,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsFocus {
    Categories,
    Options,
    Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InspectorSection {
    Overview,
    Actions,
    Settings,
}

impl InspectorSection {
    const ALL: [Self; 3] = [Self::Overview, Self::Actions, Self::Settings];

    fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Actions => "Actions",
            Self::Settings => "Settings",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileWorkspaceFocus {
    Items,
    Editor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsView {
    Expanded,
    Categories,
}

#[derive(Debug, Clone)]
enum SettingsOverlay {
    Choice {
        field: SettingsEditField,
        choices: Vec<SessionSettingsChoice>,
        selected: usize,
        custom_value: Option<String>,
    },
    Text {
        field: SettingsEditField,
        input: Input,
        tags: bool,
        masked: bool,
    },
    Groups {
        field: SettingsEditField,
        inputs: Vec<Input>,
        selected: usize,
    },
    SecretAction {
        field: SettingsEditField,
        selected: usize,
    },
    ConfirmDiscard {
        selected: usize,
    },
    ConfirmManagement {
        command: SessionSettingsCommand,
        selected: usize,
    },
}

#[derive(Debug, Clone)]
enum ActionOverlay {
    LaunchProfile {
        choices: Vec<SessionSettingsChoice>,
        selected: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileManagerAction {
    Activate,
    Rename,
    Remove,
}

impl ProfileManagerAction {
    fn label(self) -> &'static str {
        match self {
            Self::Activate => "Make active",
            Self::Rename => "Rename",
            Self::Remove => "Remove",
        }
    }
}

#[derive(Debug, Clone)]
enum ProfileOverlay {
    Actions {
        profile_id: String,
        profile_name: String,
        actions: Vec<ProfileManagerAction>,
        selected: usize,
    },
    RenameInput {
        profile_id: String,
        old_name: String,
        input: Input,
    },
    ConfirmRename {
        profile_id: String,
        old_name: String,
        new_name: String,
        selected: usize,
    },
    ConfirmRemove {
        profile_id: String,
        profile_name: String,
        selected: usize,
    },
    ConfirmAddProfile {
        selected: usize,
    },
    ConfirmDiscardProfile {
        destination: ProfileDiscardDestination,
        selected: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileDiscardDestination {
    ProfileList,
    AgentList,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsEditField {
    Session(SessionSettingsField),
    Global(GlobalSettingsField),
    Profile(ProfileSettingsField),
}

impl SettingsEditField {
    fn editor_kind(self) -> SessionSettingsEditorKind {
        match self {
            Self::Session(field) => field.editor_kind(),
            Self::Global(field) => field.editor_kind(),
            Self::Profile(field) => field.editor_kind(),
        }
    }
}

impl SettingsView {
    fn toggle(self) -> Self {
        match self {
            Self::Expanded => Self::Categories,
            Self::Categories => Self::Expanded,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Expanded => "Expanded",
            Self::Categories => "Categories",
        }
    }
}

#[derive(Debug, Clone)]
struct AppContext {
    global_snapshot: Option<GlobalSettingsSnapshot>,
    // Compatibility projections for the existing settings renderer, never
    // members of the Agent collection or its count/filter/selection.
    settings: Vec<SelectorRow>,
}

impl AppContext {
    fn extract(rows: &mut Vec<SelectorRow>) -> Self {
        let mut settings = Vec::new();
        rows.retain(|row| {
            if matches!(row.target, SelectorTarget::Agent(_) | SelectorTarget::RemoteAgent(..)) {
                true
            } else {
                settings.push(row.clone());
                false
            }
        });
        let global_snapshot = settings
            .iter()
            .find_map(|row| row.global_settings_snapshot.clone());
        Self {
            settings,
            global_snapshot,
        }
    }
}

#[derive(Debug, Clone)]
struct SelectorModel {
    details: Option<String>,
    status_scroll: views::DetailScroll,
    detail_scroll: views::DetailScroll,
    recent_detail_scroll: views::DetailScroll,
    managed_scope: usize,
    managed_table: std::cell::RefCell<TableState>,
    recent_table: std::cell::RefCell<TableState>,
    context: AppContext,
    inspector_visible: bool,
    filter_focused: bool,
    facets: super::session_tui_filters::Facets,
    sort_order: cutex::profiles::list_preferences::ListSort,
    help: Option<Help>,
    leave_review: Option<LeaveReview>,
    rows: Vec<SelectorRow>,
    remote_entries: Vec<remote_projection::Entry>,
    remote_warnings: std::collections::BTreeMap<String, String>,
    remote_worker: Option<std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Receiver<remote_projection::Reply>>>>,
    remote_due: Instant,
    profile_usage_worker: Option<super::profile_usage::ProfileUsageWorker>,
    retired_rows: Vec<SelectorRow>,
    archive_return_panel: PrimaryPanel,
    recent: RecentSessionsWorkspace,
    query: Input,
    workspace_selection: WorkspaceSelection<SelectorTarget>,
    frozen_agent_view: Option<Vec<SelectorTarget>>,
    mode: SelectorMode,
    suspended_managed_mode: Option<SelectorMode>,
    inspector_overview_focused: bool,
    recent_inspecting: bool,
    native_actions: Option<(String, usize, bool)>,
    native_archive_pending: bool,
    settings_navigation: Option<Help>,
    settings_return_panel: Option<PrimaryPanel>,
    profiles_from_settings: bool,
    confirmation_returns_to_list: bool,
    archive_confirmation: Option<cutex::agent_management::AgentArchiveRequest>,
    stock_runtime_confirmation: Option<super::stock_lifecycle::ReviewedStockRuntimeAction>,
    runtime_package_confirmation: Option<super::stock_lifecycle::ReviewedRuntimePackageUpgrade>,
    object_return: Option<(PrimaryPanel, Option<SelectorTarget>)>,
    show_thread_titles: bool,
    enhanced_keyboard: bool,
    refreshing: bool,
    warning: Option<String>,
    notice: Option<String>,
    settings_draft: SessionSettingsDraft,
    global_settings_draft: GlobalSettingsDraft,
    profile_settings_draft: ProfileSettingsDraft,
    action_overlay: Option<ActionOverlay>,
    settings_overlay: Option<SettingsOverlay>,
    profile_overlay: Option<ProfileOverlay>,
    pending_settings_refresh_override: Option<PendingSettingsRefreshOverride>,
    pending_global_settings_refresh_override: Option<PendingGlobalSettingsRefreshOverride>,
    pending_profile_refresh_override: Option<PendingProfileRefreshOverride>,
    pending_startup_warning: Option<String>,
}

impl SelectorModel {
    fn new(mut rows: Vec<SelectorRow>, refreshing: bool, enhanced_keyboard: bool) -> Self {
        debug_assert!(rows
            .iter()
            .all(|row| { SessionTuiWorkspace::PRODUCTION.contains(&row.target.workspace()) }));
        sort_rows(&mut rows);
        let context = AppContext::extract(&mut rows);
        let mut initial_facets = super::session_tui_filters::Facets::default();
        if context.global_snapshot.as_ref().is_some_and(GlobalSettingsSnapshot::default_local_host_filter) {
            let host = cutex::platform::host::current_host_name();
            initial_facets.host = Some(super::session_tui_filters::Choice {
                label: cutex::management::connections::short_display(&host), key: host,
            });
        }
        let mut recent = RecentSessionsWorkspace::default();
        recent.facets = initial_facets.clone();
        let (agent_sort, session_sort) = context.global_snapshot.as_ref().map(GlobalSettingsSnapshot::list_sort_defaults).unwrap_or_default();
        recent.sort_order = session_sort;
        let mut model = Self {
            details: None,
            status_scroll: Default::default(),
            detail_scroll: views::DetailScroll::default(),
            recent_detail_scroll: views::DetailScroll::default(),
            managed_scope: 0,
            managed_table: Default::default(),
            recent_table: Default::default(),
            context,
            inspector_visible: true,
            filter_focused: false,
            facets: initial_facets,
            sort_order: agent_sort,
            help: None,
            leave_review: None,
            rows,
            remote_entries: Vec::new(),
            remote_warnings: Default::default(),
            remote_worker:None,remote_due:Instant::now(),
            profile_usage_worker:None,
            retired_rows: Vec::new(),
            archive_return_panel: PrimaryPanel::Agents,
            recent,
            query: Input::default(),
            workspace_selection: WorkspaceSelection::default(),
            frozen_agent_view: None,
            mode: SelectorMode::Agents,
            suspended_managed_mode: None,
            inspector_overview_focused: false,
            recent_inspecting: false,
            native_actions: None,
            native_archive_pending: false,
            settings_navigation: None,
            settings_return_panel: None,
            profiles_from_settings: false,
            confirmation_returns_to_list: false,
            archive_confirmation: None,
            stock_runtime_confirmation: None,
            runtime_package_confirmation: None,
            object_return: None,
            show_thread_titles: false,
            enhanced_keyboard,
            refreshing,
            warning: None,
            notice: None,
            settings_draft: SessionSettingsDraft::default(),
            global_settings_draft: GlobalSettingsDraft::default(),
            profile_settings_draft: ProfileSettingsDraft::default(),
            action_overlay: None,
            settings_overlay: None,
            profile_overlay: None,
            pending_settings_refresh_override: None,
            pending_global_settings_refresh_override: None,
            pending_profile_refresh_override: None,
            pending_startup_warning: None,
        };
        model.ensure_selection();
        model
    }

    fn activate_primary_panel(&mut self, panel: PrimaryPanel) {
        match panel {
            PrimaryPanel::Recent if !matches!(self.mode, SelectorMode::RecentSessions) => {
                self.suspended_managed_mode = Some(self.mode.clone());
                self.mode = SelectorMode::RecentSessions;
            }
            PrimaryPanel::Agents if matches!(self.mode, SelectorMode::RecentSessions) => {
                self.recent.blur_filter();
                self.mode = self
                    .suspended_managed_mode
                    .take()
                    .unwrap_or(SelectorMode::Agents);
                self.normalize_mode_after_snapshot();
            }
            PrimaryPanel::Agents
            | PrimaryPanel::Projects
            | PrimaryPanel::Alarms
            | PrimaryPanel::Jobs
            | PrimaryPanel::Profiles
            | PrimaryPanel::Settings
            | PrimaryPanel::Recent => {}
        }
    }

    fn handle_focus_traversal(&mut self, forward: bool) -> SelectorControl {
        if self.action_overlay.is_some()
            || self.settings_overlay.is_some()
            || self.profile_overlay.is_some()
        {
            return self.handle(if forward {
                SelectorEvent::Down
            } else {
                SelectorEvent::Up
            });
        }
        match self.mode.clone() {
            SelectorMode::Agents if self.inspector_overview_focused => {
                if !forward {
                    self.inspector_overview_focused = false;
                }
                SelectorControl::Continue
            }
            SelectorMode::Agents if forward && self.selected_managed_agent().is_some() => {
                self.inspector_overview_focused = true;
                SelectorControl::Continue
            }
            SelectorMode::Agents if forward => self.handle(SelectorEvent::OpenActions),
            SelectorMode::Agents => SelectorControl::Continue,
            SelectorMode::Actions { .. } => {
                if !forward {
                    self.handle(SelectorEvent::Back)
                } else {
                    SelectorControl::Continue
                }
            }
            SelectorMode::Settings { target, category, option, focus, view: SettingsView::Categories } => {
                let focus = if target.is_global_settings() {
                    if focus == SettingsFocus::Categories { SettingsFocus::Options } else { SettingsFocus::Categories }
                } else { match (focus, forward) {
                    (SettingsFocus::Categories, true) | (SettingsFocus::Value, false) => SettingsFocus::Options,
                    (SettingsFocus::Options, true) | (SettingsFocus::Categories, false) => SettingsFocus::Value,
                    _ => SettingsFocus::Categories,
                }
                };
                self.mode = SelectorMode::Settings { target, category, option, focus, view: SettingsView::Categories };
                SelectorControl::Continue
            }
            SelectorMode::Settings { target, .. } if target.agent_key().is_some() => {
                if !forward {
                    self.handle(SelectorEvent::OpenSettings)
                } else {
                    SelectorControl::Continue
                }
            }
            SelectorMode::Settings { .. } => SelectorControl::Continue,
            SelectorMode::ProfileManager { focus, .. } => match (focus, forward) {
                (ProfileWorkspaceFocus::Items, true) => self.handle(SelectorEvent::OpenActions),
                (ProfileWorkspaceFocus::Editor, false) => self.handle(SelectorEvent::Back),
                _ => SelectorControl::Continue,
            },
            SelectorMode::RecentSessions if self.recent.review().is_some() => {
                self.handle(if self.recent.review_confirmed() {
                    SelectorEvent::Up
                } else {
                    SelectorEvent::Down
                })
            }
            SelectorMode::RecentSessions if self.recent.filter_focused() => {
                self.recent.blur_filter();
                SelectorControl::Continue
            }
            SelectorMode::RecentSessions if forward => self.handle(SelectorEvent::Activate),
            SelectorMode::RecentSessions => SelectorControl::Continue,
            SelectorMode::RetiredSessions { .. } if forward => {
                self.handle(SelectorEvent::OpenActions)
            }
            SelectorMode::RetiredSessions { .. } => self.handle(SelectorEvent::Back),
            SelectorMode::ConfirmRuntimeAction { confirmed, .. } => self.handle(if confirmed {
                SelectorEvent::Up
            } else {
                SelectorEvent::Down
            }),
            SelectorMode::ClosingRuntime { .. } => SelectorControl::Continue,
        }
    }

    fn handle_horizontal_navigation(&mut self, expand: bool) -> SelectorControl {
        // Text editors own Left/Right as cursor movement. Other overlays keep
        // the keys local and inert; none may leak into top-level navigation or
        // commit the selected operation.
        if matches!(
            self.settings_overlay.as_ref(),
            Some(SettingsOverlay::Groups { .. } | SettingsOverlay::Text { .. })
        ) || matches!(
            self.profile_overlay.as_ref(),
            Some(ProfileOverlay::RenameInput { .. })
        ) {
            return self.handle(if expand {
                SelectorEvent::OpenActions
            } else {
                SelectorEvent::Back
            });
        }
        if self.action_overlay.is_some()
            || self.settings_overlay.is_some()
            || self.profile_overlay.is_some()
        {
            return SelectorControl::Continue;
        }
        match self.mode.clone() {
            SelectorMode::Agents if self.inspector_overview_focused && !expand => {
                self.inspector_overview_focused = false;
                SelectorControl::Continue
            }
            SelectorMode::Agents => SelectorControl::Continue,
            SelectorMode::Actions { .. } if !expand => self.handle(SelectorEvent::Back),
            SelectorMode::Actions { .. } => SelectorControl::Continue,
            SelectorMode::Settings { target: SelectorTarget::GlobalSettings, focus, .. } => {
                if focus != SettingsFocus::Categories {
                    let choice = self.active_setting_option().and_then(|o| o.global_field)
                        .is_some_and(|f| matches!(SettingsEditField::Global(f).editor_kind(), SessionSettingsEditorKind::Choice));
                    if choice {
                        self.open_active_setting_editor();
                        self.handle_settings_overlay_event(if expand { SelectorEvent::Down } else { SelectorEvent::Up }, &SelectorTarget::GlobalSettings);
                        self.handle_settings_overlay_event(SelectorEvent::Activate, &SelectorTarget::GlobalSettings);
                    }
                }
                SelectorControl::Continue
            }
            SelectorMode::Settings { .. } => self.handle(if expand {
                SelectorEvent::OpenActions
            } else {
                SelectorEvent::Back
            }),
            SelectorMode::ProfileManager { focus, .. } => match (expand, focus) {
                (true, ProfileWorkspaceFocus::Items) => self.handle(SelectorEvent::OpenActions),
                (false, ProfileWorkspaceFocus::Editor) => self.handle(SelectorEvent::Back),
                _ => SelectorControl::Continue,
            },
            SelectorMode::RecentSessions if self.recent.review().is_some() => {
                self.handle(if expand {
                    SelectorEvent::Down
                } else {
                    SelectorEvent::Up
                })
            }
            SelectorMode::RecentSessions if expand => self.handle(SelectorEvent::Activate),
            SelectorMode::RecentSessions => SelectorControl::Continue,
            SelectorMode::RetiredSessions { .. } if expand => {
                self.handle(SelectorEvent::OpenActions)
            }
            SelectorMode::RetiredSessions { .. } => self.handle(SelectorEvent::Back),
            SelectorMode::ConfirmRuntimeAction { .. } => self.handle(if expand {
                SelectorEvent::OpenActions
            } else {
                SelectorEvent::Back
            }),
            SelectorMode::ClosingRuntime { .. } => SelectorControl::Continue,
        }
    }

    fn visible_indices(&self) -> Vec<usize> {
        let query = self.query.value().to_lowercase();
        let mut indices: Vec<usize> = self.rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                (matches!(row.target, SelectorTarget::Agent(_) | SelectorTarget::RemoteAgent(..))
                    && (self.managed_scope == 0
                        || (self.managed_scope == 1
                            && row.lifecycle == Some(CutexSessionLifecycleState::Online))
                        || (self.managed_scope == 2 && row.pinned))
                    && self.facets.matches(&row.host, row.project.as_ref().map(|p| p.project_id.as_str()))
                    && (query.is_empty() || selector_row_matches_query(row, &query)))
                .then_some(index)
            })
            .collect();
        use cutex::profiles::list_preferences::{ListSort, compare_projects};
        if self.sort_order != ListSort::Default {
            indices.sort_by(|a, b| {
                let left = &self.rows[*a]; let right = &self.rows[*b];
                let groups = match self.sort_order {
                    ListSort::Pinned => right.pinned.cmp(&left.pinned),
                    ListSort::Project => compare_projects(left.project.as_ref().map(|p| p.display_name.as_str()), right.project.as_ref().map(|p| p.display_name.as_str())),
                    _ => std::cmp::Ordering::Equal,
                };
                groups.then_with(|| self.sort_order.compare_names(&left.agent, &right.agent))
                    .then_with(|| left.target.agent_key().cmp(&right.target.agent_key()))
            });
        }
        if let Some(frozen) = &self.frozen_agent_view {
            let mut ordered = Vec::with_capacity(indices.len());
            for target in frozen {
                if let Some(index) = indices
                    .iter()
                    .find(|index| self.rows[**index].target == *target)
                {
                    if !ordered.contains(index) {
                        ordered.push(*index);
                    }
                }
            }
            for index in indices {
                if !ordered.contains(&index) {
                    ordered.push(index);
                }
            }
            ordered
        } else {
            indices
        }
    }

    fn freeze_agent_view(&mut self) {
        self.frozen_agent_view = Some(
            self.visible_indices()
                .into_iter()
                .map(|index| self.rows[index].target.clone())
                .collect(),
        );
    }

    fn freeze_agent_view_if_selected(&mut self) {
        if self
            .workspace_selection
            .selected()
            .is_some_and(|target| {
                matches!(target, SelectorTarget::Agent(_) | SelectorTarget::RemoteAgent(..))
            })
        {
            self.freeze_agent_view();
        }
    }

    fn clear_frozen_agent_view(&mut self) {
        self.frozen_agent_view = None;
    }

    fn visible_rows(&self) -> Vec<&SelectorRow> {
        self.visible_indices()
            .into_iter()
            .map(|index| &self.rows[index])
            .collect()
    }

    fn selected_visible_index(&self) -> Option<usize> {
        let selected_target = self.workspace_selection.selected()?;
        self.visible_indices()
            .into_iter()
            .position(|index| self.rows[index].target == *selected_target)
    }

    fn selected_row(&self) -> Option<&SelectorRow> {
        let selected_target = self.workspace_selection.selected()?;
        self.rows
            .iter()
            .chain(self.context.settings.iter())
            .find(|row| row.target == *selected_target)
    }

    fn selected_managed_agent(&self) -> Option<&SelectorRow> {
        self.selected_row()
            .filter(|row| row.managed && matches!(&row.target, SelectorTarget::Agent(_)))
    }

    fn shows_managed_inspector(&self) -> bool {
        match &self.mode {
            SelectorMode::Agents => self.inspector_visible || self.inspector_overview_focused,
            SelectorMode::Actions { .. }
            | SelectorMode::Settings { .. }
            | SelectorMode::ConfirmRuntimeAction { .. }
            | SelectorMode::ClosingRuntime { .. } => self
                .active_row()
                .is_some_and(|row| row.managed && matches!(&row.target, SelectorTarget::Agent(_))),
            SelectorMode::RecentSessions
            | SelectorMode::RetiredSessions { .. }
            | SelectorMode::ProfileManager { .. } => false,
        }
    }

    fn inspector_section(&self) -> InspectorSection {
        match &self.mode {
            SelectorMode::Actions { .. }
            | SelectorMode::ConfirmRuntimeAction { .. }
            | SelectorMode::ClosingRuntime { .. } => InspectorSection::Actions,
            SelectorMode::Settings { .. } => InspectorSection::Settings,
            SelectorMode::Agents
            | SelectorMode::RecentSessions
            | SelectorMode::RetiredSessions { .. }
            | SelectorMode::ProfileManager { .. } => InspectorSection::Overview,
        }
    }

    fn inspector_is_focused(&self) -> bool {
        !matches!(&self.mode, SelectorMode::Agents) || self.inspector_overview_focused
    }

    #[cfg(test)]
    fn selected_target(&self) -> Option<SelectorTarget> {
        self.workspace_selection.selected().cloned()
    }

    fn active_row(&self) -> Option<&SelectorRow> {
        match &self.mode {
            SelectorMode::Agents => self.selected_row(),
            SelectorMode::Actions { agent_key, .. }
            | SelectorMode::ConfirmRuntimeAction { agent_key, .. }
            | SelectorMode::ClosingRuntime { agent_key, .. } => self.row_for_action_key(agent_key),
            SelectorMode::Settings { target, .. } => self
                .rows
                .iter()
                .chain(self.context.settings.iter())
                .find(|row| row.target == *target),
            SelectorMode::ProfileManager { .. } => self
                .context
                .settings
                .iter()
                .find(|row| row.target.is_profiles()),
            SelectorMode::RecentSessions => None,
            SelectorMode::RetiredSessions { selected } => self.retired_rows.get(*selected),
        }
    }

    fn row_for_action_key(&self, agent_key: &str) -> Option<&SelectorRow> {
        self.rows
            .iter()
            .chain(self.retired_rows.iter())
            .find(|row| row.target.agent_key() == Some(agent_key))
    }

    fn selected_action_index(&self) -> Option<usize> {
        match &self.mode {
            SelectorMode::Actions { selected, .. } => Some(*selected),
            _ => None,
        }
    }

    fn selected_launch_profile(&self) -> Option<&str> {
        match &self.mode {
            SelectorMode::Actions { launch_profile, .. } => launch_profile.as_deref(),
            _ => None,
        }
    }

    fn selected_setting_category_index(&self) -> Option<usize> {
        match &self.mode {
            SelectorMode::Settings { category, .. } => Some(*category),
            _ => None,
        }
    }

    fn selected_setting_option_index(&self) -> Option<usize> {
        match &self.mode {
            SelectorMode::Settings { option, .. } => Some(*option),
            _ => None,
        }
    }

    fn settings_focus(&self) -> Option<SettingsFocus> {
        match &self.mode {
            SelectorMode::Settings { focus, .. } => Some(*focus),
            _ => None,
        }
    }

    fn settings_view(&self) -> Option<SettingsView> {
        match &self.mode {
            SelectorMode::Settings { view, .. } => Some(*view),
            _ => None,
        }
    }

    fn settings_dirty_count(&self) -> usize {
        match &self.mode {
            SelectorMode::Settings { target, .. } if target.uses_global_settings() => {
                self.global_settings_draft.dirty_count()
            }
            SelectorMode::ProfileManager { selected: 0, .. } => {
                self.global_settings_draft.dirty_count()
            }
            SelectorMode::ProfileManager { .. } => self.profile_settings_draft.dirty_count(),
            _ => self.settings_draft.dirty_count(),
        }
    }

    fn settings_are_editable(&self) -> bool {
        self.active_row().is_some_and(|row| {
            row.settings_snapshot.is_some() || row.global_settings_snapshot.is_some()
        })
    }

    fn active_setting_category(&self) -> Option<&SessionTuiSettingCategory> {
        let category = self.selected_setting_category_index()?;
        self.active_row()?.settings.get(category)
    }

    fn active_setting_option(&self) -> Option<&SessionTuiSettingOption> {
        let option = self.selected_setting_option_index()?;
        self.active_setting_category()?.options.get(option)
    }

    fn active_setting_label(&self) -> Option<&'static str> {
        if self.selected_profile_is_default() {
            return match self.selected_profile_default_option()? {
                0 => Some("Default profile"),
                1 => Some("Direct default launch"),
                _ => None,
            };
        }
        if matches!(&self.mode, SelectorMode::ProfileManager { .. }) {
            return self
                .selected_profile_setting_option()
                .map(|option| option.label);
        }
        self.active_setting_option().map(|option| option.label)
    }

    fn active_settings_snapshot(&self) -> Option<&SessionSettingsSnapshot> {
        self.active_row()?.settings_snapshot.as_ref()
    }

    fn active_global_settings_snapshot(&self) -> Option<&GlobalSettingsSnapshot> {
        self.active_row()?
            .target
            .uses_global_settings()
            .then_some(())?;
        self.context.global_snapshot.as_ref()
    }

    fn global_settings_snapshot(&self) -> Option<&GlobalSettingsSnapshot> {
        self.context.global_snapshot.as_ref()
    }

    fn selected_profile(&self) -> Option<&ProfileCatalogEntry> {
        let SelectorMode::ProfileManager {
            profiles, selected, ..
        } = &self.mode
        else {
            return None;
        };
        selected
            .checked_sub(1)
            .and_then(|index| profiles.get(index))
    }

    fn selected_profile_index(&self) -> Option<usize> {
        match &self.mode {
            SelectorMode::ProfileManager { selected, .. } => Some(*selected),
            _ => None,
        }
    }

    fn selected_profile_is_add(&self) -> bool {
        matches!(
            &self.mode,
            SelectorMode::ProfileManager {
                profiles,
                selected,
                ..
            } if *selected == profiles.len().saturating_add(1)
        )
    }

    fn selected_profile_is_default(&self) -> bool {
        matches!(&self.mode, SelectorMode::ProfileManager { selected: 0, .. })
    }

    fn profile_workspace_focus(&self) -> Option<ProfileWorkspaceFocus> {
        match &self.mode {
            SelectorMode::ProfileManager { focus, .. } => Some(*focus),
            _ => None,
        }
    }

    fn selected_profile_default_option(&self) -> Option<usize> {
        match &self.mode {
            SelectorMode::ProfileManager {
                editor_selected, ..
            } if self.selected_profile_is_default() => Some(*editor_selected),
            _ => None,
        }
    }

    fn selected_profile_settings_snapshot(&self) -> Option<ProfileSettingsSnapshot> {
        self.selected_profile()
            .map(ProfileSettingsSnapshot::from_catalog_entry)
    }

    fn selected_profile_setting_categories(&self) -> Vec<SessionTuiSettingCategory> {
        self.selected_profile_settings_snapshot()
            .map(|snapshot| snapshot.categories(&self.profile_settings_draft))
            .unwrap_or_default()
    }

    fn selected_profile_setting_option(&self) -> Option<SessionTuiSettingOption> {
        let flat_index = match &self.mode {
            SelectorMode::ProfileManager {
                editor_selected, ..
            } => *editor_selected,
            _ => return None,
        };
        self.selected_profile_setting_categories()
            .into_iter()
            .flat_map(|category| category.options)
            .nth(flat_index)
    }

    fn profile_editor_option_count(&self) -> usize {
        self.selected_profile_setting_categories()
            .iter()
            .map(|category| category.options.len())
            .sum()
    }

    fn profile_default_value(&self, field: GlobalSettingsField) -> Option<String> {
        let snapshot = self.global_settings_snapshot()?;
        Some(self.global_settings_draft.value(snapshot, field))
    }

    fn current_default_profile_name(&self) -> Option<String> {
        let snapshot = self.global_settings_snapshot()?;
        if !self
            .global_settings_draft
            .field_is_dirty(GlobalSettingsField::DefaultProfile)
        {
            return snapshot.default_profile_name().map(str::to_string);
        }
        self.profile_default_value(GlobalSettingsField::DefaultProfile)
            .filter(|value| value != "-")
    }

    fn handle(&mut self, event: SelectorEvent) -> SelectorControl {
        if matches!(&self.mode, SelectorMode::Actions { .. }) && self.action_overlay.is_some() {
            return self.handle_action_overlay_event(event);
        }
        if matches!(&self.mode, SelectorMode::ProfileManager { .. })
            && self.settings_overlay.is_some()
        {
            return self.handle_settings_overlay_event(event, &SelectorTarget::Profiles);
        }
        if matches!(&self.mode, SelectorMode::ProfileManager { .. })
            && self.profile_overlay.is_some()
        {
            return self.handle_profile_overlay_event(event);
        }
        match self.mode.clone() {
            SelectorMode::Agents => self.handle_agent_event(event),
            SelectorMode::RecentSessions => self.handle_recent_sessions_event(event),
            SelectorMode::RetiredSessions { selected } => {
                self.handle_retired_sessions_event(event, selected)
            }
            SelectorMode::Actions {
                agent_key,
                selected,
                launch_profile,
            } => self.handle_action_event(event, agent_key, selected, launch_profile),
            SelectorMode::Settings {
                target,
                category,
                option,
                focus,
                view,
            } => self.handle_settings_event(event, target, category, option, focus, view),
            SelectorMode::ProfileManager {
                profiles,
                selected,
                focus,
                editor_selected,
            } => {
                self.handle_profile_manager_event(event, profiles, selected, focus, editor_selected)
            }
            SelectorMode::ConfirmRuntimeAction {
                agent_key,
                action,
                launch_profile,
                confirmed,
            } => {
                self.handle_confirmation_event(event, agent_key, action, launch_profile, confirmed)
            }
            SelectorMode::ClosingRuntime { .. } => SelectorControl::Continue,
        }
    }

    fn handle_agent_event(&mut self, event: SelectorEvent) -> SelectorControl {
        if self.inspector_overview_focused {
            match event {
                SelectorEvent::OpenActions => {
                    self.inspector_overview_focused = false;
                    self.open_action_menu();
                }
                SelectorEvent::OpenSettings => {
                    self.inspector_overview_focused = false;
                    self.open_settings();
                }
                SelectorEvent::Back | SelectorEvent::Escape => {
                    self.inspector_overview_focused = false;
                }
                SelectorEvent::Exit => return SelectorControl::Exit,
                SelectorEvent::Up
                | SelectorEvent::Down
                | SelectorEvent::First
                | SelectorEvent::Last
                | SelectorEvent::Insert(_)
                | SelectorEvent::Backspace
                | SelectorEvent::Delete
                | SelectorEvent::ClearInput
                | SelectorEvent::Activate => {}
            }
            return SelectorControl::Continue;
        }
        match event {
            SelectorEvent::Up => self.move_selection(-1),
            SelectorEvent::Down => self.move_selection(1),
            SelectorEvent::First => self.select_edge(false),
            SelectorEvent::Last => self.select_edge(true),
            SelectorEvent::Insert(character) => {
                self.query.handle(InputRequest::InsertChar(character));
                self.ensure_selection();
            }
            SelectorEvent::Backspace => {
                self.query.handle(InputRequest::DeletePrevChar);
                self.ensure_selection();
            }
            SelectorEvent::Delete => {
                self.query.handle(InputRequest::DeleteNextChar);
                self.ensure_selection();
            }
            SelectorEvent::ClearInput => {
                self.query.handle(InputRequest::DeleteLine);
                self.ensure_selection();
            }
            SelectorEvent::Escape if !self.query.value().is_empty() => {
                self.query.reset();
                self.ensure_selection();
            }
            SelectorEvent::OpenActions
                if self
                    .workspace_selection
                    .is_selected(&SelectorTarget::RecentSessions) =>
            {
                return SelectorControl::OpenRecentSessions;
            }
            SelectorEvent::Activate
                if self
                    .workspace_selection
                    .is_selected(&SelectorTarget::RecentSessions) =>
            {
                return SelectorControl::OpenRecentSessions;
            }
            SelectorEvent::OpenSettings
                if self
                    .workspace_selection
                    .is_selected(&SelectorTarget::RecentSessions) =>
            {
                return SelectorControl::OpenRecentSessions;
            }
            SelectorEvent::OpenActions
                if self
                    .workspace_selection
                    .is_selected(&SelectorTarget::RetiredSessions) =>
            {
                return SelectorControl::OpenRetiredSessions;
            }
            SelectorEvent::Activate
                if self
                    .workspace_selection
                    .is_selected(&SelectorTarget::RetiredSessions) =>
            {
                return SelectorControl::OpenRetiredSessions;
            }
            SelectorEvent::OpenSettings
                if self
                    .workspace_selection
                    .is_selected(&SelectorTarget::RetiredSessions) =>
            {
                return SelectorControl::OpenRetiredSessions;
            }
            SelectorEvent::OpenActions
                if self
                    .workspace_selection
                    .is_selected(&SelectorTarget::Profiles) =>
            {
                return SelectorControl::OpenProfileManager;
            }
            SelectorEvent::OpenActions | SelectorEvent::OpenSettings | SelectorEvent::Activate
                if self
                    .workspace_selection
                    .is_selected(&SelectorTarget::CutexProjects) =>
            {
                return SelectorControl::OpenCutexProjects;
            }
            SelectorEvent::OpenActions | SelectorEvent::OpenSettings | SelectorEvent::Activate
                if self
                    .workspace_selection
                    .is_selected(&SelectorTarget::Projects) =>
            {
                return SelectorControl::OpenProjects;
            }
            SelectorEvent::OpenActions => self.open_action_menu(),
            SelectorEvent::OpenSettings
                if self
                    .workspace_selection
                    .is_selected(&SelectorTarget::Profiles) =>
            {
                return SelectorControl::OpenProfileManager;
            }
            SelectorEvent::OpenSettings => self.open_settings(),
            SelectorEvent::Activate => return self.activate_primary_action(),
            SelectorEvent::Back => {}
            SelectorEvent::Escape | SelectorEvent::Exit => return SelectorControl::Exit,
        }
        SelectorControl::Continue
    }

    fn handle_recent_sessions_event(&mut self, event: SelectorEvent) -> SelectorControl {
        if self.recent.filter_focused() {
            match event {
                SelectorEvent::Insert(character) => self.recent.push_filter(character),
                SelectorEvent::Backspace => self.recent.pop_filter(),
                SelectorEvent::Delete => self.recent.edit_filter(InputRequest::DeleteNextChar),
                SelectorEvent::ClearInput => self.recent.clear_filter(),
                SelectorEvent::First => self.recent.edit_filter(InputRequest::GoToStart),
                SelectorEvent::Last => self.recent.edit_filter(InputRequest::GoToEnd),
                SelectorEvent::Activate | SelectorEvent::Escape => self.recent.blur_filter(),
                SelectorEvent::Exit => return SelectorControl::Exit,
                _ => {}
            }
            return SelectorControl::Continue;
        }
        if self.recent.review().is_some() {
            if matches!(event, SelectorEvent::Activate | SelectorEvent::Escape)
                && self.recent.blur_adoption_name()
            {
                return SelectorControl::Continue;
            }
            match event {
                SelectorEvent::Up | SelectorEvent::First | SelectorEvent::Back => {
                    self.recent.set_review_confirmed(false)
                }
                SelectorEvent::Down | SelectorEvent::Last | SelectorEvent::OpenActions => {
                    self.recent.set_review_confirmed(true)
                }
                SelectorEvent::Activate if self.recent.review_confirmed() => {
                    if let Some(request) = self.recent.adoption_request() {
                        self.recent.set_review_confirmed(false);
                        return SelectorControl::AdoptRecent(request);
                    }
                    self.warning = Some("Enter an explicit formal Agent name before confirming adoption and import.".into());
                    self.recent.focus_adoption_name();
                }
                SelectorEvent::Activate | SelectorEvent::Escape => self.recent.cancel_review(),
                SelectorEvent::Exit => return SelectorControl::Exit,
                SelectorEvent::Insert(_)
                | SelectorEvent::Backspace
                | SelectorEvent::Delete
                | SelectorEvent::ClearInput
                | SelectorEvent::OpenSettings => {}
            }
            return SelectorControl::Continue;
        }
        match event {
            SelectorEvent::Up => self.recent.move_selection(-1),
            SelectorEvent::Down => self.recent.move_selection(1),
            SelectorEvent::First => self.recent.select_edge(false),
            SelectorEvent::Last => self.recent.select_edge(true),
            SelectorEvent::Activate => {
                let retry = matches!(
                    self.recent.load_state(),
                    RecentLoadState::Failed(_) | RecentLoadState::ProviderIncompatible(_)
                );
                if retry && !self.recent.loading() {
                    return SelectorControl::Recent(RecentCommand::Retry);
                }
                if let Some(row) = self
                    .recent
                    .visible_rows()
                    .get(self.recent.selected_visible())
                    .copied()
                    .cloned()
                {
                    match row.view.subject {
                        super::session_tui_view::SubjectRef::Managed(id) => {
                            return self.open_subject_context(
                                &id,
                                SelectorEvent::Activate,
                                PrimaryPanel::Recent,
                            )
                        }
                        super::session_tui_view::SubjectRef::Native { catalog, thread }
                            if row.state
                                == super::session_tui_recent::RecentThreadState::Unmanaged =>
                        {
                            if let Some(cwd) = row.cwd {
                                return SelectorControl::NativeResume {
                                    catalog,
                                    thread,
                                    cwd,
                                };
                            }
                        }
                        _ => {
                            self.warning = Some(
                                "Native resume unavailable: refresh identity and cwd observations"
                                    .into(),
                            )
                        }
                    }
                }
            }
            SelectorEvent::Insert('n' | 'N')
                if self.recent.next_cursor().is_some() && !self.recent.loading() =>
            {
                return SelectorControl::Recent(RecentCommand::LoadMore);
            }
            SelectorEvent::Insert('/') => self.recent.focus_filter(),
            SelectorEvent::OpenActions => {
                if let Some(row) = self
                    .recent
                    .visible_rows()
                    .get(self.recent.selected_visible())
                    .copied()
                    .cloned()
                {
                    if let super::session_tui_view::SubjectRef::Managed(id) = row.view.subject {
                        return self.open_subject_context(
                            &id,
                            SelectorEvent::OpenActions,
                            PrimaryPanel::Recent,
                        );
                    }
                    self.recent_inspecting = false;
                    self.native_actions = Some((row.thread_id, 0, false));
                }
            }
            SelectorEvent::Back | SelectorEvent::Escape | SelectorEvent::OpenSettings => {
                self.activate_primary_panel(PrimaryPanel::Agents);
                self.ensure_selection();
            }
            SelectorEvent::Exit => return SelectorControl::Exit,
            SelectorEvent::Insert(_)
            | SelectorEvent::Backspace
            | SelectorEvent::Delete
            | SelectorEvent::ClearInput => {}
        }
        SelectorControl::Continue
    }

    fn native_is_claude(&self,native:&str)->bool {self.recent.visible_rows().into_iter().any(|r|r.thread_id==native && r.source=="cutex-claude-session")}

    fn native_owned_id(&self, native: &str) -> Option<String> {
        self.recent.visible_rows().into_iter().find(|row| row.thread_id == native)
            .and_then(|row| row.owned_runtime_id.clone())
    }

    fn recent_catalog_reply(&mut self, reply: super::session_tui_recent::CatalogReply) {
        if let super::session_tui_recent::CatalogReply::ClosedOwned { id, result } = &reply {
            self.native_archive_pending = false;
            match result {
                Ok(()) => {
                    if let Ok(store) = load_cutex_session_store() { self.recent.reproject(&store); }
                    self.notice = Some(format!("Session runtime closed: {id}. History retained; Enter resumes."));
                }
                Err(error) => self.warning = Some(error.clone()),
            }
            return;
        }
        if let super::session_tui_recent::CatalogReply::Archived { id, result } = &reply {
            self.native_archive_pending = false;
            match result {
                Ok(()) => {
                    self.recent.remove_archived(id);
                    self.notice = Some(format!("Closed and archived. Restore history: cutex session restore-native {id}"));
                }
                Err(error) => self.warning = Some(error.clone()),
            }
            return;
        }
        match load_cutex_session_store() {
            Ok(store) => {
                let managed_names = managed_names_by_native_session_id();
                self.recent
                    .receive_with_managed_names(reply, &store, &managed_names);
                let mut archived: Vec<_> = store
                    .sessions
                    .iter()
                    .filter(|(_, r)| r.is_retired() && cutex_session_is_managed(r))
                    .map(|(key, record)| {
                        let mut row = retired_selector_row(key, record, None);
                        let mut view = selector_view(&row, None);
                        view.native_thread = record.codex_session_id.clone();
                        view.runtime =
                            Observation::Unavailable("archived runtime not observed".into());
                        row.view = Some(view);
                        row
                    })
                    .collect();
                enrich_provider_views(&mut archived, &store);
                self.recent.enrich_views(
                    &archived
                        .iter()
                        .filter_map(|r| r.view.clone())
                        .collect::<Vec<_>>(),
                );
                self.recent.enrich_views(
                    &self
                        .rows
                        .iter()
                        .filter_map(|r| r.view.clone())
                        .collect::<Vec<_>>(),
                );
            }
            Err(error) => {
                self.recent.reconciliation_failed(
                    reply,
                    format!("recent session reconciliation unavailable: {error:#}"),
                );
            }
        }
    }

    fn recent_loading_started(&mut self) {
        self.recent.mark_loading();
    }

    fn recent_adoption_succeeded(
        &mut self,
        request: &RecentAdoptionRequest,
        result: RecentAdoptionResult,
    ) {
        let selected = self.workspace_selection.selected().cloned();
        self.recent.adoption_succeeded(&result.store);
        self.workspace_selection.select(selected.clone());
        self.ensure_selection();
        self.notice = Some(format!("Adopted and imported Agent {}; unassigned. Use Projects Create/Add for explicit assignment.", request.formal_name));
        match result.snapshot {
            Ok(snapshot) => {
                self.rows = snapshot.rows;
        remote_projection::apply(self);
                self.context = AppContext::extract(&mut self.rows);
                self.workspace_selection.select(selected);
                self.ensure_selection();
                self.warning = snapshot.warning;
            }
            Err(error) => {
                self.warning = Some(format!(
                    "Native thread was adopted, but agent refresh failed: {error}"
                ));
            }
        }
    }

    fn recent_adoption_failed(&mut self, message: String) {
        self.warning = Some(format!("recent session adoption failed: {message}"));
    }

    fn handle_retired_sessions_event(
        &mut self,
        event: SelectorEvent,
        selected: usize,
    ) -> SelectorControl {
        if self.retired_rows.is_empty() {
            match event {
                SelectorEvent::Back | SelectorEvent::Escape => self.leave_archive(),
                SelectorEvent::Exit => return SelectorControl::Exit,
                _ => {}
            }
            return SelectorControl::Continue;
        }
        let mut next = selected.min(self.retired_rows.len() - 1);
        match event {
            SelectorEvent::Up => next = wrapped_index(next, -1, self.retired_rows.len()),
            SelectorEvent::Down => next = wrapped_index(next, 1, self.retired_rows.len()),
            SelectorEvent::First => next = 0,
            SelectorEvent::Last => next = self.retired_rows.len() - 1,
            SelectorEvent::Activate | SelectorEvent::OpenActions => {
                let row = &self.retired_rows[next];
                if row.actions.is_empty() {
                    self.notice = Some(
                        "Permanently retired identity; history is read-only and cannot be restored"
                            .into(),
                    );
                    return SelectorControl::Continue;
                }
                let Some(key) = row.target.agent_key() else {
                    return SelectorControl::Continue;
                };
                self.mode = SelectorMode::ConfirmRuntimeAction {
                    agent_key: key.to_string(),
                    action: SessionTuiAction::RestoreSession,
                    launch_profile: None,
                    confirmed: false,
                };
                return SelectorControl::Continue;
            }
            SelectorEvent::Back | SelectorEvent::Escape | SelectorEvent::OpenSettings => {
                self.leave_archive();
                return SelectorControl::Continue;
            }
            SelectorEvent::Exit => return SelectorControl::Exit,
            SelectorEvent::Insert(_)
            | SelectorEvent::Backspace
            | SelectorEvent::Delete
            | SelectorEvent::ClearInput => {}
        }
        self.mode = SelectorMode::RetiredSessions { selected: next };
        SelectorControl::Continue
    }

    fn leave_archive(&mut self) {
        self.mode = SelectorMode::Agents;
        self.suspended_managed_mode = None;
        self.activate_primary_panel(self.archive_return_panel);
    }

    fn open_retired_sessions(&mut self, rows: Vec<SelectorRow>) {
        let already_open = matches!(self.mode, SelectorMode::RetiredSessions { .. } | SelectorMode::ConfirmRuntimeAction { action: SessionTuiAction::RestoreSession, .. });
        if !already_open {
            self.archive_return_panel = if matches!(self.mode, SelectorMode::RecentSessions) { PrimaryPanel::Recent } else { PrimaryPanel::Agents };
            self.mode = SelectorMode::RetiredSessions { selected: 0 };
        }
        self.notice = None;
        self.warning = None;
        self.replace_retired_rows(rows);
    }

    fn replace_retired_rows(&mut self, rows: Vec<SelectorRow>) {
        let target = self.active_row().map(|r| r.target.clone());
        let stale = if let SelectorMode::ConfirmRuntimeAction { agent_key, action: SessionTuiAction::RestoreSession, .. } = &self.mode {
            let old = self.retired_rows.iter().find(|r| r.target.agent_key() == Some(agent_key));
            let new = rows.iter().find(|r| r.target.agent_key() == Some(agent_key));
            old.zip(new).is_none_or(|(a,b)| a.revision != b.revision || a.agent != b.agent || a.lifecycle != b.lifecycle)
        } else { false };
        let selected = target.and_then(|target| rows.iter().position(|r| r.target == target)).unwrap_or(0);
        self.retired_rows = rows;
        if stale || matches!(self.mode, SelectorMode::RetiredSessions { .. }) {
            self.mode = SelectorMode::RetiredSessions { selected };
        }
        if stale {
            self.archive_confirmation = None;
            self.notice = Some("Archived target changed; review it again".into());
        }
    }

    fn handle_action_event(
        &mut self,
        event: SelectorEvent,
        agent_key: String,
        selected: usize,
        launch_profile: Option<String>,
    ) -> SelectorControl {
        let global_default_profile = self.current_default_profile_name();
        let Some((control_count, has_profile_control, profile_choices)) = self
            .rows
            .iter()
            .find(|row| row.target.agent_key() == Some(agent_key.as_str()))
            .map(|row| {
                (
                    row.action_control_count(),
                    row.launch_profile_control_available(),
                    row.launch_profile_choices(global_default_profile.as_deref()),
                )
            })
        else {
            self.mode = SelectorMode::Agents;
            return SelectorControl::Continue;
        };
        if control_count == 0 {
            self.mode = SelectorMode::Agents;
            return SelectorControl::Continue;
        }
        let mut next_selected = selected.min(control_count - 1);
        match event {
            SelectorEvent::Up => next_selected = wrapped_index(next_selected, -1, control_count),
            SelectorEvent::Down => next_selected = wrapped_index(next_selected, 1, control_count),
            SelectorEvent::First => next_selected = 0,
            SelectorEvent::Last => next_selected = control_count - 1,
            SelectorEvent::Activate if has_profile_control && next_selected == 0 => {
                let overlay_selected = launch_profile
                    .as_ref()
                    .and_then(|profile| {
                        profile_choices
                            .iter()
                            .position(|choice| choice.value.as_deref() == Some(profile.as_str()))
                    })
                    .unwrap_or(0);
                self.mode = SelectorMode::Actions {
                    agent_key,
                    selected: next_selected,
                    launch_profile,
                };
                self.action_overlay = Some(ActionOverlay::LaunchProfile {
                    choices: profile_choices,
                    selected: overlay_selected,
                });
                return SelectorControl::Continue;
            }
            SelectorEvent::Activate => {
                let action = self
                    .rows
                    .iter()
                    .find(|row| row.target.agent_key() == Some(agent_key.as_str()))
                    .and_then(|row| row.action_for_control_index(next_selected));
                if let Some(action) = action {
                    return self.select_action(agent_key, action, launch_profile);
                }
            }
            SelectorEvent::Back | SelectorEvent::Escape => {
                self.action_overlay = None;
                self.mode = SelectorMode::Agents;
                return SelectorControl::Continue;
            }
            SelectorEvent::Exit => return SelectorControl::Exit,
            SelectorEvent::OpenActions
            | SelectorEvent::OpenSettings
            | SelectorEvent::Insert(_)
            | SelectorEvent::Backspace
            | SelectorEvent::Delete
            | SelectorEvent::ClearInput => {}
        }
        self.mode = SelectorMode::Actions {
            agent_key,
            selected: next_selected,
            launch_profile,
        };
        SelectorControl::Continue
    }

    fn handle_action_overlay_event(&mut self, event: SelectorEvent) -> SelectorControl {
        let Some(ActionOverlay::LaunchProfile {
            choices,
            mut selected,
        }) = self.action_overlay.clone()
        else {
            return SelectorControl::Continue;
        };
        if choices.is_empty() {
            self.action_overlay = None;
            return SelectorControl::Continue;
        }
        match event {
            SelectorEvent::Up => selected = wrapped_index(selected, -1, choices.len()),
            SelectorEvent::Down => selected = wrapped_index(selected, 1, choices.len()),
            SelectorEvent::First => selected = 0,
            SelectorEvent::Last => selected = choices.len() - 1,
            SelectorEvent::Activate => {
                if let SelectorMode::Actions { launch_profile, .. } = &mut self.mode {
                    *launch_profile = choices[selected].value.clone();
                }
                self.action_overlay = None;
                self.warning = None;
                return SelectorControl::Continue;
            }
            SelectorEvent::Back
            | SelectorEvent::Escape
            | SelectorEvent::OpenActions
            | SelectorEvent::OpenSettings => {
                self.action_overlay = None;
                return SelectorControl::Continue;
            }
            SelectorEvent::Exit => return SelectorControl::Exit,
            SelectorEvent::Insert(_)
            | SelectorEvent::Backspace
            | SelectorEvent::Delete
            | SelectorEvent::ClearInput => {}
        }
        self.action_overlay = Some(ActionOverlay::LaunchProfile { choices, selected });
        SelectorControl::Continue
    }

    fn handle_settings_event(
        &mut self,
        event: SelectorEvent,
        target: SelectorTarget,
        category: usize,
        option: usize,
        focus: SettingsFocus,
        view: SettingsView,
    ) -> SelectorControl {
        if self.settings_overlay.is_some() {
            return self.handle_settings_overlay_event(event, &target);
        }

        if event == SelectorEvent::Activate
            && target == SelectorTarget::GlobalSettings
            && focus == SettingsFocus::Categories && category == 0
        {
            return match selector_command(self, Command::Profiles) {
                SelectorKeyRoute::Control(control) => control.unwrap_or(SelectorControl::Continue),
                _ => SelectorControl::Continue,
            };
        }
        if event == SelectorEvent::Activate && focus != SettingsFocus::Categories {
            if let Some(command) = self.active_setting_option().and_then(|option| option.navigation) {
                return match selector_command(self, command) {
                    SelectorKeyRoute::Control(control) => control.unwrap_or(SelectorControl::Continue),
                    _ => SelectorControl::Continue,
                };
            }
        }

        if matches!(event, SelectorEvent::Insert('a' | 'A' | 's' | 'S')) {
            if let Some(key) = target.agent_key() {
                let changed_count = self.settings_draft.dirty_count();
                if changed_count == 0 {
                    return SelectorControl::Continue;
                }
                return SelectorControl::ApplySettings(SessionSettingsApplyRequest {
                    key: key.to_string(),
                    draft: self.settings_draft.clone(),
                    profile_names: self
                        .active_settings_snapshot()
                        .map(|snapshot| snapshot.profile_names().to_vec())
                        .unwrap_or_default(),
                    changed_count,
                });
            }
            let changed_count = self.global_settings_draft.dirty_count();
            if changed_count == 0 {
                return SelectorControl::Continue;
            }
            return SelectorControl::ApplyGlobalSettings(GlobalSettingsApplyRequest {
                draft: self.global_settings_draft.clone(),
                profile_names: self
                    .active_global_settings_snapshot()
                    .map(|snapshot| snapshot.profile_names().to_vec())
                    .unwrap_or_default(),
                changed_count,
            });
        }
        if matches!(event, SelectorEvent::Insert('d' | 'D')) {
            if self.settings_draft.is_dirty() || self.global_settings_draft.is_dirty() {
                self.discard_settings_draft(&target);
                self.notice = Some("Draft discarded".to_string());
            }
            return SelectorControl::Continue;
        }

        let Some(settings) = self
            .rows
            .iter()
            .chain(self.context.settings.iter())
            .find(|row| row.target == target)
            .map(|row| row.settings.as_slice())
        else {
            self.mode = SelectorMode::Agents;
            return SelectorControl::Continue;
        };
        if settings.is_empty() {
            self.mode = SelectorMode::Agents;
            return SelectorControl::Continue;
        }

        let mut next_category = category.min(settings.len() - 1);
        let mut next_option = option.min(settings[next_category].options.len().saturating_sub(1));
        let mut next_focus = focus;
        if settings[next_category].options.is_empty() {
            let Some((category, option)) = setting_indices_at_flat_index(settings, 0) else {
                self.mode = SelectorMode::Agents;
                return SelectorControl::Continue;
            };
            next_category = category;
            next_option = option;
        }

        let mut open_editor = false;
        if matches!(event, SelectorEvent::Insert('v' | 'V')) && !target.is_global_settings() {
            self.mode = SelectorMode::Settings {
                target,
                category: next_category,
                option: next_option,
                focus: SettingsFocus::Options,
                view: view.toggle(),
            };
            return SelectorControl::Continue;
        }

        match view {
            SettingsView::Expanded => match event {
                SelectorEvent::Up | SelectorEvent::Down => {
                    let direction = if event == SelectorEvent::Up { -1 } else { 1 };
                    if let Some((category, option)) =
                        moved_flat_setting(settings, next_category, next_option, direction)
                    {
                        next_category = category;
                        next_option = option;
                    }
                }
                SelectorEvent::First | SelectorEvent::Last => {
                    let flat_index = if event == SelectorEvent::First {
                        0
                    } else {
                        setting_option_count(settings) - 1
                    };
                    if let Some((category, option)) =
                        setting_indices_at_flat_index(settings, flat_index)
                    {
                        next_category = category;
                        next_option = option;
                    }
                }
                SelectorEvent::Activate => open_editor = true,
                SelectorEvent::Back | SelectorEvent::OpenSettings | SelectorEvent::Escape => {
                    self.request_leave_settings();
                    return SelectorControl::Continue;
                }
                SelectorEvent::Exit => return SelectorControl::Exit,
                SelectorEvent::OpenActions
                | SelectorEvent::Insert(_)
                | SelectorEvent::Backspace
                | SelectorEvent::Delete
                | SelectorEvent::ClearInput => {}
            },
            SettingsView::Categories => match event {
                SelectorEvent::Up | SelectorEvent::Down => {
                    let direction = if event == SelectorEvent::Up { -1 } else { 1 };
                    match next_focus {
                        SettingsFocus::Categories => {
                            next_category = wrapped_index(next_category, direction, settings.len());
                            next_option = 0;
                        }
                        SettingsFocus::Options => {
                            next_option = wrapped_index(
                                next_option,
                                direction,
                                settings[next_category].options.len(),
                            );
                        }
                        SettingsFocus::Value => {}
                    }
                }
                SelectorEvent::First => match next_focus {
                    SettingsFocus::Categories => {
                        next_category = 0;
                        next_option = 0;
                    }
                    SettingsFocus::Options => next_option = 0,
                    SettingsFocus::Value => {}
                },
                SelectorEvent::Last => match next_focus {
                    SettingsFocus::Categories => {
                        next_category = settings.len() - 1;
                        next_option = 0;
                    }
                    SettingsFocus::Options => {
                        next_option = settings[next_category].options.len().saturating_sub(1);
                    }
                    SettingsFocus::Value => {}
                },
                SelectorEvent::OpenActions | SelectorEvent::Activate => match next_focus {
                    SettingsFocus::Categories => next_focus = SettingsFocus::Options,
                    SettingsFocus::Options if target.is_global_settings() => open_editor = event == SelectorEvent::Activate,
                    SettingsFocus::Options => next_focus = SettingsFocus::Value,
                    SettingsFocus::Value => {
                        if event == SelectorEvent::Activate {
                            open_editor = true;
                        }
                    }
                },
                SelectorEvent::Back => match next_focus {
                    SettingsFocus::Value => next_focus = SettingsFocus::Options,
                    SettingsFocus::Options => next_focus = SettingsFocus::Categories,
                    SettingsFocus::Categories => {
                        self.request_leave_settings();
                        return SelectorControl::Continue;
                    }
                },
                SelectorEvent::OpenSettings | SelectorEvent::Escape => {
                    self.request_leave_settings();
                    return SelectorControl::Continue;
                }
                SelectorEvent::Exit => return SelectorControl::Exit,
                SelectorEvent::Insert(_)
                | SelectorEvent::Backspace
                | SelectorEvent::Delete
                | SelectorEvent::ClearInput => {}
            },
        }
        self.mode = SelectorMode::Settings {
            target,
            category: next_category,
            option: next_option,
            focus: next_focus,
            view,
        };
        if open_editor {
            self.open_active_setting_editor();
        }
        SelectorControl::Continue
    }

    fn handle_profile_manager_event(
        &mut self,
        event: SelectorEvent,
        profiles: Vec<ProfileCatalogEntry>,
        selected: usize,
        focus: ProfileWorkspaceFocus,
        editor_selected: usize,
    ) -> SelectorControl {
        if matches!(event, SelectorEvent::Insert('a' | 'A' | 's' | 'S')) {
            if selected == 0 {
                let changed_count = self.global_settings_draft.dirty_count();
                if changed_count == 0 {
                    return SelectorControl::Continue;
                }
                return SelectorControl::ApplyGlobalSettings(GlobalSettingsApplyRequest {
                    draft: self.global_settings_draft.clone(),
                    profile_names: self
                        .active_global_settings_snapshot()
                        .map(|snapshot| snapshot.profile_names().to_vec())
                        .unwrap_or_default(),
                    changed_count,
                });
            }
            let Some(profile_id) = self.selected_profile().map(|profile| profile.id.clone()) else {
                return SelectorControl::Continue;
            };
            let Some(snapshot) = self.selected_profile_settings_snapshot() else {
                return SelectorControl::Continue;
            };
            let changed_count = self.profile_settings_draft.dirty_count();
            if changed_count == 0 {
                return SelectorControl::Continue;
            }
            let patch = match self.profile_settings_draft.patch(&snapshot) {
                Ok(patch) => patch,
                Err(error) => {
                    self.warning = Some(format!("profile settings edit failed: {error:#}"));
                    self.notice = None;
                    return SelectorControl::Continue;
                }
            };
            return SelectorControl::ApplyProfileSettings(ProfileSettingsApplyRequest {
                profile_id,
                patch,
                changed_count,
            });
        }
        if matches!(event, SelectorEvent::Insert('d' | 'D')) {
            let dirty = if selected == 0 {
                self.global_settings_draft.is_dirty()
            } else {
                self.profile_settings_draft.is_dirty()
            };
            if dirty {
                self.discard_active_profile_workspace_draft();
                self.notice = Some("Draft discarded".to_string());
            }
            return SelectorControl::Continue;
        }

        let item_count = profiles.len().saturating_add(2);
        let mut next_selected = selected.min(item_count.saturating_sub(1));
        let mut next_focus = focus;
        let mut next_editor_selected = editor_selected;
        let mut open_profile_actions = false;
        let mut open_default_editor = false;
        let mut open_profile_editor = false;
        let mut open_add = false;

        match focus {
            ProfileWorkspaceFocus::Items => match event {
                SelectorEvent::Up => next_selected = wrapped_index(next_selected, -1, item_count),
                SelectorEvent::Down => next_selected = wrapped_index(next_selected, 1, item_count),
                SelectorEvent::First => next_selected = 0,
                SelectorEvent::Last => next_selected = item_count.saturating_sub(1),
                SelectorEvent::Activate | SelectorEvent::OpenActions => {
                    if next_selected == item_count.saturating_sub(1) {
                        open_add = true;
                    } else {
                        next_focus = ProfileWorkspaceFocus::Editor;
                    }
                }
                SelectorEvent::Back | SelectorEvent::OpenSettings | SelectorEvent::Escape => {
                    self.request_leave_profile_workspace();
                    return SelectorControl::Continue;
                }
                SelectorEvent::Exit => return SelectorControl::Exit,
                SelectorEvent::Insert(_)
                | SelectorEvent::Backspace
                | SelectorEvent::Delete
                | SelectorEvent::ClearInput => {}
            },
            ProfileWorkspaceFocus::Editor => match event {
                SelectorEvent::Up if selected == 0 => {
                    next_editor_selected = wrapped_index(next_editor_selected, -1, 2)
                }
                SelectorEvent::Down if selected == 0 => {
                    next_editor_selected = wrapped_index(next_editor_selected, 1, 2)
                }
                SelectorEvent::First if selected == 0 => next_editor_selected = 0,
                SelectorEvent::Last if selected == 0 => next_editor_selected = 1,
                SelectorEvent::Activate if selected == 0 => open_default_editor = true,
                SelectorEvent::Up | SelectorEvent::Down
                    if selected < item_count.saturating_sub(1) =>
                {
                    let count = self.profile_editor_option_count();
                    let direction = if event == SelectorEvent::Up { -1 } else { 1 };
                    next_editor_selected = wrapped_index(next_editor_selected, direction, count);
                }
                SelectorEvent::First if selected < item_count.saturating_sub(1) => {
                    next_editor_selected = 0;
                }
                SelectorEvent::Last if selected < item_count.saturating_sub(1) => {
                    next_editor_selected = self.profile_editor_option_count().saturating_sub(1);
                }
                SelectorEvent::Activate if selected < item_count.saturating_sub(1) => {
                    open_profile_editor = true;
                }
                SelectorEvent::Activate | SelectorEvent::OpenActions
                    if selected == item_count.saturating_sub(1) =>
                {
                    open_add = true;
                }
                SelectorEvent::OpenActions => open_profile_actions = true,
                SelectorEvent::Back => {
                    self.request_leave_profile_editor(ProfileDiscardDestination::ProfileList);
                    return SelectorControl::Continue;
                }
                SelectorEvent::OpenSettings | SelectorEvent::Escape => {
                    self.request_leave_profile_workspace();
                    return SelectorControl::Continue;
                }
                SelectorEvent::Exit => return SelectorControl::Exit,
                SelectorEvent::Activate => {}
                SelectorEvent::Up
                | SelectorEvent::Down
                | SelectorEvent::First
                | SelectorEvent::Last
                | SelectorEvent::Insert(_)
                | SelectorEvent::Backspace
                | SelectorEvent::Delete
                | SelectorEvent::ClearInput => {}
            },
        }

        if next_selected != selected {
            self.profile_settings_draft = ProfileSettingsDraft::default();
            self.global_settings_draft = GlobalSettingsDraft::default();
            next_editor_selected = 0;
        }

        self.mode = SelectorMode::ProfileManager {
            profiles,
            selected: next_selected,
            focus: next_focus,
            editor_selected: next_editor_selected,
        };
        if open_default_editor {
            self.open_profile_default_editor();
        } else if open_profile_editor {
            self.open_profile_setting_editor();
        } else if open_profile_actions {
            self.open_profile_actions();
        } else if open_add {
            self.profile_overlay = Some(ProfileOverlay::ConfirmAddProfile { selected: 0 });
        }
        SelectorControl::Continue
    }

    fn open_profile_manager(&mut self, profiles: Vec<ProfileCatalogEntry>) {
        self.global_settings_draft = GlobalSettingsDraft::default();
        self.profile_settings_draft = ProfileSettingsDraft::default();
        self.settings_overlay = None;
        self.profile_overlay = None;
        self.notice = None;
        if self
            .warning
            .as_deref()
            .is_some_and(|warning| warning.starts_with("profile catalog unavailable:"))
        {
            self.warning = None;
        }
        self.mode = SelectorMode::ProfileManager {
            profiles,
            selected: 0,
            focus: ProfileWorkspaceFocus::Items,
            editor_selected: 0,
        };
    }

    fn profile_usage_refreshed(&mut self, reply: super::profile_usage::ProfileUsageReply) {
        let SelectorMode::ProfileManager { profiles, .. } = &mut self.mode else {
            return;
        };
        let refreshed_at = Utc::now().timestamp();
        for profile in profiles {
            let Some(result) = reply.get(&profile.name) else {
                continue;
            };
            profile.usage_refreshed_at = Some(refreshed_at);
            profile.official_usage = Some(match result {
                Ok(meters) => {
                    profile.usage_error = None;
                    super::profile_usage::ProfileUsageSnapshot::Meters(meters.clone())
                }
                Err(error) => {
                    profile.usage_error = Some(error.clone());
                    super::profile_usage::ProfileUsageSnapshot::Unavailable(format!(
                        "unavailable · {error}"
                    ))
                }
            });
        }
    }

    fn request_leave_profile_workspace(&mut self) {
        if self.global_settings_draft.is_dirty() || self.profile_settings_draft.is_dirty() {
            self.profile_overlay = Some(ProfileOverlay::ConfirmDiscardProfile {
                destination: ProfileDiscardDestination::AgentList,
                selected: 0,
            });
        } else {
            self.leave_settings();
        }
    }

    fn request_leave_profile_editor(&mut self, destination: ProfileDiscardDestination) {
        let dirty = if self.selected_profile_is_default() {
            self.global_settings_draft.is_dirty()
        } else {
            self.profile_settings_draft.is_dirty()
        };
        if dirty {
            self.profile_overlay = Some(ProfileOverlay::ConfirmDiscardProfile {
                destination,
                selected: 0,
            });
            return;
        }
        self.finish_leaving_profile_editor(destination);
    }

    fn finish_leaving_profile_editor(&mut self, destination: ProfileDiscardDestination) {
        match destination {
            ProfileDiscardDestination::ProfileList => {
                if let SelectorMode::ProfileManager { focus, .. } = &mut self.mode {
                    *focus = ProfileWorkspaceFocus::Items;
                }
            }
            ProfileDiscardDestination::AgentList => self.leave_settings(),
        }
    }

    fn discard_active_profile_workspace_draft(&mut self) {
        if self.selected_profile_is_default() {
            self.global_settings_draft = GlobalSettingsDraft::default();
        } else {
            self.profile_settings_draft = ProfileSettingsDraft::default();
        }
        self.settings_overlay = None;
        self.profile_overlay = None;
        if self.warning.as_deref().is_some_and(|warning| {
            warning.starts_with("profile settings edit failed:")
                || warning.starts_with("profile settings apply failed:")
                || warning.starts_with("Apply or discard staged profile settings")
        }) {
            self.warning = None;
        }
    }

    fn open_profile_default_editor(&mut self) {
        let Some(field) = self.selected_profile_default_option().map(|selected| {
            if selected == 0 {
                GlobalSettingsField::DefaultProfile
            } else {
                GlobalSettingsField::DefaultProfileDirectLaunch
            }
        }) else {
            return;
        };
        let Some(snapshot) = self.active_global_settings_snapshot() else {
            return;
        };
        let choices = snapshot.choices(field);
        let current = self.global_settings_draft.value(snapshot, field);
        let selected = choices.iter().position(|choice| {
            choice.value.as_deref() == (current != "-").then_some(current.as_str())
        });
        self.settings_overlay = Some(SettingsOverlay::Choice {
            field: SettingsEditField::Global(field),
            choices,
            selected: selected.unwrap_or(0),
            custom_value: selected.is_none().then_some(current),
        });
    }

    fn open_profile_setting_editor(&mut self) {
        let Some(option) = self.selected_profile_setting_option() else {
            return;
        };
        let Some(field) = option.profile_field else {
            self.notice = Some("Read-only profile information".to_string());
            return;
        };
        let Some(snapshot) = self.selected_profile_settings_snapshot() else {
            return;
        };
        self.settings_overlay = Some(match field.editor_kind() {
            SessionSettingsEditorKind::Choice => {
                let choices = self.profile_settings_draft.choices(&snapshot, field);
                let current = snapshot.editor_value(&self.profile_settings_draft, field);
                let selected = choices
                    .iter()
                    .position(|choice| choice.value.as_deref() == Some(current.as_str()));
                SettingsOverlay::Choice {
                    field: SettingsEditField::Profile(field),
                    choices,
                    selected: selected.unwrap_or(0),
                    custom_value: selected.is_none().then_some(current),
                }
            }
            SessionSettingsEditorKind::Text | SessionSettingsEditorKind::Tags => {
                let value = snapshot.editor_value(&self.profile_settings_draft, field);
                SettingsOverlay::Text {
                    field: SettingsEditField::Profile(field),
                    input: Input::new(value),
                    tags: false,
                    masked: false,
                }
            }
            SessionSettingsEditorKind::Secret => SettingsOverlay::SecretAction {
                field: SettingsEditField::Profile(field),
                selected: 0,
            },
        });
    }

    fn open_profile_actions(&mut self) {
        if self.selected_profile_is_default() {
            return;
        }
        if self.selected_profile_is_add() {
            self.profile_overlay = Some(ProfileOverlay::ConfirmAddProfile { selected: 0 });
            return;
        }
        if self.profile_settings_draft.is_dirty() {
            self.warning =
                Some("Apply or discard staged profile settings before actions".to_string());
            return;
        }
        let Some(profile) = self.selected_profile().cloned() else {
            self.warning = Some("No profile selected".to_string());
            return;
        };
        let mut actions = Vec::with_capacity(3);
        if !profile.active {
            actions.push(ProfileManagerAction::Activate);
        }
        actions.extend([ProfileManagerAction::Rename, ProfileManagerAction::Remove]);
        self.profile_overlay = Some(ProfileOverlay::Actions {
            profile_id: profile.id,
            profile_name: profile.name,
            actions,
            selected: 0,
        });
        if self.warning.as_deref().is_some_and(|warning| {
            warning.starts_with("profile change failed:")
                || warning.starts_with("profile settings apply failed:")
                || warning.starts_with("profile settings edit failed:")
        }) {
            self.warning = None;
        }
    }

    fn profile_manager_failed(&mut self, message: String) {
        self.warning = Some(format!("profile catalog unavailable: {message}"));
        self.notice = None;
    }

    fn handle_profile_overlay_event(&mut self, event: SelectorEvent) -> SelectorControl {
        let Some(overlay) = self.profile_overlay.clone() else {
            return SelectorControl::Continue;
        };
        match overlay {
            ProfileOverlay::Actions {
                profile_id,
                profile_name,
                actions,
                mut selected,
            } => {
                match event {
                    SelectorEvent::Up => selected = wrapped_index(selected, -1, actions.len()),
                    SelectorEvent::Down => selected = wrapped_index(selected, 1, actions.len()),
                    SelectorEvent::First => selected = 0,
                    SelectorEvent::Last => selected = actions.len().saturating_sub(1),
                    SelectorEvent::Activate => {
                        let Some(action) = actions.get(selected).copied() else {
                            self.profile_overlay = None;
                            return SelectorControl::Continue;
                        };
                        match action {
                            ProfileManagerAction::Activate => {
                                return SelectorControl::ManageProfile(ProfileManagementRequest {
                                    profile_id,
                                    profile_name,
                                    command: ProfileManagementCommand::Activate,
                                });
                            }
                            ProfileManagerAction::Rename => {
                                self.profile_overlay = Some(ProfileOverlay::RenameInput {
                                    profile_id,
                                    old_name: profile_name.clone(),
                                    input: Input::new(profile_name),
                                });
                            }
                            ProfileManagerAction::Remove => {
                                self.profile_overlay = Some(ProfileOverlay::ConfirmRemove {
                                    profile_id,
                                    profile_name,
                                    selected: 0,
                                });
                            }
                        }
                        return SelectorControl::Continue;
                    }
                    SelectorEvent::Back | SelectorEvent::Escape | SelectorEvent::OpenSettings => {
                        self.profile_overlay = None;
                        return SelectorControl::Continue;
                    }
                    SelectorEvent::Exit => return SelectorControl::Exit,
                    SelectorEvent::OpenActions
                    | SelectorEvent::Insert(_)
                    | SelectorEvent::Backspace
                    | SelectorEvent::Delete
                    | SelectorEvent::ClearInput => {}
                }
                self.profile_overlay = Some(ProfileOverlay::Actions {
                    profile_id,
                    profile_name,
                    actions,
                    selected,
                });
            }
            ProfileOverlay::RenameInput {
                profile_id,
                old_name,
                mut input,
            } => {
                match event {
                    SelectorEvent::Insert(character) => {
                        input.handle(InputRequest::InsertChar(character));
                    }
                    SelectorEvent::Backspace => {
                        input.handle(InputRequest::DeletePrevChar);
                    }
                    SelectorEvent::Delete => {
                        input.handle(InputRequest::DeleteNextChar);
                    }
                    SelectorEvent::ClearInput => {
                        input.handle(InputRequest::DeleteLine);
                    }
                    SelectorEvent::Back => {
                        input.handle(InputRequest::GoToPrevChar);
                    }
                    SelectorEvent::OpenActions => {
                        input.handle(InputRequest::GoToNextChar);
                    }
                    SelectorEvent::First => {
                        input.handle(InputRequest::GoToStart);
                    }
                    SelectorEvent::Last => {
                        input.handle(InputRequest::GoToEnd);
                    }
                    SelectorEvent::Activate => {
                        let new_name = input.value().trim().to_string();
                        if new_name.is_empty() {
                            self.warning = Some("Profile name cannot be empty".to_string());
                        } else if new_name == old_name {
                            self.warning = Some("Enter a different profile name".to_string());
                        } else {
                            self.warning = None;
                            self.profile_overlay = Some(ProfileOverlay::ConfirmRename {
                                profile_id,
                                old_name,
                                new_name,
                                selected: 0,
                            });
                            return SelectorControl::Continue;
                        }
                    }
                    SelectorEvent::Escape | SelectorEvent::OpenSettings => {
                        self.profile_overlay = None;
                        return SelectorControl::Continue;
                    }
                    SelectorEvent::Exit => return SelectorControl::Exit,
                    SelectorEvent::Up | SelectorEvent::Down => {}
                }
                self.profile_overlay = Some(ProfileOverlay::RenameInput {
                    profile_id,
                    old_name,
                    input,
                });
            }
            ProfileOverlay::ConfirmRename {
                profile_id,
                old_name,
                new_name,
                mut selected,
            } => {
                match event {
                    SelectorEvent::Back | SelectorEvent::Up | SelectorEvent::First => selected = 0,
                    SelectorEvent::OpenActions | SelectorEvent::Down | SelectorEvent::Last => {
                        selected = 1;
                    }
                    SelectorEvent::Activate if selected == 1 => {
                        return SelectorControl::ManageProfile(ProfileManagementRequest {
                            profile_id,
                            profile_name: old_name,
                            command: ProfileManagementCommand::Rename { new_name },
                        });
                    }
                    SelectorEvent::Activate
                    | SelectorEvent::Escape
                    | SelectorEvent::OpenSettings => {
                        self.profile_overlay = None;
                        return SelectorControl::Continue;
                    }
                    SelectorEvent::Exit => return SelectorControl::Exit,
                    SelectorEvent::Insert(_)
                    | SelectorEvent::Backspace
                    | SelectorEvent::Delete
                    | SelectorEvent::ClearInput => {}
                }
                self.profile_overlay = Some(ProfileOverlay::ConfirmRename {
                    profile_id,
                    old_name,
                    new_name,
                    selected,
                });
            }
            ProfileOverlay::ConfirmRemove {
                profile_id,
                profile_name,
                mut selected,
            } => {
                match event {
                    SelectorEvent::Back | SelectorEvent::Up | SelectorEvent::First => selected = 0,
                    SelectorEvent::OpenActions | SelectorEvent::Down | SelectorEvent::Last => {
                        selected = 1;
                    }
                    SelectorEvent::Activate if selected == 1 => {
                        return SelectorControl::ManageProfile(ProfileManagementRequest {
                            profile_id,
                            profile_name,
                            command: ProfileManagementCommand::Remove,
                        });
                    }
                    SelectorEvent::Activate
                    | SelectorEvent::Escape
                    | SelectorEvent::OpenSettings => {
                        self.profile_overlay = None;
                        return SelectorControl::Continue;
                    }
                    SelectorEvent::Exit => return SelectorControl::Exit,
                    SelectorEvent::Insert(_)
                    | SelectorEvent::Backspace
                    | SelectorEvent::Delete
                    | SelectorEvent::ClearInput => {}
                }
                self.profile_overlay = Some(ProfileOverlay::ConfirmRemove {
                    profile_id,
                    profile_name,
                    selected,
                });
            }
            ProfileOverlay::ConfirmAddProfile { mut selected } => {
                match event {
                    SelectorEvent::Back | SelectorEvent::Up | SelectorEvent::First => selected = 0,
                    SelectorEvent::OpenActions | SelectorEvent::Down | SelectorEvent::Last => {
                        selected = 1;
                    }
                    SelectorEvent::Activate if selected == 1 => {
                        return SelectorControl::LoginProfile;
                    }
                    SelectorEvent::Activate
                    | SelectorEvent::Escape
                    | SelectorEvent::OpenSettings => {
                        self.profile_overlay = None;
                        return SelectorControl::Continue;
                    }
                    SelectorEvent::Exit => return SelectorControl::Exit,
                    SelectorEvent::Insert(_)
                    | SelectorEvent::Backspace
                    | SelectorEvent::Delete
                    | SelectorEvent::ClearInput => {}
                }
                self.profile_overlay = Some(ProfileOverlay::ConfirmAddProfile { selected });
            }
            ProfileOverlay::ConfirmDiscardProfile {
                destination,
                mut selected,
            } => {
                match event {
                    SelectorEvent::Back | SelectorEvent::Up | SelectorEvent::First => selected = 0,
                    SelectorEvent::OpenActions | SelectorEvent::Down | SelectorEvent::Last => {
                        selected = 1;
                    }
                    SelectorEvent::Activate if selected == 1 => {
                        self.discard_active_profile_workspace_draft();
                        self.finish_leaving_profile_editor(destination);
                        return SelectorControl::Continue;
                    }
                    SelectorEvent::Activate
                    | SelectorEvent::Escape
                    | SelectorEvent::OpenSettings => {
                        self.profile_overlay = None;
                        return SelectorControl::Continue;
                    }
                    SelectorEvent::Exit => return SelectorControl::Exit,
                    SelectorEvent::Insert(_)
                    | SelectorEvent::Backspace
                    | SelectorEvent::Delete
                    | SelectorEvent::ClearInput => {}
                }
                self.profile_overlay = Some(ProfileOverlay::ConfirmDiscardProfile {
                    destination,
                    selected,
                });
            }
        }
        SelectorControl::Continue
    }

    fn handle_settings_overlay_event(
        &mut self,
        event: SelectorEvent,
        target: &SelectorTarget,
    ) -> SelectorControl {
        let Some(overlay) = self.settings_overlay.clone() else {
            return SelectorControl::Continue;
        };
        match overlay {
            SettingsOverlay::Choice {
                field,
                choices,
                mut selected,
                custom_value,
            } => {
                let custom_offset = usize::from(custom_value.is_some());
                let choice_count = choices.len() + custom_offset;
                match event {
                    SelectorEvent::Up => selected = wrapped_index(selected, -1, choice_count),
                    SelectorEvent::Down => selected = wrapped_index(selected, 1, choice_count),
                    SelectorEvent::First => selected = 0,
                    SelectorEvent::Last => selected = choice_count.saturating_sub(1),
                    SelectorEvent::Activate => {
                        let value = if selected < custom_offset {
                            custom_value.clone()
                        } else {
                            choices
                                .get(selected - custom_offset)
                                .and_then(|choice| choice.value.clone())
                        };
                        if !self.stage_setting(target, field, value) {
                            self.settings_overlay = Some(SettingsOverlay::Choice {
                                field,
                                choices,
                                selected,
                                custom_value,
                            });
                        }
                        return SelectorControl::Continue;
                    }
                    SelectorEvent::Back | SelectorEvent::Escape | SelectorEvent::OpenSettings => {
                        self.settings_overlay = None;
                        return SelectorControl::Continue;
                    }
                    SelectorEvent::Exit => return SelectorControl::Exit,
                    SelectorEvent::OpenActions
                    | SelectorEvent::Insert(_)
                    | SelectorEvent::Backspace
                    | SelectorEvent::Delete
                    | SelectorEvent::ClearInput => {}
                }
                self.settings_overlay = Some(SettingsOverlay::Choice {
                    field,
                    choices,
                    selected,
                    custom_value,
                });
            }
            SettingsOverlay::Groups {
                field,
                mut inputs,
                mut selected,
            } => {
                if inputs.is_empty() {
                    inputs.push(Input::default());
                }
                selected = selected.min(inputs.len().saturating_sub(1));
                match event {
                    SelectorEvent::Insert(character) => {
                        inputs[selected].handle(InputRequest::InsertChar(character));
                        ensure_group_editor_trailing_input(&mut inputs);
                    }
                    SelectorEvent::Backspace => {
                        inputs[selected].handle(InputRequest::DeletePrevChar);
                    }
                    SelectorEvent::Delete => {
                        inputs[selected].handle(InputRequest::DeleteNextChar);
                    }
                    SelectorEvent::ClearInput => {
                        inputs[selected].handle(InputRequest::DeleteLine);
                    }
                    SelectorEvent::Back => {
                        inputs[selected].handle(InputRequest::GoToPrevChar);
                    }
                    SelectorEvent::OpenActions => {
                        inputs[selected].handle(InputRequest::GoToNextChar);
                    }
                    SelectorEvent::First => {
                        inputs[selected].handle(InputRequest::GoToStart);
                    }
                    SelectorEvent::Last => {
                        inputs[selected].handle(InputRequest::GoToEnd);
                    }
                    SelectorEvent::Up => {
                        selected = selected.saturating_sub(1);
                    }
                    SelectorEvent::Down => {
                        selected = (selected + 1).min(inputs.len().saturating_sub(1));
                    }
                    SelectorEvent::Activate => {
                        let staged =
                            self.stage_setting(target, field, Some(group_editor_value(&inputs)));
                        if !staged {
                            self.settings_overlay = Some(SettingsOverlay::Groups {
                                field,
                                inputs,
                                selected,
                            });
                        }
                        return SelectorControl::Continue;
                    }
                    SelectorEvent::Escape | SelectorEvent::OpenSettings => {
                        self.settings_overlay = None;
                        return SelectorControl::Continue;
                    }
                    SelectorEvent::Exit => return SelectorControl::Exit,
                }
                self.settings_overlay = Some(SettingsOverlay::Groups {
                    field,
                    inputs,
                    selected,
                });
            }
            SettingsOverlay::Text {
                field,
                mut input,
                tags,
                masked,
            } => {
                match event {
                    SelectorEvent::Insert(character) => {
                        input.handle(InputRequest::InsertChar(character));
                    }
                    SelectorEvent::Backspace => {
                        input.handle(InputRequest::DeletePrevChar);
                    }
                    SelectorEvent::Delete => {
                        input.handle(InputRequest::DeleteNextChar);
                    }
                    SelectorEvent::ClearInput => {
                        input.handle(InputRequest::DeleteLine);
                    }
                    SelectorEvent::Back => {
                        input.handle(InputRequest::GoToPrevChar);
                    }
                    SelectorEvent::OpenActions => {
                        input.handle(InputRequest::GoToNextChar);
                    }
                    SelectorEvent::First => {
                        input.handle(InputRequest::GoToStart);
                    }
                    SelectorEvent::Last => {
                        input.handle(InputRequest::GoToEnd);
                    }
                    SelectorEvent::Activate => {
                        let staged = if masked {
                            self.stage_secret_setting(
                                target,
                                field,
                                SecretSettingsAction::Replace(input.value().to_string()),
                            )
                        } else {
                            let raw_value = input.value();
                            let value = if tags {
                                Some(raw_value.trim().to_string())
                            } else {
                                let value = raw_value.trim();
                                (!value.is_empty() && value != "-").then(|| value.to_string())
                            };
                            self.stage_setting(target, field, value)
                        };
                        if !staged {
                            self.settings_overlay = Some(SettingsOverlay::Text {
                                field,
                                input,
                                tags,
                                masked,
                            });
                        }
                        return SelectorControl::Continue;
                    }
                    SelectorEvent::Escape | SelectorEvent::OpenSettings => {
                        self.settings_overlay = None;
                        return SelectorControl::Continue;
                    }
                    SelectorEvent::Exit => return SelectorControl::Exit,
                    SelectorEvent::Up | SelectorEvent::Down => {}
                }
                self.settings_overlay = Some(SettingsOverlay::Text {
                    field,
                    input,
                    tags,
                    masked,
                });
            }
            SettingsOverlay::SecretAction {
                field,
                mut selected,
            } => {
                match event {
                    SelectorEvent::Up => selected = wrapped_index(selected, -1, 3),
                    SelectorEvent::Down => selected = wrapped_index(selected, 1, 3),
                    SelectorEvent::First => selected = 0,
                    SelectorEvent::Last => selected = 2,
                    SelectorEvent::Activate if selected == 1 => {
                        self.settings_overlay = Some(SettingsOverlay::Text {
                            field,
                            input: Input::default(),
                            tags: false,
                            masked: true,
                        });
                        return SelectorControl::Continue;
                    }
                    SelectorEvent::Activate => {
                        let action = if selected == 0 {
                            SecretSettingsAction::Keep
                        } else {
                            SecretSettingsAction::Clear
                        };
                        if self.stage_secret_setting(target, field, action) {
                            self.settings_overlay = None;
                        } else {
                            self.settings_overlay =
                                Some(SettingsOverlay::SecretAction { field, selected });
                        }
                        return SelectorControl::Continue;
                    }
                    SelectorEvent::Back | SelectorEvent::Escape | SelectorEvent::OpenSettings => {
                        self.settings_overlay = None;
                        return SelectorControl::Continue;
                    }
                    SelectorEvent::Exit => return SelectorControl::Exit,
                    SelectorEvent::OpenActions
                    | SelectorEvent::Insert(_)
                    | SelectorEvent::Backspace
                    | SelectorEvent::Delete
                    | SelectorEvent::ClearInput => {}
                }
                self.settings_overlay = Some(SettingsOverlay::SecretAction { field, selected });
            }
            SettingsOverlay::ConfirmDiscard { mut selected } => {
                match event {
                    SelectorEvent::Back | SelectorEvent::Up | SelectorEvent::First => selected = 0,
                    SelectorEvent::OpenActions | SelectorEvent::Down | SelectorEvent::Last => {
                        selected = 1;
                    }
                    SelectorEvent::Activate if selected == 1 => {
                        self.discard_settings_draft(target);
                        self.leave_settings();
                        return SelectorControl::Continue;
                    }
                    SelectorEvent::Activate
                    | SelectorEvent::Escape
                    | SelectorEvent::OpenSettings => {
                        self.settings_overlay = None;
                        return SelectorControl::Continue;
                    }
                    SelectorEvent::Exit => return SelectorControl::Exit,
                    SelectorEvent::Insert(_)
                    | SelectorEvent::Backspace
                    | SelectorEvent::Delete
                    | SelectorEvent::ClearInput => {}
                }
                self.settings_overlay = Some(SettingsOverlay::ConfirmDiscard { selected });
            }
            SettingsOverlay::ConfirmManagement {
                command,
                mut selected,
            } => {
                match event {
                    SelectorEvent::Back | SelectorEvent::Up | SelectorEvent::First => selected = 0,
                    SelectorEvent::OpenActions | SelectorEvent::Down | SelectorEvent::Last => {
                        selected = 1;
                    }
                    SelectorEvent::Activate if selected == 1 => {
                        let Some(key) = target.agent_key() else {
                            self.settings_overlay = None;
                            return SelectorControl::Continue;
                        };
                        return SelectorControl::ManageSession(SessionManagementRequest {
                            key: key.to_string(),
                            command,
                            profile_names: self
                                .active_settings_snapshot()
                                .map(|snapshot| snapshot.profile_names().to_vec())
                                .unwrap_or_default(),
                        });
                    }
                    SelectorEvent::Activate
                    | SelectorEvent::Escape
                    | SelectorEvent::OpenSettings => {
                        self.settings_overlay = None;
                        return SelectorControl::Continue;
                    }
                    SelectorEvent::Exit => return SelectorControl::Exit,
                    SelectorEvent::Insert(_)
                    | SelectorEvent::Backspace
                    | SelectorEvent::Delete
                    | SelectorEvent::ClearInput => {}
                }
                self.settings_overlay =
                    Some(SettingsOverlay::ConfirmManagement { command, selected });
            }
        }
        SelectorControl::Continue
    }

    fn open_active_setting_editor(&mut self) {
        if let Some(command) = self
            .active_setting_option()
            .and_then(|option| option.command)
        {
            if self.settings_draft.is_dirty() {
                self.warning =
                    Some("Apply or discard staged settings before changing management".to_string());
                return;
            }
            self.settings_overlay = Some(SettingsOverlay::ConfirmManagement {
                command,
                selected: 0,
            });
            return;
        }
        let Some(field) = self.active_setting_option().and_then(|option| {
            option
                .field
                .map(SettingsEditField::Session)
                .or_else(|| option.global_field.map(SettingsEditField::Global))
                .or_else(|| option.profile_field.map(SettingsEditField::Profile))
        }) else {
            return;
        };
        self.settings_overlay = Some(match field.editor_kind() {
            SessionSettingsEditorKind::Choice => {
                let (choices, current) = match field {
                    SettingsEditField::Session(field) => {
                        let Some(snapshot) = self.active_settings_snapshot() else {
                            return;
                        };
                        (
                            snapshot.choices(field),
                            self.settings_draft
                                .value(snapshot, field)
                                .map(str::to_string),
                        )
                    }
                    SettingsEditField::Global(field) => {
                        let Some(snapshot) = self.active_global_settings_snapshot() else {
                            return;
                        };
                        let current = self.global_settings_draft.value(snapshot, field);
                        (snapshot.choices(field), (current != "-" && !current.is_empty()).then_some(current))
                    }
                    SettingsEditField::Profile(field) => {
                        let Some(snapshot) = self.selected_profile_settings_snapshot() else {
                            return;
                        };
                        let current = snapshot.editor_value(&self.profile_settings_draft, field);
                        (
                            self.profile_settings_draft.choices(&snapshot, field),
                            Some(current),
                        )
                    }
                };
                let selected = choices
                    .iter()
                    .position(|choice| choice.value.as_deref() == current.as_deref());
                if matches!(
                    field,
                    SettingsEditField::Session(SessionSettingsField::Profile)
                ) && choices.is_empty()
                    && current.is_none()
                {
                    if self.warning.is_none() {
                        self.warning = Some("No configured profiles available".to_string());
                    }
                    return;
                }
                SettingsOverlay::Choice {
                    field,
                    choices,
                    selected: selected.unwrap_or(0),
                    custom_value: selected.is_none().then_some(current).flatten(),
                }
            }
            SessionSettingsEditorKind::Text => SettingsOverlay::Text {
                field,
                input: Input::new(match field {
                    SettingsEditField::Session(field) => {
                        let Some(snapshot) = self.active_settings_snapshot() else {
                            return;
                        };
                        self.settings_draft
                            .value(snapshot, field)
                            .unwrap_or_default()
                            .to_string()
                    }
                    SettingsEditField::Global(field) => {
                        let Some(snapshot) = self.active_global_settings_snapshot() else {
                            return;
                        };
                        self.global_settings_draft.value(snapshot, field)
                    }
                    SettingsEditField::Profile(field) => {
                        let Some(snapshot) = self.selected_profile_settings_snapshot() else {
                            return;
                        };
                        snapshot.editor_value(&self.profile_settings_draft, field)
                    }
                }),
                tags: false,
                masked: false,
            },
            SessionSettingsEditorKind::Tags => {
                let value = match field {
                    SettingsEditField::Session(field) => {
                        let Some(snapshot) = self.active_settings_snapshot() else {
                            return;
                        };
                        self.settings_draft
                            .value(snapshot, field)
                            .unwrap_or_default()
                            .to_string()
                    }
                    SettingsEditField::Global(field) => {
                        let Some(snapshot) = self.active_global_settings_snapshot() else {
                            return;
                        };
                        self.global_settings_draft.value(snapshot, field)
                    }
                    SettingsEditField::Profile(field) => {
                        let Some(snapshot) = self.selected_profile_settings_snapshot() else {
                            return;
                        };
                        snapshot.editor_value(&self.profile_settings_draft, field)
                    }
                };
                if matches!(
                    field,
                    SettingsEditField::Session(SessionSettingsField::AgentGroups)
                ) {
                    SettingsOverlay::Groups {
                        field,
                        inputs: group_editor_inputs(&value),
                        selected: 0,
                    }
                } else {
                    SettingsOverlay::Text {
                        field,
                        input: Input::new(value),
                        tags: true,
                        masked: false,
                    }
                }
            }
            SessionSettingsEditorKind::Secret => {
                SettingsOverlay::SecretAction { field, selected: 0 }
            }
        });
    }

    fn stage_setting(
        &mut self,
        target: &SelectorTarget,
        field: SettingsEditField,
        value: Option<String>,
    ) -> bool {
        let result = match field {
            SettingsEditField::Session(field) => {
                let Some(snapshot) = self.active_settings_snapshot().cloned() else {
                    self.settings_overlay = None;
                    return false;
                };
                self.settings_draft.stage(&snapshot, field, value)
            }
            SettingsEditField::Global(field) => {
                let Some(snapshot) = self.active_global_settings_snapshot().cloned() else {
                    self.settings_overlay = None;
                    return false;
                };
                self.global_settings_draft.stage(&snapshot, field, value)
            }
            SettingsEditField::Profile(field) => {
                let Some(snapshot) = self.selected_profile_settings_snapshot() else {
                    self.settings_overlay = None;
                    return false;
                };
                self.profile_settings_draft.stage(&snapshot, field, value)
            }
        };
        if let Err(error) = result {
            self.warning = Some(format!("settings edit failed: {error:#}"));
            return false;
        }
        self.settings_overlay = None;
        self.notice = None;
        if self.warning.as_deref().is_some_and(|warning| {
            warning.starts_with("settings edit failed:")
                || warning.starts_with("profile settings edit failed:")
        }) {
            self.warning = None;
        }
        self.reproject_settings(target);
        if target.is_profiles() {
            self.clamp_profile_editor_selection();
        }
        true
    }

    fn stage_secret_setting(
        &mut self,
        target: &SelectorTarget,
        field: SettingsEditField,
        action: SecretSettingsAction,
    ) -> bool {
        let result = match field {
            SettingsEditField::Global(field) => {
                let Some(snapshot) = self.active_global_settings_snapshot().cloned() else {
                    self.settings_overlay = None;
                    return false;
                };
                self.global_settings_draft
                    .stage_secret(&snapshot, field, action)
            }
            SettingsEditField::Profile(field) => {
                let Some(snapshot) = self.selected_profile_settings_snapshot() else {
                    self.settings_overlay = None;
                    return false;
                };
                self.profile_settings_draft
                    .stage_secret(&snapshot, field, action)
            }
            SettingsEditField::Session(_) => {
                self.warning =
                    Some("Secret editor is unavailable for this session setting".to_string());
                return false;
            }
        };
        if let Err(error) = result {
            self.warning = Some(format!("settings edit failed: {error:#}"));
            return false;
        }
        self.settings_overlay = None;
        self.notice = None;
        self.warning = None;
        self.reproject_settings(target);
        if target.is_profiles() {
            self.clamp_profile_editor_selection();
        }
        true
    }

    fn request_leave_settings(&mut self) {
        if self.settings_draft.is_dirty() || self.global_settings_draft.is_dirty() {
            self.settings_overlay = Some(SettingsOverlay::ConfirmDiscard { selected: 0 });
        } else {
            self.leave_settings();
        }
    }

    fn leave_settings(&mut self) {
        self.settings_draft = SessionSettingsDraft::default();
        self.global_settings_draft = GlobalSettingsDraft::default();
        self.profile_settings_draft = ProfileSettingsDraft::default();
        for row in self.rows.iter_mut().chain(self.context.settings.iter_mut()) {
            if let Some(snapshot) = &row.settings_snapshot { row.settings = snapshot.categories(&self.settings_draft); }
            if let Some(snapshot) = &row.global_settings_snapshot { row.settings = snapshot.categories(&self.global_settings_draft); }
        }
        self.settings_overlay = None;
        self.profile_overlay = None;
        self.mode = SelectorMode::Agents;
        self.ensure_selection();
    }

    fn discard_settings_draft(&mut self, target: &SelectorTarget) {
        if target.uses_global_settings() {
            self.global_settings_draft = GlobalSettingsDraft::default();
        } else {
            self.settings_draft = SessionSettingsDraft::default();
        }
        self.settings_overlay = None;
        self.reproject_settings(target);
    }

    fn reproject_settings(&mut self, target: &SelectorTarget) {
        if target.is_profiles() {
            return;
        }
        let Some(row) = self
            .rows
            .iter_mut()
            .chain(self.context.settings.iter_mut())
            .find(|row| row.target == *target)
        else {
            return;
        };
        if let Some(snapshot) = row.settings_snapshot.as_ref() {
            row.settings = snapshot.categories(&self.settings_draft);
        } else if let Some(snapshot) = row.global_settings_snapshot.as_ref() {
            row.settings = snapshot.categories(&self.global_settings_draft);
        }
    }

    fn clamp_profile_editor_selection(&mut self) {
        let count = self.profile_editor_option_count();
        if let SelectorMode::ProfileManager {
            editor_selected, ..
        } = &mut self.mode
        {
            *editor_selected = (*editor_selected).min(count.saturating_sub(1));
        }
    }

    fn handle_confirmation_event(
        &mut self,
        event: SelectorEvent,
        agent_key: String,
        action: SessionTuiAction,
        launch_profile: Option<String>,
        confirmed: bool,
    ) -> SelectorControl {
        let mut next_confirmed = confirmed;
        match event {
            SelectorEvent::OpenActions | SelectorEvent::Down | SelectorEvent::Last => {
                next_confirmed = true;
            }
            SelectorEvent::Back | SelectorEvent::Up | SelectorEvent::First => {
                next_confirmed = false;
            }
            SelectorEvent::Activate if next_confirmed => {
                if matches!(
                    action,
                    SessionTuiAction::StockStart | SessionTuiAction::StockRestart
                ) {
                    let Some(request) = self.stock_runtime_confirmation.clone() else {
                        self.warning =
                            Some("Stock runtime review unavailable; no action submitted".into());
                        return SelectorControl::Continue;
                    };
                    if request.review.subject.cutex_session_id.as_str() != agent_key
                        || request.review.restart != (action == SessionTuiAction::StockRestart)
                    {
                        self.warning =
                            Some("Stock runtime confirmation target changed; review again".into());
                        return SelectorControl::Continue;
                    }
                    return SelectorControl::Selected(SessionTuiIntent {
                        key: agent_key,
                        action,
                        launch_profile: None,
                        stock_runtime: Some(request),
                        runtime_package: None,
                    });
                }
                if action == SessionTuiAction::UpgradeRuntimePackage {
                    let Some(request) = self.runtime_package_confirmation.clone() else {
                        self.warning =
                            Some("Runtime package review unavailable; no action submitted".into());
                        return SelectorControl::Continue;
                    };
                    if request.review.subject.cutex_session_id.as_str() != agent_key {
                        self.warning =
                            Some("Runtime package confirmation target changed; review again".into());
                        return SelectorControl::Continue;
                    }
                    return SelectorControl::Selected(SessionTuiIntent {
                        key: agent_key,
                        action,
                        launch_profile: None,
                        stock_runtime: None,
                        runtime_package: Some(request),
                    });
                }
                if matches!(
                    action,
                    SessionTuiAction::RetireSession | SessionTuiAction::RestoreSession
                ) {
                    let Some(request) = self.archive_confirmation.take() else {
                        self.warning =
                            Some("Archive review unavailable; no action submitted".into());
                        return SelectorControl::Continue;
                    };
                    if request.review.cutex_session_id.as_str() != agent_key
                        || (request.review.operation
                            == cutex::agent_management::AgentArchiveOperation::Restore)
                            != (action == SessionTuiAction::RestoreSession)
                    {
                        self.warning =
                            Some("Archive confirmation target changed; review again".into());
                        return SelectorControl::Continue;
                    }
                    return SelectorControl::ExecuteArchive(request);
                }
                return SelectorControl::Selected(SessionTuiIntent {
                    key: agent_key,
                    action,
                    launch_profile,
                    stock_runtime: None,
                    runtime_package: None,
                });
            }
            SelectorEvent::Activate | SelectorEvent::Escape => {
                self.archive_confirmation = None;
                self.stock_runtime_confirmation = None;
                self.runtime_package_confirmation = None;
                if self.confirmation_returns_to_list {
                    self.confirmation_returns_to_list = false;
                    self.inspector_overview_focused = false;
                    self.mode = SelectorMode::Agents;
                    return SelectorControl::Continue;
                }
                if action == SessionTuiAction::RestoreSession {
                    let selected = self
                        .retired_rows
                        .iter()
                        .position(|row| row.target.agent_key() == Some(agent_key.as_str()))
                        .unwrap_or(0);
                    self.mode = SelectorMode::RetiredSessions { selected };
                    return SelectorControl::Continue;
                }
                let selected = self.action_index(&agent_key, action).unwrap_or(0);
                self.mode = SelectorMode::Actions {
                    agent_key,
                    selected,
                    launch_profile,
                };
                return SelectorControl::Continue;
            }
            SelectorEvent::Exit => return SelectorControl::Exit,
            SelectorEvent::OpenSettings
            | SelectorEvent::Insert(_)
            | SelectorEvent::Backspace
            | SelectorEvent::Delete
            | SelectorEvent::ClearInput => {}
        }
        self.mode = SelectorMode::ConfirmRuntimeAction {
            agent_key,
            action,
            launch_profile,
            confirmed: next_confirmed,
        };
        SelectorControl::Continue
    }

    fn open_action_menu(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        if row.actions.is_empty() {
            return;
        }
        let Some(agent_key) = row.target.agent_key() else {
            return;
        };
        self.mode = SelectorMode::Actions {
            agent_key: agent_key.to_string(),
            selected: 0,
            launch_profile: None,
        };
    }

    fn open_subject_context(
        &mut self,
        id: &str,
        event: SelectorEvent,
        origin: PrimaryPanel,
    ) -> SelectorControl {
        let Some(target) = self
            .rows
            .iter()
            .find(|row| row.target.agent_key() == Some(id))
            .map(|row| row.target.clone())
        else {
            self.warning = Some(format!(
                "Agent {id} is absent from the current action snapshot; refresh or use Archive"
            ));
            return SelectorControl::Continue;
        };
        self.object_return = Some((origin, self.workspace_selection.selected().cloned()));
        self.workspace_selection.select(Some(target));
        self.mode = SelectorMode::Agents;
        match event {
            SelectorEvent::Activate => self.activate_primary_action(),
            SelectorEvent::OpenSettings => {
                self.open_settings();
                SelectorControl::Continue
            }
            _ => {
                self.open_action_menu();
                SelectorControl::Continue
            }
        }
    }

    fn finish_subject_context(&mut self) -> Option<PrimaryPanel> {
        let (panel, selection) = self.object_return.take()?;
        self.workspace_selection.select(selection);
        self.mode = SelectorMode::Agents;
        self.activate_primary_panel(panel);
        Some(panel)
    }

    fn open_settings(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        if row.settings.is_empty() {
            return;
        }
        let target = row.target.clone();
        let view = if target.is_global_settings() {
            SettingsView::Categories
        } else {
            SettingsView::Expanded
        };
        self.settings_draft = SessionSettingsDraft::default();
        self.settings_overlay = None;
        self.notice = None;
        self.mode = SelectorMode::Settings {
            target,
            category: 0,
            option: 0,
            focus: if view == SettingsView::Expanded {
                SettingsFocus::Options
            } else {
                SettingsFocus::Categories
            },
            view,
        };
    }

    fn activate_primary_action(&mut self) -> SelectorControl {
        self.freeze_agent_view_if_selected();
        let Some(row) = self.selected_row() else {
            return SelectorControl::Continue;
        };
        if row.target.is_global_settings() {
            self.open_settings();
            return SelectorControl::Continue;
        }
        if row.target.is_profiles() {
            return SelectorControl::OpenProfileManager;
        }
        if row.target.is_cutex_projects() {
            return SelectorControl::OpenCutexProjects;
        }
        if row.target.is_projects() {
            return SelectorControl::OpenProjects;
        }
        let Some(action) = row
            .actions
            .iter()
            .find(|item| item.primary)
            .map(|item| item.action)
        else {
            if row.managed && matches!(&row.target, SelectorTarget::Agent(_)) {
                self.warning = Some(format!(
                    "No attach or start route is currently available for {}",
                    row.agent
                ));
            }
            return SelectorControl::Continue;
        };
        let Some(agent_key) = row.target.agent_key() else {
            return SelectorControl::Continue;
        };
        let agent_key = agent_key.to_string();
        if (row.managed && row.lifecycle == Some(CutexSessionLifecycleState::Offline))
            || action.requires_confirmation()
        {
            self.stock_runtime_confirmation = None;
            self.runtime_package_confirmation = None;
            self.confirmation_returns_to_list = true;
            self.mode = SelectorMode::ConfirmRuntimeAction {
                agent_key,
                action,
                launch_profile: None,
                confirmed: false,
            };
            return SelectorControl::Continue;
        }
        SelectorControl::Selected(SessionTuiIntent {
            key: agent_key,
            action,
            launch_profile: None,
            stock_runtime: None,
            runtime_package: None,
        })
    }

    fn select_action(
        &mut self,
        agent_key: String,
        action: SessionTuiAction,
        launch_profile: Option<String>,
    ) -> SelectorControl {
        if action == SessionTuiAction::RevertHistory { return SelectorControl::RevertHistory(agent_key); }
        if launch_profile.is_some()
            && !self
                .rows
                .iter()
                .find(|row| row.target.agent_key() == Some(agent_key.as_str()))
                .is_some_and(|row| row.action_supports_launch_profile(action))
        {
            self.warning = Some(format!(
                "{} cannot apply a one-launch profile in the current state",
                action.label()
            ));
            return SelectorControl::Continue;
        }
        let starts_offline_agent = self.row_for_action_key(&agent_key).is_some_and(|row| {
            row.lifecycle == Some(CutexSessionLifecycleState::Offline)
                && matches!(
                    action,
                    SessionTuiAction::ResumeAttach
                        | SessionTuiAction::OpenTui
                        | SessionTuiAction::ResumeHere
                        | SessionTuiAction::ResumeManaged
                )
        });
        self.freeze_agent_view();
        if action.requires_confirmation() || starts_offline_agent {
            self.stock_runtime_confirmation = None;
            self.runtime_package_confirmation = None;
            self.confirmation_returns_to_list = false;
            self.mode = SelectorMode::ConfirmRuntimeAction {
                agent_key,
                action,
                launch_profile,
                confirmed: false,
            };
            SelectorControl::Continue
        } else {
            SelectorControl::Selected(SessionTuiIntent {
                key: agent_key,
                action,
                launch_profile,
                stock_runtime: None,
                runtime_package: None,
            })
        }
    }

    fn action_index(&self, agent_key: &str, action: SessionTuiAction) -> Option<usize> {
        self.rows
            .iter()
            .find(|row| row.target.agent_key() == Some(agent_key))?
            .control_index_for_action(action)
    }

    fn activate_close_shortcut(&mut self) -> SelectorControl {
        if !matches!(self.mode, SelectorMode::Agents) {
            return SelectorControl::Continue;
        }
        self.freeze_agent_view();
        let Some(row) = self.selected_row() else {
            return SelectorControl::Continue;
        };
        let Some(agent_key) = row.target.agent_key() else {
            self.warning = Some("Select an agent with a runtime to close".to_string());
            return SelectorControl::Continue;
        };
        if !row
            .actions
            .iter()
            .any(|item| item.action == SessionTuiAction::CloseRuntime)
        {
            self.warning = Some(format!(
                "No runtime is available to close for {}",
                row.agent
            ));
            return SelectorControl::Continue;
        }
        self.mode = SelectorMode::ConfirmRuntimeAction {
            agent_key: agent_key.to_string(),
            action: SessionTuiAction::CloseRuntime,
            launch_profile: None,
            confirmed: false,
        };
        self.confirmation_returns_to_list = true;
        SelectorControl::Continue
    }

    fn runtime_close_started(&mut self, intent: &SessionTuiIntent) {
        debug_assert!(intent_runs_in_selector(intent));
        self.freeze_agent_view();
        let agent_name = self
            .row_for_action_key(&intent.key)
            .map(|row| row.agent.clone())
            .unwrap_or_else(|| intent.key.clone());
        self.workspace_selection
            .select(Some(SelectorTarget::Agent(intent.key.clone())));
        self.mode = SelectorMode::ClosingRuntime {
            agent_key: intent.key.clone(),
            agent_name,
            action: intent.action,
        };
        self.confirmation_returns_to_list = false;
        self.notice = None;
        self.warning = None;
    }

    fn runtime_close_succeeded(&mut self, snapshot: SelectorSnapshot) {
        let Some((agent_key, agent_name, action)) = self.runtime_close_identity() else {
            return;
        };
        let target = if action == SessionTuiAction::RetireSession {
            SelectorTarget::RetiredSessions
        } else {
            SelectorTarget::Agent(agent_key)
        };
        self.mode = SelectorMode::Agents;
        self.workspace_selection.select(Some(target.clone()));
        self.workspace_selection.mark_transiently_visible(
            matches!(
                action,
                SessionTuiAction::CloseRuntime
                    | SessionTuiAction::RepairInterruptedHistory
                    | SessionTuiAction::RestoreSession
                    | SessionTuiAction::UpgradeRuntimePackage
                )
            .then_some(target),
        );
        self.replace_snapshot(snapshot);
        self.notice = Some(match action {
            SessionTuiAction::CloseRuntime => format!("Runtime closed: {agent_name}"),
            SessionTuiAction::UpgradeRuntimePackage => {
                format!("Runtime package upgraded; {agent_name} remains offline")
            }
            SessionTuiAction::RepairInterruptedHistory => {
                format!("Interrupted history checked and repaired if needed: {agent_name}")
            }
            SessionTuiAction::RetireSession => format!("Archived Agent: {agent_name}"),
            SessionTuiAction::RestoreSession => format!("Restored offline: {agent_name}"),
            _ => unreachable!("only selector actions enter the operation worker"),
        });
    }

    fn runtime_close_refresh_failed(&mut self, message: String) {
        let Some((agent_key, agent_name, action)) = self.runtime_close_identity() else {
            return;
        };
        if action != SessionTuiAction::CloseRuntime {
            self.mode = SelectorMode::Agents;
            self.notice = None;
            self.warning = Some(format!(
                "{} completed, but refresh failed: {message}; refresh the session list to resync",
                action.label()
            ));
            return;
        }
        let target = SelectorTarget::Agent(agent_key);
        if let Some(row) = self.rows.iter_mut().find(|row| row.target == target) {
            row.lifecycle = Some(CutexSessionLifecycleState::Offline);
            row.attachable = false;
            row.actions.clear();
        }
        self.mode = SelectorMode::Agents;
        self.workspace_selection.select(Some(target.clone()));
        self.workspace_selection
            .mark_transiently_visible(Some(target));
        self.ensure_selection();
        self.notice = Some(format!("Runtime closed: {agent_name}"));
        self.warning = Some(format!(
            "Runtime closed, but live refresh failed: {message}"
        ));
    }

    fn runtime_close_failed(&mut self, message: String) {
        let identity = self.runtime_close_identity();
        let agent_name = identity
            .as_ref()
            .map(|(_, agent_name, _)| agent_name.clone())
            .unwrap_or_else(|| "selected agent".to_string());
        let action = identity
            .map(|(_, _, action)| action.label())
            .unwrap_or("complete the operation");
        self.mode = SelectorMode::Agents;
        self.notice = None;
        self.warning = Some(format!("Failed to {action} for {agent_name}: {message}"));
    }

    #[cfg(test)]
    fn dispatch_failed(&mut self, agent_key: &str, message: String) {
        self.workspace_selection
            .select(Some(SelectorTarget::Agent(agent_key.to_string())));
        self.mode = SelectorMode::Agents;
        self.inspector_overview_focused = true;
        self.confirmation_returns_to_list = false;
        self.notice = None;
        self.warning = Some(format!(
            "Unable to enter the selected Agent; no fallback terminal was launched: {message}"
        ));
    }

    fn runtime_close_identity(&self) -> Option<(String, String, SessionTuiAction)> {
        match &self.mode {
            SelectorMode::ClosingRuntime {
                agent_key,
                agent_name,
                action,
            } => Some((agent_key.clone(), agent_name.clone(), *action)),
            _ => None,
        }
    }

    fn refresh_activity_states(&mut self, activity_states: &HashMap<String, SessionActivityState>) {
        for row in &mut self.rows {
            row.activity = row
                .activity_session_id
                .as_deref()
                .and_then(|session_id| activity_states.get(session_id))
                .and_then(selector_activity_from_state);
        }
    }

    fn replace_snapshot(&mut self, snapshot: SelectorSnapshot) {
        let previous_index = self.selected_visible_index().unwrap_or(0);
        let stale_confirmation =
            if let SelectorMode::ConfirmRuntimeAction { agent_key, action, .. } = &self.mode {
                let old = self
                    .rows
                    .iter()
                    .chain(self.retired_rows.iter())
                    .find(|r| r.target.agent_key() == Some(agent_key.as_str()));
                let candidates = if *action == SessionTuiAction::RestoreSession { &self.retired_rows } else { &snapshot.rows };
                let new = candidates.iter().find(|r| r.target.agent_key() == Some(agent_key.as_str()));
                old.zip(new).is_none_or(|(old, new)| {
                    old.revision != new.revision
                        || old.agent != new.agent
                        || old.lifecycle != new.lifecycle
                })
            } else {
                false
            };
        if stale_confirmation {
            self.mode = SelectorMode::Agents;
            self.notice =
                Some("Confirmation target changed; review the current Agent again".into());
        }
        let active_settings_target = match &self.mode {
            SelectorMode::Settings { target, .. } => Some(target.clone()),
            _ => None,
        };
        self.rows = snapshot.rows;
        remote_projection::apply(self);
        self.context = AppContext::extract(&mut self.rows);
        sort_rows(&mut self.rows);
        let mut settings_warning = None;
        if let Some(settings_override) = self.pending_settings_refresh_override.take() {
            if let Some(row) = self
                .rows
                .iter_mut()
                .find(|row| row.target == settings_override.target)
            {
                row.settings = settings_override
                    .snapshot
                    .categories(&SessionSettingsDraft::default());
                row.settings_snapshot = Some(settings_override.snapshot);
                if let Some(agent) = settings_override.agent {
                    row.agent = agent;
                }
                row.configured_profile = settings_override.configured_profile;
                row.backend = settings_override.backend;
                row.pinned = settings_override.pinned;
                row.managed = settings_override.managed;
                if let Some(actions) = settings_override.actions {
                    row.actions = actions;
                }
            }
            settings_warning = settings_override.warning;
            sort_rows(&mut self.rows);
        }
        if let Some(settings_override) = self.pending_global_settings_refresh_override.take() {
            self.context.global_snapshot = Some(settings_override.snapshot.clone());
            for row in self
                .context
                .settings
                .iter_mut()
                .filter(|row| row.target.uses_global_settings())
            {
                row.settings = settings_override
                    .snapshot
                    .categories(&GlobalSettingsDraft::default());
                if row.target.is_profiles() {
                    row.settings.clear();
                }
                row.global_settings_snapshot = Some(settings_override.snapshot.clone());
            }
        }
        if let Some(profile_override) = self.pending_profile_refresh_override.take() {
            self.apply_profile_projection(&profile_override.projection);
        }
        self.refreshing = false;
        let warning = combine_warnings(snapshot.warning, settings_warning);
        self.warning = combine_warnings(warning, self.pending_startup_warning.take());
        if self.selected_visible_index().is_none() {
            let indices = self.visible_indices();
            let target = indices
                .get(previous_index.min(indices.len().saturating_sub(1)))
                .map(|index| self.rows[*index].target.clone());
            self.workspace_selection.select(target);
        }
        self.ensure_selection();
        self.recent.enrich_views(
            &self
                .rows
                .iter()
                .filter_map(|row| row.view.clone())
                .collect::<Vec<_>>(),
        );
        if let Some(target) = active_settings_target {
            self.reproject_settings(&target);
        }
        self.normalize_mode_after_snapshot();
    }

    fn mark_refresh_failed(&mut self, message: String) {
        for row in &mut self.rows {
            if let Some(view) = &mut row.view {
                if let Some(value) = view.runtime.known().cloned() {
                    view.runtime = Observation::Stale(value, message.clone());
                }
                if let Some(value) = view.project.known().cloned() {
                    view.project = Observation::Stale(value, message.clone());
                }
            }
        }
        self.refreshing = false;
        self.pending_settings_refresh_override = None;
        self.pending_global_settings_refresh_override = None;
        self.pending_profile_refresh_override = None;
        self.warning = combine_warnings(Some(message), self.pending_startup_warning.take());
    }

    fn settings_apply_succeeded(
        &mut self,
        key: &str,
        record: &CutexSessionRecord,
        profile_names: &[String],
        changed_count: usize,
        launch_actions_changed: bool,
        warning: Option<String>,
    ) {
        let target = SelectorTarget::Agent(key.to_string());
        let snapshot = SessionSettingsSnapshot::from_record_with_profiles(record, profile_names);
        let actions = launch_actions_changed.then(|| {
            let attachable = self
                .rows
                .iter()
                .find(|row| row.target == target)
                .is_some_and(|row| row.attachable);
            settings_actions_for_record(record, attachable)
        });
        if self.refreshing {
            self.pending_settings_refresh_override = Some(PendingSettingsRefreshOverride {
                target: target.clone(),
                snapshot: snapshot.clone(),
                agent: (!cutex_session_is_managed(record))
                    .then(|| cutex_session_display_name(record)),
                configured_profile: record.profile.clone(),
                backend: runtime_backend_short_label(record.runtime_backend).to_string(),
                pinned: record.quick_action == CutexSessionQuickActionMode::Pinned,
                managed: cutex_session_is_managed(record),
                actions: actions.clone(),
                warning: warning.clone(),
            });
        }
        if let Some(row) = self.rows.iter_mut().find(|row| row.target == target) {
            row.settings = snapshot.categories(&SessionSettingsDraft::default());
            row.settings_snapshot = Some(snapshot);
            if !cutex_session_is_managed(record) {
                row.agent = cutex_session_display_name(record);
            }
            row.thread_title = cutex_session_is_managed(record)
                .then(|| record.thread_name.clone())
                .flatten();
            row.configured_profile = record.profile.clone();
            row.backend = runtime_backend_short_label(record.runtime_backend).to_string();
            row.pinned = record.quick_action == CutexSessionQuickActionMode::Pinned;
            row.managed = cutex_session_is_managed(record);
            if let Some(actions) = actions {
                row.actions = actions;
            }
        }
        sort_rows(&mut self.rows);
        self.settings_draft = SessionSettingsDraft::default();
        self.settings_overlay = None;
        if self
            .warning
            .as_deref()
            .is_some_and(|warning| warning.starts_with("settings apply failed:"))
        {
            self.warning = None;
        }
        if warning.is_some() {
            self.warning = warning;
        }
        self.notice = Some(format!("Saved {changed_count} setting(s)"));
    }

    fn global_settings_apply_succeeded(
        &mut self,
        config: &CodezConfig,
        profile_names: &[String],
        changed_count: usize,
    ) {
        let snapshot = GlobalSettingsSnapshot::from_config_with_profiles(config, profile_names);
        self.context.global_snapshot = Some(snapshot.clone());
        if self.refreshing {
            self.pending_global_settings_refresh_override =
                Some(PendingGlobalSettingsRefreshOverride {
                    snapshot: snapshot.clone(),
                });
        }
        for row in self
            .context
            .settings
            .iter_mut()
            .filter(|row| row.target.uses_global_settings())
        {
            row.settings = snapshot.categories(&GlobalSettingsDraft::default());
            if row.target.is_profiles() {
                row.settings.clear();
            }
            row.global_settings_snapshot = Some(snapshot.clone());
        }
        self.global_settings_draft = GlobalSettingsDraft::default();
        self.settings_overlay = None;
        if self
            .warning
            .as_deref()
            .is_some_and(|warning| warning.starts_with("global settings apply failed:"))
        {
            self.warning = None;
        }
        self.notice = Some(format!("Saved {changed_count} setting(s)"));
    }

    fn global_settings_apply_failed(&mut self, message: String) {
        self.warning = Some(format!("global settings apply failed: {message}"));
        self.notice = None;
    }

    fn session_management_succeeded(
        &mut self,
        key: &str,
        command: SessionSettingsCommand,
        record: &CutexSessionRecord,
        profile_names: &[String],
        warning: Option<String>,
    ) {
        let target = SelectorTarget::Agent(key.to_string());
        let snapshot = SessionSettingsSnapshot::from_record_with_profiles(record, profile_names);
        let attachable = self
            .rows
            .iter()
            .find(|row| row.target == target)
            .is_some_and(|row| row.attachable);
        let actions = settings_actions_for_record(record, attachable);
        let managed = cutex_session_is_managed(record);
        if self.refreshing {
            self.pending_settings_refresh_override = Some(PendingSettingsRefreshOverride {
                target: target.clone(),
                snapshot: snapshot.clone(),
                agent: (!managed).then(|| cutex_session_display_name(record)),
                configured_profile: record.profile.clone(),
                backend: runtime_backend_short_label(record.runtime_backend).to_string(),
                pinned: record.quick_action == CutexSessionQuickActionMode::Pinned,
                managed,
                actions: Some(actions.clone()),
                warning: warning.clone(),
            });
        }
        if let Some(row) = self.rows.iter_mut().find(|row| row.target == target) {
            row.settings = snapshot.categories(&SessionSettingsDraft::default());
            row.settings_snapshot = Some(snapshot);
            if !managed {
                row.agent = cutex_session_display_name(record);
            }
            row.thread_title = managed.then(|| record.thread_name.clone()).flatten();
            row.configured_profile = record.profile.clone();
            row.backend = runtime_backend_short_label(record.runtime_backend).to_string();
            row.pinned = record.quick_action == CutexSessionQuickActionMode::Pinned;
            row.managed = managed;
            row.actions = actions;
        }
        sort_rows(&mut self.rows);
        self.settings_draft = SessionSettingsDraft::default();
        self.settings_overlay = None;
        if self
            .warning
            .as_deref()
            .is_some_and(|warning| warning.starts_with("management change failed:"))
        {
            self.warning = None;
        }
        if warning.is_some() {
            self.warning = warning;
        }
        self.notice = Some(command.success_notice().to_string());
    }

    fn session_management_failed(&mut self, message: String) {
        self.warning = Some(format!("management change failed: {message}"));
        self.notice = None;
    }

    fn profile_management_succeeded(&mut self, result: ProfileManagementResult) {
        if self.refreshing {
            self.pending_profile_refresh_override = Some(PendingProfileRefreshOverride {
                projection: result.projection.clone(),
            });
        }
        self.apply_profile_projection(&result.projection);
        let SelectorMode::ProfileManager {
            selected,
            focus,
            editor_selected,
            ..
        } = self.mode.clone()
        else {
            return;
        };
        let selected = result
            .preferred_profile_id
            .as_deref()
            .and_then(|profile_id| {
                result
                    .profiles
                    .iter()
                    .position(|profile| profile.id == profile_id)
                    .map(|index| index + 1)
            })
            .unwrap_or_else(|| selected.min(result.profiles.len().saturating_add(1)));
        let profile_usage_worker = super::profile_usage::start(&result.profiles);
        self.mode = SelectorMode::ProfileManager {
            profiles: result.profiles,
            selected,
            focus,
            editor_selected,
        };
        self.profile_settings_draft = ProfileSettingsDraft::default();
        self.settings_overlay = None;
        self.clamp_profile_editor_selection();
        self.profile_overlay = None;
        self.profile_usage_worker = profile_usage_worker;
        if self
            .warning
            .as_deref()
            .is_some_and(|warning| warning.starts_with("profile change failed:"))
        {
            self.warning = None;
        }
        self.notice = Some(result.notice);
    }

    fn profile_management_failed(&mut self, request: &ProfileManagementRequest, message: String) {
        self.warning = Some(format!("profile change failed: {message}"));
        self.notice = None;
        match &request.command {
            ProfileManagementCommand::Rename { new_name } => {
                self.profile_overlay = Some(ProfileOverlay::RenameInput {
                    profile_id: request.profile_id.clone(),
                    old_name: request.profile_name.clone(),
                    input: Input::new(new_name.clone()),
                });
            }
            ProfileManagementCommand::Remove => {
                self.profile_overlay = Some(ProfileOverlay::ConfirmRemove {
                    profile_id: request.profile_id.clone(),
                    profile_name: request.profile_name.clone(),
                    selected: 0,
                });
            }
            ProfileManagementCommand::Activate => {}
        }
    }

    fn profile_settings_apply_failed(&mut self, message: String) {
        self.warning = Some(format!("profile settings apply failed: {message}"));
        self.notice = None;
        self.settings_overlay = None;
    }

    fn profile_management_refresh_failed(&mut self, notice: String, message: String) {
        self.notice = Some(notice);
        self.warning = Some(format!("Profile changed, but UI refresh failed: {message}"));
        self.profile_settings_draft = ProfileSettingsDraft::default();
        self.settings_overlay = None;
        self.profile_overlay = None;
    }

    fn apply_profile_projection(&mut self, projection: &ProfileProjectionSnapshot) {
        self.context.global_snapshot = Some(GlobalSettingsSnapshot::from_config_with_profiles(
            &projection.config,
            &projection.profile_names,
        ));
        for row in self.rows.iter_mut().chain(self.context.settings.iter_mut()) {
            if row.target.uses_global_settings() {
                let snapshot = GlobalSettingsSnapshot::from_config_with_profiles(
                    &projection.config,
                    &projection.profile_names,
                );
                row.settings = snapshot.categories(&GlobalSettingsDraft::default());
                if row.target.is_profiles() {
                    row.settings.clear();
                }
                row.global_settings_snapshot = Some(snapshot);
                continue;
            }
            let Some(key) = row.target.agent_key() else {
                continue;
            };
            let Some(record) = projection.records.get(key) else {
                continue;
            };
            let snapshot = SessionSettingsSnapshot::from_record_with_profiles(
                record,
                &projection.profile_names,
            );
            row.settings = snapshot.categories(&SessionSettingsDraft::default());
            row.settings_snapshot = Some(snapshot);
            row.configured_profile = record.profile.clone();
        }
    }

    fn settings_apply_failed(&mut self, message: String) {
        self.warning = Some(format!("settings apply failed: {message}"));
        self.notice = None;
    }

    fn ensure_selection(&mut self) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            self.workspace_selection.select(None);
            return;
        }
        let selection_still_visible = self.workspace_selection.selected().is_some_and(|target| {
            visible
                .iter()
                .any(|index| self.rows[*index].target == *target)
        });
        if !selection_still_visible {
            self.workspace_selection
                .select(Some(self.rows[visible[0]].target.clone()));
        }
    }

    fn normalize_mode_after_snapshot(&mut self) {
        match self.mode.clone() {
            SelectorMode::Agents => {
                if self.selected_managed_agent().is_none() {
                    self.inspector_overview_focused = false;
                }
            }
            SelectorMode::RecentSessions => {}
            SelectorMode::RetiredSessions { selected } => {
                self.mode = SelectorMode::RetiredSessions {
                    selected: selected.min(self.retired_rows.len().saturating_sub(1)),
                };
            }
            SelectorMode::Actions {
                agent_key,
                selected,
                launch_profile,
            } => {
                let Some((actions_empty, control_count, launch_profile_valid)) = self
                    .rows
                    .iter()
                    .find(|row| row.target.agent_key() == Some(agent_key.as_str()))
                    .map(|row| {
                        let valid = launch_profile.as_ref().is_none_or(|profile| {
                            row.launch_profile_control_available()
                                && row.settings_snapshot.as_ref().is_some_and(|snapshot| {
                                    snapshot.profile_names().contains(profile)
                                })
                        });
                        (row.actions.is_empty(), row.action_control_count(), valid)
                    })
                else {
                    self.action_overlay = None;
                    self.mode = SelectorMode::Agents;
                    return;
                };
                if actions_empty {
                    self.action_overlay = None;
                    self.mode = SelectorMode::Agents;
                } else {
                    let launch_profile = if launch_profile_valid {
                        launch_profile
                    } else {
                        self.action_overlay = None;
                        self.warning =
                            Some("Selected one-launch profile is no longer available".to_string());
                        None
                    };
                    self.mode = SelectorMode::Actions {
                        agent_key,
                        selected: selected.min(control_count.saturating_sub(1)),
                        launch_profile,
                    };
                }
            }
            SelectorMode::Settings {
                target,
                category,
                option,
                focus,
                view,
            } => {
                let Some(row) = self
                    .rows
                    .iter()
                    .chain(self.context.settings.iter())
                    .find(|row| row.target == target)
                else {
                    self.mode = SelectorMode::Agents;
                    return;
                };
                if row.settings.is_empty() {
                    self.mode = SelectorMode::Agents;
                } else {
                    let category = category.min(row.settings.len() - 1);
                    let option = option.min(row.settings[category].options.len().saturating_sub(1));
                    self.mode = SelectorMode::Settings {
                        target,
                        category,
                        option,
                        focus,
                        view,
                    };
                }
            }
            SelectorMode::ProfileManager {
                profiles,
                selected,
                focus,
                editor_selected,
            } => {
                let option_count = if selected == 0 {
                    2
                } else if selected <= profiles.len() {
                    self.profile_editor_option_count()
                } else {
                    1
                };
                self.mode = SelectorMode::ProfileManager {
                    selected: selected.min(profiles.len().saturating_add(1)),
                    profiles,
                    focus,
                    editor_selected: editor_selected.min(option_count.saturating_sub(1)),
                };
            }
            SelectorMode::ConfirmRuntimeAction {
                agent_key,
                action,
                launch_profile,
                confirmed,
            } => {
                let candidates = if action == SessionTuiAction::RestoreSession { &self.retired_rows } else { &self.rows };
                let still_available = candidates.iter()
                    .find(|row| row.target.agent_key() == Some(agent_key.as_str()))
                    .is_some_and(|row| {
                        row.control_index_for_action(action).is_some()
                            && launch_profile.as_ref().is_none_or(|profile| {
                                row.action_supports_launch_profile(action)
                                    && row.settings_snapshot.as_ref().is_some_and(|snapshot| {
                                        snapshot.profile_names().contains(profile)
                                    })
                            })
                    });
                if !still_available {
                    self.mode = SelectorMode::Agents;
                } else {
                    self.mode = SelectorMode::ConfirmRuntimeAction {
                        agent_key,
                        action,
                        launch_profile,
                        confirmed,
                    };
                }
            }
            SelectorMode::ClosingRuntime { .. } => {}
        }
    }

    fn move_selection(&mut self, direction: isize) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            self.workspace_selection.select(None);
            return;
        }
        let current = self.selected_visible_index().unwrap_or(0);
        let next = current
            .saturating_add_signed(direction)
            .min(visible.len() - 1);
        self.workspace_selection
            .select(Some(self.rows[visible[next]].target.clone()));
    }

    fn select_edge(&mut self, last: bool) {
        let visible = self.visible_indices();
        let index = if last {
            visible.last()
        } else {
            visible.first()
        };
        self.workspace_selection
            .select(index.map(|index| self.rows[*index].target.clone()));
    }
}

fn wrapped_index(current: usize, direction: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    if direction < 0 {
        if current == 0 {
            len - 1
        } else {
            current - 1
        }
    } else if direction > 0 {
        (current + 1) % len
    } else {
        current.min(len - 1)
    }
}

fn setting_option_count(settings: &[SessionTuiSettingCategory]) -> usize {
    settings.iter().map(|category| category.options.len()).sum()
}

fn setting_flat_index(
    settings: &[SessionTuiSettingCategory],
    category: usize,
    option: usize,
) -> Option<usize> {
    let current = settings.get(category)?;
    (option < current.options.len()).then(|| {
        settings[..category]
            .iter()
            .map(|category| category.options.len())
            .sum::<usize>()
            + option
    })
}

fn setting_indices_at_flat_index(
    settings: &[SessionTuiSettingCategory],
    mut flat_index: usize,
) -> Option<(usize, usize)> {
    for (category_index, category) in settings.iter().enumerate() {
        if flat_index < category.options.len() {
            return Some((category_index, flat_index));
        }
        flat_index = flat_index.saturating_sub(category.options.len());
    }
    None
}

fn moved_flat_setting(
    settings: &[SessionTuiSettingCategory],
    category: usize,
    option: usize,
    direction: isize,
) -> Option<(usize, usize)> {
    let count = setting_option_count(settings);
    let current = setting_flat_index(settings, category, option)?;
    setting_indices_at_flat_index(settings, wrapped_index(current, direction, count))
}

fn expanded_setting_table_row_index(
    settings: &[SessionTuiSettingCategory],
    category: usize,
    option: usize,
) -> Option<usize> {
    let current = settings.get(category)?;
    (option < current.options.len()).then(|| {
        settings[..category]
            .iter()
            .map(|category| category.options.len() + 1)
            .sum::<usize>()
            + option
            + 1
    })
}

pub(crate) fn run() -> anyhow::Result<()> {
    super::identity_styles::refresh();
    require_interactive_terminal(io::stdin().is_terminal(), io::stdout().is_terminal())?;

    let mut panel = PrimaryPanel::Agents;
    let mut refresh = spawn_snapshot_refresh()?;
    let mut selector_model = initial_selector_model(refresh.is_loading())?;
    let mut recent_catalog = None;
    let mut shell = TerminalShell::open()?;
    let mut events = ShellEvents;
    let mut projects_model = None;
    let mut refresh_project_members = false;
    loop {
        let [project_sort, job_sort] = selector_model.context.global_snapshot.as_ref()
            .map(GlobalSettingsSnapshot::other_sort_defaults).unwrap_or_default();
        let outcome = match panel {
            PrimaryPanel::Agents
            | PrimaryPanel::Recent
            | PrimaryPanel::Profiles
            | PrimaryPanel::Settings => {
                if panel == PrimaryPanel::Profiles
                    && !matches!(selector_model.mode, SelectorMode::ProfileManager { .. })
                {
                    open_profile_workspace(&mut selector_model);
                }
                if panel == PrimaryPanel::Settings && !matches!(selector_model.mode, SelectorMode::Settings { target: SelectorTarget::GlobalSettings, .. }) {
                    selector_command(&mut selector_model, Command::Settings);
                }
                selector_model.activate_primary_panel(panel);
                selector_model.enhanced_keyboard = shell.enhanced_keyboard();
                ensure_recent(panel, &mut recent_catalog, RecentCatalog::spawn)?;
                let outcome = run_event_loop(
                    shell.terminal(),
                    &mut events,
                    &mut selector_model,
                    &mut refresh,
                    &mut recent_catalog,
                )?;
                panel = if matches!(selector_model.mode, SelectorMode::RecentSessions) {
                    PrimaryPanel::Recent
                } else {
                    PrimaryPanel::Agents
                };
                outcome
            }
            PrimaryPanel::Jobs => {
                match super::session_tui_jobs::run(shell.terminal(), &mut events, job_sort)? {
                    PrimaryPanelOutcome::Exit => return Ok(()),
                    PrimaryPanelOutcome::Switch(next) => { panel = next; continue; }
                }
            }
            PrimaryPanel::Projects => {
                if std::mem::take(&mut refresh_project_members) {
                    if let Some(model) = projects_model.as_mut() {
                        super::session_tui_cutex_projects::refresh_after_member_action(model);
                    }
                }
                let (outcome, mut model) = super::session_tui_cutex_projects::run(
                    shell.terminal(),
                    &mut events,
                    projects_model.take(),
                    project_sort,
                )?;
                if std::mem::take(&mut model.open_settings_requested) {
                    selector_command(&mut selector_model, Command::Settings);
                    selector_model.settings_return_panel = Some(PrimaryPanel::Projects);
                }
                let member_action = model.member_action_requested.take();
                projects_model = Some(model);
                if let Some((id, event)) = member_action {
                    refresh_project_members = true;
                    match selector_model.open_subject_context(&id, event, PrimaryPanel::Projects) {
                        SelectorControl::Selected(intent) => {
                            SessionTuiCycleOutcome::Selected(intent)
                        }
                        _ => {
                            if selector_model.object_return.is_some() {
                                panel = PrimaryPanel::Agents;
                            } else {
                                if let Some(model) = projects_model.as_mut() {
                                    model.failure = selector_model.warning.take();
                                }
                                panel = PrimaryPanel::Projects;
                            }
                            continue;
                        }
                    }
                } else {
                    match outcome {
                        PrimaryPanelOutcome::Exit => return Ok(()),
                        PrimaryPanelOutcome::Switch(next) => {
                            panel = next;
                            continue;
                        }
                    }
                }
            }
            PrimaryPanel::Alarms => {
                match super::session_tui_alarms::run(shell.terminal(), &mut events)? {
                    PrimaryPanelOutcome::Exit => return Ok(()),
                    PrimaryPanelOutcome::Switch(next) => { panel = next; continue; }
                }
            }
        };
        match outcome {
            SessionTuiCycleOutcome::NewSession => {
                selector_model.clear_frozen_agent_view();
                match shell.handoff(|| -> anyhow::Result<std::process::ExitStatus> {
                    Ok(std::process::Command::new(std::env::current_exe()?)
                        .arg("new").status()?)
                })? {
                    Ok(status) => selector_model.notice = Some(format!("New session returned ({status})")),
                    Err(error) => selector_model.warning = Some(format!("New session: {error:#}")),
                }
                if recent_catalog.as_ref().is_some_and(|catalog| catalog.request(RecentCommand::Retry, None)) {
                    selector_model.recent_loading_started();
                }
            }
            SessionTuiCycleOutcome::NewAgent => {
                selector_model.clear_frozen_agent_view();
                match shell.handoff(super::light_new::wizard)? {
                    Ok(Some(result)) => {
                        selector_model.notice = Some(format!("Created Agent {}", result.formal_agent_name.as_deref().unwrap_or("")));
                        refresh = spawn_snapshot_refresh()?;
                        selector_model.refreshing = true;
                        refresh_project_members = true;
                        panel = PrimaryPanel::Agents;
                    }
                    Ok(None) => {},
                    Err(error) => selector_model.warning = Some(format!("New Agent: {error:#}")),
                }
            }
            SessionTuiCycleOutcome::NativeResume {
                catalog,
                thread,
                cwd,
            } => {
                selector_model.clear_frozen_agent_view();
                let result = shell.handoff(|| resume_recent_native(&catalog, &thread, &cwd))?;
                match result {
                    Ok(status) => {
                        selector_model.notice = Some(format!(
                            "Native session returned ({status}); no adoption requested"
                        ))
                    }
                    Err(error) => {
                        selector_model.warning = Some(format!("Native resume failed: {error:#}"))
                    }
                }
                panel = PrimaryPanel::Recent;
            }
            SessionTuiCycleOutcome::Exit => return Ok(()),
            SessionTuiCycleOutcome::Selected(intent) => {
                selector_model.clear_frozen_agent_view();
                let key = intent.key.clone();
                match shell
                    .handoff(|| super::session_tui_dispatch::dispatch_session_tui_intent(intent))?
                {
                    Ok(()) => {
                        selector_model.stock_runtime_confirmation = None;
                        selector_model.runtime_package_confirmation = None;
                        selector_model.notice = Some("Returned from foreground session".into())
                    }
                    Err(error) => {
                        selector_model.warning =
                            Some(format!("Foreground action for {key} failed: {error:#}"));
                    }
                }
                if let Some(origin) = selector_model.finish_subject_context() {
                    panel = origin;
                }
            }
            SessionTuiCycleOutcome::LoginProfile => {
                let startup = profile_login_startup(shell.handoff(super::auth::login_interactive)?);
                open_profile_workspace(&mut selector_model);
                selector_model.notice = startup.notice;
                selector_model.warning = startup.warning;
            }
            SessionTuiCycleOutcome::CutexProjects => {
                selector_model.clear_frozen_agent_view();
                panel = PrimaryPanel::Projects;
            }
            SessionTuiCycleOutcome::Projects => {
                selector_model.clear_frozen_agent_view();
                shell.handoff(super::session_tui_projects::run)??
            }
            SessionTuiCycleOutcome::Switch(next) => {
                selector_model.clear_frozen_agent_view();
                panel = next;
            }
        }
    }
}

fn resume_recent_native(
    catalog: &str,
    thread: &str,
    cwd: &str,
) -> anyhow::Result<std::process::ExitStatus> {
    anyhow::ensure!(
        catalog == "paired-local-app-server",
        "unsupported native catalog source"
    );
    let store = load_cutex_session_store()?;
    if let Some(record)=store.sessions.values().find(|r| r.app_server_thread_id()==Some(thread) && r.is_owned_session()) {
        if record.runtime_backend==cutex::session::model::CutexSessionRuntimeBackend::ClaudeSubscription {
            super::claude_runtime::lifecycle(&record.cutex_session_id,true)?;
            return cutex::runtime::claude::attach_status(&record.cutex_session_id);
        }
        return super::stock_lifecycle::attach_status(&record.cutex_session_id);
    }
    anyhow::ensure!(
        !store
            .sessions
            .values()
            .any(|r| r.codex_session_id.as_deref() == Some(thread)
                && (cutex_session_is_managed(r) || r.is_retired())),
        "native identity became managed/archived; refresh and use its existing Agent action"
    );
    let launch = super::session_native_workflow::NativeLaunch {
        cwd: std::path::PathBuf::from(cwd),
        native_home: cutex::config::paths::host_codex_home_dir()?,
        profile: None,
        model: None,
    };
    // Read the selected provider identity before terminal launch. No ensure,
    // adopt, import, profile selection or management initialization is called.
    use cutex::catalog::CatalogEndpoint;
    let mut endpoint = launch.endpoint()?;
    let observed = endpoint.request(
        "thread/read",
        serde_json::json!({"threadId":thread, "includeTurns":false}),
    )?;
    anyhow::ensure!(
        observed
            .pointer("/thread/id")
            .and_then(serde_json::Value::as_str)
            == Some(thread),
        "native source identity mismatch"
    );
    drop(endpoint);
    launch.interactive(Some(thread))
}

fn ensure_recent<T>(
    panel: PrimaryPanel,
    catalog: &mut Option<T>,
    factory: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<()> {
    if panel == PrimaryPanel::Recent && catalog.is_none() {
        *catalog = Some(factory()?);
    }
    Ok(())
}

fn open_profile_workspace(model: &mut SelectorModel) {
    match load_profile_catalog_read_only() {
        Ok(profiles) => {
            let profile_usage_worker = super::profile_usage::start(&profiles);
            model.open_profile_manager(profiles);
            model.profile_usage_worker = profile_usage_worker;
        }
        Err(error) => model.profile_manager_failed(format!("{error:#}")),
    }
}

fn initial_selector_model(refreshing: bool) -> anyhow::Result<SelectorModel> {
    let store = load_reconciled_session_store()?;
    let config = load_codez_config();
    let (profile_names, profile_warning) = profile_names_with_warning();
    let (activity_states, activity_warning) = activity_states_with_warning();
    let (project_contexts, project_warning) = project_contexts_with_warning();
    let (compaction_counts, compaction_warning) = compaction_counts_with_warning();
    let initial_warning = combine_warnings(
        combine_warnings(
            combine_warnings(profile_warning, activity_warning),
            compaction_warning,
        ),
        project_warning.clone(),
    );
    let initial_rows = selector_rows_from_store(
        &store,
        &[],
        &[],
        &config,
        &profile_names,
        &activity_states,
        &project_contexts,
        &compaction_counts,
    );
    let mut model = SelectorModel::new(initial_rows, refreshing, false);
    for row in &mut model.rows {
        if let Some(view) = &mut row.view {
            view.runtime = Observation::Unavailable("initial runtime snapshot pending".into());
            row.lifecycle = None;
            if let Some(error) = &project_warning {
                view.badge = None;
                view.project_id = None;
                view.project = Observation::Unavailable(error.clone());
            }
        }
    }
    model.warning = initial_warning;
    Ok(model)
}

fn profile_login_startup(result: anyhow::Result<()>) -> ProfileManagerStartup {
    match result {
        Ok(()) => ProfileManagerStartup {
            notice: Some("Profile added".to_string()),
            warning: None,
        },
        Err(error) => ProfileManagerStartup {
            notice: None,
            warning: Some(format!("Profile login did not complete: {error:#}")),
        },
    }
}

fn require_interactive_terminal(
    stdin_is_terminal: bool,
    stdout_is_terminal: bool,
) -> anyhow::Result<()> {
    if stdin_is_terminal && stdout_is_terminal {
        Ok(())
    } else {
        anyhow::bail!("`cutex tui` requires an interactive terminal on stdin and stdout")
    }
}

fn spawn_snapshot_refresh() -> anyhow::Result<WorkspaceLoad<SelectorSnapshot>> {
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("cutex-tui-refresh".to_string())
        .spawn(move || {
            let snapshot =
                load_live_snapshot().map_err(|error| format!("live refresh failed: {error:#}"));
            let _ = sender.send(snapshot);
        })
        .context("Failed to start Cutex TUI refresh worker")?;
    Ok(WorkspaceLoad::new(receiver))
}

fn spawn_runtime_close(
    intent: SessionTuiIntent,
) -> anyhow::Result<Receiver<RuntimeCloseWorkerResult>> {
    debug_assert!(intent_runs_in_selector(&intent));
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("cutex-tui-close".to_string())
        .spawn(move || {
            let result = match super::session_tui_dispatch::dispatch_session_tui_intent_in_selector(
                intent,
            ) {
                Ok(()) => match load_live_snapshot() {
                    Ok(snapshot) => RuntimeCloseWorkerResult::Closed(snapshot),
                    Err(error) => {
                        RuntimeCloseWorkerResult::ClosedRefreshFailed(format!("{error:#}"))
                    }
                },
                Err(error) => RuntimeCloseWorkerResult::Failed(format!("{error:#}")),
            };
            let _ = sender.send(result);
        })
        .context("Failed to start Cutex TUI runtime close worker")?;
    Ok(receiver)
}

fn load_live_snapshot() -> anyhow::Result<SelectorSnapshot> {
    let store = load_reconciled_session_store()?;
    let (alden_sessions, alden_warning) = match cute_alden_sessions() {
        Ok(sessions) => (sessions, None),
        Err(error) => (
            Vec::new(),
            Some(format!("cute-alden live state unavailable: {error:#}")),
        ),
    };
    let (profile_names, profile_warning) = profile_names_with_warning();
    let (activity_states, activity_warning) = activity_states_with_warning();
    let (project_contexts, project_warning) = project_contexts_with_warning();
    let (compaction_counts, compaction_warning) = compaction_counts_with_warning();
    let config = load_codez_config();
    let (live_agents, bus_warning) = match cutex::agent_bus::client::agent_bus_fetch_agents(&config)
    {
        Ok(agents) => (agents, None),
        Err(error) => (
            Vec::new(),
            Some(format!("runtime bus unavailable: {error:#}")),
        ),
    };
    let mut rows = selector_rows_from_store(
        &store,
        &alden_sessions,
        &live_agents,
        &config,
        &profile_names,
        &activity_states,
        &project_contexts,
        &compaction_counts,
    );
    for row in &mut rows {
        if let Some(view) = row.view.as_mut() {
            if let Some(error) = alden_warning.as_ref().or(bus_warning.as_ref()) {
                view.runtime = Observation::Unavailable(error.clone());
                row.lifecycle = None;
            }
            if let Some(error) = &project_warning {
                view.badge = None;
                view.project_id = None;
                view.project = Observation::Unavailable(error.clone());
            }
        }
    }
    enrich_provider_views(&mut rows, &store);
    let snapshot_warning = combine_warnings(
        combine_warnings(
            combine_warnings(
                combine_warnings(alden_warning, bus_warning),
                profile_warning,
            ),
            activity_warning,
        ),
        compaction_warning,
    );
    Ok(SelectorSnapshot {
        rows,
        warning: combine_warnings(snapshot_warning, project_warning),
    })
}

type CompactionCounts = HashMap<(String, String), Option<u64>>;

fn compaction_counts_with_warning() -> (CompactionCounts, Option<String>) {
    match cutex::management::v2::usage::load_usage_ledger() {
        Ok(ledger) => (compaction_counts_from_ledger(&ledger), None),
        Err(error) => (
            HashMap::new(),
            Some(format!("usage ledger unavailable: {error:#}")),
        ),
    }
}

fn compaction_counts_from_ledger(
    ledger: &cutex::management::v2::usage::UsageLedger,
) -> CompactionCounts {
    let mut counts = CompactionCounts::new();
    for entry in &ledger.entries {
        if let cutex::management::v2::usage::UsageLedgerEntry::Usage(sample) = entry {
            counts.insert(
                (sample.cutex_session_id.clone(), sample.thread_id.clone()),
                sample.compaction_count,
            );
        }
    }
    counts
}

fn enrich_provider_views(rows: &mut [SelectorRow], durable: &CutexSessionStore) {
    let provider =
        AgentManagementStore::open_default().and_then(|s| s.snapshot().map_err(anyhow::Error::new));
    apply_provider_views(rows, durable, &provider);
}
fn apply_provider_views(
    rows: &mut [SelectorRow],
    durable: &CutexSessionStore,
    provider: &anyhow::Result<AgentManagementSnapshot>,
) {
    for row in rows {
        let Some(view) = row.view.as_mut() else {
            continue;
        };
        let SubjectRef::Managed(id) = &view.subject else {
            continue;
        };
        match provider {
            Err(error) => {
                view.badge = None;
                view.project_id = None;
                view.role.clear();
                view.project =
                    Observation::Unavailable(format!("Project provider unavailable: {error:#}"))
            }
            Ok(snapshot) => {
                let agent = snapshot
                    .agents
                    .values()
                    .find(|agent| agent.cutex_session_id.as_str() == id);
                if let Some(agent) = agent {
                    let record = durable
                        .sessions
                        .values()
                        .find(|r| r.cutex_session_id == *id);
                    view.name = record
                        .and_then(|r| r.formal_agent_name.clone())
                        .unwrap_or_else(|| agent.spec.name.clone());
                    row.agent = view.name.clone();
                    let project_id = cutex::agent_management::current_project_id(snapshot, agent);
                    view.project_id = project_id.as_ref().map(ToString::to_string);
                    let presentation = project_id.as_ref().map(|id| {
                        effective_presentation(id, snapshot.project_presentations.get(id))
                    });
                    view.badge = presentation.as_ref().map(|p| views::ProjectBadge {
                        label: p.badge_label.clone(),
                        color: p.color,
                    });
                    view.project = Observation::Known(
                        presentation
                            .map(|p| p.display_name)
                            .unwrap_or_else(|| "-".into()),
                    );
                    let mut roles = Vec::new();
                    if snapshot
                        .projects
                        .values()
                        .any(|p| p.authorized_director_session.as_str() == id)
                    {
                        roles.push("Director");
                    }
                    if snapshot
                        .operator_grants
                        .values()
                        .any(|grants| grants.keys().any(|sid| sid.as_str() == id))
                    {
                        roles.push("Operator");
                    }
                    if roles.is_empty() && project_id.is_some() {
                        roles.push("Member");
                    }
                    view.role = roles.join("/");
                    if agent.retired_at.is_some() {
                        view.retirement_note =
                            Some("Permanently retired roster history; not restorable".into());
                    } else if record.is_some_and(|r| r.is_retired()) {
                        view.retirement_note =
                            Some("Reversibly archived; current Project membership retained".into());
                    }
                } else {
                    view.badge = None;
                    view.project_id = None;
                    view.role.clear();
                    view.project = Observation::Known("-".into());
                }
            }
        }
    }
}

fn load_reconciled_session_store() -> anyhow::Result<CutexSessionStore> {
    load_reconciled_session_store_with(
        cutex::im::registry::load_im_registry,
        super::session_reconcile::mirror_im_registry_into_cutex_session_store,
        load_cutex_session_store,
    )
}

fn load_reconciled_session_store_with(
    load_registry: impl FnOnce() -> anyhow::Result<cutex::im::registry::ImRegistry>,
    reconcile: impl FnOnce(&cutex::im::registry::ImRegistry) -> anyhow::Result<()>,
    load_store: impl FnOnce() -> anyhow::Result<CutexSessionStore>,
) -> anyhow::Result<CutexSessionStore> {
    let registry = load_registry()?;
    reconcile(&registry)?;
    load_store()
}

fn load_retired_selector_rows() -> anyhow::Result<Vec<SelectorRow>> {
    let store = load_cutex_session_store()?;
    let snapshot = AgentManagementStore::open_default()?.snapshot()?;
    let contexts = selector_project_contexts(&snapshot);
    let mut rows = retired_selector_rows_from_store(&store, &contexts);
    for row in &mut rows {
        if row.target.agent_key().is_some_and(|id| {
            snapshot
                .agents
                .values()
                .any(|agent| agent.cutex_session_id.as_str() == id && agent.retired_at.is_some())
        }) {
            row.actions.clear();
        }
    }
    Ok(rows)
}

fn profile_names_with_warning() -> (Vec<String>, Option<String>) {
    match load_profile_names_read_only() {
        Ok(names) => (names, None),
        Err(error) => (
            Vec::new(),
            Some(format!("profile catalog unavailable: {error:#}")),
        ),
    }
}

fn activity_states_with_warning() -> (HashMap<String, SessionActivityState>, Option<String>) {
    match load_session_activity_states() {
        Ok(states) => (states, None),
        Err(error) => (
            HashMap::new(),
            Some(format!("session activity unavailable: {error:#}")),
        ),
    }
}

fn project_contexts_with_warning() -> (HashMap<String, SelectorProjectContext>, Option<String>) {
    match AgentManagementStore::open_default()
        .and_then(|store| store.snapshot().map_err(anyhow::Error::new))
    {
        Ok(snapshot) => {
            let mut contexts = selector_project_contexts(&snapshot);
            if let Ok(sessions) = load_cutex_session_store() {
                for (id, context) in &mut contexts {
                    if let Some(name) = sessions
                        .sessions
                        .get(id)
                        .filter(|record| &record.cutex_session_id == id)
                        .and_then(|record| record.formal_agent_name.as_ref())
                    {
                        context.agent_name = name.clone();
                    }
                }
            }
            (contexts, None)
        }
        Err(error) => (
            HashMap::new(),
            Some(format!("Project context unavailable: {error:#}")),
        ),
    }
}

fn selector_project_contexts(
    snapshot: &AgentManagementSnapshot,
) -> HashMap<String, SelectorProjectContext> {
    snapshot
        .agents
        .iter()
        .filter_map(|(cutex_session_id, agent)| {
            let project_id = cutex::agent_management::current_project_id(snapshot, agent)?;
            let presentation = effective_presentation(
                &project_id,
                snapshot.project_presentations.get(&project_id),
            );
            Some((
                cutex_session_id.as_str().to_string(),
                SelectorProjectContext {
                    agent_name: agent.spec.name.clone(),
                    project_id: project_id.as_str().to_string(),
                    display_name: presentation.display_name,
                    badge_label: presentation.badge_label,
                    color: presentation.color,
                },
            ))
        })
        .collect()
}

fn managed_names_by_native_session_id() -> HashMap<String, String> {
    AgentManagementStore::open_default()
        .and_then(|store| store.snapshot().map_err(anyhow::Error::new))
        .map(|snapshot| {
            let sessions = load_cutex_session_store().unwrap_or_default();
            snapshot
                .agents
                .values()
                .map(|agent| {
                    (
                        agent.native_session_id.clone(),
                        sessions
                            .sessions
                            .get(agent.cutex_session_id.as_str())
                            .filter(|record| {
                                record.cutex_session_id == agent.cutex_session_id.as_str()
                            })
                            .and_then(|record| record.formal_agent_name.clone())
                            .unwrap_or_else(|| agent.spec.name.clone()),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn combine_warnings(left: Option<String>, right: Option<String>) -> Option<String> {
    match (left, right) {
        (Some(left), Some(right)) => Some(format!("{left}; {right}")),
        (Some(warning), None) | (None, Some(warning)) => Some(warning),
        (None, None) => None,
    }
}

fn selector_row_matches_query(row: &SelectorRow, query: &str) -> bool {
    row.agent.to_lowercase().contains(query)
        || row.thread_title.as_ref().is_some_and(|s| s.to_lowercase().contains(query))
        || row.target.agent_key().is_some_and(|id| id.to_lowercase().contains(query))
}

fn selector_activity_from_state(state: &SessionActivityState) -> Option<SelectorActivity> {
    let output = state
        .last_output
        .as_ref()
        .and_then(|projection| {
            parse_selector_activity_timestamp(&projection.updated_at).map(|timestamp| {
                (
                    timestamp,
                    SelectorActivity {
                        class: SelectorActivityClass::Output,
                        updated_at: projection.updated_at.clone(),
                        failed: false,
                    },
                )
            })
        })
        .or_else(|| {
            state.last_output_at.as_deref().and_then(|updated_at| {
                parse_selector_activity_timestamp(updated_at).map(|timestamp| {
                    (
                        timestamp,
                        SelectorActivity {
                            class: SelectorActivityClass::Output,
                            updated_at: updated_at.to_string(),
                            failed: false,
                        },
                    )
                })
            })
        });
    let tool = state.last_tool_call.as_ref().and_then(|projection| {
        parse_selector_activity_timestamp(&projection.updated_at).map(|timestamp| {
            (
                timestamp,
                SelectorActivity {
                    class: selector_activity_class_for_tool(projection.class),
                    updated_at: projection.updated_at.clone(),
                    failed: projection.status == SafeToolCallStatus::Failed,
                },
            )
        })
    });

    match (output, tool) {
        (Some((output_at, output)), Some((tool_at, tool))) => {
            // A tie always prefers output, so malformed ordering metadata cannot make the
            // homepage projection flicker between refreshes.
            Some(if tool_at > output_at { tool } else { output })
        }
        (Some((_, output)), None) => Some(output),
        (None, Some((_, tool))) => Some(tool),
        (None, None) => None,
    }
}

fn parse_selector_activity_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn selector_activity_class_for_tool(class: SafeToolCallClass) -> SelectorActivityClass {
    match class {
        SafeToolCallClass::Command => SelectorActivityClass::Command,
        SafeToolCallClass::McpTool => SelectorActivityClass::Mcp,
        SafeToolCallClass::DynamicTool => SelectorActivityClass::Tool,
        SafeToolCallClass::CollaborationTool => SelectorActivityClass::Agent,
        SafeToolCallClass::FileChange => SelectorActivityClass::Edit,
        SafeToolCallClass::ImageView => SelectorActivityClass::Image,
    }
}

fn selector_rows_from_store(
    store: &CutexSessionStore,
    alden_sessions: &[CuteAldenSession],
    live_agents: &[AgentBusAgent],
    config: &CodezConfig,
    profile_names: &[String],
    activity_states: &HashMap<String, SessionActivityState>,
    project_contexts: &HashMap<String, SelectorProjectContext>,
    compaction_counts: &CompactionCounts,
) -> Vec<SelectorRow> {
    let native_versions = cutex::launch::version_registry::load_relaxed();
    let mut rows = store
        .sessions
        .iter()
        .filter(|(_, record)| record.is_active() && cutex_session_is_managed(record))
        .map(|(key, record)| {
            let mut row = selector_row(key, record, alden_sessions, live_agents, profile_names);
            row.activity = activity_states
                .get(&record.cutex_session_id)
                .and_then(selector_activity_from_state);
            if row.managed {
                if let Some(context) = project_contexts.get(&record.cutex_session_id) {
                    // Managed identity is owned by Agent Management. Native
                    // thread titles remain optional secondary presentation.
                    row.agent = record
                        .formal_agent_name
                        .clone()
                        .unwrap_or_else(|| context.agent_name.clone());
                    row.project = Some(context.clone());
                } else {
                    // Explicit durable formal names survive missing membership
                    // projections. Legacy hints remain display-only fallback.
                    row.agent = managed_session_fallback_name(record);
                }
            }
            row.view = Some(selector_view(&row, config.default_profile.as_deref()));
            row.view.as_mut().unwrap().native_thread = record.codex_session_id.clone();
            row.view.as_mut().unwrap().compaction_count = record
                .codex_session_id
                .as_deref()
                .and_then(|thread_id| {
                    compaction_counts
                        .get(&(record.cutex_session_id.clone(), thread_id.to_owned()))
                        .cloned()
                })
                .flatten();
            row.view.as_mut().unwrap().cwd = record
                .managed_cwd
                .clone()
                .unwrap_or_else(|| record.cwd.clone());
            apply_runtime_version_projection(
                row.view.as_mut().unwrap(),
                record,
                native_versions.as_ref(),
            );
            let observations: Vec<_> = live_agents
                .iter()
                .filter(|agent| {
                    agent.cutex_session_id.as_deref() == Some(record.cutex_session_id.as_str())
                        && agent.session_id == record.codex_session_id
                        && record.codex_session_id.is_some()
                })
                .collect();
            if observations.len() == 1 {
                row.view.as_mut().unwrap().effective_profile =
                    Observation::Known(observations[0].profile.clone());
            }
            row
        })
        .collect::<Vec<_>>();
    rows.push(retired_sessions_row(
        store
            .sessions
            .values()
            .filter(|record| record.is_retired() && cutex_session_is_managed(record))
            .count(),
    ));
    rows.push(projects_row());
    rows.push(profiles_row(config, profile_names));
    rows.push(global_settings_row_with_profiles(config, profile_names));
    rows
}

fn apply_runtime_version_projection(
    view: &mut AgentSessionView,
    record: &CutexSessionRecord,
    native_versions: Option<&cutex::launch::version_registry::NativeVersionRegistry>,
) {
    let local = cutex::runtime::lifecycle::cutex_session_host_is_local(
        &record.host_id,
        &cutex::platform::host::current_host_name(),
    );
    if !local {
        view.runtime_version = None;
        view.runtime_version_latest = None;
        view.runtime_version_recorded_at = None;
        return;
    }
    view.runtime_version = native_versions.and_then(|registry| {
        record
            .app_server_runtime
            .as_ref()
            .and_then(|binding| registry.runtime_version_for_pid(binding.pid))
            .or_else(|| {
                record
                    .runtime_pid
                    .and_then(|pid| registry.runtime_version_for_pid(pid))
            })
            .or_else(|| {
                record
                    .explicit_launch
                    .as_ref()
                    .and_then(|contract| registry.runtime_version_for_launch_contract(contract))
            })
    });
    if let Some(registry) = native_versions {
        view.runtime_version_latest = Some(registry.latest_runtime.version.clone());
        view.runtime_version_recorded_at = Some(registry.latest_runtime.recorded_at.clone());
    }
}

fn retired_selector_rows_from_store(
    store: &CutexSessionStore,
    project_contexts: &HashMap<String, SelectorProjectContext>,
) -> Vec<SelectorRow> {
    let mut rows = store
        .sessions
        .iter()
        .filter(|(_, record)| record.is_retired())
        .map(|(key, record)| {
            retired_selector_row(key, record, project_contexts.get(&record.cutex_session_id))
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| left.agent.cmp(&right.agent));
    rows
}

fn retired_selector_row(
    key: &str,
    record: &CutexSessionRecord,
    context: Option<&SelectorProjectContext>,
) -> SelectorRow {
    SelectorRow {
        view: None,
        target: SelectorTarget::RetiredAgent(key.to_string()),
        agent: record
            .formal_agent_name
            .clone()
            .or_else(|| context.map(|context| context.agent_name.clone()))
            .unwrap_or_else(|| managed_session_fallback_name(record)),
        thread_title: record.thread_name.clone(),
        project: context.cloned(),
        configured_profile: record.profile.clone(),
        lifecycle: Some(CutexSessionLifecycleState::Offline),
        host: nonempty_or_dash(&record.host_id),
        backend: runtime_backend_short_label(record.runtime_backend).to_string(),
        managed_path: record
            .managed_cwd
            .as_deref()
            .map(compact_home_path)
            .unwrap_or_else(|| "-".to_string()),
        retired_at: record.retired_at.clone(),
        revision: record.durable_revision(),
        activity_session_id: None,
        activity: None,
        actions: vec![SessionTuiActionItem {
            action: SessionTuiAction::RestoreSession,
            detail: "Restore active and offline without launching",
            primary: true,
        }],
        settings: Vec::new(),
        settings_snapshot: None,
        global_settings_snapshot: None,
        attachable: false,
        pinned: false,
        managed: true,
    }
}

fn managed_session_fallback_name(record: &CutexSessionRecord) -> String {
    [record.formal_agent_name.as_deref(), record.display_name_hint.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| record.cutex_session_id.clone())
}

fn selector_row(
    key: &str,
    record: &CutexSessionRecord,
    alden_sessions: &[CuteAldenSession],
    live_agents: &[AgentBusAgent],
    profile_names: &[String],
) -> SelectorRow {
    let settings_snapshot =
        SessionSettingsSnapshot::from_record_with_profiles(record, profile_names);
    let settings = settings_snapshot.categories(&SessionSettingsDraft::default());
    let mut actions = session_tui_actions_for_record(record, alden_sessions, live_agents);
    if cutex_session_is_managed(record) {
        actions.push(SessionTuiActionItem {
            action: SessionTuiAction::RetireSession,
            detail: "Archive this managed session after proving its runtime is offline",
            primary: false,
        });
    }
    SelectorRow {
        view: None,
        target: SelectorTarget::Agent(key.to_string()),
        agent: if cutex_session_is_managed(record) {
            managed_session_fallback_name(record)
        } else {
            cutex_session_display_name(record)
        },
        thread_title: cutex_session_is_managed(record)
            .then(|| record.thread_name.clone())
            .flatten(),
        project: None,
        configured_profile: record.profile.clone(),
        lifecycle: Some(cutex_session_lifecycle_state_with_agents(
            record,
            alden_sessions,
            live_agents,
        )),
        host: nonempty_or_dash(&record.host_id),
        backend: runtime_backend_short_label(record.runtime_backend).to_string(),
        managed_path: record
            .managed_cwd
            .as_deref()
            .map(compact_home_path)
            .unwrap_or_else(|| "-".to_string()),
        retired_at: None,
        revision: record.durable_revision(),
        activity_session_id: Some(record.cutex_session_id.clone()),
        activity: None,
        actions,
        settings,
        settings_snapshot: Some(settings_snapshot),
        global_settings_snapshot: None,
        attachable: cutex_session_is_attachable(record, alden_sessions),
        pinned: record.quick_action == CutexSessionQuickActionMode::Pinned,
        managed: cutex_session_is_managed(record),
    }
}

fn retired_sessions_row(retired_count: usize) -> SelectorRow {
    SelectorRow {
        view: None,
        target: SelectorTarget::RetiredSessions,
        agent: format!("Archived Agents ({retired_count})"),
        thread_title: None,
        project: None,
        configured_profile: None,
        lifecycle: None,
        host: "-".to_string(),
        backend: "archive".to_string(),
        managed_path: "-".to_string(),
        retired_at: None,
        revision: 0,
        activity_session_id: None,
        activity: None,
        actions: Vec::new(),
        settings: Vec::new(),
        settings_snapshot: None,
        global_settings_snapshot: None,
        attachable: false,
        pinned: false,
        managed: false,
    }
}

fn projects_row() -> SelectorRow {
    SelectorRow {
        view: None,
        target: SelectorTarget::Projects,
        agent: "Workspaces".to_string(),
        thread_title: None,
        project: None,
        configured_profile: None,
        lifecycle: None,
        host: "-".to_string(),
        backend: "Codex catalog".to_string(),
        managed_path: "paired app-server".to_string(),
        retired_at: None,
        revision: 0,
        activity_session_id: None,
        activity: None,
        actions: Vec::new(),
        settings: Vec::new(),
        settings_snapshot: None,
        global_settings_snapshot: None,
        attachable: false,
        pinned: false,
        managed: false,
    }
}

#[cfg(test)]
fn cutex_projects_row() -> SelectorRow {
    SelectorRow {
        view: None,
        target: SelectorTarget::CutexProjects,
        agent: "Cutex Projects".to_string(),
        thread_title: None,
        project: None,
        configured_profile: None,
        lifecycle: None,
        host: "-".to_string(),
        backend: "permission model".to_string(),
        managed_path: "Agent Management provider".to_string(),
        retired_at: None,
        revision: 0,
        activity_session_id: None,
        activity: None,
        actions: Vec::new(),
        settings: Vec::new(),
        settings_snapshot: None,
        global_settings_snapshot: None,
        attachable: false,
        pinned: false,
        managed: false,
    }
}

#[cfg(test)]
fn recent_sessions_row() -> SelectorRow {
    SelectorRow {
        view: None,
        target: SelectorTarget::RecentSessions,
        agent: "Recent sessions".to_string(),
        thread_title: None,
        project: None,
        configured_profile: None,
        lifecycle: None,
        host: "-".to_string(),
        backend: "native catalog".to_string(),
        managed_path: "-".to_string(),
        retired_at: None,
        revision: 0,
        activity_session_id: None,
        activity: None,
        actions: Vec::new(),
        settings: Vec::new(),
        settings_snapshot: None,
        global_settings_snapshot: None,
        attachable: false,
        pinned: false,
        managed: false,
    }
}

fn settings_actions_for_record(
    record: &CutexSessionRecord,
    currently_attachable: bool,
) -> Vec<SessionTuiActionItem> {
    let alden_sessions = if currently_attachable {
        record
            .alden_pid
            .zip(record.alden_session_name.clone())
            .map(|(pid, name)| CuteAldenSession {
                pid,
                name: Some(name),
            })
            .into_iter()
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    session_tui_actions_for_record(record, &alden_sessions, &[])
}

#[cfg(test)]
fn global_settings_row(config: &CodezConfig) -> SelectorRow {
    global_settings_row_with_profiles(config, &[])
}

fn global_settings_row_with_profiles(
    config: &CodezConfig,
    profile_names: &[String],
) -> SelectorRow {
    let settings_snapshot =
        GlobalSettingsSnapshot::from_config_with_profiles(config, profile_names);
    SelectorRow {
        view: None,
        target: SelectorTarget::GlobalSettings,
        agent: "Global settings".to_string(),
        thread_title: None,
        project: None,
        configured_profile: None,
        lifecycle: None,
        host: "-".to_string(),
        backend: "config".to_string(),
        managed_path: "-".to_string(),
        retired_at: None,
        revision: 0,
        activity_session_id: None,
        activity: None,
        actions: Vec::new(),
        settings: settings_snapshot.categories(&GlobalSettingsDraft::default()),
        settings_snapshot: None,
        global_settings_snapshot: Some(settings_snapshot),
        attachable: false,
        pinned: false,
        managed: false,
    }
}

fn profiles_row(config: &CodezConfig, profile_names: &[String]) -> SelectorRow {
    let settings_snapshot =
        GlobalSettingsSnapshot::from_config_with_profiles(config, profile_names);
    SelectorRow {
        view: None,
        target: SelectorTarget::Profiles,
        agent: "Profiles".to_string(),
        thread_title: None,
        project: None,
        configured_profile: None,
        lifecycle: None,
        host: "-".to_string(),
        backend: "accounts".to_string(),
        managed_path: "-".to_string(),
        retired_at: None,
        revision: 0,
        activity_session_id: None,
        activity: None,
        actions: Vec::new(),
        settings: Vec::new(),
        settings_snapshot: None,
        global_settings_snapshot: Some(settings_snapshot),
        attachable: false,
        pinned: false,
        managed: false,
    }
}

fn nonempty_or_dash(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        "-".to_string()
    } else {
        value.to_string()
    }
}

fn sort_rows(rows: &mut [SelectorRow]) {
    rows.sort_by(|left, right| {
        match (
            system_row_rank(&left.target),
            system_row_rank(&right.target),
        ) {
            (Some(left), Some(right)) => left.cmp(&right),
            (Some(_), None) => std::cmp::Ordering::Greater,
            (None, Some(_)) => std::cmp::Ordering::Less,
            // Initial and failed runtime snapshots intentionally leave lifecycle
            // unknown. Keep those rows after observed states, never panic.
            (None, None) => left.lifecycle.map(lifecycle_rank).unwrap_or(u8::MAX)
                .cmp(&right.lifecycle.map(lifecycle_rank).unwrap_or(u8::MAX))
                .then_with(|| right.pinned.cmp(&left.pinned))
                .then_with(|| left.agent.to_lowercase().cmp(&right.agent.to_lowercase()))
                .then_with(|| left.target.agent_key().cmp(&right.target.agent_key())),
        }
    });
}

fn system_row_rank(target: &SelectorTarget) -> Option<u8> {
    match target {
        SelectorTarget::Agent(_) | SelectorTarget::RemoteAgent(..) | SelectorTarget::RetiredAgent(_) => None,
        SelectorTarget::RecentSessions => Some(0),
        SelectorTarget::RetiredSessions => Some(1),
        SelectorTarget::CutexProjects => Some(2),
        SelectorTarget::Projects => Some(3),
        SelectorTarget::Profiles => Some(5),
        SelectorTarget::GlobalSettings => Some(6),
    }
}

fn lifecycle_rank(state: CutexSessionLifecycleState) -> u8 {
    match state {
        CutexSessionLifecycleState::Online => 0,
        CutexSessionLifecycleState::Stale => 1,
        CutexSessionLifecycleState::Offline => 2,
    }
}

fn open_terminal() -> anyhow::Result<(CutexTerminal, TerminalRestore, bool)> {
    enable_raw_mode().context("Failed to enable terminal raw mode")?;
    let enhanced_keyboard = terminal_may_support_enhanced_keyboard()
        && supports_keyboard_enhancement().unwrap_or(false);
    let mut restore = TerminalRestore {
        enhanced_keyboard: false,
    };
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)
        .context("Failed to enter alternate screen")?;
    if enhanced_keyboard {
        execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .context("Failed to enable enhanced keyboard events")?;
        restore.enhanced_keyboard = true;
    }
    let terminal = Terminal::new(CrosstermBackend::new(stdout))
        .context("Failed to initialize Cutex TUI terminal")?;
    Ok((terminal, restore, enhanced_keyboard))
}

fn terminal_may_support_enhanced_keyboard() -> bool {
    terminal_environment_may_support_enhancement(
        std::env::var("TERM").ok().as_deref(),
        std::env::var("TERM_PROGRAM").ok().as_deref(),
        [
            "KITTY_WINDOW_ID",
            "WEZTERM_PANE",
            "ALACRITTY_WINDOW_ID",
            "FOOT_CLIENT_PID",
        ]
        .iter()
        .any(|key| std::env::var_os(key).is_some()),
    )
}

fn terminal_environment_may_support_enhancement(
    term: Option<&str>,
    term_program: Option<&str>,
    known_terminal_marker: bool,
) -> bool {
    known_terminal_marker
        || [term, term_program]
            .into_iter()
            .flatten()
            .map(str::to_ascii_lowercase)
            .any(|value| {
                ["kitty", "foot", "wezterm", "alacritty"]
                    .iter()
                    .any(|name| value.contains(name))
            })
}

struct TerminalRestore {
    enhanced_keyboard: bool,
}

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        if self.enhanced_keyboard {
            let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        }
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            LeaveAlternateScreen,
            Show
        );
        let _ = disable_raw_mode();
    }
}

/// The only terminal owner for Managed/Recent/Projects. The synchronous loops
/// borrow it serially; no event-reader thread survives a handoff. Page models
/// are retained in `run`, rather than reconstructed from array indices.
struct TerminalShell {
    active: Option<(CutexTerminal, TerminalRestore, bool)>,
}

/// One synchronous source, borrowed by exactly one page loop at a time.
/// There is no pending input thread to race foreground/legacy handoffs.
pub(super) struct ShellEvents;
impl ShellEvents {
    pub(super) fn next(&mut self) -> anyhow::Result<Option<Event>> {
        if event::poll(EVENT_POLL_INTERVAL)? {
            Ok(Some(event::read()?))
        } else {
            Ok(None)
        }
    }
}

impl TerminalShell {
    fn open() -> anyhow::Result<Self> {
        Ok(Self {
            active: Some(open_terminal()?),
        })
    }
    fn terminal(&mut self) -> &mut CutexTerminal {
        &mut self.active.as_mut().expect("terminal is suspended").0
    }
    fn enhanced_keyboard(&self) -> bool {
        self.active.as_ref().is_some_and(|active| active.2)
    }
    fn handoff<T>(
        &mut self,
        effect: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<anyhow::Result<T>> {
        drop(self.active.take());
        let result = effect();
        // Always restore after both child success and error. If reopening
        // fails its guard cleans up; callers must not run another reader.
        self.active = Some(open_terminal()?);
        Ok(result)
    }
}

enum SelectorKeyRoute {
    Switch(PrimaryPanel),
    Refresh,
    Control(Option<SelectorControl>),
}

/// Shared by the terminal loop and key-sequence tests; effects run only after routing.
fn selector_input(model: &mut SelectorModel) -> Option<&mut Input> {
    if matches!(model.mode, SelectorMode::RecentSessions) && model.recent.review().is_some() {
        return model.recent.adoption_name_input();
    }
    if matches!(model.mode, SelectorMode::RecentSessions) && model.recent.filter_focused() && model.recent.facets.field == 0 {
        return Some(model.recent.filter_input_mut());
    }
    if matches!(model.mode, SelectorMode::Agents) && model.filter_focused && model.facets.field == 0 {
        return Some(&mut model.query);
    }
    if !matches!(
        model.mode,
        SelectorMode::Settings { .. } | SelectorMode::ProfileManager { .. }
    ) {
        return None;
    }
    if let Some(overlay) = model.settings_overlay.as_mut() {
        match overlay {
            SettingsOverlay::Text { input, .. } => return Some(input),
            SettingsOverlay::Groups {
                inputs, selected, ..
            } => return inputs.get_mut(*selected),
            _ => {}
        }
    }
    if let Some(ProfileOverlay::RenameInput { input, .. }) = model.profile_overlay.as_mut() {
        return Some(input);
    }
    None
}

fn selector_dirty(model: &SelectorModel) -> bool {
    model.settings_draft.is_dirty()
        || model.global_settings_draft.is_dirty()
        || model.profile_settings_draft.is_dirty()
        || matches!(
            model.settings_overlay,
            Some(SettingsOverlay::Text { .. } | SettingsOverlay::Groups { .. })
        )
        || matches!(
            model.profile_overlay,
            Some(ProfileOverlay::RenameInput { .. })
        )
}
fn selector_modal(model: &SelectorModel) -> bool {
    model.native_actions.is_some() || model.native_archive_pending || matches!(model.mode, SelectorMode::ConfirmRuntimeAction { .. })
        || (matches!(model.mode, SelectorMode::RecentSessions) && model.recent.review().is_some())
        || model.action_overlay.is_some()
        || model.settings_overlay.as_ref().is_some_and(|o| {
            !matches!(
                o,
                SettingsOverlay::Text { .. } | SettingsOverlay::Groups { .. }
            )
        })
        || model
            .profile_overlay
            .as_ref()
            .is_some_and(|o| !matches!(o, ProfileOverlay::RenameInput { .. }))
}
fn settings_navigation_commands() -> Vec<(Command, Option<&'static str>)> {
    [
        Command::Settings,
        Command::Profiles,
        Command::Appearance,
        Command::Workspaces,
        Command::Archive,
    ]
    .into_iter()
    .map(|command| (command, None))
    .collect()
}
fn selector_commands(model: &SelectorModel) -> Vec<(Command, Option<&'static str>)> {
    input_policy::BINDINGS
        .iter()
        .map(|b| {
            let reason = match b.command {
                Command::Archived => Some("Available on Projects"),
                Command::NewProject | Command::NewManagedAgent
                    if !matches!(model.mode, SelectorMode::Agents | SelectorMode::RecentSessions) =>
                    Some("Available on Agents / Sessions"),
                Command::Sort if !matches!(model.mode, SelectorMode::Agents | SelectorMode::RecentSessions) => Some("Available on Agents / Sessions"),
                Command::Scope if !matches!(model.mode, SelectorMode::Agents) => {
                    Some("Available on Managed")
                }
                Command::NewManagedAgent if cutex::launch::local_deployment::LocalDeployment::selected().ok().flatten().is_none() && cutex::runtime::claude::ClaudeDeployment::selected().is_err() => Some("Install a local runtime to create an Agent"),
                Command::LoadMore
                    if !matches!(model.mode, SelectorMode::RecentSessions)
                        || model.recent.next_cursor().is_none()
                        || model.recent.loading() =>
                {
                    Some("No next Recent page")
                }
                Command::Inspect if matches!(model.mode, SelectorMode::RetiredSessions { .. }) => {
                    model.active_row().is_none().then_some("Select an archived identity")
                }
                Command::Actions | Command::Inspect | Command::Edit | Command::Titles
                    if !matches!(model.mode, SelectorMode::RecentSessions)
                        && model.selected_managed_agent().is_none() && remote_projection::selected(model).is_none() =>
                {
                    Some("Select an Agent")
                }
                Command::Titles
                    if matches!(model.mode, SelectorMode::RecentSessions) =>
                {
                    Some("Use Managed for this command")
                }
                _ => None,
            };
            (b.command, reason)
        })
        .collect()
}
fn selector_command(model: &mut SelectorModel, command: Command) -> SelectorKeyRoute {
    if let Some(e)=remote_projection::selected(model) {match command {
        Command::Actions=>return SelectorKeyRoute::Control(Some(SelectorControl::RemoteBrowse(e.connection,e.session.id))),
        Command::Edit=>{model.notice=Some("Edit on the owning host; Alt+A opens remote runtime actions".into());return SelectorKeyRoute::Control(None);},
        _=>{}
    }}
    let command = if command == Command::Back && model.profiles_from_settings
        && matches!(model.mode, SelectorMode::ProfileManager { .. }) {
        Command::Settings
    } else { command };
    if command == Command::Details {
        model.status_scroll.reset();
        model.details = Some(format!(
            "Current review: {:?}\nArchive confirmation: {:?}\nStock runtime review: {:?}\nRecent adoption: {:?}\nFormal name input: {}\nRecent: {:?}\nNotice: {}\nStatus: {}\nRemote connections: {}",
            model.mode,
            model.archive_confirmation,
            model.stock_runtime_confirmation.as_ref().map(|request| (
                request.action_id.as_str(),
                request.review.subject.cutex_session_id.as_str(),
                request.review.restart,
                request.review.subject.runtime_generation,
            )),
            model.recent.review(),
            model.recent.adoption_name().map(|i| i.value()).unwrap_or("not editing"),
            model.recent.load_state(),
            model.notice.as_deref().unwrap_or("None"),
            model.warning.as_deref().unwrap_or("No error"),
            if model.remote_warnings.is_empty() { "No error".into() } else { model.remote_warnings.values().cloned().collect::<Vec<_>>().join("\n") },
        ));
        return SelectorKeyRoute::Control(None);
    }
    if let Some((_, Some(reason))) = selector_commands(model)
        .into_iter()
        .find(|(c, _)| *c == command)
    {
        model.notice = Some(reason.into());
        return SelectorKeyRoute::Control(None);
    }
    let navigation = matches!(
        command,
        Command::Page(_)
            | Command::Settings
            | Command::Exit
            | Command::Back
            | Command::Actions
            | Command::Edit
            | Command::Inspect
            | Command::Profiles
            | Command::Hosts
            | Command::Services
            | Command::Workspaces
            | Command::Archive
            | Command::Appearance
    );
    if navigation {
        match input_policy::navigation_gate(
            matches!(model.mode, SelectorMode::ClosingRuntime { .. }),
            selector_modal(model),
            selector_dirty(model),
        ) {
            Gate::Block => return SelectorKeyRoute::Control(None),
            Gate::Review => {
                model.leave_review = Some(LeaveReview::new(
                    command,
                    model.settings_overlay.is_none()
                        && model.profile_overlay.is_none()
                        && matches!(
                            model.mode,
                            SelectorMode::Settings { .. } | SelectorMode::ProfileManager { .. }
                        ),
                ));
                return SelectorKeyRoute::Control(None);
            }
            Gate::Allow => {}
        }
    }
    match command {
        Command::Services => SelectorKeyRoute::Control(Some(SelectorControl::OpenServices)),
        Command::Hosts => SelectorKeyRoute::Control(Some(SelectorControl::OpenHosts)),
        Command::Profiles => {
            model.clear_frozen_agent_view();
            model.profiles_from_settings = matches!(model.mode, SelectorMode::Settings { target: SelectorTarget::GlobalSettings, .. });
            SelectorKeyRoute::Control(Some(SelectorControl::OpenProfileManager))
        },
        Command::Workspaces => {
            model.clear_frozen_agent_view();
            SelectorKeyRoute::Control(Some(SelectorControl::OpenProjects))
        }
        Command::Archive => {
            model.clear_frozen_agent_view();
            SelectorKeyRoute::Control(Some(SelectorControl::OpenRetiredSessions))
        }
        Command::Appearance => {
            model.inspector_visible = !model.inspector_visible;
            model.notice = Some(format!(
                "Appearance: Inspector {}",
                if model.inspector_visible {
                    "shown"
                } else {
                    "hidden"
                }
            ));
            SelectorKeyRoute::Control(None)
        }
        Command::Archived => SelectorKeyRoute::Control(None),
        Command::Help => {
            model.help = Some(Help::default());
            SelectorKeyRoute::Control(None)
        }
        Command::Page(panel) => {
            model.clear_frozen_agent_view();
            if matches!(
                model.mode,
                SelectorMode::Settings {
                    target: SelectorTarget::GlobalSettings,
                    ..
                } | SelectorMode::ProfileManager { .. }
            ) {
                model.leave_settings();
            }
            model.finish_subject_context();
            model.settings_return_panel = None;
            model.filter_focused = false;
            model.recent.blur_filter();
            SelectorKeyRoute::Switch(panel)
        }
        Command::Settings => {
            model.clear_frozen_agent_view();
            model.profiles_from_settings = false;
            if let Some(origin) = model.finish_subject_context() {
                model.settings_return_panel = Some(origin);
            }
            if model.settings_return_panel.is_none() {
                model.settings_return_panel =
                    Some(if matches!(model.mode, SelectorMode::RecentSessions) {
                        PrimaryPanel::Recent
                    } else {
                        PrimaryPanel::Agents
                    });
            }
            model.activate_primary_panel(PrimaryPanel::Agents);
            let selected = model.workspace_selection.selected().cloned();
            model
                .workspace_selection
                .select(Some(SelectorTarget::GlobalSettings));
            model.open_settings();
            model.workspace_selection.select(selected);
            model.settings_navigation = None;
            SelectorKeyRoute::Control(None)
        }
        Command::Exit => SelectorKeyRoute::Control(Some(SelectorControl::Exit)),
        Command::Back => {
            if matches!(model.mode, SelectorMode::RetiredSessions { .. }) {
                model.leave_archive();
                return SelectorKeyRoute::Control(None);
            }
            model.leave_settings();
            model
                .settings_return_panel
                .take()
                .map(SelectorKeyRoute::Switch)
                .unwrap_or(SelectorKeyRoute::Control(None))
        }
        Command::Refresh if matches!(model.mode, SelectorMode::RetiredSessions { .. }) =>
            SelectorKeyRoute::Control(Some(SelectorControl::OpenRetiredSessions)),
        Command::Refresh => {
            model.clear_frozen_agent_view();
            SelectorKeyRoute::Refresh
        }
        Command::Sort => {
            model.clear_frozen_agent_view();
            let order = if matches!(model.mode, SelectorMode::RecentSessions) { &mut model.recent.sort_order } else { &mut model.sort_order };
            *order = order.next();
            if matches!(model.mode, SelectorMode::RecentSessions) && *order == cutex::profiles::list_preferences::ListSort::Pinned { *order = order.next(); }
            model.notice = Some(format!("Sort: {} · Settings controls the next-launch default", order.label()));
            SelectorKeyRoute::Control(None)
        }
        Command::Scope => {
            model.clear_frozen_agent_view();
            model.managed_scope = (model.managed_scope + 1) % 3;
            if model.selected_visible_index().is_none() {
                let target = model.visible_rows().first().map(|row| row.target.clone());
                model.workspace_selection.select(target);
            }
            SelectorKeyRoute::Control(None)
        }
        Command::LoadMore => {
            if matches!(model.mode, SelectorMode::RecentSessions)
                && model.recent.next_cursor().is_some()
                && !model.recent.loading()
            {
                SelectorKeyRoute::Control(Some(SelectorControl::Recent(RecentCommand::LoadMore)))
            } else {
                SelectorKeyRoute::Control(None)
            }
        }
        Command::NewManagedAgent => SelectorKeyRoute::Control(Some(SelectorControl::NewAgent)),
        Command::NewProject => SelectorKeyRoute::Control(Some(SelectorControl::NewSession)),
        Command::Details => SelectorKeyRoute::Control(None),
        Command::Titles => {
            if matches!(model.mode, SelectorMode::Agents) {
                model.show_thread_titles = !model.show_thread_titles;
            }
            SelectorKeyRoute::Control(None)
        }
        Command::Inspect => {
            model.detail_scroll.reset();
            model.recent_detail_scroll.reset();
            model.filter_focused = false;
            model.recent.blur_filter();
            if matches!(model.mode, SelectorMode::Agents) {
                model.inspector_overview_focused = model.selected_managed_agent().is_some();
                SelectorKeyRoute::Control(None)
            } else if matches!(model.mode, SelectorMode::RecentSessions) {
                model.recent_inspecting = !model.recent.visible_rows().is_empty();
                SelectorKeyRoute::Control(None)
            } else if let SelectorMode::RetiredSessions { selected } = model.mode {
                model.details = model.retired_rows.get(selected).map(|row| format!(
                    "{}\n\nState: {}\nProfile: {}\nManaged path: {}\nRetired at: {}\nRevision: {}",
                    row.agent, if row.actions.is_empty() { "Retired (read-only)" } else { "Archived" },
                    row.configured_profile.as_deref().unwrap_or("N/A"), row.managed_path,
                    row.retired_at.as_deref().unwrap_or("N/A"), row.revision));
                model.status_scroll.reset();
                SelectorKeyRoute::Control(None)
            } else {
                SelectorKeyRoute::Control(Some(model.handle(SelectorEvent::OpenActions)))
            }
        }
        Command::Actions => {
            model.filter_focused = false;
            model.recent.blur_filter();
            SelectorKeyRoute::Control(Some(model.handle(SelectorEvent::OpenActions)))
        }
        Command::Edit => {
            model.filter_focused = false;
            model.recent.blur_filter();
            model.inspector_overview_focused = false;
            if matches!(model.mode, SelectorMode::RecentSessions) {
                if let Some(id) = model
                    .recent
                    .visible_rows()
                    .get(model.recent.selected_visible())
                    .and_then(|row| match &row.view.subject {
                        super::session_tui_view::SubjectRef::Managed(id) => Some(id.clone()),
                        _ => None,
                    })
                {
                    return SelectorKeyRoute::Control(Some(model.open_subject_context(
                        &id,
                        SelectorEvent::OpenSettings,
                        PrimaryPanel::Recent,
                    )));
                }
                model.notice = Some(
                    "Unmanaged native session: no Agent settings; explicitly Adopt first".into(),
                );
            } else {
                model.open_settings();
            }
            SelectorKeyRoute::Control(None)
        }
    }
}
fn route_selector_key(model: &mut SelectorModel, key: KeyEvent) -> SelectorKeyRoute {
    if key.kind==KeyEventKind::Release{return SelectorKeyRoute::Control(None);}
    let f=if matches!(model.mode,SelectorMode::RecentSessions){&mut model.recent.facets}else{&mut model.facets};
    if f.picker.is_some(){let old=f.project.clone();f.picker_key(key);if old.is_some() && f.project.is_none() && f.host.is_some(){model.notice=Some("Project filter cleared for the selected host".into());}model.clear_frozen_agent_view();model.ensure_selection();model.recent.filter_edited();return SelectorKeyRoute::Control(None);}

    if model.native_archive_pending { return SelectorKeyRoute::Control(None); }
    if let Some((id, mut selected, mut confirming)) = model.native_actions.take() {
        if key.kind != KeyEventKind::Press {
            model.native_actions = Some((id, selected, confirming));
            return SelectorKeyRoute::Control(None);
        }
        if model.native_is_claude(&id) {
            match key.code {
                KeyCode::Esc=>{if confirming{model.native_actions=Some((id,0,false));}},
                KeyCode::Enter if confirming=>{if let Some(owner)=model.native_owned_id(&id){return SelectorKeyRoute::Control(Some(SelectorControl::CloseOwnedSession{native:id,owner}));}},
                KeyCode::Enter=>model.native_actions=Some((id,0,true)),
                _=>model.native_actions=Some((id,0,confirming)),
            }
            return SelectorKeyRoute::Control(None);
        }
        let count = if model.native_owned_id(&id).is_some() { 3 } else { 4 };
        match key.code {
            KeyCode::Esc => {
                if confirming { model.native_actions = Some((id, selected, false)); }
                return SelectorKeyRoute::Control(None);
            }
            KeyCode::Up if !confirming => selected = (selected + count - 1) % count,
            KeyCode::Down if !confirming => selected = (selected + 1) % count,
            KeyCode::Enter if selected == 2 && !confirming => return SelectorKeyRoute::Control(Some(SelectorControl::RevertHistory(id))),
            KeyCode::Enter if confirming => return SelectorKeyRoute::Control(Some(
                match model.native_owned_id(&id) {
                    Some(owner) => SelectorControl::CloseOwnedSession { native: id, owner },
                    None if selected == 3 => SelectorControl::CloseNative(id),
                    None => SelectorControl::ArchiveNative(id),
                })),
            KeyCode::Enter if selected == 0 => {
                model.recent.begin_review();
                return SelectorKeyRoute::Control(None);
            }
            KeyCode::Enter => confirming = true,
            _ => {}
        }
        model.native_actions = Some((id, selected, confirming));
        return SelectorKeyRoute::Control(None);
    }
    if model.details.is_some() {
        if key.kind != KeyEventKind::Release && key.code == KeyCode::Esc {
            model.details = None;
        } else {
            model.status_scroll.handle(key);
        }
        return SelectorKeyRoute::Control(None);
    }
    if key.kind == KeyEventKind::Press && input_policy::resolve(key) == Some(Command::Details) {
        return selector_command(model, Command::Details);
    }
    let text_input = selector_input(model).is_some();
    if !super::session_tui_workspace_events::accepts_key(key, text_input) {
        return SelectorKeyRoute::Control(None);
    }
    if let Some(mut navigation) = model.settings_navigation.take() {
        match navigation.handle(key, &settings_navigation_commands()) {
            Some(Some(Command::Settings)) => {}
            Some(Some(command)) => return selector_command(model, command),
            Some(None) => {
                model.leave_settings();
                return SelectorKeyRoute::Switch(
                    model
                        .settings_return_panel
                        .take()
                        .unwrap_or(PrimaryPanel::Agents),
                );
            }
            None => model.settings_navigation = Some(navigation),
        }
        return SelectorKeyRoute::Control(None);
    }
    if key.code == KeyCode::Esc
        && model.settings_overlay.is_none()
        && model.profile_overlay.is_none()
        && model.help.is_none()
        && model.leave_review.is_none()
        && matches!(model.mode, SelectorMode::ProfileManager { focus: ProfileWorkspaceFocus::Editor, .. })
    {
        return SelectorKeyRoute::Control(Some(model.handle(SelectorEvent::Back)));
    }
    if key.code == KeyCode::Esc && model.profiles_from_settings
        && matches!(model.mode, SelectorMode::ProfileManager { focus: ProfileWorkspaceFocus::Items, .. })
        && !text_input && !selector_modal(model) && model.help.is_none() && model.leave_review.is_none()
    {
        return selector_command(model, Command::Back);
    }
    // Escape unwinds the local Settings columns before leaving the page.
    // In particular, the remembered origin must not steal cancellation.
    if key.code == KeyCode::Esc
        && model.settings_overlay.is_none()
        && model.help.is_none()
        && model.leave_review.is_none()
    {
        if let SelectorMode::Settings {
            focus,
            view: SettingsView::Categories,
            ..
        } = &mut model.mode
        {
            if *focus != SettingsFocus::Categories {
                *focus = match *focus {
                    SettingsFocus::Value => SettingsFocus::Options,
                    _ => SettingsFocus::Categories,
                };
                return SelectorKeyRoute::Control(None);
            }
        }
    }
    if key.code == KeyCode::Esc && model.help.is_none() && model.leave_review.is_none()
        && model.settings_overlay.is_none() && model.profile_overlay.is_none()
        && matches!(model.mode, SelectorMode::Settings { target: SelectorTarget::GlobalSettings, focus: SettingsFocus::Categories, .. }
            | SelectorMode::Settings { target: SelectorTarget::GlobalSettings, view: SettingsView::Expanded, .. }) {
        return SelectorKeyRoute::Control(None);
    }
    if key.code == KeyCode::Esc
        && !text_input
        && !selector_modal(model)
        && !selector_dirty(model)
        && model.help.is_none()
        && model.leave_review.is_none()
        && !matches!(model.mode, SelectorMode::ClosingRuntime { .. })
        && matches!(model.mode, SelectorMode::Settings { .. } | SelectorMode::ProfileManager { .. })
        && model.settings_return_panel.is_some()
    {
        model.leave_settings();
        return SelectorKeyRoute::Switch(model.settings_return_panel.take().unwrap());
    }
    if let Some(mut review) = model.leave_review.take() {
        match review.handle(key) {
            Some(0) => {}
            Some(1) => {
                let command = review.command;
                model.leave_settings();
                return selector_command(model, command);
            }
            Some(2) => {
                return SelectorKeyRoute::Control(Some(model.handle(SelectorEvent::Insert('S'))))
            }
            _ => model.leave_review = Some(review),
        }
        return SelectorKeyRoute::Control(None);
    }
    if let Some(mut help) = model.help.take() {
        let entries = selector_commands(model);
        match help.handle(key, &entries) {
            Some(Some(command)) => return selector_command(model, command),
            Some(None) => {}
            None => model.help = Some(help),
        }
        return SelectorKeyRoute::Control(None);
    }
    if matches!(model.mode, SelectorMode::ClosingRuntime { .. }) {
        return SelectorKeyRoute::Control(None);
    }
    if input_policy::resolve(key) == Some(Command::Help) && !selector_modal(model) {
        model.help = Some(Help::default());
        return SelectorKeyRoute::Control(None);
    }
    if matches!(model.mode, SelectorMode::Agents | SelectorMode::RecentSessions) && !selector_modal(model) {
        let recent=matches!(model.mode,SelectorMode::RecentSessions);
        let active=if recent {model.recent.filter_focused()}else{model.filter_focused};
        let facets=if recent {&mut model.recent.facets}else{&mut model.facets};
        let prior_project=facets.project.clone();
        if facets.picker_key(key) {
            if prior_project.is_some() && facets.project.is_none() && facets.host.is_some() {model.notice=Some("Project filter cleared for the selected host".into());}
            model.ensure_selection();model.recent.filter_edited();
            return SelectorKeyRoute::Control(None);
        }
        if active {
            match key.code {
                KeyCode::Tab | KeyCode::BackTab => {facets.field=(facets.field+if key.code==KeyCode::Tab {1}else{2})%3;return SelectorKeyRoute::Control(None);},
                KeyCode::Enter if facets.field>0 => {open_facet_picker(model);return SelectorKeyRoute::Control(None);},
                KeyCode::Esc | KeyCode::Enter => {},
                _ if facets.field>0 => {return SelectorKeyRoute::Control(None);},
                _=>{},
            }
        }
    }
    if key.code==KeyCode::Enter && !(matches!(model.mode,SelectorMode::Agents) && model.filter_focused) && !(matches!(model.mode,SelectorMode::RecentSessions) && model.recent.filter_focused()) && !selector_modal(model) {
        if let Some(e)=remote_projection::selected(model) {return SelectorKeyRoute::Control(Some(SelectorControl::RemoteBrowse(e.connection,e.session.id)));}
    }
    // Editors consume plain text and cursor keys, not page/exit commands.
    if let Some(input) = selector_input(model) {
        if input_policy::edit(input, key) {
            model.ensure_selection();
            model.recent.filter_edited();
            if let Some(SettingsOverlay::Groups { inputs, .. }) = model.settings_overlay.as_mut() {
                ensure_group_editor_trailing_input(inputs);
            }
            return SelectorKeyRoute::Control(None);
        }
    }
    let recent = matches!(model.mode, SelectorMode::RecentSessions);
    if recent
        && model.recent.review().is_some()
        && !model.recent.adoption_name_focused()
        && key.code == KeyCode::BackTab
        && !model.recent.review_confirmed()
    {
        model.recent.focus_adoption_name();
        return SelectorKeyRoute::Control(None);
    }
    if recent
        && matches!(
            key.code,
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Enter | KeyCode::Esc
        )
        && model.recent.blur_adoption_name()
    {
        return SelectorKeyRoute::Control(None);
    }
    if recent && model.recent_inspecting && model.recent.review().is_none() {
        if model.recent_detail_scroll.handle(key) {
            return SelectorKeyRoute::Control(None);
        }
        if let Some(command) = input_policy::resolve(key) {
            return selector_command(model, command);
        }
        if key.code == KeyCode::Esc {
            model.recent_inspecting = false;
        }
        return SelectorKeyRoute::Control(None);
    }
    let filter = (recent && model.recent.filter_focused())
        || (matches!(model.mode, SelectorMode::Agents) && model.filter_focused);
    if filter
        && matches!(
            key.code,
            KeyCode::Enter | KeyCode::Esc | KeyCode::Tab | KeyCode::BackTab
        )
    {
        model.filter_focused = false;
        model.recent.blur_filter();
        return SelectorKeyRoute::Control(None);
    }
    if let Some(command) = input_policy::resolve(key) {
        if selector_modal(model) {
            return SelectorKeyRoute::Control(None);
        }
        return selector_command(model, command);
    }
    if matches!(key.code, KeyCode::Esc) && !selector_modal(model) && !text_input {
        if selector_dirty(model) {
            return selector_command(model, Command::Back);
        }
        if matches!(model.mode, SelectorMode::Agents) && !model.inspector_overview_focused {
            if model.query.value().is_empty() {
                model.notice = Some("Ctrl+C exits Cutex".into());
            } else {
                model.query.reset();
                model.ensure_selection();
            }
            return SelectorKeyRoute::Control(None);
        }
        if recent && model.recent.review().is_none() {
            if !model.recent.query().is_empty() {
                model.recent.clear_filter();
            } else {
                model.notice = Some("Ctrl+C exits Cutex".into());
            }
            return SelectorKeyRoute::Control(None);
        }
    }
    if !selector_modal(model)
        && ((matches!(model.mode, SelectorMode::Agents) && !model.inspector_overview_focused)
            || recent)
    {
        if let KeyCode::Char(c) = key.code {
            if !key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
                && !c.is_control()
            {
                if recent {
                    model.recent.facets.field=0;
                    model.recent.focus_filter();
                    if c != '/' {
                        model.recent.push_filter(c);
                    }
                } else {
                    model.facets.field=0;
                    model.filter_focused = true;
                    if c != '/' {
                        input_policy::edit(&mut model.query, key);
                        model.ensure_selection();
                    }
                }
                return SelectorKeyRoute::Control(None);
            }
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            if recent {
                model.recent.focus_filter();
            } else {
                model.filter_focused = true;
            }
            return SelectorKeyRoute::Control(None);
        }
    }
    if !filter
        && matches!(model.mode, SelectorMode::Agents)
        && model.inspector_overview_focused
        && model.detail_scroll.handle(key)
    {
        return SelectorKeyRoute::Control(None);
    }
    if !filter
        && !selector_modal(model)
        && matches!(
            model.mode,
            SelectorMode::Agents | SelectorMode::RecentSessions
        )
    {
        let delta = match key.code {
            KeyCode::PageUp => Some(-10),
            KeyCode::PageDown => Some(10),
            _ => None,
        };
        if let Some(delta) = delta {
            if recent {
                model.recent.move_selection(delta);
            } else {
                model.move_selection(delta);
            }
            return SelectorKeyRoute::Control(None);
        }
    }
    if let Some(panel) = selector_list_panel_from_horizontal_key(model, key) {
        return panel.map_or(SelectorKeyRoute::Control(None), |p| {
            selector_command(model, Command::Page(p))
        });
    }
    let control = if let Some(control) = selector_navigation_control_from_key(model, key) {
        Some(control)
    } else if close_runtime_shortcut_from_key(key)
        && !text_input
        && matches!(model.mode, SelectorMode::Agents)
    {
        Some(model.activate_close_shortcut())
    } else {
        selector_event_from_key(key, model.enhanced_keyboard).map(|e| model.handle(e))
    };
    if matches!(control, Some(SelectorControl::Continue))
        && matches!(model.mode, SelectorMode::Agents)
        && model.object_return.is_some()
    {
        return SelectorKeyRoute::Switch(model.finish_subject_context().unwrap());
    }
    SelectorKeyRoute::Control(control)
}

fn handle_selector_paste(model: &mut SelectorModel, text: &str) {
    if matches!(model.mode,SelectorMode::Agents) && model.facets.paste(text) {return;}
    if matches!(model.mode,SelectorMode::RecentSessions) && model.recent.facets.paste(text) {return;}
    if model.details.is_some() {
        return;
    }
    if model.help.is_some() || model.leave_review.is_some() {
        return;
    }
    if let Some(input) = selector_input(model) {
        input_policy::paste(input, text);
        model.clear_frozen_agent_view();
        model.ensure_selection();
        model.recent.filter_edited();
        if let Some(SettingsOverlay::Groups { inputs, .. }) = model.settings_overlay.as_mut() {
            ensure_group_editor_trailing_input(inputs);
        }
    }
}

fn run_event_loop(
    terminal: &mut CutexTerminal,
    events: &mut ShellEvents,
    model: &mut SelectorModel,
    refresh: &mut WorkspaceLoad<SelectorSnapshot>,
    recent_catalog: &mut Option<RecentCatalog>,
) -> anyhow::Result<SessionTuiCycleOutcome> {
    let mut runtime_close = None;
    let mut next_activity_refresh = Instant::now() + ACTIVITY_REFRESH_INTERVAL;
    loop {
        let now = Instant::now();
        if now >= next_activity_refresh {
            if let Ok(Some(activity_states)) = cutex::management::v2::activity::try_load_session_activity_states() {
                model.refresh_activity_states(&activity_states);
            }
            next_activity_refresh = now + ACTIVITY_REFRESH_INTERVAL;
        }
        receive_refresh(model, refresh);
        if let Some(worker)=model.remote_worker.clone() {if let Ok(rx)=worker.lock(){loop {match rx.try_recv(){Ok((c,r))=>remote_projection::receive(model,c,r),Err(std::sync::mpsc::TryRecvError::Empty)=>break,Err(std::sync::mpsc::TryRecvError::Disconnected)=>{model.remote_worker=None;break;}}}}}
        if model.remote_worker.is_none() && Instant::now()>=model.remote_due {model.remote_worker=Some(std::sync::Arc::new(std::sync::Mutex::new(remote_projection::start())));model.remote_due=Instant::now()+Duration::from_secs(15);}
        if let Some(reply) = recent_catalog.as_ref().and_then(RecentCatalog::poll) {
            model.recent_catalog_reply(reply);
            remote_projection::apply(model);
        }
        if let Some(worker) = model.profile_usage_worker.clone() {
            if let Ok(receiver) = worker.lock() {
                loop {
                    match receiver.try_recv() {
                        Ok(reply) => model.profile_usage_refreshed(reply),
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            model.profile_usage_worker = None;
                            break;
                        }
                    }
                }
            }
        }
        if receive_runtime_close(model, &mut runtime_close) {
            terminal.clear()?;
            if matches!(model.mode, SelectorMode::Agents) {
                if let Some(origin) = model.finish_subject_context() {
                    return Ok(SessionTuiCycleOutcome::Switch(origin));
                }
            }
        }
        if let SelectorMode::ConfirmRuntimeAction {
            agent_key, action, ..
        } = &model.mode
        {
            if matches!(
                action,
                SessionTuiAction::StockStart | SessionTuiAction::StockRestart
            ) && model.stock_runtime_confirmation.is_none()
            {
                match super::stock_lifecycle::ReviewedStockRuntimeAction::review(
                    agent_key,
                    *action == SessionTuiAction::StockRestart,
                ) {
                    Ok(request) => model.stock_runtime_confirmation = Some(request),
                    Err(error) => {
                        model.warning = Some(format!("Stock runtime review failed: {error:#}"));
                        model.mode = SelectorMode::Agents;
                    }
                }
            } else if matches!(
                action,
                SessionTuiAction::UpgradeRuntimePackage
            ) && model.runtime_package_confirmation.is_none()
            {
                match super::stock_lifecycle::ReviewedRuntimePackageUpgrade::review(agent_key) {
                    Ok(request) => model.runtime_package_confirmation = Some(request),
                    Err(error) => {
                        model.warning = Some(format!("Runtime package review failed: {error:#}"));
                        model.mode = SelectorMode::Agents;
                    }
                }
            } else if matches!(
                action,
                SessionTuiAction::RetireSession | SessionTuiAction::RestoreSession
            ) && model.archive_confirmation.is_none()
            {
                let review = (|| -> anyhow::Result<_> {
                    let client =
                        super::management_control_plane::ManagementControlClient::connect()?;
                    let request = cutex::agent_management::AgentArchiveReviewRequest {
                        cutex_session_id: cutex::role_revision::CutexSessionId::new(
                            agent_key.clone(),
                        )
                        .map_err(|_| anyhow::anyhow!("exact durable Agent ID required"))?,
                        operation: if *action == SessionTuiAction::RestoreSession {
                            cutex::agent_management::AgentArchiveOperation::Restore
                        } else {
                            cutex::agent_management::AgentArchiveOperation::Archive
                        },
                    };
                    Ok(cutex::agent_management::AgentArchiveRequest {
                        reason: None,
                        action_id: cutex::agent_management::AgentActionId::new(format!(
                            "tui-archive-{}",
                            uuid::Uuid::new_v4()
                        ))?,
                        review: client.review_agent_archive(&request)?,
                    })
                })();
                match review {
                    Ok(request) => model.archive_confirmation = Some(request),
                    Err(error) => {
                        model.warning = Some(format!("Archive review failed: {error:#}"));
                        model.mode = SelectorMode::Agents;
                    }
                }
            }
        } else if !matches!(model.mode, SelectorMode::ClosingRuntime { .. }) {
            model.archive_confirmation = None;
            model.stock_runtime_confirmation = None;
            model.runtime_package_confirmation = None;
        }
        terminal.draw(|frame| render_selector(frame, model))?;

        let Some(event) = events.next()? else {
            continue;
        };
        match event {
            Event::Key(key) => {
                let control = match route_selector_key(model, key) {
                    SelectorKeyRoute::Switch(panel) => {
                        return Ok(SessionTuiCycleOutcome::Switch(panel))
                    }
                    SelectorKeyRoute::Refresh => {
                        if matches!(model.mode, SelectorMode::ClosingRuntime { .. }) {
                            continue;
                        }
                        if matches!(model.mode, SelectorMode::RecentSessions) {
                            if recent_catalog
                                .as_ref()
                                .is_some_and(|catalog| catalog.request(RecentCommand::Retry, None))
                            {
                                model.recent_loading_started();
                            } else {
                                model.warning = Some(
                                    "recent catalog worker stopped; retry by reopening the TUI"
                                        .to_string(),
                                );
                            }
                        } else if !model.refreshing {
                            match spawn_snapshot_refresh() {
                                Ok(next) => {
                                    *refresh = next;
                                    model.refreshing = true;
                                }
                                Err(error) => model.mark_refresh_failed(format!("{error:#}")),
                            }
                        }
                        continue;
                    }
                    SelectorKeyRoute::Control(control) => control,
                };
                if let Some(control) = control {
                    match control {
                        SelectorControl::NewSession => return Ok(SessionTuiCycleOutcome::NewSession),
                        SelectorControl::NewAgent => return Ok(SessionTuiCycleOutcome::NewAgent),
                        SelectorControl::NativeResume {
                            catalog,
                            thread,
                            cwd,
                        } => {
                            return Ok(SessionTuiCycleOutcome::NativeResume {
                                catalog,
                                thread,
                                cwd,
                            })
                        }
                        SelectorControl::ExecuteArchive(request) => {
                            model.archive_confirmation = Some(request.clone());
                            let intent = SessionTuiIntent {
                                key: request.review.cutex_session_id.as_str().to_string(),
                                action: if request.review.operation
                                    == cutex::agent_management::AgentArchiveOperation::Restore
                                {
                                    SessionTuiAction::RestoreSession
                                } else {
                                    SessionTuiAction::RetireSession
                                },
                                launch_profile: None,
                                stock_runtime: None,
                                runtime_package: None,
                            };
                            model.runtime_close_started(&intent);
                            let (send, receive) = std::sync::mpsc::channel();
                            std::thread::spawn(move || {
                                let result =
                                    super::session_archive::execute_confirmed_archive(&request)
                                        .map_err(|error| {
                                            RuntimeCloseWorkerResult::Failed(format!("{error:#}"))
                                        })
                                        .and_then(|_| {
                                            load_live_snapshot()
                                                .map(RuntimeCloseWorkerResult::Closed)
                                                .map_err(|error| {
                                                    RuntimeCloseWorkerResult::ClosedRefreshFailed(
                                                        format!("{error:#}"),
                                                    )
                                                })
                                        })
                                        .unwrap_or_else(|error| error);
                                let _ = send.send(result);
                            });
                            runtime_close = Some(receive);
                        }
                        SelectorControl::RevertHistory(id) => {
                            match super::session_history::run(terminal, events, id) {
                                Ok(Some(message)) => model.notice=Some(message),
                                Ok(None) => {},
                                Err(error) => model.warning=Some(format!("History: {error:#}")),
                            }
                            terminal.clear()?;
                        }
                        SelectorControl::Continue => {}
                        SelectorControl::Exit => return Ok(SessionTuiCycleOutcome::Exit),
                        SelectorControl::Selected(intent) if intent_runs_in_selector(&intent) => {
                            model.runtime_close_started(&intent);
                            match spawn_runtime_close(intent) {
                                Ok(receiver) => runtime_close = Some(receiver),
                                Err(error) => model.runtime_close_failed(format!("{error:#}")),
                            }
                        }
                        SelectorControl::Selected(intent) => {
                            return Ok(SessionTuiCycleOutcome::Selected(intent));
                        }
                        SelectorControl::OpenRetiredSessions => {
                            match load_retired_selector_rows() {
                                Ok(rows) => model.open_retired_sessions(rows),
                                Err(error) => {
                                    model.warning =
                                        Some(format!("archived Agents unavailable: {error:#}"))
                                }
                            }
                        }
                        SelectorControl::OpenRecentSessions => {
                            model.mode = SelectorMode::RecentSessions;
                            model.notice = None;
                        }
                        SelectorControl::CloseOwnedSession { native, owner } => {
                            if recent_catalog.as_ref().is_some_and(|catalog| catalog.close_owned(native, owner)) {
                                model.native_archive_pending = true;
                                model.notice = Some("Closing session runtime…".into());
                            } else { model.warning = Some("Session worker unavailable; no close requested".into()); }
                        }
                        SelectorControl::CloseNative(id) => {
                            if recent_catalog.as_ref().is_some_and(|catalog| catalog.close_native(id)) {
                                model.native_archive_pending = true;
                                model.notice = Some("Closing runtime and restoring history…".into());
                            } else { model.warning = Some("Session worker unavailable; no close requested".into()); }
                        }
                        SelectorControl::ArchiveNative(id) => {
                            if recent_catalog.as_ref().is_some_and(|catalog| catalog.archive(id)) {
                                model.native_archive_pending = true;
                                model.notice = Some("Closing and archiving session…".into());
                            } else { model.warning = Some("Session worker unavailable; no close requested".into()); }
                        }
                        SelectorControl::Recent(command) => {
                            let cursor = model.recent.cursor_for(command);
                            if !recent_catalog
                                .as_ref()
                                .is_some_and(|catalog| catalog.request(command, cursor))
                            {
                                model.warning = Some(
                                    "recent catalog worker stopped; retry by reopening the TUI"
                                        .to_string(),
                                );
                            } else {
                                model.recent_loading_started();
                            }
                        }
                        SelectorControl::AdoptRecent(request) => {
                            match adopt_recent_thread(&request) {
                                Ok(result) => model.recent_adoption_succeeded(&request, result),
                                Err(error) => model.recent_adoption_failed(format!("{error:#}")),
                            }
                        }
                        SelectorControl::RemoteBrowse(c,id)=>{match super::session_tui_remote::run_selected(terminal,events,c,Some(id))? {super::session_tui_remote::Outcome::Exit=>return Ok(SessionTuiCycleOutcome::Exit),_=>{}}},
                        SelectorControl::OpenServices => {
                            match super::session_tui_services::run(terminal, events) {
                                Ok(super::session_tui_remote::Outcome::Exit) => return Ok(SessionTuiCycleOutcome::Exit),
                                Ok(super::session_tui_remote::Outcome::Back) => {},
                                Err(error) => model.notice = Some(format!("Services: {error:#}")),
                            }
                        }
                        SelectorControl::OpenHosts => {
                            match super::session_tui_hosts::run(terminal, events) {
                                Ok(super::session_tui_remote::Outcome::Exit) => return Ok(SessionTuiCycleOutcome::Exit),
                                Ok(super::session_tui_remote::Outcome::Back) => {},
                                Err(error) => model.notice = Some(format!("Hosts: {error:#}")),
                            }
                        }
                        SelectorControl::OpenProfileManager => {
                            open_profile_workspace(model);
                        }
                        SelectorControl::OpenProjects => {
                            return Ok(SessionTuiCycleOutcome::Projects);
                        }
                        SelectorControl::OpenCutexProjects => {
                            return Ok(SessionTuiCycleOutcome::CutexProjects);
                        }
                        SelectorControl::ApplySettings(request) => {
                            match apply_session_settings(&request) {
                                Ok(result) => model.settings_apply_succeeded(
                                    &request.key,
                                    &result.record,
                                    &result.profile_names,
                                    request.changed_count,
                                    request.draft.launch_actions_are_dirty(),
                                    result.warning,
                                ),
                                Err(error) => model.settings_apply_failed(format!("{error:#}")),
                            }
                        }
                        SelectorControl::ApplyGlobalSettings(request) => {
                            match apply_global_settings(&request) {
                                Ok(result) => model.global_settings_apply_succeeded(
                                    &result.config,
                                    &result.profile_names,
                                    request.changed_count,
                                ),
                                Err(error) => {
                                    model.global_settings_apply_failed(format!("{error:#}"))
                                }
                            }
                        }
                        SelectorControl::ApplyProfileSettings(request) => {
                            match perform_profile_settings_update(&request) {
                                Ok(receipt) => match load_profile_management_result(receipt) {
                                    Ok(result) => model.profile_management_succeeded(result),
                                    Err((notice, error)) => model
                                        .profile_management_refresh_failed(
                                            notice,
                                            format!("{error:#}"),
                                        ),
                                },
                                Err(error) => {
                                    model.profile_settings_apply_failed(format!("{error:#}"))
                                }
                            }
                        }
                        SelectorControl::ManageSession(request) => {
                            match apply_session_management(&request) {
                                Ok(result) => model.session_management_succeeded(
                                    &request.key,
                                    request.command,
                                    &result.record,
                                    &result.profile_names,
                                    result.warning,
                                ),
                                Err(error) => model.session_management_failed(format!("{error:#}")),
                            }
                        }
                        SelectorControl::ManageProfile(request) => {
                            match perform_profile_management(&request) {
                                Ok(receipt) => match load_profile_management_result(receipt) {
                                    Ok(result) => model.profile_management_succeeded(result),
                                    Err((notice, error)) => model
                                        .profile_management_refresh_failed(
                                            notice,
                                            format!("{error:#}"),
                                        ),
                                },
                                Err(error) => {
                                    model.profile_management_failed(&request, format!("{error:#}"))
                                }
                            }
                        }
                        SelectorControl::LoginProfile => {
                            return Ok(SessionTuiCycleOutcome::LoginProfile);
                        }
                    }
                }
            }
            Event::Paste(text) => handle_selector_paste(model, &text),
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
}

fn apply_session_settings(
    request: &SessionSettingsApplyRequest,
) -> anyhow::Result<SessionSettingsApplyResult> {
    let profile_names = if matches!(
        request.draft.profile_update(),
        CutexSessionValueUpdate::Unchanged
    ) {
        request.profile_names.clone()
    } else {
        load_profile_names_read_only()?
    };
    let mut store = load_cutex_session_store()?;
    let record = apply_session_settings_to_store(&mut store, request, &profile_names)?;
    persist_cutex_session_store_and_im_record(&store, &request.key)?;
    let warning = if request.draft.agent_groups_are_dirty() {
        let target = record
            .codex_session_id
            .as_deref()
            .unwrap_or(request.key.as_str());
        live_group_propagation_warning(agent_bus_runtime::maybe_patch_live_agent_groups(
            target,
            &record.agent_groups,
            AgentGroupUpdateMode::Set,
        ))
    } else {
        None
    };
    Ok(SessionSettingsApplyResult {
        record,
        profile_names,
        warning,
    })
}

fn apply_session_management(
    request: &SessionManagementRequest,
) -> anyhow::Result<SessionManagementResult> {
    if request.command == SessionSettingsCommand::Adopt
        && cutex::launch::local_deployment::LocalDeployment::selected()?.is_some()
    {
        let store = load_cutex_session_store()?;
        let record = store.sessions.get(&request.key).context("saved session missing")?;
        let native = record.codex_session_id.as_deref().context("saved native identity missing")?;
        super::light_new::adopt_saved(native, &cutex_session_display_name(record), &record.cwd)?;
    }
    let mut store = load_cutex_session_store()?;
    apply_session_management_to_store(&mut store, request)?;
    persist_cutex_session_store_and_im_record(&store, &request.key)?;
    let record = store.sessions.get(&request.key).cloned().with_context(|| {
        format!(
            "cutex session disappeared after management change: {}",
            request.key
        )
    })?;
    let warning = if request.command == SessionSettingsCommand::Adopt {
        let target = record
            .codex_session_id
            .as_deref()
            .unwrap_or(request.key.as_str());
        live_management_group_propagation_warning(agent_bus_runtime::maybe_patch_live_agent_groups(
            target,
            &record.agent_groups,
            AgentGroupUpdateMode::Set,
        ))
    } else {
        None
    };
    Ok(SessionManagementResult {
        record,
        profile_names: request.profile_names.clone(),
        warning,
    })
}

/// Adopting a catalog thread goes only through the durable session service.
/// The fresh store read makes a second adoption (including a retired identity)
/// fail safely if another Cutex client changed state during the review.
fn adopt_recent_thread(request: &RecentAdoptionRequest) -> anyhow::Result<RecentAdoptionResult> {
    let client = super::management_control_plane::ManagementControlClient::connect()?;
    let result = client.adopt_saved_native(&cutex::agent_management::HumanAdoptRequest {
        owner_created_native: false,
            session_only: false,
            creation_defaults: None,
        action_id: cutex::agent_management::AgentActionId::new(request.action_id.clone())?, native_id:request.thread_id.clone(), cwd:request.cwd.clone(), formal_name:request.formal_name.clone(),
    }).map_err(|e| anyhow::anyhow!("Adoption response unconfirmed: {e:#}. Retry same action {}; no rollback or new identity retry claimed", request.action_id))?;
    anyhow::ensure!(
        result.imported.as_ref().is_some_and(|r| r.complete),
        "Durable identity {} adopted; import incomplete: {}. Retry same action {}",
        result.adopted.record.cutex_session_id,
        result.error.as_deref().unwrap_or("unknown import stage"),
        request.action_id
    );
    let store = load_cutex_session_store()?;
    Ok(RecentAdoptionResult {
        store,
        snapshot: load_live_snapshot().map_err(|error| format!("{error:#}")),
    })
}

fn perform_profile_management(
    request: &ProfileManagementRequest,
) -> anyhow::Result<ProfileMutationReceipt> {
    match &request.command {
        ProfileManagementCommand::Activate => {
            let account = activate_account(&request.profile_id)?;
            Ok(ProfileMutationReceipt {
                preferred_profile_id: Some(account.id),
                notice: format!("Active profile: {}", account.name),
            })
        }
        ProfileManagementCommand::Rename { new_name } => {
            let result = rename_profile(&request.profile_id, new_name)?;
            Ok(ProfileMutationReceipt {
                preferred_profile_id: Some(result.account.id),
                notice: format!("Renamed {} to {}", result.old_name, result.account.name),
            })
        }
        ProfileManagementCommand::Remove => {
            let result = remove_profile(&request.profile_id)?;
            Ok(ProfileMutationReceipt {
                preferred_profile_id: None,
                notice: format!("Removed profile {}", result.removed.name),
            })
        }
    }
}

fn perform_profile_settings_update(
    request: &ProfileSettingsApplyRequest,
) -> anyhow::Result<ProfileMutationReceipt> {
    let result = update_profile_settings(&request.profile_id, &request.patch)?;
    Ok(ProfileMutationReceipt {
        preferred_profile_id: Some(result.account.id),
        notice: if result.changed {
            format!("Saved {} profile setting(s)", request.changed_count)
        } else {
            "Profile already matched the staged settings".to_string()
        },
    })
}

fn load_profile_management_result(
    receipt: ProfileMutationReceipt,
) -> Result<ProfileManagementResult, (String, anyhow::Error)> {
    let notice = receipt.notice;
    let result = (|| -> anyhow::Result<ProfileManagementResult> {
        let profiles = load_profile_catalog_read_only()?;
        let profile_names = profiles
            .iter()
            .map(|profile| profile.name.clone())
            .collect::<Vec<_>>();
        let store = load_cutex_session_store()?;
        let config = load_codez_config_checked()?;
        Ok(ProfileManagementResult {
            profiles,
            projection: ProfileProjectionSnapshot {
                records: store.sessions,
                config,
                profile_names,
            },
            preferred_profile_id: receipt.preferred_profile_id,
            notice: notice.clone(),
        })
    })();
    result.map_err(|error| (notice, error))
}

fn apply_global_settings(
    request: &GlobalSettingsApplyRequest,
) -> anyhow::Result<GlobalSettingsApplyResult> {
    let profile_names = if request.draft.default_profile_is_dirty() {
        load_profile_names_read_only()?
    } else {
        request.profile_names.clone()
    };
    request.draft.validate_profile_catalog(&profile_names)?;
    let mut config = load_codez_config_checked()?;
    let changed = apply_global_settings_to_config(&mut config, request)?;
    if changed {
        save_codez_config(&config)?;
        super::identity_styles::refresh();
    }
    Ok(GlobalSettingsApplyResult {
        config,
        profile_names,
    })
}

fn apply_global_settings_to_config(
    config: &mut CodezConfig,
    request: &GlobalSettingsApplyRequest,
) -> anyhow::Result<bool> {
    let patch = request.draft.patch(config)?;
    apply_global_config_patch(config, &patch)
}

fn apply_session_management_to_store(
    store: &mut CutexSessionStore,
    request: &SessionManagementRequest,
) -> anyhow::Result<()> {
    let existing = store
        .sessions
        .get(&request.key)
        .cloned()
        .with_context(|| format!("cutex session is not known: {}", request.key))?;
    match request.command {
        SessionSettingsCommand::Adopt => {
            if cutex_session_is_managed(&existing) {
                anyhow::bail!("Agent is already managed");
            }
            adopt_cutex_session(
                store,
                &request.key,
                CutexSessionEnsureSeed {
                    host_id: existing.host_id,
                    cwd: existing.cwd,
                    profile: existing.profile,
                },
                CutexSessionAdoptOptions {
                    display_name: None,
                    managed_cwd: None,
                    groups: Vec::new(),
                    expose_to_im: false,
                    pin: false,
                    model_defaults: None,
                    reasoning_defaults: None,
                },
            )?;
        }
        SessionSettingsCommand::Unmanage => {
            if !cutex_session_is_managed(&existing) {
                anyhow::bail!("Agent is already unmanaged");
            }
            unmanage_cutex_session(store, &request.key)?;
        }
    }
    Ok(())
}

fn apply_session_settings_to_store(
    store: &mut CutexSessionStore,
    request: &SessionSettingsApplyRequest,
    profile_names: &[String],
) -> anyhow::Result<CutexSessionRecord> {
    let profile_update = request.draft.profile_update();
    match profile_update {
        CutexSessionValueUpdate::Unchanged => {}
        CutexSessionValueUpdate::Set(profile) => {
            if !profile_names.iter().any(|candidate| candidate == profile) {
                anyhow::bail!("Profile is no longer configured: {profile}");
            }
        }
        CutexSessionValueUpdate::Clear => {}
    }
    let provider_family = match profile_update {
        CutexSessionValueUpdate::Unchanged => None,
        _ => {
            let record = store.sessions.get(&request.key).context("session disappeared before profile update")?;
            let requested = match profile_update { CutexSessionValueUpdate::Set(profile) => Some(profile.as_str()), _ => None };
            cutex::runtime::claude_accounts::check_profile_change(record, requested)
                .map_err(|error| anyhow::anyhow!("{}: {error}", cutex::runtime::claude_accounts::MISMATCH_CODE))?;
            Some(super::management_context::check_codex_profile_family(record, requested)
                .map_err(|error| anyhow::anyhow!("{}: {}", error.code, error.message))?)
        }
    };
    if request.draft.routing_is_dirty() {
        update_cutex_session_routing_by_key(
            store,
            &request.key,
            &request.key,
            request.draft.routing_patch(),
        )?;
    }
    if let Some(agent_name) = request.draft.agent_name() {
        set_cutex_session_display_name_by_key(store, &request.key, agent_name)?;
    }
    match profile_update {
        CutexSessionValueUpdate::Unchanged => {}
        CutexSessionValueUpdate::Set(profile) => {
            set_cutex_session_profile_by_key(store, &request.key, Some(profile.clone()))?;
        }
        CutexSessionValueUpdate::Clear => {
            set_cutex_session_profile_by_key(store, &request.key, None)?;
        }
    }
    if let Some(family) = provider_family {
        store.sessions.get_mut(&request.key).expect("profile-updated session").provider_family = family;
    }
    if request.draft.runtime_defaults_are_dirty() {
        update_cutex_session_runtime_defaults_by_key(
            store,
            &request.key,
            &request.key,
            request.draft.runtime_defaults_patch(),
        )?;
    }
    store.sessions.get(&request.key).cloned().with_context(|| {
        format!(
            "cutex session disappeared after settings update: {}",
            request.key
        )
    })
}

fn live_group_propagation_warning(result: anyhow::Result<Option<String>>) -> Option<String> {
    result
        .err()
        .map(|error| format!("Saved durable groups; live update failed: {error:#}"))
}

fn live_management_group_propagation_warning(
    result: anyhow::Result<Option<String>>,
) -> Option<String> {
    result
        .err()
        .map(|error| format!("Adopted agent; live group update failed: {error:#}"))
}

fn receive_refresh(model: &mut SelectorModel, refresh: &mut WorkspaceLoad<SelectorSnapshot>) {
    match refresh.poll() {
        WorkspaceLoadPoll::Pending => {}
        WorkspaceLoadPoll::Ready(snapshot) => model.replace_snapshot(snapshot),
        WorkspaceLoadPoll::Failed(message) => model.mark_refresh_failed(message),
    }
}

fn receive_runtime_close(
    model: &mut SelectorModel,
    runtime_close: &mut Option<Receiver<RuntimeCloseWorkerResult>>,
) -> bool {
    let result = match runtime_close.as_ref().map(Receiver::try_recv) {
        Some(Ok(result)) => result,
        Some(Err(TryRecvError::Empty)) | None => return false,
        Some(Err(TryRecvError::Disconnected)) => RuntimeCloseWorkerResult::Failed(
            "runtime close worker stopped before reporting a result".to_string(),
        ),
    };
    *runtime_close = None;
    match result {
        RuntimeCloseWorkerResult::Closed(snapshot) => {
            model.archive_confirmation = None;
            model.runtime_close_succeeded(snapshot);
        }
        RuntimeCloseWorkerResult::ClosedRefreshFailed(message) => {
            model.archive_confirmation = None;
            model.runtime_close_refresh_failed(message)
        }
        RuntimeCloseWorkerResult::Failed(message) => {
            model.runtime_close_failed(message);
            if let Some(request) = &model.archive_confirmation {
                model.mode = SelectorMode::ConfirmRuntimeAction {
                    agent_key: request.review.cutex_session_id.as_str().into(),
                    action: if request.review.operation
                        == cutex::agent_management::AgentArchiveOperation::Restore
                    {
                        SessionTuiAction::RestoreSession
                    } else {
                        SessionTuiAction::RetireSession
                    },
                    launch_profile: None,
                    confirmed: false,
                };
                model.notice = Some("Retry reuses the exact action/evidence. Cancel abandons the review, not any completed stop or write.".into());
            }
        }
    }
    true
}

fn intent_runs_in_selector(intent: &SessionTuiIntent) -> bool {
    matches!(
        intent.action,
        SessionTuiAction::CloseRuntime
            | SessionTuiAction::UpgradeRuntimePackage
            | SessionTuiAction::RepairInterruptedHistory
            | SessionTuiAction::RetireSession
            | SessionTuiAction::RestoreSession
    )
}

fn close_runtime_shortcut_from_key(key: KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('x' | 'X'))
}

fn selector_navigation_control_from_key(
    model: &mut SelectorModel,
    key: KeyEvent,
) -> Option<SelectorControl> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        || key
            .modifiers
            .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }
    match key.code {
        KeyCode::Tab => Some(model.handle_focus_traversal(true)),
        KeyCode::BackTab => Some(model.handle_focus_traversal(false)),
        KeyCode::Right => Some(model.handle_horizontal_navigation(true)),
        KeyCode::Left => Some(model.handle_horizontal_navigation(false)),
        _ => None,
    }
}

fn selector_list_panel_from_horizontal_key(
    model: &SelectorModel,
    key: KeyEvent,
) -> Option<Option<PrimaryPanel>> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        || key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        || model.action_overlay.is_some()
        || model.settings_overlay.is_some()
        || model.profile_overlay.is_some()
    {
        return None;
    }
    let active = match &model.mode {
        SelectorMode::Agents if !model.inspector_overview_focused => PrimaryPanel::Agents,
        SelectorMode::RecentSessions
            if model.recent.review().is_none() && !model.recent.filter_focused() =>
        {
            PrimaryPanel::Recent
        }
        SelectorMode::Settings {
            target: SelectorTarget::GlobalSettings,
            focus: SettingsFocus::Categories,
            view: SettingsView::Categories,
            ..
        } => PrimaryPanel::Settings,
        SelectorMode::ProfileManager {
            focus: ProfileWorkspaceFocus::Items,
            ..
        } => PrimaryPanel::Profiles,
        _ => return None,
    };
    match key.code {
        KeyCode::Right => Some(active.adjacent(true)),
        KeyCode::Left => Some(active.adjacent(false)),
        _ => None,
    }
}

#[cfg(test)]
fn handle_managed_inspector_shortcut(model: &mut SelectorModel, key: KeyEvent) -> bool {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        || key.modifiers != KeyModifiers::ALT
        || model.action_overlay.is_some()
        || model.settings_overlay.is_some()
        || model.profile_overlay.is_some()
        || !model.shows_managed_inspector()
        || !matches!(
            &model.mode,
            SelectorMode::Agents | SelectorMode::Actions { .. } | SelectorMode::Settings { .. }
        )
    {
        return false;
    }
    match key.code {
        KeyCode::Char('a' | 'A') => {
            if matches!(&model.mode, SelectorMode::Settings { .. })
                && (model.settings_draft.is_dirty() || model.global_settings_draft.is_dirty())
            {
                model.warning =
                    Some("Save or discard staged settings before opening Actions".into());
            } else if !matches!(&model.mode, SelectorMode::Actions { .. }) {
                model.inspector_overview_focused = false;
                model.open_action_menu();
            }
            true
        }
        KeyCode::Char('e' | 'E') => {
            if !matches!(&model.mode, SelectorMode::Settings { .. }) {
                model.inspector_overview_focused = false;
                model.open_settings();
            }
            true
        }
        _ => false,
    }
}

#[cfg(test)]
fn toggle_managed_thread_titles_from_key(model: &mut SelectorModel, key: KeyEvent) -> bool {
    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && key.modifiers == KeyModifiers::ALT
        && matches!(key.code, KeyCode::Char('v' | 'V'))
        && matches!(model.mode, SelectorMode::Agents)
        && model.selected_managed_agent().is_some()
    {
        model.show_thread_titles = !model.show_thread_titles;
        true
    } else {
        false
    }
}

fn selector_event_from_key(key: KeyEvent, enhanced_keyboard: bool) -> Option<SelectorEvent> {
    workspace_event_from_key(key, enhanced_keyboard)
}

fn render_selector(frame: &mut Frame<'_>, model: &SelectorModel) {
    if let Some(text) = &model.details {
        views::render_details(
            frame,
            frame.area(),
            if matches!(model.mode, SelectorMode::RetiredSessions { .. }) { " Archive Details · Esc returns " } else { " CUTEX · Status / review details · read only " },
            text,
            &model.status_scroll,
        );
        return;
    }
    render_workspace(frame, model, &SelectorWorkspaceRenderer);
    if matches!(model.mode,SelectorMode::Agents) {model.facets.render_picker(frame);}
    if matches!(model.mode,SelectorMode::RecentSessions) {model.recent.facets.render_picker(frame);}
    if let Some((id, selected, confirming)) = &model.native_actions {
        let owned = model.native_owned_id(id).is_some();
        let text = if *confirming && owned {
            format!("Session: {id}\n\nClose this session's dedicated runtime.\nHistory is retained and the session stays in Recent Sessions.\n\nEnter: Close runtime    Esc: Cancel")
        } else if *confirming && *selected == 3 {
            format!("Session: {id}\n\nClose via native archive, then restore this session's history.\nNo runtime is restarted. You can Adopt afterwards.\nSpawned descendants are also closed and remain archived.\n\nEnter: Close and restore history    Esc: Cancel")
        } else if *confirming {
            format!("Session: {id}\n\nClose the runtime and archive this session and spawned descendants.\nHistory is retained. Restore with:\ncutex session restore-native {id}\n\nEnter: Close and archive    Esc: Cancel")
        } else {
            format!("Session: {id}\n\n{} Adopt as managed Agent\n{} Close and archive\n{} Revert history\n\n↑/↓ Select · Enter Open · Esc Back", if *selected == 0 { ">" } else { " " }, if *selected == 1 { ">" } else { " " }, if *selected == 2 { ">" } else { " " })
        };
        let text = if owned { text.replace("Close and archive", "Close runtime") } else if !confirming {
            text.replace("\n\n↑/↓", &format!("\n{} Close and restore history\n\n↑/↓", if *selected == 3 { ">" } else { " " }))
        } else { text };
        let text=if model.native_is_claude(id) && !confirming {format!("Session: {id}\n\n> Close runtime\n\nClaude history is retained. Enter resumes from the Sessions list.\n\nEnter Open · Esc Back")}else{text};
        views::render_details(frame, frame.area(), if *confirming { " Close session · Confirm " } else { " Session Actions " }, &text, &model.status_scroll);
    }
    if let Some(navigation) = &model.settings_navigation {
        navigation.render_titled(
            frame,
            &settings_navigation_commands(),
            " Global Settings · General / Profiles / Appearance · Utilities · Esc returns ",
        );
    }
    if let Some(help) = &model.help {
        help.render(frame, &selector_commands(model));
    }
    if let Some(review) = &model.leave_review {
        review.render(frame);
    }
}

struct SelectorWorkspaceRenderer;

impl WorkspaceRenderer<SelectorModel> for SelectorWorkspaceRenderer {
    fn render(&self, frame: &mut Frame<'_>, model: &SelectorModel) {
        render_selector_contents(frame, model);
    }
}

fn render_selector_contents(frame: &mut Frame<'_>, model: &SelectorModel) {
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(2),
    ])
    .split(area);
    let active_panel = if matches!(&model.mode, SelectorMode::ProfileManager { .. }) {
        PrimaryPanel::Profiles
    } else if matches!(&model.mode, SelectorMode::Settings { target: SelectorTarget::GlobalSettings, .. }) {
        PrimaryPanel::Settings
    } else if matches!(&model.mode, SelectorMode::RetiredSessions { .. }) {
        model.archive_return_panel
    } else if matches!(&model.mode, SelectorMode::RecentSessions) {
        PrimaryPanel::Recent
    } else {
        PrimaryPanel::Agents
    };
    frame.render_widget(
        Paragraph::new(crate::cli_app::session_tui_layout::tabs(active_panel, area.width)),
        chunks[0],
    );
    if !matches!(model.mode, SelectorMode::RecentSessions) {
        render_header(frame, chunks[1], model);
    }
    let main_area = Rect {
        y: chunks[2].y,
        height: chunks[2].height.saturating_add(chunks[3].height),
        ..chunks[2]
    };
    if model.shows_managed_inspector() {
        render_managed_workspace_with_inspector(frame, main_area, model);
    } else {
        match &model.mode {
            SelectorMode::Agents => {
                render_managed_list_pane(frame, main_area, model, true);
            }
            SelectorMode::RecentSessions => {
                render_recent_context(frame, chunks[1], model);
                render_recent_workspace(frame, main_area, model);
            }
            SelectorMode::RetiredSessions { .. } => {
                render_retired_workspace(frame, main_area, model);
            }
            SelectorMode::Actions { .. } => {
                render_item_context(frame, chunks[2], model);
                render_action_table(frame, chunks[3], model);
                render_action_overlay(frame, chunks[3], model);
            }
            SelectorMode::Settings { target: SelectorTarget::GlobalSettings, .. } => {
                render_settings_browser(frame, main_area, model);
                render_settings_overlay(frame, main_area, model);
            }
            SelectorMode::Settings { .. } => {
                render_item_context(frame, chunks[2], model);
                render_settings_browser(frame, chunks[3], model);
                render_settings_overlay(frame, chunks[3], model);
            }
            SelectorMode::ProfileManager { .. } => {
                render_profile_context(frame, chunks[2], model);
                render_profile_manager(frame, chunks[3], model);
                render_settings_overlay(frame, chunks[3], model);
                render_profile_overlay(frame, chunks[3], model);
            }
            SelectorMode::ConfirmRuntimeAction { .. } => {
                render_item_context(frame, chunks[2], model);
                render_runtime_action_confirmation(frame, chunks[3], model);
            }
            SelectorMode::ClosingRuntime { .. } => {
                render_item_context(frame, chunks[2], model);
                render_runtime_close_progress(frame, chunks[3], model);
            }
        }
    }
    let recent_status = match model.recent.load_state() {
        RecentLoadState::Failed(error) | RecentLoadState::ProviderIncompatible(error) => {
            format!("{error} · Enter retry page / F5 refresh")
        }
        _ if model.recent.loading() => "Loading Recent…".into(),
        _ => "Ready".into(),
    };
    let status = model
        .warning
        .as_deref()
        .or(model.notice.as_deref())
        .unwrap_or(if matches!(model.mode, SelectorMode::RecentSessions) {
            &recent_status
        } else if model.refreshing {
            "Refreshing…"
        } else {
            "Ready"
        });
    frame.render_widget(
        Paragraph::new(status).style(Style::new().fg(
            if model.warning.is_some()
                || (matches!(model.mode, SelectorMode::RecentSessions)
                    && matches!(
                        model.recent.load_state(),
                        RecentLoadState::Failed(_) | RecentLoadState::ProviderIncompatible(_)
                    ))
            {
                crate::cli_app::session_tui_layout::warning()
            } else {
                crate::cli_app::session_tui_layout::muted()
            },
        )),
        chunks[4],
    );
    render_footer(frame, chunks[5], model);
    let confirmed = match &model.mode {
        SelectorMode::ConfirmRuntimeAction { confirmed, .. } => Some(*confirmed),
        _ => None,
    };
    if let Some(confirmed) = confirmed {
        frame.render_widget(
            Paragraph::new(if confirmed {
                "Cancel  [Confirm] · Enter selected"
            } else {
                "[Cancel]  Confirm · Enter selected"
            })
            .style(Style::new().add_modifier(Modifier::BOLD)),
            Rect {
                y: main_area.bottom().saturating_sub(1),
                height: 1,
                ..main_area
            },
        );
    }
}

fn render_header(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    if matches!(model.mode, SelectorMode::Settings { target: SelectorTarget::GlobalSettings, .. }) {
        let mut spans = crate::cli_app::session_tui_layout::heading("Cutex","Settings").spans;
        let dirty = model.settings_dirty_count();
        if dirty > 0 { spans.push(Span::styled(format!("  Unsaved: {dirty}"), Style::new().fg(crate::cli_app::session_tui_layout::warning()))); }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }
    let (view, count) = match &model.mode {
        SelectorMode::Agents => ("Agents", model.visible_indices().len()),
        SelectorMode::RecentSessions => ("recent sessions", model.recent.visible_rows().len()),
        SelectorMode::RetiredSessions { .. } => ("Archive / Retired", model.retired_rows.len()),
        SelectorMode::Actions { .. } => (
            "actions",
            model
                .active_row()
                .map_or(0, SelectorRow::action_control_count),
        ),
        SelectorMode::Settings { .. } => (
            "settings",
            model.active_row().map_or(0, |row| {
                row.settings
                    .iter()
                    .map(|category| category.options.len())
                    .sum()
            }),
        ),
        SelectorMode::ProfileManager { profiles, .. } => {
            ("profiles", profiles.len().saturating_add(2))
        }
        SelectorMode::ConfirmRuntimeAction { .. } => ("confirm", 0),
        SelectorMode::ClosingRuntime { .. } => ("closing", 0),
    };
    let refresh = model
        .refreshing
        .then_some(Span::styled("  refreshing", Style::new().fg(Color::Yellow)));
    let mut spans = vec![
        Span::styled(
            "Cutex",
            Style::new().fg(crate::cli_app::session_tui_layout::focus()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {view}"),
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
    ];
    if count > 0 || matches!(&model.mode, SelectorMode::Agents) {
        spans.push(Span::styled(
            if matches!(model.mode, SelectorMode::Settings { .. }) { format!("  {count} settings") } else { format!("  {count} shown") },
            Style::new().fg(Color::DarkGray),
        ));
    }
    if matches!(&model.mode, SelectorMode::Agents) {
        spans.push(Span::styled(
            format!(
                "  {} · Alt+O scope · Alt+Z Archive / Retired",
                ["All", "Online", "Pinned"][model.managed_scope]
            ),
            Style::new().fg(crate::cli_app::session_tui_layout::focus()),
        ));
        if model.show_thread_titles {
            spans.push(Span::styled(
                "  thread titles expanded",
                Style::new().fg(Color::DarkGray),
            ));
        }
    }
    if matches!(model.mode, SelectorMode::Agents | SelectorMode::RecentSessions) {
        let order = if matches!(model.mode, SelectorMode::Agents) {model.sort_order} else {model.recent.sort_order};
        spans.push(Span::styled(format!("  Sort: {} · Alt+S", order.label()), Style::new().fg(crate::cli_app::session_tui_layout::focus())));
    }
    if let Some(settings_view) = model.settings_view() {
        let global = matches!(model.mode, SelectorMode::Settings { target: SelectorTarget::GlobalSettings, .. });
        if global {
            // Global Settings has one categorized layout.
        } else {
        spans.push(Span::styled("  view ", Style::new().fg(Color::DarkGray)));
        if area.width < SETTINGS_TWO_PANE_MIN_WIDTH {
            spans.push(Span::styled(
                format!("[{}]", settings_view.label()),
                Style::new().fg(crate::cli_app::session_tui_layout::focus()).add_modifier(Modifier::BOLD),
            ));
        } else {
            for view in [SettingsView::Expanded, SettingsView::Categories] {
                let label = if view == settings_view {
                    format!("[{}]", view.label())
                } else {
                    view.label().to_string()
                };
                let style = if view == settings_view {
                    Style::new().fg(crate::cli_app::session_tui_layout::focus()).add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(Color::DarkGray)
                };
                spans.push(Span::styled(label, style));
                if view == SettingsView::Expanded {
                    spans.push(Span::raw(" "));
                }
            }
        }
        }
        let dirty_count = model.settings_dirty_count();
        if dirty_count > 0 {
            spans.push(Span::styled(
                format!("  {dirty_count} pending"),
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ));
        }
    }
    if matches!(&model.mode, SelectorMode::ProfileManager { .. }) {
        let dirty_count = model.global_settings_draft.dirty_count();
        if dirty_count > 0 {
            spans.push(Span::styled(
                format!("  {dirty_count} pending"),
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ));
        }
    }
    spans.extend(refresh);
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_item_context(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    let Some(row) = model.active_row() else {
        frame.render_widget(
            Paragraph::new("Selected item is no longer available")
                .block(Block::bordered().title(" Selection "))
                .style(Style::new().fg(Color::Yellow)),
            area,
        );
        return;
    };
    let mut spans = vec![Span::styled(
        row.agent.as_str(),
        Style::new().add_modifier(Modifier::BOLD),
    )];
    if let Some(lifecycle) = row.lifecycle {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(lifecycle.label(), lifecycle_style(lifecycle)));
        spans.push(Span::styled(
            format!("  {}  {}", row.host, row.backend),
            Style::new().fg(Color::DarkGray),
        ));
    } else {
        spans.push(Span::styled(
            format!("  {}", row.backend),
            Style::new().fg(Color::DarkGray),
        ));
    }
    let title = match &row.target {
        SelectorTarget::RecentSessions => " Recent sessions ",
        SelectorTarget::RetiredSessions => " Archived Agents ",
        SelectorTarget::CutexProjects => " Cutex Projects ",
        SelectorTarget::Projects => " Workspaces ",
        SelectorTarget::Profiles => " Profiles ",
        SelectorTarget::GlobalSettings => " Global settings ",
        SelectorTarget::Agent(_) | SelectorTarget::RemoteAgent(..) | SelectorTarget::RetiredAgent(_) => " Agent ",
    };
    frame.render_widget(
        Paragraph::new(Line::from(spans)).block(
            Block::bordered()
                .title(title)
                .border_style(Style::new().fg(Color::DarkGray)),
        ),
        area,
    );
}

fn render_retired_workspace(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    let panes = crate::cli_app::session_tui_layout::list_details(area, model.inspector_visible);
    frame.render_widget(Paragraph::new("Archived and permanently retired identities")
        .style(Style::new().fg(Color::Gray))
        .block(Block::bordered().title(crate::cli_app::session_tui_layout::heading("Archive", "/ Retired"))), panes.filter);
    render_retired_table(frame, panes.list, model);
    if let Some(details) = panes.details {
        let selected = match model.mode { SelectorMode::RetiredSessions { selected } => selected, _ => 0 };
        let body = if let Some(row) = model.retired_rows.get(selected) {
            vec![
                Line::styled(row.agent.clone(), Style::new().fg(crate::cli_app::session_tui_layout::brand()).add_modifier(Modifier::BOLD)),
                Line::from(""),
                Line::from(format!("State: {}", if row.actions.is_empty() { "Retired (read-only)" } else { "Archived" })),
                Line::from(format!("Profile: {}", row.configured_profile.as_deref().unwrap_or("N/A"))),
                Line::from(format!("Managed path: {}", row.managed_path)),
                Line::from(format!("Retired at: {}", row.retired_at.as_deref().unwrap_or("N/A"))),
                Line::from(format!("Revision: {}", row.revision)),
                Line::from(""),
                Line::from(if row.actions.is_empty() { "Permanently retired; cannot restore." } else { "Enter opens restore confirmation." }),
            ]
        } else { vec![Line::from("No archived or retired sessions")] };
        frame.render_widget(Paragraph::new(body).wrap(Wrap { trim: false })
            .block(Block::bordered().title(crate::cli_app::session_tui_layout::heading("Archive", "Details"))), details);
    }
}

fn render_recent_context(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    if model.recent.review().is_some() {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "Adopt native thread",
                    Style::new().add_modifier(Modifier::BOLD),
                ),

            ])),
            area,
        );
        return;
    }
    let text = match model.recent.load_state() {
        RecentLoadState::Loading => "Loading native app-server threads…".to_string(),
        RecentLoadState::Ready => format!(
            "Native threads, newest first  {}",
            if model.recent.next_cursor().is_some() {
                "search loaded history · Alt+L load more"
            } else {
                "all history loaded"
            }
        ),
        RecentLoadState::Empty => {
            "No native threads were returned by the paired app-server".to_string()
        }
        RecentLoadState::ProviderIncompatible(message) => {
            format!("Provider incompatible: {message}")
        }
        RecentLoadState::Failed(message) => format!("Catalog unavailable: {message}"),
    };
    let mut heading = crate::cli_app::session_tui_layout::heading("Cutex", "Sessions");
    heading.spans.push(Span::styled(format!(
            " · Alt+Z Archive / Retired · {}/{} · {text}", model.recent.visible_rows().len(), model.recent.rows().len()), Style::new().fg(Color::DarkGray),
    ));
    frame.render_widget(Paragraph::new(heading), area);
}

fn render_recent_workspace(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    if let Some(row) = model.recent.review() {
        let [name_area, info_area, buttons] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(1), Constraint::Length(3)]).areas(area);
        input_policy::render_input(
            frame,
            name_area,
            model.recent.adoption_name().expect("review name"),
            " Formal Agent name · Tab to confirmation ",
            model.recent.adoption_name_focused(),
        );
        let lines = vec![
            Line::from(vec![
                Span::styled("Native title / preview (not Agent name): ", Style::new().fg(Color::DarkGray)),
                Span::raw(row.title.clone()),
            ]),
            Line::from(vec![
                Span::styled("Native thread id: ", Style::new().fg(Color::DarkGray)),
                Span::raw(row.thread_id.clone()),
            ]),
            Line::from(vec![
                Span::styled("Cwd: ", Style::new().fg(Color::DarkGray)),
                Span::raw(
                    row.cwd
                        .as_deref()
                        .map(truncate_recent_display)
                        .unwrap_or_else(|| "unavailable".to_string()),
                ),
            ]),
            Line::from(vec![
                Span::styled("Provider / source: ", Style::new().fg(Color::DarkGray)),
                Span::raw(format!("{} / {}", row.provider, row.source)),
            ]),
            Line::from(vec![
                Span::styled("Project assignment: ", Style::new().fg(Color::DarkGray)),
                Span::raw(
                    row.project_id
                        .clone()
                        .unwrap_or_else(|| "unassigned".to_string()),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Cutex defaults: persistent management; native title is metadata, never a formal Agent name; default runtime; no groups; IM hidden; unpinned.",
                Style::new().fg(crate::cli_app::session_tui_layout::focus()),
            )),
            Line::from(""),
        ];
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: true })
                .block(Block::bordered().title(" Review native thread ")),
            info_area,
        );
        let selected = model.recent.review_confirmed();
        let choice = |label: &str, chosen: bool| Span::styled(format!(" {label} "), if chosen { Style::new().fg(Color::White).bg(crate::cli_app::session_tui_layout::selection()).add_modifier(Modifier::BOLD) } else { Style::new().fg(crate::cli_app::session_tui_layout::muted()) });
        frame.render_widget(Paragraph::new(Line::from(vec![choice("Cancel", !selected), Span::raw("  "), choice("Adopt", selected), Span::raw("   ←/→ choose · Enter apply")]))
            .block(settings_panel_block(" Confirmation ".into(), !model.recent.adoption_name_focused())), buttons);
        return;
    }
    let panes = crate::cli_app::session_tui_layout::list_details(area, model.inspector_visible);
    if model.recent_inspecting && panes.details.is_none() {
        render_recent_details(frame, area, model);
        return;
    }
    if let Some(details) = panes.details { render_recent_details(frame, details, model); }
    let filter_area = panes.filter;
    let table_area = panes.list;
    model.recent.facets.render(frame,filter_area,model.recent.filter_input(),model.recent.filter_focused());
    let rows: Vec<_> = model
        .recent
        .visible_rows()
        .iter()
        .map(|row| row.view.clone())
        .collect();
    let mut state = model.recent_table.borrow_mut();
    state.select((!rows.is_empty()).then_some(model.recent.selected_visible()));
    views::render_table(frame, table_area, &rows, ListKind::Recent, &mut state);
}

fn render_recent_details(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    let rows = model.recent.visible_rows();
    let lines = rows.get(model.recent.selected_visible()).map(|row| {
        let field = |label: &str, value: String| Line::from(vec![Span::styled(format!("{label}: "), Style::new().fg(Color::Gray)), Span::raw(value)]);
        let mut lines = vec![
            Line::styled(row.title.clone(), Style::new().fg(crate::cli_app::session_tui_layout::text()).add_modifier(Modifier::BOLD)),
            field("Status", row.view.runtime.label()),
            field("Association", row.state.label().into()),
            field("Updated", row.view.updated.clone()),
            Line::default(),
            field("Agent", row.managed_name.clone().unwrap_or_else(|| "No linked Agent".into())),
            field("Project", row.view.project.label()),
            field("Directory", row.cwd.clone().unwrap_or_else(|| "N/A".into())),
            Line::default(),
            field("Provider", row.provider.clone()),
            field("Source", row.source.clone()),
            Line::styled("Technical identifiers", Style::new().fg(crate::cli_app::session_tui_layout::focus())),
            field("Session ID", row.thread_id.clone()),
        ];
        if row.view.runtime_version.is_some() || row.view.runtime_version_latest.is_some() {
            lines.extend([
                Line::default(),
                field(
                    "Runtime version",
                    row.view
                        .runtime_version
                        .clone()
                        .unwrap_or_else(|| "unknown".into()),
                ),
                field(
                    "Latest runtime",
                    row.view
                        .runtime_version_latest
                        .clone()
                        .unwrap_or_else(|| "unknown".into()),
                ),
            ]);
        }
        for (name, observation) in [("Status", &row.view.runtime), ("Project", &row.view.project), ("Profile", &row.view.effective_profile)] {
            if let Observation::Unavailable(reason) | Observation::Stale(_, reason) = observation {
                lines.push(field(&format!("{name} observation"), reason.clone()));
            }
        }
        lines
    }).unwrap_or_else(|| vec![Line::from("No session selected.")]);
    views::render_entity_details(frame, area, "Session Details", lines, &model.recent_detail_scroll, model.recent_inspecting);
}

fn truncate_recent_display(value: &str) -> String {
    const MAX_RECENT_DISPLAY_CHARS: usize = 160;
    let mut output = value
        .chars()
        .take(MAX_RECENT_DISPLAY_CHARS)
        .collect::<String>();
    if value.chars().count() > MAX_RECENT_DISPLAY_CHARS {
        output.push('…');
    }
    output
}

fn render_retired_table(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    let rows = model.retired_rows.iter().map(|row| {
        Row::new([
            Cell::from(row.agent.as_str()),
            Cell::from(if row.actions.is_empty() {
                "Retired"
            } else {
                "Archived"
            }),
            Cell::from(row.configured_profile.as_deref().unwrap_or("-")),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Min(16),
            Constraint::Length(8),
            Constraint::Length(16),
        ],
    )
    .header(
        Row::new([
            "AGENT / SESSION",
            "STATE", "PROFILE",
        ])
            .style(Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD))
            .bottom_margin(1),
    )
    .column_spacing(2)
    .block(Block::bordered().title(crate::cli_app::session_tui_layout::heading("Archived", "Agents / Sessions")))
    .row_highlight_style(Style::new().bg(crate::cli_app::session_tui_layout::selection()))
    .highlight_symbol("> ");
    let selected = match model.mode {
        SelectorMode::RetiredSessions { selected } => Some(selected),
        _ => None,
    };
    let mut state = TableState::default().with_selected(selected);
    frame.render_stateful_widget(table, area, &mut state);
}

fn render_profile_context(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    let selected = model.selected_profile();
    let body = if let Some(profile) = selected {
        let default_profile = model.current_default_profile_name();
        let mut spans = vec![Span::styled(
            profile.name.as_str(),
            Style::new().add_modifier(Modifier::BOLD),
        )];
        if profile.active {
            spans.push(Span::styled("  active home", Style::new().fg(Color::Green)));
        }
        if default_profile.as_deref() == Some(profile.name.as_str()) {
            spans.push(Span::styled(
                "  launch default",
                Style::new().fg(crate::cli_app::session_tui_layout::focus()),
            ));
        }
        Line::from(spans)
    } else if model.selected_profile_is_default() {
        Line::from(vec![
            Span::styled("Default", Style::new().add_modifier(Modifier::BOLD)),
            Span::styled("  launch policy", Style::new().fg(Color::DarkGray)),
        ])
    } else if model.selected_profile_is_add() {
        Line::from(vec![
            Span::styled("Add profile", Style::new().add_modifier(Modifier::BOLD)),
            Span::styled("  login", Style::new().fg(Color::DarkGray)),
        ])
    } else {
        Line::from(Span::styled("Profiles", Style::new().fg(Color::DarkGray)))
    };
    frame.render_widget(
        Paragraph::new(body).block(
            Block::bordered()
                .title(" Profiles ")
                .border_style(Style::new().fg(Color::DarkGray)),
        ),
        area,
    );
}

fn render_profile_manager(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    if area.width >= PROFILE_TWO_PANE_MIN_WIDTH {
        let list_width = (area.width * 46 / 100).clamp(70, 76);
        let panes =
            Layout::horizontal([Constraint::Length(list_width), Constraint::Min(24)]).split(area);
        render_profile_list(frame, panes[0], model);
        render_profile_details(frame, panes[1], model);
    } else {
        match model.profile_workspace_focus() {
            Some(ProfileWorkspaceFocus::Items) | None => render_profile_list(frame, area, model),
            Some(ProfileWorkspaceFocus::Editor) => render_profile_details(frame, area, model),
        }
    }
}

fn render_profile_list(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    let SelectorMode::ProfileManager { profiles, .. } = &model.mode else {
        return;
    };
    let default_profile = model.current_default_profile_name();
    let default_style = Style::new().fg(crate::cli_app::session_tui_layout::focus()).add_modifier(Modifier::BOLD);
    let outer = settings_panel_block(" Profiles ".to_string(), model.profile_workspace_focus() == Some(ProfileWorkspaceFocus::Items));
    let inner = outer.inner(area);
    let [defaults_area, spacer_area, table_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
    ])
    .areas(inner);
    let compact = table_area.width < 68;
    let mut rows = profiles.iter().map(|profile| {
        Row::new([
            Cell::from(profile.name.clone())
                .style(super::identity_styles::profile(&profile.name)),
            Cell::from(profile_dashboard_usage(profile, compact))
                .style(super::identity_styles::profile(&profile.name)),
            Cell::from(profile_dashboard_type(profile))
                .style(super::identity_styles::profile(&profile.name)),
            Cell::from(profile_dashboard_reset(profile)).style(Style::new().fg(Color::DarkGray)),
        ])
    }).collect::<Vec<_>>();
    rows.push(Row::new([
        Cell::from("Add profile").style(default_style),
        Cell::from("new").style(Style::new().fg(Color::DarkGray)),
        Cell::from(""),
        Cell::from(""),
    ]));
    let header = Row::new(["PROFILE", "USAGE", "TYPE", "RESETS"]);
    let widths = if compact {
        [
            Constraint::Min(12),
            Constraint::Length(19),
            Constraint::Length(14),
            Constraint::Length(6),
        ]
    } else {
        [
            Constraint::Min(16),
            Constraint::Length(23),
            Constraint::Length(16),
            Constraint::Length(9),
        ]
    };
    let active = model.profile_workspace_focus() == Some(ProfileWorkspaceFocus::Items);
    let table = Table::new(rows, widths)
        .header(
            header
                .style(Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD))
                .bottom_margin(0),
        )
        .column_spacing(1)
        .row_highlight_style(settings_highlight_style(active))
        .highlight_symbol("> ")
        .highlight_spacing(HighlightSpacing::Always);
    frame.render_widget(outer, area);
    let defaults_selected = model.selected_profile_is_default() && active;
    let default_name = default_profile.as_deref().unwrap_or("n/a");
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                if defaults_selected { "> Defaults" } else { "  Defaults" },
                if defaults_selected {
                    Style::new().fg(crate::cli_app::session_tui_layout::focus()).add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD)
                },
            ),
            Span::raw("  "),
            Span::styled(default_name, super::identity_styles::profile(default_name)),
        ])),
        defaults_area,
    );
    frame.render_widget(Paragraph::new(""), spacer_area);
    let mut state = TableState::default();
    if !model.selected_profile_is_default() {
        state.select(Some(
            model.selected_profile_index().unwrap_or(0).saturating_sub(1),
        ));
    }
    frame.render_stateful_widget(table, table_area, &mut state);
}

fn render_profile_details(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    let Some(profile) = model.selected_profile() else {
        if model.selected_profile_is_default() {
            render_profile_default_editor(frame, area, model);
            return;
        }
        if model.selected_profile_is_add() {
            frame.render_widget(
                Paragraph::new(vec![
                    profile_detail_line("Name", "Add profile"),
                    profile_detail_line("Status", "ready"),
                    profile_detail_line("Flow", "Codex login / shared Claude login"),
                ])
                .block(settings_panel_block(
                    " Add profile ".to_string(),
                    model.profile_workspace_focus() == Some(ProfileWorkspaceFocus::Editor),
                )),
                area,
            );
            return;
        }
        frame.render_widget(
            Paragraph::new("-")
                .block(settings_panel_block(" Details ".to_string(), false))
                .style(Style::new().fg(Color::DarkGray)),
            area,
        );
        return;
    };
    let categories = model.selected_profile_setting_categories();
    let rows = categories
        .iter()
        .flat_map(|category| {
            std::iter::once(
                Row::new([Cell::from(category.label), Cell::from("")])
                    .style(Style::new().fg(crate::cli_app::session_tui_layout::focus()).add_modifier(Modifier::BOLD)),
            )
            .chain(category.options.iter().map(|option| {
                let name = option
                    .presentation
                    .name
                    .as_deref()
                    .unwrap_or(option.label);
                let label = if option.dirty {
                    format!("  {name} *")
                } else {
                    format!("  {name}")
                };
                let value_style = setting_option_style(option);
                Row::new([
                    Cell::from(label).style(value_style),
                    Cell::from(option.value.as_str()).style(value_style),
                ])
            }))
        })
        .collect::<Vec<_>>();
    let header = Row::new(["SETTING", "VALUE"])
        .style(Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD))
        .bottom_margin(1);
    let widths = if area.width < SETTINGS_TWO_PANE_MIN_WIDTH {
        [Constraint::Percentage(48), Constraint::Percentage(52)]
    } else {
        [Constraint::Percentage(40), Constraint::Percentage(60)]
    };
    let active = model.profile_workspace_focus() == Some(ProfileWorkspaceFocus::Editor);
    let title = if model.profile_settings_draft.is_dirty() {
        format!(
            " {} [{} staged] ",
            profile.name,
            model.profile_settings_draft.dirty_count()
        )
    } else {
        format!(" {} ", profile.name)
    };
    let table = Table::new(rows, widths)
        .header(header)
        .block(settings_panel_block(title, active))
        .column_spacing(2)
        .row_highlight_style(settings_highlight_style(active))
        .highlight_symbol(if active { "> " } else { "  " });
    let selected = match &model.mode {
        SelectorMode::ProfileManager {
            editor_selected, ..
        } => setting_indices_at_flat_index(&categories, *editor_selected).and_then(
            |(category, option)| expanded_setting_table_row_index(&categories, category, option),
        ),
        _ => None,
    };
    let mut state = TableState::default().with_selected(selected);
    frame.render_stateful_widget(table, area, &mut state);
}

fn render_profile_default_editor(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    let default_profile = model
        .profile_default_value(GlobalSettingsField::DefaultProfile)
        .unwrap_or_else(|| "-".to_string());
    let direct_launch = model
        .profile_default_value(GlobalSettingsField::DefaultProfileDirectLaunch)
        .unwrap_or_else(|| "disabled".to_string());
    let fields = [
        (
            GlobalSettingsField::DefaultProfile,
            "Default profile",
            default_profile,
        ),
        (
            GlobalSettingsField::DefaultProfileDirectLaunch,
            "Direct default launch",
            direct_launch,
        ),
    ];
    let mut rows = vec![Row::new([Cell::from("Launch defaults"), Cell::from("")])
        .style(Style::new().fg(crate::cli_app::session_tui_layout::focus()).add_modifier(Modifier::BOLD))];
    rows.extend(fields.into_iter().map(|(field, label, value)| {
        let label = if model.global_settings_draft.field_is_dirty(field) {
            format!("  {label} *")
        } else {
            format!("  {label}")
        };
        Row::new([
            Cell::from(label),
            Cell::from(value).style(Style::new().fg(Color::White)),
        ])
    }));
    let active = model.profile_workspace_focus() == Some(ProfileWorkspaceFocus::Editor);
    let widths = if area.width < 64 {
        [Constraint::Percentage(60), Constraint::Percentage(40)]
    } else {
        [Constraint::Percentage(48), Constraint::Percentage(52)]
    };
    let table = Table::new(rows, widths)
        .block(settings_panel_block(" Default ".to_string(), active))
        .column_spacing(2)
        .row_highlight_style(settings_highlight_style(active))
        .highlight_symbol(if active { "> " } else { "  " });
    let selected = model
        .selected_profile_default_option()
        .map(|index| index + 1);
    let mut state = TableState::default().with_selected(selected);
    frame.render_stateful_widget(table, area, &mut state);
}

fn profile_detail_line(label: &'static str, value: impl Into<String>) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<11}"), Style::new().fg(Color::DarkGray)),
        Span::raw(value.into()),
    ])
}

fn profile_dashboard_usage(profile: &ProfileCatalogEntry, compact: bool) -> String {
    match profile.official_usage.as_ref() {
        Some(super::profile_usage::ProfileUsageSnapshot::Refreshing) => {
            "refreshing…".to_string()
        }
        Some(super::profile_usage::ProfileUsageSnapshot::Unavailable(_)) => "error".to_string(),
        Some(super::profile_usage::ProfileUsageSnapshot::Meters(meters)) => {
            let Some(meter) = meters
                .iter()
                .min_by_key(|meter| meter.remaining_percent)
            else {
                return "unavailable".to_string();
            };
            let value = meter
                .balance
                .clone()
                .unwrap_or_else(|| compact_usage_bar(meter.remaining_percent, compact));
            // Claude subscription dashboards deliberately show the tightest
            // quota only. The meter name (for example `Fable`) remains visible
            // in the detailed rows, where it identifies the measurement scope.
            if meters.len() == 1 || profile.cli_kind == "claude" {
                value
            } else {
                format!("{value} {}", profile_dashboard_meter_label(&meter.label))
            }
        }
        None => {
            if !profile.options.quota.enabled {
                return "disabled".to_string();
            }
            if let Some(balance) = profile.quota_balance.as_deref() {
                return balance.to_string();
            }
            let Some(percent) = profile.quota_remaining_percent else {
                return "not read".to_string();
            };
            if profile.quota_stale {
                format!("{} Q!", compact_usage_bar(percent, compact))
            } else {
                compact_usage_bar(percent, compact)
            }
        }
    }
}

fn compact_usage_bar(percent: u8, compact: bool) -> String {
    format!("{} {}%", compact_bar(percent, compact), percent)
}

fn profile_dashboard_meter_label(label: &str) -> String {
    let label = label.trim();
    label.strip_suffix(" 7d").unwrap_or(label).to_string()
}

fn compact_bar(percent: u8, compact: bool) -> String {
    let cells = if compact { 8 } else { 10 };
    let filled = (usize::from(percent) * cells / 100).min(cells);
    format!("[{}{}]", "█".repeat(filled), "░".repeat(cells - filled))
}

fn profile_dashboard_type(profile: &ProfileCatalogEntry) -> String {
    if let Some(plan) = profile.display_plan() {
        let plan = plan.as_str();
        return match plan.to_ascii_lowercase().as_str() {
            "pro" if profile.cli_kind == "codex" => "GPT Pro 20x".to_string(),
            "prolite" | "self_serve_business_prolite" if profile.cli_kind == "codex" => {
                "GPT Pro 5x".to_string()
            }
            "glm" => "GLM API".to_string(),
            _ => plan.to_string(),
        };
    }
    if profile.source.as_deref() == Some("official") {
        return match profile.cli_kind.as_str() {
            "claude" => "Claude Max 5x".to_string(),
            "codex" => "GPT account".to_string(),
            _ => "Official account".to_string(),
        };
    }
    if profile.source.as_deref() == Some("api-key") {
        let provider = profile
            .codex_config
            .as_ref()
            .and_then(|config| config.provider_name.as_deref())
            .or_else(|| config_model_provider(profile));
        if let Some(provider) = provider {
            return format!("{provider} API");
        }
        return "API key".to_string();
    }
    profile.cli_kind.clone()
}

fn config_model_provider(profile: &ProfileCatalogEntry) -> Option<&str> {
    profile
        .codex_config
        .as_ref()
        .and_then(|config| config.model_provider.as_deref())
}

fn profile_dashboard_reset(profile: &ProfileCatalogEntry) -> String {
    let Some(reset_at) = profile_dashboard_reset_at(profile) else {
        return "n/a".to_string();
    };
    let seconds = reset_at - Utc::now().timestamp();
    if seconds <= 0 {
        return "now".to_string();
    }
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    if days > 0 {
        return format!("{days}d {hours}h");
    }
    let minutes = (seconds % 3_600) / 60;
    if hours > 0 {
        return format!("{hours}h {minutes}m");
    }
    if minutes > 0 {
        return format!("{minutes}m");
    }
    "<1m".to_string()
}

fn profile_dashboard_reset_at(profile: &ProfileCatalogEntry) -> Option<i64> {
    match profile.official_usage.as_ref() {
        Some(super::profile_usage::ProfileUsageSnapshot::Meters(meters)) => meters
            .iter()
            .min_by_key(|meter| meter.remaining_percent)
            .and_then(|meter| meter.reset_at),
        Some(_) => None,
        None => profile.quota_reset_at,
    }
}

fn render_profile_overlay(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    let Some(overlay) = model.profile_overlay.as_ref() else {
        return;
    };
    match overlay {
        ProfileOverlay::Actions {
            profile_name,
            actions,
            selected,
            ..
        } => {
            let modal = centered_rect(48, actions.len() as u16 + 2, area);
            frame.render_widget(Clear, modal);
            let items = actions
                .iter()
                .map(|action| {
                    let style = if *action == ProfileManagerAction::Remove {
                        Style::new().fg(Color::Yellow)
                    } else {
                        Style::new()
                    };
                    ListItem::new(action.label()).style(style)
                })
                .collect::<Vec<_>>();
            let list = List::new(items)
                .block(settings_panel_block(
                    format!(" {} actions ", profile_name),
                    true,
                ))
                .highlight_style(settings_highlight_style(true))
                .highlight_symbol("> ");
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(list, modal, &mut state);
        }
        ProfileOverlay::RenameInput {
            old_name, input, ..
        } => {
            let modal = centered_rect(64, 3, area);
            frame.render_widget(Clear, modal);
            let input_width = modal.width.saturating_sub(2) as usize;
            let scroll = input.visual_scroll(input_width.saturating_sub(1).max(1));
            frame.render_widget(
                Paragraph::new(input.value())
                    .scroll((0, scroll as u16))
                    .block(settings_panel_block(format!(" Rename {} ", old_name), true)),
                modal,
            );
            if input_width > 0 {
                let cursor = input
                    .visual_cursor()
                    .saturating_sub(scroll)
                    .min(input_width.saturating_sub(1));
                frame.set_cursor_position((modal.x + 1 + cursor as u16, modal.y + 1));
            }
        }
        ProfileOverlay::ConfirmRename {
            old_name,
            new_name,
            selected,
            ..
        } => {
            let modal = centered_rect(68, 9, area);
            frame.render_widget(Clear, modal);
            let block = settings_panel_block(" Confirm rename ".to_string(), true);
            let inner = block.inner(modal);
            frame.render_widget(block, modal);
            let [description_area, choices_area] =
                Layout::vertical([Constraint::Min(4), Constraint::Length(2)]).areas(inner);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(format!("Rename {old_name} to {new_name}?")),
                    Line::from("Updates Global default, QuickRun, and durable session references."),
                ])
                .wrap(Wrap { trim: false }),
                description_area,
            );
            let items = ["Cancel", "Rename"]
                .into_iter()
                .map(ListItem::new)
                .collect::<Vec<_>>();
            let list = List::new(items)
                .highlight_style(settings_highlight_style(true))
                .highlight_symbol("> ");
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(list, choices_area, &mut state);
        }
        ProfileOverlay::ConfirmRemove {
            profile_name,
            selected,
            ..
        } => {
            let modal = centered_rect(68, 10, area);
            frame.render_widget(Clear, modal);
            let block = settings_panel_block(" Confirm remove ".to_string(), true);
            let inner = block.inner(modal);
            frame.render_widget(block, modal);
            let [description_area, choices_area] =
                Layout::vertical([Constraint::Min(5), Constraint::Length(2)]).areas(inner);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(format!("Remove profile {profile_name}?")),
                    Line::from("Clears Global default, QuickRun, and durable session references."),
                    Line::from("Materialized profile files are retained."),
                ])
                .wrap(Wrap { trim: false }),
                description_area,
            );
            let items = ["Cancel", "Remove"]
                .into_iter()
                .map(ListItem::new)
                .collect::<Vec<_>>();
            let list = List::new(items)
                .highlight_style(settings_highlight_style(true))
                .highlight_symbol("> ");
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(list, choices_area, &mut state);
        }
        ProfileOverlay::ConfirmAddProfile { selected } => {
            let modal = centered_rect(68, 10, area);
            frame.render_widget(Clear, modal);
            let block = settings_panel_block(" Add profile ".to_string(), true);
            let inner = block.inner(modal);
            frame.render_widget(block, modal);
            let [description_area, choices_area] =
                Layout::vertical([Constraint::Min(5), Constraint::Length(2)]).areas(inner);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from("Add a Codex or Claude Code profile?"),
                    Line::from(
                        "Codex uses its login flow. Claude Code uses the shared official login.",
                    ),
                ])
                .wrap(Wrap { trim: false }),
                description_area,
            );
            let items = ["Cancel", "Continue"]
                .into_iter()
                .map(ListItem::new)
                .collect::<Vec<_>>();
            let list = List::new(items)
                .highlight_style(settings_highlight_style(true))
                .highlight_symbol("> ");
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(list, choices_area, &mut state);
        }
        ProfileOverlay::ConfirmDiscardProfile {
            destination,
            selected,
        } => {
            let modal = centered_rect(52, 4, area);
            frame.render_widget(Clear, modal);
            let leave_label = match destination {
                ProfileDiscardDestination::ProfileList => "Discard and view profiles",
                ProfileDiscardDestination::AgentList => "Discard and view agents",
            };
            let items = ["Keep editing", leave_label]
                .into_iter()
                .map(ListItem::new)
                .collect::<Vec<_>>();
            let list = List::new(items)
                .block(settings_panel_block(
                    " Unsaved profile changes ".to_string(),
                    true,
                ))
                .highlight_style(settings_highlight_style(true))
                .highlight_symbol("> ");
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(list, modal, &mut state);
        }
    }
}

fn render_managed_workspace_with_inspector(
    frame: &mut Frame<'_>,
    area: Rect,
    model: &SelectorModel,
) {
    let list_focused =
        matches!(&model.mode, SelectorMode::Agents) && !model.inspector_overview_focused;
    if let Some((list, inspector)) =
        crate::cli_app::session_tui_layout::inspector_panes(area, model.inspector_visible)
    {
        render_managed_list_pane(frame, list, model, list_focused);
        render_agent_inspector(frame, inspector, model);
    } else if list_focused {
        render_managed_list_pane(frame, area, model, true);
    } else {
        render_agent_inspector(frame, area, model);
    }
}

fn render_managed_list_pane(
    frame: &mut Frame<'_>,
    area: Rect,
    model: &SelectorModel,
    focused: bool,
) {
    let chunks = Layout::vertical([
        Constraint::Length(if area.height < 7 { 1 } else { 3 }),
        Constraint::Min(1),
    ])
    .split(area);
    render_filter(frame, chunks[0], model, focused);
    render_table(frame, chunks[1], model);
}

fn render_agent_inspector(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    let active = model.inspector_is_focused();
    let overview = matches!(model.mode, SelectorMode::Agents);
    if overview {
        if let Some(row) = model.active_row() { render_inspector_overview(frame, area, model, row); }
        else { views::render_entity_details(frame, area, "Agent Details", vec![Line::from("No Agent selected.")], &model.detail_scroll, active); }
        return;
    }
    let block = Block::bordered()
        .title(" Inspector ")
        .border_style(Style::new().fg(if active { crate::cli_app::session_tui_layout::focus() } else { Color::DarkGray }));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(if overview { 0 } else { 2 }),
        Constraint::Min(1),
    ])
    .split(inner);
    let section = model.inspector_section();
    let mut tabs = Vec::new();
    for (index, candidate) in InspectorSection::ALL.into_iter().enumerate() {
        if index > 0 {
            tabs.push(Span::styled(" | ", Style::new().fg(Color::DarkGray)));
        }
        let label = if candidate == InspectorSection::Actions {
            format!("{} [Alt+A]", candidate.label())
        } else if candidate == InspectorSection::Settings {
            format!("{} [Alt+E]", candidate.label())
        } else {
            candidate.label().to_string()
        };
        tabs.push(Span::styled(
            label,
            if candidate == section {
                Style::new().fg(crate::cli_app::session_tui_layout::focus()).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::Gray)
            },
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(tabs)), chunks[0]);

    let Some(row) = model.active_row() else {
        frame.render_widget(
            Paragraph::new("Selected Agent is no longer available")
                .style(Style::new().fg(Color::Yellow)),
            chunks[2],
        );
        return;
    };
    let lifecycle = row
        .lifecycle
        .map(CutexSessionLifecycleState::label)
        .unwrap_or("-");
    if !overview {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    row.agent.as_str(),
                    Style::new().add_modifier(Modifier::BOLD),
                )),
                Line::from(vec![
                    Span::styled(
                        lifecycle,
                        row.lifecycle.map(lifecycle_style).unwrap_or_default(),
                    ),
                    Span::styled(
                        format!("  {}  {}", row.host, row.backend),
                        Style::new().fg(Color::DarkGray),
                    ),
                ]),
            ]),
            chunks[1],
        );
    }

    match &model.mode {
        SelectorMode::Agents => render_inspector_overview(frame, chunks[2], model, row),
        SelectorMode::Actions { .. } => {
            render_action_table(frame, chunks[2], model);
            render_action_overlay(frame, chunks[2], model);
        }
        SelectorMode::Settings { .. } => {
            render_inspector_settings(frame, chunks[2], model, row);
            render_settings_overlay(frame, chunks[2], model);
        }
        SelectorMode::ConfirmRuntimeAction { .. } => {
            render_runtime_action_confirmation(frame, chunks[2], model)
        }
        SelectorMode::ClosingRuntime { .. } => {
            render_runtime_close_progress(frame, chunks[2], model)
        }
        SelectorMode::RecentSessions
        | SelectorMode::RetiredSessions { .. }
        | SelectorMode::ProfileManager { .. } => {}
    }
}

fn render_inspector_overview(
    frame: &mut Frame<'_>,
    area: Rect,
    model: &SelectorModel,
    row: &SelectorRow,
) {
    let mut view = row
        .view
        .clone()
        .unwrap_or_else(|| selector_view(row, selector_default_profile_name(model)));
    view.name = row.agent.clone();
    view.configured_profile = row.configured_profile.clone();
    view.native_title = row.thread_title.clone();
    view.activity_details = selector_activity_details(row.activity.as_ref());
    views::render_agent_details(frame, area, &view, &model.detail_scroll, model.inspector_is_focused());
}

fn render_inspector_settings(
    frame: &mut Frame<'_>,
    area: Rect,
    model: &SelectorModel,
    row: &SelectorRow,
) {
    if let Some(project) = row.project.as_ref() {
        let chunks = Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).split(area);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!(" {} ", project.badge_label),
                    super::session_tui_cutex_projects::project_badge_style(project.color),
                ),
                Span::raw(format!(" {}  ", project.display_name)),
                Span::styled("Alt+P edit", Style::new().fg(crate::cli_app::session_tui_layout::focus())),
            ]))
            .block(Block::bordered().title(" Project badge settings ")),
            chunks[0],
        );
        render_settings_browser(frame, chunks[1], model);
    } else {
        render_settings_browser(frame, area, model);
    }
}

fn render_filter(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel, focused: bool) {
    model.facets.render(frame, area, &model.query, focused && model.filter_focused);
}

fn open_facet_picker(model: &mut SelectorModel) {
    use super::session_tui_filters::{Choice,project_key};
    let recent=matches!(model.mode,SelectorMode::RecentSessions);
    let views:Vec<_>=if recent {model.recent.all_views()}else{model.rows.iter().map(|r|r.view.clone().unwrap_or_else(||selector_view(r,None))).collect()};
    let f=if recent {&mut model.recent.facets}else{&mut model.facets};
    let mut choices=Vec::new();
    let mut seen=std::collections::HashSet::new();
    for v in views {
        let c=if f.field==2 {Some(Choice{key:v.host.to_lowercase(),label:cutex::management::connections::short_display(&v.host)})}
        else if f.host.as_ref().is_none_or(|h|h.key.eq_ignore_ascii_case(&v.host)) {v.project_id.as_ref().map(|id|Choice{key:project_key(&v.host,id),label:format!("{} · {}",v.project.label(),cutex::management::connections::short_display(&v.host))})}else{None};
        if let Some(c)=c {if !c.key.is_empty() && seen.insert(c.key.clone()){choices.push(c);}}
    }
    f.open(choices);
}

fn selector_view(row: &SelectorRow, _default_profile: Option<&str>) -> AgentSessionView {
    AgentSessionView {
        remote_provider: None, remote_profile_style: None,
            host: row.host.clone(),
        badge: row.project.as_ref().map(|p| views::ProjectBadge { label: p.badge_label.clone(), color: p.color }),
        project_id: row.project.as_ref().map(|p| p.project_id.clone()),
        subject: SubjectRef::Managed(
            row.activity_session_id
                .clone()
                .or_else(|| row.target.agent_key().map(str::to_owned))
                .unwrap_or_default(),
        ),
        name: row.agent.clone(),
        native_title: row.thread_title.clone(),
        native_thread: None,
        native_workspace: None,
        compaction_count: None,
        runtime: row
            .lifecycle
            .map(|state| Observation::Known(format!("{state:?}")))
            .unwrap_or_else(|| Observation::Unavailable("runtime not observed".into())),
        runtime_version: None,
        runtime_version_latest: None,
        runtime_version_recorded_at: None,
        project: Observation::Known(
            row.project
                .as_ref()
                .map(|p| p.display_name.clone())
                .unwrap_or_else(|| "unassigned".into()),
        ),
        configured_profile: row.configured_profile.clone(),
        effective_profile: Observation::Unavailable("effective runtime profile not observed; configured/default profile is next-launch configuration".into()),
        role: String::new(),
        activity: format_selector_activity(row.activity.as_ref(), Utc::now()),
        activity_details: selector_activity_details(row.activity.as_ref()),
        updated: "—".into(),
        cwd: row.managed_path.clone(),
        retirement_note: None,
    }
}

fn render_table(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    let rows: Vec<_> = model
        .visible_rows()
        .iter()
        .map(|row| {
            let mut view = row
                .view
                .clone()
                .unwrap_or_else(|| selector_view(row, selector_default_profile_name(model)));
            view.name = row.agent.clone();
            view.configured_profile = row.configured_profile.clone();
            view.activity = format_selector_activity(row.activity.as_ref(), Utc::now());
            if model.show_thread_titles {
                if let Some(title) = &row.thread_title {
                    view.name.push_str(&format!(" · {title}"));
                }
            }
            view
        })
        .collect();
    let mut state = model.managed_table.borrow_mut();
    state.select(model.selected_visible_index());
    views::render_table(frame, area, &rows, ListKind::Managed, &mut state);
}

fn render_action_table(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    let Some(row) = model.active_row() else {
        return;
    };
    let global_default_profile = model.current_default_profile_name();
    let mut rows = Vec::with_capacity(row.action_control_count());
    if row.launch_profile_control_available() {
        rows.push(
            Row::new([
                Cell::from("Launch profile").style(Style::new().fg(crate::cli_app::session_tui_layout::focus())),
                Cell::from(row.launch_profile_detail(
                    model.selected_launch_profile(),
                    global_default_profile.as_deref(),
                ))
                .style(Style::new().fg(Color::Gray)),
            ])
            .style(Style::new().add_modifier(Modifier::BOLD)),
        );
    }
    rows.extend(row.actions.iter().map(|item| {
        let label = if item.primary {
            format!("{}  primary", item.action.label())
        } else {
            item.action.label().to_string()
        };
        let style = if item.action.requires_confirmation() {
            Style::new().fg(Color::Yellow)
        } else {
            Style::new()
        };
        Row::new([
            Cell::from(label).style(style),
            Cell::from(item.detail).style(Style::new().fg(Color::Gray)),
        ])
    }));
    let header = Row::new(["ACTION", "DETAILS"])
        .style(Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD))
        .bottom_margin(1);
    let table = Table::new(rows, [Constraint::Length(24), Constraint::Min(24)])
        .header(header)
        .column_spacing(2)
        .row_highlight_style(
            Style::new()
                .fg(Color::White)
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    let mut state = TableState::default().with_selected(model.selected_action_index());
    frame.render_stateful_widget(table, area, &mut state);
}

fn render_action_overlay(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    let Some(ActionOverlay::LaunchProfile { choices, selected }) = model.action_overlay.as_ref()
    else {
        return;
    };
    let modal = centered_rect(54, choices.len() as u16 + 2, area);
    frame.render_widget(Clear, modal);
    let items = choices
        .iter()
        .map(|choice| ListItem::new(choice.label.as_str()))
        .collect::<Vec<_>>();
    let list = List::new(items)
        .block(settings_panel_block(" Launch profile ".to_string(), true))
        .highlight_style(settings_highlight_style(true))
        .highlight_symbol("> ");
    let mut state = ListState::default().with_selected(Some(*selected));
    frame.render_stateful_widget(list, modal, &mut state);
}

fn render_settings_browser(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    match model.settings_view() {
        Some(SettingsView::Expanded) => render_expanded_settings(frame, area, model),
        Some(SettingsView::Categories) => render_categorized_settings(frame, area, model),
        None => {}
    }
}

fn setting_option_style(option: &SessionTuiSettingOption) -> Style {
    let actionable = option.field.is_some() || option.global_field.is_some()
        || option.profile_field.is_some() || option.command.is_some() || option.navigation.is_some();
    Style::new().fg(if actionable { Color::White } else { crate::cli_app::session_tui_layout::muted() })
}

fn notification_preview() -> cutex::notify::session::Labels {
    cutex::notify::session::labels().unwrap_or_default()
}

fn notification_span(label: String, style: &cutex::notify::session::ItemStyle) -> Span<'static> {
    let color = style.fg.parse::<Color>().unwrap_or(Color::White);
    let mut rendered = Style::new().fg(color);
    if style.bold { rendered = rendered.add_modifier(Modifier::BOLD); }
    Span::styled(label, rendered)
}

fn setting_value_line(option: &SessionTuiSettingOption) -> Line<'static> {
    if let Some(color) = option.presentation.swatch {
        return Line::from(vec![Span::styled("  ", Style::new().bg(color)), Span::raw("  "), Span::styled(option.value.clone(), setting_option_style(option))]);
    }
    if let Some(style) = option.presentation.style {
        return Line::styled(option.value.clone(), style);
    }
    if option.label == "Session priority" {
        let labels = notification_preview();
        return Line::from(vec![
            notification_span(labels.important, &labels.styles.important), Span::raw(" / "),
            notification_span(labels.normal, &labels.styles.normal), Span::raw(" / "),
            notification_span(labels.off, &labels.styles.off), Span::raw("; Alt+N in cute-codex"),
        ]).style(setting_option_style(option));
    }
    if option.global_field == Some(GlobalSettingsField::DefaultNotification) {
        let labels = notification_preview();
        let (label, style) = match option.value.as_str() {
            "important" | "CIAO!" => (labels.important, labels.styles.important),
            "normal" | "ON" => (labels.normal, labels.styles.normal),
            "off" | "OFF" => (labels.off, labels.styles.off),
            _ => return Line::styled(option.value.clone(), setting_option_style(option)),
        };
        return Line::from(notification_span(label, &style));
    }
    Line::styled(option.value.clone(), setting_option_style(option))
}

fn render_expanded_settings(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    let Some(row) = model.active_row() else {
        return;
    };
    let rows = row
        .settings
        .iter()
        .flat_map(|category| {
            std::iter::once(
                Row::new([Cell::from(category.label), Cell::from("")])
                    .style(Style::new().fg(crate::cli_app::session_tui_layout::focus()).add_modifier(Modifier::BOLD)),
            )
            .chain(category.options.iter().map(|option| {
                let label = if option.dirty {
                    format!("  {} *", option.label)
                } else {
                    format!("  {}", option.label)
                };
                Row::new([
                    Cell::from(label).style(setting_option_style(option)),
                    Cell::from(setting_value_line(option)),
                ])
            }))
        })
        .collect::<Vec<_>>();
    let header = Row::new(["SETTING", "VALUE"])
        .style(Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD))
        .bottom_margin(1);
    let widths = if area.width >= SETTINGS_TWO_PANE_MIN_WIDTH {
        [Constraint::Percentage(38), Constraint::Percentage(62)]
    } else {
        [Constraint::Percentage(48), Constraint::Percentage(52)]
    };
    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(2)
        .row_highlight_style(settings_highlight_style(true))
        .highlight_symbol("> ");
    let selected = expanded_setting_table_row_index(
        &row.settings,
        model.selected_setting_category_index().unwrap_or(0),
        model.selected_setting_option_index().unwrap_or(0),
    );
    let mut state = TableState::default().with_selected(selected);
    frame.render_stateful_widget(table, area, &mut state);
}

fn render_categorized_settings(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    let Some(focus) = model.settings_focus() else {
        return;
    };
    if matches!(model.mode, SelectorMode::Settings { target: SelectorTarget::GlobalSettings, .. }) {
        let render_values = |frame: &mut Frame<'_>, area: Rect| {
            let title = format!(" {} ", model.active_setting_category().map(|c| c.label).unwrap_or_default());
            let active = model.settings_focus() == Some(SettingsFocus::Options);
            let outer = settings_panel_block(title.clone(), active);
            let inner = outer.inner(area);
            frame.render_widget(outer, area);
            input_policy::refresh_title(frame, area, &title, active);
            let help_height = if area.height >= 12 { 6 } else { 3 };
            let [table, help] = Layout::vertical([Constraint::Min(3), Constraint::Length(help_height)]).areas(inner);
            render_setting_options(frame, table, model, true);
            let category = model.active_setting_category().map(|c| c.label).unwrap_or_default();
            let option = model.active_setting_option();
            let detail = match option.and_then(|o| o.global_field) {
                Some(GlobalSettingsField::ProxyNoProxy) => "NO_PROXY: comma-separated addresses that bypass the proxy, e.g. localhost,127.0.0.1,::1.",
                _ => "",
            };
            let scope = match category {
                "Defaults" => "Used for new sessions; managed creation prefills these values. Blank model follows profile.",
                "Network" => "Takes effect for new sessions / next managed start.",
                "Services" => "Changes require coordinating service and client restarts.",
                _ => "",
            };
            let help_text = option.and_then(|o| o.presentation.detail.clone()).unwrap_or_else(|| format!("{}\n{}\n{}", option.map(|o| format!("{}: {}", o.label, o.value)).unwrap_or_default(), detail, scope));
            frame.render_widget(Paragraph::new(help_text).style(Style::new().fg(crate::cli_app::session_tui_layout::muted())).wrap(Wrap { trim: false }).block(Block::new().borders(ratatui::widgets::Borders::TOP).border_style(Style::new().fg(crate::cli_app::session_tui_layout::muted()))), help);
        };
        if area.width >= 60 {
            let [categories, values] = Layout::horizontal([Constraint::Length(26), Constraint::Min(30)]).areas(area);
            render_setting_categories(frame, categories, model);
            render_values(frame, values);
        } else if focus == SettingsFocus::Categories {
            render_setting_categories(frame, area, model);
        } else { render_values(frame, area); }
        return;
    }
    if area.width >= WIDE_LAYOUT_MIN_WIDTH {
        let panes = Layout::horizontal([
            Constraint::Length(22),
            Constraint::Length(34),
            Constraint::Min(24),
        ])
        .split(area);
        render_setting_categories(frame, panes[0], model);
        render_setting_options(frame, panes[1], model, false);
        render_setting_value(frame, panes[2], model);
    } else if area.width >= SETTINGS_TWO_PANE_MIN_WIDTH {
        let panes = Layout::horizontal([Constraint::Length(22), Constraint::Min(30)]).split(area);
        render_setting_categories(frame, panes[0], model);
        if focus == SettingsFocus::Value {
            render_setting_value(frame, panes[1], model);
        } else {
            render_setting_options(frame, panes[1], model, true);
        }
    } else {
        match focus {
            SettingsFocus::Categories => render_setting_categories(frame, area, model),
            SettingsFocus::Options => render_setting_options(frame, area, model, true),
            SettingsFocus::Value => render_setting_value(frame, area, model),
        }
    }
}

fn render_setting_categories(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    let Some(row) = model.active_row() else {
        return;
    };
    let active = model.settings_focus() == Some(SettingsFocus::Categories);
    let items = row
        .settings
        .iter()
        .map(|category| ListItem::new(format!("  {}", category.label)))
        .collect::<Vec<_>>();
    let list = List::new(items)
        .block(settings_panel_block(String::new(), active).padding(ratatui::widgets::Padding::vertical(1)))
        .highlight_style(settings_highlight_style(active))
        .highlight_symbol(if active { "> " } else { "  " });
    let mut state = ListState::default().with_selected(model.selected_setting_category_index());
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_setting_options(
    frame: &mut Frame<'_>,
    area: Rect,
    model: &SelectorModel,
    show_values: bool,
) {
    let Some(category) = model.active_setting_category() else {
        return;
    };
    let active = model.settings_focus() == Some(SettingsFocus::Options);
    let global = matches!(model.mode, SelectorMode::Settings { target: SelectorTarget::GlobalSettings, .. });
    let title = if global { format!(" {} ", category.label) } else { format!(" {} options ", category.label) };
    if global {
        let mut items = Vec::new();
        let mut group = None;
        let mut selected = None;
        let key_width = (usize::from(area.width.saturating_sub(4)) * 42 / 100).max(10);
        let heading = Style::new().fg(crate::cli_app::session_tui_layout::text()).add_modifier(Modifier::BOLD);
        let [header_area, area] = Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).areas(area);
        frame.render_widget(Paragraph::new(Line::from(vec![
            Span::styled(format!("{:<width$}", "SETTING", width = key_width + 2), heading),
            Span::styled("VALUE", heading),
        ])).block(Block::default().padding(ratatui::widgets::Padding::new(3,1,1,0))), header_area);
        for (index, option) in category.options.iter().enumerate() {
            if option.presentation.group != group {
                group = option.presentation.group;
                if let Some(title) = group {
                    if !items.is_empty() { items.push(ListItem::new("")); }
                    items.push(ListItem::new(Line::styled(title, Style::new().fg(crate::cli_app::session_tui_layout::focus()).add_modifier(Modifier::BOLD))));
                }
            }
            if Some(index) == model.selected_setting_option_index() { selected = Some(items.len()); }
            let label_style = if active && Some(index) == model.selected_setting_option_index() {
                setting_option_style(option).bg(crate::cli_app::session_tui_layout::selection())
            } else { setting_option_style(option) };
            let name = option.presentation.name.as_deref().unwrap_or(option.label);
            let mut name = if option.dirty { format!("  {name} *") } else { format!("  {name}") };
            if option.presentation.group == Some("Custom status items") {
                items.push(ListItem::new(Line::styled(name, label_style)));
            } else {
                if Line::from(name.as_str()).width() > key_width {
                    while Line::from(name.as_str()).width() + 1 > key_width && !name.is_empty() { name.pop(); }
                    name.push('…');
                }
                let padding = key_width.saturating_sub(Line::from(name.as_str()).width()) + 2;
                let mut spans = vec![Span::styled(name, label_style), Span::raw(" ".repeat(padding))];
                let value = setting_value_line(option);
                spans.extend(value.spans.into_iter().map(|span| span.patch_style(value.style)));
                items.push(ListItem::new(Line::from(spans)));
            }
        }
        let mut state = ListState::default().with_selected(selected);
        frame.render_stateful_widget(List::new(items).block(Block::default().padding(ratatui::widgets::Padding::new(1,1,0,0)))
            .highlight_style(Style::default()).highlight_symbol(if active { "> " } else { "  " }), area, &mut state);
        return;
    }
    let mut state = TableState::default().with_selected(model.selected_setting_option_index());
    if show_values {
        let rows = category
            .options
            .iter()
            .map(|option| {
                let label = if option.dirty {
                    format!("{} *", option.label)
                } else {
                    option.label.to_string()
                };
                Row::new([
                    Cell::from(label).style(setting_option_style(option)),
                    Cell::from(setting_value_line(option)),
                ])
            })
            .collect::<Vec<_>>();
        let table = Table::new(
            rows,
            [Constraint::Percentage(48), Constraint::Percentage(52)],
        )
        .block(settings_panel_block(title.clone(), active))
        .column_spacing(1)
        .row_highlight_style(settings_highlight_style(active))
        .highlight_symbol(if active { "> " } else { "  " });
        frame.render_stateful_widget(table, area, &mut state);
    } else {
        let rows = category
            .options
            .iter()
            .map(|option| {
                let label = if option.dirty {
                    format!("{} *", option.label)
                } else {
                    option.label.to_string()
                };
                Row::new([Cell::from(label).style(setting_option_style(option))])
            })
            .collect::<Vec<_>>();
        let table = Table::new(rows, [Constraint::Min(12)])
            .block(settings_panel_block(title.clone(), active))
            .row_highlight_style(settings_highlight_style(active))
            .highlight_symbol(if active { "> " } else { "  " });
        frame.render_stateful_widget(table, area, &mut state);
    }
    input_policy::refresh_title(frame, area, &title, active);
}

fn render_setting_value(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    let Some(option) = model.active_setting_option() else {
        return;
    };
    let active = model.settings_focus() == Some(SettingsFocus::Value);
    let body = vec![
        Line::from(Span::styled(
            if option.dirty {
                format!("{} *", option.label)
            } else {
                option.label.to_string()
            },
            setting_option_style(option).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        setting_value_line(option),
    ];
    frame.render_widget(
        Paragraph::new(body)
            .block(settings_panel_block(" Current value ".to_string(), active))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn group_editor_inputs(value: &str) -> Vec<Input> {
    let mut inputs = value
        .split(|character: char| character == ',' || character.is_whitespace())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| Input::new(value.to_string()))
        .collect::<Vec<_>>();
    ensure_group_editor_trailing_input(&mut inputs);
    inputs
}

fn ensure_group_editor_trailing_input(inputs: &mut Vec<Input>) {
    if inputs
        .last()
        .is_none_or(|input| !input.value().trim().is_empty())
    {
        inputs.push(Input::default());
    }
}

fn group_editor_value(inputs: &[Input]) -> String {
    inputs
        .iter()
        .map(|input| input.value().trim())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_settings_overlay(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    let Some(overlay) = model.settings_overlay.as_ref() else {
        return;
    };
    match overlay {
        SettingsOverlay::Choice {
            choices,
            selected,
            custom_value,
            ..
        } => {
            let modal = centered_rect(
                46,
                choices.len() as u16 + u16::from(custom_value.is_some()) + 2,
                area,
            );
            frame.render_widget(Clear, modal);
            let mut items = Vec::with_capacity(choices.len() + usize::from(custom_value.is_some()));
            if let Some(value) = custom_value {
                items.push(ListItem::new(format!("Current: {value}")));
            }
            items.extend(
                choices
                    .iter()
                    .map(|choice| {
                        if model.active_setting_option().is_some_and(|o| o.global_field == Some(GlobalSettingsField::DefaultNotification)) {
                            let labels = notification_preview();
                            let span = match choice.value.as_deref() {
                                Some("important") => notification_span(labels.important, &labels.styles.important),
                                Some("normal") => notification_span(labels.normal, &labels.styles.normal),
                                Some("off") => notification_span(labels.off, &labels.styles.off),
                                _ => Span::raw(choice.label.clone()),
                            };
                            ListItem::new(Line::from(span))
                        } else { ListItem::new(choice.label.as_str()) }
                    }),
            );
            let title = model
                .active_setting_label()
                .map(|label| format!(" {label} "))
                .unwrap_or_else(|| " Edit setting ".to_string());
            let list = List::new(items)
                .block(settings_panel_block(title, true))
                .highlight_style(settings_highlight_style(true))
                .highlight_symbol("> ");
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(list, modal, &mut state);
        }
        SettingsOverlay::Groups {
            inputs, selected, ..
        } => {
            let visible_rows = inputs.len().clamp(1, 10) as u16;
            let modal = centered_rect(64, visible_rows + 2, area);
            frame.render_widget(Clear, modal);
            let items = inputs
                .iter()
                .map(|input| {
                    let value = input.value();
                    if value.trim().is_empty() {
                        ListItem::new(Line::from(Span::styled(
                            "<new group>",
                            Style::new().fg(Color::DarkGray),
                        )))
                    } else {
                        ListItem::new(value.to_string())
                    }
                })
                .collect::<Vec<_>>();
            let list = List::new(items)
                .block(settings_panel_block(" Message groups ".to_string(), true))
                .highlight_style(settings_highlight_style(true))
                .highlight_symbol("> ")
                .scroll_padding(1);
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(list, modal, &mut state);

            let offset = state.offset();
            let visible_index = selected.saturating_sub(offset);
            let list_height = modal.height.saturating_sub(2) as usize;
            if *selected >= offset && visible_index < list_height {
                let input_width = modal.width.saturating_sub(4) as usize;
                if input_width > 0 {
                    let input = &inputs[*selected];
                    let scroll = input.visual_scroll(input_width.saturating_sub(1).max(1));
                    let cursor = input
                        .visual_cursor()
                        .saturating_sub(scroll)
                        .min(input_width.saturating_sub(1));
                    frame.set_cursor_position((
                        modal.x + 3 + cursor as u16,
                        modal.y + 1 + visible_index as u16,
                    ));
                }
            }
        }
        SettingsOverlay::Text {
            input,
            tags,
            masked,
            ..
        } => {
            let modal = centered_rect(64, if *tags { 5 } else { 3 }, area);
            frame.render_widget(Clear, modal);
            let title = model
                .active_setting_label()
                .map(|label| {
                    if *masked {
                        format!(" Replace {label} ")
                    } else {
                        format!(" {label} ")
                    }
                })
                .unwrap_or_else(|| " Edit setting ".to_string());
            let input_width = modal.width.saturating_sub(2) as usize;
            let scroll = input.visual_scroll(input_width.saturating_sub(1).max(1));
            let body = if *tags {
                let tags = input
                    .value()
                    .split(|character: char| character == ',' || character.is_whitespace())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|value| format!("[{value}]"))
                    .collect::<Vec<_>>()
                    .join(" ");
                vec![
                    Line::from(input.value()),
                    Line::from(""),
                    Line::from(Span::styled(tags, Style::new().fg(Color::Gray))),
                ]
            } else if *masked {
                vec![Line::from("*".repeat(input.value().chars().count()))]
            } else {
                vec![Line::from(input.value())]
            };
            frame.render_widget(
                Paragraph::new(body)
                    .scroll((0, scroll as u16))
                    .block(settings_panel_block(title, true)),
                modal,
            );
            if input_width > 0 {
                let cursor = input
                    .visual_cursor()
                    .saturating_sub(scroll)
                    .min(input_width.saturating_sub(1));
                frame.set_cursor_position((modal.x + 1 + cursor as u16, modal.y + 1));
            }
        }
        SettingsOverlay::SecretAction { selected, .. } => {
            let modal = centered_rect(48, 5, area);
            frame.render_widget(Clear, modal);
            let title = model
                .active_setting_label()
                .map(|label| format!(" {label} "))
                .unwrap_or_else(|| " Edit secret ".to_string());
            let items = ["Keep stored value", "Replace", "Clear"]
                .into_iter()
                .map(ListItem::new)
                .collect::<Vec<_>>();
            let list = List::new(items)
                .block(settings_panel_block(title, true))
                .highlight_style(settings_highlight_style(true))
                .highlight_symbol("> ");
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(list, modal, &mut state);
        }
        SettingsOverlay::ConfirmDiscard { selected } => {
            let modal = centered_rect(48, 4, area);
            frame.render_widget(Clear, modal);
            let items = ["Keep editing", "Discard and leave"]
                .into_iter()
                .map(ListItem::new)
                .collect::<Vec<_>>();
            let list = List::new(items)
                .block(settings_panel_block(" Unsaved changes ".to_string(), true))
                .highlight_style(settings_highlight_style(true))
                .highlight_symbol("> ");
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(list, modal, &mut state);
        }
        SettingsOverlay::ConfirmManagement { command, selected } => {
            let modal = centered_rect(68, 9, area);
            frame.render_widget(Clear, modal);
            let block = settings_panel_block(format!(" Confirm {} ", command.label()), true);
            let inner = block.inner(modal);
            frame.render_widget(block, modal);
            let [description_area, choices_area] =
                Layout::vertical([Constraint::Min(4), Constraint::Length(2)]).areas(inner);
            let description = match command {
                SessionSettingsCommand::Adopt => vec![
                    Line::from("Manage future launches with the platform default backend."),
                    Line::from("Keeps the cutex session and cute-codex history."),
                ],
                SessionSettingsCommand::Unmanage => vec![
                    Line::from(
                        "Clears managed launch, permission defaults, visibility, and quick action.",
                    ),
                    Line::from("Keeps session/history and does not close the current runtime."),
                ],
            };
            frame.render_widget(
                Paragraph::new(description).wrap(Wrap { trim: false }),
                description_area,
            );
            let items = ["Cancel", command.label()]
                .into_iter()
                .map(ListItem::new)
                .collect::<Vec<_>>();
            let list = List::new(items)
                .highlight_style(settings_highlight_style(true))
                .highlight_symbol("> ");
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(list, choices_area, &mut state);
        }
    }
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn settings_panel_block(title: String, active: bool) -> Block<'static> {
    Block::bordered().title(Line::styled(title, input_policy::title_style(active))).border_style(if active {
        Style::new().fg(crate::cli_app::session_tui_layout::focus())
    } else {
        Style::new().fg(Color::DarkGray)
    })
}

fn settings_highlight_style(active: bool) -> Style {
    Style::new().bg(if active { crate::cli_app::session_tui_layout::selection() } else { Color::Reset })
}

fn render_runtime_action_confirmation(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    let Some(row) = model.active_row() else {
        return;
    };
    let global_default_profile = model.current_default_profile_name();
    let SelectorMode::ConfirmRuntimeAction {
        action,
        launch_profile,
        confirmed,
        ..
    } = &model.mode
    else {
        return;
    };
    let cancel_style = if *confirmed {
        Style::new().fg(Color::Gray)
    } else {
        Style::new()
            .fg(Color::White)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    };
    let action_style = if *confirmed {
        Style::new()
            .fg(Color::White)
            .bg(Color::Red)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::Yellow)
    };
    let (title, prompt, action_label, detail) = match action {
        SessionTuiAction::StockStart | SessionTuiAction::StockRestart => {
            let restart = *action == SessionTuiAction::StockRestart;
            let (generation, profile, model_name, native_home) = model
                .stock_runtime_confirmation
                .as_ref()
                .map(|request| {
                    (
                        request.review.subject.runtime_generation,
                        request.review.configuration.profile_name.as_str(),
                        request.review.configuration.model.as_str(),
                        compact_home_path(
                            request.review.contract.native_home.to_string_lossy().as_ref(),
                        ),
                    )
                })
                .unwrap_or((0, "review pending", "review pending", "review pending".into()));
            (
                if restart {
                    " Confirm restart "
                } else {
                    " Confirm start "
                },
                format!(
                    "{} {}?",
                    if restart { "Restart" } else { "Start" },
                    row.agent
                ),
                if restart {
                    "  Restart & attach  "
                } else {
                    "  Start & attach  "
                },
                format!(
                    "Generation {generation} · profile {profile} · model {model_name} · native home {native_home}. The agent will start and its terminal will open."
                ),
            )
        }
        SessionTuiAction::UpgradeRuntimePackage => {
            let (from, to, native_home) = model
                .runtime_package_confirmation
                .as_ref()
                .map(|request| {
                    (
                        request
                            .review
                            .current_runtime_version
                            .clone()
                            .unwrap_or_else(|| "unknown".into()),
                        request
                            .review
                            .target_runtime_version
                            .clone()
                            .unwrap_or_else(|| "selected".into()),
                        compact_home_path(
                            request
                                .review
                                .target_contract
                                .native_home
                                .to_string_lossy()
                                .as_ref(),
                        ),
                    )
                })
                .unwrap_or((
                    "review pending".to_string(),
                    "review pending".to_string(),
                    "review pending".to_string(),
                ));
            (
                " Confirm runtime upgrade ",
                format!("Upgrade runtime package for {}?", row.agent),
                "  Upgrade package  ",
                format!(
                    "{from} → {to} · native ID/home preserved · native home {native_home}. The agent stays offline after the package rebinding."
                ),
            )
        }
        SessionTuiAction::Online
        | SessionTuiAction::ResumeAttach
        | SessionTuiAction::OpenTui
        | SessionTuiAction::ResumeHere
        | SessionTuiAction::ResumeManaged
            if row.lifecycle == Some(CutexSessionLifecycleState::Offline) =>
        {
            (
                " Start & attach ",
                "Start & attach?".to_string(),
                "  Start & attach  ",
                format!(
                    "Start {} using the selected managed route, then enter its exact TUI.",
                    row.agent
                ),
            )
        }
        SessionTuiAction::Online => (
            " Confirm start ",
            format!("Start managed runtime for {}?", row.agent),
            "  Start runtime  ",
            "The selected managed launch route will be used.".to_string(),
        ),
        SessionTuiAction::CloseAndRestart => (
            " Confirm restart ",
            format!("Close and restart runtime for {}?", row.agent),
            "  Close and restart  ",
            format!(
                "Restart profile: {}",
                row.launch_profile_detail(
                    launch_profile.as_deref(),
                    global_default_profile.as_deref(),
                )
            ),
        ),
        SessionTuiAction::CloseRuntime => (
            " Confirm close ",
            format!("Close runtime for {}?", row.agent),
            "  Close runtime  ",
            "The durable Cutex session and cute-codex history are kept.".to_string(),
        ),
        SessionTuiAction::RepairInterruptedHistory => (
            " Confirm history repair ",
            format!("Repair interrupted history for {}?", row.agent),
            "  Repair history  ",
            "The Agent must be offline. Cutex backs up the rollout, then closes only orphaned turns left without a terminal event.".to_string(),
        ),
        SessionTuiAction::RetireSession => (
            " Confirm Archive ",
            format!("Archive Agent {}?", model.archive_confirmation.as_ref().map(|r| r.review.formal_name.as_str()).unwrap_or(&row.agent)),
            "  Archive Agent  ",
            format!(
                "ID: {}  Project: {} — retain membership/history; hide ordinary lists; require proven Offline",
                model.archive_confirmation.as_ref().map(|r| r.review.cutex_session_id.as_str()).unwrap_or("review pending"),
                model.archive_confirmation.as_ref().and_then(|r| r.review.current_project_id.as_ref()).map(|p| p.as_str()).unwrap_or("unassigned")
            ),
        ),
        SessionTuiAction::RestoreSession => (
            " Confirm restore ",
            format!("Restore {} as active and offline?", model.archive_confirmation.as_ref().map(|r| r.review.formal_name.as_str()).unwrap_or(&row.agent)),
            "  Restore session  ",
            format!("ID: {}  Project: {} — retain current membership; no runtime launch or profile selection",
                model.archive_confirmation.as_ref().map(|r| r.review.cutex_session_id.as_str()).unwrap_or("review pending"),
                model.archive_confirmation.as_ref().and_then(|r| r.review.current_project_id.as_ref()).map(|p| p.as_str()).unwrap_or("unassigned")),
        ),
        _ => return,
    };
    let body = vec![
        Line::from(""),
        Line::from(Span::styled(
            prompt,
            Style::new().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(detail, Style::new().fg(Color::Gray))),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Cancel  ", cancel_style),
            Span::raw("    "),
            Span::styled(action_label, action_style),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(body)
            .alignment(Alignment::Center)
            .block(Block::bordered().title(title)),
        area,
    );
}

fn render_runtime_close_progress(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    let SelectorMode::ClosingRuntime {
        agent_name, action, ..
    } = &model.mode
    else {
        return;
    };
    let (verb, detail, title) = match action {
        SessionTuiAction::CloseRuntime => (
            "Closing runtime",
            "Waiting for closed or offline status.",
            " Closing runtime ",
        ),
        SessionTuiAction::UpgradeRuntimePackage => (
            "Upgrading runtime package",
            "Rebinding the offline agent to the selected immutable runtime package.",
            " Upgrading runtime ",
        ),
        SessionTuiAction::RepairInterruptedHistory => (
            "Repairing history",
            "Backing up the rollout and closing orphaned turns.",
            " Repairing history ",
        ),
        SessionTuiAction::RetireSession => (
            "Retiring session",
            "Stopping and proving the runtime offline before archive commit.",
            " Retiring session ",
        ),
        SessionTuiAction::RestoreSession => (
            "Restoring session",
            "Restoring active and offline without launching a runtime.",
            " Restoring session ",
        ),
        _ => return,
    };
    let body = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("{verb} for {agent_name}..."),
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(detail, Style::new().fg(Color::Gray))),
        Line::from(Span::styled(
            "Session and history are kept.",
            Style::new().fg(Color::Gray),
        )),
    ];
    frame.render_widget(
        Paragraph::new(body)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .block(Block::bordered().title(title)),
        area,
    );
}

#[cfg(test)]
fn homepage_action_label(action: SessionTuiAction) -> &'static str {
    match action {
        SessionTuiAction::ResumeAttach | SessionTuiAction::TakeoverExisting => "takeover",
        SessionTuiAction::AttachExisting | SessionTuiAction::StockAttach => "attach",
        SessionTuiAction::StockStart => "start",
        SessionTuiAction::StockRestart => "restart",
        SessionTuiAction::OpenTui => "open",
        SessionTuiAction::Online => "start",
        SessionTuiAction::ResumeHere | SessionTuiAction::ResumeManaged => "resume",
        SessionTuiAction::RecoverRuntime
        | SessionTuiAction::CloseAndRestart
        | SessionTuiAction::CloseRuntime
        | SessionTuiAction::RepairInterruptedHistory
        | SessionTuiAction::UpgradeRuntimePackage
        | SessionTuiAction::RetireSession
        | SessionTuiAction::RestoreSession | SessionTuiAction::RevertHistory => "manage",
    }
}

fn selector_activity_details(value: Option<&SelectorActivity>) -> Option<String> {
    value.map(|v| {
        format!(
            "{}{} · {}",
            v.class.label(),
            if v.failed { " (failed)" } else { "" },
            v.updated_at
        )
    })
}

pub(super) fn format_selector_activity(
    value: Option<&SelectorActivity>,
    now: DateTime<Utc>,
) -> String {
    let Some(value) = value else {
        return "-".to_string();
    };
    let Some(timestamp) = parse_selector_activity_timestamp(&value.updated_at) else {
        return "-".to_string();
    };
    let elapsed = now.signed_duration_since(timestamp);
    let seconds = elapsed.num_seconds().max(0);
    if seconds >= 604_800 {
        return timestamp.format("%Y-%m-%d").to_string();
    }
    // Actual classes are OUT/CMD/MCP/TOOL/AGT/EDIT/IMG (max 4), plus !.
    // Existing thresholds bound numeric fields to 59; keep units in cell 8.
    let age = match seconds {
        0..=4 => "now".to_string(),
        5..=59 => format!("{seconds:>2}s"),
        60..=3_599 => format!("{:>2}m", seconds / 60),
        3_600..=86_399 => format!("{:>2}h", seconds / 3_600),
        _ => format!("{:>2}d", seconds / 86_400),
    };
    let failed = if value.failed { "!" } else { "" };
    let action = format!("{}{failed}", value.class.label());
    format!("{action:>5} {age} ")
}

#[cfg(test)]
fn selector_state_label(row: &SelectorRow) -> &'static str {
    match row.lifecycle {
        Some(CutexSessionLifecycleState::Online) if selector_row_is_detached(row) => "DET",
        Some(CutexSessionLifecycleState::Online) => "ON",
        Some(CutexSessionLifecycleState::Offline) => "OFF",
        Some(CutexSessionLifecycleState::Stale) => "STALE",
        None if row.target.is_profiles() => "accounts",
        None if row.target.is_retired_sessions() => "archive",
        None if row.target.is_cutex_projects() => "permissions",
        None if row.target.is_projects() => "workspace",
        None => "global",
    }
}

#[cfg(test)]
fn selector_row_is_detached(row: &SelectorRow) -> bool {
    row.lifecycle == Some(CutexSessionLifecycleState::Online)
        && row.backend == "alden"
        && !row.attachable
}

fn selector_default_profile_name(model: &SelectorModel) -> Option<&str> {
    model
        .global_settings_snapshot()
        .and_then(GlobalSettingsSnapshot::default_profile_name)
}

fn lifecycle_style(state: CutexSessionLifecycleState) -> Style {
    Style::new().fg(crate::cli_app::session_tui_layout::runtime_status_color(state.label()))
}

pub(super) fn footer_hints(hints: &[(&'static str, &'static str)]) -> Vec<Span<'static>> {
    let mut spans = Vec::with_capacity(hints.len() * 4);
    let key_style = Style::new().fg(crate::cli_app::session_tui_layout::focus()).add_modifier(Modifier::BOLD);
    for (key, description) in hints {
        spans.push(Span::styled(*key, key_style));
        if !description.is_empty() {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(*description, Style::new().fg(crate::cli_app::session_tui_layout::footer_description())));
        }
        spans.push(Span::raw("  "));
    }
    spans
}

fn read_only_footer_hints(hints: &[(&'static str, &'static str)]) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(
        "read-only  ",
        Style::new().fg(Color::DarkGray),
    )];
    spans.extend(footer_hints(hints));
    spans
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, model: &SelectorModel) {
    if matches!(model.mode, SelectorMode::Agents | SelectorMode::RecentSessions)
        && (model.inspector_overview_focused || model.recent_inspecting)
        && !selector_modal(model)
    {
        frame.render_widget(Paragraph::new(Line::from(footer_hints(&[
            ("↑/↓", "scroll"), ("PgUp/Dn", "page"), ("Home/End", "edge"),
            ("F5", "refresh"), ("F2", "details"), ("F1", "commands"), ("Esc", "list"),
        ]))).wrap(Wrap { trim: true }), area);
        return;
    }
    if matches!(
        model.mode,
        SelectorMode::Agents | SelectorMode::RecentSessions
    ) && !selector_modal(model) && !model.inspector_overview_focused && !model.recent_inspecting
    {
        let line = if (matches!(model.mode, SelectorMode::Agents) && model.filter_focused)
            || (matches!(model.mode, SelectorMode::RecentSessions) && model.recent.filter_focused())
        {
            Line::from("Type · ←/→ Home/End · Backspace/Delete · Ctrl+U clear · Enter/Esc/Tab finish · F1 commands")
        } else if area.width < 120 {
            Line::from(footer_hints(&[
                ("↑/↓", "select"), ("Enter", "open"), ("Alt+I", "inspect"),
                ("Alt+M", "agent"), ("Alt+N", "session"), ("←/→", "panels"),
                ("/", "filter"), ("F5", "refresh"), ("Alt+S", "sort"), ("F1", "help"), ("Esc", "back"),
            ]))
        } else {
            Line::from(footer_hints(&[
                ("↑/↓", "select"), ("Enter", "open"), ("Alt+I", "inspect"), ("Alt+A", "actions"), ("Alt+E", "edit"),
                ("Alt+M", "new agent"), ("Alt+N", "new session"),
                ("←/→", "panels"), ("/", "filter"), ("Alt+S", "sort"),
                ("F5", "refresh"), ("F2", "details"), ("F1", "commands"), ("Esc", "back"),
            ]))
        };
        frame.render_widget(Paragraph::new(line).wrap(Wrap { trim: true }), area);
        return;
    }
    if matches!(&model.mode, SelectorMode::Settings { target: SelectorTarget::GlobalSettings, .. })
        && model.settings_overlay.is_none() && !selector_modal(model)
    {
        let categories = matches!(model.mode, SelectorMode::Settings {
            focus: SettingsFocus::Categories, view: SettingsView::Categories, ..
        });
        let mut hints = vec![("↑/↓", "select"),
            ("←/→", if categories { "panels" } else { "value" }),
            ("Enter", if categories { "open" } else { "edit" }), ("Tab", "focus")];
        if model.settings_are_editable() { hints.extend([("S", "save"), ("D", "discard")]); }
        if area.width >= 66 { hints.push(("Alt+1–6", "panels")); }
        hints.extend([("Esc", "back"), ("Ctrl+C", "exit")]);
        frame.render_widget(Paragraph::new(Line::from(footer_hints(&hints))).wrap(Wrap { trim: true }), area);
        return;
    }
    let narrow = area.width < WIDE_LAYOUT_MIN_WIDTH;
    let very_narrow = area.width < 66;
    let mut spans = if model.action_overlay.is_some() {
        if very_narrow {
            footer_hints(&[("Up/Down", ""), ("Enter", ""), ("Esc", "")])
        } else {
            footer_hints(&[("Up/Down", "choose"), ("Enter", "stage"), ("Esc", "cancel")])
        }
    } else if let Some(overlay) = model.profile_overlay.as_ref() {
        match overlay {
            ProfileOverlay::Actions { .. } if very_narrow => {
                footer_hints(&[("Up/Down", ""), ("Enter", ""), ("Esc", "")])
            }
            ProfileOverlay::Actions { .. } => footer_hints(&[
                ("Up/Down", "choose"),
                ("Enter", "select"),
                ("Esc", "cancel"),
            ]),
            ProfileOverlay::RenameInput { .. } if very_narrow => footer_hints(&[
                ("L/R", ""),
                ("Bksp/Del", ""),
                ("Ctrl+U", ""),
                ("Enter/Esc", ""),
            ]),
            ProfileOverlay::RenameInput { .. } if narrow => footer_hints(&[
                ("Left/Right", ""),
                ("Bksp/Del", "edit"),
                ("Ctrl+U", "clear"),
                ("Enter", "review"),
                ("Esc", ""),
            ]),
            ProfileOverlay::RenameInput { .. } => footer_hints(&[
                ("Left/Right", "move"),
                ("Bksp/Del", "edit"),
                ("Ctrl+U", "clear"),
                ("Enter", "review"),
                ("Esc", "cancel"),
            ]),
            ProfileOverlay::ConfirmRename { .. }
            | ProfileOverlay::ConfirmRemove { .. }
            | ProfileOverlay::ConfirmAddProfile { .. }
            | ProfileOverlay::ConfirmDiscardProfile { .. }
                if very_narrow =>
            {
                footer_hints(&[("Up/Down", ""), ("Enter", ""), ("Esc", "")])
            }
            ProfileOverlay::ConfirmRename { .. }
            | ProfileOverlay::ConfirmRemove { .. }
            | ProfileOverlay::ConfirmAddProfile { .. }
            | ProfileOverlay::ConfirmDiscardProfile { .. } => footer_hints(&[
                ("Up/Down", "choose"),
                ("Enter", "select"),
                ("Esc", "cancel"),
            ]),
        }
    } else if let Some(overlay) = model.settings_overlay.as_ref() {
        match overlay {
            SettingsOverlay::Choice { .. } if very_narrow => {
                footer_hints(&[("Up/Down", ""), ("Enter", ""), ("Esc", "")])
            }
            SettingsOverlay::SecretAction { .. } if very_narrow => {
                footer_hints(&[("Up/Down", ""), ("Enter", ""), ("Esc", "")])
            }
            SettingsOverlay::Choice { .. } => {
                footer_hints(&[("Up/Down", "choose"), ("Enter", "stage"), ("Esc", "cancel")])
            }
            SettingsOverlay::SecretAction { .. } => footer_hints(&[
                ("Up/Down", "choose"),
                ("Enter", "select"),
                ("Esc", "cancel"),
            ]),
            SettingsOverlay::Groups { .. } | SettingsOverlay::Text { .. } if very_narrow => {
                footer_hints(&[
                    ("Up/Down", ""),
                    ("L/R", ""),
                    ("Bksp/Del", ""),
                    ("Enter", ""),
                    ("Esc", ""),
                ])
            }
            SettingsOverlay::Groups { .. } | SettingsOverlay::Text { .. } if narrow => {
                footer_hints(&[
                    ("Up/Down", "line"),
                    ("Left/Right", "edit"),
                    ("Bksp/Del", "edit"),
                    ("Ctrl+U", "clear"),
                    ("Enter", "stage"),
                    ("Esc", ""),
                ])
            }
            SettingsOverlay::Groups { .. } | SettingsOverlay::Text { .. } => footer_hints(&[
                ("Up/Down", "line"),
                ("Left/Right", "move"),
                ("Bksp/Del", "edit"),
                ("Ctrl+U", "clear"),
                ("Enter", "stage"),
                ("Esc", "cancel"),
            ]),
            SettingsOverlay::ConfirmDiscard { .. } | SettingsOverlay::ConfirmManagement { .. }
                if very_narrow =>
            {
                footer_hints(&[("Up/Down", ""), ("Enter", ""), ("Esc", "")])
            }
            SettingsOverlay::ConfirmDiscard { .. } | SettingsOverlay::ConfirmManagement { .. } => {
                footer_hints(&[
                    ("Up/Down", "choose"),
                    ("Enter", "select"),
                    ("Esc", "cancel"),
                ])
            }
        }
    } else {
        match &model.mode {
            SelectorMode::Agents if model.inspector_overview_focused && very_narrow => {
                footer_hints(&[("Alt+A", ""), ("Alt+E", ""), ("Shift+Tab/Esc", "")])
            }
            SelectorMode::Agents if model.inspector_overview_focused => footer_hints(&[
                ("Alt+A", "Actions"),
                ("Alt+E", "Settings"),
                ("Shift+Tab/Esc", "Agent list"),
                ("F5", "refresh"),
            ]),
            SelectorMode::Agents if very_narrow => footer_hints(&[
                ("Up/Down", ""),
                ("Enter", ""),
                ("L/R", ""),
                ("Alt+A", ""),
                ("Alt+E", ""),
                ("Alt+V", ""),
                ("Tab", ""),
                ("Esc", ""),
            ]),
            SelectorMode::Agents if narrow => footer_hints(&[
                ("Up/Down", ""),
                ("Enter", ""),
                ("L/R", ""),
                ("Alt+A", ""),
                ("Alt+E", ""),
                ("Alt+V", ""),
                ("Tab", ""),
                ("Ctrl+X", ""),
                ("Esc", ""),
            ]),
            SelectorMode::Agents => {
                let mut spans = footer_hints(&[
                    ("Up/Down", "move"),
                    ("Enter", "attach/enter"),
                    ("L/R", "tabs"),
                    ("Alt+A", "actions"),
                    ("Alt+E", "settings"),
                    ("Alt+V", "titles"),
                    ("Tab", "Inspector"),
                    ("F5", "refresh"),
                ]);
                if area.width >= 120 {
                    spans.extend(footer_hints(&[("~", "global profile")]));
                }
                if model.enhanced_keyboard && area.width >= 180 {
                    spans.extend(footer_hints(&[("Shift+Enter", "actions")]));
                }
                if area.width >= 180 {
                    spans.extend(footer_hints(&[("Ctrl+X", "review close")]));
                }
                spans.extend(footer_hints(&[("Esc", "clear/exit")]));
                spans
            }
            SelectorMode::RecentSessions if model.recent.review().is_some() => footer_hints(&[
                ("Up/Down", "choose"),
                ("Enter", "confirm"),
                ("Esc", "cancel"),
            ]),
            SelectorMode::RecentSessions if model.recent.filter_focused() => footer_hints(&[
                ("Type", "filter loaded rows"),
                ("Enter/Esc", "finish"),
                ("Ctrl+U", "clear"),
            ]),
            SelectorMode::RecentSessions
                if matches!(model.recent.load_state(), RecentLoadState::Failed(_)) =>
            {
                footer_hints(&[("Enter/F5", "retry"), ("Tab", "focus"), ("Esc", "back")])
            }
            SelectorMode::RecentSessions => footer_hints(&[
                ("Up/Down", "move"),
                ("Enter", "native resume"),
                ("Alt+A", "actions"),
                ("Alt+L", "load more"),
                ("Alt+Z", "archive"),
                ("Tab", "review"),
                ("Left/Right", "tabs"),
                ("F5", "refresh"),
                ("Esc", "back"),
            ]),
            SelectorMode::RetiredSessions { .. } if very_narrow => {
                footer_hints(&[("Up/Down", ""), ("Enter", ""), ("Left/Esc", "")])
            }
            SelectorMode::RetiredSessions { .. } => footer_hints(&[
                ("Up/Down", "move"),
                ("Enter", "restore"),
                ("Alt+I", "details"),
                ("Alt+B", "inspector"),
                ("Esc", "back"),
            ]),
            SelectorMode::Actions { .. } if very_narrow => {
                footer_hints(&[("Up/Down", ""), ("Enter", ""), ("Left/Esc", "")])
            }
            SelectorMode::Actions { .. } if model.shows_managed_inspector() => footer_hints(&[
                ("Up/Down", "move"),
                ("Enter", "selected action"),
                ("Alt+E", "Settings"),
                ("Shift+Tab/Esc", "Agent list"),
            ]),
            SelectorMode::Actions { .. } => footer_hints(&[
                ("Up/Down", "move"),
                ("Enter", "select"),
                ("Left/Tab/Esc", "back"),
            ]),
            SelectorMode::Settings { target: SelectorTarget::GlobalSettings, .. } if very_narrow => footer_hints(&[
                ("Tab", "focus"), ("Enter", "edit"), ("S", "save"), ("Ctrl+C", "exit"),
            ]),
            SelectorMode::Settings { target: SelectorTarget::GlobalSettings, .. } => footer_hints(&[
                ("←/→", "value"), ("Enter", "edit"), ("Tab", "focus"),
                ("S", "save"), ("D", "discard"), ("Ctrl+C", "exit"),
            ]),
            SelectorMode::Settings { target, .. }
                if target.agent_key().is_some() && very_narrow =>
            {
                footer_hints(&[
                    ("Up/Down", ""),
                    ("Enter", ""),
                    ("Alt+A", ""),
                    ("V", ""),
                    ("S", ""),
                    ("D", ""),
                    ("Shift+Tab/Esc", ""),
                ])
            }
            SelectorMode::Settings { target, .. } if target.agent_key().is_some() => {
                footer_hints(&[
                    ("Up/Down", "move"),
                    ("Enter", "edit"),
                    ("V", "view"),
                    ("Alt+A", "Actions"),
                    ("S", "save"),
                    ("D", "discard"),
                    ("Shift+Tab/Esc", "Agent list"),
                ])
            }
            SelectorMode::Settings {
                view: SettingsView::Expanded,
                ..
            } if model.settings_are_editable() && very_narrow => footer_hints(&[
                ("Up/Down", ""),
                ("Enter", ""),
                ("S", ""),
                ("D", ""),
                ("Tab/Esc", ""),
            ]),
            SelectorMode::Settings {
                view: SettingsView::Expanded,
                ..
            } if model.settings_are_editable() && narrow => footer_hints(&[
                ("Up/Down", ""),
                ("Enter", "edit"),
                ("V", "view"),
                ("S", "save"),
                ("D", "discard"),
                ("Tab/Esc", ""),
            ]),
            SelectorMode::Settings {
                view: SettingsView::Expanded,
                ..
            } if model.settings_are_editable() => footer_hints(&[
                ("Up/Down", "move"),
                ("Enter", "edit"),
                ("V", "view"),
                ("S", "save"),
                ("D", "discard"),
                ("Left/Tab/Esc", "list"),
            ]),
            SelectorMode::Settings {
                view: SettingsView::Expanded,
                ..
            } if very_narrow => footer_hints(&[("Up/Down", ""), ("V", ""), ("Left/Tab/Esc", "")]),
            SelectorMode::Settings {
                view: SettingsView::Expanded,
                ..
            } if narrow => {
                read_only_footer_hints(&[("Up/Down", ""), ("V", "view"), ("Left/Tab/Esc", "")])
            }
            SelectorMode::Settings {
                view: SettingsView::Expanded,
                ..
            } => read_only_footer_hints(&[
                ("Up/Down", "move"),
                ("V", "switch view"),
                ("Left/Tab/Esc", "list"),
            ]),
            SelectorMode::Settings { .. } if model.settings_are_editable() && very_narrow => {
                footer_hints(&[
                    ("Up/Down", ""),
                    ("Enter", ""),
                    ("Left", ""),
                    ("S", ""),
                    ("D", ""),
                    ("Tab/Esc", ""),
                ])
            }
            SelectorMode::Settings { .. } if model.settings_are_editable() && narrow => {
                footer_hints(&[
                    ("Up/Down", ""),
                    ("Right/Enter", ""),
                    ("Left", ""),
                    ("V", ""),
                    ("S", ""),
                    ("D", ""),
                    ("Tab/Esc", ""),
                ])
            }
            SelectorMode::Settings { .. } if model.settings_are_editable() => footer_hints(&[
                ("Up/Down", ""),
                ("Right/Enter", "open"),
                ("Left", "back"),
                ("V", "view"),
                ("S", "save"),
                ("D", "discard"),
                ("Tab/Esc", "list"),
            ]),
            SelectorMode::Settings { .. } if very_narrow => footer_hints(&[
                ("Up/Down", ""),
                ("Enter", ""),
                ("Left", ""),
                ("V", ""),
                ("Tab/Esc", ""),
            ]),
            SelectorMode::Settings { .. } if narrow => read_only_footer_hints(&[
                ("Up/Down", ""),
                ("Right/Enter", ""),
                ("Left", ""),
                ("V", "view"),
                ("Tab/Esc", ""),
            ]),
            SelectorMode::Settings { .. } => read_only_footer_hints(&[
                ("Up/Down", "move"),
                ("Right/Enter", "open"),
                ("Left", "previous"),
                ("V", "view"),
                ("Tab/Esc", "list"),
            ]),
            SelectorMode::ProfileManager {
                focus: ProfileWorkspaceFocus::Items,
                ..
            } if very_narrow => {
                footer_hints(&[("Up/Down", ""), ("Enter/Right", ""), ("Tab/Esc", "")])
            }
            SelectorMode::ProfileManager {
                focus: ProfileWorkspaceFocus::Items,
                ..
            } => footer_hints(&[
                ("Up/Down", "move"),
                ("Home/End", "edge"),
                ("Enter", "open"),
                ("Left/Right", "tabs"),
                ("Tab/Esc", "agents"),
            ]),
            SelectorMode::ProfileManager {
                focus: ProfileWorkspaceFocus::Editor,
                selected: 0,
                ..
            } if very_narrow => footer_hints(&[
                ("Up/Down", ""),
                ("Enter", ""),
                ("Left", ""),
                ("S", ""),
                ("D", ""),
                ("Tab/Esc", ""),
            ]),
            SelectorMode::ProfileManager {
                focus: ProfileWorkspaceFocus::Editor,
                selected: 0,
                ..
            } if narrow => footer_hints(&[
                ("Up/Down", ""),
                ("Enter", ""),
                ("S", ""),
                ("D", ""),
                ("Left/Tab", ""),
            ]),
            SelectorMode::ProfileManager {
                focus: ProfileWorkspaceFocus::Editor,
                selected: 0,
                ..
            } => footer_hints(&[
                ("Up/Down", "move"),
                ("Enter", "edit"),
                ("Left", "profiles"),
                ("S", "save"),
                ("D", "discard"),
                ("Tab/Esc", "agents"),
            ]),
            SelectorMode::ProfileManager {
                focus: ProfileWorkspaceFocus::Editor,
                ..
            } if very_narrow => footer_hints(&[
                ("Up/Down", ""),
                ("Enter", ""),
                ("a", ""),
                ("S", ""),
                ("D", ""),
                ("Left/Tab", ""),
            ]),
            SelectorMode::ProfileManager {
                focus: ProfileWorkspaceFocus::Editor,
                ..
            } if narrow => footer_hints(&[
                ("Up/Down", ""),
                ("Enter", ""),
                ("a", ""),
                ("S", ""),
                ("D", ""),
                ("Left/Tab", ""),
            ]),
            SelectorMode::ProfileManager {
                focus: ProfileWorkspaceFocus::Editor,
                ..
            } => footer_hints(&[
                ("Up/Down", "move"),
                ("Enter", "edit"),
                ("a", "actions"),
                ("S", "save"),
                ("D", "discard"),
                ("Left", "profiles"),
                ("Tab/Esc", "agents"),
            ]),
            SelectorMode::ConfirmRuntimeAction { .. } if very_narrow => {
                footer_hints(&[("Left/Right", ""), ("Enter", ""), ("Esc", "")])
            }
            SelectorMode::ConfirmRuntimeAction { .. } => footer_hints(&[
                ("Left/Right", "choose"),
                ("Enter", "confirm"),
                ("Esc", "back"),
            ]),
            SelectorMode::ClosingRuntime { .. } => vec![Span::styled(
                "Close in progress",
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )],
        }
    };
    let exit_spans = if matches!(&model.mode, SelectorMode::ClosingRuntime { .. }) {
        Vec::new()
    } else {
        footer_hints(&[("Ctrl+C", "exit")])
    };
    // Status has its own shell slot; failures never replace contextual help.
    let mut help = exit_spans;
    help.extend(footer_hints(&[("F1", "commands")]));
    help.append(&mut spans);
    help.extend(footer_hints(&[("F2", "details")]));
    frame.render_widget(Paragraph::new(Line::from(help)).wrap(Wrap { trim: true }), area);
}

#[cfg(test)]
mod tests {
    #[test]
    fn owned_session_close_is_confirmed_and_never_routes_to_archive() {
        let native = "018e0000-0000-7000-8000-0000c0004901";
        let owner = format!("cutex.{native}");
        let mut record = CutexSessionRecord::new(owner.clone(), Some(native.into()), "local".into(), "/workspace".into(), None).unwrap();
        record.agent_enabled = false;
        record.registration_class = cutex::agent_bus::model::AgentRegistrationClass::LocalOnly;
        record.explicit_launch = Some(cutex::agent_management::ExplicitLaunchContract {
            version:4, migration_action_id:None, native_id:native.into(), native_home:"/native".into(),
            bundle_manifest:"/bundle.json".into(), bundle_sha256:cutex::role_revision::Sha256::new("a".repeat(64)).unwrap(),
        });
        let mut store = cutex::session::model::CutexSessionStore::default();
        store.sessions.insert(owner.clone(),record);
        let mut model = SelectorModel::new(vec![recent_sessions_row()], false, false);
        model.recent.receive(super::super::session_tui_recent::CatalogReply::Page { cursor:None,
            result:Ok(cutex::catalog::ThreadPage { data:Vec::new(), next_cursor:None, backwards_cursor:None })}, &store);
        model.native_actions = Some((native.into(),1,false));
        let enter = KeyEvent::new(KeyCode::Enter,KeyModifiers::NONE);
        route_selector_key(&mut model, enter);
        assert!(model.native_actions.as_ref().unwrap().2);
        assert!(matches!(route_selector_key(&mut model, enter),
            SelectorKeyRoute::Control(Some(SelectorControl::CloseOwnedSession { native: id, owner: key })) if id == native && key == owner));
    }

    #[test]
    fn native_actions_revert_opens_history_without_archiving_or_adopting() {
        let mut model = SelectorModel::new(vec![recent_sessions_row()], false, false);
        model.native_actions = Some(("thread-one".into(), 0, false));
        let press = |code| KeyEvent::new(code, KeyModifiers::NONE);
        route_selector_key(&mut model, press(KeyCode::Down));
        route_selector_key(&mut model, press(KeyCode::Down));
        assert_eq!(model.native_actions.as_ref().unwrap().1, 2);
        assert!(matches!(route_selector_key(&mut model, press(KeyCode::Enter)),
            SelectorKeyRoute::Control(Some(SelectorControl::RevertHistory(id))) if id=="thread-one"));
        assert!(model.native_actions.is_none());
    }

    #[test]
    fn pro_review_profile_values_keep_semantic_colors_when_focus_changes() {
        let mut model = SelectorModel::new(vec![global_row()], false, false);
        model.mode = SelectorMode::ProfileManager {
            profiles: vec![profile_catalog_entry("review-profile", true)], selected: 1,
            focus: ProfileWorkspaceFocus::Editor, editor_selected: 2,
        };
        for focus in [ProfileWorkspaceFocus::Editor, ProfileWorkspaceFocus::Items] {
            if let SelectorMode::ProfileManager { focus: current, .. } = &mut model.mode { *current = focus; }
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 36)).unwrap();
            terminal.draw(|frame| render_profile_details(frame, frame.area(), &model)).unwrap();
            let buffer = terminal.backend().buffer();
            for (label, value, expected) in [("Selected account", "yes", crate::cli_app::session_tui_layout::muted()), ("Name", "review-profile", Color::White)] {
                let y = (0..36).find(|y| (0..160).map(|x| buffer[(x,*y)].symbol()).collect::<String>().contains(label)).unwrap();
                let line = (0..160).map(|x| buffer[(x,y)].symbol()).collect::<String>();
                let x = line.find(value).unwrap() as u16;
                let value_cell = &buffer[(x,y)];
                assert_eq!(value_cell.fg, expected, "{label}, {focus:?}");
                assert_ne!(value_cell.fg, value_cell.bg);
            }
        }
    }

    #[test]
    fn pro_review_agent_discard_restores_visible_snapshot() {
        let record = editable_record();
        let mut model = editable_model(&record);
        stage_full_access(&mut model);
        assert!(model.settings_draft.is_dirty());
        selector_command(&mut model, Command::Page(PrimaryPanel::Recent));
        contract_key(&mut model, KeyCode::Right);
        route_selector_key(&mut model, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let row = model.rows.iter().find(|r| r.target.agent_key() == Some(EDITABLE_AGENT_KEY)).unwrap();
        assert!(row.settings.iter().flat_map(|c| &c.options).all(|o| !o.dirty));
        assert!(!model.settings_draft.is_dirty());
    }

    #[test]
    fn pro_review_discard_navigation_rebuilds_settings_projection() {
        let mut model = SelectorModel::new(vec![global_row()], false, false);
        selector_command(&mut model, Command::Settings);
        select_global_setting(&mut model, GlobalSettingsField::DefaultNotification);
        let original = model.active_setting_option().unwrap().value.clone();
        model.handle(SelectorEvent::Activate);
        contract_key(&mut model, KeyCode::Home);
        contract_key(&mut model, KeyCode::Enter);
        assert!(model.global_settings_draft.is_dirty());
        selector_command(&mut model, Command::Page(PrimaryPanel::Agents));
        assert!(model.leave_review.is_some());
        contract_key(&mut model, KeyCode::Right);
        route_selector_key(&mut model, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        selector_command(&mut model, Command::Settings);
        select_global_setting(&mut model, GlobalSettingsField::DefaultNotification);
        let option = model.active_setting_option().unwrap();
        assert_eq!(option.value, original);
        assert!(!option.dirty);
        assert!(!model.global_settings_draft.is_dirty());
    }

    #[test]
    fn pro_review_recent_inspector_adoption_consumes_confirmation_keys() {
        let mut model = contract_recent_model();
        selector_command(&mut model, Command::Inspect);
        assert!(model.recent_inspecting);
        selector_command(&mut model, Command::Actions);
        contract_key(&mut model, KeyCode::Enter);
        assert!(!model.recent_inspecting);
        assert!(model.recent.review().is_some());
        for c in "review-worker".chars() { contract_key(&mut model, KeyCode::Char(c)); }
        contract_key(&mut model, KeyCode::Tab);
        contract_key(&mut model, KeyCode::Right);
        assert!(matches!(route_selector_key(&mut model, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), SelectorKeyRoute::Control(Some(SelectorControl::AdoptRecent(_)))));
    }

    #[test]
    fn pro_review_archive_inspect_refresh_origin_and_restore_snapshot() {
        let record = editable_record();
        let a = retired_selector_row("archive-a", &record, None);
        let mut b = a.clone(); b.target = SelectorTarget::RetiredAgent("archive-b".into()); b.agent = "B".into();
        let mut model = SelectorModel::new(vec![], false, false);
        model.activate_primary_panel(PrimaryPanel::Recent);
        model.open_retired_sessions(vec![a.clone(), b.clone()]);
        model.mode = SelectorMode::RetiredSessions { selected: 1 };
        selector_command(&mut model, Command::Inspect);
        assert!(model.details.as_deref().unwrap().starts_with("B"));
        contract_key(&mut model, KeyCode::Esc);
        assert!(matches!(route_selector_key(&mut model, KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE)), SelectorKeyRoute::Control(Some(SelectorControl::OpenRetiredSessions))));
        model.open_retired_sessions(vec![b.clone()]);
        assert_eq!(model.archive_return_panel, PrimaryPanel::Recent);
        assert_eq!(model.active_row().unwrap().agent, "B");
        model.handle(SelectorEvent::Activate);
        assert!(matches!(model.mode, SelectorMode::ConfirmRuntimeAction { action: SessionTuiAction::RestoreSession, .. }));
        model.replace_snapshot(SelectorSnapshot { rows: vec![], warning: None });
        assert!(matches!(model.mode, SelectorMode::ConfirmRuntimeAction { action: SessionTuiAction::RestoreSession, .. }));
        b.revision += 1;
        model.replace_retired_rows(vec![b]);
        assert!(matches!(model.mode, SelectorMode::RetiredSessions { .. }));
        contract_key(&mut model, KeyCode::Esc);
        assert!(matches!(model.mode, SelectorMode::RecentSessions));
    }

    #[test]
    fn pro_review_recent_visibility_and_observation_details() {
        let mut model = contract_recent_model();
        let mut view = model.recent.rows()[0].view.clone();
        view.runtime = Observation::Unavailable("unique-runtime-diagnostic".into());
        view.native_thread = Some("native-one".into());
        model.recent.enrich_views(&[view]);
        model.inspector_visible = true;
        assert!(rendered_text_at(160, 36, &model).contains("unique-runtime-diagnostic"));
        selector_command(&mut model, Command::Appearance);
        assert!(!rendered_text_at(160, 36, &model).contains("unique-runtime-diagnostic"));
        selector_command(&mut model, Command::Inspect);
        assert!(rendered_text_at(80, 24, &model).contains("unique-runtime-diagnostic"));
    }

    #[test]
    fn new_session_shortcut_does_not_create_a_managed_agent() {
        assert_eq!(input_policy::resolve(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::ALT)), Some(Command::NewProject));
        assert_eq!(input_policy::resolve(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::ALT)), Some(Command::NewManagedAgent));
    }

    #[test]
    fn new_agent_shortcut_respects_runtime_availability() {
        let mut model = SelectorModel::new(Vec::new(), false, false);
        let available = cutex::launch::local_deployment::LocalDeployment::selected().ok().flatten().is_some();
        let route = selector_command(&mut model, Command::NewManagedAgent);
        if available {
            assert!(matches!(route, SelectorKeyRoute::Control(Some(SelectorControl::NewAgent))));
        } else {
            assert!(matches!(route, SelectorKeyRoute::Control(None)));
            assert_eq!(model.notice.as_deref(), Some("Install a local runtime to create an Agent"));
        }
    }

    #[test]
    fn ui_contract_e1_focus_scroll_status_modal_and_editor_return() {
        let mut model = SelectorModel::new(
            vec![row(
                "exact-id",
                "editable",
                CutexSessionLifecycleState::Offline,
                false,
                true,
            )],
            false,
            false,
        );
        let query = model.rows[0].agent.clone();
        model.query = Input::new(query.clone());
        let selected = model.selected_target();
        let offset = model.managed_table.borrow().offset();
        selector_command(&mut model, Command::Inspect);
        assert!(model.inspector_overview_focused);
        rendered_text_at(60, 18, &model);
        for key in [KeyCode::PageDown, KeyCode::End, KeyCode::Up, KeyCode::Home] {
            route_selector_key(&mut model, KeyEvent::new(key, KeyModifiers::NONE));
            assert_eq!(model.selected_target(), selected);
        }
        route_selector_key(&mut model, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(model.managed_table.borrow().offset(), offset);
        assert_eq!(model.query.value(), query);
        model.filter_focused = true;
        route_selector_key(&mut model, KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(model.query.cursor(), 0);
        model.warning = Some(format!("{}FINAL-ERROR", "long error ".repeat(200)));
        route_selector_key(&mut model, KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
        handle_selector_paste(&mut model, "MUST NOT EDIT");
        assert_eq!(model.query.value(), query);
        rendered_text_at(60, 18, &model);
        route_selector_key(&mut model, KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert!(rendered_text_at(60, 18, &model).contains("FINAL-ERROR"));
        route_selector_key(
            &mut model,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(model.details.is_some());
        route_selector_key(&mut model, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(model.filter_focused);
        assert_eq!(model.query.cursor(), 0);
        let mut recent = contract_recent_model();
        let selected = recent.recent.selected_visible();
        selector_command(&mut recent, Command::Inspect);
        rendered_text_at(60, 18, &recent);
        for key in [KeyCode::End, KeyCode::PageUp, KeyCode::Home] {
            route_selector_key(&mut recent, KeyEvent::new(key, KeyModifiers::NONE));
            assert!(recent.recent_inspecting);
            assert_eq!(recent.recent.selected_visible(), selected);
        }
        route_selector_key(&mut recent, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!recent.recent_inspecting);
        route_selector_key(
            &mut recent,
            KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE),
        );
        route_selector_key(
            &mut recent,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(
            recent.details.is_some(),
            "F1 actionable fallback uses the same Details binding"
        );
    }
    #[test]
    #[ignore = "scripts/tui-e1-pty.py owns the real private PTY; no services"]
    fn ui_contract_e1_terminal_resize_detail_child() {
        use std::io::Write;
        let inspect = std::env::var_os("CUTEX_UI_PTY_INSPECTOR").is_some();
        let mut record = editable_record();
        record.registration_class = AgentRegistrationClass::Persistent;
        record.managed_cwd = Some(format!("/private/{}TAIL", "中文/e\u{301}/👩‍💻/".repeat(80)));
        let mut model = editable_model(&record);
        model.warning = Some(format!("{}FINAL-ERROR", "long error ".repeat(200)));
        let selected = model.selected_target();
        let mut shell = TerminalShell::open().unwrap();
        let mut events = ShellEvents;
        let mut resized = false;
        let mut opened = false;
        println!("E1_READY");
        io::stdout().flush().unwrap();
        loop {
            shell
                .terminal()
                .draw(|f| render_selector(f, &model))
                .unwrap();
            match events.next().unwrap() {
                Some(Event::Resize(_, _)) => {
                    resized = true;
                    println!("E1_RESIZED");
                    io::stdout().flush().unwrap();
                }
                Some(Event::Key(key)) => {
                    route_selector_key(&mut model, key);
                    if key.code == KeyCode::F(2)
                        || (inspect
                            && key.code == KeyCode::Char('i')
                            && key.modifiers.contains(KeyModifiers::ALT))
                    {
                        opened = if inspect {
                            model.inspector_overview_focused
                        } else {
                            model.details.is_some()
                        };
                        println!("E1_DETAILS");
                        io::stdout().flush().unwrap();
                    }
                    if key.code == KeyCode::End {
                        println!("E1_SCROLLED");
                        io::stdout().flush().unwrap();
                    }
                    if key.code == KeyCode::Esc {
                        break;
                    }
                }
                _ => {}
            }
        }
        assert!(opened && resized);
        assert!(model.details.is_none());
        assert!(!model.inspector_overview_focused);
        assert_eq!(model.selected_target(), selected);
        drop(shell);
        println!("E1_COOKED");
    }
    #[test]
    fn ui_contract_b2_return_context_and_independent_settings() {
        let rows = (0..70)
            .map(|i| {
                row(
                    &format!("id-{i:02}"),
                    &format!("agent-{i:02}"),
                    CutexSessionLifecycleState::Online,
                    false,
                    true,
                )
            })
            .collect();
        let mut model = SelectorModel::new(rows, false, false);
        model.query = Input::new("agent".into());
        model.handle(SelectorEvent::Last);
        rendered_text_at(160, 24, &model);
        let selected = model.selected_target();
        let offset = model.managed_table.borrow().offset();
        assert!(offset > 0);
        selector_command(&mut model, Command::Inspect);
        let detail = rendered_text_at(60, 18, &model);
        assert!(detail.contains("Agent Details"));
        route_selector_key(&mut model, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(model.selected_target(), selected);
        assert_eq!(model.query.value(), "agent");
        assert_eq!(model.managed_table.borrow().offset(), offset);
        assert!(!model.inspector_overview_focused);
        model.warning = Some("fixture failure".into());
        for (width, height) in [
            (60, 18),
            (80, 24),
            (100, 30),
            (120, 36),
            (160, 48),
            (240, 50),
        ] {
            let rendered = rendered_text_at(width, height, &model);
            assert!(
                rendered.contains("CUTEX")
                    && rendered.contains("fixture failure")
                    && rendered.contains("F1")
            );
        }
        let config = CodezConfig {
            default_profile: Some("first".into()),
            ..Default::default()
        };
        let mut settings = SelectorModel::new(
            vec![global_settings_row(&config), profiles_row(&config, &[])],
            false,
            false,
        );
        assert!(settings.rows.is_empty());
        assert_eq!(selector_default_profile_name(&settings), Some("first"));
        settings.context.settings.clear(); // global projection is not carried by a fake row
        assert_eq!(selector_default_profile_name(&settings), Some("first"));
        let updated = CodezConfig {
            default_profile: Some("second".into()),
            ..Default::default()
        };
        settings.global_settings_apply_succeeded(&updated, &[], 1);
        assert_eq!(selector_default_profile_name(&settings), Some("second"));
    }
    #[test]
    fn ui_contract_b2_lazy_catalog_and_stable_geometry() {
        let mut catalog = None;
        let mut starts = 0;
        for panel in [PrimaryPanel::Agents]
            .into_iter()
            .chain((0..20).flat_map(|_| {
                [
                    PrimaryPanel::Recent,
                    PrimaryPanel::Projects,
                    PrimaryPanel::Agents,
                ]
            }))
        {
            ensure_recent(panel, &mut catalog, || {
                starts += 1;
                Ok(vec!["cached-page"])
            })
            .unwrap();
            if panel == PrimaryPanel::Agents && catalog.is_none() {
                assert_eq!(starts, 0);
            }
        }
        assert_eq!(starts, 1);
        assert_eq!(catalog.unwrap(), vec!["cached-page"]);
        let mut model = contract_recent_model();
        model.activate_primary_panel(PrimaryPanel::Agents);
        for width in 60..=240 {
            let area = Rect::new(0, 0, width, 18);
            let expected = crate::cli_app::session_tui_layout::inspector_panes(area, true);
            model.query = Input::new("no match".into());
            model.ensure_selection();
            assert!(model.shows_managed_inspector());
            assert_eq!(
                crate::cli_app::session_tui_layout::inspector_panes(area, model.inspector_visible),
                expected
            );
            if let Some((list, inspector)) = expected {
                assert!(list.width - 2 >= 72 && inspector.width - 2 >= 38);
                assert!(list.width <= crate::cli_app::session_tui_layout::LIST_PANE_MAX_WIDTH);
                assert_eq!(inspector.right(), area.right());
            }
        }
    }

    // Invoked by scripts/tui-b2-pty.py in a real isolated PTY. Normal suite
    // does not claim a terminal observation merely by selecting this test.
    #[test]
    fn ui_contract_b2_terminal_process_child() {
        if std::env::var_os("CUTEX_TUI_PTY_CHILD").is_none() {
            return;
        }
        use std::io::Write;
        let marker = |text: &str| {
            println!("{text}");
            io::stdout().flush().unwrap();
        };
        let mut shell = TerminalShell::open().unwrap();
        let mut events = ShellEvents;
        let mut selector = contract_recent_model();
        selector.activate_primary_panel(PrimaryPanel::Agents);
        let (_sender, receiver) = mpsc::channel();
        let mut refresh = WorkspaceLoad::new(receiver);
        let mut catalog = None;
        let mut projects = Some(super::super::session_tui_cutex_projects::terminal_fixture());
        for step in 0..60 {
            marker(&format!("B2_SWITCH_{step}"));
            match step % 3 {
                0 | 1 => {
                    selector.activate_primary_panel(if step % 3 == 0 {
                        PrimaryPanel::Agents
                    } else {
                        PrimaryPanel::Recent
                    });
                    let outcome = run_event_loop(
                        shell.terminal(),
                        &mut events,
                        &mut selector,
                        &mut refresh,
                        &mut catalog,
                    )
                    .unwrap();
                    assert!(matches!(outcome, SessionTuiCycleOutcome::Switch(_)));
                }
                _ => {
                    let (outcome, model) = super::super::session_tui_cutex_projects::run(
                        shell.terminal(),
                        &mut events,
                        projects.take(),
                        Default::default(),
                    )
                    .unwrap();
                    assert_eq!(outcome, PrimaryPanelOutcome::Switch(PrimaryPanel::Agents));
                    projects = Some(model);
                }
            }
        }
        marker("B2_RAW_READY");
        assert!(matches!(event::read().unwrap(), Event::Key(_)));
        shell
            .handoff(|| {
                let status = std::process::Command::new("/bin/sh")
                    .args([
                        "-c",
                        "printf 'B2_CHILD_READY\\n'; read value; test \"$value\" = child",
                    ])
                    .status()?;
                anyhow::ensure!(status.success(), "mock child failed");
                Ok(())
            })
            .unwrap()
            .unwrap();
        marker("B2_RESUMED");
        assert!(matches!(event::read().unwrap(), Event::Key(_)));
        let failed: anyhow::Result<()> = shell
            .handoff(|| anyhow::bail!("fixture foreground failure"))
            .unwrap();
        assert!(failed.is_err());
        marker("B2_ERROR_RESUMED");
        assert!(matches!(event::read().unwrap(), Event::Key(_)));
        drop(shell);
        marker("B2_COOKED_DONE");
    }
    use super::*;

    use std::collections::BTreeMap;

    use clap::Parser;
    use cutex::agent_bus::model::AgentRegistrationClass;
    use cutex::agent_management::{
        AgentManagementStoreSchema, ManagedAgentRecord, ManagedAgentSpec, ProjectId,
        ProjectPresentationSettings,
    };
    use cutex::cli::args::{Cli, CommandKind};
    use cutex::im::registry::ImRegistry;
    use cutex::observability::{
        ObservationAssociation, SafeOutputClass, SafeOutputProjection, SafeToolCallProjection,
    };
    use cutex::profiles::deepseek;
    use cutex::profiles::model::RuntimeConfig;
    use cutex::role_revision::{CutexSessionId, Rfc3339};
    use cutex::session::model::CutexSessionRuntimeBackend;
    use ratatui::backend::TestBackend;

    fn output_projection(updated_at: &str) -> SafeOutputProjection {
        SafeOutputProjection {
            association: ObservationAssociation::session("test-session"),
            class: SafeOutputClass::FinalVisible,
            display_text: "completed".to_string(),
            updated_at: updated_at.to_string(),
            runtime_generation: 1,
        }
    }

    fn tool_projection(
        class: SafeToolCallClass,
        status: SafeToolCallStatus,
        updated_at: &str,
    ) -> SafeToolCallProjection {
        SafeToolCallProjection {
            association: ObservationAssociation::session("test-session"),
            class,
            status,
            display_text: "safe label only".to_string(),
            updated_at: updated_at.to_string(),
            runtime_generation: 1,
        }
    }

    fn row(
        key: &str,
        agent: &str,
        lifecycle: CutexSessionLifecycleState,
        pinned: bool,
        managed: bool,
    ) -> SelectorRow {
        let mut actions = vec![
            SessionTuiActionItem {
                action: SessionTuiAction::ResumeAttach,
                detail: "Take over the managed TUI",
                primary: true,
            },
            SessionTuiActionItem {
                action: SessionTuiAction::Online,
                detail: "Bring the managed runtime online",
                primary: false,
            },
        ];
        if lifecycle == CutexSessionLifecycleState::Online {
            actions.push(SessionTuiActionItem {
                action: SessionTuiAction::CloseAndRestart,
                detail: "Close runtime, then bring it online with the selected profile",
                primary: false,
            });
            actions.push(SessionTuiActionItem {
                action: SessionTuiAction::CloseRuntime,
                detail: "Gracefully close runtime; keep session and history",
                primary: false,
            });
        }
        SelectorRow {
            view: None,
            target: SelectorTarget::Agent(key.to_string()),
            agent: agent.to_string(),
            thread_title: None,
            project: None,
            configured_profile: Some("orchid".to_string()),
            lifecycle: Some(lifecycle),
            host: "devbox".to_string(),
            backend: "alden".to_string(),
            managed_path: "~/Projects/cutex".to_string(),
            retired_at: None,
            revision: 1,
            activity_session_id: Some(key.to_string()),
            activity: None,
            actions,
            settings: vec![
                SessionTuiSettingCategory {
                    label: "Identity",
                    options: vec![
                        SessionTuiSettingOption {
                            label: "Agent name",
                            value: agent.to_string(),
                            field: None,
                            global_field: None,
                            profile_field: None,
                            command: None,
                            navigation: None,
                            presentation: Default::default(),
                            dirty: false,
                        },
                        SessionTuiSettingOption {
                            label: "Host",
                            value: "devbox".to_string(),
                            field: None,
                            global_field: None,
                            profile_field: None,
                            command: None,
                            navigation: None,
                            presentation: Default::default(),
                            dirty: false,
                        },
                    ],
                },
                SessionTuiSettingCategory {
                    label: "Launch",
                    options: vec![SessionTuiSettingOption {
                        label: "Runtime backend",
                        value: "alden".to_string(),
                        field: None,
                        global_field: None,
                        profile_field: None,
                        command: None,
                        navigation: None,
                        presentation: Default::default(),
                        dirty: false,
                    }],
                },
            ],
            settings_snapshot: None,
            global_settings_snapshot: None,
            attachable: lifecycle == CutexSessionLifecycleState::Online,
            pinned,
            managed,
        }
    }

    fn confirmed_close_intent(model: &mut SelectorModel) -> SessionTuiIntent {
        assert_eq!(model.activate_close_shortcut(), SelectorControl::Continue);
        model.handle(SelectorEvent::OpenActions);
        match model.handle(SelectorEvent::Activate) {
            SelectorControl::Selected(intent) => intent,
            control => panic!("expected confirmed close intent, got {control:?}"),
        }
    }

    fn project_snapshot(
        cutex_session_id: &str,
        project_id: &str,
        presentation: Option<ProjectPresentationSettings>,
    ) -> AgentManagementSnapshot {
        let cutex_session_id = CutexSessionId::new(cutex_session_id).expect("session id");
        let project_id = ProjectId::new(project_id).expect("project id");
        let timestamp = Rfc3339::new("2026-09-03T00:00:00Z").expect("timestamp");
        let agent = ManagedAgentRecord {
            project_id: Some(project_id.clone()),
            created_by_director_session: Some(CutexSessionId::new("cutex.director").unwrap()),
            created_by_operator_session: None,
            cutex_session_id: cutex_session_id.clone(),
            native_session_id: "native-worker".to_string(),
            spec: ManagedAgentSpec {
                name: "worker-zeta".to_string(),
                cwd: "/tmp/canonical-worker".to_string(),
                profile: Some("default".to_string()),
                runtime_backend: "app_server".to_string(),
                model: "gpt-test".to_string(),
                reasoning: "medium".to_string(),
                permissions: "default".to_string(),
                approval_policy: "never".to_string(),
                sandbox_mode: "workspace-write".to_string(),
                groups: vec!["workers".to_string()],
                expose_to_im: false,
                pin: false,
            },
            created_at: timestamp,
            retired_at: None,
        };
        AgentManagementSnapshot {
            bootstrap_intents: BTreeMap::new(),
            agent_archive_actions: BTreeMap::new(),
            agent_archive_audit: BTreeMap::new(),
            reversible_archive_projection: BTreeMap::new(),
            schema: AgentManagementStoreSchema::V1,
            store_revision: 1,
            durable_import_actions: BTreeMap::new(),
            durable_import_audit: BTreeMap::new(),
            projects: BTreeMap::new(),
            operator_grants: BTreeMap::new(),
            operator_grant_revisions: BTreeMap::new(),
            operator_audit_events: BTreeMap::new(),
            human_management_operator_actions: BTreeMap::new(),
            human_management_project_mutations: BTreeMap::new(),
            current_project_memberships: BTreeMap::new(),
            project_states: BTreeMap::new(),
            project_tombstones: BTreeMap::new(),
            project_audit_events: BTreeMap::new(),
            project_presentations: presentation
                .map(|presentation| BTreeMap::from([(project_id, presentation)]))
                .unwrap_or_default(),
            agents: BTreeMap::from([(cutex_session_id, agent)]),
            actions: BTreeMap::new(),
            phase_events: BTreeMap::new(),
            authority_receipts: BTreeMap::new(),
            legacy_director_ownership_import_receipts: BTreeMap::new(),
            reservation_reconciliation_receipts: BTreeMap::new(),
            reservation_reconciliation_events: BTreeMap::new(),
            failure_events: BTreeMap::new(),
            sparse_requests: BTreeMap::new(),
            extra: BTreeMap::new(),
        }
    }

    fn stored_project_presentation() -> ProjectPresentationSettings {
        ProjectPresentationSettings {
            display_name: "Nova Operations".to_string(),
            badge_label: "NX".to_string(),
            color: ProjectPaletteColor::Green,
            revision: 4,
            updated_at: Rfc3339::new("2026-09-03T00:00:00Z").unwrap(),
            updated_by_director_session: Some(CutexSessionId::new("cutex.director").unwrap()),
            updated_by_human_management: false,
            extra: BTreeMap::new(),
        }
    }

    fn global_row() -> SelectorRow {
        global_settings_row(&CodezConfig::default())
    }

    fn profiles_test_row() -> SelectorRow {
        profiles_row(&CodezConfig::default(), &[])
    }

    fn profile_catalog_entry(name: &str, active: bool) -> ProfileCatalogEntry {
        ProfileCatalogEntry {
            options: Default::default(),
            quota_usage: None,
            quota_remaining_percent: None,
            quota_balance: None,
            quota_reset_at: None,
            quota_observed_at: None,
            quota_stale: false,
            usage_refreshed_at: None,
            usage_error: None,
            official_usage: None,
            id: format!("id-{name}"),
            name: name.to_string(),
            email: Some(format!("{name}@example.test")),
            plan_type: Some("pro".to_string()),
            plan_label: None,
            account_plan: None,
            quota_preset: cutex::profiles::quota::Choice::Custom,
            source: Some("official".to_string()),
            runtime: RuntimeConfig::Host,
            proxy: None,
            session: None,
            cli_kind: "codex".to_string(),
            default_cli_args: vec!["--model".to_string(), "gpt-test".to_string()],
            agent_name: Some(format!("{name}-agent")),
            api_key_configured: false,
            codex_config: Some(Default::default()),
            codex_config_error: None,
            active,
        }
    }

    const EDITABLE_AGENT_KEY: &str = "cutex.editable";

    fn editable_record() -> CutexSessionRecord {
        let mut record = CutexSessionRecord::new_at(
            EDITABLE_AGENT_KEY.to_string(),
            Some("019e-editable".to_string()),
            "devbox".to_string(),
            "/tmp/editable".to_string(),
            None,
            "2026-08-05T00:00:00Z".to_string(),
        )
        .expect("editable record");
        record.display_name_hint = Some("editable-agent".to_string());
        record.quick_action = CutexSessionQuickActionMode::Pinned;
        record.permission_defaults = Some("workspace".to_string());
        record.approval_policy = Some("on-request".to_string());
        record.sandbox_mode = Some("workspace-write".to_string());
        record.reasoning_defaults = Some("medium".to_string());
        record
    }

    fn editable_model(record: &CutexSessionRecord) -> SelectorModel {
        editable_model_with_profiles(record, &[])
    }

    fn editable_model_with_profiles(
        record: &CutexSessionRecord,
        profile_names: &[String],
    ) -> SelectorModel {
        SelectorModel::new(
            vec![selector_row(
                EDITABLE_AGENT_KEY,
                record,
                &[],
                &[],
                profile_names,
            )],
            false,
            false,
        )
    }

    fn stage_full_access(model: &mut SelectorModel) {
        assert_eq!(
            model.handle(SelectorEvent::OpenSettings),
            SelectorControl::Continue
        );
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);
        assert!(matches!(
            model.settings_overlay.as_ref(),
            Some(SettingsOverlay::Choice {
                field: SettingsEditField::Session(SessionSettingsField::PermissionPreset),
                selected: 2,
                custom_value: None,
                ..
            })
        ));
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);
        assert_eq!(model.settings_dirty_count(), 1);
        assert_eq!(
            model
                .active_setting_option()
                .map(|option| option.value.as_str()),
            Some("full-access")
        );
    }

    fn select_management_setting(model: &mut SelectorModel) {
        if matches!(model.mode, SelectorMode::Agents) {
            assert_eq!(
                model.handle(SelectorEvent::OpenSettings),
                SelectorControl::Continue
            );
        }
        let target = model.active_row().expect("active agent row").target.clone();
        let (category, option) = model
            .active_row()
            .expect("active agent row")
            .settings
            .iter()
            .enumerate()
            .find_map(|(category, settings)| {
                settings
                    .options
                    .iter()
                    .position(|option| option.label == "Management")
                    .map(|option| (category, option))
            })
            .expect("management setting");
        model.mode = SelectorMode::Settings {
            target,
            category,
            option,
            focus: SettingsFocus::Options,
            view: SettingsView::Expanded,
        };
    }

    fn select_global_setting(model: &mut SelectorModel, field: GlobalSettingsField) {
        if matches!(model.mode, SelectorMode::Agents) {
            selector_command(model, Command::Settings);
            route_selector_key(model, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }
        let (category, option) = model
            .active_row()
            .expect("global row")
            .settings
            .iter()
            .enumerate()
            .find_map(|(category, settings)| {
                settings
                    .options
                    .iter()
                    .position(|option| option.global_field == Some(field))
                    .map(|option| (category, option))
            })
            .expect("global setting");
        model.mode = SelectorMode::Settings {
            target: SelectorTarget::GlobalSettings,
            category,
            option,
            focus: SettingsFocus::Options,
            view: SettingsView::Categories,
        };
    }

    fn open_profiles(model: &mut SelectorModel, profiles: Vec<ProfileCatalogEntry>) {
        if !model
            .context
            .settings
            .iter()
            .any(|row| row.target.is_profiles())
        {
            let snapshot = model
                .context
                .settings
                .iter()
                .find(|row| row.target.is_global_settings())
                .and_then(|row| row.global_settings_snapshot.clone())
                .expect("Global settings snapshot");
            model.context.settings.push(SelectorRow {
                view: None,
                target: SelectorTarget::Profiles,
                agent: "Profiles".to_string(),
                thread_title: None,
                project: None,
                configured_profile: None,
                lifecycle: None,
                host: "-".to_string(),
                backend: "accounts".to_string(),
                managed_path: "-".to_string(),
                retired_at: None,
                revision: 0,
                activity_session_id: None,
                activity: None,
                actions: Vec::new(),
                settings: Vec::new(),
                settings_snapshot: None,
                global_settings_snapshot: Some(snapshot),
                attachable: false,
                pinned: false,
                managed: false,
            });
            sort_rows(&mut model.rows);
        }
        model.mode = SelectorMode::Agents;
        model
            .workspace_selection
            .select(Some(SelectorTarget::Profiles));
        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::OpenProfileManager
        );
        model.open_profile_manager(profiles);
    }

    fn rendered_text(width: u16, model: &SelectorModel) -> String {
        rendered_text_at(width, 16, model)
    }

    fn rendered_text_at(width: u16, height: u16, model: &SelectorModel) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| render_selector(frame, model))
            .expect("render selector");
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer.cell((x, y)).expect("cell").symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn cutex_tui_is_an_explicit_command() {
        let cli = Cli::try_parse_from(["cutex", "tui"]).expect("parse tui command");
        assert!(matches!(cli.command, Some(CommandKind::Tui)));
    }

    #[test]
    fn ui_contract_c_d01_d02_d03_exact_member_union_and_unavailable() {
        use cutex::agent_management::{
            CutexProjectWorkspace, ProjectMemberLifecycle, ProjectMemberProjection,
        };
        let snapshot = project_snapshot("cutex.one", "alpha", None);
        let agent = snapshot.agents.values().next().unwrap().clone();
        let member = ProjectMemberProjection {
            agent: agent.clone(),
            lifecycle: ProjectMemberLifecycle::Unavailable,
            runtime: None,
            observation_error: Some("provider runtime unavailable".into()),
        };
        let mut other = member.clone();
        other.agent.cutex_session_id = CutexSessionId::new("cutex.two").unwrap();
        other.agent.native_session_id = "native-two".into();
        let mut project: CutexProjectWorkspace = serde_json::from_value(serde_json::json!({
            "project_id":"alpha", "authority_epoch":1, "director":{"cutex_session_id":"cutex.one", "member":member},
            "access_role":"human_management", "operator_grant_revision":1,
            "agent_operators":[{"member":member,"grant":{"project_id":"alpha","operator_cutex_session_id":"cutex.one","grant_revision":1,"granted_at":"2026-09-09T00:00:00Z","granted_by_primary_director_session":"cutex.one"}}],
            "presentation":effective_presentation(&ProjectId::new("alpha").unwrap(),None),
            "active_agents":[member,other],"retired_agents":[]
        })).unwrap();
        let rows = views::project_members(&project);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, rows[1].name);
        assert_eq!(rows[0].cwd, rows[1].cwd);
        assert_ne!(rows[0].subject, rows[1].subject);
        assert_eq!(rows[0].role, "Director/Operator/Member");
        assert!(rows
            .iter()
            .all(|r| r.badge.as_ref().unwrap().label == project.presentation.badge_label));
        assert_eq!(
            rows[0].project,
            Observation::Known(project.presentation.display_name.clone())
        );
        assert!(matches!(rows[0].runtime, Observation::Unavailable(_)));
        assert!(matches!(
            rows[0].effective_profile,
            Observation::Unavailable(_)
        ));
        project.director.as_mut().unwrap().member = None;
        let rows = views::project_members(&project);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].role.starts_with("Director/"));
        assert_eq!(rows[0].name, agent.spec.name);
        project.director.as_mut().unwrap().member = Some(member.clone());
        let operator = project.agent_operators[0].clone();
        project.agent_operators = (1..=2)
            .map(|i| {
                let mut operator = operator.clone();
                let id = CutexSessionId::new(format!("cutex.operator-{i}")).unwrap();
                operator.member.agent.cutex_session_id = id.clone();
                operator.grant.operator_cutex_session_id = id;
                operator
            })
            .collect();
        project.active_agents = (1..=3)
            .map(|i| {
                let mut member = member.clone();
                member.agent.cutex_session_id =
                    CutexSessionId::new(format!("cutex.member-{i}")).unwrap();
                member
            })
            .collect();
        let rows = views::project_members(&project);
        assert_eq!(rows.len(), 6);
        assert_eq!(rows.iter().filter(|r| r.role == "Director").count(), 1);
        assert_eq!(rows.iter().filter(|r| r.role == "Operator").count(), 2);
        assert_eq!(rows.iter().filter(|r| r.role == "Member").count(), 3);
    }

    #[test]
    fn ui_contract_c_d05_v06_scope_query_and_thousand_row_selection() {
        let rows = (0..1000)
            .map(|i| {
                row(
                    &format!("id-{i:04}"),
                    "duplicate name",
                    if i % 2 == 0 {
                        CutexSessionLifecycleState::Online
                    } else {
                        CutexSessionLifecycleState::Offline
                    },
                    i % 5 == 0,
                    true,
                )
            })
            .collect();
        let mut model = SelectorModel::new(rows, false, false);
        assert_eq!(model.visible_rows().len(), 1000);
        route_selector_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::ALT),
        );
        assert_eq!(model.visible_rows().len(), 500);
        route_selector_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::ALT),
        );
        assert_eq!(model.visible_rows().len(), 200);
        model.query = Input::new("duplicate".into());
        assert_eq!(model.visible_rows().len(), 200);
        selector_command(&mut model, Command::Scope);
        model.select_edge(true);
        let selected = model.selected_target();
        let mut rows = model.rows.clone();
        rows.reverse();
        model.replace_snapshot(SelectorSnapshot {
            rows,
            warning: None,
        });
        assert_eq!(model.selected_target(), selected);
        let mut rows = model.rows.clone();
        rows.retain(|r| Some(r.target.clone()) != selected);
        model.replace_snapshot(SelectorSnapshot {
            rows,
            warning: None,
        });
        assert_eq!(model.selected_visible_index(), Some(998));
    }

    #[test]
    fn ui_contract_c_global_settings_navigation_and_project_and_alarms_return() {
        let mut model = SelectorModel::new(
            vec![
                row(
                    "id",
                    "formal",
                    CutexSessionLifecycleState::Offline,
                    false,
                    true,
                ),
                global_row(),
            ],
            false,
            false,
        );
        let selected = model.selected_target();
        route_selector_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('7'), KeyModifiers::ALT),
        );
        model.settings_return_panel = Some(PrimaryPanel::Projects);
        let text = rendered_text_at(100, 30, &model);
        for label in ["Settings"] {
            assert!(text.contains(label), "{label}: {text}");
        }
        assert!(matches!(
            route_selector_key(&mut model, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            SelectorKeyRoute::Control(None)
        ));
        assert_eq!(model.selected_target(), selected);
        route_selector_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('7'), KeyModifiers::ALT),
        );
        model.settings_return_panel = Some(PrimaryPanel::Alarms);
        assert!(rendered_text_at(100, 30, &model).contains("Settings"));
        assert!(matches!(
            route_selector_key(&mut model, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            SelectorKeyRoute::Control(None)
        ));
        assert_eq!(model.selected_target(), selected);
        model.leave_settings(); // explicit panel navigation leaves Settings; Esc does not.
        route_selector_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::ALT),
        );
        assert!(
            matches!(model.mode,SelectorMode::Settings{target:SelectorTarget::Agent(ref id),..} if id=="id")
        );
        assert!(model.settings_navigation.is_none());
    }

    #[test]
    fn visual_restoration_provider_badge_uses_exact_membership_not_name_or_cwd() {
        let mut agent = row(
            "cutex.one",
            "worker-zeta",
            CutexSessionLifecycleState::Offline,
            false,
            true,
        );
        agent.view = Some(selector_view(&agent, None));
        let mut other = agent.clone();
        other.view.as_mut().unwrap().subject = SubjectRef::Managed("cutex.other".into());
        let mut rows = vec![agent, other];
        let provider = project_snapshot("cutex.one", "alpha", Some(stored_project_presentation()));
        apply_provider_views(&mut rows, &CutexSessionStore::default(), &Ok(provider));
        let assigned = rows[0].view.as_ref().unwrap();
        assert_eq!(assigned.badge.as_ref().unwrap().label, "NX");
        assert_eq!(
            assigned.project,
            Observation::Known("Nova Operations".into())
        );
        assert_eq!(assigned.role, "Member");
        assert!(rows[1].view.as_ref().unwrap().badge.is_none());
        apply_provider_views(
            &mut rows,
            &CutexSessionStore::default(),
            &Err(anyhow::anyhow!("unavailable")),
        );
        assert!(rows.iter().all(|r| r.view.as_ref().unwrap().badge.is_none()
            && r.view.as_ref().unwrap().role.is_empty()));
    }

    #[test]
    fn ui_contract_c_d04_missing_provider_and_reversible_archive_are_distinct() {
        let mut agent = row(
            "cutex.one",
            "name",
            CutexSessionLifecycleState::Offline,
            false,
            true,
        );
        agent.view = Some(selector_view(&agent, None));
        let mut rows = vec![agent];
        apply_provider_views(
            &mut rows,
            &CutexSessionStore::default(),
            &Err(anyhow::anyhow!("fixture unavailable")),
        );
        assert!(matches!(
            rows[0].view.as_ref().unwrap().project,
            Observation::Unavailable(_)
        ));
        let mut durable = editable_record();
        durable.cutex_session_id = "cutex.one".into();
        durable.archive_state = cutex::session::model::CutexSessionArchiveState::Retired;
        let store = CutexSessionStore {
            sessions: HashMap::from([("cutex.one".into(), durable)]),
            ..Default::default()
        };
        let before = serde_json::to_value(&store).unwrap();
        apply_provider_views(
            &mut rows,
            &store,
            &Ok(project_snapshot("cutex.one", "alpha", None)),
        );
        assert!(rows[0]
            .view
            .as_ref()
            .unwrap()
            .retirement_note
            .as_ref()
            .unwrap()
            .contains("Reversibly archived"));
        assert_eq!(serde_json::to_value(&store).unwrap(), before);
        let mut model = SelectorModel::new(rows, false, false);
        model.mark_refresh_failed("refresh unavailable".into());
        assert!(matches!(
            model.rows[0].view.as_ref().unwrap().project,
            Observation::Stale(_, _)
        ));
    }

    #[test]
    fn selector_names_permission_projects_and_native_workspaces_unambiguously() {
        let permission_project = cutex_projects_row();
        let native_workspace = projects_row();
        assert_eq!(permission_project.agent, "Cutex Projects");
        assert_eq!(permission_project.backend, "permission model");
        assert_eq!(native_workspace.agent, "Workspaces");
        assert_eq!(native_workspace.backend, "Codex catalog");
        assert_ne!(permission_project.target, native_workspace.target);
    }

    #[test]
    fn homepage_project_context_uses_stable_effective_defaults() {
        let snapshot = project_snapshot("cutex.worker-zeta", "project-alpha", None);
        let first = selector_project_contexts(&snapshot);
        let second = selector_project_contexts(&snapshot);

        assert_eq!(first, second);
        assert_eq!(
            first.get("cutex.worker-zeta"),
            Some(&SelectorProjectContext {
                agent_name: "worker-zeta".to_string(),
                project_id: "project-alpha".to_string(),
                display_name: "project-alpha".to_string(),
                badge_label: "PA".to_string(),
                color: effective_presentation(&ProjectId::new("project-alpha").unwrap(), None,)
                    .color,
            })
        );
    }

    #[test]
    fn managed_display_name_uses_saved_name_but_not_generated_title() {
        let mut record = editable_record();
        record.formal_agent_name = None;
        record.display_name_hint = Some("sailing-era-mod".into());
        record.thread_name = Some("A generated conversation title".into());
        assert_eq!(managed_session_fallback_name(&record), "sailing-era-mod");
        assert_eq!(retired_selector_row("key", &record, None).agent, "sailing-era-mod");
        record.formal_agent_name = Some("canonical-name".into());
        assert_eq!(managed_session_fallback_name(&record), "canonical-name");
        record.formal_agent_name = Some("  ".into());
        assert_eq!(managed_session_fallback_name(&record), "sailing-era-mod");
        record.display_name_hint = Some("  ".into());
        assert_eq!(managed_session_fallback_name(&record), record.cutex_session_id);
    }

    #[test]
    fn homepage_project_ownership_is_an_exact_canonical_session_join() {
        let snapshot = project_snapshot(
            "cutex.exact-worker",
            "project-alpha",
            Some(stored_project_presentation()),
        );
        let project_contexts = selector_project_contexts(&snapshot);
        let mut exact = editable_record();
        exact.cutex_session_id = "cutex.exact-worker".to_string();
        exact.display_name_hint = Some("exact-worker".to_string());
        exact.thread_name = Some("Generated conversation title".to_string());
        exact.registration_class = AgentRegistrationClass::Persistent;
        exact.agent_groups = vec!["unrelated".to_string()];
        exact.cwd = "/tmp/unrelated".to_string();
        exact.managed_cwd = Some("/tmp/unrelated".to_string());
        let mut decoy = editable_record();
        decoy.cutex_session_id = "cutex.decoy-worker".to_string();
        decoy.display_name_hint = Some("decoy-worker".to_string());
        decoy.thread_name = Some("Decoy conversation title".to_string());
        decoy.registration_class = AgentRegistrationClass::Persistent;
        decoy.agent_groups = vec!["project-alpha".to_string(), "NX".to_string()];
        decoy.cwd = "/tmp/Nova Operations/project-alpha".to_string();
        decoy.managed_cwd = Some("/tmp/Nova Operations/project-alpha".to_string());
        let store = CutexSessionStore {
            sessions: HashMap::from([
                ("store-exact".to_string(), exact),
                ("store-decoy".to_string(), decoy),
            ]),
            ..CutexSessionStore::default()
        };
        let compaction_counts = HashMap::from([(
            (
                "cutex.exact-worker".to_string(),
                "019e-editable".to_string(),
            ),
            Some(3_u64),
        )]);

        let rows = selector_rows_from_store(
            &store,
            &[],
            &[],
            &CodezConfig::default(),
            &[],
            &HashMap::new(),
            &project_contexts,
            &compaction_counts,
        );
        let exact = rows
            .iter()
            .find(|row| row.target == SelectorTarget::Agent("store-exact".to_string()))
            .expect("exact row");
        let decoy = rows
            .iter()
            .find(|row| row.target == SelectorTarget::Agent("store-decoy".to_string()))
            .expect("decoy row");

        assert_eq!(
            exact
                .project
                .as_ref()
                .map(|project| project.project_id.as_str()),
            Some("project-alpha")
        );
        assert_eq!(exact.agent, "worker-zeta");
        assert_eq!(
            exact.thread_title.as_deref(),
            Some("Generated conversation title")
        );
        assert_eq!(exact.view.as_ref().unwrap().compaction_count, Some(3));
        assert_eq!(decoy.agent, "decoy-worker");
        assert_eq!(decoy.view.as_ref().unwrap().compaction_count, None);
        assert_ne!(decoy.agent, "Decoy conversation title");
        assert!(decoy.project.is_none());
        assert!(rows
            .iter()
            .filter(|row| row.target.is_system())
            .all(|row| row.project.is_none()));
    }

    #[test]
    fn homepage_name_filter_is_separate_from_project_facet() {
        let mut associated = row(
            "associated",
            "worker-zeta",
            CutexSessionLifecycleState::Offline,
            false,
            true,
        );
        associated.project = Some(SelectorProjectContext {
            agent_name: "worker-zeta".to_string(),
            project_id: "project-8f31".to_string(),
            display_name: "Nova Operations".to_string(),
            badge_label: "NX".to_string(),
            color: ProjectPaletteColor::Green,
        });

        for query in ["nova operations", "project-8f31", "nx"] {
            let mut model=SelectorModel::new(vec![associated.clone()],false,false);
            model.query=Input::new(query.into());assert!(model.visible_rows().is_empty());
        }
        let mut model=SelectorModel::new(vec![associated.clone()],false,false);
        model.facets.project=Some(super::super::session_tui_filters::Choice{key:super::super::session_tui_filters::project_key(&associated.host,"project-8f31"),label:"Nova Operations".into()});
        assert_eq!(model.visible_rows().len(),1);
        model.facets.host=Some(super::super::session_tui_filters::Choice{key:"another-host".into(),label:"Other".into()});
        assert!(model.visible_rows().is_empty());

        let mut unowned = associated;
        unowned.target = SelectorTarget::Agent("unowned".to_string());
        unowned.project = None;
        unowned.managed_path = "/tmp/Nova Operations/project-8f31/NX".to_string();
        let mut model = SelectorModel::new(vec![unowned], false, false);
        for character in "project-8f31".chars() {
            model.handle(SelectorEvent::Insert(character));
        }
        assert!(model.visible_rows().is_empty());
    }

    #[test]
    fn homepage_project_name_is_secondary_to_formal_agent_name() {
        let mut agent = row(
            "associated",
            "worker-zeta",
            CutexSessionLifecycleState::Online,
            false,
            true,
        );
        agent.project = Some(SelectorProjectContext {
            agent_name: "worker-zeta".into(),
            project_id: "project-8f31".into(),
            display_name: "Nova Operations".into(),
            badge_label: "NX".into(),
            color: ProjectPaletteColor::Green,
        });
        let model = SelectorModel::new(vec![agent], false, false);
        for width in [72, 100, 160] {
            let text = rendered_text_at(width, 30, &model);
            assert!(text.contains("worker-zeta"));
            assert!(text.contains("Nova Operations"), "{text}");
        }
    }

    #[test]
    fn inspector_settings_links_the_selected_agents_project_badge_editor() {
        let mut project_row = row(
            "associated",
            "worker-zeta",
            CutexSessionLifecycleState::Online,
            false,
            true,
        );
        project_row.project = Some(SelectorProjectContext {
            agent_name: "worker-zeta".to_string(),
            project_id: "project-8f31".to_string(),
            display_name: "Nova Operations".to_string(),
            badge_label: "NX".to_string(),
            color: ProjectPaletteColor::Green,
        });
        let mut model = SelectorModel::new(vec![project_row], false, false);

        assert!(handle_managed_inspector_shortcut(
            &mut model,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::ALT),
        ));
        let rendered = rendered_text_at(180, 24, &model);
        assert!(rendered.contains("Project badge settings"));
        assert!(rendered.contains("NX  Nova Operations"));
        assert!(rendered.contains("Alt+P edit"));
    }

    #[test]
    fn v_toggles_secondary_thread_title_without_replacing_managed_name() {
        let mut managed = row(
            "cutex.stable-worker",
            "Stable Managed Name",
            CutexSessionLifecycleState::Online,
            false,
            true,
        );
        managed.thread_title = Some("Generated conversation title".to_string());
        let mut model = SelectorModel::new(vec![managed], false, false);

        let collapsed = rendered_text_at(100, 18, &model);
        assert!(collapsed.contains("Stable Managed Name"));
        assert!(!collapsed.contains("Generated conversation title"));

        assert!(!toggle_managed_thread_titles_from_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
        ));
        assert!(!model.show_thread_titles);

        assert!(toggle_managed_thread_titles_from_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT),
        ));
        let expanded = rendered_text_at(100, 18, &model);
        assert!(expanded.contains("Stable Managed Name"));
        assert!(expanded.contains("Generated"));
        assert_eq!(
            model.selected_row().map(|row| row.agent.as_str()),
            Some("Stable Managed Name")
        );

        let mut terminal = Terminal::new(TestBackend::new(72, 12)).expect("test terminal");
        terminal
            .draw(|frame| render_selector(frame, &model))
            .expect("render narrow expanded selector");
        terminal.backend_mut().resize(140, 24);
        terminal
            .draw(|frame| render_selector(frame, &model))
            .expect("render resized expanded selector");
        let resized = rendered_text_at(140, 24, &model);
        assert!(resized.contains("Stable Managed Name"));
        assert!(resized.contains("Native title: Generated"));

        assert!(toggle_managed_thread_titles_from_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('V'), KeyModifiers::ALT),
        ));
        assert!(!model.show_thread_titles);

        let mut system_row_model = SelectorModel::new(vec![projects_row()], false, false);
        assert!(!toggle_managed_thread_titles_from_key(
            &mut system_row_model,
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT),
        ));
        assert!(!system_row_model.show_thread_titles);
    }

    #[test]
    fn tab_and_horizontal_keys_stay_within_the_managed_workspace() {
        let mut model = SelectorModel::new(
            vec![row(
                "cutex.worker",
                "Worker",
                CutexSessionLifecycleState::Online,
                false,
                true,
            )],
            false,
            false,
        );

        assert_eq!(
            selector_navigation_control_from_key(
                &mut model,
                KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            ),
            Some(SelectorControl::Continue)
        );
        assert_eq!(model.mode, SelectorMode::Agents);
        assert!(model.inspector_overview_focused);
        assert_eq!(model.inspector_section(), InspectorSection::Overview);
        assert!(rendered_text_at(120, 18, &model).contains("Agent Details"));
        assert_eq!(
            selector_navigation_control_from_key(
                &mut model,
                KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            ),
            Some(SelectorControl::Continue)
        );
        assert!(model.inspector_overview_focused);
        assert_eq!(
            selector_navigation_control_from_key(
                &mut model,
                KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            ),
            Some(SelectorControl::Continue)
        );
        assert_eq!(model.mode, SelectorMode::Agents);
        assert!(!model.inspector_overview_focused);
        assert_eq!(
            selector_list_panel_from_horizontal_key(
                &model,
                KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            ),
            Some(Some(PrimaryPanel::Recent))
        );
        assert_eq!(
            selector_list_panel_from_horizontal_key(
                &model,
                KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            ),
            Some(None)
        );

        assert!(!handle_managed_inspector_shortcut(
            &mut model,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        ));
        for character in ['a', 'e', 'v'] {
            let event = selector_event_from_key(
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
                false,
            )
            .expect("printable filter character");
            model.handle(event);
        }
        assert_eq!(model.query.value(), "aev");
        model.query.reset();
        model.ensure_selection();
        model.inspector_overview_focused = true;
        assert!(handle_managed_inspector_shortcut(
            &mut model,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT),
        ));
        assert_eq!(model.inspector_section(), InspectorSection::Actions);
        assert_eq!(
            selector_navigation_control_from_key(
                &mut model,
                KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            ),
            Some(SelectorControl::Continue)
        );
        assert_eq!(model.inspector_section(), InspectorSection::Actions);
        assert!(handle_managed_inspector_shortcut(
            &mut model,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::ALT),
        ));
        assert_eq!(model.inspector_section(), InspectorSection::Settings);
        assert_eq!(
            model.handle_focus_traversal(false),
            SelectorControl::Continue
        );
        assert_eq!(model.mode, SelectorMode::Agents);
        assert!(!model.inspector_overview_focused);
    }

    #[test]
    fn recent_switch_restores_managed_editor_and_view_state() {
        let mut model = SelectorModel::new(
            vec![row(
                "cutex.worker",
                "Worker",
                CutexSessionLifecycleState::Offline,
                false,
                true,
            )],
            false,
            false,
        );
        model.open_settings();
        let original_mode = model.mode.clone();
        model.settings_overlay = Some(SettingsOverlay::Text {
            field: SettingsEditField::Session(SessionSettingsField::AgentName),
            input: Input::new("Uncommitted editor text".to_string()),
            tags: false,
            masked: false,
        });
        model.query = Input::new("worker".to_string());
        model.show_thread_titles = true;

        model.activate_primary_panel(PrimaryPanel::Recent);
        assert_eq!(model.mode, SelectorMode::RecentSessions);
        model.activate_primary_panel(PrimaryPanel::Agents);

        assert_eq!(model.mode, original_mode);
        assert_eq!(model.query.value(), "worker");
        assert!(model.show_thread_titles);
        assert!(matches!(
            &model.settings_overlay,
            Some(SettingsOverlay::Text { input, .. })
                if input.value() == "Uncommitted editor text"
        ));
    }

    #[test]
    fn non_terminal_invocations_are_rejected_before_ui_setup() {
        assert!(require_interactive_terminal(true, true).is_ok());
        assert!(require_interactive_terminal(false, true).is_err());
        assert!(require_interactive_terminal(true, false).is_err());
    }

    #[test]
    fn session_row_separates_lifecycle_from_attachability_and_keeps_takeover_primary() {
        let mut record = CutexSessionRecord::new_at(
            "cutex.alden-row".to_string(),
            Some("019e-alden-row".to_string()),
            "devbox".to_string(),
            "/tmp/alden-row".to_string(),
            Some("orchid".to_string()),
            "2026-08-05T00:00:00Z".to_string(),
        )
        .expect("record");
        record.display_name_hint = Some("alden-row".to_string());
        record.runtime_backend = cutex::session::model::CutexSessionRuntimeBackend::CuteAlden;
        record.managed_cwd = Some("/tmp/managed".to_string());
        record.alden_session_name = Some("cutex.alden-row.runtime".to_string());
        record.alden_pid = Some(std::process::id());
        let alden_sessions = vec![CuteAldenSession {
            pid: std::process::id(),
            name: record.alden_session_name.clone(),
        }];

        let row = selector_row("cutex.alden-row", &record, &alden_sessions, &[], &[]);

        assert_eq!(row.agent, "alden-row");
        assert_eq!(row.lifecycle, Some(CutexSessionLifecycleState::Online));
        assert!(row.attachable);
        assert_eq!(row.host, "devbox");
        assert_eq!(row.backend, "alden");
        assert_eq!(row.managed_path, "/tmp/managed");
        assert_eq!(row.actions[0].action, SessionTuiAction::ResumeAttach);
        assert!(row.actions[0].primary);
        assert_eq!(
            row.actions
                .iter()
                .find(|item| item.primary)
                .map(|item| item.action.label()),
            Some("takeover"),
        );
    }

    #[test]
    fn detached_alden_row_names_the_missing_tui_and_opens_it_as_primary() {
        let mut record = CutexSessionRecord::new_at(
            "cutex.detached-row".to_string(),
            Some("019e-detached-row".to_string()),
            "devbox".to_string(),
            "/tmp/detached-row".to_string(),
            Some("orchid".to_string()),
            "2026-08-08T00:00:00Z".to_string(),
        )
        .expect("record");
        record.display_name_hint = Some("detached-row".to_string());
        record.runtime_backend = cutex::session::model::CutexSessionRuntimeBackend::CuteAlden;
        record.current_runtime_agent_id = Some("cutex.detached-row.runtime".to_string());
        record.app_server_runtime = Some(cutex::session::model::CutexAppServerRuntimeBinding {
            transport: cutex::session::model::CutexAppServerTransport::UnixSocket,
            endpoint: "unix:///tmp/runtime/app.sock".to_string(),
            pid: std::process::id(),
            runtime_dir: "/tmp/runtime".to_string(),
            launched_profile: Some("orchid".to_string()),
            launched_profile_id: None,
            launch_profile_source: None,
            auth_token_path: None,
            diagnostic_journal_path: "/tmp/runtime/events.jsonl".to_string(),
            schema_version: "test".to_string(),
            schema_sha256: "hash".to_string(),
            started_at: "2026-08-08T00:00:00Z".to_string(),
        });
        let live_agents = vec![AgentBusAgent {
            id: "cutex.detached-row.runtime".to_string(),
            name: "detached-row.runtime".to_string(),
            base_name: Some("detached-row".to_string()),
            thread_name: None,
            path_key: None,
            session_id: record.codex_session_id.clone(),
            cutex_session_id: None,
            profile: "orchid".to_string(),
            cwd: record.cwd.clone(),
            pid: std::process::id(),
            host_id: Some(cutex::platform::host::current_host_name()),
            groups: Vec::new(),
            registration_class: AgentRegistrationClass::Persistent,
            last_seen_epoch_secs: 42,
        }];

        let row = selector_row("cutex.detached-row", &record, &[], &live_agents, &[]);

        assert_eq!(row.lifecycle, Some(CutexSessionLifecycleState::Online));
        assert!(!row.attachable);
        assert_eq!(selector_state_label(&row), "DET");
        let mut other = row.clone();
        other.target = SelectorTarget::Agent("other".to_string());
        other.agent = "other".to_string();
        other.attachable = true;
        let mut model = SelectorModel::new(vec![row.clone(), other], false, false);
        model.handle(SelectorEvent::Down);
        let backend = TestBackend::new(WIDE_LAYOUT_MIN_WIDTH, 12);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| render_selector(frame, &model))
            .expect("render detached row");
        assert!(rendered_text_at(WIDE_LAYOUT_MIN_WIDTH, 12, &model).contains("Online"));
        assert_eq!(
            row.actions
                .iter()
                .find(|item| item.primary)
                .map(|item| item.action),
            Some(SessionTuiAction::OpenTui)
        );
    }

    #[test]
    fn agent_filter_excludes_management_navigation_rows() {
        let mut model = SelectorModel::new(
            vec![
                row(
                    "online",
                    "alpha-online",
                    CutexSessionLifecycleState::Online,
                    false,
                    true,
                ),
                row(
                    "pinned",
                    "beta-pinned",
                    CutexSessionLifecycleState::Offline,
                    true,
                    false,
                ),
                row(
                    "managed",
                    "qa-managed",
                    CutexSessionLifecycleState::Offline,
                    false,
                    true,
                ),
                row(
                    "history",
                    "qa-history",
                    CutexSessionLifecycleState::Offline,
                    false,
                    false,
                ),
                profiles_test_row(),
                global_row(),
            ],
            false,
            false,
        );

        let initial = model
            .visible_rows()
            .iter()
            .map(|row| match &row.target {
                SelectorTarget::Agent(key) | SelectorTarget::RemoteAgent(_,key) | SelectorTarget::RetiredAgent(key) => key.as_str(),
                SelectorTarget::RecentSessions => "recent",
                SelectorTarget::RetiredSessions => "retired",
                SelectorTarget::CutexProjects => "cutex-projects",
                SelectorTarget::Projects => "workspaces",
                SelectorTarget::Profiles => "profiles",
                SelectorTarget::GlobalSettings => "global",
            })
            .collect::<Vec<_>>();
        assert_eq!(initial, vec!["online", "pinned", "history", "managed"]);

        assert_eq!(
            model.handle(SelectorEvent::Insert('q')),
            SelectorControl::Continue
        );
        let filtered = model
            .visible_rows()
            .iter()
            .map(|row| match &row.target {
                SelectorTarget::Agent(key) | SelectorTarget::RemoteAgent(_,key) | SelectorTarget::RetiredAgent(key) => key.as_str(),
                SelectorTarget::RecentSessions => "recent",
                SelectorTarget::RetiredSessions => "retired",
                SelectorTarget::CutexProjects => "cutex-projects",
                SelectorTarget::Projects => "workspaces",
                SelectorTarget::Profiles => "profiles",
                SelectorTarget::GlobalSettings => "global",
            })
            .collect::<Vec<_>>();
        assert_eq!(filtered, vec!["history", "managed"]);
        assert_eq!(model.query.value(), "q");

        for query in ['g', 'p'] {
            model.handle(SelectorEvent::ClearInput);
            model.handle(SelectorEvent::Insert(query));
            let visible = model
                .visible_rows()
                .iter()
                .map(|row| match &row.target {
                    SelectorTarget::Agent(key) | SelectorTarget::RemoteAgent(_,key) | SelectorTarget::RetiredAgent(key) => key.as_str(),
                    SelectorTarget::RecentSessions => "recent",
                    SelectorTarget::RetiredSessions => "retired",
                    SelectorTarget::CutexProjects => "cutex-projects",
                    SelectorTarget::Projects => "workspaces",
                    SelectorTarget::Profiles => "profiles",
                    SelectorTarget::GlobalSettings => "global",
                })
                .collect::<Vec<_>>();
            assert!(!visible.contains(&"profiles") && !visible.contains(&"global"));
        }
    }

    #[test]
    fn empty_filter_includes_offline_rows_in_all_scope() {
        let mut model = SelectorModel::new(
            vec![
                row(
                    "online",
                    "alpha-online",
                    CutexSessionLifecycleState::Online,
                    false,
                    true,
                ),
                row(
                    "offline-managed",
                    "beta-offline",
                    CutexSessionLifecycleState::Offline,
                    false,
                    true,
                ),
                row(
                    "offline-history",
                    "gamma-history",
                    CutexSessionLifecycleState::Offline,
                    false,
                    false,
                ),
                profiles_test_row(),
                global_row(),
            ],
            false,
            false,
        );

        assert_eq!(model.visible_rows().len(), 3);
        model.handle(SelectorEvent::Insert('b'));
        assert_eq!(model.visible_rows().len(), 1);
    }

    #[test]
    fn tui_store_load_reconciles_registry_before_reading_durable_store() {
        use std::cell::RefCell;

        let calls = RefCell::new(Vec::new());
        let store = load_reconciled_session_store_with(
            || {
                calls.borrow_mut().push("load registry");
                Ok(ImRegistry::default())
            },
            |_| {
                calls.borrow_mut().push("reconcile registry");
                Ok(())
            },
            || {
                calls.borrow_mut().push("load durable store");
                Ok(CutexSessionStore::default())
            },
        )
        .expect("load reconciled TUI store");

        assert!(store.sessions.is_empty());
        assert_eq!(
            calls.into_inner(),
            ["load registry", "reconcile registry", "load durable store"]
        );
    }

    #[test]
    fn main_navigation_stops_at_first_and_final_agent() {
        let mut model = SelectorModel::new(
            vec![
                row(
                    "alpha",
                    "alpha",
                    CutexSessionLifecycleState::Online,
                    false,
                    true,
                ),
                row(
                    "beta",
                    "beta",
                    CutexSessionLifecycleState::Online,
                    false,
                    true,
                ),
                profiles_test_row(),
                global_row(),
            ],
            false,
            false,
        );

        assert_eq!(
            model.selected_target(),
            Some(SelectorTarget::Agent("alpha".to_string()))
        );
        model.handle(SelectorEvent::Up);
        assert_eq!(
            model.selected_target(),
            Some(SelectorTarget::Agent("alpha".to_string()))
        );
        model.handle(SelectorEvent::Down);
        assert_eq!(
            model.selected_target(),
            Some(SelectorTarget::Agent("beta".to_string()))
        );
        model.handle(SelectorEvent::Last);
        assert_eq!(
            model.selected_target(),
            Some(SelectorTarget::Agent("beta".into()))
        );
        model.handle(SelectorEvent::Down);
        assert_eq!(
            model.selected_target(),
            Some(SelectorTarget::Agent("beta".into()))
        );
    }

    #[test]
    fn agent_settings_default_to_expanded_and_preserve_selection_across_views() {
        let mut model = SelectorModel::new(
            vec![row(
                "agent",
                "agent",
                CutexSessionLifecycleState::Online,
                false,
                true,
            )],
            false,
            false,
        );

        assert_eq!(
            model.handle(SelectorEvent::OpenSettings),
            SelectorControl::Continue
        );
        assert_eq!(
            model.mode,
            SelectorMode::Settings {
                target: SelectorTarget::Agent("agent".to_string()),
                category: 0,
                option: 0,
                focus: SettingsFocus::Options,
                view: SettingsView::Expanded,
            }
        );
        assert_eq!(
            expanded_setting_table_row_index(
                &model.active_row().expect("agent row").settings,
                0,
                0,
            ),
            Some(1)
        );

        model.handle(SelectorEvent::Up);
        assert_eq!(model.selected_setting_category_index(), Some(1));
        assert_eq!(model.selected_setting_option_index(), Some(0));
        assert_eq!(
            expanded_setting_table_row_index(
                &model.active_row().expect("agent row").settings,
                1,
                0,
            ),
            Some(4)
        );
        model.handle(SelectorEvent::Down);
        assert_eq!(model.selected_setting_category_index(), Some(0));
        model.handle(SelectorEvent::Down);
        assert_eq!(model.selected_setting_option_index(), Some(1));

        model.handle(SelectorEvent::Insert('v'));
        assert_eq!(model.settings_view(), Some(SettingsView::Categories));
        assert_eq!(model.settings_focus(), Some(SettingsFocus::Options));
        assert_eq!(model.selected_setting_category_index(), Some(0));
        assert_eq!(model.selected_setting_option_index(), Some(1));

        model.handle(SelectorEvent::Insert('V'));
        assert_eq!(model.settings_view(), Some(SettingsView::Expanded));
        assert_eq!(model.selected_setting_category_index(), Some(0));
        assert_eq!(model.selected_setting_option_index(), Some(1));
        model.handle(SelectorEvent::Back);
        assert_eq!(model.mode, SelectorMode::Agents);

        model.handle(SelectorEvent::OpenSettings);
        model.handle(SelectorEvent::OpenSettings);
        assert_eq!(model.mode, SelectorMode::Agents);
        assert_eq!(
            model.selected_target(),
            Some(SelectorTarget::Agent("agent".to_string()))
        );
    }

    #[test]
    fn staged_permission_is_written_only_by_apply_and_refreshes_the_same_row() {
        let record = editable_record();
        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert(EDITABLE_AGENT_KEY.to_string(), record.clone());
        let mut model = editable_model(&record);

        stage_full_access(&mut model);
        assert_eq!(store.sessions.get(EDITABLE_AGENT_KEY), Some(&record));

        let request = match model.handle(SelectorEvent::Insert('A')) {
            SelectorControl::ApplySettings(request) => request,
            control => panic!("expected apply request, got {control:?}"),
        };
        assert_eq!(request.key, EDITABLE_AGENT_KEY);
        assert_eq!(request.changed_count, 1);

        let updated =
            apply_session_settings_to_store(&mut store, &request, &[]).expect("apply draft");
        assert_eq!(updated.permission_defaults.as_deref(), Some("full-access"));
        assert_eq!(updated.approval_policy, record.approval_policy);
        // The explicit sandbox follows the preset; a stale workspace-write
        // next to full-access would be repaired back at launch.
        assert_eq!(updated.sandbox_mode.as_deref(), Some("danger-full-access"));
        assert_eq!(updated.model_defaults, record.model_defaults);
        assert_eq!(updated.reasoning_defaults, record.reasoning_defaults);

        model.settings_apply_succeeded(
            EDITABLE_AGENT_KEY,
            &updated,
            &[],
            request.changed_count,
            false,
            None,
        );
        assert_eq!(model.settings_dirty_count(), 0);
        assert_eq!(model.notice.as_deref(), Some("Saved 1 setting(s)"));
        let narrow = rendered_text_at(50, 16, &model);
        assert!(narrow.contains("Saved 1 setting(s)"));
        assert!(narrow.contains("Ctrl+C exit"));
        assert_eq!(
            model
                .active_setting_option()
                .map(|option| option.value.as_str()),
            Some("full-access")
        );
        assert!(matches!(
            model.mode,
            SelectorMode::Settings {
                target: SelectorTarget::Agent(ref key),
                ..
            } if key == EDITABLE_AGENT_KEY
        ));
    }

    #[test]
    fn management_confirmation_defaults_to_cancel_and_returns_a_typed_request() {
        let record = editable_record();
        let mut model = editable_model(&record);
        select_management_setting(&mut model);

        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::Continue
        );
        assert!(matches!(
            model.settings_overlay.as_ref(),
            Some(SettingsOverlay::ConfirmManagement {
                command: SessionSettingsCommand::Adopt,
                selected: 0,
            })
        ));
        let confirmation = rendered_text_at(80, 18, &model);
        assert!(confirmation.contains("Confirm Adopt"));
        assert!(confirmation.contains("platform default backend"));
        assert!(confirmation.contains("cute-codex history"));

        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::Continue
        );
        assert!(model.settings_overlay.is_none());

        model.handle(SelectorEvent::Activate);
        model.handle(SelectorEvent::Down);
        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::ManageSession(SessionManagementRequest {
                key: EDITABLE_AGENT_KEY.to_string(),
                command: SessionSettingsCommand::Adopt,
                profile_names: Vec::new(),
            })
        );
    }

    #[test]
    fn management_command_is_blocked_until_the_settings_draft_is_resolved() {
        let record = editable_record();
        let mut model = editable_model(&record);
        stage_full_access(&mut model);
        select_management_setting(&mut model);

        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::Continue
        );
        assert!(model.settings_overlay.is_none());
        assert_eq!(model.settings_dirty_count(), 1);
        assert_eq!(
            model.warning.as_deref(),
            Some("Apply or discard staged settings before changing management")
        );
    }

    #[test]
    fn management_commands_use_service_semantics_without_mutating_runtime_identity() {
        let mut local = editable_record();
        // This exercises store/UI projection, not machine-local runtime adoption.
        // A distinct host keeps the fixture independent of installed deployments.
        local.host_id = format!("{}-remote-test", cutex::platform::host::current_host_name());
        local.codex_session_id = Some("018e0000-0000-7000-8000-000000000001".into());
        local.current_runtime_agent_id = Some("runtime-occurrence".to_string());
        local.agent_groups.clear();
        let untouched = CutexSessionRecord::new_at(
            "cutex.untouched".to_string(),
            Some("019e-untouched".to_string()),
            "devbox".to_string(),
            "/tmp/untouched".to_string(),
            None,
            "2026-08-05T00:00:00Z".to_string(),
        )
        .expect("untouched record");
        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert(EDITABLE_AGENT_KEY.to_string(), local.clone());
        store
            .sessions
            .insert("cutex.untouched".to_string(), untouched.clone());

        let adopt = SessionManagementRequest {
            key: EDITABLE_AGENT_KEY.to_string(),
            command: SessionSettingsCommand::Adopt,
            profile_names: Vec::new(),
        };
        apply_session_management_to_store(&mut store, &adopt).expect("adopt local agent");
        let adopted = store
            .sessions
            .get(EDITABLE_AGENT_KEY)
            .expect("adopted record")
            .clone();
        assert_eq!(
            adopted.registration_class,
            AgentRegistrationClass::Persistent
        );
        assert_eq!(
            adopted.runtime_backend,
            if cfg!(windows) {
                CutexSessionRuntimeBackend::HostForeground
            } else {
                CutexSessionRuntimeBackend::CuteAlden
            }
        );
        assert!(adopted.agent_enabled);
        assert!(!adopted.agent_groups.is_empty());
        assert_eq!(adopted.codex_session_id, local.codex_session_id);
        assert_eq!(
            adopted.current_runtime_agent_id,
            local.current_runtime_agent_id
        );
        assert_eq!(adopted.cwd, local.cwd);
        assert_eq!(store.sessions.get("cutex.untouched"), Some(&untouched));

        let managed = store
            .sessions
            .get_mut(EDITABLE_AGENT_KEY)
            .expect("managed record");
        managed.exposed_to_backend = true;
        managed.managed_cwd = Some("/tmp/managed".to_string());
        managed.default_cli_args = vec!["--no-alt-screen".to_string()];

        let unmanage = SessionManagementRequest {
            key: EDITABLE_AGENT_KEY.to_string(),
            command: SessionSettingsCommand::Unmanage,
            profile_names: Vec::new(),
        };
        apply_session_management_to_store(&mut store, &unmanage).expect("unmanage agent");
        let unmanaged = store
            .sessions
            .get(EDITABLE_AGENT_KEY)
            .expect("unmanaged record");
        assert_eq!(
            unmanaged.registration_class,
            AgentRegistrationClass::LocalOnly
        );
        assert!(!unmanaged.agent_enabled);
        assert!(!unmanaged.exposed_to_backend);
        assert_eq!(unmanaged.managed_cwd, None);
        assert_eq!(unmanaged.quick_action, CutexSessionQuickActionMode::Auto);
        assert!(unmanaged.default_cli_args.is_empty());
        assert_eq!(unmanaged.permission_defaults, None);
        assert_eq!(unmanaged.approval_policy, None);
        assert_eq!(unmanaged.sandbox_mode, None);
        assert_eq!(unmanaged.model_defaults, None);
        assert_eq!(unmanaged.reasoning_defaults, None);
        assert_eq!(unmanaged.codex_session_id, local.codex_session_id);
        assert_eq!(
            unmanaged.current_runtime_agent_id,
            local.current_runtime_agent_id
        );
        assert_eq!(store.sessions.get("cutex.untouched"), Some(&untouched));
    }

    #[test]
    fn management_success_refreshes_the_row_and_survives_a_stale_snapshot() {
        let mut local = editable_record();
        // This exercises store/UI projection, not machine-local runtime adoption.
        // A distinct host keeps the fixture independent of installed deployments.
        local.host_id = format!("{}-remote-test", cutex::platform::host::current_host_name());
        local.codex_session_id = Some("018e0000-0000-7000-8000-000000000001".into());
        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert(EDITABLE_AGENT_KEY.to_string(), local.clone());
        let request = SessionManagementRequest {
            key: EDITABLE_AGENT_KEY.to_string(),
            command: SessionSettingsCommand::Adopt,
            profile_names: Vec::new(),
        };
        apply_session_management_to_store(&mut store, &request).expect("adopt agent");
        let adopted = store
            .sessions
            .get(EDITABLE_AGENT_KEY)
            .expect("adopted record")
            .clone();
        let mut model = SelectorModel::new(
            vec![selector_row(EDITABLE_AGENT_KEY, &local, &[], &[], &[])],
            true,
            false,
        );
        model.handle(SelectorEvent::OpenSettings);

        model.session_management_succeeded(
            EDITABLE_AGENT_KEY,
            SessionSettingsCommand::Adopt,
            &adopted,
            &[],
            None,
        );
        let row = model.active_row().expect("updated row");
        assert!(row.managed);
        assert_eq!(
            row.backend,
            runtime_backend_short_label(adopted.runtime_backend)
        );
        assert!(row.settings.iter().any(|category| {
            category.options.iter().any(|option| {
                option.command == Some(SessionSettingsCommand::Unmanage)
                    && option.value == "unmanage"
            })
        }));
        assert_eq!(model.notice.as_deref(), Some("Adopted agent"));
        let rendered = rendered_text_at(100, 24, &model);
        assert!(rendered.contains("Adopted agent") && rendered.contains("Ctrl+C exit"));

        model.replace_snapshot(SelectorSnapshot {
            rows: vec![selector_row(EDITABLE_AGENT_KEY, &local, &[], &[], &[])],
            warning: None,
        });
        assert!(model.active_row().expect("overridden row").managed);
    }

    #[test]
    fn unmanage_confirmation_explains_that_the_current_runtime_is_untouched() {
        let mut record = editable_record();
        record.registration_class = AgentRegistrationClass::Persistent;
        let mut model = editable_model(&record);
        select_management_setting(&mut model);

        model.handle(SelectorEvent::Activate);

        assert!(matches!(
            model.settings_overlay.as_ref(),
            Some(SettingsOverlay::ConfirmManagement {
                command: SessionSettingsCommand::Unmanage,
                selected: 0,
            })
        ));
        let confirmation = rendered_text_at(80, 18, &model);
        assert!(confirmation.contains("Confirm Unmanage"));
        assert!(confirmation.contains("does not close the current runtime"));
    }

    #[test]
    fn profile_choice_is_staged_then_applied_to_only_the_selected_session() {
        let _home = crate::cli_app::test_home::IsolatedTestHome::new("tui-profile-family").unwrap();
        let profiles = ["alpha", "beta"].into_iter().map(|name| {
            let mut profile = cutex::profiles::lifecycle::new_claude_account(name);
            profile.cli_kind = cutex::profiles::model::CliKind::Codex;
            profile.source = Some("official".into());
            profile
        }).collect();
        cutex::profiles::store::save_store(&cutex::profiles::model::AccountsStore { version: 3, accounts: profiles, active_account_id: None }).unwrap();
        let mut record = editable_record();
        record.profile = Some("alpha".to_string());
        let untouched = CutexSessionRecord::new_at(
            "cutex.untouched".to_string(),
            None,
            "devbox".to_string(),
            "/tmp/untouched".to_string(),
            Some("alpha".to_string()),
            "2026-08-05T00:00:00Z".to_string(),
        )
        .expect("untouched record");
        let profile_names = vec!["alpha".to_string(), "beta".to_string()];
        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert(EDITABLE_AGENT_KEY.to_string(), record.clone());
        store
            .sessions
            .insert("cutex.untouched".to_string(), untouched.clone());
        let mut model = editable_model_with_profiles(&record, &profile_names);

        model.handle(SelectorEvent::OpenSettings);
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);
        assert!(matches!(
            model.settings_overlay.as_ref(),
            Some(SettingsOverlay::Choice {
                field: SettingsEditField::Session(SessionSettingsField::Profile),
                choices,
                selected: 1,
                custom_value: None,
            }) if choices.iter().map(|choice| choice.label.as_str()).collect::<Vec<_>>() == ["Follow global default", "alpha", "beta"]
        ));
        let overlay = rendered_text_at(80, 24, &model);
        assert!(overlay.contains("Follow global default"));
        assert!(overlay.contains("alpha"));
        assert!(overlay.contains("beta"));
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);

        assert_eq!(model.settings_dirty_count(), 1);
        assert_eq!(store.sessions.get(EDITABLE_AGENT_KEY), Some(&record));
        let request = match model.handle(SelectorEvent::Insert('A')) {
            SelectorControl::ApplySettings(request) => request,
            control => panic!("expected apply request, got {control:?}"),
        };
        let updated = apply_session_settings_to_store(&mut store, &request, &profile_names)
            .expect("apply profile");

        assert_eq!(updated.profile.as_deref(), Some("beta"));
        assert_ne!(updated.updated_at, record.updated_at);
        assert_eq!(store.sessions.get("cutex.untouched"), Some(&untouched));
        model.settings_apply_succeeded(
            EDITABLE_AGENT_KEY,
            &updated,
            &profile_names,
            request.changed_count,
            false,
            None,
        );
        assert_eq!(model.settings_dirty_count(), 0);
        assert_eq!(model.notice.as_deref(), Some("Saved 1 setting(s)"));
        assert_eq!(
            model
                .active_row()
                .and_then(|row| row.configured_profile.as_deref()),
            Some("beta")
        );
        assert_eq!(
            model
                .active_setting_option()
                .map(|option| option.value.as_str()),
            Some("beta")
        );
        // The same local TUI apply path must fence a cross-family selection
        // before any durable fields in the draft are written.
        store.sessions.get_mut(EDITABLE_AGENT_KEY).unwrap().provider_family = Some("deepseek".into());
        let before = store.sessions[EDITABLE_AGENT_KEY].clone();
        let error = apply_session_settings_to_store(&mut store, &request, &profile_names).unwrap_err();
        assert!(error.to_string().contains("provider_family_mismatch"), "{error:#}");
        assert_eq!(store.sessions[EDITABLE_AGENT_KEY], before);

    }

    #[test]
    fn routing_tag_editor_stays_open_on_invalid_groups() {
        let mut record = editable_record();
        record.agent_groups = vec!["cutex".to_string()];
        let mut model = editable_model(&record);
        model.handle(SelectorEvent::OpenSettings);
        for _ in 0..11 {
            model.handle(SelectorEvent::Down);
        }

        model.handle(SelectorEvent::Activate);
        assert!(matches!(
            model.settings_overlay.as_ref(),
            Some(SettingsOverlay::Groups {
                field: SettingsEditField::Session(SessionSettingsField::AgentGroups),
                inputs,
                selected: 0,
            }) if inputs.len() == 2 && inputs[0].value() == "cutex"
        ));
        let editor = rendered_text_at(80, 24, &model);
        assert!(editor.contains("Message groups"));
        assert!(editor.contains("cutex"));
        for _ in 0..5 {
            model.handle(SelectorEvent::Backspace);
        }
        model.handle(SelectorEvent::Activate);

        assert!(matches!(
            model.settings_overlay.as_ref(),
            Some(SettingsOverlay::Groups {
                field: SettingsEditField::Session(SessionSettingsField::AgentGroups),
                inputs,
                ..
            }) if inputs[0].value().is_empty()
        ));
        assert_eq!(model.settings_dirty_count(), 0);
        assert!(model
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("At least one")));
    }

    #[test]
    fn message_groups_editor_uses_one_editable_line_per_group() {
        let mut record = editable_record();
        record.agent_groups = vec!["cutex".to_string()];
        let mut model = editable_model(&record);
        model.handle(SelectorEvent::OpenSettings);
        for _ in 0..11 {
            model.handle(SelectorEvent::Down);
        }
        model.handle(SelectorEvent::Activate);
        model.handle(SelectorEvent::Down);
        for character in "waveline".chars() {
            model.handle(SelectorEvent::Insert(character));
        }

        assert!(matches!(
            model.settings_overlay.as_ref(),
            Some(SettingsOverlay::Groups {
                inputs,
                selected: 1,
                ..
            }) if inputs.len() == 3
                && inputs[0].value() == "cutex"
                && inputs[1].value() == "waveline"
        ));
        let rendered = rendered_text_at(80, 24, &model);
        let cutex_line = rendered
            .lines()
            .position(|line| line.contains("cutex"))
            .expect("first group line");
        let waveline_line = rendered
            .lines()
            .position(|line| line.contains("waveline"))
            .expect("second group line");
        assert!(cutex_line < waveline_line);
        model.handle(SelectorEvent::Activate);
        assert_eq!(
            model
                .active_setting_option()
                .map(|option| option.value.as_str()),
            Some("cutex, waveline")
        );
    }

    #[test]
    fn routing_apply_updates_three_typed_fields_and_only_the_selected_session() {
        let mut record = editable_record();
        record.registration_class = AgentRegistrationClass::Persistent;
        record.agent_groups = vec!["cutex".to_string()];
        record.exposed_to_backend = false;
        record.quick_action = CutexSessionQuickActionMode::Auto;
        let snapshot = SessionSettingsSnapshot::from_record(&record);
        let mut draft = SessionSettingsDraft::default();
        draft
            .stage(
                &snapshot,
                SessionSettingsField::AgentGroups,
                Some("waveline, cutex".to_string()),
            )
            .expect("stage groups");
        draft
            .stage(
                &snapshot,
                SessionSettingsField::WorkbenchVisibility,
                Some("visible".to_string()),
            )
            .expect("stage visibility");
        draft
            .stage(
                &snapshot,
                SessionSettingsField::QuickAction,
                Some("pinned".to_string()),
            )
            .expect("stage quick action");
        let request = SessionSettingsApplyRequest {
            key: EDITABLE_AGENT_KEY.to_string(),
            draft,
            profile_names: Vec::new(),
            changed_count: 3,
        };
        let untouched = CutexSessionRecord::new_at(
            "cutex.untouched.routing".to_string(),
            None,
            "devbox".to_string(),
            "/tmp/untouched".to_string(),
            None,
            "2026-08-06T00:00:00Z".to_string(),
        )
        .expect("untouched record");
        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert(EDITABLE_AGENT_KEY.to_string(), record.clone());
        store
            .sessions
            .insert("cutex.untouched.routing".to_string(), untouched.clone());

        let updated = apply_session_settings_to_store(&mut store, &request, &[])
            .expect("apply routing settings");

        assert_eq!(updated.agent_groups, ["waveline", "cutex"]);
        assert!(updated.exposed_to_backend);
        assert_eq!(updated.quick_action, CutexSessionQuickActionMode::Pinned);
        assert_eq!(updated.permission_defaults, record.permission_defaults);
        assert_eq!(
            store.sessions.get("cutex.untouched.routing"),
            Some(&untouched)
        );
    }

    #[test]
    fn identity_and_launch_apply_updates_one_session_and_homepage_projection() {
        let mut record = editable_record();
        record.runtime_backend = CutexSessionRuntimeBackend::Host;
        record.managed_cwd = Some("/tmp/old-managed".to_string());
        record.default_cli_args = vec!["--old".to_string()];
        let snapshot = SessionSettingsSnapshot::from_record(&record);
        let mut draft = SessionSettingsDraft::default();
        draft
            .stage(
                &snapshot,
                SessionSettingsField::AgentName,
                Some("renamed-agent".to_string()),
            )
            .expect("stage name");
        draft
            .stage(
                &snapshot,
                SessionSettingsField::RuntimeBackend,
                Some("alden".to_string()),
            )
            .expect("stage backend");
        draft
            .stage(
                &snapshot,
                SessionSettingsField::ManagedCwd,
                Some("/tmp/new-managed".to_string()),
            )
            .expect("stage cwd");
        draft
            .stage(
                &snapshot,
                SessionSettingsField::ExtraCliArgs,
                Some("--model 'gpt next'".to_string()),
            )
            .expect("stage args");
        let request = SessionSettingsApplyRequest {
            key: EDITABLE_AGENT_KEY.to_string(),
            draft,
            profile_names: Vec::new(),
            changed_count: 4,
        };
        let untouched = CutexSessionRecord::new_at(
            "cutex.untouched.launch".to_string(),
            None,
            "devbox".to_string(),
            "/tmp/untouched".to_string(),
            None,
            "2026-08-06T00:00:00Z".to_string(),
        )
        .expect("untouched record");
        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert(EDITABLE_AGENT_KEY.to_string(), record.clone());
        store
            .sessions
            .insert("cutex.untouched.launch".to_string(), untouched.clone());

        let updated = apply_session_settings_to_store(&mut store, &request, &[])
            .expect("apply identity and launch");

        assert_eq!(updated.display_name_hint.as_deref(), Some("renamed-agent"));
        assert_eq!(
            updated.runtime_backend,
            CutexSessionRuntimeBackend::CuteAlden
        );
        assert_eq!(updated.managed_cwd.as_deref(), Some("/tmp/new-managed"));
        assert_eq!(updated.default_cli_args, ["--model", "gpt next"]);
        assert_eq!(updated.permission_defaults, record.permission_defaults);
        assert_eq!(
            store.sessions.get("cutex.untouched.launch"),
            Some(&untouched)
        );

        let mut model = SelectorModel::new(
            vec![
                selector_row(EDITABLE_AGENT_KEY, &record, &[], &[], &[]),
                global_row(),
            ],
            true,
            false,
        );
        model.handle(SelectorEvent::OpenSettings);
        model.settings_apply_succeeded(EDITABLE_AGENT_KEY, &updated, &[], 4, true, None);
        let row = model
            .rows
            .iter()
            .find(|row| row.target == SelectorTarget::Agent(EDITABLE_AGENT_KEY.to_string()))
            .expect("updated row");
        assert_eq!(row.agent, "renamed-agent");
        assert_eq!(row.backend, "alden");
        assert_eq!(row.actions[0].action, SessionTuiAction::ResumeAttach);

        model.replace_snapshot(SelectorSnapshot {
            rows: vec![
                selector_row(EDITABLE_AGENT_KEY, &record, &[], &[], &[]),
                global_row(),
            ],
            warning: None,
        });
        let row = model
            .rows
            .iter()
            .find(|row| row.target == SelectorTarget::Agent(EDITABLE_AGENT_KEY.to_string()))
            .expect("overridden row");
        assert_eq!(row.agent, "renamed-agent");
        assert_eq!(row.backend, "alden");
        assert_eq!(row.actions[0].action, SessionTuiAction::ResumeAttach);
    }

    #[test]
    fn failed_routing_validation_does_not_apply_a_profile_change() {
        let _home = crate::cli_app::test_home::IsolatedTestHome::new("tui-routing-profile").unwrap();
        let profiles = ["alpha", "beta"].into_iter().map(|name| {
            let mut profile = cutex::profiles::lifecycle::new_claude_account(name);
            profile.cli_kind = cutex::profiles::model::CliKind::Codex;
            profile.source = Some("official".into());
            profile
        }).collect();
        cutex::profiles::store::save_store(&cutex::profiles::model::AccountsStore { version: 3, accounts: profiles, active_account_id: None }).unwrap();
        let mut record = editable_record();
        record.profile = Some("alpha".to_string());
        let snapshot = SessionSettingsSnapshot::from_record_with_profiles(
            &record,
            &["alpha".to_string(), "beta".to_string()],
        );
        let mut draft = SessionSettingsDraft::default();
        draft
            .stage(
                &snapshot,
                SessionSettingsField::Profile,
                Some("beta".to_string()),
            )
            .expect("stage profile");
        draft
            .stage(
                &snapshot,
                SessionSettingsField::WorkbenchVisibility,
                Some("visible".to_string()),
            )
            .expect("stage invalid local visibility");
        let request = SessionSettingsApplyRequest {
            key: EDITABLE_AGENT_KEY.to_string(),
            draft,
            profile_names: vec!["alpha".to_string(), "beta".to_string()],
            changed_count: 2,
        };
        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert(EDITABLE_AGENT_KEY.to_string(), record.clone());

        let error = apply_session_settings_to_store(
            &mut store,
            &request,
            &["alpha".to_string(), "beta".to_string()],
        )
        .expect_err("local visibility must fail");

        assert!(error.to_string().contains("Adopt"));
        assert_eq!(store.sessions.get(EDITABLE_AGENT_KEY), Some(&record));
    }

    #[test]
    fn live_group_warning_only_reports_a_failed_post_save_patch() {
        assert_eq!(live_group_propagation_warning(Ok(None)), None);
        assert_eq!(
            live_group_propagation_warning(Ok(Some("runtime-id".to_string()))),
            None
        );
        assert_eq!(
            live_group_propagation_warning(Err(anyhow::anyhow!("agent bus rejected update"))),
            Some("Saved durable groups; live update failed: agent bus rejected update".to_string())
        );
    }

    #[test]
    fn quick_action_apply_updates_filtering_and_survives_a_stale_refresh() {
        let record = editable_record();
        let mut updated = record.clone();
        updated.quick_action = CutexSessionQuickActionMode::Auto;
        let mut model = SelectorModel::new(
            vec![
                selector_row(EDITABLE_AGENT_KEY, &record, &[], &[], &[]),
                global_row(),
            ],
            true,
            false,
        );
        model.handle(SelectorEvent::OpenSettings);

        model.settings_apply_succeeded(EDITABLE_AGENT_KEY, &updated, &[], 1, false, None);
        assert!(
            !model
                .rows
                .iter()
                .find(|row| row.target == SelectorTarget::Agent(EDITABLE_AGENT_KEY.to_string()))
                .expect("updated row")
                .pinned
        );

        model.replace_snapshot(SelectorSnapshot {
            rows: vec![
                selector_row(EDITABLE_AGENT_KEY, &record, &[], &[], &[]),
                global_row(),
            ],
            warning: None,
        });
        assert!(
            !model
                .rows
                .iter()
                .find(|row| row.target == SelectorTarget::Agent(EDITABLE_AGENT_KEY.to_string()))
                .expect("overridden row")
                .pinned
        );

        model.handle(SelectorEvent::Escape);
        assert_eq!(
            model.selected_target(),
            Some(SelectorTarget::Agent(EDITABLE_AGENT_KEY.into()))
        );
    }

    #[test]
    fn profile_apply_revalidates_catalog_and_keeps_stale_draft_for_retry() {
        let mut record = editable_record();
        record.profile = Some("alpha".to_string());
        let open_catalog = vec!["alpha".to_string(), "beta".to_string()];
        let current_catalog = vec!["alpha".to_string()];
        let mut model = editable_model_with_profiles(&record, &open_catalog);
        model.handle(SelectorEvent::OpenSettings);
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);
        let request = match model.handle(SelectorEvent::Insert('A')) {
            SelectorControl::ApplySettings(request) => request,
            control => panic!("expected apply request, got {control:?}"),
        };
        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert(EDITABLE_AGENT_KEY.to_string(), record.clone());

        let error = apply_session_settings_to_store(&mut store, &request, &current_catalog)
            .expect_err("removed profile must fail validation");
        model.settings_apply_failed(error.to_string());

        assert!(error
            .to_string()
            .contains("Profile is no longer configured: beta"));
        assert_eq!(store.sessions.get(EDITABLE_AGENT_KEY), Some(&record));
        assert_eq!(model.settings_dirty_count(), 1);
        assert!(model
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("no longer configured")));
    }

    #[test]
    fn profile_editor_with_an_empty_catalog_can_follow_the_global_default() {
        let mut record = editable_record();
        record.profile = Some("alpha".to_string());
        let mut model = editable_model(&record);
        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert(EDITABLE_AGENT_KEY.to_string(), record.clone());
        model.handle(SelectorEvent::OpenSettings);
        model.handle(SelectorEvent::Down);

        model.handle(SelectorEvent::Activate);

        assert!(matches!(
            model.settings_overlay.as_ref(),
            Some(SettingsOverlay::Choice {
                choices,
                selected: 0,
                custom_value: Some(value),
                ..
            }) if value == "alpha"
                && choices.iter().map(|choice| choice.label.as_str()).collect::<Vec<_>>()
                    == ["Follow global default"]
        ));
        let overlay = rendered_text_at(80, 24, &model);
        assert!(overlay.contains("Current: alpha"));
        assert!(overlay.contains("Follow global default"));

        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);

        assert_eq!(model.settings_dirty_count(), 1);
        let request = match model.handle(SelectorEvent::Insert('A')) {
            SelectorControl::ApplySettings(request) => request,
            control => panic!("expected apply request, got {control:?}"),
        };
        let updated = apply_session_settings_to_store(&mut store, &request, &[])
            .expect("clear explicit profile");
        assert_eq!(updated.profile, None);
        model.settings_apply_succeeded(
            EDITABLE_AGENT_KEY,
            &updated,
            &[],
            request.changed_count,
            false,
            None,
        );
        assert_eq!(model.settings_dirty_count(), 0);
        assert_eq!(
            model
                .active_setting_option()
                .map(|option| option.value.as_str()),
            Some("Follow global default")
        );
    }

    #[test]
    fn unknown_legacy_profile_is_visible_and_preserved_until_replaced() {
        let mut record = editable_record();
        record.profile = Some("removed-profile".to_string());
        let profile_names = vec!["alpha".to_string(), "beta".to_string()];
        let mut model = editable_model_with_profiles(&record, &profile_names);
        model.handle(SelectorEvent::OpenSettings);
        model.handle(SelectorEvent::Down);

        model.handle(SelectorEvent::Activate);

        assert!(matches!(
            model.settings_overlay.as_ref(),
            Some(SettingsOverlay::Choice {
                selected: 0,
                custom_value: Some(value),
                ..
            }) if value == "removed-profile"
        ));
        assert!(rendered_text_at(80, 24, &model).contains("Current: removed-profile"));
        model.handle(SelectorEvent::Activate);
        assert!(model.settings_overlay.is_none());
        assert_eq!(model.settings_dirty_count(), 0);
    }

    #[test]
    fn late_startup_refresh_cannot_visually_rollback_a_persisted_setting() {
        let record = editable_record();
        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert(EDITABLE_AGENT_KEY.to_string(), record.clone());
        let mut model = SelectorModel::new(
            vec![selector_row(EDITABLE_AGENT_KEY, &record, &[], &[], &[])],
            true,
            false,
        );

        stage_full_access(&mut model);
        let request = match model.handle(SelectorEvent::Insert('A')) {
            SelectorControl::ApplySettings(request) => request,
            control => panic!("expected apply request, got {control:?}"),
        };
        let updated =
            apply_session_settings_to_store(&mut store, &request, &[]).expect("apply draft");
        model.settings_apply_succeeded(
            EDITABLE_AGENT_KEY,
            &updated,
            &[],
            request.changed_count,
            false,
            None,
        );

        model.replace_snapshot(SelectorSnapshot {
            rows: vec![selector_row(EDITABLE_AGENT_KEY, &record, &[], &[], &[])],
            warning: None,
        });

        assert!(!model.refreshing);
        assert!(model.pending_settings_refresh_override.is_none());
        assert_eq!(
            model
                .active_setting_option()
                .map(|option| option.value.as_str()),
            Some("full-access")
        );
    }

    #[test]
    fn explicit_discard_restores_the_snapshot_without_an_apply_request() {
        let record = editable_record();
        let mut model = editable_model(&record);

        stage_full_access(&mut model);
        assert_eq!(
            model.handle(SelectorEvent::Insert('D')),
            SelectorControl::Continue
        );

        assert_eq!(model.settings_dirty_count(), 0);
        assert_eq!(model.notice.as_deref(), Some("Draft discarded"));
        assert_eq!(
            model
                .active_setting_option()
                .map(|option| option.value.as_str()),
            Some("workspace")
        );
        assert_eq!(
            model.handle(SelectorEvent::Insert('A')),
            SelectorControl::Continue
        );
    }

    #[test]
    fn dirty_settings_default_to_keep_editing_and_require_confirmed_discard() {
        let record = editable_record();
        let mut model = editable_model(&record);

        stage_full_access(&mut model);
        model.handle(SelectorEvent::Insert('v'));
        let categorized = rendered_text_at(120, 24, &model);
        assert!(categorized.contains("1 pending"));
        assert!(categorized.contains("Permission preset *"));
        assert!(categorized.contains("full-access"));
        assert!(categorized.contains("S save"));

        model.handle(SelectorEvent::Escape);
        assert!(matches!(
            model.settings_overlay.as_ref(),
            Some(SettingsOverlay::ConfirmDiscard { selected: 0 })
        ));
        let confirmation = rendered_text_at(120, 24, &model);
        assert!(confirmation.contains("Unsaved changes"));
        assert!(confirmation.contains("Keep editing"));
        assert!(confirmation.contains("Discard and leave"));

        model.handle(SelectorEvent::Activate);
        assert!(model.settings_overlay.is_none());
        assert_eq!(model.settings_dirty_count(), 1);
        assert!(matches!(model.mode, SelectorMode::Settings { .. }));

        model.handle(SelectorEvent::Escape);
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);
        assert_eq!(model.mode, SelectorMode::Agents);
        assert_eq!(model.settings_dirty_count(), 0);
    }

    #[test]
    fn model_text_overlay_keeps_command_characters_as_text_until_staged() {
        let record = editable_record();
        let mut model = editable_model(&record);
        model.handle(SelectorEvent::OpenSettings);
        for _ in 0..5 {
            model.handle(SelectorEvent::Down);
        }

        model.handle(SelectorEvent::Activate);
        model.handle(SelectorEvent::Insert('a'));
        model.handle(SelectorEvent::Insert('d'));
        model.handle(SelectorEvent::Insert('v'));
        assert!(matches!(
            model.settings_overlay.as_ref(),
            Some(SettingsOverlay::Text { input, .. }) if input.value() == "adv"
        ));
        assert_eq!(model.settings_dirty_count(), 0);

        model.handle(SelectorEvent::Activate);
        assert!(model.settings_overlay.is_none());
        assert_eq!(model.settings_dirty_count(), 1);
        assert_eq!(
            model
                .active_setting_option()
                .map(|option| option.value.as_str()),
            Some("adv")
        );
    }

    #[test]
    fn text_overlay_supports_clear_delete_and_keeps_invalid_name_open() {
        let record = editable_record();
        let mut model = editable_model(&record);
        model.handle(SelectorEvent::OpenSettings);
        model.handle(SelectorEvent::Activate);

        model.handle(SelectorEvent::ClearInput);
        model.handle(SelectorEvent::Activate);
        assert!(matches!(
            model.settings_overlay.as_ref(),
            Some(SettingsOverlay::Text {
                field: SettingsEditField::Session(SessionSettingsField::AgentName),
                input,
                ..
            }) if input.value().is_empty()
        ));
        assert!(model
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("Agent name cannot be empty")));
        assert_eq!(model.settings_dirty_count(), 0);

        for character in "abc".chars() {
            model.handle(SelectorEvent::Insert(character));
        }
        model.handle(SelectorEvent::First);
        model.handle(SelectorEvent::Delete);
        assert!(matches!(
            model.settings_overlay.as_ref(),
            Some(SettingsOverlay::Text { input, .. }) if input.value() == "bc"
        ));
        assert!(rendered_text_at(120, 24, &model).contains("Ctrl+U clear"));

        model.handle(SelectorEvent::ClearInput);
        for character in "renamed-agent".chars() {
            model.handle(SelectorEvent::Insert(character));
        }
        model.handle(SelectorEvent::Activate);
        assert!(model.settings_overlay.is_none());
        assert_eq!(model.settings_dirty_count(), 1);
        assert_eq!(
            model
                .active_setting_option()
                .map(|option| option.value.as_str()),
            Some("renamed-agent")
        );
    }

    #[test]
    fn unknown_existing_choice_is_shown_and_preserved_until_replaced() {
        let mut record = editable_record();
        record.permission_defaults = Some(":workspace".to_string());
        let mut model = editable_model(&record);
        model.handle(SelectorEvent::OpenSettings);
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Down);

        model.handle(SelectorEvent::Activate);
        assert!(matches!(
            model.settings_overlay.as_ref(),
            Some(SettingsOverlay::Choice {
                selected: 0,
                custom_value: Some(value),
                ..
            }) if value == ":workspace"
        ));
        assert!(rendered_text_at(80, 24, &model).contains("Current: :workspace"));

        model.handle(SelectorEvent::Activate);
        assert!(model.settings_overlay.is_none());
        assert_eq!(model.settings_dirty_count(), 0);
        assert_eq!(
            model
                .active_setting_option()
                .map(|option| option.value.as_str()),
            Some(":workspace")
        );
    }

    #[test]
    fn failed_apply_keeps_the_complete_draft_available_for_retry() {
        let record = editable_record();
        let mut model = editable_model(&record);
        stage_full_access(&mut model);

        let first = model.handle(SelectorEvent::Insert('A'));
        model.settings_apply_failed("test persistence failure".to_string());

        assert!(matches!(first, SelectorControl::ApplySettings(_)));
        assert_eq!(model.settings_dirty_count(), 1);
        assert!(model
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("test persistence failure")));
        assert!(rendered_text_at(50, 16, &model).contains("settings apply failed"));
        let retry = match model.handle(SelectorEvent::Insert('A')) {
            SelectorControl::ApplySettings(request) => request,
            control => panic!("expected retry request, got {control:?}"),
        };
        assert_eq!(retry.changed_count, 1);
        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert(EDITABLE_AGENT_KEY.to_string(), record);
        let updated =
            apply_session_settings_to_store(&mut store, &retry, &[]).expect("retry apply");
        model.settings_apply_succeeded(
            EDITABLE_AGENT_KEY,
            &updated,
            &[],
            retry.changed_count,
            false,
            None,
        );
        assert!(model.warning.is_none());
        assert_eq!(model.notice.as_deref(), Some("Saved 1 setting(s)"));
    }

    #[test]
    fn global_enter_opens_settings_without_dispatch() {
        let mut model = SelectorModel::new(vec![global_row()], false, false);

        selector_command(&mut model, Command::Settings);
        assert!(matches!(
            model.mode,
            SelectorMode::Settings {
                target: SelectorTarget::GlobalSettings,
                category: 0,
                option: 0,
                focus: SettingsFocus::Categories,
                view: SettingsView::Categories,
            }
        ));
        model.handle(SelectorEvent::Up);
        assert_eq!(
            model.selected_setting_category_index(),
            Some(model.active_row().expect("global row").settings.len() - 1)
        );
        model.handle(SelectorEvent::Down);
        assert_eq!(model.selected_setting_category_index(), Some(0));
    }

    #[test]
    fn global_profile_defaults_are_catalog_choices_staged_in_one_apply() {
        let config = CodezConfig {
            default_profile: Some("alpha".to_string()),
            ..CodezConfig::default()
        };
        let profile_names = vec!["alpha".to_string(), "beta".to_string()];
        let mut model = SelectorModel::new(
            vec![global_settings_row_with_profiles(&config, &profile_names)],
            false,
            false,
        );

        select_global_setting(&mut model, GlobalSettingsField::DefaultProfile);
        model.handle(SelectorEvent::Activate);
        assert!(matches!(
            model.settings_overlay.as_ref(),
            Some(SettingsOverlay::Choice {
                selected: 1,
                custom_value: None,
                ..
            })
        ));
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);
        assert_eq!(model.settings_dirty_count(), 1);

        select_global_setting(&mut model, GlobalSettingsField::DefaultProfileDirectLaunch);
        model.handle(SelectorEvent::Activate);
        model.handle(SelectorEvent::Up);
        model.handle(SelectorEvent::Activate);
        assert_eq!(model.settings_dirty_count(), 2);

        let request = match model.handle(SelectorEvent::Insert('A')) {
            SelectorControl::ApplyGlobalSettings(request) => request,
            control => panic!("expected global apply request, got {control:?}"),
        };
        assert_eq!(request.profile_names, profile_names);
        request
            .draft
            .validate_profile_catalog(&request.profile_names)
            .expect("fresh profile catalog");
        let mut updated = config.clone();
        assert!(apply_global_settings_to_config(&mut updated, &request).expect("apply defaults"));
        assert_eq!(updated.default_profile.as_deref(), Some("beta"));
        assert!(updated.default_profile_direct_launch);

        model.global_settings_apply_succeeded(
            &updated,
            &request.profile_names,
            request.changed_count,
        );
        assert_eq!(model.settings_dirty_count(), 0);
        assert_eq!(model.notice.as_deref(), Some("Saved 2 setting(s)"));
    }

    #[test]
    fn profile_default_editor_saves_the_shared_global_defaults() {
        let config = CodezConfig {
            default_profile: Some("alpha".to_string()),
            ..CodezConfig::default()
        };
        let profile_names = vec!["alpha".to_string(), "beta".to_string()];
        let mut model = SelectorModel::new(
            vec![global_settings_row_with_profiles(&config, &profile_names)],
            false,
            false,
        );
        open_profiles(
            &mut model,
            vec![
                profile_catalog_entry("alpha", true),
                profile_catalog_entry("beta", false),
            ],
        );

        model.handle(SelectorEvent::Activate);
        assert_eq!(
            model.profile_workspace_focus(),
            Some(ProfileWorkspaceFocus::Editor)
        );
        let narrow = rendered_text_at(50, 18, &model);
        assert!(narrow.contains("Default profile"));
        assert!(narrow.contains("Direct default launch"));
        let medium = rendered_text_at(80, 24, &model);
        assert!(medium.contains("Direct default launch"));
        model.handle(SelectorEvent::Activate);
        assert!(matches!(
            model.settings_overlay.as_ref(),
            Some(SettingsOverlay::Choice {
                selected: 1,
                custom_value: None,
                ..
            })
        ));
        assert!(rendered_text_at(100, 24, &model).contains("Default profile"));
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);

        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);
        assert!(matches!(
            model.settings_overlay.as_ref(),
            Some(SettingsOverlay::Choice {
                selected: 1,
                custom_value: None,
                ..
            })
        ));
        assert!(rendered_text_at(100, 24, &model).contains("Direct default launch"));
        model.handle(SelectorEvent::Up);
        model.handle(SelectorEvent::Activate);
        assert_eq!(model.settings_dirty_count(), 2);

        let request = match model.handle(SelectorEvent::Insert('A')) {
            SelectorControl::ApplyGlobalSettings(request) => request,
            control => panic!("expected profile default apply request, got {control:?}"),
        };
        let mut updated = config.clone();
        assert!(apply_global_settings_to_config(&mut updated, &request).expect("apply defaults"));
        assert_eq!(updated.default_profile.as_deref(), Some("beta"));
        assert!(updated.default_profile_direct_launch);

        model.global_settings_apply_succeeded(
            &updated,
            &request.profile_names,
            request.changed_count,
        );
        assert_eq!(model.settings_dirty_count(), 0);
        assert_eq!(model.notice.as_deref(), Some("Saved 2 setting(s)"));
        for target in [SelectorTarget::Profiles, SelectorTarget::GlobalSettings] {
            let snapshot = model
                .context
                .settings
                .iter()
                .find(|row| row.target == target)
                .and_then(|row| row.global_settings_snapshot.as_ref())
                .expect("shared default snapshot");
            assert_eq!(snapshot.default_profile_name(), Some("beta"));
            assert_eq!(
                GlobalSettingsDraft::default()
                    .value(snapshot, GlobalSettingsField::DefaultProfileDirectLaunch),
                "enabled"
            );
        }
    }

    #[test]
    fn profile_manager_opens_read_only_metadata_and_wraps_selection() {
        let config = CodezConfig {
            default_profile: Some("beta".to_string()),
            ..CodezConfig::default()
        };
        let profile_names = vec!["alpha".to_string(), "beta".to_string()];
        let mut model = SelectorModel::new(
            vec![global_settings_row_with_profiles(&config, &profile_names)],
            false,
            false,
        );
        open_profiles(
            &mut model,
            vec![
                profile_catalog_entry("alpha", true),
                profile_catalog_entry("beta", false),
            ],
        );
        assert!(matches!(
            model.mode,
            SelectorMode::ProfileManager { selected: 0, .. }
        ));
        let initial = rendered_text_at(100, 24, &model);
        assert!(initial.contains("PROFILE"));
        assert!(initial.contains("Default"));
        assert!(initial.contains("Add profile"));
        model.handle(SelectorEvent::Down);
        // Since 4a8b9e2 the details pane sits beside the list only from
        // PROFILE_TWO_PANE_MIN_WIDTH (104) columns.
        let alpha = rendered_text_at(140, 48, &model);
        assert!(alpha.contains("alpha@example.test"));
        assert!(alpha.contains("'--model' 'gpt-test'"));
        assert!(alpha.contains("Usage limit"));
        assert!(alpha.contains("Current usage"));
        assert!(alpha.contains("disabled"));
        assert!(!alpha.contains("API key"));
        assert!(!alpha.contains("TOP-SECRET"));
        model.handle(SelectorEvent::Up);
        let narrow = rendered_text_at(50, 18, &model);
        assert!(narrow.contains("Default"));
        assert!(narrow.contains("alpha"));
        assert!(narrow.contains("beta"));

        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Down);
        assert_eq!(
            model
                .selected_profile()
                .map(|profile| profile.name.as_str()),
            Some("beta")
        );
        let beta = rendered_text_at(140, 24, &model);
        assert!(beta.contains("beta@example.test"));
        assert!(beta.contains("launch default"));

        model.handle(SelectorEvent::Down);
        assert!(model.selected_profile_is_add());
        model.handle(SelectorEvent::Down);
        assert!(model.selected_profile_is_default());
        model.handle(SelectorEvent::Up);
        assert!(model.selected_profile_is_add());
        model.handle(SelectorEvent::Up);
        assert_eq!(
            model
                .selected_profile()
                .map(|profile| profile.name.as_str()),
            Some("beta")
        );

        assert_eq!(
            model.handle(SelectorEvent::Escape),
            SelectorControl::Continue
        );
        assert!(matches!(model.mode, SelectorMode::Agents));
        assert_eq!(model.selected_target(), None);
    }

    #[test]
    fn durable_profile_defaults_to_an_expanded_staged_editor() {
        let config = CodezConfig {
            default_profile: Some("alpha".to_string()),
            ..CodezConfig::default()
        };
        let mut model = SelectorModel::new(
            vec![global_settings_row_with_profiles(
                &config,
                &["alpha".to_string()],
            )],
            false,
            false,
        );
        open_profiles(&mut model, vec![profile_catalog_entry("alpha", true)]);
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);

        let wide = rendered_text_at(100, 48, &model);
        assert!(wide.contains("SETTING"));
        assert!(wide.contains("Identity"));
        assert!(wide.contains("  Selected account"));
        assert!(wide.contains("yes"));
        assert!(wide.contains("Imported metadata"));
        assert!(wide.contains("alpha@example.test"));
        assert!(wide.contains("Model"));
        assert!(wide.contains("Provider"));
        assert!(wide.contains("Extra CLI args"));
        assert!(!wide.contains("Status"));

        let medium = rendered_text_at(80, 24, &model);
        assert!(medium.contains("default"));
        assert!(medium.contains("S"));
        assert!(medium.contains("Left/Tab"));

        // The Usage limit category (e4b405d) sits above Imported metadata,
        // so the narrow editor needs a few more rows to show it.
        let narrow = rendered_text_at(50, 24, &model);
        assert!(narrow.contains("SETTING"));
        assert!(narrow.contains("Selected account"));
        assert!(narrow.contains("Imported metadata"));
        assert!(!narrow.contains("PROFILE  CLI"));
    }

    #[test]
    fn profile_name_editor_stages_apply_and_discard_as_typed_operations() {
        let mut model = SelectorModel::new(
            vec![global_settings_row(&CodezConfig::default())],
            false,
            false,
        );
        open_profiles(&mut model, vec![profile_catalog_entry("alpha", true)]);
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);
        model.handle(SelectorEvent::Activate);
        assert!(matches!(
            model.settings_overlay,
            Some(SettingsOverlay::Text {
                field: SettingsEditField::Profile(ProfileSettingsField::Name),
                ..
            })
        ));
        model.handle(SelectorEvent::ClearInput);
        for character in "beta".chars() {
            model.handle(SelectorEvent::Insert(character));
        }
        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::Continue
        );
        assert_eq!(model.profile_settings_draft.dirty_count(), 1);
        assert!(rendered_text_at(80, 24, &model).contains("Name *"));

        let request = match model.handle(SelectorEvent::Insert('A')) {
            SelectorControl::ApplyProfileSettings(request) => request,
            other => panic!("expected typed profile apply request, got {other:?}"),
        };
        assert_eq!(request.profile_id, "id-alpha");
        assert_eq!(request.changed_count, 1);
        assert_eq!(request.patch.name.as_deref(), Some("beta"));

        assert_eq!(
            model.handle(SelectorEvent::Insert('D')),
            SelectorControl::Continue
        );
        assert!(!model.profile_settings_draft.is_dirty());
        assert!(model.settings_overlay.is_none());
        assert_eq!(model.notice.as_deref(), Some("Draft discarded"));
    }

    #[test]
    fn optional_profile_text_editors_start_empty_instead_of_with_display_placeholders() {
        let mut model = SelectorModel::new(
            vec![global_settings_row(&CodezConfig::default())],
            false,
            false,
        );
        let mut profile = profile_catalog_entry("alpha", true);
        profile.agent_name = None;
        open_profiles(&mut model, vec![profile]);
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);

        assert!(matches!(
            model.settings_overlay,
            Some(SettingsOverlay::Text {
                field: SettingsEditField::Profile(ProfileSettingsField::AgentName),
                ref input,
                ..
            }) if input.value().is_empty()
        ));
    }

    #[test]
    fn profile_api_key_editor_masks_replacement_and_emits_a_redacted_patch() {
        let mut model = SelectorModel::new(
            vec![global_settings_row(&CodezConfig::default())],
            false,
            false,
        );
        let mut profile = profile_catalog_entry("alpha", true);
        profile.source = Some("api-key".to_string());
        profile.api_key_configured = false;
        open_profiles(&mut model, vec![profile]);
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);
        // Usage limit / Current usage (e4b405d) and Plan label (7f9798c) sit
        // above the API key row.
        for _ in 0..8 {
            model.handle(SelectorEvent::Down);
        }
        assert_eq!(
            model
                .selected_profile_setting_option()
                .and_then(|option| option.profile_field),
            Some(ProfileSettingsField::ApiKey)
        );

        model.handle(SelectorEvent::Activate);
        assert!(matches!(
            model.settings_overlay,
            Some(SettingsOverlay::SecretAction {
                field: SettingsEditField::Profile(ProfileSettingsField::ApiKey),
                selected: 0,
            })
        ));
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);
        assert!(matches!(
            model.settings_overlay,
            Some(SettingsOverlay::Text {
                field: SettingsEditField::Profile(ProfileSettingsField::ApiKey),
                masked: true,
                ..
            })
        ));

        let test_key = "sk-test-tui-replacement";
        for character in test_key.chars() {
            model.handle(SelectorEvent::Insert(character));
        }
        let rendered = rendered_text_at(80, 24, &model);
        assert!(rendered.contains("***********************"));
        assert!(!rendered.contains(test_key));
        model.handle(SelectorEvent::Activate);
        assert!(model.settings_overlay.is_none());
        assert_eq!(model.profile_settings_draft.dirty_count(), 1);
        assert_eq!(
            model
                .selected_profile_setting_option()
                .map(|option| option.value),
            Some("(replace staged)".to_string())
        );

        let request = match model.handle(SelectorEvent::Insert('A')) {
            SelectorControl::ApplyProfileSettings(request) => request,
            other => panic!("expected profile apply request, got {other:?}"),
        };
        assert!(matches!(
            request.patch.api_key,
            super::super::profile_settings::ProfileApiKeyUpdate::Replace(ref value)
                if value == test_key
        ));
        assert!(!format!("{request:?}").contains(test_key));
    }

    #[test]
    fn compact_profile_editor_scrolls_to_and_stages_the_deepseek_preset() {
        let mut model = SelectorModel::new(
            vec![global_settings_row(&CodezConfig::default())],
            false,
            false,
        );
        let mut profile = profile_catalog_entry("alpha", true);
        profile.source = Some("api-key".to_string());
        open_profiles(&mut model, vec![profile]);
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);
        // Usage limit / Current usage (e4b405d) and Plan label (7f9798c) sit
        // above the DeepSeek preset row.
        for _ in 0..9 {
            model.handle(SelectorEvent::Down);
        }
        assert_eq!(
            model
                .selected_profile_setting_option()
                .and_then(|option| option.profile_field),
            Some(ProfileSettingsField::DeepSeekPreset)
        );
        assert!(rendered_text_at(50, 18, &model).contains("DeepSeek preset"));

        model.handle(SelectorEvent::Activate);
        assert!(matches!(
            model.settings_overlay,
            Some(SettingsOverlay::Choice {
                field: SettingsEditField::Profile(ProfileSettingsField::DeepSeekPreset),
                selected: 0,
                ..
            })
        ));
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);
        assert_eq!(model.profile_settings_draft.dirty_count(), 1);
        assert!(rendered_text_at(50, 18, &model).contains("staged"));

        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);
        assert!(matches!(
            model.settings_overlay,
            Some(SettingsOverlay::Text {
                field: SettingsEditField::Profile(ProfileSettingsField::Model),
                ref input,
                ..
            }) if input.value() == deepseek::DEEPSEEK_DEFAULT_MODEL
        ));
    }

    #[test]
    fn dirty_profile_requires_explicit_discard_before_browsing_or_actions() {
        let mut model = SelectorModel::new(
            vec![global_settings_row(&CodezConfig::default())],
            false,
            false,
        );
        open_profiles(&mut model, vec![profile_catalog_entry("alpha", true)]);
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);
        let snapshot = model.selected_profile_settings_snapshot().unwrap();
        model
            .profile_settings_draft
            .stage(
                &snapshot,
                ProfileSettingsField::AgentName,
                Some("builder".to_string()),
            )
            .unwrap();

        model.handle(SelectorEvent::OpenActions);
        assert!(model.profile_overlay.is_none());
        assert_eq!(
            model.warning.as_deref(),
            Some("Apply or discard staged profile settings before actions")
        );
        model.handle(SelectorEvent::Back);
        assert!(matches!(
            model.profile_overlay,
            Some(ProfileOverlay::ConfirmDiscardProfile {
                destination: ProfileDiscardDestination::ProfileList,
                selected: 0,
            })
        ));
        model.handle(SelectorEvent::Activate);
        assert!(model.profile_overlay.is_none());
        assert!(model.profile_settings_draft.is_dirty());
        assert_eq!(
            model.profile_workspace_focus(),
            Some(ProfileWorkspaceFocus::Editor)
        );

        model.handle(SelectorEvent::Back);
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);
        assert!(!model.profile_settings_draft.is_dirty());
        assert!(model.warning.is_none());
        assert_eq!(
            model.profile_workspace_focus(),
            Some(ProfileWorkspaceFocus::Items)
        );
    }

    #[test]
    fn failed_profile_apply_keeps_the_complete_draft_for_retry() {
        let mut model = SelectorModel::new(
            vec![global_settings_row(&CodezConfig::default())],
            false,
            false,
        );
        open_profiles(&mut model, vec![profile_catalog_entry("alpha", true)]);
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);
        let snapshot = model.selected_profile_settings_snapshot().unwrap();
        model
            .profile_settings_draft
            .stage(
                &snapshot,
                ProfileSettingsField::Runtime,
                Some("docker".to_string()),
            )
            .unwrap();
        model
            .profile_settings_draft
            .stage(
                &snapshot,
                ProfileSettingsField::DockerImage,
                Some("custom-image".to_string()),
            )
            .unwrap();

        model.profile_settings_apply_failed("profile disappeared".to_string());
        assert_eq!(model.profile_settings_draft.dirty_count(), 2);
        assert_eq!(
            model.warning.as_deref(),
            Some("profile settings apply failed: profile disappeared")
        );
        let retry = model.handle(SelectorEvent::Insert('A'));
        assert!(matches!(retry, SelectorControl::ApplyProfileSettings(_)));
    }

    #[test]
    fn profile_workspace_handles_an_empty_catalog_and_shares_the_global_default_draft() {
        let config = CodezConfig::default();
        let mut model = SelectorModel::new(vec![global_settings_row(&config)], false, false);
        open_profiles(&mut model, Vec::new());
        assert!(rendered_text_at(50, 18, &model).contains("Add profile"));
        assert!(model.selected_profile_is_default());
        assert_eq!(model.handle(SelectorEvent::Down), SelectorControl::Continue);
        assert!(model.selected_profile_is_add());
        model.handle(SelectorEvent::Down);
        assert!(model.selected_profile_is_default());

        let snapshot = model
            .active_global_settings_snapshot()
            .expect("shared Global snapshot")
            .clone();
        model
            .global_settings_draft
            .stage(
                &snapshot,
                GlobalSettingsField::DefaultProfileDirectLaunch,
                Some("enabled".to_string()),
            )
            .expect("stage shared default");
        // Since 4a8b9e2 the Defaults editor sits beside the list only from
        // PROFILE_TWO_PANE_MIN_WIDTH (104) columns.
        let rendered = rendered_text_at(120, 24, &model);
        assert!(rendered.contains("enabled"));
        assert!(rendered.contains("1 pending"));
        assert_eq!(
            model.handle(SelectorEvent::Escape),
            SelectorControl::Continue
        );
        assert!(matches!(
            model.profile_overlay,
            Some(ProfileOverlay::ConfirmDiscardProfile {
                destination: ProfileDiscardDestination::AgentList,
                selected: 0,
            })
        ));
    }

    #[test]
    fn add_profile_handoff_defaults_to_cancel_and_returns_to_the_manager_context() {
        let config = CodezConfig::default();
        let mut model = SelectorModel::new(vec![global_settings_row(&config)], false, false);
        open_profiles(&mut model, Vec::new());
        model.handle(SelectorEvent::Down);

        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::Continue
        );
        assert!(matches!(
            model.profile_overlay,
            Some(ProfileOverlay::ConfirmAddProfile { selected: 0 })
        ));
        let confirmation = rendered_text_at(50, 18, &model);
        assert!(confirmation.contains("Add profile"));
        assert!(confirmation.contains("shared official login"));
        assert!(confirmation.contains("Cancel"));
        assert!(confirmation.contains("Continue"));
        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::Continue
        );
        assert!(model.profile_overlay.is_none());

        model.handle(SelectorEvent::Activate);
        model.handle(SelectorEvent::Down);
        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::LoginProfile
        );

        let success = profile_login_startup(Ok(()));
        assert_eq!(success.notice.as_deref(), Some("Profile added"));
        assert!(success.warning.is_none());
        let failure = profile_login_startup(Err(anyhow::anyhow!("cancelled")));
        assert!(failure.notice.is_none());
        assert_eq!(
            failure.warning.as_deref(),
            Some("Profile login did not complete: cancelled")
        );
        model.pending_startup_warning = failure.warning.clone();
        model.warning = failure.warning.clone();
        model.replace_snapshot(SelectorSnapshot {
            rows: model.rows.clone(),
            warning: None,
        });
        assert_eq!(model.warning, failure.warning);
        assert!(model.pending_startup_warning.is_none());
    }

    #[test]
    fn profile_actions_are_explicit_and_activate_returns_a_typed_request() {
        let config = CodezConfig {
            default_profile: Some("alpha".to_string()),
            ..CodezConfig::default()
        };
        let profile_names = vec!["alpha".to_string(), "beta".to_string()];
        let mut model = SelectorModel::new(
            vec![global_settings_row_with_profiles(&config, &profile_names)],
            false,
            false,
        );
        open_profiles(
            &mut model,
            vec![
                profile_catalog_entry("alpha", true),
                profile_catalog_entry("beta", false),
            ],
        );

        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);
        assert_eq!(
            model.handle(SelectorEvent::OpenActions),
            SelectorControl::Continue
        );
        let actions = rendered_text_at(80, 24, &model);
        assert!(actions.contains("beta actions"));
        assert!(actions.contains("Make active"));
        assert!(actions.contains("Rename"));
        assert!(actions.contains("Remove"));
        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::ManageProfile(ProfileManagementRequest {
                profile_id: "id-beta".to_string(),
                profile_name: "beta".to_string(),
                command: ProfileManagementCommand::Activate,
            })
        );

        model.profile_overlay = None;
        model.handle(SelectorEvent::Back);
        model.handle(SelectorEvent::Up);
        model.handle(SelectorEvent::Activate);
        model.handle(SelectorEvent::OpenActions);
        let active_actions = rendered_text_at(80, 24, &model);
        assert!(!active_actions.contains("Make active"));
        assert!(active_actions.contains("Rename"));
    }

    #[test]
    fn profile_rename_requires_review_and_defaults_to_cancel() {
        let config = CodezConfig::default();
        let mut model = SelectorModel::new(vec![global_settings_row(&config)], false, false);
        open_profiles(&mut model, vec![profile_catalog_entry("alpha", true)]);

        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);
        model.handle(SelectorEvent::OpenActions);
        model.handle(SelectorEvent::Activate);
        assert!(matches!(
            model.profile_overlay,
            Some(ProfileOverlay::RenameInput { .. })
        ));
        assert!(rendered_text_at(80, 24, &model).contains("Ctrl+C exit"));
        model.handle(SelectorEvent::ClearInput);
        for character in "gamma".chars() {
            model.handle(SelectorEvent::Insert(character));
        }
        model.handle(SelectorEvent::Activate);
        assert!(matches!(
            model.profile_overlay,
            Some(ProfileOverlay::ConfirmRename { selected: 0, .. })
        ));
        let confirmation = rendered_text_at(80, 24, &model);
        assert!(confirmation.contains("Rename alpha to gamma?"));
        assert!(confirmation.contains("durable session references"));
        let narrow_confirmation = rendered_text_at(50, 18, &model);
        assert!(narrow_confirmation.contains("Confirm rename"));
        assert!(narrow_confirmation.contains("Cancel"));
        assert!(narrow_confirmation.contains("Rename"));
        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::Continue
        );
        assert!(model.profile_overlay.is_none());

        model.handle(SelectorEvent::OpenActions);
        model.handle(SelectorEvent::Activate);
        model.handle(SelectorEvent::ClearInput);
        for character in "gamma".chars() {
            model.handle(SelectorEvent::Insert(character));
        }
        model.handle(SelectorEvent::Activate);
        model.handle(SelectorEvent::Down);
        let request = ProfileManagementRequest {
            profile_id: "id-alpha".to_string(),
            profile_name: "alpha".to_string(),
            command: ProfileManagementCommand::Rename {
                new_name: "gamma".to_string(),
            },
        };
        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::ManageProfile(request.clone())
        );
        model.profile_management_failed(&request, "name already exists".to_string());
        assert!(matches!(
            model.profile_overlay,
            Some(ProfileOverlay::RenameInput { ref input, .. }) if input.value() == "gamma"
        ));
    }

    #[test]
    fn profile_remove_is_confirmed_and_explains_retained_files() {
        let mut model = SelectorModel::new(
            vec![global_settings_row(&CodezConfig::default())],
            false,
            false,
        );
        open_profiles(&mut model, vec![profile_catalog_entry("alpha", true)]);

        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);
        model.handle(SelectorEvent::OpenActions);
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);
        assert!(matches!(
            model.profile_overlay,
            Some(ProfileOverlay::ConfirmRemove { selected: 0, .. })
        ));
        let confirmation = rendered_text_at(80, 24, &model);
        assert!(confirmation.contains("Remove profile alpha?"));
        assert!(confirmation.contains("Materialized profile files are retained"));
        let narrow_confirmation = rendered_text_at(50, 18, &model);
        assert!(narrow_confirmation.contains("Confirm remove"));
        assert!(narrow_confirmation.contains("Materialized profile"));
        assert!(narrow_confirmation.contains("Cancel"));
        assert!(narrow_confirmation.contains("Remove"));
        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::Continue
        );
        assert!(model.profile_overlay.is_none());

        model.handle(SelectorEvent::OpenActions);
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);
        model.handle(SelectorEvent::Down);
        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::ManageProfile(ProfileManagementRequest {
                profile_id: "id-alpha".to_string(),
                profile_name: "alpha".to_string(),
                command: ProfileManagementCommand::Remove,
            })
        );
    }

    #[test]
    fn profile_mutation_refreshes_all_profile_projections_after_a_stale_snapshot() {
        let stale_config = CodezConfig {
            default_profile: Some("alpha".to_string()),
            ..CodezConfig::default()
        };
        let stale_names = vec!["alpha".to_string(), "beta".to_string()];
        let mut stale_record = editable_record();
        stale_record.profile = Some("alpha".to_string());
        let stale_row = selector_row(
            &stale_record.cutex_session_id,
            &stale_record,
            &[],
            &[],
            &stale_names,
        );
        let mut model = SelectorModel::new(
            vec![
                stale_row.clone(),
                global_settings_row_with_profiles(&stale_config, &stale_names),
            ],
            true,
            false,
        );
        open_profiles(
            &mut model,
            vec![
                profile_catalog_entry("alpha", true),
                profile_catalog_entry("beta", false),
            ],
        );

        let mut updated_record = stale_record.clone();
        updated_record.profile = Some("gamma".to_string());
        let updated_config = CodezConfig {
            default_profile: Some("gamma".to_string()),
            ..CodezConfig::default()
        };
        let mut gamma = profile_catalog_entry("gamma", true);
        gamma.id = "id-alpha".to_string();
        let projection = ProfileProjectionSnapshot {
            records: HashMap::from([(updated_record.cutex_session_id.clone(), updated_record)]),
            config: updated_config,
            profile_names: vec!["gamma".to_string(), "beta".to_string()],
        };
        model.profile_management_succeeded(ProfileManagementResult {
            profiles: vec![gamma, profile_catalog_entry("beta", false)],
            projection,
            preferred_profile_id: Some("id-alpha".to_string()),
            notice: "Renamed alpha to gamma".to_string(),
        });
        assert_eq!(
            model
                .selected_profile()
                .map(|profile| profile.name.as_str()),
            Some("gamma")
        );
        assert!(model.pending_profile_refresh_override.is_some());

        model.replace_snapshot(SelectorSnapshot {
            rows: vec![
                stale_row,
                profiles_row(&stale_config, &stale_names),
                global_settings_row_with_profiles(&stale_config, &stale_names),
            ],
            warning: None,
        });
        let agent_profile = model
            .rows
            .iter()
            .find(|row| row.target.agent_key() == Some(EDITABLE_AGENT_KEY))
            .and_then(|row| row.settings_snapshot.as_ref())
            .and_then(|snapshot| snapshot.value(SessionSettingsField::Profile));
        let agent_profile_projection = model
            .rows
            .iter()
            .find(|row| row.target.agent_key() == Some(EDITABLE_AGENT_KEY))
            .and_then(|row| row.configured_profile.as_deref());
        let global_default = model
            .global_settings_snapshot()
            .and_then(GlobalSettingsSnapshot::default_profile_name);
        assert_eq!(agent_profile, Some("gamma"));
        assert_eq!(agent_profile_projection, Some("gamma"));
        assert_eq!(global_default, Some("gamma"));
        assert_eq!(model.notice.as_deref(), Some("Renamed alpha to gamma"));
    }

    #[test]
    fn global_choice_and_text_editors_stage_then_discard_without_an_apply_request() {
        let config = CodezConfig::default();
        let mut model = SelectorModel::new(vec![global_settings_row(&config)], false, false);
        select_global_setting(&mut model, GlobalSettingsField::ProxyEnabled);

        model.handle(SelectorEvent::Activate);
        assert!(matches!(
            model.settings_overlay.as_ref(),
            Some(SettingsOverlay::Choice {
                field: SettingsEditField::Global(GlobalSettingsField::ProxyEnabled),
                selected: 1,
                custom_value: None,
                ..
            })
        ));
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);
        assert_eq!(model.settings_dirty_count(), 1);

        select_global_setting(&mut model, GlobalSettingsField::ProxyUrl);
        model.handle(SelectorEvent::Activate);
        assert!(matches!(
            model.settings_overlay.as_ref(),
            Some(SettingsOverlay::Text {
                field: SettingsEditField::Global(GlobalSettingsField::ProxyUrl),
                input,
                tags: false,
                masked: false,
            }) if input.value() == "-"
        ));
        model.handle(SelectorEvent::ClearInput);
        for character in "socks5h://127.0.0.1:7890".chars() {
            model.handle(SelectorEvent::Insert(character));
        }
        model.handle(SelectorEvent::Activate);
        assert_eq!(model.settings_dirty_count(), 2);
        assert_eq!(
            model
                .active_setting_option()
                .map(|option| option.value.as_str()),
            Some("socks5h://127.0.0.1:7890")
        );

        assert_eq!(
            model.handle(SelectorEvent::Insert('D')),
            SelectorControl::Continue
        );
        assert_eq!(model.settings_dirty_count(), 0);
        assert_eq!(model.notice.as_deref(), Some("Draft discarded"));
        assert_eq!(
            model
                .active_setting_option()
                .map(|option| option.value.as_str()),
            Some("-")
        );
    }

    #[test]
    fn global_secret_editor_never_renders_stored_or_replacement_payloads() {
        let config = CodezConfig { agent_bus_token: Some("private-bus-token".into()), ..CodezConfig::default() };
        let mut model = SelectorModel::new(vec![global_settings_row_with_profiles(&config, &[])], false, false);
        select_global_setting(&mut model, GlobalSettingsField::AgentBusToken);
        model.handle(SelectorEvent::Activate);
        assert!(!rendered_text_at(120, 30, &model).contains("private-bus-token"));
    }

    #[test]
    fn global_general_proxy_apply_updates_config_and_survives_a_stale_refresh() {
        let mut config = CodezConfig::default();
        config.notify_service_user_message_content = Some("legacy-mode".to_string());
        config.agent_bus_token = Some("preserved-secret".to_string());
        let mut model = SelectorModel::new(vec![global_settings_row(&config)], true, false);
        selector_command(&mut model, Command::Settings);
        let snapshot = model
            .active_global_settings_snapshot()
            .expect("global snapshot")
            .clone();
        for (field, value) in [
            (GlobalSettingsField::ProxyEnabled, "enabled"),
            (GlobalSettingsField::DockerSudo, "enabled"),
            (GlobalSettingsField::ProxyEnabled, "enabled"),
            (GlobalSettingsField::ProxyUrl, "socks5h://127.0.0.1:7890"),
            (GlobalSettingsField::ProxyNoProxy, "localhost"),
        ] {
            model
                .global_settings_draft
                .stage(&snapshot, field, Some(value.to_string()))
                .expect("stage global field");
        }
        model.reproject_settings(&SelectorTarget::GlobalSettings);

        let request = match model.handle(SelectorEvent::Insert('A')) {
            SelectorControl::ApplyGlobalSettings(request) => request,
            control => panic!("expected global apply request, got {control:?}"),
        };
        assert_eq!(request.changed_count, 4);
        let mut updated = config.clone();
        assert!(apply_global_settings_to_config(&mut updated, &request).expect("apply global"));
        assert!(updated.proxy.as_ref().unwrap().enabled);
        assert!(updated.docker_use_sudo);
        let proxy = updated.proxy.as_ref().expect("enabled proxy");
        assert_eq!(proxy.url.as_deref(), Some("socks5h://127.0.0.1:7890"));
        assert_eq!(proxy.no_proxy.as_deref(), Some("localhost"));
        assert_eq!(
            updated.notify_service_user_message_content.as_deref(),
            Some("legacy-mode")
        );
        assert_eq!(updated.agent_bus_token.as_deref(), Some("preserved-secret"));

        model.global_settings_apply_succeeded(
            &updated,
            &request.profile_names,
            request.changed_count,
        );
        assert_eq!(model.settings_dirty_count(), 0);
        assert_eq!(model.notice.as_deref(), Some("Saved 4 setting(s)"));
        assert!(model
            .active_row()
            .expect("updated global row")
            .settings
            .iter()
            .any(|category| category.options.iter().any(|option| {
                option.global_field == Some(GlobalSettingsField::ProxyUrl)
                    && option.value == "socks5h://127.0.0.1:7890"
            })));

        model.replace_snapshot(SelectorSnapshot {
            rows: vec![global_settings_row(&config)],
            warning: None,
        });
        assert!(model
            .active_row()
            .expect("overridden global row")
            .settings
            .iter()
            .any(|category| category.options.iter().any(|option| {
                option.global_field == Some(GlobalSettingsField::ProxyEnabled)
                    && option.value == "enabled"
            })));
    }

    #[test]
    fn failed_global_proxy_apply_keeps_the_complete_draft_for_retry() {
        let config = CodezConfig::default();
        let mut model = SelectorModel::new(vec![global_settings_row(&config)], false, false);
        selector_command(&mut model, Command::Settings);
        let snapshot = model
            .active_global_settings_snapshot()
            .expect("global snapshot")
            .clone();
        model
            .global_settings_draft
            .stage(
                &snapshot,
                GlobalSettingsField::ProxyEnabled,
                Some("enabled".to_string()),
            )
            .expect("stage enabled");
        model
            .global_settings_draft
            .stage(
                &snapshot,
                GlobalSettingsField::ProxyUrl,
                Some("ftp://127.0.0.1:21".to_string()),
            )
            .expect("stage invalid URL");
        model.reproject_settings(&SelectorTarget::GlobalSettings);
        let request = match model.handle(SelectorEvent::Insert('A')) {
            SelectorControl::ApplyGlobalSettings(request) => request,
            control => panic!("expected global apply request, got {control:?}"),
        };
        let mut unchanged = config.clone();
        let error =
            apply_global_settings_to_config(&mut unchanged, &request).expect_err("invalid scheme");
        model.global_settings_apply_failed(error.to_string());

        assert!(error.to_string().contains("Unsupported proxy scheme"));
        assert_eq!(model.settings_dirty_count(), 2);
        assert!(model
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("Unsupported proxy scheme")));
        assert_eq!(unchanged.proxy, None);
        assert!(!unchanged.docker_use_sudo);
    }

    #[test]
    fn view_key_remains_editor_text_outside_the_managed_list() {
        let mut model = SelectorModel::new(
            vec![row(
                "agent",
                "view-agent",
                CutexSessionLifecycleState::Online,
                false,
                true,
            )],
            false,
            false,
        );
        model.open_settings();
        model.settings_overlay = Some(SettingsOverlay::Text {
            field: SettingsEditField::Session(SessionSettingsField::AgentName),
            input: Input::default(),
            tags: false,
            masked: false,
        });

        let key = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE);
        assert!(!toggle_managed_thread_titles_from_key(&mut model, key));
        model.handle(selector_event_from_key(key, false).expect("text event"));

        assert!(matches!(
            &model.settings_overlay,
            Some(SettingsOverlay::Text { input, .. }) if input.value() == "v"
        ));
        assert!(!model.show_thread_titles);
    }

    #[test]
    fn escape_clears_query_before_exiting() {
        let mut model = SelectorModel::new(
            vec![row(
                "agent",
                "agent",
                CutexSessionLifecycleState::Online,
                false,
                true,
            )],
            false,
            false,
        );
        model.handle(SelectorEvent::Insert('a'));

        assert_eq!(
            model.handle(SelectorEvent::Escape),
            SelectorControl::Continue
        );
        assert!(model.query.value().is_empty());
        assert_eq!(model.handle(SelectorEvent::Escape), SelectorControl::Exit);
    }

    #[test]
    fn action_menu_opens_with_fallback_and_left_returns_to_agents() {
        let mut model = SelectorModel::new(
            vec![row(
                "agent",
                "agent",
                CutexSessionLifecycleState::Online,
                false,
                true,
            )],
            false,
            false,
        );

        assert_eq!(
            model.handle(SelectorEvent::OpenActions),
            SelectorControl::Continue
        );
        assert_eq!(
            model.mode,
            SelectorMode::Actions {
                agent_key: "agent".to_string(),
                selected: 0,
                launch_profile: None,
            }
        );

        model.handle(SelectorEvent::Up);
        assert_eq!(model.selected_action_index(), Some(3));
        model.handle(SelectorEvent::Down);
        assert_eq!(model.selected_action_index(), Some(0));
        model.handle(SelectorEvent::Down);
        assert_eq!(model.selected_action_index(), Some(1));
        model.handle(SelectorEvent::Back);
        assert_eq!(model.mode, SelectorMode::Agents);
    }

    #[test]
    fn primary_enter_returns_a_typed_intent() {
        let mut model = SelectorModel::new(
            vec![row(
                "agent",
                "agent",
                CutexSessionLifecycleState::Online,
                false,
                true,
            )],
            false,
            false,
        );

        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::Selected(SessionTuiIntent {
                key: "agent".to_string(),
                action: SessionTuiAction::ResumeAttach,
                launch_profile: None,
                stock_runtime: None,
                runtime_package: None,
            })
        );
    }

    #[test]
    fn offline_primary_enter_reviews_start_and_attach_before_dispatch() {
        let mut model = SelectorModel::new(
            vec![row(
                "agent",
                "Stable Agent Name",
                CutexSessionLifecycleState::Offline,
                true,
                true,
            )],
            false,
            false,
        );

        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::Continue
        );
        assert!(matches!(
            model.mode,
            SelectorMode::ConfirmRuntimeAction {
                action: SessionTuiAction::ResumeAttach,
                confirmed: false,
                ..
            }
        ));
        let review = rendered_text_at(80, 18, &model);
        assert!(review.contains("Start & attach?"));
        assert!(review.contains("Stable Agent Name"));

        assert_eq!(
            selector_navigation_control_from_key(
                &mut model,
                KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            ),
            Some(SelectorControl::Continue)
        );
        assert!(matches!(
            model.mode,
            SelectorMode::ConfirmRuntimeAction {
                confirmed: true,
                ..
            }
        ));
        assert_eq!(
            selector_navigation_control_from_key(
                &mut model,
                KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            ),
            Some(SelectorControl::Continue)
        );
        assert!(matches!(
            model.mode,
            SelectorMode::ConfirmRuntimeAction {
                confirmed: false,
                ..
            }
        ));
        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::Continue
        );
        assert_eq!(model.mode, SelectorMode::Agents);

        model.handle(SelectorEvent::Activate);
        model.handle(SelectorEvent::Down);
        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::Selected(SessionTuiIntent {
                key: "agent".to_string(),
                action: SessionTuiAction::ResumeAttach,
                launch_profile: None,
                stock_runtime: None,
                runtime_package: None,
            })
        );
    }

    #[test]
    fn reviewed_stock_start_confirmation_carries_exact_review_to_dispatch() {
        let key = "cutex.stock";
        let mut stock = row(
            key,
            "Stock Agent",
            CutexSessionLifecycleState::Offline,
            true,
            true,
        );
        stock.actions = vec![SessionTuiActionItem {
            action: SessionTuiAction::StockStart,
            detail: "Review and start exact stock owner",
            primary: true,
        }];
        let mut model = SelectorModel::new(vec![stock], false, false);
        let reviewed = super::super::stock_lifecycle::ReviewedStockRuntimeAction {
            action_id: cutex::agent_management::AgentActionId::new("tui-stock-confirm").unwrap(),
            review: serde_json::from_value(serde_json::json!({
                "digest_version": 2,
                "subject": {
                    "cutex_session_id": key,
                    "formal_name": "Stock Agent",
                    "durable_sha256": "a".repeat(64),
                    "authority_sha256": "b".repeat(64),
                    "current_project_id": null,
                    "revision": 4,
                    "runtime_generation": 7
                },
                "contract": {
                    "version": 3,
                    "migration_action_id": "migration-test",
                    "native_id": "018e0000-0000-7000-8000-000000000001",
                    "native_home": "/private/stock-home",
                    "bundle_manifest": "/private/stock-home/bundle.json",
                    "bundle_sha256": "c".repeat(64)
                },
                "configuration": {
                    "profile_name": "orchid",
                    "profile_id": "profile",
                    "inherited": false,
                    "profile_sha256": "d".repeat(64),
                    "account_sha256": "e".repeat(64),
                    "model": "gpt-test",
                    "reasoning": "high",
                    "model_provider": "test",
                    "provider": {
                        "name": "test",
                        "base_url": "http://127.0.0.1:1/v1",
                        "wire_api": "responses",
                        "requires_openai_auth": false,
                        "supports_websockets": false
                    },
                    "sandbox": "danger-full-access",
                    "approval": "never"
                },
                "restart": false
            }))
            .unwrap(),
        };
        model.mode = SelectorMode::ConfirmRuntimeAction {
            agent_key: key.to_string(),
            action: SessionTuiAction::StockStart,
            launch_profile: None,
            confirmed: false,
        };
        model.stock_runtime_confirmation = Some(reviewed.clone());

        let confirmation = rendered_text_at(100, 24, &model);
        assert!(confirmation.contains("Confirm start"));
        assert!(confirmation.contains("Generation 7"));
        assert!(confirmation.contains("profile orchid"));
        model.handle(SelectorEvent::Down);
        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::Selected(SessionTuiIntent {
                key: key.to_string(),
                action: SessionTuiAction::StockStart,
                launch_profile: None,
                stock_runtime: Some(reviewed),
                runtime_package: None,
            })
        );
    }

    #[test]
    fn stale_primary_start_opens_review_instead_of_becoming_a_noop() {
        let mut stale = row(
            "agent",
            "Stable Agent Name",
            CutexSessionLifecycleState::Stale,
            true,
            true,
        );
        stale.actions[0].primary = false;
        stale.actions[1].primary = true;
        let mut model = SelectorModel::new(vec![stale], false, false);

        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::Continue
        );
        assert!(matches!(
            model.mode,
            SelectorMode::ConfirmRuntimeAction {
                action: SessionTuiAction::Online,
                confirmed: false,
                ..
            }
        ));
        assert!(rendered_text_at(80, 18, &model)
            .contains("Start managed runtime for Stable Agent Name?"));
    }

    #[test]
    fn dispatch_failure_stays_in_the_tui_without_a_fallback_terminal() {
        let mut model = SelectorModel::new(
            vec![row(
                "agent",
                "Stable Agent Name",
                CutexSessionLifecycleState::Online,
                false,
                true,
            )],
            false,
            false,
        );

        model.dispatch_failed("agent", "exact route unavailable".to_string());

        assert_eq!(model.mode, SelectorMode::Agents);
        assert!(model.inspector_overview_focused);
        assert_eq!(
            model.selected_target(),
            Some(SelectorTarget::Agent("agent".to_string()))
        );
        let rendered = rendered_text_at(120, 18, &model);
        assert!(rendered.contains("Stable Agent Name"));
        assert!(rendered.contains("no fallback terminal was launched"));
        assert!(rendered.contains("exact route unavailable"));
    }

    #[test]
    fn horizontal_keys_move_text_cursor_without_leaving_the_editor() {
        let record = editable_record();
        let mut model = editable_model(&record);
        model.handle(SelectorEvent::OpenSettings);
        model.settings_overlay = Some(SettingsOverlay::Text {
            field: SettingsEditField::Session(SessionSettingsField::AgentName),
            input: Input::new("ab".to_string()),
            tags: false,
            masked: false,
        });

        assert_eq!(
            selector_list_panel_from_horizontal_key(
                &model,
                KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            ),
            None
        );
        assert_eq!(
            selector_navigation_control_from_key(
                &mut model,
                KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            ),
            Some(SelectorControl::Continue)
        );
        model.handle(SelectorEvent::Insert('X'));
        assert!(matches!(
            &model.settings_overlay,
            Some(SettingsOverlay::Text { input, .. }) if input.value() == "aXb"
        ));
        assert!(matches!(model.mode, SelectorMode::Settings { .. }));
    }

    #[test]
    fn direct_close_shortcut_opens_review_and_defaults_to_cancel() {
        let mut model = SelectorModel::new(
            vec![row(
                "agent",
                "agent",
                CutexSessionLifecycleState::Online,
                false,
                true,
            )],
            false,
            false,
        );

        assert_eq!(model.activate_close_shortcut(), SelectorControl::Continue);
        assert_eq!(
            model.mode,
            SelectorMode::ConfirmRuntimeAction {
                agent_key: "agent".to_string(),
                action: SessionTuiAction::CloseRuntime,
                launch_profile: None,
                confirmed: false,
            }
        );
    }

    #[test]
    fn runtime_close_progress_stays_in_selector_and_blocks_exit_input() {
        let mut model = SelectorModel::new(
            vec![row(
                "agent",
                "agent",
                CutexSessionLifecycleState::Online,
                false,
                true,
            )],
            false,
            false,
        );
        assert_eq!(model.activate_close_shortcut(), SelectorControl::Continue);
        model.handle(SelectorEvent::OpenActions);
        let intent = match model.handle(SelectorEvent::Activate) {
            SelectorControl::Selected(intent) => intent,
            control => panic!("expected close intent, got {control:?}"),
        };

        model.runtime_close_started(&intent);

        assert_eq!(
            model.mode,
            SelectorMode::ClosingRuntime {
                agent_key: "agent".to_string(),
                agent_name: "agent".to_string(),
                action: SessionTuiAction::CloseRuntime,
            }
        );
        assert_eq!(model.handle(SelectorEvent::Exit), SelectorControl::Continue);
        let rendered = rendered_text_at(80, 16, &model);
        assert!(rendered.contains("Closing runtime for agent..."));
        assert!(rendered.contains("Waiting for closed or offline status."));
        assert!(!rendered.contains("Ctrl+C exit"));
    }

    #[test]
    fn runtime_package_upgrade_progress_stays_in_selector() {
        let mut model = SelectorModel::new(
            vec![row(
                "agent",
                "agent",
                CutexSessionLifecycleState::Offline,
                false,
                true,
            )],
            false,
            false,
        );
        let intent = SessionTuiIntent {
            key: "agent".to_string(),
            action: SessionTuiAction::UpgradeRuntimePackage,
            launch_profile: None,
            stock_runtime: None,
            runtime_package: None,
        };

        model.runtime_close_started(&intent);

        assert_eq!(
            model.mode,
            SelectorMode::ClosingRuntime {
                agent_key: "agent".to_string(),
                agent_name: "agent".to_string(),
                action: SessionTuiAction::UpgradeRuntimePackage,
            }
        );
        let rendered = rendered_text_at(100, 16, &model);
        assert!(rendered.contains("Upgrading runtime package for agent..."));
        assert!(rendered.contains("Rebinding the offline agent"));
        assert!(!rendered.contains("Ctrl+C exit"));
    }

    #[test]
    fn retire_is_final_non_primary_action_and_defaults_to_cancel() {
        let mut record = editable_record();
        record.formal_agent_name = Some("editable-agent".into());
        record.registration_class = AgentRegistrationClass::Persistent;
        record.profile = Some("alpha".to_string());
        record.managed_cwd = Some("/tmp/editable-managed".to_string());
        let mut model = editable_model(&record);

        model.handle(SelectorEvent::OpenActions);
        model.handle(SelectorEvent::Last);
        model.handle(SelectorEvent::Activate);

        assert!(matches!(
            model.mode,
            SelectorMode::ConfirmRuntimeAction {
                action: SessionTuiAction::RetireSession,
                confirmed: false,
                ..
            }
        ));
        let rendered = rendered_text_at(100, 20, &model);
        assert!(rendered.contains("Archive Agent editable-agent?"));
        assert!(rendered.contains("review pending"));
        assert!(model.archive_confirmation.is_none());

        model.handle(SelectorEvent::Activate);
        assert!(matches!(model.mode, SelectorMode::Actions { .. }));
    }

    #[test]
    fn profiles_entry_in_global_settings_opens_manager_on_enter() {
        let mut model = SelectorModel::new(
            vec![global_settings_row_with_profiles(&CodezConfig::default(), &[])], false, false,
        );
        model.mode = SelectorMode::Settings {
            target: SelectorTarget::GlobalSettings,
            category: 0,
            option: 0,
            focus: SettingsFocus::Options,
            view: SettingsView::Categories,
        };
        assert_eq!(model.active_setting_option().unwrap().label, "Manage profiles");
        assert!(rendered_text_at(120, 30, &model).contains("Profiles"));
        assert_eq!(model.handle(SelectorEvent::Activate), SelectorControl::OpenProfileManager);
    }

    #[test]
    fn profile_usage_reply_updates_only_open_profiles() {
        let mut model = SelectorModel::new(vec![global_row()], false, false);
        let mut profile = profile_catalog_entry("orchid", true);
        profile.official_usage = Some(super::super::profile_usage::ProfileUsageSnapshot::Refreshing);
        open_profiles(&mut model, vec![profile]);
        let mut reply = std::collections::BTreeMap::new();
        reply.insert(
            "orchid".to_string(),
            Ok(vec![super::super::profile_usage::ProfileUsageMeter {
                label: "Codex 7d".to_string(),
                remaining_percent: 90,
                balance: None,
                reset_at: None,
            }]),
        );
        reply.insert(
            "missing".to_string(),
            Err("read failed".to_string()),
        );

        model.profile_usage_refreshed(reply);

        let SelectorMode::ProfileManager { profiles, .. } = &model.mode else {
            panic!("profile manager closed");
        };
        assert_eq!(
            profiles[0].official_usage,
            Some(super::super::profile_usage::ProfileUsageSnapshot::Meters(
                vec![super::super::profile_usage::ProfileUsageMeter {
                    label: "Codex 7d".to_string(),
                    remaining_percent: 90,
                    balance: None,
                    reset_at: None,
                }]
            ))
        );
        assert!(profiles[0].usage_refreshed_at.is_some());
        assert!(profiles[0].usage_error.is_none());
    }

    #[test]
    fn profiles_dashboard_summarizes_official_meters() {
        let mut model = SelectorModel::new(vec![global_row()], false, false);
        let mut profile = profile_catalog_entry("rowan", true);
        profile.official_usage = Some(super::super::profile_usage::ProfileUsageSnapshot::Meters(
            vec![
                super::super::profile_usage::ProfileUsageMeter {
                    label: "5h".to_string(),
                    remaining_percent: 100,
                    balance: None,
                    reset_at: None,
                },
                super::super::profile_usage::ProfileUsageMeter {
                    label: "7d".to_string(),
                    remaining_percent: 96,
                    balance: None,
                    reset_at: None,
                },
                super::super::profile_usage::ProfileUsageMeter {
                    label: "Fable 7d".to_string(),
                    remaining_percent: 93,
                    balance: None,
                    reset_at: Some(Utc::now().timestamp() + (2 * 86_400) + (3 * 3_600) + 30),
                },
            ],
        ));
        open_profiles(&mut model, vec![profile]);

        let rendered = rendered_text_at(180, 40, &model);
        assert!(rendered.contains("USAGE"));
        assert!(rendered.contains("TYPE"));
        assert!(rendered.contains("RESETS"));
        assert!(rendered.contains("Defaults"));
        assert!(rendered.contains("rowan"));
        assert!(!rendered.contains("Default ·"));
        assert!(!rendered.contains("· default"));
        assert!(
            rendered.contains("[█████████░] 93% Fable"),
            "official dashboard: {rendered}"
        );
        assert!(
            rendered.contains("GPT Pro 20x"),
            "official type: {rendered}"
        );
        assert!(
            rendered.contains("2d 3h"),
            "official reset countdown: {rendered}"
        );
    }

    #[test]
    fn claude_dashboard_usage_omits_meter_scope_label() {
        let mut model = SelectorModel::new(vec![global_row()], false, false);
        let mut profile = profile_catalog_entry("rowan", true);
        profile.cli_kind = "claude".to_string();
        profile.plan_type = None;
        profile.official_usage = Some(super::super::profile_usage::ProfileUsageSnapshot::Meters(
            vec![
                super::super::profile_usage::ProfileUsageMeter {
                    label: "5h".to_string(),
                    remaining_percent: 100,
                    balance: None,
                    reset_at: None,
                },
                super::super::profile_usage::ProfileUsageMeter {
                    label: "7d".to_string(),
                    remaining_percent: 96,
                    balance: None,
                    reset_at: None,
                },
                super::super::profile_usage::ProfileUsageMeter {
                    label: "Fable 7d".to_string(),
                    remaining_percent: 93,
                    balance: None,
                    reset_at: None,
                },
            ],
        ));
        open_profiles(&mut model, vec![profile]);

        let rendered = rendered_text_at(180, 40, &model);
        assert!(
            rendered.contains("[█████████░] 93%"),
            "Claude usage bar: {rendered}"
        );
        assert!(
            !rendered.contains("Fable"),
            "Claude dashboard should not surface the meter scope label: {rendered}"
        );
    }

    #[test]
    fn profile_dashboard_type_uses_plan_and_provider_fallbacks() {
        let mut prolite = profile_catalog_entry("quince", true);
        prolite.plan_type = Some("prolite".to_string());
        assert_eq!(profile_dashboard_type(&prolite), "GPT Pro 5x");

        let mut claude = profile_catalog_entry("rowan", true);
        claude.plan_type = None;
        claude.cli_kind = "claude".to_string();
        assert_eq!(profile_dashboard_type(&claude), "Claude Max 5x");

        let mut custom = profile_catalog_entry("custom", true);
        custom.plan_type = Some("custom plan".to_string());
        assert_eq!(profile_dashboard_type(&custom), "custom plan");

        let mut overridden = profile_catalog_entry("zai", true);
        overridden.plan_type = Some("GLM".to_string());
        overridden.plan_label = Some("GLM Pro".to_string());
        assert_eq!(profile_dashboard_type(&overridden), "GLM Pro");

        // A signed-in ChatGPT login's plan beats the last-login plan_type ...
        let mut upgraded = profile_catalog_entry("work", true);
        upgraded.plan_type = Some("plus".to_string());
        upgraded.account_plan = Some("pro".to_string());
        assert_eq!(profile_dashboard_type(&upgraded), "GPT Pro 20x");
        // ... and the owner's manual label overrides it.
        upgraded.plan_label = Some("free".to_string());
        assert_eq!(profile_dashboard_type(&upgraded), "free");
    }

    #[test]
    fn profile_dashboard_reset_uses_paired_time_units() {
        let mut profile = profile_catalog_entry("orchid", true);
        profile.options.quota.enabled = true;
        let now = Utc::now().timestamp();

        profile.quota_reset_at = Some(now + (2 * 86_400) + (3 * 3_600) + 30);
        assert_eq!(profile_dashboard_reset(&profile), "2d 3h");

        profile.quota_reset_at = Some(now + (4 * 3_600) + (12 * 60) + 30);
        assert_eq!(profile_dashboard_reset(&profile), "4h 12m");

        profile.quota_reset_at = Some(now + (45 * 60) + 30);
        assert_eq!(profile_dashboard_reset(&profile), "45m");
    }

    #[test]
    fn profile_dashboard_shows_balance_without_inventing_a_quota_bar() {
        let mut profile = profile_catalog_entry("deepseek", true);
        profile.options.quota.enabled = true;
        profile.quota_balance = Some("¥83.22".to_string());
        assert_eq!(profile_dashboard_usage(&profile, false), "¥83.22");
    }

    #[test]
    fn profiles_dashboard_keeps_a_stable_gutter_and_compact_header() {
        let config = CodezConfig {
            default_profile: Some("alpha".to_string()),
            ..CodezConfig::default()
        };
        let mut model = SelectorModel::new(
            vec![global_settings_row_with_profiles(
                &config,
                &["alpha".to_string()],
            )],
            false,
            false,
        );
        open_profiles(&mut model, vec![profile_catalog_entry("alpha", true)]);

        let before = rendered_text_at(180, 40, &model);
        let before_lines = before.lines().collect::<Vec<_>>();
        let defaults_index = before_lines
            .iter()
            .position(|line| line.contains("Defaults") && line.contains("alpha"))
            .unwrap_or_else(|| panic!("defaults line: {before}"));
        assert!(before_lines[defaults_index].contains("> Defaults"));
        let spacer_left = before_lines[defaults_index + 1]
            .split("││")
            .next()
            .unwrap_or(before_lines[defaults_index + 1]);
        assert!(
            spacer_left.chars().all(|character| character.is_whitespace() || character == '│'),
            "defaults/header spacer: {before}"
        );
        let header_index = defaults_index + 2;
        assert!(before_lines[header_index].contains("PROFILE"));
        assert!(before_lines[header_index + 1].contains("alpha"));
        let header_position = before_lines[header_index]
            .find("PROFILE")
            .expect("header position");

        assert_eq!(
            model.handle(SelectorEvent::Down),
            SelectorControl::Continue
        );
        let after = rendered_text_at(180, 40, &model);
        let after_lines = after.lines().collect::<Vec<_>>();
        assert_eq!(
            after_lines[header_index].find("PROFILE"),
            Some(header_position),
            "stable profile header gutter: {after}"
        );
        assert!(after_lines[defaults_index].contains("  Defaults"));
        assert!(after_lines[header_index + 1].contains("> alpha"));
    }

    #[test]
    fn profiles_dashboard_narrow_columns_do_not_fight() {
        let config = CodezConfig {
            default_profile: Some("alpha".to_string()),
            ..CodezConfig::default()
        };
        let mut model = SelectorModel::new(
            vec![global_settings_row_with_profiles(
                &config,
                &["alpha".to_string(), "long-profile-name".to_string()],
            )],
            false,
            false,
        );
        let alpha = profile_catalog_entry("alpha", true);
        let mut official = profile_catalog_entry("long-profile-name", false);
        official.plan_type = Some("self_serve_business_prolite".to_string());
        official.official_usage = Some(super::super::profile_usage::ProfileUsageSnapshot::Meters(
            vec![super::super::profile_usage::ProfileUsageMeter {
                label: "7d".to_string(),
                remaining_percent: 100,
                balance: None,
                reset_at: None,
            }],
        ));
        open_profiles(&mut model, vec![alpha, official]);

        let rendered = rendered_text_at(64, 40, &model);
        let lines = rendered.lines().collect::<Vec<_>>();
        let header_index = lines
            .iter()
            .position(|line| line.contains("PROFILE") && line.contains("USAGE"))
            .unwrap_or_else(|| panic!("profile header: {rendered}"));
        let header = lines[header_index];
        let char_position = |line: &str, needle: &str| {
            line.chars().enumerate().find_map(|(index, first)| {
                let mut remainder = line.chars().skip(index);
                (first == needle.chars().next().expect("needle")
                    && needle
                        .chars()
                        .all(|character| remainder.next() == Some(character)))
                .then_some(index)
            })
                .unwrap_or_else(|| panic!("missing {needle:?} in {line:?}"))
        };
        let usage_position = char_position(header, "USAGE");
        let type_position = char_position(header, "TYPE");
        assert!(usage_position < type_position);
        for line in lines.iter().take(header_index + 4).skip(header_index + 1) {
            if line.contains('[') {
                assert_eq!(
                    char_position(line, "["),
                    usage_position,
                    "stable usage column: {line:?} in {rendered}"
                );
            }
            if line.contains("GPT Pro") {
                assert_eq!(
                    char_position(line, "GPT Pro"),
                    type_position,
                    "stable type column: {line:?} in {rendered}"
                );
            }
        }
    }

    #[test]
    fn profiles_dashboard_summarizes_custom_quota_and_errors() {
        let mut model = SelectorModel::new(vec![global_row()], false, false);
        let mut profile = profile_catalog_entry("GLM", false);
        profile.plan_type = Some("GLM".to_string());
        profile.source = Some("api-key".to_string());
        profile.cli_kind = "codex".to_string();
        profile.options.quota.enabled = true;
        profile.options.quota.refresh_seconds = 300;
        profile.quota_remaining_percent = Some(73);
        profile.quota_reset_at = Some(Utc::now().timestamp() + (3 * 86_400) + 30);
        profile.quota_observed_at = Some(1_800_000_000);
        profile.quota_stale = true;
        open_profiles(&mut model, vec![profile]);

        let rendered = rendered_text_at(180, 40, &model);
        assert!(
            rendered.contains("[███████░░░] 73% Q!"),
            "custom dashboard: {rendered}"
        );
        assert!(
            rendered.contains("GLM API"),
            "custom type: {rendered}"
        );
        assert!(
            rendered.contains("3d 0h"),
            "custom reset countdown: {rendered}"
        );

        let SelectorMode::ProfileManager { profiles, .. } = &mut model.mode else {
            panic!("profile manager closed");
        };
        profiles[0].official_usage = Some(
            super::super::profile_usage::ProfileUsageSnapshot::Unavailable(
                "unavailable · endpoint returned HTTP 500".to_string(),
            ),
        );
        profiles[0].usage_error = Some("endpoint returned HTTP 500".to_string());
        assert_eq!(
            model.handle(SelectorEvent::Down),
            SelectorControl::Continue
        );
        let rendered = rendered_text_at(180, 40, &model);
        assert!(rendered.contains("error"));
        assert!(
            rendered.contains("unavailable · endpoint returned HTTP 500"),
            "official usage error detail: {rendered}"
        );
    }

    #[test]
    fn top_level_panel_navigation_leaves_clean_profile_manager() {
        let mut model = SelectorModel::new(vec![global_row()], false, false);
        open_profiles(&mut model, vec![profile_catalog_entry("orchid", true)]);
        assert!(matches!(model.mode, SelectorMode::ProfileManager { .. }));
        assert!(matches!(
            selector_command(&mut model, Command::Page(PrimaryPanel::Agents)),
            SelectorKeyRoute::Switch(PrimaryPanel::Agents)
        ));
        assert!(matches!(model.mode, SelectorMode::Agents));
    }

    #[test]
    fn profiles_horizontal_arrows_switch_top_level_panels_without_opening_profile() {
        let mut model = SelectorModel::new(vec![global_row()], false, false);
        open_profiles(&mut model, vec![profile_catalog_entry("orchid", true)]);
        assert!(matches!(
            model.mode,
            SelectorMode::ProfileManager {
                focus: ProfileWorkspaceFocus::Items,
                ..
            }
        ));
        assert_eq!(
            selector_list_panel_from_horizontal_key(
                &model,
                KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)
            ),
            Some(Some(PrimaryPanel::Jobs))
        );
        assert_eq!(
            selector_list_panel_from_horizontal_key(
                &model,
                KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)
            ),
            Some(Some(PrimaryPanel::Settings))
        );
        assert!(matches!(
            route_selector_key(&mut model, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            SelectorKeyRoute::Switch(PrimaryPanel::Settings)
        ));
        // Page switches are applied by the caller after the route returns;
        // entering the profile editor must not happen while requesting one.
        assert!(matches!(model.mode, SelectorMode::Agents));
    }

    #[test]
    fn profile_usage_meters_render_as_separate_details_rows() {
        let mut model = SelectorModel::new(vec![global_row()], false, false);
        let mut profile = profile_catalog_entry("rowan", true);
        profile.official_usage = Some(super::super::profile_usage::ProfileUsageSnapshot::Meters(
            vec![
                super::super::profile_usage::ProfileUsageMeter {
                    label: "5h".to_string(),
                    remaining_percent: 100,
                    balance: None,
                    reset_at: None,
                },
                super::super::profile_usage::ProfileUsageMeter {
                    label: "7d".to_string(),
                    remaining_percent: 96,
                    balance: None,
                    reset_at: None,
                },
                super::super::profile_usage::ProfileUsageMeter {
                    label: "Fable 7d".to_string(),
                    remaining_percent: 93,
                    balance: None,
                    reset_at: None,
                },
            ],
        ));
        open_profiles(&mut model, vec![profile]);
        model.handle(SelectorEvent::Down);

        let rendered = rendered_text_at(180, 40, &model);
        assert!(rendered.contains("5h"));
        assert!(rendered.contains("[████████████████████] 100% left"));
        assert!(rendered.contains("7d"));
        assert!(rendered.contains("[███████████████████░] 96% left"));
        assert!(rendered.contains("Fable 7d"));
        assert!(rendered.contains("[███████████████████░] 93% left"));
    }

    #[test]
    fn archive_escape_returns_to_both_origins_without_resurrecting_archive() {
        for recent in [false, true] {
            for populated in [false, true] {
                let record = editable_record();
                let mut model = editable_model(&record);
                if recent { model.activate_primary_panel(PrimaryPanel::Recent); }
                let rows = if populated { vec![retired_selector_row("retired", &record, None)] } else { vec![] };
                model.open_retired_sessions(rows);
                route_selector_key(&mut model, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                assert_eq!(matches!(model.mode, SelectorMode::RecentSessions), recent);
                if !recent { assert!(matches!(model.mode, SelectorMode::Agents)); }
                model.activate_primary_panel(PrimaryPanel::Recent);
                model.activate_primary_panel(PrimaryPanel::Agents);
                assert!(matches!(model.mode, SelectorMode::Agents));
            }
        }
    }

    #[test]
    fn archive_returns_to_recent_and_retired_identity_cannot_restore() {
        let record = editable_record();
        let mut model = editable_model(&record);
        model.activate_primary_panel(PrimaryPanel::Recent);
        let mut archived = retired_selector_row("retired", &record, None);
        archived.actions.clear();
        model.open_retired_sessions(vec![archived]);
        model.handle(SelectorEvent::Activate);
        assert!(matches!(model.mode, SelectorMode::RetiredSessions { .. }));
        assert!(model.notice.as_deref().unwrap().contains("cannot be restored"));
        model.handle(SelectorEvent::Escape);
        assert!(matches!(model.mode, SelectorMode::RecentSessions));
    }

    #[test]
    fn retired_workspace_lists_archive_columns_and_restore_defaults_to_cancel() {
        let mut record = editable_record();
        record.archive_state = cutex::session::model::CutexSessionArchiveState::Retired;
        record.retired_at = Some("2026-08-14T00:01:00Z".to_string());
        record.revision = 7;
        record.registration_class = AgentRegistrationClass::Persistent;
        let mut store = CutexSessionStore::default();
        store.sessions.insert("archive-key".to_string(), record);
        let mut model = SelectorModel::new(vec![retired_sessions_row(1)], false, false);
        model.open_retired_sessions(retired_selector_rows_from_store(&store, &HashMap::new()));

        let rendered = rendered_text_at(160, 25, &model);
        for column in ["AGENT", "PROFILE", "Managed path:", "Retired at:", "Revision:"] {
            assert!(rendered.contains(column));
        }
        assert!(rendered_text_at(80, 24, &model).contains("Archive / Retired"));
        assert!(rendered_text_at(52, 16, &model).contains("Archive / Retired"));
        model.handle(SelectorEvent::Activate);
        assert!(matches!(
            model.mode,
            SelectorMode::ConfirmRuntimeAction {
                action: SessionTuiAction::RestoreSession,
                confirmed: false,
                ..
            }
        ));
        model.handle(SelectorEvent::Activate);
        assert!(matches!(model.mode, SelectorMode::RetiredSessions { .. }));
    }

    #[test]
    fn completed_runtime_close_refreshes_in_place_and_retains_the_offline_row() {
        let mut model = SelectorModel::new(
            vec![row(
                "agent",
                "agent",
                CutexSessionLifecycleState::Online,
                false,
                true,
            )],
            false,
            false,
        );
        model.handle(SelectorEvent::Insert('a'));
        let intent = confirmed_close_intent(&mut model);
        model.runtime_close_started(&intent);
        let (sender, receiver) = mpsc::channel();
        sender
            .send(RuntimeCloseWorkerResult::Closed(SelectorSnapshot {
                rows: vec![row(
                    "agent",
                    "agent",
                    CutexSessionLifecycleState::Offline,
                    false,
                    true,
                )],
                warning: None,
            }))
            .expect("close result");
        let mut runtime_close = Some(receiver);

        assert!(receive_runtime_close(&mut model, &mut runtime_close));

        assert!(runtime_close.is_none());
        assert_eq!(model.mode, SelectorMode::Agents);
        assert_eq!(model.query.value(), "a");
        assert_eq!(
            model.selected_target(),
            Some(SelectorTarget::Agent("agent".to_string()))
        );
        assert_eq!(model.visible_rows().len(), 1);
        assert_eq!(
            model.selected_row().and_then(|row| row.lifecycle),
            Some(CutexSessionLifecycleState::Offline)
        );
        assert_eq!(model.notice.as_deref(), Some("Runtime closed: agent"));
        model.query.reset();
        model.ensure_selection();
        assert_eq!(model.visible_rows().len(), 1);
    }

    #[test]
    fn terminal_screen_is_invalidated_only_after_runtime_close_completes() {
        let mut model = SelectorModel::new(
            vec![row(
                "agent",
                "agent",
                CutexSessionLifecycleState::Online,
                false,
                true,
            )],
            false,
            false,
        );
        let (_sender, receiver) = mpsc::channel();
        let mut runtime_close = Some(receiver);

        assert!(!receive_runtime_close(&mut model, &mut runtime_close));
        assert!(runtime_close.is_some());
    }

    #[test]
    fn runtime_close_failure_returns_to_the_list_with_an_inline_error() {
        let mut model = SelectorModel::new(
            vec![row(
                "agent",
                "agent",
                CutexSessionLifecycleState::Online,
                false,
                true,
            )],
            false,
            false,
        );
        let intent = confirmed_close_intent(&mut model);
        model.runtime_close_started(&intent);
        let (sender, receiver) = mpsc::channel();
        sender
            .send(RuntimeCloseWorkerResult::Failed(
                "management timeout".to_string(),
            ))
            .expect("close failure");
        let mut runtime_close = Some(receiver);

        assert!(receive_runtime_close(&mut model, &mut runtime_close));

        assert_eq!(model.mode, SelectorMode::Agents);
        assert_eq!(
            model.selected_row().and_then(|row| row.lifecycle),
            Some(CutexSessionLifecycleState::Online)
        );
        assert!(model
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("management timeout")));
    }

    #[test]
    fn closed_runtime_with_failed_refresh_is_shown_offline_without_stale_actions() {
        let mut model = SelectorModel::new(
            vec![row(
                "agent",
                "agent",
                CutexSessionLifecycleState::Online,
                false,
                true,
            )],
            false,
            false,
        );
        let intent = confirmed_close_intent(&mut model);
        model.runtime_close_started(&intent);
        let (sender, receiver) = mpsc::channel();
        sender
            .send(RuntimeCloseWorkerResult::ClosedRefreshFailed(
                "store unavailable".to_string(),
            ))
            .expect("refresh failure");
        let mut runtime_close = Some(receiver);

        assert!(receive_runtime_close(&mut model, &mut runtime_close));

        assert!(runtime_close.is_none());
        let row = model.selected_row().expect("closed row remains selected");
        assert_eq!(row.lifecycle, Some(CutexSessionLifecycleState::Offline));
        assert!(row.actions.is_empty());
        assert_eq!(model.visible_rows().len(), 1);
        assert!(model
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("live refresh failed")));
    }

    #[test]
    fn only_close_runtime_is_dispatched_inside_the_selector() {
        let mut intent = SessionTuiIntent {
            key: "agent".to_string(),
            action: SessionTuiAction::CloseRuntime,
            launch_profile: None,
            stock_runtime: None,
            runtime_package: None,
        };
        assert!(intent_runs_in_selector(&intent));
        intent.action = SessionTuiAction::CloseAndRestart;
        assert!(!intent_runs_in_selector(&intent));
    }

    #[test]
    fn direct_close_shortcut_rejects_rows_without_a_runtime() {
        let mut model = SelectorModel::new(
            vec![row(
                "agent",
                "agent",
                CutexSessionLifecycleState::Offline,
                true,
                true,
            )],
            false,
            false,
        );

        assert_eq!(model.activate_close_shortcut(), SelectorControl::Continue);
        assert!(model
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("No runtime is available to close")));
    }

    #[test]
    fn close_requires_explicit_confirmation_and_defaults_to_cancel() {
        let mut model = SelectorModel::new(
            vec![row(
                "agent",
                "agent",
                CutexSessionLifecycleState::Online,
                false,
                true,
            )],
            false,
            false,
        );
        model.handle(SelectorEvent::OpenActions);
        model.handle(SelectorEvent::Last);
        model.handle(SelectorEvent::Activate);
        assert_eq!(
            model.mode,
            SelectorMode::ConfirmRuntimeAction {
                agent_key: "agent".to_string(),
                action: SessionTuiAction::CloseRuntime,
                launch_profile: None,
                confirmed: false,
            }
        );

        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::Continue
        );
        assert_eq!(
            model.mode,
            SelectorMode::Actions {
                agent_key: "agent".to_string(),
                selected: 3,
                launch_profile: None,
            }
        );

        model.handle(SelectorEvent::Activate);
        model.handle(SelectorEvent::OpenActions);
        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::Selected(SessionTuiIntent {
                key: "agent".to_string(),
                action: SessionTuiAction::CloseRuntime,
                launch_profile: None,
                stock_runtime: None,
                runtime_package: None,
            })
        );
    }

    #[test]
    fn close_and_restart_confirms_and_keeps_the_selected_launch_profile() {
        let mut record = editable_record();
        record.formal_agent_name = Some("editable-agent".into());
        record.profile = Some("alpha".to_string());
        record.registration_class = AgentRegistrationClass::Persistent;
        record.runtime_backend = CutexSessionRuntimeBackend::CuteAlden;
        record.alden_session_name = Some("cutex.editable.runtime".to_string());
        record.alden_pid = Some(std::process::id());
        let alden_sessions = vec![CuteAldenSession {
            pid: std::process::id(),
            name: record.alden_session_name.clone(),
        }];
        let mut model = SelectorModel::new(
            vec![selector_row(
                EDITABLE_AGENT_KEY,
                &record,
                &alden_sessions,
                &[],
                &["alpha".to_string(), "beta".to_string()],
            )],
            false,
            false,
        );

        model.handle(SelectorEvent::OpenActions);
        assert_eq!(model.selected_action_index(), Some(0));
        model.handle(SelectorEvent::Activate);
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Activate);
        assert_eq!(model.selected_launch_profile(), Some("beta"));

        let restart_index = model
            .active_row()
            .and_then(|row| row.control_index_for_action(SessionTuiAction::CloseAndRestart))
            .expect("restart action index");
        while model.selected_action_index() != Some(restart_index) {
            model.handle(SelectorEvent::Down);
        }
        model.handle(SelectorEvent::Activate);
        assert_eq!(
            model.mode,
            SelectorMode::ConfirmRuntimeAction {
                agent_key: EDITABLE_AGENT_KEY.to_string(),
                action: SessionTuiAction::CloseAndRestart,
                launch_profile: Some("beta".to_string()),
                confirmed: false,
            }
        );
        let confirmation = rendered_text_at(80, 24, &model);
        assert!(confirmation.contains("Confirm restart"));
        assert!(confirmation.contains("Close and restart runtime for editable-agent?"));
        assert!(confirmation.contains("beta (this launch only)"));

        model.handle(SelectorEvent::OpenActions);
        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::Selected(SessionTuiIntent {
                key: EDITABLE_AGENT_KEY.to_string(),
                action: SessionTuiAction::CloseAndRestart,
                launch_profile: Some("beta".to_string()),
                stock_runtime: None,
                runtime_package: None,
            })
        );
    }

    #[test]
    fn restart_menu_resolves_an_inherited_global_profile_name() {
        let mut record = editable_record();
        record.profile = None;
        record.registration_class = AgentRegistrationClass::Persistent;
        record.runtime_backend = CutexSessionRuntimeBackend::CuteAlden;
        record.alden_session_name = Some("cutex.editable.runtime".to_string());
        record.alden_pid = Some(std::process::id());
        let alden_sessions = vec![CuteAldenSession {
            pid: std::process::id(),
            name: record.alden_session_name.clone(),
        }];
        let config = CodezConfig {
            default_profile: Some("relay".to_string()),
            ..CodezConfig::default()
        };
        let mut model = SelectorModel::new(
            vec![
                selector_row(
                    EDITABLE_AGENT_KEY,
                    &record,
                    &alden_sessions,
                    &[],
                    &["relay".to_string()],
                ),
                global_settings_row(&config),
            ],
            false,
            false,
        );

        model.handle(SelectorEvent::OpenActions);
        assert!(rendered_text_at(80, 24, &model).contains("Session default: relay (global)"));
        model.handle(SelectorEvent::Activate);
        assert!(rendered_text_at(80, 24, &model).contains("Session default (global: relay)"));
    }

    #[test]
    fn launch_profile_control_stages_without_dispatch_then_enriches_action_intent() {
        let mut record = editable_record();
        record.profile = Some("alpha".to_string());
        record.registration_class = AgentRegistrationClass::Persistent;
        let mut model =
            editable_model_with_profiles(&record, &["alpha".to_string(), "beta".to_string()]);

        assert_eq!(
            model.handle(SelectorEvent::OpenActions),
            SelectorControl::Continue
        );
        assert_eq!(model.selected_action_index(), Some(0));
        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::Continue
        );
        assert!(matches!(
            model.action_overlay,
            Some(ActionOverlay::LaunchProfile { selected: 0, .. })
        ));

        model.handle(SelectorEvent::Down);
        model.handle(SelectorEvent::Down);
        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::Continue
        );
        assert!(model.action_overlay.is_none());
        assert_eq!(model.selected_launch_profile(), Some("beta"));
        assert!(rendered_text_at(80, 24, &model).contains("beta (this launch only)"));

        model.handle(SelectorEvent::Down);
        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::Continue
        );
        assert!(matches!(
            model.mode,
            SelectorMode::ConfirmRuntimeAction {
                action: SessionTuiAction::Online,
                launch_profile: Some(ref profile),
                confirmed: false,
                ..
            } if profile == "beta"
        ));
        assert!(rendered_text_at(80, 24, &model).contains("Start & attach?"));
        model.handle(SelectorEvent::Down);
        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::Selected(SessionTuiIntent {
                key: EDITABLE_AGENT_KEY.to_string(),
                action: SessionTuiAction::Online,
                launch_profile: Some("beta".to_string()),
                stock_runtime: None,
                runtime_package: None,
            })
        );
    }

    #[test]
    fn root_primary_and_live_takeover_do_not_inherit_the_restart_profile() {
        let mut offline = editable_record();
        offline.profile = Some("alpha".to_string());
        offline.registration_class = AgentRegistrationClass::Persistent;
        let mut model = editable_model_with_profiles(&offline, &["beta".to_string()]);
        assert_eq!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::Continue
        );
        assert!(matches!(
            model.mode,
            SelectorMode::ConfirmRuntimeAction {
                confirmed: false,
                ..
            }
        ));
        model.handle(SelectorEvent::Down);
        assert!(matches!(
            model.handle(SelectorEvent::Activate),
            SelectorControl::Selected(SessionTuiIntent {
                launch_profile: None,
                ..
            })
        ));

        let mut live = offline;
        live.runtime_backend = CutexSessionRuntimeBackend::CuteAlden;
        live.alden_session_name = Some("cutex.editable.runtime".to_string());
        live.alden_pid = Some(std::process::id());
        let live_session = CuteAldenSession {
            pid: std::process::id(),
            name: live.alden_session_name.clone(),
        };
        let row = selector_row(
            EDITABLE_AGENT_KEY,
            &live,
            &[live_session],
            &[],
            &["alpha".to_string(), "beta".to_string()],
        );
        assert!(row.launch_profile_control_available());
        assert!(!row.action_supports_launch_profile(SessionTuiAction::ResumeAttach));
        assert!(row.action_supports_launch_profile(SessionTuiAction::CloseAndRestart));
    }

    #[test]
    fn backspace_removes_one_unicode_grapheme() {
        let mut model = SelectorModel::new(Vec::new(), false, false);
        model.handle(SelectorEvent::Insert('e'));
        model.handle(SelectorEvent::Insert('\u{301}'));
        assert_eq!(model.query.value(), "e\u{301}");

        model.handle(SelectorEvent::Backspace);

        assert!(model.query.value().is_empty());
    }

    #[test]
    fn selection_survives_snapshot_refresh_by_durable_key() {
        let mut model = SelectorModel::new(
            vec![
                row(
                    "alpha",
                    "alpha",
                    CutexSessionLifecycleState::Online,
                    false,
                    true,
                ),
                row(
                    "beta",
                    "beta",
                    CutexSessionLifecycleState::Online,
                    false,
                    true,
                ),
            ],
            true,
            false,
        );
        model.handle(SelectorEvent::Down);
        assert_eq!(
            model.selected_target(),
            Some(SelectorTarget::Agent("beta".to_string()))
        );

        model.replace_snapshot(SelectorSnapshot {
            rows: vec![
                row(
                    "beta",
                    "beta-renamed",
                    CutexSessionLifecycleState::Online,
                    false,
                    true,
                ),
                row(
                    "alpha",
                    "alpha",
                    CutexSessionLifecycleState::Online,
                    false,
                    true,
                ),
            ],
            warning: None,
        });

        assert_eq!(
            model.selected_target(),
            Some(SelectorTarget::Agent("beta".to_string()))
        );
        assert!(!model.refreshing);
    }

    #[test]
    fn refresh_clears_inspector_focus_when_the_selected_agent_disappears() {
        let mut model = SelectorModel::new(
            vec![row(
                "agent",
                "Agent",
                CutexSessionLifecycleState::Online,
                false,
                true,
            )],
            false,
            false,
        );
        model.handle_focus_traversal(true);
        assert!(model.inspector_overview_focused);

        model.replace_snapshot(SelectorSnapshot {
            rows: vec![projects_row()],
            warning: None,
        });

        assert_eq!(model.mode, SelectorMode::Agents);
        assert!(!model.inspector_overview_focused);
        assert_eq!(model.selected_target(), None);
    }

    #[test]
    fn ui_contract_c_v04_v05_responsive_name_status_and_inspectable_path() {
        let model = SelectorModel::new(
            vec![row(
                "agent",
                "Agent Unicode 界",
                CutexSessionLifecycleState::Online,
                false,
                true,
            )],
            false,
            false,
        );
        for (width, height) in [
            (60, 18),
            (80, 24),
            (100, 30),
            (120, 36),
            (160, 48),
            (240, 50),
        ] {
            let text = rendered_text_at(width, height, &model);
            assert!(
                text.contains("NAME") && text.contains("STATUS"),
                "{width}: {text}"
            );
            assert!(text.contains("Agent Unicode"));
            assert!(text.contains("F1"));
        }
        for width in 70..=120 {
            assert!(rendered_text_at(width, 30, &model).contains("NAME"));
        }
        let mut inspected = model;
        selector_command(&mut inspected, Command::Inspect);
        assert!(rendered_text_at(80, 30, &inspected).contains("Full path:"));
    }

    #[test]
    fn activity_labels_are_compact_and_safe() {
        let now = DateTime::parse_from_rfc3339("2026-08-13T12:00:00Z")
            .expect("test timestamp")
            .with_timezone(&Utc);

        assert_eq!(format_selector_activity(None, now), "-");
        assert_eq!(
            format_selector_activity(
                Some(&SelectorActivity {
                    class: SelectorActivityClass::Output,
                    updated_at: "2026-08-13T11:59:58Z".to_string(),
                    failed: false,
                }),
                now,
            ),
            "  OUT now "
        );
        assert_eq!(
            format_selector_activity(
                Some(&SelectorActivity {
                    class: SelectorActivityClass::Command,
                    updated_at: "2026-08-13T11:59:18Z".to_string(),
                    failed: true,
                }),
                now,
            ),
            " CMD! 42s "
        );
        assert_eq!(
            format_selector_activity(
                Some(&SelectorActivity {
                    class: SelectorActivityClass::Edit,
                    updated_at: "2026-08-13T11:52:00Z".to_string(),
                    failed: false,
                }),
                now,
            ),
            " EDIT  8m "
        );
        assert_eq!(
            format_selector_activity(
                Some(&SelectorActivity {
                    class: SelectorActivityClass::Image,
                    updated_at: "2026-08-13T08:00:00Z".to_string(),
                    failed: false,
                }),
                now,
            ),
            "  IMG  4h "
        );
        assert_eq!(
            format_selector_activity(
                Some(&SelectorActivity {
                    class: SelectorActivityClass::Mcp,
                    updated_at: "2026-08-10T12:00:00Z".to_string(),
                    failed: false,
                }),
                now,
            ),
            "  MCP  3d "
        );
        assert_eq!(
            format_selector_activity(
                Some(&SelectorActivity {
                    class: SelectorActivityClass::Agent,
                    updated_at: "2026-08-01T12:00:00Z".to_string(),
                    failed: false,
                }),
                now,
            ),
            "2026-08-01"
        );
        assert_eq!(
            format_selector_activity(
                Some(&SelectorActivity {
                    class: SelectorActivityClass::Tool,
                    updated_at: "invalid".to_string(),
                    failed: false,
                }),
                now,
            ),
            "-"
        );
    }

    #[test]
    fn visual_restoration_activity_subfields_and_thresholds() {
        let now = DateTime::parse_from_rfc3339("2026-09-09T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        for class in [
            SelectorActivityClass::Output,
            SelectorActivityClass::Command,
            SelectorActivityClass::Mcp,
            SelectorActivityClass::Tool,
            SelectorActivityClass::Agent,
            SelectorActivityClass::Edit,
            SelectorActivityClass::Image,
        ] {
            for (seconds, number, unit) in [
                (5, 5, 's'),
                (27, 27, 's'),
                (59, 59, 's'),
                (60, 1, 'm'),
                (120, 2, 'm'),
                (3599, 59, 'm'),
                (3600, 1, 'h'),
                (21600, 6, 'h'),
                (25200, 7, 'h'),
                (79200, 22, 'h'),
                (86399, 23, 'h'),
                (86400, 1, 'd'),
                (604799, 6, 'd'),
            ] {
                for failed in [false, true] {
                    let value = SelectorActivity {
                        class,
                        updated_at: (now - chrono::Duration::seconds(seconds)).to_rfc3339(),
                        failed,
                    };
                    let label = format_selector_activity(Some(&value), now);
                    assert_eq!(label.len(), 10);
                    assert_eq!(&label[6..8], format!("{number:>2}"));
                    assert_eq!(label.chars().nth(8), Some(unit));
                }
            }
            for seconds in [0, 4, 604800, 9999999] {
                let value = SelectorActivity {
                    class,
                    updated_at: (now - chrono::Duration::seconds(seconds)).to_rfc3339(),
                    failed: false,
                };
                let label = format_selector_activity(Some(&value), now);
                assert_eq!(label.len(), 10);
                if seconds < 5 {
                    assert_eq!(&label[6..9], "now");
                } else {
                    assert_eq!(
                        label,
                        (now - chrono::Duration::seconds(seconds))
                            .format("%Y-%m-%d")
                            .to_string()
                    );
                }
            }
        }
    }

    #[test]
    fn homepage_action_labels_are_compact_without_changing_action_identities() {
        assert_eq!(homepage_action_label(SessionTuiAction::OpenTui), "open");
        assert_eq!(homepage_action_label(SessionTuiAction::Online), "start");
        assert_eq!(
            homepage_action_label(SessionTuiAction::AttachExisting),
            "attach"
        );
        assert_eq!(
            homepage_action_label(SessionTuiAction::TakeoverExisting),
            "takeover"
        );
        assert_eq!(
            homepage_action_label(SessionTuiAction::ResumeManaged),
            "resume"
        );
        assert_eq!(
            homepage_action_label(SessionTuiAction::CloseRuntime),
            "manage"
        );
    }

    #[test]
    fn selector_rows_join_activity_by_durable_session_id() {
        let mut record = editable_record();
        record.registration_class = AgentRegistrationClass::Persistent;
        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert("store-key".to_string(), record.clone());
        let mut initial_activity = SessionActivityState::default();
        initial_activity.revision = 1;
        initial_activity.last_output = Some(output_projection("2026-08-13T06:00:00Z"));
        let activity_states = HashMap::from([(record.cutex_session_id.clone(), initial_activity)]);

        let rows = selector_rows_from_store(
            &store,
            &[],
            &[],
            &CodezConfig::default(),
            &[],
            &activity_states,
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(
            rows[0].target,
            SelectorTarget::Agent("store-key".to_string())
        );
        assert_eq!(
            rows[0].activity.as_ref().map(|activity| activity.class),
            Some(SelectorActivityClass::Output)
        );
        assert_eq!(
            rows.iter()
                .map(|row| row.target.clone())
                .collect::<Vec<_>>(),
            vec![
                SelectorTarget::Agent("store-key".to_string()),
                SelectorTarget::RetiredSessions,
                SelectorTarget::Projects,
                SelectorTarget::Profiles,
                SelectorTarget::GlobalSettings,
            ]
        );
        assert!(rows[1..].iter().all(|row| row.activity.is_none()));

        let mut model = SelectorModel::new(rows, false, false);
        let mut refreshed_activity = SessionActivityState::default();
        refreshed_activity.revision = 2;
        refreshed_activity.last_tool_call = Some(tool_projection(
            SafeToolCallClass::CollaborationTool,
            SafeToolCallStatus::Finished,
            "2026-08-13T06:00:01Z",
        ));
        model.refresh_activity_states(&HashMap::from([(
            record.cutex_session_id.clone(),
            refreshed_activity,
        )]));
        assert_eq!(
            model.rows[0]
                .activity
                .as_ref()
                .map(|activity| activity.class),
            Some(SelectorActivityClass::Agent)
        );
        assert!(model.rows[1..].iter().all(|row| row.activity.is_none()));

        model.refresh_activity_states(&HashMap::new());
        assert!(model.rows[0].activity.is_none());
    }

    #[test]
    fn homepage_activity_uses_newest_safe_projection_with_deterministic_fallbacks() {
        let mut state = SessionActivityState::default();
        // A valid safe projection remains authoritative over legacy timestamp metadata.
        state.last_output_at = Some("2099-01-01T00:00:00Z".to_string());
        state.last_output = Some(output_projection("2026-08-13T06:00:00Z"));
        state.last_tool_call = Some(tool_projection(
            SafeToolCallClass::Command,
            SafeToolCallStatus::Failed,
            "2026-08-13T06:00:01Z",
        ));
        assert_eq!(
            selector_activity_from_state(&state),
            Some(SelectorActivity {
                class: SelectorActivityClass::Command,
                updated_at: "2026-08-13T06:00:01Z".to_string(),
                failed: true,
            })
        );

        // A valid legacy output timestamp fills the compatibility gap when the projection is
        // absent, but a newer safe tool projection still wins.
        state.last_output = None;
        state.last_output_at = Some("2026-08-13T06:00:00Z".to_string());
        state.last_tool_call = Some(tool_projection(
            SafeToolCallClass::McpTool,
            SafeToolCallStatus::Finished,
            "2026-08-13T06:00:01Z",
        ));
        assert_eq!(
            selector_activity_from_state(&state),
            Some(SelectorActivity {
                class: SelectorActivityClass::Mcp,
                updated_at: "2026-08-13T06:00:01Z".to_string(),
                failed: false,
            })
        );

        state.last_tool_call = Some(tool_projection(
            SafeToolCallClass::McpTool,
            SafeToolCallStatus::Finished,
            "2026-08-13T05:59:59Z",
        ));
        assert_eq!(
            selector_activity_from_state(&state),
            Some(SelectorActivity {
                class: SelectorActivityClass::Output,
                updated_at: "2026-08-13T06:00:00Z".to_string(),
                failed: false,
            })
        );

        state.last_tool_call = Some(tool_projection(
            SafeToolCallClass::McpTool,
            SafeToolCallStatus::Finished,
            "2026-08-13T06:00:00Z",
        ));
        assert_eq!(
            selector_activity_from_state(&state)
                .expect("stable legacy-output tie projection")
                .class,
            SelectorActivityClass::Output
        );

        // Invalid safe projection timestamps use the valid legacy projection instead.
        state.last_output = Some(output_projection("invalid"));
        state.last_tool_call = None;
        assert_eq!(
            selector_activity_from_state(&state),
            Some(SelectorActivity {
                class: SelectorActivityClass::Output,
                updated_at: "2026-08-13T06:00:00Z".to_string(),
                failed: false,
            })
        );

        state.last_output = Some(output_projection("2026-08-13T06:00:00Z"));
        state.last_tool_call = Some(tool_projection(
            SafeToolCallClass::McpTool,
            SafeToolCallStatus::Finished,
            "invalid",
        ));
        assert_eq!(
            selector_activity_from_state(&state),
            Some(SelectorActivity {
                class: SelectorActivityClass::Output,
                updated_at: "2026-08-13T06:00:00Z".to_string(),
                failed: false,
            })
        );

        state.last_tool_call = Some(tool_projection(
            SafeToolCallClass::McpTool,
            SafeToolCallStatus::Finished,
            "2026-08-13T06:00:00Z",
        ));
        assert_eq!(
            selector_activity_from_state(&state)
                .expect("stable tie projection")
                .class,
            SelectorActivityClass::Output
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn selector_runtime_version_uses_local_live_process_evidence() {
        let mut record = editable_record();
        record.host_id = cutex::platform::host::current_host_name();
        record.runtime_pid = Some(std::process::id());
        let executable = cutex::platform::process::process_executable(std::process::id()).unwrap();
        let record_version = |version: &str| {
            cutex::launch::version_registry::NativeVersionRecord {
                version: version.to_string(),
                recorded_at: "2026-09-24T00:16:00+10:00".to_string(),
            }
        };
        let mut registry = cutex::launch::version_registry::NativeVersionRegistry {
            schema: cutex::launch::version_registry::REGISTRY_SCHEMA.to_string(),
            latest_cli: record_version("0.156.1-r36"),
            latest_runtime: record_version("0.156.1-r36"),
            runtime_artifacts: BTreeMap::new(),
            runtime_paths: BTreeMap::from([(
                executable.to_str().unwrap().to_string(),
                record_version("0.156.1-r35"),
            )]),
        };
        let view_row = selector_row("cutex.editable", &record, &[], &[], &[]);
        let mut view = selector_view(&view_row, None);
        apply_runtime_version_projection(&mut view, &record, Some(&registry));
        assert_eq!(view.runtime_version.as_deref(), Some("0.156.1-r35"));
        assert_eq!(view.runtime_version_latest.as_deref(), Some("0.156.1-r36"));
        assert_eq!(
            view.runtime_version_recorded_at.as_deref(),
            Some("2026-09-24T00:16:00+10:00")
        );

        record.host_id = "remote-host".to_string();
        registry.latest_runtime = record_version("0.156.1-r37");
        apply_runtime_version_projection(&mut view, &record, Some(&registry));
        assert_eq!(view.runtime_version, None);
        assert_eq!(view.runtime_version_latest, None);
        assert_eq!(view.runtime_version_recorded_at, None);
    }

    #[test]
    fn agent_profile_column_distinguishes_configured_from_unobserved_effective() {
        let mut agent = row(
            "agent",
            "agent",
            CutexSessionLifecycleState::Online,
            false,
            true,
        );
        agent.configured_profile = None;
        let view = selector_view(&agent, Some("global-default"));
        assert_eq!(view.configured_profile, None);
        assert!(matches!(
            view.effective_profile,
            Observation::Unavailable(_)
        ));
        assert!(!format!("{view:?}").contains("Known(\"global-default\")"));
        agent.configured_profile = Some("explicit".into());
        assert_eq!(
            selector_view(&agent, Some("other"))
                .configured_profile
                .as_deref(),
            Some("explicit")
        );
    }

    #[test]
    fn action_and_confirmation_views_render_current_agent_context() {
        let mut model = SelectorModel::new(
            vec![row(
                "agent",
                "cutex-dev-v5",
                CutexSessionLifecycleState::Online,
                false,
                true,
            )],
            false,
            true,
        );
        model.handle(SelectorEvent::OpenActions);

        let actions = rendered_text(96, &model);
        assert!(actions.contains("Cutex actions"));
        assert!(actions.contains("ACTION"));
        assert!(actions.contains("DETAILS"));
        assert!(actions.contains("takeover"));
        assert!(actions.contains("close and restart"));
        assert!(actions.contains("close runtime"));

        model.handle(SelectorEvent::Last);
        model.handle(SelectorEvent::Activate);
        let confirmation = rendered_text(72, &model);
        assert!(confirmation.contains("Confirm close"));
        assert!(confirmation.contains("Close runtime for cutex-dev-v5?"));
        assert!(confirmation.contains("session and cute-codex history are kept"));
    }

    #[test]
    fn settings_browser_renders_expanded_agent_and_categorized_global_views() {
        let mut agent_model = SelectorModel::new(
            vec![row(
                "agent",
                "cutex-dev-v5",
                CutexSessionLifecycleState::Online,
                false,
                true,
            )],
            false,
            false,
        );
        agent_model.handle(SelectorEvent::OpenSettings);
        let wide = rendered_text_at(220, 24, &agent_model);
        assert!(wide.contains("Inspector"));
        assert!(wide.contains("Settings [Alt+E]"));
        assert!(wide.contains("view [Expanded] Categories"));
        let setting = wide.find("SETTING").expect("setting heading");
        let value = wide.find("VALUE").expect("value heading");
        assert!(setting < value);
        assert!(wide.contains("Identity"));
        assert!(wide.contains("  Agent name"));
        assert!(wide.contains("cutex-dev-v5"));
        assert!(wide.contains("Launch"));
        assert!(wide.contains("  Runtime backend"));
        assert!(wide.contains("V view"));
        assert!(wide.contains("Alt+A Actions"));
        assert!(wide.contains("D discard"));
        assert!(!wide.contains("Identity options"));

        agent_model.handle(SelectorEvent::Down);
        agent_model.handle(SelectorEvent::Insert('v'));
        let categorized = rendered_text_at(220, 24, &agent_model);
        assert!(categorized.contains("view Expanded [Categories]"));
        let categories = categorized.find("Categories").expect("category pane");
        let options = categorized.find("Identity options").expect("option pane");
        assert!(categories < options);
        assert!(categorized.contains("Host"));
        assert!(categorized.contains("devbox"));

        let mut global_model = SelectorModel::new(vec![global_row()], false, false);
        selector_command(&mut global_model, Command::Settings);
        global_model.handle(SelectorEvent::Down);
        global_model.handle(SelectorEvent::Down);
        let medium = rendered_text(80, &global_model);
        assert!(!medium.contains("Layout:"));
        assert!(medium.contains("Settings"));
        assert!(medium.contains("Network"));
        assert!(medium.contains("Proxy enabled"));
        assert!(medium.contains("Proxy URL"));
        assert!(medium.contains("Tab focus"));
        assert!(medium.contains("Ctrl+C exit"));

        let wide_boundary = rendered_text(96, &global_model);
        assert!(wide_boundary.contains("S save"));
        assert!(wide_boundary.contains("D discard"));
        assert!(wide_boundary.contains("Ctrl+C exit"));

        global_model.handle(SelectorEvent::OpenActions);
        global_model.handle(SelectorEvent::Down);
        global_model.handle(SelectorEvent::Activate);
        let medium_value = rendered_text(80, &global_model);
        assert!(medium_value.contains("Proxy URL"));
        global_model.handle(SelectorEvent::Escape);
        assert!(medium_value.contains("Proxy URL"));

        global_model.handle(SelectorEvent::Insert('v'));
        let unchanged = rendered_text_at(120, 24, &global_model);
        assert!(!unchanged.contains("Layout:"));
        assert!(!unchanged.contains("Current value"));
        assert!(unchanged.contains("Network"));
        assert!(!unchanged.contains("Categories"));

        let mut narrow_model = SelectorModel::new(vec![global_row()], false, false);
        selector_command(&mut narrow_model, Command::Settings);
        let narrow_categories = rendered_text(50, &narrow_model);
        assert!(!narrow_categories.contains("Categories"));
        assert!(narrow_categories.contains("Notifications"));
        assert!(narrow_categories.contains("Ctrl+C exit"));
        assert!(!narrow_categories.contains("Managed sessions"));
        narrow_model.handle(SelectorEvent::Down);
        narrow_model.handle(SelectorEvent::Down);
        narrow_model.handle(SelectorEvent::OpenActions);
        let narrow_options = rendered_text(50, &narrow_model);
        assert!(narrow_options.contains("Network"));
        assert!(narrow_options.contains("Proxy enabled"));
        narrow_model.handle(SelectorEvent::Activate);
        let narrow_value = rendered_text(50, &narrow_model);
        assert!(narrow_model.settings_overlay.is_some());
        assert!(narrow_value.contains("Proxy enabled"));

        let mut narrow_choice_model = SelectorModel::new(vec![global_row()], false, false);
        selector_command(&mut narrow_choice_model, Command::Settings);
        select_global_setting(&mut narrow_choice_model, GlobalSettingsField::ProxyEnabled);
        narrow_choice_model.handle(SelectorEvent::Activate);
        let narrow_choice = rendered_text_at(50, 24, &narrow_choice_model);
        assert!(narrow_choice.contains("Proxy enabled"));
        assert!(narrow_choice.contains("Ctrl+C exit"));

        let mut global_tail_model = SelectorModel::new(vec![global_row()], false, false);
        selector_command(&mut global_tail_model, Command::Settings);
        global_tail_model.handle(SelectorEvent::Last);
        global_tail_model.handle(SelectorEvent::OpenActions);
        global_tail_model.handle(SelectorEvent::Last);
        let global_tail = rendered_text(120, &global_tail_model);
        assert!(global_tail.contains("Hosts / Connections"));
        assert!(rendered_text_at(120, 30, &global_tail_model).contains("Manage hosts"));
        assert!(global_tail.contains("Ctrl+C exit"));
    }

    #[test]
    fn footer_shortcuts_are_styled_separately_from_descriptions() {
        let record = editable_record();
        let mut model = editable_model(&record);
        model.handle(SelectorEvent::OpenSettings);
        let width = 220;
        let height = 16;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| render_selector(frame, &model))
            .expect("render selector");
        let buffer = terminal.backend().buffer();
        let footer_y = height - 2;
        let footer = (0..width)
            .map(|x| buffer.cell((x, footer_y)).expect("footer cell").symbol())
            .collect::<String>();

        for (phrase, key, description) in [
            ("S save", "S", "save"),
            ("D discard", "D", "discard"),
            ("Ctrl+C exit", "Ctrl+C", "exit"),
        ] {
            let start = footer.find(phrase).expect("footer shortcut") as u16;
            for offset in 0..key.len() as u16 {
                let cell = buffer
                    .cell((start + offset, footer_y))
                    .expect("shortcut cell");
                assert_eq!(cell.fg, crate::cli_app::session_tui_layout::focus());
                assert!(cell.modifier.contains(Modifier::BOLD));
            }
            let description_start = start + key.len() as u16 + 1;
            let description_cell = buffer
                .cell((description_start, footer_y))
                .expect("description cell");
            assert_eq!(description_cell.symbol(), &description[..1]);
            assert_ne!(description_cell.fg, crate::cli_app::session_tui_layout::focus());
            assert!(!description_cell.modifier.contains(Modifier::BOLD));
        }
    }

    fn contract_recent_model() -> SelectorModel {
        use super::super::session_tui_recent::CatalogReply;
        let mut model = SelectorModel::new(Vec::new(), false, false);
        model.activate_primary_panel(PrimaryPanel::Recent);
        model.recent.receive(
            CatalogReply::Page {
                cursor: None,
                result: Ok(cutex::catalog::ThreadPage {
                    data: vec![cutex::catalog::CatalogThread {
                        id: "native-one".into(),
                        session_id: "tree-one".into(),
                        project_id: None,
                        parent_thread_id: None,
                        preview: "Native title".into(),
                        model_provider: "openai".into(),
                        created_at: Some(1),
                        updated_at: Some(1),
                        recency_at: Some(1),
                        cwd: Some("/work".into()),
                        name: None,
                        status: serde_json::json!({}),
                        source: serde_json::json!("cli"),
                        additional_fields: Default::default(),
                    }],
                    next_cursor: Some("next".into()),
                    backwards_cursor: None,
                }),
            },
            &CutexSessionStore::default(),
        );
        model
    }

    #[test]
    fn ui_contract_c_v06_recent_inspect_is_read_only_and_preserves_query() {
        let mut model = contract_recent_model();
        let mut managed = model.recent.visible_rows()[0].view.clone();
        managed.runtime_version = Some("0.157.1-r44".into());
        managed.runtime_version_latest = Some("0.157.1-r45".into());
        model.recent.enrich_views(&[managed]);
        model.recent.push_filter('N');
        let id = model.recent.visible_rows()[0].thread_id.clone();
        assert!(matches!(
            route_selector_key(
                &mut model,
                KeyEvent::new(KeyCode::Char('i'), KeyModifiers::ALT)
            ),
            SelectorKeyRoute::Control(None)
        ));
        assert!(model.recent_inspecting);
        assert!(model.recent.review().is_none());
        let rendered = rendered_text_at(80, 30, &model);
        assert!(rendered.contains("Session Details"));
        assert!(rendered.contains("Runtime version: 0.157.1-r44"));
        assert!(rendered.contains("Latest runtime: 0.157.1-r45"));
        route_selector_key(&mut model, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!model.recent_inspecting);
        assert_eq!(model.recent.query(), "N");
        assert_eq!(model.recent.visible_rows()[0].thread_id, id);
    }

    fn contract_key(model: &mut SelectorModel, code: KeyCode) {
        assert!(matches!(
            route_selector_key(model, KeyEvent::new(code, KeyModifiers::NONE)),
            SelectorKeyRoute::Control(None | Some(SelectorControl::Continue))
        ));
    }

    #[test]
    fn ui_contract_k01_k03_recent_input_owns_editing_and_prerouter() {
        let mut model = contract_recent_model();
        contract_key(&mut model, KeyCode::Char('/'));
        for c in "abc".chars() {
            contract_key(&mut model, KeyCode::Char(c));
        }
        contract_key(&mut model, KeyCode::Backspace);
        assert_eq!(model.recent.query(), "ab");
        contract_key(&mut model, KeyCode::Home);
        contract_key(&mut model, KeyCode::Delete);
        assert_eq!(model.recent.query(), "b");
        contract_key(&mut model, KeyCode::End);
        contract_key(&mut model, KeyCode::Char('c'));
        contract_key(&mut model, KeyCode::Left);
        assert_eq!(model.recent.filter_input().cursor(), 1);
        contract_key(&mut model, KeyCode::Char('X'));
        contract_key(&mut model, KeyCode::Right);
        assert_eq!(model.recent.query(), "bXc");
        assert_eq!(model.recent.filter_input().cursor(), 3);
        assert!(matches!(
            route_selector_key(
                &mut model,
                KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)
            ),
            SelectorKeyRoute::Control(None | Some(SelectorControl::Continue))
        ));
        assert_eq!(model.recent.query(), "");
        assert_eq!(model.query.value(), "");
        assert!(model.recent.review().is_none());
    }

    #[test]
    fn ui_contract_k02_k04_k05_k06_recent_focus_does_not_activate_or_leak() {
        let mut model = contract_recent_model();
        contract_key(&mut model, KeyCode::Char('/'));
        for c in "nNqaev /".chars() {
            contract_key(&mut model, KeyCode::Char(c));
        }
        assert_eq!(model.recent.query(), "nNqaev /");
        contract_key(&mut model, KeyCode::Enter);
        assert!(!model.recent.filter_focused());
        assert!(model.recent.review().is_none());
        for exit in [KeyCode::Tab, KeyCode::BackTab, KeyCode::Esc] {
            contract_key(&mut model, KeyCode::Char('/'));
            contract_key(&mut model, exit);
            assert_eq!(model.recent.filter_focused(),exit!=KeyCode::Esc);
            if exit!=KeyCode::Esc {contract_key(&mut model,KeyCode::Esc);}
            assert!(model.recent.review().is_none());
            assert!(matches!(model.mode, SelectorMode::RecentSessions));
        }
        assert!(matches!(
            route_selector_key(
                &mut model,
                KeyEvent::new(KeyCode::Char('1'), KeyModifiers::ALT)
            ),
            SelectorKeyRoute::Switch(PrimaryPanel::Agents)
        ));
        model.activate_primary_panel(PrimaryPanel::Agents);
        contract_key(&mut model, KeyCode::Char('z'));
        assert_eq!(model.query.value(), "z");
        assert_eq!(model.recent.query(), "nNqaev /");
        // Even stale historical focus cannot redirect Managed's handler.
        model.recent.focus_filter();
        contract_key(&mut model, KeyCode::Char('x'));
        assert_eq!(model.query.value(), "zx");
        assert_eq!(model.recent.query(), "nNqaev /");
    }

    #[test]
    fn native_close_restore_menu_confirms_exact_target_and_retains_history_row() {
        let mut model = contract_recent_model();
        selector_command(&mut model, Command::Actions);
        let id = model.native_actions.as_ref().unwrap().0.clone();
        contract_key(&mut model, KeyCode::Up);
        assert_eq!(model.native_actions.as_ref().unwrap().1, 3);
        contract_key(&mut model, KeyCode::Enter);
        assert!(model.native_actions.as_ref().unwrap().2);
        let text = rendered_text_at(100, 30, &model);
        assert!(text.contains("remain archived"));
        assert!(matches!(route_selector_key(&mut model, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), SelectorKeyRoute::Control(Some(SelectorControl::CloseNative(target))) if target == id));
        model.native_archive_pending = true;
        model.recent_catalog_reply(super::super::session_tui_recent::CatalogReply::ClosedOwned { id: id.clone(), result: Ok(()) });
        assert!(!model.native_archive_pending);
        assert!(model.recent.rows().iter().any(|row| row.thread_id == id));
    }

    #[test]
    fn native_archive_menu_requires_confirmation_and_preserves_target() {
        let mut model = contract_recent_model();
        selector_command(&mut model, Command::Actions);
        let id = model.native_actions.as_ref().unwrap().0.clone();
        contract_key(&mut model, KeyCode::Down);
        contract_key(&mut model, KeyCode::Enter);
        assert!(model.native_actions.as_ref().unwrap().2);
        contract_key(&mut model, KeyCode::Esc);
        assert!(!model.native_actions.as_ref().unwrap().2);
        contract_key(&mut model, KeyCode::Enter);
        assert!(matches!(route_selector_key(&mut model, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), SelectorKeyRoute::Control(Some(SelectorControl::ArchiveNative(target))) if target == id));
        assert!(model.native_actions.is_none());
        model.native_archive_pending = true;
        assert!(matches!(route_selector_key(&mut model, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), SelectorKeyRoute::Control(None)));
        model.recent_catalog_reply(super::super::session_tui_recent::CatalogReply::Archived { id: id.clone(), result: Ok(()) });
        assert!(!model.native_archive_pending);
        assert!(!model.recent.rows().iter().any(|row| row.thread_id == id));
    }

    #[test]
    fn adoption_has_one_confirmation_pane_in_both_focus_states() {
        let mut model = contract_recent_model();
        route_selector_key(&mut model, KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT));
        contract_key(&mut model, KeyCode::Enter);
        for focused in [true, false] {
            if !focused { model.recent.blur_adoption_name(); }
            for width in [80, 160] {
                let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 30)).unwrap();
                terminal.draw(|frame| render_selector_contents(frame, &model)).unwrap();
                let buffer = terminal.backend().buffer();
                let text = (0..30).map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n");
                assert_eq!(text.matches("Confirmation").count(), 1, "{text}");
                assert_eq!(text.matches("Enter apply").count(), 1, "{text}");
                assert!(!text.contains("[Adopt]"));
                assert!(!text.contains("[Cancel]"));
            }
        }
    }

    #[test]
    fn ui_contract_k07_k08_recent_review_horizontal_and_single_submission() {
        let mut model = contract_recent_model();
        route_selector_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT),
        );
        contract_key(&mut model, KeyCode::Enter);
        assert!(model.recent.review().is_some());
        assert!(!model.recent.review_confirmed());
        assert_eq!(model.recent.adoption_name().unwrap().value(), "");
        for ch in "Explicit name".chars() {
            contract_key(&mut model, KeyCode::Char(ch));
        }
        contract_key(&mut model, KeyCode::Tab);
        contract_key(&mut model, KeyCode::Right);
        assert!(model.recent.review_confirmed());
        contract_key(&mut model, KeyCode::Left);
        assert!(model.recent.review().is_some());
        assert!(!model.recent.review_confirmed());
        contract_key(&mut model, KeyCode::Tab);
        assert!(model.recent.review_confirmed());
        contract_key(&mut model, KeyCode::BackTab);
        assert!(!model.recent.review_confirmed());
        contract_key(&mut model, KeyCode::Enter);
        assert!(model.recent.review().is_none());
        route_selector_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT),
        );
        contract_key(&mut model, KeyCode::Enter);
        for ch in "Explicit name".chars() {
            contract_key(&mut model, KeyCode::Char(ch));
        }
        contract_key(&mut model, KeyCode::Tab);
        contract_key(&mut model, KeyCode::Right);
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            assert!(matches!(
                route_selector_key(
                    &mut model,
                    KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, kind)
                ),
                SelectorKeyRoute::Control(None)
            ));
        }
        assert!(matches!(
            route_selector_key(
                &mut model,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            SelectorKeyRoute::Control(Some(SelectorControl::AdoptRecent(_)))
        ));
        assert!(model.recent.review().is_some()); // Same action retained for uncertain-result retry.
        model.recent.cancel_review();
        route_selector_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT),
        );
        contract_key(&mut model, KeyCode::Enter); // explicit fresh review
        contract_key(&mut model, KeyCode::Esc);
        assert!(model.recent.review().is_some()); // First Esc leaves the name editor.
        contract_key(&mut model, KeyCode::Esc);
        assert!(model.recent.review().is_none());
    }

    #[test]
    fn ui_contract_d06_recent_enter_native_effect_never_adopts_and_repeat_is_ignored() {
        let mut model = contract_recent_model();
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            assert!(matches!(
                route_selector_key(
                    &mut model,
                    KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, kind)
                ),
                SelectorKeyRoute::Control(None)
            ));
        }
        assert!(
            matches!(route_selector_key(&mut model, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            SelectorKeyRoute::Control(Some(SelectorControl::NativeResume { catalog, thread, cwd })) if catalog == "paired-local-app-server" && thread == "native-one" && cwd == "/work")
        );
        assert!(model.recent.review().is_none());
        assert!(matches!(model.mode, SelectorMode::RecentSessions));
    }

    #[test]
    #[ignore = "real PTY, saved private native fixture only; no creation or model turn"]
    fn ui_contract_d20_real_native_resume_terminal_child() {
        let home = crate::cli_app::test_home::IsolatedTestHome::new("cutex-d2-native-pty").unwrap();
        let native_home =
            std::path::PathBuf::from(std::env::var("CUTEX_D2_PRIVATE_SAVED_HOME").unwrap());
        assert!(native_home
            .to_string_lossy()
            .starts_with("/tmp/cutex-d2-native-"));
        let id = std::env::var("CUTEX_D2_PRIVATE_SAVED_ID").unwrap();
        cutex::session::store::save_cutex_session_store(&CutexSessionStore::default()).unwrap();
        let durable_path = cutex::session::store::cutex_sessions_path().unwrap();
        let before = cutex::session::store::session_store_json_bytes(&durable_path).unwrap();
        let roster = AgentManagementStore::open_default().unwrap();
        let roster_before = roster.snapshot().unwrap();
        let launch = super::super::session_native_workflow::NativeLaunch {
            cwd: home.root().into(),
            native_home,
            profile: None,
            model: None,
        };
        let mut shell = TerminalShell::open().unwrap();
        println!("D2_NATIVE_READY");
        use std::io::Write;
        std::io::stdout().flush().unwrap();
        let outcome = shell.handoff(|| launch.interactive(Some(&id))).unwrap();
        println!("D2_NATIVE_RETURNED {:?}", outcome.map(|s| s.code()));
        assert_eq!(std::fs::read(durable_path).unwrap(), before);
        assert_eq!(roster.snapshot().unwrap(), roster_before);
        drop(shell);
        println!("D2_NATIVE_COOKED");
    }

    #[test]
    fn ui_contract_d19_d20_shared_member_actions_cancel_preserves_origin_selection_query() {
        let mut model = editable_model(&editable_record());
        let original = model.workspace_selection.selected().cloned();
        model.query = tui_input::Input::new("kept query".into());
        let id = model.rows[0].target.agent_key().unwrap().to_string();
        assert_eq!(
            model.open_subject_context(&id, SelectorEvent::OpenActions, PrimaryPanel::Projects),
            SelectorControl::Continue
        );
        assert!(matches!(&model.mode, SelectorMode::Actions { agent_key, .. } if agent_key == &id));
        assert!(matches!(
            route_selector_key(&mut model, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            SelectorKeyRoute::Switch(PrimaryPanel::Projects)
        ));
        assert_eq!(model.workspace_selection.selected().cloned(), original);
        assert_eq!(model.query.value(), "kept query");
        assert!(model.object_return.is_none());
    }

    #[test]
    fn ui_contract_k07_k08_retire_confirmation_and_busy_guard() {
        let mut model = SelectorModel::new(Vec::new(), false, false);
        let request: cutex::agent_management::AgentArchiveRequest = serde_json::from_value(serde_json::json!({
            "action_id": "confirmed-archive", "reason": null,
            "review": { "cutex_session_id": "cutex.exact", "formal_name": "Exact Agent", "operation": "archive", "durable_sha256": "0".repeat(64), "authority_sha256": "1".repeat(64), "current_project_id": null, "revision": 3, "runtime_generation": 2 }
        })).unwrap();
        model.archive_confirmation = Some(request.clone());
        model.mode = SelectorMode::ConfirmRuntimeAction {
            agent_key: "cutex.exact".into(),
            action: SessionTuiAction::RetireSession,
            launch_profile: None,
            confirmed: false,
        };
        contract_key(&mut model, KeyCode::Right);
        contract_key(&mut model, KeyCode::Left);
        assert!(matches!(
            model.mode,
            SelectorMode::ConfirmRuntimeAction {
                confirmed: false,
                ..
            }
        ));
        contract_key(&mut model, KeyCode::Tab);
        contract_key(&mut model, KeyCode::BackTab);
        assert!(matches!(
            model.mode,
            SelectorMode::ConfirmRuntimeAction {
                confirmed: false,
                ..
            }
        ));
        contract_key(&mut model, KeyCode::Right);
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            assert!(matches!(
                route_selector_key(
                    &mut model,
                    KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, kind)
                ),
                SelectorKeyRoute::Control(None)
            ));
        }
        let SelectorKeyRoute::Control(Some(SelectorControl::ExecuteArchive(actual))) =
            route_selector_key(
                &mut model,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            )
        else {
            panic!("expected exact reviewed Archive request")
        };
        assert_eq!(actual, request);
        assert!(model.archive_confirmation.is_none());
        let intent = SessionTuiIntent {
            key: "cutex.exact".into(),
            action: SessionTuiAction::RetireSession,
            launch_profile: None,
            stock_runtime: None,
            runtime_package: None,
        };
        // Same guard used immediately by the production effect consumer.
        model.runtime_close_started(&intent);
        contract_key(&mut model, KeyCode::Enter);
        assert!(matches!(model.mode, SelectorMode::ClosingRuntime { .. }));
    }

    #[test]
    fn sorting_pending_runtime_rows_is_total_and_keeps_unknown_last() {
        let mut unknown = row("cutex.unknown", "Unknown", CutexSessionLifecycleState::Online, false, true);
        unknown.lifecycle = None;
        let online = row("cutex.online", "Online", CutexSessionLifecycleState::Online, false, true);
        let offline = row("cutex.offline", "Offline", CutexSessionLifecycleState::Offline, false, true);
        let mut rows = vec![unknown, global_row(), offline, online];
        sort_rows(&mut rows);
        assert_eq!(rows[0].target.agent_key(), Some("cutex.online"));
        assert_eq!(rows[1].target.agent_key(), Some("cutex.offline"));
        assert_eq!(rows[2].target.agent_key(), Some("cutex.unknown"));
        assert_eq!(rows[2].lifecycle, None);
        assert!(rows[3].target.uses_global_settings());
    }

    #[test]
    fn agent_action_keeps_view_snapshot_until_explicit_refresh() {
        let online = row("cutex.online", "Online", CutexSessionLifecycleState::Online, false, true);
        let offline = row("cutex.offline", "Offline", CutexSessionLifecycleState::Offline, false, true);
        let mut model = SelectorModel::new(vec![offline, online], false, false);
        model.workspace_selection.select(Some(SelectorTarget::Agent("cutex.online".to_string())));
        assert_eq!(
            model.visible_rows().iter().map(|row| row.agent.as_str()).collect::<Vec<_>>(),
            ["Online", "Offline"]
        );

        assert!(matches!(
            model.select_action(
                "cutex.online".to_string(),
                SessionTuiAction::CloseAndRestart,
                None,
            ),
            SelectorControl::Continue
        ));
        let flipped_online = row("cutex.offline", "Offline", CutexSessionLifecycleState::Online, false, true);
        let flipped_offline = row("cutex.online", "Online", CutexSessionLifecycleState::Offline, false, true);
        let appended = row("cutex.new", "New", CutexSessionLifecycleState::Online, false, true);
        model.replace_snapshot(SelectorSnapshot {
            rows: vec![appended, flipped_offline, flipped_online],
            warning: None,
        });

        assert_eq!(
            model.visible_rows().iter().map(|row| row.agent.as_str()).collect::<Vec<_>>(),
            ["Online", "Offline", "New"]
        );
        assert_eq!(
            model.selected_target(),
            Some(SelectorTarget::Agent("cutex.online".to_string()))
        );
        assert_eq!(model.selected_visible_index(), Some(0));

        assert!(matches!(
            selector_command(&mut model, Command::Refresh),
            SelectorKeyRoute::Refresh
        ));
        assert_eq!(
            model.visible_rows().iter().map(|row| row.agent.as_str()).collect::<Vec<_>>(),
            ["New", "Offline", "Online"]
        );
        assert_eq!(
            model.selected_target(),
            Some(SelectorTarget::Agent("cutex.online".to_string()))
        );
        assert_eq!(model.selected_visible_index(), Some(2));

        model.select_action(
            "cutex.online".to_string(),
            SessionTuiAction::ResumeAttach,
            None,
        );
        assert!(model.frozen_agent_view.is_some());
        model.mode = SelectorMode::Agents;
        assert!(matches!(
            selector_command(&mut model, Command::Page(PrimaryPanel::Agents)),
            SelectorKeyRoute::Switch(PrimaryPanel::Agents)
        ));
        assert!(model.frozen_agent_view.is_none());
    }

    #[test]
    fn profiles_opened_from_settings_return_through_settings() {
        for origin in [PrimaryPanel::Agents, PrimaryPanel::Recent, PrimaryPanel::Projects] {
            let mut model = SelectorModel::new(vec![global_row()], false, false);
            selector_command(&mut model, Command::Settings);
            model.settings_return_panel = Some(origin);
            selector_command(&mut model, Command::Profiles);
            model.open_profile_manager(vec![]);
            route_selector_key(&mut model, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            assert!(matches!(model.mode, SelectorMode::Settings { target: SelectorTarget::GlobalSettings, .. }));
            assert_eq!(model.settings_return_panel, Some(origin));
            assert!(matches!(route_selector_key(&mut model, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), SelectorKeyRoute::Control(None)));
            assert!(matches!(model.mode, SelectorMode::Settings { target: SelectorTarget::GlobalSettings, .. }));
        }
    }

    #[test]
    fn profile_escape_returns_to_list_before_the_origin_panel() {
        let mut model = SelectorModel::new(vec![global_row()], false, false);
        model.mode = SelectorMode::ProfileManager { profiles: vec![], selected: 0, focus: ProfileWorkspaceFocus::Items, editor_selected: 0 };
        model.settings_return_panel = Some(PrimaryPanel::Recent);
        route_selector_key(&mut model, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(matches!(model.mode, SelectorMode::ProfileManager { focus: ProfileWorkspaceFocus::Editor, .. }));
        route_selector_key(&mut model, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(model.mode, SelectorMode::ProfileManager { focus: ProfileWorkspaceFocus::Items, .. }));
        assert_eq!(model.settings_return_panel, Some(PrimaryPanel::Recent));
    }

    #[test]
    fn inherited_default_effort_selects_follow_profile_without_a_blank_choice() {
        let mut model = SelectorModel::new(vec![global_row()], false, false);
        selector_command(&mut model, Command::Settings);
        select_global_setting(&mut model, GlobalSettingsField::DefaultReasoning);
        model.handle(SelectorEvent::Activate);
        assert!(matches!(&model.settings_overlay, Some(SettingsOverlay::Choice { choices, selected: 0, custom_value: None, .. }) if choices[0].label == "Follow profile"));
    }

    #[test]
    fn agent_sort_preserves_selection_and_supports_pin_and_project_groups() {
        use cutex::profiles::list_preferences::ListSort;
        let mut config = CodezConfig::default();
        config.agent_sort = ListSort::NameAsc;
        let mut zulu = row("id-z", "Zulu", CutexSessionLifecycleState::Offline, true, true);
        zulu.project = Some(SelectorProjectContext { agent_name: "Zulu".into(), project_id: "p-z".into(), display_name: "Project Z".into(), badge_label: "PZ".into(), color: ProjectPaletteColor::Cyan });
        let mut model = SelectorModel::new(vec![global_settings_row(&config), zulu,
            row("id-a", "alpha", CutexSessionLifecycleState::Offline, false, true),
            row("id-b", "Beta", CutexSessionLifecycleState::Online, false, true)], false, false);
        assert_eq!(model.visible_rows().iter().map(|r| r.agent.as_str()).collect::<Vec<_>>(), ["alpha", "Beta", "Zulu"]);
        model.workspace_selection.select(Some(SelectorTarget::Agent("id-a".into())));
        selector_command(&mut model, Command::Sort);
        assert_eq!(model.visible_rows().iter().map(|r| r.agent.as_str()).collect::<Vec<_>>(), ["Zulu", "Beta", "alpha"]);
        assert_eq!(model.selected_managed_agent().unwrap().target.agent_key(), Some("id-a"));
        model.sort_order = ListSort::NameAsc;
        model.sort_order = ListSort::Pinned;
        assert_eq!(model.visible_rows()[0].agent, "Zulu");
        model.sort_order = ListSort::Project;
        assert_eq!(model.visible_rows().iter().map(|r| r.agent.as_str()).collect::<Vec<_>>(), ["Zulu", "alpha", "Beta"]);
        model.query = Input::new("alpha".into());
        assert_eq!(model.visible_rows().len(), 1);
        assert_eq!(model.visible_rows()[0].agent, "alpha");
    }

    #[test]
    fn sort_settings_render_as_editable_rows() {
        let mut model = SelectorModel::new(vec![global_row()], false, false);
        selector_command(&mut model, Command::Settings);
        for _ in 0..4 { model.handle(SelectorEvent::Down); }
        model.handle(SelectorEvent::OpenActions);
        for width in [80, 120, 180] {
            let appearance = rendered_text_at(width, 36, &model);
            for label in ["Agent sort", "Session sort"] { assert!(appearance.contains(label), "{appearance}"); }
            // r219: the Task Service and its settings are gone.
            for label in ["Task sort", "Task Watchdog"] { assert!(!appearance.contains(label), "{appearance}"); }
        }
    }

    #[test]
    fn local_host_default_applies_to_both_lists_only_on_open() {
        let mut config = CodezConfig::default();
        config.default_local_host_filter = true;
        let mut model = SelectorModel::new(vec![global_settings_row(&config)], false, false);
        let local = cutex::platform::host::current_host_name();
        assert_eq!(model.facets.host.as_ref().map(|h|h.key.as_str()), Some(local.as_str()));
        assert_eq!(model.recent.facets.host, model.facets.host);
        model.facets.host = None;
        model.ensure_selection();
        assert!(model.facets.host.is_none());
        config.default_local_host_filter = false;
        let model = SelectorModel::new(vec![global_settings_row(&config)], false, false);
        assert!(model.facets.host.is_none() && model.recent.facets.host.is_none());
    }

    #[test]
    fn settings_frame_encloses_details_and_keeps_selected_color_swatch() {
        let mut model = SelectorModel::new(vec![global_row()], false, false);
        selector_command(&mut model, Command::Settings);
        for _ in 0..4 { model.handle(SelectorEvent::Down); }
        for focused in [false, true] {
            if focused { model.handle(SelectorEvent::OpenActions); }
            let swatch_index = model.active_setting_category().unwrap().options.iter().position(|o| o.presentation.swatch.is_some()).unwrap();
            if let SelectorMode::Settings { option, .. } = &mut model.mode { *option = swatch_index; }
            for width in [80, 120, 160] {
                let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 24)).unwrap();
                terminal.draw(|frame| render_categorized_settings(frame, frame.area(), &model)).unwrap();
                let buffer = terminal.backend().buffer();
                for x in [0, 25, 26, width - 1] {
                    for y in 1..23 { assert_eq!(buffer[(x,y)].symbol(), "│", "broken edge at {x},{y}"); }
                }
                assert_eq!(buffer[(0,23)].symbol(), "└");
                assert_eq!(buffer[(26,23)].symbol(), "└");
                assert_eq!(buffer[(width-1,23)].symbol(), "┘");
                let swatch = model.active_setting_option().unwrap().presentation.swatch.unwrap();
                assert_eq!(buffer.content.iter().filter(|cell| cell.bg == swatch).count(), 2, "Brand swatch lost when focused={focused}");
            }
        }
    }

    #[test]
    fn settings_display_groups_do_not_resolve_status_item_values() {
        let mut config = CodezConfig::default();
        config.custom_status_items.push(serde_json::from_value(serde_json::json!({
            "id": "cutex_fixture", "title": "Fixture Status Name",
            "source": {"kind": "static", "value": "DO_NOT_RENDER_DYNAMIC_VALUE"}
        })).unwrap());
        let mut model = SelectorModel::new(vec![global_settings_row(&config)], false, false);
        selector_command(&mut model, Command::Settings);
        for _ in 0..4 { model.handle(SelectorEvent::Down); }
        model.handle(SelectorEvent::OpenActions);
        let screen = rendered_text_at(80, 64, &model);
        for text in ["Theme colors", "Custom status items", "#E08EB2"] { assert!(screen.contains(text), "{screen}"); }
        for text in ["DO_NOT_RENDER_DYNAMIC_VALUE", "20 settings", "Categories", "Global settings", "Inspector"] { assert!(!screen.contains(text), "{screen}"); }
        let first = model.active_setting_option().unwrap().label;
        model.handle(SelectorEvent::Down);
        assert_ne!(model.active_setting_option().unwrap().label, first);
        while model.active_setting_option().unwrap().presentation.name.as_deref() != Some("Fixture Status Name") {
            model.handle(SelectorEvent::Down);
        }
        assert_eq!(model.active_setting_option().unwrap().presentation.name.as_deref(), Some("Fixture Status Name"));
        let scrolled = rendered_text_at(80, 24, &model);
        assert!(scrolled.contains("ID: cutex_fixture"));
        assert!(scrolled.contains("Fixture Status Name"));
        assert!(!scrolled.contains("DO_NOT_RENDER_DYNAMIC_VALUE"));
    }

    #[test]
    fn global_settings_arrows_stage_choices_and_leave_text_untouched() {
        let mut model = SelectorModel::new(vec![global_row()], false, false);
        selector_command(&mut model, Command::Settings);
        select_global_setting(&mut model, GlobalSettingsField::DefaultNotification);
        let original = model.active_setting_option().unwrap().value.clone();
        assert!(matches!(route_selector_key(&mut model, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)), SelectorKeyRoute::Control(_)));
        assert!(model.settings_overlay.is_none());
        assert_ne!(model.active_setting_option().unwrap().value, original);
        assert_eq!(model.settings_dirty_count(), 1);
        route_selector_key(&mut model, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(model.active_setting_option().unwrap().value, original);
        select_global_setting(&mut model, GlobalSettingsField::ProxyUrl);
        let before = model.settings_dirty_count();
        route_selector_key(&mut model, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(model.settings_dirty_count(), before);
        assert!(model.settings_overlay.is_none());
        route_selector_key(&mut model, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(model.settings_overlay, Some(SettingsOverlay::Text { .. })));
        route_selector_key(&mut model, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(model.settings_overlay.is_none());
    }

    #[test]
    fn settings_tab_cycles_local_columns_without_leaving() {
        let mut model = SelectorModel::new(vec![global_row()], false, false);
        selector_command(&mut model, Command::Settings);
        model.settings_return_panel = Some(PrimaryPanel::Recent);
        for (key, expected) in [
            (KeyCode::Tab, SettingsFocus::Options),
            (KeyCode::Tab, SettingsFocus::Categories),
            (KeyCode::BackTab, SettingsFocus::Options),
            (KeyCode::BackTab, SettingsFocus::Categories),
        ] {
            route_selector_key(&mut model, KeyEvent::new(key, KeyModifiers::NONE));
            assert!(matches!(model.mode, SelectorMode::Settings { focus, .. } if focus == expected));
            assert_eq!(model.settings_return_panel, Some(PrimaryPanel::Recent));
        }
    }

    #[test]
    fn settings_escape_unwinds_columns_before_returning_to_origin() {
        for origin in [PrimaryPanel::Agents, PrimaryPanel::Recent, PrimaryPanel::Projects] {
            let mut model = SelectorModel::new(vec![global_row()], false, false);
            selector_command(&mut model, Command::Settings);
            model.settings_return_panel = Some(origin);
            if let SelectorMode::Settings { focus, .. } = &mut model.mode {
                *focus = SettingsFocus::Value;
            }
            for expected in [SettingsFocus::Options, SettingsFocus::Categories] {
                assert!(matches!(route_selector_key(&mut model, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), SelectorKeyRoute::Control(None)));
                assert!(matches!(model.mode, SelectorMode::Settings { focus, .. } if focus == expected));
                assert_eq!(model.settings_return_panel, Some(origin));
            }
            assert!(matches!(route_selector_key(&mut model, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), SelectorKeyRoute::Control(None)));
            assert!(matches!(model.mode, SelectorMode::Settings { target: SelectorTarget::GlobalSettings, .. }));
        }
    }

    #[test]
    fn settings_escape_cancels_text_without_leaving_or_staging() {
        let record = editable_record();
        let mut model = editable_model(&record);
        model.settings_return_panel = Some(PrimaryPanel::Recent);
        model.handle(SelectorEvent::OpenSettings);
        for _ in 0..5 { model.handle(SelectorEvent::Down); }
        model.handle(SelectorEvent::Activate);
        model.handle(SelectorEvent::Insert('x'));
        assert!(matches!(model.settings_overlay, Some(SettingsOverlay::Text { .. })));
        route_selector_key(&mut model, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(model.settings_overlay.is_none());
        assert!(matches!(model.mode, SelectorMode::Settings { .. }));
        assert_eq!(model.settings_dirty_count(), 0);
        assert_eq!(model.settings_return_panel, Some(PrimaryPanel::Recent));
    }

    #[test]
    fn global_settings_category_arrows_switch_panels_without_opening_options() {
        let mut model = SelectorModel::new(vec![global_row()], false, false);
        selector_command(&mut model, Command::Settings);
        let before = format!("{:?}", model.mode);
        assert!(matches!(route_selector_key(&mut model, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)), SelectorKeyRoute::Control(_)));
        assert_eq!(format!("{:?}", model.mode), before);
        // 85156fb added the Profiles panel between Jobs and Settings.
        assert!(matches!(route_selector_key(&mut model, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)), SelectorKeyRoute::Switch(PrimaryPanel::Profiles)));
    }

    #[test]
    fn global_settings_escape_then_left_leaves_options() {
        let mut model = SelectorModel::new(vec![global_row()], false, false);
        selector_command(&mut model, Command::Settings);
        if let SelectorMode::Settings { focus, .. } = &mut model.mode {
            *focus = SettingsFocus::Options;
        }
        assert_eq!(selector_list_panel_from_horizontal_key(&model,
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)), None);
        route_selector_key(&mut model, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        // 85156fb added the Profiles panel between Jobs and Settings.
        assert!(matches!(route_selector_key(&mut model,
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            SelectorKeyRoute::Switch(PrimaryPanel::Profiles)));
    }

    #[test]
    fn ui_contract_k08_recent_repeat_edits_but_release_is_ignored() {
        let mut model = contract_recent_model();
        contract_key(&mut model, KeyCode::Char('/'));
        for kind in [
            KeyEventKind::Press,
            KeyEventKind::Repeat,
            KeyEventKind::Release,
        ] {
            route_selector_key(
                &mut model,
                KeyEvent::new_with_kind(KeyCode::Char('n'), KeyModifiers::NONE, kind),
            );
        }
        assert_eq!(model.recent.query(), "nn");
        route_selector_key(
            &mut model,
            KeyEvent::new_with_kind(KeyCode::Left, KeyModifiers::NONE, KeyEventKind::Repeat),
        );
        assert_eq!(model.recent.filter_input().cursor(), 1);
    }

    #[test]
    fn ui_contract_recent_paste_and_unicode_cursor_stay_in_filter() {
        let mut model = contract_recent_model();
        contract_key(&mut model, KeyCode::Char('/'));
        let text = "中文 e\u{301} 🙂 ".repeat(12);
        handle_selector_paste(&mut model, &format!("{text}\r\n\t\u{1b}"));
        assert_eq!(model.recent.query(), text);
        let mut terminal = Terminal::new(TestBackend::new(38, 12)).unwrap();
        terminal
            .draw(|frame| render_recent_workspace(frame, frame.area(), &model))
            .unwrap();
        let cursor = terminal.get_cursor_position().unwrap();
        assert!(cursor.x > 0 && cursor.x < 37);
        assert_eq!(cursor.y, 1);
        contract_key(&mut model, KeyCode::Home);
        terminal
            .draw(|frame| render_recent_workspace(frame, frame.area(), &model))
            .unwrap();
        assert_eq!(terminal.get_cursor_position().unwrap().x, 1);
        model.activate_primary_panel(PrimaryPanel::Agents);
        handle_selector_paste(&mut model, "hidden");
        assert_eq!(model.recent.query(), text);
    }

    #[test]
    fn ui_contract_b1_managed_recent_filter_parity_and_root_escape() {
        for recent in [false, true] {
            let mut model = contract_recent_model();
            if !recent {
                model.activate_primary_panel(PrimaryPanel::Agents);
            }
            let text = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 /中文🙂";
            for c in text.chars() {
                contract_key(&mut model, KeyCode::Char(c));
            }
            let query = |m: &SelectorModel| {
                if recent {
                    m.recent.query().to_owned()
                } else {
                    m.query.value().to_owned()
                }
            };
            assert_eq!(query(&model), text);
            assert!(selector_input(&mut model).is_some());
            assert!(matches!(
                route_selector_key(
                    &mut model,
                    KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)
                ),
                SelectorKeyRoute::Control(None)
            ));
            contract_key(&mut model, KeyCode::Home);
            contract_key(&mut model, KeyCode::Delete);
            assert!(query(&model).starts_with('b'));
            handle_selector_paste(&mut model, "中e\u{301}🙂\n\r\t\u{1b}");
            assert!(query(&model).starts_with("中e\u{301}🙂b"));
            contract_key(&mut model, KeyCode::Esc);
            assert!(selector_input(&mut model).is_none());
            assert!(!query(&model).is_empty());
            contract_key(&mut model, KeyCode::Esc);
            assert_eq!(query(&model), "");
            contract_key(&mut model, KeyCode::Esc);
            assert!(model.notice.as_deref().unwrap().contains("Ctrl+C"));
            for key in [KeyCode::Tab, KeyCode::BackTab] {
                contract_key(&mut model, key);
                assert!(model.recent.review().is_none());
            }
        }
    }

    fn contract_help_command(model: &mut SelectorModel, command: Command) -> SelectorKeyRoute {
        contract_key(model, KeyCode::F(1));
        let index = selector_commands(model)
            .iter()
            .position(|(c, _)| *c == command)
            .unwrap();
        for _ in 0..index {
            contract_key(model, KeyCode::Down);
        }
        route_selector_key(model, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    }

    #[test]
    fn ui_contract_b1_f1_actions_inspect_settings_and_load_more() {
        let mut model = SelectorModel::new(
            vec![row(
                "cutex.one",
                "Formal One",
                CutexSessionLifecycleState::Online,
                true,
                true,
            )],
            false,
            false,
        );
        assert!(matches!(
            contract_help_command(&mut model, Command::Inspect),
            SelectorKeyRoute::Control(_)
        ));
        assert!(model.inspector_overview_focused);
        contract_help_command(&mut model, Command::Actions);
        assert!(matches!(model.mode, SelectorMode::Actions { .. }));
        contract_help_command(&mut model, Command::Edit);
        assert!(matches!(model.mode, SelectorMode::Settings { .. }));
        let mut model = SelectorModel::new(vec![global_row()], false, false);
        contract_help_command(&mut model, Command::Settings);
        assert!(matches!(
            model.mode,
            SelectorMode::Settings {
                target: SelectorTarget::GlobalSettings,
                ..
            }
        ));
        let mut recent = contract_recent_model();
        assert!(matches!(
            contract_help_command(&mut recent, Command::LoadMore),
            SelectorKeyRoute::Control(Some(SelectorControl::Recent(RecentCommand::LoadMore)))
        ));
        contract_key(&mut recent, KeyCode::Char('n'));
        assert_eq!(recent.recent.query(), "n");
    }

    #[test]
    fn ui_contract_b1_dirty_editor_navigation_cancel_discard_save_and_ctrl_x() {
        let mut model = editable_model(&editable_record());
        model.handle(SelectorEvent::OpenSettings);
        model.settings_overlay = Some(SettingsOverlay::Text {
            field: SettingsEditField::Session(SessionSettingsField::AgentName),
            input: Input::new("Formal name".into()),
            tags: false,
            masked: false,
        });
        route_selector_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        );
        assert!(matches!(
            model.settings_overlay,
            Some(SettingsOverlay::Text { .. })
        ));
        contract_key(&mut model, KeyCode::Home);
        handle_selector_paste(&mut model, "中文\n");
        assert!(selector_input(&mut model)
            .unwrap()
            .value()
            .starts_with("中文Formal"));
        route_selector_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        assert!(!model.leave_review.as_ref().unwrap().can_save); // unstaged field has no direct save
        contract_key(&mut model, KeyCode::Enter); // default Cancel keeps text
        assert!(model.settings_overlay.is_some());
        contract_key(&mut model, KeyCode::Enter); // stage the field through its existing validator
        assert!(model.settings_draft.is_dirty());
        route_selector_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        assert!(model.leave_review.as_ref().unwrap().can_save);
        contract_key(&mut model, KeyCode::Right);
        contract_key(&mut model, KeyCode::Right);
        assert!(matches!(
            route_selector_key(
                &mut model,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            SelectorKeyRoute::Control(Some(SelectorControl::ApplySettings(_)))
        ));
        // Save stays in the editor until its real effect result; no fake exit.
        assert!(matches!(model.mode, SelectorMode::Settings { .. }));
        route_selector_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::ALT),
        );
        contract_key(&mut model, KeyCode::Right);
        assert!(matches!(
            route_selector_key(
                &mut model,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            SelectorKeyRoute::Switch(PrimaryPanel::Recent)
        ));
        assert!(!selector_dirty(&model));
    }

    #[test]
    fn ui_contract_b1_confirmation_revision_change_requires_fresh_review() {
        let mut model = SelectorModel::new(
            vec![row(
                "cutex.one",
                "Formal One",
                CutexSessionLifecycleState::Online,
                true,
                true,
            )],
            false,
            false,
        );
        model.activate_close_shortcut();
        assert!(matches!(
            model.mode,
            SelectorMode::ConfirmRuntimeAction { .. }
        ));
        let mut rows = model.rows.clone();
        rows[0].revision += 1;
        model.replace_snapshot(SelectorSnapshot {
            rows,
            warning: None,
        });
        assert!(matches!(model.mode, SelectorMode::Agents));
    }

    #[test]
    fn ui_contract_b1_production_list_navigation_stops_and_takeover_is_unchanged() {
        let mut model = SelectorModel::new(
            vec![
                row(
                    "cutex.one",
                    "One",
                    CutexSessionLifecycleState::Online,
                    true,
                    true,
                ),
                row(
                    "cutex.two",
                    "Two",
                    CutexSessionLifecycleState::Online,
                    true,
                    true,
                ),
            ],
            false,
            false,
        );
        contract_key(&mut model, KeyCode::Up);
        assert_eq!(model.selected_visible_index(), Some(0));
        contract_key(&mut model, KeyCode::End);
        contract_key(&mut model, KeyCode::Down);
        assert_eq!(model.selected_visible_index(), Some(1));
        let SelectorKeyRoute::Control(Some(SelectorControl::Selected(intent))) = route_selector_key(
            &mut model,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        ) else {
            panic!("primary intent")
        };
        assert_eq!(intent.action, SessionTuiAction::ResumeAttach);
        assert_eq!(intent.key, "cutex.two");
    }

    #[test]
    fn released_keys_and_modified_text_do_not_change_the_filter() {
        let released = KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert_eq!(selector_event_from_key(released, false), None);
        assert!(!close_runtime_shortcut_from_key(released));
        assert_eq!(
            selector_event_from_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), false),
            Some(SelectorEvent::Insert('q'))
        );
        assert_eq!(
            selector_event_from_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                false
            ),
            Some(SelectorEvent::Exit)
        );
        assert_eq!(
            selector_event_from_key(
                KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
                false
            ),
            Some(SelectorEvent::ClearInput)
        );
        let direct_close = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert!(close_runtime_shortcut_from_key(direct_close));
        assert_eq!(selector_event_from_key(direct_close, false), None);
        assert_eq!(
            selector_event_from_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE), false),
            Some(SelectorEvent::Delete)
        );
    }

    #[test]
    fn shift_enter_is_bound_only_with_enhanced_keyboard_support() {
        let shift_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(selector_event_from_key(shift_enter, false), None);
        assert_eq!(
            selector_event_from_key(shift_enter, true),
            Some(SelectorEvent::OpenActions)
        );
        assert_eq!(
            selector_event_from_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), false),
            None
        );
        assert_eq!(
            selector_event_from_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), false),
            Some(SelectorEvent::Insert('a'))
        );
        assert_eq!(
            selector_event_from_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), false),
            None
        );
    }

    #[test]
    fn enhancement_probe_is_limited_to_known_terminal_environments() {
        assert!(terminal_environment_may_support_enhancement(
            Some("xterm-kitty"),
            None,
            false
        ));
        assert!(terminal_environment_may_support_enhancement(
            Some("xterm-256color"),
            Some("WezTerm"),
            false
        ));
        assert!(terminal_environment_may_support_enhancement(
            None, None, true
        ));
        assert!(!terminal_environment_may_support_enhancement(
            Some("xterm-256color"),
            None,
            false
        ));
    }

    #[test]
    fn recent_adoption_request_keeps_formal_name_distinct_from_native_title() {
        let request = RecentAdoptionRequest {
            action_id: "test-adopt".into(),
            formal_name: "Explicit Agent".into(),
            thread_id: "native-thread-123".to_string(),
            title: "Native preview".to_string(),
            cwd: "/native/work".to_string(),
        };

        assert_ne!(request.formal_name, request.title);
        assert_eq!(request.thread_id, "native-thread-123");
    }

    #[test]
    fn persisted_recent_adoption_stays_successful_when_agent_projection_fails() {
        let request = RecentAdoptionRequest {
            action_id: "test-adopt".into(),
            formal_name: "Explicit Agent".into(),
            thread_id: "native-thread-123".to_string(),
            title: "Native preview".to_string(),
            cwd: "/native/work".to_string(),
        };
        let mut model = SelectorModel::new(vec![recent_sessions_row()], false, false);
        model.recent_adoption_succeeded(
            &request,
            RecentAdoptionResult {
                store: CutexSessionStore::default(),
                snapshot: Err("projection unavailable".to_string()),
            },
        );

        assert_eq!(
            model.notice.as_deref(),
            Some("Adopted and imported Agent Explicit Agent; unassigned. Use Projects Create/Add for explicit assignment.")
        );
        assert!(model.warning.as_deref().is_some_and(|warning| {
            warning.starts_with("Native thread was adopted, but agent refresh failed:")
        }));
    }
}

#[path="session_tui_remote_projection.rs"]
mod remote_projection;
