//! Named Workbench workspaces: listing them when the panel opens, the empty
//! page's and the header menu's "Add workspace", and switching between them.
//!
//! The chosen workspace is [`crate::api::compat::current_project`]; the
//! panel's frame-time check rehydrates when it differs from the bound one.
//! Store access runs on core's runtime through [`crate::api::compat::blocking`].

use switchyard_api::WorkspaceEntry;
use switchyard_api::runtime::workspace::{
    WorkspaceList, create_workspace, list_workspaces, mark_workspace_opened,
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
    /// `Workspace N` and open it.
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
            .unwrap_or_else(|| "Workspace".into())
    }

    /// `true` while a workspace is being created.
    pub(super) fn adding_workspace(&self) -> bool {
        self._workspace_work.is_some() && self.workspaces.is_some()
    }
}
