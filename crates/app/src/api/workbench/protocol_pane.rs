//! WebSocket, Server-Sent Events and gRPC requests in the Compose tab.
//!
//! A streaming request keeps its URL, headers, auth and variables in the
//! panel's ordinary editors; what only its kind needs lives in a
//! [`ProtocolPane`] per request tab (the composer's first sub-tab) and is
//! saved in the request's `extensions` ([`switchyard_api::protocol`]).
//! Connecting compiles the request like an HTTP send (variables, auth,
//! pre-request script, sign-in), then runs the session on core's tokio
//! runtime; its events stream into the pane's [`ProtocolLog`], which takes
//! the response pane's place.

use std::collections::{HashMap, VecDeque};

use gpui_kit::component::{
    Disableable, Sizable,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
};
use gpui_kit::{
    AnyElement, Div, Hsla, Subscription, UniformListScrollHandle, div, prelude::*, uniform_list,
};
use switchyard_api::protocol::{
    Direction, EntryKind, LogEntry, MAX_LOG_ENTRIES, MessageFormat, MessageSnippet,
    ProtocolSettings, RequestProtocol, SessionCommand, SessionHandle, SessionState, SseSettings,
    StreamEvent, StreamRequest, WebSocketSettings, compile_url, encode_message, start_session,
    wire_url,
};

use super::view::{chip, code_box, heading, mono_field, outline_chip};
use super::*;
use crate::appearance::rpx;

/// What a pane tells the panel.
pub(super) enum ProtocolPaneEvent {
    /// The saved settings changed: the request is unsaved.
    Changed,
}

