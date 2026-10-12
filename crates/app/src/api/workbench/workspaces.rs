//! Named Workbench workspaces (called "projects" in the UI): listing them
//! when the panel opens, the empty page's and the header menu's "Add project",
//! and switching between them.
//!
//! The chosen workspace is [`crate::api::compat::current_project`]; the
//! panel's frame-time check rehydrates when it differs from the bound one.
//! Store access runs on core's runtime through [`crate::api::compat::blocking`].

use switchyard_api::WorkspaceEntry;
use switchyard_api::runtime::workspace::{
    WorkspaceList, create_workspace, delete_workspace, list_workspaces, mark_workspace_opened,
};

use super::*;

impl WorkbenchPanel {
    /// Read the workspace list; the panel stays on its empty page until a
    /// workspace exists.
    pub(super) fn load_workspaces(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.storage_loading = true;
        let data_dir = workbench_data_dir();
        self._workspace_work = Some(cx.spawn_in(window, async move |this, cx| {
            let listed = crate::api::compat::blocking(move || {
                data_dir
                    .ok_or_else(|| "Cannot resolve the Workbench data directory.".to_string())
                    .and_then(|path| list_workspaces(&path))
            })
            .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                panel._workspace_work = None;
                match listed {
                    Ok(list) => panel.apply_workspace_list(list, window, cx),
                    Err(error) => {
                        panel.workspaces = Some(Vec::new());
                        panel.storage_loading = false;
                        panel.storage_error = Some(error);
                        cx.notify();
                    }
                }
            });
        }));
    }

    fn apply_workspace_list(
        &mut self,
        list: WorkspaceList,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let chosen = list
            .last_opened
            .filter(|id| list.workspaces.iter().any(|entry| &entry.id == id))
            .or_else(|| list.workspaces.first().map(|entry| entry.id.clone()));
        self.workspaces = Some(list.workspaces);
        match chosen {
            Some(id) => self.open_workspace(id, window, cx),
            None => {
                self.storage_loading = false;
                cx.notify();
            }
        }
    }

    /// The empty page's button and the header menu's last row: create
    /// `Project N` and open it.
    pub(super) fn add_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.refuse_dirty_switch(cx) || self._workspace_work.is_some() {
            return;
        }
        let data_dir = workbench_data_dir();
        self._workspace_work = Some(cx.spawn_in(window, async move |this, cx| {
            let created = crate::api::compat::blocking(move || {
                data_dir
                    .ok_or_else(|| "Cannot resolve the Workbench data directory.".to_string())
                    .and_then(|path| create_workspace(&path))
            })
            .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                panel._workspace_work = None;
                match created {
                    Ok(entry) => {
                        let id = entry.id.clone();
                        panel.workspaces.get_or_insert_default().push(entry);
                        panel.open_workspace(id, window, cx);
                    }
                    Err(error) => {
                        panel.storage_error = Some(error);
                        cx.notify();
                    }
                }
            });
        }));
        cx.notify();
    }

    /// The header menu's workspace rows.
    pub(super) fn select_workspace(
        &mut self,
        id: WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if id == self.bound_workspace && self.workspace_data.is_some() {
            return;
        }
        if self.refuse_dirty_switch(cx) {
            return;
        }
        self.open_workspace(id, window, cx);
    }

    /// Make `id` current, hydrate it and remember it for the next launch.
    fn open_workspace(&mut self, id: WorkspaceId, window: &mut Window, cx: &mut Context<Self>) {
        crate::api::compat::set_current_project(Some(id.as_str().to_owned()));
        self.start_workspace_hydration(id.clone(), window, cx);
        let data_dir = workbench_data_dir();
        let remembered = crate::api::compat::blocking(move || {
            data_dir.map(|path| mark_workspace_opened(&path, &id))
        });
        cx.spawn(async move |_, _| {
            if let Some(Err(error)) = remembered.await {
                tracing::warn!(%error, "could not record the opened Workbench workspace");
            }
        })
        .detach();
    }

    /// The workspaces list once loaded, for the header menu.
    pub(super) fn workspace_entries(&self) -> &[WorkspaceEntry] {
        self.workspaces.as_deref().unwrap_or_default()
    }

    /// The header chip's label: the open workspace's name.
    pub(super) fn workspace_display_name(&self) -> String {
        self.workspace_entries()
            .iter()
            .find(|entry| entry.id == self.bound_workspace)
            .map(|entry| entry.name.clone())
            .unwrap_or_else(|| "Project".into())
    }

    /// `true` while a workspace is being created.
    pub(super) fn adding_workspace(&self) -> bool {
        self._workspace_work.is_some() && self.workspaces.is_some()
    }
}

