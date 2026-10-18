use cutex::profiles::appearance::{Appearance, IdentityField, ProfileOptions, Provider};
use std::fmt;

use cutex::agent_bus::identity::normalize_agent_groups;
use cutex::agent_bus::service::DEFAULT_AGENT_BUS_PORT;
use cutex::config::global_settings::{
    parse_agent_bus_port, parse_desktop_notify_port, parse_notify_events,
    parse_notify_user_message_content, parse_optional_u64, parse_rate_limit_mode,
    ConfigValueUpdate, GlobalConfigPatch,
};
use cutex::config::proxy::proxy_config_from_parts;
use cutex::notify::service::DEFAULT_DESKTOP_NOTIFY_PORT;
use cutex::profiles::model::CodezConfig;
use cutex::session::model::{
    parse_cutex_session_quick_action_mode, parse_cutex_session_runtime_backend,
    CutexSessionQuickActionMode, CutexSessionRecord, CutexSessionRuntimeBackend,
};
use cutex::session::projection::runtime_backend_short_label;
use cutex::session::service::{
    cutex_session_display_name, cutex_session_is_managed, cutex_session_launch_cwd,
    normalize_cutex_session_managed_cwd_path, CutexSessionRoutingPatch,
    CutexSessionRuntimeDefaultsPatch, CutexSessionValueUpdate,
};

