//! Drawing the Files tab: the command bar, each pane's navigation row (back, forward, up,
//! refresh, address, search), the column header, the rows and the status bar.

use std::collections::HashSet;
use std::path::PathBuf;

use gpui_kit::component::input::Input;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Div, ExternalPaths, FontWeight, InteractiveElement as _,
    IntoElement, MouseButton, MouseDownEvent, ParentElement as _, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, div, px, svg, uniform_list,
};
use switchyard_core::FsRef;
use switchyard_core::git::GitStatus;
use switchyard_core::remote::EntryKind;

use super::kinds::type_label;
use super::menu::{FileMenu, MenuKind};
use super::{
    DragPreview, DraggedFiles, FileKind, FilesTab, Layout, OpenWith, PAGE, SortBy, change_color,
    fmt_time,
};
use crate::appearance::{rpx, ts};
use crate::remote_files::human;
use crate::theme::{MONO, Palette, palette};
use crate::ui::{self, Kind};

/// Widths of the Date modified, Type, Size and Permissions columns.
const W_MODIFIED: f32 = 128.;
const W_TYPE: f32 = 118.;
const W_SIZE: f32 = 76.;
const W_MODE: f32 = 84.;
/// Width of a search result's Folder column.
const W_FOLDER: f32 = 220.;

/// A command bar button: an icon, an optional label, dimmed when it can't run.
fn bar_button(
    id: impl Into<SharedString>,
    icon: &'static str,
    label: Option<&'static str>,
    tip: &'static str,
    enabled: bool,
    p: &Palette,
) -> Stateful<Div> {
    let color = if enabled { p.fg2 } else { p.fg3 };
    div()
        .id(id.into())
        .h(rpx(26.))
        .px(rpx(7.))
        .flex()
        .items_center()
        .gap(rpx(5.))
        .rounded(px(5.))
        .text_size(ts::LABEL)
        .text_color(color)
        .when(!enabled, |d| d.opacity(0.45))
        .when(enabled, |d| d.cursor_pointer().hover(|s| s.bg(p.hover)))
        .tooltip(move |w, cx| Tooltip::new(tip).build(w, cx))
        .child(svg().path(icon).size(rpx(15.)).text_color(color))
        .children(label)
}

/// A thin vertical divider between groups of buttons.
fn bar_sep(p: &Palette) -> Div {
    div().w(px(1.)).h(rpx(16.)).mx(rpx(4.)).bg(p.bd2)
}

/// A navigation arrow.
fn nav_button(
    id: String,
    icon: &'static str,
    tip: &'static str,
    on: bool,
    p: &Palette,
) -> Stateful<Div> {
    div()
        .id(SharedString::from(id))
        .size(rpx(26.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.))
        .when(on, |d| d.cursor_pointer().hover(|s| s.bg(p.hover)))
        .tooltip(move |w, cx| Tooltip::new(tip).build(w, cx))
        .child(
            svg()
                .path(icon)
                .size(rpx(15.))
                .text_color(if on { p.fg2 } else { p.bd2 }),
        )
}

