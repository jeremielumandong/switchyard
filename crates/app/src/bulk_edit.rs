//! Bulk edit of Hosts (MX-6): one change applied to every Host of a folder or to a
//! single Host (move to folder). Each field starts with the value the Hosts share
//! (empty when they differ); only fields the user changes are applied.

use gpui_kit::component::input::InputState;
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, FontWeight, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use switchyard_core::store::{EnvironmentLabel, Host, HostPatch, ProfileId};
use switchyard_core::{Command, RuntimeHandle};

use crate::appearance::{rpx, ts};
use crate::terminal_settings::field;
use crate::theme::palette;
use crate::ui::{self, Kind};

/// Emitted when the dialog closes (applied or cancelled).
pub struct BulkEditClosed;

/// The values the edited Hosts share; `None` where they differ.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shared {
    /// Folder ("" = none).
    pub folder: Option<String>,
    /// Login user.
    pub user: Option<String>,
    /// Startup command ("" = none).
    pub startup: Option<String>,
    /// Start folder ("" = none).
    pub start_dir: Option<String>,
    /// Environment label.
    pub environment: Option<EnvironmentLabel>,
    /// Favorite.
    pub favorite: Option<bool>,
}

fn common<T: PartialEq + Clone>(mut values: impl Iterator<Item = T>) -> Option<T> {
    let first = values.next()?;
    values.all(|v| v == first).then_some(first)
}

impl Shared {
    /// What `hosts` have in common.
    pub fn of(hosts: &[&Host]) -> Self {
        let text =
            |f: fn(&Host) -> Option<String>| common(hosts.iter().map(|h| f(h).unwrap_or_default()));
        Self {
            folder: text(|h| h.folder.clone()),
            user: common(hosts.iter().map(|h| h.user.clone())),
            startup: text(|h| h.startup_command.clone()),
            start_dir: text(|h| h.start_directory.clone()),
            environment: common(hosts.iter().map(|h| h.environment)),
            favorite: common(hosts.iter().map(|h| h.favorite)),
        }
    }
}

/// The patch for the fields changed from `before` (text fields as typed; an empty text
/// field that started empty because the Hosts differed changes nothing).
pub fn build_patch(before: &Shared, after: &Shared) -> HostPatch {
    let text = |b: &Option<String>, a: &Option<String>| -> Option<Option<String>> {
        let a = a.as_deref().unwrap_or("").trim();
        let b = b.as_deref().unwrap_or("");
        (a != b).then(|| (!a.is_empty()).then(|| a.to_owned()))
    };
    let user = text(&before.user, &after.user).flatten();
    HostPatch {
        folder: text(&before.folder, &after.folder),
        environment: after.environment.filter(|e| before.environment != Some(*e)),
        user,
        favorite: after.favorite.filter(|f| before.favorite != Some(*f)),
        startup_command: text(&before.startup, &after.startup),
        start_directory: text(&before.start_dir, &after.start_dir),
        keepalive_secs: None,
    }
}

/// The dialog.
pub struct BulkEditView {
    core: RuntimeHandle,
    ids: Vec<ProfileId>,
    title: String,
    before: Shared,
    folder: Entity<InputState>,
    user: Entity<InputState>,
    startup: Entity<InputState>,
    start_dir: Entity<InputState>,
    environment: Option<EnvironmentLabel>,
    favorite: Option<bool>,
}

impl EventEmitter<BulkEditClosed> for BulkEditView {}

impl BulkEditView {
    /// A dialog editing `hosts`.
    pub fn new(
        core: RuntimeHandle,
        title: String,
        hosts: &[&Host],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let before = Shared::of(hosts);
        let mut input = |v: &Option<String>| {
            let (value, ph) = match v {
                Some(v) => (v.clone(), "None"),
                None => (String::new(), "(several values · unchanged)"),
            };
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(ph)
                    .default_value(value)
            })
        };
        Self {
            core,
            ids: hosts.iter().map(|h| h.id.clone()).collect(),
            title,
            folder: input(&before.folder),
            user: input(&before.user),
            startup: input(&before.startup),
            start_dir: input(&before.start_dir),
            environment: before.environment,
            favorite: before.favorite,
            before,
        }
    }

    fn apply(&mut self, cx: &mut Context<Self>) {
        let value = |e: &Entity<InputState>, b: &Option<String>| {
            let v = e.read(cx).value().to_string();
            // Untouched "several values" stays unchanged.
            if b.is_none() && v.trim().is_empty() {
                None
            } else {
                Some(v)
            }
        };
        let after = Shared {
            folder: value(&self.folder, &self.before.folder),
            user: value(&self.user, &self.before.user),
            startup: value(&self.startup, &self.before.startup),
            start_dir: value(&self.start_dir, &self.before.start_dir),
            environment: self.environment,
            favorite: self.favorite,
        };
        let patch = build_patch(&self.before, &after);
        if !patch.is_empty() {
            self.core.send(Command::UpdateHosts {
                ids: self.ids.clone(),
                patch,
            });
        }
        cx.emit(BulkEditClosed);
    }
}

