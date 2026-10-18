//! The scratchpad (⇧⌘N / Ctrl+Shift+N): Notepad-style scratch notes and a folder of
//! markdown notes, like a small local Obsidian.
//!
//! A scratch note is text that has no name yet. It is kept as you type (in the notes
//! folder's `.scratch`), so it is still there after a restart, until you close it or
//! save it as a note. Saved notes are `<name>.md` files in the notes folder; their edits
//! save themselves. A note can be read as rendered markdown, and its `[[links]]` open
//! (or create) the notes they name. All file work is the core's
//! ([`switchyard_core::notes`]).

use std::time::Duration;

use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, actions, div, px,
    relative,
};
use switchyard_core::notes::{Note, NotesCommand, NotesEvent, Scratch};
use switchyard_core::{Command, RequestId, RuntimeHandle};

use crate::app_state::next_id;
use crate::appearance::{rpx, ts};
use crate::theme::{MONO, Palette, palette};
use crate::ui::{self, Kind};

actions!(notes_tab, [SaveNote, CloseNote, TogglePreview]);

/// Ctrl/Cmd+S saves a scratch note as a note, Ctrl/Cmd+W closes it, Ctrl/Cmd+E
/// switches between editing and reading.
pub fn init(cx: &mut gpui_kit::App) {
    cx.bind_keys([
        gpui_kit::KeyBinding::new("secondary-s", SaveNote, Some("NotesTab")),
        gpui_kit::KeyBinding::new("secondary-w", CloseNote, Some("NotesTab")),
        gpui_kit::KeyBinding::new("secondary-e", TogglePreview, Some("NotesTab")),
    ]);
}

/// Pause in typing before the text is written.
const AUTOSAVE: Duration = Duration::from_millis(300);
/// Width of the note list.
const LIST_WIDTH: f32 = 240.;

/// The note in the editor.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Current {
    Scratch(String),
    Note(String),
}

/// What the name field above the editor is for.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Naming {
    /// Save scratch note `id` as a note.
    SaveScratch(String),
    /// A new, empty note.
    NewNote,
    /// Rename note `name`.
    Rename(String),
}

/// A question shown above the editor.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Confirm {
    /// Close scratch note `id`, losing its text.
    Discard(String),
    /// Delete note `name`.
    Delete(String),
}

/// The scratchpad tab (one per window).
pub struct NotesTab {
    core: RuntimeHandle,
    focus: FocusHandle,
    /// The notes folder, once loaded.
    dir: Option<std::path::PathBuf>,
    loaded: bool,
    scratch: Vec<Scratch>,
    notes: Vec<Note>,
    current: Option<Current>,
    editor: Entity<EditorState>,
    search: Entity<InputState>,
    name_input: Entity<InputState>,
    naming: Option<(Naming, Option<String>)>,
    confirm: Option<Confirm>,
    /// Waiting for Save as / Rename / a new note: the request and what it was.
    pending: Option<(RequestId, Naming)>,
    preview: bool,
    /// The note whose text changed and is not written yet.
    unwritten: Option<Current>,
    autosave: Option<Task<()>>,
    /// Ignore the change event fired by showing another note.
    setting_text: bool,
    /// The shortcut asked for a new scratch note before the folder was read.
    want_new: bool,
    /// Last error from the notes folder.
    error: Option<String>,
    _subs: Vec<Subscription>,
}

