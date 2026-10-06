//! Split center area: two tabs side by side or stacked. The focused pane shows the
//! workspace's active tab, the other pane shows `other`; one tab strip serves both.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::{Bounds, Pixels, Point};

/// Which way the center area is split.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SplitDir {
    /// Panes side by side.
    Right,
    /// Panes stacked.
    Down,
}

/// Two visible tabs.
pub(crate) struct Split {
    pub dir: SplitDir,
    /// The tab in the pane that is not focused.
    pub other: usize,
    /// Whether the focused pane is the second (right / bottom) one.
    pub second_focused: bool,
    /// Share of the space given to the first pane.
    pub ratio: f32,
    /// The divider is being dragged.
    pub dragging: bool,
    /// The center area's bounds, recorded each frame (for dragging).
    pub bounds: Rc<Cell<Bounds<Pixels>>>,
}

/// Smallest share either pane may get.
const MIN_RATIO: f32 = 0.15;

impl Split {
    pub fn new(dir: SplitDir, other: usize, second_focused: bool) -> Self {
        Self {
            dir,
            other,
            second_focused,
            ratio: 0.5,
            dragging: false,
            bounds: Rc::default(),
        }
    }

    /// Move the divider to the mouse.
    pub fn drag_to(&mut self, pos: Point<Pixels>) {
        let b = self.bounds.get();
        let (at, start, len) = match self.dir {
            SplitDir::Right => (pos.x, b.origin.x, b.size.width),
            SplitDir::Down => (pos.y, b.origin.y, b.size.height),
        };
        let len = f32::from(len);
        if len > 0. {
            self.ratio =
                ((f32::from(at) - f32::from(start)) / len).clamp(MIN_RATIO, 1. - MIN_RATIO);
        }
    }

    /// The tab shown after tab `removed` is closed: indices after it shift down.
    pub fn tab_removed(&mut self, removed: usize) {
        if removed < self.other {
            self.other -= 1;
        }
    }
}

/// The tab to put next to `active` in a new split: the nearest one before it, else after,
/// among tabs that can be shown (`ok`).
pub(crate) fn partner(active: usize, len: usize, ok: impl Fn(usize) -> bool) -> Option<usize> {
    (0..active).rev().chain(active + 1..len).find(|&i| ok(i))
}

/// A valid `other` for `active` among `len` tabs, keeping `other` when it still works.
pub(crate) fn fix_other(
    other: usize,
    active: usize,
    len: usize,
    ok: impl Fn(usize) -> bool,
) -> Option<usize> {
    if other < len && other != active && ok(other) {
        Some(other)
    } else {
        partner(active, len, ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partners_and_repairs() {
        let all = |_| true;
        assert_eq!(partner(0, 1, all), None);
        assert_eq!(partner(0, 3, all), Some(1));
        assert_eq!(partner(2, 3, all), Some(1));
        // Tab 0 is the Welcome tab: skip it.
        let real = |i| i != 0;
        assert_eq!(partner(1, 3, real), Some(2));
        assert_eq!(partner(1, 2, real), None);
        assert_eq!(fix_other(2, 0, 3, all), Some(2));
        assert_eq!(fix_other(0, 0, 3, all), Some(1), "never the active tab");
        assert_eq!(fix_other(5, 1, 3, all), Some(0), "closed tab");
        assert_eq!(fix_other(0, 1, 3, real), Some(2), "never the Welcome tab");
        assert_eq!(fix_other(1, 0, 1, all), None, "nothing left to show");
        let mut s = Split::new(SplitDir::Right, 3, false);
        s.tab_removed(1);
        assert_eq!(s.other, 2);
        s.tab_removed(4);
        assert_eq!(s.other, 2);
    }

    #[test]
    fn dragging_clamps() {
        let mut s = Split::new(SplitDir::Right, 1, false);
        s.bounds.set(Bounds::new(
            gpui_kit::point(gpui_kit::px(100.), gpui_kit::px(0.)),
            gpui_kit::size(gpui_kit::px(1000.), gpui_kit::px(500.)),
        ));
        s.drag_to(gpui_kit::point(gpui_kit::px(400.), gpui_kit::px(10.)));
        assert!((s.ratio - 0.3).abs() < 1e-4);
        s.drag_to(gpui_kit::point(gpui_kit::px(105.), gpui_kit::px(10.)));
        assert!((s.ratio - MIN_RATIO).abs() < 1e-4);
    }
}
