//! Postman-style entry widgets: a key/value row grid and a typed auth form.
//!
//! Both are views bound to a text `InputState` the rest of the panel already
//! reads, hydrates and builds from — *the text is the value*. The widgets
//! re-seed their cells whenever that text changes under them (hydration,
//! Bulk edit, replay) and serialise back into the same line format on every
//! cell edit, so the panel's one `Change` subscription per editor keeps
//! marking dirty exactly once per real edit, and `draft::build` never learns
//! the grids exist. Nothing here touches storage or transport.

use gpui_kit::component::ActiveTheme;
use gpui_kit::component::input::TextareaState;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt, PopupMenu, PopupMenuItem};
use gpui_kit::component::{
    Sizable,
    button::{Button, ButtonVariants},
};
use gpui_kit::{
    AnyElement, Context, Entity, Focusable, Render, SharedString, Subscription, Window, div,
    prelude::*, px,
};

use crate::api::compat::field;
use crate::api::compat::theme::tokens::space;
use crate::api::compat::theme::{palette, text};

use super::draft::{self, AuthFieldKind, AuthFieldSpec, AuthMode};
use super::view::{
    Col, chip, code_box, header_cells, heading, icon, mono_field, outline_chip, table, table_row,
};

/// The `secret:` key prefix the Vars and Envs editors use for vault rows.
const SECRET_PREFIX: &str = "secret:";

// ---------------------------------------------------------------------------
// Key/value grid
// ---------------------------------------------------------------------------

/// One editable row: enabled toggle, key cell, value cell.
struct KvRow {
    enabled: bool,
    /// The key started with `secret:` when the value cell was last styled.
    secret: bool,
    key: Entity<InputState>,
    value: Entity<InputState>,
    _subscriptions: [Subscription; 2],
}

impl KvRow {
    fn is_blank(&self, cx: &gpui_kit::App) -> bool {
        self.key.read(cx).value().trim().is_empty() && self.value.read(cx).value().trim().is_empty()
    }
}

/// A Postman-style row editor over a `#`-prefixed `key<sep>value` text.
pub struct KvGrid {
    editor: Entity<TextareaState>,
    separator: char,
    prefix: &'static str,
    hint: &'static str,
    key_placeholder: &'static str,
    value_placeholder: &'static str,
    /// Keys starting with `secret:` get a masked value cell and the
    /// "stored in vault" placeholder — the Vars and Envs convention.
    secret_rows: bool,
    rows: Vec<KvRow>,
    /// The editor text the rows were last seeded from or serialised into;
    /// `None` until the first paint.
    serialized: Option<String>,
    /// Bulk edit: the bound text editor is shown instead of the rows.
    bulk: bool,
}

impl KvGrid {
    pub fn new(
        editor: Entity<TextareaState>,
        separator: char,
        prefix: &'static str,
        hint: &'static str,
        placeholders: (&'static str, &'static str),
        secret_rows: bool,
    ) -> Self {
        Self {
            editor,
            separator,
            prefix,
            hint,
            key_placeholder: placeholders.0,
            value_placeholder: placeholders.1,
            secret_rows,
            rows: Vec::new(),
            serialized: None,
            bulk: false,
        }
    }

