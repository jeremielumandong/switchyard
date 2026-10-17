//! The Files tab's popups: the right-click menus (on files, on the folder itself), the
//! New and Sort menus of the command bar, the "Code" menu and the Properties card.

use std::path::PathBuf;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    Pixels, Point, SharedString, StatefulInteractiveElement as _, Styled as _, Window, anchored,
    deferred, div, px, svg,
};
use switchyard_core::FsRef;

use super::kinds::type_label;
use super::{FileKind, FilesTab, Layout, OpenWith, SortBy, base_name, fmt_time};
use crate::appearance::{rpx, ts};
use crate::remote_files::human;
use crate::theme::{MONO, Palette};
use crate::ui;

/// Which menu is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuKind {
    /// Right click on selected files and folders.
    Items,
    /// Right click on the folder's empty space.
    Folder,
    /// The command bar's New.
    New,
    /// The command bar's Sort.
    Sort,
    /// Right click on search results.
    Found,
}

/// An open menu: its pane, where, and which.
#[derive(Clone, Debug)]
pub struct FileMenu {
    pub pane: usize,
    pub at: Point<Pixels>,
    pub kind: MenuKind,
}

/// What a menu line does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Act {
    Open,
    OpenHere,
    OpenSystem,
    OpenTerminal,
    Cut,
    Copy,
    Paste,
    PasteInto,
    Duplicate,
    CopyAcross,
    Rename,
    Delete,
    CopyPath,
    CopyName,
    Reveal,
    Pin,
    Properties,
    NewFolder,
    NewFile,
    Sort(SortBy),
    Hidden,
    SelectAll,
    Refresh,
    FolderPath,
    OpenContaining,
}

