//! A text editor tab for a file on an SSH Host (or this computer). Save writes it back over
//! SFTP after checking nobody changed it meanwhile; a conflict asks before overwriting.

use std::path::PathBuf;

use gpui_kit::component::input::{Editor, EditorState, InputEvent};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, actions, div, px, relative,
};
use switchyard_core::{Command, Event, FsRef, RequestId, RuntimeHandle, SaveError};

use crate::app_state::next_id;
use crate::theme::{MONO, palette};
use crate::ui::{self, Kind};

actions!(editor_tab, [SaveFile]);

/// Bind Ctrl/Cmd+S inside editor tabs.
pub fn init(cx: &mut gpui_kit::App) {
    cx.bind_keys([gpui_kit::KeyBinding::new(
        "secondary-s",
        SaveFile,
        Some("EditorTab"),
    )]);
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
    pub fn new(
        core: RuntimeHandle,
        fs: FsRef,
        path: PathBuf,
        host_name: &str,
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
        Self {
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
        }
    }

    /// Whether this tab shows `path` on `fs`.
    pub fn shows(&self, fs: &FsRef, path: &std::path::Path) -> bool {
        &self.fs == fs && self.path == path
    }

    fn save(&mut self, force: bool, cx: &mut Context<Self>) {
        if matches!(
            self.state,
            State::Loading | State::LoadFailed(_) | State::Saving
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
                    Err(e) => self.state = State::LoadFailed(e.clone()),
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
                    .gap(px(8.))
                    .px(px(12.))
                    .py(px(6.))
                    .bg(p.stg_bg)
                    .text_size(px(12.))
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
                    .px(px(12.))
                    .py(px(6.))
                    .bg(p.prod_bg)
                    .text_color(p.prod)
                    .text_size(px(12.))
                    .child(e.clone())
                    .into_any_element(),
            ),
            _ => None,
        };
        let status = match &self.state {
            State::Loading => "Loading…",
            State::Saving => "Saving…",
            _ if self.dirty => "Unsaved changes · ⌘S / Ctrl+S saves",
            _ => "Saved",
        };
        div()
            .key_context("EditorTab")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &SaveFile, _, cx| this.save(false, cx)))
            .size_full()
            .flex()
            .flex_col()
            .bg(p.surface)
            .child(
                div()
                    .h(px(30.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(12.))
                    .border_b_1()
                    .border_color(p.bd)
                    .bg(p.panel)
                    .text_size(px(12.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_family(MONO)
                            .text_size(px(11.5))
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
                        ui::button("ed-save", "Save", Kind::Primary, &p)
                            .h(px(22.))
                            .text_size(px(11.5))
                            .when(!self.dirty, |b| b.opacity(0.6))
                            .on_click(cx.listener(|this, _, _, cx| this.save(false, cx))),
                    ),
            )
            .children(banner)
            .child(
                div().flex_1().min_h_0().child(
                    Editor::new(&self.editor)
                        .bordered(false)
                        .appearance(false)
                        .h(relative(1.))
                        .font_family(MONO)
                        .text_size(px(12.5)),
                ),
            )
            .when(self.state == State::Loading, |d| {
                d.child(
                    div()
                        .absolute()
                        .top(px(40.))
                        .left(px(16.))
                        .text_size(px(12.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(p.fg3)
                        .child("Loading…"),
                )
            })
    }
}
