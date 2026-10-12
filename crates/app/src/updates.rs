//! In-app updates (M6-4), ported from Emulsion: a launch-time check of the latest GitHub
//! release, a verified background download, and installation when Switchyard restarts.
//! The check and download run on the core runtime ([`Command::CheckForUpdates`],
//! [`Command::DownloadUpdate`]); this module keeps the state, draws the status-bar notice
//! and the Settings → General block, and starts the staged update after exit.
//!
//! Unsaved work is never discarded: "Restart now" goes through the normal window-close
//! confirmation, and the new AppImage or the Windows installer starts only after the event
//! loop has returned ([`run_after_exit`]). On macOS the verified disk image is opened.

use std::path::PathBuf;
use std::sync::Mutex;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, Div, Global, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
};
use switchyard_core::Command;
use switchyard_core::update::{
    AUTO_DOWNLOAD_SETTING, CHECK_SETTING, CURRENT_VERSION, Install, ReadyUpdate, UpdateInfo,
    UpdateStatus,
};

use crate::appearance::{rpx, ts};
use crate::theme::Palette;
use crate::ui::{self, Kind};
use crate::workspace::Workspace;

/// Where the log files are (set once at startup).
pub struct LogDir(pub PathBuf);
impl Global for LogDir {}

/// What the updater is doing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    /// Nothing checked yet.
    #[default]
    Idle,
    /// Asking GitHub.
    Checking,
    /// The running version is the latest.
    UpToDate,
    /// A newer release; `asset` says whether this copy can install it itself.
    Available(UpdateInfo),
    /// Downloading and verifying it.
    Downloading {
        /// The release.
        info: UpdateInfo,
        /// Bytes so far.
        done: u64,
        /// Expected size (0 when unknown).
        total: u64,
    },
    /// Verified and staged; installs on restart.
    Ready(ReadyUpdate),
    /// A failed check, or a failed download of `info`.
    Failed {
        /// The release whose download failed (`None`: the check failed).
        info: Option<UpdateInfo>,
        /// What went wrong.
        error: String,
    },
}

/// Update state the workspace keeps.
pub struct Updates {
    /// Check at startup (setting `updates.check`, on unless turned off).
    pub enabled: bool,
    /// Download a newer release by itself (setting `updates.auto_download`, on by default).
    pub auto_download: bool,
    /// Current state.
    pub phase: Phase,
    /// The status-bar notice was closed for this version.
    dismissed: Option<String>,
    /// Which of the two settings have arrived (bit 0: check, bit 1: auto download).
    loaded: u8,
    /// The startup check was sent.
    auto_sent: bool,
}

impl Default for Updates {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_download: true,
            phase: Phase::Idle,
            dismissed: None,
            loaded: 0,
            auto_sent: false,
        }
    }
}

/// The update to start once the app has quit.
static AFTER_EXIT: Mutex<Option<ReadyUpdate>> = Mutex::new(None);

/// Start what "Restart now" staged: the replaced AppImage or the Windows installer. Called
/// by `main` after the event loop returns; does nothing otherwise.
pub fn run_after_exit() {
    let ready = AFTER_EXIT.lock().unwrap_or_else(|p| p.into_inner()).take();
    if let Some(ready) = ready {
        switchyard_core::update::run_after_exit(&ready);
    }
}

/// Startup checks run in release builds only (debug builds would query GitHub on every
/// launch) and never when `SWITCHYARD_NO_UPDATE_CHECK` is set (tests, screenshots).
fn startup_check_allowed() -> bool {
    !cfg!(debug_assertions) && std::env::var_os("SWITCHYARD_NO_UPDATE_CHECK").is_none()
}

/// Ask the store for both settings; the answers go to [`Workspace::on_update_setting`].
pub fn load_setting(core: &switchyard_core::RuntimeHandle) {
    for key in [CHECK_SETTING, AUTO_DOWNLOAD_SETTING] {
        core.send(Command::LoadSetting { key: key.into() });
    }
}

/// Whether `key` is one of the update settings.
pub fn is_setting(key: &str) -> bool {
    key == CHECK_SETTING || key == AUTO_DOWNLOAD_SETTING
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / 1_000_000.0)
}