impl NotesTab {
    /// A scratchpad; the notes folder is read in the background.
    pub fn new(core: RuntimeHandle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("text")
                .line_number(false)
                .indent_guides(false)
                .soft_wrap(true)
                .placeholder("Type or paste anything. It stays here until you close it.")
        });
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search notes"));
        let name_input = cx.new(|cx| InputState::new(window, cx).placeholder("Note name"));
        let subs = vec![
            cx.subscribe_in(&editor, window, |this, _, ev: &InputEvent, _, cx| {
                if let InputEvent::Change = ev {
                    this.on_edit(cx);
                }
            }),
            cx.subscribe(&search, |_, _, ev: &InputEvent, cx| {
                if let InputEvent::Change = ev {
                    cx.notify();
                }
            }),
            cx.subscribe_in(
                &name_input,
                window,
                |this, _, ev: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = ev {
                        this.confirm_name(window, cx);
                    }
                },
            ),
        ];
        core.send(Command::Notes(NotesCommand::Load));
        Self {
            core,
            focus: cx.focus_handle(),
            dir: None,
            loaded: false,
            scratch: Vec::new(),
            notes: Vec::new(),
            current: None,
            editor,
            search,
            name_input,
            naming: None,
            confirm: None,
            pending: None,
            preview: false,
            unwritten: None,
            autosave: None,
            setting_text: false,
            want_new: false,
            error: None,
            _subs: subs,
        }
    }

    /// Put the cursor in the editor.
    pub fn focus_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preview {
            window.focus(&self.focus, cx);
        } else {
            self.editor.update(cx, |e, cx| e.focus(window, cx));
        }
    }

    /// Start a new scratch note and show it.
    pub fn new_scratch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.loaded {
            self.want_new = true;
            return;
        }
        // An empty scratch note is reused rather than piling up blank pages.
        let id = match self
            .scratch
            .iter()
            .rev()
            .find(|s| s.content.trim().is_empty())
        {
            Some(s) => s.id.clone(),
            None => {
                let id = scratch_id(&self.scratch);
                self.scratch.push(Scratch {
                    id: id.clone(),
                    content: String::new(),
                });
                id
            }
        };
        self.preview = false;
        self.show(Current::Scratch(id), window, cx);
    }

    /// Write any text not written yet (before the window or tab closes).
    pub fn flush(&mut self, cx: &mut Context<Self>) {
        self.autosave = None;
        let Some(item) = self.unwritten.take() else {
            return;
        };
        let cmd = match item {
            Current::Scratch(id) => match self.scratch.iter().find(|s| s.id == id) {
                Some(s) => NotesCommand::WriteScratch {
                    id,
                    content: s.content.clone(),
                },
                None => return,
            },
            Current::Note(name) => match self.notes.iter().find(|n| n.name == name) {
                Some(n) => NotesCommand::WriteNote {
                    name,
                    content: n.content.clone(),
                },
                None => return,
            },
        };
        self.core.send(Command::Notes(cmd));
        cx.notify();
    }

    /// The tab's title.
    pub fn title(&self) -> SharedString {
        match &self.current {
            Some(Current::Note(name)) => format!("{name} · Notes").into(),
            _ => "Scratchpad".into(),
        }
    }

    /// Scratch notes with text, for the tab's unsaved dot.
    pub fn has_scratch_text(&self) -> bool {
        self.scratch.iter().any(|s| !s.content.trim().is_empty())
    }

    /// A runtime answer about notes.
    pub fn on_event(&mut self, ev: NotesEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            NotesEvent::Loaded {
                dir,
                notes,
                scratch,
            } => {
                self.dir = Some(dir);
                self.notes = notes;
                self.scratch = scratch;
                self.loaded = true;
                let keep = self.current.take().filter(|c| self.exists(c));
                if self.want_new || self.scratch.is_empty() {
                    self.want_new = false;
                    self.new_scratch(window, cx);
                } else {
                    let item = keep.unwrap_or_else(|| match self.scratch.last() {
                        Some(s) => Current::Scratch(s.id.clone()),
                        None => Current::Note(self.notes[0].name.clone()),
                    });
                    self.show(item, window, cx);
                }
                self.focus_editor(window, cx);
            }
            NotesEvent::Saved { request, result } => {
                let Some((_, what)) = self.pending.take_if(|(r, _)| *r == request) else {
                    return;
                };
                match result {
                    Ok(note) => {
                        match &what {
                            Naming::SaveScratch(id) => self.scratch.retain(|s| &s.id != id),
                            Naming::Rename(old) => self.notes.retain(|n| &n.name != old),
                            Naming::NewNote => {}
                        }
                        let name = note.name.clone();
                        self.notes.retain(|n| n.name != name);
                        self.notes.insert(0, note);
                        self.naming = None;
                        if self.unwritten.as_ref().is_some_and(|u| !self.exists(u)) {
                            self.unwritten = None;
                        }
                        if self.scratch.is_empty() && matches!(what, Naming::SaveScratch(_)) {
                            // Keep an empty page ready, like Notepad.
                            let id = scratch_id(&self.scratch);
                            self.scratch.push(Scratch {
                                id,
                                content: String::new(),
                            });
                        }
                        self.show(Current::Note(name), window, cx);
                    }
                    Err(e) => {
                        if let Some((_, err)) = &mut self.naming {
                            *err = Some(e);
                        } else {
                            self.error = Some(e);
                        }
                    }
                }
                cx.notify();
            }
            NotesEvent::Failed(e) => {
                self.error = Some(e);
                cx.notify();
            }
        }
    }

    fn exists(&self, item: &Current) -> bool {
        match item {
            Current::Scratch(id) => self.scratch.iter().any(|s| &s.id == id),
            Current::Note(name) => self.notes.iter().any(|n| &n.name == name),
        }
    }

    fn text_of(&self, item: &Current) -> Option<&str> {
        match item {
            Current::Scratch(id) => self
                .scratch
                .iter()
                .find(|s| &s.id == id)
                .map(|s| s.content.as_str()),
            Current::Note(name) => self
                .notes
                .iter()
                .find(|n| &n.name == name)
                .map(|n| n.content.as_str()),
        }
    }

    /// Show `item` in the editor.
    fn show(&mut self, item: Current, window: &mut Window, cx: &mut Context<Self>) {
        if self.current.as_ref() != Some(&item) {
            self.flush(cx);
            self.confirm = None;
            if !matches!(self.naming, Some((Naming::NewNote, _))) {
                self.naming = None;
            }
        }
        let text = self.text_of(&item).unwrap_or_default().to_owned();
        self.current = Some(item);
        if self.editor.read(cx).value().as_ref() != text {
            self.setting_text = true;
            self.editor
                .update(cx, |e, cx| e.set_value(text, window, cx));
            self.setting_text = false;
        }
        self.focus_editor(window, cx);
        cx.notify();
    }

    fn on_edit(&mut self, cx: &mut Context<Self>) {
        if self.setting_text {
            return;
        }
        let Some(item) = self.current.clone() else {
            return;
        };
        let text = self.editor.read(cx).value().to_string();
        // Showing a note fires a change too (after the fact); nothing was typed.
        if self.text_of(&item) == Some(text.as_str()) {
            return;
        }
        match &item {
            Current::Scratch(id) => {
                if let Some(s) = self.scratch.iter_mut().find(|s| &s.id == id) {
                    s.content = text;
                }
            }
            Current::Note(name) => {
                if let Some(n) = self.notes.iter_mut().find(|n| &n.name == name) {
                    n.content = text;
                    n.modified_ms = now_ms();
                }
            }
        }
        if self.unwritten.as_ref().is_some_and(|u| u != &item) {
            self.flush(cx);
        }
        self.unwritten = Some(item);
        self.autosave = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(AUTOSAVE).await;
            let _ = this.update(cx, |this, cx| this.flush(cx));
        }));
        cx.notify();
    }

    /// Close the current scratch note (asking first when it has text).
    fn close_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Current::Scratch(id)) = self.current.clone() {
            self.close_scratch(id, window, cx);
        }
    }

    fn close_scratch(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let empty = self
            .scratch
            .iter()
            .find(|s| s.id == id)
            .is_none_or(|s| s.content.trim().is_empty());
        if empty {
            self.discard(id, window, cx);
        } else {
            if self.current.as_ref() != Some(&Current::Scratch(id.clone())) {
                self.show(Current::Scratch(id.clone()), window, cx);
            }
            self.confirm = Some(Confirm::Discard(id));
            cx.notify();
        }
    }

    fn discard(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.unwritten == Some(Current::Scratch(id.clone())) {
            self.unwritten = None;
            self.autosave = None;
        }
        let ix = self.scratch.iter().position(|s| s.id == id);
        self.scratch.retain(|s| s.id != id);
        self.core.send(Command::Notes(NotesCommand::CloseScratch {
            id: id.clone(),
        }));
        self.confirm = None;
        if self.current == Some(Current::Scratch(id)) {
            self.current = None;
            let next = ix
                .and_then(|i| self.scratch.get(i.saturating_sub(1)))
                .map(|s| Current::Scratch(s.id.clone()));
            match next {
                Some(item) => self.show(item, window, cx),
                None => self.new_scratch(window, cx),
            }
        }
        cx.notify();
    }

    fn delete_note(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.unwritten == Some(Current::Note(name.clone())) {
            self.unwritten = None;
            self.autosave = None;
        }
        self.notes.retain(|n| n.name != name);
        self.core
            .send(Command::Notes(NotesCommand::Delete { name: name.clone() }));
        self.confirm = None;
        if self.current == Some(Current::Note(name)) {
            self.current = None;
            match self.scratch.last().map(|s| s.id.clone()) {
                Some(id) => self.show(Current::Scratch(id), window, cx),
                None => self.new_scratch(window, cx),
            }
        }
        cx.notify();
    }

    /// Open the name field for `what`, filled with `name`.
    fn start_naming(
        &mut self,
        what: Naming,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.confirm = None;
        self.naming = Some((what, None));
        self.name_input.update(cx, |i, cx| {
            i.set_value(name, window, cx);
            i.focus(window, cx);
        });
        cx.notify();
    }

    /// Save (Ctrl/Cmd+S): a scratch note asks for a name; a note already saves itself.
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.current.clone() {
            Some(Current::Scratch(id)) => {
                let text = self
                    .text_of(&Current::Scratch(id.clone()))
                    .unwrap_or_default();
                let name = switchyard_core::notes::suggest_name(text);
                self.start_naming(Naming::SaveScratch(id), name, window, cx);
            }
            Some(Current::Note(_)) => self.flush(cx),
            None => {}
        }
    }

    fn confirm_name(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some((what, _)) = self.naming.clone() else {
            return;
        };
        let typed = self.name_input.read(cx).value().to_string();
        let name = match switchyard_core::notes::validate_name(&typed) {
            Ok(n) => n,
            Err(e) => {
                self.naming = Some((what, Some(e)));
                cx.notify();
                return;
            }
        };
        let request = next_id();
        let cmd = match &what {
            Naming::SaveScratch(id) => {
                // The text goes with the command; nothing is left to autosave.
                if self.unwritten == Some(Current::Scratch(id.clone())) {
                    self.unwritten = None;
                    self.autosave = None;
                }
                NotesCommand::SaveAs {
                    request,
                    scratch: Some(id.clone()),
                    name,
                    content: self
                        .text_of(&Current::Scratch(id.clone()))
                        .unwrap_or_default()
                        .to_owned(),
                }
            }
            Naming::NewNote => NotesCommand::SaveAs {
                request,
                scratch: None,
                content: format!("# {name}\n\n"),
                name,
            },
            Naming::Rename(old) => {
                if old == &name {
                    self.naming = None;
                    cx.notify();
                    return;
                }
                self.flush(cx);
                NotesCommand::Rename {
                    request,
                    from: old.clone(),
                    to: name,
                }
            }
        };
        self.pending = Some((request, what));
        self.core.send(Command::Notes(cmd));
    }

    /// Open note `name`, creating it when it does not exist (a `[[link]]`).
    fn open_link(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.notes.iter().any(|n| n.name == name) {
            self.show(Current::Note(name), window, cx);
            return;
        }
        let Ok(name) = switchyard_core::notes::validate_name(&name) else {
            self.error = Some(format!("“{name}” is not a note name"));
            cx.notify();
            return;
        };
        let request = next_id();
        self.pending = Some((request, Naming::NewNote));
        self.core.send(Command::Notes(NotesCommand::SaveAs {
            request,
            scratch: None,
            content: format!("# {name}\n\n"),
            name,
        }));
    }

    fn set_preview(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.preview = on;
        self.focus_editor(window, cx);
        cx.notify();
    }

    fn open_folder(&mut self, cx: &mut Context<Self>) {
        if let Some(dir) = &self.dir {
            cx.open_with_system(dir);
        }
    }
}

