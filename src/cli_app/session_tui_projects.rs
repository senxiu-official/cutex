//! Native Codex workspace catalog.
//!
//! The app-server connection is intentionally owned by one worker for the
//! lifetime of this workspace. The terminal thread only sends commands and
//! polls replies, keeping drawing and keyboard handling responsive.

use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

use anyhow::Context;
use crossterm::cursor::Show;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;
use super::session_tui_terminal::Terminal;
use uuid::Uuid;

use cutex::catalog::{
    CatalogClient, CatalogError, Project, ProjectCreateParams, ProjectImportParams,
    ProjectListParams, ProjectMoveParams, ProjectRoot, ProjectUpdateParams,
};

const POLL_INTERVAL: Duration = Duration::from_millis(80);
const PROJECT_PAGE_SIZE: u32 = 50;
const ERROR_LIMIT: usize = 2_048;

type ProjectTerminal = Terminal<CrosstermBackend<std::io::Stdout>>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProjectRequest {
    List {
        cursor: Option<String>,
    },
    Read {
        id: String,
    },
    Create {
        name: String,
        roots: Vec<ProjectRoot>,
        idempotency_key: String,
    },
    Import {
        name: String,
        roots: Vec<ProjectRoot>,
        threads: Option<Vec<String>>,
        idempotency_key: String,
    },
    Rename {
        id: String,
        name: String,
    },
    Update {
        id: String,
        name: String,
        roots: Vec<ProjectRoot>,
    },
    Move {
        id: String,
        before_id: Option<String>,
    },
    Delete {
        id: String,
    },
}

