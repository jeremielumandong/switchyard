//! The Workbench's chrome, matched to the Claude Design handoff
//! (`API Workbench.dc.html` + `SPEC-api-workbench.md`).
//!
//! Every literal in the mock maps to a theme token — the table lives in the
//! plan and is repeated nowhere else — and every visible affordance is
//! backed by real workspace data or by a redacted hand-off to Chat through
//! [`AskAgentRequested`]. Nothing here touches storage or transport: the
//! handlers live in `mod.rs`, this file only paints and dispatches.
//!
//! The widget helpers at the top are the mock's reusable styles (`chip`,
//! `underline`, the lavender AI button, the 1px-gap grid table); the
//! `render_*` methods below are one per panel tab, in the order of the tab
//! strip.

use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants};
use gpui_kit::component::input::InputState;
use gpui_kit::component::menu::{ContextMenuExt, DropdownMenu, PopupMenu, PopupMenuItem};
use gpui_kit::component::resizable::{h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::{ActiveTheme, Disableable, IconName, Sizable};
use gpui_kit::{
    AnyElement, App, Context, Div, Entity, Hsla, MouseButton, Render, SharedString, Stateful, Svg,
    WeakEntity, Window, div, prelude::*, px, svg,
};

use super::move_request::{Destination, DraggedRequest};
use super::*;

// ---------------------------------------------------------------------------
// Shared widgets
// ---------------------------------------------------------------------------

/// Column widths for [`table`]: the mock's `grid-template-columns`.
#[derive(Clone, Copy)]
pub(super) enum Col {
    Px(f32),
    Flex,
}

pub(super) fn icon(name: &'static str, size: f32, color: Hsla) -> Svg {
    svg()
        .size(px(size))
        .flex_none()
        .path(crate::api::compat::icon_path(name))
        .text_color(color)
}

fn dot(size: f32, color: Hsla) -> Div {
    div()
        .size(px(size))
        .flex_none()
        .rounded(radius::full())
        .bg(color)
}

/// `font-size:10px;font-weight:700;letter-spacing:.08em` — the group and
/// column headings. GPUI has no letter-spacing, so the text is upper-cased
/// and left at that.
pub(super) fn heading(label: impl Into<String>, color: Hsla) -> Div {
    div()
        .text_size(text::S10)
        .font_weight(text::weight::BOLD)
        .text_color(color)
        .whitespace_nowrap()
        .child(label.into().to_uppercase())
}

/// The mock's `chip(active)`: panel tabs, response tabs, snippet languages.
pub(super) fn chip(id: String, label: impl Into<SharedString>, active: bool, cx: &App) -> Button {
    let selector = id.clone();
    Button::new(SharedString::from(id))
        .debug_selector(move || selector.clone())
        .small()
        .ghost()
        .label(label)
        .bg(if active {
            cx.theme().accent
        } else {
            cx.theme().transparent
        })
        .text_color(if active {
            cx.theme().foreground
        } else {
            cx.theme().muted_foreground
        })
}

fn underline_tab(id: String, label: impl Into<SharedString>, active: bool, cx: &App) -> Button {
    chip(id, label, active, cx)
}

/// A tinted verb (`GET`, `POST`…) in a fixed-width mono column.
fn verb(label: impl Into<SharedString>, tint: Hsla, width: f32, cx: &App) -> Div {
    div()
        .w(px(width))
        .flex_none()
        .font_family(crate::api::compat::fonts::mono(cx))
        .text_size(text::S9)
        .font_weight(text::weight::MEDIUM)
        .text_color(tint)
        .child(label.into())
}

/// A lavender "ask the model" button. Lavender is reserved for AI: nothing
/// else in the Workbench may use this helper.
fn lavender_button(
    id: String,
    label: impl Into<SharedString>,
    tall: bool,
    cx: &App,
) -> Stateful<Div> {
    let lavender = palette::lavender(cx);
    let selector = id.clone();
    div()
        .id(SharedString::from(id))
        .debug_selector(move || selector.clone())
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .rounded(radius::sm())
        .cursor_pointer()
        .whitespace_nowrap()
        .border_1()
        .border_color(lavender.opacity(0.5))
        .text_color(lavender)
        .map(|el| {
            if tall {
                el.h(px(32.))
                    .px(px(12.))
                    .bg(lavender.opacity(0.16))
                    .text_size(text::S12)
                    .font_weight(text::weight::SEMIBOLD)
            } else {
                el.px(px(10.))
                    .py(px(3.))
                    .bg(lavender.opacity(0.12))
                    .text_size(text::S11)
            }
        })
        .child(icon("spark", 11., lavender))
        .child(label.into())
}

/// A lavender AI action: `workbench-ask-ai-{slug}`, labelled by the intent,
/// routing to [`WorkbenchPanel::ask_agent`].
fn ai_button(
    intent: assist::AssistIntent,
    tall: bool,
    cx: &mut Context<WorkbenchPanel>,
) -> Stateful<Div> {
    lavender_button(
        format!("workbench-ask-ai-{}", intent.slug()),
        intent.label(),
        tall,
        cx,
    )
    .on_click(cx.listener(move |this, _, _, cx| this.ask_agent(intent, cx)))
}

/// The accent (`#7c8cff`) call to action: Send, Import, Run.
fn accent_button(
    id: String,
    label: impl Into<SharedString>,
    height: f32,
    cx: &App,
) -> Stateful<Div> {
    let colors = cx.theme().colors;
    let selector = id.clone();
    div()
        .id(SharedString::from(id))
        .debug_selector(move || selector.clone())
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .h(px(height))
        .px(if height >= 32. { px(16.) } else { px(10.) })
        .rounded(radius::sm())
        .cursor_pointer()
        .whitespace_nowrap()
        .bg(colors.primary)
        .border_1()
        .border_color(colors.primary)
        .text_color(colors.primary_foreground)
        .text_size(if height >= 32. { text::S12 } else { text::S11 })
        .font_weight(text::weight::SEMIBOLD)
        .child(label.into())
}

/// `h 24 · px 10 · border input · 11px` — the secondary chips.
pub(super) fn outline_chip(id: String, label: impl Into<SharedString>, _cx: &App) -> Button {
    let selector = id.clone();
    Button::new(SharedString::from(id))
        .debug_selector(move || selector.clone())
        .small()
        .outline()
        .label(label)
}

/// A 32×32 icon button with the mock's visible border.
fn icon_button(id: String, name: &'static str, tint: Option<Hsla>, cx: &App) -> Stateful<Div> {
    let colors = cx.theme().colors;
    let selector = id.clone();
    let color = tint.unwrap_or_else(|| palette::text_secondary(cx));
    div()
        .id(SharedString::from(id))
        .debug_selector(move || selector.clone())
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(32.))
        .rounded(radius::sm())
        .border_1()
        .border_color(tint.map(|tint| tint.opacity(0.45)).unwrap_or(colors.input))
        .cursor_pointer()
        .child(icon(name, 14., color))
}

/// `padding 12 · border · radius 8 · bg sidebar · mono 12 · lh 1.7`.
pub(super) fn code_box(cx: &App) -> Div {
    let colors = cx.theme().colors;
    div()
        .p(space::SP_3)
        .rounded(radius::md())
        .border_1()
        .border_color(colors.border)
        .bg(colors.sidebar)
        .font_family(crate::api::compat::fonts::mono(cx))
        .text_size(text::S12)
        .line_height(gpui_kit::relative(1.7))
        .text_color(palette::text_secondary(cx))
}

/// The tinted note (`border tint .35 · bg tint .06`) with an optional icon.
fn note_card(
    tint: Hsla,
    name: Option<&'static str>,
    body: impl Into<SharedString>,
    cx: &App,
) -> Div {
    div()
        .flex()
        .items_center()
        .gap(space::SP_2)
        .px(px(10.))
        .py(space::SP_2)
        .rounded(radius::sm())
        .border_1()
        .border_color(tint.opacity(0.35))
        .bg(tint.opacity(0.06))
        .text_size(text::S11)
        .text_color(palette::text_secondary(cx))
        .when_some(name, |el, name| el.child(icon(name, 14., tint)))
        .child(div().flex_1().min_w(px(0.)).child(body.into()))
}

/// The mock's `gap 1px; background: border` grid: a column of rows whose
/// 1px gaps show the border colour as grid lines.
pub(super) fn table(cx: &App) -> Div {
    let colors = cx.theme().colors;
    div()
        .flex()
        .flex_col()
        .gap(px(1.))
        .rounded(radius::md())
        .border_1()
        .border_color(colors.border)
        .bg(colors.border)
        .overflow_hidden()
        .font_family(crate::api::compat::fonts::mono(cx))
        .text_size(text::S11)
}

/// One row of a [`table`]. Header rows sit on `background`, body rows on
/// `muted`; cells are sized by `cols` and given `padding 6px 8px`.
pub(super) fn table_row(cols: &[Col], cells: Vec<AnyElement>, header: bool, cx: &App) -> Div {
    let colors = cx.theme().colors;
    let bg = if header {
        colors.background
    } else {
        colors.muted
    };
    let fg = if header {
        colors.muted_foreground
    } else {
        palette::text_secondary(cx)
    };
    div()
        .flex()
        .gap(px(1.))
        .min_w(px(0.))
        .children(cols.iter().zip(cells).map(|(col, cell)| {
            div()
                .px(space::SP_2)
                .py(px(6.))
                .min_w(px(0.))
                .bg(bg)
                .text_color(fg)
                .whitespace_nowrap()
                .overflow_hidden()
                .map(|el| match col {
                    Col::Px(width) => el.w(px(*width)).flex_none(),
                    Col::Flex => el.flex_1(),
                })
                .child(cell)
        }))
}

pub(super) fn header_cells(labels: &[&str]) -> Vec<AnyElement> {
    labels
        .iter()
        .map(|label| div().child(label.to_string()).into_any_element())
        .collect()
}

fn cell(text: impl Into<SharedString>) -> AnyElement {
    div().truncate().child(text.into()).into_any_element()
}

fn tinted_cell(text: impl Into<SharedString>, color: Hsla) -> AnyElement {
    div()
        .truncate()
        .text_color(color)
        .child(text.into())
        .into_any_element()
}

/// `✓` in success or `○` in tertiary — the enabled marker in every table.
fn tick(enabled: bool, cx: &App) -> AnyElement {
    if enabled {
        tinted_cell("✓", cx.theme().colors.success)
    } else {
        tinted_cell("○", palette::text_tertiary(cx))
    }
}

/// A pill (`padding 2px 8px; radius 9999`) on the raised surface.
fn pill(label: impl Into<SharedString>, fg: Hsla, cx: &App) -> Div {
    div()
        .flex_none()
        .px(space::SP_2)
        .py(px(2.))
        .rounded(radius::full())
        .bg(cx.theme().colors.accent)
        .text_size(text::S10)
        .text_color(fg)
        .whitespace_nowrap()
        .child(label.into())
}

/// A single-line field frame: `h 30 · px 10 · border input · radius 6 ·
/// bg sidebar · mono 12`. The `Input` inside is [`field::bare`].
pub(super) fn mono_field(state: &Entity<InputState>, window: &Window, cx: &App) -> Div {
    let colors = cx.theme().colors;
    let focused = state.focus_handle(cx).is_focused(window);
    div()
        .flex()
        .items_center()
        .h(px(30.))
        .px(px(10.))
        .rounded(radius::sm())
        .border_1()
        .border_color(if focused { colors.ring } else { colors.input })
        .bg(colors.sidebar)
        .font_family(crate::api::compat::fonts::mono(cx))
        .text_size(text::S12)
        .child(field::bare(state).w_full())
}

/// A menu row that tests can find: the label sits in a `div` carrying
/// `selector`, painted inside the menu's own clickable row.
fn menu_item(
    selector: String,
    label: impl Into<SharedString>,
    handle: &WeakEntity<WorkbenchPanel>,
    handler: impl Fn(&mut WorkbenchPanel, &mut Window, &mut Context<WorkbenchPanel>) + 'static,
) -> PopupMenuItem {
    let label = label.into();
    let handle = handle.clone();
    PopupMenuItem::element(move |_, _| {
        let selector = selector.clone();
        div()
            .debug_selector(move || selector.clone())
            .text_size(text::S11)
            .whitespace_nowrap()
            .child(label.clone())
    })
    .on_click(move |_, window, cx| {
        let _ = handle.update(cx, |panel, cx| handler(panel, window, cx));
    })
}

/// A dropdown trigger styled as one of the mock's pills. The label is a
/// child `div` with its own colour because the vendored `Button` paints
/// hovered text in `red_400` (an upstream quirk) — the child's colour wins.
fn menu_trigger(
    id: impl Into<SharedString>,
    bg: Hsla,
    border: Hsla,
    fg: Hsla,
    content: impl IntoElement,
    cx: &App,
) -> Button {
    let id: SharedString = id.into();
    let selector = id.clone();
    Button::new(id)
        .custom(
            ButtonCustomVariant::new(cx)
                .color(bg)
                .foreground(fg)
                .hover(bg)
                .active(bg),
        )
        .rounded(radius::sm())
        .border_1()
        .border_color(border)
        .debug_selector(move || selector.to_string())
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_color(fg)
                .child(content),
        )
}

/// The kebab on a rail row: a bare 20px trigger whose menu acts on that
/// row's collection, folder or request rather than on whatever happens to
/// be selected. It sits beside the row's click target, not inside it, so
/// opening the menu never selects (and never dirties) anything.
fn rail_item_menu(
    id: String,
    selector: String,
    cx: &App,
    build: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
) -> impl IntoElement {
    let colors = cx.theme().colors;
    menu_trigger(
        id,
        cx.theme().transparent,
        cx.theme().transparent,
        colors.muted_foreground,
        icon("kebab", 12., colors.muted_foreground),
        cx,
    )
    .debug_selector(move || selector.clone())
    .size(px(20.))
    .p_0()
    .dropdown_menu(move |menu: PopupMenu, window, cx| build(menu.min_w(px(200.)), window, cx))
}

fn request_move_menu(
    menu: PopupMenu,
    request: RequestId,
    destinations: Vec<Destination>,
    handle: WeakEntity<WorkbenchPanel>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    if destinations.is_empty() {
        return menu;
    }
    menu.submenu("Move request to…", window, cx, move |mut menu, _, _| {
        for destination in &destinations {
            let request = request.clone();
            let destination = destination.clone();
            let selector = format!(
                "workbench-move-request-{}-{}",
                destination.collection.as_str(),
                destination
                    .folder
                    .as_ref()
                    .map(FolderId::as_str)
                    .unwrap_or("root")
            );
            menu = menu.item(menu_item(
                selector,
                destination.label.clone(),
                &handle,
                move |this, _, cx| {
                    this.move_saved_request(
                        &request,
                        &destination.collection,
                        destination.folder.as_ref(),
                        cx,
                    )
                },
            ));
        }
        menu
    })
}

/// Which mock tint a status code takes: 2xx success, 4xx warm, else danger.
fn status_tint(status: u16, cx: &App) -> Hsla {
    let colors = cx.theme().colors;
    if (200..300).contains(&status) {
        colors.success
    } else if (400..500).contains(&status) {
        palette::warm(cx)
    } else if status == 0 {
        colors.danger
    } else if (300..400).contains(&status) {
        colors.info
    } else {
        colors.danger
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SaveState {
    Unsaved,
    Saved,
}

impl SaveState {
    /// A clean new draft is still unsaved until it has a durable definition.
    pub(super) fn for_request(has_definition: bool, dirty: bool) -> Self {
        if has_definition && !dirty {
            Self::Saved
        } else {
            Self::Unsaved
        }
    }

    pub(super) fn tint(self, cx: &App) -> Hsla {
        match self {
            Self::Unsaved => cx.theme().colors.warning,
            Self::Saved => cx.theme().colors.primary,
        }
    }

    pub(super) fn icon(self) -> &'static str {
        match self {
            Self::Unsaved => "save",
            Self::Saved => "check",
        }
    }
}

// ---------------------------------------------------------------------------
// Panel shell: header, banners, tab bodies
// ---------------------------------------------------------------------------

impl Render for WorkbenchPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = current_workspace_id();
        let has_workspace = self
            .workspaces
            .as_ref()
            .is_some_and(|list| !list.is_empty());
        if has_workspace && self.bound_workspace != workspace {
            if self.dirty {
                self.pending_workspace = Some(workspace);
                self.storage_error = Some(
                    "Workspace changed with unsaved edits. Save or discard them to continue."
                        .into(),
                );
            } else {
                self.rehydrate_workspace(workspace, window, cx);
            }
        }
        self.load_pending_request(window, cx);
        let colors = cx.theme().colors;
        if !has_workspace {
            return div()
                .id("api-workbench")
                .track_focus(&self.focus_handle)
                .key_context(KEY_CONTEXT)
                .flex()
                .flex_col()
                .size_full()
                .bg(colors.background)
                .child(self.render_empty_workbench(cx));
        }
        let body = match self.tab {
            Tab::Compose => self.render_compose(window, cx),
            Tab::Import => self.render_import(window, cx),
            Tab::Data => self.render_data(window, cx),
            Tab::Environments => self.render_envs(window, cx),
            Tab::History => self.render_history(window, cx),
            Tab::Diff => self.render_diff(cx),
        };
        div()
            .id("api-workbench")
            .track_focus(&self.focus_handle)
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(|this, _: &CancelRequest, _, cx| this.cancel(cx)))
            .when(self.tab == Tab::Compose, |el| {
                el.on_action(
                    cx.listener(|this, _: &crate::api::compat::Save, _, cx| this.save_clicked(cx)),
                )
                .on_action(cx.listener(|this, _: &SendRequest, window, cx| {
                    if this.send_state == SendState::Idle {
                        this.send(window, cx);
                    }
                }))
                .on_action(
                    cx.listener(|this, _: &NewRequest, window, cx| this.new_request(window, cx)),
                )
                .on_action(cx.listener(|this, _: &CloseRequest, window, cx| {
                    let id = this.request_tabs[this.active_request_tab].id;
                    this.close_request_tab(id, window, cx);
                }))
                .on_action(cx.listener(|this, _: &NextRequest, window, cx| {
                    let index = (this.active_request_tab + 1) % this.request_tabs.len();
                    this.activate_request_tab(this.request_tabs[index].id, window, cx);
                }))
                .on_action(cx.listener(|this, _: &PreviousRequest, window, cx| {
                    let index = (this.active_request_tab + this.request_tabs.len() - 1)
                        % this.request_tabs.len();
                    this.activate_request_tab(this.request_tabs[index].id, window, cx);
                }))
            })
            .flex()
            .flex_col()
            .size_full()
            .bg(colors.background)
            .child(self.render_header(cx))
            .when(self.storage_loading, |el| {
                el.child(
                    div()
                        .id("workbench-storage-loading")
                        .debug_selector(|| "workbench-storage-loading".into())
                        .flex_none()
                        .px(space::SP_4)
                        .py(space::SP_2)
                        .bg(colors.primary.opacity(0.08))
                        .text_size(text::S10)
                        .text_color(colors.muted_foreground)
                        .child(if self.ux.creation.busy() {
                            "Creating requests…"
                        } else {
                            "Loading project-scoped Workbench data…"
                        }),
                )
            })
            .when_some(self.storage_error.clone(), |el, error| {
                el.child(
                    div()
                        .id("workbench-storage-error")
                        .debug_selector(|| "workbench-storage-error".into())
                        .flex_none()
                        .px(space::SP_4)
                        .py(space::SP_2)
                        .bg(colors.danger.opacity(0.12))
                        .text_size(text::S10)
                        .text_color(colors.danger)
                        .child(format!("Workbench storage: {error}")),
                )
            })
            .when_some(self.navigation_notice.clone(), |el, notice| {
                el.child(
                    div()
                        .id("workbench-navigation-notice")
                        .debug_selector(|| "workbench-navigation-notice".into())
                        .flex_none()
                        .px(space::SP_4)
                        .py(space::SP_2)
                        .flex()
                        .items_center()
                        .gap(space::SP_2)
                        .bg(colors.warning.opacity(0.12))
                        .text_size(text::S10)
                        .text_color(colors.warning)
                        .child(div().flex_1().min_w(px(0.)).truncate().child(notice))
                        .when(self.dirty, |el| {
                            el.child(
                                outline_chip("workbench-notice-save".into(), "Save", cx)
                                    .border_color(colors.primary.opacity(0.45))
                                    .text_color(colors.primary)
                                    .on_click(cx.listener(|this, _, _, cx| this.save_clicked(cx))),
                            )
                            .child(
                                outline_chip("workbench-notice-discard".into(), "Discard", cx)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.discard_changes(window, cx)
                                    })),
                            )
                        }),
                )
            })
            .child(body)
    }
}

impl WorkbenchPanel {
    /// The environment the env chip and the Envs tab call active.
    pub(super) fn active_environment(&self) -> Option<&Environment> {
        let id = self.active_environment_id.as_ref()?;
        self.workspace_data
            .as_ref()?
            .environments
            .iter()
            .find(|environment| &environment.id == id)
    }

