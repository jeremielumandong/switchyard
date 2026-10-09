//! The Workbench's empty states: the first-run page shown before any
//! project exists, and the Compose area when no request tab is open.
//!
//! Both offer the same quick actions, and every one reuses an existing
//! path: a new request goes through request creation, Import and Paste cURL
//! open the Import tab (whose parser already reads a single cURL command),
//! and the sample request is an ordinary created request with a URL filled
//! in. Nothing is sent automatically.
//!
//! Request tabs open only on demand, so `request_tabs` may be empty; the
//! helpers here are how the rest of the panel asks for the active tab.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::{Disableable, IconName};
use gpui_kit::{AnyElement, Div, div};

use super::*;
use crate::appearance::rpx;

/// The public echo endpoint the sample request points at.
pub(super) const SAMPLE_URL: &str = "https://httpbin.org/get";

/// The name and URL a newly created request starts with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RequestSeed {
    pub name: String,
    pub method: switchyard_api::HttpMethod,
    pub url: String,
    pub headers: Vec<switchyard_api::KeyValueRow>,
    pub body: Body,
}

impl Default for RequestSeed {
    fn default() -> Self {
        Self {
            name: "New request".into(),
            method: switchyard_api::HttpMethod::get(),
            url: String::new(),
            headers: Vec::new(),
            body: Body::None,
        }
    }
}

impl RequestSeed {
    /// "Try a sample request": a GET to a public echo endpoint.
    pub fn sample() -> Self {
        Self {
            name: "Sample request".into(),
            url: SAMPLE_URL.into(),
            ..Self::default()
        }
    }

    /// A request the assistant wrote.
    pub fn generated(request: &crate::api::generated::GeneratedRequest) -> Self {
        Self {
            name: request.name.clone(),
            method: request.method.clone(),
            url: request.url.clone(),
            headers: request.header_rows(),
            body: request.body(),
        }
    }
}

/// A quick action from an empty state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StartAction {
    NewRequest,
    Import,
    PasteCurl,
    Sample,
    /// Describe the request in words; the assistant writes it.
    DescribeWithAi,
}

/// The tab to make active after closing some: the preferred (or previously
/// active) tab when it survived, else the one before the first closed
/// position. `None` when no tab is left.
pub(super) fn active_after_close(
    remaining: &[u64],
    preferred: Option<u64>,
    first_closed_index: usize,
) -> Option<usize> {
    if remaining.is_empty() {
        return None;
    }
    preferred
        .and_then(|id| remaining.iter().position(|tab| *tab == id))
        .or(Some(
            first_closed_index
                .saturating_sub(1)
                .min(remaining.len() - 1),
        ))
}

/// The tab after (or before) `active`, wrapping; `None` with no tabs.
pub(super) fn neighbour_index(len: usize, active: usize, forward: bool) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let active = active.min(len - 1);
    Some(if forward {
        (active + 1) % len
    } else {
        (active + len - 1) % len
    })
}

impl WorkbenchPanel {
    /// The open request tab's id, or `None` when no tab is open.
    pub(super) fn active_request_tab_id(&self) -> Option<u64> {
        self.request_tabs
            .get(self.active_request_tab)
            .map(|tab| tab.id)
    }

    /// `false` while the Compose area shows the empty state.
    pub(super) fn has_request_tab(&self) -> bool {
        self.active_request_tab_id().is_some()
    }

    /// Open a blank tab when none is, for paths that write into the editor
    /// directly (history replay).
    pub(super) fn ensure_request_tab(&mut self) {
        if !self.request_tabs.is_empty() {
            return;
        }
        let id = self.next_request_tab_id;
        self.next_request_tab_id = self.next_request_tab_id.wrapping_add(1);
        self.request_tabs.push(RequestTabState::blank(
            id,
            self.current_collection_id.clone(),
        ));
        self.active_request_tab = 0;
    }

    /// The next/previous tab shortcut's target.
    pub(super) fn neighbour_request_tab_id(&self, forward: bool) -> Option<u64> {
        neighbour_index(self.request_tabs.len(), self.active_request_tab, forward)
            .map(|index| self.request_tabs[index].id)
    }

    /// A first-run button: create a project, then run `action` once it has
    /// opened.
    fn start_project(
        &mut self,
        action: Option<StartAction>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.add_workspace(window, cx);
        if self._workspace_work.is_some() {
            self.ux.pending_start = action;
        }
    }