impl Workspace {
    /// An update setting arrived; once both have, start the startup check unless it is off.
    pub(crate) fn on_update_setting(&mut self, key: &str, value: Option<serde_json::Value>) {
        let on = value.and_then(|v| v.as_bool()).unwrap_or(true);
        if key == CHECK_SETTING {
            self.updates.enabled = on;
            self.updates.loaded |= 1;
        } else {
            self.updates.auto_download = on;
            self.updates.loaded |= 2;
        }
        if self.updates.loaded == 3
            && self.updates.enabled
            && !self.updates.auto_sent
            && startup_check_allowed()
        {
            self.updates.auto_sent = true;
            self.updates.phase = Phase::Checking;
            self.core.send(Command::CheckForUpdates {
                manual: false,
                download: self.updates.auto_download,
            });
        }
    }

    /// A check finished. Startup checks stay quiet unless there is something new.
    pub(crate) fn on_update_status(
        &mut self,
        manual: bool,
        status: UpdateStatus,
        cx: &mut Context<Self>,
    ) {
        // A download started by an earlier check keeps its state.
        if matches!(
            self.updates.phase,
            Phase::Downloading { .. } | Phase::Ready(_)
        ) {
            return;
        }
        match status {
            UpdateStatus::Available(info) => {
                if manual {
                    self.toast(format!("Switchyard {} is available", info.version), cx);
                }
                self.updates.phase = Phase::Available(info);
            }
            UpdateStatus::UpToDate { current } => {
                if manual {
                    self.toast(format!("Switchyard {current} is up to date"), cx);
                }
                self.updates.phase = Phase::UpToDate;
            }
            UpdateStatus::Failed(error) => {
                if manual {
                    self.toast(format!("Update check failed: {error}"), cx);
                }
                self.updates.phase = Phase::Failed { info: None, error };
            }
        }
        cx.notify();
    }

    /// Download progress.
    pub(crate) fn on_update_progress(
        &mut self,
        version: String,
        done: u64,
        total: u64,
        cx: &mut Context<Self>,
    ) {
        let info = match &self.updates.phase {
            Phase::Available(info) | Phase::Downloading { info, .. } if info.version == version => {
                info.clone()
            }
            Phase::Failed {
                info: Some(info), ..
            } if info.version == version => info.clone(),
            _ => return,
        };
        self.updates.phase = Phase::Downloading { info, done, total };
        cx.notify();
    }

    /// A download finished, failed or was cancelled.
    pub(crate) fn on_update_downloaded(
        &mut self,
        info: UpdateInfo,
        result: Result<ReadyUpdate, String>,
        cancelled: bool,
        cx: &mut Context<Self>,
    ) {
        self.updates.phase = match result {
            Ok(ready) => {
                let what = if matches!(ready.install, Install::MacApp(_)) {
                    "open the disk image to install"
                } else {
                    "restart to install"
                };
                self.toast(
                    format!("Switchyard {} is ready · {what}", ready.version),
                    cx,
                );
                Phase::Ready(ready)
            }
            Err(_) if cancelled => Phase::Available(info),
            Err(error) => {
                self.toast(format!("Update download failed: {error}"), cx);
                Phase::Failed {
                    info: Some(info),
                    error,
                }
            }
        };
        cx.notify();
    }

