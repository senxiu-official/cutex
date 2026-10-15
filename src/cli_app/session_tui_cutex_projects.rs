//! Human-authenticated Cutex Project management workspace.

use std::io::{self, IsTerminal, Stdout};

use anyhow::Context;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use cutex::agent_management::{
    AgentActionId, CutexProjectSummary, CutexProjectWorkspace, ProjectAccessRole,
    ProjectAgentChoice, ProjectMemberLifecycle, ProjectPaletteColor, ProjectPresentationInput,
};
use cutex::management::control_plane::{
    HumanManagementOperatorActionRequest, HumanManagementOperatorKind,
    HumanManagementOperatorSchema, HumanManagementPresentationSchema,
    HumanManagementPresentationUpdateRequest, HumanManagementProjectMutationKind,
    HumanManagementProjectMutationRequest, HumanManagementProjectMutationSchema,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;
use super::session_tui_terminal::Terminal;
use tui_input::Input;
use uuid::Uuid;

use super::management_control_plane::ManagementControlClient;
use super::session_tui::footer_hints;
use super::session_tui_input::{self as input_policy, Command, Gate, Help, LeaveReview};
use super::session_tui_view::{self as views, AgentSessionView, ListKind, SubjectRef};
use super::session_tui_workspace::{PrimaryPanel, PrimaryPanelOutcome};

type ProjectTerminal = Terminal<CrosstermBackend<Stdout>>;

#[cfg(test)]
pub(super) fn terminal_fixture() -> CutexProjectsModel {
    CutexProjectsModel::empty_with_failure("isolated empty fixture; no provider access")
}

#[derive(Debug, Clone)]
struct PresentationEditor {
    display_name: String,
    badge_label: String,
    color: String,
    field: usize,
}

#[derive(Debug, Clone)]
struct ProjectCreateEditor {
    project_id: String,
    display_name: String,
    badge_label: String,
    color: String,
    director: Option<cutex::role_revision::CutexSessionId>,
    field: usize,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ProjectView {
    ConfirmImport,
    List,
    Details,
    Editor,
    Create,
    DirectorPicker,
    Actions,
    ConfirmProjectMutation,
    ConfirmOperator,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ProjectSection {
    Overview,
    Members,
    Operators,
}

impl ProjectSection {
    const ALL: [Self; 2] = [Self::Members, Self::Overview];

    fn shifted(self, direction: isize) -> Self {
        let index = Self::ALL
            .iter()
            .position(|section| *section == self)
            .unwrap_or(0);
        let len = Self::ALL.len() as isize;
        Self::ALL[((index as isize + direction).rem_euclid(len)) as usize]
    }

    fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Members => "Members",
            Self::Operators => "Operators",
        }
    }
}

#[derive(Debug, Clone)]
struct OperatorTarget {
    cutex_session_id: cutex::role_revision::CutexSessionId,
    name: String,
    lifecycle: ProjectMemberLifecycle,
    operation: HumanManagementOperatorKind,
}

#[derive(Debug, Clone)]
struct ProjectMutationTarget {
    label: String,
    operation: HumanManagementProjectMutationKind,
}

#[derive(Clone)]
enum ProjectMenuAction {
    AgentActions(String), AgentSettings(String), Operators, Appearance,
    Mutation(ProjectMutationTarget),
}
impl ProjectMenuAction {
    fn label(&self) -> String {
        match self {
            Self::AgentActions(name) => format!("Agent actions: {name}"),
            Self::AgentSettings(name) => format!("Agent settings: {name}"),
            Self::Operators => "Manage Operators".into(),
            Self::Appearance => "Edit project name / badge / color".into(),
            Self::Mutation(target) => target.label.clone(),
        }
    }
}

#[derive(Debug)]
pub(super) struct CutexProjectsModel {
    sort_order: cutex::profiles::list_preferences::ListSort,
    details_text: Option<String>,
    status_scroll: views::DetailScroll,
    detail_scroll: views::DetailScroll,
    member_selected: Option<SubjectRef>,
    member_index: usize,
    member_inspecting: bool,
    project_inspecting: bool,
    project_scroll: views::DetailScroll,
    member_table: std::cell::RefCell<TableState>,
    table_state: std::cell::RefCell<TableState>,
    help: Option<Help>,
    leave_review: Option<LeaveReview>,
    text_cursors: [Option<usize>; 4],
    pub(super) open_settings_requested: bool,
    pub(super) member_action_requested: Option<(String, super::session_tui::SelectorEvent)>,
    durable_candidates: Vec<cutex::agent_management::DurableAgentCandidate>,
    import_request: Option<cutex::agent_management::DurableImportRequest>,
    import_name: Input,
    import_name_focused: bool,
    projects: Vec<CutexProjectSummary>,
    archived_projects: Vec<CutexProjectSummary>,
    available_agents: Vec<ProjectAgentChoice>,
    selected: usize,
    query: Input,
    filter_focused: bool,
    show_archived: bool,
    show_archived_members: bool,
    details: Option<CutexProjectWorkspace>,
    section: ProjectSection,
    operator_selected: usize,
    pending_operator: Option<OperatorTarget>,
    pending_project_mutation: Option<ProjectMutationTarget>,
    action_selected: usize,
    action_filter: String,
    confirm_selected: bool,
    editor: Option<PresentationEditor>,
    create_editor: Option<ProjectCreateEditor>,
    director_query: Input,
    director_picker_selected: Option<cutex::role_revision::CutexSessionId>,
    view: ProjectView,
    client: Option<ManagementControlClient>,
    pub(super) failure: Option<String>,
    notice: Option<String>,
}

impl CutexProjectsModel {
    fn empty_with_failure(error: impl Into<String>) -> Self {
        Self {
            sort_order: Default::default(),
            member_selected: None,
            member_index: 0,
            member_inspecting: false,
            project_inspecting: false,
            project_scroll: Default::default(),
            detail_scroll: Default::default(),
            details_text: None,
            status_scroll: Default::default(),
            member_table: Default::default(),
            table_state: Default::default(),
            help: None,
            leave_review: None,
            text_cursors: [None; 4],
            open_settings_requested: false,
            durable_candidates: Vec::new(),
            import_request: None,
            import_name: Input::default(),
            import_name_focused: false,
            projects: Vec::new(),
            archived_projects: Vec::new(),
            available_agents: Vec::new(),
            selected: 0,
            query: Input::default(),
            filter_focused: false,
            show_archived: false,
            show_archived_members: false,
            member_action_requested: None,
            details: None,
            section: ProjectSection::Members,
            operator_selected: 0,
            pending_operator: None,
            pending_project_mutation: None,
            action_selected: 0,
            action_filter: String::new(),
            confirm_selected: false,
            editor: None,
            create_editor: None,
            director_query: Input::default(),
            director_picker_selected: None,
            view: ProjectView::List,
            client: None,
            failure: Some(error.into()),
            notice: None,
        }
    }

    fn visible_indices(&self) -> Vec<usize> {
        let query = self.query.value().trim().to_lowercase();
        let mut indices: Vec<usize> = self.projects
            .iter()
            .enumerate()
            .filter_map(|(index, project)| {
                ((!matches!(
                    project.lifecycle,
                    cutex::agent_management::ProjectLifecycle::Archived
                ) || self.show_archived)
                    && (query.is_empty()
                        || project
                            .presentation
                            .display_name
                            .to_lowercase()
                            .contains(&query)
                        || project.project_id.as_str().to_lowercase().contains(&query)
                        || project
                            .presentation
                            .badge_label
                            .to_lowercase()
                            .contains(&query)
                        || project
                            .director_name
                            .as_deref()
                            .or(project.director_cutex_session_id.as_ref().map(|id| id.as_str()))
                            .unwrap_or("")
                            .to_lowercase()
                            .contains(&query)))
                .then_some(index)
            })
            .collect();
        indices.sort_by(|a, b| self.sort_order.compare_names(
            &self.projects[*a].presentation.display_name,
            &self.projects[*b].presentation.display_name));
        indices
    }

    fn retain_selection(&mut self) {
        self.selected = self
            .selected
            .min(self.visible_indices().len().saturating_sub(1));
    }

    fn selected_project(&self) -> Option<&CutexProjectSummary> {
        let index = *self.visible_indices().get(self.selected)?;
        self.projects.get(index)
    }

    fn operator_targets(&self) -> Vec<OperatorTarget> {
        let Some(details) = self.details.as_ref() else {
            return Vec::new();
        };
        let mut targets = details
            .agent_operators
            .iter()
            .map(|operator| OperatorTarget {
                cutex_session_id: operator.member.agent.cutex_session_id.clone(),
                name: operator.member.agent.spec.name.clone(),
                lifecycle: operator.member.lifecycle,
                operation: HumanManagementOperatorKind::Revoke,
            })
            .chain(details.active_agents.iter().map(|member| OperatorTarget {
                cutex_session_id: member.agent.cutex_session_id.clone(),
                name: member.agent.spec.name.clone(),
                lifecycle: member.lifecycle,
                operation: HumanManagementOperatorKind::Grant,
            }))
            .collect::<Vec<_>>();
        targets.sort_by(|left, right| {
            operation_rank(left.operation)
                .cmp(&operation_rank(right.operation))
                .then_with(|| left.cutex_session_id.cmp(&right.cutex_session_id))
        });
        targets
    }

    fn begin_editor(&mut self) {
        self.text_cursors = [None; 4];
        let Some(details) = self.details.as_ref() else {
            return;
        };
        self.editor = Some(PresentationEditor {
            display_name: details.presentation.display_name.clone(),
            badge_label: details.presentation.badge_label.clone(),
            color: details.presentation.color.token(),
            field: 0,
        });
        self.view = ProjectView::Editor;
        self.failure = None;
    }

    fn begin_create(&mut self) {
        self.text_cursors = [None; 4];
        if self.available_agents.is_empty() {
            self.notice = Some(
                "No persistent durable Agent candidates are available. Adopt an Agent first; an Online runtime is not required.".to_string(),
            );
        }
        self.create_editor = Some(ProjectCreateEditor {
            project_id: String::new(),
            display_name: String::new(),
            badge_label: "CX".to_string(),
            color: ProjectPaletteColor::Cyan.token(),
            director: None,
            field: 0,
        });
        self.view = ProjectView::Create;
        self.failure = None;
    }

    fn visible_director_indices(&self) -> Vec<usize> {
        let query = self.director_query.value().trim().to_lowercase();
        self.available_agents
            .iter()
            .enumerate()
            .filter_map(|(index, agent)| {
                let candidate = self
                    .durable_candidates
                    .iter()
                    .find(|row| row.cutex_session_id.as_ref() == Some(&agent.cutex_session_id));
                let matches = query.is_empty()
                    || agent
                        .cutex_session_id
                        .as_str()
                        .to_lowercase()
                        .contains(&query)
                    || candidate
                        .and_then(|row| row.formal_name.as_deref())
                        .is_some_and(|name| name.to_lowercase().contains(&query))
                    || agent
                        .current_project_id
                        .as_ref()
                        .is_some_and(|id| id.as_str().to_lowercase().contains(&query));
                matches.then_some(index)
            })
            .collect()
    }

    fn begin_director_picker(&mut self) {
        self.director_query.reset();
        self.director_picker_selected = self
            .create_editor
            .as_ref()
            .and_then(|editor| editor.director.clone());
        self.view = ProjectView::DirectorPicker;
        self.failure = None;
    }

    fn begin_operator_confirmation(&mut self) {
        let targets = self.operator_targets();
        let Some(target) = targets.get(self.operator_selected).cloned() else {
            self.notice = Some("No Operator action is available in this Project.".to_string());
            return;
        };
        self.pending_operator = Some(target);
        self.confirm_selected = false;
        self.view = ProjectView::ConfirmOperator;
    }

    fn project_actions(&self) -> Vec<ProjectMutationTarget> {
        let Some(details) = self.details.as_ref() else {
            return Vec::new();
        };
        match details.lifecycle {
            cutex::agent_management::ProjectLifecycle::Active => {
                let mut actions = self
                    .available_agents
                    .iter()
                    .map(|agent| ProjectMutationTarget {
                        label: format!(
                            "Add / move member: {} ({})",
                            agent.name,
                            agent.cutex_session_id.as_str()
                        ),
                        operation: HumanManagementProjectMutationKind::AddMember {
                            cutex_session_id: agent.cutex_session_id.clone(),
                        },
                    })
                    .collect::<Vec<_>>();
                actions.extend(
                    details
                        .active_agents
                        .iter()
                        .filter(|member| {
                            details.director.as_ref().is_none_or(|director| member.agent.cutex_session_id != director.cutex_session_id)
                        })
                        .map(|member| ProjectMutationTarget {
                            label: format!(
                                "Detach member {} ({})",
                                member.agent.spec.name,
                                member.agent.cutex_session_id.as_str()
                            ),
                            operation: HumanManagementProjectMutationKind::DetachMember {
                                cutex_session_id: member.agent.cutex_session_id.clone(),
                            },
                        }),
                );
                actions.push(ProjectMutationTarget {
                    label: "Archive Project (recoverable)".to_string(),
                    operation: HumanManagementProjectMutationKind::Archive,
                });
                actions
            }
            cutex::agent_management::ProjectLifecycle::Archived => vec![
                ProjectMutationTarget {
                    label: "Restore Project".to_string(),
                    operation: HumanManagementProjectMutationKind::Restore,
                },
                ProjectMutationTarget {
                    label: "Remove Project permanently; runtime Agents remain".to_string(),
                    operation: HumanManagementProjectMutationKind::Remove,
                },
            ],
            cutex::agent_management::ProjectLifecycle::Removed => Vec::new(),
        }
    }

    fn action_menu(&self) -> Vec<ProjectMenuAction> {
        let mut actions = Vec::new();
        if self.section == ProjectSection::Members {
            if let Some(member) = selected_member(self) {
                actions.push(ProjectMenuAction::AgentActions(member.name.clone()));
                actions.push(ProjectMenuAction::AgentSettings(member.name));
            }
        }
        actions.push(ProjectMenuAction::Operators);
        actions.push(ProjectMenuAction::Appearance);
        actions.extend(self.project_actions().into_iter().map(ProjectMenuAction::Mutation));
        actions.into_iter().filter(|action| action.label().to_lowercase().contains(&self.action_filter.to_lowercase())).collect()
    }

    fn begin_project_actions(&mut self) {
        self.action_filter.clear();
        self.action_selected = 0;
        self.member_inspecting = false;
        self.view = ProjectView::Actions;
        self.failure = None;
    }
}

fn operation_rank(operation: HumanManagementOperatorKind) -> u8 {
    match operation {
        HumanManagementOperatorKind::Revoke => 0,
        HumanManagementOperatorKind::Grant => 1,
    }
}

pub(super) fn run(
    terminal: &mut ProjectTerminal,
    events: &mut super::session_tui::ShellEvents,
    previous_model: Option<CutexProjectsModel>,
    sort_order: cutex::profiles::list_preferences::ListSort,
) -> anyhow::Result<(PrimaryPanelOutcome, CutexProjectsModel)> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        anyhow::bail!("Cutex Projects requires an interactive terminal");
    }
    let needs_initial_load = previous_model.is_none();
    let mut model = previous_model.unwrap_or_else(|| {
        let mut loading = CutexProjectsModel::empty_with_failure("");
        loading.failure = None;
        loading.notice = Some("Loading Cutex Projects…".to_string());
        loading
    });
    if needs_initial_load {
        // Draw a complete first frame before any synchronous service discovery
        // or authenticated request. A slow or failed Management start must not
        // leave the user looking at a cleared terminal with no explanation.
        terminal.draw(|frame| render(frame, &model))?;
        model = load_model().unwrap_or_else(|error| {
            CutexProjectsModel::empty_with_failure(format!("Cutex Projects unavailable: {error:#}"))
        });
        model.sort_order = sort_order;
    } else if model.view == ProjectView::Create {
        // Return from saved-session selection without replacing the draft.
        match model
            .client
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Management unavailable"))
            .and_then(|c| c.durable_candidates())
        {
            Ok(candidates) => {
                let selected = model
                    .create_editor
                    .as_ref()
                    .and_then(|editor| editor.director.clone());
                model.available_agents = candidate_choices(&candidates);
                model.durable_candidates = candidates;
                if let Some(editor) = model.create_editor.as_mut() {
                    editor.director = selected.filter(|id| {
                        model
                            .available_agents
                            .iter()
                            .any(|agent| &agent.cutex_session_id == id)
                    });
                }
            }
            Err(error) => {
                model.failure = Some(format!(
                    "Candidate refresh failed; draft retained: {error:#}"
                ))
            }
        }
    }
    let result = run_loop(terminal, events, &mut model);
    Ok((result?, model))
}

pub(super) fn refresh_after_member_action(model: &mut CutexProjectsModel) {
    if let Err(error) = load_details(model) {
        model.failure = Some(format!(
            "Member snapshot is stale after action/return: {error:#}"
        ));
    }
}

fn load_model() -> anyhow::Result<CutexProjectsModel> {
    let client = ManagementControlClient::connect()
        .context("authenticated Human/Management control plane is required")?;
    let collection = client.projects()?;
    let mut projects = collection.projects;
    let archived_projects = collection.archived_projects;
    projects.extend(archived_projects.iter().cloned());
    let durable_candidates = client.durable_candidates()?;
    let available_agents = candidate_choices(&durable_candidates);
    Ok(CutexProjectsModel {
        sort_order: Default::default(),
        member_selected: None,
        member_index: 0,
        member_inspecting: false,
            project_inspecting: false,
            project_scroll: Default::default(),
        detail_scroll: Default::default(),
        details_text: None,
        status_scroll: Default::default(),
        member_table: Default::default(),
        table_state: Default::default(),
        help: None,
        leave_review: None,
        text_cursors: [None; 4],
        open_settings_requested: false,
        import_name_focused: false,
        durable_candidates,
        import_request: None,
        import_name: Input::default(),
        projects,
        archived_projects,
        available_agents,
        selected: 0,
        query: Input::default(),
        filter_focused: false,
        show_archived: false,
        show_archived_members: false,
        member_action_requested: None,
        details: None,
        section: ProjectSection::Members,
        operator_selected: 0,
        pending_operator: None,
        pending_project_mutation: None,
        action_selected: 0,
        action_filter: String::new(),
        confirm_selected: false,
        editor: None,
        create_editor: None,
        director_query: Input::default(),
        director_picker_selected: None,
        view: ProjectView::List,
        client: Some(client),
        failure: None,
        notice: None,
    })
}

fn reload(model: &mut CutexProjectsModel, open_details: bool) -> anyhow::Result<()> {
    let selected_id = model
        .selected_project()
        .map(|project| project.project_id.clone());
    let client = model
        .client
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Management control plane is unavailable"))?;
    let collection = client.projects()?;
    model.projects = collection.projects;
    model.archived_projects = collection.archived_projects;
    model
        .projects
        .extend(model.archived_projects.iter().cloned());
    model.durable_candidates = client.durable_candidates()?;
    model.available_agents = candidate_choices(&model.durable_candidates);
    let visible = model.visible_indices();
    model.selected = selected_id
        .and_then(|id| {
            visible
                .iter()
                .position(|index| model.projects[*index].project_id == id)
        })
        .unwrap_or(0)
        .min(visible.len().saturating_sub(1));
    if open_details {
        load_details(model)?;
    }
    Ok(())
}

fn reload_director_candidates(model: &mut CutexProjectsModel) -> anyhow::Result<()> {
    let candidates = model
        .client
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Management control plane is unavailable"))?
        .durable_candidates()?;
    model.durable_candidates = candidates;
    model.available_agents = candidate_choices(&model.durable_candidates);
    let intended = model.director_picker_selected.clone();
    if intended.as_ref().is_some_and(|id| {
        !model
            .available_agents
            .iter()
            .any(|agent| &agent.cutex_session_id == id)
    }) {
        model.director_picker_selected = None;
        model.failure = Some(
            "Previously highlighted Director is no longer available; explicitly select again."
                .into(),
        );
    } else {
        retain_director_picker_selection(model);
    }
    Ok(())
}