/// A menu line.
enum Line {
    /// Label, shortcut, action, check mark, red.
    Item(&'static str, &'static str, Act, bool, bool),
    Head(&'static str),
    Sep,
}

fn item(label: &'static str, key: &'static str, act: Act) -> Line {
    Line::Item(label, key, act, false, false)
}

impl FilesTab {
    /// The lines of the open menu.
    fn menu_lines(&self, m: &FileMenu) -> Vec<Line> {
        let pane = &self.panes[m.pane];
        let local = pane.fs == FsRef::Local;
        let picked = pane.selected_entries();
        let one = picked.len() == 1;
        let one_dir = one && picked[0].is_dir();
        let files: Vec<_> = picked.iter().filter(|e| !e.is_dir()).collect();
        let reveal = if cfg!(target_os = "macos") {
            "Reveal in Finder"
        } else if cfg!(windows) {
            "Show in File Explorer"
        } else {
            "Show in file manager"
        };
        let sort = |label, by| {
            let on = pane.sort.0 == by;
            Line::Item(
                label,
                if on && !pane.sort.1 { "↓" } else { "" },
                Act::Sort(by),
                on,
                false,
            )
        };
        match m.kind {
            MenuKind::Items => {
                let mut v = vec![item("Open", "↵", Act::Open)];
                if !files.is_empty() {
                    // The other way of opening than the double click's.
                    if files.iter().all(|e| FileKind::of(&e.name).opens_here()) {
                        v.push(item("Open with default app", "", Act::OpenSystem));
                    } else {
                        v.push(item("Open in Switchyard", "", Act::OpenHere));
                    }
                }
                if one_dir && pane.has_shell() {
                    v.push(item("Open in terminal", "", Act::OpenTerminal));
                }
                v.push(Line::Sep);
                v.push(item(
                    "Cut",
                    if cfg!(target_os = "macos") {
                        "⌘X"
                    } else {
                        "Ctrl+X"
                    },
                    Act::Cut,
                ));
                v.push(item(
                    "Copy",
                    if cfg!(target_os = "macos") {
                        "⌘C"
                    } else {
                        "Ctrl+C"
                    },
                    Act::Copy,
                ));
                if one_dir && self.clip.is_some() {
                    v.push(item("Paste into folder", "", Act::PasteInto));
                }
                v.push(item("Duplicate", "", Act::Duplicate));
                if self.layout == Layout::Dual {
                    v.push(item(
                        if m.pane == 0 {
                            "Copy to right pane"
                        } else {
                            "Copy to left pane"
                        },
                        "",
                        Act::CopyAcross,
                    ));
                }
                v.push(Line::Sep);
                if one {
                    v.push(item("Rename", "F2", Act::Rename));
                }
                v.push(Line::Item("Delete", "Del", Act::Delete, false, true));
                v.push(Line::Sep);
                v.push(item(
                    "Copy path",
                    if cfg!(target_os = "macos") {
                        "⇧⌘C"
                    } else {
                        "Ctrl+Shift+C"
                    },
                    Act::CopyPath,
                ));
                v.push(item("Copy name", "", Act::CopyName));
                if one_dir && local {
                    let path = pane.path.as_ref().map(|d| pane.join(d, &picked[0].name));
                    let pinned = path
                        .is_some_and(|p| self.pinned.contains(&p.to_string_lossy().into_owned()));
                    v.push(item(
                        if pinned {
                            "Unpin from quick access"
                        } else {
                            "Pin to quick access"
                        },
                        "",
                        Act::Pin,
                    ));
                }
                if local {
                    v.push(item(reveal, "", Act::Reveal));
                }
                v.push(Line::Sep);
                v.push(item(
                    "Properties",
                    if cfg!(target_os = "macos") {
                        "⌘I"
                    } else {
                        "Ctrl+I"
                    },
                    Act::Properties,
                ));
                v
            }
            MenuKind::Folder => {
                let mut v = vec![
                    item(
                        "New folder",
                        if cfg!(target_os = "macos") {
                            "⇧⌘N"
                        } else {
                            "Ctrl+Shift+N"
                        },
                        Act::NewFolder,
                    ),
                    item("New file", "", Act::NewFile),
                ];
                if self.clip.is_some() {
                    v.push(item(
                        "Paste",
                        if cfg!(target_os = "macos") {
                            "⌘V"
                        } else {
                            "Ctrl+V"
                        },
                        Act::Paste,
                    ));
                }
                v.push(Line::Sep);
                if pane.has_shell() {
                    v.push(item("Open terminal here", "", Act::OpenTerminal));
                }
                v.push(item("Copy folder path", "", Act::FolderPath));
                if local {
                    v.push(item(reveal, "", Act::Reveal));
                    let pinned = pane
                        .path
                        .as_ref()
                        .is_some_and(|p| self.pinned.contains(&p.to_string_lossy().into_owned()));
                    v.push(item(
                        if pinned {
                            "Unpin from quick access"
                        } else {
                            "Pin to quick access"
                        },
                        "",
                        Act::Pin,
                    ));
                }
                v.push(Line::Sep);
                v.push(Line::Head("Sort by"));
                v.push(sort("Name", SortBy::Name));
                v.push(sort("Date modified", SortBy::Modified));
                v.push(sort("Type", SortBy::Type));
                v.push(sort("Size", SortBy::Size));
                v.push(Line::Sep);
                v.push(Line::Item(
                    "Show hidden files",
                    "",
                    Act::Hidden,
                    pane.show_hidden,
                    false,
                ));
                v.push(item(
                    "Select all",
                    if cfg!(target_os = "macos") {
                        "⌘A"
                    } else {
                        "Ctrl+A"
                    },
                    Act::SelectAll,
                ));
                v.push(item("Refresh", "F5", Act::Refresh));
                v
            }
            MenuKind::Found => {
                let one = self
                    .found_cursor(m.pane)
                    .filter(|_| pane.found.as_ref().is_some_and(|f| f.selected.len() == 1));
                let mut v = vec![item("Open", "↵", Act::Open)];
                if let Some((_, e)) = &one
                    && !e.is_dir()
                {
                    v.push(if FileKind::of(&e.name).opens_here() {
                        item("Open with default app", "", Act::OpenSystem)
                    } else {
                        item("Open in Switchyard", "", Act::OpenHere)
                    });
                }
                v.push(item("Open containing folder", "", Act::OpenContaining));
                v.push(Line::Sep);
                v.push(item(
                    "Cut",
                    if cfg!(target_os = "macos") {
                        "⌘X"
                    } else {
                        "Ctrl+X"
                    },
                    Act::Cut,
                ));
                v.push(item(
                    "Copy",
                    if cfg!(target_os = "macos") {
                        "⌘C"
                    } else {
                        "Ctrl+C"
                    },
                    Act::Copy,
                ));
                v.push(Line::Item("Delete", "Del", Act::Delete, false, true));
                v.push(Line::Sep);
                v.push(item(
                    "Copy path",
                    if cfg!(target_os = "macos") {
                        "⇧⌘C"
                    } else {
                        "Ctrl+Shift+C"
                    },
                    Act::CopyPath,
                ));
                v.push(item("Copy name", "", Act::CopyName));
                if local {
                    v.push(item(reveal, "", Act::Reveal));
                }
                v
            }
            MenuKind::New => vec![
                item(
                    "Folder",
                    if cfg!(target_os = "macos") {
                        "⇧⌘N"
                    } else {
                        "Ctrl+Shift+N"
                    },
                    Act::NewFolder,
                ),
                item("Text file", "", Act::NewFile),
            ],
            MenuKind::Sort => vec![
                sort("Name", SortBy::Name),
                sort("Date modified", SortBy::Modified),
                sort("Type", SortBy::Type),
                sort("Size", SortBy::Size),
                Line::Sep,
                Line::Item(
                    "Show hidden files",
                    "",
                    Act::Hidden,
                    pane.show_hidden,
                    false,
                ),
            ],
        }
    }