/// A new scratch id: the time it was made (so ids sort in the order they were opened),
/// then a counter for two in the same millisecond.
fn scratch_id(existing: &[Scratch]) -> String {
    let ms = now_ms();
    (0u32..)
        .map(|n| format!("{ms:013}-{n}"))
        .find(|id| !existing.iter().any(|s| &s.id == id))
        .unwrap_or_default()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// How a note is listed: its first line, without markdown heading marks.
fn first_line(text: &str) -> Option<&str> {
    text.lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty())
}

/// "just now", "5 min ago", "3 h ago", "2 d ago".
fn ago(ms: i64, now: i64) -> String {
    let s = (now - ms).max(0) / 1000;
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86_400 => format!("{} h ago", s / 3600),
        _ => format!("{} d ago", s / 86_400),
    }
}

/// The `[[links]]` in `text`, in order, each once (`[[Name|label]]` and `[[Name#part]]`
/// link to `Name`).
fn wiki_links(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else {
            break;
        };
        let inner = &after[..end];
        let name = inner.split(['|', '#']).next().unwrap_or_default().trim();
        if !name.is_empty() && !inner.contains('\n') && !out.iter().any(|n| n == name) {
            out.push(name.to_owned());
        }
        rest = &after[end + 2..];
    }
    out
}

