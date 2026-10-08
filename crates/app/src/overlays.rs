//! Overlays: command palette host, dialogs (connection editor, Production safety,
//! parameters, settings, history), component sheet, context menu, toast, tunnels.

use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FontWeight, Hsla, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window, div, px,
};
use switchyard_core::remote::ssh::ForwardKind;
use switchyard_core::store::{EnvironmentLabel, HistoryStatus};

use crate::conn_editor::{ConnEditor, ConnKind};
use crate::palette::PaletteView;
use crate::sidebar::{CtxMenu, CtxTarget};
use crate::sql_tab::{PendingRun, SqlTab};
use crate::theme::{MONO, Palette, SANS, ThemeId};
use crate::ui::{self, Kind};
use crate::workspace::Workspace;

/// Settings pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsPage {
    General,
    Editor,
    Appearance,
    Keybindings,
    Drivers,
    Assistant,
    Security,
}

impl SettingsPage {
    const ALL: [SettingsPage; 7] = [
        SettingsPage::General,
        SettingsPage::Editor,
        SettingsPage::Appearance,
        SettingsPage::Keybindings,
        SettingsPage::Drivers,
        SettingsPage::Assistant,
        SettingsPage::Security,
    ];

    fn label(self) -> &'static str {
        match self {
            SettingsPage::General => "General",
            SettingsPage::Editor => "Editor",
            SettingsPage::Appearance => "Appearance",
            SettingsPage::Keybindings => "Keybindings",
            SettingsPage::Drivers => "Drivers",
            SettingsPage::Assistant => "Assistant",
            SettingsPage::Security => "Security",
        }
    }
}

/// The open overlay, if any.
pub enum Overlay {
    /// Command palette / quick switcher.
    Palette(Entity<PaletteView>),
    /// Connection editor.
    ConnEditor(Entity<ConnEditor>),
    /// Production confirmation.
    Safety {
        tab: Entity<SqlTab>,
        pending: PendingRun,
        input: Entity<InputState>,
    },
    /// Parameter values.
    Params {
        tab: Entity<SqlTab>,
        pending: PendingRun,
        inputs: Vec<(String, Entity<InputState>)>,
    },
    /// Settings.
    Settings(SettingsPage),
    /// Component sheet.
    Components,
    /// Query history.
    History(Entity<InputState>),
    /// Hosts found in `~/.ssh/config`, to pick before importing.
    SshImport(SshImportPreview),
}

/// The `~/.ssh/config` import preview.
pub struct SshImportPreview {
    /// File read.
    pub path: std::path::PathBuf,
    /// Entries.
    pub hosts: Vec<switchyard_core::ssh_import::SshImportCandidate>,
    /// Aliases ticked for import.
    pub chosen: std::collections::HashSet<String>,
}

fn scrim(p: &Palette, top: bool) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id("scrim")
        .absolute()
        .inset_0()
        .bg(p.scrim)
        .flex()
        .justify_center()
        .when(top, |d| d.items_start().pt(px(72.)))
        .when(!top, |d| d.items_center())
        .occlude()
}

fn dialog(p: &Palette, width: f32) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id("dialog")
        .w(px(width))
        .bg(p.elev)
        .rounded(px(10.))
        .shadow(ui::shadow(p))
        .overflow_hidden()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