    /// The bound text, split into rows: `(enabled, key, value)`.
    pub fn parse(source: &str, separator: char) -> Vec<(bool, String, String)> {
        source
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let trimmed = line.trim();
                let (enabled, rest) = match trimmed.strip_prefix('#') {
                    Some(rest) => (false, rest.trim()),
                    None => (true, trimmed),
                };
                match rest.split_once(separator) {
                    Some((key, value)) => {
                        (enabled, key.trim().to_string(), value.trim().to_string())
                    }
                    None => (enabled, rest.to_string(), String::new()),
                }
            })
            .collect()
    }

    /// The enabled row count — the composer tab badges.
    pub fn enabled_count(source: &str, separator: char) -> usize {
        Self::parse(source, separator)
            .iter()
            .filter(|(enabled, _, _)| *enabled)
            .count()
    }

    fn reseed(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.rows = Self::parse(text, self.separator)
            .into_iter()
            .map(|(enabled, key, value)| self.make_row(enabled, &key, &value, window, cx))
            .collect();
        let blank = self.make_row(true, "", "", window, cx);
        self.rows.push(blank);
        self.serialized = Some(text.to_string());
    }

    fn make_row(
        &self,
        enabled: bool,
        key: &str,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> KvRow {
        let secret = self.secret_rows && key.starts_with(SECRET_PREFIX);
        let key_placeholder = self.key_placeholder;
        let value_placeholder = if secret {
            vault_placeholder()
        } else {
            self.value_placeholder
        };
        let key = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(key_placeholder)
                .default_value(key.to_string())
        });
        let value = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(value_placeholder)
                .masked(secret)
                .default_value(value.to_string())
        });
        let on_change = |grid: &mut Self,
                         _: &Entity<InputState>,
                         event: &InputEvent,
                         window: &mut Window,
                         cx: &mut Context<Self>| {
            if matches!(event, InputEvent::Change) {
                grid.push(window, cx);
            }
        };
        let subscriptions = [
            cx.subscribe_in(&key, window, on_change),
            cx.subscribe_in(&value, window, on_change),
        ];
        KvRow {
            enabled,
            secret,
            key,
            value,
            _subscriptions: subscriptions,
        }
    }

    /// Serialise the rows into the bound editor. Only a real change reaches
    /// `set_value`, so the panel sees one `Change` per edit and none for
    /// the re-seed that follows hydration.
    fn push(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Postman's trailing placeholder: typing into the blank row makes it
        // real and a fresh blank one appears beneath it.
        if !self.rows.last().is_some_and(|row| row.is_blank(cx)) {
            let blank = self.make_row(true, "", "", window, cx);
            self.rows.push(blank);
        }
        if self.secret_rows {
            for row in &mut self.rows {
                let secret = row.key.read(cx).value().starts_with(SECRET_PREFIX);
                if secret != row.secret {
                    row.secret = secret;
                    let placeholder = if secret {
                        vault_placeholder()
                    } else {
                        self.value_placeholder
                    };
                    row.value.update(cx, |value, cx| {
                        value.set_masked(secret, window, cx);
                        value.set_placeholder(placeholder, window, cx);
                    });
                }
            }
        }
        let text = self.serialize(cx);
        if self.serialized.as_deref() != Some(text.as_str()) {
            self.serialized = Some(text.clone());
            self.editor
                .update(cx, |editor, cx| editor.set_value(text, window, cx));
        }
        cx.notify();
    }

    fn serialize(&self, cx: &gpui_kit::App) -> String {
        let separator = match self.separator {
            ':' => ": ",
            _ => "=",
        };
        self.rows
            .iter()
            .filter(|row| !row.is_blank(cx))
            .map(|row| {
                format!(
                    "{}{}{separator}{}",
                    if row.enabled { "" } else { "# " },
                    row.key.read(cx).value().trim(),
                    row.value.read(cx).value().trim()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn toggle(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get_mut(index) {
            row.enabled = !row.enabled;
            self.push(window, cx);
        }
    }

    fn remove(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index < self.rows.len() {
            self.rows.remove(index);
            self.push(window, cx);
        }
    }

    /// `+ Add`: focus the trailing blank row so typing starts a new entry.
    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.rows.last().is_some_and(|row| row.is_blank(cx)) {
            let blank = self.make_row(true, "", "", window, cx);
            self.rows.push(blank);
        }
        if let Some(row) = self.rows.last() {
            row.key.update(cx, |key, cx| key.focus(window, cx));
        }
        cx.notify();
    }

    fn row_menu(
        &self,
        key: Entity<InputState>,
        cx: &Context<Self>,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        let grid = cx.entity().downgrade();
        move |menu, _, cx| {
            let Some(owner) = grid.upgrade() else {
                return menu;
            };
            let Some(row) = owner.read(cx).rows.iter().find(|row| row.key == key) else {
                return menu;
            };
            let blank = row.is_blank(cx);
            let toggle = grid.clone();
            let toggle_key = key.clone();
            let remove = grid.clone();
            let remove_key = key.clone();
            let add = grid.clone();
            menu.action_context(key.focus_handle(cx))
                .item(
                    PopupMenuItem::new(if row.enabled {
                        "Disable row"
                    } else {
                        "Enable row"
                    })
                    .disabled(blank)
                    .on_click(move |_, window, cx| {
                        let _ = toggle.update(cx, |this, cx| {
                            if let Some(ix) = this.rows.iter().position(|row| row.key == toggle_key)
                            {
                                this.toggle(ix, window, cx);
                            }
                        });
                    }),
                )
                .item(
                    PopupMenuItem::new("Add row").on_click(move |_, window, cx| {
                        let _ = add.update(cx, |this, cx| this.add(window, cx));
                    }),
                )
                .separator()
                .item(PopupMenuItem::new("Remove row").disabled(blank).on_click(
                    move |_, window, cx| {
                        let _ = remove.update(cx, |this, cx| {
                            if let Some(ix) = this.rows.iter().position(|row| row.key == remove_key)
                            {
                                this.remove(ix, window, cx);
                            }
                        });
                    },
                ))
        }
    }
}

fn vault_placeholder() -> &'static str {
    "stored in vault · leave blank to keep"
}

const GRID_COLS: [Col; 4] = [Col::Px(28.), Col::Px(220.), Col::Flex, Col::Px(28.)];

impl Render for KvGrid {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let text = self.editor.read(cx).value().to_string();
        if self.serialized.as_deref() != Some(text.as_str()) {
            self.reseed(&text, window, cx);
        }
        let colors = cx.theme().colors;
        let prefix = self.prefix;
        let editor_focused = self.editor.focus_handle(cx).is_focused(window);
        let tertiary = palette::text_tertiary(cx);
        let last = self.rows.len().saturating_sub(1);
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::SP_2)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_size(text::S11)
                            .text_color(colors.muted_foreground)
                            .child(self.hint),
                    )
                    .child(
                        chip(
                            format!("{prefix}-bulk"),
                            if self.bulk { "Rows" } else { "Bulk edit" },
                            self.bulk,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.bulk = !this.bulk;
                            cx.notify();
                        })),
                    )
                    .context_menu({
                        let grid = cx.entity().downgrade();
                        move |menu, _, cx| {
                            let Some(owner) = grid.upgrade() else {
                                return menu;
                            };
                            let state = owner.read(cx);
                            let bulk = state.bulk;
                            let toggle = grid.clone();
                            let add = grid.clone();
                            menu.action_context(state.editor.focus_handle(cx))
                                .item(
                                    PopupMenuItem::new(if bulk { "Rows" } else { "Bulk edit" })
                                        .on_click(move |_, _, cx| {
                                            let _ = toggle.update(cx, |this, cx| {
                                                this.bulk = !this.bulk;
                                                cx.notify();
                                            });
                                        }),
                                )
                                .item(PopupMenuItem::new("Add row").disabled(bulk).on_click(
                                    move |_, window, cx| {
                                        let _ = add.update(cx, |this, cx| this.add(window, cx));
                                    },
                                ))
                        }
                    }),
            )
            .map(|el| {
                if self.bulk {
                    return el.child(
                        code_box(cx)
                            .min_h(px(72.))
                            .when(editor_focused, |el| el.border_color(colors.ring))
                            .child(field::bare(&self.editor).w_full()),
                    );
                }
                el.child(
                    table(cx)
                        .child(table_row(
                            &GRID_COLS,
                            header_cells(&["", "KEY", "VALUE", ""]),
                            true,
                            cx,
                        ))
                        .children(self.rows.iter().enumerate().map(|(index, row)| {
                            let enabled = row.enabled;
                            let trailing = index == last;
                            let toggle_id = format!("{prefix}-row-{index}-enabled");
                            let remove_id = format!("{prefix}-row-{index}-remove");
                            let toggle_selector = toggle_id.clone();
                            let remove_selector = remove_id.clone();
                            table_row(
                                &GRID_COLS,
                                vec![
                                    // Keep row commands in the gutter: input cells retain their
                                    // native Cut/Copy/Paste/Select All context menus.
                                    div()
                                        .id(SharedString::from(format!(
                                            "{prefix}-row-menu-{:?}",
                                            row.key.entity_id()
                                        )))
                                        .child(
                                            Button::new(SharedString::from(format!(
                                                "{prefix}-enabled-{:?}",
                                                row.key.entity_id()
                                            )))
                                            .xsmall()
                                            .ghost()
                                            .debug_selector(move || toggle_selector.clone())
                                            .tooltip("Enable or disable this row")
                                            .text_color(if enabled {
                                                colors.success
                                            } else {
                                                tertiary
                                            })
                                            .label(if enabled { "✓" } else { "○" })
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.toggle(index, window, cx)
                                            }))
                                            .context_menu(self.row_menu(row.key.clone(), cx)),
                                        )
                                        .into_any_element(),
                                    field::bare(&row.key)
                                        .w_full()
                                        .text_color(if enabled {
                                            palette::lavender(cx)
                                        } else {
                                            tertiary
                                        })
                                        .into_any_element(),
                                    field::bare(&row.value)
                                        .w_full()
                                        .text_color(if enabled {
                                            palette::text_secondary(cx)
                                        } else {
                                            tertiary
                                        })
                                        .into_any_element(),
                                    if trailing {
                                        div().into_any_element()
                                    } else {
                                        Button::new(SharedString::from(format!(
                                            "{prefix}-remove-{:?}",
                                            row.key.entity_id()
                                        )))
                                        .xsmall()
                                        .ghost()
                                        .tooltip("Remove row")
                                        .debug_selector(move || remove_selector.clone())
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.remove(index, window, cx)
                                        }))
                                        .child(icon("close", 11., tertiary))
                                        .into_any_element()
                                    },
                                ],
                                false,
                                cx,
                            )
                        }))
                        .child(
                            Button::new(SharedString::from(format!("{prefix}-add")))
                                .small()
                                .ghost()
                                .debug_selector(move || format!("{prefix}-add"))
                                .px(space::SP_2)
                                .py(px(6.))
                                .bg(colors.sidebar)
                                .text_color(tertiary)
                                .on_click(cx.listener(|this, _, window, cx| this.add(window, cx)))
                                .label("+ Add"),
                        ),
                )
            })
    }
}