/// The per-tab state of a streaming request: its settings editors and its
/// live session.
pub(super) struct ProtocolPane {
    /// The settings as last read from the editors (and the snippet list).
    settings: ProtocolSettings,
    message: Entity<TextareaState>,
    subprotocols: Entity<InputState>,
    snippet_name: Entity<InputState>,
    close_code: Entity<InputState>,
    close_reason: Entity<InputState>,
    ping_payload: Entity<InputState>,
    retry_ms: Entity<InputState>,
    max_reconnects: Entity<InputState>,
    /// The live log, drawn in the response pane.
    pub log: Entity<ProtocolLog>,
    session: Option<SessionHandle>,
    /// Compiling the request (auth, scripts) before connecting.
    preparing: bool,
    /// A composer error (bad hex, not connected, …).
    notice: Option<String>,
    _events: Option<gpui_kit::Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ProtocolPaneEvent> for ProtocolPane {}

impl ProtocolPane {
    pub fn new(settings: ProtocolSettings, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let message = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Message to send")
                .auto_grow(4, 12)
        });
        let line = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let subprotocols = line("graphql-transport-ws, chat.v1", window, cx);
        let snippet_name = line("Snippet name", window, cx);
        let close_code = line("1000", window, cx);
        let close_reason = line("Reason", window, cx);
        let ping_payload = line("Ping payload (optional)", window, cx);
        let retry_ms = line("3000", window, cx);
        let max_reconnects = line("10", window, cx);
        let log = cx.new(|cx| ProtocolLog::new(settings.protocol(), window, cx));
        let mut subscriptions = Vec::new();
        for input in [&subprotocols, &retry_ms, &max_reconnects] {
            subscriptions.push(cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.read_editors(cx);
                }
            }));
        }
        subscriptions.push(cx.subscribe(&message, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.notice = None;
                this.read_editors(cx);
            }
        }));
        let mut pane = Self {
            settings: settings.clone(),
            message,
            subprotocols,
            snippet_name,
            close_code,
            close_reason,
            ping_payload,
            retry_ms,
            max_reconnects,
            log,
            session: None,
            preparing: false,
            notice: None,
            _events: None,
            _subscriptions: subscriptions,
        };
        pane.load_settings(settings, window, cx);
        pane
    }

    /// The request kind.
    pub fn protocol(&self) -> RequestProtocol {
        self.settings.protocol()
    }

    /// The settings to save with the request.
    pub fn settings(&self) -> &ProtocolSettings {
        &self.settings
    }

    /// Show `settings` in the editors (a saved request was opened or its
    /// edits discarded). A live session keeps running.
    pub fn load_settings(
        &mut self,
        settings: ProtocolSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Set first: the editors' change events then read back equal settings
        // and do not mark the request unsaved.
        self.settings = settings.clone();
        match &settings {
            ProtocolSettings::WebSocket(ws) => {
                self.message.set_text(ws.message.clone(), window, cx);
                self.subprotocols
                    .set_text(ws.subprotocols.join(", "), window, cx);
            }
            ProtocolSettings::Sse(sse) => {
                self.retry_ms.set_text(sse.retry_ms.to_string(), window, cx);
                self.max_reconnects
                    .set_text(sse.max_reconnects.to_string(), window, cx);
            }
            ProtocolSettings::Grpc(_) => {}
        }
        cx.notify();
    }

    /// Fold the editors into the settings; a real change marks the request
    /// unsaved.
    fn read_editors(&mut self, cx: &mut Context<Self>) {
        let mut next = self.settings.clone();
        match &mut next {
            ProtocolSettings::WebSocket(ws) => {
                ws.message = self.message.read(cx).value().to_string();
                ws.subprotocols = self
                    .subprotocols
                    .read(cx)
                    .value()
                    .split(',')
                    .map(|protocol| protocol.trim().to_owned())
                    .filter(|protocol| !protocol.is_empty())
                    .collect();
            }
            ProtocolSettings::Sse(sse) => {
                if let Ok(retry) = self.retry_ms.read(cx).value().trim().parse::<u64>() {
                    sse.retry_ms = retry.clamp(100, 600_000);
                }
                if let Ok(max) = self.max_reconnects.read(cx).value().trim().parse::<u32>() {
                    sse.max_reconnects = max;
                }
            }
            ProtocolSettings::Grpc(_) => {}
        }
        self.replace_settings(next, cx);
    }

    fn replace_settings(&mut self, next: ProtocolSettings, cx: &mut Context<Self>) {
        if next != self.settings {
            self.settings = next;
            cx.emit(ProtocolPaneEvent::Changed);
            cx.notify();
        }
    }

    fn update_websocket(
        &mut self,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut WebSocketSettings),
    ) {
        let mut next = self.settings.clone();
        if let ProtocolSettings::WebSocket(ws) = &mut next {
            edit(ws);
        }
        self.replace_settings(next, cx);
    }

    fn update_sse(&mut self, cx: &mut Context<Self>, edit: impl FnOnce(&mut SseSettings)) {
        let mut next = self.settings.clone();
        if let ProtocolSettings::Sse(sse) = &mut next {
            edit(sse);
        }
        self.replace_settings(next, cx);
    }

    /// Whether a session is running or being prepared.
    pub fn is_live(&self) -> bool {
        self.preparing || self.session.is_some()
    }

    /// Whether messages can be sent now.
    fn is_open(&self, cx: &App) -> bool {
        self.session.is_some() && matches!(self.log.read(cx).state, Some(SessionState::Open { .. }))
    }

    /// The URL row's button label.
    pub fn action_label(&self) -> &'static str {
        match (self.protocol(), self.is_live()) {
            (_, true) if self.preparing => "Preparing…",
            (RequestProtocol::WebSocket, true) => "Disconnect",
            (RequestProtocol::Sse, true) => "Stop",
            (RequestProtocol::Grpc, true) => "Cancel",
            (RequestProtocol::WebSocket, false) => "Connect",
            (RequestProtocol::Sse, false) => "Listen",
            (RequestProtocol::Grpc, false) => "Invoke",
            (RequestProtocol::Http, _) => "Send",
        }
    }

    /// The panel is compiling the request.
    pub fn begin_preparing(&mut self, cx: &mut Context<Self>) {
        self.preparing = true;
        self.notice = None;
        self.log.update(cx, |log, cx| log.reset(cx));
        cx.notify();
    }

    /// Compiling failed; nothing connected.
    pub fn fail_preparing(&mut self, error: String, cx: &mut Context<Self>) {
        self.preparing = false;
        self.log.update(cx, |log, cx| {
            log.apply(vec![StreamEvent::State(SessionState::Failed(error))], cx);
        });
        cx.notify();
    }

    /// Start the compiled request on core's runtime and stream its events
    /// into the log.
    pub fn start(&mut self, mut request: StreamRequest, cx: &mut Context<Self>) {
        self.preparing = false;
        request.settings = self.settings.clone();
        let Some(runtime) = crate::api::compat::tokio_handle() else {
            self.fail_preparing("The background runtime is not running.".into(), cx);
            return;
        };
        let (handle, mut events) = match start_session(&runtime, request) {
            Ok(started) => started,
            Err(error) => {
                self.fail_preparing(error.to_string(), cx);
                return;
            }
        };
        self.session = Some(handle);
        let log = self.log.downgrade();
        self._events = Some(cx.spawn(async move |this, cx| {
            while let Some(first) = events.recv().await {
                let mut batch = vec![first];
                while batch.len() < 2_000 {
                    match events.try_recv() {
                        Ok(event) => batch.push(event),
                        Err(_) => break,
                    }
                }
                let finished = batch
                    .iter()
                    .any(|event| matches!(event, StreamEvent::State(state) if state.is_finished()));
                if log.update(cx, |log, cx| log.apply(batch, cx)).is_err() {
                    return;
                }
                if finished {
                    break;
                }
            }
            let _ = this.update(cx, |pane, cx| {
                pane.session = None;
                pane._events = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Stop the session (or forget a pending preparation).
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.preparing = false;
        if let Some(session) = &self.session {
            session.cancel();
        }
        cx.notify();
    }

    fn send_payload(&mut self, format: MessageFormat, body: &str, cx: &mut Context<Self>) {
        let result = encode_message(format, body)
            .map_err(|error| error.to_string())
            .and_then(|payload| match &self.session {
                Some(session) if self.is_open(cx) => session
                    .send(SessionCommand::Send(payload))
                    .map_err(|error| error.to_string()),
                _ => Err("Connect first.".into()),
            });
        self.notice = result.err();
        cx.notify();
    }

    fn send_message(&mut self, cx: &mut Context<Self>) {
        let ProtocolSettings::WebSocket(ws) = &self.settings else {
            return;
        };
        let (format, body) = (ws.format, ws.message.clone());
        self.send_payload(format, &body, cx);
    }

    fn send_snippet(&mut self, index: usize, cx: &mut Context<Self>) {
        let ProtocolSettings::WebSocket(ws) = &self.settings else {
            return;
        };
        if let Some(snippet) = ws.snippets.get(index).cloned() {
            self.send_payload(snippet.format, &snippet.body, cx);
        }
    }

    fn load_snippet(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let ProtocolSettings::WebSocket(ws) = &self.settings else {
            return;
        };
        if let Some(snippet) = ws.snippets.get(index).cloned() {
            self.message.set_text(snippet.body.clone(), window, cx);
            self.update_websocket(cx, |ws| {
                ws.format = snippet.format;
                ws.message = snippet.body;
            });
        }
    }

    fn save_snippet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ProtocolSettings::WebSocket(ws) = &self.settings else {
            return;
        };
        if ws.message.trim().is_empty() {
            self.notice = Some("Write a message to save it.".into());
            cx.notify();
            return;
        }
        if let Err(error) = encode_message(ws.format, &ws.message) {
            self.notice = Some(error.to_string());
            cx.notify();
            return;
        }
        let typed = self.snippet_name.read(cx).value().trim().to_owned();
        let name = if typed.is_empty() {
            format!("Message {}", ws.snippets.len() + 1)
        } else {
            typed
        };
        self.snippet_name.set_text(String::new(), window, cx);
        self.update_websocket(cx, |ws| {
            let snippet = MessageSnippet {
                name: name.clone(),
                format: ws.format,
                body: ws.message.clone(),
            };
            match ws.snippets.iter_mut().find(|saved| saved.name == name) {
                Some(saved) => *saved = snippet,
                None => ws.snippets.push(snippet),
            }
        });
    }

    fn delete_snippet(&mut self, index: usize, cx: &mut Context<Self>) {
        self.update_websocket(cx, |ws| {
            if index < ws.snippets.len() {
                ws.snippets.remove(index);
            }
        });
    }

    fn ping(&mut self, cx: &mut Context<Self>) {
        let payload = self.ping_payload.read(cx).value().as_bytes().to_vec();
        self.notice = match &self.session {
            Some(session) if self.is_open(cx) => session
                .send(SessionCommand::Ping(payload))
                .err()
                .map(|error| error.to_string()),
            _ => Some("Connect first.".into()),
        };
        cx.notify();
    }

    fn close_with_code(&mut self, cx: &mut Context<Self>) {
        let code_text = self.close_code.read(cx).value().trim().to_owned();
        let code = if code_text.is_empty() {
            Ok(1000)
        } else {
            code_text
                .parse::<u16>()
                .ok()
                .filter(|code| close_code_is_sendable(*code))
                .ok_or_else(|| "A close code is 1000–1003, 1007–1014 or 3000–4999.".to_string())
        };
        let reason = self.close_reason.read(cx).value().to_string();
        self.notice = match (code, &self.session) {
            (Err(error), _) => Some(error),
            (Ok(code), Some(session)) if self.is_open(cx) => session
                .send(SessionCommand::Close { code, reason })
                .err()
                .map(|error| error.to_string()),
            _ => Some("Connect first.".into()),
        };
        cx.notify();
    }
}

/// Close codes a client may send (RFC 6455 §7.4).
fn close_code_is_sendable(code: u16) -> bool {
    matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999)
}

impl Render for ProtocolPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.settings.clone() {
            ProtocolSettings::WebSocket(ws) => self.render_websocket(ws, window, cx),
            ProtocolSettings::Sse(sse) => self.render_sse(sse, window, cx),
            ProtocolSettings::Grpc(_) => div()
                .text_size(text::S11)
                .child("gRPC is not available in this build.")
                .into_any_element(),
        };
        div()
            .id("workbench-protocol-composer")
            .debug_selector(|| "workbench-protocol-composer".into())
            .flex()
            .flex_col()
            .gap(space::SP_3)
            .child(body)
            .when_some(self.notice.clone(), |el, notice| {
                el.child(
                    div()
                        .text_size(text::S11)
                        .text_color(cx.theme().colors.danger)
                        .child(notice),
                )
            })
    }
}

