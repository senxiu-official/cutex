//! Shared state for a top-level TUI workspace.
//!
//! Keep this independent of session records: future workspaces can reuse the
//! selection and transient-visibility behavior without coupling to the agent
//! selector's data model.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionTuiWorkspace {
    Agents,
    RecentSessions,
    RetiredSessions,
    CutexProjects,
    Projects,
    Profiles,
    GlobalSettings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PrimaryPanel {
    Agents,
    Projects,
    Alarms,
    Recent,
    Jobs,
    Profiles,
    Settings,
}

impl PrimaryPanel {
    pub(super) const ALL: [Self; 7] = [
        Self::Agents,
        Self::Recent,
        Self::Projects,
        Self::Alarms,
        Self::Jobs,
        Self::Profiles,
        Self::Settings,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Agents => "Agents",
            Self::Projects => "Projects",
            Self::Alarms => "Alarms",
            Self::Recent => "Sessions",
            Self::Jobs => "Jobs",
            Self::Profiles => "Profiles",
            Self::Settings => "Settings",
        }
    }

    pub(super) fn adjacent(self, forward: bool) -> Option<Self> {
        let index = Self::ALL.iter().position(|panel| *panel == self)?;
        let next = if forward {
            index.checked_add(1)?
        } else {
            index.checked_sub(1)?
        };
        Self::ALL.get(next).copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PrimaryPanelOutcome {
    Exit,
    Switch(PrimaryPanel),
}

/// Resolve only the frozen top-level workspace shortcuts. These shortcuts are
/// handled before a workspace's local input mapping, so Alt-modified text can
/// never leak into a filter or editor.
pub(super) fn primary_panel_shortcut(key: KeyEvent) -> Option<PrimaryPanel> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        || !key.modifiers.contains(KeyModifiers::ALT)
        || key.modifiers.contains(KeyModifiers::CONTROL)
    {
        return None;
    }
    match key.code {
        KeyCode::Char('1') => Some(PrimaryPanel::Agents),
        KeyCode::Char('2') => Some(PrimaryPanel::Recent),
        KeyCode::Char('3') => Some(PrimaryPanel::Projects),
        KeyCode::Char('4') => Some(PrimaryPanel::Alarms),
        KeyCode::Char('5') => Some(PrimaryPanel::Jobs),
        KeyCode::Char('6') => Some(PrimaryPanel::Profiles),
        KeyCode::Char('7') => Some(PrimaryPanel::Settings),
        _ => None,
    }
}

impl SessionTuiWorkspace {
    /// Workspaces that are intentionally available in the production selector.
    pub(super) const PRODUCTION: [Self; 7] = [
        Self::Agents,
        Self::RecentSessions,
        Self::RetiredSessions,
        Self::CutexProjects,
        Self::Projects,
        Self::Profiles,
        Self::GlobalSettings,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WorkspaceSelection<T> {
    selected: Option<T>,
    transiently_visible: Option<T>,
}

impl<T> Default for WorkspaceSelection<T> {
    fn default() -> Self {
        Self {
            selected: None,
            transiently_visible: None,
        }
    }
}

impl<T: PartialEq> WorkspaceSelection<T> {
    pub(super) fn selected(&self) -> Option<&T> {
        self.selected.as_ref()
    }

    pub(super) fn select(&mut self, target: Option<T>) {
        self.selected = target;
    }

    pub(super) fn is_selected(&self, target: &T) -> bool {
        self.selected.as_ref() == Some(target)
    }

    pub(super) fn mark_transiently_visible(&mut self, target: Option<T>) {
        self.transiently_visible = target;
    }

    #[cfg(test)]
    pub(super) fn is_transiently_visible(&self, target: &T) -> bool {
        self.transiently_visible.as_ref() == Some(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_workspace_list_keeps_cutex_projects_and_codex_workspaces_separate() {
        assert_eq!(
            SessionTuiWorkspace::PRODUCTION,
            [
                SessionTuiWorkspace::Agents,
                SessionTuiWorkspace::RecentSessions,
                SessionTuiWorkspace::RetiredSessions,
                SessionTuiWorkspace::CutexProjects,
                SessionTuiWorkspace::Projects,
                SessionTuiWorkspace::Profiles,
                SessionTuiWorkspace::GlobalSettings,
            ]
        );
    }

    #[test]
    fn primary_panels_include_alarms_jobs_profiles_and_settings_in_navigation_order() {
        assert_eq!(
            PrimaryPanel::ALL,
            [
                PrimaryPanel::Agents,
                PrimaryPanel::Recent,
                PrimaryPanel::Projects,
                PrimaryPanel::Alarms,
                PrimaryPanel::Jobs,
                PrimaryPanel::Profiles,
                PrimaryPanel::Settings,
            ]
        );
        assert_eq!(PrimaryPanel::Agents.label(), "Agents");
        assert_eq!(PrimaryPanel::Projects.label(), "Projects");
        assert_eq!(
            PrimaryPanel::Agents.adjacent(true),
            Some(PrimaryPanel::Recent)
        );
        assert_eq!(PrimaryPanel::Agents.adjacent(false), None);
        assert_eq!(PrimaryPanel::Alarms.label(), "Alarms");
        assert_eq!(PrimaryPanel::Alarms.adjacent(true), Some(PrimaryPanel::Jobs));
        assert_eq!(
            PrimaryPanel::Jobs.adjacent(true),
            Some(PrimaryPanel::Profiles)
        );
        assert_eq!(
            PrimaryPanel::Profiles.adjacent(true),
            Some(PrimaryPanel::Settings)
        );
        assert_eq!(PrimaryPanel::Settings.adjacent(true), None);
        assert_eq!(
            PrimaryPanel::Alarms.adjacent(false),
            Some(PrimaryPanel::Projects)
        );
    }

    #[test]
    fn alt_digits_select_all_six_top_level_workspaces() {
        for (character, expected) in [
            ('1', PrimaryPanel::Agents),
            ('2', PrimaryPanel::Recent),
            ('3', PrimaryPanel::Projects),
            ('4', PrimaryPanel::Alarms),
            ('5', PrimaryPanel::Jobs),
            ('6', PrimaryPanel::Profiles),
            ('7', PrimaryPanel::Settings),
        ] {
            assert_eq!(
                primary_panel_shortcut(KeyEvent::new(KeyCode::Char(character), KeyModifiers::ALT,)),
                Some(expected)
            );
        }
        assert_eq!(
            primary_panel_shortcut(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            primary_panel_shortcut(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn selection_keeps_recently_changed_items_visible_without_changing_selection() {
        let mut state = WorkspaceSelection::default();
        state.select(Some("alpha"));
        state.mark_transiently_visible(Some("beta"));

        assert!(state.is_selected(&"alpha"));
        assert!(state.is_transiently_visible(&"beta"));
        assert!(!state.is_selected(&"beta"));
    }
}
