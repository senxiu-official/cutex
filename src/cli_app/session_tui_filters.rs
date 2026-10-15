//! One-line list facets. Text search never interprets @ or # as syntax.
use super::{session_tui_input as input, session_tui_layout as theme, session_tui_view as views};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::Line,
    widgets::{Block, Clear, Paragraph},
    Frame,
};
use tui_input::Input;
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Choice {
    pub key: String,
    pub label: String,
}
#[derive(Debug, Clone, Default)]
pub(super) struct Facets {
    pub field: usize,
    pub project: Option<Choice>,
    pub host: Option<Choice>,
    pub picker: Option<Picker>,
}
#[derive(Debug, Clone)]
pub(super) struct Picker {
    pub query: Input,
    choices: Vec<Choice>,
    selected: usize,
}
pub(super) fn project_key(host: &str, project: &str) -> String {
    format!("{}\n{project}", host.to_lowercase())
}
impl Facets {
    pub fn matches(&self, host: &str, project: Option<&str>) -> bool {
        self.host
            .as_ref()
            .is_none_or(|h| h.key.eq_ignore_ascii_case(host))
            && self
                .project
                .as_ref()
                .is_none_or(|p| project.is_some_and(|id| p.key == project_key(host, id)))
    }
    pub fn open(&mut self, mut choices: Vec<Choice>) {
        choices.sort_by_key(|c| (c.label.to_lowercase(), c.key.clone()));
        choices.dedup_by(|a, b| a.key == b.key);
        choices.insert(
            0,
            Choice {
                key: String::new(),
                label: "All".into(),
            },
        );
        let current = if self.field == 1 {
            &self.project
        } else {
            &self.host
        };
        let selected = current
            .as_ref()
            .and_then(|c| choices.iter().position(|x| x.key == c.key))
            .unwrap_or(0);
        self.picker = Some(Picker {
            query: Input::default(),
            choices,
            selected,
        });
    }
    /// All keys are consumed while the popup is open. Esc leaves applied facets intact.
    pub fn picker_key(&mut self, key: KeyEvent) -> bool {
        let Some(p) = self.picker.as_mut() else {
            return false;
        };
        let visible = p.visible();
        match key.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Up => p.selected = p.selected.saturating_sub(1),
            KeyCode::Down => p.selected = (p.selected + 1).min(visible.len().saturating_sub(1)),
            KeyCode::Enter => {
                if let Some(c) = visible.get(p.selected).cloned() {
                    let value = (!c.key.is_empty()).then_some(c);
                    if self.field == 1 {
                        self.project = value;
                    } else {
                        self.host = value;
                        if self.project.as_ref().is_some_and(|p| {
                            self.host.as_ref().is_some_and(|h| {
                                !p.key.starts_with(&format!("{}\n", h.key.to_lowercase()))
                            })
                        }) {
                            self.project = None;
                        }
                    }
                }
                self.picker = None;
            }
            _ => {
                if input::edit(&mut p.query, key) {
                    p.selected = 0;
                }
            }
        }
        true
    }
    pub fn paste(&mut self, text: &str) -> bool {
        if let Some(p) = &mut self.picker {
            input::paste(&mut p.query, text);
            p.selected = 0;
            true
        } else {
            false
        }
    }
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect, query: &Input, focused: bool) {
        let inner = if area.height >= 3 {
            let b = Block::bordered().title(" Filter [/] ");
            let r = b.inner(area);
            frame.render_widget(b, area);
            input::refresh_title(frame, area, " Filter [/] ", focused);
            r
        } else {
            area
        };
        let parts = Layout::horizontal([
            Constraint::Percentage(48),
            Constraint::Percentage(32),
            Constraint::Percentage(20),
        ])
        .split(inner);
        let short = inner.width < 65;
        let labels = [
            if short { "" } else { "Search: " },
            if short { "/ P: " } else { "/ Project: " },
            if short { "/ H: " } else { "/ Host: " },
        ];
        for i in 0..3 {
            let value = match i {
                0 => query.value(),
                1 => self
                    .project
                    .as_ref()
                    .map(|c| c.label.as_str())
                    .unwrap_or("All"),
                _ => self
                    .host
                    .as_ref()
                    .map(|c| c.label.as_str())
                    .unwrap_or("All"),
            };
            let text = format!("{}{value}{}", labels[i], if i > 0 { " ▾" } else { "" });
            frame.render_widget(
                Paragraph::new(views::clipped(&text, parts[i].width as usize)).style(
                    Style::new().fg(if focused && self.field == i {
                        theme::focus()
                    } else {
                        ratatui::style::Color::White
                    }),
                ),
                parts[i],
            );
        }
        if focused && self.field == 0 {
            let prefix = labels[0].len() as u16;
            let r = Rect {
                x: parts[0].x + prefix,
                width: parts[0].width.saturating_sub(prefix),
                height: 1,
                ..parts[0]
            };
            input::render_input(frame, r, query, "", true);
        }
    }
    pub fn render_picker(&self, frame: &mut Frame<'_>) {
        let Some(p) = &self.picker else { return };
        let a = frame.area();
        let w = a.width.min(76);
        let h = a.height.min(18);
        let area = Rect::new(a.x + (a.width - w) / 2, a.y + (a.height - h) / 2, w, h);
        frame.render_widget(Clear, area);
        let b = Block::bordered().title(if self.field == 1 {
            " Project · search / Enter select / Esc cancel "
        } else {
            " Host · search / Enter select / Esc cancel "
        });
        let inner = b.inner(area);
        frame.render_widget(b, area);
        let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(inner);
        input::render_input(frame, chunks[0], &p.query, "", true);
        let visible = p.visible();
        let start = p
            .selected
            .saturating_sub(chunks[1].height.saturating_sub(1) as usize);
        let lines = visible
            .iter()
            .enumerate()
            .skip(start)
            .map(|(i, c)| {
                Line::styled(
                    format!("{}{}", if i == p.selected { "> " } else { "  " }, c.label),
                    Style::new().fg(if i == p.selected {
                        theme::focus()
                    } else {
                        ratatui::style::Color::White
                    }),
                )
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(if lines.is_empty() {
                vec![Line::from("No matches")]
            } else {
                lines
            }),
            chunks[1],
        );
    }
}
impl Picker {
    fn visible(&self) -> Vec<Choice> {
        let q = self.query.value().to_lowercase();
        self.choices
            .iter()
            .filter(|c| c.label.to_lowercase().contains(&q))
            .cloned()
            .collect()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    #[test]
    fn project_identity_includes_host() {
        let f = Facets {
            project: Some(Choice {
                key: project_key("eva", "p"),
                label: "IFM".into(),
            }),
            ..Default::default()
        };
        assert!(f.matches("EVA", Some("p")));
        assert!(!f.matches("devbox", Some("p")));
    }
    #[test]
    fn cancel_preserves_selection_and_host_switch_clears_incompatible_project() {
        let mut f = Facets {
            field: 2,
            project: Some(Choice {
                key: project_key("eva", "p"),
                label: "IFM".into(),
            }),
            ..Default::default()
        };
        f.open(vec![Choice {
            key: "devbox".into(),
            label: "devbox".into(),
        }]);
        f.picker_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(f.project.is_some());
        f.open(vec![Choice {
            key: "devbox".into(),
            label: "devbox".into(),
        }]);
        f.picker_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        f.picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(f.project.is_none());
        assert_eq!(f.host.unwrap().key, "devbox");
    }
}

#[cfg(test)]
mod visual_tests {
    use super::*;
    #[test]
    fn one_line_facets_and_searchable_popup_fit_narrow_and_wide_windows() {
        for width in [12, 30, 60, 80, 130, 220] {
            let mut term =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 24)).unwrap();
            let mut f = Facets {
                field: 1,
                ..Default::default()
            };
            let query = Input::new("标题 @name #literal".into());
            term.draw(|frame| f.render(frame, Rect::new(0, 0, width, 3), &query, true))
                .unwrap();
            f.open(
                (0..100)
                    .map(|i| Choice {
                        key: format!("{i}"),
                        label: format!("项目 {i} · WINBOX"),
                    })
                    .collect(),
            );
            f.paste("项目 99");
            assert_eq!(f.picker.as_ref().unwrap().visible().len(), 1);
            term.draw(|frame| {
                f.render(frame, Rect::new(0, 0, width, 3), &query, true);
                f.render_picker(frame)
            })
            .unwrap();
            f.picker_key(KeyEvent::new(
                KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ));
            assert_eq!(f.project.as_ref().unwrap().key, "99");
        }
    }
}
