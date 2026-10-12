//! Dialogs of server workspaces, shared credentials and remote desktops: name (new,
//! rename, duplicate), delete with its optional "also delete" list, the settings sheet
//! (members, add picker, tag and folder rules), export with optional encrypted secrets,
//! import preview with per-item choices, the missing-secrets list, "Add to workspace"
//! for the palette, the Credentials manager and the remote desktop editor.

use std::collections::HashMap;
use std::path::PathBuf;

use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, ClickEvent, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, PathPromptOptions, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, deferred, div, px, uniform_list,
};
use secrecy::SecretString;
use switchyard_core::servers::{ImportPreviewInfo, MissingSecret, ServersCommand};
use switchyard_core::store::workspace_file::{FILE_SUFFIX, ImportChoice};
use switchyard_core::store::{
    ALL_ID, Credential, CredentialKind, DesktopDisplay, DesktopProtocol, DesktopScreen,
    DesktopServer, EnvironmentLabel, Profile, ProfileId, RdpGateway, tags,
};
use switchyard_core::{Command, RequestId};

use crate::actions::fuzzy_score;
use crate::app_state::{badge_of, describe, next_id};
use crate::appearance::{rpx, ts};
use crate::overlays::{Overlay, scrim};
use crate::server_workspaces::{Pending, WsTarget, scope_profiles, scope_with_rule};
use crate::theme::Palette;
use crate::ui::{self, Kind};
use crate::workspace::Workspace;

/// Shortest passphrase accepted for an export with secrets.
pub(crate) const MIN_PASSPHRASE: usize = 8;

/// What a name dialog does.
#[derive(Clone, Debug)]
pub(crate) enum NameMode {
    /// Create a workspace, optionally holding a menu's target.
    New(Option<WsTarget>),
    /// Rename a workspace.
    Rename(String),
    /// Copy a workspace.
    Duplicate(String),
}

/// New / rename / duplicate.
pub(crate) struct NameDialog {
    pub mode: NameMode,
    pub input: Entity<InputState>,
    pub error: Option<String>,
}

/// Delete confirmation.
pub(crate) struct DeleteDialog {
    pub id: String,
    pub name: String,
    /// Items in no other workspace (`None` while the runtime is asked).
    pub exclusive: Option<Vec<(ProfileId, String, String)>>,
    /// "Also delete the N hosts and connections …".
    pub also: bool,
}

/// The workspace settings sheet.
pub(crate) struct SettingsSheet {
    pub id: String,
    /// The "Add" picker is open.
    pub picker: bool,
    pub search: Entity<InputState>,
    pub tag: Entity<InputState>,
    pub folder: Entity<InputState>,
}

/// Export options.
pub(crate) struct ExportDialog {
    pub id: String,
    pub include: bool,
    pub pass: Entity<InputState>,
    pub pass2: Entity<InputState>,
    pub error: Option<String>,
}

/// Import preview.
pub(crate) struct ImportDialog {
    pub path: PathBuf,
    pub info: ImportPreviewInfo,
    pub choices: HashMap<ProfileId, ImportChoice>,
    pub pass: Entity<InputState>,
    pub error: Option<String>,
    pub busy: bool,
}

/// Secrets the imported items still need.
pub(crate) struct MissingDialog {
    pub rows: Vec<(MissingSecret, Entity<InputState>)>,
}

/// "Add to workspace" from the palette.
pub(crate) struct PickDialog {
    pub target: WsTarget,
}

/// The Credentials manager.
pub(crate) struct CredentialsDialog {
    /// The credential in the form (`None`: a new one).
    pub selected: Option<ProfileId>,
    pub name: Entity<InputState>,
    pub username: Entity<InputState>,
    pub domain: Entity<InputState>,
    pub key_path: Entity<InputState>,
    pub secret: Entity<InputState>,
    pub note: Entity<InputState>,
    pub ssh_key: bool,
    pub error: Option<String>,
    pub request: Option<RequestId>,
    /// Delete asked: the items using it.
    pub confirm_delete: Option<Vec<(ProfileId, String)>>,
}

/// Which select of the desktop editor is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DesktopSelect {
    Credential,
    Via,
    GatewayCredential,
}

/// The remote desktop editor.
pub(crate) struct DesktopDialog {
    pub existing: Option<DesktopServer>,
    pub protocol: DesktopProtocol,
    pub name: Entity<InputState>,
    pub address: Entity<InputState>,
    pub port: Entity<InputState>,
    pub user: Entity<InputState>,
    pub domain: Entity<InputState>,
    pub password: Entity<InputState>,
    pub folder: Entity<InputState>,
    pub tags: Entity<InputState>,
    pub gw_host: Entity<InputState>,
    pub gw_user: Entity<InputState>,
    pub width: Entity<InputState>,
    pub height: Entity<InputState>,
    pub credential: Option<ProfileId>,
    pub via: Option<ProfileId>,
    pub gw_credential: Option<ProfileId>,
    pub env: EnvironmentLabel,
    pub full_screen: bool,
    pub multi_monitor: bool,
    pub clipboard: bool,
    pub drives: bool,
    pub admin: bool,
    pub open_select: Option<DesktopSelect>,
    /// Workspaces: (id, name, at open, now).
    pub workspaces: Vec<(String, String, bool, bool)>,
    pub error: Option<String>,
    pub request: Option<RequestId>,
    pub connect_after: bool,
}

/// A dialog of this module.
pub(crate) enum ServersOverlay {
    Name(NameDialog),
    Delete(DeleteDialog),
    Settings(SettingsSheet),
    Export(ExportDialog),
    Import(ImportDialog),
    Missing(MissingDialog),
    Pick(PickDialog),
    Credentials(CredentialsDialog),
    Desktop(Box<DesktopDialog>),
}

/// The workspace file name for a workspace name (`Prod EU` → `prod-eu.switchyard-workspace.json`).
pub(crate) fn export_file_name(name: &str) -> String {
    let mut slug = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    let slug = if slug.is_empty() { "workspace" } else { slug };
    format!("{slug}{FILE_SUFFIX}")
}

/// Why a new name is refused (empty, too long, or taken by another workspace).
pub(crate) fn name_problem(
    name: &str,
    taken: &[(String, String)],
    except: Option<&str>,
) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return Some("Name is required".into());
    }
    if name.chars().count() > switchyard_core::store::workspaces::MAX_NAME_LEN {
        return Some("Name is too long".into());
    }
    taken
        .iter()
        .filter(|(id, _)| Some(id.as_str()) != except)
        .any(|(_, n)| n.trim().eq_ignore_ascii_case(name))
        .then(|| format!("A workspace named “{name}” exists"))
}

/// Why export passphrases are refused.
pub(crate) fn passphrase_problem(pass: &str, again: &str) -> Option<&'static str> {
    if pass.chars().count() < MIN_PASSPHRASE {
        Some("Use at least 8 characters")
    } else if pass != again {
        Some("The passphrases differ")
    } else {
        None
    }
}

fn dialog_box(p: &Palette, width: f32) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id("ws-dialog")
        .w(rpx(width))
        .max_h(rpx(640.))
        .flex()
        .flex_col()
        .gap(rpx(12.))
        .p(rpx(20.))
        .bg(p.elev)
        .rounded(px(10.))
        .shadow(ui::shadow(p))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

fn title(text: impl Into<SharedString>) -> gpui_kit::Div {
    div()
        .text_size(ts::TITLE_PLUS)
        .font_weight(FontWeight::SEMIBOLD)
        .child(text.into())
}

fn note(text: impl Into<SharedString>, p: &Palette) -> gpui_kit::Div {
    div()
        .text_size(ts::BODY)
        .text_color(p.fg2)
        .child(text.into())
}

fn error_box(text: impl Into<SharedString>, p: &Palette) -> gpui_kit::Div {
    div()
        .px(rpx(10.))
        .py(rpx(6.))
        .rounded(px(6.))
        .bg(p.prod_bg)
        .text_size(ts::BODY)
        .text_color(p.fg)
        .child(text.into())
}

fn input_box(label: &str, e: &Entity<InputState>, p: &Palette) -> gpui_kit::Div {
    div()
        .flex()
        .flex_col()
        .gap(rpx(4.))
        .flex_1()
        .min_w_0()
        .child(
            div()
                .text_size(ts::SMALL)
                .text_color(p.fg3)
                .child(label.to_owned()),
        )
        .child(
            div()
                .h(rpx(28.))
                .flex()
                .items_center()
                .px(rpx(8.))
                .border_1()
                .border_color(p.bd2)
                .rounded(px(6.))
                .bg(p.bg)
                .text_size(ts::BODY)
                .child(Input::new(e).appearance(false).text_size(ts::BODY)),
        )
}

fn row() -> gpui_kit::Div {
    div().flex().gap(rpx(10.)).items_end()
}

fn buttons() -> gpui_kit::Div {
    div().flex().justify_end().gap(rpx(8.)).pt(rpx(4.))
}

fn list_row(id: impl Into<gpui_kit::ElementId>, p: &Palette) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id.into())
        .h(rpx(30.))
        .flex()
        .items_center()
        .gap(rpx(8.))
        .px(rpx(8.))
        .rounded(px(5.))
        .text_size(ts::UI)
        .hover(|s| s.bg(p.hover))
}

fn value(e: &Entity<InputState>, cx: &gpui_kit::App) -> String {
    e.read(cx).value().trim().to_owned()
}

impl Workspace {
    fn servers_overlay(&mut self) -> Option<&mut ServersOverlay> {
        match &mut self.overlay {
            Some(Overlay::Servers(o)) => Some(o),
            _ => None,
        }
    }

