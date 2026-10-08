//! SQL snippets in the app (DBX-4b): the user's snippets as a GPUI global (loaded from the
//! store through core, never on the UI thread) and the "Manage snippets" window.
//!
//! Matching, built-ins and placeholder expansion live in `switchyard_store::snippets`;
//! completion items are built in `completion.rs`.

use gpui_kit::component::input::{Editor, EditorState, Input, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, FontWeight, Global,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, TitlebarOptions, Window, WindowBounds,
    WindowOptions, div, px, relative, size,
};
use switchyard_core::db::Engine;
use switchyard_core::store::Snippet;
use switchyard_core::store::snippets::builtin_snippets;
use switchyard_core::{Command, RuntimeHandle};

use crate::theme::{MONO, Palette, palette};
use crate::ui::{self, Kind};

/// Engines in the order the editor offers them.
const ENGINES: [Engine; 5] = [
    Engine::Postgres,
    Engine::SqlServer,
    Engine::Oracle,
    Engine::Snowflake,
    Engine::D1,
];

/// The user's snippets (built-ins excluded), as last loaded from the store.
#[derive(Default)]
pub struct SnippetLibrary {
    /// User snippets.
    pub user: Vec<Snippet>,
    requested: bool,
    window: Option<AnyWindowHandle>,
}

impl Global for SnippetLibrary {}

/// The user's snippets (empty until loaded).
pub fn user_snippets(cx: &App) -> &[Snippet] {
    cx.try_global::<SnippetLibrary>()
        .map_or(&[], |l| l.user.as_slice())
}

/// Ask core for the snippets once per app run; the answer arrives as `Event::Snippets`.
pub fn ensure_loaded(core: &RuntimeHandle, cx: &mut App) {
    let lib = cx.default_global::<SnippetLibrary>();
    if !lib.requested {
        lib.requested = true;
        core.send(Command::LoadSnippets);
    }
}

/// `Event::Snippets`: replace the library (observers, such as the manager, refresh).
pub fn on_snippets(list: Vec<Snippet>, cx: &mut App) {
    let lib = cx.default_global::<SnippetLibrary>();
    lib.requested = true;
    lib.user = list;
}

/// Open the "Manage snippets" window, or bring it to the front.
pub fn open_manager(core: RuntimeHandle, cx: &mut App) {
    ensure_loaded(&core, cx);
    if let Some(h) = cx.default_global::<SnippetLibrary>().window
        && h.update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        return;
    }
    let bounds = Bounds::centered(None, size(px(900.), px(600.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(size(px(640.), px(420.))),
        titlebar: Some(TitlebarOptions {
            title: Some("Snippets".into()),
            ..Default::default()
        }),
        app_id: Some("dev.switchyard.Switchyard".into()),
        ..Default::default()
    };
    match gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| SnippetsView::new(core, window, cx))
    }) {
        Ok((handle, _)) => cx.default_global::<SnippetLibrary>().window = Some(handle),
        Err(e) => tracing::error!(error = %e, "failed to open the snippets window"),
    }
}

/// The snippet manager: user snippets and built-ins on the left, the editor on the right.
pub struct SnippetsView {
    core: RuntimeHandle,
    /// Id of the snippet being edited (user or built-in); `None` for a new one.
    selected: Option<String>,
    name: Entity<InputState>,
    prefix: Entity<InputState>,
    body: Entity<EditorState>,
    engine: Option<Engine>,
    error: Option<SharedString>,
    /// Select the saved snippet (prefix, engine) when the reloaded list arrives.
    reselect: Option<(String, Option<Engine>)>,
    _subs: Vec<gpui_kit::Subscription>,
}

impl SnippetsView {
    fn new(core: RuntimeHandle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Select first rows"));
        let prefix = cx.new(|cx| InputState::new(window, cx).placeholder("sel"));
        let body = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(false)
                .indent_guides(false)
                .soft_wrap(true)
                .placeholder("SELECT ${2:*} FROM ${1:table_name};")
        });
        let sub = cx.observe_global_in::<SnippetLibrary>(window, |this, window, cx| {
            if let Some((prefix, engine)) = this.reselect.take() {
                let found = user_snippets(cx)
                    .iter()
                    .find(|s| s.prefix == prefix && s.engine == engine)
                    .cloned();
                if let Some(s) = found {
                    this.select(&s, window, cx);
                }
            }
            cx.notify();
        });
        Self {
            core,
            selected: None,
            name,
            prefix,
            body,
            engine: None,
            error: None,
            reselect: None,
            _subs: vec![sub],
        }
    }

    fn select(&mut self, s: &Snippet, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(s.id.clone());
        self.engine = s.engine;
        self.error = None;
        let (name, prefix, body) = (s.name.clone(), s.prefix.clone(), s.body.clone());
        self.name.update(cx, |i, cx| i.set_value(name, window, cx));
        self.prefix
            .update(cx, |i, cx| i.set_value(prefix, window, cx));
        self.body.update(cx, |e, cx| e.set_value(body, window, cx));
        cx.notify();
    }

    fn new_snippet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select(&Snippet::new("", "", "", None), window, cx);
        self.selected = None;
    }

    fn editing_builtin(&self) -> bool {
        self.selected
            .as_deref()
            .is_some_and(|id| id.starts_with(switchyard_core::store::snippets::BUILTIN_ID_PREFIX))
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let mut s = Snippet::new(
            &self.name.read(cx).value(),
            &self.prefix.read(cx).value(),
            &self.body.read(cx).value(),
            self.engine,
        );
        s.id = self.selected.clone().unwrap_or_default();
        if let Err(e) = s.validate() {
            self.error = Some(e.message.into());
            cx.notify();
            return;
        }
        self.error = None;
        self.reselect = Some((s.prefix.trim().to_owned(), s.engine));
        self.core.send(Command::SaveSnippet(s));
        cx.notify();
    }

    fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.selected.clone()
            && !self.editing_builtin()
        {
            self.core.send(Command::DeleteSnippet { id });
            self.new_snippet(window, cx);
        }
    }

    fn row(
        &self,
        s: &Snippet,
        overridden: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.selected.as_deref() == Some(s.id.as_str());
        let badge = s.engine.map_or("All", Engine::badge);
        let snippet = s.clone();
        div()
            .id(SharedString::from(format!("snip-{}", s.id)))
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(10.))
            .py(px(5.))
            .rounded(px(5.))
            .cursor_pointer()
            .when(active, |d| d.bg(p.sel))
            .hover(|d| d.bg(p.hover))
            .on_click(cx.listener(move |this, _, window, cx| this.select(&snippet, window, cx)))
            .child(
                ui::mono(s.prefix.clone(), px(12.))
                    .w(px(64.))
                    .flex_none()
                    .truncate()
                    .text_color(if overridden { p.fg3 } else { p.fg }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.))
                    .text_color(p.fg2)
                    .when(overridden, |d| d.line_through())
                    .child(s.name.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(10.5))
                    .text_color(p.fg3)
                    .child(badge),
            )
    }
}