fn selected_member(model: &CutexProjectsModel) -> Option<AgentSessionView> {
    let rows = visible_members(model);
    model
        .member_selected
        .as_ref()
        .and_then(|id| rows.iter().find(|r| r.subject == *id))
        .or_else(|| rows.get(model.member_index))
        .cloned()
}
fn request_member_action(
    model: &mut CutexProjectsModel,
    event: super::session_tui::SelectorEvent,
) -> Option<PrimaryPanelOutcome> {
    let Some(member) = selected_member(model) else {
        model.notice =
            Some("Select a current member; refresh unavailable observations first".into());
        return None;
    };
    if member.retirement_note.is_some() {
        model.notice = Some("Archived/permanently retired member: use Archive to review supported Restore; permanent retirement cannot restore".into());
        return None;
    }
    let SubjectRef::Managed(id) = member.subject else {
        return None;
    };
    model.member_action_requested = Some((id, event));
    Some(PrimaryPanelOutcome::Switch(PrimaryPanel::Agents))
}
fn visible_members(model: &CutexProjectsModel) -> Vec<AgentSessionView> {
    let Some(project) = &model.details else {
        return Vec::new();
    };
    let mut rows = views::project_members(project);
    if model.show_archived_members {
        for member in &project.archived_agents {
            let mut view = views::project_member_view(member, project, "Member (archived)");
            view.retirement_note =
                Some("Reversibly archived; membership retained; no runtime activation".into());
            rows.push(view);
        }
    }
    rows
}
fn reconcile_member_selection(model: &mut CutexProjectsModel) {
    let rows = visible_members(model);
    model.member_index = model
        .member_selected
        .as_ref()
        .and_then(|id| rows.iter().position(|r| r.subject == *id))
        .unwrap_or(model.member_index)
        .min(rows.len().saturating_sub(1));
    model.member_selected = rows.get(model.member_index).map(|r| r.subject.clone());
}
fn load_details(model: &mut CutexProjectsModel) -> anyhow::Result<()> {
    let project_id = model
        .selected_project()
        .map(|project| project.project_id.clone())
        .ok_or_else(|| anyhow::anyhow!("no Cutex Project is selected"))?;
    let client = model
        .client
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Management control plane is unavailable"))?;
    let details = client.project(&project_id)?;
    if model
        .details
        .as_ref()
        .is_none_or(|old| old.project_id != project_id)
    {
        model.member_selected = None;
        model.member_index = 0;
        model.member_inspecting = false;
        *model.member_table.borrow_mut() = TableState::default();
    }
    model.details = Some(details);
    reconcile_member_selection(model);
    model.operator_selected = model
        .operator_selected
        .min(model.operator_targets().len().saturating_sub(1));
    model.view = ProjectView::Details;
    model.failure = None;
    Ok(())
}

fn save_editor(model: &mut CutexProjectsModel) -> anyhow::Result<()> {
    let editor = model
        .editor
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("presentation editor is unavailable"))?;
    let details = model
        .details
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("project details are unavailable"))?;
    let color = editor.color.parse::<ProjectPaletteColor>()?;
    let request = HumanManagementPresentationUpdateRequest {
        schema: HumanManagementPresentationSchema::V1,
        project_id: details.project_id.clone(),
        expected_authority_epoch: Some(details.authority_epoch),
        expected_presentation_revision: Some(details.presentation.revision),
        presentation: ProjectPresentationInput {
            display_name: editor.display_name.clone(),
            badge_label: editor.badge_label.clone(),
            color,
        },
    };
    model
        .client
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Management control plane is unavailable"))?
        .update_presentation(&request)?;
    model.editor = None;
    reload(model, true)?;
    model.section = ProjectSection::Overview;
    model.notice =
        Some("Project appearance updated through Human/Management audit boundary".into());
    Ok(())
}

fn save_project_create(model: &mut CutexProjectsModel) -> anyhow::Result<()> {
    let editor = model
        .create_editor
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("project create wizard is unavailable"))?;
    let project_id = cutex::agent_management::ProjectId::new(editor.project_id.clone())?;
    let director = editor
        .director
        .as_ref()
        .and_then(|id| {
            model
                .available_agents
                .iter()
                .find(|agent| &agent.cutex_session_id == id)
        })
        .ok_or_else(|| anyhow::anyhow!("initial Director selection is unavailable"))?;
    let request = HumanManagementProjectMutationRequest {
        schema: HumanManagementProjectMutationSchema::V1,
        action_id: AgentActionId::new(format!("management-project-create-{}", Uuid::new_v4()))?,
        project_id: project_id.clone(),
        expected_authority_epoch: Some(0),
        expected_project_revision: Some(0),
        operation: HumanManagementProjectMutationKind::Create {
            director_cutex_session_id: director.cutex_session_id.clone(),
            presentation: ProjectPresentationInput {
                display_name: editor.display_name.clone(),
                badge_label: editor.badge_label.clone(),
                color: editor.color.parse::<ProjectPaletteColor>()?,
            },
        },
    };
    begin_import_confirmation(model, request)
}

fn candidate_choices(
    candidates: &[cutex::agent_management::DurableAgentCandidate],
) -> Vec<ProjectAgentChoice> {
    candidates
        .iter()
        .filter_map(|agent| {
            Some(ProjectAgentChoice {
                cutex_session_id: agent.cutex_session_id.clone()?,
                name: candidate_label(agent),
                current_project_id: agent.current_project_id.clone(),
            })
        })
        .collect()
}

fn candidate_label(agent: &cutex::agent_management::DurableAgentCandidate) -> String {
    format!(
        "{} · {} · {} · {}{}",
        agent
            .formal_name
            .as_deref()
            .unwrap_or("Formal name required"),
        if agent.in_roster {
            "roster"
        } else {
            "durable, not in roster"
        },
        agent
            .current_project_id
            .as_ref()
            .map(|p| p.as_str())
            .unwrap_or("unassigned"),
        if agent.online { "Online" } else { "Offline" },
        agent
            .rejection
            .as_ref()
            .map(|r| format!(" · Unavailable: {r}"))
            .unwrap_or_default()
    )
}

fn retain_director_picker_selection(model: &mut CutexProjectsModel) {
    let visible = model.visible_director_indices();
    if model.director_picker_selected.as_ref().is_some_and(|id| {
        visible
            .iter()
            .any(|index| model.available_agents[*index].cutex_session_id == *id)
    }) {
        return;
    }
    model.director_picker_selected = visible
        .first()
        .map(|index| model.available_agents[*index].cutex_session_id.clone());
}

fn shift_director_picker(model: &mut CutexProjectsModel, delta: isize) {
    let visible = model.visible_director_indices();
    if visible.is_empty() {
        model.director_picker_selected = None;
        return;
    }
    let current = model
        .director_picker_selected
        .as_ref()
        .and_then(|id| {
            visible
                .iter()
                .position(|index| model.available_agents[*index].cutex_session_id == *id)
        })
        .unwrap_or(0) as isize;
    let next = (current + delta).clamp(0, visible.len().saturating_sub(1) as isize) as usize;
    model.director_picker_selected = Some(
        model.available_agents[visible[next]]
            .cutex_session_id
            .clone(),
    );
}

fn begin_import_confirmation(
    model: &mut CutexProjectsModel,
    assignment: HumanManagementProjectMutationRequest,
) -> anyhow::Result<()> {
    let id = match &assignment.operation {
        HumanManagementProjectMutationKind::Create {
            director_cutex_session_id,
            ..
        } => director_cutex_session_id,
        HumanManagementProjectMutationKind::AddMember { cutex_session_id } => cutex_session_id,
        _ => anyhow::bail!("unsupported import assignment"),
    };
    let candidate = model
        .durable_candidates
        .iter()
        .find(|c| c.cutex_session_id.as_ref() == Some(id))
        .cloned()
        .context("durable candidate unavailable; refresh")?;
    if let Some(reason) = &candidate.rejection {
        anyhow::bail!("Agent is unavailable: {reason}");
    }
    let detach = if let Some(source) = &candidate.current_project_id {
        if source == &assignment.project_id {
            anyhow::bail!("Agent already belongs to this Project");
        }
        let source = model
            .client
            .as_ref()
            .context("Management unavailable")?
            .project(source)?;
        Some(HumanManagementProjectMutationRequest {
            schema: HumanManagementProjectMutationSchema::V1,
            action_id: AgentActionId::new(format!("management-detach-{}", Uuid::new_v4()))?,
            project_id: source.project_id,
            expected_authority_epoch: Some(source.authority_epoch),
            expected_project_revision: Some(source.project_revision),
            operation: HumanManagementProjectMutationKind::DetachMember {
                cutex_session_id: id.clone(),
            },
        })
    } else {
        None
    };
    model.import_name = Input::new(candidate.formal_name.clone().unwrap_or_default());
    model.import_name_focused = candidate.formal_name.is_none();
    model.import_request = Some(cutex::agent_management::DurableImportRequest {
        action_id: AgentActionId::new(format!("management-import-{}", Uuid::new_v4()))?,
        confirmed_formal_name: candidate.formal_name.clone().unwrap_or_default(),
        candidate,
        assignment: Some(assignment),
        detach,
    });
    model.confirm_selected = false;
    model.view = ProjectView::ConfirmImport;
    Ok(())
}

fn execute_import_confirmation(model: &mut CutexProjectsModel) -> anyhow::Result<()> {
    let request = model
        .import_request
        .as_mut()
        .context("import confirmation unavailable")?;
    request.confirmed_formal_name = model.import_name.value().to_string();
    let receipt = model
        .client
        .as_ref()
        .context("Management unavailable")?
        .import_durable_agent(request)?;
    if !receipt.complete {
        anyhow::bail!("Action {} incomplete: name set={}, imported={}, completed steps={:?}. {}. Retry the same confirmation to resume, or refresh to review current state.", receipt.action_id, receipt.named, receipt.imported, receipt.steps.keys().collect::<Vec<_>>(), receipt.error.as_deref().unwrap_or("unknown failure"));
    }
    model.import_request = None;
    model.create_editor = None;
    model.pending_project_mutation = None;
    model.view = ProjectView::List;
    reload(model, false)?;
    model.notice = Some(format!(
        "Action {} complete: formal name set={}, imported={}, Project steps={:?}",
        receipt.action_id,
        receipt.named,
        receipt.imported,
        receipt.steps.keys().collect::<Vec<_>>()
    ));
    Ok(())
}

fn import_failure_message(error: &anyhow::Error) -> String {
    let raw = format!("{error:#}");
    let structured = raw
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<serde_json::Value>(line.trim()).ok());
    if let Some(error) = structured.as_ref().and_then(|value| value.get("error")) {
        let code = error.get("code").and_then(serde_json::Value::as_str);
        let message = error.get("message").and_then(serde_json::Value::as_str);
        let retryable = error.get("retryable").and_then(serde_json::Value::as_bool);
        if code == Some("stale_durable_candidate")
            || message.is_some_and(|message| message.contains("stale_durable_candidate"))
        {
            return "Candidate changed after this review; this submission did not start the import. Cancel to keep the Project draft, reopen Initial Director, press F5 to refresh, reselect by durable ID, and review again. Server code: stale_durable_candidate (not retryable).".into();
        }
        if code.is_some() || message.is_some() {
            return format!(
                "Management rejected this import: {} [code={}, retryable={}]. No automatic retry was attempted; use F2 for the retained review identifiers.",
                message.unwrap_or("unspecified error"),
                code.unwrap_or("unknown"),
                retryable
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unknown".into())
            );
        }
    }
    let sanitized = raw.replace(['\r', '\n'], " ");
    format!(
        "Import request failed: {}. No automatic retry was attempted.",
        views::clipped(&sanitized, 320)
    )
}

fn project_mutation_summary(request: &HumanManagementProjectMutationRequest) -> String {
    match &request.operation {
        HumanManagementProjectMutationKind::Create {
            director_cutex_session_id,
            presentation,
        } => format!(
            "Create Project {} ({}, badge {} / {}) with Initial Director {}",
            request.project_id,
            presentation.display_name,
            presentation.badge_label,
            presentation.color.token(),
            director_cutex_session_id.as_str()
        ),
        HumanManagementProjectMutationKind::AddMember { cutex_session_id } => {
            format!(
                "Add Agent {} to Project {}",
                cutex_session_id.as_str(),
                request.project_id
            )
        }
        HumanManagementProjectMutationKind::DetachMember { cutex_session_id } => format!(
            "Detach Agent {} from Project {}",
            cutex_session_id.as_str(),
            request.project_id
        ),
        // r219: decode-only; a recorded repair still renders.
        HumanManagementProjectMutationKind::RepairDirectorSeat { .. } => format!(
            "Repair Director seat (removed in r219) for Project {}",
            request.project_id
        ),
        HumanManagementProjectMutationKind::SetWorkspaceRoot { workspace_root } => match workspace_root {
            Some(root) => format!("Set Project {} workspace root to {}", request.project_id, root),
            None => format!("Clear Project {} workspace root", request.project_id),
        },
        HumanManagementProjectMutationKind::Archive => {
            format!("Archive Project {}", request.project_id)
        }
        HumanManagementProjectMutationKind::Restore => {
            format!("Restore Project {}", request.project_id)
        }
        HumanManagementProjectMutationKind::Remove => {
            format!("Remove Project {}", request.project_id)
        }
        HumanManagementProjectMutationKind::CreateProject {
            presentation,
            director_cutex_session_id,
            ..
        } => format!(
            "Create Project {} ({}) with {}",
            request.project_id,
            presentation.display_name,
            director_cutex_session_id
                .as_ref()
                .map_or_else(|| "no Director".to_owned(), |id| format!("Director {}", id.as_str()))
        ),
        HumanManagementProjectMutationKind::AssignDirector { cutex_session_id } => format!(
            "Make {} the Director of Project {}",
            cutex_session_id.as_str(),
            request.project_id
        ),
        HumanManagementProjectMutationKind::ClearDirector => {
            format!("Clear the Director of Project {}", request.project_id)
        }
        HumanManagementProjectMutationKind::RotateDirector {
            successor_cutex_session_id,
        } => format!(
            "Rotate the Director of Project {} to {}",
            request.project_id,
            successor_cutex_session_id.as_str()
        ),
    }
}

fn import_review_details(request: &cutex::agent_management::DurableImportRequest) -> String {
    let candidate = &request.candidate;
    let mut lines = vec![
        "IMPORT REVIEW (read only)".to_string(),
        format!("Action ID: {}", request.action_id),
        format!(
            "Durable Agent ID: {}",
            candidate
                .cutex_session_id
                .as_ref()
                .map(|id| id.as_str())
                .unwrap_or("invalid")
        ),
        format!("Formal Agent name: {}", request.confirmed_formal_name),
        format!("Candidate durable revision: {}", candidate.durable_revision),
        format!(
            "Candidate state: {}; {}; {}",
            if candidate.in_roster {
                "roster"
            } else {
                "durable, not in roster"
            },
            candidate
                .current_project_id
                .as_ref()
                .map(|id| format!("Project {id}"))
                .unwrap_or_else(|| "unassigned".into()),
            if candidate.online {
                "Online"
            } else {
                "Offline"
            }
        ),
    ];
    lines.push(
        request
            .detach
            .as_ref()
            .map(|detach| format!("Planned source step: {}", project_mutation_summary(detach)))
            .unwrap_or_else(|| "Planned source step: none".into()),
    );
    lines.push(
        request
            .assignment
            .as_ref()
            .map(|assignment| {
                format!(
                    "Planned destination step: {}",
                    project_mutation_summary(assignment)
                )
            })
            .unwrap_or_else(|| "Planned destination step: none (import only)".into()),
    );
    lines.join("\n")
}

fn project_status_details(model: &CutexProjectsModel) -> String {
    let mut sections = vec![format!(
        "STATUS\nError: {}\nNotice: {}",
        model.failure.as_deref().unwrap_or("None"),
        model.notice.as_deref().unwrap_or("None")
    )];
    if let Some(request) = &model.import_request {
        sections.push(import_review_details(request));
    }
    if let Some(request) = &model.pending_project_mutation {
        sections.push(format!(
            "PROJECT REVIEW (read only)\nAction: {}",
            request.label
        ));
    }
    if let Some(request) = &model.pending_operator {
        sections.push(format!(
            "OPERATOR REVIEW (read only)\nOperation: {:?}\nOperator: {}\nOperator durable ID: {}",
            request.operation,
            request.name,
            request.cutex_session_id.as_str()
        ));
    }
    sections.join("\n\n")
}

fn execute_operator_action(model: &mut CutexProjectsModel) -> anyhow::Result<()> {
    let target = model
        .pending_operator
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Operator action is unavailable"))?;
    let details = model
        .details
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("project details are unavailable"))?;
    let request = HumanManagementOperatorActionRequest {
        schema: HumanManagementOperatorSchema::V1,
        action_id: AgentActionId::new(format!("management-operator-{}", Uuid::new_v4()))?,
        project_id: details.project_id.clone(),
        expected_authority_epoch: Some(details.authority_epoch),
        expected_grant_revision: Some(details.operator_grant_revision),
        operation: target.operation,
        operator_cutex_session_id: target.cutex_session_id.clone(),
    };
    let receipt = model
        .client
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Management control plane is unavailable"))?
        .operator_action(&request)?;
    model.pending_operator = None;
    reload(model, true)?;
    model.section = ProjectSection::Operators;
    model.notice = Some(format!(
        "Operator {:?} committed at grant revision {} (audit {})",
        receipt.operation, receipt.grant_revision, receipt.audit_event.event_id
    ));
    Ok(())
}

fn execute_project_mutation(model: &mut CutexProjectsModel) -> anyhow::Result<()> {
    let target = model
        .pending_project_mutation
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Project action is unavailable"))?;
    let operation = target.operation.clone();
    let details = model
        .details
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("project details are unavailable"))?;
    let request = HumanManagementProjectMutationRequest {
        schema: HumanManagementProjectMutationSchema::V1,
        action_id: AgentActionId::new(format!("management-project-{}", Uuid::new_v4()))?,
        project_id: details.project_id.clone(),
        expected_authority_epoch: Some(details.authority_epoch),
        expected_project_revision: Some(details.project_revision),
        operation: operation.clone(),
    };
    if matches!(
        operation,
        HumanManagementProjectMutationKind::AddMember { .. }
    ) {
        return begin_import_confirmation(model, request);
    }
    let receipt = model
        .client
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Management control plane is unavailable"))?
        .project_mutation(&request)?;
    let leaves_active_list = matches!(
        operation,
        HumanManagementProjectMutationKind::Archive | HumanManagementProjectMutationKind::Remove
    );
    model.pending_project_mutation = None;
    model.confirm_selected = false;
    reload(model, !leaves_active_list)?;
    if leaves_active_list {
        model.details = None;
        model.view = ProjectView::List;
    }
    model.notice = Some(format!(
        "Project action committed at revision {} (audit {})",
        receipt.project_revision, receipt.audit_event.event_id
    ));
    Ok(())
}

