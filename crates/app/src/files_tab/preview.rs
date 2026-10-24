//! The preview pane beside the file list (Alt+P): the selected file without opening it.
//! Text, code and configuration show their first lines, images show whole, other binary
//! files their first bytes in hex, and documents, media and archives (which open in the
//! computer's default app) a card with their details.
//!
//! Only the start of a file is read ([`PREVIEW_TEXT_BYTES`]), one read per selection that
//! stays put for a moment, so arrowing through a folder of large logs stays quick. A file
//! that changes while it is selected (a coding CLI editing it) is read again.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, MouseButton,
    MouseDownEvent, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    StyledImage as _, div, px, svg, uniform_list,
};
use switchyard_core::files::PREVIEW_TEXT_BYTES;
use switchyard_core::remote::FileEntry;
use switchyard_core::{Command, Event, FsRef, PreviewBody, RequestId};

use super::kinds::type_label;
use super::{FileKind, FilesTab, Layout, OpenWith, fmt_time, join_path};
use crate::app_state::next_id;
use crate::appearance::{rpx, ts};
use crate::remote_files::human;
use crate::theme::{MONO, Palette};
use crate::ui::{self, Kind};

/// Default width of the pane (100 % design units).
pub(super) const PREVIEW_WIDTH: f32 = 360.;
/// Narrowest the pane can be dragged.
pub(super) const PREVIEW_MIN: f32 = 220.;
/// Narrowest the file list beside it gets: room for names and the other columns.
const LIST_MIN: f32 = 540.;
/// Width of the places column (`places.rs`).
const PLACES_WIDTH: f32 = 190.;
/// Lines of a text file the pane shows.
const MAX_LINES: usize = 2_000;
/// Rows of 8 bytes in the hex view.
const HEX_ROWS: usize = 64;
/// How long the selection stays on a file before it is read.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(120);

/// The file the pane shows.
pub(super) struct Preview {
    fs: FsRef,
    path: PathBuf,
    entry: FileEntry,
    kind: FileKind,
    request: Option<RequestId>,
    body: Body,
}

/// What shows under the file's name.
enum Body {
    /// Waiting for the read.
    Loading,
    /// Nothing to read: a folder, or a file the default app opens.
    Card,
    Text {
        lines: Rc<Vec<SharedString>>,
        /// The file goes on past what shows.
        more: bool,
    },
    Image {
        format: gpui_kit::ImageFormat,
        image: Arc<gpui_kit::Image>,
    },
    Hex(Rc<Vec<(String, String, String)>>),
    Failed(String),
}

/// `text` as display lines: tabs expanded, `\r` dropped, at most [`MAX_LINES`].
fn text_lines(text: &str) -> (Vec<SharedString>, bool) {
    let mut lines: Vec<SharedString> = text
        .lines()
        .take(MAX_LINES + 1)
        .map(|l| SharedString::from(l.replace('\t', "    ")))
        .collect();
    let cut = lines.len() > MAX_LINES;
    lines.truncate(MAX_LINES);
    (lines, cut)
}

/// Whether the pane reads the file (the rest get a card).
fn reads(entry: &FileEntry, kind: FileKind) -> bool {
    !entry.is_dir() && kind.opens_here()
}

impl FilesTab {
    /// The file the pane should show: the cursor of the one pane's selection.
    fn preview_target(&self) -> Option<(FsRef, PathBuf, FileEntry)> {
        if self.layout != Layout::Single || !self.show_preview {
            return None;
        }
        let pane = &self.panes[0];
        let posix = pane.fs.is_remote() || cfg!(unix);
        if let Some(f) = &pane.found {
            let path = f.selected.last()?;
            let hit = f
                .hits
                .iter()
                .find(|h| join_path(posix, &h.dir, &h.entry.name) == *path)?;
            return Some((pane.fs.clone(), path.clone(), hit.entry.clone()));
        }
        let name = pane.selected.last()?;
        let dir = pane.path.as_ref()?;
        let entry = pane.entries.iter().find(|e| e.name == *name)?;
        Some((pane.fs.clone(), pane.join(dir, name), entry.clone()))
    }