impl Workspace {
    pub(crate) fn render_overlays(
        &mut self,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if self.tunnels_open {
            out.push(self.render_tunnels(p, cx));
        }
        if let Some(ctx) = self.ctx.clone() {
            out.push(self.render_ctx_menu(&ctx, p, cx));
        }
        let overlay = match &self.overlay {
            None => None,
            Some(Overlay::Palette(view)) => Some(
                scrim(p, true)
                    .key_context("Overlay")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, w, cx| this.dismiss(w, cx)),
                    )
                    .child(
                        div()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(view.clone()),
                    )
                    .into_any_element(),
            ),
            Some(Overlay::ConnEditor(ed)) => Some(
                scrim(p, false)
                    .key_context("Overlay")
                    .p(px(20.))
                    .child(ed.clone())
                    .into_any_element(),
            ),
            Some(Overlay::Safety {
                tab,
                pending,
                input,
            }) => Some(self.render_safety(tab.clone(), pending.clone(), input.clone(), p, cx)),
            Some(Overlay::Params {
                tab,
                pending,
                inputs,
            }) => Some(self.render_params(tab.clone(), pending.clone(), inputs.clone(), p, cx)),
            Some(Overlay::Settings(page)) => Some(self.render_settings(*page, p, window, cx)),
            Some(Overlay::Components) => Some(self.render_components(p, cx)),
            Some(Overlay::History(input)) => Some(self.render_history(input.clone(), p, cx)),
            Some(Overlay::SshImport(preview)) => Some(self.render_ssh_import(preview, p, cx)),
        };
        out.extend(overlay);
        out.extend(self.render_ssh_prompt(p, cx));
        if let Some(t) = self.toast.clone() {
            out.push(
                div()
                    .absolute()
                    .right(px(16.))
                    .bottom(px(36.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(14.))
                    .py(px(10.))
                    .bg(p.elev)
                    .rounded(px(8.))
                    .shadow(ui::shadow(p))
                    .text_size(px(12.5))
                    .child(ui::dot(p.dev, 7.))
                    .child(t)
                    .into_any_element(),
            );
        }
        out
    }

    fn render_tunnels(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        // Saved forwards that are not running, with their Host.
        let saved: Vec<_> = self
            .profiles
            .hosts()
            .flat_map(|h| {
                h.forwards
                    .iter()
                    .map(move |f| (h.id.clone(), h.name.clone(), f.clone()))
            })
            .filter(|(host_id, _, f)| {
                !self.tunnels.iter().any(|t| {
                    t.host_id == host_id.0 && t.forward_id.as_deref() == Some(f.id.as_str())
                })
            })
            .collect();
        div()
            .id("tunnels")
            .absolute()
            .left(px(250.))
            .bottom(px(30.))
            .w(px(540.))
            .max_h(px(480.))
            .overflow_y_scroll()
            .bg(p.elev)
            .rounded(px(8.))
            .shadow(ui::shadow(p))
            .overflow_hidden()
            .occlude()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .px(px(12.))
                    .py(px(10.))
                    .border_b_1()
                    .border_color(p.bd)
                    .text_size(px(12.))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("Tunnels"))
                    .child(
                        div()
                            .id("tunnels-close")
                            .text_color(p.fg3)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.tunnels_open = false;
                                cx.notify();
                            }))
                            .child("×"),
                    ),
            )
            .when(self.tunnels.is_empty(), |d| {
                d.child(
                    div()
                        .px(px(12.))
                        .py(px(14.))
                        .text_size(px(12.))
                        .text_color(p.fg2)
                        .child("No active tunnels. Start a saved forward below, add forwards in a Host's settings, or connect a database “via Host”."),
                )
            })
            .children(self.tunnels.iter().map(|t| {
                use switchyard_core::remote::ssh::TunnelStatus;
                let (label, color) = match &t.status {
                    TunnelStatus::Active => ("Active".to_owned(), p.dev),
                    TunnelStatus::Reconnecting => ("Reconnecting".to_owned(), p.stg),
                    TunnelStatus::Failed(_) => ("Failed".to_owned(), p.prod),
                    TunnelStatus::Stopped => ("Stopped".to_owned(), p.fg3),
                };
                let tooltip = match &t.status {
                    TunnelStatus::Failed(e) => e.clone(),
                    _ => format!(
                        "{} connection{} · ↑ {} ↓ {}",
                        t.connections,
                        if t.connections == 1 { "" } else { "s" },
                        ui::bytes(t.bytes_up),
                        ui::bytes(t.bytes_down)
                    ),
                };
                let id = t.id;
                div()
                    .id(("tunnel", id as usize))
                    .h(px(32.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(12.))
                    .border_b_1()
                    .border_color(p.line)
                    .font_family(MONO)
                    .text_size(px(11.5))
                    .child(
                        div()
                            .w(px(16.))
                            .flex_none()
                            .text_color(p.fg3)
                            .child(t.kind.flag()),
                    )
                    .child(div().w(px(54.)).flex_none().child(format!(":{}", t.local_port)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(p.fg2)
                            .child(match t.kind {
                                ForwardKind::Remote => format!("{} :{} → {}", t.host, t.local_port, t.remote),
                                ForwardKind::Dynamic => format!("{} · SOCKS on {}", t.host, t.listen),
                                ForwardKind::Local => format!("{} → {}", t.host, t.remote),
                            }),
                    )
                    .child(
                        div()
                            .id(("tunnel-status", id as usize))
                            .w(px(92.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(5.))
                            .font_family(SANS)
                            .text_color(color)
                            .tooltip(move |w, cx| gpui_kit::component::tooltip::Tooltip::new(tooltip.clone()).build(w, cx))
                            .child(ui::dot(color, 6.))
                            .child(label),
                    )
                    .child(
                        div()
                            .w(px(60.))
                            .flex_none()
                            .flex()
                            .justify_end()
                            .text_color(p.fg3)
                            .child(ui::bytes(t.bytes_up + t.bytes_down)),
                    )
                    .child(
                        div()
                            .id(("tunnel-stop", id as usize))
                            .w(px(36.))
                            .flex_none()
                            .flex()
                            .justify_end()
                            .font_family(SANS)
                            .text_color(p.fg3)
                            .hover(|s| s.text_color(p.prod))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.core.send(switchyard_core::Command::StopTunnel { id });
                                cx.notify();
                            }))
                            .child("Stop"),
                    )
            }))
            .when(!saved.is_empty(), |d| {
                d.child(
                    div()
                        .px(px(12.))
                        .pt(px(10.))
                        .pb(px(4.))
                        .text_size(px(11.))
                        .text_color(p.fg3)
                        .child("Saved forwards"),
                )
                .children(saved.into_iter().enumerate().map(|(i, (host_id, host, f))| {
                    let forward = f.id.clone();
                    let label = if f.name.trim().is_empty() {
                        f.summary()
                    } else {
                        format!("{} · {}", f.name.trim(), f.summary())
                    };
                    div()
                        .id(("saved-forward", i))
                        .h(px(30.))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(12.))
                        .border_b_1()
                        .border_color(p.line)
                        .text_size(px(11.5))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(p.fg2)
                                .child(format!("{host} · {label}")),
                        )
                        .when(f.auto_start, |d| {
                            d.child(div().flex_none().text_color(p.fg3).child("auto"))
                        })
                        .child(
                            div()
                                .id(("saved-forward-start", i))
                                .w(px(36.))
                                .flex_none()
                                .flex()
                                .justify_end()
                                .text_color(p.acc)
                                .hover(|s| s.opacity(0.8))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.core.send(switchyard_core::Command::StartForward {
                                        host: host_id.clone(),
                                        forward: forward.clone(),
                                    });
                                    cx.notify();
                                }))
                                .child("Start"),
                        )
                }))
            })
            .into_any_element()
    }

    fn render_ctx_menu(&self, ctx: &CtxMenu, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let items: Vec<(&'static str, &'static str, bool, SharedString)> = match &ctx.target {
            CtxTarget::Object(_, _, kind) if kind.is_relation() => vec![
                ("open", "Open data (first 100 rows)", false, "↵".into()),
                ("select", "Generate SELECT", false, "".into()),
                ("insert", "Generate INSERT", false, "".into()),
                ("update", "Generate UPDATE", false, "".into()),
                ("delete", "Generate DELETE", false, "".into()),
                ("ddl", "View DDL", false, "".into()),
                (
                    "copy",
                    "Copy qualified name",
                    false,
                    ui::keys("⌘C", "Ctrl+C"),
                ),
                ("-", "", false, "".into()),
                ("truncate", "Truncate…", true, "".into()),
                ("drop", "Drop…", true, "".into()),
            ],
            CtxTarget::Object(..) => vec![
                ("ddl", "View DDL", false, "".into()),
                (
                    "copy",
                    "Copy qualified name",
                    false,
                    ui::keys("⌘C", "Ctrl+C"),
                ),
                ("-", "", false, "".into()),
                ("drop", "Drop…", true, "".into()),
            ],
            CtxTarget::Tab(_) => vec![
                ("close", "Close", false, ui::keys("⌘W", "Ctrl+W")),
                ("close_others", "Close others", false, "".into()),
                ("close_right", "Close to the right", false, "".into()),
                ("close_left", "Close to the left", false, "".into()),
                ("-", "", false, "".into()),
                ("close_all", "Close all", false, "".into()),
            ],
            CtxTarget::Profile(_) => vec![
                ("open", "Open", false, "↵".into()),
                ("edit", "Edit…", false, "".into()),
                ("-", "", false, "".into()),
                ("delete", "Delete", true, "".into()),
            ],
        };
        let target = ctx.target.clone();
        div()
            .id("ctx-layer")
            .absolute()
            .inset_0()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.ctx = None;
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, _, cx| {
                    this.ctx = None;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("ctx-menu")
                    .absolute()
                    .left(ctx.at.x)
                    .top(ctx.at.y)
                    .w(px(230.))
                    .p(px(4.))
                    .bg(p.elev)
                    .rounded(px(7.))
                    .shadow(ui::shadow(p))
                    .text_size(px(12.5))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .children(items.into_iter().enumerate().map(
                        |(i, (action, label, danger, key))| {
                            if action == "-" {
                                return div().h(px(1.)).bg(p.bd).my(px(4.)).into_any_element();
                            }
                            let target = target.clone();
                            div()
                                .id(("ctx", i))
                                .h(px(26.))
                                .flex()
                                .items_center()
                                .justify_between()
                                .px(px(8.))
                                .rounded(px(4.))
                                .text_color(if danger { p.prod } else { p.fg })
                                .hover(|s| s.bg(p.sel))
                                .on_click(cx.listener(move |this, _, w, cx| {
                                    this.ctx = None;
                                    this.ctx_action(action, &target, w, cx);
                                }))
                                .child(label)
                                .child(
                                    div()
                                        .font_family(MONO)
                                        .text_size(px(10.5))
                                        .text_color(p.fg3)
                                        .child(key),
                                )
                                .into_any_element()
                        },
                    )),
            )
            .into_any_element()
    }

    fn ctx_action(
        &mut self,
        action: &'static str,
        target: &CtxTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match target {
            CtxTarget::Object(s, n, k) => {
                self.object_action(action, s.clone(), n.clone(), *k, window, cx)
            }
            CtxTarget::Tab(ix) => self.close_tabs(action, *ix, cx),
            CtxTarget::Profile(id) => match action {
                "open" => self.open_profile(id, window, cx),
                "edit" => {
                    if let Some(p) = self.profiles.all.iter().find(|p| p.id() == id).cloned() {
                        self.open_conn_editor(ConnKind::Postgres, Some(p), window, cx);
                    }
                }
                "delete" => self
                    .core
                    .send(switchyard_core::Command::DeleteProfile { id: id.clone() }),
                _ => {}
            },
        }
        cx.notify();
    }

    fn render_safety(
        &self,
        tab: Entity<SqlTab>,
        pending: PendingRun,
        input: Entity<InputState>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(first) = pending.destructive.first().cloned() else {
            return div().into_any_element();
        };
        let conn = tab.read(cx).connection.clone();
        let conn_label = conn
            .as_ref()
            .map(|c| {
                let via = c
                    .via_host
                    .as_ref()
                    .and_then(|h| self.profiles.host(h))
                    .map(|h| format!(" @ {}", h.name))
                    .unwrap_or_default();
                format!("PRODUCTION · {}{via}", c.name)
            })
            .unwrap_or_default();
        let manual = tab.read(cx).txn_open;
        let short = first
            .object
            .rsplit('.')
            .next()
            .unwrap_or(&first.object)
            .trim_matches('"')
            .to_owned();
        let typed = input.read(cx).value().trim().to_owned();
        let ok = typed == short;
        let more = pending.destructive.len().saturating_sub(1);
        let sql_line = first.sql.lines().next().unwrap_or("").to_owned();
        let action_label = match first.label.as_str() {
            "no WHERE" if first.headline.starts_with("Delete") => "Delete all rows".to_owned(),
            "no WHERE" => "Update all rows".to_owned(),
            "TRUNCATE" => "Truncate".to_owned(),
            other => other.to_owned(),
        };
        scrim(p, false)
            .key_context("Overlay")
            .child(
                dialog(p, 560.)
                    .border_t_3()
                    .border_color(p.prod)
                    .child(
                        div()
                            .px(px(20.))
                            .pt(px(16.))
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .child(
                                div()
                                    .font_family(MONO)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_size(px(10.))
                                    .text_color(p.prod)
                                    .child(conn_label),
                            )
                            .child(
                                div()
                                    .text_size(px(16.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(first.headline.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .text_color(p.fg2)
                                    .child(first.explanation.clone()),
                            ),
                    )
                    .child(
                        div()
                            .mx(px(20.))
                            .my(px(14.))
                            .px(px(12.))
                            .py(px(10.))
                            .border_1()
                            .border_color(p.bd)
                            .rounded(px(6.))
                            .bg(p.bg)
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .font_family(MONO)
                            .text_size(px(12.5))
                            .child(div().text_color(p.fg3).child(first.line.to_string()))
                            .child(
                                div()
                                    .flex_1()
                                    .truncate()
                                    .text_color(p.sx_kw)
                                    .child(sql_line),
                            )
                            .child(
                                div()
                                    .px(px(3.))
                                    .rounded(px(3.))
                                    .bg(p.prod_bg)
                                    .text_color(p.prod)
                                    .child(first.label.clone()),
                            ),
                    )
                    .child(
                        div()
                            .px(px(20.))
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .text_size(px(12.5))
                            .child(kv("Object", &first.object, true, p))
                            .child(kv(
                                "Transaction",
                                if manual {
                                    "Manual — you can still roll back"
                                } else {
                                    "Auto-commit — this cannot be rolled back"
                                },
                                false,
                                p,
                            ))
                            .when(more > 0, |d| {
                                d.child(kv(
                                    "Also",
                                    &format!("{more} more destructive statement(s) in this run"),
                                    false,
                                    p,
                                ))
                            }),
                    )
                    .child(
                        div()
                            .px(px(20.))
                            .pt(px(16.))
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .child(
                                div()
                                    .flex()
                                    .gap(px(4.))
                                    .text_size(px(12.))
                                    .text_color(p.fg2)
                                    .child("Type")
                                    .child(
                                        div()
                                            .font_family(MONO)
                                            .text_color(p.fg)
                                            .child(short.clone()),
                                    )
                                    .child("to confirm"),
                            )
                            .child(
                                div()
                                    .h(px(30.))
                                    .flex()
                                    .items_center()
                                    .px(px(10.))
                                    .border_1()
                                    .border_color(p.bd2)
                                    .rounded(px(6.))
                                    .bg(p.bg)
                                    .font_family(MONO)
                                    .child(
                                        Input::new(&input).appearance(false).text_size(px(12.5)),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(6.))
                            .px(px(20.))
                            .py(px(16.))
                            .child(
                                ui::button("safety-cancel", "Cancel", Kind::Secondary, p)
                                    .h(px(30.))
                                    .on_click(cx.listener(|this, _, w, cx| this.dismiss(w, cx))),
                            )
                            .child(
                                ui::button("safety-run", action_label, Kind::Destructive, p)
                                    .h(px(30.))
                                    .opacity(if ok { 1.0 } else { 0.4 })
                                    .on_click(cx.listener(move |this, _, w, cx| {
                                        if !ok {
                                            return;
                                        }
                                        let pending = pending.clone();
                                        tab.update(cx, |t, cx| t.execute(pending, true, cx));
                                        this.dismiss(w, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_params(
        &self,
        tab: Entity<SqlTab>,
        pending: PendingRun,
        inputs: Vec<(String, Entity<InputState>)>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let fields: Vec<AnyElement> = inputs
            .iter()
            .map(|(name, input)| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .w(px(120.))
                            .font_family(MONO)
                            .text_size(px(12.5))
                            .text_color(p.fg)
                            .child(name.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .h(px(28.))
                            .flex()
                            .items_center()
                            .px(px(9.))
                            .border_1()
                            .border_color(p.bd2)
                            .rounded(px(6.))
                            .bg(p.bg)
                            .child(Input::new(input).appearance(false).text_size(px(12.5))),
                    )
                    .into_any_element()
            })
            .collect();
        scrim(p, false)
            .key_context("Overlay")
            .child(
                dialog(p, 480.)
                    .child(
                        div()
                            .px(px(20.))
                            .pt(px(16.))
                            .pb(px(10.))
                            .child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).child("Parameter values"))
                            .child(div().text_size(px(12.5)).text_color(p.fg2).child("Values are sent as typed parameters, never spliced into the SQL. Type NULL for null.")),
                    )
                    .child(div().px(px(20.)).flex().flex_col().gap(px(8.)).children(fields))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(6.))
                            .px(px(20.))
                            .py(px(16.))
                            .child(ui::button("params-cancel", "Cancel", Kind::Ghost, p).on_click(cx.listener(|this, _, w, cx| this.dismiss(w, cx))))
                            .child(ui::button("params-run", "Run", Kind::Primary, p).on_click(cx.listener(move |this, _, w, cx| {
                                let values: Vec<(String, String)> =
                                    inputs.iter().map(|(n, i)| (n.clone(), i.read(cx).value().to_string())).collect();
                                let bound = tab.read(cx).bind(pending.clone(), &values);
                                this.dismiss(w, cx);
                                if bound.destructive.is_empty() {
                                    tab.update(cx, |t, cx| t.execute(bound, false, cx));
                                } else {
                                    let input = cx.new(|cx| InputState::new(w, cx));
                                    input.update(cx, |i, cx| i.focus(w, cx));
                                    this.overlay = Some(Overlay::Safety { tab: tab.clone(), pending: bound, input });
                                }
                                cx.notify();
                            }))),
                    ),
            )
            .into_any_element()
    }

    fn render_history(
        &self,
        input: Entity<InputState>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rows: Vec<AnyElement> = self
            .history
            .iter()
            .take(300)
            .enumerate()
            .map(|(i, h)| {
                let sql = h.sql.clone();
                let color: Hsla = match h.status {
                    HistoryStatus::Ok => p.dev,
                    HistoryStatus::Error => p.prod,
                    HistoryStatus::Cancelled => p.stg,
                };
                let first_line = h.sql.lines().next().unwrap_or("").to_owned();
                div()
                    .id(("hist", i))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .h(px(30.))
                    .px(px(10.))
                    .rounded(px(6.))
                    .hover(|s| s.bg(p.sel))
                    .on_click(cx.listener(move |this, _, w, cx| {
                        if let Some(t) = this.active_sql() {
                            t.update(cx, |t, cx| t.insert_text(&sql, w, cx));
                        }
                        this.dismiss(w, cx);
                    }))
                    .child(ui::dot(color, 6.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(MONO)
                            .text_size(px(12.))
                            .child(first_line),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(p.fg3)
                            .whitespace_nowrap()
                            .child(h.connection_name.clone()),
                    )
                    .child(
                        div()
                            .w(px(90.))
                            .flex()
                            .justify_end()
                            .font_family(MONO)
                            .text_size(px(11.))
                            .text_color(p.fg2)
                            .child(match h.rows {
                                Some(r) if r > 0 => format!("{} rows", ui::thousands(r as u64)),
                                _ => format!("{} ms", h.duration_ms),
                            }),
                    )
                    .when(!h.tags.is_empty(), |d| {
                        d.child(
                            div()
                                .font_family(MONO)
                                .text_size(px(10.))
                                .text_color(p.acc)
                                .child(h.tags.join(" ")),
                        )
                    })
                    .when(h.has_plan, |d| {
                        let (id, sql) = (h.id, h.sql.clone());
                        d.child(
                            ui::button(("hist-plan", i), "Plan", Kind::Secondary, p)
                                .h(px(20.))
                                .px(px(7.))
                                .text_size(px(11.))
                                .on_click(cx.listener(move |this, _, w, cx| {
                                    cx.stop_propagation();
                                    this.open_saved_plan(id, &sql, w, cx);
                                    this.dismiss(w, cx);
                                })),
                        )
                    })
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();
        scrim(p, true)
            .key_context("Overlay")
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, w, cx| this.dismiss(w, cx)))
            .child(
                dialog(p, 760.)
                    .child(
                        div()
                            .h(px(46.))
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .px(px(14.))
                            .border_b_1()
                            .border_color(p.bd)
                            .child(
                                div()
                                    .px(px(6.))
                                    .py(px(2.))
                                    .rounded(px(4.))
                                    .bg(p.hover)
                                    .font_family(MONO)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_size(px(10.))
                                    .text_color(p.fg2)
                                    .child("HISTORY"),
                            )
                            .child(div().flex_1().child(Input::new(&input).appearance(false).text_size(px(14.)))),
                    )
                    .child(
                        div()
                            .id("hist-list")
                            .max_h(px(440.))
                            .overflow_y_scroll()
                            .p(px(4.))
                            .children(rows)
                            .when(empty, |d| d.child(div().p(px(24.)).text_color(p.fg3).text_size(px(12.5)).child("No history yet. Every executed statement is recorded here."))),
                    ),
            )
            .into_any_element()
    }

    fn render_ssh_import(
        &self,
        preview: &SshImportPreview,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let new_count = preview.hosts.iter().filter(|h| !h.exists).count();
        let chosen = preview.chosen.len();
        let rows: Vec<AnyElement> = preview
            .hosts
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let on = preview.chosen.contains(&h.alias);
                let alias = h.alias.clone();
                let via = (!h.via.is_empty()).then(|| format!("via {}", h.via.join(" → ")));
                div()
                    .id(("ssh-import", i))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .h(px(34.))
                    .px(px(12.))
                    .rounded(px(6.))
                    .when(!h.exists, |d| {
                        d.hover(|s| s.bg(p.sel))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(Overlay::SshImport(pr)) = &mut this.overlay
                                    && !pr.chosen.remove(&alias)
                                {
                                    pr.chosen.insert(alias.clone());
                                }
                                cx.notify();
                            }))
                    })
                    .when(h.exists, |d| d.opacity(0.55))
                    .child(ui::checkbox(("ssh-import-check", i), on, "", p))
                    .child(
                        div()
                            .w(px(150.))
                            .flex_none()
                            .truncate()
                            .text_size(px(12.5))
                            .font_weight(FontWeight::MEDIUM)
                            .child(h.alias.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(MONO)
                            .text_size(px(11.5))
                            .text_color(p.fg2)
                            .child(h.address.clone()),
                    )
                    .when_some(via, |d, v| {
                        d.child(
                            div()
                                .flex_none()
                                .font_family(MONO)
                                .text_size(px(11.))
                                .text_color(p.acc)
                                .child(v),
                        )
                    })
                    .child(
                        div()
                            .w(px(150.))
                            .flex_none()
                            .truncate()
                            .text_size(px(11.5))
                            .text_color(p.fg3)
                            .child(if h.exists {
                                "already saved".to_owned()
                            } else {
                                h.auth.clone()
                            }),
                    )
                    .into_any_element()
            })
            .collect();
        let all_new: Vec<String> = preview
            .hosts
            .iter()
            .filter(|h| !h.exists)
            .map(|h| h.alias.clone())
            .collect();
        let toggle_all = chosen < new_count;
        scrim(p, true)
            .key_context("Overlay")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, w, cx| this.dismiss(w, cx)),
            )
            .child(
                dialog(p, 760.)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .px(px(16.))
                            .py(px(12.))
                            .border_b_1()
                            .border_color(p.bd)
                            .child(
                                div()
                                    .text_size(px(14.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Import Hosts"),
                            )
                            .child(
                                div()
                                    .font_family(MONO)
                                    .text_size(px(11.))
                                    .text_color(p.fg3)
                                    .child(format!(
                                        "{} · {} entries, {new_count} new",
                                        preview.path.display(),
                                        preview.hosts.len()
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .id("ssh-import-list")
                            .max_h(px(420.))
                            .overflow_y_scroll()
                            .p(px(4.))
                            .children(rows)
                            .when(preview.hosts.is_empty(), |d| {
                                d.child(
                                    div()
                                        .p(px(24.))
                                        .text_color(p.fg3)
                                        .text_size(px(12.5))
                                        .child("No Host entries (wildcard patterns are skipped)."),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .px(px(16.))
                            .py(px(12.))
                            .border_t_1()
                            .border_color(p.bd)
                            .child(
                                ui::button(
                                    "ssh-import-all",
                                    if toggle_all {
                                        "Select all new"
                                    } else {
                                        "Select none"
                                    },
                                    Kind::Ghost,
                                    p,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        if let Some(Overlay::SshImport(pr)) = &mut this.overlay {
                                            pr.chosen = if toggle_all {
                                                all_new.iter().cloned().collect()
                                            } else {
                                                Default::default()
                                            };
                                        }
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(div().flex_1())
                            .child(
                                ui::button("ssh-import-cancel", "Cancel", Kind::Ghost, p)
                                    .on_click(cx.listener(|this, _, w, cx| this.dismiss(w, cx))),
                            )
                            .child(
                                ui::button(
                                    "ssh-import-go",
                                    match chosen {
                                        1 => "Import 1 Host".to_owned(),
                                        n => format!("Import {n} Hosts"),
                                    },
                                    Kind::Primary,
                                    p,
                                )
                                .when(chosen == 0, |b| b.opacity(0.5))
                                .on_click(cx.listener(
                                    move |this, _, w, cx| {
                                        let Some(Overlay::SshImport(pr)) = &this.overlay else {
                                            return;
                                        };
                                        if pr.chosen.is_empty() {
                                            return;
                                        }
                                        // Keep the file's order.
                                        let only: Vec<String> = pr
                                            .hosts
                                            .iter()
                                            .filter(|h| pr.chosen.contains(&h.alias))
                                            .map(|h| h.alias.clone())
                                            .collect();
                                        this.core.send(switchyard_core::Command::ImportSshConfig {
                                            only: Some(only),
                                        });
                                        this.dismiss(w, cx);
                                    },
                                )),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_settings(
        &mut self,
        page: SettingsPage,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let body: AnyElement = match page {
            SettingsPage::Drivers => self.render_drivers(p, window, cx),
            SettingsPage::Assistant => {
                let view = match &self.assistant_view {
                    Some(v) => v.clone(),
                    None => {
                        let (core, s, comps) = (
                            self.core.clone(),
                            self.assistant.clone(),
                            self.components.clone(),
                        );
                        let v = cx.new(|cx| {
                            crate::assistant_settings::AssistantSettingsView::new(
                                core, &s, &comps, window, cx,
                            )
                        });
                        cx.subscribe(&v, |this, _, ev, cx| {
                            let crate::assistant_settings::AssistantSettingsEvent::Saved(s) = ev;
                            this.assistant = s.clone();
                            cx.notify();
                        })
                        .detach();
                        self.assistant_view = Some(v.clone());
                        v
                    }
                };
                crate::assistant_settings::element(&view)
            }
            SettingsPage::Appearance => div()
                .flex()
                .flex_col()
                .gap(px(18.))
                .p(px(18.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(p.fg2)
                                .child("Theme"),
                        )
                        .child(div().flex().flex_wrap().gap(px(10.)).children(
                            ThemeId::ALL.into_iter().map(|id| {
                                theme_card(id, id == p.id, p).on_click(
                                    cx.listener(move |this, _, w, cx| this.set_theme(id, w, cx)),
                                )
                            }),
                        )),
                )
                .child(setting_row("Editor font", "Geist Mono · 12.5", true, p))
                .child(setting_row("Row height", "Compact · 26 px", false, p))
                .into_any_element(),
            SettingsPage::General => div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .p(px(18.))
                .child(setting_row("Workspace", &self.workspace_name, false, p))
                .child(setting_row(
                    "Default fetch limit",
                    "10,000 rows · Fetch all continues",
                    true,
                    p,
                ))
                .child(setting_row(
                    "Query history",
                    "On · per-connection switch in the connection",
                    false,
                    p,
                ))
                .child(setting_row(
                    "Telemetry",
                    "Off · crash reports are opt-in",
                    false,
                    p,
                ))
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .pt(px(8.))
                        .child(
                            ui::button("set-export", "Export profiles…", Kind::Secondary, p)
                                .on_click(cx.listener(|this, _, w, cx| {
                                    this.run_command(
                                        crate::actions::CommandId::ExportProfiles,
                                        w,
                                        cx,
                                    )
                                })),
                        )
                        .child(
                            ui::button(
                                "set-import-ssh",
                                "Import ~/.ssh/config",
                                Kind::Secondary,
                                p,
                            )
                            .on_click(cx.listener(|this, _, w, cx| {
                                this.run_command(crate::actions::CommandId::ImportSshConfig, w, cx)
                            })),
                        ),
                )
                .into_any_element(),
            SettingsPage::Editor => div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .p(px(18.))
                .child(setting_row(
                    "Highlighting",
                    "tree-sitter SQL · PostgreSQL / T-SQL keywords",
                    false,
                    p,
                ))
                .child(setting_row(
                    "Statement splitting",
                    "; with dollar quotes (PostgreSQL) · GO batches (T-SQL)",
                    false,
                    p,
                ))
                .child(setting_row(
                    "Autosave",
                    "Continuous · restored after restart",
                    false,
                    p,
                ))
                .child(setting_row("Diagnostics", "sqlparser, live", false, p))
                .into_any_element(),
            SettingsPage::Keybindings => {
                let rows = [
                    ("Command palette", ui::keys("⇧⌘P", "Ctrl+Shift+P")),
                    ("Quick switch connection", ui::keys("⌘P", "Ctrl+P")),
                    ("Run statement at cursor", ui::keys("⌘↵", "Ctrl+Enter")),
                    ("Run script", ui::keys("⇧⌘↵", "Ctrl+Shift+Enter")),
                    ("Stop query", ui::keys("⌘.", "Ctrl+.")),
                    (
                        "New terminal on current Host",
                        ui::keys("⌘T", "Ctrl+Shift+T"),
                    ),
                    ("New SQL tab", ui::keys("⌥⌘N", "Ctrl+Alt+N")),
                    ("Format SQL", ui::keys("⇧⌘F", "Ctrl+Shift+F")),
                    ("Query history", ui::keys("⇧⌘H", "Ctrl+Shift+H")),
                    ("Toggle sidebar", ui::keys("⌘B", "Ctrl+B")),
                    ("Settings", ui::keys("⌘,", "Ctrl+,")),
                ];
                div()
                    .flex()
                    .flex_col()
                    .p(px(18.))
                    .children(rows.into_iter().map(|(l, k)| {
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .h(px(32.))
                            .border_b_1()
                            .border_color(p.line)
                            .text_size(px(12.5))
                            .child(l)
                            .child(ui::kbd(k, p))
                    }))
                    .into_any_element()
            }
            SettingsPage::Security => {
                let (backend, locked) = self.secret_backend;
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .p(px(18.))
                    .child(setting_row(
                        "Secret storage",
                        if backend.is_empty() {
                            "detecting…"
                        } else {
                            backend
                        },
                        false,
                        p,
                    ))
                    .child(setting_row(
                        "Vault",
                        if locked {
                            "Locked · unlock with the master password when a secret is needed"
                        } else {
                            "Unlocked or not in use"
                        },
                        false,
                        p,
                    ))
                    .child(setting_row(
                        "TLS",
                        "rustls · certificate verification on",
                        false,
                        p,
                    ))
                    .child(setting_row(
                        "SSH host keys",
                        "Strict checking · changed keys block the connection",
                        false,
                        p,
                    ))
                    .child(setting_row(
                        "Production guards",
                        "Confirm DROP, TRUNCATE, DELETE/UPDATE without WHERE",
                        false,
                        p,
                    ))
                    .into_any_element()
            }
        };
        scrim(p, false)
            .key_context("Overlay")
            .track_focus(&self.overlay_focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, w, cx| this.dismiss(w, cx)),
            )
            .child(
                dialog(p, 920.)
                    .h(px(600.))
                    .flex()
                    .child(
                        div()
                            .w(px(190.))
                            .flex_none()
                            .bg(p.panel)
                            .border_r_1()
                            .border_color(p.bd)
                            .px(px(8.))
                            .py(px(14.))
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .px(px(8.))
                                    .pb(px(10.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_size(px(14.))
                                    .child("Settings"),
                            )
                            .children(SettingsPage::ALL.into_iter().map(|pg| {
                                let active = pg == page;
                                div()
                                    .id(pg.label())
                                    .h(px(28.))
                                    .flex()
                                    .items_center()
                                    .px(px(8.))
                                    .rounded(px(6.))
                                    .text_size(px(12.5))
                                    .text_color(if active { p.fg } else { p.fg2 })
                                    .when(active, |d| d.bg(p.sel))
                                    .hover(|s| s.bg(p.hover))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.overlay = Some(Overlay::Settings(pg));
                                        cx.notify();
                                    }))
                                    .child(pg.label())
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .h(px(46.))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .px(px(18.))
                                    .border_b_1()
                                    .border_color(p.bd)
                                    .child(
                                        div()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_size(px(14.))
                                            .child(page.label()),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        div()
                                            .id("settings-close")
                                            .px(px(8.))
                                            .py(px(2.))
                                            .rounded(px(4.))
                                            .text_color(p.fg3)
                                            .hover(|s| s.bg(p.hover))
                                            .on_click(
                                                cx.listener(|this, _, w, cx| this.dismiss(w, cx)),
                                            )
                                            .child("×"),
                                    ),
                            )
                            .child(
                                div()
                                    .id("settings-body")
                                    .flex_1()
                                    .overflow_y_scroll()
                                    .child(body),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_components(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let card = |title: &'static str| {
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .p(px(14.))
                .border_1()
                .border_color(p.bd)
                .rounded(px(8.))
                .bg(p.surface)
                .child(
                    div()
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.fg3)
                        .child(title),
                )
        };
        let envs = card("ENVIRONMENT").children(EnvironmentLabel::ALL.iter().map(|e| {
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .child(ui::dot(p.env(*e), 8.))
                .child(ui::env_badge_solid(*e, p))
                .child(ui::env_badge(*e, p))
                .child(
                    div()
                        .w(px(60.))
                        .h(px(22.))
                        .rounded_t(px(4.))
                        .bg(p.panel)
                        .border_t_2()
                        .border_color(p.env(*e)),
                )
                .child(div().text_size(px(12.)).text_color(p.fg2).child(e.name()))
        }));
        let buttons = card("BUTTONS")
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(8.))
                    .child(ui::button_with_key(
                        "c-run",
                        "Run",
                        ui::keys("⌘↵", "Ctrl+Enter"),
                        Kind::Primary,
                        p,
                    ))
                    .child(ui::button("c-sec", "Secondary", Kind::Secondary, p))
                    .child(ui::button("c-ghost", "Ghost", Kind::Ghost, p))
                    .child(ui::button("c-dest", "Destructive", Kind::Destructive, p))
                    .child(
                        ui::button("c-dis", "Disabled", Kind::Secondary, p)
                            .text_color(p.fg3)
                            .border_color(p.bd),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .h(px(28.))
                            .flex()
                            .items_center()
                            .px(px(9.))
                            .border_1()
                            .border_color(p.bd2)
                            .rounded(px(6.))
                            .bg(p.bg)
                            .text_size(px(12.5))
                            .child("Input value"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .h(px(28.))
                            .flex()
                            .items_center()
                            .px(px(9.))
                            .border_1()
                            .border_color(p.acc)
                            .rounded(px(6.))
                            .bg(p.bg)
                            .text_size(px(12.5))
                            .text_color(p.fg3)
                            .child("Focused"),
                    ),
            )
            .child(
                div().flex().flex_wrap().gap(px(6.)).children(
                    [
                        ui::keys("⇧⌘P", "Ctrl+Shift+P"),
                        ui::keys("⌘P", "Ctrl+P"),
                        ui::keys("⌘↵", "Ctrl+Enter"),
                        ui::keys("⌘.", "Ctrl+."),
                    ]
                    .into_iter()
                    .map(|k| ui::kbd(k, p)),
                ),
            );
        let types = card("CONNECTION TYPES")
            .child(
                div().flex().flex_wrap().gap(px(10.)).children(
                    [
                        ("PG", "PostgreSQL"),
                        ("MS", "SQL Server"),
                        ("SSH", "SSH Host"),
                        ("SFTP", "SFTP"),
                        ("FTP", "FTP / FTPS"),
                    ]
                    .into_iter()
                    .map(|(b, l)| {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(7.))
                            .text_size(px(12.))
                            .child(ui::monogram(b, 32., p))
                            .child(l)
                    }),
                ),
            )
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(p.fg3)
                    .child("Monogram placeholders — swap for the final line-icon set."),
            );
        let cells = card("GRID CELLS").child(
            div()
                .grid()
                .grid_cols(3)
                .border_t_1()
                .border_color(p.bd)
                .font_family(MONO)
                .text_size(px(12.))
                .child(cell("text value", false, p))
                .child(cell("4,812.40", true, p))
                .child(cell("NULL", false, p).italic().text_color(p.fg3))
                .child(cell("''", false, p).text_color(p.fg3))
                .child(
                    cell("staged", false, p)
                        .bg(p.staged)
                        .border_l_2()
                        .border_color(p.stg),
                )
                .child(
                    cell("selected", false, p)
                        .bg(p.sel)
                        .border_1()
                        .border_color(p.acc),
                ),
        );
        let rows = card("TREE ROWS · 26 PX")
            .child(
                div()
                    .h(px(26.))
                    .flex()
                    .items_center()
                    .gap(px(7.))
                    .text_size(px(12.5))
                    .child(
                        div()
                            .w(px(10.))
                            .text_size(px(9.))
                            .text_color(p.fg3)
                            .child("▾"),
                    )
                    .child(ui::dot(p.prod, 7.))
                    .child(
                        div()
                            .flex_1()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Host · connected"),
                    )
                    .child(ui::dot(p.dev, 6.)),
            )
            .child(
                div()
                    .h(px(26.))
                    .pl(px(18.))
                    .flex()
                    .items_center()
                    .gap(px(7.))
                    .bg(p.sel)
                    .text_size(px(12.5))
                    .child(ui::monogram("PG", 28., p))
                    .child(div().flex_1().child("Selected leaf"))
                    .child(
                        div()
                            .font_family(MONO)
                            .text_size(px(11.))
                            .text_color(p.fg3)
                            .child(":54012"),
                    ),
            )
            .child(
                div()
                    .h(px(26.))
                    .pl(px(18.))
                    .flex()
                    .items_center()
                    .gap(px(7.))
                    .text_size(px(12.5))
                    .child(
                        div()
                            .w(px(10.))
                            .text_size(px(9.))
                            .text_color(p.fg3)
                            .child("▾"),
                    )
                    .child(div().flex_1().child("Loading node"))
                    .child(ui::shimmer(64., p)),
            );
        div()
            .id("components")
            .absolute()
            .inset_0()
            .bg(p.bg)
            .overflow_y_scroll()
            .occlude()
            .key_context("Overlay")
            .track_focus(&self.overlay_focus)
            .child(
                div()
                    .max_w(px(1120.))
                    .mx_auto()
                    .px(px(32.))
                    .pt(px(28.))
                    .pb(px(60.))
                    .flex()
                    .flex_col()
                    .gap(px(28.))
                    .font_family(SANS)
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap(px(12.))
                            .child(
                                div()
                                    .text_size(px(20.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Component sheet"),
                            )
                            .child(div().text_size(px(12.5)).text_color(p.fg3).child(format!(
                                "gpui-component vocabulary · {} theme",
                                if p.dark { "dark" } else { "light" }
                            )))
                            .child(div().flex_1())
                            .child(
                                ui::button("comp-close", "Close", Kind::Secondary, p)
                                    .h(px(28.))
                                    .on_click(cx.listener(|this, _, w, cx| this.dismiss(w, cx))),
                            ),
                    )
                    .child(
                        div()
                            .grid()
                            .grid_cols(3)
                            .gap(px(16.))
                            .child(envs)
                            .child(buttons)
                            .child(types)
                            .child(cells)
                            .child(rows),
                    ),
            )
            .into_any_element()
    }
}

fn cell(text: &'static str, right: bool, p: &Palette) -> gpui_kit::Div {
    div()
        .h(px(26.))
        .flex()
        .items_center()
        .when(right, |d| d.justify_end())
        .px(px(10.))
        .border_r_1()
        .border_b_1()
        .border_color(p.line)
        .child(text)
}

fn kv(k: &str, v: &str, mono: bool, p: &Palette) -> AnyElement {
    div()
        .flex()
        .gap(px(12.))
        .child(
            div()
                .w(px(120.))
                .flex_none()
                .text_color(p.fg3)
                .child(k.to_owned()),
        )
        .child(
            div()
                .when(mono, |d| d.font_family(MONO))
                .child(v.to_owned()),
        )
        .into_any_element()
}

fn setting_row(label: &str, value: &str, mono: bool, p: &Palette) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(12.))
        .text_size(px(12.5))
        .child(
            div()
                .w(px(200.))
                .flex_none()
                .text_color(p.fg2)
                .child(label.to_owned()),
        )
        .child(
            div()
                .flex_1()
                .h(px(28.))
                .flex()
                .items_center()
                .px(px(9.))
                .border_1()
                .border_color(p.bd2)
                .rounded(px(6.))
                .bg(p.bg)
                .when(mono, |d| d.font_family(MONO))
                .child(value.to_owned()),
        )
        .into_any_element()
}

/// A theme preview: its own background, text, a selected line and syntax colors.
fn theme_card(id: ThemeId, current: bool, p: &Palette) -> gpui_kit::Stateful<gpui_kit::Div> {
    let t = id.palette();
    let line = |w: f32, color: gpui_kit::Hsla| div().h(px(5.)).w(px(w)).rounded(px(2.)).bg(color);
    div()
        .id(SharedString::from(format!("theme-{}", id.key())))
        .w(px(168.))
        .p(px(6.))
        .flex()
        .flex_col()
        .gap(px(6.))
        .border_1()
        .border_color(if current { p.acc } else { p.bd })
        .rounded(px(8.))
        .hover(|s| s.bg(p.hover))
        .child(
            div()
                .h(px(78.))
                .rounded(px(5.))
                .overflow_hidden()
                .border_1()
                .border_color(t.bd)
                .flex()
                .child(
                    div()
                        .w(px(34.))
                        .h_full()
                        .bg(t.panel)
                        .border_r_1()
                        .border_color(t.bd),
                )
                .child(
                    div()
                        .flex_1()
                        .h_full()
                        .bg(t.surface)
                        .flex()
                        .flex_col()
                        .gap(px(5.))
                        .p(px(7.))
                        .child(
                            div()
                                .flex()
                                .gap(px(4.))
                                .child(line(22., t.sx_kw))
                                .child(line(30., t.fg)),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .h(px(13.))
                                .px(px(3.))
                                .rounded(px(2.))
                                .bg(t.sel)
                                .text_size(px(9.))
                                .font_family(MONO)
                                .text_color(t.fg)
                                .child("selected"),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(4.))
                                .child(line(18., t.sx_fn))
                                .child(line(26., t.sx_str)),
                        )
                        .child(line(40., t.sx_cm))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .child(div().h(px(8.)).w(px(22.)).rounded(px(3.)).bg(t.acc)),
                        ),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(px(12.))
                .when(current, |d| d.child(ui::dot(p.acc, 6.)))
                .child(id.label()),
        )
}