fn run_loop(
    terminal: &mut ProjectTerminal,
    events: &mut super::session_tui::ShellEvents,
    model: &mut CutexProjectsModel,
) -> anyhow::Result<PrimaryPanelOutcome> {
    loop {
        terminal.draw(|frame| render(frame, model))?;
        let Some(event) = events.next()? else {
            continue;
        };
        let key = match event {
            Event::Key(key) => key,
            Event::Paste(text) => {
                handle_paste(model, &text);
                continue;
            }
            _ => continue,
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        if let Some(outcome) = handle_key(model, key) {
            return Ok(outcome);
        }
    }
}

fn project_text_field(model: &mut CutexProjectsModel) -> Option<(&mut String, &mut Option<usize>)> {
    let (value, field) = match model.view {
        ProjectView::Editor => {
            let e = model.editor.as_mut()?;
            (
                match e.field {
                    0 => &mut e.display_name,
                    1 => &mut e.badge_label,
                    2 => &mut e.color,
                    _ => return None,
                },
                e.field,
            )
        }
        ProjectView::Create => {
            let e = model.create_editor.as_mut()?;
            (
                match e.field {
                    0 => &mut e.project_id,
                    1 => &mut e.display_name,
                    2 => &mut e.badge_label,
                    3 => &mut e.color,
                    _ => return None,
                },
                e.field,
            )
        }
        _ => return None,
    };
    Some((value, &mut model.text_cursors[field]))
}
fn handle_paste(model: &mut CutexProjectsModel, text: &str) {
    if model.details_text.is_some() {
        return;
    }
    if model.help.is_some() || model.leave_review.is_some() {
        return;
    }
    if model.view == ProjectView::Actions {
        model.action_filter.extend(text.chars().filter(|c| !c.is_control()));
        model.action_selected = 0;
    } else if model.view == ProjectView::List && model.filter_focused {
        input_policy::paste(&mut model.query, text);
        model.retain_selection();
    } else if model.view == ProjectView::DirectorPicker {
        input_policy::paste(&mut model.director_query, text);
        retain_director_picker_selection(model);
    } else if model.view == ProjectView::ConfirmImport && model.import_name_focused {
        input_policy::paste(&mut model.import_name, text);
    } else if let Some((value, cursor)) = project_text_field(model) {
        input_policy::paste_string(value, cursor, text);
    }
}
fn project_modal(model: &CutexProjectsModel) -> bool {
    matches!(
        model.view,
        ProjectView::ConfirmImport
            | ProjectView::ConfirmOperator
            | ProjectView::ConfirmProjectMutation
            | ProjectView::DirectorPicker
    )
}
fn project_dirty(model: &CutexProjectsModel) -> bool {
    model.editor.is_some() || model.create_editor.is_some()
}
fn project_commands(model: &CutexProjectsModel) -> Vec<(Command, Option<&'static str>)> {
    input_policy::BINDINGS
        .iter()
        .map(|b| {
            let reason = match b.command {
                Command::NewManagedAgent => Some("Available on Agents / Sessions"),
                Command::Profiles
                | Command::Workspaces
                | Command::Archive
                | Command::Appearance => {
                    Some("Open Settings (Alt+7), then F1 management navigation")
                }
                Command::Archived if !(model.view == ProjectView::List || (model.view == ProjectView::Details && model.section == ProjectSection::Members)) => Some("Open the Project list or Members"),
                Command::NewProject if model.view != ProjectView::List => {
                    Some("Return to the Project list")
                }
                Command::Actions | Command::Edit | Command::Inspect
                    if !matches!(model.view, ProjectView::List | ProjectView::Details) =>
                {
                    Some("Finish the current editor/review")
                }
                Command::Sort if model.view != ProjectView::List => Some("Return to the Project list"),
                Command::LoadMore | Command::Titles | Command::Scope => {
                    Some("Available on Recent / Managed")
                }
                Command::Actions | Command::Edit | Command::Inspect
                    if model.visible_indices().is_empty() =>
                {
                    Some("Select a Project")
                }
                _ => None,
            };
            (b.command, reason)
        })
        .collect()
}
fn project_command(
    model: &mut CutexProjectsModel,
    command: Command,
) -> Option<PrimaryPanelOutcome> {
    if command == Command::Details {
        model.status_scroll.reset();
        model.details_text = Some(project_status_details(model));
        return None;
    }
    if model.view == ProjectView::Details
        && model.section == ProjectSection::Members
        && command == Command::Edit
    {
        return request_member_action(
            model,
            if command == Command::Actions {
                super::session_tui::SelectorEvent::OpenActions
            } else {
                super::session_tui::SelectorEvent::OpenSettings
            },
        );
    }
    if let Some((_, Some(reason))) = project_commands(model)
        .into_iter()
        .find(|(c, _)| *c == command)
    {
        model.notice = Some(reason.into());
        return None;
    }
    if matches!(
        command,
        Command::Page(_) | Command::Settings | Command::Exit | Command::Back
    ) {
        match input_policy::navigation_gate(false, project_modal(model), project_dirty(model)) {
            Gate::Block => return None,
            Gate::Review => {
                model.leave_review = Some(LeaveReview::new(
                    command,
                    matches!(model.view, ProjectView::Editor | ProjectView::Create),
                ));
                return None;
            }
            Gate::Allow => {}
        }
    }
    match command {
        Command::Sort => {
            let selected = model.selected_project().map(|p| p.project_id.clone());
            model.sort_order = model.sort_order.next_names();
            model.selected = model.visible_indices().iter().position(|i| Some(&model.projects[*i].project_id) == selected.as_ref()).unwrap_or(0);
            model.notice = Some(format!("Sort: {}", model.sort_order.label()));
            None
        }
        Command::Archived => {
            if model.view == ProjectView::List {
                model.show_archived = !model.show_archived;
                model.retain_selection();
            } else if model.view == ProjectView::Details && model.section == ProjectSection::Members
            {
                model.show_archived_members = !model.show_archived_members;
                reconcile_member_selection(model);
                model.notice = Some(
                    if model.show_archived_members {
                        "Members include archived; membership is retained"
                    } else {
                        "Archived members hidden (Ctrl+H to include)"
                    }
                    .into(),
                );
            }
            None
        }
        Command::Help => {
            model.help = Some(Help::default());
            None
        }
        Command::Page(panel) => {
            model.filter_focused = false;
            (panel != PrimaryPanel::Projects).then_some(PrimaryPanelOutcome::Switch(panel))
        }
        Command::Settings => {
            model.open_settings_requested = true;
            Some(PrimaryPanelOutcome::Switch(PrimaryPanel::Agents))
        }
        Command::Exit => Some(PrimaryPanelOutcome::Exit),
        Command::Back => {
            if model.member_inspecting {
                model.member_inspecting = false;
            } else {
                model.view = ProjectView::List;
            }
            None
        }
        Command::NewProject => {
            if model.view == ProjectView::List {
                model.begin_create();
            }
            None
        }
        Command::Inspect if model.view == ProjectView::List => {
            model.filter_focused = false;
            model.project_inspecting = model.selected_project().is_some();
            model.project_scroll.reset();
            None
        }
        Command::Inspect
            if model.view == ProjectView::Details && model.section == ProjectSection::Members =>
        {
            model.detail_scroll.reset();
            model.member_inspecting = selected_member(model).is_some();
            None
        }
        Command::Actions | Command::Edit | Command::Inspect => {
            if model.view == ProjectView::List {
                if let Err(e) = load_details(model) {
                    model.failure = Some(format!("{e:#}"));
                    return None;
                }
            }
            if model.view == ProjectView::Details {
                match command {
                    Command::Actions => model.begin_project_actions(),
                    Command::Edit => model.begin_editor(),
                    _ => {}
                }
            }
            None
        }
        Command::Refresh => {
            // Keep reviewed identity/version and draft context frozen. A fresh
            // observation is requested after leaving this editor/review.
            if project_modal(model) || project_dirty(model) {
                model.notice = Some("Finish the draft/review before refreshing its target".into());
                return None;
            }
            let result = if model.view == ProjectView::List {
                reload(model, false)
            } else {
                load_details(model)
            };
            if let Err(e) = result {
                model.failure = Some(format!("Refresh failed; previous snapshot is stale: {e:#}"));
            }
            None
        }
        _ => None,
    }
}
fn handle_project_action_navigation(model: &mut CutexProjectsModel, key: KeyEvent) -> Option<PrimaryPanelOutcome> {
match key.code {
            KeyCode::Esc => model.view = ProjectView::Details,
            KeyCode::Home => model.action_selected = 0,
            KeyCode::End => model.action_selected = model.action_menu().len().saturating_sub(1),
            KeyCode::PageDown => model.action_selected = (model.action_selected + 8).min(model.action_menu().len().saturating_sub(1)),
            KeyCode::PageUp => model.action_selected = model.action_selected.saturating_sub(8),
            KeyCode::Up => model.action_selected = model.action_selected.saturating_sub(1),
            KeyCode::Down => {
                model.action_selected = (model.action_selected + 1)
                    .min(model.action_menu().len().saturating_sub(1));
            }
            KeyCode::Enter => {
                match model.action_menu().get(model.action_selected).cloned() {
                    Some(ProjectMenuAction::AgentActions(_)) => { model.view = ProjectView::Details; return request_member_action(model, super::session_tui::SelectorEvent::OpenActions); }
                    Some(ProjectMenuAction::AgentSettings(_)) => { model.view = ProjectView::Details; return request_member_action(model, super::session_tui::SelectorEvent::OpenSettings); }
                    Some(ProjectMenuAction::Operators) => { model.view = ProjectView::Details; model.section = ProjectSection::Operators; model.operator_selected = 0; }
                    Some(ProjectMenuAction::Appearance) => model.begin_editor(),
                    Some(ProjectMenuAction::Mutation(target)) => { model.pending_project_mutation = Some(target); model.confirm_selected = false; model.view = ProjectView::ConfirmProjectMutation; }
                    None => {}
                }
            },
            _ => {}
        };
    None
}

fn handle_key(model: &mut CutexProjectsModel, key: KeyEvent) -> Option<PrimaryPanelOutcome> {
    if model.details_text.is_some() {
        if key.kind != KeyEventKind::Release && key.code == KeyCode::Esc {
            model.details_text = None;
        } else {
            model.status_scroll.handle(key);
        }
        return None;
    }
    if key.kind == KeyEventKind::Press && input_policy::resolve(key) == Some(Command::Details) {
        return project_command(model, Command::Details);
    }
    let text = (model.view == ProjectView::List && model.filter_focused)
        || (model.view == ProjectView::ConfirmImport && model.import_name_focused)
        || model.view == ProjectView::DirectorPicker
        || matches!(model.view, ProjectView::Editor | ProjectView::Create);
    if !super::session_tui_workspace_events::accepts_key(key, text) {
        return None;
    }
    if let Some(mut review) = model.leave_review.take() {
        match review.handle(key) {
            Some(0) => {}
            Some(1) => {
                model.editor = None;
                model.create_editor = None;
                model.view = ProjectView::List;
                return project_command(model, review.command);
            }
            Some(2) => {
                let result = if model.view == ProjectView::Editor {
                    save_editor(model)
                } else {
                    save_project_create(model)
                };
                if let Err(e) = result {
                    model.failure = Some(format!("{e:#}"));
                }
            }
            _ => model.leave_review = Some(review),
        }
        return None;
    }
    if model.view == ProjectView::Actions && model.help.is_none() {
        match key.code {
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => model.action_filter.clear(),
            KeyCode::Char(c) if !c.is_control() && !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => model.action_filter.push(c),
            KeyCode::Backspace => { model.action_filter.pop(); },
            _ => return handle_project_action_navigation(model, key),
        }
        model.action_selected = 0;
        return None;
    }
    if let Some(mut help) = model.help.take() {
        match help.handle(key, &project_commands(model)) {
            Some(Some(command)) => return project_command(model, command),
            Some(None) => {}
            None => model.help = Some(help),
        }
        return None;
    }
    if input_policy::resolve(key) == Some(Command::Help) && !project_modal(model) {
        model.help = Some(Help::default());
        return None;
    }
    if model.view == ProjectView::List && model.project_inspecting {
        if matches!(key.code, KeyCode::Esc | KeyCode::BackTab) {
            model.project_inspecting = false;
            return None;
        }
        if model.project_scroll.handle(key) || matches!(key.code, KeyCode::Enter | KeyCode::Left | KeyCode::Right) {
            return None;
        }
    }
    if model.view == ProjectView::List && model.filter_focused {
        if input_policy::edit(&mut model.query, key) {
            model.retain_selection();
            return None;
        }
        if matches!(
            key.code,
            KeyCode::Esc | KeyCode::Enter | KeyCode::Tab | KeyCode::BackTab
        ) {
            model.filter_focused = false;
            return None;
        }
    }
    if model.view == ProjectView::ConfirmImport && model.import_name_focused {
        if input_policy::edit(&mut model.import_name, key) {
            return None;
        }
        if matches!(
            key.code,
            KeyCode::Enter | KeyCode::Esc | KeyCode::Tab | KeyCode::BackTab
        ) {
            model.import_name_focused = false;
            model.confirm_selected = key.code == KeyCode::BackTab;
            return None;
        }
    }
    if model.view == ProjectView::ConfirmImport
        && model
            .import_request
            .as_ref()
            .is_some_and(|r| r.candidate.formal_name.is_none())
    {
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            if (key.code == KeyCode::Tab && model.confirm_selected)
                || (key.code == KeyCode::BackTab && !model.confirm_selected)
            {
                model.import_name_focused = true;
            } else {
                model.confirm_selected = !model.confirm_selected;
            }
            return None;
        }
    }
    if model.view == ProjectView::DirectorPicker {
        match key.code {
            KeyCode::Esc => {
                model.director_query.reset();
                model.director_picker_selected = None;
                model.view = ProjectView::Create;
            }
            KeyCode::Enter => {
                if let Some(id) = model.director_picker_selected.clone() {
                    if model
                        .available_agents
                        .iter()
                        .any(|agent| agent.cutex_session_id == id)
                    {
                        if let Some(editor) = model.create_editor.as_mut() {
                            editor.director = Some(id);
                        }
                        model.director_query.reset();
                        model.director_picker_selected = None;
                        model.view = ProjectView::Create;
                        model.notice = Some(
                            "Initial Director selected; review the draft, then press Enter again to create."
                                .into(),
                        );
                    }
                } else {
                    model.notice =
                        Some("No matching Director candidate; change the filter.".into());
                }
            }
            KeyCode::Up => shift_director_picker(model, -1),
            KeyCode::Down => shift_director_picker(model, 1),
            KeyCode::PageUp => shift_director_picker(model, -10),
            KeyCode::PageDown => shift_director_picker(model, 10),
            KeyCode::Home => {
                model.director_picker_selected = model
                    .visible_director_indices()
                    .first()
                    .map(|index| model.available_agents[*index].cutex_session_id.clone());
            }
            KeyCode::End => {
                model.director_picker_selected = model
                    .visible_director_indices()
                    .last()
                    .map(|index| model.available_agents[*index].cutex_session_id.clone());
            }
            _ if input_policy::resolve(key) == Some(Command::Refresh) => {
                if let Err(error) = reload_director_candidates(model) {
                    model.failure = Some(format!(
                        "Candidate refresh failed; draft retained: {error:#}"
                    ));
                }
            }
            _ => {
                if input_policy::edit(&mut model.director_query, key) {
                    retain_director_picker_selection(model);
                }
            }
        }
        return None;
    }
    if model.view == ProjectView::Details
        && model.member_inspecting
        && (model.detail_scroll.handle(key) || key.code == KeyCode::Enter)
    {
        return None;
    }
    let palette = model.view == ProjectView::Editor
        && model.editor.as_ref().is_some_and(|e| e.field == 2)
        || model.view == ProjectView::Create
            && model.create_editor.as_ref().is_some_and(|e| e.field == 3);
    if !(palette
        && matches!(
            key.code,
            KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right
        ))
    {
        if let Some((value, cursor)) = project_text_field(model) {
            if input_policy::edit_string(value, cursor, key) {
                return None;
            }
        }
    }
    if let Some(command) = input_policy::resolve(key) {
        return project_command(model, command);
    }
    if key.code == KeyCode::Esc && matches!(model.view, ProjectView::Editor | ProjectView::Create) {
        return project_command(model, Command::Back);
    }
    if model.view == ProjectView::List {
        match key.code {
            KeyCode::Esc => {
                if model.query.value().is_empty() {
                    model.notice = Some("Ctrl+C exits Cutex".into());
                } else {
                    model.query.reset();
                    model.retain_selection();
                }
                return None;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                model.project_inspecting = false;
                model.filter_focused = true;
                return None;
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
                    && !c.is_control() =>
            {
                model.project_inspecting = false;
                model.filter_focused = true;
                if c != '/' {
                    input_policy::edit(&mut model.query, key);
                    model.retain_selection();
                }
                return None;
            }
            KeyCode::Home => {
                model.selected = 0;
                return None;
            }
            KeyCode::End => {
                model.selected = model.visible_indices().len().saturating_sub(1);
                return None;
            }
            KeyCode::PageUp => {
                model.selected = model.selected.saturating_sub(10);
                return None;
            }
            KeyCode::PageDown => {
                model.selected =
                    (model.selected + 10).min(model.visible_indices().len().saturating_sub(1));
                return None;
            }
            _ => {}
        }
    }
    // The palette is a widget, not a text cursor or a page-navigation command.
    if palette && matches!(key.code, KeyCode::Left | KeyCode::Right) {
        if let Some((value, cursor)) = project_text_field(model) {
            let colors = ProjectPaletteColor::ALL;
            let index = colors
                .iter()
                .position(|color| color.token() == *value)
                .map_or(0, |index| {
                    if key.code == KeyCode::Left {
                        (index + colors.len() - 1) % colors.len()
                    } else {
                        (index + 1) % colors.len()
                    }
                });
            *value = colors[index].token();
            *cursor = None;
        }
        return None;
    }
    handle_project_widget_key(model, key)
}
fn handle_project_widget_key(
    model: &mut CutexProjectsModel,
    key: KeyEvent,
) -> Option<PrimaryPanelOutcome> {
    model.notice = None;
    match model.view {
        ProjectView::DirectorPicker => {}
        ProjectView::ConfirmImport => match key.code {
            KeyCode::Esc => {
                model.import_request = None;
                model.view = if model.create_editor.is_some() {
                    ProjectView::Create
                } else {
                    ProjectView::Details
                };
            }
            KeyCode::Left => model.confirm_selected = false,
            KeyCode::Right => model.confirm_selected = true,
            KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::BackTab => {
                model.confirm_selected = !model.confirm_selected
            }
            KeyCode::Enter if model.confirm_selected => match execute_import_confirmation(model) {
                Ok(()) => model.failure = None,
                Err(error) => {
                    model.failure = Some(import_failure_message(&error));
                    model.confirm_selected = false;
                }
            },
            KeyCode::Enter => {
                model.import_request = None;
                model.view = if model.create_editor.is_some() {
                    ProjectView::Create
                } else {
                    ProjectView::Details
                };
            }
            _ => {}
        },
        ProjectView::List => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                return Some(PrimaryPanelOutcome::Switch(PrimaryPanel::Agents));
            }
            KeyCode::Left => {
                return PrimaryPanel::Projects
                    .adjacent(false)
                    .map(PrimaryPanelOutcome::Switch)
            }
            KeyCode::Right => {
                return PrimaryPanel::Projects
                    .adjacent(true)
                    .map(PrimaryPanelOutcome::Switch)
            }
            KeyCode::BackTab => {}
            KeyCode::Up => model.selected = model.selected.saturating_sub(1),
            KeyCode::Down => {
                model.selected =
                    (model.selected + 1).min(model.visible_indices().len().saturating_sub(1));
            }
            KeyCode::Enter | KeyCode::Tab => {
                model.section = ProjectSection::Members;
                if let Err(error) = load_details(model) {
                    model.failure = Some(format!("{error:#}"));
                }
            }
            KeyCode::Char('/') => model.filter_focused = true,
            _ => {}
        },
        ProjectView::Details => match key.code {
            KeyCode::Esc if model.section == ProjectSection::Operators => { model.section = ProjectSection::Members; },
            KeyCode::Esc if model.member_inspecting => model.member_inspecting = false,
            KeyCode::Esc => model.view = ProjectView::List,
            KeyCode::Left => {
                model.member_inspecting = false;
                model.section = model.section.shifted(-1);
            }
            KeyCode::Right | KeyCode::Tab => {
                model.member_inspecting = false;
                model.section = model.section.shifted(1);
            }
            KeyCode::BackTab if model.section == ProjectSection::Overview => {
                model.view = ProjectView::List
            }
            KeyCode::BackTab => {
                model.member_inspecting = false;
                model.section = model.section.shifted(-1);
            }
            KeyCode::Up if model.section == ProjectSection::Operators => {
                model.operator_selected = model.operator_selected.saturating_sub(1)
            }
            KeyCode::Down if model.section == ProjectSection::Operators => {
                model.operator_selected = (model.operator_selected + 1)
                    .min(model.operator_targets().len().saturating_sub(1))
            }
            KeyCode::Up | KeyCode::Down | KeyCode::Home | KeyCode::End
                if model.section == ProjectSection::Members =>
            {
                let len = visible_members(model).len();
                model.member_index = match key.code {
                    KeyCode::Up => model.member_index.saturating_sub(1),
                    KeyCode::Down => (model.member_index + 1).min(len.saturating_sub(1)),
                    KeyCode::Home => 0,
                    _ => len.saturating_sub(1),
                };
                model.member_selected = visible_members(model)
                    .get(model.member_index)
                    .map(|v| v.subject.clone());
            }
            KeyCode::Enter => match model.section {
                ProjectSection::Operators => model.begin_operator_confirmation(),
                ProjectSection::Members => {
                    return request_member_action(
                        model,
                        super::session_tui::SelectorEvent::Activate,
                    );
                }
                ProjectSection::Overview => {
                    model.notice = Some("This section has no mutating action.".to_string())
                }
            },
            _ => {}
        },
        ProjectView::Editor => match key.code {
            KeyCode::Esc => {
                model.editor = None;
                model.view = ProjectView::Details;
                model.failure = None;
            }
            KeyCode::Tab | KeyCode::Down | KeyCode::Right => {
                if let Some(editor) = model.editor.as_mut() {
                    editor.field = (editor.field + 1) % 3;
                }
            }
            KeyCode::BackTab | KeyCode::Up | KeyCode::Left => {
                if let Some(editor) = model.editor.as_mut() {
                    editor.field = (editor.field + 2) % 3;
                }
            }
            KeyCode::Char(' ') if model.editor.as_ref().is_some_and(|e| e.field == 2) => {
                if let Some(editor) = model.editor.as_mut().filter(|editor| editor.field == 2) {
                    let index = ProjectPaletteColor::ALL
                        .iter()
                        .position(|color| color.token() == editor.color);
                    editor.color = ProjectPaletteColor::ALL
                        [index.map_or(0, |index| index + 1) % ProjectPaletteColor::ALL.len()]
                    .token();
                }
            }
            KeyCode::Enter => match save_editor(model) {
                Ok(()) => model.failure = None,
                Err(error) => model.failure = Some(format!("{error:#}")),
            },
            _ => {}
        },
        ProjectView::Create => match key.code {
            KeyCode::Esc => {
                model.create_editor = None;
                model.view = ProjectView::List;
                model.failure = None;
            }
            KeyCode::Tab | KeyCode::Down => {
                if let Some(editor) = model.create_editor.as_mut() {
                    editor.field = (editor.field + 1).min(4);
                }
            }
            KeyCode::BackTab | KeyCode::Up => {
                if let Some(editor) = model.create_editor.as_mut() {
                    editor.field = editor.field.saturating_sub(1);
                }
            }
            KeyCode::Char(' ') if model.create_editor.as_ref().is_some_and(|e| e.field == 3) => {
                if let Some(editor) = model
                    .create_editor
                    .as_mut()
                    .filter(|editor| editor.field == 3)
                {
                    let index = ProjectPaletteColor::ALL
                        .iter()
                        .position(|color| color.token() == editor.color);
                    editor.color = ProjectPaletteColor::ALL
                        [index.map_or(0, |index| index + 1) % ProjectPaletteColor::ALL.len()]
                    .token();
                }
            }
            KeyCode::Enter => {
                if model
                    .create_editor
                    .as_ref()
                    .is_some_and(|editor| editor.field < 4)
                {
                    model.create_editor.as_mut().unwrap().field += 1;
                } else if model.available_agents.is_empty() {
                    model.notice = Some("Draft kept. Create an Agent with Alt+M or Adopt a saved session in Recent, then Alt+3 returns here.".into());
                    return Some(PrimaryPanelOutcome::Switch(PrimaryPanel::Recent));
                } else if model
                    .create_editor
                    .as_ref()
                    .is_some_and(|editor| editor.director.is_none())
                {
                    model.begin_director_picker();
                } else {
                    match save_project_create(model) {
                        Ok(()) => model.failure = None,
                        Err(error) => model.failure = Some(format!("{error:#}")),
                    }
                }
            }
            _ => {}
        },
        ProjectView::Actions => {},
        ProjectView::ConfirmProjectMutation => match key.code {
            KeyCode::Esc => {
                model.pending_project_mutation = None;
                model.view = ProjectView::Actions;
            }
            KeyCode::Left => model.confirm_selected = false,
            KeyCode::Right => model.confirm_selected = true,
            KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::BackTab => {
                model.confirm_selected = !model.confirm_selected
            }
            KeyCode::Enter if model.confirm_selected => match execute_project_mutation(model) {
                Ok(()) => model.failure = None,
                Err(error) => {
                    model.failure = Some(format!("{error:#}"));
                    model.pending_project_mutation = None;
                    model.view = ProjectView::Details;
                }
            },
            KeyCode::Enter => {
                model.pending_project_mutation = None;
                model.view = ProjectView::Actions;
            }
            _ => {}
        },
        ProjectView::ConfirmOperator => match key.code {
            KeyCode::Esc => {
                model.pending_operator = None;
                model.view = ProjectView::Details;
            }
            KeyCode::Left => model.confirm_selected = false,
            KeyCode::Right => model.confirm_selected = true,
            KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::BackTab => {
                model.confirm_selected = !model.confirm_selected
            }
            KeyCode::Enter if model.confirm_selected => match execute_operator_action(model) {
                Ok(()) => model.failure = None,
                Err(error) => {
                    model.failure = Some(format!("{error:#}"));
                    model.pending_operator = None;
                    model.view = ProjectView::Details;
                }
            },
            KeyCode::Enter => {
                model.pending_operator = None;
                model.view = ProjectView::Details;
            }
            _ => {}
        },
    }
    None
}

