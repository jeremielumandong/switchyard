//! A text editor tab for a file on an SSH Host (or this computer). Save writes it back over
//! SFTP after checking nobody changed it meanwhile; a conflict asks before overwriting.
//! A terminal on the same Host, started in the file's folder, sits under the editor.
//! A binary file shows instead of the editor: an image in a viewer, anything else as a
//! card; either can open in the computer's default app (a copy downloads to a temp folder
//! through the transfer queue, so it shows progress and can be cancelled).

use std::path::PathBuf;

use gpui_kit::component::input::{Editor, EditorState, InputEvent};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement as _, IntoElement, MouseButton, MouseMoveEvent, ParentElement as _, Render,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, Subscription, Window, actions,
    div, px, relative,
};
use switchyard_core::store::EnvironmentLabel;
use switchyard_core::{
    Command, Event, FsRef, OnConflict, ReadError, RequestId, RuntimeHandle, SaveError,
    TransferError,
};

use crate::app_state::next_id;
use crate::appearance::{rpx, ts};
use crate::terminal_tab::TerminalTab;
use crate::theme::{MONO, palette};
use crate::transfers::Transfers;
use crate::ui::{self, Kind};

actions!(editor_tab, [SaveFile, ToggleTerminal]);

/// Bind Ctrl/Cmd+S and Ctrl+` inside editor tabs.
pub fn init(cx: &mut gpui_kit::App) {
    cx.bind_keys([
        gpui_kit::KeyBinding::new("secondary-s", SaveFile, Some("EditorTab")),
        gpui_kit::KeyBinding::new("ctrl-`", ToggleTerminal, Some("EditorTab")),
    ]);
}

/// Default and limits of the terminal's height under the editor.
const TERM_HEIGHT: f32 = 260.;
const TERM_MIN: f32 = 90.;

/// ` cd '<folder>' && clear` for a POSIX shell (the leading space keeps it out of history
/// where `HISTCONTROL` ignores spaces).
fn cd_into(dir: &str) -> String {
    format!(" cd '{}' && clear\r", dir.replace('\'', "'\\''"))
}

/// The folder holding `path`, POSIX style.
fn posix_parent(path: &std::path::Path) -> Option<String> {
    let s = path.to_string_lossy().replace('\\', "/");
    let (dir, _) = s.trim_end_matches('/').rsplit_once('/')?;
    Some(if dir.is_empty() {
        "/".into()
    } else {
        dir.into()
    })
}

#[derive(Clone, Debug, PartialEq)]
enum State {
    Loading,
    Ready,
    LoadFailed(String),
    Saving,
    /// Changed on the server since it was opened.
    Conflict,
    SaveFailed(String),
    /// Not text: shown by [`Viewer`], never saved.
    Binary,
}

/// What a binary file shows.
struct Viewer {
    size: u64,
    /// Decoded once, when it is an image.
    image: Option<(gpui_kit::ImageFormat, std::sync::Arc<gpui_kit::Image>)>,
    /// The download for "Open with default app", while it runs.
    opening: Option<u64>,
    /// Why the last open failed.
    open_error: Option<String>,
}

/// An editor tab.
pub struct EditorTab {
    core: RuntimeHandle,
    /// Where the file lives.
    pub fs: FsRef,
    /// Its path there.
    pub path: PathBuf,
    /// Tab title (`name · host`).
    pub title: String,
    editor: Entity<EditorState>,
    modified_ms: Option<i64>,
    /// Unsaved changes.
    pub dirty: bool,
    state: State,
    request: Option<RequestId>,
    /// Ignore the change event fired by loading the contents.
    loading_text: bool,
    focus: FocusHandle,
    _sub: Subscription,
    host_name: String,
    env: EnvironmentLabel,
    /// The terminal under the editor, created the first time it is shown.
    pub terminal: Option<Entity<TerminalTab>>,
    show_terminal: bool,
    term_height: f32,
    /// Splitter drag: (mouse y, height) when it started.
    drag: Option<(f32, f32)>,
    transfers: Entity<Transfers>,
    viewer: Option<Viewer>,
}