    /// The selected environment's stable user-facing label. The base URL is a
    /// request input, so using it for the picker label makes a rename invisible.
    pub(super) fn environment_display_name(&self) -> String {
        self.active_environment()
            .map(|environment| environment.name.clone())
            .unwrap_or_else(|| "No environment".into())
    }

    /// The env chip's status dot, shared with the Envs tab: warm when the
    /// active environment references a secret the vault cannot resolve, green
    /// when an environment is active, grey when none is.
    pub(super) fn environment_dot(&self, cx: &App) -> Hsla {
        let Some(environment) = self.active_environment() else {
            return palette::text_tertiary(cx);
        };
        if environment
            .variables
            .iter()
            .any(|variable| matches!(variable.value, VariableValue::MissingSecret(_)))
        {
            palette::warm(cx)
        } else {
            cx.theme().colors.success
        }
    }

    /// The environment picker shared by the header chip and the composer's
    /// base-URL pill: every saved environment, **No environment**, and the
    /// navigation to the Envs tab. Both switches go through
    /// [`WorkbenchPanel::activate_environment`] /
    /// [`WorkbenchPanel::deactivate_environment`], the only paths that also
    /// re-point the Envs editor that
    /// [`WorkbenchPanel::environment_base_url_value`] reads *first* — set the
    /// id alone and the departed environment's base URL stays in force.
    ///
    /// `prefix` namespaces the item selectors, because `debug_bounds` never
    /// forgets a selector it has painted once: two triggers sharing item
    /// selectors would leave a window test clicking the other menu's stale
    /// bounds.
    pub(super) fn environment_menu(
        &self,
        prefix: &'static str,
        cx: &Context<Self>,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static + use<>
    {
        let handle = cx.entity().downgrade();
        let active = self.active_environment_id.clone();
        let environments = self
            .workspace_data
            .as_ref()
            .map(|data| data.environments.clone())
            .unwrap_or_default();
        move |menu: PopupMenu, _window, _cx| {
            let mut menu = menu.min_w(px(200.));
            for environment in &environments {
                let id = environment.id.clone();
                let selected = active.as_ref() == Some(&id);
                menu = menu.item(
                    menu_item(
                        format!("{prefix}-pick-{}", id.as_str()),
                        environment.name.clone(),
                        &handle,
                        move |this, window, cx| this.activate_environment(id.clone(), window, cx),
                    )
                    .checked(selected),
                );
            }
            menu.separator()
                .item(
                    menu_item(
                        format!("{prefix}-pick-none"),
                        "No environment",
                        &handle,
                        |this, window, cx| this.deactivate_environment(window, cx),
                    )
                    .checked(active.is_none()),
                )
                .separator()
                .item(menu_item(
                    format!("{prefix}-manage"),
                    "Manage environments…",
                    &handle,
                    |this, _, cx| {
                        this.tab = Tab::Environments;
                        cx.notify();
                    },
                ))
        }
    }

    /// What the panel shows before any workspace exists: a blank page with
    /// one "Add workspace" button (or a loading line while the list loads).
    fn render_empty_workbench(&self, cx: &mut Context<Self>) -> Div {
        let colors = cx.theme().colors;
        let loading = self.workspaces.is_none();
        let adding = self.adding_workspace();
        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(space::SP_3)
            .p(space::SP_4)
            .child(
                div()
                    .text_size(text::S13)
                    .font_weight(text::weight::SEMIBOLD)
                    .text_color(colors.foreground)
                    .child(if loading {
                        "Loading workspaces…"
                    } else {
                        "No workspaces yet"
                    }),
            )
            .when(!loading, |el| {
                el.child(
                    div()
                        .max_w(px(360.))
                        .text_center()
                        .text_size(text::S11)
                        .text_color(palette::text_secondary(cx))
                        .child(
                            "A workspace holds your collections, environments and history. \
                             Add one to start sending requests.",
                        ),
                )
                .child(
                    Button::new("workbench-add-workspace")
                        .debug_selector(|| "workbench-add-workspace".into())
                        .primary()
                        .icon(IconName::Plus)
                        .label(if adding {
                            "Adding workspace…"
                        } else {
                            "Add workspace"
                        })
                        .disabled(adding)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.add_workspace(window, cx)),
                        ),
                )
            })
            .when_some(self.storage_error.clone(), |el, error| {
                el.child(
                    div()
                        .id("workbench-storage-error")
                        .text_size(text::S10)
                        .text_color(colors.danger)
                        .child(format!("Workbench storage: {error}")),
                )
            })
    }

    /// The header's workspace picker: every workspace, then "Rename workspace…"
    /// and "Add workspace".
    fn workspace_menu(
        &self,
        cx: &Context<Self>,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static + use<>
    {
        let handle = cx.entity().downgrade();
        let bound = self.bound_workspace.clone();
        let workspaces = self.workspace_entries().to_vec();
        move |menu: PopupMenu, _window, _cx| {
            let mut menu = menu.min_w(px(200.));
            for workspace in &workspaces {
                let id = workspace.id.clone();
                let selected = id == bound;
                menu = menu.item(
                    menu_item(
                        format!("workbench-workspace-pick-{}", id.as_str()),
                        workspace.name.clone(),
                        &handle,
                        move |this, window, cx| this.select_workspace(id.clone(), window, cx),
                    )
                    .checked(selected),
                );
            }
            menu.separator()
                .item(menu_item(
                    "workbench-workspace-rename".into(),
                    "Rename workspace…",
                    &handle,
                    |this, window, cx| this.open_rename_workspace(window, cx),
                ))
                .item(menu_item(
                    "workbench-workspace-add".into(),
                    "Add workspace",
                    &handle,
                    |this, window, cx| this.add_workspace(window, cx),
                ))
        }
    }

    /// `gap 16 · padding 8px 16px · border-b`: spark, title, the workspace
    /// picker, the six panel chips (the only shrinking child) and the env +
    /// Sync chips.
    fn render_header(&self, cx: &mut Context<Self>) -> Div {
        let colors = cx.theme().colors;
        let environment_name = self.environment_display_name();
        div()
            .flex()
            .items_center()
            .gap(space::SP_4)
            .px(space::SP_4)
            .py(space::SP_2)
            .border_b_1()
            .border_color(colors.border)
            .child(icon("spark", 18., colors.muted_foreground))
            .child(
                div()
                    .flex_none()
                    .text_size(text::S13)
                    .font_weight(text::weight::SEMIBOLD)
                    .text_color(colors.foreground)
                    .whitespace_nowrap()
                    .child("API Workbench"),
            )
            .child(
                menu_trigger(
                    "workbench-workspace-chip",
                    cx.theme().transparent,
                    colors.input,
                    palette::text_secondary(cx),
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(icon("folder", 12., colors.muted_foreground))
                        .child(
                            div()
                                .max_w(px(180.))
                                .truncate()
                                .text_size(text::S11)
                                .child(self.workspace_display_name()),
                        )
                        .child(icon("chevron-down", 12., colors.muted_foreground)),
                    cx,
                )
                .flex_none()
                .h(px(24.))
                .px(px(10.))
                .dropdown_menu(self.workspace_menu(cx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_x_scrollbar()
                    // `Scrollable` wraps this element, so the tabs need
                    // their own flex row inside its scroll area.
                    .child(div().flex().flex_row().gap(space::SP_1).children(
                        Tab::ALL.into_iter().map(|tab| {
                            chip(
                                format!("workbench-tab-{}", tab.id()),
                                tab.label(),
                                self.tab == tab,
                                cx,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.tab = tab;
                                    cx.notify();
                                },
                            ))
                        }),
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(space::SP_2)
                    .child(
                        Button::new("workbench-open-vault")
                            .label("Secret vault")
                            .ghost()
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(OpenVaultRequested);
                            })),
                    )
                    .child(
                        menu_trigger(
                            "workbench-env-chip",
                            cx.theme().transparent,
                            colors.input,
                            palette::text_secondary(cx),
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .child(dot(6., self.environment_dot(cx)))
                                .child(
                                    div()
                                        .font_family(crate::api::compat::fonts::mono(cx))
                                        .text_size(text::S11)
                                        .child(environment_name),
                                )
                                .child(icon("chevron-down", 12., colors.muted_foreground)),
                            cx,
                        )
                        .h(px(24.))
                        .px(px(10.))
                        .dropdown_menu(self.environment_menu("workbench-env", cx)),
                    )
                    .child(
                        outline_chip("workbench-sync".into(), "Sync", cx)
                            .child(icon("refresh", 12., palette::text_secondary(cx)))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.sync_workspace(window, cx);
                            })),
                    ),
            )
    }
}

// ---------------------------------------------------------------------------
// Compose
// ---------------------------------------------------------------------------

