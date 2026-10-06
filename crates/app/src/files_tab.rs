//! Files tab: dual-pane browser with a transfer drawer. The local pane browses the real
//! file system through the runtime; SFTP and FTP panes land in milestone M4.

use std::path::{Path, PathBuf};

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, StatefulInteractiveElement as _, Styled as _, Window, div, px, uniform_list,
};
use switchyard_core::remote::{EntryKind, FileEntry};
use switchyard_core::{Command, RequestId, RuntimeHandle};

use crate::app_state::next_id;
use crate::theme::{MONO, Palette, palette};
use crate::ui::{self, Kind};

/// The files tab.
pub struct FilesTab {
    core: RuntimeHandle,
    path: PathBuf,
    entries: Vec<FileEntry>,
    request: Option<RequestId>,
    error: Option<String>,
    show_hidden: bool,
    selected: Option<String>,
}

impl FilesTab {
    /// A browser starting at the home directory.
    pub fn new(core: RuntimeHandle, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        let mut this = Self {
            core,
            path: home.clone(),
            entries: Vec::new(),
            request: None,
            error: None,
            show_hidden: false,
            selected: None,
        };
        this.list(home, cx);
        this
    }

    fn list(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let request = next_id();
        self.request = Some(request);
        self.core.send(Command::ListLocalDir { request, path });
        cx.notify();
    }

    /// A directory listing arrived.
    pub fn on_listing(
        &mut self,
        request: RequestId,
        path: PathBuf,
        result: Result<Vec<FileEntry>, String>,
        cx: &mut Context<Self>,
    ) {
        if self.request != Some(request) {
            return;
        }
        self.request = None;
        match result {
            Ok(entries) => {
                self.path = path;
                self.entries = entries;
                self.error = None;
                self.selected = None;
            }
            Err(e) => self.error = Some(e),
        }
        cx.notify();
    }

    fn visible(&self) -> Vec<FileEntry> {
        self.entries
            .iter()
            .filter(|e| self.show_hidden || !e.is_hidden())
            .cloned()
            .collect()
    }