// ---------------------------------------------------------------------------
// Typed auth form
// ---------------------------------------------------------------------------

enum FieldValue {
    Input {
        state: Entity<InputState>,
        _subscription: Subscription,
    },
    /// Multi-line fields (certificates, header blocks).
    Area {
        state: Entity<TextareaState>,
        _subscription: Subscription,
    },
    Choice(String),
}

struct AuthField {
    spec: &'static AuthFieldSpec,
    value: FieldValue,
}

/// The per-type fields of an auth config, bound to the `key=value` text
/// `draft::parse_auth` reads. The panel owns the mode; the form owns the
/// cells and keeps the text in step with them.
pub struct AuthForm {
    editor: Entity<TextareaState>,
    prefix: &'static str,
    mode: AuthMode,
    /// A saved auth of this mode exists, so blank masked fields mean "keep
    /// the vault value" rather than "nothing entered yet".
    stored: bool,
    revealed: bool,
    fields: Vec<AuthField>,
    /// `(mode, text, stored)` the fields were last built from or serialised
    /// into; `None` until the first paint.
    seeded: Option<(AuthMode, String, bool)>,
}

impl AuthForm {
    pub fn new(editor: Entity<TextareaState>, prefix: &'static str) -> Self {
        Self {
            editor,
            prefix,
            mode: AuthMode::None,
            stored: false,
            revealed: false,
            fields: Vec::new(),
            seeded: None,
        }
    }