impl WorkbenchPanel {
    fn render_compose(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let rail = self.render_collection_rail(window, cx);
        let editor = self.render_composer_content(window, cx);
        let response = self.render_response(window, cx);
        let response_axis = if self.ux.layout.side_by_side {
            gpui_kit::Axis::Horizontal
        } else {
            gpui_kit::Axis::Vertical
        };
        let composer_size = self.ux.layout.composer_size(window);
        let minimum_request =
            window.rem_size() * if self.ux.layout.side_by_side { 16. } else { 4. };
        let minimum_response =
            window.rem_size() * if self.ux.layout.side_by_side { 20. } else { 8. };
        let split = h_resizable("workbench-main-split")
            .with_state(&self.ux.rail_split)
            .on_resize(cx.listener(
                |this,
                 state: &Entity<gpui_kit::component::resizable::ResizableState>,
                 window,
                 cx| {
                    if let Some(size) = state.read(cx).sizes().first() {
                        this.ux.layout.rail_rem = *size / window.rem_size();
                        this.persist_layout(cx);
                    }
                },
            ))
            .child(
                resizable_panel()
                    .size(window.rem_size() * self.ux.layout.rail_rem)
                    .size_range(window.rem_size() * 10. ..window.rem_size() * 30.)
                    .child(rail),
            )
            .child(
                resizable_panel().child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.))
                        .min_h(px(0.))
                        .child(self.render_request_tabs(cx))
                        .child(self.render_url_row(window, cx))
                        .child(self.render_composer_tabs(cx))
                        .child(
                            div()
                                .flex_1()
                                .min_h(px(0.))
                                .relative()
                                .overflow_hidden()
                                .child(
                                div().absolute().inset_0().child(
                                    v_resizable("workbench-response-split")
                                        .axis(response_axis)
                                        .with_state(&self.ux.response_split)
                                        .on_resize(cx.listener(
                                            |this,
                                             state: &Entity<
                                                gpui_kit::component::resizable::ResizableState,
                                            >,
                                             window,
                                             cx| {
                                                if let Some(size) = state.read(cx).sizes().first() {
                                                    let size = Some(*size / window.rem_size());
                                                    if this.ux.layout.side_by_side {
                                                        this.ux.layout.horizontal_rem = size;
                                                    } else {
                                                        this.ux.layout.stacked_rem = size;
                                                    }
                                                    this.persist_layout(cx);
                                                }
                                            },
                                        ))
                                        .child(
                                            resizable_panel()
                                                .when_some(composer_size, |panel, size| {
                                                    panel.size(size)
                                                })
                                                .size_range(minimum_request..gpui_kit::Pixels::MAX)
                                                .child(editor),
                                        )
                                        .child(
                                            resizable_panel()
                                                .size_range(minimum_response..gpui_kit::Pixels::MAX)
                                                .child(response),
                                        ),
                                ),
                            ),
                        ),
                ),
            );
        div()
            .flex_1()
            .min_h(px(0.))
            .relative()
            .overflow_hidden()
            .child(div().absolute().inset_0().child(split))
            .into_any_element()
    }

    /// The 248px rail: filter + `+` menu, then one group per collection with
    /// the selected collection's folder/request tree beneath it.
    fn render_collection_rail(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let handle = cx.entity().downgrade();
        let filter = self.rail_filter.read(cx).value().trim().to_lowercase();
        let rail_filter_focused = self.rail_filter.focus_handle(cx).is_focused(window);
        let add_menu = menu_trigger(
            "workbench-rail-add",
            colors.primary.opacity(0.12),
            colors.primary.opacity(0.45),
            colors.primary,
            icon("plus", 12., colors.primary),
            cx,
        )
        .size(px(26.))
        .p_0()
        .dropdown_menu(move |menu: PopupMenu, _window, _cx| {
            menu.min_w(px(180.))
                .item(menu_item(
                    "workbench-new-request".into(),
                    "Add request",
                    &handle,
                    |this, window, cx| this.new_request(window, cx),
                ))
                .item(menu_item(
                    "workbench-new-folder".into(),
                    "New folder",
                    &handle,
                    |this, window, cx| this.new_folder(window, cx),
                ))
                .item(menu_item(
                    "workbench-new-collection".into(),
                    "New collection",
                    &handle,
                    |this, window, cx| this.new_collection(window, cx),
                ))
                .item(menu_item(
                    "workbench-move-to-folder".into(),
                    "Move request into selected folder",
                    &handle,
                    |this, _, cx| this.move_request_to_selected_folder(cx),
                ))
        });
        div()
            .id("workbench-collection-rail")
            .track_focus(&self.rail_focus)
            .key_context(RAIL_KEY_CONTEXT)
            .on_action(cx.listener(|this, _: &RenameRailItem, window, cx| {
                this.rename_rail_selection(window, cx)
            }))
            .w_full()
            .flex_none()
            .flex()
            .flex_col()
            .min_h(px(0.))
            .border_r_1()
            .border_color(colors.border)
            .bg(colors.sidebar)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .p(space::SP_2)
                    .border_b_1()
                    .border_color(colors.sidebar_border)
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w(px(0.))
                            .items_center()
                            .gap(px(6.))
                            .h(px(26.))
                            .px(space::SP_2)
                            .rounded(radius::sm())
                            .border_1()
                            .border_color(if rail_filter_focused {
                                colors.ring
                            } else {
                                colors.border
                            })
                            .bg(colors.background)
                            .text_size(text::S11)
                            .child(icon("search", 12., palette::text_tertiary(cx)))
                            .child(field::bare(&self.rail_filter).flex_1().min_w(px(0.))),
                    )
                    .child(add_menu),
            )
            .child(
                div().flex_1().min_h(px(0.)).overflow_y_scrollbar().child(
                    div()
                        .flex()
                        .flex_col()
                        .pt(px(6.))
                        .px(px(6.))
                        .pb(space::SP_3)
                        .children(self.render_rail_groups(&filter, window, cx)),
                ),
            )
            .into_any_element()
    }

    /// `COLLECTION · name` headings, one per collection, with the selected
    /// collection's tree ([`rail_rows`]) under its heading.
    fn render_rail_groups(
        &self,
        filter: &str,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let colors = cx.theme().colors;
        let Some(data) = self.workspace_data.as_ref() else {
            return vec![
                div()
                    .px(px(6.))
                    .py(px(5.))
                    .text_size(text::S11)
                    .text_color(palette::text_tertiary(cx))
                    .child("Storage unavailable")
                    .into_any_element(),
            ];
        };
        let mut out = Vec::new();
        if data.collections.is_empty() {
            out.push(
                div()
                    .px(px(6.))
                    .py(px(5.))
                    .text_size(text::S11)
                    .text_color(palette::text_tertiary(cx))
                    .child("No collections yet. Use + → New collection.")
                    .into_any_element(),
            );
        }
        let handle = cx.entity().downgrade();
        for (index, collection) in data.collections.iter().enumerate() {
            let id = collection.id.clone();
            let selected = self.current_collection_id.as_ref() == Some(&id);
            let expanded = selected && self.collection_tree_expanded;
            let heading_label = format!("Collection · {}", collection.name);
            let build_menu = {
                let handle = handle.clone();
                let id = id.clone();
                move |menu: PopupMenu, _: &mut Window, _: &mut Context<PopupMenu>| {
                    let request_in = id.clone();
                    let folder_in = id.clone();
                    let rename = id.clone();
                    let settings = id.clone();
                    let replace_urls = id.clone();
                    let delete = id.clone();
                    let expand = id.clone();
                    let collapse = id.clone();
                    let run = id.clone();
                    let export_agentops = id.clone();
                    let export_postman = id.clone();
                    menu.item(menu_item(
                        "workbench-collection-menu-settings".into(),
                        "Collection settings…",
                        &handle,
                        move |this, window, cx| {
                            this.open_collection_settings(settings.clone(), window, cx)
                        },
                    ))
                    .item(menu_item(
                        "workbench-collection-menu-new-request".into(),
                        "Add request",
                        &handle,
                        move |this, window, cx| {
                            this.new_request_in(request_in.clone(), None, window, cx)
                        },
                    ))
                    .item(menu_item(
                        "workbench-collection-menu-new-folder".into(),
                        "New folder",
                        &handle,
                        move |this, window, cx| {
                            this.new_folder_in(folder_in.clone(), None, window, cx)
                        },
                    ))
                    .item(menu_item(
                        "workbench-collection-menu-replace-urls".into(),
                        "Replace request URLs…",
                        &handle,
                        move |this, window, cx| {
                            this.open_url_replacement(replace_urls.clone(), window, cx)
                        },
                    ))
                    .item(menu_item(
                        "workbench-collection-menu-rename".into(),
                        "Rename collection…",
                        &handle,
                        move |this, window, cx| {
                            this.open_rail_rename(
                                RenameTarget::Collection(rename.clone()),
                                window,
                                cx,
                            )
                        },
                    ))
                    .separator()
                    .item(menu_item(
                        "workbench-collection-menu-run".into(),
                        "Run collection",
                        &handle,
                        move |this, window, cx| this.run_from_rail(run.clone(), None, window, cx),
                    ))
                    .item(menu_item(
                        "workbench-collection-menu-export-agentops".into(),
                        "Export collection · AgentOps JSON…",
                        &handle,
                        move |this, _, cx| {
                            this.export_from_rail(export_agentops.clone(), None, false, cx)
                        },
                    ))
                    .item(menu_item(
                        "workbench-collection-menu-export-postman".into(),
                        "Export collection · Postman v2.1…",
                        &handle,
                        move |this, _, cx| {
                            this.export_from_rail(export_postman.clone(), None, true, cx)
                        },
                    ))
                    .separator()
                    .item(menu_item(
                        "workbench-collection-menu-expand-folders".into(),
                        "Expand all folders",
                        &handle,
                        move |this, _, cx| this.set_collection_folders_expanded(&expand, true, cx),
                    ))
                    .item(menu_item(
                        "workbench-collection-menu-collapse-folders".into(),
                        "Collapse all folders",
                        &handle,
                        move |this, _, cx| {
                            this.set_collection_folders_expanded(&collapse, false, cx)
                        },
                    ))
                    .separator()
                    .item(menu_item(
                        "workbench-collection-menu-delete".into(),
                        "Delete collection",
                        &handle,
                        move |this, window, cx| {
                            if this.focus_collection(delete.clone(), window, cx) {
                                this.delete_current_collection(window, cx);
                            }
                        },
                    ))
                }
            };
            let rename_field = self
                .rail_rename_for(&RenameTarget::Collection(collection.id.clone()))
                .map(|rename| self.render_rail_rename(rename, 0., window, cx));
            let menu = rail_item_menu(
                format!("workbench-collection-{}-menu", collection.id.as_str()),
                format!("workbench-collection-{index}-menu"),
                cx,
                build_menu.clone(),
            );
            let drop_collection = collection.id.clone();
            out.push(
                div()
                    .id(SharedString::from(format!(
                        "workbench-collection-context-{}",
                        collection.id.as_str()
                    )))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(2.))
                            .pr(px(2.))
                            .when(index > 0, |el| el.mt(space::SP_2))
                            .child(
                                div()
                                    .id(SharedString::from(format!(
                                        "workbench-collection-{}",
                                        collection.id.as_str()
                                    )))
                                    .debug_selector(move || format!("workbench-collection-{index}"))
                                    .flex()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .items_center()
                                    .gap(px(6.))
                                    .px(px(6.))
                                    .py(px(5.))
                                    .rounded(radius::sm())
                                    .cursor_pointer()
                                    .hover(|style| style.bg(colors.accent))
                                    .drag_over::<DraggedRequest>(|style, _, _, cx| {
                                        style.bg(cx.theme().accent)
                                    })
                                    .on_drop(cx.listener(
                                        move |this, dragged: &DraggedRequest, _, cx| {
                                            this.move_saved_request(
                                                &dragged.id,
                                                &drop_collection,
                                                None,
                                                cx,
                                            );
                                        },
                                    ))
                                    .on_click(cx.listener(move |this, event, window, cx| {
                                        let target = RenameTarget::Collection(id.clone());
                                        if !this.rail_row_clicked(target, event, window, cx) {
                                            this.toggle_collection_tree(id.clone(), window, cx)
                                        }
                                    }))
                                    .child(icon(
                                        if expanded {
                                            "chevron-down"
                                        } else {
                                            "chevron-right"
                                        },
                                        12.,
                                        colors.muted_foreground,
                                    ))
                                    .child(
                                        heading(heading_label, colors.muted_foreground).truncate(),
                                    ),
                            )
                            .child(menu)
                            .context_menu(build_menu),
                    )
                    .into_any_element(),
            );
            // The rename field takes the heading's place while it is open.
            if let (Some(field), Some(heading)) = (rename_field, out.last_mut()) {
                *heading = field;
            }
            if !expanded {
                continue;
            }
            let rows = rail_rows(data, Some(&collection.id));
            if !rows
                .iter()
                .any(|row| matches!(row, RailRow::Request { .. }))
            {
                out.push(
                    div()
                        .id("workbench-rail-empty")
                        .debug_selector(|| "workbench-rail-empty".into())
                        .pl(px(24.))
                        .py(px(5.))
                        .text_size(text::S11)
                        .text_color(palette::text_tertiary(cx))
                        .child("No requests in this collection.")
                        .into_any_element(),
                );
            }
            out.extend(self.render_rail_rows(rows, filter, window, cx));
        }
        out
    }

    /// The selected collection's tree — request rows are numbered
    /// `workbench-rail-{index}` in tree order, folder headings
    /// `workbench-folder-{index}`.
    fn render_rail_rows(
        &self,
        rows: Vec<RailRow<'_>>,
        filter: &str,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let colors = cx.theme().colors;
        let handle = cx.entity().downgrade();
        let mut request_index = 0;
        let mut folder_index = 0;
        let counts = folder_request_counts(&rows);
        // Filtering reveals requests inside collapsed folders temporarily.
        let rows = if filter.is_empty() {
            expanded_rail_rows(rows, &self.collapsed_folder_ids)
        } else {
            rows
        };
        rows.into_iter()
            .filter_map(|row| match row {
                RailRow::Folder { depth, folder } => {
                    let index = folder_index;
                    folder_index += 1;
                    let id = folder.id.clone();
                    if let Some(rename) = self.rail_rename_for(&RenameTarget::Folder(id.clone())) {
                        let indent = 12. * (depth as f32 + 1.);
                        return Some(self.render_rail_rename(rename, indent, window, cx));
                    }
                    let toggle_id = id.clone();
                    let expanded = !filter.is_empty() || !self.collapsed_folder_ids.contains(&id);
                    let toggle_selector = format!("workbench-folder-toggle-{}", id.as_str());
                    let count = counts.get(&id).copied().unwrap_or_default();
                    let assigned = self.selected_folder().as_ref() == Some(&folder.id);
                    let build_menu = {
                        let handle = handle.clone();
                        let collection_id = folder.collection_id.clone();
                        let id = id.clone();
                        move |menu: PopupMenu, _: &mut Window, _: &mut Context<PopupMenu>| {
                            let request_in = (collection_id.clone(), id.clone());
                            let folder_in = (collection_id.clone(), id.clone());
                            let rename = id.clone();
                            let settings = id.clone();
                            let delete = id.clone();
                            let run = (collection_id.clone(), id.clone());
                            let export_agentops = (collection_id.clone(), id.clone());
                            let export_postman = (collection_id.clone(), id.clone());
                            menu.item(menu_item(
                                "workbench-folder-menu-settings".into(),
                                "Folder settings…",
                                &handle,
                                move |this, window, cx| {
                                    this.open_folder_settings(settings.clone(), window, cx)
                                },
                            ))
                            .item(menu_item(
                                "workbench-folder-menu-new-request".into(),
                                "Add request",
                                &handle,
                                move |this, window, cx| {
                                    let (collection, folder) = request_in.clone();
                                    this.new_request_in(collection, Some(folder), window, cx)
                                },
                            ))
                            .item(menu_item(
                                "workbench-folder-menu-new-folder".into(),
                                "New subfolder",
                                &handle,
                                move |this, window, cx| {
                                    let (collection, folder) = folder_in.clone();
                                    this.new_folder_in(collection, Some(folder), window, cx)
                                },
                            ))
                            .item(menu_item(
                                "workbench-folder-menu-rename".into(),
                                "Rename folder…",
                                &handle,
                                move |this, window, cx| {
                                    this.open_rail_rename(
                                        RenameTarget::Folder(rename.clone()),
                                        window,
                                        cx,
                                    )
                                },
                            ))
                            .separator()
                            .item(menu_item(
                                "workbench-folder-menu-run".into(),
                                "Run folder",
                                &handle,
                                move |this, window, cx| {
                                    let (collection, folder) = run.clone();
                                    this.run_from_rail(collection, Some(folder), window, cx)
                                },
                            ))
                            .item(menu_item(
                                "workbench-folder-menu-export-agentops".into(),
                                "Export folder · AgentOps JSON…",
                                &handle,
                                move |this, _, cx| {
                                    let (collection, folder) = export_agentops.clone();
                                    this.export_from_rail(collection, Some(folder), false, cx)
                                },
                            ))
                            .item(menu_item(
                                "workbench-folder-menu-export-postman".into(),
                                "Export folder · Postman v2.1…",
                                &handle,
                                move |this, _, cx| {
                                    let (collection, folder) = export_postman.clone();
                                    this.export_from_rail(collection, Some(folder), true, cx)
                                },
                            ))
                            .separator()
                            .item(menu_item(
                                "workbench-folder-menu-delete".into(),
                                "Delete folder",
                                &handle,
                                move |this, window, cx| {
                                    this.select_folder(delete.clone(), window, cx);
                                    this.delete_current_folder(cx);
                                },
                            ))
                        }
                    };
                    let menu = rail_item_menu(
                        format!("workbench-folder-{}-menu", folder.id.as_str()),
                        format!("workbench-folder-{index}-menu"),
                        cx,
                        build_menu.clone(),
                    );
                    let drop_folder = id.clone();
                    let drop_collection = folder.collection_id.clone();
                    Some(
                        div()
                            .id(SharedString::from(format!(
                                "workbench-folder-context-{}",
                                folder.id.as_str()
                            )))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(2.))
                                    .ml(px(12. * (depth as f32 + 1.)))
                                    .mt(space::SP_1)
                                    .pr(px(2.))
                                    .child(
                                        Button::new(SharedString::from(format!(
                                            "folder-disclosure-{}",
                                            id.as_str()
                                        )))
                                        .debug_selector(move || toggle_selector.clone())
                                        .ghost()
                                        .xsmall()
                                        .icon(if expanded {
                                            IconName::ChevronDown
                                        } else {
                                            IconName::ChevronRight
                                        })
                                        .tooltip(if !filter.is_empty() {
                                            "Folders stay expanded while filtering".to_string()
                                        } else {
                                            format!(
                                                "{} {}",
                                                if expanded { "Collapse" } else { "Expand" },
                                                folder.name
                                            )
                                        })
                                        .disabled(!filter.is_empty())
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.toggle_folder_tree(toggle_id.clone(), cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        div()
                                            .id(SharedString::from(format!(
                                                "workbench-folder-{}",
                                                folder.id.as_str()
                                            )))
                                            .debug_selector(move || {
                                                format!("workbench-folder-{index}")
                                            })
                                            .flex()
                                            .flex_1()
                                            .min_w(px(0.))
                                            .items_center()
                                            .gap(px(6.))
                                            .px(px(6.))
                                            .py(px(5.))
                                            .rounded(radius::sm())
                                            .cursor_pointer()
                                            .when(assigned, |el| el.bg(colors.accent))
                                            .hover(|style| style.bg(colors.accent))
                                            .drag_over::<DraggedRequest>(|style, _, _, cx| {
                                                style.bg(cx.theme().accent)
                                            })
                                            .on_drop(cx.listener(
                                                move |this, dragged: &DraggedRequest, _, cx| {
                                                    this.move_saved_request(
                                                        &dragged.id,
                                                        &drop_collection,
                                                        Some(&drop_folder),
                                                        cx,
                                                    );
                                                },
                                            ))
                                            .on_click(cx.listener(
                                                move |this, event, window, cx| {
                                                    let target = RenameTarget::Folder(id.clone());
                                                    if !this
                                                        .rail_row_clicked(target, event, window, cx)
                                                    {
                                                        this.select_folder(id.clone(), window, cx)
                                                    }
                                                },
                                            ))
                                            .child(icon("folder", 12., colors.muted_foreground))
                                            .child(
                                                heading(
                                                    folder.name.clone(),
                                                    colors.muted_foreground,
                                                )
                                                .truncate(),
                                            )
                                            .child(div().flex_1())
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(colors.muted_foreground)
                                                    .child(count.to_string()),
                                            ),
                                    )
                                    .child(menu)
                                    .context_menu(build_menu),
                            )
                            .into_any_element(),
                    )
                }
                RailRow::Request { depth, request } => {
                    let index = request_index;
                    let matches = filter.is_empty()
                        || request.name.to_lowercase().contains(filter)
                        || request.url.to_lowercase().contains(filter);
                    if !matches {
                        return None;
                    }
                    request_index += 1;
                    if let Some(rename) =
                        self.rail_rename_for(&RenameTarget::Request(request.id.clone()))
                    {
                        let indent = 12. * (depth as f32 + 1.);
                        return Some(self.render_rail_rename(rename, indent, window, cx));
                    }
                    // Row callbacks retain identity, not the potentially large
                    // body, scripts and credentials of every saved request.
                    let request_for_click = request.id.clone();
                    let method = request.method.as_str().to_string();
                    let active = self.current_request_id.as_ref() == Some(&request.id);
                    let dragged = DraggedRequest {
                        id: request.id.clone(),
                        name: request.name.clone(),
                    };
                    let build_menu = {
                        let handle = handle.clone();
                        let request = request.id.clone();
                        move |menu: PopupMenu, window: &mut Window, cx: &mut Context<PopupMenu>| {
                            let delete = request.clone();
                            let rename = request.clone();
                            let open = request.clone();
                            let duplicate = request.clone();
                            let menu = menu
                                .item(menu_item(
                                    "workbench-request-menu-open".into(),
                                    "Open request",
                                    &handle,
                                    move |this, window, cx| {
                                        this.open_saved_request(&open, window, cx);
                                    },
                                ))
                                .item(menu_item(
                                    "workbench-request-menu-duplicate".into(),
                                    "Duplicate request",
                                    &handle,
                                    move |this, window, cx| {
                                        this.open_saved_request(&duplicate, window, cx);
                                        if this.current_request_id.as_ref() == Some(&duplicate) {
                                            let tab = this.request_tabs[this.active_request_tab].id;
                                            this.run_request_tab_action(
                                                tab_menu::Action::Duplicate,
                                                tab,
                                                window,
                                                cx,
                                            );
                                        }
                                    },
                                ))
                                .separator()
                                .item(menu_item(
                                    "workbench-request-menu-rename".into(),
                                    "Rename request…",
                                    &handle,
                                    move |this, window, cx| {
                                        this.open_rail_rename(
                                            RenameTarget::Request(rename.clone()),
                                            window,
                                            cx,
                                        )
                                    },
                                ));
                            let destinations = handle
                                .upgrade()
                                .map(|panel| panel.read(cx).request_destinations(&request))
                                .unwrap_or_default();
                            let menu = request_move_menu(
                                menu,
                                request.clone(),
                                destinations,
                                handle.clone(),
                                window,
                                cx,
                            );
                            menu.item(menu_item(
                                "workbench-request-menu-delete".into(),
                                "Delete request",
                                &handle,
                                move |this, window, cx| this.delete_request(&delete, window, cx),
                            ))
                        }
                    };
                    let menu = rail_item_menu(
                        format!("workbench-request-{}-menu", request.id.as_str()),
                        format!("workbench-rail-{index}-menu"),
                        cx,
                        build_menu.clone(),
                    );
                    Some(
                        div()
                            .id(SharedString::from(format!(
                                "workbench-request-context-{}",
                                request.id.as_str()
                            )))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(2.))
                                    .ml(px(12. * (depth as f32 + 1.)))
                                    .pr(px(2.))
                                    .child(
                                        div()
                                            .id(SharedString::from(format!(
                                                "workbench-request-{}",
                                                request.id.as_str()
                                            )))
                                            .debug_selector(move || {
                                                format!("workbench-rail-{index}")
                                            })
                                            .flex()
                                            .flex_1()
                                            .min_w(px(0.))
                                            .items_center()
                                            .gap(space::SP_2)
                                            .px(px(6.))
                                            .py(px(5.))
                                            .rounded(radius::sm())
                                            .cursor_pointer()
                                            .when(active, |el| el.bg(colors.accent))
                                            .hover(|style| style.bg(colors.accent))
                                            .on_drag(dragged, |dragged, _, _, cx| {
                                                cx.stop_propagation();
                                                cx.new(|_| dragged.clone())
                                            })
                                            .text_color(if active {
                                                colors.foreground
                                            } else {
                                                palette::text_secondary(cx)
                                            })
                                            .on_click(cx.listener(
                                                move |this, event, window, cx| {
                                                    let target = RenameTarget::Request(
                                                        request_for_click.clone(),
                                                    );
                                                    if !this
                                                        .rail_row_clicked(target, event, window, cx)
                                                    {
                                                        this.open_saved_request(
                                                            &request_for_click,
                                                            window,
                                                            cx,
                                                        );
                                                    }
                                                },
                                            ))
                                            .child(verb(
                                                method.clone(),
                                                self.method_tint_label(&method, cx),
                                                44.,
                                                cx,
                                            ))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w(px(0.))
                                                    .truncate()
                                                    .font_family(crate::api::compat::fonts::mono(
                                                        cx,
                                                    ))
                                                    .text_size(text::S11)
                                                    .child(request.name.clone()),
                                            ),
                                    )
                                    .child(menu)
                                    .context_menu(build_menu),
                            )
                            .into_any_element(),
                    )
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Compose: request tab strip, URL row, composer sub-tabs
// ---------------------------------------------------------------------------

impl WorkbenchPanel {
    /// The open request drafts as a tab strip. Only the active tab owns the
    /// editor field; inactive tabs render their retained snapshot by name.
    fn render_request_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let colors = cx.theme().colors;
        let active_id = self
            .request_tabs
            .get(self.active_request_tab)
            .map(|tab| tab.id);
        if self.ux.revealed_request_tab.replace(active_id) != active_id {
            self.ux
                .request_tabs_scroll
                .scroll_to_item(self.active_request_tab);
        }
        let mut strip = div()
            .id("workbench-request-tabs-scroll")
            .debug_selector(|| "workbench-request-tabs-scroll".into())
            .flex()
            .flex_1()
            .min_w_0()
            .overflow_x_scroll()
            .track_scroll(&self.ux.request_tabs_scroll)
            .h(px(32.))
            .bg(colors.sidebar);
        for (index, tab) in self.request_tabs.iter().enumerate() {
            let active = index == self.active_request_tab;
            let id = tab.id;
            let selector = format!("workbench-request-tab-{id}");
            let name = tab.name().to_string();
            let dirty = if active { self.dirty } else { tab.dirty };
            let save_state = SaveState::for_request(
                if active {
                    self.current_definition.is_some()
                } else {
                    tab.current_definition.is_some()
                },
                dirty,
            );
            let tab_element =
                div()
                    .id(SharedString::from(selector.clone()))
                    .debug_selector(move || selector.clone())
                    .flex()
                    .flex_col()
                    .flex_none()
                    .w(px(220.))
                    .border_r_1()
                    .border_color(colors.sidebar_border)
                    .bg(if active {
                        colors.background
                    } else {
                        colors.sidebar
                    })
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, _, _, cx| {
                            this.right_clicked_request_tab = Some(id);
                            cx.notify();
                        }),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.activate_request_tab(id, window, cx)
                    }))
                    // The mock's `inset 0 2px 0 0 #7c8cff`: a 2px accent
                    // strip along the top of the active tab.
                    .child(div().h(px(2.)).flex_none().bg(if active {
                        colors.primary
                    } else {
                        colors.sidebar
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .items_center()
                            .gap(space::SP_2)
                            .px(space::SP_3)
                            .text_color(if active {
                                colors.foreground
                            } else {
                                colors.muted_foreground
                            })
                            .font_family(crate::api::compat::fonts::mono(cx))
                            .text_size(text::S11)
                            .child(dot(6., save_state.tint(cx)))
                            .child(if active {
                                field::bare(&self.request_name)
                                    .flex_1()
                                    .min_w(px(0.))
                                    .into_any_element()
                            } else {
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .child(name)
                                    .into_any_element()
                            })
                            .child(
                                div()
                                    .id(SharedString::from(format!("workbench-close-tab-{id}")))
                                    .debug_selector(move || format!("workbench-close-tab-{id}"))
                                    .flex_none()
                                    .cursor_pointer()
                                    .opacity(if dirty { 0.9 } else { 0.5 })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.close_request_tab(id, window, cx);
                                    }))
                                    .child(icon("close", 11., colors.muted_foreground)),
                            ),
                    );
            strip = strip.child(tab_element);
        }
        div()
            .flex()
            .flex_none()
            .min_w_0()
            .h(px(32.))
            .border_b_1()
            .border_color(colors.border)
            .bg(colors.sidebar)
            .child(strip)
            .child(
                Button::new("workbench-new-request")
                    .debug_selector(|| "workbench-new-request".into())
                    .ghost()
                    .flex_none()
                    .size(px(32.))
                    .tooltip("Add request")
                    .on_click(cx.listener(|this, _, window, cx| this.new_request(window, cx)))
                    .icon(IconName::Plus),
            )
            // ContextMenuExt uses a fixed element ID, so this belongs on the
            // strip rather than on each tab chip.
            .context_menu({
                let panel = cx.entity();
                move |popup, _window, cx| {
                    let Some((id, subject)) = panel.read(cx).request_tab_menu_subject() else {
                        return popup;
                    };
                    tab_menu::rows(subject)
                        .into_iter()
                        .fold(popup, |popup, row| match row {
                            tab_menu::Row::Separator => popup.separator(),
                            tab_menu::Row::Item(item) => {
                                let handle = panel.downgrade();
                                let action = item.action;
                                popup.item(
                                    menu_item(
                                        format!("workbench-request-tab-menu-{action:?}"),
                                        item.label,
                                        &handle,
                                        move |this, window, cx| {
                                            this.run_request_tab_action(action, id, window, cx);
                                        },
                                    )
                                    .disabled(!item.enabled),
                                )
                            }
                        })
                }
            })
    }

    /// `gap 8 · padding 12px 16px`: method pill menu, URL frame, Send/Stop,
    /// save, kebab menu.
    fn render_url_row(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let colors = cx.theme().colors;
        let handle = cx.entity().downgrade();
        let save_state = SaveState::for_request(self.current_definition.is_some(), self.dirty);
        let method = self.current_method(cx);
        let tint = self.method_tint_label(&method, cx);
        let url_focused = self.url.focus_handle(cx).is_focused(window);
        // A relative URL shows what it will be prefixed with — or that
        // nothing will, which is the click-through to the Envs tab.
        let base_prefix = switchyard_api::is_relative_url(&self.url.read(cx).value())
            .then(|| self.environment_base_url_value(cx));
        let environment_name = self.environment_display_name();
        let sending = matches!(
            self.send_state,
            SendState::PreparingRun | SendState::Sending | SendState::Cancelling
        );
        let method_menu = menu_trigger(
            "workbench-method-pill",
            tint.opacity(0.14),
            tint.opacity(0.45),
            tint,
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .font_family(crate::api::compat::fonts::mono(cx))
                .text_size(text::S11)
                .font_weight(text::weight::MEDIUM)
                .child(method.clone())
                .child(icon("chevron-down", 12., tint)),
            cx,
        )
        .h(px(32.))
        .px(px(10.))
        .dropdown_menu({
            let handle = handle.clone();
            move |menu: PopupMenu, _window, _cx| {
                let mut menu = menu.min_w(px(160.));
                for method in transport::Method::ALL {
                    menu = menu.item(menu_item(
                        format!("workbench-method-{}", method.label()),
                        if method == transport::Method::Custom {
                            "Custom…"
                        } else {
                            method.label()
                        },
                        &handle,
                        move |this, window, cx| this.set_method(method, window, cx),
                    ));
                }
                menu
            }
        });
        let kebab_menu = menu_trigger(
            "workbench-kebab",
            cx.theme().transparent,
            colors.input,
            palette::text_secondary(cx),
            icon("kebab", 14., palette::text_secondary(cx)),
            cx,
        )
        .size(px(32.))
        .p_0()
        .dropdown_menu({
            let handle = handle.clone();
            let export_ready = self.prepared_export.is_some();
            let dirty = self.dirty;
            let private = self.allow_private_network;
            move |menu: PopupMenu, _window, _cx| {
                menu.min_w(px(220.))
                    .item(menu_item(
                        "workbench-ask-ai-review".into(),
                        "Ask AI to review this request",
                        &handle,
                        |this, _, cx| this.ask_agent(assist::AssistIntent::ReviewRequest, cx),
                    ))
                    .separator()
                    .item(menu_item(
                        "workbench-export-agentops".into(),
                        "Export collection · AgentOps JSON",
                        &handle,
                        |this, _, cx| this.export_collection(false, cx),
                    ))
                    .item(menu_item(
                        "workbench-export-postman".into(),
                        "Export collection · Postman v2.1",
                        &handle,
                        |this, _, cx| this.export_collection(true, cx),
                    ))
                    .item(
                        menu_item(
                            "workbench-export-save-file".into(),
                            "Save export to file…",
                            &handle,
                            |this, window, cx| this.save_export_file(window, cx),
                        )
                        .disabled(!export_ready),
                    )
                    .separator()
                    .item(
                        menu_item(
                            "workbench-private-network".into(),
                            if private {
                                "Private network: allowed"
                            } else {
                                "Private network: blocked"
                            },
                            &handle,
                            |this, _, cx| {
                                this.allow_private_network = !this.allow_private_network;
                                this.dirty = true;
                                this.error = None;
                                cx.notify();
                            },
                        )
                        .checked(private),
                    )
                    .separator()
                    .item(
                        menu_item(
                            "workbench-discard-menu".into(),
                            "Discard changes",
                            &handle,
                            |this, window, cx| this.discard_changes(window, cx),
                        )
                        .disabled(!dirty),
                    )
            }
        });
        div()
            .flex()
            .flex_col()
            .flex_none()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::SP_2)
                    .px(space::SP_4)
                    .py(space::SP_3)
                    .child(method_menu)
                    .when(self.method == transport::Method::Custom, |el| {
                        el.child(
                            mono_field(&self.custom_method, window, cx)
                                .w(px(96.))
                                .h(px(32.)),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w(px(0.))
                            .items_center()
                            .h(px(32.))
                            .px(px(10.))
                            .rounded(radius::sm())
                            .border_1()
                            .border_color(if url_focused {
                                colors.ring
                            } else {
                                colors.input
                            })
                            .bg(colors.sidebar)
                            .font_family(crate::api::compat::fonts::mono(cx))
                            .text_size(text::S12)
                            // The prefix is also the composer's environment
                            // picker. It names the selected environment rather
                            // than its base URL, which can stay unchanged when
                            // an environment is renamed.
                            .when_some(base_prefix, |el, base| {
                                let missing = base.is_empty();
                                let tint = if missing {
                                    palette::warm(cx)
                                } else {
                                    colors.muted_foreground
                                };
                                el.child(
                                    menu_trigger(
                                        "workbench-url-base",
                                        if missing {
                                            palette::warm(cx).opacity(0.14)
                                        } else {
                                            colors.accent
                                        },
                                        cx.theme().transparent,
                                        tint,
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(4.))
                                            .min_w(px(0.))
                                            .child(
                                                div()
                                                    .min_w(px(0.))
                                                    .text_size(text::S10)
                                                    .truncate()
                                                    .child(if missing {
                                                        "no base URL — pick an environment"
                                                            .to_string()
                                                    } else {
                                                        environment_name
                                                    }),
                                            )
                                            .child(icon("chevron-down", 10., tint)),
                                        cx,
                                    )
                                    .flex_none()
                                    .max_w(px(260.))
                                    .mr(px(6.))
                                    .h(px(18.))
                                    .px(px(6.))
                                    .overflow_hidden()
                                    .dropdown_menu(self.environment_menu("workbench-url-env", cx)),
                                )
                            })
                            .child(field::bare(&self.url).flex_1().min_w(px(0.))),
                    )
                    .child(
                        accent_button(
                            "workbench-send".into(),
                            match self.send_state {
                                SendState::Idle => "Send",
                                SendState::PreparingRun | SendState::Sending => "Stop",
                                SendState::Cancelling => "Cancelling…",
                            },
                            32.,
                            cx,
                        )
                        .when(sending, |el| {
                            el.bg(colors.danger.opacity(0.14))
                                .border_color(colors.danger.opacity(0.45))
                                .text_color(colors.danger)
                        })
                        .child(icon(
                            if sending { "stop" } else { "play" },
                            13.,
                            if sending {
                                colors.danger
                            } else {
                                colors.primary_foreground
                            },
                        ))
                        .on_click(cx.listener(|this, _, window, cx| {
                            if matches!(
                                this.send_state,
                                SendState::PreparingRun | SendState::Sending
                            ) {
                                this.cancel(cx);
                            } else if this.send_state == SendState::Idle {
                                this.send(window, cx);
                            }
                        })),
                    )
                    .child(
                        icon_button(
                            "workbench-save".into(),
                            save_state.icon(),
                            Some(save_state.tint(cx)),
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.save_clicked(cx))),
                    )
                    .child(kebab_menu),
            )
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .px(space::SP_4)
                        .pb(space::SP_2)
                        .text_size(text::S11)
                        .text_color(colors.danger)
                        .child(error),
                )
            })
    }

    /// `gap 2 · px 16 · border-b`: the seven composer sub-tabs, Headers and
    /// Params carrying their row counts.
    fn render_composer_tabs(&self, cx: &mut Context<Self>) -> Div {
        let colors = cx.theme().colors;
        let header_count = entries::KvGrid::enabled_count(&self.headers.read(cx).value(), ':');
        let param_count = entries::KvGrid::enabled_count(&self.params.read(cx).value(), '=');
        div()
            .flex()
            .flex_wrap()
            .flex_none()
            .items_center()
            .gap(px(2.))
            .px(space::SP_4)
            .border_b_1()
            .border_color(colors.border)
            .child(self.render_layout_menu(cx))
            .children(ComposerTab::ALL.into_iter().map(|tab| {
                let label = match tab {
                    ComposerTab::Headers if header_count > 0 => {
                        format!("Headers ({header_count})")
                    }
                    ComposerTab::Params if param_count > 0 => {
                        format!("Params ({param_count})")
                    }
                    other => other.label().to_string(),
                };
                underline_tab(
                    format!("workbench-composer-{}", tab.label()),
                    label,
                    tab == self.composer_tab,
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.composer_tab = tab;
                    if tab == ComposerTab::Snippet {
                        this.generate_snippet(cx);
                    }
                    cx.notify();
                }))
            }))
    }
}

