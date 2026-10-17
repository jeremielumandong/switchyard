//! SQL script files on this computer: Open SQL Script (a new SQL tab holding the file),
//! Save / Save As, and the Save / Discard question when a tab with unsaved script
//! changes closes. Reads and writes go through the core (`ReadTextFile`,
//! `WriteTextFile`); the UI thread only shows the file dialogs.

use std::path::PathBuf;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::{Context, Entity, IntoElement as _, ParentElement as _, Styled as _, Window, div};
use switchyard_core::store::BufferState;
use switchyard_core::{Command, FsRef, RequestId, TextFile};

use crate::api::compat::dialogs::{self, Dismiss};
use crate::app_state::next_id;
use crate::sql_tab::SqlTab;
use crate::workspace::{Tab, Workspace};

/// Files chosen in Open SQL Script, waiting for their contents.
#[derive(Default)]
pub struct SqlFileOpens(Vec<(RequestId, PathBuf)>);

impl Workspace {
    /// Pick `.sql` files and open each in a new SQL tab.
    pub(crate) fn open_sql_script(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Open".into()),
        });
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let _ = this.update(cx, |w, cx| {
                for path in paths {
                    // Already open: show that tab.
                    if let Some(ix) = w.tabs.iter().position(
                        |t| matches!(t, Tab::Sql(s) if s.read(cx).file.as_ref() == Some(&path)),
                    ) {
                        w.activate(ix, cx);
                        continue;
                    }
                    let request = next_id();
                    w.sql_opens.0.push((request, path.clone()));
                    core.send(Command::ReadTextFile {
                        request,
                        fs: FsRef::Local,
                        path,
                    });
                }
            });
        })
        .detach();
    }

    /// A file read finished: if Open SQL Script asked for it, show it in a new tab.
    /// Returns whether the read was one of these.
    pub(crate) fn on_sql_file_read(
        &mut self,
        request: RequestId,
        result: &Result<TextFile, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(ix) = self.sql_opens.0.iter().position(|(r, _)| *r == request) else {
            return false;
        };
        let (_, path) = self.sql_opens.0.remove(ix);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "script.sql".into());
        let file = match result {
            Ok(f) => f,
            Err(e) => {
                self.toast(format!("Could not open {name}: {e}"), cx);
                return true;
            }
        };
        let conn = self
            .active_sql()
            .and_then(|t| t.read(cx).connection.clone())
            .or_else(|| self.profiles.dbs().find(|d| d.engine.is_sql()).cloned());
        let n = self
            .tabs
            .iter()
            .filter(|t| matches!(t, Tab::Sql(_)))
            .count();
        let buffer = BufferState {
            id: format!("b{}", next_id()),
            title: name,
            connection_id: conn.as_ref().map(|c| c.id.clone()),
            text: file.content.clone(),
            cursor: 0,
            file: Some(path.to_string_lossy().into_owned()),
        };
        let tab = self.new_sql_tab(buffer, conn, n as i64, window, cx);
        let modified = file.modified_ms;
        tab.update(cx, |t, cx| t.set_file(path, modified, cx));
        self.active = self.tabs.len() - 1;
        self.save_layout(cx);
        self.sync_schema(cx);
        let ed = tab.read(cx).editor().clone();
        ed.update(cx, |e, cx| e.focus(window, cx));
        cx.notify();
        true
    }

    /// Ask where to save `tab`'s script, then save it there.
    pub(crate) fn save_sql_script_as(&mut self, tab: Entity<SqlTab>, cx: &mut Context<Self>) {
        let (dir, name) = {
            let t = tab.read(cx);
            match &t.file {
                Some(p) => (
                    p.parent().map(PathBuf::from).unwrap_or_default(),
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                ),
                None => {
                    let home = std::env::var_os("HOME")
                        .or_else(|| std::env::var_os("USERPROFILE"))
                        .map(PathBuf::from)
                        .unwrap_or_default();
                    let title = t.title.to_string();
                    let name = if title.ends_with(".sql") {
                        title
                    } else {
                        format!("{title}.sql")
                    };
                    (home, name)
                }
            }
        };
        let rx = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                tab.update(cx, |t, cx| t.save_as(path, cx));
            }
        })
        .detach();
    }

    /// Route a file save result to the SQL tab that asked for it.
    pub(crate) fn on_sql_file_saved(
        &mut self,
        request: RequestId,
        result: &Result<Option<i64>, switchyard_core::SaveError>,
        cx: &mut Context<Self>,
    ) {
        for t in &self.tabs {
            if let Tab::Sql(s) = t
                && s.read(cx).owns_save(request)
            {
                s.update(cx, |s, cx| s.on_file_saved(result, cx));
            }
        }
    }

    /// Closing a SQL tab whose script has unsaved changes: Save, Discard or Cancel.
    /// Returns `false` when the tab can close now.
    pub(crate) fn confirm_sql_close(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(Tab::Sql(tab)) = self.tabs.get(ix) else {
            return false;
        };
        if !tab.read(cx).file_dirty {
            return false;
        }
        let tab = tab.clone();
        let text = format!(
            "“{}” has changes not saved to its file.",
            tab.read(cx).title
        );
        let ws = cx.entity().downgrade();
        dialogs::open(window, cx, Dismiss::Explicit, move |dialog, window, cx| {
            let (discard_ws, discard_tab) = (ws.clone(), tab.clone());
            let (save_ws, save_tab) = (ws.clone(), tab.clone());
            let buttons = vec![
                dialogs::cancel_button(window, cx),
                Button::new("sql-unsaved-discard")
                    .danger()
                    .label("Discard")
                    .on_click(move |_, window, cx| {
                        window.close_dialog(cx);
                        let tab = discard_tab.clone();
                        let _ = discard_ws.update(cx, |w, cx| w.close_sql_tab(&tab, cx));
                    })
                    .into_any_element(),
                Button::new("sql-unsaved-save")
                    .primary()
                    .label("Save")
                    .on_click(move |_, window, cx| {
                        window.close_dialog(cx);
                        let tab = save_tab.clone();
                        if tab.read(cx).file.is_some() {
                            // The core writes it even after the tab is gone.
                            tab.update(cx, |t, cx| t.save_file(cx));
                            let _ = save_ws.update(cx, |w, cx| w.close_sql_tab(&tab, cx));
                        }
                    })
                    .into_any_element(),
            ];
            dialog
                .title("Unsaved changes")
                .w(dialogs::dialog_width(window, window.rem_size() * 30.))
                .child(div().whitespace_normal().child(text.clone()))
                .footer(dialogs::footer_row(buttons))
        });
        true
    }

    fn close_sql_tab(&mut self, tab: &Entity<SqlTab>, cx: &mut Context<Self>) {
        if let Some(ix) = self
            .tabs
            .iter()
            .position(|t| matches!(t, Tab::Sql(s) if s == tab))
        {
            self.close_tab_now(ix, cx);
        }
    }
}
