//! Agent Bus alarms workspace (Alt+4).
//!
//! Lists the durable one-shot reminders from `agent_bus::alarms` with a filter
//! and an inspector; Alt+C cancels the selected alarm. Alarms are created with
//! `alarm_set` (MCP) or `cutex alarm set` (CLI), never from here.

use std::collections::HashMap;
use std::io::Stdout;
use std::time::{Duration, Instant};

use chrono::DateTime;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use cutex::agent_bus::alarms::Alarm;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;
use tui_input::{Input, InputRequest};

use super::session_tui::{footer_hints, ShellEvents};
use super::session_tui_input as input_policy;
use super::session_tui_layout as theme;
use super::session_tui_terminal::Terminal;
use super::session_tui_workspace::{primary_panel_shortcut, PrimaryPanel, PrimaryPanelOutcome};

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Model {
    rows: Vec<Alarm>,
    /// Cutex session id → formal Agent name.
    names: HashMap<String, String>,
    selected: usize,
    query: Input,
    editing: bool,
    inspect: bool,
    notice: String,
}

impl Model {
    fn name<'a>(&'a self, session: &'a str) -> &'a str {
        self.names.get(session).map(String::as_str).unwrap_or(session)
    }

    fn visible(&self) -> Vec<&Alarm> {
        let needle = self.query.value().to_lowercase();
        self.rows
            .iter()
            .filter(|a| {
                format!("{} {} {} {}", a.alarm_id, self.name(&a.to), a.message, a.state)
                    .to_lowercase()
                    .contains(&needle)
            })
            .collect()
    }

    fn clamp_selection(&mut self) {
        let len = self.visible().len();
        self.selected = self.selected.min(len.saturating_sub(1));
    }

    fn current(&self) -> Option<&Alarm> {
        self.visible().get(self.selected).copied()
    }

    fn refresh(&mut self) {
        match cutex::agent_bus::alarms::list(None).and_then(|v| Ok(serde_json::from_value(v)?)) {
            Ok(rows) => self.rows = rows,
            Err(error) => self.notice = error.to_string(),
        }
        if let Ok(store) = cutex::session::store::load_cutex_session_store() {
            self.names = store
                .sessions
                .values()
                .filter_map(|r| r.formal_agent_name.clone().map(|n| (r.cutex_session_id.clone(), n)))
                .collect();
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum KeyAction {
    Continue,
    Refresh,
    Cancel { owner: String, alarm_id: String },
    Leave(PrimaryPanelOutcome),
}

pub(super) fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    events: &mut ShellEvents,
) -> anyhow::Result<PrimaryPanelOutcome> {
    let mut model = Model::default();
    let mut refreshed_at: Option<Instant> = None;
    loop {
        if refreshed_at.is_none_or(|at| at.elapsed() >= REFRESH_INTERVAL) {
            model.refresh();
            refreshed_at = Some(Instant::now());
        }
        model.clamp_selection();
        terminal.draw(|frame| render(frame, &model))?;
        let Some(event) = events.next()? else { continue };
        let Event::Key(key) = event else { continue };
        match handle_key(&mut model, key) {
            KeyAction::Continue => {}
            KeyAction::Refresh => refreshed_at = None,
            KeyAction::Cancel { owner, alarm_id } => {
                model.notice = match cutex::agent_bus::alarms::cancel(Some(&owner), &alarm_id) {
                    Ok(_) => "Alarm cancelled".into(),
                    Err(error) => error.to_string(),
                };
                refreshed_at = None;
            }
            KeyAction::Leave(outcome) => return Ok(outcome),
        }
    }
}

fn handle_key(model: &mut Model, key: KeyEvent) -> KeyAction {
    if key.kind == KeyEventKind::Release {
        return KeyAction::Continue;
    }
    if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
        return KeyAction::Leave(PrimaryPanelOutcome::Exit);
    }
    if let Some(panel) = primary_panel_shortcut(key).filter(|panel| *panel != PrimaryPanel::Alarms) {
        return KeyAction::Leave(PrimaryPanelOutcome::Switch(panel));
    }
    if key.code == KeyCode::F(5) {
        return KeyAction::Refresh;
    }
    if key.modifiers == KeyModifiers::ALT && matches!(key.code, KeyCode::Char('c' | 'C')) {
        return model.current().map_or(KeyAction::Continue, |a| KeyAction::Cancel {
            owner: a.owner.clone(),
            alarm_id: a.alarm_id.clone(),
        });
    }
    if model.editing {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => model.editing = false,
            KeyCode::Backspace => {
                model.query.handle(InputRequest::DeletePrevChar);
                model.selected = 0;
            }
            KeyCode::Char(c) if key.modifiers.is_empty() => {
                model.query.handle(InputRequest::InsertChar(c));
                model.selected = 0;
            }
            _ => {}
        }
        return KeyAction::Continue;
    }
    match key.code {
        KeyCode::Esc => model.inspect = false,
        KeyCode::Left | KeyCode::Right => {
            if let Some(panel) = PrimaryPanel::Alarms.adjacent(key.code == KeyCode::Right) {
                return KeyAction::Leave(PrimaryPanelOutcome::Switch(panel));
            }
        }
        KeyCode::Up => model.selected = model.selected.saturating_sub(1),
        KeyCode::Down => model.selected = model.selected.saturating_add(1),
        KeyCode::Enter => model.inspect = !model.inspect,
        KeyCode::Char('/') => model.editing = true,
        _ => {}
    }
    model.clamp_selection();
    KeyAction::Continue
}

