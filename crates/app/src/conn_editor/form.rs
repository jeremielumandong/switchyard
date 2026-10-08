//! What a database engine plugs into the connection editor: an [`EngineForm`] creates its
//! inputs, lays out its fields and writes them back into a [`DbConnection`]. The editor
//! owns everything shared: the Name field, environment, history, agent access, Test and
//! Save.

use gpui_kit::{App, Context, Window};
use switchyard_core::db::{Engine, SslMode};
use switchyard_core::store::{DbConnection, ProfileId};

use super::{ConnEditor, Select, text_input};

/// A field error: the field's key (None for the whole form) and the message.
pub(crate) type FieldError = (Option<&'static str>, String);

/// One database engine's part of the connection editor.
///
/// To add an engine: implement this in `engines/<engine>.rs` and register it in
/// `engines.rs` (`ALL` for the picker, `form` for the lookup).
pub(crate) trait EngineForm: Sync {
    /// The engine this form edits.
    fn engine(&self) -> Engine;

    /// Placeholder for the Name field.
    fn name_placeholder(&self) -> &'static str;

    /// A new connection's defaults.
    fn new_profile(&self) -> DbConnection {
        DbConnection::new("", self.engine())
    }

    /// Creates the engine's inputs and selects, filled from `d`.
    fn init(&self, d: &DbConnection, f: &mut FieldSet<'_, '_, '_>);

    /// The fields to show below Name, in a 6-column grid, given the current values.
    fn layout(&self, v: &Values<'_>) -> Vec<Field>;

    /// Writes the engine's fields into `d`. Name and the shared settings are set by the
    /// editor.
    fn apply(&self, v: &Values<'_>, d: &mut DbConnection) -> Result<(), FieldError>;

    /// A Driver Manager component `d` needs before it can connect; Test shows its
    /// install card while it is missing.
    fn required_component(&self, _d: &DbConnection) -> Option<&'static str> {
        None
    }
}

/// A field in the form grid. The key names an input or select created in
/// [`EngineForm::init`].
pub(crate) struct Field {
    pub(crate) key: &'static str,
    pub(crate) label: &'static str,
    /// Columns out of 6.
    pub(crate) span: u16,
    pub(crate) mono: bool,
    pub(crate) hint: Option<&'static str>,
}

impl Field {
    /// A full-width field.
    pub(crate) fn new(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            label,
            span: 6,
            mono: false,
            hint: None,
        }
    }

    /// Spans `n` of 6 columns.
    pub(crate) fn span(mut self, n: u16) -> Self {
        self.span = n;
        self
    }

    /// Monospace text (hosts, ids, paths).
    pub(crate) fn mono(mut self) -> Self {
        self.mono = true;
        self
    }

    /// Help text under the field.
    pub(crate) fn hint(mut self, hint: &'static str) -> Self {
        self.hint = Some(hint);
        self
    }

    /// Help text under the field, if any.
    pub(crate) fn hint_opt(mut self, hint: Option<&'static str>) -> Self {
        self.hint = hint;
        self
    }

    /// `password`, half width, stored in the keychain.
    pub(crate) fn password(label: &'static str) -> Self {
        Self::new("password", label)
            .span(3)
            .hint("Stored in the OS keychain")
    }

    /// The `via` select from [`FieldSet::via`].
    pub(crate) fn via() -> Self {
        Self::new("via", "Connect via Host").hint("Opens an ephemeral local port automatically")
    }
}

/// Creates a form's inputs and selects.
pub(crate) struct FieldSet<'a, 'w, 'c> {
    pub(super) editor: &'a mut ConnEditor,
    pub(super) window: &'w mut Window,
    pub(super) cx: &'a mut Context<'c, ConnEditor>,
}

impl FieldSet<'_, '_, '_> {
    /// A text input.
    pub(crate) fn text(&mut self, key: &'static str, value: &str, placeholder: &str) {
        let i = text_input(self.window, self.cx, value, placeholder, false);
        self.editor.inputs.insert(key, i);
    }

    /// The masked `password` input. A stored secret is never shown, only noted in the
    /// placeholder; otherwise `placeholder` is used.
    pub(crate) fn secret(&mut self, d: &DbConnection, placeholder: &str) {
        let ph = if d.secret.is_some() {
            "•••••••• (stored)"
        } else {
            placeholder
        };
        let i = text_input(self.window, self.cx, "", ph, true);
        self.editor.inputs.insert("password", i);
    }

    /// A select of (label, value) options with `value` chosen (the first if absent).
    pub(crate) fn select(
        &mut self,
        key: &'static str,
        options: Vec<(String, String)>,
        value: &str,
    ) {
        let chosen = options.iter().position(|(_, v)| v == value).unwrap_or(0);
        self.editor.selects.insert(key, Select { options, chosen });
    }

    /// The `ssl` select over [`SslMode`].
    pub(crate) fn ssl(&mut self, d: &DbConnection) {
        let options = SslMode::ALL
            .iter()
            .map(|m| (m.label().to_owned(), m.label().to_owned()))
            .collect();
        self.select("ssl", options, d.ssl_mode.label());
    }

    /// The `via` select: direct, or an SSH tunnel through a saved Host.
    pub(crate) fn via(&mut self, d: &DbConnection) {
        let options = self.editor.host_options("None — direct");
        self.select(
            "via",
            options,
            d.via_host.as_ref().map_or("", |h| h.0.as_str()),
        );
    }
}

/// Reads a form's current values.
pub(crate) struct Values<'a> {
    pub(super) editor: &'a ConnEditor,
    pub(super) cx: &'a App,
}

impl Values<'_> {
    /// A text input's trimmed value ("" if there is none).
    pub(crate) fn text(&self, key: &str) -> String {
        self.editor
            .inputs
            .get(key)
            .map(|i| i.read(self.cx).value().trim().to_owned())
            .unwrap_or_default()
    }

    /// A text input's value, None when empty.
    pub(crate) fn opt(&self, key: &str) -> Option<String> {
        Some(self.text(key)).filter(|v| !v.is_empty())
    }

    /// A select's chosen value ("" if there is none).
    pub(crate) fn chosen(&self, key: &str) -> String {
        self.editor.chosen(key)
    }

    /// The `port` input, `default` when empty.
    pub(crate) fn port(&self, default: u16) -> Result<u16, FieldError> {
        let v = self.text("port");
        if v.is_empty() {
            return Ok(default);
        }
        v.parse::<u16>()
            .ok()
            .filter(|p| *p > 0)
            .ok_or_else(|| (Some("port"), "Port must be 1–65535".to_owned()))
    }

    /// The `ssl` select.
    pub(crate) fn ssl(&self) -> SslMode {
        let chosen = self.chosen("ssl");
        SslMode::ALL
            .into_iter()
            .find(|m| m.label() == chosen)
            .unwrap_or_default()
    }

    /// The `via` select.
    pub(crate) fn via(&self) -> Option<ProfileId> {
        Some(self.chosen("via"))
            .filter(|v| !v.is_empty())
            .map(ProfileId)
    }
}