    pub fn set_mode(&mut self, mode: AuthMode, stored: bool, cx: &mut Context<Self>) {
        if self.mode != mode || self.stored != stored {
            self.mode = mode;
            self.stored = stored;
            cx.notify();
        }
    }

    fn reseed(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let values = draft::auth_field_values(text);
        let revealed = self.revealed;
        let stored = self.stored;
        self.fields = draft::auth_fields(self.mode)
            .iter()
            .map(|spec| {
                let current = values.get(spec.key).cloned().unwrap_or_default();
                let value = match spec.kind {
                    AuthFieldKind::Choice(options) => {
                        FieldValue::Choice(if options.contains(&current.as_str()) {
                            current
                        } else {
                            options.first().copied().unwrap_or_default().to_string()
                        })
                    }
                    kind => {
                        let placeholder = if kind == AuthFieldKind::Secret && stored {
                            vault_placeholder()
                        } else {
                            spec.placeholder
                        };
                        if matches!(kind, AuthFieldKind::Multiline | AuthFieldKind::Headers) {
                            let state = cx.new(|cx| {
                                TextareaState::new(window, cx)
                                    .placeholder(placeholder)
                                    .default_value(current.clone())
                                    .auto_grow(3, 10)
                            });
                            let subscription = cx.subscribe_in(
                                &state,
                                window,
                                |form, _, event: &InputEvent, window, cx| {
                                    if matches!(event, InputEvent::Change) {
                                        form.push(window, cx);
                                    }
                                },
                            );
                            FieldValue::Area {
                                state,
                                _subscription: subscription,
                            }
                        } else {
                            let state = cx.new(|cx| {
                                let state = InputState::new(window, cx)
                                    .placeholder(placeholder)
                                    .default_value(current.clone());
                                match kind {
                                    AuthFieldKind::Secret => state.masked(!revealed),
                                    _ => state,
                                }
                            });
                            let subscription = cx.subscribe_in(
                                &state,
                                window,
                                |form, _, event: &InputEvent, window, cx| {
                                    if matches!(event, InputEvent::Change) {
                                        form.push(window, cx);
                                    }
                                },
                            );
                            FieldValue::Input {
                                state,
                                _subscription: subscription,
                            }
                        }
                    }
                };
                AuthField { spec, value }
            })
            .collect();
        self.seeded = Some((self.mode, text.to_string(), stored));
    }