impl Render for BulkEditView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        div()
            .id("bulk-edit")
            .w(rpx(560.))
            .flex()
            .flex_col()
            .gap(rpx(12.))
            .p(rpx(20.))
            .bg(p.elev)
            .rounded(px(10.))
            .shadow(ui::shadow(&p))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .text_size(ts::TITLE_PLUS)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.title.clone()),
            )
            .child(div().text_size(ts::BODY).text_color(p.fg3).child(format!(
                "{} Host{} · only the fields you change are applied",
                self.ids.len(),
                if self.ids.len() == 1 { "" } else { "s" }
            )))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(rpx(10.))
                    .child(field("Folder", &self.folder, 250., &p))
                    .child(field("User", &self.user, 250., &p))
                    .child(field("Start folder", &self.start_dir, 250., &p))
                    .child(field("Startup command", &self.startup, 250., &p)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rpx(6.))
                    .text_size(ts::BODY)
                    .child(div().w(rpx(90.)).text_color(p.fg3).child("Environment"))
                    .children(EnvironmentLabel::ALL.iter().map(|&e| {
                        let on = self.environment == Some(e);
                        div()
                            .id(SharedString::from(format!("bulk-env-{e:?}")))
                            .flex()
                            .items_center()
                            .gap(rpx(5.))
                            .px(rpx(8.))
                            .py(rpx(3.))
                            .border_1()
                            .border_color(if on { p.acc } else { p.bd2 })
                            .rounded(px(5.))
                            .bg(if on { p.sel } else { p.bg })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.environment = Some(e);
                                cx.notify();
                            }))
                            .child(ui::dot(p.env(e), 6.))
                            .child(e.name())
                    })),
            )
            .child(
                ui::checkbox(
                    "bulk-fav",
                    self.favorite == Some(true),
                    match self.favorite {
                        None => "Favorites (several values · unchanged)",
                        Some(_) => "Show in Favorites",
                    },
                    &p,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.favorite = Some(this.favorite != Some(true));
                    cx.notify();
                })),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(rpx(8.))
                    .child(
                        ui::button("bulk-cancel", "Cancel", Kind::Secondary, &p)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(BulkEditClosed))),
                    )
                    .child(
                        ui::button("bulk-apply", "Apply", Kind::Primary, &p)
                            .on_click(cx.listener(|this, _, _, cx| this.apply(cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str, folder: Option<&str>, user: &str) -> Host {
        let mut h = Host::new(name, "10.0.0.1", user);
        h.folder = folder.map(str::to_owned);
        h
    }

    #[test]
    fn shared_values_and_patch() {
        let a = host("a", Some("Prod"), "deploy");
        let mut b = host("b", Some("Prod"), "root");
        b.startup_command = Some("uptime".into());
        let before = Shared::of(&[&a, &b]);
        assert_eq!(before.folder.as_deref(), Some("Prod"));
        assert_eq!(before.user, None);
        assert_eq!(before.startup, None);
        assert_eq!(before.start_dir.as_deref(), Some(""));

        // Nothing touched: nothing changes.
        assert!(build_patch(&before, &before).is_empty());

        // Rename the folder, set a user for all, clear nothing else.
        let after = Shared {
            folder: Some("Production".into()),
            user: Some("ops".into()),
            ..before.clone()
        };
        let patch = build_patch(&before, &after);
        assert_eq!(patch.folder, Some(Some("Production".into())));
        assert_eq!(patch.user.as_deref(), Some("ops"));
        assert_eq!(patch.startup_command, None);

        // Emptying the shared folder moves the Hosts out of it.
        let after = Shared {
            folder: Some(String::new()),
            ..before.clone()
        };
        assert_eq!(build_patch(&before, &after).folder, Some(None));
    }
}