/// Whether `query` (lowercase words) matches a note's name and text.
fn matches(query: &[String], name: &str, text: &str) -> bool {
    let (name, text) = (name.to_lowercase(), text.to_lowercase());
    query
        .iter()
        .all(|w| name.contains(w.as_str()) || text.contains(w.as_str()))
}

impl Focusable for NotesTab {
    fn focus_handle(&self, _cx: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl NotesTab {
    fn render_list(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let query: Vec<String> = self
            .search
            .read(cx)
            .value()
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let now = now_ms();
        let row = |id: SharedString, selected: bool| {
            let hover = p.hover;
            div()
                .id(id)
                .w_full()
                .flex()
                .items_center()
                .gap(rpx(6.))
                .px(rpx(8.))
                .py(rpx(5.))
                .rounded(px(5.))
                .cursor_pointer()
                .when(selected, |d| d.bg(p.sel))
                .when(!selected, move |d| d.hover(move |s| s.bg(hover)))
        };
        let scratch: Vec<AnyElement> = self
            .scratch
            .iter()
            .enumerate()
            .filter(|(_, s)| matches(&query, "", &s.content))
            .map(|(i, s)| {
                let selected = self.current.as_ref() == Some(&Current::Scratch(s.id.clone()));
                let title = first_line(&s.content).map_or_else(
                    || "Empty scratch note".to_owned(),
                    |l| l.chars().take(80).collect(),
                );
                let (open_id, close_id) = (s.id.clone(), s.id.clone());
                row(format!("notes-s-{i}").into(), selected)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.show(Current::Scratch(open_id.clone()), window, cx)
                    }))
                    .child(ui::dot(p.stg, 6.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(ts::BODY)
                            .when(s.content.trim().is_empty(), |d| d.text_color(p.fg3))
                            .child(title),
                    )
                    .child(
                        div()
                            .id(("notes-s-close", i))
                            .flex_none()
                            .px(rpx(4.))
                            .rounded(px(4.))
                            .text_color(p.fg3)
                            .hover(|s| s.text_color(p.fg).bg(p.hover))
                            .child("×")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_scratch(close_id.clone(), window, cx)
                            })),
                    )
                    .into_any_element()
            })
            .collect();
        let notes: Vec<AnyElement> = self
            .notes
            .iter()
            .enumerate()
            .filter(|(_, n)| matches(&query, &n.name, &n.content))
            .map(|(i, n)| {
                let selected = self.current.as_ref() == Some(&Current::Note(n.name.clone()));
                let name = n.name.clone();
                row(format!("notes-n-{i}").into(), selected)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.show(Current::Note(name.clone()), window, cx)
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(ts::BODY)
                            .child(n.name.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(ts::CAPTION_PLUS)
                            .text_color(p.fg3)
                            .child(ago(n.modified_ms, now)),
                    )
                    .into_any_element()
            })
            .collect();
        let heading = |label: &str, count: usize| {
            div()
                .px(rpx(8.))
                .pt(rpx(10.))
                .pb(rpx(4.))
                .flex()
                .items_center()
                .gap(rpx(6.))
                .child(ui::caption(label.to_owned(), p))
                .child(
                    div()
                        .font_family(MONO)
                        .text_size(ts::CAPTION)
                        .text_color(p.fg3)
                        .child(count.to_string()),
                )
        };
        let empty_hint = |text: &str| {
            div()
                .px(rpx(8.))
                .py(rpx(4.))
                .text_size(ts::LABEL)
                .text_color(p.fg3)
                .child(text.to_owned())
        };
        let (n_scratch, n_notes) = (scratch.len(), notes.len());
        div()
            .w(rpx(LIST_WIDTH))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .bg(p.panel)
            .border_r_1()
            .border_color(p.bd)
            .child(
                div()
                    .h(rpx(34.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(rpx(4.))
                    .px(rpx(10.))
                    .child(
                        div()
                            .flex_1()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(ts::BASE)
                            .child("Scratchpad"),
                    )
                    .child(
                        ui::button("notes-new-scratch", "+ Scratch", Kind::Ghost, p)
                            .h(rpx(22.))
                            .px(rpx(6.))
                            .text_size(ts::LABEL)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.new_scratch(window, cx)),
                            ),
                    )
                    .child(
                        ui::button("notes-new-note", "+ Note", Kind::Ghost, p)
                            .h(rpx(22.))
                            .px(rpx(6.))
                            .text_size(ts::LABEL)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.start_naming(Naming::NewNote, String::new(), window, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .mx(rpx(8.))
                    .mb(rpx(4.))
                    .px(rpx(6.))
                    .h(rpx(26.))
                    .flex()
                    .items_center()
                    .rounded(px(6.))
                    .bg(p.bg)
                    .border_1()
                    .border_color(p.bd)
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.search)
                                .appearance(false)
                                .text_size(ts::BODY),
                        ),
                    ),
            )
            .child(
                div()
                    .id("notes-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(rpx(6.))
                    .pb(rpx(8.))
                    .child(heading("Scratch · not saved", n_scratch))
                    .children(scratch)
                    .when(n_scratch == 0 && !query.is_empty(), |d| {
                        d.child(empty_hint("No scratch note matches"))
                    })
                    .child(heading("Notes", n_notes))
                    .children(notes)
                    .when(n_notes == 0, |d| {
                        d.child(empty_hint(if query.is_empty() {
                            "Save a scratch note (⌘S / Ctrl+S) to keep it here"
                        } else {
                            "No note matches"
                        }))
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .px(rpx(8.))
                    .py(rpx(6.))
                    .border_t_1()
                    .border_color(p.bd)
                    .flex()
                    .items_center()
                    .gap(rpx(6.))
                    .child(
                        ui::button("notes-folder", "Open folder", Kind::Ghost, p)
                            .h(rpx(22.))
                            .px(rpx(6.))
                            .text_size(ts::LABEL)
                            .on_click(cx.listener(|this, _, _, cx| this.open_folder(cx))),
                    )
                    .child(
                        ui::button("notes-reload", "Reload", Kind::Ghost, p)
                            .h(rpx(22.))
                            .px(rpx(6.))
                            .text_size(ts::LABEL)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.flush(cx);
                                this.core.send(Command::Notes(NotesCommand::Load));
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_toolbar(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let small = |b: gpui_kit::Stateful<gpui_kit::Div>| b.h(rpx(22.)).text_size(ts::LABEL);
        let (title, status): (String, String) = match &self.current {
            Some(Current::Scratch(_)) => (
                "Scratch note".into(),
                "Not saved · kept until you close it".into(),
            ),
            Some(Current::Note(name)) => (
                name.clone(),
                if self.unwritten.is_some() {
                    "Saving…".into()
                } else {
                    "Saved".into()
                },
            ),
            None => ("Scratchpad".into(), String::new()),
        };
        let scratch = matches!(self.current, Some(Current::Scratch(_)));
        let note = match &self.current {
            Some(Current::Note(n)) => Some(n.clone()),
            _ => None,
        };
        let mode = |id: &'static str, label: &'static str, on: bool, cx: &mut Context<Self>| {
            small(ui::button(
                id,
                label,
                if on == self.preview {
                    Kind::Secondary
                } else {
                    Kind::Ghost
                },
                p,
            ))
            .on_click(cx.listener(move |this, _, window, cx| this.set_preview(on, window, cx)))
        };
        div()
            .h(rpx(34.))
            .flex_none()
            .flex()
            .items_center()
            .gap(rpx(8.))
            .px(rpx(12.))
            .border_b_1()
            .border_color(p.bd)
            .bg(p.panel)
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(ts::UI)
                    .child(title),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(ts::LABEL)
                    .text_color(if scratch { p.stg } else { p.fg3 })
                    .child(status),
            )
            .child(mode("notes-edit", "Edit", false, cx))
            .child(mode("notes-preview", "Read", true, cx))
            .child(ui::vdivider(p, 16.))
            .when(scratch, |d| {
                d.child(
                    small(ui::button_with_key(
                        "notes-save",
                        "Save as note",
                        ui::keys("⌘S", "Ctrl+S"),
                        Kind::Primary,
                        p,
                    ))
                    .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                )
                .child(
                    small(ui::button("notes-close", "Close", Kind::Ghost, p)).on_click(
                        cx.listener(|this, _, window, cx| this.close_current(window, cx)),
                    ),
                )
            })
            .when_some(note, |d, name| {
                let (rename, delete) = (name.clone(), name);
                d.child(
                    small(ui::button("notes-rename", "Rename", Kind::Ghost, p)).on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.start_naming(
                                Naming::Rename(rename.clone()),
                                rename.clone(),
                                window,
                                cx,
                            )
                        }),
                    ),
                )
                .child(
                    small(ui::button("notes-delete", "Delete", Kind::Ghost, p)).on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.naming = None;
                            this.confirm = Some(Confirm::Delete(delete.clone()));
                            cx.notify();
                        }),
                    ),
                )
            })
            .into_any_element()
    }

    /// The name field (Save as, New note, Rename) or a question, above the editor.
    fn render_bar(&self, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let bar = || {
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(rpx(8.))
                .px(rpx(12.))
                .py(rpx(6.))
                .border_b_1()
                .border_color(p.bd)
                .text_size(ts::BODY)
        };
        let small = |b: gpui_kit::Stateful<gpui_kit::Div>| b.h(rpx(22.)).text_size(ts::LABEL);
        if let Some((what, err)) = &self.naming {
            let (label, action) = match what {
                Naming::SaveScratch(_) => ("Save as", "Save"),
                Naming::NewNote => ("New note", "Create"),
                Naming::Rename(_) => ("Rename to", "Rename"),
            };
            return Some(
                bar()
                    .bg(p.surface)
                    .child(div().flex_none().text_color(p.fg2).child(label))
                    .child(
                        div()
                            .w(rpx(260.))
                            .h(rpx(26.))
                            .px(rpx(6.))
                            .flex()
                            .items_center()
                            .rounded(px(6.))
                            .bg(p.bg)
                            .border_1()
                            .border_color(if err.is_some() { p.prod } else { p.bd2 })
                            .child(
                                div().flex_1().min_w_0().child(
                                    Input::new(&self.name_input)
                                        .appearance(false)
                                        .text_size(ts::BODY),
                                ),
                            ),
                    )
                    .child(div().text_color(p.fg3).child(".md"))
                    .child(
                        small(ui::button("notes-name-ok", action, Kind::Primary, p)).on_click(
                            cx.listener(|this, _, window, cx| this.confirm_name(window, cx)),
                        ),
                    )
                    .child(
                        small(ui::button("notes-name-cancel", "Cancel", Kind::Ghost, p)).on_click(
                            cx.listener(|this, _, window, cx| {
                                this.naming = None;
                                this.focus_editor(window, cx);
                                cx.notify();
                            }),
                        ),
                    )
                    .when_some(err.clone(), |d, e| {
                        d.child(div().min_w_0().truncate().text_color(p.prod).child(e))
                    })
                    .into_any_element(),
            );
        }
        let (text, button, confirm) = match self.confirm.clone()? {
            Confirm::Discard(id) => (
                "Close this scratch note? Its text is not saved anywhere else.".to_owned(),
                "Discard",
                Confirm::Discard(id),
            ),
            Confirm::Delete(name) => (
                format!("Delete “{name}”? Its file is removed from the notes folder."),
                "Delete",
                Confirm::Delete(name),
            ),
        };
        Some(
            bar()
                .bg(p.prod_bg)
                .child(div().flex_1().min_w_0().child(text))
                .child(
                    small(ui::button("notes-confirm-ok", button, Kind::Destructive, p)).on_click(
                        cx.listener(move |this, _, window, cx| match confirm.clone() {
                            Confirm::Discard(id) => this.discard(id, window, cx),
                            Confirm::Delete(name) => this.delete_note(name, window, cx),
                        }),
                    ),
                )
                .child(
                    small(ui::button("notes-confirm-keep", "Keep", Kind::Secondary, p)).on_click(
                        cx.listener(|this, _, window, cx| {
                            this.confirm = None;
                            this.focus_editor(window, cx);
                            cx.notify();
                        }),
                    ),
                )
                .into_any_element(),
        )
    }

    fn render_preview(&self, text: &str, p: &Palette) -> AnyElement {
        let order = std::cell::Cell::new(0u64);
        let next = || {
            order.set(order.get() + 1);
            order.get()
        };
        let blocks = crate::chat_markdown::parse(text);
        div()
            .id("notes-preview-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(rpx(28.))
            .py(rpx(18.))
            .child(
                div()
                    .max_w(rpx(760.))
                    .flex()
                    .flex_col()
                    .gap(rpx(8.))
                    .when(blocks.is_empty(), |d| {
                        d.child(
                            div()
                                .text_color(p.fg3)
                                .text_size(ts::BODY)
                                .child("Nothing here yet"),
                        )
                    })
                    .children(blocks.into_iter().enumerate().map(|(i, b)| {
                        div()
                            .flex_none()
                            .child(crate::assistant_panel::markdown_block(
                                b,
                                &format!("notes-md-{i}"),
                                &next,
                                p,
                            ))
                    })),
            )
            .into_any_element()
    }

    /// The note's `[[links]]`, under the editor.
    fn render_links(&self, text: &str, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let links = wiki_links(text);
        if links.is_empty() {
            return None;
        }
        Some(
            div()
                .flex_none()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(rpx(6.))
                .px(rpx(12.))
                .py(rpx(6.))
                .border_t_1()
                .border_color(p.bd)
                .bg(p.panel)
                .child(ui::caption("Links", p))
                .children(links.into_iter().enumerate().map(|(i, name)| {
                    let exists = self.notes.iter().any(|n| n.name == name);
                    let target = name.clone();
                    let hover = p.hover;
                    div()
                        .id(("notes-link", i))
                        .px(rpx(8.))
                        .py(rpx(2.))
                        .rounded(px(10.))
                        .border_1()
                        .border_color(if exists { p.bd2 } else { p.bd })
                        .text_size(ts::LABEL)
                        .text_color(if exists { p.acc } else { p.fg3 })
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover))
                        .child(if exists { name } else { format!("{name} +") })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_link(target.clone(), window, cx)
                        }))
                }))
                .into_any_element(),
        )
    }
}