impl FilesTab {
    /// The command bar above the panes; acts on the active pane.
    pub(super) fn render_command_bar(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let ix = self.active;
        let pane = &self.panes[ix];
        let n_sel = pane.selected.len();
        let has_dir = pane.path.is_some();
        let can_paste = self.clip.is_some() && has_dir;
        let dual = self.layout == Layout::Dual;
        let shell = pane.has_shell() && has_dir;
        let hidden = pane.show_hidden;
        div()
            .h(rpx(38.))
            .flex_none()
            .px(rpx(8.))
            .flex()
            .items_center()
            .gap(rpx(1.))
            .border_b_1()
            .border_color(p.bd)
            .bg(p.panel)
            .child(
                bar_button("files-new", "icons/swy-folder-plus.svg", Some("New"), "New folder or file", has_dir, p)
                    .text_color(p.acc)
                    .child(svg().path("icons/swy-chevron-down.svg").size(rpx(11.)).text_color(p.fg3))
                    .on_click(cx.listener(move |this, ev: &gpui_kit::ClickEvent, _, cx| {
                        this.open_menu(this.active, MenuKind::New, ev.position(), cx)
                    })),
            )
            .child(bar_sep(p))
            .child(
                bar_button("files-cut", "icons/swy-scissors.svg", None, "Cut · Ctrl/Cmd+X", n_sel > 0, p)
                    .when(n_sel > 0, |d| {
                        d.on_click(cx.listener(move |this, _, _, cx| this.clip_selection(ix, true, cx)))
                    }),
            )
            .child(
                bar_button("files-copy", "icons/swy-copy.svg", None, "Copy · Ctrl/Cmd+C", n_sel > 0, p)
                    .when(n_sel > 0, |d| {
                        d.on_click(cx.listener(move |this, _, _, cx| this.clip_selection(ix, false, cx)))
                    }),
            )
            .child(
                bar_button("files-paste", "icons/swy-clipboard-paste.svg", None, "Paste · Ctrl/Cmd+V", can_paste, p)
                    .when(can_paste, |d| {
                        d.on_click(cx.listener(move |this, _, _, cx| this.paste(ix, None, cx)))
                    }),
            )
            .child(
                bar_button("files-rename", "icons/swy-pencil.svg", None, "Rename · F2", n_sel == 1, p)
                    .when(n_sel == 1, |d| {
                        d.on_click(cx.listener(move |this, _, w, cx| this.start_rename(ix, w, cx)))
                    }),
            )
            .child(
                bar_button("files-delete", "icons/swy-trash.svg", None, "Delete · Del", n_sel > 0, p)
                    .when(n_sel > 0, |d| {
                        d.on_click(cx.listener(move |this, _, _, cx| this.delete(ix, cx)))
                    }),
            )
            .child(bar_sep(p))
            .child(
                bar_button("files-sort", "icons/swy-arrow-up-down.svg", Some("Sort"), "Sort by", true, p)
                    .child(svg().path("icons/swy-chevron-down.svg").size(rpx(11.)).text_color(p.fg3))
                    .on_click(cx.listener(move |this, ev: &gpui_kit::ClickEvent, _, cx| {
                        this.open_menu(this.active, MenuKind::Sort, ev.position(), cx)
                    })),
            )
            .child(
                bar_button(
                    "files-hidden",
                    "icons/swy-eye.svg",
                    Some(if hidden { "Hidden ●" } else { "Hidden" }),
                    "Show hidden files",
                    true,
                    p,
                )
                .when(hidden, |d| d.text_color(p.acc))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_hidden(cx))),
            )
            .when(shell, |d| {
                d.child(bar_sep(p))
                    .child(
                        bar_button(
                            "files-term",
                            "icons/swy-terminal.svg",
                            Some("Terminal"),
                            "A terminal in this folder (or the selected one), files beside it · Ctrl/Cmd+`",
                            true,
                            p,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.run_here(ix, None, cx))),
                    )
                    .child(
                        bar_button(
                            "files-code",
                            "icons/swy-code.svg",
                            Some("Code"),
                            "Run a coding CLI in this folder",
                            true,
                            p,
                        )
                        .text_color(p.acc)
                        .child(svg().path("icons/swy-chevron-down.svg").size(rpx(11.)).text_color(p.fg3))
                        .on_click(cx.listener(move |this, ev: &gpui_kit::ClickEvent, _, cx| {
                            this.menu = None;
                            this.code_menu = Some((ix, ev.position()));
                            cx.notify();
                        })),
                    )
            })
            .child(div().flex_1())
            .when(dual && n_sel > 0, |d| {
                d.child(
                    ui::button(
                        "files-copy-across",
                        if ix == 0 { "Copy →" } else { "← Copy" },
                        Kind::Primary,
                        p,
                    )
                    .h(rpx(24.))
                    .text_size(ts::LABEL)
                    .on_click(cx.listener(move |this, _, _, cx| this.copy_selection_across(ix, cx))),
                )
            })
            .child(
                bar_button(
                    "files-places-toggle",
                    "icons/swy-hard-drive.svg",
                    None,
                    if self.show_places { "Hide places" } else { "Show places" },
                    true,
                    p,
                )
                .when(self.show_places, |d| d.text_color(p.acc))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.show_places = !this.show_places;
                    this.save_settings();
                    cx.notify();
                })),
            )
            .child(
                bar_button(
                    "files-layout",
                    "icons/swy-columns-2.svg",
                    Some(if dual { "2 panes" } else { "1 pane" }),
                    if dual { "Back to one pane" } else { "Two panes side by side (copy between computers)" },
                    true,
                    p,
                )
                .when(dual, |d| d.text_color(p.acc))
                .on_click(cx.listener(|this, _, _, cx| {
                    let next = if this.layout == Layout::Dual { Layout::Single } else { Layout::Dual };
                    this.set_layout(next, cx);
                    this.save_settings();
                })),
            )
            .into_any_element()
    }

    pub(super) fn toggle_hidden(&mut self, cx: &mut Context<Self>) {
        self.close_menus();
        let on = !self.panes[self.active].show_hidden;
        for pane in &mut self.panes {
            pane.show_hidden = on;
        }
        self.save_settings();
        cx.notify();
    }

    pub(super) fn open_menu(
        &mut self,
        ix: usize,
        kind: MenuKind,
        at: gpui_kit::Point<gpui_kit::Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.code_menu = None;
        self.active = ix;
        self.menu = Some(FileMenu { pane: ix, at, kind });
        cx.notify();
    }

    /// The navigation row: arrows, the address bar and the search box.
    fn render_nav(&mut self, ix: usize, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let pane = &self.panes[ix];
        let compact = self.layout == Layout::Sidebar;
        let crumbs = pane.crumbs();
        let goto = self
            .goto
            .as_ref()
            .filter(|(i, ..)| *i == ix)
            .map(|(_, input, _)| input.clone());
        let remote = pane.fs.is_remote();
        let source_chip = (remote || compact).then(|| {
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(rpx(5.))
                .pr(rpx(4.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.fg)
                .when(remote, |d| {
                    d.child(
                        svg()
                            .path(match pane.fs {
                                FsRef::Host(_) => "icons/swy-server.svg",
                                _ => "icons/swy-cloud.svg",
                            })
                            .size(rpx(13.))
                            .text_color(p.acc),
                    )
                })
                .child(pane.label.clone())
        });
        let address = div()
            .id(SharedString::from(format!("f{ix}-path")))
            .flex_1()
            .min_w_0()
            .h(rpx(26.))
            .px(rpx(8.))
            .flex()
            .items_center()
            .gap(rpx(2.))
            .overflow_hidden()
            .rounded(px(5.))
            .border_1()
            .border_color(p.bd2)
            .bg(p.bg)
            .text_size(ts::LABEL)
            .children(source_chip)
            .when_some(goto, |d, input| {
                d.border_color(p.acc)
                    .child(Input::new(&input).appearance(false).text_size(ts::LABEL))
            })
            .when(
                self.goto.as_ref().is_none_or(|(i, ..)| *i != ix) && !compact,
                |d| {
                    d.cursor_text()
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.open_goto(ix, window, cx)),
                        )
                        .children(crumbs.into_iter().enumerate().map(|(i, (label, path))| {
                            div()
                                .flex()
                                .flex_none()
                                .items_center()
                                .gap(rpx(2.))
                                .when(i > 0, |d| {
                                    d.child(
                                        svg()
                                            .path("icons/swy-chevron-right.svg")
                                            .size(rpx(11.))
                                            .text_color(p.fg3),
                                    )
                                })
                                .child(
                                    div()
                                        .id(SharedString::from(format!("f{ix}-crumb-{i}")))
                                        .px(rpx(4.))
                                        .rounded(px(3.))
                                        .text_color(p.fg2)
                                        .hover(|s| s.bg(p.hover).text_color(p.fg))
                                        .cursor_pointer()
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.navigate(ix, Some(path.clone()), cx)
                                        }))
                                        .child(label),
                                )
                        }))
                        .child(div().flex_1().h_full())
                },
            );
        let (can_back, can_fwd) = (!pane.back.is_empty(), !pane.forward.is_empty());
        let can_up = pane.path.as_ref().and_then(|d| pane.parent_of(d)).is_some();
        let search = self.search[ix].as_ref().map(|(input, _)| {
            div()
                .w(if compact { rpx(110.) } else { rpx(210.) })
                .flex_none()
                .h(rpx(26.))
                .flex()
                .items_center()
                .gap(rpx(6.))
                .px(rpx(8.))
                .rounded(px(5.))
                .border_1()
                .border_color(if pane.filter.is_empty() { p.bd2 } else { p.acc })
                .bg(p.bg)
                .child(
                    svg()
                        .path("icons/swy-search.svg")
                        .size(rpx(13.))
                        .text_color(p.fg3),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Input::new(input).appearance(false).text_size(ts::LABEL)),
                )
                .when(!pane.filter.is_empty(), |d| {
                    d.child(
                        div()
                            .id(SharedString::from(format!("f{ix}-search-clear")))
                            .cursor_pointer()
                            .text_size(ts::SMALL)
                            .text_color(p.fg3)
                            .hover(|s| s.text_color(p.fg))
                            .on_click(cx.listener(move |this, _, _, cx| this.clear_search(ix, cx)))
                            .child("✕"),
                    )
                })
        });
        div()
            .h(rpx(36.))
            .flex_none()
            .px(rpx(6.))
            .flex()
            .items_center()
            .gap(rpx(2.))
            .border_b_1()
            .border_color(p.bd)
            .when(!compact, |d| {
                d.child(
                    nav_button(
                        format!("f{ix}-back"),
                        "icons/swy-arrow-left.svg",
                        "Back · Alt+←",
                        can_back,
                        p,
                    )
                    .when(can_back, |d| {
                        d.on_click(cx.listener(move |this, _, _, cx| this.go_back(ix, cx)))
                    }),
                )
                .child(
                    nav_button(
                        format!("f{ix}-fwd"),
                        "icons/swy-arrow-right.svg",
                        "Forward · Alt+→",
                        can_fwd,
                        p,
                    )
                    .when(can_fwd, |d| {
                        d.on_click(cx.listener(move |this, _, _, cx| this.go_forward(ix, cx)))
                    }),
                )
            })
            .child(
                nav_button(
                    format!("f{ix}-up"),
                    "icons/swy-arrow-up.svg",
                    "Up · Alt+↑",
                    can_up,
                    p,
                )
                .when(can_up, |d| {
                    d.on_click(cx.listener(move |this, _, _, cx| this.up(ix, cx)))
                }),
            )
            .child(
                nav_button(
                    format!("f{ix}-refresh"),
                    "icons/swy-refresh-cw.svg",
                    "Refresh · F5",
                    true,
                    p,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.refresh(ix, cx))),
            )
            .child(div().w(rpx(4.)))
            .child(address)
            .child(div().w(rpx(4.)))
            .children(search)
            .into_any_element()
    }

    pub(super) fn render_pane(
        &mut self,
        ix: usize,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let nav = self.render_nav(ix, p, cx);
        let pane = &self.panes[ix];
        let rows = pane.visible();
        let count = rows.len();
        // A background refresh (live folders) shows no loading state.
        let loading = pane.request.is_some() && !pane.quiet;
        let active = self.active == ix;
        let n_sel = pane.selected.len();
        let full = self.layout != Layout::Sidebar;
        let mode_col = full && matches!(pane.fs, FsRef::Host(_));
        let finding = pane.found.is_some();

        let (by, asc) = pane.sort;
        let head = |id: &'static str, label: &'static str, col: SortBy| {
            let mark = if by == col {
                if asc { " ↑" } else { " ↓" }
            } else {
                ""
            };
            div()
                .id(SharedString::from(format!("f{ix}-{id}")))
                .cursor_pointer()
                .hover(|s| s.text_color(p.fg))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let s = &mut this.panes[ix].sort;
                    *s = if s.0 == col { (col, !s.1) } else { (col, true) };
                    cx.notify();
                }))
                .child(format!("{label}{mark}"))
        };
        let header = div()
            .h(rpx(26.))
            .flex_none()
            .flex()
            .items_center()
            .px(rpx(10.))
            .gap(rpx(10.))
            .text_size(ts::SMALL)
            .text_color(p.fg2)
            .border_b_1()
            .border_color(p.bd)
            .when(finding, |d| {
                d.child(div().flex_1().pl(rpx(25.)).child("Name"))
                    .when(full, |d| {
                        d.child(div().w(rpx(W_FOLDER)).flex_none().child("Folder"))
                            .child(div().w(rpx(W_MODIFIED)).flex_none().child("Date modified"))
                            .child(
                                div()
                                    .w(rpx(W_SIZE))
                                    .flex_none()
                                    .flex()
                                    .justify_end()
                                    .child("Size"),
                            )
                    })
            })
            .when(!finding, |d| {
                d.child(
                    div()
                        .flex_1()
                        .pl(rpx(22.))
                        .child(head("sort-name", "Name", SortBy::Name)),
                )
            })
            .when(full && !finding, |d| {
                d.child(div().w(rpx(W_MODIFIED)).flex_none().child(head(
                    "sort-mod",
                    "Date modified",
                    SortBy::Modified,
                )))
                .child(div().w(rpx(W_TYPE)).flex_none().child(head(
                    "sort-type",
                    "Type",
                    SortBy::Type,
                )))
                .child(
                    div()
                        .w(rpx(W_SIZE))
                        .flex_none()
                        .flex()
                        .justify_end()
                        .child(head("sort-size", "Size", SortBy::Size)),
                )
                .when(mode_col, |d| {
                    d.child(div().w(rpx(W_MODE)).flex_none().child("Permissions"))
                })
            });

        let filter = pane.filter.trim().to_owned();
        let search_hint: Option<AnyElement> = {
            let hint: Option<String> = if !pane.prefix.is_empty() && filter == pane.prefix {
                Some(format!("Names starting with “{}”", pane.prefix))
            } else if pane.paged() && !filter.is_empty() {
                Some("Enter searches the whole folder".into())
            } else {
                None
            };
            hint.map(|h| {
                div()
                    .flex_none()
                    .px(rpx(10.))
                    .py(rpx(3.))
                    .text_size(ts::SMALL)
                    .text_color(p.fg3)
                    .border_b_1()
                    .border_color(p.bd)
                    .child(h)
                    .into_any_element()
            })
        };
        let loading_bar = (loading && !pane.appending && pane.path.is_some())
            .then(|| ui::loading_bar(SharedString::from(format!("f{ix}-loading")), p));

        let footer: Option<AnyElement> = pane.path.is_some().then(|| {
            let loaded = pane.entries.len();
            let more = if pane.next.is_some() { "+" } else { "" };
            let visible_total = rows.len();
            let status: AnyElement = if let Some(f) = &pane.found {
                let n = f.hits.len();
                let what = match (&f.error, f.done, f.truncated) {
                    (Some(e), ..) => format!("Search failed: {e}"),
                    (None, false, _) => format!("Searching subfolders… {n} found"),
                    (None, true, true) => {
                        format!(
                            "First {n} matches (narrow the search for more) · {}",
                            f.engine
                        )
                    }
                    (None, true, false) => {
                        format!("{n} found in this folder and below · {}", f.engine)
                    }
                };
                div()
                    .flex()
                    .items_center()
                    .gap(rpx(6.))
                    .when(!f.done, |d| {
                        d.child(ui::pulse_dot(
                            SharedString::from(format!("f{ix}-finding")),
                            p.acc,
                            6.,
                        ))
                    })
                    .child(what)
                    .into_any_element()
            } else if loading {
                div()
                    .flex()
                    .items_center()
                    .gap(rpx(6.))
                    .child(ui::pulse_dot(
                        SharedString::from(format!("f{ix}-busy")),
                        p.acc,
                        6.,
                    ))
                    .child(if pane.appending {
                        "Loading more…"
                    } else {
                        "Loading…"
                    })
                    .into_any_element()
            } else if filter.is_empty() {
                format!("{visible_total}{more} items").into_any_element()
            } else {
                format!("{visible_total} of {loaded}{more} items").into_any_element()
            };
            let picked = pane.selected_entries();
            let files = picked.iter().filter(|e| !e.is_dir()).count();
            let bytes: u64 = picked.iter().filter(|e| !e.is_dir()).map(|e| e.size).sum();
            let n_sel = pane.found.as_ref().map_or(n_sel, |f| f.selected.len());
            let sel_text = match n_sel {
                0 => None,
                n if files > 0 => Some(format!("{n} selected · {}", human(bytes))),
                n => Some(format!("{n} selected")),
            };
            div()
                .h(rpx(26.))
                .flex_none()
                .px(rpx(10.))
                .flex()
                .items_center()
                .gap(rpx(10.))
                .border_t_1()
                .border_color(p.bd)
                .text_size(ts::SMALL)
                .text_color(p.fg3)
                .overflow_hidden()
                // Beside a terminal the footer has room for git only (or loading).
                .when(full || loading, |d| d.child(status))
                .when_some(sel_text.filter(|_| full), |d, t| {
                    d.child(div().text_color(p.fg2).child(t))
                })
                .child(div().flex_1())
                .when_some(pane.git.as_ref().map(GitStatus::summary), |d, summary| {
                    let on = pane.changes_only;
                    d.child(
                        div()
                            .font_family(MONO)
                            .text_color(p.fg2)
                            .child(format!("⎇ {summary}")),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("f{ix}-changes")))
                            .px(rpx(6.))
                            .rounded(px(4.))
                            .cursor_pointer()
                            .hover(|s| s.bg(p.hover))
                            .when(on, |t| t.text_color(p.acc))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let pane = &mut this.panes[ix];
                                pane.changes_only = !pane.changes_only;
                                cx.notify();
                            }))
                            .child(if on { "Changes ●" } else { "Changes ○" }),
                    )
                })
                .when(pane.next.is_some() && !loading, |d| {
                    d.child(
                        ui::button(
                            SharedString::from(format!("f{ix}-more")),
                            format!("Load {PAGE} more"),
                            Kind::Secondary,
                            p,
                        )
                        .h(rpx(20.))
                        .text_size(ts::SMALL)
                        .on_click(cx.listener(move |this, _, _, cx| this.load_more(ix, cx))),
                    )
                })
                .into_any_element()
        });

        let confirm: Option<AnyElement> = (self.confirm_delete == Some(ix)).then(|| {
            let what = match pane.selected.as_slice() {
                [one] => format!("Delete “{one}”?"),
                many => format!("Delete {} items?", many.len()),
            };
            div()
                .flex_none()
                .px(rpx(10.))
                .py(rpx(6.))
                .flex()
                .items_center()
                .gap(rpx(8.))
                .border_t_1()
                .border_color(p.bd)
                .bg(p.prod_bg)
                .text_size(ts::BODY)
                .child(
                    div()
                        .text_color(p.prod)
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(what),
                )
                .child(
                    div()
                        .flex_1()
                        .text_color(p.fg2)
                        .child(if pane.fs == FsRef::Local {
                            "It is deleted for good, not moved to the trash."
                        } else {
                            "It is deleted on the server for good."
                        }),
                )
                .child(
                    ui::button(
                        SharedString::from(format!("f{ix}-del-ok")),
                        "Delete",
                        Kind::Destructive,
                        p,
                    )
                    .h(rpx(22.))
                    .on_click(cx.listener(move |this, _, _, cx| this.delete(ix, cx))),
                )
                .child(
                    ui::button(
                        SharedString::from(format!("f{ix}-del-no")),
                        "Cancel",
                        Kind::Ghost,
                        p,
                    )
                    .h(rpx(22.))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.confirm_delete = None;
                        cx.notify();
                    })),
                )
                .into_any_element()
        });

        let found_count = pane.found.as_ref().map(|f| f.hits.len());
        let body: AnyElement = if let Some(n) = found_count {
            if n == 0 {
                div()
                    .flex_1()
                    .p(rpx(14.))
                    .text_size(ts::BODY)
                    .text_color(p.fg3)
                    .child(if pane.found.as_ref().is_some_and(|f| f.done) {
                        format!("Nothing named like “{filter}” in this folder or below")
                    } else {
                        "Searching…".to_owned()
                    })
                    .into_any_element()
            } else {
                uniform_list(
                    SharedString::from(format!("f{ix}-found")),
                    n,
                    cx.processor(move |this, range: std::ops::Range<usize>, _w, cx| {
                        this.render_hits(ix, range, cx)
                    }),
                )
                .track_scroll(&self.scroll[ix])
                .flex_1()
                .into_any_element()
            }
        } else if let Some(e) = &pane.error {
            div()
                .flex_1()
                .p(rpx(14.))
                .text_size(ts::BODY)
                .text_color(p.prod)
                .child(e.clone())
                .into_any_element()
        } else if pane.path.is_none() || (count == 0 && loading) {
            let label = match pane.fs {
                _ if pane.path.is_some() => "Loading…",
                FsRef::Host(_) => "Opening SFTP on the Host's session…",
                FsRef::Conn(_) => "Connecting…",
                FsRef::Local => "Loading…",
            };
            div()
                .flex_1()
                .p(rpx(14.))
                .flex()
                .flex_col()
                .gap(rpx(10.))
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(rpx(6.))
                        .child(ui::pulse_dot(
                            SharedString::from(format!("f{ix}-opening")),
                            p.acc,
                            6.,
                        ))
                        .child(label),
                )
                .children(
                    [220., 160., 190., 130., 170.]
                        .into_iter()
                        .map(|w| ui::shimmer(w, p)),
                )
                .into_any_element()
        } else if count == 0 {
            div()
                .id(SharedString::from(format!("f{ix}-empty")))
                .flex_1()
                .p(rpx(14.))
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                        this.open_menu(ix, MenuKind::Folder, ev.position, cx)
                    }),
                )
                .child(if filter.is_empty() {
                    "This folder is empty".to_owned()
                } else if pane.paged() && filter != pane.prefix {
                    format!("No loaded names contain “{filter}”. Press Enter to search the whole folder.")
                } else {
                    format!("No names contain “{filter}”")
                })
                .into_any_element()
        } else {
            div()
                .id(SharedString::from(format!("f{ix}-list-wrap")))
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                // A click on empty space clears the selection; a right click there is the
                // folder's menu.
                .on_click(cx.listener(move |this, _, window, cx| {
                    window.focus(&this.focus, cx);
                    this.active = ix;
                    this.close_menus();
                    this.panes[ix].selected.clear();
                    this.confirm_delete = None;
                    cx.notify();
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                        this.open_menu(ix, MenuKind::Folder, ev.position, cx)
                    }),
                )
                .child(
                    uniform_list(
                        SharedString::from(format!("f{ix}-list")),
                        count,
                        cx.processor(move |this, range: std::ops::Range<usize>, _w, cx| {
                            this.render_rows(ix, range, cx)
                        }),
                    )
                    .track_scroll(&self.scroll[ix])
                    .flex_1(),
                )
                .into_any_element()
        };

        let dual = self.layout == Layout::Dual;
        let drop_bg = p.sel;
        div()
            .id(SharedString::from(format!("files-pane-{ix}")))
            .relative()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .when(ix == 0 && dual, |d| d.border_r_1().border_color(p.bd))
            .when(dual, |d| {
                d.border_t_2()
                    .border_color(if active { p.acc } else { p.bd })
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    if this.active != ix {
                        this.active = ix;
                        cx.notify();
                    }
                }),
            )
            .drag_over::<DraggedFiles>(
                move |s, d, _, _| if d.pane != ix { s.bg(drop_bg) } else { s },
            )
            .drag_over::<ExternalPaths>(move |s, _, _, _| s.bg(drop_bg))
            .on_drop(cx.listener(move |this, d: &DraggedFiles, _, cx| {
                if d.pane != ix {
                    this.copy_into(d.fs.clone(), d.paths.clone(), ix, None, cx);
                }
            }))
            .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                this.copy_into(FsRef::Local, paths.paths().to_vec(), ix, None, cx);
            }))
            .child(nav)
            .children(loading_bar)
            .children(search_hint)
            .child(header)
            .child(body)
            .children(confirm)
            .children(footer)
            .into_any_element()
    }

    /// Search results in `range` of pane `ix`: name, the folder it is in (from the
    /// searched folder), time and size.
    fn render_hits(
        &mut self,
        ix: usize,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let p = palette(cx);
        let pane = &self.panes[ix];
        let Some(f) = pane.found.as_ref() else {
            return Vec::new();
        };
        let full = self.layout != Layout::Sidebar;
        let root = f.root.to_string_lossy().replace('\\', "/");
        let root = root.trim_end_matches('/');
        let selected: HashSet<&PathBuf> = f.selected.iter().collect();
        let fs = pane.fs.clone();
        range
            .filter_map(|i| f.hits.get(i))
            .map(|h| {
                let e = &h.entry;
                let path = pane.join(&h.dir, &e.name);
                let is_sel = selected.contains(&path);
                let kind = if e.is_dir() {
                    FileKind::Folder
                } else {
                    FileKind::of(&e.name)
                };
                let dir = h.dir.to_string_lossy().replace('\\', "/");
                let rel = dir
                    .strip_prefix(root)
                    .unwrap_or(&dir)
                    .trim_start_matches('/');
                let folder = if rel.is_empty() {
                    "·".to_owned()
                } else {
                    rel.to_owned()
                };
                let (p1, p2) = (path.clone(), path.clone());
                let drag = DraggedFiles {
                    pane: ix,
                    fs: fs.clone(),
                    paths: vec![path.clone()],
                };
                let label = e.name.clone();
                div()
                    .id(SharedString::from(format!(
                        "f{ix}-hit-{}",
                        path.to_string_lossy()
                    )))
                    .w_full()
                    .h(rpx(26.))
                    .px(rpx(10.))
                    .flex()
                    .items_center()
                    .gap(rpx(10.))
                    .text_size(ts::UI)
                    .when(is_sel, |d| d.bg(p.sel))
                    .when(!is_sel, |d| d.hover(|s| s.bg(p.hover)))
                    .on_click(
                        cx.listener(move |this, ev: &gpui_kit::ClickEvent, window, cx| {
                            window.focus(&this.focus, cx);
                            this.close_menus();
                            let m = ev.modifiers();
                            if ev.click_count() >= 2 && !m.secondary() && !m.shift {
                                this.select_found(ix, p1.clone(), false, false, cx);
                                this.open_found(ix, OpenWith::Default, false, cx);
                            } else {
                                this.select_found(ix, p1.clone(), m.secondary(), m.shift, cx);
                            }
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                            window.focus(&this.focus, cx);
                            let picked = this.panes[ix]
                                .found
                                .as_ref()
                                .is_some_and(|f| f.selected.contains(&p2));
                            if !picked {
                                this.select_found(ix, p2.clone(), false, false, cx);
                            }
                            this.open_menu(ix, MenuKind::Found, ev.position, cx);
                        }),
                    )
                    .on_drag(drag, move |_, _, _, cx| {
                        let label = label.clone();
                        cx.new(|_| DragPreview(label))
                    })
                    .child(
                        svg()
                            .path(kind.icon())
                            .flex_none()
                            .size(rpx(15.))
                            .text_color(if kind == FileKind::Folder {
                                p.acc
                            } else {
                                p.fg3
                            }),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(e.name.clone()))
                    .when(full, |d| {
                        d.child(
                            div()
                                .w(rpx(W_FOLDER))
                                .flex_none()
                                .truncate()
                                .text_size(ts::SMALL)
                                .text_color(p.fg3)
                                .child(folder),
                        )
                        .child(
                            div()
                                .w(rpx(W_MODIFIED))
                                .flex_none()
                                .text_size(ts::SMALL)
                                .text_color(p.fg3)
                                .child(e.modified_ms.map(fmt_time).unwrap_or_default()),
                        )
                        .child(
                            div()
                                .w(rpx(W_SIZE))
                                .flex_none()
                                .flex()
                                .justify_end()
                                .text_size(ts::SMALL)
                                .text_color(p.fg2)
                                .child(if e.is_dir() {
                                    String::new()
                                } else {
                                    human(e.size)
                                }),
                        )
                    })
                    .into_any_element()
            })
            .collect()
    }

    /// The rows in `range` of pane `ix` (only those on screen are drawn).
    fn render_rows(
        &mut self,
        ix: usize,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let p = palette(cx);
        let pane = &self.panes[ix];
        let rows = pane.visible();
        let full = self.layout != Layout::Sidebar;
        let mode_col = full && matches!(pane.fs, FsRef::Host(_));
        let fs = pane.fs.clone();
        let dir = pane.path.clone().unwrap_or_default();
        let selected: HashSet<&str> = pane.selected.iter().map(String::as_str).collect();
        // Cut files show dimmed until they are pasted.
        let cut: HashSet<PathBuf> = match &self.clip {
            Some(c) if c.cut && c.fs == fs => c.paths.iter().cloned().collect(),
            _ => HashSet::new(),
        };
        let renaming = self
            .rename
            .as_ref()
            .filter(|r| r.pane == ix)
            .map(|r| (r.old.clone(), r.input.clone()));
        range
            .filter_map(|i| rows.get(i).map(|e| (i, e)))
            .map(|(_, e)| {
                let change = pane.change(&e.name);
                let is_sel = selected.contains(e.name.as_str());
                let kind = if e.is_dir() {
                    FileKind::Folder
                } else {
                    FileKind::of(&e.name)
                };
                let icon = match (&e.kind, kind) {
                    (EntryKind::Symlink(_), FileKind::Folder) => "icons/swy-folder-symlink.svg",
                    (EntryKind::Symlink(_), _) => "icons/swy-file-symlink.svg",
                    _ => kind.icon(),
                };
                let icon_color = if kind == FileKind::Folder {
                    p.acc
                } else {
                    p.fg3
                };
                let full_path = pane.join(&dir, &e.name);
                let dimmed = cut.contains(&full_path);
                // Drag the whole selection when the row is part of it.
                let paths: Vec<PathBuf> = if is_sel {
                    pane.selected.iter().map(|n| pane.join(&dir, n)).collect()
                } else {
                    vec![full_path]
                };
                let label = if paths.len() == 1 {
                    e.name.clone()
                } else {
                    format!("{} items", paths.len())
                };
                let drag = DraggedFiles {
                    pane: ix,
                    fs: fs.clone(),
                    paths,
                };
                let name = e.name.clone();
                let name2 = e.name.clone();
                let is_dir = e.is_dir();
                let editing = renaming
                    .as_ref()
                    .filter(|(old, _)| *old == e.name)
                    .map(|(_, input)| input.clone());
                let name_cell: AnyElement = match editing {
                    Some(input) => div()
                        .flex_1()
                        .min_w_0()
                        .h(rpx(20.))
                        .flex()
                        .items_center()
                        .px(rpx(4.))
                        .rounded(px(3.))
                        .border_1()
                        .border_color(p.acc)
                        .bg(p.bg)
                        .child(Input::new(&input).appearance(false).text_size(ts::UI))
                        .into_any_element(),
                    None => div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .when_some(change, |d, c| d.text_color(change_color(c, &p)))
                        .child(e.name.clone())
                        .into_any_element(),
                };
                div()
                    .id(SharedString::from(format!("f{ix}-row-{}", e.name)))
                    .w_full()
                    .h(rpx(26.))
                    .px(rpx(10.))
                    .flex()
                    .items_center()
                    .gap(rpx(10.))
                    .text_size(ts::UI)
                    .when(is_sel, |d| d.bg(p.sel))
                    .when(!is_sel, |d| d.hover(|s| s.bg(p.hover)))
                    .when(dimmed, |d| d.opacity(0.5))
                    .on_click(
                        cx.listener(move |this, ev: &gpui_kit::ClickEvent, window, cx| {
                            cx.stop_propagation();
                            if this
                                .rename
                                .as_ref()
                                .is_some_and(|r| r.pane == ix && r.old == name)
                            {
                                return;
                            }
                            // Arrow keys and Enter work after a click.
                            window.focus(&this.focus, cx);
                            this.close_menus();
                            let m = ev.modifiers();
                            if ev.click_count() >= 2 && !m.secondary() && !m.shift {
                                this.panes[ix].selected = vec![name.clone()];
                                this.open_selection(ix, OpenWith::Default, cx);
                            } else {
                                this.select(ix, &name, m.secondary(), m.shift, cx);
                            }
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            window.focus(&this.focus, cx);
                            if !this.panes[ix].selected.contains(&name2) {
                                this.select(ix, &name2, false, false, cx);
                            }
                            this.open_menu(ix, MenuKind::Items, ev.position, cx);
                        }),
                    )
                    .on_drag(drag, move |_, _, _, cx| {
                        let label = label.clone();
                        cx.new(|_| DragPreview(label))
                    })
                    .child(
                        svg()
                            .path(icon)
                            .flex_none()
                            .size(rpx(15.))
                            .text_color(icon_color),
                    )
                    .child(name_cell)
                    .child(
                        div()
                            .w(rpx(12.))
                            .flex_none()
                            .font_family(MONO)
                            .text_size(ts::SMALL)
                            .font_weight(FontWeight::SEMIBOLD)
                            .when_some(change, |d, c| {
                                d.text_color(change_color(c, &p)).child(c.badge())
                            }),
                    )
                    .when(full, |d| {
                        d.child(
                            div()
                                .w(rpx(W_MODIFIED))
                                .flex_none()
                                .text_size(ts::SMALL)
                                .text_color(p.fg3)
                                .child(e.modified_ms.map(fmt_time).unwrap_or_default()),
                        )
                        .child(
                            div()
                                .w(rpx(W_TYPE))
                                .flex_none()
                                .truncate()
                                .text_size(ts::SMALL)
                                .text_color(p.fg3)
                                .child(if is_dir {
                                    "Folder".to_owned()
                                } else {
                                    type_label(&e.name, kind)
                                }),
                        )
                        .child(
                            div()
                                .w(rpx(W_SIZE))
                                .flex_none()
                                .flex()
                                .justify_end()
                                .text_size(ts::SMALL)
                                .text_color(p.fg2)
                                .child(if is_dir { String::new() } else { human(e.size) }),
                        )
                        .when(mode_col, |d| {
                            d.child(
                                div()
                                    .w(rpx(W_MODE))
                                    .flex_none()
                                    .font_family(MONO)
                                    .text_size(ts::CAPTION_PLUS)
                                    .text_color(p.fg3)
                                    .child(e.mode_string()),
                            )
                        })
                    })
                    .into_any_element()
            })
            .collect()
    }
}
