//! The Mocks tab: serve a collection's saved examples on `127.0.0.1`
//! ([`switchyard_api::mock`]) so a front end or a test suite can call an API
//! that does not exist yet. Each collection gets its own server; the tab
//! lists the routes it serves and the calls it answered.
//!
//! Servers run on core's runtime and stop when stopped here, or when the
//! Workbench closes.

use std::sync::Arc;
use std::time::Duration;

use gpui_kit::{AnyElement, SharedString, div, prelude::*};
use switchyard_api::mock::{MockRoute, MockServer, mock_routes};

use super::view::{
    Col, accent_button, cell, header_cells, heading, mono_field, note_card, outline_chip, pill,
    table, table_row, tinted_cell, verb,
};
use super::*;
use crate::appearance::rpx;
use gpui_kit::component::Disableable;
use gpui_kit::component::scroll::ScrollableElement;

/// How often the call log refreshes while a server runs.
const REFRESH: Duration = Duration::from_millis(500);

/// A collection being served.
struct RunningMock {
    collection_id: CollectionId,
    collection_name: String,
    server: Arc<MockServer>,
}

/// The Mocks tab's state.
pub(super) struct MockState {
    port: Entity<InputState>,
    servers: Vec<RunningMock>,
    selected: Option<CollectionId>,
    error: Option<String>,
    /// Calls answered across servers at the last refresh.
    seen_hits: u64,
    _starting: Option<gpui_kit::Task<()>>,
    _refresh: Option<gpui_kit::Task<()>>,
}

impl MockState {
    pub(super) fn new(window: &mut Window, cx: &mut Context<WorkbenchPanel>) -> Self {
        Self {
            port: cx.new(|cx| InputState::new(window, cx).placeholder("any free port")),
            servers: Vec::new(),
            selected: None,
            error: None,
            seen_hits: 0,
            _starting: None,
            _refresh: None,
        }
    }

    fn server(&self, collection: &CollectionId) -> Option<&RunningMock> {
        self.servers
            .iter()
            .find(|running| &running.collection_id == collection)
    }
}

/// Port typed in the form: blank means any free port.
pub(super) fn parse_port(value: &str) -> Result<u16, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(0);
    }
    match value.parse::<u16>() {
        Ok(port) if port > 0 => Ok(port),
        _ => Err("Port must be a number from 1 to 65535.".into()),
    }
}

impl WorkbenchPanel {
    /// Routes `collection` would serve now, from its saved requests and examples.
    fn mock_routes_for(&self, collection: &CollectionId) -> Vec<MockRoute> {
        let Some(data) = self.workspace_data.as_ref() else {
            return Vec::new();
        };
        let requests: Vec<SavedRequest> = data
            .requests
            .iter()
            .filter(|request| &request.collection_id == collection)
            .cloned()
            .collect();
        mock_routes(&requests, &data.examples)
    }