impl Render for NotesTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let text = self
            .current
            .as_ref()
            .and_then(|c| self.text_of(c))
            .unwrap_or_default()
            .to_owned();
        let list = self.render_list(&p, cx);
        let toolbar = self.render_toolbar(&p, cx);
        let bar = self.render_bar(&p, cx);
        let links = self.render_links(&text, &p, cx);
        let body = if !self.loaded {
            div()
                .flex_1()
                .p(rpx(16.))
                .text_color(p.fg3)
                .text_size(ts::BODY)
                .child("Loading notes…")
                .into_any_element()
        } else if self.preview {
            self.render_preview(&text, &p)
        } else {
            div()
                .flex_1()
                .min_h_0()
                .px(rpx(8.))
                .pt(rpx(6.))
                .child(
                    Editor::new(&self.editor)
                        .bordered(false)
                        .appearance(false)
                        .h(relative(1.))
                        .font_family(crate::appearance::editor_font_family(cx))
                        .text_size(crate::appearance::editor_font_size(cx)),
                )
                .into_any_element()
        };
        let error = self.error.clone().map(|e| {
            div()
                .id("notes-error")
                .flex_none()
                .flex()
                .items_center()
                .gap(rpx(8.))
                .px(rpx(12.))
                .py(rpx(5.))
                .bg(p.prod_bg)
                .text_size(ts::BODY)
                .child(div().flex_1().min_w_0().child(e))
                .child(
                    ui::button("notes-error-ok", "Dismiss", Kind::Ghost, &p)
                        .h(rpx(20.))
                        .text_size(ts::LABEL)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.error = None;
                            cx.notify();
                        })),
                )
        });
        div()
            .id("notes-tab")
            .key_context("NotesTab")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &SaveNote, window, cx| this.save(window, cx)))
            .on_action(cx.listener(|this, _: &CloseNote, window, cx| {
                // A saved note has nothing to close: the shortcut closes the tab.
                if matches!(this.current, Some(Current::Scratch(_))) {
                    this.close_current(window, cx)
                } else {
                    window.dispatch_action(Box::new(crate::actions::CloseTab), cx)
                }
            }))
            .on_action(cx.listener(|this, _: &TogglePreview, window, cx| {
                let on = !this.preview;
                this.set_preview(on, window, cx)
            }))
            .size_full()
            .flex()
            .bg(p.surface)
            .child(list)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(toolbar)
                    .children(bar)
                    .children(error)
                    .child(body)
                    .children(links),
            )
    }
}

