//! A read-only tab showing an object's DDL from the catalog (`ObjectDetail::ddl`).
//! The text can be selected and copied; nothing here runs it.

use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::{
    AppContext as _, ClipboardItem, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, relative,
};

use crate::appearance::{rpx, ts};
use crate::theme::{MONO, palette};
use crate::ui::{self, Kind};

/// A read-only DDL tab.
pub struct DdlTab {
    /// `<connection id>/<qualified name>`: which object this tab shows.
    pub key: String,
    /// Tab title (`DDL · <name>`).
    pub title: SharedString,
    /// Engine badge for the tab strip.
    pub badge: &'static str,
    /// The qualified object name shown in the header.
    qualified: SharedString,
    ddl: String,
    editor: Entity<EditorState>,
}

impl DdlTab {
    /// A tab showing `ddl` for `qualified` (named `name` in the tab strip).
    pub fn new(
        key: String,
        name: &str,
        qualified: &str,
        badge: &'static str,
        ddl: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let text = SharedString::from(ddl.clone());
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
                .indent_guides(false)
                .soft_wrap(false)
                .default_value(text)
        });
        Self {
            key,
            title: format!("DDL · {name}").into(),
            badge,
            qualified: qualified.to_owned().into(),
            ddl,
            editor,
        }
    }
}

impl Render for DdlTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(p.surface)
            .child(
                div()
                    .h(rpx(30.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .px(rpx(12.))
                    .border_b_1()
                    .border_color(p.bd)
                    .bg(p.panel)
                    .text_size(ts::BODY)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_family(MONO)
                            .text_size(ts::LABEL)
                            .text_color(p.fg2)
                            .truncate()
                            .child(self.qualified.clone()),
                    )
                    .child(div().text_color(p.fg3).child("Read-only"))
                    .child(
                        ui::button("ddl-copy", "Copy", Kind::Secondary, &p).on_click(cx.listener(
                            |this, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(this.ddl.clone()));
                            },
                        )),
                    ),
            )
            .child(
                div().id("ddl-text").flex_1().min_h_0().child(
                    Editor::new(&self.editor)
                        // Read-only, not disabled: a disabled editor swallows mouse input,
                        // so the text could not be selected or copied.
                        .readonly(true)
                        .bordered(false)
                        .appearance(false)
                        .h(relative(1.))
                        .font_family(crate::appearance::editor_font_family(cx))
                        .text_size(crate::appearance::editor_font_size(cx)),
                ),
            )
    }
}
