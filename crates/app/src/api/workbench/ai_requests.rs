//! The assistant's way back into the Workbench: the context a "Describe with
//! AI" question carries, and opening a request the answer wrote.
//!
//! The context is names only — the collection, the environment's keys, the
//! sibling requests' method and URL as saved (with their `{{variables}}`) —
//! never a resolved value, so it can cross the same boundary as [`assist`].

use super::empty_state::RequestSeed;
use super::*;
use crate::api::generated::GeneratedRequest;

/// The most sibling requests a description's context lists.
const MAX_SIBLINGS: usize = 15;

impl WorkbenchPanel {
    /// Sections for [`describe_prompt`]: what a new request in the current
    /// collection should look like.
    pub fn ai_context(&self) -> Vec<(&'static str, String)> {
        let mut sections = Vec::new();
        let Some(data) = self.workspace_data.as_ref() else {
            return sections;
        };
        let collection = self
            .current_collection_id
            .as_ref()
            .and_then(|id| data.collection(id))
            .or_else(|| data.collections.first());
        if let Some(collection) = collection {
            sections.push(("Collection", collection.name.clone()));
            let siblings = data
                .requests
                .iter()
                .filter(|r| r.collection_id == collection.id)
                .take(MAX_SIBLINGS)
                .map(|r| format!("{} {}", r.method.as_str(), r.url))
                .collect::<Vec<_>>();
            if !siblings.is_empty() {
                sections.push(("Requests in this collection", siblings.join("\n")));
            }
        }
        if let Some(environment) = self.active_environment() {
            let keys = environment
                .variables
                .iter()
                .filter(|v| v.enabled)
                .map(|v| v.key.as_str())
                .filter(|k| !k.is_empty())
                .collect::<Vec<_>>();
            sections.push((
                "Environment",
                format!(
                    "{} · keys: {}",
                    environment.name,
                    if keys.is_empty() {
                        "none".into()
                    } else {
                        keys.join(", ")
                    }
                ),
            ));
        }
        sections
    }

    /// Open `request` as a new, saved request in the current collection, on
    /// Compose. It is not sent.
    pub fn open_generated_request(
        &mut self,
        request: &GeneratedRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.workspace_data.is_none() {
            self.navigation_notice =
                Some("Create a project first, then open the request again.".into());
            cx.notify();
            return;
        }
        self.tab = Tab::Compose;
        self.create_seeded_request_in_selection(Some(RequestSeed::generated(request)), window, cx);
    }
}