fn language(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("sql") => "sql",
        Some("json") => "json",
        _ => "text",
    }
}

impl EditorTab {
    /// Open `path` on `fs`; the contents arrive asynchronously.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        core: RuntimeHandle,
        fs: FsRef,
        path: PathBuf,
        host_name: &str,
        env: EnvironmentLabel,
        transfers: Entity<Transfers>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let lang = language(&path);
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language(lang)
                .line_number(true)
                .indent_guides(false)
                .soft_wrap(false)
        });
        let sub = cx.subscribe_in(&editor, window, |this, _, ev: &InputEvent, _, cx| {
            if let InputEvent::Change = ev {
                if this.loading_text {
                    this.loading_text = false;
                } else if !this.dirty {
                    this.dirty = true;
                    cx.notify();
                }
            }
        });
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let request = next_id();
        core.send(Command::ReadTextFile {
            request,
            fs: fs.clone(),
            path: path.clone(),
        });
        let mut this = Self {
            core,
            fs,
            path,
            title: format!("{name} · {host_name}"),
            editor,
            modified_ms: None,
            dirty: false,
            state: State::Loading,
            request: Some(request),
            loading_text: false,
            focus: cx.focus_handle(),
            _sub: sub,
            host_name: host_name.to_owned(),
            env,
            terminal: None,
            show_terminal: false,
            term_height: TERM_HEIGHT,
            drag: None,
            transfers,
            viewer: None,
        };
        this.set_terminal(true, cx);
        this
    }

    /// Show or hide the terminal; hiding keeps its shell running.
    fn set_terminal(&mut self, show: bool, cx: &mut Context<Self>) {
        self.show_terminal = show;
        if show && self.terminal.is_none() {
            let host = match &self.fs {
                FsRef::Host(h) => Some(h.clone()),
                FsRef::Local | FsRef::Conn(_) => None,
            };
            // An FTP file has no shell beside it: the terminal starts where it starts.
            let start = if host.is_some() || (cfg!(unix) && self.fs == FsRef::Local) {
                posix_parent(&self.path).map(|d| cd_into(&d))
            } else {
                None
            };
            let (core, name, env) = (self.core.clone(), self.host_name.clone(), self.env);
            self.terminal = Some(cx.new(|cx| {
                let t = TerminalTab::new(core, name, env, host, cx);
                match start {
                    Some(s) => t.with_startup(s),
                    None => t,
                }
            }));
        }
        cx.notify();
    }

    /// Close the terminal's shell (tab closed).
    pub fn shutdown(&mut self, cx: &mut Context<Self>) {
        if let Some(t) = self.terminal.take() {
            t.update(cx, |t, _| t.shutdown());
        }
    }

    /// Whether this tab shows `path` on `fs`.
    pub fn shows(&self, fs: &FsRef, path: &std::path::Path) -> bool {
        &self.fs == fs && self.path == path
    }

    /// Whether a plain save can start now (the file loaded and no save or conflict is
    /// pending). A conflict needs the banner's Overwrite / Reload choice first.
    pub fn can_save(&self) -> bool {
        matches!(self.state, State::Ready | State::SaveFailed(_))
    }

    /// Save the file (not over a server-side change).
    pub fn save_file(&mut self, cx: &mut Context<Self>) {
        self.save(false, cx);
    }

    /// After [`Self::save_file`]: `None` while the save runs, `Some(true)` once the file
    /// is saved, `Some(false)` if it failed or hit a conflict.
    pub fn save_outcome(&self) -> Option<bool> {
        match self.state {
            State::Saving | State::Loading => None,
            State::Ready => Some(!self.dirty),
            // Nothing to save.
            State::Binary => Some(true),
            State::Conflict | State::SaveFailed(_) | State::LoadFailed(_) => Some(false),
        }
    }

    fn save(&mut self, force: bool, cx: &mut Context<Self>) {
        if matches!(
            self.state,
            State::Loading | State::LoadFailed(_) | State::Saving | State::Binary
        ) {
            return;
        }
        let request = next_id();
        self.request = Some(request);
        self.state = State::Saving;
        self.core.send(Command::WriteTextFile {
            request,
            fs: self.fs.clone(),
            path: self.path.clone(),
            content: self.editor.read(cx).value().to_string(),
            expect_modified: self.modified_ms,
            force,
        });
        cx.notify();
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let request = next_id();
        self.request = Some(request);
        self.state = State::Loading;
        self.core.send(Command::ReadTextFile {
            request,
            fs: self.fs.clone(),
            path: self.path.clone(),
        });
        cx.notify();
    }

    /// Runtime answers for this tab.
    pub fn on_event(&mut self, ev: &Event, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            Event::TextFileRead { request, result } if Some(*request) == self.request => {
                self.request = None;
                match result {
                    Ok(f) => {
                        self.modified_ms = f.modified_ms;
                        self.loading_text = true;
                        let text = f.content.clone();
                        self.editor
                            .update(cx, |e, cx| e.set_value(text, window, cx));
                        self.dirty = false;
                        self.state = State::Ready;
                        self.editor.update(cx, |e, cx| e.focus(window, cx));
                    }
                    Err(ReadError::Binary { size, image }) => {
                        let image = image.as_ref().and_then(|b| {
                            let f = crate::viewer::image_format(&b.0)?;
                            let img = gpui_kit::Image::from_bytes(f, b.0.to_vec());
                            Some((f, std::sync::Arc::new(img)))
                        });
                        self.viewer = Some(Viewer {
                            size: *size,
                            image,
                            opening: None,
                            open_error: None,
                        });
                        self.state = State::Binary;
                    }
                    Err(ReadError::Failed(e)) => self.state = State::LoadFailed(e.clone()),
                }
            }
            Event::TransferDone { id, result }
                if self.viewer.as_ref().and_then(|v| v.opening) == Some(*id) =>
            {
                if let Some(v) = self.viewer.as_mut() {
                    v.opening = None;
                    match result {
                        Ok(local) => cx.open_with_system(local),
                        Err(TransferError::Cancelled | TransferError::Paused) => {}
                        Err(e) => v.open_error = Some(transfer_error(e)),
                    }
                }
            }
            Event::TextFileSaved { request, result } if Some(*request) == self.request => {
                self.request = None;
                match result {
                    Ok(m) => {
                        self.modified_ms = *m;
                        self.dirty = false;
                        self.state = State::Ready;
                    }
                    Err(SaveError::Conflict(_)) => self.state = State::Conflict,
                    Err(SaveError::Failed(e)) => self.state = State::SaveFailed(e.clone()),
                }
            }
            _ => return,
        }
        cx.notify();
    }
}

