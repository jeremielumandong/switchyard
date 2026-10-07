//! Request locations change independently of open editor drafts.

use super::*;

#[derive(Clone)]
pub(super) struct DraggedRequest {
    pub id: RequestId,
    pub name: String,
}

impl gpui_kit::Render for DraggedRequest {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        gpui_kit::div()
            .px_3()
            .py_2()
            .rounded(cx.theme().radius)
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .text_sm()
            .child(self.name.clone())
    }
}

#[derive(Clone)]
pub(super) struct Destination {
    pub collection: CollectionId,
    pub folder: Option<FolderId>,
    pub label: String,
}

impl WorkbenchPanel {
    pub(super) fn request_destinations(&self, request: &RequestId) -> Vec<Destination> {
        let Some(data) = self.workspace_data.as_ref() else {
            return Vec::new();
        };
        let Some(request) = data.requests.iter().find(|saved| &saved.id == request) else {
            return Vec::new();
        };
        let mut destinations = Vec::new();
        for collection in &data.collections {
            if collection.id != request.collection_id || request.folder_id.is_some() {
                destinations.push(Destination {
                    collection: collection.id.clone(),
                    folder: None,
                    label: collection.name.clone(),
                });
            }
            for row in rail_rows(data, Some(&collection.id)) {
                if let RailRow::Folder { folder, .. } = row {
                    if request.folder_id.as_ref() == Some(&folder.id) {
                        continue;
                    }
                    let mut names = vec![folder.name.clone()];
                    let mut seen = HashSet::from([folder.id.clone()]);
                    let mut parent = folder.parent_id.as_ref();
                    while let Some(id) = parent {
                        if !seen.insert(id.clone()) {
                            break;
                        }
                        let Some(folder) = data.folders.iter().find(|folder| &folder.id == id)
                        else {
                            break;
                        };
                        names.push(folder.name.clone());
                        parent = folder.parent_id.as_ref();
                    }
                    names.push(collection.name.clone());
                    names.reverse();
                    destinations.push(Destination {
                        collection: collection.id.clone(),
                        folder: Some(folder.id.clone()),
                        label: names.join(" / "),
                    });
                }
            }
        }
        destinations
    }

    pub(super) fn move_saved_request(
        &mut self,
        id: &RequestId,
        destination: &CollectionId,
        folder: Option<&FolderId>,
        cx: &mut Context<Self>,
    ) {
        if self.storage_loading || self.request_tab_switch_is_blocked(cx) {
            return;
        }
        let Some(data) = self.workspace_data.as_mut() else {
            return;
        };
        let Some(current) = data.requests.iter().find(|request| &request.id == id) else {
            return;
        };
        if &current.collection_id == destination && current.folder_id.as_ref() == folder {
            return;
        }
        let moved = match data.store.move_request(
            &data.workspace,
            &current.collection_id,
            id,
            destination,
            folder,
        ) {
            Ok(request) => request,
            Err(error) => {
                self.storage_error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        if let Some(saved) = data.requests.iter_mut().find(|request| &request.id == id) {
            *saved = moved.clone();
        }
        let relocate = |definition: &mut Option<SavedRequest>| {
            if let Some(request) = definition.as_mut().filter(|request| &request.id == id) {
                request.collection_id = moved.collection_id.clone();
                request.folder_id = moved.folder_id.clone();
                request.sort_key = moved.sort_key;
            }
        };
        relocate(&mut self.current_definition);
        for tab in &mut self.request_tabs {
            if tab.current_request_id.as_ref() == Some(id) {
                relocate(&mut tab.current_definition);
                tab.current_collection_id = Some(destination.clone());
                tab.selected_folder_id = folder.cloned();
            }
        }
        if self.current_request_id.as_ref() == Some(id) {
            self.current_collection_id = Some(destination.clone());
            self.selected_folder_id = folder.cloned();
            self.collection_tree_expanded = true;
            self.expand_folder_ancestors(folder);
        }
        self.storage_error = None;
        cx.notify();
    }
}