    fn show_servers_overlay(
        &mut self,
        o: ServersOverlay,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mode_menu = false;
        self.overlay = Some(Overlay::Servers(o));
        window.focus(&self.overlay_focus, cx);
        cx.notify();
    }

    // ------------------------------------------------------------ opening

    /// "New workspace…" (with a menu's items, or empty).
    pub(crate) fn open_new_workspace(
        &mut self,
        target: Option<WsTarget>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = self.text_input("", "Production servers", false, window, cx);
        input.update(cx, |i, cx| i.focus(window, cx));
        self.show_servers_overlay(
            ServersOverlay::Name(NameDialog {
                mode: NameMode::New(target),
                input,
                error: None,
            }),
            window,
            cx,
        );
    }

    /// Rename or duplicate workspace `id`.
    pub(crate) fn open_name_dialog(
        &mut self,
        mode: NameMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let start = match &mode {
            NameMode::Rename(id) => self.servers.ws(id).map(|w| w.name.clone()),
            NameMode::Duplicate(id) => self.servers.ws(id).map(|w| format!("{} copy", w.name)),
            NameMode::New(_) => None,
        }
        .unwrap_or_default();
        if matches!(&mode, NameMode::Rename(id) if id == ALL_ID) {
            self.toast("“All” cannot be renamed", cx);
            return;
        }
        let input = self.text_input(&start, "Name", false, window, cx);
        input.update(cx, |i, cx| i.focus(window, cx));
        self.show_servers_overlay(
            ServersOverlay::Name(NameDialog {
                mode,
                input,
                error: None,
            }),
            window,
            cx,
        );
    }

    /// Ask before deleting workspace `id`.
    pub(crate) fn open_delete_workspace(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if id == ALL_ID {
            self.toast("“All” cannot be deleted", cx);
            return;
        }
        let Some(name) = self.servers.ws(id).map(|w| w.name.clone()) else {
            return;
        };
        self.core
            .send(Command::Servers(ServersCommand::PreviewDelete {
                id: id.to_owned(),
            }));
        self.show_servers_overlay(
            ServersOverlay::Delete(DeleteDialog {
                id: id.to_owned(),
                name,
                exclusive: None,
                also: false,
            }),
            window,
            cx,
        );
    }

    pub(crate) fn on_delete_preview(
        &mut self,
        id: &str,
        exclusive: Vec<(ProfileId, String, String)>,
        cx: &mut Context<Self>,
    ) {
        if let Some(ServersOverlay::Delete(d)) = self.servers_overlay()
            && d.id == id
        {
            d.exclusive = Some(exclusive);
        }
        cx.notify();
    }

    /// The settings sheet of workspace `id`.
    pub(crate) fn open_workspace_settings(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let search = self.text_input("", "Search everything in All", false, window, cx);
        let tag = self.text_input("", "role:web", false, window, cx);
        let folder = self.text_input("", "Production", false, window, cx);
        let sub = cx.subscribe(
            &search,
            |_, _, ev: &gpui_kit::component::input::InputEvent, cx| {
                if let gpui_kit::component::input::InputEvent::Change = ev {
                    cx.notify();
                }
            },
        );
        self._subs.push(sub);
        self.show_servers_overlay(
            ServersOverlay::Settings(SettingsSheet {
                id: id.to_owned(),
                picker: false,
                search,
                tag,
                folder,
            }),
            window,
            cx,
        );
    }

    /// Export options of workspace `id`.
    pub(crate) fn open_export_workspace(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pass = self.text_input("", "Passphrase", true, window, cx);
        let pass2 = self.text_input("", "Repeat it", true, window, cx);
        self.show_servers_overlay(
            ServersOverlay::Export(ExportDialog {
                id: id.to_owned(),
                include: false,
                pass,
                pass2,
                error: None,
            }),
            window,
            cx,
        );
    }