// ---------------------------------------------------------------------------
// Compose: sub-tab bodies
// ---------------------------------------------------------------------------

impl WorkbenchPanel {
    fn render_composer_content(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let body_fills_pane = self.composer_tab == ComposerTab::Body
            && !self.method.forbids_body()
            && !matches!(
                self.body_mode,
                draft::BodyMode::Form | draft::BodyMode::Multipart | draft::BodyMode::GraphQl
            );
        let content = match self.composer_tab {
            ComposerTab::Params => self.params_grid.clone().into_any_element(),
            ComposerTab::Headers => self.headers_grid.clone().into_any_element(),
            ComposerTab::Vars => self.render_variables_tab(cx).into_any_element(),
            ComposerTab::Body => self.render_body_tab(window, cx).into_any_element(),
            ComposerTab::Auth => self.render_auth_tab(window, cx).into_any_element(),
            ComposerTab::Tests => self.render_tests_tab(window, cx).into_any_element(),
            ComposerTab::Snippet => self.render_snippet_tab(cx).into_any_element(),
            ComposerTab::Settings => self.render_request_settings(cx).into_any_element(),
        };
        div()
            .id("workbench-request-editor")
            .debug_selector(|| "workbench-request-editor".into())
            .flex_1()
            .min_h(px(0.))
            .overflow_hidden()
            .child(if body_fills_pane {
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .px(space::SP_4)
                    .py(space::SP_3)
                    .child(content)
                    .into_any_element()
            } else {
                div()
                    .size_full()
                    .overflow_y_scrollbar()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .px(space::SP_4)
                            .py(space::SP_3)
                            .child(content),
                    )
                    .into_any_element()
            })
            .into_any_element()
    }

    fn render_variables_tab(&self, cx: &mut Context<Self>) -> Div {
        let colors = cx.theme().colors;
        let handle = cx.entity().downgrade();
        let presets = [
            (
                "DateFrom",
                "Today at 00:00 (UTC)",
                "{{$datetime:%Y-%m-%dT00:00}}",
            ),
            (
                "DateTo",
                "Today at 23:59 (UTC)",
                "{{$datetime:%Y-%m-%dT23:59}}",
            ),
            (
                "currentDateTime",
                "Formatted date / time (UTC)",
                "{{$datetime:%Y-%m-%dT%H:%M}}",
            ),
            (
                "localDateTime",
                "Formatted date / time (local)",
                "{{$localDatetime:%Y-%m-%dT%H:%M}}",
            ),
            ("isoTimestamp", "ISO timestamp (UTC)", "{{$isoTimestamp}}"),
            ("timestamp", "Unix timestamp · seconds", "{{$timestamp}}"),
            (
                "timestampMs",
                "Unix timestamp · milliseconds",
                "{{$timestampMs}}",
            ),
            ("currentDate", "Current date (UTC)", "{{$date}}"),
            ("currentTime", "Current time (UTC)", "{{$time}}"),
            ("requestId", "UUID · request / correlation ID", "{{$uuid}}"),
            ("randomInt", "Random integer · 0–1000", "{{$randomInt}}"),
        ];
        let add = menu_trigger(
            "workbench-runtime-variable",
            colors.background,
            colors.border,
            colors.foreground,
            "Add runtime variable",
            cx,
        )
        .dropdown_menu(move |menu: PopupMenu, _, _| {
            let mut menu = menu.min_w(px(280.));
            for (key, label, expression) in presets {
                menu = menu.item(menu_item(
                    format!("workbench-runtime-{key}"),
                    label,
                    &handle,
                    move |this, window, cx| {
                        let source = this.variables.read(cx).value().to_string();
                        let rows = entries::KvGrid::parse(&source, '=');
                        let mut name = key.to_string();
                        let mut suffix = 2;
                        while rows.iter().any(|(_, existing, _)| {
                            existing.strip_prefix("secret:").unwrap_or(existing) == name
                        }) {
                            name = format!("{key}{suffix}");
                            suffix += 1;
                        }
                        let value = if source.is_empty() {
                            format!("{name}={expression}")
                        } else {
                            format!("{source}\n{name}={expression}")
                        };
                        this.variables
                            .update(cx, |input, cx| input.set_value(value, window, cx));
                        this.dirty = true;
                        cx.notify();
                    },
                ));
            }
            menu
        });
        div()
            .flex()
            .flex_col()
            .gap(space::SP_2)
            .child(div().flex().child(add))
            .child(
                div()
                    .text_size(text::S11)
                    .text_color(colors.muted_foreground)
                    .child("Use {{name}} in the body, URL or headers. Runtime values refresh on send. Edit date formats with %Y (year), %m (month), %d (day), %H:%M:%S (time)."),
            )
            .child(self.variables_grid.clone())
            .child(self.render_variable_inspector(cx))
    }

    fn render_body_tab(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let colors = cx.theme().colors;
        self.ux
            .body_editor
            .update(cx, |editor, cx| editor.set_mode(self.body_mode, cx));
        if self.method.forbids_body() {
            return div()
                .flex()
                .flex_col()
                .gap(space::SP_1)
                .child(
                    div()
                        .text_size(text::S12)
                        .font_weight(text::weight::SEMIBOLD)
                        .text_color(colors.foreground)
                        .child(format!("{} does not send a body", self.current_method(cx))),
                )
                .child(
                    div()
                        .text_size(text::S11)
                        .text_color(colors.muted_foreground)
                        .child("Your body draft is retained. Switch back to a method with a payload to continue editing it."),
                );
        }
        let source = self.body.read(cx).value().to_string();
        let hint = match self.body_mode {
            draft::BodyMode::Json if source.trim().is_empty() => "Empty JSON body".to_string(),
            draft::BodyMode::Json if source.contains("{{") => {
                "Template body · JSON is validated after variable substitution".to_string()
            }
            draft::BodyMode::Json => match serde_json::from_str::<serde_json::Value>(&source) {
                Ok(_) => format!("Valid JSON · {}", pretty::human_size(source.len() as u64)),
                Err(error) => format!("Invalid JSON — {error}"),
            },
            mode => format!(
                "{} body · {}",
                mode.label(),
                pretty::human_size(source.len() as u64)
            ),
        };
        let focused = self.body.focus_handle(cx).is_focused(window);
        div()
            .flex()
            .flex_col()
            .when(
                !matches!(
                    self.body_mode,
                    draft::BodyMode::Form | draft::BodyMode::Multipart | draft::BodyMode::GraphQl
                ),
                |el| el.flex_1().min_h_0(),
            )
            .gap(px(10.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::SP_2)
                    .flex_wrap()
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(2.))
                            .children(draft::BodyMode::ALL.into_iter().map(|mode| {
                                chip(
                                    format!("workbench-body-{}", mode.label()),
                                    mode.label(),
                                    self.body_mode == mode,
                                    cx,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        if this.body_mode != mode {
                                            this.body_mode = mode;
                                            this.dirty = true;
                                        }
                                        cx.notify();
                                    },
                                ))
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_size(text::S11)
                            .text_color(colors.muted_foreground)
                            .child(hint),
                    )
                    .when(
                        self.body_mode == draft::BodyMode::Json
                            && pretty::format_json(&source).is_some_and(|f| f != source),
                        |el| {
                            el.child(
                                outline_chip("workbench-body-format".into(), "Format", cx)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.format_body(window, cx)
                                    })),
                            )
                        },
                    )
                    .child(ai_button(assist::AssistIntent::GenerateBody, false, cx)),
            )
            .child(
                if matches!(
                    self.body_mode,
                    draft::BodyMode::Form | draft::BodyMode::Multipart | draft::BodyMode::GraphQl
                ) {
                    self.ux.body_editor.clone().into_any_element()
                } else {
                    code_box(cx)
                        .id("workbench-body-editor-frame")
                        .debug_selector(|| "workbench-body-editor-frame".into())
                        .flex_1()
                        .min_h_0()
                        .overflow_hidden()
                        .when(focused, |el| el.border_color(colors.ring))
                        .child(field::bare(&self.body).w_full().h_full())
                        .into_any_element()
                },
            )
            .when(
                matches!(
                    self.body_mode,
                    draft::BodyMode::Binary | draft::BodyMode::Multipart
                ),
                |el| {
                    el.child(
                        div().flex().child(
                            outline_chip(
                                "workbench-authorize-files".into(),
                                "Choose files for one send",
                                cx,
                            )
                            .child(icon("upload", 12., palette::text_secondary(cx)))
                            .on_click(cx.listener(
                                |this, _, window, cx| this.choose_upload_files(window, cx),
                            )),
                        ),
                    )
                },
            )
    }

    /// Type list on the left, the typed form for that type on the right,
    /// then cookies and the vault note.
    fn render_auth_tab(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let colors = cx.theme().colors;
        let stored = self
            .current_definition
            .as_ref()
            .is_some_and(|request| auth_mode(&request.auth) == self.auth_mode);
        self.auth_form.update(cx, |form, cx| {
            form.set_mode(self.auth_mode, stored, cx);
        });
        let explanation = match self.auth_mode {
            draft::AuthMode::Inherit => Some(format!("→ {}", self.inherited_auth_source())),
            draft::AuthMode::None => Some("No credentials are sent with this request.".into()),
            _ => None,
        };
        let cookies_focused = self.cookies.focus_handle(cx).is_focused(window);
        div()
            .flex()
            .gap(space::SP_6)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .w(px(180.))
                    .gap(px(6.))
                    .child(heading("Type", colors.muted_foreground).px(space::SP_2))
                    .children(draft::AuthMode::ALL.into_iter().map(|mode| {
                        let active = self.auth_mode == mode;
                        div()
                            .id(SharedString::from(format!("workbench-auth-{}", mode.label())))
                            .debug_selector(move || format!("workbench-auth-{}", mode.label()))
                            .px(space::SP_2)
                            .py(px(6.))
                            .rounded(radius::sm())
                            .cursor_pointer()
                            .text_size(text::S12)
                            .when(active, |el| el.bg(colors.accent))
                            .text_color(if active {
                                colors.foreground
                            } else {
                                palette::text_secondary(cx)
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.auth_mode != mode {
                                    this.auth_mode = mode;
                                    this.dirty = true;
                                }
                                cx.notify();
                            }))
                            .child(mode.label())
                    })),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(10.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::SP_2)
                            .child(heading(self.auth_mode.label(), colors.muted_foreground))
                            .when_some(explanation, |el, explanation| {
                                el.child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .truncate()
                                        .font_family(crate::api::compat::fonts::mono(cx))
                                        .text_size(text::S11)
                                        .text_color(palette::lavender(cx))
                                        .child(explanation),
                                )
                            })
                            .child(div().flex_1())
                            .when(self.auth_mode == draft::AuthMode::OAuth2, |el| {
                                el.child(
                                    outline_chip(
                                        "workbench-oauth-authorize".into(),
                                        "Acquire token",
                                        cx,
                                    )
                                    .border_color(colors.primary.opacity(0.45))
                                    .text_color(colors.primary)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.acquire_oauth_token(cx)
                                    })),
                                )
                            }),
                    )
                    .child(self.auth_form.clone())
                    .child(note_card(
                        colors.info,
                        Some("shield"),
                        "Secrets resolve at send time from the vault and are never written to history.",
                        cx,
                    ))
                    .child(heading("Cookies", colors.muted_foreground))
                    .child(
                        code_box(cx)
                            .when(cookies_focused, |el| el.border_color(colors.ring))
                            .child(field::bare(&self.cookies).w_full()),
                    ),
            )
    }

    fn render_tests_tab(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let colors = cx.theme().colors;
        let pre_focused = self.pre_request_script.focus_handle(cx).is_focused(window);
        let post_focused = self.assertions.focus_handle(cx).is_focused(window);
        let results = self
            .response
            .as_ref()
            .map(|response| response.test_results.clone())
            .unwrap_or_default();
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(heading(
                "Pre-request script · pm.* sandbox",
                colors.muted_foreground,
            ))
            .child(
                div()
                    .text_size(text::S11)
                    .text_color(colors.muted_foreground)
                    .child("Runs before send. Use pm.encoding.base64Encode(text), then pm.variables.set('name', value) to fill {{name}} in the body or headers."),
            )
            .child(
                code_box(cx)
                    .when(pre_focused, |el| el.border_color(colors.ring))
                    .child(field::bare(&self.pre_request_script).w_full()),
            )
            .child(heading(
                "Post-response script · pm.test(name, fn) and pm.expect(…)",
                colors.muted_foreground,
            ))
            .child(self.render_script_snippets(cx))
            .child(
                div()
                    .text_size(text::S11)
                    .text_color(colors.muted_foreground)
                    .child("Save a response token for other requests: pm.environment.set('access_token', pm.response.json().access_token). Select an environment, then use {{access_token}}."),
            )
            .child(
                code_box(cx)
                    .when(post_focused, |el| el.border_color(colors.ring))
                    .child(field::bare(&self.assertions).w_full()),
            )
            .children(
                results
                    .iter()
                    .map(|result| self.test_result_row(result, cx)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .py(px(6.))
                    .px(px(2.))
                    .text_size(text::S11)
                    .text_color(colors.primary)
                    .child(icon("plus", 12., colors.primary))
                    .child(if results.is_empty() {
                        "Send the request to run these tests"
                    } else {
                        "Add an assertion above and send again"
                    }),
            )
    }

    /// `✓ name` or `✕ name — got …` on a `muted` row (danger wash on failure).
    fn test_result_row(&self, result: &switchyard_api::TestResult, cx: &App) -> Div {
        let colors = cx.theme().colors;
        div()
            .flex()
            .items_center()
            .gap(space::SP_2)
            .px(px(10.))
            .py(space::SP_2)
            .rounded(radius::sm())
            .border_1()
            .border_color(if result.passed || result.skipped {
                colors.border
            } else {
                colors.danger.opacity(0.35)
            })
            .bg(if result.passed || result.skipped {
                colors.muted
            } else {
                colors.danger.opacity(0.06)
            })
            .font_family(crate::api::compat::fonts::mono(cx))
            .text_size(text::S11)
            .text_color(palette::text_secondary(cx))
            .child(
                div()
                    .flex_none()
                    .text_color(if result.skipped {
                        colors.muted_foreground
                    } else if result.passed {
                        colors.success
                    } else {
                        colors.danger
                    })
                    .child(if result.skipped {
                        "SKIP"
                    } else if result.passed {
                        "✓"
                    } else {
                        "✕"
                    }),
            )
            .child(div().child(result.name.clone()))
            .when_some(result.error.clone(), |el, error| {
                el.child(
                    div()
                        .text_color(colors.muted_foreground)
                        .truncate()
                        .child(format!("— {error}")),
                )
            })
    }

    fn render_snippet_tab(&self, cx: &mut Context<Self>) -> Div {
        let snippet_copy = self.snippet_output.clone();
        let snippet = self.snippet_output.clone().unwrap_or_else(|| {
            "Select Snippet or choose a language to generate a redacted snippet.".into()
        });
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div().flex().items_center().gap(px(2.)).children(
                    SNIPPET_LANGUAGES
                        .into_iter()
                        .enumerate()
                        .map(|(index, (language, label))| {
                            chip(
                                format!("workbench-snippet-language-{index}"),
                                label,
                                self.snippet_language == language,
                                cx,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.snippet_language = language;
                                    this.generate_snippet(cx);
                                    cx.notify();
                                },
                            ))
                        }),
                ),
            )
            .child(div().id("workbench-snippet-context").child(
                code_box(cx).child(snippet).context_menu(move |menu, _, _| {
                    let snippet = snippet_copy.clone();
                    menu.item(
                        PopupMenuItem::new("Copy snippet")
                            .disabled(snippet.is_none())
                            .on_click(move |_, _, cx| {
                                if let Some(snippet) = &snippet {
                                    cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                                        snippet.clone(),
                                    ));
                                }
                            }),
                    )
                }),
            ))
            .when_some(self.export_output.clone(), |el, export| {
                el.child(heading("Export", cx.theme().colors.muted_foreground))
                    .child(
                        div().id("workbench-export-context").child(
                            code_box(cx)
                                .id("workbench-export-output")
                                .debug_selector(|| "workbench-export-output".into())
                                .child(export.clone())
                                .context_menu(move |menu, _, _| {
                                    let export = export.clone();
                                    menu.item(PopupMenuItem::new("Copy export").on_click(
                                        move |_, _, cx| {
                                            cx.write_to_clipboard(
                                                gpui_kit::ClipboardItem::new_string(export.clone()),
                                            );
                                        },
                                    ))
                                }),
                        ),
                    )
            })
    }
}

