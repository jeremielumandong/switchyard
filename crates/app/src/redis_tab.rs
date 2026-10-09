//! Redis key browser, laid out like Redis Insight: a key list on the left filtered by
//! pattern and type (a tree of folders split on the connection's delimiter, `:` by
//! default, or a flat list, with type, TTL and size; drag its edge to widen it), the selected key's value on the right as a table (hash, list, set,
//! sorted set, stream) or text (string, JSON) with in-place edits and a full-value viewer
//! for the selected row, and a `redis-cli`-style console underneath (Up / Down recall
//! earlier commands, this connection's history first). Values and console
//! output sit in read-only editors, so any of it can be selected and copied.
//!
//! The tab owns one Redis session ([`Command::RedisOpen`]); every read and write goes
//! through the core on the runtime. Read-only connections hide the edit controls (the
//! core refuses writes too); destructive console commands on Production ask first.

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, px, relative, uniform_list,
};
use switchyard_core::bus::{RedisInfo, RedisOutcome};
use switchyard_core::db::redis::command::{escape, quote, redacted, split};
use switchyard_core::db::redis::{
    KeyDetails, KeyEdit, KeyEntry, KeyKind, KeyValue, ScanPage, TREE_DELIMITER_OPTION,
    tree_delimiter,
};
use switchyard_core::store::{DbConnection, HistoryEntry};
use switchyard_core::{Command, RequestId, RuntimeHandle, SessionId};

use crate::app_state::next_id;
use crate::appearance::{rpx, ts};
use crate::theme::{MONO, Palette, SANS, palette};
use crate::ui::{self, Kind};
use crate::workspace::{Tab, Workspace};

/// Row height of the key list and value tables.
const ROW_H: f32 = 26.;
/// Keys to gather before the list stops scanning on its own; "Load more" adds a page.
const FIRST_KEYS: usize = 200;
const MORE_KEYS: usize = 500;
/// Console lines kept, and characters kept per reply.
const CONSOLE_LINES: usize = 200;
const REPLY_CHARS: usize = 20_000;
/// Characters of a value shown in a table cell.
const CELL_CHARS: usize = 300;
/// Key list width: default and narrowest; the value pane keeps at least `VALUE_MIN`.
const KEYS_W: f32 = 360.;
const KEYS_MIN: f32 = 220.;
const VALUE_MIN: f32 = 320.;
/// Console commands kept for Up / Down.
const HISTORY_LINES: usize = 500;
/// Height of the selected row's full-value viewer.
const DETAIL_H: f32 = 190.;

/// How the key list shows keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyView {
    /// Folders split on the connection's delimiter, like Redis Insight's tree view.
    Tree,
    /// One row per key.
    List,
}

/// One row of the key list.
#[derive(Clone, Debug, PartialEq)]
enum KeyRow {
    /// A folder of keys sharing `prefix` (which ends with the delimiter).
    Folder {
        prefix: Vec<u8>,
        name: String,
        depth: usize,
        /// Keys under it, at any depth.
        keys: usize,
        open: bool,
    },
    /// A key: its index in the key list, and the name shown (the part after its folder).
    Key {
        ix: usize,
        name: String,
        depth: usize,
    },
}

/// What the value pane is doing besides showing the key.
#[derive(Clone, Debug, PartialEq)]
enum Mode {
    View,
    Rename,
    Ttl,
    ConfirmDelete,
    NewKey(KeyKind),
}

/// Console lines for Up / Down, oldest first, like a shell's history.
#[derive(Debug, Default)]
struct ConsoleHistory {
    lines: Vec<String>,
    /// The line being shown while browsing; `None` when editing a new line.
    pos: Option<usize>,
    /// What was typed before browsing started (Down past the newest brings it back).
    draft: String,
}

impl ConsoleHistory {
    /// Add older lines (the stored history, newest first) ahead of this session's.
    fn load_older(&mut self, newest_first: impl IntoIterator<Item = String>) {
        let mut older: Vec<String> = newest_first.into_iter().collect();
        older.reverse();
        older.dedup();
        if older.last().is_some_and(|l| self.lines.first() == Some(l)) {
            older.pop();
        }
        older.append(&mut self.lines);
        let extra = older.len().saturating_sub(HISTORY_LINES);
        older.drain(..extra);
        self.lines = older;
        self.pos = None;
    }

    /// Remember a line that ran (a repeat of the last one is kept once).
    fn push(&mut self, line: &str) {
        self.pos = None;
        self.draft.clear();
        if self.lines.last().is_some_and(|l| l == line) {
            return;
        }
        self.lines.push(line.to_owned());
        if self.lines.len() > HISTORY_LINES {
            self.lines.remove(0);
        }
    }

    /// Up: the previous line, keeping `current` as the draft when browsing starts.
    fn prev(&mut self, current: &str) -> Option<&str> {
        let pos = match self.pos {
            None if self.lines.is_empty() => return None,
            None => {
                self.draft = current.to_owned();
                self.lines.len() - 1
            }
            Some(p) => p.saturating_sub(1),
        };
        self.pos = Some(pos);
        self.lines.get(pos).map(String::as_str)
    }

    /// Down: the next line, or the draft after the newest.
    fn next(&mut self) -> Option<&str> {
        let p = self.pos?;
        if p + 1 < self.lines.len() {
            self.pos = Some(p + 1);
            self.lines.get(p + 1).map(String::as_str)
        } else {
            self.pos = None;
            Some(self.draft.as_str())
        }
    }
}

/// Whether a console line may be recalled: not one carrying a secret (AUTH, HELLO …
/// AUTH, CONFIG SET requirepass…), which history stores masked.
fn recallable(line: &str) -> bool {
    split(line).is_ok_and(|args| !args.is_empty() && !redacted(&args).ends_with("***"))
}

/// The console lines of stored history entries (newest first): this connection's
/// own commands, not a coding agent's.
fn history_lines(entries: &[HistoryEntry]) -> Vec<String> {
    entries
        .iter()
        .filter(|e| !e.tags.iter().any(|t| t.starts_with("agent")))
        .map(|e| e.sql.trim().to_owned())
        .filter(|l| recallable(l))
        .collect()
}

/// One console exchange.
struct ConsoleLine {
    line: String,
    text: String,
    error: bool,
    ms: Option<u64>,
}

/// A Redis key browser tab.
pub struct RedisTab {
    core: RuntimeHandle,
    /// The connection.
    pub connection: DbConnection,
    session: SessionId,
    info: Option<RedisInfo>,
    open_error: Option<String>,

    pattern: Entity<InputState>,
    /// The pattern the shown keys were scanned with.
    scanned: String,
    /// The type filter picked, and the one the shown keys were scanned with.
    kind_filter: Option<KeyKind>,
    scanned_kind: Option<KeyKind>,
    /// The tree view's folder separator (`DbConfig::options`, `:` by default).
    delimiter: Vec<u8>,
    /// Shared with the virtualized key list, which renders without copying it.
    keys: Arc<Vec<KeyEntry>>,
    cursor: u64,
    scan_request: Option<RequestId>,
    /// Keep scanning until this many keys are listed (or the scan ends).
    want: usize,
    list_error: Option<String>,
    view: KeyView,
    /// Open folders of the tree view (their prefixes).
    expanded: HashSet<Vec<u8>>,
    /// The key list's rows, rebuilt when the keys, the view or a folder changes.
    rows: Arc<Vec<KeyRow>>,
    keys_width: f32,
    /// Dragging the key list's edge: (mouse x, width) when it started.
    keys_drag: Option<(f32, f32)>,
    /// The tab's width at the last frame (bounds the key list's width).
    tab_w: Rc<Cell<f32>>,

    selected: Option<Vec<u8>>,
    load_request: Option<RequestId>,
    details: Option<Arc<KeyDetails>>,
    value_error: Option<String>,
    text_editor: Entity<EditorState>,
    /// Selected row of a collection value.
    row: Option<usize>,
    /// The selected row's full value (read-only, selectable).
    row_view: Entity<EditorState>,
    /// Field / member / value inputs of the edit bar.
    in_a: Entity<InputState>,
    in_b: Entity<InputState>,
    /// Key name (new key, rename) or TTL seconds.
    in_key: Entity<InputState>,
    mode: Mode,
    edit_request: Option<RequestId>,
    status: Option<(bool, String)>,

    console_open: bool,
    console_input: Entity<InputState>,
    console: Vec<ConsoleLine>,
    /// The console transcript (read-only, selectable).
    console_view: Entity<EditorState>,
    /// Up / Down recall.
    history: ConsoleHistory,
    /// The stored-history search filling [`Self::history`].
    history_request: Option<RequestId>,
    run_request: Option<RequestId>,
    /// A destructive line waiting for "Run anyway": (line, reason).
    confirm: Option<(String, String)>,
    _subs: Vec<Subscription>,
}

