//! What the API workbench (ported from AgentOps) expects from its host shell, answered with
//! Switchyard's theme, toasts, settings and runtime: design tokens, the palette accents,
//! the mono font, bare text fields, dialogs, notifications, preferences, the data
//! directory and background work.
//!
//! The names mirror AgentOps's (`theme::tokens::space::SP_2`, `palette::warm(cx)`, …) so
//! the ported files keep their call sites.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::dialog::Dialog;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::{App, Div, Entity, Global, Pixels, SharedString, Styled, Window, div, px};
use switchyard_core::RuntimeHandle;

/// Where the API workspace keeps its data and which runtime runs its work. Set once at
/// startup by [`init`].
pub struct ApiHost {
    /// The core runtime.
    pub runtime: RuntimeHandle,
}

impl Global for ApiHost {}

/// Install the host for the API workspace.
pub fn init(cx: &mut App, data_dir: PathBuf, runtime: RuntimeHandle) {
    let _ = settings::data_dir_cell().set(data_dir.clone());
    let _ = RUNTIME.set(runtime.clone());
    switchyard_core::api::runtime::set_process_defaults(runtime.tokio(), data_dir);
    bind_keys(cx);
    cx.set_global(ApiHost { runtime });
}

static RUNTIME: OnceLock<RuntimeHandle> = OnceLock::new();

/// Run `f` on the core runtime's blocking pool; await the result from a GPUI task.
/// (AgentOps ran these on GPUI's background executor; Switchyard keeps disk and network
/// work on the core runtime.)
pub fn blocking<T, F>(f: F) -> impl Future<Output = T> + 'static
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let handle = RUNTIME.get().map(|rt| rt.spawn_blocking(f));
    async move {
        match handle {
            Some(handle) => match handle.await {
                Ok(v) => v,
                // The job panicked or the runtime is shutting down: nothing will answer.
                Err(_) => std::future::pending().await,
            },
            None => std::future::pending().await,
        }
    }
}

/// The API workspace's secret store (Switchyard's keychain or vault).
pub fn secrets(cx: &App) -> std::sync::Arc<dyn switchyard_core::api::SecretStore> {
    cx.global::<ApiHost>().runtime.api_secrets()
}

/// The Workbench workspace the panel shows, once one is chosen: the scope
/// [`super::workbench`]'s `current_workspace_id` resolves to.
pub fn current_project() -> Option<String> {
    selected_project()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Choose the Workbench workspace (its id); the panel rehydrates on its next frame.
pub fn set_current_project(project: Option<String>) {
    *selected_project()
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = project;
}

fn selected_project() -> &'static Mutex<Option<String>> {
    static SELECTED: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    SELECTED.get_or_init(Default::default)
}

gpui_kit::actions!(api_compat, [Save]);

/// Bind the workbench's keys (AgentOps's shell table for its panel).
pub fn bind_keys(cx: &mut App) {
    use super::workbench::{
        CancelRequest, CloseRequest, KEY_CONTEXT, NewRequest, NextRequest, PreviousRequest,
        SendRequest,
    };
    use gpui_kit::KeyBinding;
    let ctx = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("secondary-s", Save, ctx),
        KeyBinding::new("secondary-enter", SendRequest, ctx),
        KeyBinding::new("secondary-shift-enter", CancelRequest, ctx),
        KeyBinding::new("secondary-n", NewRequest, ctx),
        KeyBinding::new("secondary-w", CloseRequest, ctx),
        KeyBinding::new("ctrl-pagedown", NextRequest, ctx),
        KeyBinding::new("ctrl-pageup", PreviousRequest, ctx),
    ]);
}

/// The kit's (Lucide) icon for one of AgentOps's icon names.
pub fn icon_path(name: &str) -> SharedString {
    let name = match name {
        "alert" => "triangle-alert",
        "filter" => "funnel",
        "history" => "clock",
        "kebab" => "ellipsis-vertical",
        "refresh" => "refresh-cw",
        "spark" => "sparkles",
        "stop" => "circle-stop",
        other => other,
    };
    format!("icons/{name}.svg").into()
}

pub mod theme {
    //! AgentOps's design tokens, palette accents and type scale.

    pub mod tokens {
        pub mod space {
            use gpui_kit::{Pixels, px};
            pub const SP_1: Pixels = px(4.);
            pub const SP_2: Pixels = px(8.);
            pub const SP_3: Pixels = px(12.);
            pub const SP_4: Pixels = px(16.);
            pub const SP_5: Pixels = px(20.);
            pub const SP_6: Pixels = px(24.);
        }

        pub mod radius {
            use gpui_kit::{Pixels, px};
            pub fn xs() -> Pixels {
                px(3.)
            }
            pub fn sm() -> Pixels {
                px(5.)
            }
            pub fn md() -> Pixels {
                px(6.)
            }
            pub fn full() -> Pixels {
                px(9999.)
            }
        }
    }