    fn start_mock(&mut self, collection: CollectionId, cx: &mut Context<Self>) {
        if self.mocks.server(&collection).is_some() {
            return;
        }
        let port = match parse_port(&self.mocks.port.read(cx).value()) {
            Ok(port) => port,
            Err(error) => {
                self.mocks.error = Some(error);
                cx.notify();
                return;
            }
        };
        let routes = self.mock_routes_for(&collection);
        if routes.is_empty() {
            self.mocks.error = Some(
                "This collection has no saved examples yet. Send a request and choose \
                 “Save example” to give the mock something to answer with."
                    .into(),
            );
            cx.notify();
            return;
        }
        let Some(runtime) = crate::api::compat::tokio_handle() else {
            self.mocks.error = Some("The background runtime is not running.".into());
            cx.notify();
            return;
        };
        let name = self
            .workspace_data
            .as_ref()
            .and_then(|data| data.collection(&collection))
            .map(|collection| collection.name.clone())
            .unwrap_or_else(|| "Collection".into());
        self.mocks.error = None;
        self.mocks.selected = Some(collection.clone());
        // Binding the socket is I/O: do it on core's runtime.
        self.mocks._starting = Some(cx.spawn(async move |this, cx| {
            let started =
                crate::api::compat::blocking(move || MockServer::start(&runtime, routes, port))
                    .await;
            let _ = this.update(cx, |panel, cx| {
                match started {
                    Ok(server) => {
                        if panel.mocks.server(&collection).is_none() {
                            panel.mocks.servers.push(RunningMock {
                                collection_id: collection,
                                collection_name: name,
                                server: Arc::new(server),
                            });
                        }
                        panel.ensure_mock_refresh(cx);
                    }
                    Err(error) => panel.mocks.error = Some(error),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn stop_mock(&mut self, collection: &CollectionId, cx: &mut Context<Self>) {
        self.mocks
            .servers
            .retain(|running| &running.collection_id != collection);
        if self.mocks.servers.is_empty() {
            self.mocks._refresh = None;
        }
        cx.notify();
    }

    /// Repaint the call log while any server runs and has new calls.
    fn ensure_mock_refresh(&mut self, cx: &mut Context<Self>) {
        if self.mocks._refresh.is_some() {
            return;
        }
        self.mocks._refresh = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH).await;
                let running = this
                    .update(cx, |panel, cx| {
                        let state = &mut panel.mocks;
                        if state.servers.is_empty() {
                            state._refresh = None;
                            return false;
                        }
                        let hits: u64 = state
                            .servers
                            .iter()
                            .map(|running| running.server.total_hits())
                            .sum();
                        if hits != state.seen_hits {
                            state.seen_hits = hits;
                            if panel.tab == Tab::Mocks {
                                cx.notify();
                            }
                        }
                        true
                    })
                    .unwrap_or(false);
                if !running {
                    return;
                }
            }
        }));
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

impl WorkbenchPanel {
    pub(super) fn render_mocks(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_1()
            .min_h(rpx(0.))
            .child(self.render_mock_list(window, cx))
            .child(self.render_mock_detail(cx))
            .into_any_element()
    }

    /// The collection the detail pane shows: the selected one, else the
    /// first running one, else the collection open in the rail.
    fn mock_focus(&self) -> Option<CollectionId> {
        self.mocks
            .selected
            .clone()
            .or_else(|| {
                self.mocks
                    .servers
                    .first()
                    .map(|running| running.collection_id.clone())
            })
            .or_else(|| self.current_collection_id.clone())
    }

    fn render_mock_list(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let focus = self.mock_focus();
        let collections: Vec<(CollectionId, String)> = self
            .workspace_data
            .as_ref()
            .map(|data| {
                data.collections
                    .iter()
                    .map(|collection| (collection.id.clone(), collection.name.clone()))
                    .collect()
            })
            .unwrap_or_default();
        // Servers keep running across project switches; list them too.
        let others: Vec<(CollectionId, String)> = self
            .mocks
            .servers
            .iter()
            .filter(|running| {
                !collections
                    .iter()
                    .any(|(id, _)| id == &running.collection_id)
            })
            .map(|running| {
                (
                    running.collection_id.clone(),
                    running.collection_name.clone(),
                )
            })
            .collect();
        let rows: Vec<_> = collections
            .into_iter()
            .chain(others)
            .map(|(id, name)| {
                let running = self.mocks.server(&id);
                let routes = if running.is_none() {
                    self.mock_routes_for(&id).len()
                } else {
                    running.map(|r| r.server.routes().len()).unwrap_or(0)
                };
                let selected = focus.as_ref() == Some(&id);
                let select_id = id.clone();
                let action_id = id.clone();
                let action = match running {
                    Some(_) => {
                        outline_chip(format!("workbench-mock-stop-{}", id.as_str()), "Stop", cx)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.stop_mock(&action_id, cx)),
                            )
                            .into_any_element()
                    }
                    None => {
                        outline_chip(format!("workbench-mock-start-{}", id.as_str()), "Start", cx)
                            .disabled(routes == 0)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.start_mock(action_id.clone(), cx)
                            }))
                            .into_any_element()
                    }
                };
                div()
                    .id(SharedString::from(format!(
                        "workbench-mock-row-{}",
                        id.as_str()
                    )))
                    .flex()
                    .flex_col()
                    .gap(rpx(6.))
                    .p(rpx(10.))
                    .rounded(radius::md())
                    .border_1()
                    .border_color(if selected { colors.ring } else { colors.border })
                    .when(selected, |el| el.bg(colors.muted))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.mocks.selected = Some(select_id.clone());
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::SP_2)
                            .child(super::view::dot(
                                7.,
                                if running.is_some() {
                                    colors.success
                                } else {
                                    palette::text_tertiary(cx)
                                },
                            ))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(rpx(0.))
                                    .truncate()
                                    .text_size(text::S12)
                                    .font_weight(text::weight::MEDIUM)
                                    .child(name),
                            )
                            .child(action),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::SP_2)
                            .text_size(text::S11)
                            .text_color(palette::text_tertiary(cx))
                            .child(match routes {
                                0 => "No saved examples".to_string(),
                                1 => "1 route".to_string(),
                                n => format!("{n} routes"),
                            })
                            .when_some(running, |el, running| {
                                let url = running.server.url();
                                el.child(div().flex_1())
                                    .child(
                                        div()
                                            .font_family(crate::api::compat::fonts::mono(cx))
                                            .text_color(colors.success)
                                            .child(url.clone()),
                                    )
                                    .child(self.copy_row_icon(
                                        format!(
                                            "workbench-mock-copy-{}",
                                            running.collection_id.as_str()
                                        ),
                                        url,
                                        cx,
                                    ))
                            }),
                    )
            })
            .collect();
        div()
            .flex_none()
            .w(rpx(340.))
            .min_h(rpx(0.))
            .border_r_1()
            .border_color(colors.border)
            .overflow_y_scrollbar()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::SP_3)
                    .p(space::SP_4)
                    .child(heading("Mock servers", colors.muted_foreground))
                    .child(
                        div()
                            .text_size(text::S11)
                            .line_height(gpui_kit::relative(1.5))
                            .text_color(palette::text_tertiary(cx))
                            .child(
                                "Serve a collection's saved examples on this computer. \
                                 Point an app or a test suite at the URL; nothing leaves \
                                 your machine.",
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(space::SP_2)
                            .text_size(text::S12)
                            .text_color(palette::text_secondary(cx))
                            .child("Port")
                            .child(
                                mono_field(&self.mocks.port, window, cx)
                                    .w(rpx(140.))
                                    .h(rpx(24.))
                                    .text_size(text::S11),
                            ),
                    )
                    .when_some(self.mocks.error.clone(), |el, error| {
                        el.child(
                            div()
                                .debug_selector(|| "workbench-mock-error".into())
                                .child(note_card(colors.danger, Some("alert"), error, cx)),
                        )
                    })
                    .children(rows),
            )
            .into_any_element()
    }

    fn render_mock_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let Some(focus) = self.mock_focus() else {
            return div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_size(text::S12)
                .text_color(palette::text_tertiary(cx))
                .child("Create a collection to mock it.")
                .into_any_element();
        };
        let running = self.mocks.server(&focus);
        let routes: Vec<MockRoute> = match running {
            Some(running) => running.server.routes().to_vec(),
            None => self.mock_routes_for(&focus),
        };
        let route_cols = [Col::Px(64.), Col::Flex, Col::Flex];
        let routes_table = table(cx)
            .debug_selector(|| "workbench-mock-routes".into())
            .child(table_row(
                &route_cols,
                header_cells(&["METHOD", "PATH", "EXAMPLES"]),
                true,
                cx,
            ))
            .children(routes.iter().map(|route| {
                let tint = self.method_tint_label(&route.method, cx);
                let examples = route
                    .responses
                    .iter()
                    .map(|response| format!("{} {}", response.status, response.name))
                    .collect::<Vec<_>>()
                    .join(" · ");
                table_row(
                    &route_cols,
                    vec![
                        verb(route.method.clone(), tint, 52., cx).into_any_element(),
                        cell(route.path.clone()),
                        cell(examples),
                    ],
                    false,
                    cx,
                )
            }));
        let hits = running.map(|r| r.server.hits()).unwrap_or_default();
        let hit_cols = [
            Col::Px(84.),
            Col::Px(64.),
            Col::Flex,
            Col::Px(56.),
            Col::Flex,
        ];
        let now = now_millis();
        let log = table(cx)
            .debug_selector(|| "workbench-mock-log".into())
            .child(table_row(
                &hit_cols,
                header_cells(&["WHEN", "METHOD", "PATH", "STATUS", "EXAMPLE"]),
                true,
                cx,
            ))
            .children(hits.iter().map(|hit| {
                table_row(
                    &hit_cols,
                    vec![
                        cell(pretty::relative_time(hit.at_ms, now)),
                        cell(hit.method.clone()),
                        cell(hit.path.clone()),
                        tinted_cell(
                            hit.status.to_string(),
                            super::view::status_tint(hit.status, cx),
                        ),
                        cell(hit.example.clone().unwrap_or_else(|| "—".into())),
                    ],
                    false,
                    cx,
                )
            }));
        let url = running.map(|running| running.server.url());
        div()
            .id("workbench-mock-detail")
            .flex()
            .flex_col()
            .flex_1()
            .min_w(rpx(0.))
            .min_h(rpx(0.))
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::SP_4)
                    .p(space::SP_4)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::SP_2)
                            .child(match &url {
                                Some(url) => pill(format!("Serving at {url}"), colors.success, cx)
                                    .debug_selector(|| "workbench-mock-url".into()),
                                None => pill("Stopped", palette::text_tertiary(cx), cx),
                            })
                            .child(div().flex_1())
                            .when(running.is_none() && !routes.is_empty(), |el| {
                                let id = focus.clone();
                                el.child(
                                    accent_button(
                                        "workbench-mock-start".into(),
                                        "Start mock server",
                                        28.,
                                        cx,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| this.start_mock(id.clone(), cx),
                                    )),
                                )
                            }),
                    )
                    .when(routes.is_empty(), |el| {
                        el.child(note_card(
                            palette::warm(cx),
                            Some("alert"),
                            "No saved examples in this collection. Send a request and use \
                             “Save example”; imported Postman examples count too.",
                            cx,
                        ))
                    })
                    .when(!routes.is_empty(), |el| {
                        el.child(heading(
                            format!("Routes · {}", routes.len()),
                            colors.muted_foreground,
                        ))
                        .child(routes_table)
                        .child(
                            div()
                                .text_size(text::S11)
                                .line_height(gpui_kit::relative(1.5))
                                .text_color(palette::text_tertiary(cx))
                                .child(
                                    "Path segments like :id, {id} and {{id}} match any value. A \
                                     call gets the first 2xx example unless it sends \
                                     x-mock-response-name or x-mock-response-code. CORS is \
                                     open, so browser apps can call it directly. Restart the \
                                     server to pick up new examples.",
                                ),
                        )
                    })
                    .when(running.is_some(), |el| {
                        el.child(heading(
                            format!(
                                "Calls · {}",
                                running.map(|r| r.server.total_hits()).unwrap_or(0)
                            ),
                            colors.muted_foreground,
                        ))
                        .child(if hits.is_empty() {
                            div()
                                .text_size(text::S11)
                                .text_color(palette::text_tertiary(cx))
                                .child(format!(
                                    "No calls yet. Try: curl {}{}",
                                    url.clone().unwrap_or_default(),
                                    routes.first().map(|r| r.path.as_str()).unwrap_or("/")
                                ))
                        } else {
                            log
                        })
                    }),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::parse_port;

    #[test]
    fn a_blank_port_means_any_free_one() {
        assert_eq!(parse_port(""), Ok(0));
        assert_eq!(parse_port(" 4010 "), Ok(4010));
        assert!(parse_port("0").is_err());
        assert!(parse_port("70000").is_err());
        assert!(parse_port("http").is_err());
    }
}
