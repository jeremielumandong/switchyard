//! The sidebar's Files panel for an SSH Host: browse the server over SFTP (on the Host's
//! shared session), drop files from the OS to upload, download, delete, and open files in
//! an editor tab. Transfers show under the list with progress and cancel.

use std::path::{Path, PathBuf};

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, EventEmitter, ExternalPaths, FontWeight, InteractiveElement as _,
    IntoElement, ParentElement as _, PathPromptOptions, SharedString,
    StatefulInteractiveElement as _, Styled as _, div, px, relative, uniform_list,
};
use switchyard_core::remote::{EntryKind, FileEntry};
use switchyard_core::store::ProfileId;
use switchyard_core::{
    Command, Event, FsOp, FsRef, OnConflict, RequestId, RuntimeHandle, TransferError,
};

use crate::app_state::next_id;
use crate::theme::{MONO, Palette, palette};
use crate::ui::{self, Kind};

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
}

#[derive(Clone, Debug, PartialEq)]
enum TransferState {
    Running {
        done: u64,
        total: Option<u64>,
    },
    Done(PathBuf),
    Failed(String),
    /// Target exists: ask Replace / Keep both / Skip.
    Exists,
}

#[derive(Clone, Debug)]
struct Transfer {
    id: u64,
    name: String,
    upload: bool,
    /// The command, to re-send with a conflict policy.
    from: PathBuf,
    dir: Option<PathBuf>,
    state: TransferState,
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
    transfers: Vec<Transfer>,
    last_click: Option<(String, std::time::Instant)>,
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
    pub fn new(core: RuntimeHandle, host: ProfileId, cx: &mut Context<Self>) -> Self {
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
            transfers: Vec::new(),
            last_click: None,
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
        let id = next_id();
        let name = from
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (src, dst) = if upload {
            (FsRef::Local, self.fs())
        } else {
            (self.fs(), FsRef::Local)
        };
        self.core.send(Command::Transfer {
            id,
            from: src,
            path: from.clone(),
            to: dst,
            dir: dir.clone(),
            on_conflict,
        });
        self.transfers.push(Transfer {
            id,
            name,
            upload,
            from,
            dir,
            state: TransferState::Running {
                done: 0,
                total: None,
            },
        });
        // Keep the list short: finished transfers beyond the last 6 go away.
        let finished = self
            .transfers
            .iter()
            .filter(|t| matches!(t.state, TransferState::Done(_)))
            .count();
        if finished > 6
            && let Some(i) = self
                .transfers
                .iter()
                .position(|t| matches!(t.state, TransferState::Done(_)))
        {
            self.transfers.remove(i);
        }
        cx.notify();
    }

    fn resolve(&mut self, id: u64, policy: Option<OnConflict>, cx: &mut Context<Self>) {
        let Some(i) = self.transfers.iter().position(|t| t.id == id) else {
            return;
        };
        let t = self.transfers.remove(i);
        if let Some(policy) = policy {
            self.start(t.upload, t.from, t.dir, policy, cx);
        }
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
            Event::TransferProgress {
                id, done, total, ..
            } => {
                if let Some(t) = self.transfers.iter_mut().find(|t| t.id == *id) {
                    t.state = TransferState::Running {
                        done: *done,
                        total: *total,
                    };
                }
            }
            Event::TransferDone { id, result } => {
                let Some(t) = self.transfers.iter_mut().find(|t| t.id == *id) else {
                    return;
                };
                let upload = t.upload;
                t.state = match result {
                    Ok(p) => TransferState::Done(p.clone()),
                    Err(TransferError::Exists(_)) => TransferState::Exists,
                    Err(TransferError::Cancelled) => TransferState::Failed("Cancelled".into()),
                    Err(TransferError::Failed(e)) => TransferState::Failed(e.clone()),
                };
                if upload && result.is_ok() {
                    self.refresh(cx);
                }
            }
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
        let header = div()
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
                    .child(
                        icon_btn("rf-up", "↑", &p).on_click(cx.listener(|this, _, _, cx| {
                            if let Some(up) = this.path.as_deref().and_then(parent) {
                                this.list(Some(up), cx);
                            }
                        })),
                    )
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
                    .child(
                        icon_btn("rf-upload", "Upload", &p)
                            .on_click(cx.listener(|this, _, _, cx| this.pick_upload(cx))),
                    ),
            )
            .child(
                div()
                    .font_family(MONO)
                    .text_size(px(10.5))
                    .text_color(p.fg3)
                    .truncate()
                    .child(path),
            );

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

