//! Drawing of the containers tab: header (context, namespace, view, filter), hint cards,
//! the virtualized list and the detail pane (actions, logs, inspect text, forms).

use crate::menus::DropdownMenu as _;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Editor, Input};
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, ClipboardItem, Context, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, px, relative, uniform_list,
};
use switchyard_core::containers::clusters::{ClusterProvider, plugin_install_hint};
use switchyard_core::containers::{Action, ContainersError, LogSource};
use switchyard_core::{ContainerForward, ForwardState};

use super::{ContainersTab, Detail, Form, Selected, TAILS, needs_typed_name, typed_name_ok};
use crate::appearance::{rpx, ts};
use crate::theme::{MONO, Palette, SANS, palette};
use crate::ui::{self, Kind};

use super::rows::{Tone, View};

/// Row height of the lists.
const ROW_H: f32 = 26.;
/// Line height of the log pane.
const LOG_H: f32 = 18.;

fn tone_color(t: Tone, p: &Palette) -> Hsla {
    match t {
        Tone::Good => p.dev,
        Tone::Pending => p.stg,
        Tone::Bad => p.prod,
        Tone::Muted => p.fg3,
    }
}

/// A banner: amber for hints, red for errors.
fn card(p: &Palette, color: Hsla) -> gpui_kit::Div {
    div()
        .flex_none()
        .flex()
        .flex_col()
        .gap(rpx(4.))
        .mx(rpx(12.))
        .mt(rpx(8.))
        .px(rpx(10.))
        .py(rpx(6.))
        .rounded(px(6.))
        .bg(color.opacity(0.08))
        .border_1()
        .border_color(color.opacity(0.4))
        .text_size(ts::BODY)
        .text_color(p.fg)
}

fn tail_label(t: Option<u32>) -> String {
    t.map_or("All lines".to_owned(), |n| format!("Last {n}"))
}

/// The label of a kubeconfig context in the picker: `AKS · prod-weu`.
fn context_label(provider: ClusterProvider, flavor: Option<&str>, name: &str) -> String {
    match flavor {
        Some(f) if provider == ClusterProvider::Local => format!("Local ({f}) · {name}"),
        _ => format!("{} · {name}", provider.label()),
    }
}

