//! Terminal tab. The GPU terminal view and SSH sessions land in milestone M2; until then
//! the tab shows its chrome and an explanation instead of a fake session.

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Context, FontWeight, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Window, div, px,
};
use switchyard_core::store::EnvironmentLabel;

use crate::theme::{MONO, palette};
use crate::ui::{self, Kind};

/// A terminal tab.
pub struct TerminalTab {
    /// Tab title (Host name or "Local shell").
    pub title: SharedString,
    /// Environment of the Host.
    pub env: EnvironmentLabel,
    remote: bool,
}

impl TerminalTab {
    /// A terminal for a Host (`remote`) or the local machine.
    pub fn new(title: String, env: EnvironmentLabel, remote: bool) -> Self {
        Self {
            title: title.into(),
            env,
            remote,
        }
    }
}

impl Render for TerminalTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(36.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(10.))
                    .border_b_1()
                    .border_color(p.bd)
                    .text_size(px(12.))
                    .child(ui::dot(p.env(self.env), 7.))
                    .child(div().font_weight(FontWeight::MEDIUM).child(self.title.clone()))
                    .child(
                        div()
                            .font_family(MONO)
                            .text_color(p.fg3)
                            .child(if self.remote { "SSH" } else { "local shell" }),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_color(p.fg2)
                            .child(ui::dot(p.fg3, 6.))
                            .child("Not connected"),
                    )
                    .child(ui::vdivider(&p, 16.))
                    .child(ui::button("t-split", "Split", Kind::Ghost, &p).h(px(24.)).text_color(p.fg3))
                    .child(ui::button("t-bcast", "Broadcast: off", Kind::Ghost, &p).h(px(24.)).text_color(p.fg3)),
            )
            .child(
                div()
                    .flex_1()
                    .bg(p.term)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .w(px(480.))
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .p(px(20.))
                            .border_1()
                            .border_dashed()
                            .border_color(p.bd2)
                            .rounded(px(10.))
                            .text_size(px(12.5))
                            .text_color(p.fg2)
                            .child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).text_color(p.fg).child("Terminals arrive in milestone M2"))
                            .child("SSH sessions (russh) with host-key verification, jump hosts and one shared session per Host, plus local shells through a pseudo-terminal. The GPU terminal view renders alacritty_terminal state.")
                            .when(self.remote, |d| {
                                d.child(div().font_family(MONO).text_size(px(11.5)).text_color(p.fg3).child("This Host is saved; its terminal will connect from this tab."))
                            }),
                    ),
            )
    }
}
