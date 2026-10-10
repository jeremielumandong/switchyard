//! The secret vault window: named secrets (`{{vault.name}}`) used by connection settings
//! and the API workbench. A secret is stored in the keychain, read from 1Password with
//! the `op` CLI, or linked to a secret in
//! Azure Key Vault, AWS Secrets Manager or Parameter Store through a saved cloud
//! connection and read from there each time it is used.
//!
//! The catalog arrives from core as `Event::NamedSecrets`; values never reach the UI.

use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, ClipboardItem, Context, Entity, FontWeight,
    Global, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, TitlebarOptions, Window, WindowBounds,
    WindowOptions, div, px, size,
};
use secrecy::SecretString;
use switchyard_core::store::named_secrets::{NamedSecret, NamedSecretSource, expression};
use switchyard_core::store::{CloudService, Profile, ProfileId};
use switchyard_core::{Command, RequestId, RuntimeHandle};

use crate::appearance::{rpx, ts};
use crate::theme::{MONO, Palette, palette};
use crate::ui::{self, Kind};

/// A cloud connection secrets can be read through.
#[derive(Clone, Debug, PartialEq)]
pub struct SecretStoreConn {
    /// Profile id.
    pub id: ProfileId,
    /// Connection name.
    pub name: String,
    /// Key Vault, Secrets Manager or Parameter Store.
    pub service: CloudService,
}

/// An answer from core to the window's last request.
#[derive(Clone, Debug)]
struct Answer {
    request: RequestId,
    result: Result<String, String>,
}

/// The catalog and the cloud stores, as last loaded.
#[derive(Default)]
pub struct SecretVault {
    /// Named secrets by name.
    pub list: Vec<NamedSecret>,
    /// Cloud connections that hold secrets.
    pub stores: Vec<SecretStoreConn>,
    requested: bool,
    answer: Option<Answer>,
    window: Option<AnyWindowHandle>,
}

impl Global for SecretVault {}

/// Ask core for the catalog once per app run.
pub fn ensure_loaded(core: &RuntimeHandle, cx: &mut App) {
    let v = cx.default_global::<SecretVault>();
    if !v.requested {
        v.requested = true;
        core.send(Command::LoadNamedSecrets);
    }
}

/// `Event::NamedSecrets`.
pub fn on_list(list: Vec<NamedSecret>, cx: &mut App) {
    let v = cx.default_global::<SecretVault>();
    v.requested = true;
    v.list = list;
}

/// `Event::NamedSecretError` and `Event::NamedSecretTested`.
pub fn on_answer(request: RequestId, result: Result<String, String>, cx: &mut App) {
    cx.default_global::<SecretVault>().answer = Some(Answer { request, result });
}

/// `Event::Profiles`: keep the cloud connections that can hold secrets.
pub fn on_profiles(all: &[Profile], cx: &mut App) {
    let stores = all
        .iter()
        .filter_map(|p| match p {
            Profile::Cloud(c) if holds_secrets(c.service) => Some(SecretStoreConn {
                id: c.id.clone(),
                name: c.name.clone(),
                service: c.service,
            }),
            _ => None,
        })
        .collect();
    cx.default_global::<SecretVault>().stores = stores;
}

/// Services a named secret can be linked to.
pub fn holds_secrets(s: CloudService) -> bool {
    matches!(
        s,
        CloudService::KeyVault | CloudService::SecretsManager | CloudService::ParameterStore
    )
}

/// Open the secret vault window, or bring it to the front.
pub fn open_manager(core: RuntimeHandle, cx: &mut App) {
    ensure_loaded(&core, cx);
    if let Some(h) = cx.default_global::<SecretVault>().window
        && h.update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        return;
    }
    let bounds = Bounds::centered(None, size(px(860.), px(560.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(size(px(640.), px(420.))),
        titlebar: Some(TitlebarOptions {
            title: Some("Secret vault".into()),
            ..Default::default()
        }),
        app_id: Some("switchyard".into()),
        ..Default::default()
    };
    match gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| SecretVaultView::new(core, window, cx))
    }) {
        Ok((handle, _)) => cx.default_global::<SecretVault>().window = Some(handle),
        Err(e) => tracing::error!(error = %e, "failed to open the secret vault window"),
    }
}