    fn run_act(
        &mut self,
        ix: usize,
        kind: MenuKind,
        act: Act,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_menus();
        window.focus(&self.focus, cx);
        let pane = &self.panes[ix];
        // On the folder's own menu, "here" is the folder shown.
        let target: Option<PathBuf> = match kind {
            MenuKind::Items => pane
                .selected
                .last()
                .and_then(|n| pane.path.as_ref().map(|d| pane.join(d, n))),
            _ => pane.path.clone(),
        };
        match act {
            Act::Open => self.open_selection(ix, OpenWith::Default, cx),
            Act::OpenHere => self.open_selection(ix, OpenWith::Here, cx),
            Act::OpenSystem => self.open_selection(ix, OpenWith::System, cx),
            Act::OpenTerminal => {
                if kind != MenuKind::Items {
                    self.panes[ix].selected.clear();
                }
                self.run_here(ix, None, cx)
            }
            Act::Cut => self.clip_selection(ix, true, cx),
            Act::Copy => self.clip_selection(ix, false, cx),
            Act::Paste => self.paste(ix, None, cx),
            Act::PasteInto => self.paste(ix, target, cx),
            Act::Duplicate => self.duplicate(ix, cx),
            Act::CopyAcross => self.copy_selection_across(ix, cx),
            Act::Rename => self.start_rename(ix, window, cx),
            Act::Delete => self.delete(ix, cx),
            Act::CopyPath => self.copy_paths(ix, false, cx),
            Act::CopyName => self.copy_paths(ix, true, cx),
            Act::FolderPath => {
                self.panes[ix].selected.clear();
                self.copy_paths(ix, false, cx)
            }
            Act::Reveal => {
                if kind != MenuKind::Items {
                    self.panes[ix].selected.clear();
                }
                self.reveal_in_os(ix, cx)
            }
            Act::Pin => {
                if let Some(t) = target {
                    self.toggle_pin(t, cx)
                }
            }
            Act::Properties => {
                self.properties = Some(ix);
                cx.notify();
            }
            Act::NewFolder => self.new_folder(ix, cx),
            Act::NewFile => self.new_file(ix, cx),
            Act::Sort(by) => {
                let s = &mut self.panes[ix].sort;
                *s = if s.0 == by { (by, !s.1) } else { (by, true) };
                cx.notify();
            }
            Act::Hidden => self.toggle_hidden(cx),
            Act::SelectAll => {
                let names = self.panes[ix]
                    .visible()
                    .iter()
                    .map(|e| e.name.clone())
                    .collect();
                self.panes[ix].selected = names;
                cx.notify();
            }
            Act::Refresh => self.refresh(ix, cx),
            Act::OpenContaining => self.open_found(ix, OpenWith::Default, true, cx),
        }
    }

