//! The sidebar's Files panel for an SSH Host: browse the server over SFTP (on the Host's
//! shared session), drop files from the OS to upload, download, delete, and open files in
//! an editor tab. Transfers show under the list with progress and cancel.

use std::path::{Path, PathBuf};

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, ExternalPaths, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, PathPromptOptions, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px, uniform_list,
};
use switchyard_core::remote::{EntryKind, FileEntry};
use switchyard_core::store::ProfileId;
use switchyard_core::{Command, Event, FsOp, FsRef, OnConflict, RequestId, RuntimeHandle};

use crate::app_state::next_id;
use crate::theme::{MONO, Palette, palette};
use crate::transfers::Transfers;

/// What the panel asks the workspace to do.
pub enum RemoteFilesEvent {
    /// Open a remote file in an editor tab.
    Open {
        /// Host.
        host: ProfileId,
        /// File.
        path: PathBuf,
    },
    /// Show a message.
    Toast(String),
    /// Open the dual-pane Files tab with this Host.
    OpenTab(ProfileId),
}

/// Files of one Host.
pub struct RemoteFiles {
    core: RuntimeHandle,
    host: ProfileId,
    path: Option<PathBuf>,
    entries: Vec<FileEntry>,
    request: Option<RequestId>,
    op: Option<RequestId>,
    error: Option<String>,
    show_hidden: bool,
    selected: Option<String>,
    /// Row whose Delete was clicked once (click again to confirm).
    confirm_delete: Option<String>,
    transfers: Entity<Transfers>,
    last_click: Option<(String, std::time::Instant)>,
    /// The "go to folder" box while it is open.
    goto: Option<(Entity<InputState>, Subscription)>,
}

impl EventEmitter<RemoteFilesEvent> for RemoteFiles {}

fn join(dir: &Path, name: &str) -> PathBuf {
    let d = dir.to_string_lossy().replace('\\', "/");
    PathBuf::from(if d.ends_with('/') {
        format!("{d}{name}")
    } else {
        format!("{d}/{name}")
    })
}

/// Where a typed folder points: absolute and `~` paths as typed (the runtime expands `~`
/// and folds `..`), anything else relative to `dir`.
pub(crate) fn typed_target(dir: Option<&Path>, typed: &str) -> Option<PathBuf> {
    let t = typed.trim();
    if t.is_empty() {
        return None;
    }
    if t.starts_with('/') || t.starts_with('~') {
        return Some(PathBuf::from(t));
    }
    Some(join(dir?, t))
}

/// `/`, `srv`, `app` with the path up to each (POSIX).
fn crumbs(path: &Path) -> Vec<(String, PathBuf)> {
    let s = path.to_string_lossy().replace('\\', "/");
    let mut out = vec![("/".to_owned(), PathBuf::from("/"))];
    let mut acc = String::new();
    for part in s.split('/').filter(|p| !p.is_empty()) {
        acc.push('/');
        acc.push_str(part);
        out.push((part.to_owned(), PathBuf::from(&acc)));
    }
    out
}

fn parent(dir: &Path) -> Option<PathBuf> {
    let d = dir.to_string_lossy().replace('\\', "/");
    let d = d.trim_end_matches('/');
    if d.is_empty() {
        return None;
    }
    let up = match d.rfind('/') {
        Some(0) => "/".to_owned(),
        Some(i) => d[..i].to_owned(),
        None => return None,
    };
    Some(PathBuf::from(up))
}