/// Where the edited secret's value comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceKind {
    Keychain,
    Cloud,
    OnePassword,
}

/// The secret vault: named secrets on the left, the editor on the right.
pub struct SecretVaultView {
    core: RuntimeHandle,
    /// Name of the secret being edited; `None` for a new one.
    selected: Option<String>,
    name: Entity<InputState>,
    description: Entity<InputState>,
    value: Entity<InputState>,
    key: Entity<InputState>,
    field: Entity<InputState>,
    /// 1Password secret reference (`op://…`).
    op_reference: Entity<InputState>,
    /// 1Password account, when several are signed in.
    op_account: Entity<InputState>,
    /// The cloud connection, when the source is a cloud store.
    store: Option<ProfileId>,
    /// Where the value comes from.
    kind: SourceKind,
    /// Request whose answer is shown, and whether it was a save.
    pending: Option<(RequestId, bool)>,
    /// Last answer: a test result or an error.
    status: Option<Result<String, String>>,
    /// Select this name when the reloaded list arrives.
    reselect: Option<String>,
    _subs: Vec<gpui_kit::Subscription>,
}

impl SecretVaultView {
    fn new(core: RuntimeHandle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = |ph: &str, masked: bool, window: &mut Window, cx: &mut Context<Self>| {
            let ph = ph.to_owned();
            cx.new(|cx| InputState::new(window, cx).placeholder(ph).masked(masked))
        };
        let name = input("orders-api-key", false, window, cx);
        let description = input("Orders API key for staging", false, window, cx);
        let value = input("", true, window, cx);
        let key = input("orders-api-key", false, window, cx);
        let field = input("password (optional)", false, window, cx);
        let op_reference = input("op://Private/Orders DB/password", false, window, cx);
        let op_account = input("my.1password.com (optional)", false, window, cx);
        let sub = cx.observe_global_in::<SecretVault>(window, |this, window, cx| {
            if let Some(name) = this.reselect.clone() {
                let found = cx
                    .global::<SecretVault>()
                    .list
                    .iter()
                    .find(|s| s.name == name)
                    .cloned();
                if let Some(s) = found {
                    this.reselect = None;
                    this.select(&s, window, cx);
                    this.status = Some(Ok("Saved".into()));
                }
            }
            let answer = cx.global::<SecretVault>().answer.clone();
            if let (Some((req, save)), Some(a)) = (this.pending, answer)
                && a.request == req
            {
                this.pending = None;
                this.status = Some(a.result);
                if save {
                    // A failed save keeps the form; nothing to reselect.
                    this.reselect = None;
                }
            }
            cx.notify();
        });
        Self {
            core,
            selected: None,
            name,
            description,
            value,
            key,
            field,
            op_reference,
            op_account,
            store: None,
            kind: SourceKind::Keychain,
            pending: None,
            status: None,
            reselect: None,
            _subs: vec![sub],
        }
    }

    fn set(input: &Entity<InputState>, v: String, window: &mut Window, cx: &mut Context<Self>) {
        input.update(cx, |i, cx| i.set_value(v, window, cx));
    }