fn field(label: &str, input: &Entity<InputState>, p: &Palette) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .text_size(px(11.))
                .text_color(p.fg3)
                .child(label.to_owned()),
        )
        .child(
            div()
                .h(px(28.))
                .flex()
                .items_center()
                .px(px(8.))
                .border_1()
                .border_color(p.bd2)
                .rounded(px(6.))
                .bg(p.bg)
                .font_family(MONO)
                .text_size(px(12.))
                .child(Input::new(input).appearance(false).text_size(px(12.))),
        )
}

impl Render for SnippetsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let user: Vec<Snippet> = user_snippets(cx).to_vec();
        let mut list = div()
            .id("snippet-list")
            .w(px(300.))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(1.))
            .p(px(8.))
            .border_r_1()
            .border_color(p.bd)
            .bg(p.panel)
            .overflow_y_scroll()
            .child(ui::caption("YOUR SNIPPETS", &p).px(px(10.)).py(px(4.)));
        if user.is_empty() {
            list = list.child(
                div()
                    .px(px(10.))
                    .py(px(4.))
                    .text_size(px(12.))
                    .text_color(p.fg3)
                    .child("None yet. Edit a built-in and save it to override it."),
            );
        }
        for s in &user {
            list = list.child(self.row(s, false, &p, cx));
        }
        for engine in ENGINES {
            list = list.child(
                ui::caption(
                    format!("BUILT-IN · {}", engine.display_name().to_uppercase()),
                    &p,
                )
                .px(px(10.))
                .pt(px(10.))
                .pb(px(4.)),
            );
            for b in builtin_snippets(engine) {
                let overridden = user
                    .iter()
                    .any(|u| u.applies_to(engine) && u.prefix.eq_ignore_ascii_case(&b.prefix));
                list = list.child(self.row(&b, overridden, &p, cx));
            }
        }

        let engine_options = std::iter::once(None)
            .chain(ENGINES.into_iter().map(Some))
            .map(|e| {
                let label: SharedString = e.map_or("All engines", Engine::display_name).into();
                let on_click: ui::OnClick = Box::new({
                    let view = cx.entity().downgrade();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.engine = e;
                            cx.notify();
                        });
                    }
                });
                (label, self.engine == e, on_click)
            })
            .collect();
        let builtin = self.editing_builtin();
        let title = match (&self.selected, builtin) {
            (None, _) => "New snippet",
            (Some(_), true) => "Built-in snippet (save to override)",
            (Some(_), false) => "Edit snippet",
        };
        let form = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(18.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.fg)
                            .child(title),
                    )
                    .child(
                        ui::button("snip-new", "New", Kind::Secondary, &p)
                            .on_click(cx.listener(|this, _, window, cx| this.new_snippet(window, cx))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap(px(10.))
                    .child(div().flex_1().child(field("Name", &self.name, &p)))
                    .child(div().w(px(140.)).child(field("Prefix (completion trigger)", &self.prefix, &p))),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(div().text_size(px(11.)).text_color(p.fg3).child("Engine"))
                    .child(ui::segmented("snip-engine", engine_options, 22., &p)),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(p.fg3)
                    .child("Body. ${1:placeholder} marks a tab stop: its text is inserted and the first one is selected. $1 alone stays as written."),
            )
            .child(
                div()
                    .id("snip-body")
                    .flex_1()
                    .min_h(px(120.))
                    .border_1()
                    .border_color(p.bd2)
                    .rounded(px(6.))
                    .bg(p.bg)
                    .child(
                        Editor::new(&self.body)
                            .bordered(false)
                            .appearance(false)
                            .h(relative(1.))
                            .font_family(MONO)
                            .text_size(px(12.5)),
                    ),
            )
            .when_some(self.error.clone(), |d, e| {
                d.child(div().text_size(px(12.)).text_color(p.prod).child(e))
            })
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .justify_end()
                    .when(self.selected.is_some() && !builtin, |d| {
                        d.child(
                            ui::button("snip-delete", "Delete", Kind::Destructive, &p)
                                .on_click(cx.listener(|this, _, window, cx| this.delete(window, cx))),
                        )
                    })
                    .child(
                        ui::button(
                            "snip-save",
                            if builtin { "Save as override" } else { "Save" },
                            Kind::Primary,
                            &p,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ),
            );
        div()
            .size_full()
            .flex()
            .bg(p.surface)
            .text_color(p.fg)
            .child(list)
            .child(form)
    }
}
