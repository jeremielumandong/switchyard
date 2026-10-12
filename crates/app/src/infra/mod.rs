//! Infra engineer tools as tabs: Terraform / OpenTofu state, cloud inventory, network
//! checks and logs. Each tab talks to the runtime with [`Command::Infra`] and receives
//! [`InfraUpdate`]s routed here by request id.

mod checks;
mod inventory;
mod logs;
mod tf;

use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Div, Entity, FontWeight, InteractiveElement as _,
    IntoElement, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    WeakEntity, Window, div, px,
};
use switchyard_core::RequestId;
use switchyard_core::infra::InfraUpdate;
use switchyard_core::store::{DbConnection, Host};

use crate::app_state::Profiles;
use crate::appearance::{rpx, ts};
use crate::theme::Palette;
use crate::workspace::{Tab, Workspace};

pub use checks::ChecksTab;
pub use inventory::InventoryTab;
pub use logs::LogsTab;
pub use tf::TfTab;

/// Which tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InfraTool {
    /// Terraform / OpenTofu state viewer.
    TfState,
    /// Cloud resource inventory.
    Inventory,
    /// Network checks.
    Checks,
    /// Log viewer.
    Logs,
}

impl InfraTool {
    /// Every tool, in Tools-pane order.
    pub const ALL: [InfraTool; 4] = [
        InfraTool::Inventory,
        InfraTool::TfState,
        InfraTool::Checks,
        InfraTool::Logs,
    ];

    /// Monogram.
    pub fn badge(self) -> &'static str {
        match self {
            InfraTool::TfState => "TF",
            InfraTool::Inventory => "INV",
            InfraTool::Checks => "NET",
            InfraTool::Logs => "LOG",
        }
    }

    /// Name.
    pub fn name(self) -> &'static str {
        match self {
            InfraTool::TfState => "Terraform state",
            InfraTool::Inventory => "Cloud inventory",
            InfraTool::Checks => "Network checks",
            InfraTool::Logs => "Logs",
        }
    }

    /// One line for the Tools pane and the New chooser.
    pub fn desc(self) -> &'static str {
        match self {
            InfraTool::TfState => {
                "Read state from a file, S3, Blob or HCP Terraform; compare versions"
            }
            InfraTool::Inventory => {
                "VMs, databases, load balancers, clusters and storage across clouds"
            }
            InfraTool::Checks => "DNS, TLS certificates, TCP ports and HTTP, here or from a server",
            InfraTool::Logs => "Tail files and journald on servers, CloudWatch and Log Analytics",
        }
    }

    /// Palette label.
    pub fn palette_label(self) -> &'static str {
        match self {
            InfraTool::TfState => "Terraform State Viewer",
            InfraTool::Inventory => "Cloud Resource Inventory",
            InfraTool::Checks => "Network Checks (DNS, TLS, TCP, HTTP)",
            InfraTool::Logs => "Log Viewer (Server, CloudWatch, Log Analytics)",
        }
    }
}

/// An infra tool tab.
pub enum InfraTab {
    /// State viewer.
    Tf(Entity<TfTab>),
    /// Inventory.
    Inventory(Entity<InventoryTab>),
    /// Network checks.
    Checks(Entity<ChecksTab>),
    /// Log viewer.
    Logs(Entity<LogsTab>),
}

/// What a tool asks the workspace to do.
pub enum InfraEvent {
    /// Open the connection editor on a new Host.
    AddHost(Box<Host>),
    /// Open the connection editor on a new database connection.
    AddDb(Box<DbConnection>),
    /// Show a toast.
    Toast(String),
}

impl InfraTab {
    /// The tool.
    pub fn tool(&self) -> InfraTool {
        match self {
            InfraTab::Tf(_) => InfraTool::TfState,
            InfraTab::Inventory(_) => InfraTool::Inventory,
            InfraTab::Checks(_) => InfraTool::Checks,
            InfraTab::Logs(_) => InfraTool::Logs,
        }
    }

    /// The tab's view.
    pub fn view(&self) -> AnyElement {
        match self {
            InfraTab::Tf(t) => t.clone().into_any_element(),
            InfraTab::Inventory(t) => t.clone().into_any_element(),
            InfraTab::Checks(t) => t.clone().into_any_element(),
            InfraTab::Logs(t) => t.clone().into_any_element(),
        }
    }

    /// Badge and title for the tab strip.
    pub fn chip(&self, cx: &gpui_kit::App) -> (SharedString, SharedString) {
        let tool = self.tool();
        let title = match self {
            InfraTab::Tf(t) => t.read(cx).title(),
            InfraTab::Logs(t) => t.read(cx).title(),
            _ => tool.name().to_owned(),
        };
        (tool.badge().into(), title.into())
    }