    /// Follow the selection: a new file is read once the selection settles there, and the
    /// same file again when its size or time changed.
    pub(super) fn sync_preview(&mut self, cx: &mut Context<Self>) {
        let Some((fs, path, entry)) = self.preview_target() else {
            self.preview = None;
            return;
        };
        if let Some(p) = &self.preview
            && p.fs == fs
            && p.path == path
        {
            let changed = p.entry.size != entry.size || p.entry.modified_ms != entry.modified_ms;
            if changed && let Some(p) = self.preview.as_mut() {
                // Keep what shows until the new read arrives.
                p.entry = entry;
                if reads(&p.entry, p.kind) {
                    self.read_preview_soon(cx);
                }
            }
            return;
        }
        let kind = if entry.is_dir() {
            FileKind::Folder
        } else {
            FileKind::of(&entry.name)
        };
        let reads = reads(&entry, kind);
        self.preview = Some(Preview {
            fs,
            path,
            entry,
            kind,
            request: None,
            body: if reads { Body::Loading } else { Body::Card },
        });
        if reads {
            self.read_preview_soon(cx);
        }
    }

    /// Read the previewed file once the selection has stayed on it for [`SETTLE`].
    fn read_preview_soon(&mut self, cx: &mut Context<Self>) {
        self.preview_ticket += 1;
        let ticket = self.preview_ticket;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SETTLE).await;
            this.update(cx, |t, cx| {
                if t.preview_ticket != ticket {
                    return;
                }
                let Some(p) = t.preview.as_mut() else { return };
                let request = next_id();
                p.request = Some(request);
                t.core.send(Command::PreviewFile {
                    request,
                    fs: p.fs.clone(),
                    path: p.path.clone(),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The answer to a read; an older one (the selection moved on) is dropped.
    pub(super) fn on_preview(&mut self, ev: &Event) {
        let Event::FilePreview { request, result } = ev else {
            return;
        };
        let Some(p) = self
            .preview
            .as_mut()
            .filter(|p| p.request == Some(*request))
        else {
            return;
        };
        p.request = None;
        p.body = match result {
            Ok(f) => match &f.body {
                PreviewBody::Text { text, truncated } => {
                    let (lines, cut) = text_lines(text);
                    Body::Text {
                        lines: Rc::new(lines),
                        more: *truncated || cut,
                    }
                }
                PreviewBody::Image(bytes) => match crate::viewer::image_format(&bytes.0) {
                    Some(format) => Body::Image {
                        format,
                        image: Arc::new(gpui_kit::Image::from_bytes(format, bytes.0.to_vec())),
                    },
                    None => Body::Hex(Rc::new(crate::viewer::hex_rows(&bytes.0, HEX_ROWS))),
                },
                PreviewBody::Binary(bytes) => {
                    Body::Hex(Rc::new(crate::viewer::hex_rows(&bytes.0, HEX_ROWS)))
                }
            },
            Err(e) => Body::Failed(e.clone()),
        };
    }

    /// The widest the pane may be: the list beside it keeps [`LIST_MIN`] (and the places
    /// column its width). `None` before the tab's first frame.
    pub(super) fn preview_max(&self, cx: &gpui_kit::App) -> Option<f32> {
        let row = self.body_width.get();
        if row <= 0. {
            return None;
        }
        let places = if self.show_places { PLACES_WIDTH } else { 0. };
        let max = row / crate::appearance::zoom(cx) - places - LIST_MIN;
        Some(max.max(PREVIEW_MIN))
    }

    /// The width the pane is drawn at: the chosen one, narrowed to fit.
    fn preview_shown_width(&self, cx: &gpui_kit::App) -> f32 {
        self.preview_max(cx)
            .map_or(self.preview_width, |m| self.preview_width.min(m))
    }

    /// Show or hide the pane, and remember the choice.
    pub(super) fn toggle_preview(&mut self, cx: &mut Context<Self>) {
        self.show_preview = !self.show_preview;
        if !self.show_preview {
            self.preview = None;
        }
        self.save_settings();
        cx.notify();
    }

    /// The pane, when it shows.
    pub(super) fn render_preview(
        &mut self,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.layout != Layout::Single || !self.show_preview {
            return None;
        }
        let width = self.preview_shown_width(cx);
        let pane = &self.panes[0];
        let multi = if pane.found.is_some() {
            pane.found.as_ref().map_or(0, |f| f.selected.len())
        } else {
            pane.selected.len()
        };
        let resize = div()
            .id("files-preview-resize")
            .absolute()
            .left(rpx(-3.))
            .top_0()
            .bottom_0()
            .w(rpx(6.))
            .cursor_col_resize()
            .hover(|s| s.bg(p.acc.opacity(0.35)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    this.preview_drag = Some((ev.position.x.into(), width));
                    cx.stop_propagation();
                }),
            );
        let content = match &self.preview {
            None => Self::preview_hint(
                if multi > 1 {
                    format!("{multi} items selected")
                } else {
                    "Select a file to preview it".into()
                },
                p,
            ),
            Some(pv) => self.render_preview_file(pv, p, cx),
        };
        Some(
            div()
                .relative()
                .flex_none()
                .w(rpx(width))
                .h_full()
                .flex()
                .flex_col()
                .border_l_1()
                .border_color(p.bd)
                .bg(p.panel)
                .child(content)
                .child(resize)
                .into_any_element(),
        )
    }

    fn preview_hint(text: String, p: &Palette) -> AnyElement {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(rpx(8.))
            .child(
                svg()
                    .path("icons/swy-panel-right.svg")
                    .size(rpx(22.))
                    .text_color(p.fg3),
            )
            .child(div().text_size(ts::BODY).text_color(p.fg3).child(text))
            .into_any_element()
    }

    fn render_preview_file(&self, pv: &Preview, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let e = &pv.entry;
        let what = match &pv.body {
            Body::Image { format, .. } => {
                format!("{} image", crate::viewer::format_name(*format))
            }
            _ if e.is_dir() => "Folder".into(),
            _ => type_label(&e.name, pv.kind),
        };
        let mut facts: Vec<(&'static str, String)> = vec![("Type", what)];
        if !e.is_dir() {
            facts.push(("Size", human(e.size)));
        }
        if let Some(m) = e.modified_ms {
            facts.push(("Modified", fmt_time(m)));
        }
        if e.mode.is_some() {
            facts.push(("Permissions", e.mode_string()));
        }
        let header = div()
            .flex_none()
            .px(rpx(12.))
            .py(rpx(9.))
            .flex()
            .items_center()
            .gap(rpx(8.))
            .border_b_1()
            .border_color(p.bd)
            .child(
                svg()
                    .path(pv.kind.icon())
                    .size(rpx(16.))
                    .flex_none()
                    .text_color(if e.is_dir() { p.acc } else { p.fg2 }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_size(ts::UI)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(p.fg)
                    .child(e.name.clone()),
            )
            .child(
                div()
                    .id("files-preview-close")
                    .flex_none()
                    .size(rpx(22.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .text_size(ts::BODY)
                    .text_color(p.fg3)
                    .hover(|s| s.bg(p.hover))
                    .tooltip(|w, cx| {
                        gpui_kit::component::tooltip::Tooltip::new("Hide preview · Alt+P")
                            .build(w, cx)
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_preview(cx)))
                    .child("✕"),
            );
        let body = match &pv.body {
            Body::Loading => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .child("Loading…")
                .into_any_element(),
            Body::Card => Self::preview_card(pv, p),
            Body::Failed(err) => div()
                .flex_1()
                .p(rpx(14.))
                .text_size(ts::BODY)
                .text_color(p.prod)
                .child(format!("Can't preview it: {err}"))
                .into_any_element(),
            Body::Image { image, .. } => div()
                .flex_1()
                .min_h_0()
                .p(rpx(12.))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    gpui_kit::img(image.clone())
                        .max_w_full()
                        .max_h_full()
                        .object_fit(gpui_kit::ObjectFit::Contain),
                )
                .into_any_element(),
            Body::Text { lines, more } => {
                let lines = lines.clone();
                let n = lines.len();
                let fg = p.fg;
                let gutter = p.fg3;
                let digits = n.max(1).to_string().len() as f32;
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        uniform_list(
                            "files-preview-lines",
                            n,
                            move |range: std::ops::Range<usize>, _w, _cx| {
                                range
                                    .map(|i| {
                                        div()
                                            .w_full()
                                            .h(rpx(17.))
                                            .px(rpx(8.))
                                            .flex()
                                            .gap(rpx(10.))
                                            .font_family(MONO)
                                            .text_size(ts::SMALL)
                                            .whitespace_nowrap()
                                            .overflow_hidden()
                                            .child(
                                                div()
                                                    .flex_none()
                                                    .w(rpx(7. * digits))
                                                    .text_color(gutter)
                                                    .child((i + 1).to_string()),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .text_color(fg)
                                                    .child(lines[i].clone()),
                                            )
                                    })
                                    .collect()
                            },
                        )
                        .track_scroll(&self.preview_scroll)
                        .py(rpx(6.))
                        .flex_1(),
                    )
                    .when(*more, |d| {
                        d.child(
                            div()
                                .flex_none()
                                .px(rpx(12.))
                                .py(rpx(4.))
                                .border_t_1()
                                .border_color(p.bd)
                                .text_size(ts::CAPTION)
                                .text_color(p.fg3)
                                .child(format!(
                                    "The first {} · open it to see the rest",
                                    if n >= MAX_LINES {
                                        format!("{MAX_LINES} lines")
                                    } else {
                                        human(PREVIEW_TEXT_BYTES)
                                    }
                                )),
                        )
                    })
                    .into_any_element()
            }
            Body::Hex(rows) => div()
                .id("files-preview-hex")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .px(rpx(10.))
                .py(rpx(8.))
                .font_family(MONO)
                .text_size(ts::SMALL)
                .child(
                    div()
                        .pb(rpx(6.))
                        .text_color(p.fg3)
                        .font_family(crate::theme::SANS)
                        .text_size(ts::CAPTION)
                        .child(format!(
                            "Binary file · the first {} bytes",
                            rows.iter().map(|r| r.2.chars().count()).sum::<usize>()
                        )),
                )
                .children(rows.iter().map(|(off, hex, ascii)| {
                    div()
                        .flex()
                        .gap(rpx(10.))
                        .whitespace_nowrap()
                        .child(div().text_color(p.fg3).child(off.clone()))
                        .child(div().text_color(p.fg).child(hex.clone()))
                        .child(div().text_color(p.fg2).child(ascii.clone()))
                }))
                .into_any_element(),
        };
        let facts = div()
            .flex_none()
            .px(rpx(12.))
            .py(rpx(8.))
            .flex()
            .flex_col()
            .gap(rpx(3.))
            .border_t_1()
            .border_color(p.bd)
            .children(facts.into_iter().map(|(k, v)| {
                div()
                    .flex()
                    .gap(rpx(8.))
                    .text_size(ts::LABEL)
                    .child(div().flex_none().w(rpx(78.)).text_color(p.fg3).child(k))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_color(p.fg2)
                            .child(v),
                    )
            }))
            .child(self.preview_actions(pv, p, cx));
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header)
            .child(body)
            .child(facts)
            .into_any_element()
    }

    /// A folder, or a file the default app opens: a large icon and what it is.
    fn preview_card(pv: &Preview, p: &Palette) -> AnyElement {
        let note = if pv.entry.is_dir() {
            "Double click or press Enter to open the folder".to_owned()
        } else if pv.fs.is_remote() {
            "Opens in your computer's default app (downloads a copy first)".to_owned()
        } else {
            "Opens in your computer's default app".to_owned()
        };
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(rpx(10.))
            .px(rpx(16.))
            .child(
                svg()
                    .path(pv.kind.icon())
                    .size(rpx(56.))
                    .text_color(if pv.entry.is_dir() { p.acc } else { p.fg3 }),
            )
            .child(
                div()
                    .text_size(ts::BODY)
                    .text_color(p.fg3)
                    .text_center()
                    .child(note),
            )
            .into_any_element()
    }

    /// Open (here or in the default app) and, for files Switchyard opens, the default app.
    fn preview_actions(&self, pv: &Preview, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let path = pv.path.clone();
        let dir = pv.entry.is_dir();
        let here = !dir && pv.kind.opens_here();
        let primary = ui::button(
            "files-preview-open",
            if dir || here {
                "Open"
            } else {
                "Open in default app"
            },
            Kind::Primary,
            p,
        )
        .h(rpx(24.))
        .text_size(ts::LABEL)
        .on_click(cx.listener({
            let path = path.clone();
            move |this, _, _, cx| {
                if dir {
                    this.navigate(0, Some(path.clone()), cx);
                } else {
                    this.open_path(0, path.clone(), OpenWith::Default, cx);
                }
            }
        }));
        let system = here.then(|| {
            ui::button("files-preview-system", "Default app", Kind::Secondary, p)
                .h(rpx(24.))
                .text_size(ts::LABEL)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open_path(0, path.clone(), OpenWith::System, cx)
                }))
        });
        div()
            .pt(rpx(6.))
            .flex()
            .gap(rpx(6.))
            .child(primary)
            .children(system)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_lines_expand_tabs_and_stop_at_the_limit() {
        let (lines, cut) = text_lines("a\r\n\tb\n");
        assert_eq!(lines, ["a", "    b"]);
        assert!(!cut);
        let long = "x\n".repeat(MAX_LINES + 5);
        let (lines, cut) = text_lines(&long);
        assert_eq!(lines.len(), MAX_LINES);
        assert!(cut);
    }
}