fn render(frame: &mut Frame<'_>, model: &CutexProjectsModel) {
    if let Some(text) = &model.details_text {
        views::render_details(
            frame,
            frame.area(),
            " CUTEX · Status / review details · read only ",
            text,
            &model.status_scroll,
        );
        return;
    }
    let areas = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(2),
    ])
    .split(frame.area());
    frame.render_widget(
        Paragraph::new(crate::cli_app::session_tui_layout::tabs(
            PrimaryPanel::Projects,
            frame.area().width,
        )),
        areas[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Cutex", Style::new().fg(crate::cli_app::session_tui_layout::focus()).add_modifier(Modifier::BOLD)),
            Span::styled(" Projects", Style::new().fg(Color::White).add_modifier(Modifier::BOLD)),
        ])),
        areas[1],
    );
    match model.view {
        ProjectView::ConfirmImport => render_import_confirmation(frame, areas[2], model),
        ProjectView::List => render_list(frame, areas[2], model),
        ProjectView::Details => render_details(frame, areas[2], model),
        ProjectView::Editor => {
            render_details(frame, areas[2], model);
            render_editor(frame, areas[2], model.editor.as_ref());
        }
        ProjectView::Create => render_create_editor(frame, areas[2], model),
        ProjectView::DirectorPicker => render_director_picker(frame, areas[2], model),
        ProjectView::Actions => {
            render_details(frame, areas[2], model);
            render_project_actions(frame, areas[2], model);
        }
        ProjectView::ConfirmProjectMutation => {
            render_details(frame, areas[2], model);
            render_project_mutation_confirmation(frame, areas[2], model);
        }
        ProjectView::ConfirmOperator => {
            render_details(frame, areas[2], model);
            render_operator_confirmation(frame, areas[2], model);
        }
    }
    let mut footer = {
        match model.view {
            ProjectView::ConfirmImport => footer_hints(&[
                ("Type", "formal name if required"),
                ("←/→/Tab", "Cancel/Confirm"),
                ("Enter", "selected choice"),
                ("Esc", "cancel"),
            ]),
            ProjectView::List if model.filter_focused => footer_hints(&[
                ("Type", "filter"),
                ("Tab/Enter", "finish"),
                ("Esc", "cancel"),
            ]),
            ProjectView::List if model.project_inspecting => footer_hints(&[
                ("↑/↓", "scroll"), ("PgUp/Dn", "page"), ("F2", "details"), ("F1", "commands"), ("Esc", "list"),
            ]),
            ProjectView::List => footer_hints(&[
                ("↑/↓", "select"),
                ("Enter", "open"),
                ("Alt+I", "inspect"),
                ("Alt+N", "create"),
                ("←/→", "panels"),
                ("/", "filter"),
                ("Alt+S", "sort"),
                ("F5", "refresh"),
                ("F2", "details"),
                ("F1", "commands"),
                ("Esc", "agents"),
            ]),
            ProjectView::Details if model.member_inspecting => footer_hints(&[
                ("↑/↓", "scroll"),
                ("PgUp/Dn", "page"),
                ("Esc", "members"),
                ("F1", "commands"),
            ]),
            ProjectView::Details => footer_hints(&[
                ("←/→/Tab", "section"),
                ("Alt+A", "actions"),
                ("Esc", "list"),
                ("F1", "commands"),
            ]),
            ProjectView::Editor => footer_hints(&[
                ("Tab/←/→", "field"),
                ("Space", "color"),
                ("Enter", "save"),
                ("Esc", "cancel"),
            ]),
            ProjectView::Create => footer_hints(&[
                ("Enter/Tab", "next"),
                ("↑/↓", "step"),
                ("Enter", "choose Director/create"),
                ("Space", "palette"),
                ("Esc", "cancel"),
            ]),
            ProjectView::DirectorPicker => footer_hints(&[
                ("Type", "filter"),
                ("↑/↓ PgUp/Dn", "select"),
                ("Enter", "choose only"),
                ("Esc", "cancel"),
            ]),
            ProjectView::Actions => {
                footer_hints(&[("↑/↓ PgUp/Dn", "choose"), ("Enter", "open"), ("Esc", "details")])
            }
            ProjectView::ConfirmProjectMutation => footer_hints(&[
                ("←/→/Tab", "Cancel/Confirm"),
                ("Enter", "choose"),
                ("Esc", "cancel"),
            ]),
            ProjectView::ConfirmOperator => footer_hints(&[
                ("←/→/Tab", "Cancel/Confirm"),
                ("Enter", "choose"),
                ("Esc", "cancel"),
            ]),
        }
    };
    if !matches!(model.view, ProjectView::List) || model.filter_focused {
        footer.extend(footer_hints(&[("F2", "details")]));
    }
    frame.render_widget(
        Paragraph::new(Line::from(footer))
            .wrap(Wrap { trim: true })
            .style(Style::new().fg(if model.failure.is_some() {
                Color::Red
            } else {
                Color::DarkGray
            })),
        areas[4],
    );
    frame.render_widget(
        Paragraph::new(format!(
            "{}",
            model
                .failure
                .as_deref()
                .or(model.notice.as_deref())
                .unwrap_or("Ready"),
        ))
        .style(Style::new().fg(if model.failure.is_some() {
            crate::cli_app::session_tui_layout::error()
        } else {
            crate::cli_app::session_tui_layout::muted()
        })),
        areas[3],
    );
    let field = match model.view {
        ProjectView::Editor => model.editor.as_ref().map(|e| {
            (
                e.field,
                match e.field {
                    0 => e.display_name.as_str(),
                    1 => e.badge_label.as_str(),
                    _ => e.color.as_str(),
                },
            )
        }),
        ProjectView::Create => model.create_editor.as_ref().and_then(|e| {
            Some((
                e.field,
                match e.field {
                    0 => e.project_id.as_str(),
                    1 => e.display_name.as_str(),
                    2 => e.badge_label.as_str(),
                    3 => e.color.as_str(),
                    _ => return None,
                },
            ))
        }),
        _ => None,
    };
    let input_area = Rect {
        x: areas[2].x,
        y: areas[2].bottom().saturating_sub(3),
        width: areas[2].width,
        height: 3.min(areas[2].height),
    };
    if let Some((index, value)) = field {
        input_policy::render_input(
            frame,
            input_area,
            &input_policy::string_input(value, model.text_cursors[index]),
            " Focused field · Tab next · Enter save/next ",
            true,
        );
    } else if model.view == ProjectView::ConfirmImport && model.import_name_focused {
        input_policy::render_input(
            frame,
            input_area,
            &model.import_name,
            " Formal Agent name · Tab to choices ",
            true,
        );
    }
    // Context footer above is deliberately compact. Full actionable command
    // inventory remains in F1; do not overwrite it with every global binding.
    if let Some(help) = &model.help {
        help.render(frame, &project_commands(model));
    }
    if let Some(review) = &model.leave_review {
        review.render(frame);
    }
    if matches!(
        model.view,
        ProjectView::ConfirmOperator | ProjectView::ConfirmProjectMutation
    ) && !model.import_name_focused
        && model.help.is_none()
        && model.leave_review.is_none()
    {
        frame.render_widget(
            Paragraph::new(if model.confirm_selected {
                "Cancel  [Confirm] · Enter selected"
            } else {
                "[Cancel]  Confirm · Enter selected"
            })
            .style(Style::new().add_modifier(Modifier::BOLD)),
            Rect {
                y: areas[2].bottom().saturating_sub(1),
                height: 1,
                ..areas[2]
            },
        );
    }
}

fn render_list(frame: &mut Frame<'_>, area: Rect, model: &CutexProjectsModel) {
    let panes = crate::cli_app::session_tui_layout::list_details(area, true);
    if model.project_inspecting && panes.details.is_none() {
        render_project_summary(frame, area, model);
        return;
    }
    if let Some(details) = panes.details { render_project_summary(frame, details, model); }
    let chunks = [panes.filter, panes.list];
    let filter_title = " Filter projects / id / badge [/] ";
    input_policy::render_input(
        frame,
        chunks[0],
        &model.query,
        filter_title,
        model.filter_focused,
    );
    let visible = model.visible_indices();
    let width = chunks[1].width.saturating_sub(4);
    let mut columns = if width >= 36 {
        vec![("Name", width - 29), ("Director", 20), ("Members", 7)]
    } else {
        vec![("Name", width.max(1))]
    };
    if width >= 100 {
        columns[0].1 = width - 65;
        columns.push(("Project ID", 35));
    }
    let rows = visible.iter().enumerate().map(|(row_index, index)| {
        let project = &model.projects[*index];
        Row::new(
            columns
                .iter()
                .map(|(column, width)| {
                    if *column == "Name" {
                        return Cell::from(project_name_line(project, usize::from(*width)));
                    }
                    let value = match *column {
                        "Director" => project.director_name.clone().unwrap_or_else(|| {
                            project.director_cutex_session_id.as_ref().map_or_else(|| "(none)".to_owned(), |id| id.as_str().to_owned())
                        }),
                        "Members" => project.active_member_count.to_string(),
                        _ => project.project_id.to_string(),
                    };
                    Cell::from(views::clipped(&value, usize::from(*width)))
                })
                .collect::<Vec<_>>(),
        )
        .style(if row_index == model.selected {
            Style::new()
                .bg(crate::cli_app::session_tui_layout::selection())
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        })
    });
    let table = Table::new(
        rows,
        columns
            .iter()
            .map(|(_, width)| Constraint::Length(*width))
            .collect::<Vec<_>>(),
    )
    .header(Row::new(
        columns.iter().map(|(label, _)| *label).collect::<Vec<_>>(),
    ))
    .block(Block::bordered())
    // Each row owns its selection base so the configured badge span remains
    // visible on the selected row instead of being erased by a late highlight.
    .row_highlight_style(Style::new())
    .highlight_symbol("> ");
    let mut state = model.table_state.borrow_mut();
    state.select((!visible.is_empty()).then_some(model.selected));
    frame.render_stateful_widget(table, chunks[1], &mut state);
    if visible.is_empty() && chunks[1].height > 3 {
        frame.render_widget(
            Paragraph::new(if model.projects.is_empty() {
                "No canonical Cutex Projects exist."
            } else {
                "No Projects match this filter."
            })
            .alignment(Alignment::Center)
            .style(Style::new().fg(Color::DarkGray)),
            Rect {
                y: chunks[1].y.saturating_add(2),
                height: chunks[1].height.saturating_sub(3),
                ..chunks[1]
            },
        );
    }
}

fn render_project_summary(frame: &mut Frame<'_>, area: Rect, model: &CutexProjectsModel) {
    let lines = model.selected_project().map(|project| {
        let field = |label: &str, value: String| Line::from(vec![Span::styled(format!("{label}: "), Style::new().fg(Color::Gray)), Span::raw(value)]);
        vec![
            project_name_line(project, usize::from(area.width.saturating_sub(2))),
            field("State", format!("{:?}", project.lifecycle)),
            field("Director", project.director_name.clone().unwrap_or_else(|| project.director_cutex_session_id.as_ref().map_or_else(|| "(none)".to_owned(), |id| id.as_str().to_owned()))),
            field("Members", project.active_member_count.to_string()),
            field("Archived members", project.archived_member_count.to_string()),
            field("Operators", project.operator_count.to_string()),
            Line::default(),
            Line::from("Enter opens the project workspace."),
            Line::from("Alt+A actions · Alt+E appearance"),
            Line::default(),
            Line::styled("Technical identifiers", Style::new().fg(crate::cli_app::session_tui_layout::focus())),
            field("Project ID", project.project_id.to_string()),
            field("Authority epoch", project.authority_epoch.to_string()),
        ]
    }).unwrap_or_else(|| vec![Line::from("No project selected.")]);
    views::render_entity_details(frame, area, "Project Details", lines, &model.project_scroll, model.project_inspecting);
}

fn project_name_line(project: &CutexProjectSummary, width: usize) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }
    let label = views::clipped(&project.presentation.badge_label, 2);
    let badge_width = width.min(4);
    let badge = if label.is_empty() {
        Span::raw(" ".repeat(badge_width))
    } else if badge_width < 4 {
        Span::styled(
            format!(
                "{}{}",
                label,
                " ".repeat(
                    badge_width
                        .saturating_sub(unicode_width::UnicodeWidthStr::width(label.as_str()))
                )
            ),
            project_badge_style(project.presentation.color),
        )
    } else {
        Span::styled(
            format!(
                " {}{} ",
                label,
                " ".repeat(
                    2usize.saturating_sub(unicode_width::UnicodeWidthStr::width(label.as_str()))
                )
            ),
            project_badge_style(project.presentation.color),
        )
    };
    let mut spans = vec![badge];
    if width > 4 {
        spans.push(Span::raw(" "));
    }
    if width > 5 {
        spans.push(Span::raw(views::clipped(
            &project.presentation.display_name,
            width - 5,
        )));
    }
    Line::from(spans)
}

fn render_details(frame: &mut Frame<'_>, area: Rect, model: &CutexProjectsModel) {
    let Some(project) = model.details.as_ref() else {
        frame.render_widget(
            Paragraph::new("Project details are unavailable.")
                .block(Block::bordered().title(" Cutex Project ")),
            area,
        );
        return;
    };
    let chunks = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(area);
    let mut tabs = Vec::new();
    for (index, section) in ProjectSection::ALL.into_iter().enumerate() {
        if index > 0 {
            tabs.push(Span::styled(" | ", Style::new().fg(Color::DarkGray)));
        }
        tabs.push(Span::styled(
            section.label(),
            if section == model.section || (section == ProjectSection::Members && model.section == ProjectSection::Operators) {
                Style::new().fg(crate::cli_app::session_tui_layout::focus()).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::Gray)
            },
        ));
    }
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    format!(" {} ", project.presentation.badge_label),
                    project_badge_style(project.presentation.color),
                ),
                Span::raw(" "),
                Span::styled(
                    project.presentation.display_name.clone(),
                    Style::new().add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(tabs),
        ]),
        chunks[0],
    );
    match model.section {
        ProjectSection::Overview => render_overview(frame, chunks[1], project),
        ProjectSection::Members => {
            let members = visible_members(model);
            if model.member_inspecting {
                if let Some(member) = selected_member(model) {
                    views::render_inspector(frame, chunks[1], &member, &model.detail_scroll);
                }
            } else {
                let mut state = model.member_table.borrow_mut();
                state.select((!members.is_empty()).then_some(model.member_index));
                views::render_table(frame, chunks[1], &members, ListKind::Members, &mut state);
            }
        }
        ProjectSection::Operators => render_operators(frame, chunks[1], model, project),
    }
}

fn render_overview(frame: &mut Frame<'_>, area: Rect, project: &CutexProjectWorkspace) {
    let role = match project.access_role {
        ProjectAccessRole::PrimaryDirector => "Primary Director",
        ProjectAccessRole::AgentOperator => "Agent Operator",
        ProjectAccessRole::HumanManagement => "Human Management",
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(format!("Display name: {}", project.presentation.display_name)),
            Line::from(format!("Badge: {} · Color: {}", project.presentation.badge_label, project.presentation.color.token())),
            Line::from("Alt+E edit name / badge / color · Alt+A project actions"),
            Line::from(""),
            Line::from(format!("Canonical project_id: {}", project.project_id)),
            Line::from(format!("Authority epoch: {}", project.authority_epoch)),
            Line::from(format!(
                "Primary Director: {}",
                project.director.as_ref().map_or("(none)", |director| director.cutex_session_id.as_str())
            )),
            Line::from(format!("Access boundary: {role}")),
            Line::from(format!(
                "Members: {} ordinary, {} archived, {} permanently retired, {} operators (Ctrl+H includes archived)",
                views::project_members(project).len(),
                project.archived_agents.len(),
                project.retired_agents.len(),
                project.agent_operators.len()
            )),
            Line::from(format!(
                "Operator CAS revision: {}",
                project.operator_grant_revision
            )),
        ])
        .wrap(Wrap { trim: true })
        .block(Block::bordered().title(" Overview ")),
        area,
    );
}