impl RedisTab {
    /// A tab on `connection`; connects right away.
    pub fn new(
        core: RuntimeHandle,
        connection: DbConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = |ph: &str, window: &mut Window, cx: &mut Context<Self>| {
            let ph = ph.to_owned();
            cx.new(|cx| InputState::new(window, cx).placeholder(ph))
        };
        let pattern = input("Filter keys: user:* or *session*", window, cx);
        let in_a = input("Value", window, cx);
        let in_b = input("Value", window, cx);
        let in_key = input("Key name", window, cx);
        let console_input = input("Command, e.g. GET user:1 or INFO memory", window, cx);
        let viewer = |window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                EditorState::new(window, cx)
                    .language("text")
                    .line_number(false)
                    .indent_guides(false)
                    .soft_wrap(true)
            })
        };
        let text_editor = viewer(window, cx);
        let row_view = viewer(window, cx);
        let console_view = viewer(window, cx);
        let subs = vec![
            cx.subscribe_in(&pattern, window, |this, _, ev: &InputEvent, _, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.rescan(cx);
                }
            }),
            cx.subscribe_in(&console_input, window, |this, _, ev: &InputEvent, w, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.run_console(w, cx);
                }
            }),
            cx.subscribe_in(&in_key, window, |this, _, ev: &InputEvent, w, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.apply_mode(w, cx);
                }
            }),
        ];
        let session = next_id();
        core.send(Command::RedisOpen {
            session,
            connection: connection.id.clone(),
        });
        // Earlier console commands of this connection, for Up / Down.
        let history_request = connection.history_enabled.then(|| {
            let request = next_id();
            core.send(Command::SearchHistory {
                request,
                query: String::new(),
                connection: Some(connection.id.clone()),
            });
            request
        });
        let delimiter = tree_delimiter(connection.option(TREE_DELIMITER_OPTION));
        Self {
            core,
            connection,
            session,
            info: None,
            open_error: None,
            pattern,
            scanned: String::new(),
            kind_filter: None,
            scanned_kind: None,
            delimiter,
            keys: Arc::default(),
            cursor: 0,
            scan_request: None,
            want: FIRST_KEYS,
            list_error: None,
            view: KeyView::Tree,
            expanded: HashSet::new(),
            rows: Arc::default(),
            keys_width: KEYS_W,
            keys_drag: None,
            tab_w: Rc::new(Cell::new(0.)),
            selected: None,
            load_request: None,
            details: None,
            value_error: None,
            text_editor,
            row: None,
            row_view,
            in_a,
            in_b,
            in_key,
            mode: Mode::View,
            edit_request: None,
            status: None,
            console_open: false,
            console_input,
            console: Vec::new(),
            console_view,
            history: ConsoleHistory::default(),
            history_request,
            run_request: None,
            confirm: None,
            _subs: subs,
        }
    }

    /// Whether this tab owns `session`.
    pub fn owns(&self, session: SessionId) -> bool {
        self.session == session
    }

    /// Connected (for the sidebar's live dot).
    pub fn is_open(&self) -> bool {
        self.info.is_some()
    }

    /// Close the session (the tab is closing).
    pub fn shutdown(&mut self) {
        self.core.send(Command::CloseSession {
            session: self.session,
        });
    }

    fn read_only(&self) -> bool {
        self.connection.read_only || self.info.as_ref().is_some_and(|i| i.read_only)
    }

    fn value(input: &Entity<InputState>, cx: &Context<Self>) -> String {
        input.read(cx).value().to_string()
    }

    fn set(input: &Entity<InputState>, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = text.to_owned();
        input.update(cx, |i, cx| i.set_value(text, window, cx));
    }

    /// Label the edit inputs for `kind` (Field / Value, Member / Score…).
    fn sync_placeholders(&self, kind: &KeyKind, window: &mut Window, cx: &mut Context<Self>) {
        let (a, b) = new_key_inputs(kind);
        self.in_a.update(cx, |i, cx| {
            i.set_placeholder(a.unwrap_or("Value"), window, cx)
        });
        self.in_b.update(cx, |i, cx| {
            i.set_placeholder(b.unwrap_or("Value"), window, cx)
        });
    }

    // ------------------------------------------------------------- core events

    /// [`switchyard_core::Event::RedisOpened`].
    pub fn on_opened(&mut self, result: Result<RedisInfo, String>, cx: &mut Context<Self>) {
        match result {
            Ok(info) => {
                self.info = Some(info);
                self.open_error = None;
                self.rescan(cx);
            }
            Err(e) => {
                // A stopped tunnel ends the session: nothing it had is live any more.
                self.info = None;
                self.scan_request = None;
                self.load_request = None;
                self.edit_request = None;
                self.run_request = None;
                self.open_error = Some(e);
            }
        }
        cx.notify();
    }

    fn rescan(&mut self, cx: &mut Context<Self>) {
        if self.info.is_none() {
            return;
        }
        self.scanned = Self::value(&self.pattern, cx).trim().to_owned();
        self.scanned_kind = self.kind_filter.clone();
        self.keys = Arc::default();
        self.rows = Arc::default();
        self.cursor = 0;
        self.want = FIRST_KEYS;
        self.list_error = None;
        self.scan_next();
        cx.notify();
    }

    fn scan_next(&mut self) {
        let request = next_id();
        self.scan_request = Some(request);
        self.core.send(Command::RedisScan {
            session: self.session,
            request,
            pattern: self.scanned.clone(),
            kind: self.scanned_kind.clone(),
            cursor: self.cursor,
        });
    }

    /// Show only keys of `kind` (`None`: every type) and scan again.
    fn set_kind_filter(&mut self, kind: Option<KeyKind>, cx: &mut Context<Self>) {
        self.kind_filter = kind;
        self.rescan(cx);
        cx.notify();
    }

    /// Whether this tab asked for the stored-history search `request`.
    pub fn owns_history(&self, request: RequestId) -> bool {
        self.history_request == Some(request)
    }

    /// [`switchyard_core::Event::History`]: this connection's stored commands.
    pub fn on_history(&mut self, entries: &[HistoryEntry]) {
        self.history_request = None;
        self.history.load_older(history_lines(entries));
    }

    /// Up / Down in the console input.
    fn recall(&mut self, up: bool, window: &mut Window, cx: &mut Context<Self>) {
        let current = Self::value(&self.console_input, cx);
        let line = if up {
            self.history.prev(&current)
        } else {
            self.history.next()
        };
        if let Some(line) = line.map(str::to_owned) {
            Self::set(&self.console_input, &line, window, cx);
        }
    }

    fn load_more(&mut self, cx: &mut Context<Self>) {
        if self.scan_request.is_none() && self.cursor != 0 {
            self.want = self.keys.len() + MORE_KEYS;
            self.scan_next();
            cx.notify();
        }
    }

    /// [`switchyard_core::Event::RedisKeys`].
    pub fn on_keys(
        &mut self,
        request: RequestId,
        result: Result<ScanPage, String>,
        cx: &mut Context<Self>,
    ) {
        if self.scan_request != Some(request) {
            return;
        }
        self.scan_request = None;
        match result {
            Ok(page) => {
                self.cursor = page.cursor;
                // SCAN can repeat a key across pages. The stable sort keeps a known key
                // ahead of its repeat, which dedup then drops.
                let keys = Arc::make_mut(&mut self.keys);
                keys.extend(page.keys);
                keys.sort_by(|a, b| a.key.cmp(&b.key));
                keys.dedup_by(|later, first| later.key == first.key);
                self.rebuild_rows();
                if self.cursor != 0 && self.keys.len() < self.want {
                    self.scan_next();
                }
            }
            Err(e) => self.list_error = Some(e),
        }
        cx.notify();
    }

    fn select(&mut self, key: Vec<u8>, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(key);
        self.mode = Mode::View;
        self.status = None;
        self.reload(window, cx);
    }

    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.selected.clone() else {
            return;
        };
        let request = next_id();
        self.load_request = Some(request);
        self.row = None;
        Self::set(&self.in_a, "", window, cx);
        Self::set(&self.in_b, "", window, cx);
        self.core.send(Command::RedisLoad {
            session: self.session,
            request,
            key,
        });
        cx.notify();
    }

    /// [`switchyard_core::Event::RedisKey`].
    pub fn on_key(
        &mut self,
        request: RequestId,
        result: Result<Arc<KeyDetails>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.load_request != Some(request) {
            return;
        }
        self.load_request = None;
        match result {
            Ok(d) => {
                self.value_error = None;
                let (text, lang) = match &d.value {
                    KeyValue::String(b) => match std::str::from_utf8(b) {
                        Ok(s) => (
                            s.to_owned(),
                            if looks_like_json(s) { "json" } else { "text" },
                        ),
                        Err(_) => (escape(b), "text"),
                    },
                    KeyValue::Json(s) => (pretty_json(s), "json"),
                    _ => (String::new(), "text"),
                };
                self.text_editor.update(cx, |e, cx| {
                    e.set_highlighter(lang, cx);
                    e.set_value(text, window, cx);
                });
                if d.kind == KeyKind::Missing {
                    Arc::make_mut(&mut self.keys).retain(|k| k.key != d.key);
                    self.rebuild_rows();
                }
                self.sync_placeholders(&d.kind, window, cx);
                self.details = Some(d);
            }
            Err(e) => {
                self.details = None;
                self.value_error = Some(e);
            }
        }
        cx.notify();
    }

    fn send_edit(&mut self, key: Vec<u8>, edit: KeyEdit, create: bool, cx: &mut Context<Self>) {
        if self.read_only() || self.edit_request.is_some() {
            return;
        }
        let request = next_id();
        self.edit_request = Some(request);
        self.status = None;
        self.core.send(Command::RedisEdit {
            session: self.session,
            request,
            key,
            edit,
            create,
        });
        cx.notify();
    }

    /// [`switchyard_core::Event::RedisEdited`].
    pub fn on_edited(
        &mut self,
        request: RequestId,
        key: Vec<u8>,
        result: Result<String, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.edit_request != Some(request) {
            return;
        }
        self.edit_request = None;
        match result {
            Ok(m) => {
                self.status = Some((true, m));
                let new_kind = match &self.mode {
                    Mode::NewKey(k) => Some(k.clone()),
                    _ => None,
                };
                let was_new = new_kind.is_some();
                let renamed = self.mode == Mode::Rename;
                let deleted = self.mode == Mode::ConfirmDelete;
                self.mode = Mode::View;
                if deleted {
                    Arc::make_mut(&mut self.keys).retain(|k| k.key != key);
                    self.selected = None;
                    self.details = None;
                } else {
                    if renamed && let Some(old) = &self.selected {
                        let old = old.clone();
                        Arc::make_mut(&mut self.keys).retain(|k| k.key != old);
                    }
                    if (was_new || renamed) && !self.keys.iter().any(|k| k.key == key) {
                        let kind = self
                            .details
                            .as_ref()
                            .filter(|_| renamed)
                            .map(|d| d.kind.clone())
                            .or(new_kind)
                            .unwrap_or(KeyKind::String);
                        Arc::make_mut(&mut self.keys).push(KeyEntry {
                            key: key.clone(),
                            kind,
                            ttl_ms: None,
                            memory: None,
                        });
                        Arc::make_mut(&mut self.keys).sort_by(|a, b| a.key.cmp(&b.key));
                    }
                    self.selected = Some(key);
                    self.reload(window, cx);
                }
            }
            Err(e) => self.status = Some((false, e)),
        }
        self.rebuild_rows();
        cx.notify();
    }

    // ---------------------------------------------------------------- key list

    /// The key list's width, leaving the value pane at least [`VALUE_MIN`].
    fn keys_w(&self) -> f32 {
        let tab = self.tab_w.get();
        if tab <= 0. {
            return self.keys_width;
        }
        self.keys_width.min(tab - VALUE_MIN).max(KEYS_MIN)
    }

    fn rebuild_rows(&mut self) {
        self.rows = Arc::new(match self.view {
            KeyView::Tree => tree_rows(&self.keys, &self.delimiter, &self.expanded),
            KeyView::List => list_rows(&self.keys),
        });
    }

    fn set_view(&mut self, view: KeyView, cx: &mut Context<Self>) {
        self.view = view;
        self.rebuild_rows();
        cx.notify();
    }

    fn toggle_folder(&mut self, prefix: Vec<u8>, cx: &mut Context<Self>) {
        if !self.expanded.remove(&prefix) {
            self.expanded.insert(prefix);
        }
        self.rebuild_rows();
        cx.notify();
    }

    /// Open every folder of the listed keys, or close them all.
    fn expand_all(&mut self, open: bool, cx: &mut Context<Self>) {
        if open {
            self.expanded = folder_prefixes(&self.keys, &self.delimiter);
        } else {
            self.expanded.clear();
        }
        self.rebuild_rows();
        cx.notify();
    }

    // ----------------------------------------------------------- value editing

    fn select_row(&mut self, r: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.details.clone() else {
            return;
        };
        self.row = Some(r);
        let (a, b) = match &d.value {
            KeyValue::Hash(v) => v.get(r).map(|(f, v)| (text(f), text(v))),
            KeyValue::List(v) | KeyValue::Set(v) => v.get(r).map(|m| (text(m), String::new())),
            KeyValue::ZSet(v) => v.get(r).map(|(m, s)| (text(m), s.to_string())),
            KeyValue::Stream(v) => v.get(r).map(|e| (fields_line(&e.fields), String::new())),
            _ => None,
        }
        .unwrap_or_default();
        Self::set(&self.in_a, &a, window, cx);
        Self::set(&self.in_b, &b, window, cx);
        let full = row_value(&d.value, r).unwrap_or_default();
        let lang = if looks_like_json(&full) {
            "json"
        } else {
            "text"
        };
        let full = if lang == "json" {
            pretty_json(&full)
        } else {
            full
        };
        self.row_view.update(cx, |e, cx| {
            e.set_highlighter(lang, cx);
            e.set_value(full, window, cx);
        });
        cx.notify();
    }

    /// An edit-bar button.
    fn edit_action(&mut self, action: EditAction, cx: &mut Context<Self>) {
        let (Some(d), Some(key)) = (self.details.clone(), self.selected.clone()) else {
            return;
        };
        let a = Self::value(&self.in_a, cx).into_bytes();
        let b = Self::value(&self.in_b, cx);
        let row = self.row;
        let edit = match (action, &d.value) {
            (EditAction::Save, KeyValue::String(_)) => Some(KeyEdit::SetString(
                self.text_editor.read(cx).value().to_string().into_bytes(),
            )),
            (EditAction::Save, KeyValue::Hash(_)) => Some(KeyEdit::HashSet(a, b.into_bytes())),
            (EditAction::Remove, KeyValue::Hash(v)) => row
                .and_then(|r| v.get(r))
                .map(|(f, _)| KeyEdit::HashDelete(f.clone())),
            (EditAction::Save, KeyValue::List(_)) => {
                row.map(|r| KeyEdit::ListSet(r as i64, a.clone()))
            }
            (EditAction::PushHead, KeyValue::List(_)) => Some(KeyEdit::ListPush {
                value: a,
                head: true,
            }),
            (EditAction::PushTail, KeyValue::List(_)) => Some(KeyEdit::ListPush {
                value: a,
                head: false,
            }),
            (EditAction::Remove, KeyValue::List(v)) => row
                .and_then(|r| v.get(r))
                .map(|m| KeyEdit::ListRemove(m.clone())),
            (EditAction::Save, KeyValue::Set(_)) => Some(KeyEdit::SetAdd(a)),
            (EditAction::Remove, KeyValue::Set(v)) => row
                .and_then(|r| v.get(r))
                .map(|m| KeyEdit::SetRemove(m.clone())),
            (EditAction::Save, KeyValue::ZSet(_)) => match parse_score(&b) {
                Ok(s) => Some(KeyEdit::ZAdd(a, s)),
                Err(e) => {
                    self.status = Some((false, e));
                    None
                }
            },
            (EditAction::Remove, KeyValue::ZSet(v)) => row
                .and_then(|r| v.get(r))
                .map(|(m, _)| KeyEdit::ZRemove(m.clone())),
            (EditAction::Save, KeyValue::Stream(_)) => {
                match stream_fields(&String::from_utf8_lossy(&a)) {
                    Ok(f) => Some(KeyEdit::StreamAdd(f)),
                    Err(e) => {
                        self.status = Some((false, e));
                        None
                    }
                }
            }
            (EditAction::Remove, KeyValue::Stream(v)) => row
                .and_then(|r| v.get(r))
                .map(|e| KeyEdit::StreamDelete(e.id.clone())),
            _ => None,
        };
        match edit {
            Some(e) => self.send_edit(key, e, false, cx),
            None => cx.notify(),
        }
    }

    fn start_mode(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        let seed = match &mode {
            Mode::Rename => self.selected.as_deref().map(text).unwrap_or_default(),
            Mode::Ttl => self
                .details
                .as_ref()
                .and_then(|d| d.ttl_ms)
                .map(|t| (t / 1000).to_string())
                .unwrap_or_default(),
            _ => String::new(),
        };
        if let Mode::NewKey(kind) = &mode {
            self.sync_placeholders(kind, window, cx);
            self.row = None;
            Self::set(&self.in_a, "", window, cx);
            Self::set(&self.in_b, "", window, cx);
            self.text_editor
                .update(cx, |e, cx| e.set_value(String::new(), window, cx));
        }
        Self::set(&self.in_key, &seed, window, cx);
        self.in_key.update(cx, |i, cx| i.focus(window, cx));
        self.mode = mode;
        self.status = None;
        cx.notify();
    }

    /// Apply Rename, TTL, Delete or New key.
    fn apply_mode(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let input = Self::value(&self.in_key, cx);
        match self.mode.clone() {
            Mode::View => {}
            Mode::Rename => {
                if let Some(key) = self.selected.clone()
                    && !input.is_empty()
                    && input.as_bytes() != key
                {
                    self.send_edit(key, KeyEdit::Rename(input.into_bytes()), false, cx);
                }
            }
            Mode::Ttl => match parse_ttl(&input) {
                Ok(ttl) => {
                    if let Some(key) = self.selected.clone() {
                        self.send_edit(key, KeyEdit::Expire(ttl), false, cx);
                    }
                }
                Err(e) => {
                    self.status = Some((false, e));
                    cx.notify();
                }
            },
            Mode::ConfirmDelete => {
                if let Some(key) = self.selected.clone() {
                    self.send_edit(key, KeyEdit::Delete, false, cx);
                }
            }
            Mode::NewKey(kind) => {
                if input.is_empty() {
                    self.status = Some((false, "Name the key first".into()));
                    return cx.notify();
                }
                let a = Self::value(&self.in_a, cx);
                let b = Self::value(&self.in_b, cx);
                let first = match kind {
                    KeyKind::String => Ok(KeyEdit::SetString(
                        self.text_editor.read(cx).value().to_string().into_bytes(),
                    )),
                    KeyKind::Hash => Ok(KeyEdit::HashSet(a.into_bytes(), b.into_bytes())),
                    KeyKind::List => Ok(KeyEdit::ListPush {
                        value: a.into_bytes(),
                        head: false,
                    }),
                    KeyKind::Set => Ok(KeyEdit::SetAdd(a.into_bytes())),
                    KeyKind::ZSet => parse_score(&b).map(|s| KeyEdit::ZAdd(a.into_bytes(), s)),
                    KeyKind::Stream => stream_fields(&a).map(KeyEdit::StreamAdd),
                    _ => Err("This type can't be created here; use the console".into()),
                };
                match first {
                    Ok(e) => self.send_edit(input.into_bytes(), e, true, cx),
                    Err(e) => {
                        self.status = Some((false, e));
                        cx.notify();
                    }
                }
            }
        }
    }

    // ----------------------------------------------------------------- console

    fn run_console(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let line = Self::value(&self.console_input, cx).trim().to_owned();
        if line.is_empty() || self.run_request.is_some() {
            return;
        }
        Self::set(&self.console_input, "", window, cx);
        if recallable(&line) {
            self.history.push(&line);
        } else {
            self.history.pos = None;
        }
        self.send_run(line, false, window, cx);
    }

    fn send_run(
        &mut self,
        line: String,
        confirmed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = next_id();
        self.run_request = Some(request);
        self.confirm = None;
        self.console.push(ConsoleLine {
            line: line.clone(),
            text: String::new(),
            error: false,
            ms: None,
        });
        if self.console.len() > CONSOLE_LINES {
            self.console.remove(0);
        }
        self.core.send(Command::RedisRun {
            session: self.session,
            request,
            line,
            confirmed,
        });
        self.sync_console(window, cx);
        cx.notify();
    }

    /// Show the transcript in the console view, scrolled to its end.
    fn sync_console(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = format!("db{}>", self.info.as_ref().map_or(0, |i| i.db));
        let text = console_text(&self.console, &prompt);
        let end = text.len();
        self.console_view.update(cx, |e, cx| {
            e.set_value(text, window, cx);
            e.set_selected_range(end..end, cx);
        });
    }

    /// [`switchyard_core::Event::RedisReply`].
    pub fn on_reply(
        &mut self,
        request: RequestId,
        outcome: RedisOutcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.run_request != Some(request) {
            return;
        }
        self.run_request = None;
        let Some(last) = self.console.last_mut() else {
            return;
        };
        match outcome {
            RedisOutcome::Output { text, error, ms } => {
                last.text = clip(&text, REPLY_CHARS);
                last.error = error;
                last.ms = Some(ms);
            }
            RedisOutcome::Failed(e) => {
                last.text = e;
                last.error = true;
            }
            RedisOutcome::NeedsConfirmation { reason } => {
                let line = last.line.clone();
                self.console.pop();
                self.confirm = Some((line, reason));
            }
        }
        self.sync_console(window, cx);
        cx.notify();
    }

    // ------------------------------------------------------------------ render

    fn render_header(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let info = match (&self.info, &self.open_error) {
            (Some(i), _) => format!(
                "{} · db {} · {} keys",
                i.version,
                i.db,
                ui::thousands(i.keys)
            ),
            (None, Some(_)) => "Not connected".into(),
            (None, None) => "Connecting…".into(),
        };
        let can_write = self.info.is_some() && !self.read_only();
        div()
            .flex_none()
            .h(rpx(38.))
            .flex()
            .items_center()
            .gap(rpx(10.))
            .px(rpx(12.))
            .border_b_1()
            .border_color(p.bd)
            .child(ui::monogram("RD", 24., p))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(ts::BASE)
                    .child(self.connection.name.clone()),
            )
            .when(self.connection.environment.is_production(), |d| {
                d.child(tag("PRODUCTION", p.prod))
            })
            .when(self.read_only(), |d| d.child(tag("READ-ONLY", p.fg3)))
            .child(
                div()
                    .font_family(MONO)
                    .text_size(ts::SMALL)
                    .text_color(p.fg3)
                    .truncate()
                    .child(info),
            )
            .child(div().flex_1())
            .when(can_write, |d| {
                d.child(
                    ui::button("rd-new", "New key", Kind::Secondary, p)
                        .h(rpx(24.))
                        .on_click(cx.listener(|t, _, w, cx| {
                            t.start_mode(Mode::NewKey(KeyKind::String), w, cx)
                        })),
                )
            })
            .child(
                ui::button(
                    "rd-console",
                    if self.console_open {
                        "Hide console"
                    } else {
                        "Console"
                    },
                    Kind::Secondary,
                    p,
                )
                .h(rpx(24.))
                .on_click(cx.listener(|t, _, w, cx| {
                    t.console_open = !t.console_open;
                    if t.console_open {
                        t.console_input.update(cx, |i, cx| i.focus(w, cx));
                    }
                    cx.notify();
                })),
            )
            .child(
                ui::button("rd-refresh", "Refresh", Kind::Secondary, p)
                    .h(rpx(24.))
                    .on_click(cx.listener(|t, _, w, cx| {
                        t.rescan(cx);
                        t.reload(w, cx);
                    })),
            )
            .into_any_element()
    }

    fn render_keys(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let keys = self.keys.clone();
        let rows = self.rows.clone();
        let selected = self.selected.clone();
        let delim = self.delimiter.clone();
        let p2 = *p;
        let scanning = self.scan_request.is_some();
        let footer = format!(
            "{} {}key{}{}",
            ui::thousands(self.keys.len() as u64),
            self.scanned_kind
                .as_ref()
                .map(|k| format!("{} ", k.label().to_lowercase()))
                .unwrap_or_default(),
            if self.keys.len() == 1 { "" } else { "s" },
            if self.scanned.is_empty() {
                String::new()
            } else {
                format!(" matching {}", self.scanned)
            }
        );
        let body = if let Some(e) = &self.list_error {
            div()
                .p(rpx(12.))
                .text_size(ts::BODY)
                .text_color(p.prod)
                .child(e.clone())
                .into_any_element()
        } else if keys.is_empty() && !scanning && self.info.is_some() {
            div()
                .p(rpx(12.))
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .child(if self.scanned.is_empty() && self.scanned_kind.is_none() {
                    "This database is empty."
                } else {
                    "No keys match."
                })
                .into_any_element()
        } else {
            uniform_list(
                "redis-keys",
                rows.len(),
                cx.processor(move |_this, range: std::ops::Range<usize>, _w, cx| {
                    let p = &p2;
                    range
                        .map(|r| {
                            let row = div()
                                .id(("rd-key", r))
                                .w_full()
                                .h(rpx(ROW_H))
                                .flex()
                                .items_center()
                                .gap(rpx(6.))
                                .pr(rpx(10.))
                                .text_size(ts::BODY)
                                .cursor_pointer();
                            match &rows[r] {
                                KeyRow::Folder {
                                    prefix,
                                    name,
                                    depth,
                                    keys: n,
                                    open,
                                } => {
                                    let prefix = prefix.clone();
                                    row.pl(rpx(indent(*depth)))
                                        .hover(|s| s.bg(p.hover))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.toggle_folder(prefix.clone(), cx)
                                        }))
                                        .child(
                                            div()
                                                .w(rpx(12.))
                                                .flex_none()
                                                .text_color(p.fg3)
                                                .child(if *open { "▾" } else { "▸" }),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .font_family(MONO)
                                                .text_color(p.fg2)
                                                .child(format!("{name}{}", text(&delim))),
                                        )
                                        .child(
                                            div()
                                                .flex_none()
                                                .px(rpx(6.))
                                                .rounded(px(8.))
                                                .bg(p.hover)
                                                .text_size(ts::CAPTION_PLUS)
                                                .text_color(p.fg3)
                                                .child(ui::thousands(*n as u64)),
                                        )
                                }
                                KeyRow::Key { ix, name, depth } => {
                                    let k = &keys[*ix];
                                    let key = k.key.clone();
                                    let is_sel = selected.as_deref() == Some(k.key.as_slice());
                                    row.pl(rpx(indent(*depth) + 18.))
                                        .when(is_sel, |d| d.bg(p.sel))
                                        .when(!is_sel, |d| d.hover(|s| s.bg(p.hover)))
                                        .on_click(cx.listener(move |this, _, w, cx| {
                                            this.select(key.clone(), w, cx)
                                        }))
                                        .child(kind_badge(&k.kind, p))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .font_family(MONO)
                                                .child(name.clone()),
                                        )
                                        .child(
                                            div()
                                                .flex_none()
                                                .w(rpx(48.))
                                                .text_right()
                                                .font_family(MONO)
                                                .text_size(ts::CAPTION_PLUS)
                                                .text_color(p.fg3)
                                                .child(short_ttl(k.ttl_ms)),
                                        )
                                        .child(
                                            div()
                                                .flex_none()
                                                .w(rpx(58.))
                                                .text_right()
                                                .font_family(MONO)
                                                .text_size(ts::CAPTION_PLUS)
                                                .text_color(p.fg3)
                                                .child(k.memory.map(ui::bytes).unwrap_or_default()),
                                        )
                                }
                            }
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .flex_1()
            .into_any_element()
        };
        let view = self.view;
        let this = cx.entity().downgrade();
        let view_option = |label: &'static str, v: KeyView| {
            let this = this.clone();
            let on: ui::OnClick = Box::new(move |_, _, cx| {
                let _ = this.update(cx, |t, cx| t.set_view(v, cx));
            });
            (SharedString::from(label), view == v, on)
        };
        let tools = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(rpx(6.))
            .px(rpx(8.))
            .pb(rpx(6.))
            .text_size(ts::SMALL)
            .child(ui::segmented(
                "rd-key-view",
                vec![
                    view_option("Tree", KeyView::Tree),
                    view_option("List", KeyView::List),
                ],
                20.,
                p,
            ))
            .child(div().flex_1())
            .when(view == KeyView::Tree, |d| {
                d.child(
                    div()
                        .id("rd-expand")
                        .text_color(p.acc)
                        .cursor_pointer()
                        .on_click(cx.listener(|t, _, _, cx| t.expand_all(true, cx)))
                        .child("Expand all"),
                )
                .child(
                    div()
                        .id("rd-collapse")
                        .text_color(p.acc)
                        .cursor_pointer()
                        .on_click(cx.listener(|t, _, _, cx| t.expand_all(false, cx)))
                        .child("Collapse"),
                )
            });
        let columns = div()
            .flex_none()
            .h(rpx(22.))
            .flex()
            .items_center()
            .gap(rpx(6.))
            .px(rpx(10.))
            .border_b_1()
            .border_color(p.bd)
            .text_size(ts::CAPTION)
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(p.fg3)
            .child(div().w(rpx(38.)).flex_none().child("TYPE"))
            .child(div().flex_1().min_w_0().child("KEY"))
            .child(div().w(rpx(48.)).flex_none().text_right().child("TTL"))
            .child(div().w(rpx(58.)).flex_none().text_right().child("SIZE"));
        div()
            .w(px(self.keys_w()))
            .flex_none()
            .relative()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(p.bd)
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(rpx(6.))
                    .p(rpx(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.pattern).text_size(ts::BODY)),
                    )
                    .child(self.render_kind_filter(cx)),
            )
            .child(tools)
            .child(columns)
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
            .child(
                div()
                    .flex_none()
                    .h(rpx(30.))
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .px(rpx(10.))
                    .border_t_1()
                    .border_color(p.bd)
                    .text_size(ts::SMALL)
                    .text_color(p.fg3)
                    .child(div().flex_1().min_w_0().truncate().child(footer))
                    .when(scanning, |d| d.child(ui::shimmer(40., p)))
                    .when(!scanning && self.cursor != 0, |d| {
                        d.child(
                            div()
                                .id("rd-more")
                                .text_color(p.acc)
                                .cursor_pointer()
                                .on_click(cx.listener(|t, _, _, cx| t.load_more(cx)))
                                .child("Load more"),
                        )
                    }),
            )
            .child(
                // Drag the right edge to widen the key list.
                div()
                    .id("rd-keys-resize")
                    .absolute()
                    .right(rpx(-3.))
                    .top_0()
                    .bottom_0()
                    .w(rpx(6.))
                    .cursor_col_resize()
                    .hover(|s| s.bg(p.acc.opacity(0.35)))
                    .when(self.keys_drag.is_some(), |d| d.bg(p.acc.opacity(0.35)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                            this.keys_drag = Some((ev.position.x.into(), this.keys_w()));
                            cx.stop_propagation();
                        }),
                    ),
            )
            .into_any_element()
    }

    /// The key type dropdown next to the pattern (`SCAN … TYPE`).
    fn render_kind_filter(&self, cx: &mut Context<Self>) -> AnyElement {
        let chosen = self.kind_filter.clone();
        let label = chosen
            .as_ref()
            .map_or("All types".to_owned(), |k| k.label().to_owned());
        let this = cx.entity().downgrade();
        Button::new("rd-kind-filter")
            .outline()
            .small()
            .label(label)
            .dropdown_menu(move |mut menu, _, _| {
                let options = std::iter::once(None).chain(KeyKind::FILTERS.map(Some));
                for kind in options {
                    let this = this.clone();
                    let label = kind
                        .as_ref()
                        .map_or("All types".to_owned(), |k| k.label().to_owned());
                    menu = menu.item(PopupMenuItem::new(label).checked(kind == chosen).on_click(
                        move |_, _, cx| {
                            let kind = kind.clone();
                            let _ = this.update(cx, |t, cx| t.set_kind_filter(kind, cx));
                        },
                    ));
                }
                menu
            })
            .into_any_element()
    }

    fn render_mode_bar(&self, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let busy = self.edit_request.is_some();
        let (label, apply, kind) = match &self.mode {
            Mode::View | Mode::NewKey(_) => return None,
            Mode::Rename => ("New name", "Rename", Kind::Primary),
            Mode::Ttl => ("Seconds (empty for no expiry)", "Set expiry", Kind::Primary),
            Mode::ConfirmDelete => ("", "Delete key", Kind::Destructive),
        };
        Some(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(rpx(8.))
                .px(rpx(12.))
                .py(rpx(8.))
                .border_b_1()
                .border_color(p.bd)
                .bg(p.panel)
                .text_size(ts::BODY)
                .map(|d| {
                    if self.mode == Mode::ConfirmDelete {
                        d.child(
                            div().flex_1().text_color(p.prod).child(
                                "Delete this key and its whole value? This can't be undone.",
                            ),
                        )
                    } else {
                        d.child(div().text_color(p.fg2).child(label)).child(
                            div()
                                .flex_1()
                                .child(Input::new(&self.in_key).text_size(ts::BODY)),
                        )
                    }
                })
                .child(
                    ui::button("rd-apply", apply, kind, p)
                        .when(busy, |b| b.opacity(0.5))
                        .on_click(cx.listener(|t, _, w, cx| t.apply_mode(w, cx))),
                )
                .child(
                    ui::button("rd-cancel", "Cancel", Kind::Ghost, p).on_click(cx.listener(
                        |t, _, _, cx| {
                            t.mode = Mode::View;
                            cx.notify();
                        },
                    )),
                )
                .into_any_element(),
        )
    }

    fn render_new_key(&self, kind: &KeyKind, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let options = KeyKind::CREATABLE
            .iter()
            .map(|k| {
                let this = this.clone();
                let k2 = k.clone();
                let on: ui::OnClick = Box::new(move |_, w, cx| {
                    let _ = this.update(cx, |t, cx| {
                        t.sync_placeholders(&k2, w, cx);
                        t.mode = Mode::NewKey(k2.clone());
                        t.status = None;
                        cx.notify();
                    });
                });
                (SharedString::from(k.label().to_owned()), k == kind, on)
            })
            .collect();
        let (a, b) = new_key_inputs(kind);
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(rpx(12.))
            .p(rpx(16.))
            .text_size(ts::BODY)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(ts::BASE)
                    .child("New key"),
            )
            .child(ui::segmented("rd-new-kind", options, 22., p))
            .child(labelled(
                "Key",
                Input::new(&self.in_key).text_size(ts::BODY),
                p,
            ))
            .map(|d| {
                if *kind == KeyKind::String {
                    d.child(
                        div()
                            .flex_1()
                            .min_h(rpx(120.))
                            .border_1()
                            .border_color(p.bd2)
                            .rounded(px(6.))
                            .child(
                                Editor::new(&self.text_editor)
                                    .bordered(false)
                                    .appearance(false)
                                    .h(relative(1.))
                                    .font_family(MONO)
                                    .text_size(ts::BODY),
                            ),
                    )
                } else {
                    d.when_some(a, |d, a| {
                        d.child(labelled(a, Input::new(&self.in_a).text_size(ts::BODY), p))
                    })
                    .when_some(b, |d, b| {
                        d.child(labelled(b, Input::new(&self.in_b).text_size(ts::BODY), p))
                    })
                }
            })
            .child(
                div()
                    .flex()
                    .gap(rpx(8.))
                    .child(
                        ui::button("rd-create", "Create", Kind::Primary, p)
                            .when(self.edit_request.is_some(), |b| b.opacity(0.5))
                            .on_click(cx.listener(|t, _, w, cx| t.apply_mode(w, cx))),
                    )
                    .child(
                        ui::button("rd-create-cancel", "Cancel", Kind::Ghost, p).on_click(
                            cx.listener(|t, _, w, cx| {
                                t.mode = Mode::View;
                                t.reload(w, cx);
                            }),
                        ),
                    ),
            )
            .into_any_element()
    }

    fn render_value(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        if let Mode::NewKey(kind) = &self.mode {
            return self.render_new_key(kind, p, cx);
        }
        let Some(d) = self.details.clone() else {
            let msg = match (&self.value_error, &self.selected, self.load_request) {
                (Some(e), _, _) => e.clone(),
                (_, Some(_), Some(_)) => String::new(),
                _ => "Select a key to see its value.".into(),
            };
            return div()
                .flex_1()
                .p(rpx(16.))
                .text_size(ts::BODY)
                .text_color(if self.value_error.is_some() {
                    p.prod
                } else {
                    p.fg3
                })
                .when(msg.is_empty(), |d| d.child(ui::shimmer(200., p)))
                .child(msg)
                .into_any_element();
        };
        let can_write = !self.read_only();
        let key_text = text(&d.key);
        let copy_key = key_text.clone();
        let mut facts = vec![d.kind.label().to_owned()];
        if d.kind != KeyKind::Missing {
            facts.push(match d.kind {
                KeyKind::String => ui::bytes(d.len),
                _ => format!("{} items", ui::thousands(d.len)),
            });
            facts.push(ttl_text(d.ttl_ms));
            if let Some(m) = d.memory {
                facts.push(format!("{} in memory", ui::bytes(m)));
            }
        }
        let header = div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(rpx(4.))
            .px(rpx(12.))
            .py(rpx(8.))
            .border_b_1()
            .border_color(p.bd)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .child(kind_badge(&d.kind, p))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(MONO)
                            .text_size(ts::BASE)
                            .font_weight(FontWeight::MEDIUM)
                            .child(key_text),
                    )
                    .child(
                        ui::button("rd-copy-key", "Copy key", Kind::Ghost, p).on_click(
                            cx.listener(move |_, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy_key.clone()))
                            }),
                        ),
                    )
                    .when(can_write && d.kind != KeyKind::Missing, |el| {
                        el.child(
                            ui::button("rd-rename", "Rename", Kind::Ghost, p).on_click(
                                cx.listener(|t, _, w, cx| t.start_mode(Mode::Rename, w, cx)),
                            ),
                        )
                        .child(
                            ui::button("rd-ttl", "Expiry", Kind::Ghost, p).on_click(
                                cx.listener(|t, _, w, cx| t.start_mode(Mode::Ttl, w, cx)),
                            ),
                        )
                        .child(
                            ui::button("rd-del", "Delete", Kind::Ghost, p).on_click(
                                cx.listener(|t, _, w, cx| t.start_mode(Mode::ConfirmDelete, w, cx)),
                            ),
                        )
                    }),
            )
            .child(
                div()
                    .text_size(ts::SMALL)
                    .text_color(p.fg3)
                    .font_family(MONO)
                    .child(facts.join(" · ")),
            );
        let truncated = d.truncated().then(|| {
            div()
                .flex_none()
                .px(rpx(12.))
                .py(rpx(4.))
                .text_size(ts::SMALL)
                .text_color(p.stg)
                .child(match d.kind {
                    KeyKind::String => format!(
                        "Showing the first {} of {}; use GETRANGE in the console for the rest.",
                        ui::bytes(shown_len(&d.value) as u64),
                        ui::bytes(d.len)
                    ),
                    _ => format!(
                        "Showing {} of {} items; use the console for the rest.",
                        ui::thousands(shown_len(&d.value) as u64),
                        ui::thousands(d.len)
                    ),
                })
        });
        let body = match &d.value {
            KeyValue::String(b) => {
                let binary = std::str::from_utf8(b).is_err();
                let editable = can_write && !binary && !d.truncated();
                self.render_text(editable, binary, p, cx)
            }
            KeyValue::Json(_) => self.render_text(false, false, p, cx),
            KeyValue::Missing => note("This key no longer exists.", p),
            KeyValue::Unsupported => note(
                "The browser can't show this type. Use the console (module commands work there).",
                p,
            ),
            _ => self.render_table(&d, can_write, p, cx),
        };
        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header)
            .children(self.render_mode_bar(p, cx))
            .children(truncated)
            .child(body)
            .into_any_element()
    }

    fn render_text(
        &self,
        editable: bool,
        binary: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div().flex_1().min_h_0().child(
                    Editor::new(&self.text_editor)
                        .readonly(!editable)
                        .bordered(false)
                        .appearance(false)
                        .h(relative(1.))
                        .font_family(MONO)
                        .text_size(ts::BODY),
                ),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .px(rpx(12.))
                    .py(rpx(6.))
                    .border_t_1()
                    .border_color(p.bd)
                    .text_size(ts::SMALL)
                    .text_color(p.fg3)
                    .child(div().flex_1().child(if binary {
                        "Binary value, shown escaped. Edit it in the console."
                    } else if editable {
                        "Saving keeps the key's expiry."
                    } else {
                        ""
                    }))
                    .child(
                        ui::button("rd-copy-text", "Copy value", Kind::Ghost, p).on_click(
                            cx.listener(|t, _, _, cx| {
                                let v = t.text_editor.read(cx).value().to_string();
                                cx.write_to_clipboard(ClipboardItem::new_string(v));
                            }),
                        ),
                    )
                    .child(
                        ui::button("rd-format", "Format JSON", Kind::Ghost, p).on_click(
                            cx.listener(|t, _, w, cx| {
                                let s = t.text_editor.read(cx).value().to_string();
                                if looks_like_json(&s) {
                                    let pretty = pretty_json(&s);
                                    t.text_editor.update(cx, |e, cx| {
                                        e.set_highlighter("json", cx);
                                        e.set_value(pretty, w, cx);
                                    });
                                }
                            }),
                        ),
                    )
                    .when(editable, |d| {
                        d.child(
                            ui::button("rd-save", "Save", Kind::Primary, p)
                                .when(self.edit_request.is_some(), |b| b.opacity(0.5))
                                .on_click(
                                    cx.listener(|t, _, _, cx| t.edit_action(EditAction::Save, cx)),
                                ),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_table(
        &self,
        d: &KeyDetails,
        can_write: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (headers, rows) = table_rows(&d.value);
        let rows: Arc<[Vec<String>]> = rows.into();
        let widths = column_widths(&d.kind);
        let selected = self.row;
        let p2 = *p;
        let cell = move |w: Option<f32>| {
            let c = div().px(rpx(6.)).min_w_0().truncate();
            match w {
                Some(w) => c.w(rpx(w)).flex_none(),
                None => c.flex_1(),
            }
        };
        let header = div()
            .flex_none()
            .flex()
            .h(rpx(ROW_H))
            .items_center()
            .px(rpx(6.))
            .border_b_1()
            .border_color(p.bd)
            .text_size(ts::CAPTION_PLUS)
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(p.fg3)
            .children(
                headers
                    .iter()
                    .zip(widths.iter())
                    .map(|(h, w)| cell(*w).child(*h)),
            );
        let list = if rows.is_empty() {
            note("Empty.", p)
        } else {
            uniform_list(
                "redis-value",
                rows.len(),
                cx.processor(move |_this, range: std::ops::Range<usize>, _w, cx| {
                    let p = &p2;
                    range
                        .map(|r| {
                            let is_sel = selected == Some(r);
                            div()
                                .id(("rd-row", r))
                                .w_full()
                                .h(rpx(ROW_H))
                                .flex()
                                .items_center()
                                .px(rpx(6.))
                                .border_b_1()
                                .border_color(p.line)
                                .text_size(ts::BODY)
                                .font_family(MONO)
                                .cursor_pointer()
                                .when(is_sel, |d| d.bg(p.sel))
                                .when(!is_sel, |d| d.hover(|s| s.bg(p.hover)))
                                .on_click(
                                    cx.listener(move |this, _, w, cx| this.select_row(r, w, cx)),
                                )
                                .children(rows[r].iter().zip(widths.iter()).enumerate().map(
                                    |(c, (t, w))| {
                                        cell(*w)
                                            .when(c == 0 && widths.len() > 1, |d| {
                                                d.text_color(p.fg2)
                                            })
                                            .child(t.clone())
                                    },
                                ))
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .flex_1()
            .into_any_element()
        };
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header)
            .child(div().flex_1().min_h_0().flex().flex_col().child(list))
            .children(self.render_row_detail(d, p, cx))
            .when(can_write, |el| el.child(self.render_edit_bar(d, p, cx)))
            .into_any_element()
    }

    /// The selected row's full value, selectable, with copy buttons.
    fn render_row_detail(
        &self,
        d: &KeyDetails,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let r = self.row?;
        let (title, name) = row_title(&d.value, r)?;
        let copy_name = name.clone();
        Some(
            div()
                .flex_none()
                .h(rpx(DETAIL_H))
                .flex()
                .flex_col()
                .border_t_1()
                .border_color(p.bd)
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(rpx(8.))
                        .px(rpx(12.))
                        .py(rpx(4.))
                        .text_size(ts::SMALL)
                        .child(div().flex_none().text_color(p.fg3).child(title))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(MONO)
                                .text_color(p.fg2)
                                .child(clip(&name, CELL_CHARS)),
                        )
                        .when(!copy_name.is_empty(), |el| {
                            el.child(
                                ui::button("rd-copy-name", "Copy name", Kind::Ghost, p).on_click(
                                    cx.listener(move |_, _, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            copy_name.clone(),
                                        ))
                                    }),
                                ),
                            )
                        })
                        .child(
                            ui::button("rd-copy-value", "Copy value", Kind::Ghost, p).on_click(
                                cx.listener(|t, _, _, cx| {
                                    let v = t.row_view.read(cx).value().to_string();
                                    cx.write_to_clipboard(ClipboardItem::new_string(v));
                                }),
                            ),
                        )
                        .child(
                            ui::button("rd-row-close", "Close", Kind::Ghost, p).on_click(
                                cx.listener(|t, _, _, cx| {
                                    t.row = None;
                                    cx.notify();
                                }),
                            ),
                        ),
                )
                .child(
                    div().flex_1().min_h_0().px(rpx(6.)).child(
                        Editor::new(&self.row_view)
                            .readonly(true)
                            .bordered(false)
                            .appearance(false)
                            .h(relative(1.))
                            .font_family(MONO)
                            .text_size(ts::BODY),
                    ),
                )
                .into_any_element(),
        )
    }

    fn render_edit_bar(&self, d: &KeyDetails, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let row = self.row;
        let (a, b) = new_key_inputs(&d.kind);
        let btn =
            |id: &'static str, label: String, kind: Kind, enabled: bool, action: EditAction| {
                ui::button(id, label, kind, p)
                    .when(!enabled, |b| b.opacity(0.45))
                    .when(enabled, |b| {
                        b.on_click(cx.listener(move |t, _, _, cx| t.edit_action(action, cx)))
                    })
            };
        let selected = row.is_some();
        let buttons: Vec<AnyElement> = match &d.kind {
            KeyKind::Hash => vec![
                btn(
                    "rd-e-save",
                    "Save field".into(),
                    Kind::Primary,
                    true,
                    EditAction::Save,
                )
                .into_any_element(),
                btn(
                    "rd-e-rm",
                    "Remove field".into(),
                    Kind::Secondary,
                    selected,
                    EditAction::Remove,
                )
                .into_any_element(),
            ],
            KeyKind::List => vec![
                btn(
                    "rd-e-save",
                    row.map_or("Save at row".into(), |r| format!("Save at #{r}")),
                    Kind::Primary,
                    selected,
                    EditAction::Save,
                )
                .into_any_element(),
                btn(
                    "rd-e-head",
                    "Add to head".into(),
                    Kind::Secondary,
                    true,
                    EditAction::PushHead,
                )
                .into_any_element(),
                btn(
                    "rd-e-tail",
                    "Add to tail".into(),
                    Kind::Secondary,
                    true,
                    EditAction::PushTail,
                )
                .into_any_element(),
                btn(
                    "rd-e-rm",
                    "Remove".into(),
                    Kind::Secondary,
                    selected,
                    EditAction::Remove,
                )
                .into_any_element(),
            ],
            KeyKind::Set | KeyKind::ZSet => vec![
                btn(
                    "rd-e-save",
                    "Save member".into(),
                    Kind::Primary,
                    true,
                    EditAction::Save,
                )
                .into_any_element(),
                btn(
                    "rd-e-rm",
                    "Remove member".into(),
                    Kind::Secondary,
                    selected,
                    EditAction::Remove,
                )
                .into_any_element(),
            ],
            KeyKind::Stream => vec![
                btn(
                    "rd-e-save",
                    "Add entry".into(),
                    Kind::Primary,
                    true,
                    EditAction::Save,
                )
                .into_any_element(),
                btn(
                    "rd-e-rm",
                    "Remove entry".into(),
                    Kind::Secondary,
                    selected,
                    EditAction::Remove,
                )
                .into_any_element(),
            ],
            _ => Vec::new(),
        };
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(rpx(8.))
            .px(rpx(12.))
            .py(rpx(8.))
            .border_t_1()
            .border_color(p.bd)
            .bg(p.panel)
            .text_size(ts::BODY)
            .when(a.is_some(), |el| {
                el.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Input::new(&self.in_a).text_size(ts::BODY)),
                )
            })
            .when(b.is_some(), |el| {
                el.child(
                    div()
                        .w(rpx(if d.kind == KeyKind::ZSet { 110. } else { 0. }))
                        .when(d.kind != KeyKind::ZSet, |d| d.flex_1())
                        .min_w_0()
                        .child(Input::new(&self.in_b).text_size(ts::BODY)),
                )
            })
            .children(buttons)
            .into_any_element()
    }

    fn render_console(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let prompt = format!("db{}>", self.info.as_ref().map_or(0, |i| i.db));
        let confirm = self.confirm.as_ref().map(|(line, reason)| {
            let line = line.clone();
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(rpx(8.))
                .px(rpx(12.))
                .py(rpx(6.))
                .bg(p.prod_bg)
                .text_size(ts::BODY)
                .child(div().flex_1().text_color(p.prod).child(reason.clone()))
                .child(
                    ui::button("rd-run-anyway", "Run anyway", Kind::Destructive, p).on_click(
                        cx.listener(move |t, _, w, cx| t.send_run(line.clone(), true, w, cx)),
                    ),
                )
                .child(
                    ui::button("rd-run-cancel", "Cancel", Kind::Ghost, p).on_click(cx.listener(
                        |t, _, _, cx| {
                            t.confirm = None;
                            cx.notify();
                        },
                    )),
                )
        });
        div()
            .flex_none()
            .h(rpx(220.))
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(p.bd)
            .bg(p.panel)
            .child(if self.console.is_empty() {
                div()
                    .flex_1()
                    .min_h_0()
                    .px(rpx(12.))
                    .py(rpx(6.))
                    .text_size(ts::BODY)
                    .text_color(p.fg3)
                    .child(
                        "Run any Redis command. Blocking and connection commands \
                         (SUBSCRIBE, MONITOR, SELECT) are not available here.",
                    )
                    .into_any_element()
            } else {
                // A read-only editor: replies can be selected and copied.
                div()
                    .flex_1()
                    .min_h_0()
                    .px(rpx(6.))
                    .child(
                        Editor::new(&self.console_view)
                            .readonly(true)
                            .bordered(false)
                            .appearance(false)
                            .h(relative(1.))
                            .font_family(MONO)
                            .text_size(ts::BODY),
                    )
                    .into_any_element()
            })
            .children(confirm)
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .px(rpx(12.))
                    .py(rpx(6.))
                    .border_t_1()
                    .border_color(p.bd)
                    .child(
                        div()
                            .font_family(MONO)
                            .text_size(ts::BODY)
                            .text_color(p.acc)
                            .child(prompt),
                    )
                    .child(
                        div()
                            .flex_1()
                            .capture_key_down(cx.listener(|t, ev: &KeyDownEvent, w, cx| {
                                let m = &ev.keystroke.modifiers;
                                if m.control || m.alt || m.shift || m.platform {
                                    return;
                                }
                                let up = match ev.keystroke.key.as_str() {
                                    "up" => true,
                                    "down" => false,
                                    _ => return,
                                };
                                t.recall(up, w, cx);
                                cx.stop_propagation();
                            }))
                            .child(
                                Input::new(&self.console_input)
                                    .appearance(false)
                                    .font_family(MONO)
                                    .text_size(ts::BODY),
                            ),
                    )
                    .child(
                        ui::button("rd-run", "Run", Kind::Secondary, p)
                            .h(rpx(24.))
                            .when(self.run_request.is_some(), |b| b.opacity(0.5))
                            .on_click(cx.listener(|t, _, w, cx| t.run_console(w, cx))),
                    )
                    .when(!self.console.is_empty(), |d| {
                        d.child(
                            ui::button("rd-console-copy", "Copy all", Kind::Ghost, p)
                                .h(rpx(24.))
                                .on_click(cx.listener(|t, _, _, cx| {
                                    let all = t.console_view.read(cx).value().to_string();
                                    cx.write_to_clipboard(ClipboardItem::new_string(all));
                                })),
                        )
                        .child(
                            ui::button("rd-console-clear", "Clear", Kind::Ghost, p)
                                .h(rpx(24.))
                                .on_click(cx.listener(|t, _, w, cx| {
                                    if t.run_request.is_none() {
                                        t.console.clear();
                                        t.sync_console(w, cx);
                                        cx.notify();
                                    }
                                })),
                        )
                    }),
            )
            .into_any_element()
    }
}

