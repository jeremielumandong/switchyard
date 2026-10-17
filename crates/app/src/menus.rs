//! Dropdown and context menus that scroll when their items do not fit.
//!
//! gpui-component's `PopupMenu` only scrolls when it is told to (or is built from more than
//! 20 owned items), so a menu built item by item, such as a long namespace or label list,
//! ran off the window with no way to reach its last entries. These traits shadow
//! `DropdownMenu::dropdown_menu` and `ContextMenuExt::context_menu` with the same names and
//! turn scrolling on for every menu. Import them instead of gpui-component's.

use gpui_kit::component::menu::{self as base, ContextMenu, PopupMenu};
use gpui_kit::{Context, InteractiveElement, IntoElement, ParentElement, Styled, Window};

/// Caps a menu at the window-relative height gpui-component uses and lets it scroll.
pub fn scrolling(menu: PopupMenu) -> PopupMenu {
    menu.scrollable(true)
}

/// A dropdown menu on a button whose list scrolls once it is taller than the window allows.
pub trait DropdownMenu: base::DropdownMenu + Sized {
    /// Opens `f`'s menu under the trigger, anchored to its top-left corner.
    fn dropdown_menu(
        self,
        f: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> impl IntoElement {
        base::DropdownMenu::dropdown_menu(self, move |menu, window, cx| {
            scrolling(f(menu, window, cx))
        })
    }
}

impl<T: base::DropdownMenu> DropdownMenu for T {}

/// A right-click menu whose list scrolls once it is taller than the window allows.
pub trait ContextMenuExt: InteractiveElement + ParentElement + Styled + Sized {
    /// Opens `f`'s menu at the pointer on right click.
    #[track_caller]
    fn context_menu(
        self,
        f: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> ContextMenu<Self> {
        base::ContextMenuExt::context_menu(self, move |menu, window, cx| {
            scrolling(f(menu, window, cx))
        })
    }
}

impl<E: InteractiveElement + ParentElement + Styled> ContextMenuExt for E {}