    fn select(&mut self, s: &NamedSecret, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = (!s.name.is_empty()).then(|| s.name.clone());
        self.status = None;
        self.pending = None;
        Self::set(&self.name, s.name.clone(), window, cx);
        Self::set(&self.description, s.description.clone(), window, cx);
        Self::set(&self.value, String::new(), window, cx);
        let (mut store, mut key, mut field) = (None, String::new(), String::new());
        let (mut reference, mut account) = (String::new(), String::new());
        self.kind = match &s.source {
            NamedSecretSource::Local => SourceKind::Keychain,
            NamedSecretSource::Cloud {
                connection,
                key: k,
                field: f,
            } => {
                store = Some(connection.clone());
                key = k.clone();
                field = f.clone().unwrap_or_default();
                SourceKind::Cloud
            }
            NamedSecretSource::OnePassword {
                reference: r,
                account: a,
            } => {
                reference = r.clone();
                account = a.clone().unwrap_or_default();
                SourceKind::OnePassword
            }
        };
        self.store = store;
        Self::set(&self.key, key, window, cx);
        Self::set(&self.field, field, window, cx);
        Self::set(&self.op_reference, reference, window, cx);
        Self::set(&self.op_account, account, window, cx);
        let ph = if self.selected.is_some() {
            "stored · leave blank to keep"
        } else {
            ""
        };
        self.value
            .update(cx, |i, cx| i.set_placeholder(ph, window, cx));
        cx.notify();
    }

    fn new_secret(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select(&NamedSecret::default(), window, cx);
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let read = |i: &Entity<InputState>, cx: &Context<Self>| i.read(cx).value().to_string();
        let name = read(&self.name, cx).trim().to_owned();
        let source = match self.kind {
            SourceKind::Cloud => {
                let Some(connection) = self.store.clone() else {
                    self.status = Some(Err("Choose the cloud connection to read it from".into()));
                    cx.notify();
                    return;
                };
                let field = read(&self.field, cx).trim().to_owned();
                NamedSecretSource::Cloud {
                    connection,
                    key: read(&self.key, cx).trim().to_owned(),
                    field: (!field.is_empty()).then_some(field),
                }
            }
            SourceKind::OnePassword => {
                let account = read(&self.op_account, cx).trim().to_owned();
                NamedSecretSource::OnePassword {
                    reference: read(&self.op_reference, cx).trim().to_owned(),
                    account: (!account.is_empty()).then_some(account),
                }
            }
            SourceKind::Keychain => NamedSecretSource::Local,
        };
        let value = read(&self.value, cx);
        if source == NamedSecretSource::Local && value.is_empty() && self.selected.is_none() {
            self.status = Some(Err("Enter the value to store".into()));
            cx.notify();
            return;
        }
        let secret = NamedSecret {
            name: name.clone(),
            description: read(&self.description, cx).trim().to_owned(),
            source,
        };
        if let Err(e) = secret.validate() {
            self.status = Some(Err(e.message));
            cx.notify();
            return;
        }
        let request = crate::app_state::next_id();
        self.pending = Some((request, true));
        self.status = None;
        self.reselect = Some(name);
        self.core.send(Command::SaveNamedSecret {
            request,
            secret,
            value: (!value.is_empty()).then(|| SecretString::from(value)),
            previous: self.selected.clone(),
        });
        cx.notify();
    }

    fn test(&mut self, cx: &mut Context<Self>) {
        if let Some(name) = self.selected.clone() {
            let request = crate::app_state::next_id();
            self.pending = Some((request, false));
            self.status = Some(Ok("Reading…".into()));
            self.core.send(Command::TestNamedSecret { request, name });
            cx.notify();
        }
    }

    fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(name) = self.selected.clone() {
            self.core.send(Command::DeleteNamedSecret { name });
            self.new_secret(window, cx);
        }
    }

    fn source_label(s: &NamedSecret, stores: &[SecretStoreConn]) -> String {
        match &s.source {
            NamedSecretSource::Local => "Keychain".into(),
            NamedSecretSource::OnePassword { .. } => "1Password".into(),
            NamedSecretSource::Cloud { connection, .. } => stores
                .iter()
                .find(|c| &c.id == connection)
                .map_or_else(|| "Missing connection".into(), |c| c.name.clone()),
        }
    }

    fn row(
        &self,
        s: &NamedSecret,
        stores: &[SecretStoreConn],
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.selected.as_deref() == Some(s.name.as_str());
        let secret = s.clone();
        div()
            .id(SharedString::from(format!("vault-{}", s.name)))
            .flex()
            .items_center()
            .gap(rpx(8.))
            .px(rpx(10.))
            .py(rpx(5.))
            .rounded(px(5.))
            .cursor_pointer()
            .when(active, |d| d.bg(p.sel))
            .hover(|d| d.bg(p.hover))
            .on_click(cx.listener(move |this, _, window, cx| this.select(&secret, window, cx)))
            .child(
                ui::mono(s.name.clone(), px(12.))
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(p.fg),
            )
            .child(
                div()
                    .flex_none()
                    .max_w(rpx(120.))
                    .truncate()
                    .text_size(ts::CAPTION_PLUS)
                    .text_color(p.fg3)
                    .child(Self::source_label(s, stores)),
            )
    }
}