impl Render for RedisTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let header = self.render_header(&p, cx);
        let status = self.status.as_ref().map(|(ok, m)| {
            div()
                .flex_none()
                .px(rpx(12.))
                .py(rpx(4.))
                .text_size(ts::BODY)
                .text_color(if *ok { p.dev } else { p.prod })
                .child(m.clone())
        });
        let main = match &self.open_error {
            Some(e) => div()
                .flex_1()
                .p(rpx(16.))
                .flex()
                .flex_col()
                .gap(rpx(10.))
                .text_size(ts::BODY)
                .child(div().text_color(p.prod).child(e.clone()))
                .child(
                    ui::button("rd-reconnect", "Reconnect", Kind::Secondary, &p).on_click(
                        cx.listener(|t, _, _, cx| {
                            t.open_error = None;
                            t.core.send(Command::RedisOpen {
                                session: t.session,
                                connection: t.connection.id.clone(),
                            });
                            cx.notify();
                        }),
                    ),
                )
                .into_any_element(),
            None => div()
                .flex_1()
                .min_h_0()
                .flex()
                .child(self.render_keys(&p, cx))
                .child(self.render_value(&p, cx))
                .into_any_element(),
        };
        let console =
            (self.console_open && self.info.is_some()).then(|| self.render_console(&p, cx));
        let tab_w = self.tab_w.clone();
        div()
            .id("redis-tab")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .font_family(SANS)
            .bg(p.bg)
            .text_color(p.fg)
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                if let Some((x0, w0)) = this.keys_drag {
                    if ev.pressed_button == Some(MouseButton::Left) {
                        let x: f32 = ev.position.x.into();
                        let max = (this.tab_w.get() - VALUE_MIN).max(KEYS_MIN);
                        this.keys_width = (w0 + (x - x0)).clamp(KEYS_MIN, max);
                        cx.notify();
                    } else {
                        this.keys_drag = None;
                        cx.notify();
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.keys_drag.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .child(
                gpui_kit::canvas(
                    move |b, _, _| tab_w.set(f32::from(b.size.width)),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(header)
            .children(status)
            .child(main)
            .children(console)
    }
}

/// An edit-bar action, interpreted per value type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditAction {
    Save,
    Remove,
    PushHead,
    PushTail,
}

fn tag(label: &'static str, color: Hsla) -> gpui_kit::Div {
    div()
        .px(rpx(5.))
        .rounded(px(3.))
        .border_1()
        .border_color(color)
        .text_size(ts::TINY_PLUS)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(color)
        .child(label)
}

fn kind_color(kind: &KeyKind, p: &Palette) -> Hsla {
    match kind {
        KeyKind::String => p.sx_str,
        KeyKind::Hash => p.acc,
        KeyKind::List => p.sx_fn,
        KeyKind::Set => p.dev,
        KeyKind::ZSet => p.sx_num,
        KeyKind::Stream => p.stg,
        KeyKind::Json => p.sx_kw,
        KeyKind::Missing | KeyKind::Other(_) => p.fg3,
    }
}

fn kind_badge(kind: &KeyKind, p: &Palette) -> gpui_kit::Div {
    let c = kind_color(kind, p);
    div()
        .w(rpx(38.))
        .flex_none()
        .flex()
        .justify_center()
        .rounded(px(3.))
        .border_1()
        .border_color(c.opacity(0.5))
        .text_color(c)
        .font_family(MONO)
        .font_weight(FontWeight::SEMIBOLD)
        .text_size(ts::TINY)
        .line_height(rpx(15.))
        .child(SharedString::from(kind.badge().to_owned()))
}

fn labelled(label: &'static str, input: impl IntoElement, p: &Palette) -> gpui_kit::Div {
    div()
        .flex()
        .flex_col()
        .gap(rpx(4.))
        .child(div().text_size(ts::LABEL).text_color(p.fg2).child(label))
        .child(input)
}

fn note(text: &'static str, p: &Palette) -> AnyElement {
    div()
        .p(rpx(16.))
        .text_size(ts::BODY)
        .text_color(p.fg3)
        .child(text)
        .into_any_element()
}

/// Labels of the first and second edit-bar inputs for a type (`None`: not used).
fn new_key_inputs(kind: &KeyKind) -> (Option<&'static str>, Option<&'static str>) {
    match kind {
        KeyKind::Hash => (Some("Field"), Some("Value")),
        KeyKind::List => (Some("Value"), None),
        KeyKind::Set => (Some("Member"), None),
        KeyKind::ZSet => (Some("Member"), Some("Score")),
        KeyKind::Stream => (Some("Fields: name value [name value …]"), None),
        _ => (None, None),
    }
}

/// Bytes as display text: UTF-8 as is, anything else escaped like `redis-cli`.
pub fn text(b: &[u8]) -> String {
    match std::str::from_utf8(b) {
        Ok(s) => s.to_owned(),
        Err(_) => escape(b),
    }
}

/// One table cell: single line, clipped.
fn cell_text(b: &[u8]) -> String {
    let t = text(b).replace(['\n', '\r'], "⏎");
    clip(&t, CELL_CHARS)
}

fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

/// Column headers and rows of a collection value.
fn table_rows(v: &KeyValue) -> (Vec<&'static str>, Vec<Vec<String>>) {
    match v {
        KeyValue::Hash(f) => (
            vec!["FIELD", "VALUE"],
            f.iter()
                .map(|(k, v)| vec![cell_text(k), cell_text(v)])
                .collect(),
        ),
        KeyValue::List(l) => (
            vec!["#", "VALUE"],
            l.iter()
                .enumerate()
                .map(|(i, v)| vec![i.to_string(), cell_text(v)])
                .collect(),
        ),
        KeyValue::Set(s) => (
            vec!["MEMBER"],
            s.iter().map(|m| vec![cell_text(m)]).collect(),
        ),
        KeyValue::ZSet(z) => (
            vec!["SCORE", "MEMBER"],
            z.iter()
                .map(|(m, s)| vec![s.to_string(), cell_text(m)])
                .collect(),
        ),
        KeyValue::Stream(e) => (
            vec!["ID", "FIELDS"],
            e.iter()
                .map(|e| vec![e.id.clone(), clip(&fields_line(&e.fields), CELL_CHARS)])
                .collect(),
        ),
        _ => (Vec::new(), Vec::new()),
    }
}

fn column_widths(kind: &KeyKind) -> Vec<Option<f32>> {
    match kind {
        KeyKind::Hash => vec![Some(220.), None],
        KeyKind::List => vec![Some(60.), None],
        KeyKind::ZSet => vec![Some(110.), None],
        KeyKind::Stream => vec![Some(170.), None],
        _ => vec![None],
    }
}

fn shown_len(v: &KeyValue) -> usize {
    match v {
        KeyValue::String(b) => b.len(),
        KeyValue::List(v) | KeyValue::Set(v) => v.len(),
        KeyValue::ZSet(v) => v.len(),
        KeyValue::Hash(v) => v.len(),
        KeyValue::Stream(v) => v.len(),
        _ => 0,
    }
}

/// Stream fields as one editable line (`name value …`, quoted where needed).
fn fields_line(fields: &[(Vec<u8>, Vec<u8>)]) -> String {
    fields
        .iter()
        .flat_map(|(f, v)| [quote(f), quote(v)])
        .collect::<Vec<_>>()
        .join(" ")
}

/// A stream field and its value.
type Pair = (Vec<u8>, Vec<u8>);

/// Parse `name value [name value …]` for XADD.
fn stream_fields(line: &str) -> Result<Vec<Pair>, String> {
    let args = split(line).map_err(|e| e.to_string())?;
    if args.is_empty() || args.len() % 2 != 0 {
        return Err("Enter field and value pairs: name value [name value …]".into());
    }
    Ok(args
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[f, v]| (f.clone(), v.clone()))
        .collect())
}