impl ProtocolPane {
    fn render_websocket(
        &mut self,
        ws: WebSocketSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors;
        let open = self.is_open(cx);
        let format_chips =
            div()
                .flex()
                .items_center()
                .gap(rpx(2.))
                .children(MessageFormat::ALL.into_iter().map(|format| {
                    chip(
                        format!("workbench-ws-format-{}", format.label()),
                        format.label(),
                        ws.format == format,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.update_websocket(cx, |ws| ws.format = format);
                    }))
                }));
        let snippets = ws.snippets.iter().enumerate().map(|(index, snippet)| {
            div()
                .flex()
                .items_center()
                .gap(space::SP_2)
                .py(rpx(2.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(text::S11)
                        .child(format!("{} · {}", snippet.name, snippet.format.label())),
                )
                .child(
                    outline_chip(format!("workbench-ws-snippet-load-{index}"), "Load", cx)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.load_snippet(index, window, cx)
                        })),
                )
                .child(
                    outline_chip(format!("workbench-ws-snippet-send-{index}"), "Send", cx)
                        .disabled(!open)
                        .on_click(cx.listener(move |this, _, _, cx| this.send_snippet(index, cx))),
                )
                .child(
                    outline_chip(format!("workbench-ws-snippet-delete-{index}"), "Delete", cx)
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.delete_snippet(index, cx)),
                        ),
                )
        });
        div()
            .flex()
            .flex_col()
            .gap(space::SP_3)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(space::SP_2)
                    .child(format_chips)
                    .child(div().flex_1())
                    .child(
                        Button::new("workbench-ws-send")
                            .debug_selector(|| "workbench-ws-send".into())
                            .small()
                            .primary()
                            .label("Send")
                            .disabled(!open)
                            .on_click(cx.listener(|this, _, _, cx| this.send_message(cx))),
                    ),
            )
            .child(
                div()
                    .rounded(radius::sm())
                    .border_1()
                    .border_color(colors.input)
                    .bg(colors.sidebar)
                    .px(rpx(10.))
                    .py(rpx(6.))
                    .font_family(crate::api::compat::fonts::mono(cx))
                    .text_size(text::S12)
                    .child(field::bare(&self.message).w_full()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::SP_2)
                    .child(mono_field(&self.snippet_name, window, cx).flex_1())
                    .child(
                        outline_chip("workbench-ws-snippet-save".into(), "Save as snippet", cx)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.save_snippet(window, cx)),
                            ),
                    ),
            )
            .when(!ws.snippets.is_empty(), |el| {
                el.child(heading("Saved messages", colors.muted_foreground))
                    .child(div().flex().flex_col().children(snippets))
            })
            .child(heading("Connection", colors.muted_foreground))
            .child(labelled(
                "Subprotocols",
                mono_field(&self.subprotocols, window, cx).flex_1(),
                cx,
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::SP_2)
                    .child(mono_field(&self.ping_payload, window, cx).flex_1())
                    .child(
                        outline_chip("workbench-ws-ping".into(), "Ping", cx)
                            .disabled(!open)
                            .on_click(cx.listener(|this, _, _, cx| this.ping(cx))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::SP_2)
                    .child(mono_field(&self.close_code, window, cx).w(rpx(80.)))
                    .child(mono_field(&self.close_reason, window, cx).flex_1())
                    .child(
                        outline_chip("workbench-ws-close".into(), "Close", cx)
                            .disabled(!open)
                            .on_click(cx.listener(|this, _, _, cx| this.close_with_code(cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_sse(
        &mut self,
        sse: SseSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(space::SP_3)
            .child(
                div()
                    .text_size(text::S11)
                    .text_color(palette::text_secondary(cx))
                    .child(
                        "The method, Params, Headers, Auth and Body tabs apply as for HTTP; \
                         Accept: text/event-stream is added unless a header sets it.",
                    ),
            )
            .child(
                Checkbox::new("workbench-sse-reconnect")
                    .label("Reconnect when the stream ends")
                    .checked(sse.reconnect)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        let checked = *checked;
                        this.update_sse(cx, |sse| sse.reconnect = checked);
                    })),
            )
            .child(
                Checkbox::new("workbench-sse-last-event-id")
                    .label("Send Last-Event-ID when reconnecting")
                    .checked(sse.send_last_event_id)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        let checked = *checked;
                        this.update_sse(cx, |sse| sse.send_last_event_id = checked);
                    })),
            )
            .child(labelled(
                "Retry (ms)",
                mono_field(&self.retry_ms, window, cx).w(rpx(120.)),
                cx,
            ))
            .child(labelled(
                "Max reconnects",
                mono_field(&self.max_reconnects, window, cx).w(rpx(120.)),
                cx,
            ))
            .into_any_element()
    }
}