    fn render_local(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.visible();
        let count = rows.len();
        let crumbs: Vec<String> = self
            .path
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        let p2 = *p;
        let parent = self.path.parent().map(Path::to_path_buf);
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .bg(p.surface)
            .child(
                div()
                    .h(px(32.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(10.))
                    .border_b_1()
                    .border_color(p.bd)
                    .text_size(px(12.))
                    .overflow_hidden()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("Local"))
                    .child(div().text_color(p.fg3).child("·"))
                    .children(crumbs.into_iter().map(|c| {
                        div()
                            .flex()
                            .gap(px(6.))
                            .child(
                                div()
                                    .font_family(MONO)
                                    .text_size(px(11.5))
                                    .text_color(p.fg2)
                                    .child(c),
                            )
                            .child(div().text_color(p.fg3).text_size(px(10.)).child("›"))
                    }))
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_color(p.fg3)
                            .text_size(px(11.))
                            .whitespace_nowrap()
                            .child(format!("{count} items")),
                    ),
            )
            .child(header(p))
            .when_some(self.error.clone(), |d, e| {
                d.child(
                    div()
                        .px(px(10.))
                        .py(px(6.))
                        .bg(p.prod_bg)
                        .text_size(px(12.))
                        .child(e),
                )
            })
            .when_some(parent, |d, parent| {
                d.child(
                    div()
                        .id("up")
                        .h(px(26.))
                        .flex()
                        .items_center()
                        .px(px(10.))
                        .text_size(px(12.5))
                        .text_color(p.fg3)
                        .hover(|s| s.bg(p.hover))
                        .on_click(cx.listener(move |this, _, _, cx| this.list(parent.clone(), cx)))
                        .child(div().w(px(30.)))
                        .child(".."),
                )
            })
            .child(
                uniform_list(
                    "local-files",
                    count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _w, cx| {
                        range
                            .map(|i| {
                                let e = &rows[i];
                                let dir = e.is_dir();
                                let target = this.path.join(&e.name);
                                let name = e.name.clone();
                                let selected = this.selected.as_deref() == Some(e.name.as_str());
                                let kind = match &e.kind {
                                    EntryKind::Dir => "DIR".to_owned(),
                                    EntryKind::Symlink(_) => "LNK".to_owned(),
                                    EntryKind::File => e
                                        .name
                                        .rsplit_once('.')
                                        .map(|(_, x)| x.to_uppercase())
                                        .filter(|x| x.len() <= 4)
                                        .unwrap_or_default(),
                                };
                                div()
                                    .id(("lf", i))
                                    .w_full()
                                    .h(px(26.))
                                    .flex()
                                    .items_center()
                                    .px(px(10.))
                                    .text_size(px(12.5))
                                    .when(selected, |d| d.bg(p2.sel))
                                    .hover(|s| s.bg(p2.hover))
                                    .on_click(cx.listener(
                                        move |this, ev: &gpui_kit::ClickEvent, _, cx| {
                                            if dir && ev.click_count() >= 2
                                                || dir
                                                    && ev.click_count() == 1
                                                    && this.selected.as_deref()
                                                        == Some(name.as_str())
                                            {
                                                this.list(target.clone(), cx);
                                            } else {
                                                this.selected = Some(name.clone());
                                                cx.notify();
                                            }
                                        },
                                    ))
                                    .child(
                                        div()
                                            .w(px(30.))
                                            .flex_none()
                                            .font_family(MONO)
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_size(px(9.))
                                            .text_color(p2.fg3)
                                            .child(kind),
                                    )
                                    .child(
                                        div().flex_1().min_w_0().truncate().child(e.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .w(px(80.))
                                            .flex_none()
                                            .flex()
                                            .justify_end()
                                            .font_family(MONO)
                                            .text_size(px(11.5))
                                            .text_color(p2.fg2)
                                            .child(if dir {
                                                "—".to_owned()
                                            } else {
                                                ui::bytes(e.size)
                                            }),
                                    )
                                    .child(
                                        div()
                                            .w(px(120.))
                                            .flex_none()
                                            .pl(px(14.))
                                            .text_size(px(12.))
                                            .text_color(p2.fg2)
                                            .child(e.modified_ms.map(fmt_time).unwrap_or_default()),
                                    )
                                    .child(
                                        div()
                                            .w(px(90.))
                                            .flex_none()
                                            .font_family(MONO)
                                            .text_size(px(11.))
                                            .text_color(p2.fg3)
                                            .child(e.mode_string()),
                                    )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .flex_1(),
            )
            .into_any_element()
    }
}

fn header(p: &Palette) -> AnyElement {
    div()
        .h(px(26.))
        .flex_none()
        .flex()
        .items_center()
        .px(px(10.))
        .text_size(px(11.5))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(p.fg2)
        .border_b_1()
        .border_color(p.bd)
        .bg(p.panel)
        .child(div().flex_1().pl(px(30.)).child("Name ↑"))
        .child(div().w(px(80.)).flex().justify_end().child("Size"))
        .child(div().w(px(120.)).pl(px(14.)).child("Modified"))
        .child(div().w(px(90.)).child("Mode"))
        .into_any_element()
}

fn fmt_time(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let (y, m, d) = switchyard_core::db::value::civil_from_days(days);
    let mins = ms.rem_euclid(86_400_000) / 60_000;
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let _ = y;
    format!(
        "{} {d}, {:02}:{:02}",
        MONTHS[(m as usize).saturating_sub(1) % 12],
        mins / 60,
        mins % 60
    )
}

impl Render for FilesTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let local = self.render_local(&p, cx);
        let hidden = self.show_hidden;
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(36.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .px(px(10.))
                    .border_b_1()
                    .border_color(p.bd)
                    .child(ui::button("f-up", "Upload", Kind::Ghost, &p).h(px(24.)).text_color(p.fg3))
                    .child(ui::button("f-down", "Download", Kind::Ghost, &p).h(px(24.)).text_color(p.fg3))
                    .child(ui::button("f-refresh", "Refresh", Kind::Ghost, &p).h(px(24.)).on_click(cx.listener(|this, _, _, cx| {
                        let path = this.path.clone();
                        this.list(path, cx);
                    })))
                    .child(
                        ui::button("f-hidden", if hidden { "Hide hidden" } else { "Show hidden" }, Kind::Ghost, &p)
                            .h(px(24.))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_hidden = !this.show_hidden;
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1())
                    .child(div().font_family(MONO).text_size(px(11.5)).text_color(p.fg3).child("Local file system")),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .gap(px(1.))
                    .bg(p.bd)
                    .child(local)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .bg(p.surface)
                            .flex()
                            .items_center()
                            .justify_center()
                            .p(px(24.))
                            .child(
                                div()
                                    .max_w(px(360.))
                                    .flex()
                                    .flex_col()
                                    .gap(px(6.))
                                    .text_size(px(12.5))
                                    .text_color(p.fg2)
                                    .child(div().text_color(p.fg).font_weight(FontWeight::MEDIUM).child("Remote pane"))
                                    .child("SFTP over a Host's SSH session and FTP/FTPS arrive in milestone M4, with a resumable transfer queue."),
                            ),
                    ),
            )
            .child(
                div()
                    .h(px(120.))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .border_t_1()
                    .border_color(p.bd)
                    .bg(p.panel)
                    .child(
                        div()
                            .h(px(32.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .px(px(12.))
                            .border_b_1()
                            .border_color(p.bd)
                            .text_size(px(12.))
                            .child(div().font_weight(FontWeight::SEMIBOLD).child("Transfers"))
                            .child(div().text_color(p.fg2).child("No transfers"))
                            .child(div().flex_1())
                            .child(div().font_family(MONO).text_size(px(11.5)).text_color(p.fg3).child("4 parallel")),
                    )
                    .child(div().flex_1().flex().items_center().justify_center().text_size(px(12.)).text_color(p.fg3).child("Queued uploads and downloads appear here.")),
            )
    }
}