fn render(frame: &mut Frame<'_>, model: &Model) {
    let visible = model.visible();
    let current = visible.get(model.selected).copied();
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(1),
        Constraint::Length(2),
    ])
    .split(frame.area());
    frame.render_widget(Paragraph::new(theme::tabs(PrimaryPanel::Alarms, frame.area().width)), chunks[0]);
    frame.render_widget(Paragraph::new(theme::heading("Cutex", "Alarms")), chunks[1]);
    let panes = theme::list_details(chunks[2], true);
    input_policy::render_input(frame, panes.filter, &model.query, " Filter alarms [/] ", model.editing);
    let table = Table::new(
        visible.iter().map(|a| {
            let time = DateTime::parse_from_rfc3339(&a.at)
                .map(|t| t.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string())
                .unwrap_or_else(|_| a.at.clone());
            Row::new(vec![a.alarm_id.clone(), model.name(&a.to).to_owned(), time, a.state.clone()])
        }),
        [
            Constraint::Percentage(32),
            Constraint::Percentage(30),
            Constraint::Length(12),
            Constraint::Length(10),
        ],
    )
    .header(Row::new(["ALARM", "AGENT", "TIME", "STATUS"]).style(Style::new().fg(theme::muted())))
    .block(Block::bordered())
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
    .highlight_symbol("> ");
    let mut state = TableState::default().with_selected(current.map(|_| model.selected));
    frame.render_stateful_widget(table, panes.list, &mut state);
    if let Some(area) = panes.details.or_else(|| model.inspect.then_some(chunks[2])) {
        let text = current.map(|a| details(model, a)).unwrap_or_else(|| {
            "Create with alarm_set (MCP) or cutex alarm set (CLI).".into()
        });
        frame.render_widget(
            Paragraph::new(text).wrap(Wrap { trim: false }).block(Block::bordered().title(" Alarm details ")),
            area,
        );
    }
    frame.render_widget(Paragraph::new(model.notice.as_str()), chunks[3]);
    frame.render_widget(
        Paragraph::new(Line::from(footer_hints(&[
            ("↑/↓", "select"),
            ("Enter", "details"),
            ("/", "filter"),
            ("Alt+C", "cancel alarm"),
            ("F5", "refresh"),
            ("←/→", "panels"),
        ])))
        .wrap(Wrap { trim: true }),
        chunks[4],
    );
}