/// `label  field` on one row.
fn labelled(label: &'static str, field: impl IntoElement, cx: &App) -> Div {
    div()
        .flex()
        .items_center()
        .gap(space::SP_2)
        .child(
            div()
                .w(rpx(110.))
                .flex_none()
                .text_size(text::S11)
                .text_color(palette::text_secondary(cx))
                .child(label),
        )
        .child(field)
}

/// Which list the log shows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LogTab {
    Messages,
    Headers,
}

/// A session's live log: virtualized message rows with a filter, the
/// response headers and trailers, and the selected message in full.
pub(super) struct ProtocolLog {
    protocol: RequestProtocol,
    entries: VecDeque<LogEntry>,
    /// Entries dropped from the front to stay under [`MAX_LOG_ENTRIES`].
    dropped: usize,
    state: Option<SessionState>,
    headers: Vec<(String, String)>,
    trailers: Vec<(String, String)>,
    filter: Entity<InputState>,
    tab: LogTab,
    /// Index into `entries` of the row shown in full.
    selected: Option<usize>,
    /// Keep the newest row in view.
    follow: bool,
    sent: usize,
    received: usize,
    scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl ProtocolLog {
    fn new(protocol: RequestProtocol, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter messages"));
        let subscriptions = vec![cx.subscribe(&filter, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        })];
        Self {
            protocol,
            entries: VecDeque::new(),
            dropped: 0,
            state: None,
            headers: Vec::new(),
            trailers: Vec::new(),
            filter,
            tab: LogTab::Messages,
            selected: None,
            follow: true,
            sent: 0,
            received: 0,
            scroll: UniformListScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }

    /// Clear for a new session.
    fn reset(&mut self, cx: &mut Context<Self>) {
        self.entries.clear();
        self.dropped = 0;
        self.state = Some(SessionState::Connecting);
        self.headers.clear();
        self.trailers.clear();
        self.selected = None;
        self.sent = 0;
        self.received = 0;
        self.follow = true;
        cx.notify();
    }

    /// Append a batch of session events.
    fn apply(&mut self, events: Vec<StreamEvent>, cx: &mut Context<Self>) {
        for event in events {
            match event {
                StreamEvent::State(state) => self.state = Some(state),
                StreamEvent::Log(entry) => {
                    match entry.direction {
                        Direction::Sent => self.sent += 1,
                        Direction::Received => self.received += 1,
                        Direction::Info => {}
                    }
                    self.entries.push_back(entry);
                }
                StreamEvent::ResponseHeaders(headers) => self.headers = headers,
                StreamEvent::Trailers(trailers) => self.trailers = trailers,
            }
        }
        let overflow = self.entries.len().saturating_sub(MAX_LOG_ENTRIES);
        if overflow > 0 {
            self.entries.drain(..overflow);
            self.dropped += overflow;
            self.selected = self
                .selected
                .and_then(|selected| selected.checked_sub(overflow));
        }
        if self.follow && !self.entries.is_empty() {
            self.scroll.scroll_to_bottom();
        }
        cx.notify();
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.entries.clear();
        self.selected = None;
        self.dropped = 0;
        cx.notify();
    }

    /// Indices of the entries that match the filter (all when it is empty).
    fn visible(&self, cx: &App) -> Vec<usize> {
        let filter = self.filter.read(cx).value().trim().to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry_matches(entry, &filter))
            .map(|(index, _)| index)
            .collect()
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let Some(entry) = self.entries.get(index) else {
            return div().into_any_element();
        };
        let (arrow, tint) = match (entry.direction, entry.kind) {
            (_, EntryKind::Error) => ("!", colors.danger),
            (Direction::Sent, _) => ("↑", colors.primary),
            (Direction::Received, _) => ("↓", palette::warm(cx)),
            (Direction::Info, _) => ("•", colors.muted_foreground),
        };
        let selected = self.selected == Some(index);
        let preview = entry.text.lines().next().unwrap_or_default().to_owned();
        div()
            .id(("workbench-protocol-row", index))
            .flex()
            .items_center()
            .gap(space::SP_2)
            .h(rpx(24.))
            .px(space::SP_2)
            .cursor_pointer()
            .when(selected, |el| el.bg(colors.accent))
            .font_family(crate::api::compat::fonts::mono(cx))
            .text_size(text::S11)
            .child(
                div()
                    .w(rpx(84.))
                    .flex_none()
                    .text_color(colors.muted_foreground)
                    .child(clock(entry.at_ms)),
            )
            .child(div().w(rpx(12.)).flex_none().text_color(tint).child(arrow))
            .when(!entry.label.is_empty(), |el| {
                el.child(
                    div()
                        .flex_none()
                        .max_w(rpx(120.))
                        .truncate()
                        .text_color(tint)
                        .child(entry.label.clone()),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(if entry.direction == Direction::Info {
                        colors.muted_foreground
                    } else {
                        colors.foreground
                    })
                    .child(preview),
            )
            .when(entry.direction != Direction::Info, |el| {
                el.child(
                    div()
                        .flex_none()
                        .text_color(colors.muted_foreground)
                        .child(format_size(entry.size)),
                )
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected = if this.selected == Some(index) {
                    None
                } else {
                    Some(index)
                };
                cx.notify();
            }))
            .into_any_element()
    }

    fn status_line(&self, cx: &App) -> (String, Hsla) {
        let colors = cx.theme().colors;
        match &self.state {
            None => (
                format!("Not connected · {}", self.protocol.name()),
                colors.muted_foreground,
            ),
            Some(SessionState::Connecting) => ("Connecting…".into(), palette::warm(cx)),
            Some(SessionState::Open {
                status,
                subprotocol,
            }) => (
                match subprotocol {
                    Some(protocol) => format!("Open · {status} · {protocol}"),
                    None => format!("Open · {status}"),
                },
                colors.success,
            ),
            Some(SessionState::Reconnecting { attempt, delay_ms }) => (
                format!("Reconnecting in {delay_ms} ms (attempt {attempt})"),
                palette::warm(cx),
            ),
            Some(SessionState::Closed { code, reason }) => (
                match code {
                    Some(code) if reason.is_empty() => format!("Closed · {code}"),
                    Some(code) => format!("Closed · {code} · {reason}"),
                    None if reason.is_empty() => "Closed".into(),
                    None => format!("Closed · {reason}"),
                },
                colors.muted_foreground,
            ),
            Some(SessionState::Failed(error)) => (format!("Failed · {error}"), colors.danger),
        }
    }
}

impl Render for ProtocolLog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors;
        let (status, tint) = self.status_line(cx);
        let visible = self.visible(cx);
        let rows: std::rc::Rc<[usize]> = visible.into();
        let header_rows = self
            .headers
            .iter()
            .map(|(name, value)| ("header", name.clone(), value.clone()))
            .chain(
                self.trailers
                    .iter()
                    .map(|(name, value)| ("trailer", name.clone(), value.clone())),
            )
            .collect::<Vec<_>>();
        let detail = self
            .selected
            .and_then(|index| self.entries.get(index))
            .map(|entry| pretty::format_json(&entry.text).unwrap_or_else(|| entry.text.clone()));
        let count = rows.len();
        div()
            .id("workbench-protocol-log")
            .debug_selector(|| "workbench-protocol-log".into())
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(space::SP_2)
                    .px(space::SP_4)
                    .py(space::SP_2)
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(text::S11)
                            .text_color(tint)
                            .child(status),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(text::S10)
                            .text_color(colors.muted_foreground)
                            .child(format!("↑ {} · ↓ {}", self.sent, self.received)),
                    )
                    .child(
                        chip(
                            "workbench-protocol-messages".into(),
                            "Messages",
                            self.tab == LogTab::Messages,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.tab = LogTab::Messages;
                            cx.notify();
                        })),
                    )
                    .child(
                        chip(
                            "workbench-protocol-headers".into(),
                            format!("Headers ({})", header_rows.len()),
                            self.tab == LogTab::Headers,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.tab = LogTab::Headers;
                            cx.notify();
                        })),
                    ),
            )
            .when(self.tab == LogTab::Messages, |el| {
                el.child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(space::SP_2)
                        .px(space::SP_4)
                        .py(space::SP_2)
                        .child(mono_field(&self.filter, window, cx).flex_1())
                        .child(
                            Checkbox::new("workbench-protocol-follow")
                                .label("Follow")
                                .checked(self.follow)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    this.follow = *checked;
                                    cx.notify();
                                })),
                        )
                        .child(
                            outline_chip("workbench-protocol-clear".into(), "Clear", cx)
                                .on_click(cx.listener(|this, _, _, cx| this.clear(cx))),
                        ),
                )
                .when(self.dropped > 0, |el| {
                    el.child(
                        div()
                            .px(space::SP_4)
                            .text_size(text::S10)
                            .text_color(colors.muted_foreground)
                            .child(format!(
                                "{} older messages were dropped (the log keeps {MAX_LOG_ENTRIES}).",
                                self.dropped
                            )),
                    )
                })
                .child(
                    div()
                        .flex_1()
                        .min_h(rpx(60.))
                        .child(
                            uniform_list(
                                "workbench-protocol-rows",
                                count,
                                cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                                    range
                                        .filter_map(|row| rows.get(row).copied())
                                        .map(|index| this.render_row(index, cx))
                                        .collect::<Vec<_>>()
                                }),
                            )
                            .track_scroll(&self.scroll)
                            .size_full(),
                        )
                        .when(count == 0, |el| {
                            el.child(
                                div()
                                    .px(space::SP_4)
                                    .py(space::SP_2)
                                    .text_size(text::S11)
                                    .text_color(colors.muted_foreground)
                                    .child("No messages yet."),
                            )
                        }),
                )
                .when_some(detail, |el, detail| {
                    el.child(
                        div()
                            .id("workbench-protocol-detail")
                            .flex_none()
                            .max_h(rpx(220.))
                            .overflow_y_scroll()
                            .px(space::SP_4)
                            .py(space::SP_2)
                            .border_t_1()
                            .border_color(colors.border)
                            .child(code_box(cx).whitespace_normal().child(detail)),
                    )
                })
            })
            .when(self.tab == LogTab::Headers, |el| {
                el.child(
                    div()
                        .id("workbench-protocol-header-list")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .px(space::SP_4)
                        .py(space::SP_2)
                        .font_family(crate::api::compat::fonts::mono(cx))
                        .text_size(text::S11)
                        .when(header_rows.is_empty(), |el| {
                            el.text_color(colors.muted_foreground)
                                .child("No headers received yet.")
                        })
                        .children(header_rows.into_iter().map(|(kind, name, value)| {
                            div()
                                .flex()
                                .gap(space::SP_2)
                                .child(
                                    div()
                                        .flex_none()
                                        .text_color(colors.muted_foreground)
                                        .child(format!("{kind} {name}:")),
                                )
                                .child(div().flex_1().min_w_0().child(value))
                        })),
                )
            })
    }
}