/// A failed download, for the viewer.
fn transfer_error(e: &TransferError) -> String {
    match e {
        TransferError::Failed(e) => format!("Could not download it: {e}"),
        TransferError::Exists(_) | TransferError::Partial(_) => {
            "Could not download it: a copy is already there".into()
        }
        TransferError::Cancelled | TransferError::Paused => String::new(),
    }
}

/// `1.2 KB`, `3.4 MB`.
fn human_size(n: u64) -> String {
    match n {
        0..1024 => format!("{n} bytes"),
        1024..1_048_576 => format!("{:.1} KB", n as f64 / 1024.),
        1_048_576..1_073_741_824 => format!("{:.1} MB", n as f64 / 1_048_576.),
        _ => format!("{:.1} GB", n as f64 / 1_073_741_824.),
    }
}

impl EditorTab {
    /// Open the file in the computer's default app for its type: a local file directly,
    /// a remote one after downloading a copy to a temp folder.
    fn open_with_system(&mut self, cx: &mut Context<Self>) {
        let Some(v) = self.viewer.as_mut() else {
            return;
        };
        if v.opening.is_some() {
            return;
        }
        v.open_error = None;
        if self.fs == FsRef::Local {
            cx.open_with_system(&self.path);
            return;
        }
        let dir = switchyard_core::files::open_copy_dir(next_id());
        let (fs, path) = (self.fs.clone(), self.path.clone());
        let id = self.transfers.update(cx, |t, cx| {
            t.start(fs, path, FsRef::Local, Some(dir), OnConflict::Replace, cx)
        });
        v.opening = Some(id);
        cx.notify();
    }