    pub mod palette {
        use gpui_kit::{App, Hsla};
        /// Secondary text.
        pub fn text_secondary(cx: &App) -> Hsla {
            crate::theme::palette(cx).fg2
        }
        /// Tertiary (muted) text.
        pub fn text_tertiary(cx: &App) -> Hsla {
            crate::theme::palette(cx).fg3
        }
        /// The warm (amber) accent.
        pub fn warm(cx: &App) -> Hsla {
            crate::theme::palette(cx).stg
        }
        /// The lavender accent.
        pub fn lavender(cx: &App) -> Hsla {
            crate::theme::palette(cx).sx_kw
        }
    }

    pub mod text {
        //! Type sizes in pixels (AgentOps steps were rems at a 13 px root; Switchyard
        //! sizes text in pixels).
        use gpui_kit::{Pixels, px};
        pub const S9: Pixels = px(9.);
        pub const S10: Pixels = px(10.);
        pub const S11: Pixels = px(11.);
        pub const S12: Pixels = px(12.);
        pub const S13: Pixels = px(13.);
        pub const S14: Pixels = px(14.);

        pub mod weight {
            use gpui_kit::FontWeight;
            pub const MEDIUM: FontWeight = FontWeight::MEDIUM;
            pub const SEMIBOLD: FontWeight = FontWeight::SEMIBOLD;
            pub const BOLD: FontWeight = FontWeight::BOLD;
        }
    }
}

pub mod fonts {
    use gpui_kit::{App, SharedString};
    /// The monospace family.
    pub fn mono(_cx: &App) -> SharedString {
        crate::theme::MONO.into()
    }
}

pub mod field {
    //! A text field without its own chrome, for frames that draw one border.
    use super::*;
    use gpui_kit::IntoElement;
    use gpui_kit::component::input::{Textarea, TextareaState};

    /// An input kind that renders bare.
    pub trait Bare {
        /// The widget.
        type Out: IntoElement + Styled;
        /// The widget with no border, background or padding.
        fn bare(&self) -> Self::Out;
    }

    impl Bare for Entity<InputState> {
        type Out = Input;
        fn bare(&self) -> Input {
            Input::new(self).appearance(false).p_0()
        }
    }

    impl Bare for Entity<TextareaState> {
        type Out = Textarea;
        fn bare(&self) -> Textarea {
            Textarea::new(self).appearance(false).p_0()
        }
    }

    /// The input alone: no border, background or padding.
    pub fn bare<T: Bare>(state: &T) -> T::Out {
        state.bare()
    }
}

pub mod dialogs {
    //! Dialogs with a stated dismissal policy (gpui-component's dialog layer).
    use super::*;

    /// Whether a press outside closes the dialog.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Dismiss {
        /// Lists, pickers, notices: closing loses nothing.
        #[allow(dead_code)]
        Light,
        /// Forms and destructive questions: only Cancel, the cross or Escape.
        Explicit,
    }

    impl Dismiss {
        /// `Dialog::overlay_closable` for this policy.
        pub fn overlay_closable(self) -> bool {
            matches!(self, Dismiss::Light)
        }
    }

    /// Open a dialog with its dismissal policy.
    pub fn open<F>(window: &mut Window, cx: &mut App, dismiss: Dismiss, build: F)
    where
        F: Fn(Dialog, &mut Window, &mut App) -> Dialog + 'static,
    {
        open_with(window, cx, move |_| dismiss, build)
    }

    /// [`open`], with the policy decided every frame.
    pub fn open_with<P, F>(window: &mut Window, cx: &mut App, policy: P, build: F)
    where
        P: Fn(&mut App) -> Dismiss + 'static,
        F: Fn(Dialog, &mut Window, &mut App) -> Dialog + 'static,
    {
        window.open_dialog(cx, move |dialog, window, cx| {
            let dismiss = policy(cx);
            build(dialog, window, cx).overlay_closable(dismiss.overlay_closable())
        });
    }

    /// The dialog's Cancel button (gpui-component 0.5 passed one to the footer builder). It
    /// runs the dialog's `on_cancel` like the old one did.
    pub fn cancel_button(_window: &mut Window, _cx: &mut App) -> gpui_kit::AnyElement {
        use gpui_kit::IntoElement as _;
        use gpui_kit::component::dialog::DialogClose;
        DialogClose::new()
            .trigger(|button| button.label("Cancel"))
            .into_any_element()
    }

    /// Cancel and OK buttons for a dialog with an `on_ok` handler (what `.confirm()` drew
    /// in gpui-component 0.5).
    pub fn confirm_footer(ok_text: &'static str) -> gpui_kit::component::dialog::DialogFooter {
        use gpui_kit::ParentElement as _;
        use gpui_kit::component::button::{Button, ButtonVariants as _};
        use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
        DialogFooter::new()
            .child(DialogClose::new().trigger(|button| button.label("Cancel")))
            .child(DialogAction::new().child(Button::new("ok").primary().label(ok_text)))
    }

    /// A dialog footer: the buttons right-aligned in a row.
    pub fn footer_row(buttons: Vec<gpui_kit::AnyElement>) -> Div {
        use gpui_kit::ParentElement as _;
        div().flex().justify_end().gap_2().children(buttons)
    }

    /// How tall a dialog may get before its body scrolls.
    pub fn dialog_ceiling(window: &Window) -> Pixels {
        (window.viewport_size().height * 0.8).max(px(320.))
    }

    /// `preferred`, clamped to the window.
    pub fn dialog_width(window: &Window, preferred: Pixels) -> Pixels {
        preferred
            .min(window.viewport_size().width * 0.92)
            .max(px(320.))
    }
}

