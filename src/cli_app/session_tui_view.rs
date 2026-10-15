//! Ephemeral, read-only list projection. Never a persisted identity or authority.
use cutex::agent_management::{
    CutexProjectWorkspace, ProjectMemberProjection, ProjectPaletteColor,
};
use ratatui::{
    layout::{Constraint, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Cell, Clear, Paragraph, Row, Table, TableState},
    Frame,
};
use unicode_width::UnicodeWidthStr;

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn visual_row() -> AgentSessionView {
        AgentSessionView {
            remote_provider: None, remote_profile_style: None,
            host: String::new(),
            badge: Some(ProjectBadge {
                label: "CX".into(),
                color: ProjectPaletteColor::Rgb(240, 220, 90),
            }),
            subject: SubjectRef::Managed("cutex.018e0000-0000-7000-8000-0001d000eeb9".into()),
            project_id: Some("cutex-platform".into()),
            name: "cutex.018e0000-0000-7000-8000-0001d000eeb9".into(),
            native_title: Some("查询 pat 的 ifm+gears · 中文e\u{301}👩‍💻".into()),
            native_thread: Some("018e0000-0000-7000-8000-0001d000eeb9".into()),
            native_workspace: None,
            compaction_count: None,
            runtime: Observation::Known("Online".into()),
            runtime_version: None,
            runtime_version_latest: None,
            runtime_version_recorded_at: None,
            project: Observation::Known("Cutex Platform".into()),
            configured_profile: None,
            effective_profile: Observation::Known("orchid".into()),
            role: "Director".into(),
            activity: "2026-08-12".into(),
            activity_details: Some("OUT · 2026-08-12T12:00:00Z".into()),
            updated: "—".into(),
            cwd: "/private/Projects/IFM/agent-home/ifm-ema-figures/中文/e\u{301}/👩‍💻/UNIQUE-END"
                .into(),
            retirement_note: None,
        }
    }
    fn text(buffer: &ratatui::buffer::Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                let mut line = String::new();
                let mut x = 0;
                while x < buffer.area.width {
                    let symbol = buffer[(x, y)].symbol();
                    line.push_str(symbol);
                    x += symbol.width().max(1) as u16;
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn capture(buffer: &ratatui::buffer::Buffer, name: &str) {
        let Some(root) = std::env::var_os("CUTEX_UI_CAPTURE_DIR") else {
            return;
        };
        let root = std::path::Path::new(&root);
        std::fs::write(root.join(format!("{name}.txt")), text(buffer)).unwrap();
        let cells:Vec<_>=buffer.content.iter().map(|c|serde_json::json!({"text":c.symbol(),"fg":format!("{:?}",c.fg),"bg":format!("{:?}",c.bg),"modifiers":format!("{:?}",c.modifier)})).collect();
        std::fs::write(root.join(format!("{name}.json")),serde_json::to_vec(&serde_json::json!({"width":buffer.area.width,"height":buffer.area.height,"cells":cells})).unwrap()).unwrap();
    }
    #[test]
    fn visual_restoration_badge_profile_status_and_width_cells() {
        let mut rows = vec![visual_row(); 3];
        rows[1].badge = None;
        rows[1].name = "Unassigned".into();
        rows[1].project = Observation::Known("-".into());
        rows[1].configured_profile = Some("explicit".into());
        rows[1].runtime = Observation::Known("Offline".into());
        rows[2].runtime = Observation::Unavailable("provider missing".into());
        rows[2].effective_profile = Observation::Unavailable("not observed".into());
        for width in [60, 80, 100, 120, 160, 204, 240] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 18)).unwrap();
            let mut state = TableState::default().with_selected(Some(0));
            terminal
                .draw(|f| render_table(f, f.area(), &rows, ListKind::Managed, &mut state))
                .unwrap();
            let buffer = terminal.backend().buffer();
            assert_eq!(buffer[(4, 2)].symbol(), "C");
            assert_eq!(buffer[(4, 2)].bg, Color::Rgb(240, 220, 90));
            assert_eq!(buffer[(4, 2)].fg, Color::Black);
            assert_eq!(buffer[(8, 2)].symbol(), "c");
            assert_eq!(buffer[(8, 3)].symbol(), "U", "aligned empty badge slot");
            assert_eq!(buffer[(8, 2)].bg, crate::cli_app::session_tui_layout::selection());
            let mut columns = visible_columns(width - 4, ListKind::Managed);
            columns.retain(|(c,_)|*c!=Column::Host);
            assert!(columns[0].1 <= 47);
            let mut x = 3;
            for (column, w) in columns {
                if column == Column::Status {
                    assert_eq!(buffer[(x, 2)].fg, crate::cli_app::session_tui_layout::status_online());
                    assert_eq!(buffer[(x, 3)].fg, Color::DarkGray);
                    assert_eq!(buffer[(x, 4)].fg, crate::cli_app::session_tui_layout::status_unknown());
                }
                if column == Column::Profile {
                    assert_eq!(buffer[(x, 2)].symbol(), "~");
                    assert_eq!(buffer[(x, 2)].fg, profile_style(&rows[0]).fg.unwrap_or(Color::White));
                    assert_eq!(buffer[(x, 3)].fg, profile_style(&rows[1]).fg.unwrap_or(Color::White));
                    assert!(text(buffer).contains("~orchid"));
                    assert!(text(buffer).contains("~?"));
                }
                x += w + 1;
            }
            if width >= 108 {
                assert!(text(buffer).contains("2026-08-12"));
            }
            if width == 204 {
                capture(buffer, "managed-204");
            }
        }
    }
    #[test]
    fn visual_restoration_inspector_graphemes_borders_resize_and_sections() {
        let mut row = visual_row();
        row.cwd = format!("{}{}", row.cwd, "/中文e\u{301}👩‍💻".repeat(60));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(240, 50)).unwrap();
        let scroll = DetailScroll::default();
        for (width, height) in [
            (60, 18),
            (80, 24),
            (100, 30),
            (120, 36),
            (160, 48),
            (240, 50),
            (204, 50),
        ]
        .into_iter()
        .chain((70..=120).map(|w| (w, 24)))
        {
            terminal.backend_mut().resize(width, height);
            terminal.resize(Rect::new(0, 0, width, height)).unwrap();
            scroll.reset();
            terminal
                .draw(|f| render_inspector(f, f.area(), &row, &scroll))
                .unwrap();
            let buffer = terminal.backend().buffer();
            for y in 1..height - 1 {
                assert_eq!(buffer[(0, y)].symbol(), "│");
                assert_eq!(buffer[(width - 1, y)].symbol(), "│", "{width}:{y}");
            }
            let rendered = text(buffer);
            assert!(rendered.contains("Profile"));
            assert!(!rendered.contains("Managed("));
            assert!(wrap_details(inspector_lines(&row), usize::from(width - 2))
                .iter()
                .all(|l| l.width() <= usize::from(width - 2)));
            scroll.handle(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::End,
                crossterm::event::KeyModifiers::NONE,
            ));
            terminal
                .draw(|f| render_inspector(f, f.area(), &row, &scroll))
                .unwrap();
            assert!(text(terminal.backend().buffer()).contains("Native ID"));
            row.name = "Short name".into();
            scroll.reset();
            terminal
                .draw(|f| render_inspector(f, f.area(), &row, &scroll))
                .unwrap();
            let name_line: String = (1..width - 1)
                .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
                .collect();
            assert!(
                name_line.contains("Short name") && !name_line.contains("018e0000"),
                "no old name residue"
            );
        }
        let pieces = detail_lines("中文e\u{301}👩‍💻/path", 5);
        assert_eq!(pieces.concat(), "中文e\u{301}👩‍💻/path");
        assert!(pieces.iter().any(|p| p.contains("👩‍💻")));
        let row = visual_row();
        assert!(inspector_lines(&row).iter().any(
            |l| l.style.fg == Some(crate::cli_app::session_tui_layout::focus()) && l.spans.iter().any(|s| s.content == "Profile")
        ));
        if std::env::var_os("CUTEX_UI_CAPTURE_DIR").is_some() {
            terminal.backend_mut().resize(60, 30);
            terminal.resize(Rect::new(0, 0, 60, 30)).unwrap();
            scroll.reset();
            terminal
                .draw(|f| render_inspector(f, f.area(), &row, &scroll))
                .unwrap();
            capture(terminal.backend().buffer(), "inspector-60");
        }
    }
    #[test]
    fn ui_contract_e1_details_geometry_unicode_and_reachable_tail() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let text = format!(
            "Name: {}\nFull path: C:\\work\\{}\\UNIQUE-END\nError: final-error-tail",
            "中文e\u{301}👩‍💻 ".repeat(90),
            "long-segment".repeat(90)
        );
        for (width, height) in [
            (60, 18),
            (80, 24),
            (100, 30),
            (120, 36),
            (160, 48),
            (240, 50),
        ]
        .into_iter()
        .chain((70..=120).map(|w| (w, 18)))
        {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            let scroll = DetailScroll::default();
            terminal
                .draw(|f| render_details(f, f.area(), "Inspector", &text, &scroll))
                .unwrap();
            assert!(scroll.total.get() > 1);
            assert!(scroll.handle(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)));
            terminal
                .draw(|f| render_details(f, f.area(), "Inspector", &text, &scroll))
                .unwrap();
            let rendered = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            assert!(rendered.contains("final-error-tail"), "{width}x{height}");
            assert!(
                rendered.replace(['│', ' '], "").contains("UNIQUE-END"),
                "{width}x{height}"
            );
            assert!(scroll.handle(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)));
            assert_eq!(scroll.offset.get(), 0);
            for line in detail_lines(&text, (width - 2) as usize) {
                assert!(line.width() <= (width - 2) as usize);
            }
        }
        let raw = "中文 e\u{301} 👩‍💻 C:\\x\\identity";
        assert_eq!(detail_lines(raw, 5).concat(), raw);
        assert!(detail_lines("\u{1b}bad", 10).concat().contains("\\u{1b}"));
    }

    #[test]
    fn details_footer_reports_line_range_not_fake_page_count() {
        let draw = |width, height, body: &str, end: bool| {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            let scroll = DetailScroll::default();
            terminal
                .draw(|f| render_details(f, f.area(), "Details", body, &scroll))
                .unwrap();
            if end {
                scroll.handle(crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::End,
                    crossterm::event::KeyModifiers::NONE,
                ));
                terminal
                    .draw(|f| render_details(f, f.area(), "Details", body, &scroll))
                    .unwrap();
            }
            text(terminal.backend().buffer())
        };
        let all = draw(80, 12, "one\ntwo\nthree", false);
        assert!(all.contains("lines 1–3 of 3 · all visible"));
        assert!(!all.contains("1/3"));
        let overflow = draw(
            80,
            7,
            &(1..=20)
                .map(|n| format!("line {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
            true,
        );
        assert!(overflow.contains("lines 17–20 of 20"));
        assert!(overflow.contains("PgUp/Dn"));
        let narrow = draw(30, 7, "one\ntwo", false);
        assert!(narrow.contains("Esc · all 2 lines"));
        let empty = draw(30, 7, "", false);
        assert!(empty.contains("Esc · all 1 lines"));
    }

    #[test]
    fn inspector_shows_authoritative_compaction_count_only() {
        let mut row = visual_row();
        row.compaction_count = Some(3);
        let rendered = inspector_lines(&row)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(rendered.contains(&"Compactions: 3".to_string()));

        row.compaction_count = None;
        let rendered = inspector_lines(&row)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(!rendered.iter().any(|line| line.contains("Compactions:")));
    }
    #[test]
    fn ui_contract_c_v04_columns_fit_inner_width_without_zero_placeholders() {
        for width in 1..=240 {
            for kind in [ListKind::Managed, ListKind::Recent, ListKind::Members] {
                let columns = visible_columns(width, kind);
                assert_eq!(columns[0].0, Column::Name);
                assert!(columns.iter().all(|(_, w)| *w > 0));
                assert!(
                    columns.iter().map(|(_, w)| *w).sum::<u16>() + columns.len() as u16 - 1
                        <= width
                );
                if width >= 36 && kind == ListKind::Members {
                    assert!(columns.iter().any(|(c, _)| *c == Column::Role));
                }
            }
        }
    }
    #[test]
    fn ui_contract_c_v04_unicode_and_windows_tail_identity() {
        for width in 1..40 {
            let value = tail_path(r"C:\長い名前\shared\unique-final", width);
            assert!(value.width() <= width);
            assert!(clipped("界界 emoji 🦀 end", width).width() <= width);
        }
        assert!(tail_path(r"C:\same\different\unique-final", 18).ends_with("unique-final"));
        assert_ne!(
            SubjectRef::Managed("one".into()),
            SubjectRef::Managed("two".into())
        );
        assert_ne!(
            SubjectRef::Native {
                catalog: "a".into(),
                thread: "same".into()
            },
            SubjectRef::Native {
                catalog: "b".into(),
                thread: "same".into()
            }
        );
        assert_ne!(
            Observation::Known("Offline".into()),
            Observation::<String>::Unavailable("missing".into())
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum SubjectRef {
    Managed(String),
    Native { catalog: String, thread: String },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Observation<T> {
    Known(T),
    Unavailable(String),
    Stale(T, String),
}
impl<T> Observation<T> {
    pub fn known(&self) -> Option<&T> {
        match self {
            Self::Known(v) | Self::Stale(v, _) => Some(v),
            _ => None,
        }
    }
}
impl Observation<String> {
    pub fn label(&self) -> String {
        match self {
            Self::Known(v) => v.clone(),
            Self::Unavailable(_) => "Unavailable".into(),
            Self::Stale(v, _) => format!("{v} (stale)"),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AgentSessionView {
    pub remote_provider: Option<cutex::profiles::appearance::Provider>,
    pub remote_profile_style: Option<cutex::profiles::appearance::TextStyle>,
    pub host: String,
    pub badge: Option<ProjectBadge>,
    pub project_id: Option<String>,
    pub subject: SubjectRef,
    pub name: String,
    pub native_title: Option<String>,
    pub native_thread: Option<String>,
    pub native_workspace: Option<String>,
    pub compaction_count: Option<u64>,
    pub runtime: Observation<String>,
    pub runtime_version: Option<String>,
    pub runtime_version_latest: Option<String>,
    pub runtime_version_recorded_at: Option<String>,
    pub project: Observation<String>,
    pub configured_profile: Option<String>,
    pub effective_profile: Observation<String>,
    pub role: String,
    pub activity: String,
    pub activity_details: Option<String>,
    pub updated: String,
    pub cwd: String,
    pub retirement_note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProjectBadge {
    pub label: String,
    pub color: ProjectPaletteColor,
}

pub(super) fn member_view(
    member: &ProjectMemberProjection,
    project: &str,
    role: &str,
) -> AgentSessionView {
    let native_versions = cutex::launch::version_registry::load_relaxed();
    let runtime = match (&member.runtime, &member.observation_error) {
        (_, Some(error)) => Observation::Unavailable(error.clone()),
        (Some(runtime), None)
            if runtime.cutex_session_id == member.agent.cutex_session_id
                && runtime.native_session_id == member.agent.native_session_id =>
        {
            Observation::Known(if runtime.active { "Online" } else { "Offline" }.into())
        }
        _ => Observation::Unavailable("runtime observation missing or identity mismatch".into()),
    };
    AgentSessionView {
        remote_provider: None, remote_profile_style: None,
            host: String::new(),
        badge: None,
        project_id: None,
        subject: SubjectRef::Managed(member.agent.cutex_session_id.as_str().to_owned()),
        name: member.agent.spec.name.clone(), // current provider formal-name projection
        native_title: None, native_thread: Some(member.agent.native_session_id.clone()), native_workspace: None,
        compaction_count: None,
        runtime,
        runtime_version: member.runtime.as_ref().and_then(|runtime| runtime.runtime_version.clone()),
        runtime_version_latest: native_versions.as_ref().map(|registry| registry.latest_runtime.version.clone()),
        runtime_version_recorded_at: native_versions.as_ref().map(|registry| registry.latest_runtime.recorded_at.clone()),
        project: Observation::Known(project.into()),
        configured_profile: member.agent.spec.profile.clone(),
        effective_profile: member.runtime.as_ref().filter(|r| r.cutex_session_id == member.agent.cutex_session_id && r.native_session_id == member.agent.native_session_id && member.observation_error.is_none()).map(|r| Observation::Known(r.profile.clone())).unwrap_or_else(|| Observation::Unavailable("effective launch profile not observed".into())),
        role: role.into(), activity: "—".into(), activity_details: None, updated: "—".into(), cwd: member.agent.spec.cwd.clone(),
        retirement_note: member.agent.retired_at.as_ref().map(|_| "Provider marks this identity retired; durable retirement not independently reconciled".into()),
    }
}

pub(super) fn project_member_view(
    member: &ProjectMemberProjection,
    project: &CutexProjectWorkspace,
    role: &str,
) -> AgentSessionView {
    let mut view = member_view(member, &project.presentation.display_name, role);
    view.project_id = Some(project.project_id.to_string());
    view.badge = (!project.presentation.badge_label.is_empty()).then(|| ProjectBadge {
        label: project.presentation.badge_label.clone(),
        color: project.presentation.color,
    });
    view
}

pub(super) fn project_members(project: &CutexProjectWorkspace) -> Vec<AgentSessionView> {
    let mut members = std::collections::BTreeMap::<SubjectRef, AgentSessionView>::new();
    for (member, role) in project
        .director
        .iter()
        .filter_map(|director| director.member.as_ref())
        .map(|m| (m, "Director"))
        .chain(
            project
                .agent_operators
                .iter()
                .map(|m| (&m.member, "Operator")),
        )
        .chain(project.active_agents.iter().map(|m| (m, "Member")))
    {
        let view = project_member_view(member, project, role);
        members
            .entry(view.subject.clone())
            .and_modify(|existing| {
                if !existing.role.split('/').any(|r| r == role) {
                    existing.role.push('/');
                    existing.role.push_str(role);
                }
            })
            .or_insert(view);
    }
    if let Some(director) = project.director.as_ref().filter(|director| {
        director.member.is_none()
            && !project
                .archived_agents
                .iter()
                .any(|m| m.agent.cutex_session_id == director.cutex_session_id)
    }) {
        let id = director.cutex_session_id.as_str().to_owned();
        members
            .entry(SubjectRef::Managed(id.clone()))
            .and_modify(|view| {
                view.role = format!("Director/{}", view.role);
            })
            .or_insert(AgentSessionView {
                remote_provider: None, remote_profile_style: None,
            host: String::new(),
                badge: (!project.presentation.badge_label.is_empty()).then(|| ProjectBadge {
                    label: project.presentation.badge_label.clone(),
                    color: project.presentation.color,
                }),
                subject: SubjectRef::Managed(id.clone()),
                project_id: Some(project.project_id.to_string()),
                name: format!("Unavailable ({id})"),
                native_title: None,
                native_thread: None,
                native_workspace: None,
                compaction_count: None,
                runtime: Observation::Unavailable("Director member projection missing".into()),
                runtime_version: None,
                runtime_version_latest: None,
                runtime_version_recorded_at: None,
                project: Observation::Known(project.presentation.display_name.clone()),
                configured_profile: None,
                effective_profile: Observation::Unavailable(
                    "Director member projection missing".into(),
                ),
                role: "Director".into(),
                activity: "—".into(),
                activity_details: None,
                updated: "—".into(),
                cwd: "Unavailable".into(),
                retirement_note: None,
            });
    }
    let mut rows: Vec<_> = members.into_values().collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name).then(a.subject.cmp(&b.subject)));
    rows
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ListKind {
    Managed,
    Recent,
    Members,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Column {
    Name,
    Status,
    Role,
    Activity,
    Project,
    Profile,
    Updated,
    Host,
}
impl Column {
    fn label(self) -> &'static str {
        match self {
            Self::Host => "HOST",
            Self::Name => "NAME",
            Self::Status => "STATUS",
            Self::Role => "ROLE",
            Self::Activity => "ACTIVITY",
            Self::Project => "PROJECT",
            Self::Profile => "PROFILE",
            Self::Updated => "RECENCY",
        }
    }
    fn value(self, row: &AgentSessionView) -> String {
        match self {
            Self::Host => cutex::management::connections::short_display(&row.host),
            Self::Name => row.name.clone(),
            Self::Status => match &row.runtime {
                Observation::Unavailable(_) => "N/A".into(),
                Observation::Stale(..) => "Stale".into(),
                _ => row.runtime.label(),
            },
            Self::Role => row.role.clone(),
            Self::Activity => row.activity.clone(),
            Self::Project => match &row.project {
                Observation::Known(value) if value == "unassigned" => "-".into(),
                Observation::Unavailable(_) => "N/A".into(),
                _ => row.project.label(),
            },
            Self::Profile => profile_label(row),
            Self::Updated => row.updated.clone(),
        }
    }
}
pub(super) fn visible_columns(width: u16, kind: ListKind) -> Vec<(Column, u16)> {
    let mut columns = vec![(Column::Status, 7)];
    if kind == ListKind::Members && width >= 36 {
        columns.push((Column::Role, 16));
    }
    if kind == ListKind::Recent && width >= 44 {
        columns.push((Column::Updated, 16));
    }
    if kind != ListKind::Members && width >= 120 { columns.push((Column::Host, 14)); }
    if width >= 66 {
        columns.push((Column::Project, 18));
    }
    if width >= 86 {
        columns.push((Column::Profile, 16));
    }
    if kind != ListKind::Recent && width >= 104 {
        columns.push((Column::Activity, 12));
    }
    let used: u16 = columns.iter().map(|(_, w)| w + 1).sum();
    // 42 cells of name (one complete durable ID), plus a 4-cell badge and gap.
    // Do not let a wide viewport turn Name into an unbounded spacer.
    let name = width.saturating_sub(used).clamp(1, 47);
    if width < 18 {
        return vec![(Column::Name, width.max(1))];
    }
    columns.insert(0, (Column::Name, name));
    let mut spare = width
        .saturating_sub(columns.iter().map(|(_, w)| *w).sum::<u16>() + columns.len() as u16 - 1);
    for (column, w) in &mut columns {
        let cap = match column {
            Column::Project => 28,
            Column::Profile => 24,
            Column::Updated => 20,
            _ => *w,
        };
        let extra = spare.min(cap.saturating_sub(*w));
        *w += extra;
        spare -= extra;
    }
    columns
}

fn profile_label(row: &AgentSessionView) -> String {
    if let Some(profile) = &row.configured_profile {
        return profile.clone();
    }
    match &row.effective_profile {
        Observation::Known(name) if !name.trim().is_empty() => format!("~{name}"),
        Observation::Stale(name, _) if !name.trim().is_empty() => format!("~{name} (stale)"),
        _ => "~?".into(),
    }
}

fn runtime_style(runtime: &Observation<String>) -> Style {
    let color = match runtime {
        Observation::Known(value) => crate::cli_app::session_tui_layout::runtime_status_color(value),
        Observation::Stale(_, _) => crate::cli_app::session_tui_layout::status_stale(),
        Observation::Unavailable(_) => crate::cli_app::session_tui_layout::status_unknown(),
    };
    Style::new().fg(color)
}

fn row_remote(row:&AgentSessionView)->bool { !row.host.is_empty() && !cutex::runtime::lifecycle::cutex_session_host_is_local(&row.host,&cutex::platform::host::current_host_name()) }
fn row_profile(row:&AgentSessionView)->Option<&str> {row.configured_profile.as_deref().or_else(||match &row.effective_profile{Observation::Known(v)=>Some(v.as_str()),_=>None})}
fn profile_style(row:&AgentSessionView)->Style {
    if row_remote(row){return super::identity_styles::remote_profile(row.remote_provider,row.remote_profile_style.as_ref());}
    row_profile(row).map(super::identity_styles::profile).unwrap_or_else(||Style::new().fg(super::session_tui_layout::status_unknown()))
}

fn runtime_version_outdated(row: &AgentSessionView) -> bool {
    row.runtime_version
        .as_deref()
        .zip(row.runtime_version_latest.as_deref())
        .is_some_and(|(current, latest)| current != latest)
}

fn runtime_version_style(row: &AgentSessionView) -> Style {
    Style::new().fg(if runtime_version_outdated(row) {
        Color::Red
    } else {
        Color::Gray
    })
}

fn name_line(row: &AgentSessionView, width: usize) -> Line<'static> {
    if width < 6 {
        return Line::from(Span::styled(clipped(&row.name, width),super::identity_styles::agent(row_profile(row),row.remote_provider,row_remote(row))));
    }
    let badge = match &row.badge {
        Some(badge) => {
            let label = clipped(&badge.label, 2);
            Span::styled(
                format!(
                    " {}{} ",
                    label,
                    " ".repeat(2usize.saturating_sub(label.width()))
                ),
                super::session_tui_cutex_projects::project_badge_style(badge.color),
            )
        }
        None => Span::raw("    "),
    };
    let remote = !row.host.is_empty() && !cutex::runtime::lifecycle::cutex_session_host_is_local(
        &row.host, &cutex::platform::host::current_host_name(),
    );
    let suffix = if remote { format!("@{}", cutex::management::connections::short_display(&row.host)) } else { String::new() };
    let available = width - 5;
    let suffix = clipped(&suffix, available.saturating_sub(1).min(available / 2));
    let warning = runtime_version_outdated(row)
        .then(|| Span::styled(" ⚠", Style::new().fg(Color::Yellow)));
    let warning_width = warning.as_ref().map(|span| span.width()).unwrap_or_default();
    let mut spans = vec![
        badge,
        Span::raw(" "),
        Span::styled(
            clipped(
                &row.name,
                available
                    .saturating_sub(suffix.width())
                    .saturating_sub(warning_width),
            ),
            super::identity_styles::agent(row_profile(row),row.remote_provider,remote),
        ),
    ];
    if let Some(warning) = warning {
        spans.push(warning);
    }
    spans.push(Span::styled(suffix, super::identity_styles::host()));
    Line::from(spans)
}

pub(super) fn clipped(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.into();
    }
    if width == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut used = 0;
    for grapheme in Span::raw(text).styled_graphemes(Style::new()) {
        let w = grapheme.symbol.width();
        if used + w > width - 1 {
            break;
        }
        result.push_str(grapheme.symbol);
        used += w;
    }
    result.push('…');
    result
}
pub(super) fn tail_path(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.into();
    }
    if width < 2 {
        return clipped(text, width);
    }
    let mut result = Vec::new();
    let mut used = 1;
    let span = Span::raw(text);
    let graphemes: Vec<_> = span.styled_graphemes(Style::new()).collect();
    for grapheme in graphemes.into_iter().rev() {
        let w = grapheme.symbol.width();
        if used + w > width {
            break;
        }
        result.push(grapheme.symbol);
        used += w;
    }
    format!("…{}", result.into_iter().rev().collect::<String>())
}
pub(super) fn render_table(
    frame: &mut Frame<'_>,
    area: Rect,
    rows: &[AgentSessionView],
    kind: ListKind,
    state: &mut TableState,
) {
    let block = Block::bordered();
    let inner = block.inner(area);
    let mut columns = visible_columns(inner.width.saturating_sub(2), kind);
    // Remote identity is visible beside the name at every table width.
    columns.retain(|(c,_)|*c!=Column::Host);
    let table = Table::new(
        rows.iter().enumerate().map(|(index, row)| {
            let selected = state.selected() == Some(index);
            Row::new(columns.iter().map(|(c, w)| {
                if *c == Column::Name {
                    return Cell::from(name_line(row, usize::from(*w)));
                }
                let style = match c {
                    Column::Status => runtime_style(&row.runtime),
                    Column::Profile => profile_style(row),
                    Column::Project if matches!(row.project, Observation::Unavailable(_)) => Style::new().fg(crate::cli_app::session_tui_layout::status_unknown()),
                    Column::Role => Style::new().fg(crate::cli_app::session_tui_layout::focus()),
                    Column::Host => Style::new().fg(if cutex::runtime::lifecycle::cutex_session_host_is_local(&row.host,&cutex::platform::host::current_host_name()) {Color::Gray}else{super::session_tui_layout::focus()}),
                    Column::Activity | Column::Updated => Style::new().fg(Color::Gray),
                    _ => Style::new(),
                };
                let line = Line::from(clipped(&c.value(row), usize::from(*w)));
                Cell::from(if *c == Column::Activity {
                    line.centered()
                } else {
                    line
                })
                .style(style)
            }))
            .style(if selected {
                Style::new()
                    .bg(crate::cli_app::session_tui_layout::selection())
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            })
        }),
        columns.iter().map(|(_, w)| Constraint::Length(*w)),
    )
    .header(
        Row::new(columns.iter().map(|(c, _)| c.label()))
            .style(Style::new().add_modifier(Modifier::BOLD)),
    )
    .column_spacing(1)
    .block(block)
    .highlight_symbol("> ")
    // Row base supplies selection; late highlight colors would erase badge and
    // semantic cell colors. The highlight symbol remains the non-color cue.
    .row_highlight_style(Style::new());
    frame.render_stateful_widget(table, area, state);
    if rows.is_empty() && inner.height > 1 {
        frame.render_widget(
            Paragraph::new("No rows in this scope / filter"),
            Rect {
                y: inner.y + 1,
                height: inner.height - 1,
                ..inner
            },
        );
    }
}
#[derive(Debug, Default, Clone)]
pub(super) struct DetailScroll {
    offset: std::cell::Cell<usize>,
    page: std::cell::Cell<usize>,
    total: std::cell::Cell<usize>,
}
impl DetailScroll {
    pub(super) fn reset(&self) {
        self.offset.set(0);
    }
    pub(super) fn handle(&self, key: crossterm::event::KeyEvent) -> bool {
        use crossterm::event::{KeyCode, KeyEventKind};
        if key.kind == KeyEventKind::Release || !key.modifiers.is_empty() {
            return false;
        }
        let max = self.total.get().saturating_sub(self.page.get());
        let next = match key.code {
            KeyCode::Up => self.offset.get().saturating_sub(1),
            KeyCode::Down => self.offset.get().saturating_add(1),
            KeyCode::PageUp => self.offset.get().saturating_sub(self.page.get().max(1)),
            KeyCode::PageDown => self.offset.get().saturating_add(self.page.get().max(1)),
            KeyCode::Home => 0,
            KeyCode::End => max,
            _ => return false,
        };
        self.offset.set(next.min(max));
        true
    }
}

// Pre-wrap by terminal cells, retaining every printable scalar and whitespace.
// Unlike word wrapping, long unbroken IDs/paths always have reachable tails.
#[cfg(test)]
fn detail_lines(text: &str, width: usize) -> Vec<String> {
    wrap_details(
        text.split('\n').map(|s| Line::from(s.to_owned())).collect(),
        width,
    )
    .into_iter()
    .map(|line| {
        line.spans
            .into_iter()
            .map(|s| s.content.into_owned())
            .collect()
    })
    .collect()
}

fn wrap_details(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    let mut wrapped = Vec::new();
    for source in lines {
        let mut spans = Vec::new();
        let mut cells = 0;
        for span in source.spans {
            let text: String = span
                .content
                .chars()
                .map(|ch| {
                    if ch.is_control() {
                        format!("\\u{{{:x}}}", ch as u32)
                    } else {
                        ch.to_string()
                    }
                })
                .collect();
            let printable = Span::styled(text, span.style);
            // Use the renderer's grapheme segmentation and width convention:
            // splitting ZWJ/combining sequences can shift a terminal border.
            for grapheme in printable.styled_graphemes(source.style) {
                let size = grapheme.symbol.width();
                if cells + size > width && !spans.is_empty() {
                    wrapped.push(Line::from(std::mem::take(&mut spans)));
                    cells = 0;
                }
                if size > width {
                    // only possible in a <2-cell viewport
                    spans.push(Span::styled("…", grapheme.style));
                    cells = 1;
                } else {
                    spans.push(Span::styled(grapheme.symbol.to_owned(), grapheme.style));
                    cells += size;
                }
            }
        }
        wrapped.push(Line::from(spans));
    }
    wrapped
}

pub(super) fn render_details(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    text: &str,
    scroll: &DetailScroll,
) {
    render_styled_details(
        frame,
        area,
        Some(title),
        text.split('\n').map(|s| Line::from(s.to_owned())).collect(),
        scroll,
    );
}

pub(super) fn render_styled_details(
    frame: &mut Frame<'_>,
    area: Rect,
    title: Option<&str>,
    lines: Vec<Line<'static>>,
    scroll: &DetailScroll,
) {
    frame.render_widget(Clear, area);
    let inner = if let Some(title) = title {
        let block = Block::bordered().title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        inner
    } else {
        area
    };
    let page = usize::from(inner.height.saturating_sub(1));
    let lines = wrap_details(lines, usize::from(inner.width));
    scroll.total.set(lines.len());
    scroll.page.set(page);
    let offset = scroll.offset.get().min(lines.len().saturating_sub(page));
    scroll.offset.set(offset);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(
            lines
                .into_iter()
                .skip(offset)
                .take(page)
                .collect::<Vec<_>>(),
        ),
        Rect {
            height: page as u16,
            ..inner
        },
    );
    let total = scroll.total.get();
    let first = if total == 0 { 0 } else { offset + 1 };
    let last = (offset + page).min(total);
    let footer = if total <= page {
        if inner.width >= 44 {
            format!("Esc back · lines {first}–{last} of {total} · all visible")
        } else {
            format!("Esc · all {total} lines")
        }
    } else if inner.width >= 54 {
        format!("↑↓ PgUp/Dn Home/End · Esc back · lines {first}–{last} of {total}")
    } else {
        format!("↑↓ Pg · lines {first}–{last}/{total} · Esc")
    };
    frame.render_widget(
        Paragraph::new(footer).style(Style::new().fg(crate::cli_app::session_tui_layout::focus()).add_modifier(Modifier::BOLD)),
        Rect {
            y: inner.y + inner.height - 1,
            height: 1,
            ..inner
        },
    );
}

pub(super) fn render_entity_details(
    frame: &mut Frame<'_>, area: Rect, title: &str,
    lines: Vec<Line<'static>>, scroll: &DetailScroll, focused: bool,
) {
    let block = Block::bordered().title(format!(" {title} "))
        .border_style(Style::new().fg(if focused { crate::cli_app::session_tui_layout::focus() } else { Color::DarkGray }));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if !focused { scroll.offset.set(0); }
    render_styled_details(frame, inner, None, lines, scroll);
    if !focused && inner.height > 0 {
        let footer = Rect { y: inner.bottom() - 1, height: 1, ..inner };
        frame.render_widget(Clear, footer);
        frame.render_widget(Paragraph::new("Alt+I inspect").style(Style::new().fg(crate::cli_app::session_tui_layout::focus())), footer);
    }
}

pub(super) fn render_agent_details(frame: &mut Frame<'_>, area: Rect, row: &AgentSessionView, scroll: &DetailScroll, focused: bool) {
    render_entity_details(frame, area, "Agent Details", inspector_lines(row), scroll, focused);
}

pub(super) fn render_inspector(
    frame: &mut Frame<'_>,
    area: Rect,
    row: &AgentSessionView,
    scroll: &DetailScroll,
) {
    render_styled_details(
        frame,
        area,
        Some(" Inspector · read only "),
        inspector_lines(row),
        scroll,
    );
}

pub(super) fn render_inspector_body(
    frame: &mut Frame<'_>,
    area: Rect,
    row: &AgentSessionView,
    scroll: &DetailScroll,
) {
    render_styled_details(frame, area, None, inspector_lines(row), scroll);
}

fn inspector_lines(row: &AgentSessionView) -> Vec<Line<'static>> {
    let heading = |text: &str| {
        Line::styled(
            text.to_owned(),
            Style::new().fg(crate::cli_app::session_tui_layout::focus()).add_modifier(Modifier::BOLD),
        )
    };
    let field = |label: &str, value: String, style: Style| {
        Line::from(vec![
            Span::styled(format!("{label}: "), Style::new().fg(Color::Gray)),
            Span::styled(value, style),
        ])
    };
    let mut lines = vec![
        name_line(row, row.name.width() + cutex::management::connections::short_display(&row.host).width() + 6),
        field(
            "Status",
            row.runtime.label(),
            runtime_style(&row.runtime),
        ),
        field("Host", cutex::management::connections::display(&row.host), Style::new()),
        field("Host identity", if row.host.is_empty(){"N/A".into()}else{row.host.clone()}, Style::new().fg(Color::Gray)),
        field("Cutex Project", row.project.label(), Style::new()),
    ];
    if row.runtime_version.is_some() {
        lines.extend([
            field(
                "Runtime version",
                row.runtime_version.clone().unwrap_or_default(),
                runtime_version_style(row),
            ),
            field(
                "Latest runtime",
                row.runtime_version_latest
                    .clone()
                    .unwrap_or_else(|| "unknown".into()),
                Style::new().fg(Color::Gray),
            ),
            field(
                "Version recorded",
                row.runtime_version_recorded_at
                    .clone()
                    .unwrap_or_else(|| "unknown".into()),
                Style::new().fg(Color::Gray),
            ),
        ]);
    }
    if !row.role.trim().is_empty() {
        lines.push(field(
            "Role",
            row.role.clone(),
            Style::new().fg(crate::cli_app::session_tui_layout::focus()),
        ));
    }
    if let Some(activity) = &row.activity_details {
        lines.push(field(
            "Activity",
            activity.clone(),
            Style::new().fg(Color::Gray),
        ));
    }
    lines.extend([
        Line::default(),
        heading("Profile"),
        field(
            "Configured",
            row.configured_profile
                .clone()
                .unwrap_or_else(|| "inherit".into()),
            profile_style(row),
        ),
        field(
            "Effective",
            row.effective_profile.label(),
            profile_style(row),
        ),
        Line::default(),
        heading("Context"),
        field("Full path", row.cwd.clone(), Style::new().fg(Color::Gray)),
        field(
            "Native workspace",
            row.native_workspace
                .clone()
                .unwrap_or_else(|| "not observed".into()),
            Style::new().fg(Color::Gray),
        ),
        field(
            "Native title",
            row.native_title
                .clone()
                .unwrap_or_else(|| "not observed".into()),
            Style::new().fg(Color::Gray),
        ),
    ]);
    if let Some(compaction_count) = row.compaction_count {
        lines.push(field(
            "Compactions",
            compaction_count.to_string(),
            Style::new().fg(Color::Gray),
        ));
    }
    for observation in [&row.runtime, &row.project, &row.effective_profile] {
        match observation {
            Observation::Unavailable(reason) | Observation::Stale(_, reason) => lines.push(field(
                "Observation",
                reason.clone(),
                Style::new().fg(Color::Yellow),
            )),
            _ => {}
        }
    }
    if let Some(note) = &row.retirement_note {
        lines.push(field(
            "Lifecycle",
            note.clone(),
            Style::new().fg(Color::Yellow),
        ));
    }
    lines.extend([Line::default(), heading("Technical identifiers")]);
    if let Some(id) = &row.project_id {
        lines.push(field(
            "Project ID",
            id.clone(),
            Style::new().fg(Color::Gray),
        ));
    }
    match &row.subject {
        SubjectRef::Managed(id) => lines.push(field(
            "Durable ID",
            id.clone(),
            Style::new().fg(Color::Gray),
        )),
        SubjectRef::Native { catalog, thread } => {
            lines.push(field(
                "Catalog",
                catalog.clone(),
                Style::new().fg(Color::Gray),
            ));
            lines.push(field(
                "Native ID",
                thread.clone(),
                Style::new().fg(Color::Gray),
            ));
        }
    }
    if let Some(id) = &row.native_thread {
        lines.push(field("Native ID", id.clone(), Style::new().fg(Color::Gray)));
    }
    lines
}

#[cfg(test)] mod stale_status_test {
use super::*;
#[test] fn stale_state_is_not_hidden_after_column_clipping(){let mut row=tests::visual_row();row.runtime=Observation::Stale("Online".into(),"host unreachable".into());assert_eq!(Column::Status.value(&row),"Stale");}
}

#[cfg(test)]
#[test]
fn remote_suffix_is_visible_and_colored_even_when_name_is_long() {
    let mut row = tests::visual_row();
    row.name = "a-very-long-remote-agent-name".into();
    row.host = "eva-remote-fixture".into();
    for width in [24, 48, 80] {
        let line = name_line(&row, width);
        assert!(line.width() <= width);
        let suffix = line.spans.last().unwrap();
        assert!(suffix.content.starts_with('@'));
        assert_eq!(suffix.style.fg, Some(super::session_tui_layout::focus()));
    }
    row.host = cutex::platform::host::current_host_name();
    assert!(!name_line(&row, 80).to_string().contains('@'));
}

#[cfg(test)]
#[test]
fn outdated_runtime_version_adds_warning_without_replacing_remote_suffix() {
    let mut row = tests::visual_row();
    row.name = "worker".into();
    row.host = "eva-remote-fixture".into();
    row.runtime_version = Some("0.156.1-r35".into());
    row.runtime_version_latest = Some("0.156.1-r36".into());
    row.runtime_version_recorded_at = Some("2026-09-23T22:15:00Z".into());
    let line = name_line(&row, 80);
    let text = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(text.contains("worker ⚠@"));
    assert!(line.spans.last().unwrap().content.starts_with('@'));
    let details = inspector_lines(&row);
    let version = details
        .iter()
        .find(|line| line.to_string().starts_with("Runtime version:"))
        .unwrap();
    assert!(version.to_string().contains("0.156.1-r35"));
    assert!(runtime_version_style(&row).fg == Some(Color::Red));
}

#[cfg(test)]
#[test]
fn current_runtime_version_remains_visible_in_agent_details() {
    let mut row = tests::visual_row();
    row.name = "current-worker".into();
    row.runtime_version = Some("0.157.1-r45".into());
    row.runtime_version_latest = Some("0.157.1-r45".into());
    row.runtime_version_recorded_at = Some("2026-09-28T01:52+10:00".into());
    let details = inspector_lines(&row);
    let rendered = details
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("Runtime version: 0.157.1-r45"));
    assert!(rendered.contains("Latest runtime: 0.157.1-r45"));
    assert!(!rendered.contains("current-worker ⚠"));
    assert!(runtime_version_style(&row).fg == Some(Color::Gray));
}