// ---------------------------------------------------------------------------
// Compose: response
// ---------------------------------------------------------------------------

/// One painted row of the Trace pane.
struct TraceRow {
    offset_ms: u64,
    tag: &'static str,
    text: String,
    duration: Option<u64>,
}

impl WorkbenchPanel {
    /// `flex-1 · border-t · bg sidebar`: tab chips, readouts, the lavender
    /// "Explain · write tests" menu, then the selected pane.
    fn render_response(&self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let response = self.response.as_deref();
        let handle = cx.entity().downgrade();
        let lavender = palette::lavender(cx);
        let sending = self.send_state != SendState::Idle;
        let failed =
            response.is_some_and(|response| response.status >= 400) || self.error.is_some();
        let ask_menu = menu_trigger(
            "workbench-ask-ai",
            lavender.opacity(0.12),
            lavender.opacity(0.5),
            lavender,
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(text::S11)
                .child(icon("spark", 11., lavender))
                .child("Explain · write tests")
                .child(icon("chevron-down", 11., lavender)),
            cx,
        )
        .h(px(24.))
        .px(px(10.))
        .dropdown_menu({
            let has_response = response.is_some();
            move |menu: PopupMenu, _window, _cx| {
                let mut menu = menu
                    .min_w(px(200.))
                    .item(
                        menu_item(
                            "workbench-ask-ai-explain".into(),
                            "Explain this response",
                            &handle,
                            |this, _, cx| this.ask_agent(assist::AssistIntent::ExplainResponse, cx),
                        )
                        .disabled(!has_response),
                    )
                    .item(
                        menu_item(
                            "workbench-ask-ai-tests".into(),
                            "Write tests for it",
                            &handle,
                            |this, _, cx| this.ask_agent(assist::AssistIntent::WriteTests, cx),
                        )
                        .disabled(!has_response),
                    );
                if failed {
                    menu = menu.item(menu_item(
                        "workbench-ask-ai-debug".into(),
                        "Debug the failure",
                        &handle,
                        |this, _, cx| this.ask_agent(assist::AssistIntent::DiagnoseFailure, cx),
                    ));
                }
                menu.separator().item(
                    menu_item(
                        "workbench-save-example".into(),
                        "Save as example",
                        &handle,
                        |this, _, cx| this.save_example(cx),
                    )
                    .disabled(!has_response),
                )
            }
        });
        let virtualized_body = matches!(self.response_tab, ResponseTab::Pretty | ResponseTab::Raw);
        let pane = match (self.response_tab, response) {
            (ResponseTab::Console, _) => self.render_console(cx),
            (_, None) if sending => div()
                .text_size(text::S11)
                .text_color(colors.primary)
                .child("Sending request off the frame thread…")
                .into_any_element(),
            (_, None) => div()
                .text_size(text::S11)
                .text_color(palette::text_tertiary(cx))
                .child("Send a request to see the response.")
                .into_any_element(),
            (ResponseTab::Pretty, Some(_)) => {
                if let Some(preview) = self.ux.response_view.preview.as_ref() {
                    gpui_kit::component::text::TextView::html(
                        "workbench-response-preview",
                        preview.clone(),
                    )
                    .selectable(true)
                    .scrollable(true)
                    .size_full()
                    .into_any_element()
                } else {
                    self.render_response_editor(true, cx)
                }
            }
            (ResponseTab::Raw, Some(_)) => self.render_response_editor(false, cx),
            (ResponseTab::Headers, Some(response)) => {
                let cols = [Col::Px(220.), Col::Flex];
                div()
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .rounded(radius::md())
                    .overflow_hidden()
                    .bg(colors.muted)
                    .font_family(crate::api::compat::fonts::mono(cx))
                    .text_size(text::S11)
                    .children(response.headers.iter().map(|(name, value)| {
                        table_row(
                            &cols,
                            vec![
                                tinted_cell(name.clone(), colors.muted_foreground),
                                cell(value.clone()),
                            ],
                            false,
                            cx,
                        )
                        .bg(colors.sidebar)
                    }))
                    .when(response.headers.is_empty(), |el| {
                        el.child(
                            div()
                                .p(space::SP_2)
                                .text_color(palette::text_tertiary(cx))
                                .child("(no headers)"),
                        )
                    })
                    .into_any_element()
            }
            (ResponseTab::Trace, Some(response)) => self.render_trace(response, cx),
            (ResponseTab::Tests, Some(response)) => {
                if response.test_results.is_empty() {
                    div()
                        .text_size(text::S11)
                        .text_color(palette::text_tertiary(cx))
                        .child("No test results. Add assertions in Compose → Scripts, then send the request." )
                        .into_any_element()
                } else {
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .children(
                            response
                                .test_results
                                .iter()
                                .map(|result| self.response_test_row(result, cx)),
                        )
                        .into_any_element()
                }
            }
        };
        let readouts = response.map(|response| {
            let tests = &response.test_results;
            let passed = tests
                .iter()
                .filter(|result| result.passed && !result.skipped)
                .count();
            let skipped = tests.iter().filter(|result| result.skipped).count();
            (
                if response.status == 0 {
                    "No HTTP response".into()
                } else {
                    format!("{} {}", response.status, response.reason)
                },
                status_tint(response.status, cx),
                format!(
                    "{} ms",
                    self.elapsed_ms
                        .map(|ms| ms.to_string())
                        .unwrap_or_else(|| response.duration_ms.to_string())
                ),
                pretty::human_size(response.received_bytes.max(response.body.len() as u64)),
                (!tests.is_empty()).then(|| {
                    (
                        format!(
                            "{passed} passed · {skipped} skipped · {} failed",
                            tests.len() - passed - skipped
                        ),
                        if passed + skipped == tests.len() {
                            colors.success
                        } else {
                            colors.danger
                        },
                    )
                }),
            )
        });
        let copy_payload = self
            .response_body
            .as_ref()
            .and_then(|body| match self.response_tab {
                ResponseTab::Pretty => Some(
                    match &self
                        .ux
                        .response_view
                        .body
                        .as_ref()
                        .unwrap_or(body)
                        .presentation
                    {
                        pretty::BodyPresentation::Inline { text, .. }
                        | pretty::BodyPresentation::Virtualized { text, .. } => text.clone(),
                    },
                ),
                ResponseTab::Raw => Some(body.raw.clone()),
                _ => None,
            });
        div()
            .id("workbench-response-pane")
            .debug_selector(|| "workbench-response-pane".into())
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .overflow_hidden()
            .border_t_1()
            .border_color(colors.border)
            .bg(colors.sidebar)
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(space::SP_3)
                    .px(space::SP_4)
                    .py(space::SP_2)
                    .border_b_1()
                    .border_color(colors.sidebar_border)
                    .flex_wrap()
                    .child(div().flex().flex_wrap().gap(px(2.)).children(
                        ResponseTab::ALL.into_iter().map(|tab| {
                            chip(
                                format!("workbench-response-{}", tab.label()),
                                tab.label(),
                                tab == self.response_tab,
                                cx,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.response_tab = tab;
                                    cx.notify();
                                },
                            ))
                        }),
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w(px(0.))
                            .items_center()
                            .gap(space::SP_3)
                            .font_family(crate::api::compat::fonts::mono(cx))
                            .text_size(text::S11)
                            .text_color(colors.muted_foreground)
                            .flex_wrap()
                            .when(sending, |el| {
                                el.child(div().text_color(colors.primary).child("streaming…"))
                            })
                            .when_some(readouts, |el, (status, tint, elapsed, size, tests)| {
                                el.child(div().text_color(tint).child(status))
                                    .child(div().child(elapsed))
                                    .child(div().child(size))
                                    .when_some(tests, |el, (label, tint)| {
                                        el.child(div().text_color(tint).child(label))
                                    })
                            }),
                    )
                    .when(response.is_some(), |el| {
                        el.child(
                            outline_chip("workbench-inspect-response".into(), "Inspect body", cx)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_response_inspector(window, cx);
                                })),
                        )
                    })
                    .when_some(copy_payload, |el, payload| {
                        el.child(
                            outline_chip("workbench-copy-response".into(), "Copy view", cx)
                                .child(icon("copy", 11., palette::text_secondary(cx)))
                                .on_click(move |_, _, cx| {
                                    if payload.is_empty() {
                                        crate::api::compat::notify::warning(
                                            cx,
                                            "Response output is empty — nothing to copy.",
                                        );
                                    } else {
                                        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                                            payload.to_string(),
                                        ));
                                        crate::api::compat::notify::success(
                                            cx,
                                            "Response output copied.",
                                        );
                                    }
                                }),
                        )
                    })
                    .child(ask_menu)
                    .context_menu({
                        let handle = cx.entity().downgrade();
                        let has_response = response.is_some();
                        move |menu, _, _| {
                            menu.item(
                                menu_item(
                                    "workbench-response-menu-inspect".into(),
                                    "Inspect response body",
                                    &handle,
                                    |this, window, cx| this.open_response_inspector(window, cx),
                                )
                                .disabled(!has_response),
                            )
                            .item(
                                menu_item(
                                    "workbench-response-menu-example".into(),
                                    "Save response as example",
                                    &handle,
                                    |this, _, cx| this.save_example(cx),
                                )
                                .disabled(!has_response),
                            )
                            .separator()
                            .item(
                                menu_item(
                                    "workbench-response-menu-explain".into(),
                                    "Explain this response",
                                    &handle,
                                    |this, _, cx| {
                                        this.ask_agent(assist::AssistIntent::ExplainResponse, cx)
                                    },
                                )
                                .disabled(!has_response),
                            )
                            .item(
                                menu_item(
                                    "workbench-response-menu-tests".into(),
                                    "Write tests for this response",
                                    &handle,
                                    |this, _, cx| {
                                        this.ask_agent(assist::AssistIntent::WriteTests, cx)
                                    },
                                )
                                .disabled(!has_response),
                            )
                            .item(
                                menu_item(
                                    "workbench-response-menu-debug".into(),
                                    "Debug the failure",
                                    &handle,
                                    |this, _, cx| {
                                        this.ask_agent(assist::AssistIntent::DiagnoseFailure, cx)
                                    },
                                )
                                .disabled(!failed),
                            )
                        }
                    }),
            )
            .when(virtualized_body, |el| {
                el.child(self.render_response_controls(cx))
            })
            .when(self.response_tab == ResponseTab::Tests, |el| {
                el.child(self.render_rerun_tests(cx))
            })
            .child(if virtualized_body {
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .px(space::SP_4)
                    .py(space::SP_3)
                    .child(pane)
                    .into_any_element()
            } else {
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scrollbar()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .px(space::SP_4)
                            .py(space::SP_3)
                            .child(pane),
                    )
                    .into_any_element()
            })
            .into_any_element()
    }

    /// Trace rows built from the real exchange: connection timings, each
    /// redirect, `done`, and the error line — no synthetic events.
    fn render_trace(&self, response: &transport::Response, cx: &App) -> AnyElement {
        let colors = cx.theme().colors;
        let timings = &response.timings;
        let mut rows = Vec::new();
        let target = self
            .response_request
            .as_ref()
            .map(|request| format!("{} {}", request.method, request.url))
            .unwrap_or_else(|| response.final_url.clone());
        rows.push(TraceRow {
            offset_ms: 0,
            tag: "send",
            text: target,
            duration: None,
        });
        // What the request authenticated with — the redacted snapshot keeps a
        // bearer JWT's public claims, which is how two tokens are told apart.
        if let Some((_, value)) = self.response_request.as_ref().and_then(|request| {
            request
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("Authorization"))
        }) {
            rows.push(TraceRow {
                offset_ms: 0,
                tag: "auth",
                text: value.clone(),
                duration: None,
            });
        }
        let mut offset = 0;
        for (tag, value) in [
            ("dns", timings.dns_ms),
            ("conn", timings.connect_ms),
            ("tls", timings.tls_ms),
        ] {
            if let Some(ms) = value {
                rows.push(TraceRow {
                    offset_ms: offset,
                    tag,
                    text: format!("{tag} resolved"),
                    duration: Some(ms),
                });
                offset += ms;
            }
        }
        if let Some(ms) = timings.first_byte_ms {
            rows.push(TraceRow {
                offset_ms: ms,
                tag: "ttfb",
                text: format!("first byte · HTTP {}", response.http_version),
                duration: None,
            });
        }
        for redirect in &response.redirects {
            rows.push(TraceRow {
                offset_ms: timings.first_byte_ms.unwrap_or(0),
                tag: "redir",
                text: format!(
                    "{} {} → {}{}",
                    redirect.status,
                    redirect.from,
                    redirect.to,
                    if redirect.cross_origin {
                        " · cross-origin credentials/body stripped"
                    } else {
                        ""
                    }
                ),
                duration: None,
            });
        }
        if let Some(ms) = timings.download_ms {
            rows.push(TraceRow {
                offset_ms: response.duration_ms.saturating_sub(ms),
                tag: "body",
                text: format!("{} received", pretty::human_size(response.received_bytes)),
                duration: Some(ms),
            });
        }
        rows.push(TraceRow {
            offset_ms: response.duration_ms,
            tag: if response.truncated { "warn" } else { "done" },
            text: format!(
                "{} {} · {}{}",
                response.status,
                response.reason,
                response.final_url,
                if response.truncated {
                    " · response truncated at limit"
                } else {
                    ""
                }
            ),
            duration: Some(response.duration_ms),
        });
        if let Some(error) = &self.error {
            rows.push(TraceRow {
                offset_ms: response.duration_ms,
                tag: "error",
                text: error.clone(),
                duration: None,
            });
        }
        div()
            .flex()
            .flex_col()
            .font_family(crate::api::compat::fonts::mono(cx))
            .text_size(text::S11)
            .children(rows.into_iter().map(|row| {
                let tint = match row.tag {
                    "send" => colors.primary,
                    "dns" | "conn" | "tls" | "ttfb" | "body" | "auth" => colors.info,
                    "done" => colors.success,
                    "warn" | "redir" => palette::warm(cx),
                    _ => colors.danger,
                };
                div()
                    .flex()
                    .items_center()
                    .gap(space::SP_3)
                    .py(px(7.))
                    .border_b_1()
                    .border_color(colors.muted)
                    .child(
                        div()
                            .w(px(56.))
                            .flex_none()
                            .text_right()
                            .text_color(palette::text_tertiary(cx))
                            .child(format!("{} ms", row.offset_ms)),
                    )
                    .child(div().w(px(56.)).flex_none().text_color(tint).child(row.tag))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_color(palette::text_secondary(cx))
                            .child(row.text),
                    )
                    .when_some(row.duration, |el, ms| {
                        el.child(
                            div()
                                .flex_none()
                                .text_color(palette::text_tertiary(cx))
                                .child(format!("{ms} ms")),
                        )
                    })
            }))
            .into_any_element()
    }

    /// `PASS name … ms` / `FAIL name` + detail line, on a `muted` row.
    fn response_test_row(&self, result: &switchyard_api::TestResult, cx: &App) -> Div {
        let colors = cx.theme().colors;
        div()
            .flex()
            .flex_col()
            .gap(space::SP_1)
            .px(px(10.))
            .py(space::SP_2)
            .rounded(radius::sm())
            .when(!result.passed && !result.skipped, |el| {
                el.border_1()
                    .border_color(colors.danger.opacity(0.35))
                    .bg(colors.danger.opacity(0.06))
            })
            .when(result.passed || result.skipped, |el| el.bg(colors.muted))
            .font_family(crate::api::compat::fonts::mono(cx))
            .text_size(text::S11)
            .text_color(palette::text_secondary(cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .flex_none()
                            .text_color(if result.skipped {
                                colors.muted_foreground
                            } else if result.passed {
                                colors.success
                            } else {
                                colors.danger
                            })
                            .child(if result.skipped {
                                "SKIP"
                            } else if result.passed {
                                "PASS"
                            } else {
                                "FAIL"
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .child(result.name.clone()),
                    ),
            )
            .when_some(result.error.clone(), |el, error| {
                el.child(
                    div()
                        .pl(px(38.))
                        .text_color(colors.muted_foreground)
                        .child(error),
                )
            })
    }
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

pub(super) fn import_format_label(format: &switchyard_api::ImportFormat) -> &'static str {
    use switchyard_api::ImportFormat;
    match format {
        ImportFormat::OpenApi => "OpenAPI",
        ImportFormat::PostmanCollection => "Postman collection",
        ImportFormat::PostmanEnvironment => "Postman environment",
        ImportFormat::Insomnia => "Insomnia",
        ImportFormat::Har => "HAR",
        ImportFormat::Curl => "cURL",
        ImportFormat::AgentOps => "AgentOps bundle",
    }
}

impl WorkbenchPanel {
    /// `padding 16 · gap 16`: the drop zone + PARSED card, then (once
    /// staged) the generated-collection card and the review column.
    fn render_import(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let source_focused = self.import_source.focus_handle(cx).is_focused(window);
        let selected_requests = self
            .import_selection
            .as_ref()
            .map(|selection| selection.request_ids.len())
            .unwrap_or(0);
        let drop_zone = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .gap(space::SP_2)
            .p(space::SP_5)
            .rounded(radius::md())
            .border_1()
            .border_dashed()
            .border_color(colors.input)
            .bg(colors.sidebar)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::SP_2)
                    .text_size(text::S13)
                    .font_weight(text::weight::SEMIBOLD)
                    .text_color(colors.foreground)
                    .child(icon("file", 16., colors.muted_foreground))
                    .child("Paste or open an OpenAPI, Postman, Insomnia, HAR, AgentOps or cURL source"),
            )
            .child(
                div()
                    .text_size(text::S11)
                    .text_color(colors.muted_foreground)
                    .child("OpenAPI 3.0/3.1 JSON or YAML · Postman 2.1 collections and environments · Insomnia v4 · HAR browser captures · AgentOps bundles · a single cURL command"),
            )
            .child(
                code_box(cx)
                    .min_h(px(120.))
                    .text_size(text::S11)
                    .bg(colors.background)
                    .when(source_focused, |el| el.border_color(colors.ring))
                    .child(field::bare(&self.import_source).w_full()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::SP_2)
                    .mt(space::SP_1)
                    .child(
                        outline_chip("workbench-import-file".into(), "Open file…", cx)
                            .h(px(30.))
                            .px(space::SP_3)
                            .child(icon("upload", 12., palette::text_secondary(cx)))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.choose_import_file(window, cx)
                            })),
                    )
                    .child(
                        outline_chip("workbench-import-preview".into(), "Parse", cx)
                            .h(px(30.))
                            .px(space::SP_3)
                            .border_color(colors.primary.opacity(0.45))
                            .text_color(colors.primary)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.import_from_source(window, cx)
                            })),
                    )
                    .when_some(self.import_status.clone(), |el, status| {
                        el.child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .text_size(text::S11)
                                .text_color(colors.muted_foreground)
                                .child(status),
                        )
                    }),
            );
        let parsed = div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(280.))
            .gap(px(10.))
            .p(space::SP_4)
            .rounded(radius::md())
            .border_1()
            .border_color(colors.border)
            .bg(colors.muted)
            .child(heading("Parsed", colors.muted_foreground))
            .map(|el| match self.staged_import.as_ref() {
                None => el
                    .child(
                        div()
                            .text_size(text::S14)
                            .font_weight(text::weight::SEMIBOLD)
                            .text_color(colors.foreground)
                            .child("Nothing parsed yet"),
                    )
                    .child(
                        div()
                            .text_size(text::S11)
                            .text_color(colors.muted_foreground)
                            .child("Parse a source to review what it contains before anything is written to the workspace."),
                    ),
                Some(imported) => el
                    .child(
                        div()
                            .text_size(text::S14)
                            .font_weight(text::weight::SEMIBOLD)
                            .text_color(colors.foreground)
                            .truncate()
                            .child(imported.collection.name.clone()),
                    )
                    .child(
                        div()
                            .font_family(crate::api::compat::fonts::mono(cx))
                            .text_size(text::S11)
                            .text_color(colors.muted_foreground)
                            .child(format!(
                                "{} · {} requests",
                                import_format_label(&imported.format),
                                imported.requests.len()
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(px(6.))
                            .child(pill(
                                format!("{} folders", imported.folders.len()),
                                palette::text_secondary(cx),
                                cx,
                            ))
                            .child(pill(
                                format!("{} environments", imported.environments.len()),
                                palette::text_secondary(cx),
                                cx,
                            ))
                            .child(pill(
                                format!("{} examples", imported.examples.len()),
                                palette::text_secondary(cx),
                                cx,
                            ))
                            .when(!imported.warnings.is_empty(), |el| {
                                el.child(pill(
                                    format!("{} warnings", imported.warnings.len()),
                                    palette::warm(cx),
                                    cx,
                                ))
                            }),
                    )
                    .child(
                        ai_button(assist::AssistIntent::ReviewImport, true, cx)
                        .mt(space::SP_1),
                    ),
            });
        div()
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scrollbar()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::SP_4)
                    .p(space::SP_4)
                    .child(div().flex().gap(space::SP_3).child(drop_zone).child(parsed))
                    .when_some(self.staged_import.as_ref(), |el, imported| {
                        el.child(
                            div()
                                .flex()
                                .items_start()
                                .gap(space::SP_3)
                                .child(self.render_import_tree(imported, selected_requests, cx))
                                .child(self.render_import_review(imported, selected_requests, cx)),
                        )
                    }),
            )
            .into_any_element()
    }

    /// The generated-collection card: every staged folder, request,
    /// environment and example with its selection toggle.
    fn render_import_tree(
        &self,
        imported: &ImportResult,
        selected_requests: usize,
        cx: &mut Context<Self>,
    ) -> Div {
        let colors = cx.theme().colors;
        let selection = self.import_selection.as_ref();
        let tertiary = palette::text_tertiary(cx);
        let row = |id: String, selected: bool, indent: bool| {
            let selector = id.clone();
            div()
                .id(SharedString::from(id))
                .debug_selector(move || selector.clone())
                .flex()
                .items_center()
                .gap(space::SP_2)
                .py(px(5.))
                .pr(px(6.))
                .pl(if indent { px(18.) } else { px(6.) })
                .rounded(radius::sm())
                .cursor_pointer()
                .font_family(crate::api::compat::fonts::mono(cx))
                .text_size(text::S11)
                .text_color(if selected {
                    palette::text_secondary(cx)
                } else {
                    tertiary
                })
                .child(tick(selected, cx))
        };
        let collection_selected = selection.is_some_and(|selection| selection.include_collection);
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .rounded(radius::md())
            .border_1()
            .border_color(colors.border)
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::SP_2)
                    .px(space::SP_3)
                    .py(space::SP_2)
                    .bg(colors.muted)
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .text_size(text::S11)
                            .font_weight(text::weight::SEMIBOLD)
                            .text_color(colors.foreground)
                            .child("Generated collection"),
                    )
                    .child(
                        div()
                            .font_family(crate::api::compat::fonts::mono(cx))
                            .text_size(text::S10)
                            .text_color(colors.muted_foreground)
                            .child(format!(
                                "{} requests · {selected_requests} selected",
                                imported.requests.len()
                            )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .p(space::SP_2)
                    .bg(colors.sidebar)
                    .max_h(px(360.))
                    .overflow_y_scrollbar()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                row(
                                    "workbench-import-collection".into(),
                                    collection_selected,
                                    false,
                                )
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.toggle_import_collection(cx)),
                                )
                                .child(
                                    heading(
                                        format!("Collection · {}", imported.collection.name),
                                        colors.muted_foreground,
                                    )
                                    .truncate(),
                                ),
                            )
                            .children(imported.folders.iter().enumerate().map(|(index, folder)| {
                                let id = folder.id.clone();
                                let selected = selection
                                    .is_some_and(|selection| selection.folder_ids.contains(&id));
                                row(format!("workbench-import-folder-{index}"), selected, false)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_import_folder(id.clone(), cx)
                                    }))
                                    .child(icon("folder", 12., colors.muted_foreground))
                                    .child(
                                        heading(folder.name.clone(), colors.muted_foreground)
                                            .truncate(),
                                    )
                            }))
                            .children(imported.requests.iter().enumerate().map(
                                |(index, request)| {
                                    let id = request.id.clone();
                                    let selected = selection.is_some_and(|selection| {
                                        selection.request_ids.contains(&id)
                                    });
                                    let method = request.method.as_str().to_string();
                                    let destructive =
                                        request.method.as_str().eq_ignore_ascii_case("DELETE");
                                    row(format!("workbench-import-request-{index}"), selected, true)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.toggle_import_request(id.clone(), cx)
                                        }))
                                        .child(verb(
                                            method.clone(),
                                            if selected {
                                                self.method_tint_label(&method, cx)
                                            } else {
                                                tertiary
                                            },
                                            40.,
                                            cx,
                                        ))
                                        .child(div().flex_1().min_w(px(0.)).truncate().child(
                                            if request.url.is_empty() {
                                                request.name.clone()
                                            } else {
                                                request.url.clone()
                                            },
                                        ))
                                        .when(destructive, |el| {
                                            el.child(
                                                div()
                                                    .flex_none()
                                                    .text_color(if selected {
                                                        palette::warm(cx)
                                                    } else {
                                                        tertiary
                                                    })
                                                    .child(if selected {
                                                        "destructive — review"
                                                    } else {
                                                        "destructive — skipped"
                                                    }),
                                            )
                                        })
                                        .when(!destructive && !selected, |el| {
                                            el.child(div().flex_none().child("skipped"))
                                        })
                                },
                            ))
                            .children(imported.environments.iter().enumerate().map(
                                |(index, environment)| {
                                    let id = environment.id.clone();
                                    let selected = selection.is_some_and(|selection| {
                                        selection.environment_ids.contains(&id)
                                    });
                                    row(
                                        format!("workbench-import-environment-{index}"),
                                        selected,
                                        true,
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_import_environment(id.clone(), cx)
                                    }))
                                    .child(verb("ENV", palette::warm(cx), 40., cx))
                                    .child(div().truncate().child(environment.name.clone()))
                                },
                            ))
                            .children(imported.examples.iter().enumerate().map(
                                |(index, example)| {
                                    let id = example.id.clone();
                                    let selected = selection.is_some_and(|selection| {
                                        selection.example_ids.contains(&id)
                                    });
                                    row(format!("workbench-import-example-{index}"), selected, true)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.toggle_import_example(id.clone(), cx)
                                        }))
                                        .child(verb("EX", colors.info, 40., cx))
                                        .child(div().truncate().child(example.name.clone()))
                                },
                            )),
                    ),
            )
    }

    /// The review column: what the parser found (warnings, origin) and the
    /// accent "Import n requests" call to action.
    fn render_import_review(
        &self,
        imported: &ImportResult,
        selected_requests: usize,
        cx: &mut Context<Self>,
    ) -> Div {
        let colors = cx.theme().colors;
        let card = |title: String, body: String, tint: Option<Hsla>| {
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .p(px(10.))
                .rounded(radius::md())
                .border_1()
                .border_color(tint.map(|tint| tint.opacity(0.35)).unwrap_or(colors.border))
                .bg(tint.map(|tint| tint.opacity(0.06)).unwrap_or(colors.muted))
                .child(
                    div()
                        .text_size(text::S12)
                        .font_weight(text::weight::SEMIBOLD)
                        .text_color(tint.unwrap_or(colors.foreground))
                        .child(title),
                )
                .child(
                    div()
                        .text_size(text::S11)
                        .text_color(if tint.is_some() {
                            palette::text_secondary(cx)
                        } else {
                            colors.muted_foreground
                        })
                        .child(body),
                )
        };
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(300.))
            .gap(px(10.))
            .child(heading("What the parser found", palette::lavender(cx)))
            .child(card(
                format!("{} source", import_format_label(&imported.format)),
                format!(
                    "{} folders, {} requests, {} environments and {} examples are staged. Nothing is written until you import.",
                    imported.folders.len(),
                    imported.requests.len(),
                    imported.environments.len(),
                    imported.examples.len()
                ),
                None,
            ))
            .when_some(self.import_origin.clone(), |el, origin| {
                el.child(
                    div()
                        .id("workbench-import-origin")
                        .debug_selector(|| "workbench-import-origin".into())
                        .child(card("Origin".into(), origin, None)),
                )
            })
            .children(imported.warnings.iter().map(|warning| {
                card("Parser warning".into(), warning.clone(), Some(palette::warm(cx)))
            }))
            .child(
                accent_button(
                    "workbench-import-commit".into(),
                    format!("Import {selected_requests} requests"),
                    32.,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| this.commit_staged_import(cx))),
            )
    }
}

