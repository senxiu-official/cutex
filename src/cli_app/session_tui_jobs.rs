//! Human Job metadata workspace; refresh never blocks the terminal thread.
use super::{
    management_control_plane::ManagementControlClient,
    session_tui::ShellEvents,
    session_tui_input, session_tui_layout as theme,
    session_tui_terminal::Terminal,
    session_tui_workspace::{primary_panel_shortcut, PrimaryPanel, PrimaryPanelOutcome},
};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Cell, Paragraph, Row, Table, TableState, Wrap},
};
use serde_json::{json, Value};
use std::{
    io::Stdout,
    sync::mpsc,
    time::{Duration, Instant},
};
use tui_input::Input;

#[derive(Default)]
struct Model {
    sort_order: cutex::profiles::list_preferences::ListSort,
    original_rows: Vec<Value>,
    rows: Vec<Value>,
    selected: usize,
    query: Input,
    filtering: bool,
    cursors: Vec<Option<String>>,
    next: Option<String>,
    detail: bool,
    scroll: u16,
    hide_details: bool,
    loading: bool,
    notice: String,
    generation: u64,
}
impl Model {
    fn apply_sort(&mut self) {
        let selected = self.rows.get(self.selected).map(|v| text(v, "jobId"));
        self.rows = self.original_rows.clone();
        self.rows.sort_by(|a, b| self.sort_order.compare_names(&text(a, "actionId"), &text(b, "actionId")));
        self.selected = selected.and_then(|id| self.rows.iter().position(|v| text(v, "jobId") == id)).unwrap_or(0);
    }
}
fn text(value: &Value, key: &str) -> String {
    value[key]
        .as_str()
        .unwrap_or("N/A")
        .chars()
        .filter(|c| !c.is_control())
        .take(1024)
        .collect()
}
fn append_filter(query: &str, paste: &str) -> String {
    let mut result = query.to_owned();
    for ch in paste.chars().filter(|c| !c.is_control()) {
        if result.len() + ch.len_utf8() > 256 {
            break;
        }
        result.push(ch);
    }
    result
}
fn stamp(value: &Value, key: &str) -> String {
    value[key]
        .as_i64()
        .and_then(|v| chrono::DateTime::from_timestamp(v, 0))
        .map(|v| {
            v.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "N/A".into())
}
fn state_color(value: &Value) -> ratatui::style::Color {
    match value["state"].as_str().unwrap_or("") {
        "running" | "launch_pending" => theme::accent(),
        "failed" => theme::error(),
        "launch_unknown" => theme::warning(),
        "interrupted" => theme::focus(),
        _ => theme::text(),
    }
}
fn details(value: &Value) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(
            text(value, "jobId"),
            Style::default().fg(theme::warning()),
        )),
        Line::from(format!("Action: {}", text(value, "actionId"))),
        Line::from(format!(
            "Agent: {}",
            text(
                value,
                if value["agentName"].is_string() {
                    "agentName"
                } else {
                    "sessionId"
                }
            )
        )),
        Line::from(format!("Session: {}", text(value, "sessionId"))),
        Line::from(format!(
            "State: {} · exit code: {}",
            text(value, "state"),
            value["exitCode"]
                .as_i64()
                .map(|v| v.to_string())
                .unwrap_or_else(|| "N/A".into())
        )),
        Line::from(format!("Created: {}", stamp(value, "createdAt"))),
        Line::from(format!("Updated: {}", stamp(value, "updatedAt"))),
        Line::from(format!("Directory: {}", text(value, "cwd"))),
        Line::from(format!("Completion delivery: {}", text(value, "delivery"))),
    ];
    if let Some(ms) = value["execution"]["observedRunDurationMillis"].as_u64() {
        lines.push(Line::from(format!(
            "Observed runtime: {}m {:.1}s",
            ms / 60000,
            (ms % 60000) as f64 / 1000.0
        )));
    }
    for stream in ["stdout", "stderr"] {
        let v = &value[stream];
        lines.push(Line::from(format!(
            "{stream}: {} B observed · {} B retained{}",
            v["observedBytes"].as_u64().unwrap_or(0),
            v["retainedBytes"].as_u64().unwrap_or(0),
            if v["truncated"] == true {
                " · truncated"
            } else {
                ""
            }
        )));
    }
    lines.push(Line::from(format!(
        "Output reference: {}",
        text(value, "outputReference")
    )));
    lines
}
fn render(frame: &mut ratatui::Frame, model: &mut Model) {
    let areas = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(2),
    ])
    .split(frame.area());
    frame.render_widget(
        Paragraph::new(theme::tabs(PrimaryPanel::Jobs, frame.area().width)),
        areas[0],
    );
    frame.render_widget(Paragraph::new(theme::heading("Cutex","Jobs")),areas[1]);
    let panes = theme::inspector_panes(areas[2], !model.hide_details);
    let (left, right) = panes.map(|(l, r)| (l, Some(r))).unwrap_or((areas[2], None));
    if !model.detail || right.is_some() {
        let list = Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).split(left);
        frame.render_widget(
            Paragraph::new(model.query.value()).block(
                Block::bordered()
                    .title(" Filter jobs · ID / action / session [/] ")
                    .border_style(Style::default().fg(if model.filtering {
                        theme::focus()
                    } else {
                        theme::muted()
                    })),
            ),
            list[0],
        );
        let rows = model.rows.iter().map(|v| {
            Row::new(vec![
                Cell::from(text(v, "actionId")),
                Cell::from(text(v, "state")).style(Style::default().fg(state_color(v))),
                Cell::from(text(
                    v,
                    if v["agentName"].is_string() {
                        "agentName"
                    } else {
                        "sessionId"
                    },
                ))
                .style(Style::default().fg(theme::brand())),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Percentage(48),
                Constraint::Length(14),
                Constraint::Min(12),
            ],
        )
        .header(Row::new(["ACTION", "STATE", "AGENT"]).style(Style::default().fg(theme::muted())))
        .block(
            Block::bordered()
                .border_style(Style::default().fg(theme::muted())),
        )
        .row_highlight_style(Style::default().bg(theme::selection()))
        .highlight_symbol("> ");
        let mut selection =
            TableState::default().with_selected((!model.rows.is_empty()).then_some(model.selected));
        frame.render_stateful_widget(table, list[1], &mut selection);
    }
    if let Some(area) = right.or_else(|| model.detail.then_some(areas[2])) {
        let lines = model
            .rows
            .get(model.selected)
            .map(details)
            .unwrap_or_else(|| {
                vec![Line::from(if model.notice.is_empty() {
                    "Select a Job".to_string()
                } else {
                    model.notice.clone()
                })]
            });
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((model.scroll, 0))
                .block(
                    Block::bordered()
                        .title(" Details ")
                        .border_style(Style::default().fg(if model.detail {
                            theme::focus()
                        } else {
                            theme::muted()
                        })),
                ),
            area,
        );
    }
    let status = if model.loading {
        "Loading Jobs…".into()
    } else if !model.notice.is_empty() {
        model.notice.clone()
    } else {
        format!(
            "Page {} · {} Jobs{}",
            model.cursors.len().max(1),
            model.rows.len(),
            if model.next.is_some() {
                " · more available"
            } else {
                ""
            }
        )
    };
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(theme::muted())),
        areas[3],
    );
    let hints = if model.filtering {
        vec![("Enter/Esc", "finish filter"), ("Ctrl+U", "clear")]
    } else {
        vec![
            ("←/→", "panels"),
            ("↑/↓", "select"),
            ("Enter", "details"),
            ("/", "filter"),
            ("N/P", "pages"),
            ("Alt+S", "sort"),
            ("F5", "refresh"),
            ("Alt+B", "details"),
            ("Esc", "back"),
            ("Ctrl+C", "exit"),
        ]
    };
    frame.render_widget(
        Paragraph::new(Line::from(super::session_tui::footer_hints(&hints)))
            .wrap(Wrap { trim: true }),
        areas[4],
    );
}

