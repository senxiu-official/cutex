//! Shared input/command policy for Managed, Recent and Cutex Projects.
use super::session_tui_workspace::PrimaryPanel;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::Rect,
    text::Line,
    widgets::{Block, Clear, Paragraph},
    Frame,
};
use tui_input::{Input, InputRequest};

pub(super) fn edit(input: &mut Input, key: KeyEvent) -> bool {
    if key.kind == KeyEventKind::Release {
        return true;
    }
    let request = match key.code {
        KeyCode::Char('u' | 'U') if key.modifiers == KeyModifiers::CONTROL => {
            Some(InputRequest::DeleteLine)
        }
        KeyCode::Char('x' | 'X') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
        _ if key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
        {
            return false
        }
        KeyCode::Left => Some(InputRequest::GoToPrevChar),
        KeyCode::Right => Some(InputRequest::GoToNextChar),
        KeyCode::Home => Some(InputRequest::GoToStart),
        KeyCode::End => Some(InputRequest::GoToEnd),
        KeyCode::Backspace => Some(InputRequest::DeletePrevChar),
        KeyCode::Delete => Some(InputRequest::DeleteNextChar),
        KeyCode::Char(c) if !c.is_control() => Some(InputRequest::InsertChar(c)),
        _ => None,
    };
    if let Some(request) = request {
        input.handle(request);
        true
    } else {
        false
    }
}

pub(super) fn paste(input: &mut Input, text: &str) {
    for c in text.chars().filter(|c| !c.is_control()) {
        input.handle(InputRequest::InsertChar(c));
    }
}

// Legacy form strings remain the sole draft values; only their cursor is stored
// separately. All editing is delegated to the same locked tui-input API.
pub(super) fn string_input(value: &str, cursor: Option<usize>) -> Input {
    let input = Input::new(value.to_owned());
    if let Some(cursor) = cursor {
        input.with_cursor(cursor)
    } else {
        input
    }
}
pub(super) fn edit_string(value: &mut String, cursor: &mut Option<usize>, key: KeyEvent) -> bool {
    let mut input = string_input(value, *cursor);
    let handled = edit(&mut input, key);
    *value = input.value().to_owned();
    *cursor = Some(input.cursor());
    handled
}
pub(super) fn paste_string(value: &mut String, cursor: &mut Option<usize>, text: &str) {
    let mut input = string_input(value, *cursor);
    paste(&mut input, text);
    *value = input.value().to_owned();
    *cursor = Some(input.cursor());
}
pub(super) fn title_style(focused: bool) -> ratatui::style::Style {
    let color = if focused { crate::cli_app::session_tui_layout::focus() } else { crate::cli_app::session_tui_layout::muted() };
    // Explicit RGB avoids terminal bold/intensity remapping of ANSI gray.
    let color = match color { ratatui::style::Color::DarkGray => ratatui::style::Color::Rgb(128,128,128), other => other };
    ratatui::style::Style::reset().fg(color)
}

pub(super) fn refresh_title(frame: &mut Frame<'_>, area: Rect, title: &str, focused: bool) {
    if area.width < 3 || area.height == 0 { return; }
    let width = (Line::from(title).width() as u16).min(area.width - 2);
    let rect = Rect::new(area.x + 1, area.y, width, 1);
    frame.render_widget(Paragraph::new(title).style(title_style(focused)), rect);
    use ratatui::buffer::CellWidth;
    let mut x = rect.x;
    while x < rect.right() {
        let cell = &mut frame.buffer_mut()[(x, rect.y)];
        cell.set_diff_option(ratatui::buffer::CellDiffOption::AlwaysUpdate);
        x += cell.cell_width().max(1);
    }
}