/// Whether `entry` matches a lower-cased filter.
fn entry_matches(entry: &LogEntry, filter: &str) -> bool {
    filter.is_empty()
        || entry.text.to_lowercase().contains(filter)
        || entry.label.to_lowercase().contains(filter)
        || entry
            .event_id
            .as_deref()
            .is_some_and(|id| id.to_lowercase().contains(filter))
}

/// Local wall-clock time with milliseconds.
fn clock(at_ms: i64) -> String {
    use chrono::TimeZone as _;
    chrono::Local
        .timestamp_millis_opt(at_ms)
        .single()
        .map(|time| time.format("%H:%M:%S%.3f").to_string())
        .unwrap_or_default()
}

/// `512 B`, `1.5 KB`, `2.0 MB`.
fn format_size(bytes: usize) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KB", bytes as f64 / 1024.),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.),
    }
}

/// One pane per request tab that holds a streaming request.
pub(super) struct PaneSlot {
    pub pane: Entity<ProtocolPane>,
    _subscriptions: [Subscription; 2],
}

impl WorkbenchPanel {
    /// The open tab's streaming pane, when it holds a streaming request.
    pub(super) fn active_protocol_pane(&self) -> Option<Entity<ProtocolPane>> {
        self.active_request_tab_id()
            .and_then(|id| self.protocol_panes.get(&id))
            .map(|slot| slot.pane.clone())
    }