// ---------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------

/// The scenario presets the mock lists; toggled chips ride along in the
/// Generate-sample-data prompt.
pub(super) const DATA_PRESETS: [&str; 6] = [
    "Happy path",
    "Empty states",
    "Quota pressure",
    "Adversarial input",
    "Localised",
    "10k rows",
];

/// Column names whose values the data table masks.
pub(super) fn sensitive_column(name: &str) -> bool {
    let lower = name.to_lowercase();
    ["secret", "token", "password", "key", "authorization"]
        .iter()
        .any(|needle| lower.contains(needle))
}

pub(super) struct DataPreview {
    pub(super) rows: Result<Vec<Vec<Variable>>, String>,
    columns: Vec<String>,
    summary: String,
}

impl WorkbenchPanel {
    /// Success and parse errors both survive hover, selection and tab changes.
    /// The single source input invalidates this preview for typed and
    /// programmatic edits alike.
    pub(super) fn data_preview(&self, cx: &App) -> std::rc::Rc<DataPreview> {
        if let Some(preview) = self.data_preview.borrow().as_ref() {
            return preview.clone();
        }
        let source = self.data_source.read(cx).value().to_string();
        let rows = if source.trim().is_empty() {
            Ok(Vec::new())
        } else {
            draft::parse_data_rows(&source)
        };
        let mut seen = HashSet::new();
        let columns = rows
            .as_ref()
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .filter(|variable| seen.insert(variable.key.as_str()))
            .map(|variable| variable.key.clone())
            .collect();
        let preview = std::rc::Rc::new(DataPreview {
            rows,
            columns,
            summary: data_summary(&source),
        });
        *self.data_preview.borrow_mut() = Some(preview.clone());
        preview
    }

    fn render_data(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_1()
            .min_h(px(0.))
            .child(self.render_data_scenario(window, cx))
            .child(self.render_data_results(window, cx))
            .into_any_element()
    }