pub(super) fn render_input(
    frame: &mut Frame<'_>,
    area: Rect,
    input: &Input,
    title: &str,
    focused: bool,
) {
    if area.height < 3 {
        let width = usize::from(area.width);
        let scroll = input.visual_scroll(width.saturating_sub(1).max(1));
        frame.render_widget(
            Paragraph::new(input.value()).scroll((0, scroll.min(u16::MAX as usize) as u16)),
            area,
        );
        if focused && width > 0 && area.height > 0 {
            frame.set_cursor_position((
                area.x + input.visual_cursor().saturating_sub(scroll).min(width - 1) as u16,
                area.y,
            ));
        }
        return;
    }
    let width = area.width.saturating_sub(2) as usize;
    let scroll = input.visual_scroll(width.saturating_sub(1).max(1));
    frame.render_widget(
        Paragraph::new(input.value())
            .scroll((0, scroll.min(u16::MAX as usize) as u16))
            .block(
                Block::bordered()
                    .title(ratatui::text::Line::styled(title, ratatui::style::Style::reset().fg(if focused {
                        crate::cli_app::session_tui_layout::focus()
                    } else {
                        crate::cli_app::session_tui_layout::muted()
                    })))
                    .border_style(ratatui::style::Style::new().fg(if focused {
                        crate::cli_app::session_tui_layout::focus()
                    } else {
                        crate::cli_app::session_tui_layout::muted()
                    })),
            ),
        area,
    );
    refresh_title(frame, area, title, focused);
    if focused && width > 0 && area.height >= 3 {
        frame.set_cursor_position((
            area.x + 1 + input.visual_cursor().saturating_sub(scroll).min(width - 1) as u16,
            area.y + 1,
        ));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Command {
    Details,
    Page(PrimaryPanel),
    Settings,
    Actions,
    Inspect,
    Edit,
    NewProject,
    NewManagedAgent,
    LoadMore,
    Scope,
    Sort,
    Titles,
    Refresh,
    Exit,
    Back,
    Help,
    Archived,
    Profiles,
    Hosts,
    Services,
    Workspaces,
    Archive,
    Appearance,
}
#[derive(Clone, Copy)]
pub(super) struct Binding {
    pub command: Command,
    pub key: KeyCode,
    pub modifiers: KeyModifiers,
    pub label: &'static str,
    pub hint: &'static str,
}
const fn alt(command: Command, key: char, label: &'static str, hint: &'static str) -> Binding {
    Binding {
        command,
        key: KeyCode::Char(key),
        modifiers: KeyModifiers::ALT,
        label,
        hint,
    }
}
pub(super) const BINDINGS: &[Binding] = &[
    Binding {
        command: Command::Details,
        key: KeyCode::F(2),
        modifiers: KeyModifiers::NONE,
        label: "Details",
        hint: "F2",
    },
    Binding {
        command: Command::Archived,
        key: KeyCode::Char('h'),
        modifiers: KeyModifiers::CONTROL,
        label: "Include archived Projects / members",
        hint: "Ctrl+H",
    },
    Binding {
        command: Command::Help,
        key: KeyCode::F(1),
        modifiers: KeyModifiers::NONE,
        label: "Commands",
        hint: "F1",
    },
    alt(Command::Profiles, 'g', "Profiles", "Alt+G"),
    alt(
        Command::Workspaces,
        'w',
        "Workspaces (Native catalog)",
        "Alt+W",
    ),
    alt(Command::Archive, 'z',
        "Archived / Retired Agents and Sessions", "Alt+Z",
    ),
    alt(
        Command::Appearance,
        'b',
        "Appearance: toggle Inspector",
        "Alt+B",
    ),
    alt(Command::Page(PrimaryPanel::Agents), '1', "Agents", "Alt+1"),
    alt(Command::Page(PrimaryPanel::Recent), '2', "Sessions", "Alt+2",
    ),
    alt(Command::Page(PrimaryPanel::Projects), '3', "Projects", "Alt+3",
    ),
    alt(Command::Page(PrimaryPanel::Alarms), '4', "Alarms", "Alt+4"),
    alt(Command::Page(PrimaryPanel::Jobs), '5', "Jobs", "Alt+5"),
    alt(Command::Page(PrimaryPanel::Profiles), '6', "Profiles", "Alt+6"),
    alt(Command::Settings, '7', "Settings", "Alt+7"),
    alt(Command::Actions, 'a', "Object actions", "Alt+A"),
    alt(Command::Inspect, 'i', "Inspect", "Alt+I"),
    alt(Command::Edit, 'e', "Edit object", "Alt+E"),
    alt(Command::NewManagedAgent, 'm', "New managed Agent", "Alt+M"),
    alt(Command::NewProject, 'n', "New", "Alt+N"),
    alt(Command::LoadMore, 'l', "Load more recent rows", "Alt+L"),
    alt(
        Command::Scope,
        'o',
        "Managed scope: All / Online / Pinned",
        "Alt+O",
    ),
    alt(Command::Sort, 's', "Cycle list sort", "Alt+S"),
    alt(Command::Titles, 'v', "Toggle thread titles", "Alt+V"),
    Binding {
        command: Command::Refresh,
        key: KeyCode::F(5),
        modifiers: KeyModifiers::NONE,
        label: "Refresh",
        hint: "F5",
    },
    Binding {
        command: Command::Exit,
        key: KeyCode::Char('c'),
        modifiers: KeyModifiers::CONTROL,
        label: "Exit",
        hint: "Ctrl+C",
    },
];
pub(super) fn footer(entries: &[(Command, Option<&'static str>)]) -> Line<'static> {
    let mut spans = Vec::new();
    for binding in entries
        .iter()
        .filter(|(_, reason)| reason.is_none())
        .filter_map(|(c, _)| BINDINGS.iter().find(|b| b.command == *c))
    {
        if !spans.is_empty() {
            spans.push(ratatui::text::Span::raw(" · "));
        }
        spans.push(ratatui::text::Span::styled(
            binding.hint,
            ratatui::style::Style::new()
                .fg(crate::cli_app::session_tui_layout::focus())
                .add_modifier(ratatui::style::Modifier::BOLD),
        ));
        spans.push(ratatui::text::Span::raw(format!(" {}", binding.label)));
    }
    Line::from(spans)
}
pub(super) fn resolve(key: KeyEvent) -> Option<Command> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let code = match key.code {
        KeyCode::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
        other => other,
    };
    BINDINGS
        .iter()
        .find(|b| b.key == code && b.modifiers == key.modifiers)
        .map(|b| b.command)
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Gate {
    Allow,
    Block,
    Review,
}
pub(super) fn navigation_gate(busy: bool, modal: bool, dirty: bool) -> Gate {
    if busy || modal {
        Gate::Block
    } else if dirty {
        Gate::Review
    } else {
        Gate::Allow
    }
}
#[derive(Debug, Clone)]
pub(super) struct LeaveReview {
    pub command: Command,
    pub selected: usize,
    pub can_save: bool,
}
impl LeaveReview {
    pub fn new(command: Command, can_save: bool) -> Self {
        Self {
            command,
            selected: 0,
            can_save,
        }
    }
    pub fn labels(&self) -> Vec<&'static str> {
        let mut v = vec!["Keep editing", "Discard and leave"];
        if self.can_save {
            v.push("Save");
        }
        v
    }
    pub fn handle(&mut self, key: KeyEvent) -> Option<usize> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        let len = self.labels().len();
        match key.code {
            KeyCode::Esc => Some(0),
            KeyCode::Enter => Some(self.selected),
            KeyCode::Right | KeyCode::Down | KeyCode::Tab => {
                self.selected = (self.selected + 1) % len;
                None
            }
            KeyCode::Left | KeyCode::Up | KeyCode::BackTab => {
                self.selected = (self.selected + len - 1) % len;
                None
            }
            _ => None,
        }
    }
    pub fn render(&self, frame: &mut Frame<'_>) {
        use crate::cli_app::session_tui_layout as theme;
        use ratatui::style::{Modifier, Style};
        use ratatui::text::Span;
        use ratatui::widgets::Wrap;
        let screen = frame.area();
        let width = screen.width.min(64);
        let height = screen.height.min(10);
        let area = Rect::new(
            screen.x + screen.width.saturating_sub(width) / 2,
            screen.y + screen.height.saturating_sub(height) / 2,
            width,
            height,
        );
        let block = Block::bordered()
            .title(" Unsaved changes ")
            .border_style(Style::new().fg(theme::focus()));
        let inner = block.inner(area);
        frame.render_widget(Clear, area);
        frame.render_widget(block, area);
        let mut lines = vec![
            Line::from("Leave this page with unsaved changes?"),
            Line::default(),
        ];
        for (index, label) in self.labels().iter().enumerate() {
            let style = if index == self.selected {
                Style::new()
                    .fg(theme::text())
                    .bg(theme::selection())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme::text())
            };
            lines.push(Line::from(Span::styled(
                format!("{} {label}", if index == self.selected { ">" } else { " " }),
                style,
            )));
        }
        lines.push(Line::default());
        lines.push(Line::styled(
            "←/→ or Tab choose · Enter confirm · Esc keep editing",
            Style::new().fg(theme::muted()),
        ));
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: true })
                .style(Style::new().fg(theme::text())),
            inner,
        );
    }
}
#[derive(Debug, Clone, Default)]
pub(super) struct Help {
    pub selected: usize,
}
impl Help {
    pub fn handle(
        &mut self,
        key: KeyEvent,
        entries: &[(Command, Option<&'static str>)],
    ) -> Option<Option<Command>> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        match key.code {
            KeyCode::Esc if key.kind == KeyEventKind::Press => Some(None),
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                None
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(entries.len().saturating_sub(1));
                None
            }
            KeyCode::Enter if key.kind == KeyEventKind::Press => entries
                .get(self.selected)
                .and_then(|(c, reason)| reason.is_none().then_some(Some(*c))),
            _ => None,
        }
    }
    pub fn render(&self, frame: &mut Frame<'_>, entries: &[(Command, Option<&'static str>)]) {
        self.render_titled(frame, entries, " F1 commands · arrows / Enter · Esc ");
    }
    pub fn render_titled(
        &self,
        frame: &mut Frame<'_>,
        entries: &[(Command, Option<&'static str>)],
        title: &str,
    ) {
        overlay(
            frame,
            title,
            entries
                .iter()
                .enumerate()
                .map(|(i, (c, reason))| {
                    let b = BINDINGS.iter().find(|b| b.command == *c).unwrap();
                    format!(
                        "{} {} {}{}",
                        if i == self.selected { ">" } else { " " },
                        b.hint,
                        b.label,
                        reason.map(|s| format!(" — {s}")).unwrap_or_default()
                    )
                })
                .collect(),
        );
    }
}
fn overlay(frame: &mut Frame<'_>, title: &str, lines: Vec<String>) {
    let area = frame.area();
    let first = lines
        .iter()
        .position(|line| line.starts_with('>'))
        .unwrap_or(0)
        .saturating_sub(area.height.saturating_sub(3) as usize);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(
            lines
                .into_iter()
                .skip(first)
                .map(Line::from)
                .collect::<Vec<_>>(),
        )
        .block(Block::bordered().title(title)),
        area,
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn filter_title_resets_underlying_color_and_modifiers() {
        use ratatui::{backend::TestBackend, style::{Color, Modifier, Style}, Terminal};
        let mut terminal = Terminal::new(TestBackend::new(50, 3)).unwrap();
        terminal.draw(|frame| {
            frame.render_widget(Paragraph::new("underlying panel title, including a pale suffix")
                .style(Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD)), frame.area());
            render_input(frame, frame.area(), &Input::default(), " Filter alarms · active [/] ", false);
        }).unwrap();
        for x in 1..27 {
            let cell = &terminal.backend().buffer()[(x, 0)];
            assert_eq!(Some(cell.fg), title_style(false).fg);
            assert!(!cell.modifier.contains(Modifier::BOLD));
        }
    }

    use super::*;
    #[test]
    fn visual_restoration_shortcuts_have_semantic_style_and_same_commands() {
        use ratatui::style::Modifier;
        let line = footer(&[(Command::Help, None), (Command::Settings, None)]);
        assert!(line.spans.iter().any(|s| s.content == "F1"
            && s.style.fg == Some(crate::cli_app::session_tui_layout::focus())
            && s.style.add_modifier.contains(Modifier::BOLD)));
        // Settings moved from Alt+6 to Alt+7 when 85156fb added the Profiles panel.
        assert!(line.to_string().contains("Alt+7"));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 1)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(ratatui::widgets::Paragraph::new(line.clone()), frame.area())
            })
            .unwrap();
        let cell = &terminal.backend().buffer()[(0, 0)];
        assert_eq!(cell.symbol(), "F");
        assert_eq!(cell.fg, crate::cli_app::session_tui_layout::focus());
        assert!(cell.modifier.contains(Modifier::BOLD));
        assert_eq!(
            resolve(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)),
            Some(Command::Help)
        );
    }
    #[test]
    fn ui_contract_b1_bindings_help_and_repeat_use_one_source() {
        for binding in BINDINGS {
            assert_eq!(
                resolve(KeyEvent::new(binding.key, binding.modifiers)),
                Some(binding.command)
            );
            for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
                assert_eq!(
                    resolve(KeyEvent::new_with_kind(
                        binding.key,
                        binding.modifiers,
                        kind
                    )),
                    None
                );
            }
            assert!(footer(&[(binding.command, None)])
                .to_string()
                .contains(binding.hint));
        }
        let mut help = Help::default();
        let entries = [(Command::NewProject, Some("No Director candidate"))];
        assert_eq!(
            help.handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &entries),
            None
        );
        assert_eq!(navigation_gate(true, false, true), Gate::Block);
        assert_eq!(navigation_gate(false, true, false), Gate::Block);
        assert_eq!(navigation_gate(false, false, true), Gate::Review);
    }
}