fn render_operators(frame: &mut Frame<'_>, area: Rect, model: &CutexProjectsModel, _project: &CutexProjectWorkspace) {
    let targets = model.operator_targets();
    let rows = targets.iter().map(|target| Row::new([
        Cell::from(target.name.clone()),
        Cell::from(match target.operation { HumanManagementOperatorKind::Grant => "Grant", HumanManagementOperatorKind::Revoke => "Revoke" }),
        Cell::from(lifecycle_label(target.lifecycle)),
        Cell::from(target.cutex_session_id.as_str().to_owned()),
    ]));
    let mut state = TableState::default().with_selected((!targets.is_empty()).then_some(model.operator_selected));
    frame.render_stateful_widget(Table::new(rows, [Constraint::Percentage(36), Constraint::Length(8), Constraint::Length(8), Constraint::Min(12)])
        .header(Row::new(["AGENT", "ACTION", "STATUS", "ID"]).style(Style::new().fg(Color::Gray)))
        .block(Block::bordered().title(" Members / Operators · Enter review · Esc Members "))
        .row_highlight_style(Style::new().bg(crate::cli_app::session_tui_layout::selection())).highlight_symbol("> "), area, &mut state);
}

fn render_editor(frame: &mut Frame<'_>, area: Rect, editor: Option<&PresentationEditor>) {
    let popup = centered_rect(62, 11, area);
    frame.render_widget(Clear, popup);
    let Some(editor) = editor else {
        return;
    };
    let field = |index, label: &str, value: String| {
        Line::from(vec![
            Span::styled(
                if editor.field == index { "> " } else { "  " },
                Style::new().fg(crate::cli_app::session_tui_layout::focus()),
            ),
            Span::styled(
                format!("{label}: "),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Span::raw(value),
        ])
    };
    frame.render_widget(
        Paragraph::new(vec![
            field(0, "Display name", editor.display_name.clone()),
            field(1, "Badge (1-2 cells)", editor.badge_label.clone()),
            field(2, "Color", editor.color.clone()),
            Line::from(""),
            Line::from(Span::styled(
                "Type cyan/blue/green/magenta/yellow/red or #RRGGBB · Space cycles palette",
                Style::new().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "Human Management write: authority epoch + presentation revision CAS",
                Style::new().fg(Color::DarkGray),
            )),
        ])
        .block(Block::bordered().title(" Edit appearance ")),
        popup,
    );
}

fn render_import_confirmation(frame: &mut Frame<'_>, area: Rect, model: &CutexProjectsModel) {
    let Some(request) = &model.import_request else {
        return;
    };
    let field = |label: &'static str, value: String| {
        Line::from(vec![
            Span::styled(
                format!("{label:<14}"),
                Style::new().fg(crate::cli_app::session_tui_layout::muted()),
            ),
            Span::styled(value, Style::new().fg(crate::cli_app::session_tui_layout::text())),
        ])
    };
    let assignment = request
        .assignment
        .as_ref()
        .map(project_mutation_summary)
        .unwrap_or_else(|| "Import only; remains unassigned".into());
    let source = request
        .detach
        .as_ref()
        .map(project_mutation_summary)
        .unwrap_or_else(|| "None".into());
    let selected = Style::new()
        .fg(Color::White)
        .bg(crate::cli_app::session_tui_layout::selection())
        .add_modifier(Modifier::BOLD);
    let idle = Style::new().fg(crate::cli_app::session_tui_layout::muted());
    let buttons = Line::from(vec![
        Span::styled(
            " [ Cancel ] ",
            if model.confirm_selected {
                idle
            } else {
                selected
            },
        ),
        Span::raw("   "),
        Span::styled(
            " [ Confirm import + Project step ] ",
            if model.confirm_selected {
                selected
            } else {
                idle
            },
        ),
    ]);
    let block = Block::bordered()
        .border_style(Style::new().fg(crate::cli_app::session_tui_layout::focus()))
        .title(" Confirm durable Agent import / Project assignment ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "Review the exact Agent and Project plan",
                Style::new()
                    .fg(crate::cli_app::session_tui_layout::focus())
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            field(
                "Agent",
                request
                    .candidate
                    .formal_name
                    .clone()
                    .unwrap_or_else(|| "Formal name required".into()),
            ),
            field(
                "Durable ID",
                request
                    .candidate
                    .cutex_session_id
                    .as_ref()
                    .map(|id| id.as_str().to_owned())
                    .unwrap_or_else(|| "invalid — cannot confirm".into()),
            ),
            field(
                "Current state",
                format!(
                    "{} · {} · {}",
                    if request.candidate.in_roster {
                        "roster"
                    } else {
                        "durable, not in roster"
                    },
                    request
                        .candidate
                        .current_project_id
                        .as_ref()
                        .map(|id| format!("Project {id}"))
                        .unwrap_or_else(|| "unassigned".into()),
                    if request.candidate.online {
                        "Online"
                    } else {
                        "Offline"
                    }
                ),
            ),
            field("Formal name", model.import_name.value().to_string()),
            Line::from(""),
            Line::from(Span::styled(
                "Planned steps",
                Style::new().add_modifier(Modifier::BOLD),
            )),
            field(
                "1 · Import",
                if request.candidate.in_roster {
                    "Already present; validate retained roster identity".into()
                } else {
                    "Add this durable Agent to the roster".into()
                },
            ),
            field("2 · Detach", source),
            field("3 · Project", assignment),
            Line::from(""),
            Line::from(Span::styled(
                "Completed steps are retained if a later step fails. The server never retries this action automatically.",
                Style::new().fg(crate::cli_app::session_tui_layout::warning()),
            )),
            field("Action ID", request.action_id.to_string()),
        ])
        .wrap(Wrap { trim: true }),
        chunks[0],
    );
    frame.render_widget(Paragraph::new(buttons), chunks[1]);
}

fn render_create_editor(frame: &mut Frame<'_>, area: Rect, model: &CutexProjectsModel) {
    let Some(editor) = model.create_editor.as_ref() else {
        return;
    };
    let director = editor
        .director
        .as_ref()
        .and_then(|id| {
            model
                .available_agents
                .iter()
                .find(|agent| &agent.cutex_session_id == id)
        })
        .map(|agent| format!("{} ({})", agent.name, agent.cutex_session_id.as_str()))
        .unwrap_or_else(|| {
            if model.available_agents.is_empty() {
                "None — Enter: Recent, then Alt+M creates an Agent"
                    .to_string()
            } else {
                "Choose… Enter opens searchable candidates".to_string()
            }
        });
    let field = |index, label: &str, value: String| {
        Line::from(vec![
            Span::styled(
                if editor.field == index { "> " } else { "  " },
                Style::new().fg(crate::cli_app::session_tui_layout::focus()),
            ),
            Span::styled(
                format!("{label}: "),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Span::raw(value),
        ])
    };
    frame.render_widget(
        Paragraph::new(vec![
            field(0, "Immutable project_id", editor.project_id.clone()),
            field(1, "Display name", editor.display_name.clone()),
            field(2, "Badge (1-2 cells)", editor.badge_label.clone()),
            field(3, "Color", editor.color.clone()),
            field(4, "Initial Director", director),
            Line::from(model.durable_candidates.iter().filter(|row| row.cutex_session_id.is_none()).map(|row| format!("Rejected store key {:?}: {}", row.raw_store_key, row.rejection.as_deref().unwrap_or("invalid identity"))).collect::<Vec<_>>().join("; ")),
            Line::from(""),
            Line::from(Span::styled(
                "The final step commits Project, membership, Director authority, and presentation.",
                Style::new().fg(Color::DarkGray),
            )),
        ])
        .wrap(Wrap { trim: true })
        .block(Block::bordered().border_style(Style::new().fg(crate::cli_app::session_tui_layout::focus())).title(" Create Cutex Project ")),
        area,
    );
}

fn render_director_picker(frame: &mut Frame<'_>, area: Rect, model: &CutexProjectsModel) {
    let chunks = Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).split(area);
    input_policy::render_input(
        frame,
        chunks[0],
        &model.director_query,
        " Search Initial Director · formal name / durable ID / project ID ",
        true,
    );
    let visible = model.visible_director_indices();
    let narrow = chunks[1].width < 90;
    let rows = visible.iter().map(|index| {
        let agent = &model.available_agents[*index];
        let candidate = model
            .durable_candidates
            .iter()
            .find(|row| row.cutex_session_id.as_ref() == Some(&agent.cutex_session_id));
        let selected = model.director_picker_selected.as_ref() == Some(&agent.cutex_session_id);
        let style = if selected {
            Style::new()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        let mut cells = vec![
            Cell::from(
                candidate
                    .and_then(|row| row.formal_name.as_deref())
                    .unwrap_or("<formal name required>"),
            ),
            Cell::from(
                agent
                    .current_project_id
                    .as_ref()
                    .map(|id| id.as_str())
                    .unwrap_or("-"),
            ),
            Cell::from(if candidate.is_some_and(|row| row.online) {
                "Online"
            } else {
                "Offline"
            }),
            Cell::from(if candidate.is_some_and(|row| row.in_roster) {
                "roster"
            } else {
                "durable"
            }),
            Cell::from(agent.cutex_session_id.as_str()),
        ];
        if narrow {
            cells.remove(3);
        }
        Row::new(cells).style(style)
    });
    let title = if model.available_agents.is_empty() {
        " No eligible durable candidates · Recent saved sessions remains available "
    } else if visible.is_empty() {
        " No candidates match this filter "
    } else {
        " Initial Director candidates · Offline remains eligible "
    };
    let widths = if narrow {
        vec![
            Constraint::Percentage(34),
            Constraint::Percentage(22),
            Constraint::Length(8),
            Constraint::Percentage(44),
        ]
    } else {
        vec![
            Constraint::Percentage(24),
            Constraint::Percentage(18),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Percentage(50),
        ]
    };
    let header = if narrow {
        Row::new(["FORMAL NAME", "PROJECT", "STATE", "DURABLE ID"])
    } else {
        Row::new(["FORMAL NAME", "PROJECT", "STATE", "SOURCE", "DURABLE ID"])
    };
    let rows = rows.collect::<Vec<_>>();
    frame.render_widget(
        Table::new(rows, widths)
            .header(header.style(Style::new().fg(crate::cli_app::session_tui_layout::focus()).add_modifier(Modifier::BOLD)))
            .column_spacing(1)
            .block(Block::bordered().title(title)),
        chunks[1],
    );
}

fn render_project_actions(frame: &mut Frame<'_>, area: Rect, model: &CutexProjectsModel) {
    let popup = centered_rect(86, area.height.min(22), area);
    frame.render_widget(Clear, popup);
    let [list_area, description] = Layout::vertical([Constraint::Min(3), Constraint::Length(4)]).areas(popup);
    let actions = model.action_menu();
    let items = actions.iter().map(|a| ListItem::new(a.label())).collect::<Vec<_>>();
    let mut state = ListState::default().with_selected((!actions.is_empty()).then_some(model.action_selected));
    frame.render_stateful_widget(List::new(items).block(Block::bordered().title(format!(" Project Actions · search: {} · Esc back ", model.action_filter)))
        .highlight_style(Style::new().bg(crate::cli_app::session_tui_layout::selection())).highlight_symbol("> "), list_area, &mut state);
    frame.render_widget(Paragraph::new(actions.get(model.action_selected).map(|a| a.label()).unwrap_or_default())
        .wrap(Wrap { trim: false }).block(Block::bordered().title(" Selected action · Enter opens ")), description);
}

fn render_project_mutation_confirmation(
    frame: &mut Frame<'_>,
    area: Rect,
    model: &CutexProjectsModel,
) {
    let popup = centered_rect(76, 10, area);
    frame.render_widget(Clear, popup);
    let description = model
        .pending_project_mutation
        .as_ref()
        .map(|target| target.label.clone())
        .unwrap_or_else(|| "Project action unavailable".to_string());
    let option = |confirmed: bool, label: &'static str| {
        Span::styled(
            format!(" {label} "),
            if model.confirm_selected == confirmed {
                Style::new()
                    .fg(Color::Black)
                    .bg(if confirmed {
                        Color::Yellow
                    } else {
                        crate::cli_app::session_tui_layout::focus()
                    })
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::Gray)
            },
        )
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(description),
            Line::from(""),
            Line::from(vec![option(false, "Cancel"), Span::raw("  "), option(true, "Confirm")]),
            Line::from(""),
            Line::from(Span::styled(
                "Detach/remove fail while matching non-Closed assignments exist. Remove never closes runtime Agents.",
                Style::new().fg(Color::DarkGray),
            )),
        ])
        .wrap(Wrap { trim: true })
        .block(Block::bordered().title(" Confirm Project action ")),
        popup,
    );
}

fn render_operator_confirmation(frame: &mut Frame<'_>, area: Rect, model: &CutexProjectsModel) {
    let popup = centered_rect(70, 10, area);
    frame.render_widget(Clear, popup);
    let description = model
        .pending_operator
        .as_ref()
        .map(|target| {
            format!(
                "{:?} Agent Operator {} ({})?",
                target.operation,
                target.name,
                target.cutex_session_id.as_str()
            )
        })
        .unwrap_or_else(|| "Operator action is unavailable.".to_string());
    let option = |confirmed: bool, label: &'static str| {
        Span::styled(
            format!(" {label} "),
            if model.confirm_selected == confirmed {
                Style::new()
                    .fg(Color::Black)
                    .bg(if confirmed {
                        Color::Yellow
                    } else {
                        crate::cli_app::session_tui_layout::focus()
                    })
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::Gray)
            },
        )
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(description),
            Line::from(""),
            Line::from(vec![
                option(false, "Cancel"),
                Span::raw("  "),
                option(true, "Confirm"),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "A stale authority epoch or grant revision fails without a write.",
                Style::new().fg(Color::DarkGray),
            )),
        ])
        .wrap(Wrap { trim: true })
        .block(Block::bordered().title(" Confirm Operator action ")),
        popup,
    );
}

fn lifecycle_label(lifecycle: ProjectMemberLifecycle) -> &'static str {
    match lifecycle {
        ProjectMemberLifecycle::Online => "online",
        ProjectMemberLifecycle::Offline => "offline",
        ProjectMemberLifecycle::Unavailable => "unavailable",
    }
}