    /// `w 340 · gap 12 · padding 16 · border-r`: scenario prose, presets,
    /// constraints, rows/seed/delay and the lavender generate button.
    fn render_data_scenario(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let colors = cx.theme().colors;
        let lavender = palette::lavender(cx);
        let prompt_focused = self.data_prompt.focus_handle(cx).is_focused(window);
        let constraint =
            |id: &'static str, label: &'static str, on: bool, cx: &mut Context<Self>| {
                div()
                    .id(id)
                    .debug_selector(move || id.into())
                    .flex()
                    .items_center()
                    .gap(space::SP_2)
                    .cursor_pointer()
                    .text_size(text::S12)
                    .text_color(if on {
                        palette::text_secondary(cx)
                    } else {
                        palette::text_tertiary(cx)
                    })
                    .child(tick(on, cx))
                    .child(label)
            };
        let value_field =
            |label: &'static str, state: &Entity<InputState>, cx: &mut Context<Self>| {
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(space::SP_2)
                    .text_size(text::S12)
                    .text_color(palette::text_secondary(cx))
                    .child(label)
                    .child(
                        mono_field(state, window, cx)
                            .w(px(120.))
                            .h(px(24.))
                            .text_size(text::S11),
                    )
            };
        div()
            .flex_none()
            .w(px(340.))
            .min_h(px(0.))
            .border_r_1()
            .border_color(colors.border)
            .overflow_y_scrollbar()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::SP_3)
                    .p(space::SP_4)
                    .child(heading("Describe the scenario", lavender))
                    .child(
                        div()
                            .p(px(10.))
                            .min_h(px(96.))
                            .rounded(radius::md())
                            .border_1()
                            .border_color(if prompt_focused {
                                lavender
                            } else {
                                lavender.opacity(0.45)
                            })
                            .bg(colors.sidebar)
                            .text_size(text::S12)
                            .line_height(gpui_kit::relative(1.6))
                            .text_color(colors.foreground)
                            .child(field::bare(&self.data_prompt).w_full()),
                    )
                    .child(div().flex().flex_wrap().gap(px(6.)).children(
                        DATA_PRESETS.iter().enumerate().map(|(index, preset)| {
                            let active = self.data_presets.contains(&index);
                            div()
                                .id(SharedString::from(format!("workbench-data-preset-{index}")))
                                .debug_selector(move || format!("workbench-data-preset-{index}"))
                                .px(px(10.))
                                .py(space::SP_1)
                                .rounded(radius::full())
                                .border_1()
                                .border_color(if active {
                                    lavender.opacity(0.5)
                                } else {
                                    colors.input
                                })
                                .when(active, |el| el.bg(lavender.opacity(0.12)))
                                .cursor_pointer()
                                .text_size(text::S11)
                                .text_color(if active {
                                    lavender
                                } else {
                                    palette::text_secondary(cx)
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if !this.data_presets.remove(&index) {
                                        this.data_presets.insert(index);
                                    }
                                    cx.notify();
                                }))
                                .child(*preset)
                        }),
                    ))
                    .child(div().h(px(1.)).bg(colors.border))
                    .child(heading("Constraints", colors.muted_foreground))
                    .child(
                        constraint(
                            "workbench-data-schema-valid",
                            "Schema-valid values",
                            self.data_schema_valid,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.data_schema_valid = !this.data_schema_valid;
                            cx.notify();
                        })),
                    )
                    .child(
                        constraint(
                            "workbench-data-unique-keys",
                            "Unique keys per row",
                            self.data_unique_keys,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.data_unique_keys = !this.data_unique_keys;
                            cx.notify();
                        })),
                    )
                    .child(
                        constraint(
                            "workbench-run-stop-on-error",
                            "Stop on first error",
                            self.stop_on_error,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.stop_on_error = !this.stop_on_error;
                            cx.notify();
                        })),
                    )
                    .child(
                        constraint(
                            "workbench-run-keep-variables",
                            "Keep script variables between iterations",
                            self.keep_runner_variables,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.keep_runner_variables = !this.keep_runner_variables;
                            cx.notify();
                        })),
                    )
                    .child(value_field("Rows", &self.runner_iterations, cx))
                    .child(value_field("Seed", &self.runner_seed, cx))
                    .child(value_field("Delay (ms)", &self.runner_delay_ms, cx))
                    .child(ai_button(assist::AssistIntent::GenerateData, true, cx)),
            )
    }

    /// The right column: header with the run controls, the data editor, the
    /// parsed table, then the run results and inspector.
    fn render_data_results(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let colors = cx.theme().colors;
        let handle = cx.entity().downgrade();
        let preview = self.data_preview(cx);
        let rows = &preview.rows;
        let row_count = rows.as_ref().map(|rows| rows.len()).unwrap_or(0);
        let summary = preview.summary.clone();
        let source_focused = self.data_source.focus_handle(cx).is_focused(window);
        let run_scope_label = match (&self.runner_folder_id, self.runner_request_selection_active) {
            (Some(id), _) => self
                .workspace_data
                .as_ref()
                .and_then(|data| data.folders.iter().find(|folder| &folder.id == id))
                .map(|folder| format!("Scope: {}", folder.name))
                .unwrap_or_else(|| "Scope: folder".into()),
            (None, true) => format!("Scope: {} requests", self.runner_request_ids.len()),
            (None, false) => "Scope: collection".into(),
        };
        let scope_folders = self
            .workspace_data
            .as_ref()
            .map(|data| {
                data.folders
                    .iter()
                    .filter(|folder| {
                        self.current_collection_id.as_ref() == Some(&folder.collection_id)
                    })
                    .map(|folder| {
                        (
                            folder.id.clone(),
                            folder.name.clone(),
                            self.runner_folder_id.as_ref() == Some(&folder.id),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let all_folders = self
            .workspace_data
            .as_ref()
            .map(|data| data.folders.as_slice())
            .unwrap_or_default();
        let scope_requests = self
            .workspace_data
            .as_ref()
            .map(|data| {
                data.requests
                    .iter()
                    .filter(|request| {
                        self.current_collection_id.as_ref() == Some(&request.collection_id)
                            && self.runner_folder_id.as_ref().is_none_or(|folder| {
                                request.folder_id.as_ref().is_some_and(|request_folder| {
                                    folder_is_within(request_folder, folder, all_folders)
                                })
                            })
                    })
                    .map(|request| {
                        (
                            request.id.clone(),
                            format!("{} {}", request.method.as_str(), request.name),
                            !self.runner_request_selection_active
                                || self.runner_request_ids.contains(&request.id),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let scope_menu = menu_trigger(
            "workbench-run-scope",
            cx.theme().transparent,
            colors.input,
            palette::text_secondary(cx),
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(text::S11)
                .child(run_scope_label)
                .child(icon("chevron-down", 11., colors.muted_foreground)),
            cx,
        )
        .h(px(24.))
        .px(px(10.))
        .dropdown_menu(move |menu: PopupMenu, _window, _cx| {
            let mut menu = menu.min_w(px(240.)).max_h(px(360.)).scrollable(true).item(
                menu_item(
                    "workbench-run-folder-all".into(),
                    "Whole collection",
                    &handle,
                    |this, _, cx| {
                        this.runner_folder_id = None;
                        this.runner_request_ids.clear();
                        this.runner_request_selection_active = false;
                        cx.notify();
                    },
                )
                .checked(scope_folders.iter().all(|(_, _, active)| !active)),
            );
            for (id, name, active) in scope_folders.clone() {
                let toggle_id = id.clone();
                menu = menu.item(
                    menu_item(
                        format!("workbench-run-folder-{}", id.as_str()),
                        format!("Folder · {name}"),
                        &handle,
                        move |this, _, cx| this.toggle_runner_folder(toggle_id.clone(), cx),
                    )
                    .checked(active),
                );
            }
            if !scope_requests.is_empty() {
                menu = menu.separator().label("Requests");
            }
            for (id, name, selected) in scope_requests.clone() {
                let toggle_id = id.clone();
                menu = menu.item(
                    menu_item(
                        format!("workbench-run-request-{}", id.as_str()),
                        name,
                        &handle,
                        move |this, _, cx| this.toggle_runner_request(toggle_id.clone(), cx),
                    )
                    .checked(selected),
                );
            }
            menu
        });
        let run_label = match self.send_state {
            SendState::Idle => format!(
                "Run {} requests",
                row_count.max(1) * scope_requests_len(self)
            ),
            SendState::PreparingRun => "Cancel run preparation".into(),
            SendState::Sending => "Cancel active request".into(),
            SendState::Cancelling => "Cancelling…".into(),
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(space::SP_3)
                    .px(space::SP_4)
                    .py(space::SP_2)
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .flex_none()
                            .text_size(text::S12)
                            .font_weight(text::weight::SEMIBOLD)
                            .text_color(colors.foreground)
                            .child(summary),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .font_family(crate::api::compat::fonts::mono(cx))
                            .text_size(text::S11)
                            .text_color(colors.muted_foreground)
                            .child(self.run_status.clone().unwrap_or_else(|| {
                                format!("seed {}", self.runner_seed.read(cx).value().trim())
                            })),
                    )
                    .child(
                        outline_chip("workbench-data-use-as-body".into(), "Use as body", cx)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.use_row_as_body(window, cx)),
                            ),
                    )
                    .child(scope_menu)
                    .child(
                        accent_button("workbench-run-collection".into(), run_label, 24., cx)
                            .when(self.send_state != SendState::Idle, |el| {
                                el.bg(colors.danger.opacity(0.14))
                                    .border_color(colors.danger.opacity(0.45))
                                    .text_color(colors.danger)
                            })
                            .child(icon(
                                if self.send_state == SendState::Idle {
                                    "play"
                                } else {
                                    "stop"
                                },
                                11.,
                                if self.send_state == SendState::Idle {
                                    colors.primary_foreground
                                } else {
                                    colors.danger
                                },
                            ))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.send_state == SendState::Idle {
                                    this.run_collection(cx)
                                } else if matches!(
                                    this.send_state,
                                    SendState::PreparingRun | SendState::Sending
                                ) {
                                    this.cancel(cx)
                                }
                            })),
                    )
                    .context_menu({
                        let handle = cx.entity().downgrade();
                        let idle = self.send_state == SendState::Idle;
                        let cancelling = self.send_state == SendState::Cancelling;
                        move |menu, _, _| {
                            menu.item(
                                menu_item(
                                    "workbench-run-menu-start".into(),
                                    "Run collection",
                                    &handle,
                                    |this, _, cx| {
                                        if this.send_state == SendState::Idle {
                                            this.run_collection(cx);
                                        }
                                    },
                                )
                                .disabled(!idle),
                            )
                            .item(
                                menu_item(
                                    "workbench-run-menu-cancel".into(),
                                    "Cancel run",
                                    &handle,
                                    |this, _, cx| this.cancel(cx),
                                )
                                .disabled(idle || cancelling),
                            )
                            .separator()
                            .item(
                                menu_item(
                                    "workbench-run-menu-use-body".into(),
                                    "Use selected row as request body",
                                    &handle,
                                    |this, window, cx| this.use_row_as_body(window, cx),
                                )
                                .disabled(row_count == 0),
                            )
                        }
                    }),
            )
            .child(
                div().flex_1().min_h(px(0.)).overflow_y_scrollbar().child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(space::SP_3)
                        .p(space::SP_4)
                        .child(heading(
                            "Iteration data · JSON array or CSV",
                            colors.muted_foreground,
                        ))
                        .child(
                            code_box(cx)
                                .min_h(px(72.))
                                .text_size(text::S11)
                                .when(source_focused, |el| el.border_color(colors.ring))
                                .child(field::bare(&self.data_source).w_full()),
                        )
                        .map(|el| match rows {
                            Ok(rows) if !rows.is_empty() => {
                                el.child(self.render_data_table(rows, &preview.columns, cx))
                            }
                            Ok(_) => el,
                            Err(error) => {
                                el.child(note_card(colors.danger, Some("alert"), error.clone(), cx))
                            }
                        })
                        .children(self.render_run_results(cx)),
                ),
            )
    }

    /// `#` + one column per key, twelve rows at most; sensitive columns are
    /// masked and the selected row is highlighted.
    fn render_data_table(
        &self,
        rows: &[Vec<Variable>],
        columns: &[String],
        cx: &mut Context<Self>,
    ) -> Div {
        let colors = cx.theme().colors;
        let mut cols = vec![Col::Px(40.)];
        cols.extend(columns.iter().map(|_| Col::Flex));
        let mut headers = vec!["#".to_string()];
        headers.extend(columns.iter().map(|column| column.to_uppercase()));
        let header_refs = headers.iter().map(String::as_str).collect::<Vec<_>>();
        table(cx)
            .child(table_row(&cols, header_cells(&header_refs), true, cx))
            .children(rows.iter().take(12).enumerate().map(|(index, row)| {
                let selected = self.data_selected_row == Some(index);
                let mut cells = vec![tinted_cell(
                    (index + 1).to_string(),
                    palette::text_tertiary(cx),
                )];
                for column in columns {
                    let value = row
                        .iter()
                        .find(|variable| &variable.key == column)
                        .map(|variable| match &variable.value {
                            VariableValue::Plain(value) => value.clone(),
                            VariableValue::Secret(_) | VariableValue::MissingSecret(_) => {
                                "••••••••".into()
                            }
                        })
                        .unwrap_or_default();
                    let value = if sensitive_column(column) && !value.is_empty() {
                        "••••••••".to_string()
                    } else {
                        value
                    };
                    let tint = match (column.to_lowercase().as_str(), value.as_str()) {
                        (_, "") => palette::text_tertiary(cx),
                        ("tier", "pro") | ("tier", "enterprise") => colors.success,
                        ("tier", "free") => palette::warm(cx),
                        (name, _) if name.contains("error") => colors.danger,
                        _ => palette::text_secondary(cx),
                    };
                    cells.push(tinted_cell(value, tint));
                }
                let row = table_row(&cols, cells, false, cx)
                    .id(SharedString::from(format!("workbench-data-row-{index}")))
                    .debug_selector(move || format!("workbench-data-row-{index}"))
                    .cursor_pointer()
                    .when(selected, |el| el.bg(colors.accent))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.data_selected_row = if this.data_selected_row == Some(index) {
                            None
                        } else {
                            Some(index)
                        };
                        cx.notify();
                    }));
                div()
                    .id(SharedString::from(format!(
                        "workbench-data-row-menu-{index}"
                    )))
                    .child(row.context_menu({
                        let handle = cx.entity().downgrade();
                        move |menu, _, _| {
                            menu.item(menu_item(
                                "workbench-data-row-use-body".into(),
                                "Use row as request body",
                                &handle,
                                move |this, window, cx| {
                                    this.data_selected_row = Some(index);
                                    this.use_row_as_body(window, cx);
                                },
                            ))
                        }
                    }))
            }))
            .when(rows.len() > 12, |el| {
                el.child(
                    div()
                        .px(space::SP_2)
                        .py(px(6.))
                        .bg(colors.sidebar)
                        .text_color(palette::text_tertiary(cx))
                        .child(format!("… {} more rows", rows.len() - 12)),
                )
            })
    }

    /// The latest twenty runs as summary rows with their item rows, plus the
    /// inspector for the selected item.
    fn render_run_results(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let colors = cx.theme().colors;
        let Some(data) = self.workspace_data.as_ref() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if data.runs.is_empty() {
            return out;
        }
        out.push(heading("Runs", colors.muted_foreground).into_any_element());
        for (index, run) in data.runs.iter().take(20).enumerate() {
            let failures = run
                .item_results
                .iter()
                .filter(|item| item.error.is_some())
                .count();
            let selection = if run.selected_folder_id.is_some() {
                "folder scope".to_string()
            } else if run.selected_request_ids.is_empty() {
                "all requests".to_string()
            } else {
                format!("{} selected requests", run.selected_request_ids.len())
            };
            let run_id = run.id.clone();
            out.push(
                div()
                    .id(SharedString::from(format!("workbench-run-result-{index}")))
                    .debug_selector(move || format!("workbench-run-result-{index}"))
                    .flex()
                    .flex_col()
                    .rounded(radius::md())
                    .border_1()
                    .border_color(colors.border)
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::SP_2)
                            .px(space::SP_3)
                            .py(space::SP_2)
                            .bg(colors.muted)
                            .text_size(text::S11)
                            .text_color(palette::text_secondary(cx))
                            .child(
                                div()
                                    .font_weight(text::weight::SEMIBOLD)
                                    .text_color(colors.foreground)
                                    .child(format!("{:?}", run.status)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .font_family(crate::api::compat::fonts::mono(cx))
                                    .text_size(text::S10)
                                    .text_color(colors.muted_foreground)
                                    .child(format!(
                                        "{} iterations · {} items · {} failed{} · {} · {} ms delay · keep variables: {}",
                                        run.iteration_count,
                                        run.item_results.len(),
                                        failures,
                                        if run.stop_on_error { " · stopped on error" } else { "" },
                                        selection,
                                        run.delay_ms,
                                        if run.keep_variable_values { "on" } else { "off" },
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .bg(colors.sidebar)
                            .children(run.item_results.iter().enumerate().map(
                                |(item_index, item)| {
                                    let run_id = run_id.clone();
                                    let menu_run_id = run_id.clone();
                                    let selected = self.selected_run_result.as_ref()
                                        == Some(&(run.id.clone(), item_index));
                                    let status_label = item
                                        .status
                                        .map(|status| status.to_string())
                                        .unwrap_or_else(|| "error".into());
                                    let row = div()
                                        .id(SharedString::from(format!(
                                            "workbench-run-item-{index}-{item_index}"
                                        )))
                                        .debug_selector(move || {
                                            format!("workbench-run-item-{index}-{item_index}")
                                        })
                                        .flex()
                                        .items_center()
                                        .gap(space::SP_3)
                                        .px(space::SP_3)
                                        .py(px(6.))
                                        .border_t_1()
                                        .border_color(colors.sidebar_border)
                                        .cursor_pointer()
                                        .when(selected, |el| el.bg(colors.accent))
                                        .font_family(crate::api::compat::fonts::mono(cx))
                                        .text_size(text::S11)
                                        .text_color(palette::text_secondary(cx))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.select_run_result(run_id.clone(), item_index, cx)
                                        }))
                                        .child(
                                            div()
                                                .w(px(80.))
                                                .flex_none()
                                                .text_color(palette::text_tertiary(cx))
                                                .child(format!("iter {}", item.iteration + 1)),
                                        )
                                        .child(
                                            div()
                                                .w(px(56.))
                                                .flex_none()
                                                .text_color(
                                                    item.status
                                                        .map(|status| status_tint(status, cx))
                                                        .unwrap_or(colors.danger),
                                                )
                                                .child(status_label),
                                        )
                                        .child(
                                            div()
                                                .w(px(72.))
                                                .flex_none()
                                                .child(format!("{} ms", item.duration_ms)),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w(px(0.))
                                                .truncate()
                                                .text_color(if item.error.is_some() {
                                                    colors.danger
                                                } else {
                                                    colors.muted_foreground
                                                })
                                                .child(
                                                    item.error
                                                        .clone()
                                                        .unwrap_or_else(|| item.request_id.to_string()),
                                                ),
                                        );
                                    div()
                                        .id(SharedString::from(format!("workbench-run-item-menu-{}-{item_index}", menu_run_id)))
                                        .child(row.context_menu({
                                            let handle = cx.entity().downgrade();
                                            move |menu, _, _| {
                                                let run_id = menu_run_id.clone();
                                                menu.item(menu_item(
                                                    "workbench-run-item-inspect".into(),
                                                    "Inspect run result",
                                                    &handle,
                                                    move |this, _, cx| this.select_run_result(run_id.clone(), item_index, cx),
                                                ))
                                            }
                                        }))
                                },
                            )),
                    )
                    .into_any_element(),
            );
        }
        if let Some(item) = self
            .selected_run_result
            .as_ref()
            .and_then(|(run_id, index)| {
                data.runs
                    .iter()
                    .find(|run| &run.id == run_id)?
                    .item_results
                    .get(*index)
            })
        {
            let response = item.response.as_ref();
            let body = response
                .map(transport::response_from_snapshot)
                .map(|response| response.body)
                .filter(|body| !body.is_empty())
                .unwrap_or_else(|| "(body omitted or empty)".into());
            let tests = item
                .test_results
                .iter()
                .map(|test| {
                    format!(
                        "{} {}{}",
                        if test.skipped {
                            "SKIP"
                        } else if test.passed {
                            "PASS"
                        } else {
                            "FAIL"
                        },
                        test.name,
                        test.error
                            .as_ref()
                            .map(|error| format!(" · {error}"))
                            .unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            let detail = format!(
                "Request {} · iteration {}\nStatus: {} · {} ms\nFinal URL: {}\nHTTP: {} · received {} B · stored {} B{}\nFull body SHA-256: {}\n\nTests\n{}\n\nBody\n{}",
                item.request_id,
                item.iteration + 1,
                item.status
                    .map(|status| status.to_string())
                    .unwrap_or_else(|| "error".into()),
                item.duration_ms,
                response
                    .map(|response| response.final_url.as_str())
                    .unwrap_or("-"),
                response
                    .map(|response| response.http_version.as_str())
                    .unwrap_or("-"),
                response
                    .map(|response| response.received_bytes)
                    .unwrap_or(0),
                response.map(|response| response.stored_bytes).unwrap_or(0),
                response
                    .filter(|response| response.truncated)
                    .map(|_| " · truncated")
                    .unwrap_or(""),
                response
                    .and_then(|response| response.full_body_sha256.as_deref())
                    .unwrap_or("-"),
                if tests.is_empty() { "No tests" } else { &tests },
                body
            );
            out.push(
                div()
                    .id("workbench-run-inspector")
                    .debug_selector(|| "workbench-run-inspector".into())
                    .child(code_box(cx).text_size(text::S11).child(detail))
                    .into_any_element(),
            );
        }
        out
    }
}

/// How many requests one iteration of the run touches under the current scope.
fn scope_requests_len(panel: &WorkbenchPanel) -> usize {
    let Some(data) = panel.workspace_data.as_ref() else {
        return 0;
    };
    data.requests
        .iter()
        .filter(|request| panel.current_collection_id.as_ref() == Some(&request.collection_id))
        .filter(|request| {
            panel.runner_folder_id.as_ref().is_none_or(|folder| {
                request.folder_id.as_ref().is_some_and(|request_folder| {
                    folder_is_within(request_folder, folder, &data.folders)
                })
            })
        })
        .filter(|request| {
            !panel.runner_request_selection_active || panel.runner_request_ids.contains(&request.id)
        })
        .count()
}

// ---------------------------------------------------------------------------
// Environments
// ---------------------------------------------------------------------------

/// Every `{{name}}` reference in the serialised request set: URL, params,
/// headers, auth, body and scripts alike.
pub(super) fn variable_references(requests: &[SavedRequest]) -> HashSet<String> {
    let mut names = HashSet::new();
    for request in requests {
        let Ok(text) = serde_json::to_string(request) else {
            continue;
        };
        let mut search = 0;
        while let Some(open) = text[search..].find("{{") {
            let open = search + open;
            let Some(close) = text[open..].find("}}") else {
                break;
            };
            let name = text[open + 2..open + close].trim();
            if !name.is_empty() && !name.contains('"') {
                names.insert(name.to_string());
            }
            search = open + close + 2;
        }
    }
    names
}

impl WorkbenchPanel {
    fn render_envs(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let lavender = palette::lavender(cx);
        let environments = self
            .workspace_data
            .as_ref()
            .map(|data| data.environments.as_slice())
            .unwrap_or_default();
        let active = self.active_environment();
        let requests = self
            .workspace_data
            .as_ref()
            .map(|data| data.requests.as_slice())
            .unwrap_or_default();
        let references = variable_references(requests);
        let variables = active
            .map(|env| env.variables.as_slice())
            .unwrap_or_default();
        let secrets = variables
            .iter()
            .filter(|variable| {
                matches!(
                    variable.value,
                    VariableValue::Secret(_) | VariableValue::MissingSecret(_)
                )
            })
            .count();
        let unused = variables
            .iter()
            .filter(|variable| !references.contains(&variable.key))
            .count();
        let missing_refs = references
            .iter()
            .filter(|name| !variables.iter().any(|variable| &variable.key == *name))
            .count();
        let env_dot = self.environment_dot(cx);
        div()
            .flex()
            .flex_1()
            .min_h(px(0.))
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(space::SP_1)
                    .w(px(220.))
                    .p(space::SP_3)
                    .border_r_1()
                    .border_color(colors.border)
                    .overflow_y_scrollbar()
                    .child(heading("Environments", colors.muted_foreground).px(space::SP_2).pb(space::SP_1))
                    .child(Button::new("workbench-globals-open").debug_selector(|| "workbench-globals-open".into()).ghost().small().label("Workspace globals").disabled(self.storage_loading).on_click(cx.listener(|this, _, window, cx| this.open_globals(window, cx))))
                    .children(environments.iter().map(|environment| {
                        let id = environment.id.clone();
                        let menu_id = id.clone();
                        let selected = self.active_environment_id.as_ref() == Some(&id);
                        let missing = environment
                            .variables
                            .iter()
                            .any(|variable| matches!(variable.value, VariableValue::MissingSecret(_)));
                        let selector = format!("workbench-env-{}", id.as_str());
                        let row = div()
                            .id(SharedString::from(selector.clone()))
                            .debug_selector(move || selector.clone())
                            .flex()
                            .items_center()
                            .gap(space::SP_2)
                            .py(px(7.))
                            .px(space::SP_2)
                            .rounded(radius::sm())
                            .cursor_pointer()
                            .text_size(text::S12)
                            .text_color(if selected {
                                colors.foreground
                            } else {
                                palette::text_secondary(cx)
                            })
                            .when(selected, |el| el.bg(colors.accent))
                            .hover(|el| el.bg(colors.accent))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.activate_environment(id.clone(), window, cx)
                            }))
                            .child(dot(
                                6.,
                                if missing {
                                    palette::warm(cx)
                                } else if environment.active {
                                    colors.success
                                } else {
                                    palette::text_tertiary(cx)
                                },
                            ))
                            .child(div().flex_1().min_w(px(0.)).truncate().child(environment.name.clone()))
                            .child(
                                div()
                                    .font_family(crate::api::compat::fonts::mono(cx))
                                    .text_size(text::S10)
                                    .text_color(palette::text_tertiary(cx))
                                    .child(environment.variables.len().to_string()),
                            );
                        div()
                            .id(SharedString::from(format!("workbench-env-menu-{menu_id}")))
                            .child(row.context_menu({
                                let handle = cx.entity().downgrade();
                                move |menu, _, _| {
                                    let activate_id = menu_id.clone();
                                    let duplicate_id = menu_id.clone();
                                    let delete_id = menu_id.clone();
                                    menu.item(menu_item(
                                        "workbench-env-menu-activate".into(), "Activate environment", &handle,
                                        move |this, window, cx| this.activate_environment(activate_id.clone(), window, cx),
                                    ).checked(selected).disabled(selected))
                                    .item(menu_item(
                                        "workbench-env-menu-duplicate".into(), "Duplicate environment", &handle,
                                        move |this, window, cx| {
                                            if this.active_environment_id.as_ref() == Some(&duplicate_id) {
                                                this.duplicate_environment(window, cx);
                                                return;
                                            }
                                            let Some(mut environment) = this.workspace_data.as_ref()
                                                .and_then(|data| data.environments.iter().find(|environment| environment.id == duplicate_id))
                                                .cloned() else { return; };
                                            environment.name = format!("{} copy", environment.name);
                                            this.active_environment_id = None;
                                            this.load_environment_editor(Some(&environment), window, cx);
                                            this.storage_error = None;
                                            cx.notify();
                                        },
                                    ))
                                    .item(menu_item(
                                        "workbench-env-menu-new".into(), "New environment", &handle,
                                        |this, window, cx| this.new_environment(window, cx),
                                    ))
                                    .separator().item(menu_item(
                                        "workbench-env-menu-delete".into(), "Delete environment", &handle,
                                        move |this, window, cx| {
                                            if this.active_environment_id.as_ref() == Some(&delete_id) {
                                                this.delete_active_environment(window, cx);
                                            } else {
                                                this.run_storage_command(coordinator::StorageCommand::DeleteEnvironment(delete_id.clone()), cx);
                                            }
                                        },
                                    ))
                                }
                            }))
                    }))
                    .child(
                        div()
                            .id("workbench-env-new")
                            .debug_selector(|| "workbench-env-new".into())
                            .mt(space::SP_1)
                            .py(px(7.))
                            .px(space::SP_2)
                            .rounded(radius::sm())
                            .border_1()
                            .border_dashed()
                            .border_color(colors.input)
                            .cursor_pointer()
                            .text_size(text::S12)
                            .text_color(colors.muted_foreground)
                            .hover(|el| el.bg(colors.accent))
                            .on_click(cx.listener(|this, _, window, cx| this.new_environment(window, cx)))
                            .child("+ New environment"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .min_h(px(0.))
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(px(10.))
                            .px(space::SP_4)
                            .py(px(10.))
                            .border_b_1()
                            .border_color(colors.border)
                            .child(dot(8., env_dot))
                            .child(
                                // The design's name label, editable in place:
                                // this is where a new environment gets its name.
                                div()
                                    .flex_none()
                                    .w(px(200.))
                                    .text_size(text::S13)
                                    .font_weight(text::weight::SEMIBOLD)
                                    .text_color(colors.foreground)
                                    .child(field::bare(&self.environment_name).w_full()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .font_family(crate::api::compat::fonts::mono(cx))
                                    .text_size(text::S11)
                                    .text_color(colors.muted_foreground)
                                    .child(format!(
                                        "{} variables · {} secrets",
                                        variables.len(),
                                        secrets
                                    )),
                            )
                            .child(
                                ai_button(assist::AssistIntent::FillEnvironment, false, cx),
                            )
                            .child(
                                outline_chip("workbench-env-duplicate".into(), "Duplicate", cx)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.duplicate_environment(window, cx)
                                    })),
                            )
                            .child(
                                outline_chip("workbench-env-save".into(), "Save", cx)
                                    .border_color(colors.primary.opacity(0.45))
                                    .text_color(colors.primary)
                                    .on_click(cx.listener(|this, _, _, cx| this.save_environment(cx))),
                            )
                            .child(
                                outline_chip("workbench-env-delete".into(), "Delete", cx)
                                    .border_color(colors.danger.opacity(0.45))
                                    .text_color(colors.danger)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.delete_active_environment(window, cx)
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(0.))
                            .overflow_y_scrollbar()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(space::SP_3)
                                    .p(space::SP_4)
                                    .child(self.render_env_base_url(window, cx))
                                    .child(self.render_env_auth(cx))
                                    .child(heading("Variables", colors.muted_foreground))
                                    .child(self.environment_grid.clone())
                                    .child(note_card(
                                        colors.muted_foreground,
                                        Some("shield"),
                                        "Secrets never leave the machine — they resolve from the local keyring at send time and are redacted from history, examples and AI prompts.".to_string(),
                                        cx,
                                    ))
                                    .when(unused > 0, |el| {
                                        el.child(note_card(
                                            lavender,
                                            Some("sparkles"),
                                            format!(
                                                "{unused} {} unused by any request in this workspace.",
                                                if unused == 1 { "variable is" } else { "variables are" }
                                            ),
                                            cx,
                                        ))
                                    })
                                    .when(missing_refs > 0, |el| {
                                        el.child(note_card(
                                            palette::warm(cx),
                                            Some("alert"),
                                            format!(
                                                "{missing_refs} {{{{variable}}}} {} referenced by requests but not defined here — Fill from spec drafts them.",
                                                if missing_refs == 1 { "reference is" } else { "references are" }
                                            ),
                                            cx,
                                        ))
                                    }),
                            ),
                    ),
            )
            .into_any_element()
    }
}

