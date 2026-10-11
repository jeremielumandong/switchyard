//! Key / value tool for cloud connections: Azure App Configuration (labels, locks, feature
//! flags, Key Vault references), Azure Key Vault secrets, AWS Secrets Manager, AWS
//! Parameter Store and Cloudflare Workers KV. A filtered list on the left (virtualized,
//! paged, flat or grouped by key prefix), the selected item on the right with its value
//! in an editor, and Save / Delete / Lock pinned underneath.
//!
//! Beyond one item at a time: earlier versions with restore, two labels compared side by
//! side with copying across, import and export (JSON, `.env`, kvset), bulk delete and copy
//! to a label, a form for feature flag filters, and Key Vault's deleted secrets.
//!
//! The tab owns one cloud session ([`Command::CloudOpen`]); every read and write goes
//! through the core on the runtime. Secret values are fetched only when asked for and
//! stay masked until revealed; deletes (and every change on Production) ask first.
//! Read-only connections hide the edit controls; the core refuses writes too.

mod compare;
mod detail;
mod files;
mod flags;
mod history;
mod list;
mod tree;

use std::collections::HashSet;

use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, PathPromptOptions, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px,
    relative, uniform_list,
};
use serde_json::json;
use switchyard_core::cloud::appconfig::{
    FEATURE_FLAG_CONTENT_TYPE, FEATURE_FLAG_PREFIX, KEY_VAULT_REF_CONTENT_TYPE, format_tags,
    is_feature_flag, is_key_vault_ref, key_vault_ref, key_vault_ref_uri, new_flag, parse_flag,
    parse_tags, update_flag,
};
use switchyard_core::cloud::keyvault::parse_secret_uri;
use switchyard_core::cloud::kv_file::{self, KvFormat};
use switchyard_core::cloud::{KvItem, KvPage, KvQuery, KvWrite, display_ms, parse_utc_ms};
use switchyard_core::store::{CloudConnection, CloudService};
use switchyard_core::{
    CloudEdit, CloudInfo, Command, FsRef, RequestId, RuntimeHandle, SessionId, TextFile,
};

use crate::app_state::next_id;
use crate::appearance::{rpx, ts};
use crate::theme::{MONO, Palette, palette};
use crate::ui::{self, Kind};
use crate::workspace::{Tab, Workspace};

use compare::Compare;
use flags::FlagForm;
use tree::Row;

/// Row height of the item list (two lines: name, then value and date).
const ROW_H: f32 = 40.;
/// Item list width.
const LIST_W: f32 = 400.;
/// Indent per folder level in the grouped list.
const INDENT: f32 = 14.;

/// What the list shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    /// Settings, secrets, parameters or keys.
    Items,
    /// Feature flags (App Configuration).
    Flags,
    /// Deleted secrets that can be recovered (Key Vault).
    Deleted,
}

/// Flat list or grouped by key prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    Flat,
    Tree,
}

/// What the detail pane edits.
#[derive(Clone, Debug, PartialEq)]
enum Detail {
    None,
    /// A listed item (index into `items`).
    Item(usize),
    /// A new item.
    New,
}

/// The selected item's value, or its earlier versions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DetailTab {
    Value,
    History,
}

/// A change waiting for the user to confirm it.
#[derive(Clone, Debug, PartialEq)]
enum Confirm {
    Delete,
    Save,
    /// Any other change: what it does, the button's text, and the change.
    Edit {
        prompt: String,
        button: &'static str,
        edit: Box<CloudEdit>,
    },
}

/// Earlier versions of the selected item.
#[derive(Default)]
struct History {
    request: Option<RequestId>,
    items: Vec<KvItem>,
    error: Option<String>,
    /// The version shown (index into `items`).
    chosen: Option<usize>,
    /// Fetching a version's value (Key Vault lists versions without values).
    value_request: Option<RequestId>,
    /// A secret version's value is shown.
    revealed: bool,
}

/// The secret a Key Vault reference points at.
#[derive(Default)]
struct Resolved {
    request: Option<RequestId>,
    value: Option<String>,
    error: Option<String>,
    /// Shown in clear (else masked).
    shown: bool,
    /// Copy it when it arrives.
    copy: bool,
}

/// Settings read from a file, waiting for the user to import them.
struct ImportPlan {
    /// File name.
    source: String,
    format: KvFormat,
    writes: Vec<KvWrite>,
}

/// The tab.
pub struct CloudTab {
    core: RuntimeHandle,
    /// The connection.
    pub connection: CloudConnection,
    session: SessionId,
    info: Option<CloudInfo>,
    open_error: Option<String>,
    scope: Option<String>,
    view: View,
    layout: Layout,
    /// Open folders of the grouped list (their full prefix).
    expanded: HashSet<String>,
    rows: Vec<Row>,
    filter: Entity<InputState>,
    label_filter: Entity<InputState>,
    labels: Vec<Option<String>>,
    items: Vec<KvItem>,
    next: Option<String>,
    list_request: Option<RequestId>,
    list_error: Option<String>,
    /// Ticked items (indexes into `items`).
    picked: HashSet<usize>,
    /// The "copy to label" field of the bulk bar is open.
    bulk_copy: bool,
    bulk_label: Entity<InputState>,
    detail: Detail,
    tab: DetailTab,
    /// The selected item's value is loaded into the editor.
    loaded: bool,
    /// A loaded secret value is shown in clear.
    revealed: bool,
    /// Copy the value once it is fetched.
    copy_when_loaded: bool,
    get_request: Option<RequestId>,
    edit_request: Option<RequestId>,
    confirm: Option<Confirm>,
    history: History,
    resolved: Resolved,
    key: Entity<InputState>,
    label: Entity<InputState>,
    content_type: Entity<InputState>,
    tags: Entity<InputState>,
    description: Entity<InputState>,
    not_before: Entity<InputState>,
    expires: Entity<InputState>,
    value: Entity<EditorState>,
    flag_form: FlagForm,
    /// The flag is edited as JSON instead of the form.
    flag_json: bool,
    enabled: bool,
    kind: Option<&'static str>,
    compare: Option<Compare>,
    import: Option<ImportPlan>,
    import_request: Option<RequestId>,
    import_label: Entity<InputState>,
    /// Export once every page is loaded.
    export_pending: Option<KvFormat>,
    /// Select this item (key, label) again once the list reloads after a change.
    reselect: Option<(String, Option<String>)>,
    status: Option<(bool, String)>,
    _subs: Vec<Subscription>,
}