    /// The OAuth 2 flow the visible fields belong to (`""` = plain token).
    fn flow(&self) -> &str {
        self.fields
            .iter()
            .find(|field| field.spec.key == "flow")
            .and_then(|field| match &field.value {
                FieldValue::Choice(value) => Some(value.as_str()),
                FieldValue::Input { .. } | FieldValue::Area { .. } => None,
            })
            .unwrap_or("")
    }

    fn visible(&self, field: &AuthField, cx: &gpui_kit::App) -> bool {
        if self.mode == AuthMode::Basic
            && matches!(
                field.spec.key,
                "method" | "token_path" | "ttl_secs" | "headers"
            )
        {
            return self.fields.iter().any(|field| {
                field.spec.key == "auth_url"
                    && matches!(&field.value, FieldValue::Input { state, .. }
                        if !state.read(cx).value().trim().is_empty())
            });
        }
        field.spec.flows.is_empty() || field.spec.flows.contains(&self.flow())
    }

    fn serialize(&self, cx: &gpui_kit::App) -> String {
        let mut lines = Vec::new();
        let mut body = None;
        for field in self.fields.iter().filter(|field| self.visible(field, cx)) {
            match &field.value {
                FieldValue::Choice(value) => {
                    if !value.is_empty() {
                        lines.push(format!("{}={value}", field.spec.key));
                    }
                }
                FieldValue::Input { .. } | FieldValue::Area { .. } => {
                    let value = match &field.value {
                        FieldValue::Input { state, .. } => state.read(cx).value().to_string(),
                        FieldValue::Area { state, .. } => state.read(cx).value().to_string(),
                        FieldValue::Choice(_) => String::new(),
                    };
                    if value.trim().is_empty() {
                        continue;
                    }
                    if field.spec.kind == AuthFieldKind::Multiline {
                        body = Some(value.trim().to_string());
                    } else if field.spec.kind == AuthFieldKind::Headers {
                        lines.push(format!("headers={}", serde_json::Value::from(value.trim())));
                    } else {
                        lines.push(format!("{}={}", field.spec.key, value.trim()));
                    }
                }
            }
        }
        if let Some(body) = body {
            lines.push(format!("body={body}"));
        }
        lines.join("\n")
    }

    fn push(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.serialize(cx);
        let seeded = self
            .seeded
            .as_ref()
            .is_some_and(|(_, seeded, _)| seeded == &text);
        if !seeded {
            self.seeded = Some((self.mode, text.clone(), self.stored));
            self.editor
                .update(cx, |editor, cx| editor.set_value(text, window, cx));
        }
        cx.notify();
    }