impl crate::workspace::Workspace {
    /// The scratchpad shortcut: show the scratchpad with the cursor in its note, or, when
    /// it is already in front, start a new scratch note.
    pub(crate) fn open_notes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::workspace::{AppMode, Tab};
        if self.mode != AppMode::Default {
            self.set_mode(AppMode::Default, window, cx);
        }
        let ix = self.tabs.iter().position(|t| matches!(t, Tab::Notes(_)));
        let tab = match ix {
            Some(ix) => {
                let Some(Tab::Notes(tab)) = self.tabs.get(ix) else {
                    return;
                };
                let tab = tab.clone();
                if self.active == ix {
                    tab.update(cx, |t, cx| t.new_scratch(window, cx));
                } else {
                    self.activate(ix, cx);
                }
                tab
            }
            None => {
                let core = self.core.clone();
                let tab = cx.new(|cx| NotesTab::new(core, window, cx));
                self.tabs.push(Tab::Notes(tab.clone()));
                self.activate(self.tabs.len() - 1, cx);
                tab
            }
        };
        tab.update(cx, |t, cx| t.focus_editor(window, cx));
        cx.notify();
    }

    /// Reopen the scratchpad from a saved layout, without taking the front.
    pub(crate) fn reopen_notes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::workspace::Tab;
        if !self.tabs.iter().any(|t| matches!(t, Tab::Notes(_))) {
            let core = self.core.clone();
            let tab = cx.new(|cx| NotesTab::new(core, window, cx));
            self.tabs.push(Tab::Notes(tab));
        }
    }

    /// Route a notes answer to the scratchpad.
    pub(crate) fn on_notes_event(
        &mut self,
        ev: NotesEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for t in &self.tabs {
            if let crate::workspace::Tab::Notes(n) = t {
                n.update(cx, |n, cx| n.on_event(ev.clone(), window, cx));
            }
        }
    }

    /// Write the scratchpad's pending text now (the window is closing).
    pub(crate) fn flush_notes(&mut self, cx: &mut Context<Self>) {
        for t in &self.tabs {
            if let crate::workspace::Tab::Notes(n) = t {
                n.update(cx, |n, cx| n.flush(cx));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_in_order_once() {
        let text =
            "See [[Deploy]] and [[Runbook|the runbook]], [[Deploy#steps]], [[ ]] [[a\nb]] [[open";
        assert_eq!(wiki_links(text), ["Deploy", "Runbook"]);
    }

    #[test]
    fn search_matches_every_word() {
        let q = |s: &str| {
            s.split_whitespace()
                .map(str::to_lowercase)
                .collect::<Vec<_>>()
        };
        assert!(matches(&q("deploy PROD"), "Deploy", "push to prod"));
        assert!(!matches(&q("deploy staging"), "Deploy", "push to prod"));
        assert!(matches(&q(""), "", ""));
    }

    #[test]
    fn scratch_ids_sort_by_time_and_stay_unique() {
        let a = scratch_id(&[]);
        let b = scratch_id(&[Scratch {
            id: a.clone(),
            content: String::new(),
        }]);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_digit() || c == '-'));
        assert_eq!(a.split('-').next().map(str::len), Some(13));
    }

    #[test]
    fn list_titles_and_times() {
        assert_eq!(first_line("\n  ## Todo \nmore"), Some("Todo"));
        assert_eq!(first_line("  \n"), None);
        assert_eq!(ago(0, 30_000), "just now");
        assert_eq!(ago(0, 5 * 60_000), "5 min ago");
        assert_eq!(ago(0, 3 * 3_600_000), "3 h ago");
        assert_eq!(ago(0, 2 * 86_400_000), "2 d ago");
    }
}