    /// The binary file view: the image, or a card saying what it is.
    fn render_viewer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let v = self.viewer.as_ref()?;
        let p = palette(cx);
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let what = match &v.image {
            Some((f, _)) => format!(
                "{} image · {}",
                crate::viewer::format_name(*f),
                human_size(v.size)
            ),
            None => format!("Binary file · {}", human_size(v.size)),
        };
        let open = ui::button(
            "ed-open-app",
            if v.opening.is_some() {
                "Downloading…"
            } else {
                "Open with default app"
            },
            if v.image.is_some() {
                Kind::Secondary
            } else {
                Kind::Primary
            },
            &p,
        )
        .when(v.opening.is_some(), |b| b.opacity(0.6))
        .on_click(cx.listener(|this, _, _, cx| this.open_with_system(cx)));
        let error = v
            .open_error
            .clone()
            .map(|e| div().text_size(ts::BODY).text_color(p.prod).child(e));
        let body = match &v.image {
            Some((_, img)) => div()
                .flex_1()
                .min_h_0()
                .p(rpx(16.))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    gpui_kit::img(img.clone())
                        .max_w_full()
                        .max_h_full()
                        .object_fit(gpui_kit::ObjectFit::Contain),
                )
                .into_any_element(),
            None => div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(rpx(10.))
                .child(
                    div()
                        .text_size(ts::BODY)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(p.fg)
                        .child(name),
                )
                .child(
                    div()
                        .text_size(ts::BODY)
                        .text_color(p.fg3)
                        .child("This file can't be edited as text. Open it in the app your computer uses for it."),
                )
                .into_any_element(),
        };
        Some(
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(body)
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap(rpx(10.))
                        .px(rpx(12.))
                        .pb(rpx(12.))
                        .child(div().text_size(ts::LABEL).text_color(p.fg3).child(what))
                        .child(open)
                        .children(error),
                )
                .into_any_element(),
        )
    }
}

