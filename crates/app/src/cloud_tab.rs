//! Key / value tool for cloud connections: Azure App Configuration (labels, locks, feature
//! flags), Azure Key Vault secrets, AWS Secrets Manager, AWS Parameter Store and
//! Cloudflare Workers KV. A filtered list on the left (virtualized, paged), the selected
//! item on the right with its value in an editor, and Save / Delete / Lock underneath.
//!
//! The tab owns one cloud session ([`Command::CloudOpen`]); every read and write goes
//! through the core on the runtime. Secret values are fetched only when asked for, and
//! deletes (and every save on Production) ask first. Read-only connections hide the edit
//! controls; the core refuses writes too.

use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, px, relative, uniform_list,
};
use switchyard_core::cloud::appconfig::{
    FEATURE_FLAG_CONTENT_TYPE, FEATURE_FLAG_PREFIX, format_tags, is_feature_flag, new_flag,
    parse_flag, parse_tags, update_flag,
};
use switchyard_core::cloud::{KvItem, KvPage, KvQuery, KvWrite, display_ms};
use switchyard_core::store::{CloudConnection, CloudService};
use switchyard_core::{CloudEdit, CloudInfo, Command, RequestId, RuntimeHandle, SessionId};

use crate::app_state::next_id;
use crate::appearance::{rpx, ts};
use crate::theme::{MONO, Palette, palette};
use crate::ui::{self, Kind};
use crate::workspace::{Tab, Workspace};

/// Row height of the item list.
const ROW_H: f32 = 30.;
/// Item list width.
const LIST_W: f32 = 380.;

/// Settings or feature flags (App Configuration).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    Items,
    Flags,
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

/// A change waiting for the user to confirm it.
#[derive(Clone, Debug, PartialEq)]
enum Confirm {
    Delete,
    Save,
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
    filter: Entity<InputState>,
    label_filter: Entity<InputState>,
    items: Vec<KvItem>,
    next: Option<String>,
    list_request: Option<RequestId>,
    list_error: Option<String>,
    detail: Detail,
    /// The selected item's value is loaded into the editor.
    loaded: bool,
    get_request: Option<RequestId>,
    edit_request: Option<RequestId>,
    confirm: Option<Confirm>,
    key: Entity<InputState>,
    label: Entity<InputState>,
    content_type: Entity<InputState>,
    tags: Entity<InputState>,
    description: Entity<InputState>,
    value: Entity<EditorState>,
    enabled: bool,
    kind: Option<&'static str>,
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
        let key = input("Key", window, cx);
        let label = input("No label", window, cx);
        let content_type = input("text/plain, application/json…", window, cx);
        let tags = input("env=prod; team=web", window, cx);
        let description = input("Optional", window, cx);
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
            filter,
            label_filter,
            items: Vec::new(),
            next: None,
            list_request: None,
            list_error: None,
            detail: Detail::None,
            loaded: false,
            get_request: None,
            edit_request: None,
            confirm: None,
            key,
            label,
            content_type,
            tags,
            description,
            value,
            enabled: true,
            kind: None,
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