    /// A popup panel that closes on a click outside it (placed by `anchored`).
    fn popup(
        &self,
        id: &'static str,
        width: f32,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        div()
            .id(id)
            .w(rpx(width))
            .p(rpx(4.))
            .bg(p.elev)
            .border_1()
            .border_color(p.bd2)
            .rounded(px(7.))
            .shadow(ui::shadow(p))
            .text_size(ts::UI)
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                if this.menu.is_some() || this.code_menu.is_some() {
                    this.close_menus();
                    cx.notify();
                }
            }))
    }

    /// The open context, New or Sort menu.
    pub(super) fn render_menu(
        &mut self,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let m = self.menu.clone()?;
        let lines = self.menu_lines(&m);
        let (ix, kind) = (m.pane, m.kind);
        let mut panel = self.popup("files-menu", 236., p, cx);
        for (i, line) in lines.into_iter().enumerate() {
            panel = match line {
                Line::Sep => panel.child(div().h(px(1.)).bg(p.bd).my(rpx(4.))),
                Line::Head(label) => panel.child(
                    div()
                        .px(rpx(8.))
                        .pt(rpx(2.))
                        .pb(rpx(2.))
                        .text_size(ts::SMALL)
                        .text_color(p.fg3)
                        .child(label),
                ),
                Line::Item(label, key, act, checked, danger) => panel.child(
                    div()
                        .id(("files-menu-item", i))
                        .h(rpx(26.))
                        .px(rpx(8.))
                        .flex()
                        .items_center()
                        .gap(rpx(6.))
                        .rounded(px(4.))
                        .cursor_pointer()
                        .text_color(if danger { p.prod } else { p.fg })
                        .hover(|s| s.bg(p.sel))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.run_act(ix, kind, act, window, cx)
                        }))
                        .child(
                            div()
                                .w(rpx(12.))
                                .flex_none()
                                .text_color(p.acc)
                                .child(if checked { "✓" } else { "" }),
                        )
                        .child(div().flex_1().child(label))
                        .child(
                            div()
                                .font_family(MONO)
                                .text_size(ts::CAPTION_PLUS)
                                .text_color(p.fg3)
                                .child(key),
                        ),
                ),
            };
        }
        Some(
            deferred(
                anchored()
                    .position(m.at)
                    .snap_to_window_with_margin(px(8.))
                    .child(panel),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }

    /// The "Code" menu: run a coding CLI in the folder.
    pub(super) fn render_code_menu(
        &mut self,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (ix, at) = self.code_menu?;
        let pane = &self.panes[ix];
        let repo = pane.fs == FsRef::Local && pane.git.is_some();
        let worktree = self.use_worktree && repo;
        let target = self
            .run_dir(ix)
            .map(|d| crate::coding_clis::folder_name(&d))
            .unwrap_or_default();
        let panel = self
            .popup("files-code-menu", 290., p, cx)
            .child(
                div()
                    .px(rpx(8.))
                    .py(rpx(4.))
                    .text_size(ts::SMALL)
                    .text_color(p.fg3)
                    .child(format!("Run a coding CLI in {target}")),
            )
            .children(self.clis.iter().cloned().enumerate().map(|(i, c)| {
                let status = c.status.clone();
                let missing = c.missing;
                div()
                    .id(SharedString::from(format!("f{ix}-cli-{i}")))
                    .px(rpx(8.))
                    .h(rpx(26.))
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover(|s| s.bg(p.sel))
                    .child(div().flex_1().child(c.name.clone()))
                    .children(status.map(|s| {
                        div()
                            .text_size(ts::SMALL)
                            .text_color(if missing { p.stg } else { p.fg3 })
                            .child(s)
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| this.run_here(ix, Some(c.clone()), cx)))
            }))
            .when(repo, |d| {
                d.child(div().my(rpx(4.)).h(px(1.)).bg(p.bd)).child(
                    div()
                        .id(SharedString::from(format!("f{ix}-worktree")))
                        .px(rpx(8.))
                        .py(rpx(4.))
                        .flex()
                        .flex_col()
                        .gap(rpx(2.))
                        .rounded(px(4.))
                        .cursor_pointer()
                        .hover(|s| s.bg(p.sel))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.use_worktree = !this.use_worktree;
                            cx.notify();
                        }))
                        .child(div().child(if worktree {
                            "● In a new git worktree"
                        } else {
                            "○ In a new git worktree"
                        }))
                        .child(div().text_size(ts::SMALL).text_color(p.fg3).child(
                            "Its own branch and folder, so several CLIs can work on this repository at once",
                        )),
                )
            });
        Some(
            deferred(
                anchored()
                    .position(at)
                    .snap_to_window_with_margin(px(8.))
                    .child(panel),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }

    /// The Properties card of the selection (or the folder).
    pub(super) fn render_properties(
        &mut self,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let ix = self.properties?;
        let pane = &self.panes[ix];
        let dir = pane.path.clone()?;
        let picked = pane.selected_entries();
        let location = if pane.fs.is_remote() {
            format!("{} · {}", pane.label, dir.to_string_lossy())
        } else {
            dir.to_string_lossy().into_owned()
        };
        let mut rows: Vec<(&'static str, String)> = Vec::new();
        let (icon, title) = match picked.as_slice() {
            [] => {
                rows.push(("Type", "Folder".into()));
                rows.push(("Path", location.clone()));
                rows.push(("Contains", format!("{} items", pane.entries.len())));
                ("icons/swy-folder.svg", base_name(&dir))
            }
            [e] => {
                let kind = if e.is_dir() {
                    FileKind::Folder
                } else {
                    FileKind::of(&e.name)
                };
                rows.push((
                    "Type",
                    if e.is_dir() {
                        "Folder".into()
                    } else {
                        type_label(&e.name, kind)
                    },
                ));
                rows.push(("Location", location.clone()));
                if !e.is_dir() {
                    rows.push(("Size", format!("{} ({} bytes)", human(e.size), e.size)));
                    rows.push((
                        "Opens in",
                        if kind.opens_here() {
                            "Switchyard"
                        } else {
                            "The default app"
                        }
                        .into(),
                    ));
                }
                if let Some(m) = e.modified_ms {
                    rows.push(("Modified", fmt_time(m)));
                }
                if e.mode.is_some() {
                    rows.push(("Permissions", e.mode_string()));
                }
                if let switchyard_core::remote::EntryKind::Symlink(t) = &e.kind {
                    rows.push(("Link to", t.clone()));
                }
                (kind.icon(), e.name.clone())
            }
            many => {
                let files: Vec<_> = many.iter().filter(|e| !e.is_dir()).collect();
                let bytes: u64 = files.iter().map(|e| e.size).sum();
                rows.push(("Location", location.clone()));
                rows.push((
                    "Contains",
                    format!(
                        "{} files, {} folders",
                        files.len(),
                        many.len() - files.len()
                    ),
                ));
                rows.push(("Size of files", human(bytes)));
                ("icons/swy-files.svg", format!("{} items", many.len()))
            }
        };
        let icon = if icon == "icons/swy-files.svg" {
            "icons/swy-file.svg"
        } else {
            icon
        };
        Some(
            deferred(
                div()
                    .id("files-props-scrim")
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(p.scrim)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.properties = None;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .id("files-props")
                            .w(rpx(380.))
                            .p(rpx(16.))
                            .flex()
                            .flex_col()
                            .gap(rpx(10.))
                            .bg(p.elev)
                            .border_1()
                            .border_color(p.bd2)
                            .rounded(px(8.))
                            .shadow(ui::shadow(p))
                            .on_click(|_, _, cx| cx.stop_propagation())
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(rpx(10.))
                                    .child(svg().path(icon).size(rpx(22.)).text_color(p.acc))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(ts::TITLE)
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(title),
                                    ),
                            )
                            .child(div().h(px(1.)).bg(p.bd))
                            .children(rows.into_iter().map(|(k, v)| {
                                div()
                                    .flex()
                                    .gap(rpx(10.))
                                    .text_size(ts::BODY)
                                    .child(div().w(rpx(90.)).flex_none().text_color(p.fg3).child(k))
                                    .child(div().flex_1().min_w_0().text_color(p.fg).child(v))
                            }))
                            .child(
                                div().flex().justify_end().child(
                                    ui::button(
                                        "files-props-close",
                                        "Close",
                                        ui::Kind::Secondary,
                                        p,
                                    )
                                    .h(rpx(24.))
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            this.properties = None;
                                            cx.notify();
                                        },
                                    )),
                                ),
                            ),
                    ),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }
}