impl Focusable for EditorTab {
    fn focus_handle(&self, _cx: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for EditorTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let banner: Option<AnyElement> = match &self.state {
            State::Conflict => Some(
                div()
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .px(rpx(12.))
                    .py(rpx(6.))
                    .bg(p.stg_bg)
                    .text_size(ts::BODY)
                    .child(
                        div()
                            .flex_1()
                            .child("This file changed on the server since you opened it."),
                    )
                    .child(
                        ui::button("ed-overwrite", "Overwrite", Kind::Secondary, &p).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.state = State::Ready;
                                this.save(true, cx)
                            }),
                        ),
                    )
                    .child(
                        ui::button("ed-reload", "Discard mine and reload", Kind::Ghost, &p)
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                    )
                    .into_any_element(),
            ),
            State::SaveFailed(e) | State::LoadFailed(e) => Some(
                div()
                    .px(rpx(12.))
                    .py(rpx(6.))
                    .bg(p.prod_bg)
                    .text_color(p.prod)
                    .text_size(ts::BODY)
                    .child(e.clone())
                    .into_any_element(),
            ),
            _ => None,
        };
        let binary = self.state == State::Binary;
        let viewer = self.render_viewer(cx);
        let status = match &self.state {
            State::Loading => "Loading…",
            State::Binary => "Read only",
            State::Saving => "Saving…",
            _ if self.dirty => "Unsaved changes · ⌘S / Ctrl+S saves",
            _ => "Saved",
        };
        let terminal = self
            .terminal
            .clone()
            .filter(|_| self.show_terminal)
            .map(|t| {
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .id("ed-splitter")
                            .h(rpx(6.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .border_t_1()
                            .border_color(p.bd)
                            .bg(p.panel)
                            .cursor_row_resize()
                            .hover(|s| s.bg(p.hover))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, ev: &gpui_kit::MouseDownEvent, _, cx| {
                                    this.drag = Some((ev.position.y.into(), this.term_height));
                                    cx.stop_propagation();
                                }),
                            )
                            .child(div().w(rpx(28.)).h(rpx(2.)).rounded(px(2.)).bg(p.bd2)),
                    )
                    .child(div().h(rpx(self.term_height)).child(t))
            });
        div()
            .key_context("EditorTab")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &SaveFile, _, cx| this.save(false, cx)))
            .on_action(cx.listener(|this, _: &ToggleTerminal, _, cx| {
                let show = !this.show_terminal;
                this.set_terminal(show, cx)
            }))
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
                if let Some((y0, h0)) = this.drag {
                    if ev.pressed_button == Some(MouseButton::Left) {
                        let y: f32 = ev.position.y.into();
                        // In 100 % design units (drawn with `rpx`).
                        let z = crate::appearance::zoom(cx);
                        let max =
                            (f32::from(window.viewport_size().height) / z - 200.).max(TERM_MIN);
                        this.term_height = (h0 - (y - y0) / z).clamp(TERM_MIN, max);
                        cx.notify();
                    } else {
                        this.drag = None;
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.drag = None),
            )
            .size_full()
            .flex()
            .flex_col()
            .bg(p.surface)
            .child(
                div()
                    .h(rpx(30.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .px(rpx(12.))
                    .border_b_1()
                    .border_color(p.bd)
                    .bg(p.panel)
                    .text_size(ts::BODY)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_family(MONO)
                            .text_size(ts::LABEL)
                            .text_color(p.fg2)
                            .truncate()
                            .child(self.path.to_string_lossy().into_owned()),
                    )
                    .child(
                        div()
                            .text_color(if self.dirty { p.stg } else { p.fg3 })
                            .child(status),
                    )
                    .child(
                        ui::button(
                            "ed-term",
                            "Terminal",
                            if self.show_terminal {
                                Kind::Secondary
                            } else {
                                Kind::Ghost
                            },
                            &p,
                        )
                        .h(rpx(22.))
                        .text_size(ts::LABEL)
                        .on_click(cx.listener(|this, _, _, cx| {
                            let show = !this.show_terminal;
                            this.set_terminal(show, cx)
                        })),
                    )
                    .when(!binary, |d| {
                        d.child(
                            ui::button("ed-save", "Save", Kind::Primary, &p)
                                .h(rpx(22.))
                                .text_size(ts::LABEL)
                                .when(!self.dirty, |b| b.opacity(0.6))
                                .on_click(cx.listener(|this, _, _, cx| this.save(false, cx))),
                        )
                    }),
            )
            .children(banner)
            .children(viewer)
            .when(!binary, |d| {
                d.child(
                    div().flex_1().min_h_0().child(
                        Editor::new(&self.editor)
                            .bordered(false)
                            .appearance(false)
                            .h(relative(1.))
                            .font_family(crate::appearance::editor_font_family(cx))
                            .text_size(crate::appearance::editor_font_size(cx)),
                    ),
                )
            })
            .children(terminal)
            .when(self.state == State::Loading, |d| {
                d.child(
                    div()
                        .absolute()
                        .top(rpx(40.))
                        .left(rpx(16.))
                        .text_size(ts::BODY)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(p.fg3)
                        .child("Loading…"),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_starts_in_the_files_folder() {
        assert_eq!(
            posix_parent(std::path::Path::new("/app/emulsion/docker-compose.yml")).as_deref(),
            Some("/app/emulsion")
        );
        assert_eq!(
            posix_parent(std::path::Path::new("/x")).as_deref(),
            Some("/")
        );
        assert_eq!(cd_into("/srv/it's"), " cd '/srv/it'\\''s' && clear\r");
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(12), "12 bytes");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(3 * 1_048_576), "3.0 MB");
    }
}