pub(crate) fn human(bytes: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

impl RemoteFiles {
    /// A panel for `host`, listing its home folder.
    pub fn new(
        core: RuntimeHandle,
        host: ProfileId,
        transfers: Entity<Transfers>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            core,
            host,
            path: None,
            entries: Vec::new(),
            request: None,
            op: None,
            error: None,
            show_hidden: false,
            selected: None,
            confirm_delete: None,
            transfers,
            last_click: None,
            goto: None,
        };
        this.list(None, cx);
        this
    }

    fn fs(&self) -> FsRef {
        FsRef::Host(self.host.clone())
    }

    fn list(&mut self, path: Option<PathBuf>, cx: &mut Context<Self>) {
        let request = next_id();
        self.request = Some(request);
        self.core.send(Command::ListDir {
            request,
            fs: self.fs(),
            path,
        });
        cx.notify();
    }

    fn open_goto(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self
            .path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("/path, ~/folder or a subfolder")
                .default_value(current)
        });
        let sub = cx.subscribe_in(
            &input,
            window,
            |this, input, ev: &InputEvent, _, cx| match ev {
                InputEvent::PressEnter { .. } => {
                    let typed = input.read(cx).value().to_string();
                    this.goto = None;
                    if let Some(target) = typed_target(this.path.as_deref(), &typed) {
                        this.list(Some(target), cx);
                    }
                    cx.notify();
                }
                InputEvent::Blur => {
                    this.goto = None;
                    cx.notify();
                }
                _ => {}
            },
        );
        input.update(cx, |i, cx| {
            i.focus(window, cx);
            i.select_all(window, cx);
        });
        self.goto = Some((input, sub));
        cx.notify();
    }

    /// List the folder on screen again.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.list(self.path.clone(), cx);
    }

    /// Upload local files or folders into the folder on screen.
    pub fn upload(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let Some(dir) = self.path.clone() else {
            return;
        };
        for p in paths {
            self.start(true, p, Some(dir.clone()), OnConflict::Ask, cx);
        }
    }

    fn start(
        &mut self,
        upload: bool,
        from: PathBuf,
        dir: Option<PathBuf>,
        on_conflict: OnConflict,
        cx: &mut Context<Self>,
    ) {
        let (src, dst) = if upload {
            (FsRef::Local, self.fs())
        } else {
            (self.fs(), FsRef::Local)
        };
        self.transfers
            .update(cx, |t, cx| t.start(src, from, dst, dir, on_conflict, cx));
        cx.notify();
    }

    /// Runtime events for this Host.
    pub fn on_event(&mut self, ev: &Event, cx: &mut Context<Self>) {
        match ev {
            Event::FsListing {
                request,
                path,
                result,
                ..
            } if Some(*request) == self.request => {
                self.request = None;
                match result {
                    Ok(entries) => {
                        if self.path.as_ref() != Some(path) {
                            self.selected = None;
                        }
                        self.path = Some(path.clone());
                        self.entries = entries.clone();
                        self.error = None;
                    }
                    // A folder that can't be opened leaves the current one on screen.
                    Err(e) if self.path.is_some() && self.error.is_none() => {
                        cx.emit(RemoteFilesEvent::Toast(format!(
                            "Can't open {}: {e}",
                            path.to_string_lossy()
                        )));
                    }
                    Err(e) => self.error = Some(e.clone()),
                }
            }
            Event::FsOpDone {
                request, result, ..
            } if Some(*request) == self.op => {
                self.op = None;
                if let Err(e) = result {
                    cx.emit(RemoteFilesEvent::Toast(e.clone()));
                }
                self.refresh(cx);
            }
            // Changed from the Files tab: show it here too.
            Event::FsOpDone { fs, .. } if *fs == self.fs() => self.refresh(cx),
            // Uploads land in the folder on screen.
            Event::TransferDone { result: Ok(_), .. } => self.refresh(cx),
            _ => return,
        }
        cx.notify();
    }

    fn click(&mut self, e: &FileEntry, cx: &mut Context<Self>) {
        let now = std::time::Instant::now();
        let double = self
            .last_click
            .as_ref()
            .is_some_and(|(n, t)| *n == e.name && now.duration_since(*t).as_millis() < 450);
        self.last_click = Some((e.name.clone(), now));
        self.selected = Some(e.name.clone());
        self.confirm_delete = None;
        if double {
            let Some(dir) = self.path.clone() else { return };
            let full = join(&dir, &e.name);
            if e.is_dir() {
                self.list(Some(full), cx);
            } else {
                cx.emit(RemoteFilesEvent::Open {
                    host: self.host.clone(),
                    path: full,
                });
            }
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

    fn pick_upload(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: true,
            prompt: Some("Upload".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                let _ = this.update(cx, |this, cx| this.upload(paths, cx));
            }
        })
        .detach();
    }

    /// The panel body (inside the sidebar).
    pub fn render_panel(&mut self, host_name: &str, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let rows = self.visible();
        let count = rows.len();
        let path = self
            .path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "connecting…".into());
        let path_line: AnyElement = match (&self.goto, &self.path) {
            (Some((input, _)), _) => div()
                .h(px(22.))
                .flex()
                .items_center()
                .px(px(6.))
                .border_1()
                .border_color(p.acc)
                .rounded(px(4.))
                .child(
                    Input::new(input)
                        .appearance(false)
                        .font_family(MONO)
                        .text_size(px(11.)),
                )
                .into_any_element(),
            (None, Some(current)) => {
                let crumbs = crumbs(current);
                let last = crumbs.len().saturating_sub(1);
                div()
                    .id("rf-path")
                    .tooltip(|w, cx| {
                        gpui_kit::component::tooltip::Tooltip::new("Click to type a folder path")
                            .build(w, cx)
                    })
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .font_family(MONO)
                    .text_size(px(10.5))
                    .text_color(p.fg3)
                    .rounded(px(3.))
                    .cursor_text()
                    .hover(|s| s.bg(p.hover))
                    .on_click(cx.listener(|this, _, window, cx| this.open_goto(window, cx)))
                    .children(crumbs.into_iter().enumerate().map(|(i, (label, target))| {
                        div()
                            .flex()
                            .items_center()
                            .when(i > 1, |d| d.child("/"))
                            .child(
                                div()
                                    .id(SharedString::from(format!("rf-crumb-{i}")))
                                    .px(px(2.))
                                    .rounded(px(3.))
                                    .cursor_pointer()
                                    .when(i == last, |d| d.text_color(p.fg2))
                                    .hover(|s| s.text_color(p.fg).bg(p.sel))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.list(Some(target.clone()), cx);
                                    }))
                                    .child(label),
                            )
                    }))
                    .into_any_element()
            }
            (None, None) => div()
                .font_family(MONO)
                .text_size(px(10.5))
                .text_color(p.fg3)
                .child(path)
                .into_any_element(),
        };
        let icon_btn = |id: &'static str, label: &'static str, p: &Palette| {
            div()
                .id(id)
                .px(px(6.))
                .h(px(22.))
                .flex()
                .items_center()
                .rounded(px(4.))
                .text_size(px(12.))
                .text_color(p.fg2)
                .hover(|s| s.bg(p.hover))
                .child(label)
        };
        let header =
            div()
                .flex_none()
                .px(px(8.))
                .pb(px(6.))
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(2.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(12.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .truncate()
                                .child(host_name.to_owned()),
                        )
                        .child(icon_btn("rf-up", "↑", &p).on_click(cx.listener(
                            |this, _, _, cx| {
                                if let Some(up) = this.path.as_deref().and_then(parent) {
                                    this.list(Some(up), cx);
                                }
                            },
                        )))
                        .child(icon_btn("rf-home", "~", &p).on_click(
                            cx.listener(|this, _, _, cx| this.list(Some(PathBuf::from("~")), cx)),
                        ))
                        .child(
                            icon_btn("rf-refresh", "⟳", &p)
                                .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                        )
                        .child(
                            icon_btn("rf-hidden", if self.show_hidden { "●" } else { "○" }, &p)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.show_hidden = !this.show_hidden;
                                    cx.notify();
                                })),
                        )
                        .child(icon_btn("rf-tab", "⇆", &p).on_click(cx.listener(
                            |this, _, _, cx| cx.emit(RemoteFilesEvent::OpenTab(this.host.clone())),
                        )))
                        .child(
                            icon_btn("rf-upload", "Upload", &p)
                                .on_click(cx.listener(|this, _, _, cx| this.pick_upload(cx))),
                        ),
                )
                .child(path_line);

        let list: AnyElement = if let Some(e) = &self.error {
            div()
                .flex_1()
                .p(px(12.))
                .text_size(px(12.))
                .text_color(p.prod)
                .child(e.clone())
                .into_any_element()
        } else if self.path.is_none() {
            div()
                .flex_1()
                .p(px(12.))
                .text_size(px(12.))
                .text_color(p.fg3)
                .child("Opening SFTP on the Host's session…")
                .into_any_element()
        } else {
            let selected = self.selected.clone();
            let confirm = self.confirm_delete.clone();
            uniform_list(
                "rf-list",
                count,
                cx.processor(move |this, range: std::ops::Range<usize>, _w, cx| {
                    let p = palette(cx);
                    range
                        .filter_map(|i| rows.get(i).cloned())
                        .map(|e| {
                            let is_sel = selected.as_deref() == Some(e.name.as_str());
                            let confirming = confirm.as_deref() == Some(e.name.as_str());
                            let (icon, icon_color) = match &e.kind {
                                EntryKind::Dir => ("▸", p.acc),
                                EntryKind::Symlink(_) => ("↪", p.fg3),
                                EntryKind::File => ("·", p.fg3),
                            };
                            let entry = e.clone();
                            let dl = e.clone();
                            let del = e.clone();
                            let dir = this.path.clone().unwrap_or_default();
                            div()
                                .id(SharedString::from(format!("rf-{}", e.name)))
                                .group("rf-row")
                                .w_full()
                                .h(px(24.))
                                .px(px(8.))
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .text_size(px(12.5))
                                .when(is_sel, |d| d.bg(p.sel))
                                .when(!is_sel, |d| d.hover(|s| s.bg(p.hover)))
                                .on_click(cx.listener(move |this, _, _, cx| this.click(&entry, cx)))
                                .child(div().w(px(10.)).text_color(icon_color).child(icon))
                                .child(div().flex_1().min_w_0().truncate().child(e.name.clone()))
                                .when(!e.is_dir(), |d| {
                                    d.child(
                                        div()
                                            .font_family(MONO)
                                            .text_size(px(10.5))
                                            .text_color(p.fg3)
                                            .child(human(e.size)),
                                    )
                                })
                                .child(
                                    div()
                                        .flex()
                                        .gap(px(2.))
                                        .when(!confirming, |d| {
                                            d.invisible().group_hover("rf-row", |s| s.visible())
                                        })
                                        .child(
                                            div()
                                                .id(SharedString::from(format!("rf-dl-{}", e.name)))
                                                .px(px(4.))
                                                .rounded(px(3.))
                                                .text_color(p.fg2)
                                                .hover(|s| s.bg(p.hover))
                                                .on_click({
                                                    let dir = dir.clone();
                                                    cx.listener(move |this, _, _, cx| {
                                                        cx.stop_propagation();
                                                        this.start(
                                                            false,
                                                            join(&dir, &dl.name),
                                                            None,
                                                            OnConflict::KeepBoth,
                                                            cx,
                                                        );
                                                    })
                                                })
                                                .child("↓"),
                                        )
                                        .child(
                                            div()
                                                .id(SharedString::from(format!("rf-rm-{}", e.name)))
                                                .px(px(4.))
                                                .rounded(px(3.))
                                                .text_color(if confirming { p.prod } else { p.fg2 })
                                                .hover(|s| s.bg(p.hover))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    cx.stop_propagation();
                                                    if this.confirm_delete.as_deref()
                                                        == Some(del.name.as_str())
                                                    {
                                                        let request = next_id();
                                                        this.op = Some(request);
                                                        this.confirm_delete = None;
                                                        this.core.send(Command::FsOp {
                                                            request,
                                                            fs: this.fs(),
                                                            op: FsOp::Delete(join(&dir, &del.name)),
                                                        });
                                                    } else {
                                                        this.confirm_delete =
                                                            Some(del.name.clone());
                                                    }
                                                    cx.notify();
                                                }))
                                                .child(if confirming { "Delete?" } else { "✕" }),
                                        ),
                                )
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .flex_1()
            .into_any_element()
        };

        let fs = self.fs();
        let transfers: Vec<AnyElement> = self.transfers.update(cx, |t, cx| {
            let mine: Vec<_> = t
                .items
                .iter()
                .filter(|i| i.from == fs || i.to == fs)
                .rev()
                .take(6)
                .cloned()
                .collect();
            mine.iter()
                .map(|i| t.render_item(i, true, &p, cx))
                .collect()
        });

        div()
            .id("remote-files")
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .drag_over::<ExternalPaths>(move |s, _, _, _| s.bg(p.sel))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.upload(paths.paths().to_vec(), cx);
            }))
            .child(header)
            .child(list)
            .when(!transfers.is_empty(), |d| {
                d.child(
                    div()
                        .flex_none()
                        .border_t_1()
                        .border_color(p.bd)
                        .py(px(4.))
                        .flex()
                        .flex_col()
                        .children(transfers),
                )
            })
            .child(
                div()
                    .flex_none()
                    .px(px(8.))
                    .py(px(4.))
                    .text_size(px(10.5))
                    .text_color(p.fg3)
                    .child("Drop files here to upload · double-click to open"),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_paths() {
        assert_eq!(join(Path::new("/srv"), "a"), PathBuf::from("/srv/a"));
        assert_eq!(parent(Path::new("/srv/app")), Some(PathBuf::from("/srv")));
        assert_eq!(parent(Path::new("/srv")), Some(PathBuf::from("/")));
        assert_eq!(parent(Path::new("/")), None);
        assert_eq!(human(1536), "1.5 KB");
    }

    #[test]
    fn typed_folders() {
        let here = Some(Path::new("/srv/app"));
        assert_eq!(
            typed_target(here, "logs"),
            Some(PathBuf::from("/srv/app/logs"))
        );
        assert_eq!(typed_target(here, " /etc "), Some(PathBuf::from("/etc")));
        assert_eq!(typed_target(here, "~/x"), Some(PathBuf::from("~/x")));
        assert_eq!(typed_target(here, "  "), None);
        assert_eq!(typed_target(None, "logs"), None);
        let c: Vec<_> = crumbs(Path::new("/srv/app"))
            .into_iter()
            .map(|c| c.0)
            .collect();
        assert_eq!(c, ["/", "srv", "app"]);
        assert_eq!(crumbs(Path::new("/"))[0].1, PathBuf::from("/"));
    }
}