    /// What the open tab's request talks.
    pub(super) fn active_protocol(&self, cx: &App) -> RequestProtocol {
        self.active_protocol_pane()
            .map_or(RequestProtocol::Http, |pane| pane.read(cx).protocol())
    }

    /// Give the open tab the pane `settings` call for: none for HTTP, the
    /// same pane (session kept) when the kind is unchanged.
    pub(super) fn sync_protocol_pane(
        &mut self,
        settings: Option<ProtocolSettings>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.active_request_tab_id() else {
            return;
        };
        let Some(settings) = settings else {
            self.protocol_panes.remove(&tab);
            return;
        };
        if let Some(slot) = self.protocol_panes.get(&tab)
            && slot.pane.read(cx).protocol() == settings.protocol()
        {
            slot.pane
                .update(cx, |pane, cx| pane.load_settings(settings, window, cx));
            return;
        }
        if self.composer_tab == ComposerTab::Params || self.composer_tab == ComposerTab::Body {
            self.composer_tab = ComposerTab::Protocol;
        }
        let pane = cx.new(|cx| ProtocolPane::new(settings, window, cx));
        let subscriptions = [
            cx.subscribe(
                &pane,
                |this, _, event: &ProtocolPaneEvent, cx| match event {
                    ProtocolPaneEvent::Changed => {
                        this.dirty = true;
                        cx.notify();
                    }
                },
            ),
            cx.observe(&pane, |_, _, cx| cx.notify()),
        ];
        self.protocol_panes.insert(
            tab,
            PaneSlot {
                pane,
                _subscriptions: subscriptions,
            },
        );
    }

