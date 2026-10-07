//! Switchyard desktop application.

// Release builds on Windows are GUI-subsystem binaries: no console window behind the app.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

extern crate gpui_kit as gpui;

mod actions;
mod api;
mod app_state;
mod completion;
mod conn_editor;
mod drivers_page;
mod editor_tab;
mod files_tab;
mod folds;
mod grid;
mod overlays;
mod palette;
mod plan_view;
mod remote_files;
mod sidebar;
mod split;
mod sql_tab;
mod ssh_prompts;
mod terminal_tab;
mod theme;
mod transfers;
mod ui;
mod viewer;
mod workload_tab;
mod workspace;

use std::borrow::Cow;

use anyhow::{Context as _, Result};
use gpui_kit::component::TitleBar;
use gpui_kit::{App, AppContext as _, Bounds, Global, WindowBounds, WindowOptions, px, size};
use switchyard_core::store::AppPaths;
use switchyard_core::{Core, ServiceConfig};
use tracing_subscriber::EnvFilter;

/// Keeps the core runtime alive for the life of the app.
struct CoreHolder(#[allow(dead_code)] Core);
impl Global for CoreHolder {}

const FONTS: &[&[u8]] = &[
    include_bytes!("../assets/fonts/Geist-Regular.ttf"),
    include_bytes!("../assets/fonts/Geist-Medium.ttf"),
    include_bytes!("../assets/fonts/Geist-SemiBold.ttf"),
    include_bytes!("../assets/fonts/Geist-Italic.ttf"),
    include_bytes!("../assets/fonts/GeistMono-Regular.ttf"),
    include_bytes!("../assets/fonts/GeistMono-Medium.ttf"),
    include_bytes!("../assets/fonts/GeistMono-SemiBold.ttf"),
    include_bytes!("../assets/fonts/GeistMono-Italic.ttf"),
];

fn load_fonts(cx: &mut App) {
    let fonts: Vec<Cow<'static, [u8]>> = FONTS.iter().map(|f| Cow::Borrowed(*f)).collect();
    if let Err(e) = cx.text_system().add_fonts(fonts) {
        tracing::warn!(error = %e, "could not load bundled fonts");
    }
}

fn main() -> Result<()> {
    // The API workspace runs `pm.*` scripts in a sandbox process: this binary with a hidden
    // argument, talking JSON over stdin/stdout. It must not start logging or the UI.
    if std::env::args().any(|a| a == switchyard_core::api::script::WORKER_ARG) {
        let code = switchyard_core::api::script::run_worker(
            std::io::stdin().lock(),
            std::io::stdout().lock(),
        );
        std::process::exit(i32::from(code));
    }
    let paths = AppPaths::resolve().context("could not determine a home directory")?;
    // Oracle Instant Client needs its folder on the loader path from process start.
    #[cfg(target_os = "linux")]
    switchyard_core::drivers::registry::reexec_with_loader_path(&paths.drivers_dir());
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("SWITCHYARD_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let (core, events) = Core::start(ServiceConfig::from_paths(&paths))?;
    let handle = core.handle();

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            load_fonts(cx);
            api::compat::init(cx, paths.data.join("api"), handle.clone());
            cx.set_global(CoreHolder(core));
            actions::init(cx);
            editor_tab::init(cx);
            // The saved theme arrives from the store once the workspace loads.
            let start = std::env::var("SWITCHYARD_THEME")
                .map(|k| theme::ThemeId::from_key(&k))
                .unwrap_or(theme::ThemeId::SwitchyardDark);
            theme::apply(start.palette(), None, cx);
            let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(900.), px(560.))),
                app_id: Some("dev.switchyard.Switchyard".into()),
                ..TitleBar::window_options()
            };
            let opened = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| workspace::Workspace::new(handle, events, window, cx))
            });
            if let Err(e) = opened {
                tracing::error!(error = %e, "failed to open the main window");
                cx.quit();
                return;
            }
            cx.activate(true);
        });
    Ok(())
}