pub mod notify {
    //! Toasts: queued here, shown by the workspace.
    use super::*;
    use gpui_kit::BorrowAppContext;

    /// A queued toast message.
    #[derive(Default)]
    pub struct Toasts(pub Vec<String>);

    impl Global for Toasts {}

    fn push(cx: &mut App, message: impl Into<SharedString>) {
        let message: SharedString = message.into();
        // `update_default_global` notifies the workspace's observer.
        cx.update_default_global::<Toasts, _>(|toasts, _| toasts.0.push(message.to_string()));
    }

    /// A success toast.
    pub fn success(cx: &mut App, message: impl Into<SharedString>) {
        push(cx, message);
    }

    /// A warning toast.
    pub fn warning(cx: &mut App, message: impl Into<SharedString>) {
        push(cx, message);
    }

    /// Take the queued toasts.
    pub fn drain(cx: &mut App) -> Vec<String> {
        if !cx.has_global::<Toasts>() || cx.global::<Toasts>().0.is_empty() {
            return Vec::new();
        }
        // Taking through `global_mut` would notify observers again; replace quietly.
        let taken = cx.global::<Toasts>().0.clone();
        cx.global_mut::<Toasts>().0.clear();
        taken
    }
}

// The layout keeps its preferences only outside tests.
#[cfg_attr(test, allow(dead_code))]
pub mod settings {
    //! Small UI preferences (layout) and the data directory.
    use super::*;

    fn prefs() -> MutexGuard<'static, HashMap<String, String>> {
        static PREFS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
        PREFS
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A stored preference (kept for the session).
    pub fn preference(key: &str) -> Option<String> {
        prefs().get(key).cloned()
    }

    /// Store a preference (kept for the session).
    pub fn set_preference(key: &str, value: &str) -> Result<(), String> {
        prefs().insert(key.to_owned(), value.to_owned());
        Ok(())
    }

    /// The API workspace's data directory, once [`super::init`] ran.
    pub fn user_data_dir() -> Option<PathBuf> {
        data_dir_cell().get().cloned()
    }

    pub(crate) fn data_dir_cell() -> &'static OnceLock<PathBuf> {
        static DIR: OnceLock<PathBuf> = OnceLock::new();
        &DIR
    }
}

/// Reading and replacing the text of any input kind (gpui-component 0.7 split AgentOps's
/// one `InputState` into single-line, text-area and editor states).
pub trait TextValue {
    /// The current text.
    fn text(&self, cx: &App) -> SharedString;
    /// Replace the text without recording undo or emitting a change.
    fn set_text(&self, value: String, window: &mut Window, cx: &mut App);
}

macro_rules! text_value {
    ($t:ty) => {
        impl TextValue for Entity<$t> {
            fn text(&self, cx: &App) -> SharedString {
                self.read(cx).value()
            }
            fn set_text(&self, value: String, window: &mut Window, cx: &mut App) {
                self.update(cx, move |input, cx| input.set_value(value, window, cx));
            }
        }
    };
}

text_value!(InputState);
text_value!(gpui_kit::component::input::TextareaState);
text_value!(gpui_kit::component::input::EditorState);

/// One of the request editor's inputs, whichever kind it is.
pub enum AnyInput<'a> {
    /// A single-line input.
    Line(&'a Entity<InputState>),
    /// A multi-line text area.
    Area(&'a Entity<gpui_kit::component::input::TextareaState>),
}

impl AnyInput<'_> {
    /// The input as [`TextValue`].
    pub fn as_text(&self) -> &dyn TextValue {
        match self {
            AnyInput::Line(e) => *e,
            AnyInput::Area(e) => *e,
        }
    }
}
