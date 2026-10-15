use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Repeats may edit or navigate, but never activate an operation or shortcut.
pub(super) fn accepts_key(key: KeyEvent, text_input: bool) -> bool {
    match key.kind {
        KeyEventKind::Release => false,
        KeyEventKind::Press => true,
        KeyEventKind::Repeat => {
            !key.modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                && (matches!(
                    key.code,
                    KeyCode::Up
                        | KeyCode::Down
                        | KeyCode::Left
                        | KeyCode::Right
                        | KeyCode::Home
                        | KeyCode::End
                        | KeyCode::PageUp
                        | KeyCode::PageDown
                        | KeyCode::Backspace
                        | KeyCode::Delete
                ) || (text_input && matches!(key.code, KeyCode::Char(_))))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WorkspaceEvent {
    Up,
    Down,
    First,
    Last,
    Insert(char),
    Backspace,
    Delete,
    ClearInput,
    OpenActions,
    OpenSettings,
    Activate,
    #[allow(dead_code)]
    Back,
    Escape,
    Exit,
}

pub(super) fn workspace_event_from_key(
    key: KeyEvent,
    enhanced_keyboard: bool,
) -> Option<WorkspaceEvent> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c' | 'C'))
    {
        return Some(WorkspaceEvent::Exit);
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('u' | 'U'))
    {
        return Some(WorkspaceEvent::ClearInput);
    }
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }
    if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::SHIFT) {
        return enhanced_keyboard.then_some(WorkspaceEvent::OpenActions);
    }
    match key.code {
        KeyCode::Up => Some(WorkspaceEvent::Up),
        KeyCode::Down => Some(WorkspaceEvent::Down),
        KeyCode::Home => Some(WorkspaceEvent::First),
        KeyCode::End => Some(WorkspaceEvent::Last),
        // Focus traversal and view expansion are workspace-specific. Keeping
        // them out of this shared action map prevents Left/Right from ever
        // becoming an implicit lifecycle action.
        KeyCode::Right | KeyCode::Left | KeyCode::Tab | KeyCode::BackTab => None,
        KeyCode::Enter => Some(WorkspaceEvent::Activate),
        KeyCode::Char(character) => Some(WorkspaceEvent::Insert(character)),
        KeyCode::Backspace => Some(WorkspaceEvent::Backspace),
        KeyCode::Delete => Some(WorkspaceEvent::Delete),
        KeyCode::Esc => Some(WorkspaceEvent::Escape),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_shared_workspace_keys_without_exposing_view_specific_behavior() {
        assert_eq!(
            workspace_event_from_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), false),
            None
        );
        assert_eq!(
            workspace_event_from_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), false),
            None
        );
        assert_eq!(
            workspace_event_from_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT), false),
            None
        );
        assert_eq!(
            workspace_event_from_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT), true),
            Some(WorkspaceEvent::OpenActions)
        );
        assert_eq!(
            workspace_event_from_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), false),
            None
        );
        assert_eq!(
            workspace_event_from_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), false),
            None
        );
        assert_eq!(
            workspace_event_from_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), false),
            Some(WorkspaceEvent::Insert('a'))
        );
        assert_eq!(
            workspace_event_from_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE), false),
            Some(WorkspaceEvent::Insert('e'))
        );
        assert_eq!(
            workspace_event_from_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE), false),
            Some(WorkspaceEvent::Insert('v'))
        );
        assert_eq!(
            workspace_event_from_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT), false),
            None
        );
    }
}
