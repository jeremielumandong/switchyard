//! Switchyard desktop application.

// Release builds on Windows are GUI-subsystem binaries: no console window behind the app.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

extern crate gpui_kit as gpui;

mod actions;
mod activity_tab;
mod api;
mod app_state;
mod appearance;
mod assistant_panel;
mod assistant_settings;
mod bulk_edit;
mod chat_markdown;
mod cloud_tab;
mod completion;
mod conn_editor;
mod ddl_tab;
mod drivers_page;
mod editor_tab;
mod er_tab;
mod explorer;
mod explorer_tree;
mod files_tab;
mod folds;
mod forwards_editor;
mod grid;
mod log_file;
mod object_search;
mod object_tab;
mod omarchy;
mod overlays;
mod palette;
mod plan_view;
mod rail;
mod redis_tab;
mod remote_files;
mod result_diff;
mod rich_text;
mod sidebar;
mod snippets;
mod split;
mod sql_tab;
mod ssh_prompts;
mod terminal_settings;
mod terminal_tab;
mod theme;
mod transfers;
mod ui;
mod unsaved;
mod updates;
mod viewer;
mod workload_tab;
mod workspace;

use std::borrow::Cow;

use anyhow::{Context as _, Result};
use gpui_kit::component::TitleBar;
use gpui_kit::{App, AppContext as _, Bounds, Global, WindowBounds, WindowOptions, px, size};
use switchyard_core::store::AppPaths;
use switchyard_core::{Command, Core, ServiceConfig};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

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

/// Log to stdout and to `<data>/logs/switchyard.log` (rotated; see [`log_file`]). Release
/// builds on Windows have no console, so the file is the only record there. Panics are
/// logged too, then reported as usual.
fn init_logging(paths: &AppPaths) {
    let filter =
        EnvFilter::try_from_env("SWITCHYARD_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let file = match log_file::LogFile::start(&paths.logs_dir()) {
        Ok(f) => Some(f),
        Err(e) => {
            eprintln!(
                "switchyard: no log file in {}: {e}",
                paths.logs_dir().display()
            );
            None
        }
    };
    let file_layer = file.map(|f| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(f)
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
        .with(file_layer)
        .init();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(panic = %info, "the app panicked");
        default_hook(info);
    }));
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        portable = paths.portable,
        "Switchyard starting"
    );
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
    init_logging(&paths);

    let (core, events) = Core::start(ServiceConfig::from_paths(&paths))?;
    let handle = core.handle();
    // `swy explain --open` hands plans to this app over a loopback socket.
    handle.send(Command::StartHandoff {
        data_dir: paths.data.clone(),
    });

    gpui_kit::application()
        .with_assets(rail::AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            load_fonts(cx);
            api::compat::init(cx, paths.data.join("api"), handle.clone());
            cx.set_global(CoreHolder(core));
            cx.set_global(updates::LogDir(paths.logs_dir()));
            actions::init(cx);
            editor_tab::init(cx);
            // The saved theme arrives from the store once the workspace loads.
            // SWITCHYARD_ZOOM (tests, screenshots: `1.5`) wins over the saved zoom.
            if let Some(z) = appearance::env_zoom() {
                appearance::set(
                    appearance::AppearanceSettings {
                        zoom: z,
                        ..Default::default()
                    },
                    cx,
                );
            }
            let start = std::env::var("SWITCHYARD_THEME")
                .map(|k| theme::ThemeId::from_key(&k))
                .unwrap_or(theme::ThemeId::SwitchyardDark);
            theme::apply(start.palette(), None, cx);
            let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(900.), px(560.))),
                // Matches switchyard.desktop (and its StartupWMClass), so Wayland and X11 docks show its icon.
                app_id: Some("switchyard".into()),
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