impl ContainersTab {
    fn render_header(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let c = &self.connection;
        let this = cx.entity().downgrade();
        let context_menu = self.contexts.as_ref().map(|contexts| {
            let active = self.active_context();
            let label = active.map_or_else(
                || "Context".to_owned(),
                |a| context_label(a.provider, a.local_flavor, &a.name),
            );
            let active_name = active.map(|a| a.name.clone());
            let list: Vec<(String, String)> = contexts
                .contexts
                .iter()
                .map(|k| {
                    (
                        k.name.clone(),
                        context_label(k.provider, k.local_flavor, &k.name),
                    )
                })
                .collect();
            let this = this.clone();
            Button::new("ct-context")
                .outline()
                .small()
                .label(label)
                .dropdown_menu(move |mut menu, _, _| {
                    for (name, label) in &list {
                        let this = this.clone();
                        let name = name.clone();
                        menu = menu.item(
                            PopupMenuItem::new(label.clone())
                                .checked(active_name.as_deref() == Some(name.as_str()))
                                .on_click(move |_, _, cx| {
                                    let name = name.clone();
                                    let _ = this.update(cx, |t, cx| t.set_context(name, cx));
                                }),
                        );
                    }
                    menu
                })
                .into_any_element()
        });
        let ns_menu = self.is_kube().then(|| {
            let current = self.namespace.clone();
            let mut names = self.namespaces.clone();
            if let Some(n) = &current
                && !names.contains(n)
            {
                names.insert(0, n.clone());
            }
            let this = this.clone();
            Button::new("ct-namespace")
                .outline()
                .small()
                .label(current.clone().unwrap_or_else(|| "All namespaces".into()))
                .dropdown_menu(move |mut menu, _, _| {
                    let options = std::iter::once(None).chain(names.iter().cloned().map(Some));
                    for ns in options {
                        let this = this.clone();
                        let label = ns.clone().unwrap_or_else(|| "All namespaces".into());
                        menu =
                            menu.item(PopupMenuItem::new(label).checked(ns == current).on_click(
                                move |_, _, cx| {
                                    let ns = ns.clone();
                                    let _ = this.update(cx, |t, cx| t.set_namespace(ns, cx));
                                },
                            ));
                    }
                    menu
                })
                .into_any_element()
        });
        let where_ = match &c.host_id {
            Some(_) => "on its Host",
            None => "this computer",
        };
        div()
            .flex_none()
            .h(rpx(38.))
            .flex()
            .items_center()
            .gap(rpx(10.))
            .px(rpx(12.))
            .border_b_1()
            .border_color(p.bd)
            .child(ui::monogram(c.platform.badge(), 30., p))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(ts::BASE)
                    .child(c.name.clone()),
            )
            .when(c.environment.is_production(), |d| {
                d.child(
                    div()
                        .px(rpx(5.))
                        .rounded(px(3.))
                        .border_1()
                        .border_color(p.prod)
                        .text_size(ts::TINY_PLUS)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.prod)
                        .child("PRODUCTION"),
                )
            })
            .when(c.read_only, |d| {
                d.child(
                    div()
                        .text_size(ts::SMALL)
                        .text_color(p.fg3)
                        .child("read-only"),
                )
            })
            .child(
                div()
                    .text_size(ts::SMALL)
                    .text_color(p.fg3)
                    .child(format!("{} · {where_}", c.platform.cli())),
            )
            .child(div().flex_1())
            .children(context_menu)
            .children(ns_menu)
            .child(if self.list_request.is_some() && self.listing.is_none() {
                ui::shimmer(70., p)
            } else {
                ui::button("ct-refresh", "Refresh", Kind::Secondary, p)
                    .h(rpx(24.))
                    .when(self.list_request.is_some(), |b| b.opacity(0.6))
                    .on_click(cx.listener(|t, _, _, cx| t.refresh(cx)))
                    .into_any_element()
            })
            .into_any_element()
    }

    fn render_toolbar(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let views = View::of(self.connection.platform);
        let options = views
            .iter()
            .map(|v| {
                let v = *v;
                let this = cx.entity().downgrade();
                let label: SharedString = match v {
                    View::Forwards if !self.forwards.is_empty() => {
                        format!("{} ({})", v.label(), self.forwards.len()).into()
                    }
                    _ => v.label().into(),
                };
                let on: ui::OnClick = Box::new(move |_, _, cx| {
                    let _ = this.update(cx, |t, cx| t.set_view(v, cx));
                });
                (label, self.view == v, on)
            })
            .collect();
        div()
            .flex_none()
            .h(rpx(36.))
            .flex()
            .items_center()
            .gap(rpx(10.))
            .px(rpx(12.))
            .child(ui::segmented("ct-views", options, 20., p))
            .child(
                div()
                    .w(rpx(220.))
                    .flex_none()
                    .child(Input::new(&self.filter).text_size(ts::BODY)),
            )
            .when(self.view == View::Containers, |d| {
                d.child(
                    ui::checkbox("ct-show-all", self.show_all, "Show stopped", p)
                        .cursor_pointer()
                        .on_click(cx.listener(|t, _, _, cx| t.toggle_show_all(cx))),
                )
            })
            .child(div().flex_1())
            .child(
                div()
                    .font_family(MONO)
                    .text_size(ts::SMALL)
                    .text_color(p.fg3)
                    .child(format!("{} shown", self.rows.len())),
            )
            .into_any_element()
    }

    /// Missing CLI, expired sign-in, missing exec plugin, errors and action outcomes.
    fn render_banners(&self, p: &Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if let Some(ctx) = self.active_context()
            && ctx.plugin_missing
            && let Some(plugin) = &ctx.exec
        {
            out.push(
                card(p, p.stg)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("{plugin} is not installed")),
                    )
                    .child(div().text_color(p.fg2).child(plugin_install_hint(plugin)))
                    .into_any_element(),
            );
        }
        match &self.list_error {
            Some(e @ ContainersError::Missing(tool)) => out.push(
                card(p, p.stg)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("{} was not found", tool.program())),
                    )
                    .child(div().text_color(p.fg2).child(tool.install_hint()))
                    .when(self.connection.host_id.is_some(), |d| {
                        d.child(
                            div()
                                .text_color(p.fg3)
                                .child("It is looked for on the Host, over SSH."),
                        )
                    })
                    .child(
                        div()
                            .text_size(ts::SMALL)
                            .text_color(p.fg3)
                            .child(e.to_string()),
                    )
                    .into_any_element(),
            ),
            Some(ContainersError::SignIn { message, hint }) => out.push(
                card(p, p.stg)
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(hint.clone()))
                    .child(
                        div()
                            .font_family(MONO)
                            .text_size(ts::SMALL)
                            .text_color(p.fg2)
                            .child(message.clone()),
                    )
                    .into_any_element(),
            ),
            Some(e) => out.push(
                card(p, p.prod)
                    .child(
                        div()
                            .font_family(MONO)
                            .text_size(ts::SMALL)
                            .child(e.to_string()),
                    )
                    .into_any_element(),
            ),
            None => {}
        }
        if let Some((ok, m)) = &self.status {
            out.push(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .mx(rpx(12.))
                    .mt(rpx(6.))
                    .text_size(ts::BODY)
                    .text_color(if *ok { p.dev } else { p.prod })
                    .child(div().flex_1().min_w_0().child(m.clone()))
                    .child(
                        ui::button("ct-status-x", "Dismiss", Kind::Ghost, p)
                            .h(rpx(20.))
                            .text_size(ts::SMALL)
                            .on_click(cx.listener(|t, _, _, cx| {
                                t.status = None;
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            );
        }
        if let Some(c) = self.render_confirm(p, cx) {
            out.push(c);
        }
        out
    }

    fn render_confirm(&self, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let action = self.pending.as_ref()?;
        let typed = needs_typed_name(action, self.production());
        let ok = !typed || typed_name_ok(&self.confirm_input.read(cx).value(), action);
        Some(
            card(p, p.prod)
                .gap(rpx(6.))
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(action.question()),
                )
                .when(typed, |d| {
                    d.child(div().text_color(p.prod).child(format!(
                        "Production connection: type {} to confirm.",
                        action.subject()
                    )))
                    .child(
                        div()
                            .w(rpx(260.))
                            .child(Input::new(&self.confirm_input).text_size(ts::BODY)),
                    )
                })
                .child(
                    div()
                        .flex()
                        .gap(rpx(8.))
                        .child(
                            ui::button("ct-confirm-no", "Cancel", Kind::Secondary, p).on_click(
                                cx.listener(|t, _, _, cx| {
                                    t.pending = None;
                                    cx.notify();
                                }),
                            ),
                        )
                        .child(
                            ui::button(
                                "ct-confirm-yes",
                                action.label(),
                                if ok {
                                    Kind::Destructive
                                } else {
                                    Kind::Secondary
                                },
                                p,
                            )
                            .when(!ok, |b| b.opacity(0.5))
                            .on_click(cx.listener(|t, _, window, cx| t.confirm(window, cx))),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_list(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let columns = self.view.columns(self.all_namespaces());
        let cell = |w: f32| {
            let d = div().min_w_0().px(rpx(5.)).truncate();
            if w == 0. {
                d.flex_1().min_w(rpx(140.))
            } else {
                d.w(rpx(w)).flex_none()
            }
        };
        let header = div()
            .flex_none()
            .flex()
            .h(rpx(ROW_H))
            .items_center()
            .px(rpx(8.))
            .border_b_1()
            .border_color(p.bd)
            .text_size(ts::CAPTION_PLUS)
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(p.fg3)
            .child(div().w(rpx(14.)).flex_none())
            .children(columns.iter().map(|(h, w)| cell(*w).child(*h)));
        let body = if self.rows.is_empty() {
            let text = if self.list_request.is_some() {
                "Loading…"
            } else if self.list_error.is_some() {
                ""
            } else if !self.filter.read(cx).value().is_empty() {
                "Nothing matches the filter."
            } else if self.view == View::Forwards {
                "No port forwards. Select a pod or service and choose Port forward."
            } else {
                "Nothing here."
            };
            div()
                .p(rpx(16.))
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .child(text)
                .into_any_element()
        } else {
            let p2 = *p;
            let widths: Vec<f32> = columns.iter().map(|(_, w)| *w).collect();
            uniform_list(
                "ct-rows",
                self.rows.len(),
                cx.processor(move |this, range: std::ops::Range<usize>, _w, cx| {
                    let p = &p2;
                    range
                        .filter_map(|r| {
                            let row = this.rows.get(r)?.clone();
                            let is_sel = this.selected.as_deref() == Some(row.key.as_str());
                            let key = row.key.clone();
                            let cell = |w: f32| {
                                let d = div().min_w_0().px(rpx(5.)).truncate();
                                if w == 0. {
                                    d.flex_1().min_w(rpx(140.))
                                } else {
                                    d.w(rpx(w)).flex_none()
                                }
                            };
                            Some(
                                div()
                                    .id(("ct-row", r))
                                    .w_full()
                                    .h(rpx(ROW_H))
                                    .flex()
                                    .items_center()
                                    .px(rpx(8.))
                                    .border_b_1()
                                    .border_color(p.line)
                                    .text_size(ts::BODY)
                                    .text_color(if row.tone == Tone::Muted { p.fg2 } else { p.fg })
                                    .when(is_sel, |d| d.bg(p.sel))
                                    .when(!is_sel, |d| d.hover(|st| st.bg(p.hover)))
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.select(key.clone(), cx)
                                    }))
                                    .child(
                                        div()
                                            .w(rpx(14.))
                                            .flex_none()
                                            .child(ui::dot(tone_color(row.tone, p), 7.)),
                                    )
                                    .children(
                                        row.cells
                                            .into_iter()
                                            .zip(widths.iter())
                                            .map(|(text, w)| cell(*w).child(text)),
                                    ),
                            )
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.list_scroll)
            .flex_1()
            .into_any_element()
        };
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header)
            .child(body)
            .into_any_element()
    }

    /// The buttons of the selected object.
    fn render_actions(
        &self,
        sel: &Selected<'_>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let busy = self.action_request.is_some() || self.pending.is_some();
        let writable = !self.connection.read_only;
        let btn = |id: &'static str, label: &'static str, kind: Kind| {
            ui::button(id, label, kind, p).h(rpx(24.))
        };
        let act = |id: &'static str, action: Action, cx: &mut Context<Self>| -> AnyElement {
            let enabled = writable && !busy;
            let destructive = action.is_destructive() && action.always_confirm();
            btn(
                id,
                action.label(),
                if destructive {
                    Kind::Destructive
                } else {
                    Kind::Secondary
                },
            )
            .when(!enabled, |b| b.opacity(0.45))
            .when(enabled, |b| {
                b.on_click(cx.listener(move |t, _, window, cx| t.ask(action.clone(), window, cx)))
            })
            .into_any_element()
        };
        let mut out: Vec<AnyElement> = Vec::new();
        let logs = |cx: &mut Context<Self>| {
            btn("ct-logs", "Logs", Kind::Secondary)
                .on_click(cx.listener(|t, _, _, cx| t.open_logs(None, cx)))
                .into_any_element()
        };
        let shell = |cx: &mut Context<Self>| {
            btn("ct-shell", "Open shell", Kind::Secondary)
                .on_click(cx.listener(|t, _, _, cx| t.open_shell(None, cx)))
                .into_any_element()
        };
        let inspect = |label: &'static str, yaml: bool, cx: &mut Context<Self>| {
            btn(
                if yaml { "ct-yaml" } else { "ct-inspect" },
                label,
                Kind::Secondary,
            )
            .on_click(cx.listener(move |t, _, window, cx| t.inspect_selected(yaml, window, cx)))
            .into_any_element()
        };
        let forward = |cx: &mut Context<Self>| {
            btn("ct-forward", "Port forward", Kind::Secondary)
                .when(self.connection.host_id.is_some(), |b| b.opacity(0.45))
                .when(self.connection.host_id.is_none(), |b| {
                    b.on_click(cx.listener(|t, _, window, cx| t.open_forward(window, cx)))
                })
                .into_any_element()
        };
        match sel {
            Selected::Container(c) => {
                let (id, name) = (c.id.clone(), c.name.clone());
                out.push(logs(cx));
                out.push(inspect("Inspect", false, cx));
                if c.state.is_running() {
                    out.push(shell(cx));
                    out.push(act(
                        "ct-stop",
                        Action::Stop {
                            id: id.clone(),
                            name: name.clone(),
                        },
                        cx,
                    ));
                    out.push(act(
                        "ct-restart",
                        Action::Restart {
                            id: id.clone(),
                            name: name.clone(),
                        },
                        cx,
                    ));
                } else {
                    out.push(act(
                        "ct-start",
                        Action::Start {
                            id: id.clone(),
                            name: name.clone(),
                        },
                        cx,
                    ));
                }
                out.push(act("ct-remove", Action::Remove { id, name }, cx));
            }
            Selected::Image(_) | Selected::Volume(_) | Selected::Network(_) => {
                out.push(inspect("Inspect", false, cx));
            }
            Selected::Pod(pod) => {
                out.push(logs(cx));
                out.push(inspect("Describe", false, cx));
                out.push(inspect("YAML", true, cx));
                out.push(shell(cx));
                out.push(forward(cx));
                out.push(act(
                    "ct-delete-pod",
                    Action::DeletePod {
                        namespace: pod.namespace.clone(),
                        name: pod.name.clone(),
                    },
                    cx,
                ));
            }
            Selected::Deployment(d) => {
                out.push(inspect("Describe", false, cx));
                out.push(inspect("YAML", true, cx));
                out.push(
                    btn("ct-scale", "Scale…", Kind::Secondary)
                        .when(!writable || busy, |b| b.opacity(0.45))
                        .when(writable && !busy, |b| {
                            b.on_click(cx.listener(|t, _, window, cx| t.open_scale(window, cx)))
                        })
                        .into_any_element(),
                );
                out.push(act(
                    "ct-rollout",
                    Action::RolloutRestart {
                        namespace: d.namespace.clone(),
                        name: d.name.clone(),
                    },
                    cx,
                ));
            }
            Selected::Service(_) => {
                out.push(inspect("Describe", false, cx));
                out.push(inspect("YAML", true, cx));
                out.push(forward(cx));
            }
            Selected::Forward(f) => {
                let id = f.id;
                if let ForwardState::Running(port) = f.state {
                    let url = format!("http://127.0.0.1:{port}");
                    out.push(
                        btn("ct-copy-url", "Copy URL", Kind::Secondary)
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                            })
                            .into_any_element(),
                    );
                }
                let label = if matches!(f.state, ForwardState::Failed(_)) {
                    "Remove"
                } else {
                    "Stop"
                };
                out.push(
                    btn("ct-stop-forward", label, Kind::Destructive)
                        .on_click(cx.listener(move |t, _, _, cx| t.stop_forward(id, cx)))
                        .into_any_element(),
                );
            }
        }
        out
    }

    fn selected_title(sel: &Selected<'_>) -> String {
        match sel {
            Selected::Container(c) => format!("{} · {}", c.name, c.image),
            Selected::Image(i) => i.reference(),
            Selected::Volume(v) => v.name.clone(),
            Selected::Network(n) => n.name.clone(),
            Selected::Pod(p) => format!("{}/{}", p.namespace, p.name),
            Selected::Deployment(d) => format!("{}/{}", d.namespace, d.name),
            Selected::Service(s) => format!("{}/{}", s.namespace, s.name),
            Selected::Forward(f) => forward_title(f),
        }
    }

    fn render_form(&self, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let row = || {
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
        match &self.form {
            Form::None => None,
            Form::Scale { namespace, name } => Some(
                row()
                    .child(format!("Scale {namespace}/{name} to"))
                    .child(
                        div()
                            .w(rpx(90.))
                            .child(Input::new(&self.scale_input).text_size(ts::BODY)),
                    )
                    .child(
                        ui::button("ct-scale-go", "Scale", Kind::Primary, p)
                            .h(rpx(24.))
                            .on_click(cx.listener(|t, _, window, cx| t.submit_scale(window, cx))),
                    )
                    .child(
                        ui::button("ct-scale-x", "Cancel", Kind::Ghost, p)
                            .h(rpx(24.))
                            .on_click(cx.listener(|t, _, _, cx| {
                                t.form = Form::None;
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            ),
            Form::Forward { name, ports, .. } => {
                let suggestions =
                    ports.iter().take(6).enumerate().map(|(i, port)| {
                        let port = *port;
                        ui::button(("ct-port", i), format!(":{port}"), Kind::Ghost, p)
                            .h(rpx(22.))
                            .on_click(cx.listener(move |t, _, window, cx| {
                                t.set_remote_port(port, window, cx)
                            }))
                            .into_any_element()
                    });
                Some(
                    row()
                        .child(format!("Forward {name}"))
                        .child(
                            div()
                                .w(rpx(110.))
                                .child(Input::new(&self.remote_port).text_size(ts::BODY)),
                        )
                        .children(suggestions)
                        .child(div().text_color(p.fg3).child("to 127.0.0.1:"))
                        .child(
                            div()
                                .w(rpx(130.))
                                .child(Input::new(&self.local_port).text_size(ts::BODY)),
                        )
                        .child(
                            ui::button("ct-forward-go", "Start", Kind::Primary, p)
                                .h(rpx(24.))
                                .on_click(cx.listener(|t, _, _, cx| t.submit_forward(cx))),
                        )
                        .child(
                            ui::button("ct-forward-x", "Cancel", Kind::Ghost, p)
                                .h(rpx(24.))
                                .on_click(cx.listener(|t, _, _, cx| {
                                    t.form = Form::None;
                                    cx.notify();
                                })),
                        )
                        .into_any_element(),
                )
            }
        }
    }

    fn render_logs(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let tail = self.log_tail;
        let tail_menu = Button::new("ct-log-tail")
            .outline()
            .xsmall()
            .label(tail_label(tail))
            .dropdown_menu({
                let this = this.clone();
                move |mut menu, _, _| {
                    for t in TAILS {
                        let this = this.clone();
                        menu = menu.item(
                            PopupMenuItem::new(tail_label(t))
                                .checked(t == tail)
                                .on_click(move |_, _, cx| {
                                    let _ = this.update(cx, |tab, cx| tab.set_log_tail(t, cx));
                                }),
                        );
                    }
                    menu
                }
            })
            .into_any_element();
        // A pod with several containers: pick whose logs.
        let container_menu = match (&self.log_source, self.selected()) {
            (Some(LogSource::Pod { container, .. }), Some(Selected::Pod(pod)))
                if pod.containers.len() > 1 =>
            {
                let names = pod.containers.clone();
                let current = container.clone();
                let this = this.clone();
                Some(
                    Button::new("ct-log-container")
                        .outline()
                        .xsmall()
                        .label(current.clone().unwrap_or_else(|| "Container".into()))
                        .dropdown_menu(move |mut menu, _, _| {
                            for n in &names {
                                let this = this.clone();
                                let name = n.clone();
                                menu = menu.item(
                                    PopupMenuItem::new(n.clone())
                                        .checked(current.as_deref() == Some(n.as_str()))
                                        .on_click(move |_, _, cx| {
                                            let name = name.clone();
                                            let _ = this
                                                .update(cx, |t, cx| t.set_log_container(name, cx));
                                        }),
                                );
                            }
                            menu
                        })
                        .into_any_element(),
                )
            }
            _ => None,
        };
        let state: (String, Hsla) = match (&self.log_request, &self.log_ended) {
            (Some(_), _) if self.log_follow => ("following".into(), p.dev),
            (Some(_), _) => ("reading…".into(), p.fg3),
            (None, Some(Err(e))) => (e.to_string(), p.prod),
            (None, Some(Ok(()))) => ("ended".into(), p.fg3),
            (None, None) => ("paused".into(), p.fg3),
        };
        let count = if self.logs.filtered() {
            format!("{} of {} lines", self.logs.visible_len(), self.logs.len())
        } else {
            format!("{} lines", self.logs.len())
        };
        let toolbar = div()
            .flex_none()
            .h(rpx(32.))
            .flex()
            .items_center()
            .gap(rpx(8.))
            .px(rpx(12.))
            .border_b_1()
            .border_color(p.bd)
            .text_size(ts::SMALL)
            .children(container_menu)
            .child(tail_menu)
            .child(
                ui::checkbox("ct-follow", self.log_follow, "Follow", p)
                    .cursor_pointer()
                    .on_click(cx.listener(|t, _, _, cx| t.toggle_follow(cx))),
            )
            .child(
                div()
                    .w(rpx(200.))
                    .flex_none()
                    .child(Input::new(&self.log_filter).text_size(ts::BODY)),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(state.1)
                    .child(state.0),
            )
            .child(div().flex_1())
            .child(div().font_family(MONO).text_color(p.fg3).child(count))
            .child(
                ui::button("ct-log-copy", "Copy", Kind::Ghost, p)
                    .h(rpx(22.))
                    .on_click(cx.listener(|t, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(t.logs.visible_text()));
                    })),
            )
            .child(
                ui::button("ct-log-clear", "Clear", Kind::Ghost, p)
                    .h(rpx(22.))
                    .on_click(cx.listener(|t, _, _, cx| {
                        t.logs.clear();
                        cx.notify();
                    })),
            );
        let fg = p.fg;
        let lines = uniform_list(
            "ct-log-lines",
            self.logs.visible_len(),
            cx.processor(move |this, range: std::ops::Range<usize>, _w, _cx| {
                range
                    .map(|i| {
                        div()
                            .h(rpx(LOG_H))
                            .px(rpx(12.))
                            .whitespace_nowrap()
                            .text_color(fg)
                            .child(this.logs.visible(i).unwrap_or_default())
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.log_scroll)
        .flex_1()
        .font_family(MONO)
        .text_size(ts::SMALL);
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(toolbar)
            .child(lines)
            .into_any_element()
    }

    fn render_detail(&self, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sel = self.selected()?;
        let title = Self::selected_title(&sel);
        let actions = self.render_actions(&sel, p, cx);
        let form = self.render_form(p, cx);
        let open = self.detail != Detail::None;
        let body: Option<AnyElement> = match &self.detail {
            Detail::None => None,
            Detail::Logs => Some(self.render_logs(p, cx)),
            Detail::Text(heading) => {
                let text = self.text.clone();
                Some(
                    div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex_none()
                                .h(rpx(28.))
                                .flex()
                                .items_center()
                                .gap(rpx(8.))
                                .px(rpx(12.))
                                .border_b_1()
                                .border_color(p.bd)
                                .text_size(ts::SMALL)
                                .text_color(p.fg2)
                                .child(div().flex_1().min_w_0().truncate().child(heading.clone()))
                                .when(self.inspect_request.is_some(), |d| {
                                    d.child(ui::shimmer(60., p))
                                })
                                .child(
                                    ui::button("ct-text-copy", "Copy", Kind::Ghost, p)
                                        .h(rpx(22.))
                                        .on_click(move |_, _, cx| {
                                            let v = text.read(cx).value().to_string();
                                            cx.write_to_clipboard(ClipboardItem::new_string(v));
                                        }),
                                ),
                        )
                        .map(|d| match &self.text_error {
                            Some(e) => d.child(
                                div()
                                    .p(rpx(12.))
                                    .font_family(MONO)
                                    .text_size(ts::SMALL)
                                    .text_color(p.prod)
                                    .child(e.clone()),
                            ),
                            None => d.child(
                                div().flex_1().min_h_0().child(
                                    Editor::new(&self.text)
                                        .readonly(true)
                                        .bordered(false)
                                        .appearance(false)
                                        .h(relative(1.))
                                        .font_family(MONO)
                                        .text_size(ts::SMALL),
                                ),
                            ),
                        })
                        .into_any_element(),
                )
            }
        };
        Some(
            div()
                .flex_none()
                .when(open, |d| d.h(relative(0.5)))
                .flex()
                .flex_col()
                .border_t_1()
                .border_color(p.bd)
                .child(
                    div()
                        .flex_none()
                        .h(rpx(36.))
                        .flex()
                        .items_center()
                        .gap(rpx(8.))
                        .px(rpx(12.))
                        .border_b_1()
                        .border_color(p.bd)
                        .text_size(ts::BODY)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(MONO)
                                .child(title),
                        )
                        .children(actions)
                        .when(open, |d| {
                            d.child(
                                ui::button("ct-detail-x", "Close", Kind::Ghost, p)
                                    .h(rpx(24.))
                                    .on_click(cx.listener(|t, _, _, cx| {
                                        t.close_detail();
                                        cx.notify();
                                    })),
                            )
                        }),
                )
                .children(form)
                .children(body)
                .into_any_element(),
        )
    }
}

fn forward_title(f: &ContainerForward) -> String {
    match f.state {
        ForwardState::Running(port) => format!("{} → 127.0.0.1:{port}", f.forward.label()),
        _ => f.forward.label(),
    }
}

impl Render for ContainersTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let header = self.render_header(&p, cx);
        let toolbar = self.render_toolbar(&p, cx);
        let banners = self.render_banners(&p, cx);
        let list = self.render_list(&p, cx);
        let detail = self.render_detail(&p, cx);
        div()
            .id("containers-tab")
            .size_full()
            .flex()
            .flex_col()
            .font_family(SANS)
            .bg(p.bg)
            .text_color(p.fg)
            .child(header)
            .child(toolbar)
            .children(banners)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .mt(rpx(4.))
                    .flex()
                    .flex_col()
                    .child(list),
            )
            .children(detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_labels_name_the_provider() {
        assert_eq!(
            context_label(ClusterProvider::Aks, None, "prod-weu"),
            "AKS · prod-weu"
        );
        assert_eq!(
            context_label(ClusterProvider::Local, Some("kind"), "kind-dev"),
            "Local (kind) · kind-dev"
        );
        assert_eq!(tail_label(None), "All lines");
        assert_eq!(tail_label(Some(500)), "Last 500");
    }
}