use super::prompt::{cli_args_label, parse_cli_args_value};
use super::session_tui_profile_settings::ProfileSettingsField;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct SettingPresentation {
    pub group: Option<&'static str>,
    pub name: Option<String>,
    pub detail: Option<String>,
    pub style: Option<ratatui::style::Style>,
    pub swatch: Option<ratatui::style::Color>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SessionTuiSettingOption {
    pub(super) label: &'static str,
    pub(super) value: String,
    pub(super) field: Option<SessionSettingsField>,
    pub(super) global_field: Option<GlobalSettingsField>,
    pub(super) profile_field: Option<ProfileSettingsField>,
    pub(super) command: Option<SessionSettingsCommand>,
    pub(super) navigation: Option<super::session_tui_input::Command>,
    pub(super) dirty: bool,
    pub(super) presentation: SettingPresentation,
}

impl SessionTuiSettingOption {
    fn new(label: &'static str, value: impl Into<String>) -> Self {
        Self {
            label,
            value: value.into(),
            field: None,
            global_field: None,
            profile_field: None,
            command: None,
            navigation: None,
            presentation: SettingPresentation::default(),
            dirty: false,
        }
    }

    fn editable(
        label: &'static str,
        value: impl Into<String>,
        field: SessionSettingsField,
        dirty: bool,
    ) -> Self {
        Self {
            label,
            value: value.into(),
            field: Some(field),
            global_field: None,
            profile_field: None,
            command: None,
            navigation: None,
            presentation: SettingPresentation::default(),
            dirty,
        }
    }

    fn command(
        label: &'static str,
        value: impl Into<String>,
        command: SessionSettingsCommand,
    ) -> Self {
        Self {
            label,
            value: value.into(),
            field: None,
            global_field: None,
            profile_field: None,
            command: Some(command),
            navigation: None,
            presentation: SettingPresentation::default(),
            dirty: false,
        }
    }

    fn global_editable(
        label: &'static str,
        value: impl Into<String>,
        field: GlobalSettingsField,
        dirty: bool,
    ) -> Self {
        Self {
            label,
            value: value.into(),
            field: None,
            global_field: Some(field),
            profile_field: None,
            command: None,
            navigation: None,
            presentation: SettingPresentation::default(),
            dirty,
        }
    }

    pub(super) fn profile_editable(
        label: &'static str,
        value: impl Into<String>,
        field: ProfileSettingsField,
        dirty: bool,
    ) -> Self {
        Self {
            label,
            value: value.into(),
            field: None,
            global_field: None,
            profile_field: Some(field),
            command: None,
            navigation: None,
            presentation: SettingPresentation::default(),
            dirty,
        }
    }

    pub(super) fn profile_read_only(label: &'static str, value: impl Into<String>) -> Self {
        Self::new(label, value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SessionTuiSettingCategory {
    pub(super) label: &'static str,
    pub(super) options: Vec<SessionTuiSettingOption>,
}

impl SessionTuiSettingCategory {
    fn new(label: &'static str, options: Vec<SessionTuiSettingOption>) -> Self {
        Self { label, options }
    }

    pub(super) fn profile(label: &'static str, options: Vec<SessionTuiSettingOption>) -> Self {
        Self::new(label, options)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionSettingsField {
    AgentName,
    Profile,
    PermissionPreset,
    ApprovalPolicy,
    SandboxMode,
    Model,
    ReasoningEffort,
    RuntimeBackend,
    ManagedCwd,
    ExtraCliArgs,
    AgentGroups,
    WorkbenchVisibility,
    QuickAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionSettingsCommand {
    Adopt,
    Unmanage,
}

impl SessionSettingsCommand {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Adopt => "Adopt",
            Self::Unmanage => "Unmanage",
        }
    }

    pub(super) fn success_notice(self) -> &'static str {
        match self {
            Self::Adopt => "Adopted agent",
            Self::Unmanage => "Unmanaged agent",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SessionSettingsChoice {
    pub(super) label: String,
    pub(super) value: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionSettingsEditorKind {
    Choice,
    Text,
    Tags,
    Secret,
}

const PERMISSION_PRESET_CHOICES: &[(&str, Option<&str>)] = &[
    ("Inherit", None),
    ("Read only", Some("read-only")),
    ("Workspace", Some("workspace")),
    ("Full access", Some("full-access")),
];

const APPROVAL_POLICY_CHOICES: &[(&str, Option<&str>)] = &[
    ("Inherit", None),
    ("On request", Some("on-request")),
    ("Never", Some("never")),
];

const SANDBOX_MODE_CHOICES: &[(&str, Option<&str>)] = &[
    ("Inherit", None),
    ("Read only", Some("read-only")),
    ("Workspace write", Some("workspace-write")),
    ("Danger full access", Some("danger-full-access")),
];

const REASONING_EFFORT_CHOICES: &[(&str, Option<&str>)] = &[
    ("Inherit", None),
    ("Minimal", Some("minimal")),
    ("Low", Some("low")),
    ("Medium", Some("medium")),
    ("High", Some("high")),
    ("Extra high", Some("xhigh")),
];

const RUNTIME_BACKEND_CHOICES: &[(&str, Option<&str>)] = &[
    ("Host", Some("host")),
    ("Native foreground", Some("native")),
    ("Docker", Some("docker")),
    ("Cute Alden", Some("alden")),
    ("Future", Some("future")),
];

const WORKBENCH_VISIBILITY_CHOICES: &[(&str, Option<&str>)] =
    &[("Visible", Some("visible")), ("Hidden", Some("hidden"))];

const QUICK_ACTION_CHOICES: &[(&str, Option<&str>)] = &[
    ("Auto", Some("auto")),
    ("Pinned", Some("pinned")),
    ("Hidden", Some("hidden")),
];

impl SessionSettingsField {
    pub(super) fn editor_kind(self) -> SessionSettingsEditorKind {
        match self {
            Self::Profile
            | Self::PermissionPreset
            | Self::ApprovalPolicy
            | Self::SandboxMode
            | Self::ReasoningEffort
            | Self::RuntimeBackend
            | Self::WorkbenchVisibility
            | Self::QuickAction => SessionSettingsEditorKind::Choice,
            Self::AgentName | Self::Model | Self::ManagedCwd | Self::ExtraCliArgs => {
                SessionSettingsEditorKind::Text
            }
            Self::AgentGroups => SessionSettingsEditorKind::Tags,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SessionSettingsSnapshot {
    agent_name: String,
    profile: Option<String>,
    profile_names: Vec<String>,
    permission_defaults: Option<String>,
    approval_policy: Option<String>,
    sandbox_mode: Option<String>,
    model_defaults: Option<String>,
    reasoning_defaults: Option<String>,
    runtime_backend: CutexSessionRuntimeBackend,
    session_cwd: String,
    working_directory: String,
    managed_cwd: Option<String>,
    default_cli_args: Vec<String>,
    default_cli_args_label: String,
    agent_groups: Vec<String>,
    agent_groups_label: String,
    exposed_to_backend: bool,
    quick_action: CutexSessionQuickActionMode,
    managed: bool,
    cutex_session_id: String,
    codex_session_id: Option<String>,
    host_id: String,
}

impl SessionSettingsSnapshot {
    pub(super) fn uses_codex_profiles(&self) -> bool {
        self.runtime_backend != CutexSessionRuntimeBackend::ClaudeSubscription
    }
    #[cfg(test)]
    pub(super) fn from_record(record: &CutexSessionRecord) -> Self {
        Self::from_record_with_profiles(record, &[])
    }

    pub(super) fn from_record_with_profiles(
        record: &CutexSessionRecord,
        profile_names: &[String],
    ) -> Self {
        Self {
            agent_name: cutex_session_display_name(record),
            profile: record.profile.clone(),
            profile_names: profile_names.to_vec(),
            permission_defaults: record.permission_defaults.clone(),
            approval_policy: record.approval_policy.clone(),
            sandbox_mode: record.sandbox_mode.clone(),
            model_defaults: record.model_defaults.clone(),
            reasoning_defaults: record.reasoning_defaults.clone(),
            runtime_backend: record.runtime_backend,
            session_cwd: record.cwd.clone(),
            working_directory: cutex_session_launch_cwd(record).to_string(),
            managed_cwd: record.managed_cwd.clone(),
            default_cli_args: record.default_cli_args.clone(),
            default_cli_args_label: cli_args_label(&record.default_cli_args),
            agent_groups: record.agent_groups.clone(),
            agent_groups_label: record.agent_groups.join(", "),
            exposed_to_backend: record.exposed_to_backend,
            quick_action: record.quick_action,
            managed: cutex_session_is_managed(record),
            cutex_session_id: record.cutex_session_id.clone(),
            codex_session_id: record.codex_session_id.clone(),
            host_id: record.host_id.clone(),
        }
    }

    pub(super) fn value(&self, field: SessionSettingsField) -> Option<&str> {
        match field {
            SessionSettingsField::AgentName => Some(self.agent_name.as_str()),
            SessionSettingsField::Profile => self.profile.as_deref(),
            SessionSettingsField::PermissionPreset => self.permission_defaults.as_deref(),
            SessionSettingsField::ApprovalPolicy => self.approval_policy.as_deref(),
            SessionSettingsField::SandboxMode => self.sandbox_mode.as_deref(),
            SessionSettingsField::Model => self.model_defaults.as_deref(),
            SessionSettingsField::ReasoningEffort => self.reasoning_defaults.as_deref(),
            SessionSettingsField::RuntimeBackend => {
                Some(runtime_backend_short_label(self.runtime_backend))
            }
            SessionSettingsField::ManagedCwd => self.managed_cwd.as_deref(),
            SessionSettingsField::ExtraCliArgs => Some(self.default_cli_args_label.as_str()),
            SessionSettingsField::AgentGroups => Some(self.agent_groups_label.as_str()),
            SessionSettingsField::WorkbenchVisibility => Some(if self.exposed_to_backend {
                "visible"
            } else {
                "hidden"
            }),
            SessionSettingsField::QuickAction => Some(self.quick_action.label()),
        }
    }

    pub(super) fn choices(&self, field: SessionSettingsField) -> Vec<SessionSettingsChoice> {
        if field == SessionSettingsField::Profile {
            let mut choices = vec![SessionSettingsChoice {
                label: "Follow global default".to_string(),
                value: None,
            }];
            choices.extend(self.profile_names.iter().map(|name| SessionSettingsChoice {
                label: name.clone(),
                value: Some(name.clone()),
            }));
            return choices;
        }
        let choices = match field {
            SessionSettingsField::PermissionPreset => PERMISSION_PRESET_CHOICES,
            SessionSettingsField::ApprovalPolicy => APPROVAL_POLICY_CHOICES,
            SessionSettingsField::SandboxMode => SANDBOX_MODE_CHOICES,
            SessionSettingsField::ReasoningEffort => REASONING_EFFORT_CHOICES,
            SessionSettingsField::RuntimeBackend => RUNTIME_BACKEND_CHOICES,
            SessionSettingsField::WorkbenchVisibility => WORKBENCH_VISIBILITY_CHOICES,
            SessionSettingsField::QuickAction => QUICK_ACTION_CHOICES,
            SessionSettingsField::AgentName
            | SessionSettingsField::Profile
            | SessionSettingsField::Model
            | SessionSettingsField::ManagedCwd
            | SessionSettingsField::ExtraCliArgs
            | SessionSettingsField::AgentGroups => &[],
        };
        choices
            .iter()
            .map(|(label, value)| SessionSettingsChoice {
                label: (*label).to_string(),
                value: value.map(str::to_string),
            })
            .collect()
    }

    pub(super) fn profile_names(&self) -> &[String] {
        &self.profile_names
    }

    pub(super) fn categories(
        &self,
        draft: &SessionSettingsDraft,
    ) -> Vec<SessionTuiSettingCategory> {
        vec![
            SessionTuiSettingCategory::new(
                "Identity",
                vec![
                    self.editable_option("Agent name", SessionSettingsField::AgentName, draft),
                    self.editable_option("Profile", SessionSettingsField::Profile, draft),
                ],
            ),
            SessionTuiSettingCategory::new(
                "Permissions",
                vec![
                    self.editable_option(
                        "Permission preset",
                        SessionSettingsField::PermissionPreset,
                        draft,
                    ),
                    self.editable_option(
                        "Approval policy",
                        SessionSettingsField::ApprovalPolicy,
                        draft,
                    ),
                    self.editable_option("Sandbox mode", SessionSettingsField::SandboxMode, draft),
                    self.editable_option("Model", SessionSettingsField::Model, draft),
                    self.editable_option(
                        "Reasoning effort",
                        SessionSettingsField::ReasoningEffort,
                        draft,
                    ),
                ],
            ),
            SessionTuiSettingCategory::new(
                "Launch",
                vec![
                    self.editable_option(
                        "Runtime backend",
                        SessionSettingsField::RuntimeBackend,
                        draft,
                    ),
                    SessionTuiSettingOption::new(
                        "Working directory",
                        draft.effective_working_directory(self),
                    ),
                    self.editable_option("Managed cwd", SessionSettingsField::ManagedCwd, draft),
                    self.editable_option(
                        "Extra CLI args",
                        SessionSettingsField::ExtraCliArgs,
                        draft,
                    ),
                ],
            ),
            SessionTuiSettingCategory::new(
                "Routing",
                vec![
                    self.editable_option(
                        "Message groups",
                        SessionSettingsField::AgentGroups,
                        draft,
                    ),
                    if self.managed {
                        self.editable_option(
                            "Workbench visibility",
                            SessionSettingsField::WorkbenchVisibility,
                            draft,
                        )
                    } else {
                        SessionTuiSettingOption::new(
                            "Workbench visibility",
                            if self.exposed_to_backend {
                                "visible"
                            } else {
                                "hidden"
                            },
                        )
                    },
                    self.editable_option("Quick action", SessionSettingsField::QuickAction, draft),
                ],
            ),
            SessionTuiSettingCategory::new(
                "Details",
                vec![
                    SessionTuiSettingOption::new("Managed", enabled(self.managed)),
                    SessionTuiSettingOption::command(
                        "Management",
                        if self.managed { "unmanage" } else { "adopt" },
                        if self.managed {
                            SessionSettingsCommand::Unmanage
                        } else {
                            SessionSettingsCommand::Adopt
                        },
                    ),
                    SessionTuiSettingOption::new("Cutex session", self.cutex_session_id.clone()),
                    SessionTuiSettingOption::new(
                        "Cute-codex session",
                        optional(self.codex_session_id.as_deref()),
                    ),
                    SessionTuiSettingOption::new("Host", nonempty(&self.host_id)),
                ],
            ),
        ]
    }

    fn editable_option(
        &self,
        label: &'static str,
        field: SessionSettingsField,
        draft: &SessionSettingsDraft,
    ) -> SessionTuiSettingOption {
        let value = draft.value(self, field);
        let value = if field == SessionSettingsField::Profile && value.is_none() {
            "Follow global default".to_string()
        } else {
            optional(value)
        };
        SessionTuiSettingOption::editable(label, value, field, draft.field_is_dirty(field))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct SessionSettingsDraft {
    agent_name: Option<String>,
    profile: CutexSessionValueUpdate<String>,
    permission_defaults: CutexSessionValueUpdate<String>,
    approval_policy: CutexSessionValueUpdate<String>,
    sandbox_mode: CutexSessionValueUpdate<String>,
    model_defaults: CutexSessionValueUpdate<String>,
    reasoning_defaults: CutexSessionValueUpdate<String>,
    runtime_backend: Option<CutexSessionRuntimeBackend>,
    managed_cwd: CutexSessionValueUpdate<String>,
    default_cli_args: Option<Vec<String>>,
    default_cli_args_label: Option<String>,
    agent_groups: Option<Vec<String>>,
    agent_groups_label: Option<String>,
    exposed_to_backend: Option<bool>,
    quick_action: Option<CutexSessionQuickActionMode>,
}

impl SessionSettingsDraft {
    pub(super) fn stage(
        &mut self,
        snapshot: &SessionSettingsSnapshot,
        field: SessionSettingsField,
        value: Option<String>,
    ) -> anyhow::Result<()> {
        match field {
            SessionSettingsField::AgentName => {
                let name = value.unwrap_or_default().trim().to_string();
                if name.is_empty() {
                    anyhow::bail!("Agent name cannot be empty");
                }
                self.agent_name = (name != snapshot.agent_name).then_some(name);
                return Ok(());
            }
            SessionSettingsField::RuntimeBackend => {
                let value = value
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("runtime backend cannot inherit"))?;
                let backend = parse_cutex_session_runtime_backend(value)?;
                self.runtime_backend = (backend != snapshot.runtime_backend).then_some(backend);
                return Ok(());
            }
            SessionSettingsField::ManagedCwd => {
                self.managed_cwd = match value {
                    Some(value) => {
                        let normalized = normalize_cutex_session_managed_cwd_path(&value)?;
                        if snapshot.managed_cwd.as_deref() == Some(normalized.as_str()) {
                            CutexSessionValueUpdate::Unchanged
                        } else {
                            CutexSessionValueUpdate::Set(normalized)
                        }
                    }
                    None if snapshot.managed_cwd.is_none() => CutexSessionValueUpdate::Unchanged,
                    None => CutexSessionValueUpdate::Clear,
                };
                return Ok(());
            }
            SessionSettingsField::ExtraCliArgs => {
                let args = parse_cli_args_value(value.as_deref().unwrap_or_default())?;
                if args == snapshot.default_cli_args {
                    self.default_cli_args = None;
                    self.default_cli_args_label = None;
                } else {
                    self.default_cli_args_label = Some(cli_args_label(&args));
                    self.default_cli_args = Some(args);
                }
                return Ok(());
            }
            SessionSettingsField::AgentGroups => {
                let raw = value.unwrap_or_default();
                let groups = normalize_agent_groups(
                    raw.split(|character: char| character == ',' || character.is_whitespace())
                        .map(str::to_string)
                        .collect(),
                );
                if groups.is_empty() && !snapshot.agent_groups.is_empty() {
                    anyhow::bail!("At least one non-empty group is required");
                }
                if groups == snapshot.agent_groups {
                    self.agent_groups = None;
                    self.agent_groups_label = None;
                } else {
                    self.agent_groups_label = Some(groups.join(", "));
                    self.agent_groups = Some(groups);
                }
                return Ok(());
            }
            SessionSettingsField::WorkbenchVisibility => {
                let visible = match value.as_deref() {
                    Some("visible") => true,
                    Some("hidden") => false,
                    Some(other) => anyhow::bail!("unsupported workbench visibility: {other}"),
                    None => anyhow::bail!("workbench visibility cannot inherit"),
                };
                self.exposed_to_backend =
                    (visible != snapshot.exposed_to_backend).then_some(visible);
                return Ok(());
            }
            SessionSettingsField::QuickAction => {
                let value = value
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("quick action cannot inherit"))?;
                let mode = parse_cutex_session_quick_action_mode(value)?;
                self.quick_action = (mode != snapshot.quick_action).then_some(mode);
                return Ok(());
            }
            _ => {}
        }
        let update = if value.as_deref() == snapshot.value(field) {
            CutexSessionValueUpdate::Unchanged
        } else if let Some(value) = value {
            CutexSessionValueUpdate::Set(value)
        } else {
            CutexSessionValueUpdate::Clear
        };
        *self.field_mut(field) = update;
        Ok(())
    }

    pub(super) fn value<'a>(
        &'a self,
        snapshot: &'a SessionSettingsSnapshot,
        field: SessionSettingsField,
    ) -> Option<&'a str> {
        match field {
            SessionSettingsField::AgentName => {
                return self.agent_name.as_deref().or_else(|| snapshot.value(field));
            }
            SessionSettingsField::RuntimeBackend => {
                return self
                    .runtime_backend
                    .map(runtime_backend_short_label)
                    .or_else(|| snapshot.value(field));
            }
            SessionSettingsField::ManagedCwd => {
                return match &self.managed_cwd {
                    CutexSessionValueUpdate::Unchanged => snapshot.value(field),
                    CutexSessionValueUpdate::Set(value) => Some(value.as_str()),
                    CutexSessionValueUpdate::Clear => None,
                };
            }
            SessionSettingsField::ExtraCliArgs => {
                return self
                    .default_cli_args_label
                    .as_deref()
                    .or_else(|| snapshot.value(field));
            }
            SessionSettingsField::AgentGroups => {
                return self
                    .agent_groups_label
                    .as_deref()
                    .or_else(|| snapshot.value(field));
            }
            SessionSettingsField::WorkbenchVisibility => {
                return self
                    .exposed_to_backend
                    .map(|visible| if visible { "visible" } else { "hidden" })
                    .or_else(|| snapshot.value(field));
            }
            SessionSettingsField::QuickAction => {
                return self
                    .quick_action
                    .map(CutexSessionQuickActionMode::label)
                    .or_else(|| snapshot.value(field));
            }
            _ => {}
        }
        match self.field(field) {
            CutexSessionValueUpdate::Unchanged => snapshot.value(field),
            CutexSessionValueUpdate::Set(value) => Some(value.as_str()),
            CutexSessionValueUpdate::Clear => None,
        }
    }

    pub(super) fn field_is_dirty(&self, field: SessionSettingsField) -> bool {
        match field {
            SessionSettingsField::AgentName => self.agent_name.is_some(),
            SessionSettingsField::RuntimeBackend => self.runtime_backend.is_some(),
            SessionSettingsField::ManagedCwd => {
                !matches!(self.managed_cwd, CutexSessionValueUpdate::Unchanged)
            }
            SessionSettingsField::ExtraCliArgs => self.default_cli_args.is_some(),
            SessionSettingsField::AgentGroups => self.agent_groups.is_some(),
            SessionSettingsField::WorkbenchVisibility => self.exposed_to_backend.is_some(),
            SessionSettingsField::QuickAction => self.quick_action.is_some(),
            _ => !matches!(self.field(field), CutexSessionValueUpdate::Unchanged),
        }
    }

    pub(super) fn dirty_count(&self) -> usize {
        [
            SessionSettingsField::AgentName,
            SessionSettingsField::Profile,
            SessionSettingsField::PermissionPreset,
            SessionSettingsField::ApprovalPolicy,
            SessionSettingsField::SandboxMode,
            SessionSettingsField::Model,
            SessionSettingsField::ReasoningEffort,
            SessionSettingsField::RuntimeBackend,
            SessionSettingsField::ManagedCwd,
            SessionSettingsField::ExtraCliArgs,
            SessionSettingsField::AgentGroups,
            SessionSettingsField::WorkbenchVisibility,
            SessionSettingsField::QuickAction,
        ]
        .into_iter()
        .filter(|field| self.field_is_dirty(*field))
        .count()
    }

    pub(super) fn is_dirty(&self) -> bool {
        self.dirty_count() > 0
    }

    pub(super) fn runtime_defaults_patch(&self) -> CutexSessionRuntimeDefaultsPatch {
        CutexSessionRuntimeDefaultsPatch {
            runtime_backend: self.runtime_backend,
            managed_cwd: self.managed_cwd.clone(),
            permission_defaults: self.permission_defaults.clone(),
            approval_policy: self.approval_policy.clone(),
            sandbox_mode: self.sandbox_mode.clone(),
            model_defaults: self.model_defaults.clone(),
            reasoning_defaults: self.reasoning_defaults.clone(),
            default_cli_args: self.default_cli_args.clone(),
            ..CutexSessionRuntimeDefaultsPatch::default()
        }
    }

    pub(super) fn agent_name(&self) -> Option<&str> {
        self.agent_name.as_deref()
    }

    pub(super) fn profile_update(&self) -> &CutexSessionValueUpdate<String> {
        &self.profile
    }

    pub(super) fn routing_patch(&self) -> CutexSessionRoutingPatch {
        CutexSessionRoutingPatch {
            agent_groups: self.agent_groups.clone(),
            exposed_to_backend: self.exposed_to_backend,
            quick_action: self.quick_action,
        }
    }

    pub(super) fn routing_is_dirty(&self) -> bool {
        self.agent_groups.is_some()
            || self.exposed_to_backend.is_some()
            || self.quick_action.is_some()
    }

    pub(super) fn agent_groups_are_dirty(&self) -> bool {
        self.agent_groups.is_some()
    }

    pub(super) fn launch_actions_are_dirty(&self) -> bool {
        self.runtime_backend.is_some()
            || !matches!(self.managed_cwd, CutexSessionValueUpdate::Unchanged)
    }

    fn effective_working_directory(&self, snapshot: &SessionSettingsSnapshot) -> String {
        match &self.managed_cwd {
            CutexSessionValueUpdate::Unchanged => snapshot.working_directory.clone(),
            CutexSessionValueUpdate::Set(path) => path.clone(),
            CutexSessionValueUpdate::Clear => snapshot.session_cwd.clone(),
        }
    }

    pub(super) fn runtime_defaults_are_dirty(&self) -> bool {
        [
            SessionSettingsField::RuntimeBackend,
            SessionSettingsField::ManagedCwd,
            SessionSettingsField::ExtraCliArgs,
            SessionSettingsField::PermissionPreset,
            SessionSettingsField::ApprovalPolicy,
            SessionSettingsField::SandboxMode,
            SessionSettingsField::Model,
            SessionSettingsField::ReasoningEffort,
        ]
        .into_iter()
        .any(|field| self.field_is_dirty(field))
    }

    fn field(&self, field: SessionSettingsField) -> &CutexSessionValueUpdate<String> {
        match field {
            SessionSettingsField::Profile => &self.profile,
            SessionSettingsField::PermissionPreset => &self.permission_defaults,
            SessionSettingsField::ApprovalPolicy => &self.approval_policy,
            SessionSettingsField::SandboxMode => &self.sandbox_mode,
            SessionSettingsField::Model => &self.model_defaults,
            SessionSettingsField::ReasoningEffort => &self.reasoning_defaults,
            SessionSettingsField::AgentName
            | SessionSettingsField::RuntimeBackend
            | SessionSettingsField::ManagedCwd
            | SessionSettingsField::ExtraCliArgs
            | SessionSettingsField::AgentGroups
            | SessionSettingsField::WorkbenchVisibility
            | SessionSettingsField::QuickAction => {
                unreachable!("routing fields do not use optional string patches")
            }
        }
    }

    fn field_mut(&mut self, field: SessionSettingsField) -> &mut CutexSessionValueUpdate<String> {
        match field {
            SessionSettingsField::Profile => &mut self.profile,
            SessionSettingsField::PermissionPreset => &mut self.permission_defaults,
            SessionSettingsField::ApprovalPolicy => &mut self.approval_policy,
            SessionSettingsField::SandboxMode => &mut self.sandbox_mode,
            SessionSettingsField::Model => &mut self.model_defaults,
            SessionSettingsField::ReasoningEffort => &mut self.reasoning_defaults,
            SessionSettingsField::AgentName
            | SessionSettingsField::RuntimeBackend
            | SessionSettingsField::ManagedCwd
            | SessionSettingsField::ExtraCliArgs
            | SessionSettingsField::AgentGroups
            | SessionSettingsField::WorkbenchVisibility
            | SessionSettingsField::QuickAction => {
                unreachable!("routing fields do not use optional string patches")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GlobalSettingsField {
    Identity(Option<Provider>, IdentityField),
    ManagedSessions,
    DockerSudo,
    DefaultProfile,
    DefaultProfileDirectLaunch,
    DefaultModel,
    DefaultReasoning,
    DefaultNotification,
    ClaudeUsageRefresh,
    TerminalColors,
    DefaultLocalHostFilter,
    AgentSort,
    SessionSort,
    ProjectSort,
    JobSort,
    ProxyEnabled,
    ProxyUrl,
    ProxyNoProxy,
    NotifyServiceUrl,
    NotifyServiceToken,
    NotifyIdleTimeout,
    NotifyComposerTimeout,
    NotifyApprovalTimeout,
    NotifyStartupTimeout,
    NotifyEvents,
    NotifyMessageContent,
    NotifyPreviewChars,
    DesktopNotifyEnabled,
    DesktopNotifyPort,
    DesktopNotifyToken,
    RateLimitThresholdWarning,
    RateLimitModelNudge,
    AgentBusEnabled,
    AgentBusPort,
    AgentBusToken,
}

impl GlobalSettingsField {
    pub(super) fn editor_kind(self) -> SessionSettingsEditorKind {
        match self {
            Self::Identity(_,f)=>if f.choices().is_empty(){SessionSettingsEditorKind::Text}else{SessionSettingsEditorKind::Choice},
            Self::AgentSort | Self::SessionSort | Self::ProjectSort | Self::JobSort | Self::DefaultLocalHostFilter | Self::TerminalColors | Self::DefaultReasoning | Self::DefaultNotification | Self::ManagedSessions
            | Self::DockerSudo
            | Self::DefaultProfile
            | Self::DefaultProfileDirectLaunch
            | Self::ProxyEnabled
            | Self::NotifyMessageContent
            | Self::DesktopNotifyEnabled
            | Self::RateLimitThresholdWarning
            | Self::RateLimitModelNudge
            | Self::AgentBusEnabled => SessionSettingsEditorKind::Choice,
            Self::ClaudeUsageRefresh
            | Self::DefaultModel | Self::ProxyUrl
            | Self::ProxyNoProxy
            | Self::NotifyServiceUrl
            | Self::NotifyIdleTimeout
            | Self::NotifyComposerTimeout
            | Self::NotifyApprovalTimeout
            | Self::NotifyStartupTimeout
            | Self::NotifyPreviewChars
            | Self::DesktopNotifyPort
            | Self::AgentBusPort => SessionSettingsEditorKind::Text,
            Self::NotifyEvents => SessionSettingsEditorKind::Tags,
            Self::NotifyServiceToken | Self::DesktopNotifyToken | Self::AgentBusToken => {
                SessionSettingsEditorKind::Secret
            }
        }
    }
}

const ENABLED_CHOICES: &[(&str, Option<&str>)] =
    &[("Enabled", Some("enabled")), ("Disabled", Some("disabled"))];
const NOTIFY_MESSAGE_CONTENT_CHOICES: &[(&str, Option<&str>)] = &[
    ("Default", None),
    ("None", Some("none")),
    ("Preview", Some("preview")),
    ("Full", Some("full")),
];
const RATE_LIMIT_MODE_CHOICES: &[(&str, Option<&str>)] = &[
    ("Default", None),
    ("Off", Some("off")),
    ("Daily", Some("daily")),
    ("Always", Some("always")),
];

#[derive(Clone, PartialEq, Eq)]
pub(super) enum SecretSettingsAction {
    Keep,
    Replace(String),
    Clear,
}

impl fmt::Debug for SecretSettingsAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Keep => formatter.write_str("Keep"),
            Self::Replace(_) => formatter.write_str("Replace(<redacted>)"),
            Self::Clear => formatter.write_str("Clear"),
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct GlobalSettingsSnapshot {
    claude_profiles: Vec<String>,
    config: CodezConfig,
    profile_names: Vec<String>,
    inherited_defaults: std::collections::BTreeMap<String, cutex::profiles::model::NewSessionDefaults>,
}

impl GlobalSettingsSnapshot {
    #[cfg(test)]
    pub(super) fn from_config(config: &CodezConfig) -> Self {
        Self::from_config_with_profiles(config, &[])
    }

    pub(super) fn from_config_with_profiles(
        config: &CodezConfig,
        profile_names: &[String],
    ) -> Self {
        Self {
            claude_profiles: super::account_store::load_store_read_only().map(|s|s.accounts.into_iter().filter(|a|a.cli_kind==cutex::profiles::model::CliKind::Claude).map(|a|a.name).collect()).unwrap_or_default(),
            config: config.clone(),
            profile_names: profile_names.to_vec(),
            inherited_defaults: profile_names.iter().filter_map(|name| {
                cutex::config::new_session::inherited(name).ok().map(|values| (name.clone(), values))
            }).collect(),
        }
    }

    pub(super) fn profile_names(&self) -> &[String] {
        &self.profile_names
    }

    pub(super) fn other_sort_defaults(&self) -> [cutex::profiles::list_preferences::ListSort; 2] {
        [self.config.project_sort, self.config.job_sort]
    }

    pub(super) fn list_sort_defaults(&self) -> (cutex::profiles::list_preferences::ListSort, cutex::profiles::list_preferences::ListSort) {
        (self.config.agent_sort, self.config.session_sort)
    }

    pub(super) fn default_local_host_filter(&self) -> bool {
        self.config.default_local_host_filter
    }

    pub(super) fn default_profile_name(&self) -> Option<&str> {
        self.config.default_profile.as_deref()
    }

    pub(super) fn choices(&self, field: GlobalSettingsField) -> Vec<SessionSettingsChoice> {
        match field {
            GlobalSettingsField::Identity(_,f)=>return f.choices().into_iter().map(|(label,value)|SessionSettingsChoice{label:label.into(),value:Some(value.into())}).collect(),
            GlobalSettingsField::DefaultReasoning => &[("Follow profile", None), ("none", Some("none")), ("minimal", Some("minimal")), ("low", Some("low")), ("medium", Some("medium")), ("high", Some("high")), ("xhigh", Some("xhigh"))][..],
            GlobalSettingsField::TerminalColors => &[("Auto", Some("auto")), ("TrueColor", Some("truecolor"))][..],
            GlobalSettingsField::DefaultNotification => &[("CIAO!", Some("important")), ("ON", Some("normal")), ("OFF", Some("off"))][..],
            GlobalSettingsField::AgentSort => &[("Panel default", Some("default")), ("Name A–Z", Some("name_asc")), ("Name Z–A", Some("name_desc")), ("Pin first", Some("pinned")), ("Project first", Some("project"))][..],
            GlobalSettingsField::ProjectSort | GlobalSettingsField::JobSort => &[("Panel default", Some("default")), ("A–Z", Some("name_asc")), ("Z–A", Some("name_desc"))][..],
            GlobalSettingsField::SessionSort => &[("Panel default", Some("default")), ("Name A–Z", Some("name_asc")), ("Name Z–A", Some("name_desc")), ("Project first", Some("project"))][..],
            GlobalSettingsField::DefaultLocalHostFilter => &[("Local", Some("local")), ("All hosts", Some("all"))][..],
            GlobalSettingsField::ManagedSessions
            | GlobalSettingsField::DockerSudo
            | GlobalSettingsField::DefaultProfileDirectLaunch
            | GlobalSettingsField::ProxyEnabled
            | GlobalSettingsField::DesktopNotifyEnabled
            | GlobalSettingsField::AgentBusEnabled => ENABLED_CHOICES,
            GlobalSettingsField::NotifyMessageContent => NOTIFY_MESSAGE_CONTENT_CHOICES,
            GlobalSettingsField::RateLimitThresholdWarning
            | GlobalSettingsField::RateLimitModelNudge => RATE_LIMIT_MODE_CHOICES,
            GlobalSettingsField::DefaultProfile => {
                return std::iter::once(SessionSettingsChoice {
                    label: "None".to_string(),
                    value: None,
                })
                .chain(self.profile_names.iter().map(|name| SessionSettingsChoice {
                    label: name.clone(),
                    value: Some(name.clone()),
                }))
                .collect();
            }
            GlobalSettingsField::ClaudeUsageRefresh
            | GlobalSettingsField::DefaultModel | GlobalSettingsField::ProxyUrl
            | GlobalSettingsField::ProxyNoProxy
            | GlobalSettingsField::NotifyServiceUrl
            | GlobalSettingsField::NotifyServiceToken
            | GlobalSettingsField::NotifyIdleTimeout
            | GlobalSettingsField::NotifyComposerTimeout
            | GlobalSettingsField::NotifyApprovalTimeout
            | GlobalSettingsField::NotifyStartupTimeout
            | GlobalSettingsField::NotifyEvents
            | GlobalSettingsField::NotifyPreviewChars
            | GlobalSettingsField::DesktopNotifyPort
            | GlobalSettingsField::DesktopNotifyToken
            | GlobalSettingsField::AgentBusPort
            | GlobalSettingsField::AgentBusToken => &[],
        }
        .iter()
        .map(|(label, value)| SessionSettingsChoice {
            label: (*label).to_string(),
            value: value.map(str::to_string),
        })
        .collect()
    }

    pub(super) fn categories(&self, draft: &GlobalSettingsDraft) -> Vec<SessionTuiSettingCategory> {
        let mut claude_usage = self.editable_option(
            "Claude usage refresh (m)",
            GlobalSettingsField::ClaudeUsageRefresh,
            draft,
        );
        claude_usage.presentation.detail = Some(
            "How often cute-codex asks a Claude runtime for OAuth usage while its TUI is open. 1–60 minutes; applies to newly launched Agents.".into(),
        );
        vec![
            SessionTuiSettingCategory::new("Profiles", vec![
                SessionTuiSettingOption {
                    navigation: Some(super::session_tui_input::Command::Profiles),
                    ..SessionTuiSettingOption::new("Manage profiles", "Enter to open")
                },
            ]),
            SessionTuiSettingCategory::new("Defaults", vec![
                self.editable_option("Default profile", GlobalSettingsField::DefaultProfile, draft),
                self.editable_option("Skip new-session picker", GlobalSettingsField::DefaultProfileDirectLaunch, draft),
                self.editable_option("Model (selected default profile)", GlobalSettingsField::DefaultModel, draft),
                if effective_optional_value(self.config.default_profile.as_ref(),&draft.default_profile).is_some_and(|p|self.claude_profiles.contains(&p)) {SessionTuiSettingOption::new("Reasoning effort","Set in Claude Profile")}else{self.editable_option("Reasoning effort", GlobalSettingsField::DefaultReasoning, draft)},
                self.editable_option("Notification", GlobalSettingsField::DefaultNotification, draft),
                claude_usage,
            ]),
            SessionTuiSettingCategory::new("Network", vec![
                self.editable_option("Proxy enabled", GlobalSettingsField::ProxyEnabled, draft),
                self.editable_option("Proxy URL", GlobalSettingsField::ProxyUrl, draft),
                self.editable_option("Bypass proxy", GlobalSettingsField::ProxyNoProxy, draft),
            ]),
            SessionTuiSettingCategory::new("Notifications", self.notification_display_options()),
            SessionTuiSettingCategory::new("Appearance", {
                let mut terminal = self.editable_option("Terminal colors", GlobalSettingsField::TerminalColors, draft);
                terminal.presentation.detail = Some("Auto preserves terminal detection. TrueColor supplies COLORTERM=truecolor to Cutex foreground TUI launches, including SSH; background runtimes are unchanged.".into());
                let mut host = self.editable_option("Default host filter", GlobalSettingsField::DefaultLocalHostFilter, draft);
                host.presentation.detail = Some("Local starts Agents and Sessions on this host. All hosts includes remote rows. Applies next time Cutex opens; the filter remains changeable.".into());
                let mut agents = self.editable_option("Agent sort", GlobalSettingsField::AgentSort, draft);
                agents.presentation.detail = Some("Next launch: Default keeps status/pinned order. Pin first puts pinned Agents first. Project first groups assigned rows by project name, then agent name; unassigned rows follow. Alt+S changes the current list without changing this preference.".into());
                let mut sessions = self.editable_option("Session sort", GlobalSettingsField::SessionSort, draft);
                sessions.presentation.detail = Some("Next launch: Default keeps recency order. Name sorting applies to loaded rows; Alt+L loads more history. Alt+S changes the current list.".into());
                let mut options = vec![terminal, host, agents, sessions];
                for (label, field, detail) in [
                    ("Project sort", GlobalSettingsField::ProjectSort, "Next launch: order Projects by display name. Alt+S changes the current list."),
                    ("Job sort", GlobalSettingsField::JobSort, "Order Jobs by action name within the loaded page. Alt+S changes the current list."),
                ] {
                    let mut option = self.editable_option(label, field, draft);
                    option.presentation.detail = Some(detail.into());
                    options.push(option);
                }
                let a=draft.appearance.as_ref().unwrap_or(&self.config.appearance);
                for provider in Provider::ALL.into_iter().map(Some).chain(std::iter::once(None)) {
                    for f in [IdentityField::Color,IdentityField::Bold] {
                        let mut option=self.editable_option(f.label(),GlobalSettingsField::Identity(provider,f),draft);
                        option.presentation.group=Some(provider.map(|p|p.label()).unwrap_or("Remote @host"));
                        let style=provider.map(|p|a.provider_style(p)).unwrap_or_else(||a.host_suffix.clone());
                        option.presentation.style=Some(super::identity_styles::render(&style));
                        option.presentation.detail=Some("Display only. Empty color resets this style to its default. Advanced dim/italic/underlined fields are available in the configuration.".into());
                        options.push(option);
                    }
                }
                options.extend(self.appearance_display_options());
                options
            }),
            SessionTuiSettingCategory::new("Messages", vec![
                SessionTuiSettingOption::new("Sender identity", "Structured Agent Bus provenance"),
                SessionTuiSettingOption::new("Message display", "Sender and delivery timing are shown with each message"),
            ]),
            SessionTuiSettingCategory::new("Services", vec![
                SessionTuiSettingOption {
                    navigation: Some(super::session_tui_input::Command::Services),
                    presentation: SettingPresentation { detail: Some("Explicit local Agent Bus / Management start, stop, restart and status. Does not stop agent runtimes or Job Service.".into()), ..Default::default() },
                    ..SessionTuiSettingOption::new("Manage services", "Enter to open")
                },
                SessionTuiSettingOption::new("Agent Bus", "Required for managed agents"),
                self.editable_option("Agent Bus port", GlobalSettingsField::AgentBusPort, draft),
                self.editable_option("Agent Bus token", GlobalSettingsField::AgentBusToken, draft),
            ]),
            SessionTuiSettingCategory::new("Hosts / Connections", vec![SessionTuiSettingOption {
                navigation: Some(super::session_tui_input::Command::Hosts),
                presentation: SettingPresentation { detail: Some("Manage display names, identities, SSH targets, ports and credential files. Test verifies an existing tunnel without starting runtimes.".into()), ..Default::default() },
                ..SessionTuiSettingOption::new("Manage hosts", "Enter to open")
            }]),

        ]
    }

    fn notification_display_options(&self) -> Vec<SessionTuiSettingOption> {
        let labels = cutex::notify::session::labels();
        let error = labels.as_ref().err().map(|e| format!("Cannot read notification labels: {e}"));
        let labels = labels.unwrap_or_default();
        let mut options: Vec<_> = [("Important", labels.important, labels.styles.important),
         ("Normal", labels.normal, labels.styles.normal),
         ("Off", labels.off, labels.styles.off)].into_iter().map(|(name, value, style)| {
            let mut option = SessionTuiSettingOption::new(name, value);
            let mut rendered = ratatui::style::Style::new().fg(style.fg.parse().unwrap_or(ratatui::style::Color::White));
            if style.bold { rendered = rendered.add_modifier(ratatui::style::Modifier::BOLD); }
            option.presentation = SettingPresentation {
                group: Some("Session priority labels"),
                detail: Some(error.clone().unwrap_or_else(|| "Read-only label preview · ~/.cutex/notifications/config.json".into())),
                style: Some(rendered), ..Default::default()
            };
            option
        }).collect();
        match cutex::notify::outbound::config() {
            Ok(config) => {
                for (name,value) in [
                    ("Enabled", if config.enabled { "On" } else { "Off" }.to_string()),
                    ("URL", config.url.unwrap_or_else(|| "Not configured".into())),
                    ("Token", if config.token.is_some() { "Configured" } else { "Not configured" }.to_string()),
                    ("Events", config.events.join(", ")),
                ] {
                    let mut option = SessionTuiSettingOption::new(name, value);
                    option.presentation.group = Some("Webhook delivery");
                    option.presentation.detail = Some("Configure: cutex notify configure <file.json> · status/test/deliveries/retry · ~/.cutex/notifications/outbound.json".into());
                    options.push(option);
                }
            }
            Err(_) => {
                let mut option = SessionTuiSettingOption::new("Configuration", "Invalid; inspect outbound.json");
                option.presentation.group = Some("Webhook delivery");
                options.push(option);
            }
        }
        options
    }

    fn appearance_display_options(&self) -> Vec<SessionTuiSettingOption> {
        use super::session_tui_layout as theme;
        use ratatui::style::Color;
        let mut options = Vec::new();
        for (name, color) in [("Brand", theme::brand()), ("Accent", theme::accent()),
            ("Focus", theme::focus()), ("Warning", theme::warning()),
            ("Selection", theme::selection()), ("Text", theme::text()),
            ("Muted", theme::muted()), ("Success", theme::success()), ("Error", theme::error())] {
            let value = match color {
                Color::Rgb(r,g,b) => format!("#{r:02X}{g:02X}{b:02X}"),
                _ => format!("Terminal {color:?}"),
            };
            let mut option = SessionTuiSettingOption::new(name, value);
            option.presentation = SettingPresentation {
                group: Some("Theme colors"), swatch: Some(color),
                detail: Some("Effective Cutex theme · read-only · ~/.cutex/theme.json · changes apply next launch".into()),
                ..Default::default()
            };
            options.push(option);
        }
        for item in cutex::profiles::materialize::normalize_custom_status_items(&self.config.custom_status_items) {
            let mut option = SessionTuiSettingOption::new("Status item", "");
            option.presentation = SettingPresentation {
                group: Some("Custom status items"), name: Some(item.title.clone()),
                detail: Some(format!("ID: {} · ~/.cutex/config.json · custom_status_items", item.id)),
                ..Default::default()
            };
            options.push(option);
        }
        if !options.iter().any(|option| option.presentation.detail.as_deref().is_some_and(|detail| detail.starts_with("ID: cutex_notification ·"))) {
            let mut option = SessionTuiSettingOption::new("Status item", "");
            option.presentation = SettingPresentation {
                group: Some("Custom status items"), name: Some("Cutex Notification".into()),
                detail: Some("ID: cutex_notification · built-in cute-codex status item".into()),
                ..Default::default()
            };
            options.push(option);
        }
        options
    }

    fn editable_option(
        &self,
        label: &'static str,
        field: GlobalSettingsField,
        draft: &GlobalSettingsDraft,
    ) -> SessionTuiSettingOption {
        let mut value = draft.value(self, field);
        if matches!(field, GlobalSettingsField::DefaultModel | GlobalSettingsField::DefaultReasoning) {
            if value.is_empty() {
                let name = effective_optional_value(self.config.default_profile.as_ref(), &draft.default_profile);
                let inherited = name.as_ref().and_then(|n| self.inherited_defaults.get(n));
                let resolved = inherited.and_then(|d| if field == GlobalSettingsField::DefaultModel { d.model.as_deref() } else { d.reasoning.as_deref() });
                value = format!("{} · follow profile", resolved.unwrap_or("Default"));
            } else {
                value.push_str(" · new-session override");
            }
        }
        SessionTuiSettingOption::global_editable(
            label,
            value,
            field,
            draft.field_is_dirty(field),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct GlobalSettingsDraft {
    appearance: Option<Appearance>,
    identity_dirty: Vec<(Option<Provider>,IdentityField)>,
    terminal_truecolor: Option<bool>,
    default_local_host_filter: Option<bool>,
    agent_sort: Option<cutex::profiles::list_preferences::ListSort>,
    session_sort: Option<cutex::profiles::list_preferences::ListSort>,
    project_sort: Option<cutex::profiles::list_preferences::ListSort>,
    job_sort: Option<cutex::profiles::list_preferences::ListSort>,

    managed_sessions: Option<bool>,
    docker_sudo: Option<bool>,
    default_profile: ConfigValueUpdate<String>,
    default_profile_direct_launch: Option<bool>,
    new_session_defaults: Option<std::collections::BTreeMap<String, cutex::profiles::model::NewSessionDefaults>>,
    new_session_notification: Option<cutex::notify::session::Level>,
    claude_usage_refresh_minutes: Option<u8>,
    default_model_dirty: bool,
    default_reasoning_dirty: bool,
    proxy_enabled: Option<bool>,
    proxy_url: ConfigValueUpdate<String>,
    proxy_no_proxy: ConfigValueUpdate<String>,
    notify_service_url: ConfigValueUpdate<String>,
    notify_service_token: ConfigValueUpdate<String>,
    notify_idle_timeout: ConfigValueUpdate<u64>,
    notify_composer_timeout: ConfigValueUpdate<u64>,
    notify_approval_timeout: ConfigValueUpdate<u64>,
    notify_startup_timeout: ConfigValueUpdate<u64>,
    notify_events: ConfigValueUpdate<Vec<String>>,
    notify_message_content: ConfigValueUpdate<String>,
    notify_preview_chars: ConfigValueUpdate<u64>,
    desktop_notify_enabled: Option<bool>,
    desktop_notify_port: ConfigValueUpdate<u16>,
    desktop_notify_token: ConfigValueUpdate<String>,
    rate_limit_threshold_warning: ConfigValueUpdate<String>,
    rate_limit_model_nudge: ConfigValueUpdate<String>,
    agent_bus_enabled: Option<bool>,
    agent_bus_port: ConfigValueUpdate<u16>,
    agent_bus_token: ConfigValueUpdate<String>,
}

impl GlobalSettingsDraft {
    pub(super) fn stage(
        &mut self,
        snapshot: &GlobalSettingsSnapshot,
        field: GlobalSettingsField,
        value: Option<String>,
    ) -> anyhow::Result<()> {
        match field {
            GlobalSettingsField::Identity(provider,f)=>{
                let mut a=self.appearance.clone().unwrap_or_else(||snapshot.config.appearance.clone());
                let base=provider.map(|p|a.provider_style(p)).unwrap_or_else(||a.host_suffix.clone());
                let mut options=ProfileOptions{style:Some(base.clone()),..Default::default()};
                f.apply(&mut options,value.as_deref().unwrap_or("inherit"),&base)?;
                if let Some(provider)=provider {if let Some(style)=options.style {a.providers.insert(provider,style);}else{a.providers.remove(&provider);}} else {a.host_suffix=options.style.unwrap_or_else(||Appearance::default().host_suffix);}
                self.identity_dirty.clear();
                for p in Provider::ALL.into_iter().map(Some).chain(std::iter::once(None)) {
                    let current=ProfileOptions{style:Some(p.map(|p|a.provider_style(p)).unwrap_or_else(||a.host_suffix.clone())),..Default::default()};
                    let original=ProfileOptions{style:Some(p.map(|p|snapshot.config.appearance.provider_style(p)).unwrap_or_else(||snapshot.config.appearance.host_suffix.clone())),..Default::default()};
                    for f in [IdentityField::Color,IdentityField::Bold] {if f.value(&current)!=f.value(&original){self.identity_dirty.push((p,f));}}
                }
                self.appearance=(!self.identity_dirty.is_empty()).then_some(a);
            },
            GlobalSettingsField::DefaultModel | GlobalSettingsField::DefaultReasoning => {
                let name = effective_optional_value(snapshot.config.default_profile.as_ref(), &self.default_profile)
                    .ok_or_else(|| anyhow::anyhow!("Select a default profile first"))?;
                let value = value.filter(|s| !s.trim().is_empty());
                if let Some(value) = &value {
                    anyhow::ensure!(!value.chars().any(char::is_control), "Control characters are not allowed");
                    if field == GlobalSettingsField::DefaultReasoning {
                        anyhow::ensure!(["none", "minimal", "low", "medium", "high", "xhigh"].contains(&value.as_str()), "Unknown reasoning effort");
                    }
                }
                let mut defaults = self.new_session_defaults.clone().unwrap_or_else(|| snapshot.config.new_session_defaults.clone());
                let entry = defaults.entry(name.clone()).or_default();
                if field == GlobalSettingsField::DefaultModel { entry.model = value; } else { entry.reasoning = value; }
                if entry.model.is_none() && entry.reasoning.is_none() { defaults.remove(&name); }
                self.default_model_dirty = defaults.keys().chain(snapshot.config.new_session_defaults.keys()).any(|name| defaults.get(name).and_then(|d| d.model.as_ref()) != snapshot.config.new_session_defaults.get(name).and_then(|d| d.model.as_ref()));
                self.default_reasoning_dirty = defaults.keys().chain(snapshot.config.new_session_defaults.keys()).any(|name| defaults.get(name).and_then(|d| d.reasoning.as_ref()) != snapshot.config.new_session_defaults.get(name).and_then(|d| d.reasoning.as_ref()));
                self.new_session_defaults = (defaults != snapshot.config.new_session_defaults).then_some(defaults);
            }
            GlobalSettingsField::ClaudeUsageRefresh => {
                let minutes: u8 = value
                    .as_deref()
                    .unwrap_or_default()
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Enter a whole number of minutes"))?;
                anyhow::ensure!(
                    (1..=60).contains(&minutes),
                    "Claude usage refresh interval must be 1–60 minutes"
                );
                self.claude_usage_refresh_minutes = (minutes
                    != snapshot.config.claude_usage_refresh_minutes)
                    .then_some(minutes);
            }
            GlobalSettingsField::ProjectSort | GlobalSettingsField::JobSort => {
                use cutex::profiles::list_preferences::ListSort;
                let order = ListSort::parse(value.as_deref().unwrap_or("default"))?;
                anyhow::ensure!(matches!(order, ListSort::Default | ListSort::NameAsc | ListSort::NameDesc), "Expected default, name_asc or name_desc");
                match field {
                    GlobalSettingsField::ProjectSort => self.project_sort = (order != snapshot.config.project_sort).then_some(order),
                    _ => self.job_sort = (order != snapshot.config.job_sort).then_some(order),
                }
            }
            GlobalSettingsField::AgentSort | GlobalSettingsField::SessionSort => {
                let order = cutex::profiles::list_preferences::ListSort::parse(value.as_deref().unwrap_or("default"))?;
                if field == GlobalSettingsField::AgentSort {
                    self.agent_sort = (order != snapshot.config.agent_sort).then_some(order);
                } else {
                    anyhow::ensure!(order != cutex::profiles::list_preferences::ListSort::Pinned, "Sessions do not expose a Pin state");
                    self.session_sort = (order != snapshot.config.session_sort).then_some(order);
                }
            }
            GlobalSettingsField::DefaultLocalHostFilter => {
                let enabled = match value.as_deref() {
                    Some("local") => true,
                    Some("all") => false,
                    _ => anyhow::bail!("Expected local or all"),
                };
                self.default_local_host_filter = (enabled != snapshot.config.default_local_host_filter).then_some(enabled);
            }
            GlobalSettingsField::TerminalColors => {
                let enabled = match value.as_deref() {
                    Some("auto") => false,
                    Some("truecolor") => true,
                    _ => anyhow::bail!("Expected auto or truecolor"),
                };
                self.terminal_truecolor = (enabled != snapshot.config.terminal_truecolor).then_some(enabled);
            }
            GlobalSettingsField::DefaultNotification => {
                let level = match value.as_deref() {
                    Some("important") => cutex::notify::session::Level::Important,
                    Some("normal") => cutex::notify::session::Level::Normal,
                    Some("off") => cutex::notify::session::Level::Off,
                    _ => anyhow::bail!("Unknown notification level"),
                };
                self.new_session_notification = (level != snapshot.config.new_session_notification).then_some(level);
            }
            GlobalSettingsField::ManagedSessions => {
                self.managed_sessions = changed_bool(
                    snapshot.config.session.enabled,
                    parse_enabled(value.as_deref())?,
                );
            }
            GlobalSettingsField::DockerSudo => {
                self.docker_sudo = changed_bool(
                    snapshot.config.docker_use_sudo,
                    parse_enabled(value.as_deref())?,
                );
            }
            GlobalSettingsField::DefaultProfile => {
                if value.as_deref() != snapshot.config.default_profile.as_deref() {
                    if let Some(name) = value.as_deref() {
                        if !snapshot
                            .profile_names
                            .iter()
                            .any(|candidate| candidate == name)
                        {
                            anyhow::bail!("Profile is no longer available: {name}");
                        }
                    }
                }
                self.default_profile =
                    changed_optional_value(snapshot.config.default_profile.as_ref(), value);
            }
            GlobalSettingsField::DefaultProfileDirectLaunch => {
                self.default_profile_direct_launch = changed_bool(
                    snapshot.config.default_profile_direct_launch,
                    parse_enabled(value.as_deref())?,
                );
            }
            GlobalSettingsField::ProxyEnabled => {
                let next = parse_enabled(value.as_deref())?;
                let current = snapshot
                    .config
                    .proxy
                    .as_ref()
                    .is_some_and(|proxy| proxy.enabled);
                self.proxy_enabled = changed_bool(current, next);
                if !next {
                    self.proxy_url = ConfigValueUpdate::Unchanged;
                    self.proxy_no_proxy = ConfigValueUpdate::Unchanged;
                }
            }
            GlobalSettingsField::ProxyUrl => {
                self.proxy_url = changed_optional_string(
                    snapshot
                        .config
                        .proxy
                        .as_ref()
                        .and_then(|proxy| proxy.url.as_deref()),
                    value,
                );
            }
            GlobalSettingsField::ProxyNoProxy => {
                self.proxy_no_proxy = changed_optional_string(
                    snapshot
                        .config
                        .proxy
                        .as_ref()
                        .and_then(|proxy| proxy.no_proxy.as_deref()),
                    value,
                );
            }
            GlobalSettingsField::NotifyServiceUrl => {
                self.notify_service_url =
                    changed_optional_string(snapshot.config.notify_service_url.as_deref(), value);
            }
            GlobalSettingsField::NotifyServiceToken
            | GlobalSettingsField::DesktopNotifyToken
            | GlobalSettingsField::AgentBusToken => {
                anyhow::bail!("Use Keep, Replace, or Clear for secret settings");
            }
            GlobalSettingsField::NotifyIdleTimeout => {
                self.notify_idle_timeout = changed_optional_value(
                    snapshot.config.notify_service_idle_timeout_secs.as_ref(),
                    parse_optional_u64(value.as_deref().unwrap_or("-"))?,
                );
            }
            GlobalSettingsField::NotifyComposerTimeout => {
                self.notify_composer_timeout = changed_optional_value(
                    snapshot
                        .config
                        .notify_service_composer_idle_timeout_secs
                        .as_ref(),
                    parse_optional_u64(value.as_deref().unwrap_or("-"))?,
                );
            }
            GlobalSettingsField::NotifyApprovalTimeout => {
                self.notify_approval_timeout = changed_optional_value(
                    snapshot
                        .config
                        .notify_service_approval_timeout_secs
                        .as_ref(),
                    parse_optional_u64(value.as_deref().unwrap_or("-"))?,
                );
            }
            GlobalSettingsField::NotifyStartupTimeout => {
                self.notify_startup_timeout = changed_optional_value(
                    snapshot
                        .config
                        .notify_service_startup_idle_timeout_secs
                        .as_ref(),
                    parse_optional_u64(value.as_deref().unwrap_or("-"))?,
                );
            }
            GlobalSettingsField::NotifyEvents => {
                self.notify_events = changed_optional_value(
                    snapshot.config.notify_service_events.as_ref(),
                    parse_notify_events(value.as_deref().unwrap_or("-")),
                );
            }
            GlobalSettingsField::NotifyMessageContent => {
                let current = snapshot.config.notify_service_user_message_content.as_ref();
                self.notify_message_content = if value.as_deref() == current.map(String::as_str) {
                    ConfigValueUpdate::Unchanged
                } else {
                    changed_optional_value(
                        current,
                        parse_notify_user_message_content(value.as_deref().unwrap_or("-"))?,
                    )
                };
            }
            GlobalSettingsField::NotifyPreviewChars => {
                self.notify_preview_chars = changed_optional_value(
                    snapshot
                        .config
                        .notify_service_user_message_preview_chars
                        .as_ref(),
                    parse_optional_u64(value.as_deref().unwrap_or("-"))?,
                );
            }
            GlobalSettingsField::DesktopNotifyEnabled => {
                self.desktop_notify_enabled = changed_bool(
                    snapshot.config.desktop_notify_enabled,
                    parse_enabled(value.as_deref())?,
                );
            }
            GlobalSettingsField::DesktopNotifyPort => {
                let next = parse_desktop_notify_port(value.as_deref().unwrap_or("-"))?;
                self.desktop_notify_port = match next {
                    Some(next)
                        if next
                            == snapshot
                                .config
                                .desktop_notify_port
                                .unwrap_or(DEFAULT_DESKTOP_NOTIFY_PORT) =>
                    {
                        ConfigValueUpdate::Unchanged
                    }
                    Some(next) => ConfigValueUpdate::Set(next),
                    None if snapshot.config.desktop_notify_port.is_none() => {
                        ConfigValueUpdate::Unchanged
                    }
                    None => ConfigValueUpdate::Clear,
                };
            }
            GlobalSettingsField::RateLimitThresholdWarning => {
                let current = snapshot.config.rate_limit_threshold_warning_mode.as_ref();
                self.rate_limit_threshold_warning =
                    if value.as_deref() == current.map(String::as_str) {
                        ConfigValueUpdate::Unchanged
                    } else {
                        changed_optional_value(
                            current,
                            parse_rate_limit_mode(value.as_deref().unwrap_or("-"))?,
                        )
                    };
            }
            GlobalSettingsField::RateLimitModelNudge => {
                let current = snapshot.config.rate_limit_model_nudge_mode.as_ref();
                self.rate_limit_model_nudge = if value.as_deref() == current.map(String::as_str) {
                    ConfigValueUpdate::Unchanged
                } else {
                    changed_optional_value(
                        current,
                        parse_rate_limit_mode(value.as_deref().unwrap_or("-"))?,
                    )
                };
            }
            GlobalSettingsField::AgentBusEnabled => {
                self.agent_bus_enabled = changed_bool(
                    snapshot.config.agent_bus_enabled,
                    parse_enabled(value.as_deref())?,
                );
            }
            GlobalSettingsField::AgentBusPort => {
                let next = parse_agent_bus_port(value.as_deref().unwrap_or("-"))?;
                self.agent_bus_port = match next {
                    Some(next)
                        if next
                            == snapshot
                                .config
                                .agent_bus_port
                                .unwrap_or(DEFAULT_AGENT_BUS_PORT) =>
                    {
                        ConfigValueUpdate::Unchanged
                    }
                    Some(next) => ConfigValueUpdate::Set(next),
                    None if snapshot.config.agent_bus_port.is_none() => {
                        ConfigValueUpdate::Unchanged
                    }
                    None => ConfigValueUpdate::Clear,
                };
            }
        }
        Ok(())
    }

    pub(super) fn stage_secret(
        &mut self,
        snapshot: &GlobalSettingsSnapshot,
        field: GlobalSettingsField,
        action: SecretSettingsAction,
    ) -> anyhow::Result<()> {
        let update = match field {
            GlobalSettingsField::NotifyServiceToken => {
                changed_secret(snapshot.config.notify_service_token.as_deref(), action)?
            }
            GlobalSettingsField::DesktopNotifyToken => {
                changed_secret(snapshot.config.desktop_notify_token.as_deref(), action)?
            }
            GlobalSettingsField::AgentBusToken => {
                changed_secret(snapshot.config.agent_bus_token.as_deref(), action)?
            }
            _ => anyhow::bail!("Selected setting is not a secret"),
        };
        match field {
            GlobalSettingsField::NotifyServiceToken => self.notify_service_token = update,
            GlobalSettingsField::DesktopNotifyToken => self.desktop_notify_token = update,
            GlobalSettingsField::AgentBusToken => self.agent_bus_token = update,
            _ => unreachable!("secret field checked above"),
        }
        Ok(())
    }

    pub(super) fn value(
        &self,
        snapshot: &GlobalSettingsSnapshot,
        field: GlobalSettingsField,
    ) -> String {
        match field {
            GlobalSettingsField::Identity(provider,f)=>{let a=self.appearance.as_ref().unwrap_or(&snapshot.config.appearance); f.value(&ProfileOptions{style:Some(provider.map(|p|a.provider_style(p)).unwrap_or_else(||a.host_suffix.clone())),..Default::default()})},
            GlobalSettingsField::DefaultModel | GlobalSettingsField::DefaultReasoning => {
                let name = effective_optional_value(snapshot.config.default_profile.as_ref(), &self.default_profile);
                let defaults = self.new_session_defaults.as_ref().unwrap_or(&snapshot.config.new_session_defaults);
                let entry = name.as_ref().and_then(|n| defaults.get(n));
                entry.and_then(|e| if field == GlobalSettingsField::DefaultModel { e.model.clone() } else { e.reasoning.clone() }).unwrap_or_default()
            }
            GlobalSettingsField::AgentSort => self.agent_sort.unwrap_or(snapshot.config.agent_sort).key().into(),
            GlobalSettingsField::SessionSort => self.session_sort.unwrap_or(snapshot.config.session_sort).key().into(),
            GlobalSettingsField::JobSort => self.job_sort.unwrap_or(snapshot.config.job_sort).key().into(),
            GlobalSettingsField::ProjectSort => self.project_sort.unwrap_or(snapshot.config.project_sort).key().into(),
            GlobalSettingsField::DefaultLocalHostFilter => if self.default_local_host_filter.unwrap_or(snapshot.config.default_local_host_filter) { "local" } else { "all" }.into(),
            GlobalSettingsField::TerminalColors => if self.terminal_truecolor.unwrap_or(snapshot.config.terminal_truecolor) { "truecolor" } else { "auto" }.into(),
            GlobalSettingsField::DefaultNotification => match self.new_session_notification.unwrap_or(snapshot.config.new_session_notification) {
                cutex::notify::session::Level::Important => "important",
                cutex::notify::session::Level::Normal => "normal",
                cutex::notify::session::Level::Off => "off",
            }.into(),
            GlobalSettingsField::ClaudeUsageRefresh => self
                .claude_usage_refresh_minutes
                .unwrap_or(snapshot.config.claude_usage_refresh_minutes)
                .to_string(),
            GlobalSettingsField::ManagedSessions => enabled(
                self.managed_sessions
                    .unwrap_or(snapshot.config.session.enabled),
            ),
            GlobalSettingsField::DockerSudo => {
                enabled(self.docker_sudo.unwrap_or(snapshot.config.docker_use_sudo))
            }
            GlobalSettingsField::DefaultProfile => optional_owned(effective_optional_value(
                snapshot.config.default_profile.as_ref(),
                &self.default_profile,
            )),
            GlobalSettingsField::DefaultProfileDirectLaunch => enabled(
                self.default_profile_direct_launch
                    .unwrap_or(snapshot.config.default_profile_direct_launch),
            ),
            GlobalSettingsField::ProxyEnabled => enabled(self.proxy_enabled.unwrap_or_else(|| {
                snapshot
                    .config
                    .proxy
                    .as_ref()
                    .is_some_and(|proxy| proxy.enabled)
            })),
            GlobalSettingsField::ProxyUrl => optional_owned(effective_optional_string(
                snapshot
                    .config
                    .proxy
                    .as_ref()
                    .and_then(|proxy| proxy.url.as_deref()),
                &self.proxy_url,
            )),
            GlobalSettingsField::ProxyNoProxy => optional_owned(effective_optional_string(
                snapshot
                    .config
                    .proxy
                    .as_ref()
                    .and_then(|proxy| proxy.no_proxy.as_deref()),
                &self.proxy_no_proxy,
            )),
            GlobalSettingsField::NotifyServiceUrl => optional_owned(effective_optional_string(
                snapshot.config.notify_service_url.as_deref(),
                &self.notify_service_url,
            )),
            GlobalSettingsField::NotifyServiceToken => secret_update(
                snapshot.config.notify_service_token.as_deref(),
                &self.notify_service_token,
            ),
            GlobalSettingsField::NotifyIdleTimeout => optional_number(effective_optional_value(
                snapshot.config.notify_service_idle_timeout_secs.as_ref(),
                &self.notify_idle_timeout,
            )),
            GlobalSettingsField::NotifyComposerTimeout => {
                optional_number(effective_optional_value(
                    snapshot
                        .config
                        .notify_service_composer_idle_timeout_secs
                        .as_ref(),
                    &self.notify_composer_timeout,
                ))
            }
            GlobalSettingsField::NotifyApprovalTimeout => {
                optional_number(effective_optional_value(
                    snapshot
                        .config
                        .notify_service_approval_timeout_secs
                        .as_ref(),
                    &self.notify_approval_timeout,
                ))
            }
            GlobalSettingsField::NotifyStartupTimeout => optional_number(effective_optional_value(
                snapshot
                    .config
                    .notify_service_startup_idle_timeout_secs
                    .as_ref(),
                &self.notify_startup_timeout,
            )),
            GlobalSettingsField::NotifyEvents => effective_optional_value(
                snapshot.config.notify_service_events.as_ref(),
                &self.notify_events,
            )
            .as_deref()
            .map(csv_or_dash)
            .unwrap_or_else(|| "-".to_string()),
            GlobalSettingsField::NotifyMessageContent => optional_owned(effective_optional_value(
                snapshot.config.notify_service_user_message_content.as_ref(),
                &self.notify_message_content,
            )),
            GlobalSettingsField::NotifyPreviewChars => optional_number(effective_optional_value(
                snapshot
                    .config
                    .notify_service_user_message_preview_chars
                    .as_ref(),
                &self.notify_preview_chars,
            )),
            GlobalSettingsField::DesktopNotifyEnabled => enabled(
                self.desktop_notify_enabled
                    .unwrap_or(snapshot.config.desktop_notify_enabled),
            ),
            GlobalSettingsField::DesktopNotifyPort => match &self.desktop_notify_port {
                ConfigValueUpdate::Unchanged => snapshot
                    .config
                    .desktop_notify_port
                    .unwrap_or(DEFAULT_DESKTOP_NOTIFY_PORT),
                ConfigValueUpdate::Set(port) => *port,
                ConfigValueUpdate::Clear => DEFAULT_DESKTOP_NOTIFY_PORT,
            }
            .to_string(),
            GlobalSettingsField::DesktopNotifyToken => secret_update(
                snapshot.config.desktop_notify_token.as_deref(),
                &self.desktop_notify_token,
            ),
            GlobalSettingsField::RateLimitThresholdWarning => {
                optional_owned(effective_optional_value(
                    snapshot.config.rate_limit_threshold_warning_mode.as_ref(),
                    &self.rate_limit_threshold_warning,
                ))
            }
            GlobalSettingsField::RateLimitModelNudge => optional_owned(effective_optional_value(
                snapshot.config.rate_limit_model_nudge_mode.as_ref(),
                &self.rate_limit_model_nudge,
            )),
            GlobalSettingsField::AgentBusEnabled => enabled(
                self.agent_bus_enabled
                    .unwrap_or(snapshot.config.agent_bus_enabled),
            ),
            GlobalSettingsField::AgentBusPort => match &self.agent_bus_port {
                ConfigValueUpdate::Unchanged => snapshot
                    .config
                    .agent_bus_port
                    .unwrap_or(DEFAULT_AGENT_BUS_PORT),
                ConfigValueUpdate::Set(port) => *port,
                ConfigValueUpdate::Clear => DEFAULT_AGENT_BUS_PORT,
            }
            .to_string(),
            GlobalSettingsField::AgentBusToken => secret_update(
                snapshot.config.agent_bus_token.as_deref(),
                &self.agent_bus_token,
            ),
        }
    }

    pub(super) fn field_is_dirty(&self, field: GlobalSettingsField) -> bool {
        match field {
            GlobalSettingsField::Identity(p,f)=>self.identity_dirty.contains(&(p,f)),
            GlobalSettingsField::DefaultModel => self.default_model_dirty,
            GlobalSettingsField::DefaultReasoning => self.default_reasoning_dirty,
            GlobalSettingsField::AgentSort => self.agent_sort.is_some(),
            GlobalSettingsField::SessionSort => self.session_sort.is_some(),
            GlobalSettingsField::JobSort => self.job_sort.is_some(),
            GlobalSettingsField::ProjectSort => self.project_sort.is_some(),
            GlobalSettingsField::DefaultLocalHostFilter => self.default_local_host_filter.is_some(),
            GlobalSettingsField::TerminalColors => self.terminal_truecolor.is_some(),
            GlobalSettingsField::DefaultNotification => self.new_session_notification.is_some(),
            GlobalSettingsField::ClaudeUsageRefresh => {
                self.claude_usage_refresh_minutes.is_some()
            }
            GlobalSettingsField::ManagedSessions => self.managed_sessions.is_some(),
            GlobalSettingsField::DockerSudo => self.docker_sudo.is_some(),
            GlobalSettingsField::DefaultProfile => {
                !matches!(self.default_profile, ConfigValueUpdate::Unchanged)
            }
            GlobalSettingsField::DefaultProfileDirectLaunch => {
                self.default_profile_direct_launch.is_some()
            }
            GlobalSettingsField::ProxyEnabled => self.proxy_enabled.is_some(),
            GlobalSettingsField::ProxyUrl => {
                !matches!(self.proxy_url, ConfigValueUpdate::Unchanged)
            }
            GlobalSettingsField::ProxyNoProxy => {
                !matches!(self.proxy_no_proxy, ConfigValueUpdate::Unchanged)
            }
            GlobalSettingsField::NotifyServiceUrl => {
                !matches!(self.notify_service_url, ConfigValueUpdate::Unchanged)
            }
            GlobalSettingsField::NotifyServiceToken => {
                !matches!(self.notify_service_token, ConfigValueUpdate::Unchanged)
            }
            GlobalSettingsField::NotifyIdleTimeout => {
                !matches!(self.notify_idle_timeout, ConfigValueUpdate::Unchanged)
            }
            GlobalSettingsField::NotifyComposerTimeout => {
                !matches!(self.notify_composer_timeout, ConfigValueUpdate::Unchanged)
            }
            GlobalSettingsField::NotifyApprovalTimeout => {
                !matches!(self.notify_approval_timeout, ConfigValueUpdate::Unchanged)
            }
            GlobalSettingsField::NotifyStartupTimeout => {
                !matches!(self.notify_startup_timeout, ConfigValueUpdate::Unchanged)
            }
            GlobalSettingsField::NotifyEvents => {
                !matches!(self.notify_events, ConfigValueUpdate::Unchanged)
            }
            GlobalSettingsField::NotifyMessageContent => {
                !matches!(self.notify_message_content, ConfigValueUpdate::Unchanged)
            }
            GlobalSettingsField::NotifyPreviewChars => {
                !matches!(self.notify_preview_chars, ConfigValueUpdate::Unchanged)
            }
            GlobalSettingsField::DesktopNotifyEnabled => self.desktop_notify_enabled.is_some(),
            GlobalSettingsField::DesktopNotifyPort => {
                !matches!(self.desktop_notify_port, ConfigValueUpdate::Unchanged)
            }
            GlobalSettingsField::DesktopNotifyToken => {
                !matches!(self.desktop_notify_token, ConfigValueUpdate::Unchanged)
            }
            GlobalSettingsField::RateLimitThresholdWarning => !matches!(
                self.rate_limit_threshold_warning,
                ConfigValueUpdate::Unchanged
            ),
            GlobalSettingsField::RateLimitModelNudge => {
                !matches!(self.rate_limit_model_nudge, ConfigValueUpdate::Unchanged)
            }
            GlobalSettingsField::AgentBusEnabled => self.agent_bus_enabled.is_some(),
            GlobalSettingsField::AgentBusPort => {
                !matches!(self.agent_bus_port, ConfigValueUpdate::Unchanged)
            }
            GlobalSettingsField::AgentBusToken => {
                !matches!(self.agent_bus_token, ConfigValueUpdate::Unchanged)
            }
        }
    }

    pub(super) fn dirty_count(&self) -> usize {
        [
            GlobalSettingsField::ManagedSessions,
            GlobalSettingsField::DockerSudo,
            GlobalSettingsField::DefaultProfile,
            GlobalSettingsField::DefaultProfileDirectLaunch,
            GlobalSettingsField::DefaultModel,
            GlobalSettingsField::DefaultReasoning,
            GlobalSettingsField::DefaultNotification,
            GlobalSettingsField::ClaudeUsageRefresh,
            GlobalSettingsField::TerminalColors,
            GlobalSettingsField::DefaultLocalHostFilter,
            GlobalSettingsField::SessionSort,
            GlobalSettingsField::JobSort,
            GlobalSettingsField::ProjectSort,
            GlobalSettingsField::AgentSort,
            GlobalSettingsField::ProxyEnabled,
            GlobalSettingsField::ProxyUrl,
            GlobalSettingsField::ProxyNoProxy,
            GlobalSettingsField::NotifyServiceUrl,
            GlobalSettingsField::NotifyServiceToken,
            GlobalSettingsField::NotifyIdleTimeout,
            GlobalSettingsField::NotifyComposerTimeout,
            GlobalSettingsField::NotifyApprovalTimeout,
            GlobalSettingsField::NotifyStartupTimeout,
            GlobalSettingsField::NotifyEvents,
            GlobalSettingsField::NotifyMessageContent,
            GlobalSettingsField::NotifyPreviewChars,
            GlobalSettingsField::DesktopNotifyEnabled,
            GlobalSettingsField::DesktopNotifyPort,
            GlobalSettingsField::DesktopNotifyToken,
            GlobalSettingsField::RateLimitThresholdWarning,
            GlobalSettingsField::RateLimitModelNudge,
            GlobalSettingsField::AgentBusEnabled,
            GlobalSettingsField::AgentBusPort,
            GlobalSettingsField::AgentBusToken,
        ]
        .into_iter()
        .filter(|field| self.field_is_dirty(*field))
        .count() + self.identity_dirty.len()
    }

    pub(super) fn is_dirty(&self) -> bool {
        self.dirty_count() != 0
    }

    pub(super) fn patch(&self, current: &CodezConfig) -> anyhow::Result<GlobalConfigPatch> {
        let proxy_is_dirty = [
            GlobalSettingsField::ProxyEnabled,
            GlobalSettingsField::ProxyUrl,
            GlobalSettingsField::ProxyNoProxy,
        ]
        .into_iter()
        .any(|field| self.field_is_dirty(field));
        let proxy = if !proxy_is_dirty {
            ConfigValueUpdate::Unchanged
        } else {
            let current_proxy = current.proxy.as_ref();
            let enabled = self
                .proxy_enabled
                .unwrap_or_else(|| current_proxy.is_some_and(|proxy| proxy.enabled));
            if !enabled {
                if !matches!(self.proxy_url, ConfigValueUpdate::Unchanged)
                    || !matches!(self.proxy_no_proxy, ConfigValueUpdate::Unchanged)
                {
                    anyhow::bail!("Enable Proxy before saving its URL or bypass settings");
                }
                ConfigValueUpdate::Clear
            } else {
                ConfigValueUpdate::Set(proxy_config_from_parts(
                    true,
                    effective_optional_string(
                        current_proxy.and_then(|proxy| proxy.url.as_deref()),
                        &self.proxy_url,
                    ),
                    effective_optional_string(
                        current_proxy.and_then(|proxy| proxy.no_proxy.as_deref()),
                        &self.proxy_no_proxy,
                    ),
                )?)
            }
        };
        Ok(GlobalConfigPatch {
            appearance: self.appearance.clone(),
            docker_use_sudo: self.docker_sudo,
            terminal_truecolor: self.terminal_truecolor,
            default_local_host_filter: self.default_local_host_filter,
            session_sort: self.session_sort,
            job_sort: self.job_sort,
            project_sort: self.project_sort,
            agent_sort: self.agent_sort,
            session_enabled: self.managed_sessions,
            default_profile: self.default_profile.clone(),
            default_profile_direct_launch: self.default_profile_direct_launch,
            // Not edited in the TUI (Management /v2/settings only).
            queue_owner_may_pass_asks: None,
            quick_chat_default_profile: ConfigValueUpdate::Unchanged,
            new_session_defaults: self.new_session_defaults.clone(),
            new_session_notification: self.new_session_notification,
            new_thread_defaults: None,
            claude_usage_refresh_minutes: self.claude_usage_refresh_minutes,
            profile_quota_refresh_minutes: None,
            archive_delete_empty: None,
            documents: None,
            // Management /v2/settings only.
            auto_thread_titles: None,
            codex_config_follow_shared: None,
            thread_title_model: ConfigValueUpdate::Unchanged,
            agent_security: None,
            attachments: None,
            proxy,
            notify_service_url: self.notify_service_url.clone(),
            notify_service_token: self.notify_service_token.clone(),
            notify_service_idle_timeout_secs: self.notify_idle_timeout.clone(),
            notify_service_composer_idle_timeout_secs: self.notify_composer_timeout.clone(),
            notify_service_approval_timeout_secs: self.notify_approval_timeout.clone(),
            notify_service_startup_idle_timeout_secs: self.notify_startup_timeout.clone(),
            notify_service_events: self.notify_events.clone(),
            notify_service_user_message_content: self.notify_message_content.clone(),
            notify_service_user_message_preview_chars: self.notify_preview_chars.clone(),
            rate_limit_threshold_warning_mode: self.rate_limit_threshold_warning.clone(),
            rate_limit_model_nudge_mode: self.rate_limit_model_nudge.clone(),
            desktop_notify_enabled: self.desktop_notify_enabled,
            desktop_notify_port: self.desktop_notify_port.clone(),
            desktop_notify_token: self.desktop_notify_token.clone(),
            agent_bus_enabled: self.agent_bus_enabled,
            agent_bus_port: self.agent_bus_port.clone(),
            agent_bus_token: self.agent_bus_token.clone(),
        })
    }

    pub(super) fn default_profile_is_dirty(&self) -> bool {
        !matches!(self.default_profile, ConfigValueUpdate::Unchanged)
    }

    pub(super) fn validate_profile_catalog(&self, profile_names: &[String]) -> anyhow::Result<()> {
        if let ConfigValueUpdate::Set(name) = &self.default_profile {
            if !profile_names.iter().any(|candidate| candidate == name) {
                anyhow::bail!("Profile is no longer available: {name}");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
fn global_setting_categories(config: &CodezConfig) -> Vec<SessionTuiSettingCategory> {
    GlobalSettingsSnapshot::from_config(config).categories(&GlobalSettingsDraft::default())
}

fn parse_enabled(value: Option<&str>) -> anyhow::Result<bool> {
    match value.map(str::trim) {
        Some("enabled") => Ok(true),
        Some("disabled") => Ok(false),
        Some(value) => anyhow::bail!("Unsupported enabled state: {value}"),
        None => anyhow::bail!("Enabled state cannot be cleared"),
    }
}

fn changed_bool(current: bool, next: bool) -> Option<bool> {
    (current != next).then_some(next)
}

fn changed_optional_value<T: PartialEq>(
    current: Option<&T>,
    next: Option<T>,
) -> ConfigValueUpdate<T> {
    match next {
        Some(next) if current == Some(&next) => ConfigValueUpdate::Unchanged,
        Some(next) => ConfigValueUpdate::Set(next),
        None if current.is_none() => ConfigValueUpdate::Unchanged,
        None => ConfigValueUpdate::Clear,
    }
}

fn changed_optional_string(
    current: Option<&str>,
    next: Option<String>,
) -> ConfigValueUpdate<String> {
    let next = next
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && value != "-");
    match next {
        Some(next) if current == Some(next.as_str()) => ConfigValueUpdate::Unchanged,
        Some(next) => ConfigValueUpdate::Set(next),
        None if current.is_none() => ConfigValueUpdate::Unchanged,
        None => ConfigValueUpdate::Clear,
    }
}

fn effective_optional_string(
    current: Option<&str>,
    update: &ConfigValueUpdate<String>,
) -> Option<String> {
    match update {
        ConfigValueUpdate::Unchanged => current.map(str::to_string),
        ConfigValueUpdate::Set(value) => Some(value.clone()),
        ConfigValueUpdate::Clear => None,
    }
}

fn effective_optional_value<T: Clone>(
    current: Option<&T>,
    update: &ConfigValueUpdate<T>,
) -> Option<T> {
    match update {
        ConfigValueUpdate::Unchanged => current.cloned(),
        ConfigValueUpdate::Set(value) => Some(value.clone()),
        ConfigValueUpdate::Clear => None,
    }
}

fn changed_secret(
    current: Option<&str>,
    action: SecretSettingsAction,
) -> anyhow::Result<ConfigValueUpdate<String>> {
    match action {
        SecretSettingsAction::Keep => Ok(ConfigValueUpdate::Unchanged),
        SecretSettingsAction::Clear if current.is_none() => Ok(ConfigValueUpdate::Unchanged),
        SecretSettingsAction::Clear => Ok(ConfigValueUpdate::Clear),
        SecretSettingsAction::Replace(value) => {
            let value = value.trim();
            if value.is_empty() {
                anyhow::bail!("Replacement secret cannot be empty");
            }
            if current == Some(value) {
                Ok(ConfigValueUpdate::Unchanged)
            } else {
                Ok(ConfigValueUpdate::Set(value.to_string()))
            }
        }
    }
}

fn secret_update(current: Option<&str>, update: &ConfigValueUpdate<String>) -> String {
    match update {
        ConfigValueUpdate::Unchanged => secret(current),
        ConfigValueUpdate::Set(_) => "(replace staged)".to_string(),
        ConfigValueUpdate::Clear => "(clear staged)".to_string(),
    }
}

fn optional_owned(value: Option<String>) -> String {
    optional(value.as_deref())
}

fn nonempty(value: &str) -> String {
    optional(Some(value))
}

fn optional(value: Option<&str>) -> String {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("-")
        .to_string()
}

fn enabled(value: bool) -> String {
    if value { "enabled" } else { "disabled" }.to_string()
}

fn secret(value: Option<&str>) -> String {
    if value.is_some_and(|value| !value.is_empty()) {
        "(set)".to_string()
    } else {
        "-".to_string()
    }
}

fn optional_number(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_string(), |value| value.to_string())
}

fn csv_or_dash(values: &[String]) -> String {
    if values.is_empty() {
        "-".to_string()
    } else {
        values.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use cutex::agent_bus::model::AgentRegistrationClass;
    use cutex::profiles::model::ProxyConfig;
    use cutex::session::model::{CutexSessionQuickActionMode, CutexSessionRuntimeBackend};

    #[test]
    fn other_panel_sort_defaults_survive_save_and_reload() {
        use cutex::profiles::list_preferences::ListSort;
        let mut config = CodezConfig::default();
        let snapshot = GlobalSettingsSnapshot::from_config(&config);
        let mut draft = GlobalSettingsDraft::default();
        for field in [GlobalSettingsField::ProjectSort, GlobalSettingsField::JobSort] {
            draft.stage(&snapshot, field, Some("name_desc".into())).unwrap();
            assert!(draft.stage(&snapshot, field, Some("pinned".into())).is_err());
        }
        let patch = draft.patch(&config).unwrap();
        cutex::config::global_settings::apply_global_config_patch(&mut config, &patch).unwrap();
        let restored: CodezConfig = serde_json::from_value(serde_json::to_value(config).unwrap()).unwrap();
        assert_eq!(GlobalSettingsSnapshot::from_config(&restored).other_sort_defaults(), [ListSort::NameDesc; 2]);
        for field in [GlobalSettingsField::ProjectSort, GlobalSettingsField::JobSort] {
            draft.stage(&snapshot, field, Some("default".into())).unwrap();
        }
        assert!(draft.project_sort.is_none() && draft.job_sort.is_none());
    }

    fn flattened(categories: &[SessionTuiSettingCategory]) -> String {
        categories
            .iter()
            .flat_map(|category| {
                category
                    .options
                    .iter()
                    .map(|option| format!("{}:{}={}", category.label, option.label, option.value))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn list_sort_settings_validate_save_and_restore_panel_defaults() {
        use cutex::profiles::list_preferences::ListSort;
        let snapshot = GlobalSettingsSnapshot::from_config(&CodezConfig::default());
        let mut draft = GlobalSettingsDraft::default();
        draft.stage(&snapshot, GlobalSettingsField::AgentSort, Some("pinned".into())).unwrap();
        draft.stage(&snapshot, GlobalSettingsField::SessionSort, Some("name_asc".into())).unwrap();
        assert_eq!(draft.dirty_count(), 2);
        assert!(draft.stage(&snapshot, GlobalSettingsField::SessionSort, Some("pinned".into())).is_err());
        assert_eq!(draft.session_sort, Some(ListSort::NameAsc));
        let mut config = CodezConfig::default();
        cutex::config::global_settings::apply_global_config_patch(&mut config, &cutex::config::global_settings::GlobalConfigPatch {
            agent_sort: draft.agent_sort, session_sort: draft.session_sort,
            ..Default::default()
        }).unwrap();
        let restored: CodezConfig = serde_json::from_value(serde_json::to_value(&config).unwrap()).unwrap();
        assert_eq!((restored.agent_sort, restored.session_sort), (ListSort::Pinned, ListSort::NameAsc));
        draft.stage(&snapshot, GlobalSettingsField::AgentSort, Some("default".into())).unwrap();
        draft.stage(&snapshot, GlobalSettingsField::SessionSort, Some("default".into())).unwrap();
        assert_eq!(draft.dirty_count(), 0);
    }

    #[test]
    fn host_filter_default_can_be_saved_and_discarded() {
        let config = CodezConfig::default();
        let snapshot = GlobalSettingsSnapshot::from_config_with_profiles(&config, &[]);
        let mut draft = GlobalSettingsDraft::default();
        draft.stage(&snapshot, GlobalSettingsField::DefaultLocalHostFilter, Some("local".into())).unwrap();
        assert_eq!(draft.dirty_count(), 1);
        assert!(draft.default_local_host_filter.unwrap());
        draft.stage(&snapshot, GlobalSettingsField::DefaultLocalHostFilter, Some("all".into())).unwrap();
        assert_eq!(draft.dirty_count(), 0);
        let mut saved = config;
        cutex::config::global_settings::apply_global_config_patch(&mut saved, &cutex::config::global_settings::GlobalConfigPatch {
            default_local_host_filter: Some(true), ..Default::default()
        }).unwrap();
        assert!(saved.default_local_host_filter);
    }

    #[test]
    fn claude_usage_refresh_edits_validate_stage_and_save() {
        let config = CodezConfig::default();
        let snapshot = GlobalSettingsSnapshot::from_config(&config);
        let mut draft = GlobalSettingsDraft::default();

        draft
            .stage(
                &snapshot,
                GlobalSettingsField::ClaudeUsageRefresh,
                Some("2".into()),
            )
            .expect("stage Claude usage refresh");
        assert_eq!(draft.dirty_count(), 1);
        assert_eq!(
            draft.value(&snapshot, GlobalSettingsField::ClaudeUsageRefresh),
            "2"
        );
        assert!(draft
            .stage(
                &snapshot,
                GlobalSettingsField::ClaudeUsageRefresh,
                Some("61".into()),
            )
            .is_err());

        let mut saved = config;
        cutex::config::global_settings::apply_global_config_patch(
            &mut saved,
            &draft.patch(&snapshot.config).unwrap(),
        )
        .unwrap();
        assert_eq!(saved.claude_usage_refresh_minutes, 2);
    }

    #[test]
    fn terminal_colors_default_stage_save_and_reset() {
        let config = CodezConfig::default();
        assert!(!config.terminal_truecolor);
        let snapshot = GlobalSettingsSnapshot::from_config_with_profiles(&config, &[]);
        let mut draft = GlobalSettingsDraft::default();
        draft.stage(&snapshot, GlobalSettingsField::TerminalColors, Some("truecolor".into())).unwrap();
        assert_eq!(draft.dirty_count(), 1);
        assert!(draft.stage(&snapshot, GlobalSettingsField::TerminalColors, Some("invalid".into())).is_err());
        let mut saved = config;
        cutex::config::global_settings::apply_global_config_patch(&mut saved, &draft.patch(&snapshot.config).unwrap()).unwrap();
        assert!(saved.terminal_truecolor);
        draft.stage(&snapshot, GlobalSettingsField::TerminalColors, Some("auto".into())).unwrap();
        assert_eq!(draft.dirty_count(), 0);
    }

    #[test]
    fn agent_projection_groups_durable_settings_without_runtime_identity() {
        let mut record = CutexSessionRecord::new_at(
            "cutex.settings".to_string(),
            Some("019e-settings".to_string()),
            "devbox".to_string(),
            "/tmp/original".to_string(),
            Some("orchid".to_string()),
            "2026-08-05T00:00:00Z".to_string(),
        )
        .expect("record");
        record.display_name_hint = Some("settings-agent".to_string());
        record.managed_cwd = Some("/tmp/managed".to_string());
        record.runtime_backend = CutexSessionRuntimeBackend::CuteAlden;
        record.permission_defaults = Some("workspace".to_string());
        record.model_defaults = Some("gpt-test".to_string());
        record.agent_groups = vec!["cutex".to_string(), "waveline".to_string()];
        record.quick_action = CutexSessionQuickActionMode::Pinned;
        record.current_runtime_agent_id = Some("volatile-runtime-id".to_string());

        let categories = SessionSettingsSnapshot::from_record(&record)
            .categories(&SessionSettingsDraft::default());
        assert_eq!(
            categories
                .iter()
                .map(|category| category.label)
                .collect::<Vec<_>>(),
            vec!["Identity", "Permissions", "Launch", "Routing", "Details",]
        );
        let settings = flattened(&categories);

        assert!(settings.contains("Identity:Agent name=settings-agent"));
        assert!(settings.contains("Launch:Runtime backend=alden"));
        assert!(settings.contains("Launch:Working directory=/tmp/managed"));
        assert!(settings.contains("Permissions:Model=gpt-test"));
        assert!(settings.contains("Routing:Message groups=cutex, waveline"));
        assert!(settings.contains("Details:Management=adopt"));
        assert!(settings.contains("Details:Cutex session=cutex.settings"));
        assert!(!settings.contains("volatile-runtime-id"));
        assert_eq!(
            categories
                .iter()
                .flat_map(|category| category.options.iter())
                .filter(|option| option.field.is_some())
                .count(),
            12
        );
    }

    #[test]
    fn management_projection_offers_adopt_for_local_and_unmanage_for_persistent() {
        let mut record = CutexSessionRecord::new_at(
            "cutex.management".to_string(),
            Some("019e-management".to_string()),
            "devbox".to_string(),
            "/tmp".to_string(),
            None,
            "2026-08-06T00:00:00Z".to_string(),
        )
        .expect("record");

        let local = SessionSettingsSnapshot::from_record(&record)
            .categories(&SessionSettingsDraft::default())
            .into_iter()
            .find(|category| category.label == "Details")
            .and_then(|category| {
                category
                    .options
                    .into_iter()
                    .find(|option| option.label == "Management")
            })
            .expect("local management command");
        assert_eq!(local.value, "adopt");
        assert_eq!(local.command, Some(SessionSettingsCommand::Adopt));
        assert_eq!(local.field, None);

        record.registration_class = AgentRegistrationClass::Persistent;
        let managed = SessionSettingsSnapshot::from_record(&record)
            .categories(&SessionSettingsDraft::default())
            .into_iter()
            .find(|category| category.label == "Details")
            .and_then(|category| {
                category
                    .options
                    .into_iter()
                    .find(|option| option.label == "Management")
            })
            .expect("managed management command");
        assert_eq!(managed.value, "unmanage");
        assert_eq!(managed.command, Some(SessionSettingsCommand::Unmanage));
        assert_eq!(managed.field, None);
    }

    #[test]
    fn permission_draft_projects_effective_values_and_only_changed_patch_fields() {
        let mut record = CutexSessionRecord::new_at(
            "cutex.draft".to_string(),
            Some("019e-draft".to_string()),
            "devbox".to_string(),
            "/tmp/draft".to_string(),
            None,
            "2026-08-05T00:00:00Z".to_string(),
        )
        .expect("record");
        record.permission_defaults = Some("workspace".to_string());
        record.approval_policy = Some("on-request".to_string());
        record.model_defaults = Some("gpt-old".to_string());
        let snapshot = SessionSettingsSnapshot::from_record(&record);
        let mut draft = SessionSettingsDraft::default();

        draft
            .stage(
                &snapshot,
                SessionSettingsField::PermissionPreset,
                Some("full-access".to_string()),
            )
            .expect("stage permission");
        draft
            .stage(&snapshot, SessionSettingsField::ApprovalPolicy, None)
            .expect("stage approval");
        draft
            .stage(
                &snapshot,
                SessionSettingsField::Model,
                Some("gpt-next".to_string()),
            )
            .expect("stage model");
        draft
            .stage(
                &snapshot,
                SessionSettingsField::SandboxMode,
                Some("danger-full-access".to_string()),
            )
            .expect("stage sandbox");
        draft
            .stage(
                &snapshot,
                SessionSettingsField::ReasoningEffort,
                Some("high".to_string()),
            )
            .expect("stage reasoning");

        assert_eq!(draft.dirty_count(), 5);
        assert_eq!(
            draft.value(&snapshot, SessionSettingsField::PermissionPreset),
            Some("full-access")
        );
        assert_eq!(
            draft.value(&snapshot, SessionSettingsField::ApprovalPolicy),
            None
        );
        let categories = snapshot.categories(&draft);
        let permissions = categories
            .iter()
            .find(|category| category.label == "Permissions")
            .expect("permissions");
        assert_eq!(permissions.options[0].value, "full-access");
        assert!(permissions.options[0].dirty);
        assert_eq!(permissions.options[1].value, "-");
        assert!(permissions.options[1].dirty);
        assert_eq!(permissions.options[2].value, "danger-full-access");
        assert!(permissions.options[2].dirty);
        assert_eq!(permissions.options[4].value, "high");
        assert!(permissions.options[4].dirty);

        let patch = draft.runtime_defaults_patch();
        assert_eq!(
            patch.permission_defaults,
            CutexSessionValueUpdate::Set("full-access".to_string())
        );
        assert_eq!(patch.approval_policy, CutexSessionValueUpdate::Clear);
        assert_eq!(
            patch.sandbox_mode,
            CutexSessionValueUpdate::Set("danger-full-access".to_string())
        );
        assert_eq!(
            patch.model_defaults,
            CutexSessionValueUpdate::Set("gpt-next".to_string())
        );
        assert_eq!(
            patch.reasoning_defaults,
            CutexSessionValueUpdate::Set("high".to_string())
        );

        draft
            .stage(
                &snapshot,
                SessionSettingsField::Model,
                Some("gpt-old".to_string()),
            )
            .expect("restore model");
        assert_eq!(draft.dirty_count(), 4);
        assert!(!draft.field_is_dirty(SessionSettingsField::Model));
    }

    #[test]
    fn permission_editor_kinds_expose_finite_choices_and_model_text() {
        let record = CutexSessionRecord::new_at(
            "cutex.choices".to_string(),
            None,
            "devbox".to_string(),
            "/tmp".to_string(),
            None,
            "2026-08-05T00:00:00Z".to_string(),
        )
        .expect("record");
        let snapshot = SessionSettingsSnapshot::from_record(&record);
        let permission_choices = snapshot.choices(SessionSettingsField::PermissionPreset);
        assert_eq!(
            SessionSettingsField::PermissionPreset.editor_kind(),
            SessionSettingsEditorKind::Choice
        );
        assert_eq!(permission_choices[0].value, None);
        assert!(permission_choices
            .iter()
            .any(|choice| choice.value.as_deref() == Some("full-access")));
        assert_eq!(
            SessionSettingsField::Model.editor_kind(),
            SessionSettingsEditorKind::Text
        );
        assert_eq!(
            SessionSettingsField::RuntimeBackend.editor_kind(),
            SessionSettingsEditorKind::Choice
        );
        assert_eq!(
            snapshot
                .choices(SessionSettingsField::RuntimeBackend)
                .iter()
                .map(|choice| (choice.label.as_str(), choice.value.as_deref()))
                .collect::<Vec<_>>(),
            [
                ("Host", Some("host")),
                ("Native foreground", Some("native")),
                ("Docker", Some("docker")),
                ("Cute Alden", Some("alden")),
                ("Future", Some("future")),
            ]
        );
    }

    #[test]
    fn identity_and_launch_draft_reuses_typed_parsers_and_updates_effective_cwd() {
        let mut record = CutexSessionRecord::new_at(
            "cutex.launch.draft".to_string(),
            None,
            "devbox".to_string(),
            "/tmp/original".to_string(),
            None,
            "2026-08-06T00:00:00Z".to_string(),
        )
        .expect("record");
        record.display_name_hint = Some("old agent".to_string());
        record.runtime_backend = CutexSessionRuntimeBackend::CuteAlden;
        record.managed_cwd = Some("/tmp/managed".to_string());
        record.default_cli_args = vec!["--no-alt-screen".to_string()];
        let snapshot = SessionSettingsSnapshot::from_record(&record);
        let mut draft = SessionSettingsDraft::default();

        draft
            .stage(
                &snapshot,
                SessionSettingsField::AgentName,
                Some("  renamed agent  ".to_string()),
            )
            .expect("stage name");
        draft
            .stage(
                &snapshot,
                SessionSettingsField::RuntimeBackend,
                Some("native".to_string()),
            )
            .expect("stage backend");
        draft
            .stage(
                &snapshot,
                SessionSettingsField::ManagedCwd,
                Some(r"  E:\Projects (Orchid)\waveline-backend  ".to_string()),
            )
            .expect("stage Windows cwd");
        draft
            .stage(
                &snapshot,
                SessionSettingsField::ExtraCliArgs,
                Some("--model 'gpt next' --no-alt-screen".to_string()),
            )
            .expect("stage CLI args");

        assert_eq!(draft.dirty_count(), 4);
        assert_eq!(draft.agent_name(), Some("renamed agent"));
        assert_eq!(
            draft.value(&snapshot, SessionSettingsField::ManagedCwd),
            Some(r"E:\Projects (Orchid)\waveline-backend")
        );
        assert_eq!(
            draft.effective_working_directory(&snapshot),
            r"E:\Projects (Orchid)\waveline-backend"
        );
        assert_eq!(
            draft.value(&snapshot, SessionSettingsField::ExtraCliArgs),
            Some("'--model' 'gpt next' '--no-alt-screen'")
        );
        let patch = draft.runtime_defaults_patch();
        assert_eq!(
            patch.runtime_backend,
            Some(CutexSessionRuntimeBackend::HostForeground)
        );
        assert_eq!(
            patch.managed_cwd,
            CutexSessionValueUpdate::Set(r"E:\Projects (Orchid)\waveline-backend".to_string())
        );
        assert_eq!(
            patch.default_cli_args,
            Some(vec![
                "--model".to_string(),
                "gpt next".to_string(),
                "--no-alt-screen".to_string(),
            ])
        );
        assert!(draft.launch_actions_are_dirty());
    }

    #[test]
    fn launch_clear_and_invalid_values_preserve_the_last_valid_draft() {
        let mut record = CutexSessionRecord::new_at(
            "cutex.launch.validation".to_string(),
            None,
            "devbox".to_string(),
            "/tmp/original".to_string(),
            None,
            "2026-08-06T00:00:00Z".to_string(),
        )
        .expect("record");
        record.display_name_hint = Some("old agent".to_string());
        record.managed_cwd = Some("/tmp/managed".to_string());
        record.default_cli_args = vec!["--no-alt-screen".to_string()];
        let snapshot = SessionSettingsSnapshot::from_record(&record);
        let mut draft = SessionSettingsDraft::default();

        draft
            .stage(&snapshot, SessionSettingsField::ManagedCwd, None)
            .expect("clear cwd");
        draft
            .stage(&snapshot, SessionSettingsField::ExtraCliArgs, None)
            .expect("clear args");
        assert_eq!(
            draft.effective_working_directory(&snapshot),
            "/tmp/original"
        );
        assert_eq!(
            draft.runtime_defaults_patch().managed_cwd,
            CutexSessionValueUpdate::Clear
        );
        assert_eq!(
            draft.runtime_defaults_patch().default_cli_args,
            Some(vec![])
        );
        let valid = draft.clone();

        assert!(draft
            .stage(&snapshot, SessionSettingsField::AgentName, None)
            .is_err());
        assert!(draft
            .stage(
                &snapshot,
                SessionSettingsField::RuntimeBackend,
                Some("unknown".to_string()),
            )
            .is_err());
        assert!(draft
            .stage(
                &snapshot,
                SessionSettingsField::ManagedCwd,
                Some("-".to_string()),
            )
            .is_err());
        assert!(draft
            .stage(
                &snapshot,
                SessionSettingsField::ExtraCliArgs,
                Some("--model 'unterminated".to_string()),
            )
            .is_err());
        assert_eq!(draft, valid);
    }

    #[test]
    fn routing_projection_only_exposes_workbench_visibility_for_managed_sessions() {
        let mut local = CutexSessionRecord::new_at(
            "cutex.routing.local".to_string(),
            None,
            "devbox".to_string(),
            "/tmp".to_string(),
            None,
            "2026-08-06T00:00:00Z".to_string(),
        )
        .expect("local record");
        local.agent_groups = vec!["cutex".to_string()];
        let local_snapshot = SessionSettingsSnapshot::from_record(&local);
        let local_routing = local_snapshot
            .categories(&SessionSettingsDraft::default())
            .into_iter()
            .find(|category| category.label == "Routing")
            .expect("local routing");

        assert_eq!(
            local_routing
                .options
                .iter()
                .map(|option| option.field)
                .collect::<Vec<_>>(),
            [
                Some(SessionSettingsField::AgentGroups),
                None,
                Some(SessionSettingsField::QuickAction),
            ]
        );

        local.registration_class = AgentRegistrationClass::Persistent;
        let managed_snapshot = SessionSettingsSnapshot::from_record(&local);
        let managed_routing = managed_snapshot
            .categories(&SessionSettingsDraft::default())
            .into_iter()
            .find(|category| category.label == "Routing")
            .expect("managed routing");
        assert_eq!(
            managed_routing
                .options
                .iter()
                .map(|option| option.field)
                .collect::<Vec<_>>(),
            [
                Some(SessionSettingsField::AgentGroups),
                Some(SessionSettingsField::WorkbenchVisibility),
                Some(SessionSettingsField::QuickAction),
            ]
        );
    }

    #[test]
    fn routing_draft_normalizes_tags_and_projects_one_typed_patch() {
        let mut record = CutexSessionRecord::new_at(
            "cutex.routing.draft".to_string(),
            None,
            "devbox".to_string(),
            "/tmp".to_string(),
            None,
            "2026-08-06T00:00:00Z".to_string(),
        )
        .expect("record");
        record.registration_class = AgentRegistrationClass::Persistent;
        record.agent_groups = vec!["cutex".to_string()];
        let snapshot = SessionSettingsSnapshot::from_record(&record);
        let mut draft = SessionSettingsDraft::default();

        draft
            .stage(
                &snapshot,
                SessionSettingsField::AgentGroups,
                Some(" waveline, cutex  waveline ".to_string()),
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

        assert_eq!(draft.dirty_count(), 3);
        assert_eq!(
            draft.value(&snapshot, SessionSettingsField::AgentGroups),
            Some("waveline, cutex")
        );
        assert_eq!(
            draft.routing_patch(),
            CutexSessionRoutingPatch {
                agent_groups: Some(vec!["waveline".to_string(), "cutex".to_string()]),
                exposed_to_backend: Some(true),
                quick_action: Some(CutexSessionQuickActionMode::Pinned),
            }
        );
        assert!(!draft.runtime_defaults_are_dirty());

        let original_patch = draft.routing_patch();
        let error = draft
            .stage(
                &snapshot,
                SessionSettingsField::AgentGroups,
                Some(" ,  ".to_string()),
            )
            .expect_err("empty groups must be rejected");
        assert!(error.to_string().contains("At least one"));
        assert_eq!(draft.routing_patch(), original_patch);
    }

    #[test]
    fn profile_editor_uses_the_read_only_catalog_and_stages_outside_runtime_defaults() {
        let record = CutexSessionRecord::new_at(
            "cutex.profile".to_string(),
            None,
            "devbox".to_string(),
            "/tmp".to_string(),
            Some("alpha".to_string()),
            "2026-08-05T00:00:00Z".to_string(),
        )
        .expect("record");
        let profile_names = vec!["alpha".to_string(), "beta".to_string()];
        let snapshot = SessionSettingsSnapshot::from_record_with_profiles(&record, &profile_names);
        let choices = snapshot.choices(SessionSettingsField::Profile);
        assert_eq!(
            choices
                .iter()
                .map(|choice| choice.label.as_str())
                .collect::<Vec<_>>(),
            ["Follow global default", "alpha", "beta"]
        );
        assert_eq!(choices[0].value, None);

        let mut draft = SessionSettingsDraft::default();
        draft
            .stage(
                &snapshot,
                SessionSettingsField::Profile,
                Some("beta".to_string()),
            )
            .expect("stage profile");

        assert_eq!(draft.dirty_count(), 1);
        assert_eq!(
            draft.profile_update(),
            &CutexSessionValueUpdate::Set("beta".to_string())
        );
        assert!(!draft.runtime_defaults_are_dirty());

        draft
            .stage(&snapshot, SessionSettingsField::Profile, None)
            .expect("stage inherited profile");
        assert_eq!(draft.profile_update(), &CutexSessionValueUpdate::Clear);
        assert_eq!(draft.dirty_count(), 1);
    }

    #[test]
    fn global_projection_is_categorized_and_masks_every_stored_token() {
        let config = CodezConfig {
            notify_service_token: Some("notify-secret".to_string()),
            desktop_notify_token: Some("desktop-secret".to_string()),
            agent_bus_token: Some("bus-secret".to_string()),
            ..CodezConfig::default()
        };

        let categories = global_setting_categories(&config);
        assert_eq!(
            categories
                .iter()
                .map(|category| category.label)
                .collect::<Vec<_>>(),
            vec![
                "Profiles", "Defaults", "Network", "Notifications", "Appearance", "Messages", "Services", "Hosts / Connections",
            ]
        );
        let settings = flattened(&categories);

        assert!(settings.contains("Defaults:Default profile="));
        assert!(settings.contains("Manage profiles"));
        assert_eq!(settings.matches("=(set)").count(), 1);
        assert!(!settings.contains("notify-secret"));
        assert!(!settings.contains("desktop-secret"));
        assert!(!settings.contains("bus-secret"));
        assert!(!settings.contains("runtime_agent_id"));
        assert!(settings.contains("Defaults:Claude usage refresh (m)=5"));
        assert_eq!(
            categories
                .iter()
                .flat_map(|category| category.options.iter())
                .filter(|option| option.global_field.is_some())
                .count(),
            29 // r219: minus Task sort and the three Task Watchdog rows
        );
    }

    #[test]
    fn new_defaults_keep_models_scoped_and_discard_without_writes() {
        let config = CodezConfig { default_profile: Some("alpha".into()), ..Default::default() };
        let snapshot = GlobalSettingsSnapshot::from_config_with_profiles(&config, &["alpha".into(), "beta".into()]);
        let mut draft = GlobalSettingsDraft::default();
        draft.stage(&snapshot, GlobalSettingsField::DefaultModel, Some("model-a".into())).unwrap();
        draft.stage(&snapshot, GlobalSettingsField::DefaultReasoning, Some("high".into())).unwrap();
        draft.stage(&snapshot, GlobalSettingsField::DefaultProfile, Some("beta".into())).unwrap();
        assert_eq!(draft.value(&snapshot, GlobalSettingsField::DefaultModel), "");
        draft.stage(&snapshot, GlobalSettingsField::DefaultModel, Some("model-b".into())).unwrap();
        draft.stage(&snapshot, GlobalSettingsField::DefaultNotification, Some("normal".into())).unwrap();
        let mut saved = config.clone();
        cutex::config::global_settings::apply_global_config_patch(&mut saved, &draft.patch(&config).unwrap()).unwrap();
        assert_eq!(saved.new_session_defaults["alpha"].model.as_deref(), Some("model-a"));
        assert_eq!(saved.new_session_defaults["alpha"].reasoning.as_deref(), Some("high"));
        assert_eq!(saved.new_session_defaults["beta"].model.as_deref(), Some("model-b"));
        assert_eq!(saved.new_session_defaults["beta"].reasoning, None);
        assert_eq!(saved.new_session_notification, cutex::notify::session::Level::Normal);
        assert!(config.new_session_defaults.is_empty());
        assert!(draft.is_dirty());
        draft = GlobalSettingsDraft::default();
        assert!(!draft.is_dirty());
    }

    #[test]
    fn global_profile_defaults_use_catalog_choices_and_one_typed_patch() {
        let config = CodezConfig {
            default_profile: Some("alpha".to_string()),
            ..CodezConfig::default()
        };
        let profile_names = vec!["alpha".to_string(), "beta".to_string()];
        let snapshot = GlobalSettingsSnapshot::from_config_with_profiles(&config, &profile_names);
        let choices = snapshot.choices(GlobalSettingsField::DefaultProfile);
        assert_eq!(
            choices
                .iter()
                .map(|choice| (choice.label.as_str(), choice.value.as_deref()))
                .collect::<Vec<_>>(),
            vec![
                ("None", None),
                ("alpha", Some("alpha")),
                ("beta", Some("beta")),
            ]
        );

        let mut draft = GlobalSettingsDraft::default();
        draft
            .stage(
                &snapshot,
                GlobalSettingsField::DefaultProfile,
                Some("beta".to_string()),
            )
            .expect("stage default profile");
        draft
            .stage(
                &snapshot,
                GlobalSettingsField::DefaultProfileDirectLaunch,
                Some("enabled".to_string()),
            )
            .expect("stage direct launch");

        assert_eq!(draft.dirty_count(), 2);
        assert_eq!(
            draft.patch(&config).expect("profile defaults patch"),
            GlobalConfigPatch {
                default_profile: ConfigValueUpdate::Set("beta".to_string()),
                default_profile_direct_launch: Some(true),
                ..GlobalConfigPatch::default()
            }
        );
        assert!(draft.validate_profile_catalog(&profile_names).is_ok());
        assert!(draft
            .validate_profile_catalog(&["alpha".to_string()])
            .expect_err("stale profile must fail")
            .to_string()
            .contains("no longer available"));
    }

    #[test]
    fn stale_configured_default_can_be_preserved_or_cleared_without_becoming_a_choice() {
        let config = CodezConfig {
            default_profile: Some("removed-profile".to_string()),
            ..CodezConfig::default()
        };
        let snapshot =
            GlobalSettingsSnapshot::from_config_with_profiles(&config, &["available".to_string()]);
        let mut draft = GlobalSettingsDraft::default();

        draft
            .stage(
                &snapshot,
                GlobalSettingsField::DefaultProfile,
                Some("removed-profile".to_string()),
            )
            .expect("preserve stale current value");
        assert_eq!(draft.dirty_count(), 0);
        assert!(!snapshot
            .choices(GlobalSettingsField::DefaultProfile)
            .iter()
            .any(|choice| choice.value.as_deref() == Some("removed-profile")));

        draft
            .stage(&snapshot, GlobalSettingsField::DefaultProfile, None)
            .expect("clear stale default");
        assert_eq!(draft.dirty_count(), 1);
        assert_eq!(
            draft.patch(&config).expect("clear patch").default_profile,
            ConfigValueUpdate::Clear
        );
    }

    #[test]
    fn global_general_proxy_draft_builds_one_typed_patch() {
        let mut config = CodezConfig::default();
        config.proxy = Some(ProxyConfig {
            enabled: true,
            url: Some("http://127.0.0.1:7890".to_string()),
            no_proxy: None,
        });
        let snapshot = GlobalSettingsSnapshot::from_config(&config);
        let mut draft = GlobalSettingsDraft::default();
        draft
            .stage(
                &snapshot,
                GlobalSettingsField::ManagedSessions,
                Some("enabled".to_string()),
            )
            .expect("stage sessions");
        draft
            .stage(
                &snapshot,
                GlobalSettingsField::DockerSudo,
                Some("enabled".to_string()),
            )
            .expect("stage sudo");
        draft
            .stage(
                &snapshot,
                GlobalSettingsField::ProxyUrl,
                Some("socks5h://127.0.0.1:7891".to_string()),
            )
            .expect("stage URL");
        draft
            .stage(
                &snapshot,
                GlobalSettingsField::ProxyNoProxy,
                Some("localhost,127.0.0.1".to_string()),
            )
            .expect("stage NO_PROXY");

        assert_eq!(draft.dirty_count(), 4);
        assert_eq!(
            draft.value(&snapshot, GlobalSettingsField::ProxyUrl),
            "socks5h://127.0.0.1:7891"
        );
        assert_eq!(
            draft.patch(&config).expect("valid patch"),
            GlobalConfigPatch {
                docker_use_sudo: Some(true),
                session_enabled: Some(true),
                proxy: ConfigValueUpdate::Set(ProxyConfig {
                    enabled: true,
                    url: Some("socks5h://127.0.0.1:7891".to_string()),
                    no_proxy: Some("localhost,127.0.0.1".to_string()),
                }),
                ..GlobalConfigPatch::default()
            }
        );
        let projected = flattened(&snapshot.categories(&draft));

    }

    #[test]
    fn global_proxy_validation_keeps_the_complete_draft_for_correction() {
        let config = CodezConfig::default();
        let snapshot = GlobalSettingsSnapshot::from_config(&config);
        let mut draft = GlobalSettingsDraft::default();
        draft
            .stage(
                &snapshot,
                GlobalSettingsField::ProxyEnabled,
                Some("enabled".to_string()),
            )
            .expect("stage enabled");
        assert!(draft.patch(&config).is_err());
        assert_eq!(draft.dirty_count(), 1);

        draft
            .stage(
                &snapshot,
                GlobalSettingsField::ProxyUrl,
                Some("ftp://127.0.0.1:21".to_string()),
            )
            .expect("stage invalid URL for apply validation");
        let valid_draft = draft.clone();
        let error = draft.patch(&config).expect_err("scheme must be rejected");
        assert!(error.to_string().contains("Unsupported proxy scheme"));
        assert_eq!(draft, valid_draft);

        draft
            .stage(
                &snapshot,
                GlobalSettingsField::ProxyEnabled,
                Some("disabled".to_string()),
            )
            .expect("disable proxy");
        assert_eq!(draft.dirty_count(), 0);
    }

    #[test]
    fn global_notification_rate_draft_builds_one_masked_typed_patch() {
        let mut config = CodezConfig::default();
        config.notify_service_token = Some("old-notify-secret".to_string());
        config.notify_service_composer_idle_timeout_secs = Some(30);
        config.desktop_notify_token = Some("old-desktop-secret".to_string());
        let snapshot = GlobalSettingsSnapshot::from_config(&config);
        let mut draft = GlobalSettingsDraft::default();

        for (field, value) in [
            (
                GlobalSettingsField::NotifyServiceUrl,
                Some("https://notify.test/push"),
            ),
            (GlobalSettingsField::NotifyIdleTimeout, Some("90")),
            (GlobalSettingsField::NotifyComposerTimeout, None),
            (GlobalSettingsField::NotifyApprovalTimeout, Some("60")),
            (GlobalSettingsField::NotifyStartupTimeout, Some("120")),
            (
                GlobalSettingsField::NotifyEvents,
                Some("turn-completed approval_requested"),
            ),
            (GlobalSettingsField::NotifyMessageContent, Some("preview")),
            (GlobalSettingsField::NotifyPreviewChars, Some("80")),
            (GlobalSettingsField::DesktopNotifyEnabled, Some("enabled")),
            (GlobalSettingsField::DesktopNotifyPort, Some("24251")),
            (
                GlobalSettingsField::RateLimitThresholdWarning,
                Some("daily"),
            ),
            (GlobalSettingsField::RateLimitModelNudge, Some("always")),
        ] {
            draft
                .stage(&snapshot, field, value.map(str::to_string))
                .expect("stage notification field");
        }
        draft
            .stage_secret(
                &snapshot,
                GlobalSettingsField::NotifyServiceToken,
                SecretSettingsAction::Replace("new-notify-secret".to_string()),
            )
            .expect("replace notify secret");
        draft
            .stage_secret(
                &snapshot,
                GlobalSettingsField::DesktopNotifyToken,
                SecretSettingsAction::Clear,
            )
            .expect("clear desktop secret");

        assert_eq!(draft.dirty_count(), 14);
        assert_eq!(
            draft.value(&snapshot, GlobalSettingsField::NotifyServiceToken),
            "(replace staged)"
        );
        assert_eq!(
            draft.value(&snapshot, GlobalSettingsField::DesktopNotifyToken),
            "(clear staged)"
        );
        let projected = flattened(&snapshot.categories(&draft));
        assert!(!projected.contains("old-notify-secret"));
        assert!(!projected.contains("new-notify-secret"));
        assert!(!projected.contains("old-desktop-secret"));
        assert_eq!(
            draft.patch(&config).expect("notification patch"),
            GlobalConfigPatch {
                notify_service_url: ConfigValueUpdate::Set("https://notify.test/push".to_string()),
                notify_service_token: ConfigValueUpdate::Set("new-notify-secret".to_string()),
                notify_service_idle_timeout_secs: ConfigValueUpdate::Set(90),
                notify_service_composer_idle_timeout_secs: ConfigValueUpdate::Clear,
                notify_service_approval_timeout_secs: ConfigValueUpdate::Set(60),
                notify_service_startup_idle_timeout_secs: ConfigValueUpdate::Set(120),
                notify_service_events: ConfigValueUpdate::Set(vec![
                    "turn_completed".to_string(),
                    "approval_requested".to_string(),
                ]),
                notify_service_user_message_content: ConfigValueUpdate::Set("preview".to_string()),
                notify_service_user_message_preview_chars: ConfigValueUpdate::Set(80),
                rate_limit_threshold_warning_mode: ConfigValueUpdate::Set("daily".to_string()),
                rate_limit_model_nudge_mode: ConfigValueUpdate::Set("always".to_string()),
                desktop_notify_enabled: Some(true),
                desktop_notify_port: ConfigValueUpdate::Set(24251),
                desktop_notify_token: ConfigValueUpdate::Clear,
                ..GlobalConfigPatch::default()
            }
        );
    }

    #[test]
    fn global_notification_validation_and_secret_keep_leave_draft_coherent() {
        let mut config = CodezConfig::default();
        config.notify_service_token = Some("stored-secret".to_string());
        config.notify_service_user_message_content = Some("legacy-message".to_string());
        config.rate_limit_threshold_warning_mode = Some("legacy-rate".to_string());
        let snapshot = GlobalSettingsSnapshot::from_config(&config);
        let mut draft = GlobalSettingsDraft::default();

        assert!(draft
            .stage(
                &snapshot,
                GlobalSettingsField::NotifyIdleTimeout,
                Some("not-a-number".to_string()),
            )
            .is_err());
        assert!(draft
            .stage(
                &snapshot,
                GlobalSettingsField::DesktopNotifyPort,
                Some("8080".to_string()),
            )
            .is_err());
        assert!(draft
            .stage_secret(
                &snapshot,
                GlobalSettingsField::NotifyServiceToken,
                SecretSettingsAction::Replace("  ".to_string()),
            )
            .is_err());
        assert_eq!(draft.dirty_count(), 0);
        assert_eq!(
            draft.value(&snapshot, GlobalSettingsField::NotifyMessageContent),
            "legacy-message"
        );
        assert_eq!(
            draft.value(&snapshot, GlobalSettingsField::RateLimitThresholdWarning),
            "legacy-rate"
        );
        draft
            .stage(
                &snapshot,
                GlobalSettingsField::NotifyMessageContent,
                Some("legacy-message".to_string()),
            )
            .expect("keep legacy message mode");
        draft
            .stage(
                &snapshot,
                GlobalSettingsField::RateLimitThresholdWarning,
                Some("legacy-rate".to_string()),
            )
            .expect("keep legacy rate mode");
        assert_eq!(draft.dirty_count(), 0);

        draft
            .stage_secret(
                &snapshot,
                GlobalSettingsField::NotifyServiceToken,
                SecretSettingsAction::Replace("replacement".to_string()),
            )
            .expect("stage replacement");
        assert_eq!(draft.dirty_count(), 1);
        draft
            .stage_secret(
                &snapshot,
                GlobalSettingsField::NotifyServiceToken,
                SecretSettingsAction::Keep,
            )
            .expect("restore keep");
        assert_eq!(draft.dirty_count(), 0);
    }

    #[test]
    fn global_agent_bus_draft_validates_port_and_masks_token() {
        // Default-instance port rules (59995 invalid, 24261 valid).
        cutex::config::instance::with_default_ports_for_test(global_agent_bus_draft_validates_port_and_masks_token_inner);
    }

    fn global_agent_bus_draft_validates_port_and_masks_token_inner() {
        let mut config = CodezConfig::default();
        config.agent_bus_enabled = false;
        config.agent_bus_token = Some("stored-bus-secret".to_string());
        let snapshot = GlobalSettingsSnapshot::from_config(&config);
        let mut draft = GlobalSettingsDraft::default();
        assert!(draft
            .stage(
                &snapshot,
                GlobalSettingsField::AgentBusPort,
                Some("59995".to_string()),
            )
            .is_err());
        assert_eq!(draft.dirty_count(), 0);

        for (field, value) in [
            (GlobalSettingsField::AgentBusEnabled, "enabled"),
            (GlobalSettingsField::AgentBusPort, "24261"),
        ] {
            draft
                .stage(&snapshot, field, Some(value.to_string()))
                .expect("stage Agent Bus field");
        }
        draft
            .stage_secret(
                &snapshot,
                GlobalSettingsField::AgentBusToken,
                SecretSettingsAction::Replace("new-bus-secret".to_string()),
            )
            .expect("replace Agent Bus token");

        assert_eq!(draft.dirty_count(), 3);
        assert_eq!(
            draft.value(&snapshot, GlobalSettingsField::AgentBusToken),
            "(replace staged)"
        );
        assert_eq!(
            draft.patch(&config).expect("Agent Bus patch"),
            GlobalConfigPatch {
                agent_bus_enabled: Some(true),
                agent_bus_port: ConfigValueUpdate::Set(24261),
                agent_bus_token: ConfigValueUpdate::Set("new-bus-secret".to_string()),
                ..GlobalConfigPatch::default()
            }
        );
        let projected = flattened(&snapshot.categories(&draft));
        assert!(!projected.contains("stored-bus-secret"));
        assert!(!projected.contains("new-bus-secret"));
    }
}
