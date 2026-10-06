//! Switchyard desktop application.

// Release builds on Windows are GUI-subsystem binaries: no console window behind the app.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

extern crate gpui_kit as gpui;

mod actions;
mod app_state;
mod completion;
mod conn_editor;
mod files_tab;
mod grid;
mod overlays;
mod palette;
mod sidebar;
mod sql_tab;
mod terminal_tab;
mod theme;
mod ui;
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
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("SWITCHYARD_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let paths = AppPaths::resolve().context("could not determine a home directory")?;
    let (core, events) = Core::start(ServiceConfig::from_paths(&paths))?;
    let handle = core.handle();

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            load_fonts(cx);
            cx.set_global(CoreHolder(core));
            actions::init(cx);
            let dark = !matches!(std::env::var("SWITCHYARD_THEME").as_deref(), Ok("light"));
            theme::apply(
                if dark {
                    theme::Palette::dark()
                } else {
                    theme::Palette::light()
                },
                None,
                cx,
            );
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