fn field(label: &str, input: &Entity<InputState>, p: &Palette) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(rpx(4.))
        .child(
            div()
                .text_size(ts::SMALL)
                .text_color(p.fg3)
                .child(label.to_owned()),
        )
        .child(
            div()
                .h(rpx(28.))
                .flex()
                .items_center()
                .px(rpx(8.))
                .border_1()
                .border_color(p.bd2)
                .rounded(px(6.))
                .bg(p.bg)
                .font_family(MONO)
                .text_size(ts::BODY)
                .child(Input::new(input).appearance(false).text_size(ts::BODY)),
        )
}

impl Render for SecretVaultView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let (list, stores) = {
            let v = cx.default_global::<SecretVault>();
            (v.list.clone(), v.stores.clone())
        };
        let mut rows = div()
            .id("vault-list")
            .w(rpx(280.))
            .flex_none()
            .flex()
            .flex_col()
            .gap(rpx(1.))
            .p(rpx(8.))
            .border_r_1()
            .border_color(p.bd)
            .bg(p.panel)
            .overflow_y_scroll()
            .child(ui::caption("NAMED SECRETS", &p).px(rpx(10.)).py(rpx(4.)));
        if list.is_empty() {
            rows = rows.child(
                div()
                    .px(rpx(10.))
                    .py(rpx(4.))
                    .text_size(ts::BODY)
                    .text_color(p.fg3)
                    .child("None yet. Add one, then type {{vault.name}} in a password field or a request."),
            );
        }
        for s in &list {
            rows = rows.child(self.row(s, &stores, &p, cx));
        }

        let view = cx.entity().downgrade();
        let source_options = [
            (SourceKind::Keychain, "Keychain"),
            (SourceKind::OnePassword, "1Password"),
            (SourceKind::Cloud, "Cloud secret store"),
        ]
        .into_iter()
        .map(|(kind, label)| {
            let view = view.clone();
            let on_click: ui::OnClick = Box::new(move |_, _, cx| {
                let _ = view.update(cx, |this, cx| {
                    this.kind = kind;
                    if kind != SourceKind::Cloud {
                        this.store = None;
                    }
                    cx.notify();
                });
            });
            (SharedString::from(label), self.kind == kind, on_click)
        })
        .collect();
        let store_options: Vec<(SharedString, bool, ui::OnClick)> = stores
            .iter()
            .map(|c| {
                let id = c.id.clone();
                let view = view.clone();
                let label: SharedString =
                    format!("{} · {}", c.name, c.service.display_name()).into();
                let on_click: ui::OnClick = Box::new(move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.store = Some(id.clone());
                        cx.notify();
                    });
                });
                (label, self.store.as_ref() == Some(&c.id), on_click)
            })
            .collect();

        let title = if self.selected.is_some() {
            "Edit secret"
        } else {
            "New secret"
        };
        let reference = self.selected.as_deref().map(expression);
        let mut form = div()
            .id("vault-form")
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(rpx(12.))
            .p(rpx(18.))
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(ts::BASE)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.fg)
                            .child(title),
                    )
                    .child(
                        ui::button("vault-new", "New", Kind::Secondary, &p).on_click(
                            cx.listener(|this, _, window, cx| this.new_secret(window, cx)),
                        ),
                    ),
            )
            .child(div().text_size(ts::SMALL).text_color(p.fg3).child(
                "Type {{vault.name}} in a connection's password, token or key field, \
                         or anywhere in an API request, to use a secret without storing it \
                         there. Linked secrets are read from the cloud each time they are \
                         used (kept in memory for a minute).",
            ))
            .when_some(reference, |d, r| {
                let copy = r.clone();
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(rpx(8.))
                        .child(ui::mono(r, px(12.)).text_color(p.fg2))
                        .child(
                            ui::button("vault-copy", "Copy reference", Kind::Secondary, &p)
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                                }),
                        ),
                )
            })
            .child(
                div()
                    .flex()
                    .gap(rpx(10.))
                    .child(div().w(rpx(220.)).child(field("Name", &self.name, &p)))
                    .child(
                        div()
                            .flex_1()
                            .child(field("Description", &self.description, &p)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(rpx(4.))
                    .child(
                        div()
                            .text_size(ts::SMALL)
                            .text_color(p.fg3)
                            .child("Value comes from"),
                    )
                    .child(ui::segmented("vault-source", source_options, 22., &p)),
            );
        form = if self.kind == SourceKind::Cloud {
            let picker = if store_options.is_empty() {
                div()
                    .text_size(ts::BODY)
                    .text_color(p.fg3)
                    .child(
                        "No Key Vault, Secrets Manager or Parameter Store connection yet. \
                         Add one with New connection › Cloud, then come back.",
                    )
                    .into_any_element()
            } else {
                ui::segmented("vault-store", store_options, 22., &p).into_any_element()
            };
            form.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(rpx(4.))
                    .child(
                        div()
                            .text_size(ts::SMALL)
                            .text_color(p.fg3)
                            .child("Connection"),
                    )
                    .child(picker),
            )
            .child(
                div()
                    .flex()
                    .gap(rpx(10.))
                    .child(
                        div()
                            .flex_1()
                            .child(field("Secret name in the store", &self.key, &p)),
                    )
                    .child(div().w(rpx(200.)).child(field(
                        "JSON field (optional)",
                        &self.field,
                        &p,
                    ))),
            )
        } else if self.kind == SourceKind::OnePassword {
            form.child(
                div()
                    .flex()
                    .gap(rpx(10.))
                    .child(
                        div()
                            .flex_1()
                            .child(field("Secret reference", &self.op_reference, &p)),
                    )
                    .child(div().w(rpx(220.)).child(field(
                        "Account (optional)",
                        &self.op_account,
                        &p,
                    ))),
            )
            .child(div().text_size(ts::SMALL).text_color(p.fg3).child(
                "Read with the 1Password CLI (op). In 1Password, right-click a field \
                         and choose Copy Secret Reference. Turn on Settings › Developer › \
                         Integrate with 1Password CLI so the app can unlock it.",
            ))
        } else {
            form.child(field("Value", &self.value, &p))
        };
        form = form
            .when_some(self.status.clone(), |d, s| {
                let (text, color) = match s {
                    Ok(t) => (t, p.dev),
                    Err(e) => (e, p.prod),
                };
                d.child(div().text_size(ts::BODY).text_color(color).child(text))
            })
            .child(
                div()
                    .flex()
                    .gap(rpx(8.))
                    .justify_end()
                    .when(self.selected.is_some(), |d| {
                        d.child(
                            ui::button("vault-delete", "Delete", Kind::Destructive, &p).on_click(
                                cx.listener(|this, _, window, cx| this.delete(window, cx)),
                            ),
                        )
                        .child(
                            ui::button("vault-test", "Test", Kind::Secondary, &p)
                                .on_click(cx.listener(|this, _, _, cx| this.test(cx))),
                        )
                    })
                    .child(
                        ui::button("vault-save", "Save", Kind::Primary, &p)
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ),
            );
        div()
            .size_full()
            .flex()
            .bg(p.surface)
            .text_color(p.fg)
            .child(rows)
            .child(form)
    }
}