    fn secret_values(&self) -> bool {
        self.info.as_ref().is_some_and(|i| i.caps.secret_values)
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
                self.info = Some(info);
                self.open_error = None;
                self.reload(cx);
            }
            Err(e) => self.open_error = Some(e),
        }
        cx.notify();
    }

    /// Lists from the start with the current filters.
    fn reload(&mut self, cx: &mut Context<Self>) {
        self.items.clear();
        self.next = None;
        self.detail = Detail::None;
        self.confirm = None;
        self.list(None, cx);
    }

    fn list(&mut self, cursor: Option<String>, cx: &mut Context<Self>) {
        let Some(info) = &self.info else { return };
        if info.caps.scope_label.is_some() && self.scope.is_none() {
            self.list_error = None;
            cx.notify();
            return;
        }
        let mut key = Self::text(&self.filter, cx);
        if self.view == View::Flags {
            key = format!("{FEATURE_FLAG_PREFIX}{key}");
        }
        let label = if info.caps.labels {
            match Self::text(&self.label_filter, cx).as_str() {
                "" => String::new(),
                // `(No label)` in the filter lists settings without one.
                "-" | "(none)" | "(No label)" => "\0".into(),
                l => l.to_owned(),
            }
        } else {
            String::new()
        };
        let request = next_id();
        self.list_request = Some(request);
        self.list_error = None;
        self.core.send(Command::CloudList {
            session: self.session,
            request,
            query: KvQuery {
                scope: self.scope.clone(),
                key,
                label,
                cursor,
            },
        });
        cx.notify();
    }

    /// A page of items arrived.
    pub fn on_items(
        &mut self,
        request: RequestId,
        result: Result<KvPage, String>,
        cx: &mut Context<Self>,
    ) {
        if self.list_request != Some(request) {
            return;
        }
        self.list_request = None;
        match result {
            Ok(page) => {
                let flags = self.view == View::Flags;
                let feature_flags = self.info.as_ref().is_some_and(|i| i.caps.feature_flags);
                self.items.extend(
                    page.items
                        .into_iter()
                        .filter(|i| !feature_flags || is_feature_flag(&i.key) == flags),
                );
                self.next = page.next;
            }
            Err(e) => self.list_error = Some(e),
        }
        cx.notify();
    }

    fn select(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.items.get(ix).cloned() else {
            return;
        };
        self.detail = Detail::Item(ix);
        self.confirm = None;
        self.status = None;
        self.fill(&item, window, cx);
        let values_in_list = self.info.as_ref().is_some_and(|i| i.caps.values_in_list);
        self.loaded = item.value.is_some() && values_in_list;
        if !self.loaded && !self.hides_value(&item) {
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
        });
        cx.notify();
    }

    /// The selected item's full value arrived.
    pub fn on_item(
        &mut self,
        request: RequestId,
        result: Result<KvItem, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
                }
            }
            Err(e) => self.status = Some((false, e)),
        }
        cx.notify();
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
        self.enabled = parsed
            .as_ref()
            .map(|f| f.enabled)
            .or(item.enabled)
            .unwrap_or(true);
        self.kind = None;
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
        self.detail = Detail::New;
        self.confirm = None;
        self.status = None;
        self.loaded = true;
        self.fill(&KvItem::default(), window, cx);
        // A new setting gets the label being filtered on.
        let label = Self::text(&self.label_filter, cx);
        if !label.is_empty() && label != "-" {
            Self::set_input(&self.label, label, window, cx);
        }
        self.kind = self
            .info
            .as_ref()
            .and_then(|i| i.caps.kinds.first().copied());
        cx.notify();
    }

    fn selected(&self) -> Option<&KvItem> {
        match self.detail {
            Detail::Item(ix) => self.items.get(ix),
            _ => None,
        }
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
        let mut key_out = key.clone();
        let description = opt(Self::text(&self.description, cx));
        if self.view == View::Flags {
            if create {
                let (k, v) = new_flag(&key, self.enabled, description.as_deref().unwrap_or(""));
                key_out = k;
                value = v;
            } else {
                value = update_flag(
                    &value,
                    self.enabled,
                    Some(description.as_deref().unwrap_or("")),
                )
                .map_err(|e| e.to_string())?;
                key_out = existing.map_or(key_out, |i| i.key.clone());
            }
            content_type = Some(FEATURE_FLAG_CONTENT_TYPE.to_owned());
        } else if !create && let Some(i) = existing {
            // Keys are renamed by creating a new item; the field is read-only here.
            key_out = i.key.clone();
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
        if self.connection.environment.is_production() && !confirmed {
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
        if self.connection.environment.is_production() {
            // Production changes go through Save, which asks first.
            self.status = Some((
                false,
                "Production: open the flag and use Save to change it".into(),
            ));
            cx.notify();
            return;
        }
        match update_flag(&value, !flag.enabled, None) {
            Ok(v) => self.send_edit(
                CloudEdit::Put(KvWrite {
                    key: item.key,
                    label: item.label,
                    value: v,
                    content_type: Some(FEATURE_FLAG_CONTENT_TYPE.to_owned()),
                    tags: item.tags,
                    etag: item.etag,
                    ..KvWrite::default()
                }),
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
        self.core.send(Command::CloudEdit {
            session: self.session,
            request,
            edit,
        });
        cx.notify();
    }

    /// A change finished: the list is reloaded and the item selected again.
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
        match result {
            Ok(m) => {
                self.status = Some((true, m));
                let keep = self.status.clone();
                self.reload(cx);
                self.status = keep;
            }
            Err(e) => self.status = Some((false, e)),
        }
        cx.notify();
    }

    fn set_view(&mut self, view: View, cx: &mut Context<Self>) {
        if self.view != view {
            self.view = view;
            self.reload(cx);
        }
    }

    fn set_scope(&mut self, scope: String, cx: &mut Context<Self>) {
        self.scope = Some(scope);
        self.reload(cx);
    }

    fn render_header(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let info = self.info.as_ref();
        let caps = info.map(|i| i.caps.clone()).unwrap_or_default();
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
            .when(self.read_only(), |d| {
                d.child(
                    div()
                        .text_size(ts::SMALL)
                        .text_color(p.fg3)
                        .child("Read-only"),
                )
            })
            .child(div().flex_1())
            .children(scope_picker)
            .when(caps.feature_flags, |d| {
                d.child(ui::segmented(
                    "cl-view",
                    vec![
                        view_option("Settings", View::Items),
                        view_option("Feature flags", View::Flags),
                    ],
                    22.,
                    p,
                ))
            })
            .child(
                ui::button("cl-refresh", "Refresh", Kind::Secondary, p)
                    .on_click(cx.listener(|t, _, _, cx| t.reload(cx))),
            )
            .when(!self.read_only() && info.is_some(), |d| {
                d.child(
                    ui::button("cl-new", new_label, Kind::Primary, p)
                        .on_click(cx.listener(|t, _, w, cx| t.new_item(w, cx))),
                )
            })
            .into_any_element()
    }

    fn render_list(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let caps = self
            .info
            .as_ref()
            .map(|i| i.caps.clone())
            .unwrap_or_default();
        let loading = self.list_request.is_some();
        let flags = self.view == View::Flags;
        let selected = match self.detail {
            Detail::Item(ix) => Some(ix),
            _ => None,
        };
        let filter_bar = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(rpx(6.))
            .p(rpx(8.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&self.filter).text_size(ts::BODY)),
            )
            .when(caps.labels, |d| {
                d.child(
                    div()
                        .w(rpx(110.))
                        .flex_none()
                        .child(Input::new(&self.label_filter).text_size(ts::BODY)),
                )
            });
        let hint = div()
            .flex_none()
            .px(rpx(10.))
            .pb(rpx(6.))
            .text_size(ts::SMALL)
            .text_color(p.fg3)
            .child(if caps.labels {
                format!(
                    "{} · label \"-\" lists settings without one · ↵ to search",
                    caps.filter_hint
                )
            } else {
                format!("{} · ↵ to search", caps.filter_hint)
            });
        let body: AnyElement = if let Some(e) = &self.list_error {
            div()
                .p(rpx(12.))
                .text_size(ts::BODY)
                .text_color(p.prod)
                .child(e.clone())
                .into_any_element()
        } else if caps.scope_label.is_some() && self.scope.is_none() && self.info.is_some() {
            div()
                .p(rpx(12.))
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .child("No namespaces in this account yet.")
                .into_any_element()
        } else if self.items.is_empty() && !loading && self.info.is_some() {
            div()
                .p(rpx(12.))
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .child(if flags {
                    "No feature flags match."
                } else {
                    "Nothing matches."
                })
                .into_any_element()
        } else {
            let items: Vec<KvItem> = self.items.clone();
            let p2 = *p;
            let read_only = self.read_only();
            uniform_list(
                "cloud-items",
                items.len(),
                cx.processor(move |_this, range: std::ops::Range<usize>, _w, cx| {
                    let p = &p2;
                    range
                        .map(|r| {
                            let item = &items[r];
                            let is_sel = selected == Some(r);
                            let flag = flags
                                .then(|| parse_flag(&item.key, item.value.as_deref().unwrap_or("")))
                                .flatten();
                            let title = flag
                                .as_ref()
                                .map_or_else(|| item.key.clone(), |f| f.id.clone());
                            let sub = if let Some(f) = &flag {
                                let mut s = f.description.clone();
                                if f.filters > 0 {
                                    if !s.is_empty() {
                                        s.push_str(" · ");
                                    }
                                    s.push_str(&format!(
                                        "{} filter{}",
                                        f.filters,
                                        if f.filters == 1 { "" } else { "s" }
                                    ));
                                }
                                s
                            } else {
                                let mut parts = Vec::new();
                                if let Some(k) = &item.kind {
                                    parts.push(k.clone());
                                }
                                if let Some(v) =
                                    item.value.as_deref().filter(|_| caps.values_in_list)
                                {
                                    parts.push(
                                        v.chars().take(80).collect::<String>().replace('\n', " "),
                                    );
                                }
                                if let Some(ms) = item.modified_ms {
                                    parts.push(display_ms(ms));
                                }
                                parts.join(" · ")
                            };
                            div()
                                .id(("cl-item", r))
                                .w_full()
                                .h(rpx(ROW_H))
                                .flex()
                                .items_center()
                                .gap(rpx(8.))
                                .px(rpx(10.))
                                .border_b_1()
                                .border_color(p.line)
                                .cursor_pointer()
                                .when(is_sel, |d| d.bg(p.sel))
                                .when(!is_sel, |d| d.hover(|s| s.bg(p.hover)))
                                .on_click(cx.listener(move |this, _, w, cx| this.select(r, w, cx)))
                                .when_some(flag.as_ref().map(|f| f.enabled), |d, on| {
                                    d.child(
                                        div()
                                            .id(("cl-flag", r))
                                            .flex_none()
                                            .w(rpx(28.))
                                            .h(rpx(16.))
                                            .rounded(px(8.))
                                            .bg(if on { p.dev } else { p.bd2 })
                                            .flex()
                                            .items_center()
                                            .when(on, |d| d.justify_end())
                                            .px(rpx(2.))
                                            .when(!read_only, |d| {
                                                d.on_click(cx.listener(move |this, _, _, cx| {
                                                    cx.stop_propagation();
                                                    this.toggle_flag(r, cx)
                                                }))
                                            })
                                            .child(div().size(rpx(12.)).rounded(px(6.)).bg(p.elev)),
                                    )
                                })
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .flex()
                                        .flex_col()
                                        .child(
                                            div()
                                                .text_size(ts::BODY)
                                                .font_family(MONO)
                                                .truncate()
                                                .child(title),
                                        )
                                        .when(!sub.is_empty(), |d| {
                                            d.child(
                                                div()
                                                    .text_size(ts::SMALL)
                                                    .text_color(p.fg3)
                                                    .truncate()
                                                    .child(sub),
                                            )
                                        }),
                                )
                                .when_some(item.label.clone(), |d, l| {
                                    d.child(
                                        div()
                                            .flex_none()
                                            .max_w(rpx(110.))
                                            .truncate()
                                            .px(rpx(6.))
                                            .rounded(px(4.))
                                            .bg(p.surface)
                                            .border_1()
                                            .border_color(p.bd)
                                            .text_size(ts::SMALL)
                                            .text_color(p.fg2)
                                            .child(l),
                                    )
                                })
                                .when(item.locked == Some(true), |d| {
                                    d.child(
                                        div()
                                            .flex_none()
                                            .text_size(ts::SMALL)
                                            .text_color(p.fg3)
                                            .child("locked"),
                                    )
                                })
                                .when(item.enabled == Some(false), |d| {
                                    d.child(
                                        div()
                                            .flex_none()
                                            .text_size(ts::SMALL)
                                            .text_color(p.fg3)
                                            .child("disabled"),
                                    )
                                })
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .flex_1()
            .into_any_element()
        };
        let footer =
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(rpx(8.))
                .px(rpx(10.))
                .py(rpx(6.))
                .border_t_1()
                .border_color(p.bd)
                .text_size(ts::SMALL)
                .text_color(p.fg3)
                .child(if loading {
                    "Loading…".to_owned()
                } else {
                    format!(
                        "{}{} {}",
                        self.items.len(),
                        if self.next.is_some() { "+" } else { "" },
                        if flags { "flags" } else { "items" }
                    )
                })
                .child(div().flex_1())
                .when(self.next.is_some() && !loading, |d| {
                    d.child(ui::button("cl-more", "Load more", Kind::Ghost, p).on_click(
                        cx.listener(|t, _, _, cx| {
                            let next = t.next.clone();
                            t.list(next, cx);
                        }),
                    ))
                });
        div()
            .w(rpx(LIST_W))
            .flex_none()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(p.bd)
            .child(filter_bar)
            .child(hint)
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
            .child(footer)
            .into_any_element()
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

    fn render_detail(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let Some(info) = &self.info else {
            return div().flex_1().into_any_element();
        };
        if self.detail == Detail::None {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .child(if self.read_only() {
                    "Select an item to see it."
                } else {
                    "Select an item to see or edit it."
                })
                .into_any_element();
        }
        let caps = &info.caps;
        let create = self.detail == Detail::New;
        let flags = self.view == View::Flags;
        let read_only = self.read_only();
        let item = self.selected();
        let locked = item.and_then(|i| i.locked).unwrap_or(false);
        let frozen = read_only || locked;
        let busy = self.edit_request.is_some();
        let key_label = if flags {
            "Flag ID"
        } else if caps.hierarchical {
            "Name"
        } else {
            "Key"
        };
        let mut fields: Vec<AnyElement> = vec![Self::field(
            key_label,
            Self::boxed(&self.key, true, !create, p),
            p,
        )];
        if caps.labels {
            fields.push(Self::field(
                "Label",
                Self::boxed(&self.label, true, !create, p),
                p,
            ));
        }
        if create && !caps.kinds.is_empty() {
            let this = cx.entity().downgrade();
            let options = caps
                .kinds
                .iter()
                .map(|k| {
                    let k = *k;
                    let this = this.clone();
                    let on: ui::OnClick = Box::new(move |_, _, cx| {
                        let _ = this.update(cx, |t, cx| {
                            t.kind = Some(k);
                            cx.notify();
                        });
                    });
                    (SharedString::from(k), self.kind == Some(k), on)
                })
                .collect();
            fields.push(Self::field(
                "Type",
                ui::segmented("cl-kind", options, 22., p).into_any_element(),
                p,
            ));
        }
        if caps.content_type && !flags {
            fields.push(Self::field(
                "Content type",
                Self::boxed(&self.content_type, true, frozen, p),
                p,
            ));
        }
        if flags
            || matches!(
                self.connection.service,
                CloudService::SecretsManager | CloudService::ParameterStore
            )
        {
            fields.push(Self::field(
                "Description",
                Self::boxed(&self.description, false, frozen, p),
                p,
            ));
        }
        if caps.tags && !flags {
            fields.push(Self::field(
                "Tags",
                Self::boxed(&self.tags, true, frozen, p),
                p,
            ));
        }
        let enabled_box = (caps.enabled || flags).then(|| {
            ui::checkbox(
                "cl-enabled",
                self.enabled,
                if flags {
                    "Enabled"
                } else {
                    "Enabled (apps can read it)"
                },
                p,
            )
            .when(!frozen, |d| {
                d.on_click(cx.listener(|t, _, _, cx| {
                    t.enabled = !t.enabled;
                    cx.notify();
                }))
            })
        });
        let hidden = !self.loaded && item.is_some_and(|i| self.hides_value(i));
        let loading_value = self.get_request.is_some();
        let value: AnyElement = if hidden {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_start()
                .gap(rpx(8.))
                .p(rpx(10.))
                .border_1()
                .border_color(p.bd)
                .rounded(px(6.))
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .child("The value is a secret and is fetched only when you ask.")
                .child(
                    ui::button(
                        "cl-reveal",
                        if loading_value {
                            "Loading…"
                        } else {
                            "Show value"
                        },
                        Kind::Secondary,
                        p,
                    )
                    .on_click(cx.listener(|t, _, _, cx| {
                        if let Some(i) = t.selected().cloned() {
                            t.fetch(&i, cx);
                        }
                    })),
                )
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_h(rpx(120.))
                .border_1()
                .border_color(p.bd2)
                .rounded(px(6.))
                .bg(p.bg)
                .p(rpx(6.))
                .child(
                    Editor::new(&self.value)
                        .readonly(frozen || !self.loaded)
                        .bordered(false)
                        .appearance(false)
                        .h(relative(1.))
                        .font_family(MONO)
                        .text_size(ts::BODY),
                )
                .into_any_element()
        };
        let meta = item.map(|i| {
            let mut parts = Vec::new();
            if let Some(ms) = i.modified_ms {
                parts.push(format!("Changed {}", display_ms(ms)));
            }
            if locked {
                parts.push("Locked: unlock to change or delete it".into());
            }
            parts.join(" · ")
        });
        let actions = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(rpx(8.))
            .pt(rpx(4.))
            .when(!read_only, |d| match &self.confirm {
                Some(Confirm::Delete) => d
                    .child(div().text_size(ts::BODY).child(format!(
                        "Delete {}{}?",
                        item.map(|i| i.key.as_str()).unwrap_or(""),
                        match self.connection.service {
                            CloudService::SecretsManager => " (recoverable for 7 days)",
                            CloudService::KeyVault => " (soft-deleted when the vault keeps them)",
                            _ => "",
                        }
                    )))
                    .child(
                        ui::button("cl-del-yes", "Delete", Kind::Destructive, p)
                            .on_click(cx.listener(|t, _, _, cx| t.delete(true, cx))),
                    )
                    .child(ui::button("cl-del-no", "Cancel", Kind::Ghost, p).on_click(
                        cx.listener(|t, _, _, cx| {
                            t.confirm = None;
                            cx.notify();
                        }),
                    )),
                Some(Confirm::Save) => d
                    .child(
                        div()
                            .text_size(ts::BODY)
                            .child("This is a Production connection. Save the change?"),
                    )
                    .child(
                        ui::button("cl-save-yes", "Save to Production", Kind::Destructive, p)
                            .on_click(cx.listener(|t, _, _, cx| t.save(true, cx))),
                    )
                    .child(ui::button("cl-save-no", "Cancel", Kind::Ghost, p).on_click(
                        cx.listener(|t, _, _, cx| {
                            t.confirm = None;
                            cx.notify();
                        }),
                    )),
                None => d
                    .when(!locked, |d| {
                        d.child(
                            ui::button(
                                "cl-save",
                                if busy {
                                    "Saving…"
                                } else if create {
                                    "Create"
                                } else {
                                    "Save"
                                },
                                Kind::Primary,
                                p,
                            )
                            .on_click(cx.listener(|t, _, _, cx| t.save(false, cx))),
                        )
                    })
                    .when(!create && !locked, |d| {
                        d.child(
                            ui::button("cl-delete", "Delete", Kind::Secondary, p)
                                .on_click(cx.listener(|t, _, _, cx| t.delete(false, cx))),
                        )
                    })
                    .when(!create && caps.locks, |d| {
                        d.child(
                            ui::button(
                                "cl-lock",
                                if locked { "Unlock" } else { "Lock" },
                                Kind::Ghost,
                                p,
                            )
                            .on_click(cx.listener(|t, _, _, cx| t.toggle_lock(cx))),
                        )
                    })
                    .when(create, |d| {
                        d.child(ui::button("cl-cancel", "Cancel", Kind::Ghost, p).on_click(
                            cx.listener(|t, _, _, cx| {
                                t.detail = Detail::None;
                                cx.notify();
                            }),
                        ))
                    }),
            })
            .child(div().flex_1())
            .when_some(meta, |d, m| {
                d.child(div().text_size(ts::SMALL).text_color(p.fg3).child(m))
            });
        div()
            .id("cl-detail")
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(rpx(10.))
            .p(rpx(14.))
            .child(div().grid().grid_cols(2).gap(rpx(10.)).children(fields))
            .children(enabled_box)
            .child(
                div()
                    .text_size(ts::LABEL)
                    .text_color(p.fg2)
                    .font_weight(FontWeight::MEDIUM)
                    .child(if flags {
                        "Definition (JSON; Enabled and Description above win)"
                    } else {
                        "Value"
                    }),
            )
            .child(value)
            .child(actions)
            .into_any_element()
    }
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
            None => div()
                .flex_1()
                .min_h_0()
                .flex()
                .child(self.render_list(&p, cx))
                .child(self.render_detail(&p, cx))
                .into_any_element(),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(p.surface)
            .child(header)
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
            | Event::CloudEdited { session, .. } => *session,
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
            } => t.on_items(request, result, cx),
            Event::CloudItem {
                request, result, ..
            } => t.on_item(request, result, window, cx),
            Event::CloudEdited {
                request, result, ..
            } => t.on_edited(request, result, cx),
            _ => {}
        });
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