    fn owns(&self, request: RequestId, cx: &gpui_kit::App) -> bool {
        match self {
            InfraTab::Tf(t) => t.read(cx).owns(request),
            InfraTab::Inventory(t) => t.read(cx).owns(request),
            InfraTab::Checks(t) => t.read(cx).owns(request),
            InfraTab::Logs(t) => t.read(cx).owns(request),
        }
    }

    fn on_update(
        &self,
        request: RequestId,
        update: InfraUpdate,
        window: &mut Window,
        cx: &mut gpui_kit::App,
    ) {
        match self {
            InfraTab::Tf(t) => t.update(cx, |t, cx| t.on_update(request, update, window, cx)),
            InfraTab::Inventory(t) => t.update(cx, |t, cx| t.on_update(request, update, cx)),
            InfraTab::Checks(t) => t.update(cx, |t, cx| t.on_update(request, update, cx)),
            InfraTab::Logs(t) => t.update(cx, |t, cx| t.on_update(request, update, window, cx)),
        }
    }

    fn set_profiles(&self, profiles: &Profiles, cx: &mut gpui_kit::App) {
        let p = profiles.clone();
        match self {
            InfraTab::Tf(t) => t.update(cx, |t, cx| t.set_profiles(p, cx)),
            InfraTab::Inventory(t) => t.update(cx, |t, cx| t.set_profiles(p, cx)),
            InfraTab::Checks(t) => t.update(cx, |t, cx| t.set_profiles(p, cx)),
            InfraTab::Logs(t) => t.update(cx, |t, cx| t.set_profiles(p, cx)),
        }
    }

    /// Stop whatever the tab is running (closing it).
    pub fn stop(&self, cx: &mut gpui_kit::App) {
        if let InfraTab::Logs(t) = self {
            t.update(cx, |t, cx| t.stop(cx));
        }
    }
}

impl Workspace {
    /// Open a tool: the existing tab for single-instance tools, a new one for logs.
    pub(crate) fn open_infra(
        &mut self,
        tool: InfraTool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if tool != InfraTool::Logs
            && let Some(ix) = self
                .tabs
                .iter()
                .position(|t| matches!(t, Tab::Infra(i) if i.tool() == tool))
        {
            self.active = ix;
            cx.notify();
            return;
        }
        let core = self.core.clone();
        let profiles = self.profiles.clone();
        let tab = match tool {
            InfraTool::TfState => InfraTab::Tf(cx.new(|cx| TfTab::new(core, profiles, window, cx))),
            InfraTool::Inventory => {
                let t = cx.new(|cx| InventoryTab::new(core, profiles, window, cx));
                let sub = cx.subscribe_in(&t, window, |this, _, ev: &InfraEvent, w, cx| {
                    this.on_infra_tool_event(ev, w, cx)
                });
                self._subs.push(sub);
                InfraTab::Inventory(t)
            }
            InfraTool::Checks => {
                InfraTab::Checks(cx.new(|cx| ChecksTab::new(core, profiles, window, cx)))
            }
            InfraTool::Logs => {
                InfraTab::Logs(cx.new(|cx| LogsTab::new(core, profiles, window, cx)))
            }
        };
        self.tabs.push(Tab::Infra(tab));
        self.active = self.tabs.len() - 1;
        cx.notify();
    }

    fn on_infra_tool_event(
        &mut self,
        ev: &InfraEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::conn_editor::ConnKind;
        use switchyard_core::store::Profile;
        match ev {
            InfraEvent::AddHost(h) => self.open_conn_editor(
                ConnKind::Ssh,
                Some(Profile::Host((**h).clone())),
                window,
                cx,
            ),
            InfraEvent::AddDb(d) => self.open_conn_editor(
                ConnKind::Db(d.engine),
                Some(Profile::Db((**d).clone())),
                window,
                cx,
            ),
            InfraEvent::Toast(t) => self.toast(t.clone(), cx),
        }
    }