fn details(model: &Model, a: &Alarm) -> String {
    let at = DateTime::parse_from_rfc3339(&a.at)
        .map(|t| t.with_timezone(&chrono::Local).to_string())
        .unwrap_or_else(|_| a.at.clone());
    let delivery = if a.state == "queued" {
        "Message submitted to Agent Bus."
    } else {
        "Delivery: after_turn"
    };
    format!(
        "{}\n\nTo: {}\nAt: {}\nStatus: {}\nAttempts: {}\nCreated by: {}\n\n{}\n\nLast attempt: {}\nLast error: {}\n\n{}",
        a.alarm_id,
        model.name(&a.to),
        at,
        a.state,
        a.attempts,
        model.name(&a.owner),
        a.message,
        a.last_attempt.as_deref().unwrap_or("—"),
        a.last_error.as_deref().unwrap_or("—"),
        delivery,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alarm(id: &str, to: &str, state: &str) -> Alarm {
        Alarm {
            alarm_id: id.into(),
            owner: "cutex.owner".into(),
            to: to.into(),
            message: format!("wake {id}"),
            at: "2026-10-09T03:00:00Z".into(),
            delay_seconds: None,
            message_id: format!("msg-{id}"),
            state: state.into(),
            attempts: 0,
            next_attempt: 0,
            receipt: None,
            last_error: None,
            last_attempt: None,
        }
    }

    fn model() -> Model {
        Model {
            rows: vec![alarm("standup", "cutex.a", "scheduled"), alarm("deploy", "cutex.b", "queued")],
            names: HashMap::from([("cutex.a".to_string(), "Alpha".to_string())]),
            ..Default::default()
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn rendered(width: u16, model: &Model) -> String {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 24)).unwrap();
        terminal.draw(|frame| render(frame, model)).unwrap();
        terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn alarms_is_its_own_primary_panel_between_projects_and_jobs() {
        let mut model = model();
        assert_eq!(handle_key(&mut model, key(KeyCode::Char('4'), KeyModifiers::ALT)), KeyAction::Continue);
        assert_eq!(
            handle_key(&mut model, key(KeyCode::Char('5'), KeyModifiers::ALT)),
            KeyAction::Leave(PrimaryPanelOutcome::Switch(PrimaryPanel::Jobs))
        );
        assert_eq!(
            handle_key(&mut model, key(KeyCode::Left, KeyModifiers::NONE)),
            KeyAction::Leave(PrimaryPanelOutcome::Switch(PrimaryPanel::Projects))
        );
        assert_eq!(
            handle_key(&mut model, key(KeyCode::Right, KeyModifiers::NONE)),
            KeyAction::Leave(PrimaryPanelOutcome::Switch(PrimaryPanel::Jobs))
        );
        // Esc closes the inspector but never leaves the panel.
        model.inspect = true;
        assert_eq!(handle_key(&mut model, key(KeyCode::Esc, KeyModifiers::NONE)), KeyAction::Continue);
        assert!(!model.inspect);
        assert_eq!(handle_key(&mut model, key(KeyCode::Esc, KeyModifiers::NONE)), KeyAction::Continue);
        assert_eq!(
            handle_key(&mut model, key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            KeyAction::Leave(PrimaryPanelOutcome::Exit)
        );
    }

    #[test]
    fn filter_uses_agent_names_and_cancel_targets_the_selected_alarm() {
        let mut model = model();
        handle_key(&mut model, key(KeyCode::Char('/'), KeyModifiers::NONE));
        for c in "alpha".chars() {
            handle_key(&mut model, key(KeyCode::Char(c), KeyModifiers::NONE));
        }
        handle_key(&mut model, key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(model.visible().iter().map(|a| a.alarm_id.as_str()).collect::<Vec<_>>(), ["standup"]);
        assert_eq!(
            handle_key(&mut model, key(KeyCode::Char('c'), KeyModifiers::ALT)),
            KeyAction::Cancel { owner: "cutex.owner".into(), alarm_id: "standup".into() }
        );
        model.query = Input::default();
        handle_key(&mut model, key(KeyCode::Down, KeyModifiers::NONE));
        handle_key(&mut model, key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(model.current().map(|a| a.alarm_id.as_str()), Some("deploy"));
    }

    #[test]
    fn alarms_render_with_their_own_tab_and_title_at_any_width() {
        for width in [40, 80, 160] {
            let mut model = model();
            model.inspect = true;
            let text = rendered(width, &model);
            assert!(text.contains("Alarms"), "{text}");
            assert!(!text.contains("Tasks"), "{text}");
        }
        let text = rendered(160, &model());
        assert!(text.contains("Cutex Alarms"), "{text}");
        assert!(text.contains("Alpha") && text.contains("wake standup"), "{text}");
    }
}
