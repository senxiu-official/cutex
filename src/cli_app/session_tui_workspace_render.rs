use ratatui::Frame;

/// Narrow rendering boundary for a workspace. The shell owns terminal setup,
/// polling, and status; each workspace owns only its drawing implementation.
pub(super) trait WorkspaceRenderer<Model> {
    fn render(&self, frame: &mut Frame<'_>, model: &Model);
}

pub(super) fn render_workspace<Model>(
    frame: &mut Frame<'_>,
    model: &Model,
    renderer: &impl WorkspaceRenderer<Model>,
) {
    renderer.render(frame, model);
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use ratatui::{backend::TestBackend, Terminal};

    use super::*;

    struct TestRenderer(Cell<bool>);

    impl WorkspaceRenderer<()> for TestRenderer {
        fn render(&self, _frame: &mut Frame<'_>, _model: &()) {
            self.0.set(true);
        }
    }

    #[test]
    fn invokes_the_workspace_renderer_without_shell_knowledge_of_the_model() {
        let renderer = TestRenderer(Cell::new(false));
        let mut terminal = Terminal::new(TestBackend::new(4, 2)).expect("test terminal");

        terminal
            .draw(|frame| render_workspace(frame, &(), &renderer))
            .expect("draw workspace");

        assert!(renderer.0.get());
    }
}
