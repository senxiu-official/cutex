//! Manual local service controls; subprocess work stays outside the input loop.
use super::{service_control::{self, Action, Service}, session_tui::ShellEvents,
    session_tui_terminal::Terminal, session_tui_remote::Outcome, session_tui_layout as theme};
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{backend::CrosstermBackend, layout::{Constraint, Layout}, style::Style,
    text::Line, widgets::{Block, Paragraph, Wrap}};
use std::{io::Stdout, sync::mpsc};

pub(super) fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>, events: &mut ShellEvents) -> anyhow::Result<Outcome> {
    let services = [Service::Bus, Service::Management];
    let mut selected = 0;
    let mut status = "Select a service. R refreshes status. Actions affect this machine only.".to_string();
    let mut confirmation = None;
    let mut busy = false;
    let (tx, rx) = mpsc::channel();
    loop {
        if let Ok(result) = rx.try_recv() { status = result; busy = false; }
        terminal.draw(|frame| {
            let areas = Layout::vertical([Constraint::Min(5), Constraint::Length(3)]).split(frame.area());
            let panes = Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)]).split(areas[0]);
            let rows: Vec<_> = services.iter().enumerate().map(|(index, service)|
                Line::styled(format!("{} {}", if index == selected { ">" } else { " " }, service.name()),
                    Style::new().fg(if index == selected { theme::focus() } else { theme::text() }))).collect();
            frame.render_widget(Paragraph::new(rows).block(Block::bordered().title(" Local services ")), panes[0]);
            let text = if let Some(action) = confirmation {
                format!("{action:?} {}?\n\nMessages and management requests may be briefly unavailable. Agent runtimes and Job Service are not stopped.\nOn Windows, restarting Agent Bus also restarts Management API; stopping it stops both.\nSaved settings apply on startup.\n\nEnter confirms · Esc cancels", services[selected].name())
            } else { format!("{status}\n\nOpening this page never starts a service.\nLinux uses installed user systemd units.\nWindows uses the shared Cutex controller and runtime host. The TethysUNE desktop is optional.\n\nJob Service is managed separately.") };
            frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }).block(Block::bordered().title(" Details ")), panes[1]);
            frame.render_widget(Paragraph::new(if busy { "Working… · Esc returns (requested action continues)" } else { "↑/↓ select · S start · X stop · T restart · R status · Esc back" }).style(Style::new().fg(theme::focus())).wrap(Wrap { trim: false }), areas[1]);
        })?;
        let Some(Event::Key(key)) = events.next()? else { continue };
        if key.kind != KeyEventKind::Press { continue; }
        if key.code == KeyCode::Esc {
            if confirmation.take().is_some() { continue; }
            return Ok(Outcome::Back);
        }
        if busy { continue; }
        let action = if confirmation.is_some() {
            if key.code == KeyCode::Enter { confirmation.take() } else { None }
        } else {
            match key.code {
                KeyCode::Up | KeyCode::Down => { selected = 1 - selected; None },
                KeyCode::Char('r' | 'R') => Some(Action::Status),
                KeyCode::Char('s' | 'S') => { confirmation = Some(Action::Start); None },
                KeyCode::Char('x' | 'X') => { confirmation = Some(Action::Stop); None },
                KeyCode::Char('t' | 'T') => { confirmation = Some(Action::Restart); None },
                _ => None,
            }
        };
        if let Some(action) = action {
            let tx = tx.clone();
            let service = services[selected];
            std::thread::Builder::new().name("cutex-service-control".into()).spawn(move || {
                let result = service_control::run(service, action).unwrap_or_else(|error| format!("{error:#}"));
                let _ = tx.send(result);
            })?;
            busy = true;
        }
    }
}