    fn choose(
        &mut self,
        key: &'static str,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(FieldValue::Choice(current)) = self
            .fields
            .iter_mut()
            .find(|field| field.spec.key == key)
            .map(|field| &mut field.value)
            && current != value
        {
            *current = value.to_string();
            self.push(window, cx);
        }
    }

    fn set_revealed(&mut self, revealed: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.revealed = revealed;
        for field in &self.fields {
            if let (AuthFieldKind::Secret, FieldValue::Input { state, .. }) =
                (field.spec.kind, &field.value)
            {
                state.update(cx, |state, cx| state.set_masked(!revealed, window, cx));
            }
        }
        cx.notify();
    }

    fn render_field(
        &self,
        field: &AuthField,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors;
        let spec = field.spec;
        let label = heading(spec.label, colors.muted_foreground);
        match (&field.value, spec.kind) {
            (FieldValue::Choice(current), AuthFieldKind::Choice(options)) => div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(label)
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(2.))
                        .children(options.iter().map(|option| {
                            let value: &'static str = option;
                            let key = spec.key;
                            chip(
                                format!("{}-{}-{}", self.prefix, key, choice_slug(value)),
                                choice_label(key, value),
                                current == value,
                                cx,
                            )
                            .on_click(cx.listener(
                                move |this, _, window, cx| this.choose(key, value, window, cx),
                            ))
                        })),
                )
                .into_any_element(),
            (FieldValue::Area { state, .. }, _) => {
                let focused = state.focus_handle(cx).is_focused(window);
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(label)
                    .child(
                        code_box(cx)
                            .min_h(px(72.))
                            .when(focused, |el| el.border_color(colors.ring))
                            .child(field::bare(state).w_full()),
                    )
                    .into_any_element()
            }
            (FieldValue::Input { state, .. }, _) => div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(label)
                .child(mono_field(state, window, cx).text_color(palette::lavender(cx)))
                .when(self.mode == AuthMode::Basic && spec.key == "auth_url", |el| {
                    el.child(
                        div()
                            .text_sm()
                            .text_color(colors.muted_foreground)
                            .child("Set an Auth URL to sign in automatically and use the JSON token for API requests. Leave blank to send Basic credentials with each request."),
                    )
                })
                .into_any_element(),
            (FieldValue::Choice(_), _) => div().into_any_element(),
        }
    }
}

fn choice_slug(value: &str) -> &str {
    if value.is_empty() { "token" } else { value }
}

fn choice_label(key: &str, value: &str) -> &'static str {
    match (key, value) {
        ("flow", "") => "Bearer token",
        ("flow", "client_credentials") => "Client credentials",
        ("flow", "password") => "Password",
        ("flow", "authorization_code_pkce") => "Authorization code · PKCE",
        (_, "POST") => "POST",
        (_, "GET") => "GET",
        _ => "",
    }
}

impl Render for AuthForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let text = self.editor.read(cx).value().to_string();
        let seeded = self.seeded.as_ref().is_some_and(|(mode, seeded, stored)| {
            *mode == self.mode && seeded == &text && *stored == self.stored
        });
        if !seeded {
            self.reseed(&text, window, cx);
        }
        let has_secret = self
            .fields
            .iter()
            .any(|field| field.spec.kind == AuthFieldKind::Secret && self.visible(field, cx));
        let prefix = self.prefix;
        let fields = self
            .fields
            .iter()
            .filter_map(|field| {
                if self.visible(field, cx) {
                    Some(self.render_field(field, window, cx))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .when(has_secret, |el| {
                el.child(
                    div().flex().justify_end().child(
                        outline_chip(
                            format!("{prefix}-reveal"),
                            if self.revealed { "Mask" } else { "Reveal" },
                            cx,
                        )
                        .child(icon("eye", 12., palette::text_secondary(cx)))
                        .on_click(cx.listener(|this, _, window, cx| {
                            let revealed = !this.revealed;
                            this.set_revealed(revealed, window, cx);
                        })),
                    ),
                )
            })
            .children(fields)
    }
}