pub(super) fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    events: &mut ShellEvents,
    sort_order: cutex::profiles::list_preferences::ListSort,
) -> anyhow::Result<PrimaryPanelOutcome> {
    let mut model = Model {
        sort_order,
        cursors: vec![None],
        ..Default::default()
    };
    let (tx, rx) = mpsc::channel::<(u64, Result<Value, String>)>();
    let mut due = Instant::now();
    loop {
        if let Ok((generation, result)) = rx.try_recv() {
            model.loading = false;
            if generation == model.generation {
                match result {
                    Ok(value) => {
                        let selected = model.rows.get(model.selected).map(|v| text(v, "jobId"));
                        model.original_rows = value["data"].as_array().cloned().unwrap_or_default();
                        model.apply_sort();
                        model.selected = selected
                            .and_then(|id| model.rows.iter().position(|v| text(v, "jobId") == id))
                            .unwrap_or(0);
                        model.next = value["nextCursor"].as_str().map(str::to_owned);
                        model.notice.clear();
                    }
                    Err(error) => model.notice = error,
                }
                due = Instant::now() + Duration::from_secs(5);
            }
        }
        if !model.loading && Instant::now() >= due {
            model.loading = true;
            let tx = tx.clone();
            let generation = model.generation;
            let params = json!({"query":model.query.value(),"cursor":model.cursors.last().cloned().flatten(),"limit":50});
            std::thread::spawn(move || {
                let result = ManagementControlClient::connect()
                    .and_then(|c| c.jobs(&params))
                    .map_err(|e| format!("Jobs unavailable: {e:#}"));
                let _ = tx.send((generation, result));
            });
        }
        terminal.draw(|f| render(f, &mut model))?;
        let event = events.next()?;
        if let Some(Event::Paste(paste)) = event.as_ref() {
            if model.filtering {
                model.query = Input::from(append_filter(model.query.value(), paste));
                model.cursors = vec![None];
                model.next = None;
                model.generation += 1;
                due = Instant::now() + Duration::from_millis(250);
            }
        }
        let Some(Event::Key(key)) = event else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C'))
        {
            return Ok(PrimaryPanelOutcome::Exit);
        }
        if let Some(panel) = primary_panel_shortcut(key).filter(|p| *p != PrimaryPanel::Jobs) {
            return Ok(PrimaryPanelOutcome::Switch(panel));
        }
        if key.modifiers == KeyModifiers::ALT && matches!(key.code, KeyCode::Char('b' | 'B')) {
            model.hide_details = !model.hide_details;
            continue;
        }
        if key.kind == KeyEventKind::Press
            && key.modifiers.is_empty()
            && key.code == KeyCode::F(5)
        {
            due = Instant::now();
            continue;
        }
        if model.filtering {
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Tab) {
                model.filtering = false;
                continue;
            }
            let old = model.query.value().to_string();
            session_tui_input::edit(&mut model.query, key);
            if model.query.value().len() > 256 {
                model.query = Input::from(old.clone())
            }
            if old != model.query.value() {
                model.cursors = vec![None];
                model.next = None;
                model.generation += 1;
                due = Instant::now() + Duration::from_millis(250);
            }
            continue;
        }
        if key.modifiers == KeyModifiers::ALT && matches!(key.code, KeyCode::Char('s' | 'S')) {
            model.sort_order = model.sort_order.next_names();
            model.apply_sort();
            model.notice = format!("Sort: {} (action name, current page)", model.sort_order.label());
            continue;
        }
        if !key.modifiers.is_empty() {
            continue;
        }
        match key.code {
            KeyCode::Esc => {
                model.detail = false;
                model.scroll = 0
            }
            KeyCode::Enter => model.detail = !model.rows.is_empty(),
            KeyCode::Tab | KeyCode::BackTab => {
                model.detail = !model.detail && !model.rows.is_empty();
                model.scroll = 0
            }
            KeyCode::Up if model.detail => model.scroll = model.scroll.saturating_sub(1),
            KeyCode::Down if model.detail => model.scroll = model.scroll.saturating_add(1).min(100),
            KeyCode::Up => {
                model.selected = model.selected.saturating_sub(1);
                model.scroll = 0
            }
            KeyCode::Down => {
                model.selected = (model.selected + 1).min(model.rows.len().saturating_sub(1));
                model.scroll = 0
            }
            KeyCode::Left => return Ok(PrimaryPanelOutcome::Switch(PrimaryPanel::Alarms)),
            KeyCode::Right => return Ok(PrimaryPanelOutcome::Switch(PrimaryPanel::Profiles)),
            KeyCode::Char('/') => model.filtering = true,
            KeyCode::Char('r' | 'R') => due = Instant::now(),
            KeyCode::Char('n' | 'N') if model.next.is_some() && !model.loading => {
                model.cursors.push(model.next.clone());
                model.generation += 1;
                due = Instant::now()
            }
            KeyCode::Char('p' | 'P') if model.cursors.len() > 1 && !model.loading => {
                model.cursors.pop();
                model.generation += 1;
                due = Instant::now()
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sorting_keeps_job_selection_and_restores_server_order() {
        let original = vec![json!({"jobId":"b","actionId":"Zebra"}), json!({"jobId":"a","actionId":"alpha"})];
        let mut model = Model { rows: original.clone(), original_rows: original.clone(), ..Default::default() };
        model.sort_order = model.sort_order.next_names();
        model.apply_sort();
        assert_eq!(model.rows[0]["jobId"], "a");
        assert_eq!(model.rows[model.selected]["jobId"], "b");
        model.sort_order = Default::default();
        model.apply_sort();
        assert_eq!(model.rows, original);
        assert_eq!(model.selected, 0);
    }
    #[test]
    fn pasted_filter_respects_utf8_byte_limit() {
        let result = append_filter(
            &"a".repeat(252),
            "中中文
",
        );
        assert_eq!(result.len(), 255);
        assert!(result.ends_with('中'));
    }
    #[test]
    fn job_details_explain_exit_and_truncation_without_reading_output() {
        let row = json!({"jobId":"job_1","actionId":"a","sessionId":"cutex.1","state":"exited","exitCode":7,
            "stdout":{"observedBytes":99,"retainedBytes":10,"truncated":true},"stderr":{},"createdAt":100});
        let display = details(&row)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(display.contains("exit code: 7"));
        assert!(display.contains("99 B observed · 10 B retained · truncated"));
    }
    #[test]
    fn jobs_render_narrow_details_and_wide_split_with_unicode_names() {
        for width in [35, 80, 160, 240] {
            for detail in [false, true] {
                let mut model = Model {
                    rows: vec![
                        json!({"jobId":"job_1","actionId":"中文作业","agentName":"测试Agent","state":"running"}),
                    ],
                    detail,
                    ..Default::default()
                };
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 24)).unwrap();
                terminal.draw(|f| render(f, &mut model)).unwrap();
                let buffer = terminal.backend().buffer();
                assert_eq!(buffer.area.width, width);
                let text = buffer
                    .content
                    .iter()
                    .map(|c| c.symbol())
                    .collect::<String>();
                if detail {
                    assert!(text.contains("Details"));
                }
            }
        }
    }
}