    /// Drop the panes of closed tabs (their sessions stop with them).
    pub(super) fn prune_protocol_panes(&mut self) {
        if self.protocol_panes.len() > self.request_tabs.len()
            || self
                .protocol_panes
                .keys()
                .any(|id| !self.request_tabs.iter().any(|tab| tab.id == *id))
        {
            let open = self
                .request_tabs
                .iter()
                .map(|tab| tab.id)
                .collect::<std::collections::HashSet<_>>();
            self.protocol_panes.retain(|id, _| open.contains(id));
        }
    }

    /// Write the open pane's settings into a draft (or make it plain HTTP).
    pub(super) fn apply_protocol_to_draft(&self, request: &mut SavedRequest, cx: &App) {
        let settings = self
            .active_protocol_pane()
            .map(|pane| pane.read(cx).settings().clone());
        request.set_protocol_settings(settings.as_ref());
    }

    /// The rail's `+` items for streaming requests.
    pub(super) fn new_protocol_request(
        &mut self,
        protocol: RequestProtocol,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.create_seeded_request_in_selection(
            Some(empty_state::RequestSeed::protocol(protocol)),
            window,
            cx,
        );
        self.composer_tab = ComposerTab::Protocol;
    }

    /// The composer sub-tabs the open request shows.
    pub(super) fn composer_tabs(&self, cx: &App) -> Vec<ComposerTab> {
        match self.active_protocol(cx) {
            RequestProtocol::Http => ComposerTab::ALL.to_vec(),
            RequestProtocol::Sse => vec![
                ComposerTab::Protocol,
                ComposerTab::Params,
                ComposerTab::Headers,
                ComposerTab::Body,
                ComposerTab::Auth,
                ComposerTab::Vars,
                ComposerTab::Tests,
                ComposerTab::Settings,
            ],
            RequestProtocol::WebSocket | RequestProtocol::Grpc => vec![
                ComposerTab::Protocol,
                ComposerTab::Params,
                ComposerTab::Headers,
                ComposerTab::Auth,
                ComposerTab::Vars,
                ComposerTab::Tests,
                ComposerTab::Settings,
            ],
        }
    }