    /// Run the first-run action once its project is created and hydrated.
    /// Called every frame; cheap when nothing is pending.
    pub(super) fn run_pending_start(
        &mut self,
        has_workspace: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.ux.pending_start.is_none()
            || !has_workspace
            || self._workspace_work.is_some()
            || self._storage_work.is_some()
            || self.storage_loading
        {
            return;
        }
        let Some(action) = self.ux.pending_start.take() else {
            return;
        };
        // Hydration failed: its error is already on screen.
        if self.workspace_data.is_some() {
            self.run_start_action(action, window, cx);
        }
    }

    fn run_start_action(
        &mut self,
        action: StartAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            StartAction::NewRequest => {
                self.tab = Tab::Compose;
                self.new_request(window, cx);
            }
            StartAction::DescribeWithAi => cx.emit(DescribeRequestRequested),
            StartAction::Sample => {
                self.tab = Tab::Compose;
                self.create_seeded_request_in_selection(Some(RequestSeed::sample()), window, cx);
            }
            StartAction::Import | StartAction::PasteCurl => {
                self.tab = Tab::Import;
                if action == StartAction::PasteCurl {
                    self.import_status = Some(
                        "Paste a cURL command above, then Parse to review it before it is added."
                            .into(),
                    );
                }
                self.import_source.focus_handle(cx).focus(window, cx);
            }
        }
        cx.notify();
    }

    /// What the panel shows before any project exists (or a loading line
    /// while the list loads).
    pub(super) fn render_first_run(&self, cx: &mut Context<Self>) -> Div {
        let colors = cx.theme().colors;
        let loading = self.workspaces.is_none();
        let adding = self.adding_workspace();
        let action = |id: &'static str, label: &'static str, start: StartAction| {
            Button::new(id)
                .debug_selector(move || id.into())
                .outline()
                .label(label)
                .disabled(adding)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.start_project(Some(start), window, cx)
                }))
        };
        div()
            .flex_1()
            .min_h(rpx(0.))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(space::SP_3)
            .p(space::SP_4)
            .child(
                div()
                    .text_size(text::S13)
                    .font_weight(text::weight::SEMIBOLD)
                    .text_color(colors.foreground)
                    .child(if loading {
                        "Loading projects…"
                    } else {
                        "No projects yet"
                    }),
            )
            .when(!loading, |el| {
                el.child(
                    div()
                        .max_w(rpx(380.))
                        .text_center()
                        .text_size(text::S11)
                        .text_color(palette::text_secondary(cx))
                        .child(
                            "A project keeps related collections, environments and \
                             history together. Create one, or start from something you have.",
                        ),
                )
                .child(
                    Button::new("workbench-add-workspace")
                        .debug_selector(|| "workbench-add-workspace".into())
                        .primary()
                        .icon(IconName::Plus)
                        .label(if adding {
                            "Creating project…"
                        } else {
                            "Create project"
                        })
                        .disabled(adding)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.start_project(None, window, cx)),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .justify_center()
                        .gap(space::SP_2)
                        .child(action(
                            "workbench-first-run-import",
                            "Import collection…",
                            StartAction::Import,
                        ))
                        .child(action(
                            "workbench-first-run-curl",
                            "Paste cURL",
                            StartAction::PasteCurl,
                        ))
                        .child(action(
                            "workbench-first-run-sample",
                            "Try a sample request",
                            StartAction::Sample,
                        )),
                )
            })
            .when_some(self.storage_error.clone(), |el, error| {
                el.child(
                    div()
                        .id("workbench-storage-error")
                        .text_size(text::S10)
                        .text_color(colors.danger)
                        .child(format!("Workbench storage: {error}")),
                )
            })
    }

    /// Compose with no request tab open: the rail, then the quick actions
    /// where the editor and response would be.
    pub(super) fn render_compose_without_tab(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rail = self.render_collection_rail(window, cx);
        let split = h_resizable("workbench-main-split")
            .with_state(&self.ux.rail_split)
            .on_resize(cx.listener(
                |this,
                 state: &Entity<gpui_kit::component::resizable::ResizableState>,
                 window,
                 cx| {
                    if let Some(size) = state.read(cx).sizes().first() {
                        this.ux.layout.rail_rem = *size / window.rem_size();
                        this.persist_layout(cx);
                    }
                },
            ))
            .child(
                resizable_panel()
                    .size(window.rem_size() * self.ux.layout.rail_rem)
                    .size_range(window.rem_size() * 10. ..window.rem_size() * 30.)
                    .child(rail),
            )
            .child(resizable_panel().child(self.render_compose_empty(cx)));
        div()
            .flex_1()
            .min_h(rpx(0.))
            .relative()
            .overflow_hidden()
            .child(div().absolute().inset_0().child(split))
            .into_any_element()
    }

    fn render_compose_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let empty_project = self
            .workspace_data
            .as_ref()
            .is_some_and(|data| data.collections.is_empty() && data.requests.is_empty());
        let action =
            |id: &'static str, label: &'static str, start: StartAction| {
                Button::new(id)
                    .debug_selector(move || id.into())
                    .label(label)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.run_start_action(start, window, cx)
                    }))
            };
        div()
            .id("workbench-compose-empty")
            .debug_selector(|| "workbench-compose-empty".into())
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(space::SP_3)
            .p(space::SP_4)
            .bg(colors.background)
            .child(
                div()
                    .text_size(text::S13)
                    .font_weight(text::weight::SEMIBOLD)
                    .text_color(colors.foreground)
                    .child(if empty_project {
                        "This project is empty"
                    } else {
                        "No request open"
                    }),
            )
            .child(
                div()
                    .max_w(rpx(380.))
                    .text_center()
                    .text_size(text::S11)
                    .text_color(palette::text_secondary(cx))
                    .child(if empty_project {
                        "Start with a new request, or bring in a collection or a cURL command."
                    } else {
                        "Open a request from the rail, or start a new one."
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .justify_center()
                    .gap(space::SP_2)
                    .child(
                        action(
                            "workbench-empty-new-request",
                            "New request",
                            StartAction::NewRequest,
                        )
                        .primary()
                        .icon(IconName::Plus),
                    )
                    .child(
                        action("workbench-empty-import", "Import…", StartAction::Import).outline(),
                    )
                    .child(
                        action("workbench-empty-curl", "Paste cURL", StartAction::PasteCurl)
                            .outline(),
                    )
                    .child(
                        action(
                            "workbench-empty-describe",
                            "Describe with AI",
                            StartAction::DescribeWithAi,
                        )
                        .outline(),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_seed_is_a_get_to_the_echo_endpoint() {
        let collection = CollectionId::new();
        let request = request_creation::new_request_definition(
            &[],
            collection.clone(),
            None,
            Some(RequestSeed::sample()),
        );
        assert_eq!(request.name, "Sample request");
        assert_eq!(request.url, "https://httpbin.org/get");
        assert_eq!(request.method.as_str(), "GET");
        assert_eq!(request.collection_id, collection);
        assert_eq!(request.sort_key, 0);
    }

    #[test]
    fn unseeded_requests_are_numbered_past_their_siblings() {
        let collection = CollectionId::new();
        let first = request_creation::new_request_definition(&[], collection.clone(), None, None);
        assert_eq!(first.name, "New request");
        assert!(first.url.is_empty());
        let second = request_creation::new_request_definition(
            std::slice::from_ref(&first),
            collection,
            None,
            None,
        );
        assert_eq!(second.name, "New request 2");
        assert_eq!(second.sort_key, 1);
    }

    #[test]
    fn a_generated_seed_keeps_method_headers_and_body() {
        let answer = "```http\n# Create order\nPOST {{baseUrl}}/orders\nContent-Type: application/json\n\n{\"sku\": \"a\"}\n```";
        let generated = crate::api::generated::http_requests(answer);
        let collection = CollectionId::new();
        let request = request_creation::new_request_definition(
            &[],
            collection,
            None,
            Some(RequestSeed::generated(&generated[0])),
        );
        assert_eq!(request.name, "Create order");
        assert_eq!(request.method.as_str(), "POST");
        assert_eq!(request.url, "{{baseUrl}}/orders");
        assert_eq!(request.headers.len(), 1);
        assert_eq!(request.headers[0].key, "Content-Type");
        assert!(matches!(
            request.body,
            Body::Raw { media_type: switchyard_api::RawBodyKind::Json, ref text } if text == "{\"sku\": \"a\"}"
        ));
    }

    #[test]
    fn closing_the_last_tab_leaves_none_active() {
        assert_eq!(active_after_close(&[], Some(3), 0), None);
        assert_eq!(active_after_close(&[], None, 0), None);
    }

    #[test]
    fn closing_keeps_the_surviving_active_tab_or_the_left_neighbour() {
        assert_eq!(active_after_close(&[1, 3], Some(3), 1), Some(1));
        assert_eq!(active_after_close(&[1, 3], Some(2), 1), Some(0));
        assert_eq!(active_after_close(&[3], None, 0), Some(0));
        assert_eq!(active_after_close(&[1, 2], None, 5), Some(1));
    }

    #[test]
    fn tab_cycling_with_no_tabs_does_nothing() {
        assert_eq!(neighbour_index(0, 0, true), None);
        assert_eq!(neighbour_index(0, 0, false), None);
        assert_eq!(neighbour_index(3, 2, true), Some(0));
        assert_eq!(neighbour_index(3, 0, false), Some(2));
        assert_eq!(neighbour_index(1, 0, true), Some(0));
    }
}
