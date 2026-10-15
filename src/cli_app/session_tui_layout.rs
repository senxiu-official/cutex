//! Shared shell geometry and colors for every primary panel.
use super::session_tui_workspace::PrimaryPanel;
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

// Theme overrides are loaded once per Cutex process; absent keys retain defaults.
static THEME: std::sync::LazyLock<std::collections::BTreeMap<String, String>> = std::sync::LazyLock::new(|| {
    let value = (|| -> anyhow::Result<_> {
        let path = cutex::config::paths::config_dir()?.join("theme.json");
        if !path.exists() { return Ok(std::collections::BTreeMap::new()); }
        anyhow::ensure!(std::fs::metadata(&path)?.len() <= 8192, "theme.json exceeds 8 KiB");
        Ok(serde_json::from_slice(&std::fs::read(path)?)?)
    })();
    value.unwrap_or_default()
});
fn theme_color(key: &str, fallback: Color) -> Color {
    THEME.get(key).and_then(|value| {
        let hex = value.strip_prefix('#')?;
        if hex.len() != 6 { return None; }
        u32::from_str_radix(hex, 16).ok().map(|rgb| Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8))
    }).unwrap_or(fallback)
}
pub(super) fn brand() -> Color { theme_color("brand", Color::Rgb(247, 179, 205)) }
pub(super) fn accent() -> Color { theme_color("accent", Color::Rgb(224, 142, 178)) }
pub(super) fn text() -> Color { theme_color("text", Color::White) }
pub(super) fn muted() -> Color { theme_color("muted", Color::DarkGray) }
pub(super) fn focus() -> Color { theme_color("focus", Color::Rgb(116, 186, 195)) }
pub(super) fn selection() -> Color { theme_color("selection", Color::Rgb(38, 52, 79)) }
pub(super) fn success() -> Color { theme_color("success", Color::Green) }
pub(super) fn warning() -> Color { theme_color("warning", Color::Rgb(217, 180, 95)) }
pub(super) fn error() -> Color { theme_color("error", Color::Red) }
pub(super) fn status_online() -> Color { accent() }
pub(super) fn status_stale() -> Color { focus() }
pub(super) fn status_offline() -> Color { muted() }
pub(super) fn status_unknown() -> Color { warning() }
pub(super) fn footer_description() -> Color { text() }

pub(super) fn runtime_status_color(status: &str) -> Color {
    match status.to_ascii_lowercase().as_str() {
        "online" => status_online(),
        "stale" => status_stale(),
        "offline" | "retired" => status_offline(),
        "managed" | "unmanaged" => text(),
        _ => status_unknown(),
    }
}

/// Keep the list readable in normal windows, but give all growth beyond its
/// useful column width to details. Includes borders and the selection marker.
pub(super) const LIST_PANE_MAX_WIDTH: u16 = 130;

pub(super) fn inspector_panes(area: Rect, visible: bool) -> Option<(Rect, Rect)> {
    if !visible || area.width < 115 {
        return None;
    }
    let available = area.width - 1;
    let left = ((u32::from(available) * 62 / 100) as u16)
        .max(74)
        .min(LIST_PANE_MAX_WIDTH)
        .min(available - 40);
    let right = available - left;
    Some((
        Rect {
            width: left,
            ..area
        },
        Rect {
            x: area.x + left + 1,
            width: right,
            ..area
        },
    ))
}

/// Shared geometry: split horizontally first, then put the filter above the list.
pub(super) struct ListDetailsLayout {
    pub filter: Rect,
    pub list: Rect,
    pub details: Option<Rect>,
}

pub(super) fn list_details(area: Rect, visible: bool) -> ListDetailsLayout {
    let (left, details) =
        inspector_panes(area, visible).map_or((area, None), |(left, right)| (left, Some(right)));
    let [filter, list] = ratatui::layout::Layout::vertical([
        ratatui::layout::Constraint::Length(if left.height < 7 { 1 } else { 3 }),
        ratatui::layout::Constraint::Min(1),
    ])
    .areas(left);
    ListDetailsLayout {
        filter,
        list,
        details,
    }
}

/// Explicit styles prevent stale colors when a shorter heading replaces another.
pub(super) fn heading(prefix: &str, title: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            prefix.to_owned(),
            Style::new().fg(focus()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {title}"),
            Style::new().fg(text()).add_modifier(Modifier::BOLD),
        ),
    ])
}

pub(super) fn tabs(active: PrimaryPanel, width: u16) -> Line<'static> {
    let mut spans = vec![
        Span::styled("CUTEX", Style::new().fg(brand()).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
    ];
    if width < 60 {
        spans.push(Span::styled(
            active.label().to_owned(),
            Style::new().fg(text()).add_modifier(Modifier::BOLD),
        ));
    } else {
        for panel in PrimaryPanel::ALL {
            let label = if panel == PrimaryPanel::Projects {
                "Projects"
            } else {
                panel.label()
            };
            spans.push(Span::styled(
                format!(" {label} "),
                if panel == active {
                    Style::new()
                        .fg(text())
                        .bg(selection())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(muted())
                },
            ));
        }
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_stay_in_the_left_column_and_details_span_full_body() {
        for width in [80, 120, 180, 280] {
            let area = Rect::new(0, 2, width, 25);
            let panes = list_details(area, true);
            assert_eq!(panes.filter.x, panes.list.x);
            assert_eq!(panes.filter.width, panes.list.width);
            assert_eq!(panes.filter.bottom(), panes.list.y);
            assert_eq!(panes.list.bottom(), area.bottom());
            if let Some(details) = panes.details {
                assert_eq!(details.y, panes.filter.y);
                assert_eq!(details.bottom(), panes.list.bottom());
                assert_eq!(panes.list.right() + 1, details.x);
                assert!(panes.list.width <= LIST_PANE_MAX_WIDTH);
            } else {
                assert_eq!(width, 80);
            }
        }
    }

    #[test]
    fn wide_windows_give_all_extra_space_to_details() {
        let (list, detail) = inspector_panes(Rect::new(3, 2, 240, 30), true).unwrap();
        let (wider_list, wider_detail) = inspector_panes(Rect::new(3, 2, 320, 30), true).unwrap();
        assert_eq!(list.width, LIST_PANE_MAX_WIDTH);
        assert_eq!(wider_list, list);
        assert_eq!(wider_detail.x, detail.x);
        assert_eq!(wider_detail.width, detail.width + 80);
        assert_eq!(wider_detail.right(), 323);
    }

    #[test]
    fn split_preserves_minimums_gap_and_narrow_fallback() {
        for width in 0..=500 {
            let area = Rect::new(3, 2, width, 20);
            assert!(inspector_panes(area, false).is_none());
            if width < 115 {
                assert!(inspector_panes(area, true).is_none());
            } else {
                let (list, detail) = inspector_panes(area, true).unwrap();
                assert!(list.width >= 74 && list.width <= LIST_PANE_MAX_WIDTH);
                assert!(detail.width >= 40);
                assert_eq!(list.right() + 1, detail.x);
                assert_eq!(detail.right(), area.right());
            }
        }
    }
}