impl CloudTab {
    /// Opens a session on `connection`.
    pub fn new(
        core: RuntimeHandle,
        connection: CloudConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = |ph: &str, window: &mut Window, cx: &mut Context<Self>| {
            let ph = ph.to_owned();
            cx.new(|cx| InputState::new(window, cx).placeholder(ph))
        };
        let filter = input("Filter by key", window, cx);
        let label_filter = input("Any label", window, cx);
        let bulk_label = input("Label (empty: no label)", window, cx);
        let import_label = input("No label", window, cx);
        let key = input("Key", window, cx);
        let label = input("No label", window, cx);
        let content_type = input("text/plain, application/json…", window, cx);
        let tags = input("name=value; name=value", window, cx);
        let description = input("Optional", window, cx);
        let not_before = input("2026-01-31 or 2026-01-31 09:00 (UTC)", window, cx);
        let expires = input("Never", window, cx);
        let value = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("text")
                .line_number(false)
                .indent_guides(false)
                .soft_wrap(true)
        });
        let subs = vec![
            cx.subscribe_in(&filter, window, |this, _, ev: &InputEvent, _, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.reload(cx);
                }
            }),
            cx.subscribe_in(&label_filter, window, |this, _, ev: &InputEvent, _, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.reload(cx);
                }
            }),
            cx.subscribe_in(&bulk_label, window, |this, _, ev: &InputEvent, _, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.copy_picked(cx);
                }
            }),
        ];
        let session = next_id();
        core.send(Command::CloudOpen {
            session,
            connection: connection.id.clone(),
        });
        let scope = connection
            .default_path
            .clone()
            .filter(|_| connection.service == CloudService::WorkersKv);
        Self {
            core,
            connection,
            session,
            info: None,
            open_error: None,
            scope,
            view: View::Items,
            layout: Layout::Tree,
            expanded: HashSet::new(),
            rows: Vec::new(),
            filter,
            label_filter,
            labels: Vec::new(),
            items: Vec::new(),
            next: None,
            list_request: None,
            list_error: None,
            picked: HashSet::new(),
            bulk_copy: false,
            bulk_label,
            detail: Detail::None,
            tab: DetailTab::Value,
            loaded: false,
            revealed: false,
            copy_when_loaded: false,
            get_request: None,
            edit_request: None,
            confirm: None,
            history: History::default(),
            resolved: Resolved::default(),
            key,
            label,
            content_type,
            tags,
            description,
            not_before,
            expires,
            value,
            flag_form: FlagForm::default(),
            flag_json: false,
            enabled: true,
            kind: None,
            compare: None,
            import: None,
            import_request: None,
            import_label,
            export_pending: None,
            reselect: None,
            status: None,
            _subs: subs,
        }
    }

    /// Whether `session` is this tab's.
    pub fn owns(&self, session: SessionId) -> bool {
        self.session == session
    }

    /// Connected (for the sidebar's live dot).
    pub fn is_open(&self) -> bool {
        self.info.is_some()
    }

    /// Closes the session.
    pub fn shutdown(&mut self) {
        self.core.send(Command::CloseSession {
            session: self.session,
        });
    }

    fn read_only(&self) -> bool {
        self.connection.read_only || self.info.as_ref().is_some_and(|i| i.read_only)
    }

    fn caps(&self) -> switchyard_core::cloud::KvCaps {
        self.info
            .as_ref()
            .map(|i| i.caps.clone())
            .unwrap_or_default()
    }

    fn secret_values(&self) -> bool {
        self.info.as_ref().is_some_and(|i| i.caps.secret_values)
    }

    /// Settings can be exported and imported as text (values come with the list and
    /// are not secrets).
    fn files(&self) -> bool {
        let caps = self.caps();
        caps.values_in_list && !caps.secret_values
    }

    /// Where keys split into folders.
    fn delimiter(&self) -> &'static str {
        match self.connection.service {
            CloudService::KeyVault => "--",
            CloudService::ParameterStore | CloudService::SecretsManager => "/",
            _ => ":",
        }
    }

    fn text(input: &Entity<InputState>, cx: &Context<Self>) -> String {
        input.read(cx).value().trim().to_owned()
    }

    fn set_input(
        input: &Entity<InputState>,
        v: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let v = v.into();
        input.update(cx, |i, cx| i.set_value(v, window, cx));
    }

    fn production(&self) -> bool {
        self.connection.environment.is_production()
    }

    /// The tab opened, or failed to.
    pub fn on_opened(&mut self, result: Result<CloudInfo, String>, cx: &mut Context<Self>) {
        match result {
            Ok(info) => {
                if info.caps.scope_label.is_some()
                    && self
                        .scope
                        .as_ref()
                        .is_none_or(|s| !info.scopes.iter().any(|(id, _)| id == s))
                {
                    self.scope = info.scopes.first().map(|(id, _)| id.clone());
                }
                let labels = info.caps.labels;
                self.info = Some(info);
                self.open_error = None;
                if labels {
                    self.core.send(Command::CloudLabels {
                        session: self.session,
                    });
                }
                self.reload(cx);
            }
            Err(e) => self.open_error = Some(e),
        }
        cx.notify();
    }

    /// Labels in use arrived.
    pub fn on_labels(
        &mut self,
        result: Result<Vec<Option<String>>, String>,
        cx: &mut Context<Self>,
    ) {
        if let Ok(l) = result {
            self.labels = l;
            cx.notify();
        }
    }

    /// Lists from the start with the current filters.
    fn reload(&mut self, cx: &mut Context<Self>) {
        self.items.clear();
        self.rows.clear();
        self.picked.clear();
        self.next = None;
        self.detail = Detail::None;
        self.confirm = None;
        self.list(None, cx);
    }

    /// The list request for the current filters.
    fn query(&self, label: Option<String>, cursor: Option<String>, cx: &Context<Self>) -> KvQuery {
        let mut key = Self::text(&self.filter, cx);
        if self.view == View::Flags {
            key = format!("{FEATURE_FLAG_PREFIX}{key}");
        }
        let labels = self.caps().labels;
        let label = match label {
            Some(l) => l,
            None if labels => label_query(&Self::text(&self.label_filter, cx)),
            None => String::new(),
        };
        KvQuery {
            scope: self.scope.clone(),
            key,
            label,
            cursor,
            deleted: self.view == View::Deleted,
        }
    }

    fn list(&mut self, cursor: Option<String>, cx: &mut Context<Self>) {
        let Some(info) = &self.info else { return };
        if info.caps.scope_label.is_some() && self.scope.is_none() {
            self.list_error = None;
            cx.notify();
            return;
        }
        let request = next_id();
        self.list_request = Some(request);
        self.list_error = None;
        let query = self.query(None, cursor, cx);
        self.core.send(Command::CloudList {
            session: self.session,
            request,
            query,
        });
        cx.notify();
    }

    /// A page of items arrived (the list's, or a side of the label comparison).
    pub fn on_items(
        &mut self,
        request: RequestId,
        result: Result<KvPage, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.list_request != Some(request) {
            self.on_compare_page(request, result, cx);
            return;
        }
        self.list_request = None;
        match result {
            Ok(page) => {
                let flags = self.view == View::Flags;
                let feature_flags = self.caps().feature_flags;
                self.items.extend(
                    page.items
                        .into_iter()
                        .filter(|i| !feature_flags || is_feature_flag(&i.key) == flags),
                );
                self.next = page.next;
                self.refresh_rows(cx);
                if let Some((key, label)) = self.reselect.take()
                    && let Some(ix) = self
                        .items
                        .iter()
                        .position(|i| i.key == key && i.label == label)
                {
                    // Open the folders it sits in.
                    let delim = self.delimiter();
                    for (pos, _) in key.match_indices(delim) {
                        if pos > 0 {
                            self.expanded.insert(key[..pos + delim.len()].to_owned());
                        }
                    }
                    self.refresh_rows(cx);
                    let status = self.status.take();
                    self.select(ix, window, cx);
                    self.status = status;
                }
                if let Some(format) = self.export_pending {
                    match self.next.clone() {
                        Some(next) => self.list(Some(next), cx),
                        None => {
                            self.export_pending = None;
                            self.export(format, cx);
                        }
                    }
                }
            }
            Err(e) => {
                self.export_pending = None;
                self.list_error = Some(e);
            }
        }
        cx.notify();
    }

    /// Rebuild the list rows (flat, or grouped by key prefix).
    fn refresh_rows(&mut self, cx: &Context<Self>) {
        let grouped = self.layout == Layout::Tree && self.view == View::Items;
        self.rows = if grouped {
            let keys: Vec<&str> = self.items.iter().map(|i| i.key.as_str()).collect();
            // While filtering, every folder is open.
            let all_open = !Self::text(&self.filter, cx).is_empty();
            tree::rows(&keys, self.delimiter(), &self.expanded, all_open)
        } else {
            (0..self.items.len())
                .map(|ix| Row::Item {
                    ix,
                    depth: 0,
                    name: String::new(),
                })
                .collect()
        };
    }

    fn toggle_folder(&mut self, path: String, cx: &mut Context<Self>) {
        if !self.expanded.remove(&path) {
            self.expanded.insert(path);
        }
        self.refresh_rows(cx);
        cx.notify();
    }

    fn set_layout(&mut self, layout: Layout, cx: &mut Context<Self>) {
        self.layout = layout;
        self.refresh_rows(cx);
        cx.notify();
    }

    fn select(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.items.get(ix).cloned() else {
            return;
        };
        self.detail = Detail::Item(ix);
        self.tab = DetailTab::Value;
        self.history = History::default();
        self.resolved = Resolved::default();
        self.confirm = None;
        self.status = None;
        self.revealed = false;
        self.copy_when_loaded = false;
        self.fill(&item, window, cx);
        let values_in_list = self.caps().values_in_list;
        self.loaded = item.value.is_some() && values_in_list;
        if !self.loaded && !self.hides_value(&item) && self.view != View::Deleted {
            self.fetch(&item, cx);
        }
        cx.notify();
    }

    /// Secret values stay hidden until "Show value".
    fn hides_value(&self, item: &KvItem) -> bool {
        self.secret_values() || item.kind.as_deref() == Some("SecureString")
    }

    fn fetch(&mut self, item: &KvItem, cx: &mut Context<Self>) {
        let request = next_id();
        self.get_request = Some(request);
        self.core.send(Command::CloudGet {
            session: self.session,
            request,
            scope: self.scope.clone(),
            key: item.key.clone(),
            label: item.label.clone(),
            version: None,
        });
        cx.notify();
    }

    /// An item's full value arrived (the selected one, or a version in History).
    pub fn on_item(
        &mut self,
        request: RequestId,
        result: Result<KvItem, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.history.value_request == Some(request) {
            self.history.value_request = None;
            match result {
                Ok(v) => {
                    if let Some(h) = self
                        .history
                        .chosen
                        .and_then(|i| self.history.items.get_mut(i))
                    {
                        h.value = v.value;
                    }
                }
                Err(e) => self.status = Some((false, e)),
            }
            cx.notify();
            return;
        }
        if self.get_request != Some(request) {
            return;
        }
        self.get_request = None;
        match result {
            Ok(mut item) => {
                if let Detail::Item(ix) = self.detail
                    && let Some(listed) = self.items.get_mut(ix)
                    && listed.key == item.key
                {
                    // Keep what only the list knows (label, lock, kind).
                    item.label = item.label.or(listed.label.clone());
                    item.kind = item.kind.or(listed.kind.clone());
                    item.locked = item.locked.or(listed.locked);
                    *listed = item.clone();
                    self.fill(&item, window, cx);
                    self.loaded = true;
                    if std::mem::take(&mut self.copy_when_loaded) {
                        self.copy_text(item.value.clone().unwrap_or_default(), cx);
                    } else {
                        self.revealed = true;
                    }
                }
            }
            Err(e) => self.status = Some((false, e)),
        }
        cx.notify();
    }

    fn copy_text(&mut self, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.status = Some((true, "Copied to the clipboard".into()));
        cx.notify();
    }

    /// Copy the selected value, fetching it first when it is a secret not loaded yet.
    fn copy_value(&mut self, cx: &mut Context<Self>) {
        if self.loaded {
            let v = self.value.read(cx).value().to_string();
            self.copy_text(v, cx);
        } else if let Some(i) = self.selected().cloned() {
            self.copy_when_loaded = true;
            self.fetch(&i, cx);
        }
    }

    fn fill(&mut self, item: &KvItem, window: &mut Window, cx: &mut Context<Self>) {
        let flag = self.view == View::Flags;
        let parsed = flag
            .then(|| parse_flag(&item.key, item.value.as_deref().unwrap_or_default()))
            .flatten();
        let key = match &parsed {
            Some(f) => f.id.clone(),
            None => item.key.clone(),
        };
        Self::set_input(&self.key, key, window, cx);
        Self::set_input(
            &self.label,
            item.label.clone().unwrap_or_default(),
            window,
            cx,
        );
        Self::set_input(
            &self.content_type,
            item.content_type.clone().unwrap_or_default(),
            window,
            cx,
        );
        Self::set_input(&self.tags, format_tags(&item.tags), window, cx);
        Self::set_input(
            &self.description,
            parsed
                .as_ref()
                .map(|f| f.description.clone())
                .or_else(|| item.description.clone())
                .unwrap_or_default(),
            window,
            cx,
        );
        let date = |ms: Option<i64>| ms.map(display_ms).unwrap_or_default();
        Self::set_input(&self.not_before, date(item.not_before_ms), window, cx);
        Self::set_input(&self.expires, date(item.expires_ms), window, cx);
        self.enabled = parsed
            .as_ref()
            .map(|f| f.enabled)
            .or(item.enabled)
            .unwrap_or(true);
        self.kind = None;
        let raw = item.value.as_deref().unwrap_or_default();
        self.flag_json = false;
        self.flag_form = if flag {
            FlagForm::load(raw, window, cx)
        } else {
            FlagForm::default()
        };
        let (text, lang) = match item.value.as_deref() {
            Some(v) if looks_like_json(v) => (pretty_json(v), "json"),
            Some(v) => (v.to_owned(), "text"),
            None => (String::new(), "text"),
        };
        self.value.update(cx, |e, cx| {
            e.set_highlighter(lang, cx);
            e.set_value(text, window, cx);
        });
    }

    fn new_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.compare = None;
        self.import = None;
        self.detail = Detail::New;
        self.tab = DetailTab::Value;
        self.confirm = None;
        self.status = None;
        self.loaded = true;
        self.revealed = true;
        self.resolved = Resolved::default();
        let template = if self.view == View::Flags {
            KvItem {
                value: Some(new_flag("", true, "").1),
                ..KvItem::default()
            }
        } else {
            KvItem::default()
        };
        self.fill(&template, window, cx);
        // A new setting gets the label being filtered on.
        let label = Self::text(&self.label_filter, cx);
        if !label.is_empty() && label != "-" && !label.contains('*') {
            Self::set_input(&self.label, label, window, cx);
        }
        self.kind = self
            .info
            .as_ref()
            .and_then(|i| i.caps.kinds.first().copied());
        cx.notify();
    }

    /// Turn the new setting into a Key Vault reference.
    fn make_reference(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        Self::set_input(&self.content_type, KEY_VAULT_REF_CONTENT_TYPE, window, cx);
        let value = pretty_json(&key_vault_ref(
            "https://<vault>.vault.azure.net/secrets/<name>",
        ));
        self.value.update(cx, |e, cx| {
            e.set_highlighter("json", cx);
            e.set_value(value, window, cx);
        });
        cx.notify();
    }

    fn selected(&self) -> Option<&KvItem> {
        match self.detail {
            Detail::Item(ix) => self.items.get(ix),
            _ => None,
        }
    }

    fn date_field(
        input: &Entity<InputState>,
        what: &str,
        cx: &Context<Self>,
    ) -> Result<Option<i64>, String> {
        let t = Self::text(input, cx);
        if t.is_empty() {
            return Ok(None);
        }
        parse_utc_ms(&t)
            .map(Some)
            .ok_or_else(|| format!("{what}: write it as 2026-01-31 or 2026-01-31 09:00 (UTC)"))
    }

    /// The write for the detail pane, or why it can't be made.
    fn write(&self, cx: &Context<Self>) -> Result<KvWrite, String> {
        let info = self.info.as_ref().ok_or("Not connected")?;
        let caps = &info.caps;
        let create = self.detail == Detail::New;
        let existing = self.selected();
        let key = Self::text(&self.key, cx);
        if key.is_empty() {
            return Err("Enter a key".into());
        }
        let opt = |s: String| Some(s).filter(|s| !s.is_empty());
        let label = if caps.labels {
            opt(Self::text(&self.label, cx))
        } else {
            None
        };
        let tags = if caps.tags {
            parse_tags(&Self::text(&self.tags, cx)).map_err(|e| e.to_string())?
        } else {
            Default::default()
        };
        let mut value = self.value.read(cx).value().to_string();
        let mut content_type = if caps.content_type {
            opt(Self::text(&self.content_type, cx))
        } else {
            None
        };
        if self.view == View::Items
            && is_key_vault_ref(content_type.as_deref())
            && key_vault_ref_uri(&value).is_none_or(|u| parse_secret_uri(&u).is_err())
        {
            return Err(
                "A Key Vault reference's value is {\"uri\": \"https://<vault>.vault.azure.net/secrets/<name>\"}"
                    .into(),
            );
        }
        let mut key_out = key.clone();
        let description = opt(Self::text(&self.description, cx));
        if self.view == View::Flags {
            if create {
                let (k, v) = new_flag(&key, self.enabled, description.as_deref().unwrap_or(""));
                key_out = k;
                value = self.flag_value(&v, cx)?;
            } else {
                value = self.flag_value(&value, cx)?;
                key_out = existing.map_or(key_out, |i| i.key.clone());
            }
            value = update_flag(
                &value,
                self.enabled,
                Some(description.as_deref().unwrap_or("")),
            )
            .map_err(|e| e.to_string())?;
            content_type = Some(FEATURE_FLAG_CONTENT_TYPE.to_owned());
        } else if !create && let Some(i) = existing {
            // Keys are renamed by creating a new item; the field is read-only here.
            key_out = i.key.clone();
        }
        let (not_before_ms, expires_ms) = if caps.dates {
            (
                Self::date_field(&self.not_before, "Activation", cx)?,
                Self::date_field(&self.expires, "Expiry", cx)?,
            )
        } else {
            (None, None)
        };
        if let (Some(a), Some(b)) = (not_before_ms, expires_ms)
            && b <= a
        {
            return Err("Expiry must come after activation".into());
        }
        Ok(KvWrite {
            scope: self.scope.clone(),
            key: key_out,
            label,
            value,
            content_type,
            tags,
            enabled: caps.enabled.then_some(self.enabled),
            kind: if create {
                self.kind.map(str::to_owned)
            } else {
                existing.and_then(|i| i.kind.clone())
            },
            description: if self.view == View::Flags {
                None
            } else {
                description
            },
            create,
            etag: if create {
                None
            } else {
                existing.and_then(|i| i.etag.clone())
            },
            not_before_ms,
            expires_ms,
        })
    }

    fn save(&mut self, confirmed: bool, cx: &mut Context<Self>) {
        if self.read_only() || self.edit_request.is_some() {
            return;
        }
        if !self.loaded {
            self.status = Some((false, "Show the value before saving".into()));
            cx.notify();
            return;
        }
        let w = match self.write(cx) {
            Ok(w) => w,
            Err(e) => {
                self.status = Some((false, e));
                cx.notify();
                return;
            }
        };
        if self.production() && !confirmed {
            self.confirm = Some(Confirm::Save);
            cx.notify();
            return;
        }
        self.confirm = None;
        self.send_edit(CloudEdit::Put(w), cx);
    }

    fn delete(&mut self, confirmed: bool, cx: &mut Context<Self>) {
        let Some(item) = self.selected().cloned() else {
            return;
        };
        if !confirmed {
            self.confirm = Some(Confirm::Delete);
            cx.notify();
            return;
        }
        self.confirm = None;
        self.send_edit(
            CloudEdit::Delete {
                scope: self.scope.clone(),
                key: item.key,
                label: item.label,
            },
            cx,
        );
    }

    /// Ask before `edit`; with `always` false, only on Production.
    fn ask(
        &mut self,
        prompt: String,
        button: &'static str,
        edit: CloudEdit,
        always: bool,
        cx: &mut Context<Self>,
    ) {
        if self.read_only() || self.edit_request.is_some() {
            return;
        }
        if always || self.production() {
            self.confirm = Some(Confirm::Edit {
                prompt,
                button,
                edit: Box::new(edit),
            });
            cx.notify();
        } else {
            self.send_edit(edit, cx);
        }
    }

    fn toggle_lock(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.selected().cloned() else {
            return;
        };
        self.send_edit(
            CloudEdit::Lock {
                key: item.key,
                label: item.label,
                locked: !item.locked.unwrap_or(false),
            },
            cx,
        );
    }

    /// Turns a feature flag on or off from the list.
    fn toggle_flag(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.read_only() || self.edit_request.is_some() {
            return;
        }
        let Some(item) = self.items.get(ix).cloned() else {
            return;
        };
        let value = item.value.clone().unwrap_or_default();
        let Some(flag) = parse_flag(&item.key, &value) else {
            self.status = Some((false, "This flag's value is not valid JSON".into()));
            cx.notify();
            return;
        };
        match update_flag(&value, !flag.enabled, None) {
            Ok(v) => self.ask(
                format!(
                    "Turn {} {} on Production?",
                    flag.id,
                    if flag.enabled { "off" } else { "on" }
                ),
                if flag.enabled { "Turn off" } else { "Turn on" },
                CloudEdit::Put(KvWrite {
                    key: item.key,
                    label: item.label,
                    value: v,
                    content_type: Some(FEATURE_FLAG_CONTENT_TYPE.to_owned()),
                    tags: item.tags,
                    etag: item.etag,
                    ..KvWrite::default()
                }),
                false,
                cx,
            ),
            Err(e) => self.status = Some((false, e.to_string())),
        }
        cx.notify();
    }

    fn send_edit(&mut self, edit: CloudEdit, cx: &mut Context<Self>) {
        let request = next_id();
        self.edit_request = Some(request);
        self.status = None;
        self.confirm = None;
        self.core.send(Command::CloudEdit {
            session: self.session,
            request,
            edit,
        });
        cx.notify();
    }

    /// A change finished: the list is reloaded.
    pub fn on_edited(
        &mut self,
        request: RequestId,
        result: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        if self.edit_request != Some(request) {
            return;
        }
        self.edit_request = None;
        // Even a partly failed batch changed something.
        self.reselect = self.selected().map(|i| (i.key.clone(), i.label.clone()));
        let keep = Some(match result {
            Ok(m) => (true, m),
            Err(e) => (false, e),
        });
        self.import = None;
        self.bulk_copy = false;
        if self.caps().labels {
            self.core.send(Command::CloudLabels {
                session: self.session,
            });
        }
        if let Some(c) = self.compare.as_mut() {
            c.picked.clear();
            let (l, r) = (c.left.label.clone(), c.right.label.clone());
            self.start_compare(l, r, cx);
        }
        self.reload(cx);
        self.status = keep;
        cx.notify();
    }

    fn set_view(&mut self, view: View, cx: &mut Context<Self>) {
        if self.view != view {
            self.view = view;
            self.compare = None;
            self.import = None;
            self.reload(cx);
        }
    }

    fn set_scope(&mut self, scope: String, cx: &mut Context<Self>) {
        self.scope = Some(scope);
        self.reload(cx);
    }

    fn set_label_filter(
        &mut self,
        label: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = match label {
            Some(l) => l,
            None => "-".into(),
        };
        Self::set_input(&self.label_filter, text, window, cx);
        self.reload(cx);
    }

    // Rendering

    fn render_header(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let info = self.info.as_ref();
        let caps = self.caps();
        let scope_picker = caps.scope_label.map(|label| {
            let scopes = info.map(|i| i.scopes.clone()).unwrap_or_default();
            let chosen = self.scope.clone();
            let current = scopes
                .iter()
                .find(|(id, _)| Some(id) == chosen.as_ref())
                .map_or_else(
                    || format!("Choose a {}", label.to_lowercase()),
                    |(_, t)| t.clone(),
                );
            let this = cx.entity().downgrade();
            Button::new("cl-scope")
                .outline()
                .small()
                .label(current)
                .dropdown_menu(move |mut menu, _, _| {
                    for (id, title) in &scopes {
                        let this = this.clone();
                        let id = id.clone();
                        let checked = chosen.as_ref() == Some(&id);
                        menu =
                            menu.item(PopupMenuItem::new(title.clone()).checked(checked).on_click(
                                move |_, _, cx| {
                                    let id = id.clone();
                                    let _ = this.update(cx, |t, cx| t.set_scope(id, cx));
                                },
                            ));
                    }
                    menu
                })
        });
        let this = cx.entity().downgrade();
        let view_option = |label: &'static str, v: View| {
            let this = this.clone();
            let on: ui::OnClick = Box::new(move |_, _, cx| {
                let _ = this.update(cx, |t, cx| t.set_view(v, cx));
            });
            (SharedString::from(label), self.view == v, on)
        };
        let mut views = Vec::new();
        if caps.feature_flags {
            views.push(view_option("Settings", View::Items));
            views.push(view_option("Feature flags", View::Flags));
        } else if caps.recoverable {
            views.push(view_option("Secrets", View::Items));
            views.push(view_option("Deleted", View::Deleted));
        }
        let new_label = if self.view == View::Flags {
            "New flag"
        } else {
            match self.connection.service {
                CloudService::AppConfig => "New setting",
                CloudService::KeyVault | CloudService::SecretsManager => "New secret",
                CloudService::ParameterStore => "New parameter",
                _ => "New key",
            }
        };
        let files = self.files() && info.is_some() && self.view != View::Deleted;
        let read_only = self.read_only();
        let files_menu = files.then(|| {
            let this = cx.entity().downgrade();
            Button::new("cl-files")
                .outline()
                .small()
                .label("Import / export")
                .dropdown_menu(move |mut menu, _, _| {
                    if !read_only {
                        let t = this.clone();
                        menu = menu
                            .item(PopupMenuItem::new("Import from a file…").on_click(
                                move |_, _, cx| {
                                    let _ = t.update(cx, |t, cx| t.pick_import(cx));
                                },
                            ))
                            .separator();
                    }
                    for f in KvFormat::ALL {
                        let t = this.clone();
                        menu = menu.item(
                            PopupMenuItem::new(format!("Export as {}", f.title())).on_click(
                                move |_, _, cx| {
                                    let _ = t.update(cx, |t, cx| t.export(f, cx));
                                },
                            ),
                        );
                    }
                    menu
                })
        });
        let compare_open = self.compare.is_some();
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(rpx(8.))
            .px(rpx(12.))
            .py(rpx(8.))
            .border_b_1()
            .border_color(p.bd)
            .child(ui::monogram(self.connection.service.badge(), 34., p))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(ts::UI)
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(self.connection.name.clone()),
                    )
                    .child(
                        div()
                            .text_size(ts::SMALL)
                            .text_color(p.fg3)
                            .child(self.connection.service.display_name()),
                    ),
            )
            .child(ui::env_badge(self.connection.environment, p))
            .when(read_only, |d| {
                d.child(
                    div()
                        .text_size(ts::SMALL)
                        .text_color(p.fg3)
                        .child("Read-only"),
                )
            })
            .child(div().flex_1())
            .children(scope_picker)
            .when(!views.is_empty(), |d| {
                d.child(ui::segmented("cl-view", views, 22., p))
            })
            .when(caps.labels && self.view == View::Items, |d| {
                d.child(
                    ui::button(
                        "cl-compare",
                        if compare_open {
                            "Close comparison"
                        } else {
                            "Compare labels"
                        },
                        Kind::Secondary,
                        p,
                    )
                    .on_click(cx.listener(|t, _, _, cx| {
                        if t.compare.is_some() {
                            t.compare = None;
                            cx.notify();
                        } else {
                            t.open_compare(cx);
                        }
                    })),
                )
            })
            .children(files_menu)
            .child(
                ui::button("cl-refresh", "Refresh", Kind::Secondary, p).on_click(cx.listener(
                    |t, _, _, cx| {
                        if let Some(c) = &t.compare {
                            let (l, r) = (c.left.label.clone(), c.right.label.clone());
                            t.start_compare(l, r, cx);
                        }
                        t.reload(cx)
                    },
                )),
            )
            .when(
                !read_only && info.is_some() && self.view != View::Deleted,
                |d| {
                    d.child(
                        ui::button("cl-new", new_label, Kind::Primary, p)
                            .on_click(cx.listener(|t, _, w, cx| t.new_item(w, cx))),
                    )
                },
            )
            .into_any_element()
    }

    /// The strip asking to confirm a bulk or Production change.
    fn render_confirm(&self, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let Some(Confirm::Edit { prompt, button, .. }) = &self.confirm else {
            return None;
        };
        let kind = if matches!(*button, "Delete" | "Purge") || self.production() {
            Kind::Destructive
        } else {
            Kind::Primary
        };
        Some(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(rpx(8.))
                .px(rpx(12.))
                .py(rpx(6.))
                .bg(p.elev)
                .border_b_1()
                .border_color(p.bd)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(ts::BODY)
                        .child(if self.production() {
                            format!("Production: {prompt}")
                        } else {
                            prompt.clone()
                        }),
                )
                .child(
                    ui::button("cl-confirm-yes", *button, kind, p).on_click(cx.listener(
                        |t, _, _, cx| {
                            if let Some(Confirm::Edit { edit, .. }) = t.confirm.take() {
                                t.send_edit(*edit, cx);
                            }
                        },
                    )),
                )
                .child(
                    ui::button("cl-confirm-no", "Cancel", Kind::Ghost, p).on_click(cx.listener(
                        |t, _, _, cx| {
                            t.confirm = None;
                            cx.notify();
                        },
                    )),
                )
                .into_any_element(),
        )
    }

    fn field(label: &'static str, body: AnyElement, p: &Palette) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(rpx(4.))
            .min_w_0()
            .child(
                div()
                    .text_size(ts::LABEL)
                    .text_color(p.fg2)
                    .font_weight(FontWeight::MEDIUM)
                    .child(label),
            )
            .child(body)
            .into_any_element()
    }

    fn boxed(input: &Entity<InputState>, mono: bool, disabled: bool, p: &Palette) -> AnyElement {
        div()
            .h(rpx(28.))
            .flex()
            .items_center()
            .px(rpx(8.))
            .border_1()
            .border_color(p.bd2)
            .rounded(px(6.))
            .bg(p.bg)
            .when(mono, |d| d.font_family(MONO))
            .when(disabled, |d| d.opacity(0.6))
            .child(
                Input::new(input)
                    .appearance(false)
                    .disabled(disabled)
                    .text_size(ts::BODY),
            )
            .into_any_element()
    }

    fn panel(p: &Palette) -> gpui_kit::Div {
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(rpx(8.))
            .p(rpx(10.))
            .border_1()
            .border_color(p.bd)
            .rounded(px(6.))
            .bg(p.bg)
    }
}