impl WorkbenchPanel {
    /// `BASE URL` — the host relative request URLs are prefixed with.
    fn render_env_base_url(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let colors = cx.theme().colors;
        let source = self
            .active_environment_id
            .as_ref()
            .is_some_and(|id| self.imported_environment_ids.contains(id));
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::SP_2)
                    .child(heading("Base URL", colors.muted_foreground))
                    .when(source, |el| {
                        el.child(pill("import", palette::text_tertiary(cx), cx))
                    }),
            )
            .child(
                mono_field(&self.environment_base_url, window, cx)
                    .text_color(palette::lavender(cx)),
            )
            .child(
                div()
                    .text_size(text::S11)
                    .text_color(colors.muted_foreground)
                    .child(
                        "Relative request URLs (/pets) are prefixed with this · also available as {{base_url}}",
                    ),
            )
    }

    /// `AUTHENTICATION` — what requests inheriting all the way up send:
    /// type chips, the typed form, and for a sign-in request the button that
    /// runs it now plus the cached session's status.
    fn render_env_auth(&self, cx: &mut Context<Self>) -> Div {
        let colors = cx.theme().colors;
        let saved = self.active_environment();
        let stored = saved
            .is_some_and(|environment| auth_mode(&environment.auth) == self.environment_auth_mode);
        self.environment_auth_form.update(cx, |form, cx| {
            form.set_mode(self.environment_auth_mode, stored, cx);
        });
        let modes = [
            draft::AuthMode::None,
            draft::AuthMode::Bearer,
            draft::AuthMode::ApiKeyHeader,
            draft::AuthMode::ApiKeyQuery,
            draft::AuthMode::Basic,
            draft::AuthMode::OAuth2,
            draft::AuthMode::Login,
        ];
        // Both managed modes cache something the user can fetch right now.
        let fetch_now = match self.environment_auth_mode {
            draft::AuthMode::Login => Some(("workbench-env-login", "Sign in now")),
            draft::AuthMode::Basic
                if draft::auth_field_values(&self.environment_auth.read(cx).value())
                    .get("auth_url")
                    .is_some_and(|url| !url.trim().is_empty()) =>
            {
                Some(("workbench-env-login", "Sign in now"))
            }
            draft::AuthMode::OAuth2 => Some(("workbench-env-oauth-authorize", "Acquire token")),
            _ => None,
        };
        let status = self.environment_login_status.clone().or_else(|| {
            saved.and_then(|environment| {
                super::login::session_status(&environment.auth, now_seconds())
            })
        });
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::SP_2)
                    .child(heading("Authentication", colors.muted_foreground))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_size(text::S11)
                            .text_color(colors.muted_foreground)
                            .child("Requests set to Inherit use this unless a folder or collection overrides it"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(2.))
                    .children(modes.into_iter().map(|mode| {
                        chip(
                            format!("workbench-env-auth-{}", mode.label()),
                            mode.label(),
                            self.environment_auth_mode == mode,
                            cx,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.environment_auth_mode = mode;
                            this.environment_login_status = None;
                            cx.notify();
                        }))
                    })),
            )
            .child(self.environment_auth_form.clone())
            .when_some(fetch_now, |el, (selector, label)| {
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(space::SP_2)
                        .child(
                            outline_chip(selector.into(), label, cx)
                                .border_color(colors.primary.opacity(0.45))
                                .text_color(colors.primary)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    match this.environment_auth_mode {
                                        draft::AuthMode::OAuth2 => {
                                            this.acquire_environment_oauth_token(cx)
                                        }
                                        _ => this.sign_in_environment(cx),
                                    }
                                })),
                        )
                        .when_some(status, |el, status| {
                            el.child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .font_family(crate::api::compat::fonts::mono(cx))
                                    .text_size(text::S11)
                                    .text_color(colors.muted_foreground)
                                    .child(status),
                            )
                        }),
                )
            })
    }
}

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

const HISTORY_COLS: [Col; 7] = [
    Col::Px(120.),
    Col::Flex,
    Col::Px(90.),
    Col::Px(90.),
    Col::Px(90.),
    Col::Px(110.),
    Col::Px(100.),
];

impl WorkbenchPanel {
    fn render_history(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let filter = self.history_filter.read(cx).value().trim().to_lowercase();
        let filter_focused = self.history_filter.focus_handle(cx).is_focused(window);
        let now = now_millis();
        let cutoff = now - 24 * 3_600_000;
        let rows: Vec<(usize, &HistoryEntry)> = self
            .history
            .iter()
            .enumerate()
            .filter(|(_, entry)| history_matches(entry, &filter))
            .filter(|(_, entry)| !self.history_last_24h || entry.exchange.completed_at >= cutoff)
            .collect();
        let note = match self.selected_history.len() {
            2 => "Diff ready → open the Diff tab",
            1 => "Select one more run to compare",
            _ => "Select two runs to compare",
        };
        let examples = self
            .workspace_data
            .as_ref()
            .map(|data| data.examples.as_slice())
            .unwrap_or_default();
        let example_cols = [Col::Px(120.), Col::Flex, Col::Px(90.), Col::Px(100.)];
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scrollbar()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::SP_3)
                    .p(space::SP_4)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::SP_2)
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .h(px(26.))
                                    .w(px(220.))
                                    .px(space::SP_2)
                                    .rounded(radius::sm())
                                    .border_1()
                                    .border_color(if filter_focused { colors.ring } else { colors.border })
                                    .bg(colors.background)
                                    .text_size(text::S11)
                                    .child(icon("filter", 12., colors.muted_foreground))
                                    .child(field::bare(&self.history_filter).flex_1()),
                            )
                            .child(
                                chip("workbench-history-24h".into(), "Last 24 h", self.history_last_24h, cx)
                                    .h(px(26.))
                                    .border_1()
                                    .border_color(colors.border)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.history_last_24h = !this.history_last_24h;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                chip("workbench-history-retention".into(), format!("Keep latest {}", self.history_retention), false, cx)
                                    .h(px(26.))
                                    .border_1()
                                    .border_color(colors.border)
                                    .on_click(cx.listener(|this, _, _, cx| this.cycle_history_retention(cx))),
                            )
                            .child(div().flex_1())
                            .child(
                                div()
                                    .text_size(text::S11)
                                    .text_color(if self.selected_history.len() == 2 {
                                        colors.primary
                                    } else {
                                        palette::text_tertiary(cx)
                                    })
                                    .child(note),
                            ),
                    )
                    .map(|el| {
                        if self.history.is_empty() {
                            return el.child(
                                div()
                                    .p(space::SP_4)
                                    .rounded(radius::md())
                                    .border_1()
                                    .border_color(colors.border)
                                    .text_size(text::S11)
                                    .text_color(colors.muted_foreground)
                                    .child("No requests have completed in this workspace."),
                            );
                        }
                        el.child(
                            table(cx)
                                .child(table_row(
                                    &HISTORY_COLS,
                                    header_cells(&["When", "Target", "Status", "Time", "Tests", "Env", ""]),
                                    true,
                                    cx,
                                ))
                                .children(rows.iter().map(|(index, entry)| {
                                    let index = *index;
                                    let selected = self.selected_history.contains(&index);
                                    let passed = entry
                                        .response
                                        .test_results
                                        .iter()
                                        .filter(|test| test.passed && !test.skipped)
                                        .count();
                                    let skipped = entry.response.test_results.iter().filter(|test| test.skipped).count();
                                    let total = entry.response.test_results.len();
                                    let tint = self.method_tint_label(&entry.method, cx);
                                    let target = div()
                                        .flex()
                                        .items_center()
                                        .gap(space::SP_2)
                                        .min_w(px(0.))
                                        .child(verb(entry.method.clone(), tint, 44., cx))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w(px(0.))
                                                .truncate()
                                                .text_color(colors.foreground)
                                                .child(entry.target.clone()),
                                        )
                                        .into_any_element();
                                    let actions = div()
                                        .flex()
                                        .items_center()
                                        .gap(space::SP_1)
                                        .child(
                                            history_action(format!("workbench-history-replay-{index}"), "play", colors.primary, cx)
                                                .on_click(cx.listener(move |this, _, window, cx| {
                                                    this.replay_history(index, window, cx)
                                                })),
                                        )
                                        .child(
                                            history_action(format!("workbench-history-save-{index}"), "save", colors.primary, cx)
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.save_history_as_request(index, cx)
                                                })),
                                        )
                                        .child(
                                            history_action(format!("workbench-history-delete-{index}"), "trash", colors.danger, cx)
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.delete_history_entry(index, cx)
                                                })),
                                        )
                                        .into_any_element();
                                    let row = table_row(
                                        &HISTORY_COLS,
                                        vec![
                                            tinted_cell(
                                                pretty::relative_time(entry.exchange.completed_at, now),
                                                palette::text_tertiary(cx),
                                            ),
                                            target,
                                            tinted_cell(
                                                entry.response.status.to_string(),
                                                status_tint(entry.response.status, cx),
                                            ),
                                            cell(format!("{} ms", entry.elapsed_ms)),
                                            if total == 0 {
                                                tinted_cell("—", palette::text_tertiary(cx))
                                            } else {
                                                tinted_cell(
                                                    format!("{passed}/{total} · {skipped} skipped"),
                                                    if passed + skipped == total { colors.success } else { colors.danger },
                                                )
                                            },
                                            tinted_cell("—", palette::text_tertiary(cx)),
                                            actions,
                                        ],
                                        false,
                                        cx,
                                    )
                                    .id(SharedString::from(format!("workbench-history-{index}")))
                                    .debug_selector(move || format!("workbench-history-{index}"))
                                    .cursor_pointer()
                                    .when(selected, |el| el.bg(colors.accent))
                                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_history(index, cx)));
                                    let exchange_id = entry.exchange.id.clone();
                                    div()
                                        .id(SharedString::from(format!("workbench-history-menu-{exchange_id}")))
                                        .child(row.context_menu({
                                            let handle = cx.entity().downgrade();
                                            move |mut menu, _, _| {
                                                for (action, label) in [
                                                    ("replay", "Replay request"),
                                                    ("save", "Save as request"),
                                                    ("select", "Select for comparison"),
                                                    ("delete", "Delete history entry"),
                                                ] {
                                                    let id = exchange_id.clone();
                                                    if action == "delete" { menu = menu.separator(); }
                                                    menu = menu.item(menu_item(
                                                        format!("workbench-history-menu-{action}"), label, &handle,
                                                        move |this, window, cx| {
                                                            let Some(index) = this.history.iter().position(|entry| entry.exchange.id == id) else { return; };
                                                            match action {
                                                                "replay" => this.replay_history(index, window, cx),
                                                                "save" => this.save_history_as_request(index, cx),
                                                                "select" => this.toggle_history(index, cx),
                                                                _ => this.delete_history_entry(index, cx),
                                                            }
                                                        },
                                                    ).checked(action == "select" && selected));
                                                }
                                                menu
                                            }
                                        }))
                                }))
                                .when(rows.is_empty(), |el| {
                                    el.child(
                                        div()
                                            .px(space::SP_2)
                                            .py(px(6.))
                                            .bg(colors.muted)
                                            .text_color(palette::text_tertiary(cx))
                                            .child("No runs match the filter."),
                                    )
                                }),
                        )
                    })
                    .when(!examples.is_empty(), |el| {
                        el.child(heading("Saved examples", colors.muted_foreground)).child(
                            table(cx)
                                .child(table_row(
                                    &example_cols,
                                    header_cells(&["Example", "Name", "Status", ""]),
                                    true,
                                    cx,
                                ))
                                .children(examples.iter().enumerate().map(|(index, example)| {
                                    let replay_id = example.id.clone();
                                    let delete_id = example.id.clone();
                                    let method = example
                                        .request
                                        .as_ref()
                                        .map(|request| request.method.clone())
                                        .unwrap_or_else(|| "EX".into());
                                    let tint = self.method_tint_label(&method, cx);
                                    let row = table_row(
                                        &example_cols,
                                        vec![
                                            verb(method, tint, 44., cx).into_any_element(),
                                            cell(example.name.clone()),
                                            tinted_cell(
                                                example.response.status.to_string(),
                                                status_tint(example.response.status, cx),
                                            ),
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap(space::SP_1)
                                                .child(
                                                    history_action(format!("workbench-example-replay-{index}"), "play", colors.primary, cx)
                                                        .on_click(cx.listener(move |this, _, window, cx| {
                                                            this.replay_example(replay_id.clone(), window, cx)
                                                        })),
                                                )
                                                .child(
                                                    history_action(format!("workbench-example-delete-{index}"), "trash", colors.danger, cx)
                                                        .on_click(cx.listener(move |this, _, _, cx| {
                                                            this.delete_example(delete_id.clone(), cx)
                                                        })),
                                                )
                                                .into_any_element(),
                                        ],
                                        false,
                                        cx,
                                    )
                                    .id(SharedString::from(format!("workbench-example-{index}")));
                                    let id = example.id.clone();
                                    let replayable = example.request.is_some();
                                    div()
                                        .id(SharedString::from(format!("workbench-example-menu-{id}")))
                                        .child(row.context_menu({
                                            let handle = cx.entity().downgrade();
                                            move |menu, _, _| {
                                                let replay_id = id.clone();
                                                let delete_id = id.clone();
                                                menu.item(menu_item(
                                                    "workbench-example-menu-replay".into(), "Replay example", &handle,
                                                    move |this, window, cx| this.replay_example(replay_id.clone(), window, cx),
                                                ).disabled(!replayable))
                                                .separator().item(menu_item(
                                                    "workbench-example-menu-delete".into(), "Delete example", &handle,
                                                    move |this, _, cx| this.delete_example(delete_id.clone(), cx),
                                                ))
                                            }
                                        }))
                                })),
                        )
                    }),
            )
            .into_any_element()
    }
}

/// A 22px icon button inside a history row; stops the click from also
/// toggling the row's selection.
fn history_action(selector: String, name: &'static str, tint: Hsla, cx: &App) -> Stateful<Div> {
    let colors = cx.theme().colors;
    let id = selector.clone();
    div()
        .id(SharedString::from(selector))
        .debug_selector(move || id.clone())
        .flex()
        .items_center()
        .justify_center()
        .size(px(22.))
        .rounded(radius::xs())
        .border_1()
        .border_color(colors.input)
        .cursor_pointer()
        .hover(|el| el.bg(colors.accent))
        .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
            cx.stop_propagation()
        })
        .child(icon(name, 12., tint))
}

// ---------------------------------------------------------------------------
// Diff
// ---------------------------------------------------------------------------

impl WorkbenchPanel {
    fn render_diff(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let entries: Vec<&HistoryEntry> = self
            .selected_history
            .iter()
            .filter_map(|index| self.history.get(*index))
            .collect();
        let placeholder = |title: &'static str, body: &'static str, cx: &App| {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .items_center()
                .justify_center()
                .gap(space::SP_2)
                .p(space::SP_4)
                .child(
                    div()
                        .text_size(text::S13)
                        .font_weight(text::weight::SEMIBOLD)
                        .text_color(palette::text_secondary(cx))
                        .child(title),
                )
                .child(
                    div()
                        .text_size(text::S11)
                        .text_color(palette::text_tertiary(cx))
                        .child(body),
                )
                .into_any_element()
        };
        if entries.len() != 2 {
            return placeholder(
                "Select two responses in History",
                "Diff compares the durable, redacted response records saved with this workspace.",
                cx,
            );
        }
        let Some(diff) = self.diff_result.as_ref() else {
            return placeholder(
                "Comparing selected exchanges…",
                "Semantic diffing runs away from the frame thread.",
                cx,
            );
        };
        let (base, head) = (entries[0], entries[1]);
        let now = now_millis();
        let PreparedDiff {
            changes,
            left,
            right,
            added,
            removed,
        } = diff;
        let run_chip = |entry: &HistoryEntry, cx: &mut Context<Self>| {
            let tint = self.method_tint_label(&entry.method, cx);
            div()
                .flex()
                .items_center()
                .gap(space::SP_2)
                .py(px(6.))
                .px(px(10.))
                .rounded(radius::sm())
                .border_1()
                .border_color(colors.border)
                .bg(colors.muted)
                .font_family(crate::api::compat::fonts::mono(cx))
                .text_size(text::S11)
                .text_color(palette::text_secondary(cx))
                .child(verb(entry.method.clone(), tint, 44., cx))
                .child(div().truncate().max_w(px(320.)).child(entry.target.clone()))
                .child(
                    div()
                        .text_color(palette::text_tertiary(cx))
                        .child(pretty::relative_time(entry.exchange.completed_at, now)),
                )
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scrollbar()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::SP_3)
                    .p(space::SP_4)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::SP_2)
                            .child(run_chip(base, cx))
                            .child(icon("chevron-right", 14., colors.muted_foreground))
                            .child(run_chip(head, cx))
                            .child(div().flex_1())
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(space::SP_2)
                                    .font_family(crate::api::compat::fonts::mono(cx))
                                    .text_size(text::S11)
                                    .child(div().text_color(colors.success).child(format!("+{added}")))
                                    .child(div().text_color(colors.danger).child(format!("−{removed}")))
                                    .child(div().text_color(colors.muted_foreground).child(format!(
                                        "· {} ms → {} ms",
                                        base.elapsed_ms, head.elapsed_ms
                                    ))),
                            ),
                    )
                    .child(
                        div()
                            .id("workbench-diff-panes")
                            .debug_selector(|| "workbench-diff-panes".into())
                            .flex()
                            .gap(space::SP_3)
                            .child(self.render_diff_pane("BASE", base, left, cx))
                            .child(self.render_diff_pane("HEAD", head, right, cx)),
                    )
                    .child(if changes.is_empty() {
                        note_card(
                            colors.success,
                            Some("check"),
                            "No semantic differences — bodies and headers match; only timing differs.".to_string(),
                            cx,
                        )
                    } else {
                        div()
                            .flex()
                            .flex_col()
                            .gap(space::SP_1)
                            .p(px(10.))
                            .rounded(radius::md())
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.muted)
                            .font_family(crate::api::compat::fonts::mono(cx))
                            .text_size(text::S11)
                            .text_color(palette::text_secondary(cx))
                            .child(heading(format!("{} semantic changes", changes.len()), colors.muted_foreground))
                            .children(changes.iter().map(|change| {
                                let tint = match change.chars().next() {
                                    Some('+') => colors.success,
                                    Some('-') => colors.danger,
                                    Some('~') => palette::warm(cx),
                                    _ => colors.muted_foreground,
                                };
                                div().text_color(tint).child(change.clone())
                            }))
                    }),
            )
            .into_any_element()
    }

    /// One Diff pane: `p 6 10 bg muted mono 10` header, `p 10 bg sidebar mono
    /// 11 lh 1.8` body with `-`/`+` washes.
    fn render_diff_pane(
        &self,
        label: &'static str,
        entry: &HistoryEntry,
        lines: &[pretty::DiffLine],
        cx: &mut Context<Self>,
    ) -> Div {
        let colors = cx.theme().colors;
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .rounded(radius::md())
            .border_1()
            .border_color(colors.border)
            .overflow_hidden()
            .child(
                div()
                    .py(px(6.))
                    .px(px(10.))
                    .bg(colors.muted)
                    .border_b_1()
                    .border_color(colors.border)
                    .font_family(crate::api::compat::fonts::mono(cx))
                    .text_size(text::S10)
                    .text_color(colors.muted_foreground)
                    .child(format!(
                        "{label} · {} {} · {} ms",
                        entry.response.status, entry.response.reason, entry.elapsed_ms
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .p(px(10.))
                    .bg(colors.sidebar)
                    .font_family(crate::api::compat::fonts::mono(cx))
                    .text_size(text::S11)
                    .line_height(gpui_kit::relative(1.8))
                    .overflow_x_scrollbar()
                    .children(lines.iter().map(|line| {
                        match line {
                            pretty::DiffLine::Same(text) => div()
                                .whitespace_nowrap()
                                .text_color(palette::text_tertiary(cx))
                                .child(format!("  {text}")),
                            pretty::DiffLine::Removed(text) => div()
                                .whitespace_nowrap()
                                .bg(colors.danger.opacity(0.10))
                                .text_color(colors.danger)
                                .child(format!("- {text}")),
                            pretty::DiffLine::Added(text) => div()
                                .whitespace_nowrap()
                                .bg(colors.success.opacity(0.10))
                                .text_color(colors.success)
                                .child(format!("+ {text}")),
                            pretty::DiffLine::Elided(count) => div()
                                .text_color(colors.muted_foreground)
                                .child(format!("… {count} more lines")),
                        }
                    }))
                    .when(lines.is_empty(), |el| {
                        el.child(
                            div()
                                .text_color(palette::text_tertiary(cx))
                                .child("(empty body)"),
                        )
                    }),
            )
    }
}
