//! Keeps a virtualized `list` in step with rows named by identity (the
//! collection rail): only rows that scroll into view are built, and a change
//! re-measures just the rows that differ, so the scroll position survives
//! expanding a folder or a new request.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use gpui_kit::{ListAlignment, ListState, px};

/// A [`ListState`] and the rows it currently shows.
pub(super) struct VirtualRows<T> {
    state: ListState,
    items: RefCell<Rc<[T]>>,
}

impl<T: PartialEq> VirtualRows<T> {
    /// An empty list.
    pub fn new() -> Self {
        Self {
            state: ListState::new(0, ListAlignment::Top, px(200.)),
            items: RefCell::new(Rc::from(Vec::new())),
        }
    }

    /// Show `items`: the rows that changed since the last call are spliced
    /// (and re-measured); unchanged rows keep their measured heights.
    pub fn sync(&self, items: Vec<T>) -> Rc<[T]> {
        let mut current = self.items.borrow_mut();
        if let Some((range, count)) = changed_range(&current, &items) {
            self.state.splice(range, count);
            *current = Rc::from(items);
        }
        current.clone()
    }

    /// The list state, for `list()` and its scrollbar.
    pub fn state(&self) -> &ListState {
        &self.state
    }
}

/// The rows of `old` to replace, and how many rows of `new` replace them, so
/// that `old` becomes `new`; `None` when they are equal. Keeps the longest
/// common prefix and suffix.
pub(super) fn changed_range<T: PartialEq>(old: &[T], new: &[T]) -> Option<(Range<usize>, usize)> {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    if prefix == old.len() && prefix == new.len() {
        return None;
    }
    let max_suffix = old.len().min(new.len()) - prefix;
    let suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    Some((prefix..old.len() - suffix, new.len() - prefix - suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_range_keeps_common_ends() {
        assert_eq!(changed_range(&[1, 2, 3], &[1, 2, 3]), None);
        assert_eq!(changed_range::<u8>(&[], &[]), None);
        // Insert in the middle (a folder expanded).
        assert_eq!(changed_range(&[1, 2, 5], &[1, 2, 3, 4, 5]), Some((2..2, 2)));
        // Remove from the middle (collapsed).
        assert_eq!(changed_range(&[1, 2, 3, 4, 5], &[1, 5]), Some((1..4, 0)));
        // Replace one row.
        assert_eq!(changed_range(&[1, 2, 3], &[1, 9, 3]), Some((1..2, 1)));
        // Append, prepend, clear, fill.
        assert_eq!(changed_range(&[1, 2], &[1, 2, 3]), Some((2..2, 1)));
        assert_eq!(changed_range(&[2, 3], &[1, 2, 3]), Some((0..0, 1)));
        assert_eq!(changed_range(&[1, 2], &[]), Some((0..2, 0)));
        assert_eq!(changed_range(&[], &[1, 2]), Some((0..0, 2)));
        // Repeated rows never overlap the prefix and suffix.
        assert_eq!(changed_range(&[1, 1], &[1, 1, 1]), Some((2..2, 1)));
    }

    #[test]
    fn sync_splices_only_on_change() {
        let rows = VirtualRows::new();
        let a = rows.sync(vec![1, 2, 3]);
        assert_eq!(rows.state().item_count(), 3);
        // Same rows: the shared list is reused, not rebuilt.
        let b = rows.sync(vec![1, 2, 3]);
        assert!(Rc::ptr_eq(&a, &b));
        let c = rows.sync(vec![1, 3]);
        assert_eq!(&*c, &[1, 3]);
        assert_eq!(rows.state().item_count(), 2);
    }
}