fn chip(text: String, p: &Palette) -> gpui_kit::Div {
    div()
        .flex_none()
        .max_w(rpx(120.))
        .truncate()
        .px(rpx(6.))
        .rounded(px(4.))
        .bg(p.surface)
        .border_1()
        .border_color(p.bd)
        .text_size(ts::SMALL)
        .text_color(p.fg2)
        .child(text)
}

/// The label filter as App Configuration's list expects it: empty for any label, `\0`
/// for none (`-` in the filter field).
fn label_query(text: &str) -> String {
    match text.trim() {
        "" => String::new(),
        "-" | "(none)" | "(No label)" => "\0".into(),
        l => l.to_owned(),
    }
}

/// A label for people: `(No label)` for the null label.
fn label_text(label: Option<&str>) -> String {
    match label {
        Some(l) if !l.is_empty() => l.to_owned(),
        _ => "(No label)".into(),
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// A JSON object or array (shown pretty and highlighted).
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

impl Render for CloudTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let header = self.render_header(&p, cx);
        let confirm = self.render_confirm(&p, cx);
        let status = self.status.as_ref().map(|(ok, m)| {
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(rpx(8.))
                .px(rpx(12.))
                .py(rpx(4.))
                .border_b_1()
                .border_color(p.line)
                .text_size(ts::BODY)
                .text_color(if *ok { p.dev } else { p.prod })
                .child(div().flex_1().min_w_0().child(m.clone()))
                .child(
                    ui::button("cl-status-x", "Dismiss", Kind::Ghost, &p).on_click(cx.listener(
                        |t, _, _, cx| {
                            t.status = None;
                            cx.notify();
                        },
                    )),
                )
        });
        let main = match &self.open_error {
            Some(e) => div()
                .flex_1()
                .p(rpx(16.))
                .flex()
                .flex_col()
                .items_start()
                .gap(rpx(10.))
                .text_size(ts::BODY)
                .child(div().text_color(p.prod).child(e.clone()))
                .child(
                    ui::button("cl-reconnect", "Try again", Kind::Secondary, &p).on_click(
                        cx.listener(|t, _, _, cx| {
                            t.open_error = None;
                            t.core.send(Command::CloudOpen {
                                session: t.session,
                                connection: t.connection.id.clone(),
                            });
                            cx.notify();
                        }),
                    ),
                )
                .into_any_element(),
            None if self.info.is_none() => div()
                .flex_1()
                .p(rpx(16.))
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .child("Connecting… (a Microsoft sign-in may open in your browser)")
                .into_any_element(),
            None => {
                let right = if let Some(c) = &self.compare {
                    self.render_compare(c, &p, cx)
                } else if let Some(plan) = &self.import {
                    self.render_import(plan, &p, cx)
                } else {
                    self.render_detail(&p, window, cx)
                };
                let full_width = self.compare.is_some();
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .flex()
                    .when(!full_width, |d| d.child(self.render_list(&p, window, cx)))
                    .child(right)
                    .into_any_element()
            }
        };
        div()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .bg(p.surface)
            .child(header)
            .children(confirm)
            .children(status)
            .child(main)
    }
}

