//! Creating a collection member is independent of saving its later edits.
use std::collections::VecDeque;
use switchyard_api::HttpMethod;

use super::*;

#[derive(Default)]
pub(super) struct CreationState {
    queue: VecDeque<(SavedRequest, Option<Collection>)>,
    work: Option<gpui_kit::Task<()>>,
    pending_collections: Vec<Collection>,
}
impl CreationState {
    pub fn busy(&self) -> bool {
        !self.queue.is_empty()
    }

    pub fn pending_collection(&self, id: &CollectionId) -> Option<Collection> {
        self.pending_collections
            .iter()
            .find(|collection| &collection.id == id)
            .cloned()
    }

    pub fn collection_persisted(&mut self, id: &CollectionId) {
        self.pending_collections
            .retain(|collection| &collection.id != id);
    }
}

impl WorkbenchPanel {
    pub(super) fn create_request_in_selection(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.request_tab_switch_is_blocked(cx) {
            return;
        }
        let Some(data) = self.workspace_data.as_ref() else {
            self.storage_error = Some("Workbench storage is unavailable.".into());
            cx.notify();
            return;
        };
        let collection = self
            .current_collection_id
            .as_ref()
            .and_then(|id| data.collection(id))
            .or_else(|| data.collections.first());
        let (id, new_collection) = match collection {
            Some(collection) => (collection.id.clone(), None),
            None => {
                let collection = Collection {
                    id: CollectionId::new(),
                    workspace_id: self.bound_workspace.clone(),
                    name: "My API".into(),
                    description: String::new(),
                    auth: AuthConfig::None,
                    variables: Vec::new(),
                    scripts: Default::default(),
                    extensions: Default::default(),
                };
                (collection.id.clone(), Some(collection))
            }
        };
        let folder = self.selected_folder().filter(|folder_id| {
            data.folders
                .iter()
                .any(|folder| &folder.id == folder_id && folder.collection_id == id)
        });
        self.create_request_at(id, folder, new_collection, window, cx);
    }

    pub(super) fn create_request_at(
        &mut self,
        collection: CollectionId,
        folder: Option<FolderId>,
        new_collection: Option<Collection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.request_tab_switch_is_blocked(cx) {
            return;
        }
        let Some(data) = self.workspace_data.as_mut() else {
            return;
        };
        if new_collection.is_none() && data.collection(&collection).is_none()
            || folder.as_ref().is_some_and(|id| {
                !data
                    .folders
                    .iter()
                    .any(|f| &f.id == id && f.collection_id == collection)
            })
        {
            self.storage_error = Some(
                "The destination no longer exists. Choose a collection or folder again.".into(),
            );
            cx.notify();
            return;
        }
        let mut number = 1usize;
        let name =
            loop {
                let name = if number == 1 {
                    "New request".to_string()
                } else {
                    format!("New request {number}")
                };
                if !data.requests.iter().any(|r| {
                    r.collection_id == collection && r.folder_id == folder && r.name == name
                }) {
                    break name;
                }
                number += 1;
            };
        let sort_key = data
            .requests
            .iter()
            .filter(|r| r.collection_id == collection && r.folder_id == folder)
            .map(|r| r.sort_key)
            .max()
            .unwrap_or(-1)
            .saturating_add(1);
        let request = SavedRequest {
            id: RequestId::new(),
            collection_id: collection,
            folder_id: folder.clone(),
            name,
            method: HttpMethod::get(),
            url: String::new(),
            params: Vec::new(),
            headers: Vec::new(),
            auth: AuthConfig::Inherit,
            body: Body::None,
            variables: Vec::new(),
            scripts: Default::default(),
            settings: Default::default(),
            extensions: Default::default(),
            sort_key,
        };
        if let Some(collection) = &new_collection {
            data.collections.push(collection.clone());
            self.ux
                .creation
                .pending_collections
                .push(collection.clone());
        }
        data.requests.push(request.clone());
        self.ux
            .creation
            .queue
            .push_back((request.clone(), new_collection));
        // An explicit Add always opens its own tab, including when the initial
        // scratch tab is pristine. Retain that tab and any unsaved edits.
        self.open_request_tab(window, cx);
        self.current_request_id = Some(request.id.clone());
        self.load_saved_request(&request, window, cx);
        self.selected_folder_id = folder.clone();
        self.collection_tree_expanded = true;
        self.expand_folder_ancestors(folder.as_ref());
        set_input(&self.rail_filter, "", window, cx);
        self.capture_active_request_tab(cx);
        self.url.focus_handle(cx).focus(window, cx);
        self.persist_next_created_request(cx);
        cx.notify();
    }

    fn persist_next_created_request(&mut self, cx: &mut Context<Self>) {
        if self.ux.creation.work.is_some() {
            return;
        }
        let Some((request, collection)) = self.ux.creation.queue.front().cloned() else {
            self.storage_loading = false;
            return;
        };
        let Some(data) = self.workspace_data.as_ref() else {
            return;
        };
        let store = data.store.clone();
        let workspace = self.bound_workspace.clone();
        let generation = self.storage_generation;
        let id = request.id.clone();
        let collection =
            collection.or_else(|| self.ux.creation.pending_collection(&request.collection_id));
        let collection_id = collection.as_ref().map(|collection| collection.id.clone());
        self.storage_loading = true;
        self.ux.creation.work = Some(cx.spawn(async move |this, cx| {
            let result = crate::api::compat::blocking(move || {
                if let Some(collection) = collection
                    && let Err(error) = store.upsert_collection(&collection)
                {
                    return (false, Err(error.to_string()));
                }
                (
                    true,
                    store.upsert_request(&request).map_err(|e| e.to_string()),
                )
            })
            .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.bound_workspace != workspace || panel.storage_generation != generation {
                    return;
                }
                panel.ux.creation.work = None;
                panel.ux.creation.queue.pop_front();
                if result.0
                    && let Some(id) = collection_id.as_ref()
                {
                    panel.ux.creation.collection_persisted(id);
                }
                if let Err(error) = result.1 {
                    panel.creation_failed(&id, error, cx);
                }
                panel.persist_next_created_request(cx);
                cx.notify();
            });
        }));
    }

    fn creation_failed(&mut self, id: &RequestId, error: String, cx: &mut Context<Self>) {
        if let Some(data) = self.workspace_data.as_mut() {
            data.requests.retain(|r| &r.id != id);
        }
        for tab in &mut self.request_tabs {
            if tab.current_request_id.as_ref() == Some(id) {
                tab.current_definition = None;
                tab.dirty = true;
            }
        }
        if self.current_request_id.as_ref() == Some(id) {
            self.current_definition = None;
            self.dirty = true;
        }
        self.storage_error = Some(format!(
            "Could not create request: {error}. Its open tab retains your draft; use Save to retry."
        ));
        cx.notify();
    }
}
