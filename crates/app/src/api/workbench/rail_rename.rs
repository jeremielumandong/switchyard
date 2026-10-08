//! Inline renaming in the collection rail: one field, drawn in place of the
//! collection, folder or request row it renames.
//!
//! Opened by a row's "Rename…" item, a double-click on the row, or F2 on the
//! selected row. Enter commits, Escape cancels, and moving focus away
//! commits (an unchanged or empty name just closes the field). Requests are
//! renamed as metadata only, independent of a dirty request editor.

use super::view::mono_field;
use super::*;
use gpui_kit::{AnyElement, ClickEvent, Div, KeyDownEvent, div, px};

/// The rail's open inline rename field.
#[derive(Clone, Debug)]
pub(super) struct RailRename {
    pub(super) target: RenameTarget,
    /// The workspace the field was opened in.
    workspace: WorkspaceId,
    /// The name when the field opened; a request rename is refused if the
    /// stored name has changed since.
    original: String,
    /// A request rename is being written.
    saving: bool,
    error: Option<String>,
}

/// What committing the rename field should do before touching storage.
#[derive(Debug, PartialEq, Eq)]
enum CommitCheck {
    /// Nothing to write: close the field.
    Close,
    /// Keep the field open with this message.
    Blocked(&'static str),
    /// Write the new name.
    Write,
}

/// Decide a commit of `name` (trimmed) over `original`. A commit caused by
/// focus leaving (`blurred`) closes quietly where Enter would show why.
fn commit_check(
    name: &str,
    original: &str,
    blurred: bool,
    workspace_changed: bool,
    storage_busy: bool,
) -> CommitCheck {
    if name == original || (blurred && (name.is_empty() || workspace_changed)) {
        CommitCheck::Close
    } else if name.is_empty() {
        CommitCheck::Blocked("Enter a name.")
    } else if workspace_changed {
        CommitCheck::Blocked("The workspace changed. Press Escape and rename again.")
    } else if storage_busy {
        CommitCheck::Blocked("Wait for the current save to finish.")
    } else {
        CommitCheck::Write
    }
}

impl WorkbenchPanel {
    /// Commit on Enter or when the field loses focus; clear a stale error
    /// as the name is edited.
    pub(super) fn subscribe_rail_rename(
        input: &Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.subscribe_in(
            input,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    if let Some(rename) = this.rail_rename.as_mut()
                        && rename.error.take().is_some()
                    {
                        cx.notify();
                    }
                }
                InputEvent::PressEnter { .. } => this.commit_rail_rename(false, window, cx),
                InputEvent::Blur => this.commit_rail_rename(true, window, cx),
                InputEvent::Focus => {}
            },
        )
        .detach();
    }

    /// The rename field's state when it is open on `target`.
    pub(super) fn rail_rename_for(&self, target: &RenameTarget) -> Option<&RailRename> {
        self.rail_rename
            .as_ref()
            .filter(|rename| &rename.target == target)
    }

    /// Remember the row a click selected, so F2 renames it, and give the
    /// rail key focus. A second click of a double-click opens the rename.
    pub(super) fn rail_row_clicked(
        &mut self,
        target: RenameTarget,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // Clicking another row is "clicking away": commit an open field now,
        // before the click changes the selection under it.
        self.commit_rail_rename(true, window, cx);
        self.rail_selection = Some(target.clone());
        if event.click_count() >= 2 {
            self.open_rail_rename(target, window, cx);
            return true;
        }
        self.rail_focus.focus(window, cx);
        false
    }

    /// F2 in the rail: rename the row last selected there, else the open
    /// request, else the current collection.
    pub(super) fn rename_rail_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.container_name.focus_handle(cx).is_focused(window) {
            return;
        }
        let target = self
            .rail_selection
            .clone()
            .filter(|target| self.rail_item_name(target).is_some())
            .or_else(|| self.current_request_id.clone().map(RenameTarget::Request))
            .or_else(|| {
                self.current_collection_id
                    .clone()
                    .map(RenameTarget::Collection)
            });
        if let Some(target) = target {
            self.open_rail_rename(target, window, cx);
        }
    }

    /// Open the rename field on `target`, seeded with its name and focused.
    /// A collection is made current first (behind the unsaved-changes
    /// guard) and a folder selected; a request is renamed without opening.
    pub(super) fn open_rail_rename(
        &mut self,
        target: RenameTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .rail_rename
            .as_ref()
            .is_some_and(|rename| rename.saving)
        {
            return;
        }
        let Some(name) = self.rail_item_name(&target) else {
            return;
        };
        match &target {
            RenameTarget::Collection(id) => {
                if !self.focus_collection(id.clone(), window, cx) {
                    return;
                }
            }
            RenameTarget::Folder(id) => self.select_folder(id.clone(), window, cx),
            RenameTarget::Request(_) => {}
        }
        // Drop any field still open elsewhere before seeding this one, so
        // the seed's Change and the old field's Blur act on nothing.
        self.rail_rename = None;
        set_input(&self.container_name, &name, window, cx);
        self.container_name.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        self.rail_selection = Some(target.clone());
        self.rail_rename = Some(RailRename {
            target,
            workspace: self.bound_workspace.clone(),
            original: name,
            saving: false,
            error: None,
        });
        cx.notify();
    }

    /// Escape: close the field without renaming.
    fn cancel_rail_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .rail_rename
            .as_ref()
            .is_some_and(|rename| rename.saving)
        {
            return;
        }
        self.close_rail_rename(window, cx);
    }

    /// Close the field; if it still had focus, hand focus back to the rail
    /// so F2 keeps working.
    fn close_rail_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rail_rename = None;
        if self.container_name.focus_handle(cx).is_focused(window) {
            self.rail_focus.focus(window, cx);
        }
        cx.notify();
    }

    /// The stored name of a rail row, `None` when it no longer exists.
    fn rail_item_name(&self, target: &RenameTarget) -> Option<String> {
        let data = self.workspace_data.as_ref()?;
        match target {
            RenameTarget::Collection(id) => data
                .collections
                .iter()
                .find(|collection| &collection.id == id)
                .map(|collection| collection.name.clone()),
            RenameTarget::Folder(id) => data
                .folders
                .iter()
                .find(|folder| &folder.id == id)
                .map(|folder| folder.name.clone()),
            RenameTarget::Request(id) => data
                .requests
                .iter()
                .find(|request| &request.id == id)
                .map(|request| request.name.clone()),
        }
    }

    /// Apply the field's name to its row. `blurred` when focus moved away:
    /// then a workspace switch closes the field quietly instead of showing
    /// an error nobody is looking at.
    fn commit_rail_rename(&mut self, blurred: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rename) = self.rail_rename.clone() else {
            return;
        };
        if rename.saving {
            return;
        }
        let name = self.container_name.read(cx).value().trim().to_string();
        let workspace_changed =
            self.bound_workspace != rename.workspace || current_workspace_id() != rename.workspace;
        match commit_check(
            &name,
            &rename.original,
            blurred,
            workspace_changed,
            self.storage_loading,
        ) {
            CommitCheck::Close => {
                self.close_rail_rename(window, cx);
                return;
            }
            CommitCheck::Blocked(error) => {
                self.set_rail_rename_error(error.into(), cx);
                return;
            }
            CommitCheck::Write => {}
        }
        let Some(data) = self.workspace_data.as_ref() else {
            self.set_rail_rename_error("Workbench storage is unavailable.".into(), cx);
            return;
        };
        let command = match &rename.target {
            RenameTarget::Folder(id) => data
                .folders
                .iter()
                .find(|folder| &folder.id == id)
                .cloned()
                .ok_or_else(|| "This folder no longer exists.".to_string())
                .map(|mut folder| {
                    folder.name = name;
                    coordinator::StorageCommand::UpsertFolder {
                        collection: None,
                        folder,
                    }
                }),
            RenameTarget::Collection(id) => data
                .collections
                .iter()
                .find(|collection| &collection.id == id)
                .cloned()
                .ok_or_else(|| "This collection no longer exists.".to_string())
                .map(|mut collection| {
                    collection.name = name;
                    coordinator::StorageCommand::UpsertCollection(collection)
                }),
            RenameTarget::Request(id) => {
                let request = data
                    .requests
                    .iter()
                    .find(|request| &request.id == id && request.name == rename.original)
                    .cloned();
                match request {
                    Some(request) => {
                        self.save_request_name(request, name, rename.workspace, window, cx)
                    }
                    None => self.set_rail_rename_error(
                        "The request changed. Press Escape and rename again.".into(),
                        cx,
                    ),
                }
                return;
            }
        };
        match command {
            Ok(command) => {
                if self.run_storage_command(command, cx) {
                    self.close_rail_rename(window, cx);
                }
            }
            Err(error) => self.set_rail_rename_error(error, cx),
        }
    }

    fn set_rail_rename_error(&mut self, error: String, cx: &mut Context<Self>) {
        if let Some(rename) = self.rail_rename.as_mut() {
            rename.saving = false;
            rename.error = Some(error);
        } else {
            self.storage_error = Some(error);
        }
        cx.notify();
    }

    /// Write a request's new name with the store's stale-name check, then
    /// carry it into open tabs and the request editor.
    fn save_request_name(
        &mut self,
        request: SavedRequest,
        name: String,
        workspace: WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.workspace_data.as_ref().map(|data| data.store.clone()) else {
            return;
        };
        if let Some(rename) = self.rail_rename.as_mut() {
            rename.saving = true;
        }
        self.storage_generation = self.storage_generation.wrapping_add(1);
        let generation = self.storage_generation;
        self.storage_loading = true;
        self.storage_error = None;
        self._storage_work = Some(cx.spawn_in(window, async move |this, cx| {
            let expected_workspace = workspace.clone();
            let result = crate::api::compat::blocking(move || {
                store
                    .rename_request(
                        &workspace,
                        &request.collection_id,
                        &request.id,
                        &request.name,
                        &name,
                    )
                    .map_err(|e| e.to_string())
            })
            .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                if panel.bound_workspace != expected_workspace
                    || panel.storage_generation != generation
                {
                    return;
                }
                panel.storage_loading = false;
                panel._storage_work = None;
                let result = result.and_then(|request| {
                    if current_workspace_id() != expected_workspace
                        || !panel.workspace_data.as_ref().is_some_and(|data| {
                            data.requests.iter().any(|saved| {
                                saved.id == request.id
                                    && saved.collection_id == request.collection_id
                            })
                        })
                    {
                        Err(
                            "The workspace or request changed. Press Escape and rename again."
                                .into(),
                        )
                    } else {
                        Ok(request)
                    }
                });
                match result {
                    Ok(request) => {
                        panel.synchronize_request_name(&request, window, cx);
                        panel.close_rail_rename(window, cx);
                    }
                    Err(error) => panel.set_rail_rename_error(error, cx),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn synchronize_request_name(
        &mut self,
        request: &SavedRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.capture_active_request_tab(cx);
        if let Some(saved) = self.workspace_data.as_mut().and_then(|data| {
            data.requests
                .iter_mut()
                .find(|r| r.id == request.id && r.collection_id == request.collection_id)
        }) {
            saved.name = request.name.clone();
        }
        for tab in &mut self.request_tabs {
            if tab.current_request_id.as_ref() == Some(&request.id) {
                if let Some(saved) = &mut tab.current_definition {
                    saved.name = request.name.clone();
                }
                tab.input_values[1] = request.name.clone();
            }
        }
        if self.current_request_id.as_ref() == Some(&request.id) {
            if let Some(saved) = &mut self.current_definition {
                saved.name = request.name.clone();
            }
            if self.request_name.read(cx).value().as_str() != request.name {
                self.pending_editor_hydration_changes += 1;
                set_input(&self.request_name, &request.name, window, cx);
            }
        }
        self.navigation_notice = Some("Request renamed.".into());
        cx.notify();
    }

    /// The rename field drawn in place of `rename`'s row, indented by
    /// `indent` like the row it replaces.
    pub(super) fn render_rail_rename(
        &self,
        rename: &RailRename,
        indent: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors;
        let (message, color) = match (&rename.error, rename.saving) {
            (Some(error), _) => (Some(error.clone()), cx.theme().danger),
            (None, true) => (Some("Renaming…".to_string()), colors.muted_foreground),
            (None, false) => (None, colors.muted_foreground),
        };
        let field: Div = mono_field(&self.container_name, window, cx)
            .w_full()
            .h(px(26.))
            .text_size(text::S11);
        div()
            .id("workbench-rail-rename")
            .debug_selector(|| "workbench-rail-rename".into())
            .flex()
            .flex_col()
            .gap(px(2.))
            .ml(px(indent))
            .mt(px(2.))
            .pr(px(2.))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    this.cancel_rail_rename(window, cx);
                }
            }))
            .child(field)
            .when_some(message, |el, message| {
                el.child(
                    div()
                        .px(px(2.))
                        .text_size(text::S11)
                        .text_color(color)
                        .child(message),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{CommitCheck, commit_check};

    #[test]
    fn an_unchanged_name_just_closes_the_field() {
        assert_eq!(
            commit_check("Users", "Users", false, false, false),
            CommitCheck::Close
        );
        assert_eq!(
            commit_check("Users", "Users", true, true, true),
            CommitCheck::Close
        );
    }

    #[test]
    fn enter_explains_why_a_rename_cannot_be_written() {
        assert_eq!(
            commit_check("", "Users", false, false, false),
            CommitCheck::Blocked("Enter a name.")
        );
        assert!(matches!(
            commit_check("Accounts", "Users", false, true, false),
            CommitCheck::Blocked(message) if message.contains("workspace changed")
        ));
        assert!(matches!(
            commit_check("Accounts", "Users", false, false, true),
            CommitCheck::Blocked(message) if message.contains("current save")
        ));
    }

    #[test]
    fn clicking_away_commits_or_closes_quietly() {
        assert_eq!(
            commit_check("", "Users", true, false, false),
            CommitCheck::Close
        );
        assert_eq!(
            commit_check("Accounts", "Users", true, true, false),
            CommitCheck::Close
        );
        assert_eq!(
            commit_check("Accounts", "Users", true, false, false),
            CommitCheck::Write
        );
        // A busy store keeps the typed name and says why.
        assert!(matches!(
            commit_check("Accounts", "Users", true, false, true),
            CommitCheck::Blocked(_)
        ));
    }

    #[test]
    fn enter_writes_a_new_name() {
        assert_eq!(
            commit_check("Accounts", "Users", false, false, false),
            CommitCheck::Write
        );
    }
}
