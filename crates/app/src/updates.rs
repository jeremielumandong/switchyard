//! Update notice, "Check for Updates" and "Open Log Folder" (M6-4). The check itself runs on
//! the core runtime ([`Command::CheckForUpdates`]); this module only keeps the result and
//! draws the status-bar notice.

use std::path::PathBuf;

use gpui_kit::{
    AnyElement, Context, Global, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, div, px,
};
use switchyard_core::Command;
use switchyard_core::update::{CHECK_SETTING, UpdateInfo, UpdateStatus};

use crate::theme::Palette;
use crate::workspace::Workspace;

/// Where the log files are (set once at startup).
pub struct LogDir(pub PathBuf);
impl Global for LogDir {}

/// Update state the workspace keeps.
pub struct Updates {
    /// Check at startup (setting `updates.check`, on unless turned off).
    pub enabled: bool,
    /// A newer release, until dismissed.
    pub available: Option<UpdateInfo>,
    /// The startup check was sent.
    auto_sent: bool,
}

impl Default for Updates {
    fn default() -> Self {
        Self {
            enabled: true,
            available: None,
            auto_sent: false,
        }
    }
}

/// Startup checks run in release builds only (debug builds would query GitHub on every
/// launch) and never when `SWITCHYARD_NO_UPDATE_CHECK` is set (tests, screenshots).
fn startup_check_allowed() -> bool {
    !cfg!(debug_assertions) && std::env::var_os("SWITCHYARD_NO_UPDATE_CHECK").is_none()
}

/// Ask the store for the setting; the answer goes to [`Workspace::on_update_setting`].
pub fn load_setting(core: &switchyard_core::RuntimeHandle) {
    core.send(Command::LoadSetting {
        key: CHECK_SETTING.into(),
    });
}

impl Workspace {
    /// The `updates.check` setting arrived: start the startup check unless it is off.
    pub(crate) fn on_update_setting(&mut self, value: Option<serde_json::Value>) {
        self.updates.enabled = value.and_then(|v| v.as_bool()).unwrap_or(true);
        if self.updates.enabled && !self.updates.auto_sent && startup_check_allowed() {
            self.updates.auto_sent = true;
            self.core.send(Command::CheckForUpdates { manual: false });
        }
    }

    /// A check finished. Startup checks stay quiet unless there is something new.
    pub(crate) fn on_update_status(
        &mut self,
        manual: bool,
        status: UpdateStatus,
        cx: &mut Context<Self>,
    ) {
        match status {
            UpdateStatus::Available(info) => {
                if manual {
                    self.toast(format!("Switchyard {} is available", info.version), cx);
                }
                self.updates.available = Some(info);
            }
            UpdateStatus::UpToDate { current } if manual => {
                self.toast(format!("Switchyard {current} is up to date"), cx);
            }
            UpdateStatus::Failed(e) if manual => {
                self.toast(format!("Update check failed: {e}"), cx);
            }
            UpdateStatus::UpToDate { .. } | UpdateStatus::Failed(_) => {}
        }
        cx.notify();
    }

    /// "Check for Updates".
    pub(crate) fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        self.core.send(Command::CheckForUpdates { manual: true });
        self.toast("Checking for updates…", cx);
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

    /// Open the verified installer if one was downloaded, otherwise the release page.
    fn open_update(&mut self, cx: &mut Context<Self>) {
        let Some(info) = &self.updates.available else {
            return;
        };
        match &info.installer {
            Some(path) => cx.open_with_system(path),
            None => cx.open_url(&info.release_url),
        }
    }

    /// "Open Log Folder".
    pub(crate) fn open_log_folder(&mut self, cx: &mut Context<Self>) {
        match cx.try_global::<LogDir>().map(|d| d.0.clone()) {
            Some(dir) => cx.open_with_system(&dir),
            None => self.toast("No log folder", cx),
        }
    }

    /// The status-bar notice while a newer release is known.
    pub(crate) fn render_update_notice(
        &self,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let info = self.updates.available.as_ref()?;
        let label: SharedString = if info.installer.is_some() {
            format!("Install Switchyard {}", info.version).into()
        } else {
            format!("Update available: {}", info.version).into()
        };
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(
                    div()
                        .id("sb-update")
                        .flex()
                        .items_center()
                        .gap(px(5.))
                        .text_color(p.acc)
                        .hover(|s| s.underline())
                        .on_click(cx.listener(|this, _, _, cx| this.open_update(cx)))
                        .child(crate::ui::dot(p.acc, 6.))
                        .child(label),
                )
                .child(
                    div()
                        .id("sb-update-dismiss")
                        .text_color(p.fg3)
                        .hover(|s| s.text_color(p.fg))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.updates.available = None;
                            cx.notify();
                        }))
                        .child("×"),
                )
                .into_any_element(),
        )
    }
}