    /// Pick a workspace file, then preview it.
    pub(crate) fn import_workspace(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        });
        let core = self.core.clone();
        cx.spawn(async move |_, _| {
            if let Ok(Ok(Some(mut paths))) = rx.await
                && let Some(path) = paths.pop()
            {
                core.send(Command::Servers(ServersCommand::PreviewImport { path }));
            }
        })
        .detach();
    }

    pub(crate) fn on_import_preview(
        &mut self,
        path: PathBuf,
        result: Result<ImportPreviewInfo, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(info) => {
                let pass = self.text_input(
                    "",
                    "Passphrase (empty: import without secrets)",
                    true,
                    window,
                    cx,
                );
                self.show_servers_overlay(
                    ServersOverlay::Import(ImportDialog {
                        path,
                        info,
                        choices: HashMap::new(),
                        pass,
                        error: None,
                        busy: false,
                    }),
                    window,
                    cx,
                );
            }
            Err(e) => self.toast(format!("Not a workspace file: {e}"), cx),
        }
    }

    pub(crate) fn on_import_failed(
        &mut self,
        message: String,
        wrong: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(ServersOverlay::Import(d)) = self.servers_overlay() {
            d.busy = false;
            d.error = Some(if wrong {
                "Wrong passphrase · try again, or leave it empty to import without secrets".into()
            } else {
                message
            });
            cx.notify();
        } else {
            self.toast(message, cx);
        }
    }

    pub(crate) fn open_missing_secrets(
        &mut self,
        missing: Vec<MissingSecret>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rows = missing
            .into_iter()
            .map(|m| {
                let ph = format!("{} for {}", m.purpose, m.name);
                let i = self.text_input("", &ph, true, window, cx);
                (m, i)
            })
            .collect();
        self.show_servers_overlay(ServersOverlay::Missing(MissingDialog { rows }), window, cx);
    }

    /// "Add to Workspace…" from the palette.
    pub(crate) fn open_pick_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.palette_ws_target(cx) {
            Some(target) => {
                self.show_servers_overlay(ServersOverlay::Pick(PickDialog { target }), window, cx)
            }
            None => self.toast(
                "Pick items in the Explorer or Servers view (Ctrl/⌘-click), or open one first",
                cx,
            ),
        }
    }

    /// The Credentials manager, on `selected` (or the first credential).
    pub(crate) fn open_credentials(
        &mut self,
        selected: Option<ProfileId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let first = selected.or_else(|| {
            self.profiles.all.iter().find_map(|p| match p {
                Profile::Credential(c) => Some(c.id.clone()),
                _ => None,
            })
        });
        let d = self.credential_form(first, window, cx);
        self.show_servers_overlay(ServersOverlay::Credentials(d), window, cx);
    }

    fn credential_form(
        &mut self,
        id: Option<ProfileId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> CredentialsDialog {
        let c = id
            .as_ref()
            .and_then(|id| {
                self.profiles.all.iter().find_map(|p| match p {
                    Profile::Credential(c) if &c.id == id => Some(c.clone()),
                    _ => None,
                })
            })
            .unwrap_or_else(|| Credential::new("", ""));
        let key = match &c.auth {
            CredentialKind::SshKey { key_path } => key_path.clone(),
            CredentialKind::Password => String::new(),
        };
        let secret_ph = if c.secret.is_some() {
            "Stored · type to change (rotates it everywhere)"
        } else {
            "Password or key passphrase"
        };
        CredentialsDialog {
            selected: id,
            name: self.text_input(&c.name, "ops-admin", false, window, cx),
            username: self.text_input(&c.username, "administrator", false, window, cx),
            domain: self.text_input(
                c.domain.as_deref().unwrap_or(""),
                "CORP (optional)",
                false,
                window,
                cx,
            ),
            key_path: self.text_input(&key, "~/.ssh/id_ed25519", false, window, cx),
            secret: self.text_input("", secret_ph, true, window, cx),
            note: self.text_input(
                c.note.as_deref().unwrap_or(""),
                "Owner, rotation schedule (no secrets)",
                false,
                window,
                cx,
            ),
            ssh_key: matches!(c.auth, CredentialKind::SshKey { .. }),
            error: None,
            request: None,
            confirm_delete: None,
        }
    }

    pub(crate) fn on_credential_users(
        &mut self,
        id: &ProfileId,
        users: Vec<(ProfileId, String)>,
        cx: &mut Context<Self>,
    ) {
        if let Some(ServersOverlay::Credentials(d)) = self.servers_overlay()
            && d.selected.as_ref() == Some(id)
            && d.confirm_delete.is_some()
        {
            d.confirm_delete = Some(users);
            cx.notify();
        }
    }

    /// The remote desktop editor: a new server of `protocol`, or `existing`.
    pub(crate) fn open_desktop_editor(
        &mut self,
        protocol: DesktopProtocol,
        existing: Option<DesktopServer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let d = existing
            .clone()
            .unwrap_or_else(|| DesktopServer::new("", protocol));
        let gw = d.gateway.clone().unwrap_or_default();
        let (w, h) = match d.display.screen {
            DesktopScreen::Window { width, height } => (width.to_string(), height.to_string()),
            DesktopScreen::FullScreen => ("1600".into(), "900".into()),
        };
        let pw_ph = match (&d.secret, d.protocol) {
            (Some(_), _) => "Stored · type to change",
            (None, DesktopProtocol::Vnc) => "VNC password (optional)",
            (None, DesktopProtocol::Rdp) => "Optional · the client can ask",
        };
        let workspaces = self
            .workspace_choices(existing.clone().map(Profile::Desktop).as_ref())
            .into_iter()
            .map(|(id, name, on)| (id, name, on, on))
            .collect();
        let dialog = DesktopDialog {
            protocol: d.protocol,
            name: self.text_input(&d.name, "dc01", false, window, cx),
            address: self.text_input(&d.address, "dc01.corp.example", false, window, cx),
            port: self.text_input(&d.port.to_string(), "", false, window, cx),
            user: self.text_input(&d.user, "administrator", false, window, cx),
            domain: self.text_input(d.domain.as_deref().unwrap_or(""), "CORP", false, window, cx),
            password: self.text_input("", pw_ph, true, window, cx),
            folder: self.text_input(d.folder.as_deref().unwrap_or(""), "None", false, window, cx),
            tags: self.text_input(
                &tags::format_tags(&d.tags),
                "windows, role:dc",
                false,
                window,
                cx,
            ),
            gw_host: self.text_input(&gw.host, "None", false, window, cx),
            gw_user: self.text_input(&gw.user, "Same as the server", false, window, cx),
            width: self.text_input(&w, "", false, window, cx),
            height: self.text_input(&h, "", false, window, cx),
            credential: d.credential.clone(),
            via: d.via_host.clone(),
            gw_credential: gw.credential.clone(),
            env: d.environment,
            full_screen: matches!(d.display.screen, DesktopScreen::FullScreen),
            multi_monitor: d.display.multi_monitor,
            clipboard: d.display.clipboard,
            drives: d.display.drives,
            admin: d.admin_session,
            open_select: None,
            workspaces,
            error: None,
            request: None,
            connect_after: existing.is_none(),
            existing,
        };
        self.show_servers_overlay(ServersOverlay::Desktop(Box::new(dialog)), window, cx);
    }

    /// A profile was saved: close the dialog that asked, and apply its workspace boxes.
    pub(crate) fn on_servers_profile_saved(
        &mut self,
        request: RequestId,
        id: &ProfileId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(Pending::Membership(changes)) = self.servers.pending.remove(&request) {
            self.apply_membership(id, changes);
        }
        let mut launch = None;
        let mut credential_saved = false;
        match self.servers_overlay() {
            Some(ServersOverlay::Desktop(d)) if d.request == Some(request) => {
                if d.connect_after {
                    launch = Some(id.clone());
                }
                self.overlay = None;
            }
            Some(ServersOverlay::Credentials(d)) if d.request == Some(request) => {
                // Stay on it; the list refreshes with the profiles.
                d.selected = Some(id.clone());
                d.request = None;
                d.error = None;
                let secret = d.secret.clone();
                secret.update(cx, |i, cx| i.set_value("", window, cx));
                credential_saved = true;
            }
            _ => {}
        }
        if credential_saved {
            self.toast(
                "Credential saved · every item using it signs in with it",
                cx,
            );
        }
        if let Some(id) = launch {
            self.toast("Saved", cx);
            self.launch_desktop(&id, cx);
        }
        cx.notify();
    }

    /// A save failed: show it in the dialog that asked.
    pub(crate) fn on_servers_profile_error(
        &mut self,
        request: RequestId,
        message: &str,
        cx: &mut Context<Self>,
    ) {
        match self.servers_overlay() {
            Some(ServersOverlay::Desktop(d)) if d.request == Some(request) => {
                d.request = None;
                d.error = Some(message.to_owned());
            }
            Some(ServersOverlay::Credentials(d)) if d.request == Some(request) => {
                d.request = None;
                d.error = Some(message.to_owned());
            }
            _ => return,
        }
        self.servers.pending.remove(&request);
        cx.notify();
    }

    // ------------------------------------------------------------ actions

    fn confirm_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ServersOverlay::Name(d)) = self.servers_overlay() else {
            return;
        };
        let name = value(&d.input, cx);
        let mode = d.mode.clone();
        let taken: Vec<(String, String)> = self
            .servers
            .workspaces
            .iter()
            .map(|w| (w.id.clone(), w.name.clone()))
            .collect();
        let except = match &mode {
            NameMode::Rename(id) => Some(id.as_str()),
            _ => None,
        };
        if let Some(problem) = name_problem(&name, &taken, except) {
            if let Some(ServersOverlay::Name(d)) = self.servers_overlay() {
                d.error = Some(problem);
            }
            cx.notify();
            return;
        }
        match mode {
            NameMode::New(target) => {
                let request = next_id();
                let scope = target.as_ref().map(WsTarget::scope).unwrap_or_default();
                self.servers.pending.insert(
                    request,
                    Pending::NewWorkspace {
                        switch: target.is_none(),
                    },
                );
                self.core.send(Command::Servers(ServersCommand::Create {
                    request,
                    name,
                    scope,
                }));
            }
            NameMode::Rename(id) => {
                if let Some(w) = self.servers.workspaces.iter_mut().find(|w| w.id == id) {
                    w.name = name.clone();
                }
                if id == self.servers.current {
                    self.workspace_name = name.clone();
                }
                self.core
                    .send(Command::Servers(ServersCommand::Rename { id, name }));
            }
            NameMode::Duplicate(id) => {
                let request = next_id();
                self.servers
                    .pending
                    .insert(request, Pending::NewWorkspace { switch: true });
                self.core.send(Command::Servers(ServersCommand::Duplicate {
                    request,
                    id,
                    name,
                }));
            }
        }
        self.dismiss(window, cx);
    }

    fn confirm_delete_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ServersOverlay::Delete(d)) = self.servers_overlay() else {
            return;
        };
        let (id, also) = (d.id.clone(), d.also);
        self.core.send(Command::Servers(ServersCommand::Delete {
            id,
            delete_profiles: also,
        }));
        self.dismiss(window, cx);
    }

    fn confirm_export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ServersOverlay::Export(d)) = self.servers_overlay() else {
            return;
        };
        let id = d.id.clone();
        let passphrase = if d.include {
            let (a, b) = (
                d.pass.read(cx).value().to_string(),
                d.pass2.read(cx).value().to_string(),
            );
            if let Some(problem) = passphrase_problem(&a, &b) {
                if let Some(ServersOverlay::Export(d)) = self.servers_overlay() {
                    d.error = Some(problem.into());
                }
                cx.notify();
                return;
            }
            Some(SecretString::from(a))
        } else {
            None
        };
        let name = self
            .servers
            .ws(&id)
            .map(|w| w.name.clone())
            .unwrap_or_default();
        let dir = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let rx = cx.prompt_for_new_path(&dir, Some(&export_file_name(&name)));
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                core.send(Command::Servers(ServersCommand::Export {
                    id,
                    path,
                    passphrase,
                }));
            } else {
                let _ = this.update(cx, |w, cx| w.toast("Export cancelled", cx));
            }
        })
        .detach();
        self.dismiss(window, cx);
    }

    fn confirm_import(&mut self, cx: &mut Context<Self>) {
        let Some(ServersOverlay::Import(d)) = self.servers_overlay() else {
            return;
        };
        let pass = d.pass.read(cx).value().to_string();
        let passphrase = (d.info.encrypted && !pass.is_empty()).then(|| SecretString::from(pass));
        let choices = d
            .info
            .items
            .iter()
            .filter(|i| i.existing.is_some())
            .map(|i| {
                (
                    i.id.clone(),
                    d.choices.get(&i.id).copied().unwrap_or_default(),
                )
            })
            .collect();
        let path = d.path.clone();
        d.busy = true;
        d.error = None;
        self.core.send(Command::Servers(ServersCommand::Import {
            path,
            passphrase,
            choices,
        }));
        cx.notify();
    }

    fn save_missing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ServersOverlay::Missing(d)) = self.servers_overlay() else {
            return;
        };
        let filled: Vec<(ProfileId, String)> = d
            .rows
            .iter()
            .map(|(m, i)| (m.id.clone(), i.read(cx).value().to_string()))
            .filter(|(_, v)| !v.is_empty())
            .collect();
        let mut saved = 0;
        for (id, v) in filled {
            if let Some(p) = self.profiles.all.iter().find(|p| p.id() == &id).cloned() {
                self.core.send(Command::SaveProfile {
                    request: next_id(),
                    profile: p,
                    secret: Some(SecretString::from(v)),
                });
                saved += 1;
            }
        }
        if saved > 0 {
            self.toast(
                format!("Stored {saved} secret{}", if saved == 1 { "" } else { "s" }),
                cx,
            );
        }
        self.dismiss(window, cx);
    }

    fn save_credential(&mut self, cx: &mut Context<Self>) {
        let Some(ServersOverlay::Credentials(d)) = self.servers_overlay() else {
            return;
        };
        let existing = d.selected.clone();
        let mut c = existing
            .as_ref()
            .and_then(|id| {
                self.profiles.all.iter().find_map(|p| match p {
                    Profile::Credential(c) if &c.id == id => Some(c.clone()),
                    _ => None,
                })
            })
            .unwrap_or_else(|| Credential::new("", ""));
        let Some(ServersOverlay::Credentials(d)) = self.servers_overlay() else {
            return;
        };
        c.name = value(&d.name, cx);
        c.username = value(&d.username, cx);
        c.domain = Some(value(&d.domain, cx)).filter(|s| !s.is_empty());
        c.note = Some(value(&d.note, cx)).filter(|s| !s.is_empty());
        c.auth = if d.ssh_key {
            CredentialKind::SshKey {
                key_path: value(&d.key_path, cx),
            }
        } else {
            CredentialKind::Password
        };
        let pw = d.secret.read(cx).value().to_string();
        let profile = Profile::Credential(c);
        if let Err(v) = profile.validate() {
            d.error = Some(v.message);
            cx.notify();
            return;
        }
        let request = next_id();
        d.request = Some(request);
        d.error = None;
        self.core.send(Command::SaveProfile {
            request,
            profile,
            secret: (!pw.is_empty()).then(|| SecretString::from(pw)),
        });
        cx.notify();
    }

    fn delete_credential(&mut self, cx: &mut Context<Self>) {
        let Some(ServersOverlay::Credentials(d)) = self.servers_overlay() else {
            return;
        };
        let Some(id) = d.selected.clone() else {
            return;
        };
        match d.confirm_delete.take() {
            None => {
                let users: Vec<(ProfileId, String)> = self
                    .profiles
                    .all
                    .iter()
                    .filter(|p| p.uses_credential(&id))
                    .map(|p| (p.id().clone(), p.name().to_owned()))
                    .collect();
                if let Some(ServersOverlay::Credentials(d)) = self.servers_overlay() {
                    d.confirm_delete = Some(users);
                }
                self.core
                    .send(Command::Servers(ServersCommand::CredentialUsers { id }));
            }
            Some(users) => {
                self.core
                    .send(Command::Servers(ServersCommand::DeleteCredential {
                        id,
                        detach: !users.is_empty(),
                    }));
                if let Some(ServersOverlay::Credentials(d)) = self.servers_overlay() {
                    d.selected = None;
                }
            }
        }
        cx.notify();
    }

    fn save_desktop(&mut self, cx: &mut Context<Self>) {
        let Some(ServersOverlay::Desktop(d)) = self.servers_overlay() else {
            return;
        };
        let mut s = d
            .existing
            .clone()
            .unwrap_or_else(|| DesktopServer::new("", d.protocol));
        s.protocol = d.protocol;
        s.name = value(&d.name, cx);
        s.address = value(&d.address, cx);
        let port = value(&d.port, cx);
        s.port = if port.is_empty() {
            d.protocol.default_port()
        } else {
            match port.parse::<u16>() {
                Ok(p) if p > 0 => p,
                _ => {
                    d.error = Some("Port must be 1–65535".into());
                    cx.notify();
                    return;
                }
            }
        };
        s.user = value(&d.user, cx);
        s.domain = Some(value(&d.domain, cx))
            .filter(|x| !x.is_empty() && d.protocol == DesktopProtocol::Rdp);
        s.credential = d.credential.clone();
        s.via_host = d.via.clone();
        let gw_host = value(&d.gw_host, cx);
        s.gateway =
            (d.protocol == DesktopProtocol::Rdp && !gw_host.is_empty()).then(|| RdpGateway {
                host: gw_host,
                user: value(&d.gw_user, cx),
                credential: d.gw_credential.clone(),
            });
        let screen = if d.full_screen {
            DesktopScreen::FullScreen
        } else {
            let (w, h) = (
                value(&d.width, cx).parse::<u16>().unwrap_or(0),
                value(&d.height, cx).parse::<u16>().unwrap_or(0),
            );
            DesktopScreen::Window {
                width: w,
                height: h,
            }
        };
        s.display = DesktopDisplay {
            screen,
            multi_monitor: d.multi_monitor && d.protocol == DesktopProtocol::Rdp,
            clipboard: d.clipboard,
            drives: d.drives && d.protocol == DesktopProtocol::Rdp,
        };
        s.admin_session = d.admin && d.protocol == DesktopProtocol::Rdp;
        s.environment = d.env;
        s.folder = Some(value(&d.folder, cx)).filter(|x| !x.is_empty());
        s.tags = tags::parse_tags(&value(&d.tags, cx));
        let pw = d.password.read(cx).value().to_string();
        let changes: Vec<(String, bool)> = d
            .workspaces
            .iter()
            .filter(|(_, _, before, now)| before != now)
            .map(|(id, _, _, now)| (id.clone(), *now))
            .collect();
        let profile = Profile::Desktop(s);
        if let Err(v) = profile.validate() {
            d.error = Some(v.message);
            cx.notify();
            return;
        }
        let request = next_id();
        d.request = Some(request);
        d.error = None;
        if !changes.is_empty() {
            self.servers
                .pending
                .insert(request, Pending::Membership(changes));
        }
        self.core.send(Command::SaveProfile {
            request,
            profile,
            secret: (!pw.is_empty()).then(|| SecretString::from(pw)),
        });
        cx.notify();
    }

    // ------------------------------------------------------------ rendering

    /// The open dialog of this module.
    pub(crate) fn render_servers_overlay(
        &mut self,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let body = match &self.overlay {
            Some(Overlay::Servers(ServersOverlay::Name(d))) => self.render_name_dialog(d, p, cx),
            Some(Overlay::Servers(ServersOverlay::Delete(d))) => {
                self.render_delete_dialog(d, p, cx)
            }
            Some(Overlay::Servers(ServersOverlay::Settings(d))) => {
                self.render_settings_sheet(d, p, window, cx)
            }
            Some(Overlay::Servers(ServersOverlay::Export(d))) => {
                self.render_export_dialog(d, p, cx)
            }
            Some(Overlay::Servers(ServersOverlay::Import(d))) => {
                self.render_import_dialog(d, p, cx)
            }
            Some(Overlay::Servers(ServersOverlay::Missing(d))) => {
                self.render_missing_dialog(d, p, cx)
            }
            Some(Overlay::Servers(ServersOverlay::Pick(d))) => self.render_pick_dialog(d, p, cx),
            Some(Overlay::Servers(ServersOverlay::Credentials(d))) => {
                self.render_credentials(d, p, cx)
            }
            Some(Overlay::Servers(ServersOverlay::Desktop(d))) => {
                self.render_desktop_editor(d, p, cx)
            }
            _ => div().into_any_element(),
        };
        scrim(p, false)
            .key_context("Overlay")
            .track_focus(&self.overlay_focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, w, cx| this.dismiss(w, cx)),
            )
            .child(body)
            .into_any_element()
    }

    fn render_name_dialog(
        &self,
        d: &NameDialog,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (heading, action, sub) = match &d.mode {
            NameMode::New(None) => (
                "New workspace".to_owned(),
                "Create",
                "An empty workspace. Add Hosts and connections from their menus (Add to workspace), or tag and folder rules in its settings.".to_owned(),
            ),
            NameMode::New(Some(t)) => (
                "New workspace".to_owned(),
                "Create",
                format!("Starts with {}.", t.describe()),
            ),
            NameMode::Rename(id) => (
                format!(
                    "Rename “{}”",
                    self.servers.ws(id).map(|w| w.name.as_str()).unwrap_or("")
                ),
                "Rename",
                String::new(),
            ),
            NameMode::Duplicate(id) => (
                format!(
                    "Duplicate “{}”",
                    self.servers.ws(id).map(|w| w.name.as_str()).unwrap_or("")
                ),
                "Duplicate",
                "The copy has the same items, rules, layout and SQL editors.".to_owned(),
            ),
        };
        dialog_box(p, 440.)
            .on_key_down(cx.listener(|this, ev: &gpui_kit::KeyDownEvent, w, cx| {
                if ev.keystroke.key == "enter" {
                    this.confirm_name(w, cx);
                }
            }))
            .child(title(heading))
            .when(!sub.is_empty(), |d| d.child(note(sub, p)))
            .child(input_box("Name", &d.input, p))
            .when_some(d.error.clone(), |d, e| d.child(error_box(e, p)))
            .child(
                buttons()
                    .child(
                        ui::button("ws-name-cancel", "Cancel", Kind::Secondary, p)
                            .on_click(cx.listener(|this, _, w, cx| this.dismiss(w, cx))),
                    )
                    .child(
                        ui::button("ws-name-ok", action, Kind::Primary, p)
                            .on_click(cx.listener(|this, _, w, cx| this.confirm_name(w, cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_delete_dialog(
        &self,
        d: &DeleteDialog,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let also = d.also;
        let extra: AnyElement = match &d.exclusive {
            None => note("Checking which items are in no other workspace…", p).into_any_element(),
            Some(list) if list.is_empty() => note(
                "Every item here is also in another workspace; nothing else would be deleted.",
                p,
            )
            .into_any_element(),
            Some(list) => {
                let n = list.len();
                div()
                    .flex()
                    .flex_col()
                    .gap(rpx(8.))
                    .child(
                        ui::checkbox(
                            "ws-del-also",
                            also,
                            format!(
                                "Also delete the {n} host{} and connection{} that are in no other workspace",
                                if n == 1 { "" } else { "s" },
                                if n == 1 { "" } else { "s" }
                            ),
                            p,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(ServersOverlay::Delete(d)) = this.servers_overlay() {
                                d.also = !d.also;
                            }
                            cx.notify();
                        })),
                    )
                    .when(also, |d| {
                        d.child(
                            div()
                                .id("ws-del-list")
                                .max_h(rpx(200.))
                                .overflow_y_scroll()
                                .flex()
                                .flex_col()
                                .gap(rpx(2.))
                                .pl(rpx(22.))
                                .children(list.iter().map(|(_, name, kind)| {
                                    div()
                                        .flex()
                                        .gap(rpx(8.))
                                        .items_center()
                                        .text_size(ts::BODY)
                                        .child(ui::mono(kind.clone(), ts::CAPTION).text_color(p.fg3))
                                        .child(name.clone())
                                })),
                        )
                        .child(note(
                            "They are deleted for good, with their stored passwords and keys.",
                            p,
                        ))
                    })
                    .into_any_element()
            }
        };
        dialog_box(p, 500.)
            .child(title(format!("Delete workspace “{}”?", d.name)))
            .child(note(
                "This removes the workspace: its scope, tabs and layout. Its hosts and connections stay and still show in All.",
                p,
            ))
            .child(extra)
            .child(
                buttons()
                    .child(
                        ui::button("ws-del-cancel", "Cancel", Kind::Secondary, p)
                            .on_click(cx.listener(|this, _, w, cx| this.dismiss(w, cx))),
                    )
                    .child(
                        ui::button(
                            "ws-del-ok",
                            if also { "Delete workspace and items" } else { "Delete workspace" },
                            Kind::Destructive,
                            p,
                        )
                        .on_click(cx.listener(|this, _, w, cx| this.confirm_delete_workspace(w, cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_settings_sheet(
        &self,
        d: &SettingsSheet,
        p: &Palette,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(ws) = self.servers.ws(&d.id).cloned() else {
            return dialog_box(p, 420.)
                .child(note("This workspace no longer exists.", p))
                .into_any_element();
        };
        let id = ws.id.clone();
        let is_all = id == ALL_ID;
        let p = *p;
        let header = div()
            .flex()
            .items_center()
            .gap(rpx(8.))
            .child(
                div()
                    .flex_1()
                    .child(title(format!("Workspace “{}”", ws.name))),
            )
            .when(!is_all, |d| {
                let (a, b, c, e) = (id.clone(), id.clone(), id.clone(), id.clone());
                d.child(
                    ui::button("ws-set-rename", "Rename…", Kind::Ghost, &p).on_click(cx.listener(
                        move |this, _, w, cx| {
                            this.open_name_dialog(NameMode::Rename(a.clone()), w, cx)
                        },
                    )),
                )
                .child(
                    ui::button("ws-set-dup", "Duplicate…", Kind::Ghost, &p).on_click(cx.listener(
                        move |this, _, w, cx| {
                            this.open_name_dialog(NameMode::Duplicate(b.clone()), w, cx)
                        },
                    )),
                )
                .child(
                    ui::button("ws-set-export", "Export…", Kind::Ghost, &p).on_click(
                        cx.listener(move |this, _, w, cx| this.open_export_workspace(&c, w, cx)),
                    ),
                )
                .child(
                    ui::button("ws-set-delete", "Delete…", Kind::Ghost, &p).on_click(
                        cx.listener(move |this, _, w, cx| this.open_delete_workspace(&e, w, cx)),
                    ),
                )
            });
        if is_all {
            let dup = id.clone();
            let exp = id.clone();
            return dialog_box(&p, 560.)
                .child(header)
                .child(note(
                    "“All” shows every Host, connection and remote desktop. It has no members or rules, and cannot be renamed or deleted. Duplicate it to start a workspace with today's items.",
                    &p,
                ))
                .child(
                    buttons()
                        .child(
                            ui::button("ws-all-dup", "Duplicate…", Kind::Secondary, &p).on_click(
                                cx.listener(move |this, _, w, cx| {
                                    this.open_name_dialog(NameMode::Duplicate(dup.clone()), w, cx)
                                }),
                            ),
                        )
                        .child(
                            ui::button("ws-all-export", "Export…", Kind::Secondary, &p).on_click(
                                cx.listener(move |this, _, w, cx| {
                                    this.open_export_workspace(&exp, w, cx)
                                }),
                            ),
                        )
                        .child(
                            ui::button("ws-all-close", "Close", Kind::Primary, &p)
                                .on_click(cx.listener(|this, _, w, cx| this.dismiss(w, cx))),
                        ),
                )
                .into_any_element();
        }
        // Members: what the workspace matches itself (not what it only pulls in).
        let members: Vec<Profile> = self
            .profiles
            .all
            .iter()
            .filter(|p| p.is_connection() && ws.scope.matches(p))
            .cloned()
            .collect();
        let excluded: Vec<Profile> = self
            .profiles
            .all
            .iter()
            .filter(|p| ws.scope.excluded.contains(p.id()))
            .cloned()
            .collect();
        let member_rows = members.clone();
        let mid = id.clone();
        let members_list = if member_rows.is_empty() {
            note(
                "No items yet. Add some below, or add a tag or folder rule.",
                &p,
            )
            .into_any_element()
        } else {
            let n = member_rows.len();
            uniform_list(
                "ws-members",
                n,
                cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                    range
                        .map(|i| {
                            let m = &member_rows[i];
                            let pid = m.id().clone();
                            let ws = mid.clone();
                            let rule = !this
                                .servers
                                .ws(&ws)
                                .is_some_and(|w| w.scope.members.contains(m.id()));
                            list_row(("ws-member", i), &p)
                                .child(ui::monogram(badge_of(m), 30., &p))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .child(m.name().to_owned()),
                                )
                                .child(
                                    div().text_size(ts::SMALL).text_color(p.fg3).child(if rule {
                                        "by rule"
                                    } else {
                                        "member"
                                    }),
                                )
                                .child(
                                    ui::button(("ws-member-rm", i), "Remove", Kind::Ghost, &p)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.core.send(Command::Servers(
                                                ServersCommand::Remove {
                                                    id: ws.clone(),
                                                    profiles: vec![pid.clone()],
                                                },
                                            ));
                                            cx.notify();
                                        })),
                                )
                                .into_any_element()
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .h(rpx((n.min(7) as f32) * 30.))
            .into_any_element()
        };
        // The picker: everything in All not shown here yet.
        let picker =
            d.picker.then(|| {
                let q = d.search.read(cx).value().trim().to_owned();
                let shown = scope_profiles(&self.profiles.all, &ws.scope);
                let mut cands: Vec<(usize, Profile)> = self
                    .profiles
                    .all
                    .iter()
                    .filter(|x| x.is_connection() && !shown.iter().any(|s| s.id() == x.id()))
                    .filter_map(|x| {
                        let hay = format!(
                            "{} {} {}",
                            x.name(),
                            describe(x, &self.profiles),
                            x.tags().join(" ")
                        );
                        fuzzy_score(&q, &hay).map(|s| (s, x.clone()))
                    })
                    .collect();
                cands.sort_by_key(|(s, _)| *s);
                let cands: Vec<Profile> = cands.into_iter().map(|(_, x)| x).collect();
                let n = cands.len();
                let pid = id.clone();
                let profiles = self.profiles.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap(rpx(6.))
                    .p(rpx(8.))
                    .border_1()
                    .border_color(p.bd)
                    .rounded(px(7.))
                    .child(input_box("Add from All", &d.search, &p))
                    .child(if n == 0 {
                        note("Nothing else matches.", &p).into_any_element()
                    } else {
                        uniform_list(
                            "ws-picker",
                            n,
                            cx.processor(move |_, range: std::ops::Range<usize>, _, cx| {
                                range
                                    .map(|i| {
                                        let c = &cands[i];
                                        let cid = c.id().clone();
                                        let ws = pid.clone();
                                        list_row(("ws-pick", i), &p)
                                            .child(ui::monogram(badge_of(c), 30., &p))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .truncate()
                                                    .child(c.name().to_owned()),
                                            )
                                            .child(
                                                div()
                                                    .text_size(ts::SMALL)
                                                    .text_color(p.fg3)
                                                    .truncate()
                                                    .child(describe(c, &profiles)),
                                            )
                                            .child(
                                                ui::button(
                                                    ("ws-pick-add", i),
                                                    "Add",
                                                    Kind::Secondary,
                                                    &p,
                                                )
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.core.send(Command::Servers(
                                                        ServersCommand::Add {
                                                            id: ws.clone(),
                                                            profiles: vec![cid.clone()],
                                                        },
                                                    ));
                                                    cx.notify();
                                                })),
                                            )
                                            .into_any_element()
                                    })
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .h(rpx((n.min(6) as f32) * 30.))
                        .into_any_element()
                    })
            });
        let chip = |id: SharedString, label: String, on_remove: ui::OnClick| {
            div()
                .flex()
                .items_center()
                .gap(rpx(4.))
                .pl(rpx(8.))
                .pr(rpx(4.))
                .h(rpx(22.))
                .border_1()
                .border_color(p.bd2)
                .rounded(px(11.))
                .text_size(ts::LABEL)
                .child(label)
                .child(
                    div()
                        .id(id)
                        .px(rpx(4.))
                        .text_color(p.fg3)
                        .hover(|s| s.text_color(p.fg))
                        .on_click(on_remove)
                        .child("×"),
                )
        };
        let rule_chips =
            |kind: &'static str, list: &[String], cx: &mut Context<Self>| -> Vec<AnyElement> {
                list.iter()
                    .map(|v| {
                        let target = if kind == "tag" {
                            WsTarget::Tag(v.clone())
                        } else {
                            WsTarget::Folder(v.clone())
                        };
                        let ws = id.clone();
                        let label = if kind == "tag" {
                            format!("# {v}")
                        } else {
                            format!("▣ {v}")
                        };
                        chip(
                            SharedString::from(format!("ws-rule-{kind}-{v}")),
                            label,
                            Box::new(cx.listener(move |this, _, _, cx| {
                                if let Some(scope) = this.servers.ws(&ws).map(|w| w.scope.clone()) {
                                    let scope =
                                        scope_with_rule(&scope, &target, false, &this.profiles.all);
                                    this.core.send(Command::Servers(ServersCommand::SetScope {
                                        id: ws.clone(),
                                        scope,
                                    }));
                                }
                                cx.notify();
                            })),
                        )
                        .into_any_element()
                    })
                    .collect()
            };
        let tag_chips = rule_chips("tag", &ws.scope.tags, cx);
        let folder_chips = rule_chips("folder", &ws.scope.folders, cx);
        let add_rule = |kind: &'static str, cx: &mut Context<Self>| -> ui::OnClick {
            let ws = id.clone();
            Box::new(cx.listener(move |this, _: &ClickEvent, w, cx| {
                let Some(ServersOverlay::Settings(d)) = this.servers_overlay() else {
                    return;
                };
                let input = if kind == "tag" {
                    d.tag.clone()
                } else {
                    d.folder.clone()
                };
                let v = input.read(cx).value().trim().to_owned();
                if v.is_empty() {
                    return;
                }
                let target = if kind == "tag" {
                    WsTarget::Tag(v)
                } else {
                    WsTarget::Folder(v)
                };
                if let Some(scope) = this.servers.ws(&ws).map(|w| w.scope.clone()) {
                    let scope = scope_with_rule(&scope, &target, true, &this.profiles.all);
                    this.core.send(Command::Servers(ServersCommand::SetScope {
                        id: ws.clone(),
                        scope,
                    }));
                }
                input.update(cx, |i, cx| i.set_value("", w, cx));
                cx.notify();
            }))
        };
        let excluded_rows: Vec<AnyElement> = excluded
            .iter()
            .enumerate()
            .map(|(i, x)| {
                let ws = id.clone();
                let xid = x.id().clone();
                list_row(("ws-excl", i), &p)
                    .child(ui::monogram(badge_of(x), 30., &p))
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .text_color(p.fg2)
                            .child(x.name().to_owned()),
                    )
                    .child(
                        ui::button(("ws-excl-in", i), "Include again", Kind::Ghost, &p).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.core.send(Command::Servers(ServersCommand::Add {
                                    id: ws.clone(),
                                    profiles: vec![xid.clone()],
                                }));
                                cx.notify();
                            }),
                        ),
                    )
                    .into_any_element()
            })
            .collect();
        let picker_open = d.picker;
        dialog_box(&p, 620.)
            .child(header)
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .child(ui::caption(format!("ITEMS · {}", members.len()), &p)),
                    )
                    .child(
                        ui::button(
                            "ws-set-add",
                            if picker_open { "Done adding" } else { "Add…" },
                            Kind::Secondary,
                            &p,
                        )
                        .on_click(cx.listener(|this, _, w, cx| {
                            if let Some(ServersOverlay::Settings(d)) = this.servers_overlay() {
                                d.picker = !d.picker;
                                let s = d.search.clone();
                                if d.picker {
                                    s.update(cx, |i, cx| i.focus(w, cx));
                                }
                            }
                            cx.notify();
                        })),
                    ),
            )
            .children(picker)
            .child(members_list)
            .when(!excluded_rows.is_empty(), |d| {
                d.child(ui::caption("REMOVED (A RULE STILL MATCHES THEM)", &p))
                    .children(excluded_rows)
            })
            .child(ui::caption("TAG RULES · items with any of these tags", &p))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(rpx(6.))
                    .items_center()
                    .children(tag_chips)
                    .child(div().w(rpx(160.)).child(input_box("", &d.tag, &p)))
                    .child(
                        ui::button("ws-add-tag", "Add tag rule", Kind::Secondary, &p)
                            .on_click(add_rule("tag", cx)),
                    ),
            )
            .child(ui::caption("FOLDER RULES · items in these folders", &p))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(rpx(6.))
                    .items_center()
                    .children(folder_chips)
                    .child(div().w(rpx(160.)).child(input_box("", &d.folder, &p)))
                    .child(
                        ui::button("ws-add-folder", "Add folder rule", Kind::Secondary, &p)
                            .on_click(add_rule("folder", cx)),
                    ),
            )
            .child(
                buttons().child(
                    ui::button("ws-set-close", "Close", Kind::Primary, &p)
                        .on_click(cx.listener(|this, _, w, cx| this.dismiss(w, cx))),
                ),
            )
            .into_any_element()
    }

    fn render_export_dialog(
        &self,
        d: &ExportDialog,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = self
            .servers
            .ws(&d.id)
            .map(|w| w.name.clone())
            .unwrap_or_default();
        dialog_box(p, 520.)
            .child(title(format!("Export “{name}”")))
            .child(note(
                format!(
                    "Writes {} with its hosts, connections, remote desktops, the credentials they use (as references), tags, folders and rules. Keep it in Git to share the setup.",
                    export_file_name(&name)
                ),
                p,
            ))
            .child(note(
                "Secrets stay out by default: each password or key passphrase is listed as “needs a value” for whoever imports it. Vault references ({{vault.name}}) stay references.",
                p,
            ))
            .child(
                ui::checkbox(
                    "ws-exp-secrets",
                    d.include,
                    "Include secrets, encrypted with a passphrase",
                    p,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(ServersOverlay::Export(d)) = this.servers_overlay() {
                        d.include = !d.include;
                        d.error = None;
                    }
                    cx.notify();
                })),
            )
            .when(d.include, |el| {
                el.child(
                    row()
                        .child(input_box("Passphrase", &d.pass, p))
                        .child(input_box("Repeat", &d.pass2, p)),
                )
                .child(note(
                    "Only the secret values are encrypted (Argon2id + ChaCha20-Poly1305); the rest stays readable for review. Share the passphrase another way.",
                    p,
                ))
            })
            .when_some(d.error.clone(), |el, e| el.child(error_box(e, p)))
            .child(
                buttons()
                    .child(
                        ui::button("ws-exp-cancel", "Cancel", Kind::Secondary, p)
                            .on_click(cx.listener(|this, _, w, cx| this.dismiss(w, cx))),
                    )
                    .child(
                        ui::button("ws-exp-ok", "Export…", Kind::Primary, p)
                            .on_click(cx.listener(|this, _, w, cx| this.confirm_export(w, cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_import_dialog(
        &self,
        d: &ImportDialog,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = *p;
        let items = d.info.items.clone();
        let choices = d.choices.clone();
        let n = items.len();
        let needs = items.iter().filter(|i| i.needs_secret).count();
        let list = uniform_list(
            "ws-import-items",
            n,
            cx.processor(move |_, range: std::ops::Range<usize>, _, cx| {
                range
                    .map(|i| {
                        let it = &items[i];
                        let status = match (&it.existing, it.identical) {
                            (None, _) => "new".to_owned(),
                            (Some(_), true) => "unchanged".to_owned(),
                            (Some(name), false) => format!("updates {name}"),
                        };
                        let secret = if it.secret_included {
                            "secret included"
                        } else if it.needs_secret {
                            "needs a value"
                        } else {
                            ""
                        };
                        let choice = choices.get(&it.id).copied().unwrap_or_default();
                        let conflict = it.existing.is_some() && !it.identical;
                        let opts: Vec<(SharedString, bool, ui::OnClick)> = [
                            (ImportChoice::Skip, "Skip"),
                            (ImportChoice::Replace, "Replace"),
                            (ImportChoice::KeepBoth, "Keep both"),
                        ]
                        .into_iter()
                        .map(|(c, label)| {
                            let id = it.id.clone();
                            let on: ui::OnClick = Box::new(cx.listener(move |this, _, _, cx| {
                                if let Some(ServersOverlay::Import(d)) = this.servers_overlay() {
                                    d.choices.insert(id.clone(), c);
                                }
                                cx.notify();
                            }));
                            (SharedString::from(label), choice == c, on)
                        })
                        .collect();
                        list_row(("ws-imp", i), &p)
                            .h(rpx(32.))
                            .child(
                                ui::mono(it.kind.clone(), ts::CAPTION)
                                    .w(rpx(64.))
                                    .text_color(p.fg3),
                            )
                            .child(div().flex_1().min_w_0().truncate().child(it.name.clone()))
                            .child(
                                div()
                                    .text_size(ts::SMALL)
                                    .text_color(if it.needs_secret { p.stg } else { p.fg3 })
                                    .child(secret),
                            )
                            .child(div().text_size(ts::SMALL).text_color(p.fg3).child(status))
                            .when(conflict, |d| {
                                d.child(ui::segmented(("ws-imp-choice", i), opts, 20., &p))
                            })
                            .into_any_element()
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .h(rpx((n.clamp(1, 9) as f32) * 32.));
        dialog_box(&p, 680.)
            .child(title(format!("Import “{}”", d.info.workspace)))
            .child(note(
                if d.info.workspace_exists {
                    "This workspace exists here: the import updates it. Items keep their ids, so a newer file updates them instead of adding copies."
                } else {
                    "A new workspace. Items keep their ids, so importing a newer file later updates them instead of adding copies."
                },
                &p,
            ))
            .child(list)
            .child(note(
                format!(
                    "{n} item{} · {needs} need{} a secret you can fill in after the import",
                    if n == 1 { "" } else { "s" },
                    if needs == 1 { "s" } else { "" }
                ),
                &p,
            ))
            .when(d.info.encrypted, |el| {
                el.child(note(
                    "The file carries encrypted secrets. Enter its passphrase to import them.",
                    &p,
                ))
                .child(input_box("Passphrase", &d.pass, &p))
            })
            .when_some(d.error.clone(), |el, e| el.child(error_box(e, &p)))
            .child(
                buttons()
                    .child(
                        ui::button("ws-imp-cancel", "Cancel", Kind::Secondary, &p)
                            .on_click(cx.listener(|this, _, w, cx| this.dismiss(w, cx))),
                    )
                    .child(
                        ui::button(
                            "ws-imp-ok",
                            if d.busy { "Importing…" } else { "Import" },
                            Kind::Primary,
                            &p,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.confirm_import(cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_missing_dialog(
        &self,
        d: &MissingDialog,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        dialog_box(p, 520.)
            .child(title("Missing secrets"))
            .child(note(
                "These imported items need a password or key passphrase. Fill in what you have; leave the rest empty and set it later in the item's editor.",
                p,
            ))
            .child(
                div()
                    .id("ws-missing")
                    .max_h(rpx(360.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(rpx(8.))
                    .children(d.rows.iter().map(|(m, input)| {
                        input_box(&format!("{} · {}", m.name, m.purpose), input, p)
                    })),
            )
            .child(
                buttons()
                    .child(
                        ui::button("ws-miss-later", "Later", Kind::Secondary, p)
                            .on_click(cx.listener(|this, _, w, cx| this.dismiss(w, cx))),
                    )
                    .child(
                        ui::button("ws-miss-save", "Store", Kind::Primary, p)
                            .on_click(cx.listener(|this, _, w, cx| this.save_missing(w, cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_pick_dialog(
        &self,
        d: &PickDialog,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let targets: Vec<(String, String)> = self
            .servers
            .workspaces
            .iter()
            .filter(|w| w.id != ALL_ID)
            .map(|w| (w.id.clone(), w.name.clone()))
            .collect();
        let target = d.target.clone();
        dialog_box(p, 420.)
            .child(title(format!("Add {} to workspace", target.describe())))
            .children(targets.into_iter().enumerate().map(|(i, (id, name))| {
                let t = target.clone();
                list_row(("ws-pick-ws", i), p)
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.add_to_workspace(&id, &t, cx);
                        this.dismiss(w, cx);
                    }))
                    .child(div().flex_1().child(name))
            }))
            .child({
                let t = target.clone();
                list_row("ws-pick-new", p)
                    .cursor_pointer()
                    .text_color(p.acc)
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.open_new_workspace(Some(t.clone()), w, cx)
                    }))
                    .child("New workspace…")
            })
            .into_any_element()
    }

    fn render_credentials(
        &self,
        d: &CredentialsDialog,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let creds: Vec<&Credential> = self
            .profiles
            .all
            .iter()
            .filter_map(|p| match p {
                Profile::Credential(c) => Some(c),
                _ => None,
            })
            .collect();
        let users = |id: &ProfileId| -> Vec<String> {
            self.profiles
                .all
                .iter()
                .filter(|p| p.uses_credential(id))
                .map(|p| p.name().to_owned())
                .collect()
        };
        let list = div()
            .id("cred-list")
            .w(rpx(220.))
            .flex_none()
            .flex()
            .flex_col()
            .gap(rpx(2.))
            .max_h(rpx(420.))
            .overflow_y_scroll()
            .children(creds.iter().enumerate().map(|(i, c)| {
                let id = c.id.clone();
                let n = users(&c.id).len();
                let on = d.selected.as_ref() == Some(&c.id);
                list_row(("cred", i), p)
                    .h(rpx(40.))
                    .cursor_pointer()
                    .when(on, |d| d.bg(p.sel))
                    .on_click(cx.listener(move |this, _, w, cx| {
                        let f = this.credential_form(Some(id.clone()), w, cx);
                        this.overlay = Some(Overlay::Servers(ServersOverlay::Credentials(f)));
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .child(div().truncate().child(c.name.clone()))
                            .child(
                                div()
                                    .text_size(ts::SMALL)
                                    .text_color(p.fg3)
                                    .truncate()
                                    .child(format!("{} · used by {n}", c.qualified_user())),
                            ),
                    )
            }))
            .child(
                list_row("cred-new", p)
                    .cursor_pointer()
                    .text_color(p.acc)
                    .on_click(cx.listener(|this, _, w, cx| {
                        let f = this.credential_form(None, w, cx);
                        this.overlay = Some(Overlay::Servers(ServersOverlay::Credentials(f)));
                        cx.notify();
                    }))
                    .child("+ New credential"),
            );
        let used_by = d.selected.as_ref().map(&users).unwrap_or_default();
        let ssh = d.ssh_key;
        let form = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(rpx(10.))
            .child(row().child(input_box("Name", &d.name, p)))
            .child(
                row()
                    .child(input_box("User name", &d.username, p))
                    .child(input_box("Domain", &d.domain, p)),
            )
            .child(ui::segmented(
                "cred-kind",
                vec![
                    (
                        "Password".into(),
                        !ssh,
                        Box::new(cx.listener(|this, _, _, cx| {
                            if let Some(ServersOverlay::Credentials(d)) = this.servers_overlay() {
                                d.ssh_key = false;
                            }
                            cx.notify();
                        })) as ui::OnClick,
                    ),
                    (
                        "SSH key".into(),
                        ssh,
                        Box::new(cx.listener(|this, _, _, cx| {
                            if let Some(ServersOverlay::Credentials(d)) = this.servers_overlay() {
                                d.ssh_key = true;
                            }
                            cx.notify();
                        })) as ui::OnClick,
                    ),
                ],
                22.,
                p,
            ))
            .when(ssh, |el| el.child(row().child(input_box("Key file (read in place)", &d.key_path, p))))
            .child(row().child(input_box(
                if ssh { "Key passphrase" } else { "Password" },
                &d.secret,
                p,
            )))
            .child(row().child(input_box("Note", &d.note, p)))
            .child(note(
                if used_by.is_empty() {
                    "Hosts, database connections and remote desktops can use it instead of their own sign-in. Its secret lives in the keychain or vault."
                        .to_owned()
                } else {
                    format!(
                        "Used by {}: changing the password here updates all of them.",
                        used_by.join(", ")
                    )
                },
                p,
            ))
            .when_some(d.confirm_delete.clone(), |el, users| {
                el.child(error_box(
                    if users.is_empty() {
                        "Nothing uses it. Delete it and its stored secret?".to_owned()
                    } else {
                        format!(
                            "{} use{} it: {}. Deleting detaches them; they keep the user name and need their own password. Delete?",
                            users.len(),
                            if users.len() == 1 { "s" } else { "" },
                            users
                                .iter()
                                .map(|(_, n)| n.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    },
                    p,
                ))
            })
            .when_some(d.error.clone(), |el, e| el.child(error_box(e, p)))
            .child(
                buttons()
                    .when(d.selected.is_some(), |el| {
                        el.child(
                            ui::button(
                                "cred-delete",
                                if d.confirm_delete.is_some() { "Delete" } else { "Delete…" },
                                Kind::Destructive,
                                p,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.delete_credential(cx))),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        ui::button("cred-close", "Close", Kind::Secondary, p)
                            .on_click(cx.listener(|this, _, w, cx| this.dismiss(w, cx))),
                    )
                    .child(
                        ui::button("cred-save", "Save", Kind::Primary, p)
                            .on_click(cx.listener(|this, _, _, cx| this.save_credential(cx))),
                    ),
            );
        dialog_box(p, 760.)
            .child(title("Credentials"))
            .child(note(
                "Shared sign-ins: one user name with a password or SSH key, used by many Hosts, connections and remote desktops. Rotate it once here.",
                p,
            ))
            .child(div().flex().gap(rpx(16.)).child(list).child(form))
            .into_any_element()
    }

    /// A small select of the desktop editor: `options` are (label, value).
    #[allow(clippy::too_many_arguments)]
    fn desktop_select(
        &self,
        which: DesktopSelect,
        label: &str,
        options: Vec<(String, Option<ProfileId>)>,
        chosen: &Option<ProfileId>,
        open: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = options
            .iter()
            .find(|(_, v)| v == chosen)
            .map(|(l, _)| l.clone())
            .unwrap_or_default();
        let key = format!("{which:?}");
        let p = *p;
        div()
            .flex()
            .flex_col()
            .gap(rpx(4.))
            .flex_1()
            .min_w_0()
            .child(
                div()
                    .text_size(ts::SMALL)
                    .text_color(p.fg3)
                    .child(label.to_owned()),
            )
            .child(
                div()
                    .relative()
                    .child(
                        div()
                            .id(SharedString::from(format!("dsel-{key}")))
                            .h(rpx(28.))
                            .flex()
                            .items_center()
                            .px(rpx(8.))
                            .border_1()
                            .border_color(if open { p.acc } else { p.bd2 })
                            .rounded(px(6.))
                            .bg(p.bg)
                            .text_size(ts::BODY)
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(ServersOverlay::Desktop(d)) = this.servers_overlay() {
                                    d.open_select = if d.open_select == Some(which) {
                                        None
                                    } else {
                                        Some(which)
                                    };
                                }
                                cx.notify();
                            }))
                            .child(div().flex_1().truncate().child(current))
                            .child(div().text_color(p.fg3).text_size(ts::CAPTION).child("▾")),
                    )
                    .when(open, |d| {
                        d.child(deferred(
                            div()
                                .id(SharedString::from(format!("dsel-menu-{key}")))
                                .absolute()
                                .top(rpx(30.))
                                .left_0()
                                .right_0()
                                .max_h(rpx(220.))
                                .overflow_y_scroll()
                                .p(rpx(4.))
                                .bg(p.elev)
                                .rounded(px(7.))
                                .shadow(ui::shadow(&p))
                                .occlude()
                                .children(options.into_iter().enumerate().map(|(i, (l, v))| {
                                    div()
                                        .id(SharedString::from(format!("dsel-{key}-{i}")))
                                        .h(rpx(26.))
                                        .flex()
                                        .items_center()
                                        .px(rpx(8.))
                                        .rounded(px(4.))
                                        .text_size(ts::BODY)
                                        .hover(|s| s.bg(p.sel))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if let Some(ServersOverlay::Desktop(d)) =
                                                this.servers_overlay()
                                            {
                                                match which {
                                                    DesktopSelect::Credential => {
                                                        d.credential = v.clone()
                                                    }
                                                    DesktopSelect::Via => d.via = v.clone(),
                                                    DesktopSelect::GatewayCredential => {
                                                        d.gw_credential = v.clone()
                                                    }
                                                }
                                                d.open_select = None;
                                            }
                                            cx.notify();
                                        }))
                                        .child(l)
                                })),
                        ))
                    }),
            )
            .into_any_element()
    }

    fn render_desktop_editor(
        &self,
        d: &DesktopDialog,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rdp = d.protocol == DesktopProtocol::Rdp;
        let creds: Vec<(String, Option<ProfileId>)> =
            std::iter::once(("None · use the fields here".to_owned(), None))
                .chain(self.profiles.all.iter().filter_map(|p| match p {
                    Profile::Credential(c) => Some((
                        format!("{} · {}", c.name, c.qualified_user()),
                        Some(c.id.clone()),
                    )),
                    _ => None,
                }))
                .collect();
        let hosts: Vec<(String, Option<ProfileId>)> =
            std::iter::once(("Direct (no tunnel)".to_owned(), None))
                .chain(
                    self.profiles
                        .hosts()
                        .map(|h| (format!("{} · SSH tunnel", h.name), Some(h.id.clone()))),
                )
                .collect();
        let gw_creds: Vec<(String, Option<ProfileId>)> =
            std::iter::once(("Same sign-in as the server".to_owned(), None))
                .chain(creds.iter().skip(1).cloned())
                .collect();
        let title_text = match (&d.existing, rdp) {
            (Some(e), _) => format!("Edit {}", e.name),
            (None, true) => "New RDP server".into(),
            (None, false) => "New VNC server".into(),
        };
        let toggle = |id: &'static str,
                      on: bool,
                      label: &'static str,
                      f: fn(&mut DesktopDialog),
                      cx: &mut Context<Self>| {
            ui::checkbox(id, on, label, p).on_click(cx.listener(move |this, _, _, cx| {
                if let Some(ServersOverlay::Desktop(d)) = this.servers_overlay() {
                    f(d);
                }
                cx.notify();
            }))
        };
        let proto = |pr: DesktopProtocol, cx: &mut Context<Self>| -> ui::OnClick {
            Box::new(cx.listener(move |this, _, w, cx| {
                if let Some(ServersOverlay::Desktop(d)) = this.servers_overlay()
                    && d.protocol != pr
                {
                    d.protocol = pr;
                    let port = d.port.clone();
                    port.update(cx, |i, cx| {
                        i.set_value(pr.default_port().to_string(), w, cx)
                    });
                }
                cx.notify();
            }))
        };
        let open = d.open_select;
        let ws_boxes: Vec<AnyElement> = d
            .workspaces
            .iter()
            .enumerate()
            .map(|(i, (_, name, _, on))| {
                ui::checkbox(("dws", i), *on, name.clone(), p)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(ServersOverlay::Desktop(d)) = this.servers_overlay()
                            && let Some(w) = d.workspaces.get_mut(i)
                        {
                            w.3 = !w.3;
                        }
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();
        dialog_box(p, 640.)
            .id("desktop-editor")
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(div().flex_1().child(title(title_text)))
                    .when(d.existing.is_none(), |el| {
                        el.child(ui::segmented(
                            "desk-proto",
                            vec![
                                ("RDP".into(), rdp, proto(DesktopProtocol::Rdp, cx)),
                                ("VNC".into(), !rdp, proto(DesktopProtocol::Vnc, cx)),
                            ],
                            22.,
                            p,
                        ))
                    }),
            )
            .child(
                row()
                    .child(input_box("Name", &d.name, p))
                    .child(input_box("Address", &d.address, p))
                    .child(div().w(rpx(90.)).child(input_box("Port", &d.port, p))),
            )
            .child(row().child(self.desktop_select(
                DesktopSelect::Credential,
                "Shared credential",
                creds.clone(),
                &d.credential,
                open == Some(DesktopSelect::Credential),
                p,
                cx,
            )))
            .when(d.credential.is_none(), |el| {
                el.child(
                    row()
                        .when(rdp, |r| {
                            r.child(input_box("User", &d.user, p))
                                .child(input_box("Domain", &d.domain, p))
                        })
                        .child(input_box(if rdp { "Password" } else { "VNC password" }, &d.password, p)),
                )
            })
            .child(row().child(self.desktop_select(
                DesktopSelect::Via,
                "Reach it through",
                hosts,
                &d.via,
                open == Some(DesktopSelect::Via),
                p,
                cx,
            )))
            .when(rdp, |el| {
                el.child(ui::caption("RD GATEWAY (OPTIONAL)", p)).child(
                    row()
                        .child(input_box("Gateway", &d.gw_host, p))
                        .child(input_box("Gateway user", &d.gw_user, p))
                        .child(self.desktop_select(
                            DesktopSelect::GatewayCredential,
                            "Gateway credential",
                            gw_creds,
                            &d.gw_credential,
                            open == Some(DesktopSelect::GatewayCredential),
                            p,
                            cx,
                        )),
                )
            })
            .child(ui::caption("DISPLAY", p))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(rpx(14.))
                    .items_center()
                    .child(toggle("desk-full", d.full_screen, "Full screen", |d| d.full_screen = !d.full_screen, cx))
                    .when(rdp, |el| {
                        el.child(toggle("desk-multi", d.multi_monitor, "All monitors", |d| d.multi_monitor = !d.multi_monitor, cx))
                            .child(toggle("desk-drives", d.drives, "Share local drives", |d| d.drives = !d.drives, cx))
                            .child(toggle("desk-admin", d.admin, "Admin / console session", |d| d.admin = !d.admin, cx))
                    })
                    .child(toggle("desk-clip", d.clipboard, "Shared clipboard", |d| d.clipboard = !d.clipboard, cx)),
            )
            .when(!d.full_screen, |el| {
                el.child(
                    row()
                        .child(div().w(rpx(120.)).child(input_box("Width", &d.width, p)))
                        .child(div().w(rpx(120.)).child(input_box("Height", &d.height, p))),
                )
            })
            .child(
                row()
                    .child(input_box("Folder", &d.folder, p))
                    .child(input_box("Tags", &d.tags, p)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rpx(6.))
                    .text_size(ts::BODY)
                    .child(div().w(rpx(90.)).text_color(p.fg3).child("Environment"))
                    .children(EnvironmentLabel::ALL.iter().map(|&e| {
                        let on = d.env == e;
                        div()
                            .id(SharedString::from(format!("desk-env-{e:?}")))
                            .flex()
                            .items_center()
                            .gap(rpx(5.))
                            .px(rpx(8.))
                            .py(rpx(3.))
                            .border_1()
                            .border_color(if on { p.acc } else { p.bd2 })
                            .rounded(px(5.))
                            .bg(if on { p.sel } else { p.bg })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(ServersOverlay::Desktop(d)) = this.servers_overlay() {
                                    d.env = e;
                                }
                                cx.notify();
                            }))
                            .child(ui::dot(p.env(e), 6.))
                            .child(e.name())
                    })),
            )
            .when(!ws_boxes.is_empty(), |el| {
                el.child(ui::caption("WORKSPACES", p)).child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(rpx(14.))
                        .children(ws_boxes),
                )
            })
            .child(note(
                if rdp {
                    "Opens in the system client: mstsc on Windows, Windows App on macOS, FreeRDP (or Remmina) on Linux. The password goes to FreeRDP on its input, never on the command line; mstsc and Windows App ask for it themselves."
                } else {
                    "Opens in TigerVNC's vncviewer, Remmina, or macOS Screen Sharing."
                },
                p,
            ))
            .when_some(d.error.clone(), |el, e| el.child(error_box(e, p)))
            .child(
                buttons()
                    .child(
                        ui::button("desk-cancel", "Cancel", Kind::Secondary, p)
                            .on_click(cx.listener(|this, _, w, cx| this.dismiss(w, cx))),
                    )
                    .child(
                        ui::button(
                            "desk-save",
                            if d.connect_after { "Save and connect" } else { "Save" },
                            Kind::Primary,
                            p,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.save_desktop(cx))),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_file_names() {
        assert_eq!(
            export_file_name("Prod EU"),
            "prod-eu.switchyard-workspace.json"
        );
        assert_eq!(
            export_file_name("  "),
            "workspace.switchyard-workspace.json"
        );
        assert_eq!(export_file_name("a//b"), "a-b.switchyard-workspace.json");
    }

    #[test]
    fn names_and_passphrases_are_checked() {
        let taken = vec![
            ("all".to_owned(), "All".to_owned()),
            ("p".to_owned(), "Prod".to_owned()),
        ];
        assert!(name_problem(" ", &taken, None).is_some());
        assert!(name_problem("prod", &taken, None).is_some());
        // Renaming a workspace to its own name (other case) is fine.
        assert!(name_problem("PROD", &taken, Some("p")).is_none());
        assert!(name_problem("Lab", &taken, None).is_none());
        assert_eq!(
            passphrase_problem("short", "short"),
            Some("Use at least 8 characters")
        );
        assert_eq!(
            passphrase_problem("long enough", "long enougH"),
            Some("The passphrases differ")
        );
        assert_eq!(passphrase_problem("long enough", "long enough"), None);
    }
}
