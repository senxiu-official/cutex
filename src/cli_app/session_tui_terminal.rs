//! Keep erased wide-character tails explicit in terminal output.
//!
//! Ratatui 0.30's diff can omit a blank trailing cell when replacing an
//! unstyled CJK character with narrow text. JetBrains terminals retain their
//! internal wide-character placeholder there, shifting the rendered suffix.
//! Repair only those cells; do not clear the screen or repaint every frame.
use std::collections::HashSet;
use std::ops::{Deref, DerefMut};

use ratatui::backend::Backend;
use ratatui::buffer::{Buffer, CellDiffOption, CellWidth};
use ratatui::{CompletedFrame, Frame};

#[derive(Default)]
struct WideTails(HashSet<(u16, u16)>);

impl WideTails {
    fn repair(&mut self, buffer: &mut Buffer) {
        let mut next = HashSet::new();
        let area = buffer.area;
        for y in area.y..area.bottom() {
            let mut x = area.x;
            while x < area.right() {
                let cell = &mut buffer[(x, y)];
                let width = cell.cell_width().max(1);
                if self.0.contains(&(x, y)) && cell.diff_option == CellDiffOption::None {
                    cell.set_diff_option(CellDiffOption::AlwaysUpdate);
                }
                // Walk visible character starts, never mark the continuation of
                // a currently wide character: writing there would erase it.
                for tail in x.saturating_add(1)..x.saturating_add(width).min(area.right()) {
                    next.insert((tail, y));
                }
                x = x.saturating_add(width);
            }
        }
        self.0 = next;
    }
}

pub(super) struct Terminal<B: Backend> {
    inner: ratatui::Terminal<B>,
    wide_tails: WideTails,
}

impl<B: Backend> Terminal<B> {
    pub(super) fn new(backend: B) -> Result<Self, B::Error> {
        Ok(Self {
            inner: ratatui::Terminal::new(backend)?,
            wide_tails: WideTails::default(),
        })
    }

    pub(super) fn draw<F>(&mut self, render: F) -> Result<CompletedFrame<'_>, B::Error>
    where
        F: FnOnce(&mut Frame),
    {
        let tails = &mut self.wide_tails;
        self.inner.draw(|frame| {
            render(frame);
            tails.repair(frame.buffer_mut());
        })
    }
}

impl<B: Backend> Deref for Terminal<B> {
    type Target = ratatui::Terminal<B>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl<B: Backend> DerefMut for Terminal<B> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    #[test]
    fn cjk_to_ascii_emits_blank_tails_even_without_background() {
        for (name, missing_tails) in [("scpolya-2", 3), ("IFM", 5)] {
            let mut before = Buffer::empty(Rect::new(0, 0, 40, 1));
            before.set_string(0, 0, "编写SVG鹈鹕骑车HTML", Style::default());
            let mut tails = WideTails::default();
            tails.repair(&mut before);
            let previous_tails = tails.0.clone();
            let mut after = Buffer::empty(before.area);
            after.set_string(0, 0, name, Style::default());
            let stock: HashSet<_> = before.diff_iter(&after).map(|(x, y, _)| (x, y)).collect();
            // Matches the user's three-column / five-column shift exactly.
            assert_eq!(
                previous_tails
                    .iter()
                    .filter(|pos| !stock.contains(pos))
                    .count(),
                missing_tails,
                "{name}"
            );
            tails.repair(&mut after);
            let fixed: HashSet<_> = before.diff_iter(&after).map(|(x, y, _)| (x, y)).collect();
            assert!(
                previous_tails.iter().all(|pos| fixed.contains(pos)),
                "{name}"
            );
        }
    }

    #[test]
    fn unchanged_cjk_is_not_repainted_and_new_wide_tails_are_not_written() {
        let mut before = Buffer::empty(Rect::new(0, 0, 12, 1));
        before.set_string(0, 0, "中文", Style::default());
        let mut tails = WideTails::default();
        tails.repair(&mut before);
        let mut same = before.clone();
        tails.repair(&mut same);
        assert_eq!(before.diff_iter(&same).count(), 0);
        let mut shifted = Buffer::empty(before.area);
        shifted.set_string(0, 0, "a中文", Style::default());
        tails.repair(&mut shifted);
        let updates: Vec<_> = before.diff_iter(&shifted).map(|(x, _, _)| x).collect();
        assert!(!updates.contains(&2));
        assert!(!updates.contains(&4));
        let mut stable = Buffer::empty(shifted.area);
        stable.set_string(0, 0, "a中文", Style::default());
        tails.repair(&mut stable);
        // Changing AlwaysUpdate back to None may cause one final diff; the
        // repair itself must not force any cells on a stable wide-text frame.
        assert!(stable
            .content
            .iter()
            .all(|c| c.diff_option == CellDiffOption::None));
    }
}