fn parse_score(s: &str) -> Result<f64, String> {
    let s = s.trim();
    match s.to_ascii_lowercase().as_str() {
        "inf" | "+inf" => return Ok(f64::INFINITY),
        "-inf" => return Ok(f64::NEG_INFINITY),
        _ => {}
    }
    s.parse::<f64>()
        .ok()
        .filter(|f| !f.is_nan())
        .ok_or_else(|| "Score must be a number".into())
}

/// TTL input: empty or `0`-free text means no expiry; otherwise whole seconds.
fn parse_ttl(s: &str) -> Result<Option<u64>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    s.parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .map(Some)
        .ok_or_else(|| "Expiry must be a whole number of seconds, or empty for none".into())
}

fn ttl_text(ttl_ms: Option<i64>) -> String {
    match ttl_ms {
        None => "No expiry".into(),
        Some(ms) => {
            let s = ms / 1000;
            let (d, h, m, sec) = (s / 86_400, s / 3600 % 24, s / 60 % 60, s % 60);
            let t = if d > 0 {
                format!("{d}d {h}h")
            } else if h > 0 {
                format!("{h}h {m}m")
            } else if m > 0 {
                format!("{m}m {sec}s")
            } else {
                format!("{sec}s")
            };
            format!("Expires in {t}")
        }
    }
}