        let transfers: Vec<AnyElement> = self
            .transfers
            .iter()
            .map(|t| self.render_transfer(t, &p, cx))
            .collect();

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

    fn render_transfer(&self, t: &Transfer, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let id = t.id;
        let arrow = if t.upload { "↑" } else { "↓" };
        let (status, color, frac): (String, _, Option<f32>) = match &t.state {
            TransferState::Running { done, total } => match total {
                Some(tot) if *tot > 0 => (
                    format!("{} / {}", human(*done), human(*tot)),
                    p.fg2,
                    Some((*done as f32 / *tot as f32).min(1.0)),
                ),
                _ => ("starting…".into(), p.fg3, Some(0.0)),
            },
            TransferState::Done(path) => (
                if t.upload {
                    "uploaded".into()
                } else {
                    format!("saved to {}", path.display())
                },
                p.dev,
                None,
            ),
            TransferState::Failed(e) => (e.clone(), p.prod, None),
            TransferState::Exists => ("already exists".into(), p.stg, None),
        };
        let small = |id: String, label: &'static str, kind: Kind| {
            ui::button(SharedString::from(id), label, kind, p)
                .h(px(20.))
                .px(px(6.))
                .text_size(px(11.))
        };
        div()
            .px(px(8.))
            .py(px(3.))
            .flex()
            .flex_col()
            .gap(px(3.))
            .text_size(px(11.5))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(div().text_color(p.fg3).child(arrow))
                    .child(div().flex_1().min_w_0().truncate().child(t.name.clone()))
                    .when(matches!(t.state, TransferState::Running { .. }), |d| {
                        d.child(
                            div()
                                .id(SharedString::from(format!("tx-x-{id}")))
                                .px(px(4.))
                                .text_color(p.fg3)
                                .hover(|s| s.text_color(p.fg))
                                .on_click(cx.listener(move |this, _, _, _| {
                                    this.core.send(Command::CancelTransfer { id });
                                }))
                                .child("✕"),
                        )
                    })
                    .when(
                        matches!(t.state, TransferState::Done(_) | TransferState::Failed(_)),
                        |d| {
                            d.child(
                                div()
                                    .id(SharedString::from(format!("tx-rm-{id}")))
                                    .px(px(4.))
                                    .text_color(p.fg3)
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| {
                                            this.resolve(id, None, cx)
                                        }),
                                    )
                                    .child("×"),
                            )
                        },
                    ),
            )
            .when_some(frac, |d, f| {
                d.child(
                    div()
                        .h(px(3.))
                        .rounded(px(2.))
                        .bg(p.bd)
                        .child(div().h_full().rounded(px(2.)).bg(p.acc).w(relative(f))),
                )
            })
            .child(
                div()
                    .font_family(MONO)
                    .text_size(px(10.5))
                    .text_color(color)
                    .truncate()
                    .child(status),
            )
            .when(t.state == TransferState::Exists, |d| {
                d.child(
                    div()
                        .flex()
                        .gap(px(4.))
                        .child(
                            small(format!("tx-rep-{id}"), "Replace", Kind::Secondary).on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.resolve(id, Some(OnConflict::Replace), cx)
                                }),
                            ),
                        )
                        .child(
                            small(format!("tx-both-{id}"), "Keep both", Kind::Secondary).on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.resolve(id, Some(OnConflict::KeepBoth), cx)
                                }),
                            ),
                        )
                        .child(
                            small(format!("tx-skip-{id}"), "Skip", Kind::Ghost).on_click(
                                cx.listener(move |this, _, _, cx| this.resolve(id, None, cx)),
                            ),
                        ),
                )
            })
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
}