#[derive(Debug)]
enum WorkerEvent {
    Connected,
    Page(Result<(Vec<Project>, Option<String>), ProjectFailure>),
    Project(Result<Project, ProjectFailure>),
    Mutated(Result<(), ProjectFailure>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FailureKind {
    ProviderIncompatible,
    Transport,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectFailure {
    kind: FailureKind,
    message: String,
}

impl From<CatalogError> for ProjectFailure {
    fn from(error: CatalogError) -> Self {
        let kind = match error {
            CatalogError::ProviderIncompatible(_) => FailureKind::ProviderIncompatible,
            CatalogError::Launch(_) | CatalogError::Transport(_) | CatalogError::Timeout { .. } => {
                FailureKind::Transport
            }
            _ => FailureKind::Other,
        };
        Self {
            kind,
            message: bounded(&error.to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormKind {
    Create,
    Import,
    Rename,
    Update,
}

#[derive(Debug, Clone)]
struct Form {
    kind: FormKind,
    name: String,
    roots: String,
    threads: String,
    field: usize,
}

#[derive(Debug, Clone)]
enum ProjectView {
    List,
    Details(Project),
    Form(Form),
    Confirm {
        request: ProjectRequest,
        prompt: String,
        confirmed: bool,
    },
}

#[derive(Debug)]
struct ProjectModel {
    projects: Vec<Project>,
    selected: usize,
    cursor: Option<String>,
    next_cursor: Option<String>,
    previous_cursors: Vec<Option<String>>,
    view: ProjectView,
    loading: bool,
    connected: bool,
    failure: Option<ProjectFailure>,
    notice: Option<String>,
    preferred_project_id: Option<String>,
    retry: Option<ProjectRequest>,
}

impl Default for ProjectModel {
    fn default() -> Self {
        Self {
            projects: Vec::new(),
            selected: 0,
            cursor: None,
            next_cursor: None,
            previous_cursors: Vec::new(),
            view: ProjectView::List,
            loading: true,
            connected: false,
            failure: None,
            notice: None,
            preferred_project_id: None,
            retry: None,
        }
    }
}

impl ProjectModel {
    fn selected_project(&self) -> Option<&Project> {
        self.projects.get(self.selected)
    }

    fn receive(&mut self, event: WorkerEvent) -> Option<ProjectRequest> {
        self.loading = false;
        match event {
            WorkerEvent::Connected => {
                self.connected = true;
                self.failure = None;
            }
            WorkerEvent::Page(result) => match result {
                Ok((projects, next_cursor)) => {
                    self.projects = projects;
                    self.next_cursor = next_cursor;
                    self.selected = self
                        .preferred_project_id
                        .take()
                        .and_then(|id| self.projects.iter().position(|project| project.id == id))
                        .unwrap_or(0)
                        .min(self.projects.len().saturating_sub(1));
                    self.failure = None;
                }
                Err(failure) => self.set_failure(failure),
            },
            WorkerEvent::Project(result) => match result {
                Ok(project) if matches!(self.retry, Some(ProjectRequest::Read { .. })) => {
                    self.view = ProjectView::Details(project);
                    self.failure = None;
                }
                Ok(project) => {
                    self.preferred_project_id = Some(project.id);
                    self.view = ProjectView::List;
                    self.failure = None;
                    self.notice = Some("Codex workspace catalog updated".to_string());
                    return Some(self.list_request(self.cursor.clone()));
                }
                Err(failure) => self.set_failure(failure),
            },
            WorkerEvent::Mutated(result) => match result {
                Ok(()) => {
                    self.view = ProjectView::List;
                    self.failure = None;
                    self.notice = Some("Codex workspace catalog updated".to_string());
                    return Some(self.list_request(self.cursor.clone()));
                }
                Err(failure) => self.set_failure(failure),
            },
        }
        None
    }

    fn begin_form(&mut self, kind: FormKind) {
        let selected_name = self.selected_project().map(|project| project.name.clone());
        let selected_roots = self.selected_project().map(project_roots_text);
        self.failure = None;
        self.view = ProjectView::Form(Form {
            kind,
            name: if matches!(kind, FormKind::Rename | FormKind::Update) {
                selected_name.unwrap_or_default()
            } else {
                String::new()
            },
            roots: if kind == FormKind::Rename {
                String::new()
            } else {
                selected_roots.unwrap_or_default()
            },
            threads: String::new(),
            field: 0,
        });
    }

    fn form_request(&self) -> Result<ProjectRequest, String> {
        let ProjectView::Form(form) = &self.view else {
            return Err("No workspace form is active".to_string());
        };
        let name = valid_name(&form.name)?;
        let roots = if form.kind == FormKind::Rename {
            Vec::new()
        } else {
            valid_roots(&form.roots)?
        };
        Ok(match form.kind {
            FormKind::Create => ProjectRequest::Create {
                name,
                roots,
                idempotency_key: Uuid::new_v4().to_string(),
            },
            FormKind::Import => ProjectRequest::Import {
                name,
                roots,
                threads: valid_threads(&form.threads)?,
                idempotency_key: Uuid::new_v4().to_string(),
            },
            FormKind::Rename => ProjectRequest::Rename {
                id: self
                    .selected_project()
                    .ok_or_else(|| "Select a workspace first".to_string())?
                    .id
                    .clone(),
                name,
            },
            FormKind::Update => ProjectRequest::Update {
                id: self
                    .selected_project()
                    .ok_or_else(|| "Select a workspace first".to_string())?
                    .id
                    .clone(),
                name,
                roots,
            },
        })
    }

    fn start_request(&mut self, request: ProjectRequest) {
        self.loading = true;
        self.failure = None;
        self.retry = Some(request);
    }

    fn list_request(&mut self, cursor: Option<String>) -> ProjectRequest {
        self.cursor = cursor.clone();
        self.start_request(ProjectRequest::List { cursor });
        self.retry.clone().expect("request was stored")
    }

    fn set_failure(&mut self, failure: ProjectFailure) {
        if failure.kind == FailureKind::Transport {
            self.connected = false;
        }
        self.failure = Some(failure);
    }

    fn reconnect_required(&self) -> bool {
        !self.connected
    }
}

pub(super) fn run() -> anyhow::Result<()> {
    let mut terminal = open_terminal()?;
    let restore = ProjectTerminalRestore;
    let mut runtime = ProjectRuntime::spawn()?;
    let mut model = ProjectModel::default();
    let request = model.list_request(None);
    runtime.send(request)?;

    let result = run_loop(&mut terminal, &mut runtime, &mut model);
    drop(terminal);
    drop(restore);
    result
}

fn run_loop(
    terminal: &mut ProjectTerminal,
    runtime: &mut ProjectRuntime,
    model: &mut ProjectModel,
) -> anyhow::Result<()> {
    loop {
        while let Some(event) = runtime.poll() {
            if let Some(request) = model.receive(event) {
                runtime.send(request)?;
            }
        }
        terminal.draw(|frame| render(frame, model))?;
        if !event::poll(POLL_INTERVAL)? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C'))
        {
            return Ok(());
        }
        if handle_key(key, model, runtime)? {
            return Ok(());
        }
    }
}

fn handle_key(
    key: KeyEvent,
    model: &mut ProjectModel,
    runtime: &mut ProjectRuntime,
) -> anyhow::Result<bool> {
    if model.loading {
        return Ok(false);
    }
    match &mut model.view {
        ProjectView::List => match key.code {
            KeyCode::Esc | KeyCode::Left => return Ok(true),
            KeyCode::Up => model.selected = model.selected.saturating_sub(1),
            KeyCode::Down => model.selected = (model.selected + 1).min(model.projects.len().saturating_sub(1)),
            KeyCode::Home => model.selected = 0,
            KeyCode::End => model.selected = model.projects.len().saturating_sub(1),
            KeyCode::Enter => if let Some(project) = model.selected_project() {
                let request = ProjectRequest::Read { id: project.id.clone() }; model.start_request(request.clone()); runtime.send(request)?;
            },
            KeyCode::Char('c') => model.begin_form(FormKind::Create),
            KeyCode::Char('i') => model.begin_form(FormKind::Import),
            KeyCode::Char('r') => model.begin_form(FormKind::Rename),
            KeyCode::Char('u') => model.begin_form(FormKind::Update),
            KeyCode::Char('d') => if let Some(project) = model.selected_project() {
                model.view = ProjectView::Confirm { request: ProjectRequest::Delete { id: project.id.clone() }, prompt: format!("Delete Codex workspace '{}' permanently?", project.name), confirmed: false };
            },
            KeyCode::Char('m') => if model.selected > 0 {
                let id = model.projects[model.selected].id.clone();
                let before_id = Some(model.projects[model.selected - 1].id.clone());
                model.preferred_project_id = Some(id.clone());
                let request = ProjectRequest::Move { id, before_id }; model.start_request(request.clone()); runtime.send(request)?;
            },
            KeyCode::Char('n') => if let Some(next) = model.next_cursor.clone() {
                model.previous_cursors.push(model.cursor.clone()); let request = model.list_request(Some(next)); runtime.send(request)?;
            },
            KeyCode::Char('p') => if let Some(previous) = model.previous_cursors.pop() {
                let request = model.list_request(previous); runtime.send(request)?;
            },
            KeyCode::Char('R') => retry(model, runtime)?,
            _ => {}
        },
        ProjectView::Details(_) => match key.code {
            KeyCode::Esc | KeyCode::Left => model.view = ProjectView::List,
            KeyCode::Char('r') => model.begin_form(FormKind::Rename),
            KeyCode::Char('u') => model.begin_form(FormKind::Update),
            KeyCode::Char('d') => if let Some(project) = model.selected_project() {
                model.view = ProjectView::Confirm { request: ProjectRequest::Delete { id: project.id.clone() }, prompt: format!("Delete Codex workspace '{}' permanently?", project.name), confirmed: false };
            },
            KeyCode::Char('R') => retry(model, runtime)?,
            _ => {}
        },
        ProjectView::Form(form) => match key.code {
            KeyCode::Esc => model.view = ProjectView::List,
            KeyCode::Tab | KeyCode::Down => form.field = (form.field + 1) % form_field_count(form.kind),
            KeyCode::BackTab | KeyCode::Up => form.field = (form.field + form_field_count(form.kind) - 1) % form_field_count(form.kind),
            KeyCode::Backspace => { active_form_text(form).pop(); }
            KeyCode::Char(character) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => active_form_text(form).push(character),
            KeyCode::Enter => match model.form_request() {
                Ok(request @ ProjectRequest::Import { .. }) => model.view = ProjectView::Confirm { request, prompt: "Import may assign existing threads to this Codex workspace. Review the thread IDs above; default is Cancel.".to_string(), confirmed: false },
                Ok(request) => { model.start_request(request.clone()); runtime.send(request)?; }
                Err(message) => model.failure = Some(ProjectFailure { kind: FailureKind::Other, message }),
            },
            _ => {}
        },
        ProjectView::Confirm { request, confirmed, .. } => match key.code {
            KeyCode::Up | KeyCode::Char('y') => *confirmed = true,
            KeyCode::Down | KeyCode::Char('n') => *confirmed = false,
            KeyCode::Enter if *confirmed => { let request = request.clone(); model.start_request(request.clone()); runtime.send(request)?; }
            KeyCode::Enter => model.view = ProjectView::List,
            KeyCode::Esc => model.view = ProjectView::List,
            _ => {}
        },
    }
    Ok(false)
}

fn retry(model: &mut ProjectModel, runtime: &mut ProjectRuntime) -> anyhow::Result<()> {
    let Some(request) = model.retry.clone() else {
        return Ok(());
    };
    if model.reconnect_required() {
        runtime.restart()?;
    }
    model.start_request(request.clone());
    runtime.send(request)
}

fn form_field_count(kind: FormKind) -> usize {
    if kind == FormKind::Rename {
        1
    } else if kind == FormKind::Import {
        3
    } else {
        2
    }
}
fn active_form_text(form: &mut Form) -> &mut String {
    match form.field {
        0 => &mut form.name,
        1 => &mut form.roots,
        _ => &mut form.threads,
    }
}

fn valid_name(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("Workspace name is required".to_string());
    }
    if value.chars().count() > 200 {
        return Err("Workspace name must be at most 200 characters".to_string());
    }
    if value.chars().any(char::is_control) {
        return Err("Workspace name cannot contain control characters".to_string());
    }
    Ok(value.to_string())
}

fn valid_roots(value: &str) -> Result<Vec<ProjectRoot>, String> {
    let roots = value
        .lines()
        .flat_map(|line| line.split(','))
        .map(str::trim)
        .filter(|root| !root.is_empty())
        .map(|root| {
            if Path::new(root).is_absolute() {
                Ok(ProjectRoot { path: root.into() })
            } else {
                Err(format!("Workspace root must be an absolute path: {root}"))
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    if roots.is_empty() {
        Err("At least one absolute workspace root is required".to_string())
    } else {
        Ok(roots)
    }
}

fn valid_threads(value: &str) -> Result<Option<Vec<String>>, String> {
    let threads = value
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if threads.iter().any(|id| id.chars().any(char::is_control)) {
        return Err("Thread IDs cannot contain control characters".to_string());
    }
    Ok((!threads.is_empty()).then_some(threads))
}

fn project_roots_text(project: &Project) -> String {
    project
        .roots
        .iter()
        .map(|root| root.path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}
fn bounded(value: &str) -> String {
    value.chars().take(ERROR_LIMIT).collect()
}

struct ProjectRuntime {
    sender: Sender<ProjectRequest>,
    receiver: Receiver<WorkerEvent>,
}
impl ProjectRuntime {
    fn spawn() -> anyhow::Result<Self> {
        let (sender, command_receiver) = mpsc::channel();
        let (event_sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("cutex-project-catalog".to_string())
            .spawn(move || project_worker(command_receiver, event_sender))
            .context("Failed to start Codex workspace catalog worker")?;
        Ok(Self { sender, receiver })
    }
    fn restart(&mut self) -> anyhow::Result<()> {
        *self = Self::spawn()?;
        Ok(())
    }
    fn send(&self, request: ProjectRequest) -> anyhow::Result<()> {
        self.sender
            .send(request)
            .context("Codex workspace catalog worker stopped")
    }
    fn poll(&self) -> Option<WorkerEvent> {
        match self.receiver.try_recv() {
            Ok(event) => Some(event),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => None,
        }
    }
}

fn project_worker(receiver: Receiver<ProjectRequest>, sender: Sender<WorkerEvent>) {
    let mut client = match CatalogClient::spawn_local() {
        Ok(client) => {
            let _ = sender.send(WorkerEvent::Connected);
            client
        }
        Err(error) => {
            let _ = sender.send(WorkerEvent::Page(Err(error.into())));
            return;
        }
    };
    while let Ok(request) = receiver.recv() {
        let event = match request {
            ProjectRequest::List { cursor } => WorkerEvent::Page(
                client
                    .project_list(ProjectListParams {
                        cursor,
                        limit: Some(PROJECT_PAGE_SIZE),
                    })
                    .map(|page| (page.data, page.next_cursor))
                    .map_err(Into::into),
            ),
            ProjectRequest::Read { id } => {
                WorkerEvent::Project(client.project_read(&id).map_err(Into::into))
            }
            ProjectRequest::Create {
                name,
                roots,
                idempotency_key,
            } => WorkerEvent::Project(
                client
                    .project_create(ProjectCreateParams {
                        name,
                        roots,
                        metadata: None,
                        idempotency_key,
                    })
                    .map_err(Into::into),
            ),
            ProjectRequest::Import {
                name,
                roots,
                threads,
                idempotency_key,
            } => WorkerEvent::Project(
                client
                    .project_import(ProjectImportParams {
                        name,
                        roots,
                        metadata: None,
                        threads,
                        idempotency_key,
                    })
                    .map_err(Into::into),
            ),
            ProjectRequest::Rename { id, name } => WorkerEvent::Project(
                client
                    .project_update(ProjectUpdateParams {
                        project_id: id,
                        name: Some(name),
                        roots: None,
                        metadata: None,
                    })
                    .map_err(Into::into),
            ),
            ProjectRequest::Update { id, name, roots } => WorkerEvent::Project(
                client
                    .project_update(ProjectUpdateParams {
                        project_id: id,
                        name: Some(name),
                        roots: Some(roots),
                        metadata: None,
                    })
                    .map_err(Into::into),
            ),
            ProjectRequest::Move { id, before_id } => WorkerEvent::Mutated(
                client
                    .project_move(ProjectMoveParams {
                        project_id: id,
                        before_project_id: before_id,
                    })
                    .map_err(Into::into),
            ),
            ProjectRequest::Delete { id } => {
                WorkerEvent::Mutated(client.project_delete(&id).map_err(Into::into))
            }
        };
        let _ = sender.send(event);
    }
}

fn render(frame: &mut Frame<'_>, model: &ProjectModel) {
    let sections = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .split(frame.area());
    let status = if model.loading {
        " loading"
    } else if !model.connected {
        " unavailable"
    } else {
        ""
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "Workspaces",
                Style::new().fg(crate::cli_app::session_tui_layout::focus()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(status, Style::new().fg(Color::Yellow)),
        ])),
        sections[0],
    );
    match &model.view {
        ProjectView::List => render_list(frame, sections[1], model),
        ProjectView::Details(project) => render_details(frame, sections[1], project),
        ProjectView::Form(form) => {
            render_list(frame, sections[1], model);
            render_form(frame, sections[1], form);
        }
        ProjectView::Confirm {
            prompt,
            request,
            confirmed,
        } => {
            render_list(frame, sections[1], model);
            render_confirm(frame, sections[1], prompt, request, *confirmed);
        }
    }
    let message = model.failure.as_ref().map(|failure| match failure.kind { FailureKind::ProviderIncompatible => format!("Provider incompatible: {}; press R to retry", failure.message), FailureKind::Transport => format!("Transport failure: {}; press R to retry", failure.message), FailureKind::Other => format!("{}; press R to retry", failure.message) }).or_else(|| model.notice.clone()).unwrap_or_else(|| "Enter details  c create  i import  r rename  u update  m move up  d delete  n/p page  R retry  Esc back".to_string());
    frame.render_widget(
        Paragraph::new(message)
            .wrap(Wrap { trim: true })
            .style(Style::new().fg(if model.failure.is_some() {
                Color::Red
            } else {
                Color::DarkGray
            })),
        sections[2],
    );
}

fn render_list(frame: &mut Frame<'_>, area: Rect, model: &ProjectModel) {
    let rows = model
        .projects
        .iter()
        .map(|project| {
            Row::new(vec![
                Cell::from(project.name.clone()),
                Cell::from(project_roots_text(project)),
                Cell::from(project.position.to_string()),
            ])
        })
        .collect::<Vec<_>>();
    let table = Table::new(
        rows,
        [
            Constraint::Length(24),
            Constraint::Min(18),
            Constraint::Length(8),
        ],
    )
    .header(
        Row::new(["NAME", "ROOTS", "ORDER"])
            .style(Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD)),
    )
    .block(Block::bordered().title(" Native Codex workspaces "))
    .row_highlight_style(
        Style::new()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("> ");
    let mut state =
        TableState::default().with_selected((!model.projects.is_empty()).then_some(model.selected));
    frame.render_stateful_widget(table, area, &mut state);
    if model.projects.is_empty() && !model.loading {
        frame.render_widget(
            Paragraph::new("No Codex workspaces. Press c to create one.")
                .alignment(Alignment::Center)
                .style(Style::new().fg(Color::DarkGray)),
            Rect {
                y: area.y.saturating_add(3),
                height: area.height.saturating_sub(4),
                ..area
            },
        );
    }
}

fn render_details(frame: &mut Frame<'_>, area: Rect, project: &Project) {
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Name: ", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(&project.name),
        ]),
        Line::from(format!("ID: {}", project.id)),
        Line::from(format!("Order: {}", project.position)),
        Line::from("Roots:"),
    ];
    lines.extend(
        project
            .roots
            .iter()
            .map(|root| Line::from(format!("  {}", root.path.display()))),
    );
    if !project.metadata.is_empty() {
        lines.push(Line::from("Metadata:"));
        lines.extend(
            project
                .metadata
                .iter()
                .map(|(key, value)| Line::from(format!("  {key}: {value}"))),
        );
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(" Codex workspace details "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_form(frame: &mut Frame<'_>, area: Rect, form: &Form) {
    let title = match form.kind {
        FormKind::Create => " Create Codex workspace ",
        FormKind::Import => " Import Codex workspace ",
        FormKind::Rename => " Rename Codex workspace ",
        FormKind::Update => " Update Codex workspace ",
    };
    let mut lines = vec![Line::from(format!("Name: {}", form.name))];
    if form.kind != FormKind::Rename {
        lines.push(Line::from(format!(
            "Roots (absolute, comma-separated): {}",
            form.roots
        )));
    }
    if form.kind == FormKind::Import {
        lines.push(Line::from(format!(
            "Thread IDs (optional, comma-separated): {}",
            form.threads
        )));
        lines.push(Line::from(
            "Import always opens a default-cancelled review.",
        ));
    }
    lines.push(Line::from(
        "Tab/Up/Down select field; Enter submits; Esc cancels.",
    ));
    let popup = centered(area, 78, 9);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::bordered()
                    .title(title)
                    .border_style(Style::new().fg(crate::cli_app::session_tui_layout::focus())),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn render_confirm(
    frame: &mut Frame<'_>,
    area: Rect,
    prompt: &str,
    request: &ProjectRequest,
    confirmed: bool,
) {
    let choices = if confirmed {
        "Cancel    [Confirm]"
    } else {
        "[Cancel]    Confirm"
    };
    let review = match request {
        ProjectRequest::Import { threads, .. } => format!(
            "Threads to assign: {}",
            threads
                .as_ref()
                .map(|ids| ids.join(", "))
                .unwrap_or_else(|| "none".to_string())
        ),
        _ => String::new(),
    };
    let popup = centered(area, 66, 7);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(prompt),
            Line::from(review),
            Line::from(""),
            Line::from(choices),
            Line::from("Up/Down chooses; Enter continues; Esc cancels."),
        ])
        .block(Block::bordered().title(" Confirm "))
        .wrap(Wrap { trim: true }),
        popup,
    );
}

fn centered(area: Rect, width_percent: u16, height: u16) -> Rect {
    let horizontal = Layout::horizontal([
        Constraint::Percentage((100 - width_percent) / 2),
        Constraint::Percentage(width_percent),
        Constraint::Percentage((100 - width_percent) / 2),
    ])
    .split(area);
    let vertical = Layout::vertical([
        Constraint::Length(area.height.saturating_sub(height) / 2),
        Constraint::Length(height),
        Constraint::Min(0),
    ])
    .split(horizontal[1]);
    vertical[1]
}

fn open_terminal() -> anyhow::Result<ProjectTerminal> {
    enable_raw_mode().context("Failed to enable raw mode")?;
    let mut stdout = std::io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(error).context("Failed to enter alternate screen");
    }
    Terminal::new(CrosstermBackend::new(stdout)).context("Failed to initialize Workspaces terminal")
}

impl Drop for ProjectTerminalRestore {
    fn drop(&mut self) {
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen, Show);
        let _ = disable_raw_mode();
    }
}
struct ProjectTerminalRestore;

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    #[test]
    fn validates_names_and_absolute_roots() {
        assert!(valid_name(" ").is_err());
        assert!(valid_name(&"a".repeat(201)).is_err());
        assert!(valid_roots("relative").is_err());
        let roots = if cfg!(windows) {
            r"C:\work\a, C:\work\b"
        } else {
            "/work/a, /work/b"
        };
        assert_eq!(valid_roots(roots).unwrap().len(), 2);
    }
    #[test]
    fn pagination_keeps_the_selected_project_id_when_present() {
        let mut model = ProjectModel {
            preferred_project_id: Some("b".to_string()),
            ..ProjectModel::default()
        };
        let _ = model.receive(WorkerEvent::Page(Ok((
            vec![project("a"), project("b")],
            None,
        ))));
        assert_eq!(model.selected, 1);
    }
    #[test]
    fn delete_and_import_default_to_cancel() {
        let request = ProjectRequest::Delete {
            id: "a".to_string(),
        };
        let view = ProjectView::Confirm {
            request,
            prompt: "delete".to_string(),
            confirmed: false,
        };
        assert!(matches!(
            view,
            ProjectView::Confirm {
                confirmed: false,
                ..
            }
        ));
    }
    #[test]
    fn successful_mutation_requests_a_refresh_and_preserves_its_id() {
        let mut model = ProjectModel::default();
        model.retry = Some(ProjectRequest::Create {
            name: "alpha".to_string(),
            roots: vec![],
            idempotency_key: "create-1".to_string(),
        });
        let refresh = model.receive(WorkerEvent::Project(Ok(project("new"))));
        assert!(matches!(
            refresh,
            Some(ProjectRequest::List { cursor: None })
        ));
        assert_eq!(model.preferred_project_id.as_deref(), Some("new"));
        assert_eq!(
            model.notice.as_deref(),
            Some("Codex workspace catalog updated")
        );
    }
    #[test]
    fn list_requests_remain_available_for_retry() {
        let mut model = ProjectModel::default();
        let request = model.list_request(Some("cursor-2".to_string()));
        assert_eq!(model.retry, Some(request));
        assert!(model.loading);
    }
    #[test]
    fn initial_list_is_retained_for_a_first_launch_reconnect() {
        let mut model = ProjectModel::default();
        let request = model.list_request(None);
        assert_eq!(request, ProjectRequest::List { cursor: None });
        assert_eq!(model.retry, Some(ProjectRequest::List { cursor: None }));
        assert!(model.reconnect_required());
    }
    #[test]
    fn transport_failure_marks_the_catalog_connection_disconnected() {
        let mut model = ProjectModel {
            connected: true,
            ..ProjectModel::default()
        };
        let _ = model.receive(WorkerEvent::Page(Err(ProjectFailure {
            kind: FailureKind::Transport,
            message: "broken pipe".to_string(),
        })));
        assert!(!model.connected);
        assert!(model.reconnect_required());
    }
    #[test]
    fn create_and_import_retries_keep_the_same_idempotency_key() {
        let create = ProjectRequest::Create {
            name: "alpha".to_string(),
            roots: vec![],
            idempotency_key: "create-key".to_string(),
        };
        let import = ProjectRequest::Import {
            name: "beta".to_string(),
            roots: vec![],
            threads: Some(vec!["thread-1".to_string()]),
            idempotency_key: "import-key".to_string(),
        };
        let mut model = ProjectModel::default();
        model.start_request(create.clone());
        assert_eq!(model.retry, Some(create));
        model.start_request(import.clone());
        assert_eq!(model.retry, Some(import));
    }
    #[test]
    fn create_form_has_two_editable_fields_and_import_has_three() {
        assert_eq!(form_field_count(FormKind::Create), 2);
        assert_eq!(form_field_count(FormKind::Import), 3);
        assert_eq!(form_field_count(FormKind::Update), 2);
        let form = Form {
            kind: FormKind::Import,
            name: "a".to_string(),
            roots: if cfg!(windows) { r"C:\a" } else { "/a" }.to_string(),
            threads: "t1".to_string(),
            field: 2,
        };
        let model = ProjectModel {
            view: ProjectView::Form(form),
            ..ProjectModel::default()
        };
        assert!(matches!(
            model.form_request(),
            Ok(ProjectRequest::Import { .. })
        ));
    }
    #[test]
    fn render_shows_an_empty_catalog_state() {
        let mut terminal = Terminal::new(TestBackend::new(80, 18)).expect("test terminal");
        let model = ProjectModel {
            connected: true,
            loading: false,
            ..ProjectModel::default()
        };
        terminal
            .draw(|frame| render(frame, &model))
            .expect("render");
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("No Codex workspaces"));
    }
    #[test]
    fn provider_errors_are_classified() {
        let failure: ProjectFailure = CatalogError::ProviderIncompatible("bad".to_string()).into();
        assert_eq!(failure.kind, FailureKind::ProviderIncompatible);
    }
    fn project(id: &str) -> Project {
        Project {
            id: id.to_string(),
            name: id.to_string(),
            roots: vec![],
            metadata: Default::default(),
            position: 0,
            created_at: 0,
            updated_at: 0,
        }
    }
}