impl Workspace {
    /// Open (or focus) the key / value tool of a cloud connection.
    pub(crate) fn open_cloud(
        &mut self,
        connection: CloudConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ix) = self
            .tabs
            .iter()
            .position(|t| matches!(t, Tab::Cloud(c) if c.read(cx).connection.id == connection.id))
        {
            self.activate(ix, cx);
            return;
        }
        let core = self.core.clone();
        let tab = cx.new(|cx| CloudTab::new(core, connection, window, cx));
        self.tabs.push(Tab::Cloud(tab));
        self.activate(self.tabs.len() - 1, cx);
    }

    /// Route the `Cloud*` events to their tab.
    pub(crate) fn on_cloud_event(
        &mut self,
        ev: switchyard_core::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use switchyard_core::Event;
        let session = match &ev {
            Event::CloudOpened { session, .. }
            | Event::CloudItems { session, .. }
            | Event::CloudItem { session, .. }
            | Event::CloudEdited { session, .. }
            | Event::CloudRevisions { session, .. }
            | Event::CloudLabels { session, .. }
            | Event::CloudResolved { session, .. } => *session,
            _ => return,
        };
        let Some(tab) = self.tabs.iter().find_map(|t| match t {
            Tab::Cloud(c) if c.read(cx).owns(session) => Some(c.clone()),
            _ => None,
        }) else {
            if let Event::CloudOpened { result: Ok(_), .. } = ev {
                // The tab closed while connecting.
                self.core.send(Command::CloseSession { session });
            }
            return;
        };
        tab.update(cx, |t, cx| match ev {
            Event::CloudOpened { result, .. } => t.on_opened(result, cx),
            Event::CloudItems {
                request, result, ..
            } => t.on_items(request, result, window, cx),
            Event::CloudItem {
                request, result, ..
            } => t.on_item(request, result, window, cx),
            Event::CloudEdited {
                request, result, ..
            } => t.on_edited(request, result, cx),
            Event::CloudRevisions {
                request, result, ..
            } => t.on_revisions(request, result, cx),
            Event::CloudLabels { result, .. } => t.on_labels(result, cx),
            Event::CloudResolved {
                request, result, ..
            } => t.on_resolved(request, result, cx),
            _ => {}
        });
    }

    /// A file read for a cloud tab's import.
    pub(crate) fn on_cloud_text_file(
        &mut self,
        request: RequestId,
        result: &Result<TextFile, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for t in &self.tabs {
            if let Tab::Cloud(c) = t {
                c.update(cx, |c, cx| c.on_text_file(request, result, window, cx));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_values_are_recognized() {
        assert!(looks_like_json(r#"{"a":1}"#));
        assert!(looks_like_json("[1, 2]"));
        assert!(!looks_like_json("{not json"));
        assert!(!looks_like_json("plain"));
        assert_eq!(pretty_json(r#"{"a":1}"#), "{\n  \"a\": 1\n}");
    }
}