impl WorkbenchPanel {
    /// The header menu's "Delete project…": ask, then delete the open
    /// workspace and everything in it.
    pub(super) fn confirm_delete_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::api::compat::dialogs::{self, Dismiss};
        let workspace = self.bound_workspace.clone();
        if !self
            .workspace_entries()
            .iter()
            .any(|entry| entry.id == workspace)
            || self._workspace_work.is_some()
        {
            return;
        }
        let name = self.workspace_display_name();
        let panel = cx.entity().downgrade();
        let danger = cx.theme().colors.danger;
        self.focus_handle.focus(window, cx);
        dialogs::open(window, cx, Dismiss::Explicit, move |dialog, window, _| {
            let panel = panel.clone();
            let workspace = workspace.clone();
            dialog
                .title("Delete project?")
                .w(dialogs::dialog_width(window, window.rem_size() * 30.))
                .child(
                    gpui_kit::div()
                        .id("workbench-delete-workspace-confirm")
                        .debug_selector(|| "workbench-delete-workspace-confirm".into())
                        .flex()
                        .flex_col()
                        .gap_2()
                        .pl_3()
                        .border_l_2()
                        .border_color(danger)
                        .whitespace_normal()
                        .child(format!("Delete “{name}”?"))
                        .child(
                            "Its collections, requests, environments, history and runs \
                             are removed. This cannot be undone.",
                        ),
                )
                .footer(dialogs::confirm_footer("Delete project"))
                .on_ok(move |_, window, cx| {
                    let workspace = workspace.clone();
                    let _ = panel.update(cx, |panel, cx| {
                        panel.delete_workspace(workspace, window, cx)
                    });
                    true
                })
        });
        cx.notify();
    }

    fn delete_workspace(&mut self, id: WorkspaceId, window: &mut Window, cx: &mut Context<Self>) {
        if self._workspace_work.is_some() {
            return;
        }
        let data_dir = workbench_data_dir();
        let target = id.clone();
        self._workspace_work = Some(cx.spawn_in(window, async move |this, cx| {
            let remaining = crate::api::compat::blocking(move || {
                data_dir
                    .ok_or_else(|| "Cannot resolve the Workbench data directory.".to_string())
                    .and_then(|path| delete_workspace(&path, &target))
            })
            .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                panel._workspace_work = None;
                match remaining {
                    Ok(remaining) => panel.apply_workspace_deletion(id, remaining, window, cx),
                    Err(error) => {
                        panel.storage_error = Some(error);
                        cx.notify();
                    }
                }
            });
        }));
        cx.notify();
    }

    /// Forget a deleted workspace; when it was open, open another one or
    /// show the "Add project" page.
    fn apply_workspace_deletion(
        &mut self,
        deleted: WorkspaceId,
        remaining: Vec<WorkspaceEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let next = remaining.first().map(|entry| entry.id.clone());
        self.workspaces = Some(remaining);
        if self.bound_workspace == deleted {
            // Its tabs and drafts went with it.
            self.request_tabs.clear();
            self.protocol_panes.clear();
            self.dirty = false;
            self.persisted_tab_session = None;
            match next {
                Some(id) => self.open_workspace(id, window, cx),
                None => {
                    crate::api::compat::set_current_project(None);
                    self.workspace_data = None;
                    self.history.clear();
                    self.clear_request_editor(window, cx);
                    self.dirty = false;
                }
            }
        }
        self.navigation_notice = Some("Project deleted.".into());
        cx.notify();
    }
}