    /// The New chooser's "Infra tools" links.
    pub(crate) fn render_infra_links(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let mut row = div().flex().flex_wrap().gap(rpx(6.));
        for (i, tool) in InfraTool::ALL.into_iter().enumerate() {
            row = row.child(
                div()
                    .id(("new-infra", i))
                    .h(rpx(30.))
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .px(rpx(10.))
                    .border_1()
                    .border_color(p.bd)
                    .rounded(px(6.))
                    .text_size(ts::UI)
                    .cursor_pointer()
                    .hover(|s| s.bg(p.hover))
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.overlay = None;
                        this.open_infra(tool, w, cx);
                    }))
                    .child(crate::ui::monogram(tool.badge(), 26., p))
                    .child(tool.name()),
            );
        }
        div()
            .px(rpx(20.))
            .pb(rpx(10.))
            .flex()
            .flex_col()
            .gap(rpx(6.))
            .child(
                div()
                    .text_size(ts::SMALL)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.fg3)
                    .child("INFRA TOOLS"),
            )
            .child(row)
            .into_any_element()
    }

    /// Pass the new profile list to the infra tabs.
    pub(crate) fn infra_profiles(&mut self, cx: &mut Context<Self>) {
        for t in &self.tabs {
            if let Tab::Infra(i) = t {
                i.set_profiles(&self.profiles, cx);
            }
        }
    }

    /// Saved network check lists arrived.
    pub(crate) fn on_check_lists(
        &mut self,
        value: Option<serde_json::Value>,
        cx: &mut Context<Self>,
    ) {
        for t in &self.tabs {
            if let Tab::Infra(InfraTab::Checks(c)) = t {
                let v = value.clone();
                c.update(cx, |c, cx| c.set_lists(v, cx));
            }
        }
    }

    /// Route an infra update to the tab that asked for it.
    pub(crate) fn on_infra_event(
        &mut self,
        request: RequestId,
        update: InfraUpdate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let owner = self.tabs.iter().find_map(|t| match t {
            Tab::Infra(i) if i.owns(request, cx) => Some(match i {
                InfraTab::Tf(t) => InfraTab::Tf(t.clone()),
                InfraTab::Inventory(t) => InfraTab::Inventory(t.clone()),
                InfraTab::Checks(t) => InfraTab::Checks(t.clone()),
                InfraTab::Logs(t) => InfraTab::Logs(t.clone()),
            }),
            _ => None,
        });
        match owner {
            Some(t) => t.on_update(request, update, window, cx),
            // The tab is gone: stop anything still streaming to it.
            None => {
                if matches!(update, InfraUpdate::LogLines(_) | InfraUpdate::LogStatus(_)) {
                    self.core
                        .send(switchyard_core::Command::InfraCancel { request });
                }
            }
        }
    }
}

/// The Tools pane's "Infra tools" group.
pub(crate) fn tool_group() -> crate::rail::ToolGroup {
    crate::rail::ToolGroup {
        badge: "INF",
        name: "Infra tools",
        ident: "State, inventory, checks and logs".into(),
        ready: true,
        tools: InfraTool::ALL
            .into_iter()
            .map(|t| crate::rail::Tool {
                badge: t.badge(),
                name: t.name(),
                desc: t.desc(),
                cmd: crate::actions::CommandId::OpenInfra(t),
                tag: "Open",
            })
            .collect(),
    }
}

// ------------------------------------------------------------------ helpers

/// `n thing` / `n things`.
pub(crate) fn plural(n: usize, one: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {one}s")
    }
}

/// A small caption over a field.
pub(crate) fn field(label: &str, child: impl IntoElement, p: &Palette) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(rpx(3.))
        .min_w_0()
        .child(
            div()
                .text_size(ts::SMALL)
                .text_color(p.fg3)
                .font_weight(FontWeight::MEDIUM)
                .child(label.to_owned()),
        )
        .child(child)
}

/// A bordered box holding an input.
pub(crate) fn input_box(p: &Palette) -> Div {
    div()
        .h(rpx(26.))
        .flex()
        .items_center()
        .px(rpx(6.))
        .border_1()
        .border_color(p.bd2)
        .rounded(px(5.))
        .bg(p.surface)
        .min_w_0()
}

/// A dropdown button choosing one of `items` (title, checked, value); picking calls `on`.
pub(crate) fn pick<V: 'static, T: Clone + 'static>(
    id: impl Into<gpui_kit::ElementId>,
    label: impl Into<SharedString>,
    items: Vec<(String, bool, T)>,
    this: WeakEntity<V>,
    on: fn(&mut V, T, &mut Context<V>),
) -> impl IntoElement {
    Button::new(id)
        .outline()
        .small()
        .label(label)
        .dropdown_menu(move |mut menu, _, _| {
            for (title, checked, v) in &items {
                let this = this.clone();
                let v = v.clone();
                menu = menu.item(
                    PopupMenuItem::new(title.clone())
                        .checked(*checked)
                        .on_click(move |_, _, cx| {
                            let _ = this.update(cx, |t, cx| on(t, v.clone(), cx));
                        }),
                );
            }
            menu
        })
}

/// A status / error line.
pub(crate) fn status_line(ok: bool, text: impl Into<SharedString>, p: &Palette) -> Div {
    div()
        .px(rpx(12.))
        .py(rpx(4.))
        .text_size(ts::BODY)
        .text_color(if ok { p.fg2 } else { p.prod })
        .child(text.into())
}

/// A tag pill.
pub(crate) fn tag(text: impl Into<SharedString>, color: gpui_kit::Hsla) -> AnyElement {
    div()
        .flex_none()
        .px(rpx(5.))
        .rounded(px(3.))
        .border_1()
        .border_color(color.opacity(0.5))
        .text_size(ts::TINY_PLUS)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(color)
        .child(text.into())
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_have_names() {
        for t in InfraTool::ALL {
            assert!(!t.name().is_empty() && !t.desc().is_empty() && !t.badge().is_empty());
        }
        assert_eq!(plural(1, "check"), "1 check");
        assert_eq!(plural(3, "check"), "3 checks");
    }
}