fn looks_like_json(s: &str) -> bool {
    let t = s.trim_start();
    (t.starts_with('{') || t.starts_with('['))
        && serde_json::from_str::<serde_json::Value>(s).is_ok()
}

fn pretty_json(s: &str) -> String {
    serde_json::from_str::<serde_json::Value>(s)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| s.to_owned())
}

/// Left padding of a key-list row at `depth`.
fn indent(depth: usize) -> f32 {
    8. + depth as f32 * 14.
}

/// TTL for the key list's column (`—` without expiry).
fn short_ttl(ttl_ms: Option<i64>) -> String {
    let Some(ms) = ttl_ms else {
        return "—".into();
    };
    let s = ms / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86_400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// The flat list view: every key, full name.
fn list_rows(keys: &[KeyEntry]) -> Vec<KeyRow> {
    keys.iter()
        .enumerate()
        .map(|(ix, k)| KeyRow::Key {
            ix,
            name: text(&k.key),
            depth: 0,
        })
        .collect()
}

/// The tree view of `keys` (sorted by bytes): keys sharing a prefix up to `delim` group
/// into a folder, folders first at each level, like Redis Insight. Only folders in
/// `expanded` show their contents.
fn tree_rows(keys: &[KeyEntry], delim: &[u8], expanded: &HashSet<Vec<u8>>) -> Vec<KeyRow> {
    let mut out = Vec::new();
    tree_level(keys, 0, keys.len(), 0, 0, delim, expanded, &mut out);
    out
}

/// Rows for `keys[start..end]`, which all share the first `skip` bytes.
#[allow(clippy::too_many_arguments)]
fn tree_level(
    keys: &[KeyEntry],
    start: usize,
    end: usize,
    skip: usize,
    depth: usize,
    delim: &[u8],
    expanded: &HashSet<Vec<u8>>,
    out: &mut Vec<KeyRow>,
) {
    let mut leaves = Vec::new();
    let mut i = start;
    while i < end {
        let key = &keys[i].key;
        let Some(at) = find(&key[skip..], delim) else {
            leaves.push(KeyRow::Key {
                ix: i,
                name: text(&key[skip..]),
                depth,
            });
            i += 1;
            continue;
        };
        // Sorted keys keep a prefix's keys together.
        let prefix = &key[..skip + at + delim.len()];
        let mut j = i + 1;
        while j < end && keys[j].key.starts_with(prefix) {
            j += 1;
        }
        let open = expanded.contains(prefix);
        out.push(KeyRow::Folder {
            prefix: prefix.to_vec(),
            name: text(&key[skip..skip + at]),
            depth,
            keys: j - i,
            open,
        });
        if open {
            tree_level(keys, i, j, prefix.len(), depth + 1, delim, expanded, out);
        }
        i = j;
    }
    out.extend(leaves);
}

/// Where `needle` first occurs in `hay` (an empty needle never does).
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Every folder prefix in `keys` (for "Expand all").
fn folder_prefixes(keys: &[KeyEntry], delim: &[u8]) -> HashSet<Vec<u8>> {
    let mut out = HashSet::new();
    for k in keys {
        let mut pos = 0;
        while let Some(at) = find(&k.key[pos..], delim) {
            pos += at + delim.len();
            out.insert(k.key[..pos].to_vec());
        }
    }
    out
}

/// The full value of row `r` of a collection, for the row viewer.
fn row_value(v: &KeyValue, r: usize) -> Option<String> {
    match v {
        KeyValue::Hash(f) => f.get(r).map(|(_, v)| text(v)),
        KeyValue::List(l) | KeyValue::Set(l) => l.get(r).map(|m| text(m)),
        KeyValue::ZSet(z) => z.get(r).map(|(m, _)| text(m)),
        KeyValue::Stream(e) => e.get(r).map(|e| {
            // Fields as a JSON object when they are text, else the XADD line.
            let map: Option<serde_json::Map<String, serde_json::Value>> = e
                .fields
                .iter()
                .map(|(f, v)| {
                    Some((
                        String::from_utf8(f.clone()).ok()?,
                        serde_json::Value::String(String::from_utf8(v.clone()).ok()?),
                    ))
                })
                .collect();
            map.and_then(|m| serde_json::to_string_pretty(&m).ok())
                .unwrap_or_else(|| fields_line(&e.fields))
        }),
        _ => None,
    }
}

/// What the row viewer shows above the value: a label and the row's name (field, index,
/// score or entry id; empty when the value is all there is).
fn row_title(v: &KeyValue, r: usize) -> Option<(String, String)> {
    match v {
        KeyValue::Hash(f) => f.get(r).map(|(k, _)| ("FIELD".into(), text(k))),
        KeyValue::List(l) => l.get(r).map(|_| (format!("INDEX {r}"), String::new())),
        KeyValue::Set(s) => s.get(r).map(|_| ("MEMBER".into(), String::new())),
        KeyValue::ZSet(z) => z.get(r).map(|(_, s)| (format!("SCORE {s}"), String::new())),
        KeyValue::Stream(e) => e.get(r).map(|e| ("ENTRY".into(), e.id.clone())),
        _ => None,
    }
}

/// The console as one text, `redis-cli` style: prompt and command, then the reply.
fn console_text(lines: &[ConsoleLine], prompt: &str) -> String {
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate() {
        if i > 0 {
            out.push_str("\n\n");
        }
        out.push_str(prompt);
        out.push(' ');
        out.push_str(&l.line);
        if let Some(ms) = l.ms {
            out.push_str(&format!("    ({ms} ms)"));
        }
        out.push('\n');
        if l.text.is_empty() && l.ms.is_none() && !l.error {
            out.push('…');
        } else if l.error && !l.text.starts_with("(error)") {
            out.push_str("(error) ");
            out.push_str(&l.text);
        } else {
            out.push_str(&l.text);
        }
    }
    out
}

impl Workspace {
    /// Open (or focus) the key browser of a Redis connection.
    pub(crate) fn open_redis(
        &mut self,
        connection: DbConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ix) = self
            .tabs
            .iter()
            .position(|t| matches!(t, Tab::Redis(r) if r.read(cx).connection.id == connection.id))
        {
            self.activate(ix, cx);
            return;
        }
        let core = self.core.clone();
        let tab = cx.new(|cx| RedisTab::new(core, connection, window, cx));
        self.tabs.push(Tab::Redis(tab));
        self.activate(self.tabs.len() - 1, cx);
    }

    fn redis_tab(&self, session: SessionId, cx: &Context<Self>) -> Option<Entity<RedisTab>> {
        self.tabs.iter().find_map(|t| match t {
            Tab::Redis(r) if r.read(cx).owns(session) => Some(r.clone()),
            _ => None,
        })
    }

    /// Route the `Redis*` events to their tab.
    pub(crate) fn on_redis_event(
        &mut self,
        ev: switchyard_core::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use switchyard_core::Event;
        let session = match &ev {
            Event::RedisOpened { session, .. }
            | Event::RedisKeys { session, .. }
            | Event::RedisKey { session, .. }
            | Event::RedisEdited { session, .. }
            | Event::RedisReply { session, .. } => *session,
            _ => return,
        };
        let Some(tab) = self.redis_tab(session, cx) else {
            if let Event::RedisOpened { result: Ok(_), .. } = ev {
                // The tab closed while connecting.
                self.core.send(Command::CloseSession { session });
            }
            return;
        };
        tab.update(cx, |t, cx| match ev {
            Event::RedisOpened { result, .. } => t.on_opened(result, cx),
            Event::RedisKeys {
                request, result, ..
            } => t.on_keys(request, result, cx),
            Event::RedisKey {
                request, result, ..
            } => t.on_key(request, result, window, cx),
            Event::RedisEdited {
                request,
                key,
                result,
                ..
            } => t.on_edited(request, key, result, window, cx),
            Event::RedisReply {
                request, outcome, ..
            } => t.on_reply(request, outcome, window, cx),
            _ => {}
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_core::db::redis::StreamEntry;

    #[test]
    fn ttl_text_reads_well() {
        assert_eq!(ttl_text(None), "No expiry");
        assert_eq!(ttl_text(Some(42_000)), "Expires in 42s");
        assert_eq!(ttl_text(Some(125_000)), "Expires in 2m 5s");
        assert_eq!(ttl_text(Some(3_700_000)), "Expires in 1h 1m");
        assert_eq!(ttl_text(Some(90_000_000)), "Expires in 1d 1h");
    }

    #[test]
    fn parses_inputs() {
        assert_eq!(parse_ttl(""), Ok(None));
        assert_eq!(parse_ttl(" 60 "), Ok(Some(60)));
        assert!(parse_ttl("0").is_err());
        assert!(parse_ttl("1.5").is_err());
        assert_eq!(parse_score("1.5"), Ok(1.5));
        assert_eq!(parse_score("-inf"), Ok(f64::NEG_INFINITY));
        assert!(parse_score("abc").is_err());
        assert_eq!(
            stream_fields("a 1 \"b c\" 2"),
            Ok(vec![
                (b"a".to_vec(), b"1".to_vec()),
                (b"b c".to_vec(), b"2".to_vec())
            ])
        );
        assert!(stream_fields("a").is_err());
        assert!(stream_fields("").is_err());
    }

    #[test]
    fn stream_fields_round_trip() {
        let f = vec![
            (b"name".to_vec(), b"a b".to_vec()),
            (b"n".to_vec(), b"1".to_vec()),
        ];
        assert_eq!(stream_fields(&fields_line(&f)), Ok(f));
    }

    #[test]
    fn table_rows_per_type() {
        let (h, r) = table_rows(&KeyValue::ZSet(vec![(b"m".to_vec(), 2.5)]));
        assert_eq!(h, ["SCORE", "MEMBER"]);
        assert_eq!(r, vec![vec!["2.5".to_owned(), "m".to_owned()]]);
        let (_, r) = table_rows(&KeyValue::List(vec![b"x\ny".to_vec(), vec![0xff]]));
        assert_eq!(r[0], ["0", "x⏎y"]);
        assert_eq!(r[1], ["1", "\\xff"]);
        let (h, r) = table_rows(&KeyValue::Stream(vec![StreamEntry {
            id: "1-0".into(),
            fields: vec![(b"f".to_vec(), b"v".to_vec())],
        }]));
        assert_eq!(h, ["ID", "FIELDS"]);
        assert_eq!(r[0], ["1-0", "f v"]);
    }

    #[test]
    fn clips_long_text() {
        assert_eq!(clip("abcdef", 3), "abc…");
        assert_eq!(clip("ab", 3), "ab");
        assert_eq!(clip("ééé", 2), "éé…");
    }

    fn entries(keys: &[&str]) -> Vec<KeyEntry> {
        let mut v: Vec<_> = keys
            .iter()
            .map(|k| KeyEntry {
                key: k.as_bytes().to_vec(),
                kind: KeyKind::String,
                ttl_ms: None,
                memory: None,
            })
            .collect();
        v.sort_by(|a, b| a.key.cmp(&b.key));
        v
    }

    /// Rows as `depth:name` (folders end with `/` and their key count).
    fn shape(rows: &[KeyRow]) -> Vec<String> {
        rows.iter()
            .map(|r| match r {
                KeyRow::Folder {
                    name, depth, keys, ..
                } => format!("{depth}:{name}/{keys}"),
                KeyRow::Key { name, depth, .. } => format!("{depth}:{name}"),
            })
            .collect()
    }

    #[test]
    fn tree_groups_by_delimiter() {
        let keys = entries(&["user:1", "user:2", "user:admin:9", "zebra", "a", "order:7"]);
        let closed = tree_rows(&keys, b":", &HashSet::new());
        assert_eq!(shape(&closed), ["0:order/1", "0:user/3", "0:a", "0:zebra"]);

        let open: HashSet<Vec<u8>> = [b"user:".to_vec()].into();
        let rows = tree_rows(&keys, b":", &open);
        assert_eq!(
            shape(&rows),
            [
                "0:order/1",
                "0:user/3",
                "1:admin/1",
                "1:1",
                "1:2",
                "0:a",
                "0:zebra"
            ]
        );
        // Key rows point back at their entry.
        let KeyRow::Key { ix, .. } = &rows[3] else {
            panic!()
        };
        assert_eq!(keys[*ix].key, b"user:1");

        let all = folder_prefixes(&keys, b":");
        assert_eq!(all.len(), 3);
        let rows = tree_rows(&keys, b":", &all);
        assert_eq!(rows.len(), 9);
        assert_eq!(shape(&rows)[4], "2:9");
        assert_eq!(list_rows(&keys).len(), keys.len());
    }

    #[test]
    fn tree_takes_other_delimiters() {
        let keys = entries(&["a::b::c", "a::b::d", "a::e", "x:y", "z"]);
        let all = folder_prefixes(&keys, b"::");
        assert_eq!(all.len(), 2);
        assert!(all.contains(b"a::b::".as_slice()));
        let rows = tree_rows(&keys, b"::", &all);
        assert_eq!(
            shape(&rows),
            ["0:a/3", "1:b/2", "2:c", "2:d", "1:e", "0:x:y", "0:z"]
        );
        let slash = tree_rows(&entries(&["p/q", "p/r", "s"]), b"/", &HashSet::new());
        assert_eq!(shape(&slash), ["0:p/2", "0:s"]);
    }

    #[test]
    fn console_history_recalls_like_a_shell() {
        let mut h = ConsoleHistory::default();
        assert_eq!(h.prev("typed"), None);
        h.push("GET a");
        h.push("GET b");
        h.push("GET b");
        assert_eq!(h.prev("draft"), Some("GET b"));
        assert_eq!(h.prev(""), Some("GET a"));
        assert_eq!(h.prev(""), Some("GET a"));
        assert_eq!(h.next(), Some("GET b"));
        assert_eq!(h.next(), Some("draft"));
        assert_eq!(h.next(), None);
        // Stored history (newest first) goes ahead of this session's lines.
        h.load_older(["GET a".to_owned(), "PING".to_owned(), "PING".to_owned()]);
        assert_eq!(h.lines, ["PING", "GET a", "GET b"]);
        assert!(recallable("SET k \"v w\""));
        assert!(!recallable("AUTH default secret"));
        assert!(!recallable(""));
    }

    #[test]
    fn history_entries_skip_agents_and_secrets() {
        let entry = |sql: &str, tags: &[&str]| HistoryEntry {
            id: 0,
            connection_id: None,
            connection_name: "cache".into(),
            sql: sql.into(),
            started_at: 0,
            duration_ms: 0,
            rows: None,
            affected: None,
            status: switchyard_core::store::HistoryStatus::Ok,
            error: None,
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
            has_plan: false,
        };
        let lines = history_lines(&[
            entry("HGETALL user:1", &[]),
            entry("GET x", &["agent", "agent:codex"]),
            entry("AUTH default ***", &[]),
        ]);
        assert_eq!(lines, ["HGETALL user:1"]);
    }

    #[test]
    fn short_ttls() {
        assert_eq!(short_ttl(None), "—");
        assert_eq!(short_ttl(Some(42_000)), "42s");
        assert_eq!(short_ttl(Some(125_000)), "2m");
        assert_eq!(short_ttl(Some(7_200_000)), "2h");
        assert_eq!(short_ttl(Some(90_000_000)), "1d");
    }

    #[test]
    fn row_viewer_shows_full_values() {
        let long = "x".repeat(CELL_CHARS * 2);
        let h = KeyValue::Hash(vec![(b"f".to_vec(), long.clone().into_bytes())]);
        assert_eq!(row_value(&h, 0), Some(long));
        assert_eq!(row_title(&h, 0), Some(("FIELD".into(), "f".into())));
        assert_eq!(row_value(&h, 1), None);
        let st = KeyValue::Stream(vec![StreamEntry {
            id: "1-0".into(),
            fields: vec![(b"a".to_vec(), b"1".to_vec())],
        }]);
        assert_eq!(row_value(&st, 0).as_deref(), Some("{\n  \"a\": \"1\"\n}"));
    }

    #[test]
    fn console_reads_like_redis_cli() {
        let lines = [
            ConsoleLine {
                line: "GET a".into(),
                text: "\"1\"".into(),
                error: false,
                ms: Some(2),
            },
            ConsoleLine {
                line: "NOPE".into(),
                text: "unknown command".into(),
                error: true,
                ms: None,
            },
        ];
        assert_eq!(
            console_text(&lines, "db0>"),
            "db0> GET a    (2 ms)\n\"1\"\n\ndb0> NOPE\n(error) unknown command"
        );
    }

    #[test]
    fn json_detection() {
        assert!(looks_like_json(r#"{"a":1}"#));
        assert!(!looks_like_json("42"));
        assert!(!looks_like_json("{nope"));
        assert_eq!(pretty_json("[1]"), "[\n  1\n]");
    }
}