pub(super) fn palette_color(color: ProjectPaletteColor) -> Color {
    match color {
        ProjectPaletteColor::Cyan => Color::Cyan,
        ProjectPaletteColor::Blue => Color::LightBlue,
        ProjectPaletteColor::Green => Color::LightGreen,
        ProjectPaletteColor::Magenta => Color::LightMagenta,
        ProjectPaletteColor::Yellow => Color::Yellow,
        ProjectPaletteColor::Red => Color::LightRed,
        ProjectPaletteColor::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

pub(super) fn project_badge_style(color: ProjectPaletteColor) -> Style {
    // Historical badges used fixed White. This approved addition compares
    // black/white contrast using sRGB relative luminance. ANSI colors retain
    // their existing palette mapping (actual terminal palettes may differ).
    let rgb = match color {
        ProjectPaletteColor::Cyan => (0, 128, 128),
        ProjectPaletteColor::Blue => (0, 0, 255),
        ProjectPaletteColor::Green => (0, 255, 0),
        ProjectPaletteColor::Magenta => (255, 0, 255),
        ProjectPaletteColor::Yellow => (128, 128, 0),
        ProjectPaletteColor::Red => (255, 0, 0),
        ProjectPaletteColor::Rgb(r, g, b) => (r, g, b),
    };
    let linear = |v: u8| {
        let v = f64::from(v) / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    let luminance = 0.2126 * linear(rgb.0) + 0.7152 * linear(rgb.1) + 0.0722 * linear(rgb.2);
    let foreground = if (luminance + 0.05) / 0.05 >= 1.05 / (luminance + 0.05) {
        Color::Black
    } else {
        Color::White
    };
    Style::new()
        .fg(foreground)
        .bg(palette_color(color))
        .add_modifier(Modifier::BOLD)
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn project_action_search_accepts_typing_paste_and_clear() {
        let mut model = model_with_projects();
        model.begin_project_actions();
        for c in "Oper".chars() { handle_key(&mut model, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)); }
        handle_paste(&mut model, "ators");
        assert_eq!(model.action_menu().len(), 1);
        assert!(matches!(model.action_menu()[0], ProjectMenuAction::Operators));
        handle_key(&mut model, KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(model.action_menu().len() > 1);
        assert!(model.action_filter.is_empty());
    }

    #[test]
    fn unified_project_menu_opens_operators_and_returns_to_members() {
        let mut model = model_with_projects();
        model.view = ProjectView::Details;
        model.section = ProjectSection::Members;
        assert_eq!(ProjectSection::ALL, [ProjectSection::Members, ProjectSection::Overview]);
        handle_key(&mut model, KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT));
        assert_eq!(model.view, ProjectView::Actions);
        model.action_selected = model.action_menu().iter()
            .position(|a| matches!(a, ProjectMenuAction::Operators)).unwrap();
        handle_key(&mut model, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(model.section, ProjectSection::Operators);
        handle_key(&mut model, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(model.section, ProjectSection::Members);
        assert_eq!(model.view, ProjectView::Details);
    }

    #[test]
    fn unified_project_menu_keeps_last_action_visible_in_short_terminal() {
        let mut model = model_with_projects();
        model.begin_project_actions();
        handle_key(&mut model, KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(model.action_selected, model.action_menu().len() - 1);
        for (width, height) in [(60, 12), (80, 24), (160, 36)] {
            let screen = rendered(&model, width, height);
            assert!(screen.contains(&model.action_menu().last().unwrap().label()), "{screen}");
            assert!(screen.contains("Selected action"), "{screen}");
        }
    }

    #[test]
    fn ui_contract_e1_project_details_modal_preserves_input_and_small_confirm_choices() {
        let mut model = model_with_projects();
        model.query = Input::new("draft query".into());
        model.filter_focused = true;
        model.failure = Some(format!("{}FINAL-ERROR", "long error ".repeat(200)));
        handle_key(&mut model, KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
        handle_paste(&mut model, "DO NOT EDIT");
        assert_eq!(model.query.value(), "draft query");
        rendered(&model, 60, 18);
        handle_key(&mut model, KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert!(rendered(&model, 60, 18).contains("FINAL-ERROR"));
        handle_key(&mut model, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(model.filter_focused);
        model.view = ProjectView::ConfirmOperator;
        model.confirm_selected = false;
        for (w, h) in [
            (60, 18),
            (80, 24),
            (100, 30),
            (120, 36),
            (160, 48),
            (240, 50),
        ] {
            let screen = rendered(&model, w, h);
            assert!(screen.contains("[Cancel]"));
            assert!(screen.contains("F2 details"));
        }
        let entries = project_commands(&model);
        assert!(entries
            .iter()
            .any(|(command, reason)| *command == Command::Details && reason.is_none()));
    }
    #[test]
    fn ui_contract_d11_no_candidate_saved_recent_roundtrip_keeps_draft() {
        let mut model = model_with_projects();
        model.available_agents.clear();
        assert_eq!(
            handle_key(
                &mut model,
                KeyEvent::new(KeyCode::Char('n'), KeyModifiers::ALT)
            ),
            None
        );
        handle_paste(&mut model, "draft-id");
        for _ in 0..4 {
            handle_key(&mut model, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        }
        assert_eq!(
            handle_key(
                &mut model,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            Some(PrimaryPanelOutcome::Switch(PrimaryPanel::Recent))
        );
        assert_eq!(model.create_editor.as_ref().unwrap().project_id, "draft-id");
        assert_eq!(model.view, ProjectView::Create);
        assert!(model.import_request.is_none());
        assert!(model
            .notice
            .as_ref()
            .unwrap()
            .contains("Alt+M"));
    }
    use super::*;
    use std::time::Duration;

    #[test]
    fn ui_contract_production_durable_import_http_tui_create_add_cancel_auth_and_rename_and_d1_archive_gap(
    ) {
        use cutex::session::{
            model::{CutexSessionRecord, CutexSessionStore},
            store::{load_cutex_session_store, save_cutex_session_store},
        };
        use std::{
            net::TcpListener,
            sync::{
                atomic::{AtomicBool, Ordering},
                Arc,
            },
            thread,
        };
        let _home =
            crate::cli_app::test_home::IsolatedTestHome::new("cutex-import-http-tui").unwrap();
        let _isolated_bus = TcpListener::bind("127.0.0.1:0").unwrap();
        cutex::config::store::save_codez_config(&cutex::profiles::model::CodezConfig {
            agent_bus_port: Some(_isolated_bus.local_addr().unwrap().port()),
            ..Default::default()
        })
        .unwrap();
        let mut sessions = CutexSessionStore::default();
        for (id, name) in [
            ("cutex.director", Some("Formal Director")),
            ("cutex.worker", None),
        ] {
            let mut record = CutexSessionRecord::new(
                id.into(),
                Some(format!("native-{id}")),
                cutex::platform::host::current_host_name(),
                _home.root().to_string_lossy().into_owned(),
                None,
            )
            .unwrap();
            cutex::session::runtime_defaults::apply_managed_session_defaults(
                &mut record,
                name,
                None,
                vec!["test".into()],
                false,
                false,
            );
            record.thread_name = Some("NEVER USE THIS TITLE".into());
            record.display_name_hint = record.thread_name.clone();
            sessions.sessions.insert(id.into(), record);
        }
        let mut malformed = sessions.sessions["cutex.worker"].clone();
        malformed.codex_session_id = Some("bad-native".into());
        sessions.sessions.insert("bad key".into(), malformed);
        save_cutex_session_store(&sessions).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let server = thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let request =
                            cutex::http::server::read_simple_http_request(&mut stream).unwrap();
                        let mut context =
                            crate::cli_app::management_context::management_request_context();
                        // Only fresh native observation is simulated here. Root
                        // authentication, provider persistence and replay are real.
                        context.adopt_saved_native = |principal, request| {
                            if load_cutex_session_store()
                                .unwrap()
                                .human_adoption_receipts
                                .contains_key(request.action_id.as_str())
                            {
                                return (crate::cli_app::management_context::management_request_context().adopt_saved_native)(principal, request);
                            }
                            cutex::agent_management::AgentManagementProvider::open_default().unwrap().adopt_saved_native(
                                principal, &cutex::session::store::cutex_sessions_path().unwrap(), request,
                                &cutex::platform::host::current_host_name(),
                            ).map_err(|e| cutex::agent_management::AgentManagementError::OwnerActionRequired(e.to_string()))
                        };
                        cutex::management::v2::server::handle_v2_request(
                            &mut stream,
                            &request,
                            Some("ordinary"),
                            Some("human-root"),
                            context,
                        )
                        .unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("{error}"),
                }
            }
        });
        struct ServerGuard(Arc<AtomicBool>, Option<std::thread::JoinHandle<()>>);
        impl Drop for ServerGuard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
                self.1.take().unwrap().join().unwrap();
            }
        }
        let _server = ServerGuard(stop, Some(server));
        let client = ManagementControlClient::test_endpoint(base.clone(), "human-root".into());
        for wrong in ["ordinary", "seat", "bus", ""] {
            let denied = ManagementControlClient::test_endpoint(base.clone(), wrong.into());
            assert!(denied.durable_candidates().is_err());
            assert!(denied
                .adopt_saved_native(&cutex::agent_management::HumanAdoptRequest {
            session_only: false,
            creation_defaults: None,
                    action_id: cutex::agent_management::AgentActionId::new("denied-adopt").unwrap(),
                    native_id: "saved-native".into(),
                    cwd: "/private/fixture".into(),
                    formal_name: "Human Name".into(),
                })
                .is_err());
            assert!(denied
                .review_agent_archive(&cutex::agent_management::AgentArchiveReviewRequest {
                    cutex_session_id: cutex::role_revision::CutexSessionId::new("cutex.worker")
                        .unwrap(),
                    operation: cutex::agent_management::AgentArchiveOperation::Archive,
                })
                .is_err());
        }
        let mut model = CutexProjectsModel::empty_with_failure("test");
        model.failure = None;
        model.client = Some(client.clone());
        model.durable_candidates = client.durable_candidates().unwrap();
        model.available_agents = candidate_choices(&model.durable_candidates);
        assert_eq!(model.available_agents.len(), 2);
        assert!(model
            .durable_candidates
            .iter()
            .any(|row| row.raw_store_key == "bad key"
                && row.cutex_session_id.is_none()
                && row.rejection.is_some()));
        let before = cutex::session::store::session_store_json_bytes(&cutex::session::store::cutex_sessions_path().unwrap()).unwrap();
        model.begin_create();
        let initial_director = model.available_agents[0].cutex_session_id.clone();
        let editor = model.create_editor.as_mut().unwrap();
        editor.project_id = "alpha".into();
        editor.display_name = "Alpha".into();
        editor.director = Some(initial_director);
        save_project_create(&mut model).unwrap();
        assert_eq!(model.view, ProjectView::ConfirmImport);
        assert!(!model.confirm_selected);
        handle_key(&mut model, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(
            cutex::session::store::session_store_json_bytes(&cutex::session::store::cutex_sessions_path().unwrap()).unwrap(),
            before
        );
        assert!(
            cutex::agent_management::AgentManagementProvider::open_default()
                .unwrap()
                .store()
                .snapshot()
                .unwrap()
                .agents
                .is_empty()
        );
        // Use the production confirmation dispatcher. Refresh is supplied through
        // the same authenticated client, without starting a daemon.
        save_project_create(&mut model).unwrap();
        let create = model.import_request.clone().unwrap();
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
        );
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            handle_key(
                &mut model,
                KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, kind),
            );
        }
        assert_eq!(model.view, ProjectView::ConfirmImport);
        assert_eq!(
            cutex::session::store::session_store_json_bytes(&cutex::session::store::cutex_sessions_path().unwrap()).unwrap(),
            before
        );
        assert!(
            cutex::agent_management::AgentManagementProvider::open_default()
                .unwrap()
                .store()
                .snapshot()
                .unwrap()
                .agents
                .is_empty()
        );
        // Exercise the actual runtime-registration writer between the HTTP review
        // and HTTP confirmation. Only occurrence/observation fields change, so
        // the Human-reviewed import must remain valid.
        let reviewed_id = create
            .candidate
            .cutex_session_id
            .as_ref()
            .unwrap()
            .as_str()
            .to_string();
        let mut observed = load_cutex_session_store().unwrap();
        let reviewed = observed.sessions[&reviewed_id].clone();
        let reviewed_revision = reviewed.revision;
        let registration = cutex::agent_bus::model::AgentBusAgent {
            id: "runtime-observation-between-review-and-confirm".into(),
            name: "runtime-observation".into(),
            base_name: None,
            thread_name: reviewed.thread_name.clone(),
            path_key: None,
            session_id: reviewed.codex_session_id.clone(),
            cutex_session_id: None,
            profile: "not-import-authority".into(),
            cwd: reviewed.cwd.clone(),
            pid: 4242,
            host_id: Some(reviewed.host_id.clone()),
            groups: reviewed.agent_groups.clone(),
            registration_class: reviewed.registration_class,
            last_seen_epoch_secs: 1,
        };
        cutex::session::runtime_reconciliation::reconcile_cutex_session_store_for_registration(
            &mut observed,
            &registration,
            &reviewed.host_id,
            "2026-09-09T01:02:03Z",
        )
        .unwrap();
        assert_eq!(observed.sessions[&reviewed_id].revision, reviewed_revision);
        assert_ne!(
            observed.sessions[&reviewed_id].updated_at,
            reviewed.updated_at
        );
        save_cutex_session_store(&observed).unwrap();
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(model.failure.is_none(), "{:?}", model.failure);
        assert_eq!(model.view, ProjectView::List);
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            handle_key(
                &mut model,
                KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, kind),
            );
            assert_eq!(model.view, ProjectView::List);
        }
        let receipt = client.import_durable_agent(&create).unwrap();
        assert!(receipt.complete, "{:?}", receipt.error);
        let project = client
            .project(&cutex::agent_management::ProjectId::new("alpha").unwrap())
            .unwrap();
        model.details = Some(project);
        model.view = ProjectView::Details;
        model.section = ProjectSection::Members;
        reconcile_member_selection(&mut model);
        let member_id = model.member_selected.clone();
        assert!(member_id.is_some(), "Director alone must appear in Members");
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(
            model.member_action_requested.take(),
            Some((
                "cutex.director".into(),
                super::super::session_tui::SelectorEvent::Activate
            ))
        );
        project_command(&mut model, Command::Inspect);
        assert!(model.member_inspecting);
        handle_key(&mut model, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(model.member_action_requested.is_none(), "Inspector Enter must not activate the real selected member");
        assert!(rendered(&model, 80, 30).contains("Inspector"));
        let selected_member = model.member_selected.clone();
        let offset = model.member_table.borrow().offset();
        for key in [KeyCode::End, KeyCode::PageUp, KeyCode::Home] {
            handle_key(&mut model, KeyEvent::new(key, KeyModifiers::NONE));
            assert!(model.member_inspecting);
            assert_eq!(model.member_selected, selected_member);
            assert_eq!(model.member_table.borrow().offset(), offset);
        }
        handle_key(&mut model, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!model.member_inspecting);
        assert_eq!(model.view, ProjectView::Details);
        assert_eq!(model.member_selected, member_id);
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT),
        );
        assert_eq!(model.view, ProjectView::Actions);
        assert!(model.member_action_requested.is_none());
        handle_key(&mut model, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(model.view, ProjectView::Details);
        assert_eq!(
            model.member_action_requested.take(),
            Some((
                "cutex.director".into(),
                super::super::session_tui::SelectorEvent::OpenActions
            ))
        );
        model.view = ProjectView::List;
        model.durable_candidates = client.durable_candidates().unwrap();
        let worker = model
            .durable_candidates
            .iter()
            .find(|c| {
                c.cutex_session_id
                    .as_ref()
                    .is_some_and(|id| id.as_str() == "cutex.worker")
            })
            .unwrap()
            .clone();
        let add = HumanManagementProjectMutationRequest {
            schema: HumanManagementProjectMutationSchema::V1,
            action_id: AgentActionId::new("production-add").unwrap(),
            project_id: cutex::agent_management::ProjectId::new("alpha").unwrap(),
            expected_authority_epoch: Some(1),
            expected_project_revision: Some(1),
            operation: HumanManagementProjectMutationKind::AddMember {
                cutex_session_id: worker.cutex_session_id.clone().unwrap(),
            },
        };
        begin_import_confirmation(&mut model, add).unwrap();
        assert_eq!(model.import_name.value(), "");
        handle_paste(&mut model, "Explicit Worker");
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
        );
        assert!(!model.import_name_focused && model.confirm_selected);
        handle_key(&mut model, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(model.import_name_focused);
        handle_key(&mut model, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let mut add = model.import_request.clone().unwrap();
        add.confirmed_formal_name = model.import_name.value().into();
        let wrong = ManagementControlClient::test_endpoint(base, "ordinary".into());
        assert!(wrong.import_durable_agent(&add).is_err());
        assert!(
            !client
                .durable_candidates()
                .unwrap()
                .iter()
                .find(|c| c.cutex_session_id == worker.cutex_session_id)
                .unwrap()
                .in_roster
        );
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
        );
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(model.failure.is_none(), "{:?}", model.failure);
        assert!(client
            .durable_candidates()
            .unwrap()
            .iter()
            .any(|c| c.cutex_session_id == worker.cutex_session_id && c.in_roster));
        let receipt = client.import_durable_agent(&add).unwrap();
        assert!(receipt.complete && receipt.named && receipt.imported);
        let mut sessions = load_cutex_session_store().unwrap();
        cutex::session::service::set_cutex_session_display_name_by_key(
            &mut sessions,
            "cutex.worker",
            "Renamed Worker",
        )
        .unwrap();
        cutex::session::service::set_cutex_session_display_name_by_key(
            &mut sessions,
            "cutex.director",
            "Renamed Director",
        )
        .unwrap();
        cutex::session::service::set_cutex_session_profile_by_key(
            &mut sessions,
            "cutex.worker",
            Some("changed-profile".into()),
        )
        .unwrap();
        save_cutex_session_store(&sessions).unwrap();
        let collection = client.projects().unwrap();
        let imported = receipt.imported_agent.as_ref().unwrap();
        assert_eq!(imported.spec.profile, None);
        crate::cli_app::agent_management::validate_managed_recovery_record(
            &sessions.sessions["cutex.worker"],
            &imported.cutex_session_id,
            &imported.native_session_id,
            &imported.spec,
        )
        .expect(
            "imported nullable profile and later profile/name change preserve lifecycle identity",
        );
        assert_eq!(
            cutex::session::service::cutex_session_display_name(&sessions.sessions["cutex.worker"]),
            "Renamed Worker"
        );
        assert_eq!(
            collection.projects[0].director_name.as_deref(),
            Some("Renamed Director")
        );
        let project = client
            .project(&add.assignment.as_ref().unwrap().project_id)
            .unwrap();
        assert!(project
            .active_agents
            .iter()
            .any(|m| m.agent.spec.name == "Renamed Worker"));
        assert_eq!(
            project.director.unwrap().member.unwrap().agent.spec.name,
            "Renamed Director"
        );
        assert_eq!(client.import_durable_agent(&add).unwrap(), receipt);
        assert!(client
            .durable_candidates()
            .unwrap()
            .iter()
            .any(|c| c.formal_name.as_deref() == Some("Renamed Worker")
                && c.current_project_id.is_some()));

        // D1R2: approved reversible archive retains roster membership while
        // the authoritative durable projection hides ordinary member rows.
        _isolated_bus.set_nonblocking(true).unwrap();
        let bus_stop = Arc::new(AtomicBool::new(false));
        let stopping = bus_stop.clone();
        let bus = thread::spawn(move || {
            use std::io::Write;
            while !stopping.load(Ordering::SeqCst) {
                match _isolated_bus.accept() {
                    Ok((mut stream, _)) => {
                        let _ = cutex::http::server::read_simple_http_request(&mut stream).unwrap();
                        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]").unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("{error}"),
                }
            }
        });
        let _bus = ServerGuard(bus_stop, Some(bus));
        let provider = cutex::agent_management::AgentManagementProvider::open_default().unwrap();
        let roster_before = serde_json::to_value(provider.store().snapshot().unwrap()).unwrap();
        let mut durable_only = load_cutex_session_store().unwrap();
        let mut ordinary = durable_only.sessions["cutex.worker"].clone();
        ordinary.cutex_session_id = "cutex.durable-only".into();
        ordinary.codex_session_id = Some("native-durable-only".into());
        ordinary.formal_agent_name = Some("Durable Only".into());
        durable_only
            .sessions
            .insert(ordinary.cutex_session_id.clone(), ordinary);
        save_cutex_session_store(&durable_only).unwrap();
        // Real process boundary: unsupported online backend is rejected by the
        // authenticated production adapter before either stop or persistence.
        #[cfg(unix)]
        let owned = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        #[cfg(windows)]
        let owned = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 30"])
            .spawn().unwrap();
        // This test process is the runtime's parent. Reap it as soon as it
        // exits: an unreaped Unix child stays a zombie, which `kill(pid, 0)`
        // still reports as running, so the stop would wait out its timeout.
        let owned_pid = owned.id();
        let (reaped_tx, reaped) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut owned = owned;
            let _ = reaped_tx.send(owned.wait());
        });
        let mut uncontained = load_cutex_session_store().unwrap();
        let original = uncontained.sessions["cutex.durable-only"].clone();
        let record = uncontained.sessions.get_mut("cutex.durable-only").unwrap();
        record.runtime_backend = cutex::session::model::CutexSessionRuntimeBackend::HostForeground;
        record.runtime_generation = 1;
        record.runtime_pid = Some(owned_pid);
        save_cutex_session_store(&uncontained).unwrap();
        let unsupported = cutex::agent_management::AgentArchiveRequest {
            reason: None,
            action_id: cutex::agent_management::AgentActionId::new("running-real-process")
                .unwrap(),
            review: client
                .review_agent_archive(&cutex::agent_management::AgentArchiveReviewRequest {
                    cutex_session_id: cutex::role_revision::CutexSessionId::new(
                        "cutex.durable-only",
                    )
                    .unwrap(),
                    operation: cutex::agent_management::AgentArchiveOperation::Archive,
                })
                .unwrap(),
        };
        // Archive stops the recorded running runtime as part of the call.
        let receipt = client.execute_agent_archive(&unsupported).unwrap();
        assert_eq!(
            receipt.stage,
            cutex::agent_management::AgentArchiveStage::Committed
        );
        assert!(
            reaped.recv_timeout(Duration::from_secs(5)).is_ok(),
            "runtime stopped before Archive"
        );
        assert!(load_cutex_session_store().unwrap().sessions["cutex.durable-only"].is_retired());
        let mut uncontained = load_cutex_session_store().unwrap();
        uncontained
            .sessions
            .insert("cutex.durable-only".into(), original);
        save_cutex_session_store(&uncontained).unwrap();
        assert!(
            client
                .review_agent_archive(&cutex::agent_management::AgentArchiveReviewRequest {
                    cutex_session_id: cutex::role_revision::CutexSessionId::new("cutex.director")
                        .unwrap(),
                    operation: cutex::agent_management::AgentArchiveOperation::Archive,
                })
                .is_err(),
            "the Director cannot be archived while it holds project authority"
        );
        for id in ["cutex.durable-only", "cutex.worker"] {
            let before = load_cutex_session_store().unwrap().sessions[id].clone();
            let request = cutex::agent_management::AgentArchiveRequest {
                reason: None,
                action_id: cutex::agent_management::AgentActionId::new(format!("archive-{id}"))
                    .unwrap(),
                review: client
                    .review_agent_archive(&cutex::agent_management::AgentArchiveReviewRequest {
                        cutex_session_id: cutex::role_revision::CutexSessionId::new(id).unwrap(),
                        operation: cutex::agent_management::AgentArchiveOperation::Archive,
                    })
                    .unwrap(),
            };
            let receipt = client.execute_agent_archive(&request).unwrap();
            let cli_receipt =
                crate::cli_app::session_archive::execute_confirmed_archive_with_client(
                    &client, &request,
                )
                .unwrap();
            assert_eq!(cli_receipt.cutex_session_id, id);
            assert_eq!(cli_receipt.lifecycle, "archived");
            assert_eq!(cli_receipt.retry_request.as_ref(), Some(&request));
            assert!(
                wrong.execute_agent_archive(&request).is_err(),
                "ordinary token cannot replay root Archive"
            );
            assert_eq!(
                receipt.stage,
                cutex::agent_management::AgentArchiveStage::Committed
            );
            let archived = load_cutex_session_store().unwrap().sessions[id].clone();
            assert!(archived.is_retired());
            let mut recent = crate::cli_app::session_tui_recent::RecentSessionsWorkspace::default();
            recent.receive(
                crate::cli_app::session_tui_recent::CatalogReply::Page {
                    cursor: None,
                    result: Ok(cutex::catalog::ThreadPage {
                        data: vec![cutex::catalog::CatalogThread {
                            id: archived.codex_session_id.clone().unwrap(),
                            session_id: "not-identity".into(),
                            project_id: Some("native-workspace".into()),
                            parent_thread_id: None,
                            preview: "not-formal-name".into(),
                            model_provider: "fixture".into(),
                            created_at: Some(1),
                            updated_at: Some(1),
                            recency_at: Some(1),
                            cwd: Some(_home.root().into()),
                            name: None,
                            status: serde_json::json!({}),
                            source: serde_json::json!("cli"),
                            additional_fields: Default::default(),
                        }],
                        next_cursor: None,
                        backwards_cursor: None,
                    }),
                },
                &load_cutex_session_store().unwrap(),
            );
            assert!(recent.visible_rows().is_empty());
            assert!(!recent.rows()[0].state.can_adopt());
            assert!(crate::cli_app::session_archive::retired_sessions()
                .unwrap()
                .iter()
                .any(|r| r.cutex_session_id == id));
            let current = serde_json::to_value(provider.store().snapshot().unwrap()).unwrap();
            for key in [
                "agents",
                "projects",
                "current_project_memberships",
                "durable_import_actions",
                "durable_import_audit",
            ] {
                assert_eq!(current[key], roster_before[key], "Archive preserves {key}");
            }
            let project = client
                .project(&cutex::agent_management::ProjectId::new("alpha").unwrap())
                .unwrap();
            if id == "cutex.worker" {
                assert!(
                    !project
                        .active_agents
                        .iter()
                        .any(|m| m.agent.cutex_session_id.as_str() == id),
                    "archived worker is hidden in default Project members"
                );
                assert!(project
                    .archived_agents
                    .iter()
                    .any(|m| m.agent.cutex_session_id.as_str() == id));
                model.details = Some(project.clone());
                model.show_archived_members = false;
                assert!(!visible_members(&model)
                    .iter()
                    .any(|m| m.subject == SubjectRef::Managed(id.into())));
                model.show_archived_members = true;
                assert!(visible_members(&model)
                    .iter()
                    .any(|m| m.subject == SubjectRef::Managed(id.into())));
            } else {
                assert!(!project
                    .active_agents
                    .iter()
                    .any(|m| m.agent.cutex_session_id.as_str() == id));
            }
            if id == "cutex.worker" {
                let archived_project = client
                    .project_mutation(&HumanManagementProjectMutationRequest {
                        schema: HumanManagementProjectMutationSchema::V1,
                        action_id: cutex::agent_management::AgentActionId::new(
                            "archive-project-d1r2",
                        )
                        .unwrap(),
                        project_id: project.project_id.clone(),
                        expected_authority_epoch: Some(project.authority_epoch),
                        expected_project_revision: Some(project.project_revision),
                        operation: HumanManagementProjectMutationKind::Archive,
                    })
                    .unwrap();
                assert!(
                    client
                        .review_agent_archive(&cutex::agent_management::AgentArchiveReviewRequest {
                            cutex_session_id: cutex::role_revision::CutexSessionId::new(id)
                                .unwrap(),
                            operation: cutex::agent_management::AgentArchiveOperation::Restore,
                        })
                        .is_ok(),
                    "restore review does not depend on the Project lifecycle"
                );
                client
                    .project_mutation(&HumanManagementProjectMutationRequest {
                        schema: HumanManagementProjectMutationSchema::V1,
                        action_id: cutex::agent_management::AgentActionId::new(
                            "restore-project-d1r2",
                        )
                        .unwrap(),
                        project_id: project.project_id.clone(),
                        expected_authority_epoch: Some(project.authority_epoch),
                        expected_project_revision: Some(archived_project.project_revision),
                        operation: HumanManagementProjectMutationKind::Restore,
                    })
                    .unwrap();
            }
            let restore = cutex::agent_management::AgentArchiveRequest {
                reason: None,
                action_id: cutex::agent_management::AgentActionId::new(format!("restore-{id}"))
                    .unwrap(),
                review: client
                    .review_agent_archive(&cutex::agent_management::AgentArchiveReviewRequest {
                        cutex_session_id: cutex::role_revision::CutexSessionId::new(id).unwrap(),
                        operation: cutex::agent_management::AgentArchiveOperation::Restore,
                    })
                    .unwrap(),
            };
            assert_eq!(
                client.execute_agent_archive(&restore).unwrap().stage,
                cutex::agent_management::AgentArchiveStage::Committed
            );
            let restored = load_cutex_session_store().unwrap().sessions[id].clone();
            assert!(restored.is_active());
            assert_eq!(restored.cutex_session_id, before.cutex_session_id);
            assert_eq!(restored.codex_session_id, before.codex_session_id);
            assert_eq!(restored.formal_agent_name, before.formal_agent_name);
            assert_eq!(restored.profile, before.profile);
            assert!(!cutex::session::archive::record_has_runtime_claim(
                &restored
            ));
            assert_eq!(
                client.execute_agent_archive(&request).unwrap(),
                receipt,
                "historical replay unchanged after Restore"
            );
        }
        // The real root HTTP route must replay an already committed adoption
        // without launching a native process, and retain its exact snapshot.
        let adoption = cutex::agent_management::HumanAdoptRequest {
            session_only: false,
            creation_defaults: None,
            action_id: cutex::agent_management::AgentActionId::new("http-adopt-replay").unwrap(),
            native_id: "saved-http-native".into(),
            cwd: _home.root().to_string_lossy().into_owned(),
            formal_name: "Explicit HTTP Agent".into(),
        };
        let adopted = client.adopt_saved_native(&adoption).unwrap();
        assert!(
            adopted
                .imported
                .as_ref()
                .is_some_and(|receipt| receipt.complete),
            "{adopted:?}"
        );
        assert_eq!(client.adopt_saved_native(&adoption).unwrap(), adopted);
        let mut changed = adoption;
        changed.formal_name = "Changed request".into();
        assert!(client.adopt_saved_native(&changed).is_err());
    }
    use ratatui::backend::TestBackend;

    fn project(
        id: &str,
        name: &str,
        badge: &str,
        color: ProjectPaletteColor,
    ) -> CutexProjectSummary {
        serde_json::from_value(serde_json::json!({
            "project_id": id,
            "authority_epoch": 3,
            "director_cutex_session_id": "cutex.director",
            "access_role": "human_management",
            "operator_count": 0,
            "presentation": {
                "display_name": name, "badge_label": badge, "color": color.token(),
                "revision": 0, "stored": false
            },
            "active_member_count": 1, "retired_member_count": 0
        }))
        .unwrap()
    }

    #[test]
    fn project_summary_inspection_is_local_and_preserves_list_selection() {
        let mut model = model_with_projects();
        model.selected = 1;
        assert!(model.client.is_none());
        let wide = rendered(&model, 180, 30);
        assert!(wide.contains("Project Details"));
        assert!(wide.contains("Filter projects"));
        assert!(project_command(&mut model, Command::Inspect).is_none());
        assert!(model.project_inspecting);
        assert!(model.details.is_none());
        assert!(model.failure.is_none());
        let narrow = rendered(&model, 80, 30);
        assert!(narrow.contains("Project Details"));
        assert!(!narrow.contains("Filter projects"));
        handle_key(&mut model, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(model.selected, 1);
        handle_key(&mut model, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!model.project_inspecting);
        assert_eq!(model.selected, 1);
    }

    #[test]
    fn pro_review_inspectors_own_enter_and_filter_focus() {
        for key in [KeyCode::Tab, KeyCode::Char('/'), KeyCode::Char('x')] {
            let mut model = model_with_projects();
            model.project_inspecting = true;
            handle_key(&mut model, KeyEvent::new(key, KeyModifiers::NONE));
            assert!(model.filter_focused);
            assert!(!model.project_inspecting);
            assert!(rendered(&model, 80, 24).contains("Filter"));
        }
        let mut model = model_with_projects();
        model.view = ProjectView::Details;
        model.section = ProjectSection::Members;
        model.member_inspecting = true;
        handle_key(&mut model, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(model.member_action_requested.is_none());
        handle_key(&mut model, KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));
        assert!(model.show_archived_members);
        handle_key(&mut model, KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));
        assert!(!model.show_archived_members);
    }

    fn model_with_projects() -> CutexProjectsModel {
        let mut model = CutexProjectsModel::empty_with_failure("fixture");
        model.failure = None;
        model.projects = vec![
            project(
                "cutex-stack-main",
                "Cutex Stack Main",
                "CS",
                ProjectPaletteColor::Blue,
            ),
            project(
                "render-lab",
                "Render Lab",
                "CX",
                ProjectPaletteColor::Magenta,
            ),
        ];
        model
    }

    fn add_director_candidate(
        model: &mut CutexProjectsModel,
        id: &str,
        name: Option<&str>,
        project_id: Option<&str>,
        online: bool,
        in_roster: bool,
    ) {
        let id = cutex::role_revision::CutexSessionId::new(id).unwrap();
        let project_id = project_id.map(|id| cutex::agent_management::ProjectId::new(id).unwrap());
        model
            .durable_candidates
            .push(cutex::agent_management::DurableAgentCandidate {
                raw_store_key: id.as_str().to_string(),
                cutex_session_id: Some(id.clone()),
                formal_name: name.map(str::to_string),
                durable_revision: 1,
                durable_sha256: cutex::role_revision::Sha256::new("1".repeat(64)).unwrap(),
                roster_sha256: cutex::role_revision::Sha256::new("2".repeat(64)).unwrap(),
                agent_sha256: cutex::role_revision::Sha256::new("3".repeat(64)).unwrap(),
                in_roster,
                current_project_id: project_id.clone(),
                online,
                rejection: None,
            });
        model.available_agents.push(ProjectAgentChoice {
            cutex_session_id: id,
            name: name.unwrap_or("Formal name required").to_string(),
            current_project_id: project_id,
        });
    }

    fn import_review_model(with_detach: bool) -> CutexProjectsModel {
        let mut model = model_with_projects();
        add_director_candidate(
            &mut model,
            "cutex.018e0000-0000-7000-8000-0001b000b82e",
            Some("scpolya-2"),
            with_detach.then_some("source-project"),
            true,
            false,
        );
        let candidate = model.durable_candidates[0].clone();
        let id = candidate.cutex_session_id.clone().unwrap();
        let assignment = HumanManagementProjectMutationRequest {
            schema: HumanManagementProjectMutationSchema::V1,
            action_id: AgentActionId::new("management-project-create-test").unwrap(),
            project_id: cutex::agent_management::ProjectId::new("scpolya").unwrap(),
            expected_authority_epoch: Some(0),
            expected_project_revision: Some(0),
            operation: HumanManagementProjectMutationKind::Create {
                director_cutex_session_id: id.clone(),
                presentation: ProjectPresentationInput {
                    display_name: "ScPolyA 中文".into(),
                    badge_label: "SP".into(),
                    color: ProjectPaletteColor::Cyan,
                },
            },
        };
        let detach = with_detach.then(|| HumanManagementProjectMutationRequest {
            schema: HumanManagementProjectMutationSchema::V1,
            action_id: AgentActionId::new("management-detach-test").unwrap(),
            project_id: cutex::agent_management::ProjectId::new("source-project").unwrap(),
            expected_authority_epoch: Some(7),
            expected_project_revision: Some(11),
            operation: HumanManagementProjectMutationKind::DetachMember {
                cutex_session_id: id,
            },
        });
        model.import_name = Input::new("scpolya-2".into());
        model.import_request = Some(cutex::agent_management::DurableImportRequest {
            action_id: AgentActionId::new("management-import-fc80c7ae-75b9-46e8-99fe-7153eaa68bef")
                .unwrap(),
            confirmed_formal_name: "scpolya-2".into(),
            candidate,
            assignment: Some(assignment),
            detach,
        });
        model.view = ProjectView::ConfirmImport;
        model
    }

    #[test]
    fn import_confirmation_is_human_readable_and_keeps_exact_review_details() {
        for with_detach in [false, true] {
            let mut model = import_review_model(with_detach);
            for (width, height) in [(62, 22), (100, 30), (160, 40)] {
                let screen = rendered(&model, width, height);
                assert!(screen.contains("Review the exact Agent and Project plan"));
                assert!(screen.contains("scpolya-2"));
                assert!(screen.contains("[ Cancel ]"));
                assert!(screen.contains("[ Confirm import + Project step ]"));
            }
            let wide = rendered(&model, 160, 40);
            assert!(wide.contains("ScPolyA 中文"));
            assert!(wide.contains("SP / cyan"));
            if !with_detach {
                if let Ok(path) = std::env::var("CUTEX_CONFIRM_FRAME_CAPTURE") {
                    let frame = rendered_buffer(&model, 100, 30);
                    let mut text = String::new();
                    for y in 0..frame.area.height {
                        for x in 0..frame.area.width {
                            text.push_str(frame[(x, y)].symbol());
                        }
                        text.push('\n');
                    }
                    std::fs::write(path, text).unwrap();
                }
            }
            model.confirm_selected = true;
            let buffer = rendered_buffer(&model, 120, 32);
            let (x, y) = badge_cell(&buffer, "Confirm import + Project step");
            assert_eq!(
                buffer[(x, y)].bg,
                crate::cli_app::session_tui_layout::selection()
            );
            model.details_text = Some(project_status_details(&model));
            let details = model.details_text.as_deref().unwrap();
            assert!(details.contains("Durable Agent ID: cutex.018e0000"));
            assert!(details.contains("Action ID: management-import-fc80c7ae"));
            assert!(details.contains("Candidate durable revision: 1"));
            assert!(!details.contains("ProjectPresentationInput"));
            assert!(!details.contains("Create {"));
            if with_detach {
                assert!(details.contains("Detach Agent"));
            } else {
                assert!(details.contains("Planned source step: none"));
            }
        }
    }

    #[test]
    fn stale_candidate_http_error_is_safe_actionable_and_not_auto_retryable() {
        let error = anyhow::anyhow!(
            "HTTP 409 Conflict\r\nsecret-header: hidden\r\n{}",
            serde_json::json!({
                "contractVersion": 2,
                "error": {
                    "code": "conflict",
                    "details": {},
                    "message": "conflict: stale_durable_candidate",
                    "retryable": false,
                    "source": "cutex"
                }
            })
        );
        let message = import_failure_message(&error);
        assert!(message.contains("Candidate changed after this review"));
        assert!(message.contains("F5 to refresh"));
        assert!(message.contains("reselect by durable ID"));
        assert!(message.contains("not retryable"));
        assert!(!message.contains("secret-header"));
        assert!(!message.contains('\r'));
    }

    #[test]
    fn create_director_picker_filters_selects_by_id_and_never_falls_through_to_create() {
        let mut model = model_with_projects();
        add_director_candidate(
            &mut model,
            "cutex.alpha-durable",
            Some("Alpha Director"),
            Some("source-project"),
            false,
            true,
        );
        add_director_candidate(
            &mut model,
            "cutex.beta-durable",
            Some("贝塔 Director"),
            None,
            true,
            false,
        );
        model.begin_create();
        {
            let editor = model.create_editor.as_mut().unwrap();
            editor.project_id = "new-project".into();
            editor.display_name = "New Project".into();
            editor.field = 4;
        }
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(model.view, ProjectView::DirectorPicker);
        assert!(model.create_editor.as_ref().unwrap().director.is_none());
        assert!(model.import_request.is_none());

        handle_paste(&mut model, "贝塔");
        assert_eq!(model.visible_director_indices(), vec![1]);
        assert_eq!(
            model
                .director_picker_selected
                .as_ref()
                .map(|id| id.as_str()),
            Some("cutex.beta-durable")
        );
        let frame = rendered_buffer(&model, 120, 30);
        assert!(format!("{frame:?}").contains("贝塔 Director"));
        if let Ok(path) = std::env::var("CUTEX_DIRECTOR_PICKER_FRAME_CAPTURE") {
            let mut text = String::new();
            for y in 0..frame.area.height {
                for x in 0..frame.area.width {
                    text.push_str(frame[(x, y)].symbol());
                }
                text.push('\n');
            }
            std::fs::write(path, text).unwrap();
        }
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(model.view, ProjectView::Create);
        assert_eq!(
            model
                .create_editor
                .as_ref()
                .and_then(|editor| editor.director.as_ref())
                .map(|id| id.as_str()),
            Some("cutex.beta-durable")
        );
        assert!(model.import_request.is_none(), "picker Enter only chooses");
    }

    #[test]
    fn director_picker_cancel_retains_draft_and_identity_survives_reorder() {
        let mut model = model_with_projects();
        add_director_candidate(&mut model, "cutex.first", Some("First"), None, false, true);
        add_director_candidate(&mut model, "cutex.second", None, None, false, false);
        model.begin_create();
        let editor = model.create_editor.as_mut().unwrap();
        editor.project_id = "draft-id".into();
        editor.display_name = "草稿项目".into();
        editor.director = Some(cutex::role_revision::CutexSessionId::new("cutex.second").unwrap());
        editor.field = 4;
        model.begin_director_picker();
        model.available_agents.reverse();
        assert_eq!(
            model
                .director_picker_selected
                .as_ref()
                .map(|id| id.as_str()),
            Some("cutex.second")
        );
        assert!(rendered(&model, 140, 18).contains("<formal name required>"));
        handle_key(&mut model, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let editor = model.create_editor.as_ref().unwrap();
        assert_eq!(editor.project_id, "draft-id");
        assert_eq!(editor.display_name, "草稿项目");
        assert_eq!(
            editor.director.as_ref().map(|id| id.as_str()),
            Some("cutex.second")
        );
        assert!(model.import_request.is_none());
    }

    #[test]
    fn director_picker_distinguishes_no_candidates_from_no_filter_matches() {
        let mut model = model_with_projects();
        model.begin_create();
        model.begin_director_picker();
        assert!(rendered(&model, 100, 24).contains("No eligible durable candidates"));
        add_director_candidate(
            &mut model,
            "cutex.offline",
            Some("Offline Candidate"),
            None,
            false,
            true,
        );
        model.director_query = Input::new("no-such-id".into());
        retain_director_picker_selection(&mut model);
        let screen = rendered(&model, 100, 24);
        assert!(screen.contains("No candidates match this filter"));
        assert!(!screen.contains("cutex.offline"));
    }

    fn rendered(model: &CutexProjectsModel, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, model)).unwrap();
        format!("{:?}", terminal.backend().buffer())
    }

    fn rendered_buffer(
        model: &CutexProjectsModel,
        width: u16,
        height: u16,
    ) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, model)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn badge_cell(buffer: &ratatui::buffer::Buffer, label: &str) -> (u16, u16) {
        let symbols = label.chars().map(|c| c.to_string()).collect::<Vec<_>>();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width.saturating_sub(symbols.len() as u16 - 1) {
                if symbols
                    .iter()
                    .enumerate()
                    .all(|(offset, symbol)| buffer[(x + offset as u16, y)].symbol() == symbol)
                {
                    return (x, y);
                }
            }
        }
        panic!("badge {label:?} not rendered");
    }

    #[test]
    fn ui_contract_k09_project_spaces_and_paste_parity() {
        for create in [false, true] {
            for field in 0..if create { 3 } else { 2 } {
                let mut typed = model_with_projects();
                typed.view = if create {
                    ProjectView::Create
                } else {
                    ProjectView::Editor
                };
                typed.editor = Some(PresentationEditor {
                    display_name: String::new(),
                    badge_label: String::new(),
                    color: "green".into(),
                    field,
                });
                typed.create_editor = Some(ProjectCreateEditor {
                    project_id: String::new(),
                    display_name: String::new(),
                    badge_label: String::new(),
                    color: "green".into(),
                    director: None,
                    field,
                });
                let mut pasted = model_with_projects();
                pasted.view = typed.view;
                pasted.editor = typed.editor.clone();
                pasted.create_editor = typed.create_editor.clone();
                for c in "My Project 中文".chars() {
                    assert_eq!(
                        handle_key(
                            &mut typed,
                            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
                        ),
                        None
                    );
                }
                handle_paste(&mut pasted, "My Project 中文\r\n\t\u{1b}");
                if create {
                    let a = typed.create_editor.unwrap();
                    let b = pasted.create_editor.unwrap();
                    assert_eq!(
                        (&a.project_id, &a.display_name, &a.badge_label, &a.color),
                        (&b.project_id, &b.display_name, &b.badge_label, &b.color)
                    );
                    assert_eq!(
                        [a.project_id, a.display_name, a.badge_label][field],
                        "My Project 中文"
                    );
                } else {
                    let a = typed.editor.unwrap();
                    let b = pasted.editor.unwrap();
                    assert_eq!(
                        (&a.display_name, &a.badge_label, &a.color),
                        (&b.display_name, &b.badge_label, &b.color)
                    );
                    assert_eq!([a.display_name, a.badge_label][field], "My Project 中文");
                }
            }
        }
    }

    #[test]
    fn ui_contract_project_filter_consumes_shortcuts_and_paste() {
        let mut model = model_with_projects();
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
        );
        handle_paste(&mut model, "My Project\n");
        handle_key(&mut model, KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE),
        );
        assert_eq!(model.query.value(), "y Project");
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT),
        );
        assert_eq!(model.view, ProjectView::List);
        assert!(model.create_editor.is_none());
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        );
        assert_eq!(model.query.value(), "");
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(!model.filter_focused);
        assert_eq!(model.view, ProjectView::List);
    }

    #[test]
    fn ui_contract_k07_k08_project_confirmations_focus_cancel_and_repeat() {
        for view in [
            ProjectView::ConfirmImport,
            ProjectView::ConfirmOperator,
            ProjectView::ConfirmProjectMutation,
        ] {
            let mut model = model_with_projects();
            model.view = view;
            assert!(!model.confirm_selected);
            for (code, selected) in [
                (KeyCode::Right, true),
                (KeyCode::Left, false),
                (KeyCode::Tab, true),
                (KeyCode::Tab, false),
                (KeyCode::BackTab, true),
            ] {
                handle_key(&mut model, KeyEvent::new(code, KeyModifiers::NONE));
                assert_eq!(model.confirm_selected, selected);
                assert_eq!(model.view, view);
                assert!(model.failure.is_none());
            }
            for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
                handle_key(
                    &mut model,
                    KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, kind),
                );
                assert_eq!(model.view, view);
                assert!(model.failure.is_none());
            }
            handle_key(&mut model, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
            handle_key(
                &mut model,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            );
            assert_ne!(model.view, view);
            assert!(model.failure.is_none()); // Cancel did not attempt a service call.
            model.view = view;
            model.confirm_selected = true;
            handle_key(&mut model, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            assert_ne!(model.view, view);
            assert!(model.failure.is_none());
        }
        let mut model = model_with_projects();
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            handle_key(
                &mut model,
                KeyEvent::new_with_kind(KeyCode::Char('a'), KeyModifiers::ALT, kind),
            );
            assert!(model.create_editor.is_none());
        }
    }

    #[test]
    fn ui_contract_b1_project_filter_and_editor_cursor_parity() {
        let mut model = model_with_projects();
        let text = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 /中文🙂";
        for c in text.chars() {
            handle_key(
                &mut model,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            );
        }
        assert_eq!(model.query.value(), text);
        for code in [KeyCode::Enter, KeyCode::Esc, KeyCode::Esc] {
            assert_eq!(
                handle_key(&mut model, KeyEvent::new(code, KeyModifiers::NONE)),
                None
            );
        }
        assert!(model.query.value().is_empty());
        assert!(model.notice.as_deref().unwrap().contains("Ctrl+C"));
        model.view = ProjectView::Editor;
        model.editor = Some(PresentationEditor {
            display_name: "ab".into(),
            badge_label: "CX".into(),
            color: "green".into(),
            field: 0,
        });
        handle_key(&mut model, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        handle_paste(&mut model, "中文e\u{301}🙂\n\t\u{1b}");
        assert_eq!(
            model.editor.as_ref().unwrap().display_name,
            "a中文e\u{301}🙂b"
        );
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        );
        assert_eq!(model.editor.as_ref().unwrap().field, 0);
        handle_key(&mut model, KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE),
        );
        assert!(model
            .editor
            .as_ref()
            .unwrap()
            .display_name
            .starts_with('中'));
        let mut terminal = Terminal::new(TestBackend::new(38, 18)).unwrap();
        terminal.draw(|frame| render(frame, &model)).unwrap();
        assert!(terminal.get_cursor_position().unwrap().x < 37);
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        );
        assert!(model.editor.as_ref().unwrap().display_name.is_empty());
    }

    #[test]
    fn ui_contract_b1_project_f1_new_settings_and_dirty_navigation_gate() {
        let mut model = model_with_projects();
        model.available_agents.push(ProjectAgentChoice {
            cutex_session_id: cutex::role_revision::CutexSessionId::new("cutex.director").unwrap(),
            name: "Formal Director".into(),
            current_project_id: None,
        });
        handle_key(&mut model, KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE));
        let index = project_commands(&model)
            .iter()
            .position(|(c, _)| *c == Command::NewProject)
            .unwrap();
        for _ in 0..index {
            handle_key(&mut model, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(model.view, ProjectView::Create);
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('3'), KeyModifiers::ALT),
        );
        assert!(model.leave_review.is_some());
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(model.create_editor.is_some()); // Cancel is default
        // Settings is Alt+7 since 85156fb added the Profiles panel (Alt+6).
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('7'), KeyModifiers::ALT),
        );
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
        );
        assert_eq!(
            handle_key(
                &mut model,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            Some(PrimaryPanelOutcome::Switch(PrimaryPanel::Agents))
        );
        assert!(model.open_settings_requested);
        assert!(model.create_editor.is_none());
        model.view = ProjectView::ConfirmProjectMutation;
        for key in [
            KeyEvent::new(KeyCode::Char('1'), KeyModifiers::ALT),
            KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE),
        ] {
            assert_eq!(handle_key(&mut model, key), None);
            assert_eq!(model.view, ProjectView::ConfirmProjectMutation);
        }
    }

    #[test]
    fn project_filter_matches_name_id_and_badge() {
        let mut model = model_with_projects();
        for query in ["render lab", "render-lab", "cx"] {
            model.query = Input::new(query.to_string());
            assert_eq!(model.visible_indices(), vec![1]);
        }
        model.query = Input::new("cs".to_string());
        assert_eq!(model.visible_indices(), vec![0]);
    }

    #[test]
    fn bare_action_letters_filter_while_alt_n_opens_create() {
        let mut model = model_with_projects();
        for character in ['a', 'e'] {
            handle_key(
                &mut model,
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
            );
            assert_eq!(model.view, ProjectView::List);
            assert!(model.filter_focused);
        }

        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
        );
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        );
        assert_eq!(model.query.value(), "ae/a");
        model.filter_focused = false;
        model.available_agents.push(ProjectAgentChoice {
            cutex_session_id: cutex::role_revision::CutexSessionId::new("cutex.director-new")
                .unwrap(),
            name: "New Director".to_string(),
            current_project_id: None,
        });
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::ALT),
        );
        assert_eq!(model.view, ProjectView::Create);
        assert!(model.create_editor.is_some());
    }

    #[test]
    fn list_arrows_switch_tabs_palette_arrows_stay_in_widget() {
        let mut model = model_with_projects();
        assert_eq!(
            handle_key(&mut model, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            Some(PrimaryPanelOutcome::Switch(PrimaryPanel::Recent))
        );
        assert_eq!(
            handle_key(
                &mut model,
                KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)
            ),
            Some(PrimaryPanelOutcome::Switch(PrimaryPanel::Alarms))
        );
        assert!(model.pending_operator.is_none());

        model.view = ProjectView::Editor;
        model.editor = Some(PresentationEditor {
            display_name: "Draft Name".to_string(),
            badge_label: "DN".to_string(),
            color: ProjectPaletteColor::Green.token(),
            field: 2,
        });
        handle_key(&mut model, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(model.editor.as_ref().map(|editor| editor.field), Some(2));
        assert_eq!(
            model.editor.as_ref().map(|editor| editor.color.as_str()),
            Some("blue")
        );
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
        );
        assert_eq!(model.editor.as_ref().map(|editor| editor.field), Some(2));
        assert_eq!(
            model.editor.as_ref().map(|editor| editor.color.as_str()),
            Some("green")
        );
    }

    #[test]
    fn alt_shortcut_switch_preserves_filter_and_editor_input() {
        let mut model = model_with_projects();
        model.query = Input::new("render".to_string());
        model.editor = Some(PresentationEditor {
            display_name: "Draft Name".to_string(),
            badge_label: "DN".to_string(),
            color: ProjectPaletteColor::Green.token(),
            field: 1,
        });
        model.view = ProjectView::Editor;

        assert_eq!(
            handle_key(
                &mut model,
                KeyEvent::new(KeyCode::Char('4'), KeyModifiers::ALT)
            ),
            None
        );
        assert!(model.leave_review.is_some());
        assert_eq!(model.query.value(), "render");
        assert_eq!(model.view, ProjectView::Editor);
        assert_eq!(
            model
                .editor
                .as_ref()
                .map(|editor| editor.display_name.as_str()),
            Some("Draft Name")
        );
    }

    #[test]
    fn refresh_preserves_editor_input_when_the_backing_reload_fails() {
        let mut model = model_with_projects();
        model.editor = Some(PresentationEditor {
            display_name: "Uncommitted Draft".to_string(),
            badge_label: "UD".to_string(),
            color: ProjectPaletteColor::Green.token(),
            field: 1,
        });
        model.view = ProjectView::Editor;

        assert_eq!(
            handle_key(&mut model, KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE)),
            None
        );
        assert_eq!(model.view, ProjectView::Editor);
        assert_eq!(
            model
                .editor
                .as_ref()
                .map(|editor| editor.display_name.as_str()),
            Some("Uncommitted Draft")
        );
        assert!(model.failure.is_none());
        assert!(model
            .notice
            .as_deref()
            .unwrap()
            .contains("before refreshing"));
    }

    #[test]
    fn operator_confirmation_defaults_to_cancel_and_left_right_never_commit() {
        let mut model = model_with_projects();
        model.view = ProjectView::ConfirmOperator;
        model.pending_operator = Some(OperatorTarget {
            cutex_session_id: cutex::role_revision::CutexSessionId::new("cutex.worker").unwrap(),
            name: "Worker".to_string(),
            lifecycle: ProjectMemberLifecycle::Online,
            operation: HumanManagementOperatorKind::Grant,
        });
        assert!(!model.confirm_selected);
        for key in [KeyCode::Left, KeyCode::Right] {
            assert_eq!(
                handle_key(&mut model, KeyEvent::new(key, KeyModifiers::NONE)),
                None
            );
            assert_eq!(model.view, ProjectView::ConfirmOperator);
            assert!(model.pending_operator.is_some());
            assert_eq!(model.confirm_selected, key == KeyCode::Right);
        }
        handle_key(&mut model, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(model.view, ProjectView::Details);
        assert!(model.pending_operator.is_none());
    }

    #[test]
    fn badge_style_contrasts_project_color_and_cx_is_two_cells() {
        assert_eq!(unicode_width::UnicodeWidthStr::width("CX"), 2);
        let style = project_badge_style(ProjectPaletteColor::Magenta);
        assert_eq!(style.fg, Some(Color::Black));
        assert_eq!(style.bg, Some(Color::LightMagenta));
        assert!(rendered(&model_with_projects(), 90, 18).contains("Render Lab"));
    }

    #[test]
    fn project_name_sort_preserves_original_order_when_reset() {
        let mut model = model_with_projects();
        model.projects[0].presentation.display_name = "Zebra".into();
        model.projects[1].presentation.display_name = "alpha".into();
        model.sort_order = cutex::profiles::list_preferences::ListSort::NameAsc;
        assert_eq!(model.visible_indices(), vec![1, 0]);
        model.sort_order = Default::default();
        assert_eq!(model.visible_indices(), vec![0, 1]);
    }
    #[test]
    fn projects_rows_show_configured_badges_selected_narrow_wide_and_blank_slot() {
        let mut model = model_with_projects();
        model.projects[0].presentation.badge_label = "QZ".into();
        model.projects[0].presentation.color = ProjectPaletteColor::Green;
        model.selected = 1;
        model.projects.push(project(
            "no-badge",
            "No Badge",
            "",
            ProjectPaletteColor::Red,
        ));

        let wide = rendered_buffer(&model, 90, 18);
        let (qx, qy) = badge_cell(&wide, "QZ");
        assert_eq!(wide[(qx, qy)].bg, Color::LightGreen);
        assert_eq!(wide[(qx, qy)].fg, Color::Black);
        let (cx, cy) = badge_cell(&wide, "CX");
        assert_eq!(wide[(cx, cy)].bg, Color::LightMagenta);
        assert_eq!(wide[(cx, cy)].fg, Color::Black);
        assert_eq!(wide[(cx + 4, cy)].bg, crate::cli_app::session_tui_layout::selection(), "selected row base");

        let text = (0..wide.area.height)
            .map(|y| {
                (0..wide.area.width).fold(String::new(), |mut line, x| {
                    line.push_str(wide[(x, y)].symbol());
                    line
                })
            })
            .collect::<Vec<_>>();
        let starts = ["Cutex Stack Main", "Render Lab", "No Badge"].map(|name| {
            text.iter()
                .find_map(|line| line.find(name))
                .unwrap_or_else(|| panic!("missing project name {name}"))
        });
        assert_eq!(starts[0], starts[1]);
        assert_eq!(
            starts[1], starts[2],
            "unset badge keeps the fixed blank slot"
        );
        assert!(
            wide.content.iter().all(|cell| cell.bg != Color::LightRed),
            "unset badge must not paint its configured color"
        );

        model.selected = 0;
        let narrow = rendered_buffer(&model, 10, 12);
        let (x, y) = badge_cell(&narrow, "QZ");
        assert_eq!(narrow[(x, y)].bg, Color::LightGreen);
        assert_eq!(narrow[(x, y)].fg, Color::Black);
    }

    #[test]
    fn archived_project_member_reuses_configured_badge_projection() {
        use cutex::agent_management::{
            EffectiveProjectPresentation, ManagedAgentRecord, ManagedAgentSpec,
            ProjectDirectorProjection, ProjectId, ProjectLifecycle, ProjectMemberProjection,
        };
        use cutex::role_revision::{CutexSessionId, Rfc3339};
        let member = ProjectMemberProjection {
            agent: ManagedAgentRecord {
                project_id: Some(ProjectId::new("render-lab").unwrap()),
                created_by_director_session: Some(CutexSessionId::new("cutex.director").unwrap()),
                created_by_operator_session: None,
                cutex_session_id: CutexSessionId::new("cutex.archived").unwrap(),
                native_session_id: "native-archived".into(),
                spec: ManagedAgentSpec {
                    name: "Formal Archived Agent".into(),
                    cwd: "/private/archived".into(),
                    profile: Some("orchid".into()),
                    runtime_backend: "app_server".into(),
                    model: "gpt-test".into(),
                    reasoning: "medium".into(),
                    permissions: "default".into(),
                    approval_policy: "never".into(),
                    sandbox_mode: "workspace-write".into(),
                    groups: vec!["workers".into()],
                    expose_to_im: false,
                    pin: false,
                },
                created_at: Rfc3339::new("2026-09-09T00:00:00Z").unwrap(),
                retired_at: None,
            },
            lifecycle: ProjectMemberLifecycle::Offline,
            runtime: None,
            observation_error: None,
        };
        let project = CutexProjectWorkspace {
            project_id: ProjectId::new("render-lab").unwrap(),
            authority_epoch: 1,
            lifecycle: ProjectLifecycle::Active,
            project_revision: 1,
            director: Some(ProjectDirectorProjection {
                cutex_session_id: CutexSessionId::new("cutex.director").unwrap(),
                member: None,
            }),
            access_role: ProjectAccessRole::HumanManagement,
            operator_grant_revision: 0,
            agent_operators: Vec::new(),
            presentation: EffectiveProjectPresentation {
                display_name: "Render Lab".into(),
                badge_label: "CX".into(),
                color: ProjectPaletteColor::Magenta,
                revision: 2,
                stored: true,
            },
            active_agents: Vec::new(),
            archived_agents: vec![member],
            retired_agents: Vec::new(),
            workspace_root: None,
        };
        let mut model = model_with_projects();
        model.details = Some(project);
        model.show_archived_members = true;
        let rows = visible_members(&model);
        let archived = rows
            .iter()
            .find(|row| row.name == "Formal Archived Agent")
            .expect("archived member row");
        assert_eq!(archived.project_id.as_deref(), Some("render-lab"));
        assert_eq!(
            archived.badge.as_ref().map(|badge| badge.label.as_str()),
            Some("CX")
        );

        let mut terminal = Terminal::new(TestBackend::new(70, 10)).unwrap();
        let mut state = TableState::default().with_selected(Some(0));
        terminal
            .draw(|frame| {
                views::render_table(frame, frame.area(), &rows, ListKind::Members, &mut state)
            })
            .unwrap();
        let (x, y) = badge_cell(terminal.backend().buffer(), "CX");
        assert_eq!(terminal.backend().buffer()[(x, y)].bg, Color::LightMagenta);
    }

    #[test]
    fn custom_rgb_badge_uses_ratatui_rgb_background() {
        let style = project_badge_style(ProjectPaletteColor::Rgb(0x12, 0x34, 0x56));
        assert_eq!(style.fg, Some(Color::White));
        assert_eq!(style.bg, Some(Color::Rgb(0x12, 0x34, 0x56)));
    }

    #[test]
    fn visual_restoration_badge_contrast_palette_and_rgb() {
        for (color, foreground) in [
            (ProjectPaletteColor::Rgb(255, 255, 255), Color::Black),
            (ProjectPaletteColor::Rgb(0, 0, 0), Color::White),
            (ProjectPaletteColor::Rgb(240, 220, 90), Color::Black),
            (ProjectPaletteColor::Rgb(18, 52, 86), Color::White),
            (ProjectPaletteColor::Cyan, Color::White),
            (ProjectPaletteColor::Blue, Color::White),
            (ProjectPaletteColor::Green, Color::Black),
            (ProjectPaletteColor::Magenta, Color::Black),
            (ProjectPaletteColor::Yellow, Color::Black),
            (ProjectPaletteColor::Red, Color::Black),
        ] {
            let style = project_badge_style(color);
            assert_eq!(style.fg, Some(foreground));
            assert_eq!(style.bg, Some(palette_color(color)));
        }
    }

    #[test]
    fn visual_restoration_projects_footer_primary_and_f1_inventory() {
        let model = model_with_projects();
        let output = rendered(&model, 80, 24);
        assert!(output.contains("F1"));
        let bottom = output.lines().rev().take(2).collect::<Vec<_>>().join("\n");
        assert!(!bottom.contains("Alt+M") && !bottom.contains("Alt+T"));
        assert!(project_commands(&model)
            .iter()
            .any(|(c, reason)| *c == Command::Settings && reason.is_none()));
    }

    #[test]
    fn editor_color_field_accepts_text_edits_and_keeps_palette_cycle_shortcut() {
        let mut model = model_with_projects();
        model.view = ProjectView::Editor;
        model.editor = Some(PresentationEditor {
            display_name: "Draft Name".to_string(),
            badge_label: "CX".to_string(),
            color: "#12345".to_string(),
            field: 2,
        });

        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('6'), KeyModifiers::NONE),
        );
        assert_eq!(
            model.editor.as_ref().map(|editor| editor.color.as_str()),
            Some("#123456")
        );
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        );
        assert_eq!(
            model.editor.as_ref().map(|editor| editor.color.as_str()),
            Some("#12345")
        );

        model.editor.as_mut().unwrap().color.clear();
        handle_paste(&mut model, "#12aB3c");
        assert_eq!(
            model.editor.as_ref().map(|editor| editor.color.as_str()),
            Some("#12aB3c")
        );

        model.editor.as_mut().unwrap().color = "green".to_string();
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        );
        assert_eq!(
            model.editor.as_ref().map(|editor| editor.color.as_str()),
            Some("magenta")
        );

        model.editor.as_mut().unwrap().color = "#123456".to_string();
        handle_key(
            &mut model,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        );
        assert_eq!(
            model.editor.as_ref().map(|editor| editor.color.as_str()),
            Some("cyan")
        );
    }

    #[test]
    fn editor_text_fields_take_printable_keys_before_local_action_shortcuts() {
        let mut model = model_with_projects();
        model.view = ProjectView::Editor;
        model.editor = Some(PresentationEditor {
            display_name: String::new(),
            badge_label: String::new(),
            color: String::new(),
            field: 1,
        });

        for character in ['C', 'X'] {
            assert_eq!(
                handle_key(
                    &mut model,
                    KeyEvent::new(KeyCode::Char(character), KeyModifiers::SHIFT),
                ),
                None
            );
        }
        assert_eq!(
            model
                .editor
                .as_ref()
                .map(|editor| editor.badge_label.as_str()),
            Some("CX")
        );

        model.editor.as_mut().unwrap().field = 2;
        assert_eq!(
            handle_key(
                &mut model,
                KeyEvent::new(KeyCode::Char('#'), KeyModifiers::SHIFT),
            ),
            None
        );
        for character in ['1', '2', 'a', 'B', '3', 'c'] {
            handle_key(
                &mut model,
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
            );
        }
        assert_eq!(
            model.editor.as_ref().map(|editor| editor.color.as_str()),
            Some("#12aB3c")
        );

        let before = model.editor.clone().unwrap();
        assert_eq!(
            handle_key(
                &mut model,
                KeyEvent::new(KeyCode::Char('1'), KeyModifiers::ALT),
            ),
            None
        );
        assert!(model.leave_review.is_some());
        let after = model.editor.as_ref().unwrap();
        assert_eq!(after.display_name, before.display_name);
        assert_eq!(after.badge_label, before.badge_label);
        assert_eq!(after.color, before.color);
        assert_eq!(after.field, before.field);
    }

    #[test]
    fn narrow_terminal_and_resize_render_without_panicking() {
        let model = model_with_projects();
        let mut terminal = Terminal::new(TestBackend::new(38, 9)).unwrap();
        terminal.draw(|frame| render(frame, &model)).unwrap();
        terminal.backend_mut().resize(120, 24);
        terminal.draw(|frame| render(frame, &model)).unwrap();
        assert!(format!("{:?}", terminal.backend().buffer()).contains("Director"));
    }

    #[test]
    fn empty_error_state_never_falls_back_to_codex_workspaces() {
        let model = CutexProjectsModel::empty_with_failure("not authorized");
        let text = rendered(&model, 80, 16);
        assert!(text.contains("Cutex Projects"));
        assert!(text.contains("No canonical Cutex Projects"));
        assert!(!text.contains("Codex Workspaces"));
    }
}