    /// "Check for Updates": checks, and downloads when automatic downloads are on.
    pub(crate) fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        match &self.updates.phase {
            Phase::Checking => return,
            Phase::Downloading { info, .. } => {
                self.toast(format!("Downloading Switchyard {}…", info.version), cx);
                return;
            }
            Phase::Ready(r) => {
                self.toast(format!("Switchyard {} is ready to install", r.version), cx);
                return;
            }
            _ => {}
        }
        self.updates.phase = Phase::Checking;
        self.core.send(Command::CheckForUpdates {
            manual: true,
            download: self.updates.auto_download,
        });
        self.toast("Checking for updates…", cx);
        cx.notify();
    }

    /// Download the available release (or retry a failed download).
    pub(crate) fn download_update(&mut self, cx: &mut Context<Self>) {
        let info = match &self.updates.phase {
            Phase::Available(info)
            | Phase::Failed {
                info: Some(info), ..
            } if info.asset.is_some() => info.clone(),
            _ => return,
        };
        self.updates.phase = Phase::Downloading {
            info: info.clone(),
            done: 0,
            total: info.asset.as_ref().map_or(0, |a| a.size),
        };
        self.core.send(Command::DownloadUpdate { info });
        cx.notify();
    }

    /// Stop a running download.
    pub(crate) fn cancel_update_download(&mut self) {
        self.core.send(Command::CancelUpdateDownload);
    }

    /// Finish a staged update: quit (asking about unsaved files as usual) and start the new
    /// version or its installer, or open the macOS disk image.
    pub(crate) fn apply_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Phase::Ready(ready) = &self.updates.phase else {
            return;
        };
        if matches!(ready.install, Install::MacApp(_)) {
            // Finder mounts the image; dragging Switchyard to Applications replaces this copy.
            cx.open_with_system(&ready.file);
            return;
        }
        *AFTER_EXIT.lock().unwrap_or_else(|p| p.into_inner()) = Some(ready.clone());
        if self.request_close_window(window, cx) {
            window.remove_window();
        }
    }

    /// Settings → General: turn startup checks on or off.
    pub(crate) fn set_update_checks(&mut self, on: bool, cx: &mut Context<Self>) {
        self.updates.enabled = on;
        self.core.send(Command::SetSetting {
            key: CHECK_SETTING.into(),
            value: on.into(),
        });
        cx.notify();
    }

    /// Settings → General: download newer releases by themselves, or only announce them.
    pub(crate) fn set_update_auto_download(&mut self, on: bool, cx: &mut Context<Self>) {
        self.updates.auto_download = on;
        self.core.send(Command::SetSetting {
            key: AUTO_DOWNLOAD_SETTING.into(),
            value: on.into(),
        });
        cx.notify();
    }

    /// The release page of the update in play, if any.
    fn update_release_url(&self) -> Option<String> {
        match &self.updates.phase {
            Phase::Available(info)
            | Phase::Downloading { info, .. }
            | Phase::Failed {
                info: Some(info), ..
            } => Some(info.release_url.clone()),
            Phase::Ready(r) => Some(r.release_url.clone()),
            _ => None,
        }
    }

    /// "Open Log Folder".
    pub(crate) fn open_log_folder(&mut self, cx: &mut Context<Self>) {
        match cx.try_global::<LogDir>().map(|d| d.0.clone()) {
            Some(dir) => cx.open_with_system(&dir),
            None => self.toast("No log folder", cx),
        }
    }

    /// The status-bar notice while an update is available, downloading or ready.
    pub(crate) fn render_update_notice(
        &self,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (version, label): (&str, SharedString) = match &self.updates.phase {
            Phase::Available(info) if info.asset.is_some() => (
                &info.version,
                format!("Download Switchyard {}", info.version).into(),
            ),
            Phase::Available(info) => (
                &info.version,
                format!("Update available: {}", info.version).into(),
            ),
            Phase::Downloading { info, done, total } => (
                &info.version,
                if *total > 0 {
                    format!(
                        "Downloading {} · {}%",
                        info.version,
                        done.saturating_mul(100) / total
                    )
                } else {
                    format!("Downloading {}…", info.version)
                }
                .into(),
            ),
            Phase::Ready(r) if matches!(r.install, Install::MacApp(_)) => (
                &r.version,
                format!("Install Switchyard {}", r.version).into(),
            ),
            Phase::Ready(r) => (
                &r.version,
                format!("Restart to update to {}", r.version).into(),
            ),
            Phase::Failed {
                info: Some(info), ..
            } => (
                &info.version,
                format!("Update {} failed · Retry", info.version).into(),
            ),
            _ => return None,
        };
        if self.updates.dismissed.as_deref() == Some(version) {
            return None;
        }
        let downloading = matches!(self.updates.phase, Phase::Downloading { .. });
        Some(
            div()
                .flex()
                .items_center()
                .gap(rpx(6.))
                .child(
                    div()
                        .id("sb-update")
                        .flex()
                        .items_center()
                        .gap(rpx(5.))
                        .text_color(p.acc)
                        .when(!downloading, |d| d.hover(|s| s.underline()))
                        .on_click(
                            cx.listener(|this, _, window, cx| match &this.updates.phase {
                                Phase::Available(info) if info.asset.is_none() => {
                                    cx.open_url(&info.release_url)
                                }
                                Phase::Available(_) | Phase::Failed { .. } => {
                                    this.download_update(cx)
                                }
                                Phase::Ready(_) => this.apply_update(window, cx),
                                _ => {}
                            }),
                        )
                        .child(crate::ui::dot(p.acc, 6.))
                        .child(label),
                )
                .child(
                    div()
                        .id("sb-update-dismiss")
                        .text_color(p.fg3)
                        .hover(|s| s.text_color(p.fg))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if matches!(this.updates.phase, Phase::Downloading { .. }) {
                                this.cancel_update_download();
                            } else {
                                this.updates.dismissed = match &this.updates.phase {
                                    Phase::Available(i) | Phase::Failed { info: Some(i), .. } => {
                                        Some(i.version.clone())
                                    }
                                    Phase::Ready(r) => Some(r.version.clone()),
                                    _ => None,
                                };
                            }
                            cx.notify();
                        }))
                        .child("×"),
                )
                .into_any_element(),
        )
    }

    /// The Updates block of Settings → General.
    pub(crate) fn render_update_settings(&self, p: &Palette, cx: &mut Context<Self>) -> Div {
        let line: String = match &self.updates.phase {
            Phase::Idle => format!("Switchyard {CURRENT_VERSION}"),
            Phase::Checking => "Checking GitHub for a newer release…".into(),
            Phase::UpToDate => format!("Switchyard {CURRENT_VERSION} is up to date"),
            Phase::Available(info) if info.asset.is_some() => {
                format!("Switchyard {} is available", info.version)
            }
            Phase::Available(info) => format!(
                "Switchyard {} is available · this copy wasn't installed from a release \
                 package, so download it from the release page",
                info.version
            ),
            Phase::Downloading { info, done, total } => format!(
                "Downloading Switchyard {} · {} of {} MB",
                info.version,
                megabytes(*done),
                megabytes(*total)
            ),
            Phase::Ready(r) => match r.install {
                Install::AppImage(_) => {
                    format!("Switchyard {} is installed · restart to use it", r.version)
                }
                Install::MacApp(_) => format!(
                    "Switchyard {} is downloaded and verified · open the disk image and drag \
                     Switchyard to Applications",
                    r.version
                ),
                _ => format!(
                    "Switchyard {} is downloaded and verified · its installer runs when \
                     Switchyard restarts",
                    r.version
                ),
            },
            Phase::Failed { info: None, error } => format!("Update check failed: {error}"),
            Phase::Failed { error, .. } => format!("Update download failed: {error}"),
        };
        let failed = matches!(self.updates.phase, Phase::Failed { .. });
        let mut actions = div().flex().flex_wrap().items_center().gap(rpx(8.));
        actions = match &self.updates.phase {
            Phase::Available(info)
            | Phase::Failed {
                info: Some(info), ..
            } if info.asset.is_some() => actions.child(
                ui::button("set-update-download", "Download", Kind::Primary, p)
                    .on_click(cx.listener(|this, _, _, cx| this.download_update(cx))),
            ),
            Phase::Downloading { .. } => actions.child(
                ui::button("set-update-cancel", "Cancel", Kind::Secondary, p).on_click(
                    cx.listener(|this, _, _, cx| {
                        this.cancel_update_download();
                        cx.notify();
                    }),
                ),
            ),
            Phase::Ready(r) => actions.child(
                ui::button(
                    "set-update-apply",
                    if matches!(r.install, Install::MacApp(_)) {
                        "Open disk image"
                    } else {
                        "Restart now"
                    },
                    Kind::Primary,
                    p,
                )
                .on_click(cx.listener(|this, _, window, cx| this.apply_update(window, cx))),
            ),
            Phase::Checking => actions,
            _ => actions.child(
                ui::button("set-update-now", "Check now", Kind::Ghost, p)
                    .on_click(cx.listener(|this, _, _, cx| this.check_for_updates(cx))),
            ),
        };
        if let Some(url) = self.update_release_url() {
            actions = actions.child(
                ui::button("set-update-notes", "Release notes", Kind::Ghost, p)
                    .on_click(move |_, _, cx| cx.open_url(&url)),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap(rpx(8.))
            .child(
                div()
                    .text_size(ts::BODY)
                    .text_color(if failed { p.prod } else { p.fg })
                    .child(line),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(rpx(16.))
                    .child(
                        ui::checkbox(
                            "set-update-check",
                            self.updates.enabled,
                            "Check for updates at startup (GitHub Releases)",
                            p,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            let on = !this.updates.enabled;
                            this.set_update_checks(on, cx)
                        })),
                    )
                    .child(
                        ui::checkbox(
                            "set-update-auto",
                            self.updates.auto_download,
                            "Download and verify updates automatically",
                            p,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            let on = !this.updates.auto_download;
                            this.set_update_auto_download(on, cx)
                        })),
                    ),
            )
            .child(actions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_settings_are_recognized() {
        assert!(is_setting("updates.check"));
        assert!(is_setting("updates.auto_download"));
        assert!(!is_setting("updates.other"));
    }

    #[test]
    fn megabytes_show_one_decimal() {
        assert_eq!(megabytes(0), "0.0");
        assert_eq!(megabytes(12_345_678), "12.3");
    }
}