    /// The label of a composer sub-tab for the open request.
    pub(super) fn composer_tab_label(&self, tab: ComposerTab, cx: &App) -> &'static str {
        match (tab, self.active_protocol(cx)) {
            (ComposerTab::Protocol, RequestProtocol::WebSocket) => "Messages",
            (ComposerTab::Protocol, RequestProtocol::Sse) => "Stream",
            (ComposerTab::Protocol, _) => "Call",
            (ComposerTab::Headers, RequestProtocol::Grpc) => "Metadata",
            (other, _) => other.label(),
        }
    }

    /// Stop the open tab's session.
    pub(super) fn stop_protocol(&mut self, cx: &mut Context<Self>) {
        if let Some(pane) = self.active_protocol_pane() {
            pane.update(cx, |pane, cx| pane.stop(cx));
        }
    }

    /// Connect the open streaming request: compile it like an HTTP send
    /// (variables, auth, pre-request script, sign-in) on core's runtime,
    /// then start its session.
    pub(super) fn send_protocol(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self.active_protocol_pane() else {
            return;
        };
        if pane.read(cx).is_live() {
            return;
        }
        let protocol = pane.read(cx).protocol();
        let (mut definition, secrets) = match self.draft_request(cx) {
            Ok(request) => request,
            Err(error) => {
                pane.update(cx, |pane, cx| pane.fail_preparing(error, cx));
                return;
            }
        };
        let Some(data) = self.workspace_data.as_ref() else {
            self.storage_error = Some("Workbench storage is unavailable".into());
            cx.notify();
            return;
        };
        let environment = match self.environment_send_context(cx) {
            Ok(environment) => environment,
            Err(error) => {
                pane.update(cx, |pane, cx| pane.fail_preparing(error, cx));
                return;
            }
        };
        let manual_cookies = match draft::parse_cookie_pairs(&self.cookies.read(cx).value()) {
            Ok(cookies) => cookies,
            Err(error) => {
                pane.update(cx, |pane, cx| pane.fail_preparing(error, cx));
                return;
            }
        };
        // The compiler sends HTTP: give it the http(s) form of the URL and,
        // for WebSocket and gRPC, no body.
        definition.url = compile_url(protocol, &definition.url);
        if protocol != RequestProtocol::Sse {
            definition.method = switchyard_api::HttpMethod::get();
            definition.body = Body::None;
        }
        let collection = data.collection(&definition.collection_id).cloned();
        let folders = folder_chain(&data.folders, &definition)
            .into_iter()
            .cloned()
            .collect();
        let mut secrets = secrets;
        secrets.merge(environment.secrets);
        let input = StandaloneSendInput {
            operation_id: RequestId::new().as_str().to_string(),
            definition,
            collection,
            folders,
            environment: environment.environment,
            environment_source: environment.source,
            environment_scope: environment.scope,
            environment_base_url: environment.base_url,
            environment_auth: environment.auth,
            browser_authorization: None,
            secrets,
            workspace: data.workspace.clone(),
            store: data.store.clone(),
            secret_store: self.secret_store.clone(),
            cookie_jar: self.cookie_jar.clone(),
            manual_cookies,
            file_capabilities: self.file_capabilities.clone(),
            transport: self.transport.clone(),
        };
        pane.update(cx, |pane, cx| pane.begin_preparing(cx));
        let settings = pane.read(cx).settings().clone();
        let weak_pane = pane.downgrade();
        cx.spawn(async move |_, cx| {
            let prepared =
                crate::api::compat::blocking(move || prepare_standalone_send(input)).await;
            let _ = weak_pane.update(cx, |pane, cx| {
                if !pane.preparing {
                    // Stopped while compiling.
                    return;
                }
                match prepared {
                    Ok(prepared) => {
                        let mut request = prepared.prepared;
                        request.url = wire_url(protocol, &request.url);
                        pane.start(
                            StreamRequest {
                                prepared: request,
                                settings,
                            },
                            cx,
                        );
                    }
                    Err(error) => pane.fail_preparing(error, cx),
                }
            });
        })
        .detach();
        cx.notify();
    }
}

/// Keep `HashMap` in the panel's field type readable.
pub(super) type ProtocolPanes = HashMap<u64, PaneSlot>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_codes_follow_rfc_6455() {
        for code in [1000, 1001, 1003, 1007, 1014, 3000, 4999] {
            assert!(close_code_is_sendable(code), "{code}");
        }
        for code in [0, 999, 1004, 1005, 1006, 1015, 2999, 5000] {
            assert!(!close_code_is_sendable(code), "{code}");
        }
    }

    #[test]
    fn filter_matches_text_label_and_id() {
        let mut entry = LogEntry::now(Direction::Received, EntryKind::Text, "tick", "Hello", 5);
        entry.event_id = Some("ID-7".into());
        assert!(entry_matches(&entry, ""));
        assert!(entry_matches(&entry, "hello"));
        assert!(entry_matches(&entry, "tick"));
        assert!(entry_matches(&entry, "id-7"));
        assert!(!entry_matches(&entry, "bye"));
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(format_size(12), "12 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(3 * 1_048_576), "3.0 MB");
    }
}
