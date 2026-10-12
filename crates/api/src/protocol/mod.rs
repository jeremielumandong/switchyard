//! Streaming request kinds beside plain HTTP: WebSocket, Server-Sent Events
//! and gRPC.
//!
//! A saved request keeps its URL, headers, auth, params and variables in the
//! ordinary [`SavedRequest`] fields, so the request compiler resolves
//! variables and auth exactly as it does for HTTP. What only a streaming kind
//! needs (subprotocols, saved message snippets, the gRPC method, …) lives in
//! [`ProtocolSettings`], stored under [`PROTOCOL_EXTENSION_KEY`] in the
//! request's `extensions`, so collections, import/export and history keep
//! working for requests saved before these kinds existed.
//!
//! A connection runs as a session on the host's tokio runtime
//! ([`start_session`]): the caller gets a [`SessionHandle`] to send commands
//! and cancel, and a channel of [`StreamEvent`]s for the live log. The
//! WebSocket and gRPC clients sit behind the `websocket` and `grpc` cargo
//! features (on by default); without one, starting that kind fails with
//! [`ProtocolError::Disabled`].

#[cfg(feature = "grpc")]
pub mod grpc;
pub mod sse;
#[cfg(feature = "websocket")]
pub mod websocket;

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{PreparedRequest, SavedRequest};

/// The `extensions` key a saved request's [`ProtocolSettings`] live under.
pub const PROTOCOL_EXTENSION_KEY: &str = "switchyard_protocol";

/// The most log entries a session keeps text for; older entries are dropped
/// by the viewer, which reads this to size its buffer.
pub const MAX_LOG_ENTRIES: usize = 10_000;

/// Message text kept per log entry for display; the full size is still
/// reported in [`LogEntry::size`].
pub const MAX_LOG_TEXT_BYTES: usize = 64 * 1024;

/// What a request talks: plain HTTP (the default) or a streaming protocol.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestProtocol {
    /// An ordinary HTTP request (GraphQL is an HTTP body mode).
    #[default]
    Http,
    /// A WebSocket connection (`ws://` / `wss://`).
    WebSocket,
    /// A Server-Sent Events stream (`text/event-stream` over HTTP).
    Sse,
    /// A gRPC call over HTTP/2.
    Grpc,
}

impl RequestProtocol {
    /// Every kind, in menu order.
    pub const ALL: [RequestProtocol; 4] = [
        RequestProtocol::Http,
        RequestProtocol::WebSocket,
        RequestProtocol::Sse,
        RequestProtocol::Grpc,
    ];

    /// Short label for badges and menus.
    pub fn label(self) -> &'static str {
        match self {
            RequestProtocol::Http => "HTTP",
            RequestProtocol::WebSocket => "WS",
            RequestProtocol::Sse => "SSE",
            RequestProtocol::Grpc => "gRPC",
        }
    }

    /// Long name.
    pub fn name(self) -> &'static str {
        match self {
            RequestProtocol::Http => "HTTP",
            RequestProtocol::WebSocket => "WebSocket",
            RequestProtocol::Sse => "Server-Sent Events",
            RequestProtocol::Grpc => "gRPC",
        }
    }

    /// Whether this build can open this kind (its cargo feature is on).
    pub fn available(self) -> bool {
        match self {
            RequestProtocol::Http | RequestProtocol::Sse => true,
            RequestProtocol::WebSocket => cfg!(feature = "websocket"),
            RequestProtocol::Grpc => cfg!(feature = "grpc"),
        }
    }
}

/// How a typed message body is turned into bytes on the wire.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageFormat {
    /// UTF-8 text, sent as is.
    #[default]
    Text,
    /// JSON, validated before it is sent as text.
    Json,
    /// Binary written as hex digits (whitespace ignored).
    Hex,
    /// Binary written as standard base64.
    Base64,
}

impl MessageFormat {
    /// Every format, in picker order.
    pub const ALL: [MessageFormat; 4] = [
        MessageFormat::Text,
        MessageFormat::Json,
        MessageFormat::Hex,
        MessageFormat::Base64,
    ];

    /// Picker label.
    pub fn label(self) -> &'static str {
        match self {
            MessageFormat::Text => "Text",
            MessageFormat::Json => "JSON",
            MessageFormat::Hex => "Hex",
            MessageFormat::Base64 => "Base64",
        }
    }
}

/// A message ready for the wire.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutgoingPayload {
    /// A text frame.
    Text(String),
    /// A binary frame.
    Binary(Vec<u8>),
}

impl OutgoingPayload {
    /// Bytes on the wire.
    pub fn len(&self) -> usize {
        match self {
            OutgoingPayload::Text(text) => text.len(),
            OutgoingPayload::Binary(bytes) => bytes.len(),
        }
    }

    /// `true` for an empty message.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Encode `body` written in `format`; JSON must parse, hex and base64 must
/// decode.
pub fn encode_message(format: MessageFormat, body: &str) -> Result<OutgoingPayload, ProtocolError> {
    match format {
        MessageFormat::Text => Ok(OutgoingPayload::Text(body.to_owned())),
        MessageFormat::Json => {
            serde_json::from_str::<serde_json::Value>(body)
                .map_err(|error| ProtocolError::Invalid(format!("message is not JSON: {error}")))?;
            Ok(OutgoingPayload::Text(body.to_owned()))
        }
        MessageFormat::Hex => {
            let digits: String = body.chars().filter(|c| !c.is_whitespace()).collect();
            let digits = digits
                .strip_prefix("0x")
                .or_else(|| digits.strip_prefix("0X"))
                .unwrap_or(&digits);
            hex::decode(digits)
                .map(OutgoingPayload::Binary)
                .map_err(|error| ProtocolError::Invalid(format!("message is not hex: {error}")))
        }
        MessageFormat::Base64 => {
            let compact: String = body.chars().filter(|c| !c.is_whitespace()).collect();
            base64::engine::general_purpose::STANDARD
                .decode(compact)
                .map(OutgoingPayload::Binary)
                .map_err(|error| ProtocolError::Invalid(format!("message is not base64: {error}")))
        }
    }
}

/// A saved message the composer can send again.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct MessageSnippet {
    /// What the snippet list shows.
    pub name: String,
    /// How `body` is encoded.
    #[serde(default)]
    pub format: MessageFormat,
    /// The message text (hex or base64 digits for binary formats).
    #[serde(default)]
    pub body: String,
}

/// WebSocket options saved with a request.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct WebSocketSettings {
    /// Offered in `Sec-WebSocket-Protocol`, in preference order.
    #[serde(default)]
    pub subprotocols: Vec<String>,
    /// The composer's current message.
    #[serde(default)]
    pub message: String,
    /// How the composer's message is encoded.
    #[serde(default)]
    pub format: MessageFormat,
    /// Saved messages.
    #[serde(default)]
    pub snippets: Vec<MessageSnippet>,
}

/// Server-Sent Events options saved with a request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SseSettings {
    /// Reconnect when the stream ends or drops, as browsers do.
    #[serde(default = "default_true")]
    pub reconnect: bool,
    /// Send the last seen event id as `Last-Event-ID` when reconnecting.
    #[serde(default = "default_true")]
    pub send_last_event_id: bool,
    /// Reconnect delay until the server sends `retry:`.
    #[serde(default = "default_retry_ms")]
    pub retry_ms: u64,
    /// Give up after this many reconnects in a row without an event.
    #[serde(default = "default_max_reconnects")]
    pub max_reconnects: u32,
}

impl Default for SseSettings {
    fn default() -> Self {
        Self {
            reconnect: true,
            send_last_event_id: true,
            retry_ms: default_retry_ms(),
            max_reconnects: default_max_reconnects(),
        }
    }
}

const fn default_true() -> bool {
    true
}

const fn default_retry_ms() -> u64 {
    3_000
}

const fn default_max_reconnects() -> u32 {
    10
}

/// Where a gRPC call finds its service definitions.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DescriptorSource {
    /// Ask the server (gRPC server reflection, v1 then v1alpha).
    #[default]
    Reflection,
    /// Compile `.proto` files from disk.
    ProtoFiles {
        /// The files to compile.
        #[serde(default)]
        files: Vec<String>,
        /// Import roots; each file's own folder is always added.
        #[serde(default)]
        include_dirs: Vec<String>,
    },
}

/// Transport security for a gRPC call.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrpcTransport {
    /// TLS for `https://` and `grpcs://`, plaintext (h2c) otherwise.
    #[default]
    FromUrl,
    /// Always plaintext HTTP/2.
    Plaintext,
    /// Always TLS, verified against the system trust store.
    Tls,
}

/// gRPC options saved with a request.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct GrpcSettings {
    /// Fully-qualified service name (`package.Service`).
    #[serde(default)]
    pub service: String,
    /// Method name within the service.
    #[serde(default)]
    pub method: String,
    /// The request message as JSON (the first one, for client streaming).
    #[serde(default)]
    pub message: String,
    /// Where the service definitions come from.
    #[serde(default)]
    pub descriptors: DescriptorSource,
    /// TLS or plaintext.
    #[serde(default)]
    pub transport: GrpcTransport,
    /// The call's deadline (`grpc-timeout`); none when absent.
    #[serde(default)]
    pub deadline_ms: Option<u64>,
}

/// The per-kind options of a streaming request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProtocolSettings {
    /// WebSocket options.
    WebSocket(WebSocketSettings),
    /// Server-Sent Events options.
    Sse(SseSettings),
    /// gRPC options.
    Grpc(GrpcSettings),
}

impl ProtocolSettings {
    /// The default settings for `protocol`; `None` for HTTP.
    pub fn for_protocol(protocol: RequestProtocol) -> Option<Self> {
        match protocol {
            RequestProtocol::Http => None,
            RequestProtocol::WebSocket => Some(Self::WebSocket(WebSocketSettings::default())),
            RequestProtocol::Sse => Some(Self::Sse(SseSettings::default())),
            RequestProtocol::Grpc => Some(Self::Grpc(GrpcSettings::default())),
        }
    }

    /// The kind these settings belong to.
    pub fn protocol(&self) -> RequestProtocol {
        match self {
            ProtocolSettings::WebSocket(_) => RequestProtocol::WebSocket,
            ProtocolSettings::Sse(_) => RequestProtocol::Sse,
            ProtocolSettings::Grpc(_) => RequestProtocol::Grpc,
        }
    }
}

impl SavedRequest {
    /// The streaming settings saved with this request; `None` for HTTP (or
    /// settings this build cannot read).
    pub fn protocol_settings(&self) -> Option<ProtocolSettings> {
        self.extensions
            .get(PROTOCOL_EXTENSION_KEY)
            .and_then(|value| serde_json::from_value(value.clone()).ok())
    }

    /// What this request talks.
    pub fn protocol(&self) -> RequestProtocol {
        self.protocol_settings()
            .map_or(RequestProtocol::Http, |settings| settings.protocol())
    }

    /// Store `settings` with the request; `None` makes it plain HTTP.
    pub fn set_protocol_settings(&mut self, settings: Option<&ProtocolSettings>) {
        match settings.and_then(|settings| serde_json::to_value(settings).ok()) {
            Some(value) => {
                self.extensions
                    .insert(PROTOCOL_EXTENSION_KEY.to_owned(), value);
            }
            None => {
                self.extensions.remove(PROTOCOL_EXTENSION_KEY);
            }
        }
    }
}

/// The URL the request compiler sees for a streaming request: it only
/// accepts `http(s)`, so `ws(s)://` and `grpc(s)://` are written as their
/// HTTP equivalents for compiling ([`wire_url`] maps back).
pub fn compile_url(protocol: RequestProtocol, url: &str) -> String {
    let trimmed = url.trim_start();
    let swap = |from: &str, to: &str| {
        trimmed
            .get(..from.len())
            .filter(|prefix| prefix.eq_ignore_ascii_case(from))
            .map(|_| format!("{to}{}", &trimmed[from.len()..]))
    };
    let mapped = match protocol {
        RequestProtocol::WebSocket => {
            swap("wss://", "https://").or_else(|| swap("ws://", "http://"))
        }
        RequestProtocol::Grpc => {
            swap("grpcs://", "https://").or_else(|| swap("grpc://", "http://"))
        }
        RequestProtocol::Http | RequestProtocol::Sse => None,
    };
    mapped.unwrap_or_else(|| url.to_owned())
}

/// The URL to connect to once the compiler has resolved it: WebSocket
/// requests go back to `ws(s)://`; the others stay `http(s)://`.
pub fn wire_url(protocol: RequestProtocol, compiled: &str) -> String {
    if protocol != RequestProtocol::WebSocket {
        return compiled.to_owned();
    }
    if let Some(rest) = compiled.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = compiled.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        compiled.to_owned()
    }
}

/// Errors from starting or running a streaming session.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    /// The request or a message is malformed.
    #[error("{0}")]
    Invalid(String),
    /// This build was made without the protocol's cargo feature.
    #[error("{0} support is not included in this build")]
    Disabled(&'static str),
    /// Connecting or talking to the server failed.
    #[error("{0}")]
    Connection(String),
}

/// Which way a log entry went.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    /// Sent by the client.
    Sent,
    /// Received from the server.
    Received,
    /// A connection event (connected, reconnecting, closed, …).
    Info,
}

/// What a log entry carries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    /// A text message (WebSocket text frame, SSE event data, gRPC message as JSON).
    Text,
    /// Binary data, shown as hex.
    Binary,
    /// A WebSocket ping.
    Ping,
    /// A WebSocket pong.
    Pong,
    /// Response or gRPC header metadata.
    Headers,
    /// gRPC trailers.
    Trailers,
    /// A connection status change.
    Status,
    /// An error.
    Error,
}

/// One row of a session's live log.
#[derive(Clone, Debug, PartialEq)]
pub struct LogEntry {
    /// Sent, received or a connection event.
    pub direction: Direction,
    /// What it is.
    pub kind: EntryKind,
    /// Unix time in milliseconds.
    pub at_ms: i64,
    /// A short label: the SSE event type, a close code, a gRPC status.
    pub label: String,
    /// The SSE event id, when the server sent one.
    pub event_id: Option<String>,
    /// Display text, at most [`MAX_LOG_TEXT_BYTES`] (hex for binary).
    pub text: String,
    /// The full payload size in bytes.
    pub size: usize,
}

impl LogEntry {
    /// An entry stamped now.
    pub fn now(
        direction: Direction,
        kind: EntryKind,
        label: impl Into<String>,
        text: &str,
        size: usize,
    ) -> Self {
        Self {
            direction,
            kind,
            at_ms: now_millis(),
            label: label.into(),
            event_id: None,
            text: clip_text(text),
            size,
        }
    }

    /// A binary payload, shown as spaced hex.
    pub fn binary(direction: Direction, label: impl Into<String>, bytes: &[u8]) -> Self {
        let shown = &bytes[..bytes.len().min(MAX_LOG_TEXT_BYTES / 3)];
        let mut text = String::with_capacity(shown.len() * 3);
        for (index, byte) in shown.iter().enumerate() {
            if index > 0 {
                text.push(' ');
            }
            text.push_str(&format!("{byte:02x}"));
        }
        if shown.len() < bytes.len() {
            text.push_str(" …");
        }
        Self {
            direction,
            kind: EntryKind::Binary,
            at_ms: now_millis(),
            label: label.into(),
            event_id: None,
            text,
            size: bytes.len(),
        }
    }

    /// A connection event.
    pub fn info(text: impl Into<String>) -> Self {
        let text = text.into();
        Self::now(Direction::Info, EntryKind::Status, "", &text, 0)
    }

    /// An error event.
    pub fn error(text: impl Into<String>) -> Self {
        let text = text.into();
        Self::now(Direction::Info, EntryKind::Error, "error", &text, 0)
    }
}

/// `text` cut to [`MAX_LOG_TEXT_BYTES`] on a character boundary.
fn clip_text(text: &str) -> String {
    if text.len() <= MAX_LOG_TEXT_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_LOG_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Unix time in milliseconds.
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

/// Where a session's connection stands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionState {
    /// Resolving, connecting, handshaking.
    Connecting,
    /// Connected; for WebSocket, the subprotocol the server chose.
    Open {
        /// The HTTP status of the handshake or stream response.
        status: u16,
        /// The negotiated WebSocket subprotocol.
        subprotocol: Option<String>,
    },
    /// Waiting before reconnect `attempt` (SSE).
    Reconnecting {
        /// 1 for the first reconnect.
        attempt: u32,
        /// How long until it connects again.
        delay_ms: u64,
    },
    /// Finished. `code` is the WebSocket close code or gRPC status code.
    Closed {
        /// Close or status code, when there is one.
        code: Option<u32>,
        /// Close reason or status message.
        reason: String,
    },
    /// Stopped by an error.
    Failed(String),
}

impl SessionState {
    /// Whether the session has ended.
    pub fn is_finished(&self) -> bool {
        matches!(self, SessionState::Closed { .. } | SessionState::Failed(_))
    }
}

/// What a running session reports.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    /// The connection changed state.
    State(SessionState),
    /// A log row.
    Log(LogEntry),
    /// Response headers of the handshake or stream.
    ResponseHeaders(Vec<(String, String)>),
    /// gRPC trailers.
    Trailers(Vec<(String, String)>),
}

/// What the viewer asks a running session to do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionCommand {
    /// Send a message (WebSocket frame, gRPC client-stream message as JSON).
    Send(OutgoingPayload),
    /// Send a WebSocket ping with this payload.
    Ping(Vec<u8>),
    /// Close the WebSocket with a code and reason.
    Close {
        /// RFC 6455 close code (1000 normal).
        code: u16,
        /// Close reason.
        reason: String,
    },
    /// Finish the client side of a gRPC client stream.
    CloseSend,
}

/// The viewer's side of a running session: send commands, stop it. Dropping
/// the handle cancels the session.
pub struct SessionHandle {
    commands: mpsc::UnboundedSender<SessionCommand>,
    cancel: CancellationToken,
}

impl SessionHandle {
    /// Queue `command`; fails once the session has ended.
    pub fn send(&self, command: SessionCommand) -> Result<(), ProtocolError> {
        self.commands
            .send(command)
            .map_err(|_| ProtocolError::Connection("the connection is closed".into()))
    }

    /// Stop the session now.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Whether [`Self::cancel`] was called.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

impl fmt::Debug for SessionHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionHandle")
            .field("cancelled", &self.cancel.is_cancelled())
            .finish()
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// The session side of a [`SessionHandle`]: where commands arrive, the
/// cancel signal, and the event sender (which redacts secrets from text).
pub(crate) struct SessionIo {
    pub commands: mpsc::UnboundedReceiver<SessionCommand>,
    pub cancel: CancellationToken,
    events: mpsc::UnboundedSender<StreamEvent>,
    redactions: Vec<String>,
}

impl SessionIo {
    /// Report `event`, with any resolved secret in its text replaced.
    pub fn emit(&self, event: StreamEvent) {
        let _ = self.events.send(self.redact_event(event));
    }

    /// Log one entry.
    pub fn log(&self, entry: LogEntry) {
        self.emit(StreamEvent::Log(entry));
    }

    /// Report a state change, with a matching log row.
    pub fn state(&self, state: SessionState) {
        let line = match &state {
            SessionState::Connecting => None,
            SessionState::Open {
                status,
                subprotocol,
            } => Some(match subprotocol {
                Some(protocol) => format!("Connected ({status}, subprotocol {protocol})"),
                None => format!("Connected ({status})"),
            }),
            SessionState::Reconnecting { attempt, delay_ms } => {
                Some(format!("Reconnecting in {delay_ms} ms (attempt {attempt})"))
            }
            SessionState::Closed { code, reason } => Some(match (code, reason.is_empty()) {
                (Some(code), false) => format!("Closed ({code}): {reason}"),
                (Some(code), true) => format!("Closed ({code})"),
                (None, false) => format!("Closed: {reason}"),
                (None, true) => "Closed".to_owned(),
            }),
            SessionState::Failed(error) => {
                self.log(LogEntry::error(error.clone()));
                None
            }
        };
        if let Some(line) = line {
            self.log(LogEntry::info(line));
        }
        self.emit(StreamEvent::State(state));
    }

    /// `text` with resolved secrets replaced.
    pub fn redact(&self, text: &str) -> String {
        self.redactions
            .iter()
            .filter(|value| value.len() >= 4)
            .fold(text.to_owned(), |text, value| {
                text.replace(value.as_str(), "<redacted>")
            })
    }

    fn redact_event(&self, event: StreamEvent) -> StreamEvent {
        if self.redactions.is_empty() {
            return event;
        }
        let headers = |rows: Vec<(String, String)>| {
            rows.into_iter()
                .map(|(name, value)| (name, self.redact(&value)))
                .collect()
        };
        match event {
            StreamEvent::Log(mut entry) => {
                entry.text = self.redact(&entry.text);
                entry.label = self.redact(&entry.label);
                StreamEvent::Log(entry)
            }
            StreamEvent::State(SessionState::Failed(error)) => {
                StreamEvent::State(SessionState::Failed(self.redact(&error)))
            }
            StreamEvent::ResponseHeaders(rows) => StreamEvent::ResponseHeaders(headers(rows)),
            StreamEvent::Trailers(rows) => StreamEvent::Trailers(headers(rows)),
            other => other,
        }
    }
}

/// A connected handle and session pair for one run.
pub(crate) fn session_pair(
    redactions: Vec<String>,
) -> (
    SessionHandle,
    SessionIo,
    mpsc::UnboundedReceiver<StreamEvent>,
) {
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    (
        SessionHandle {
            commands: command_tx,
            cancel: cancel.clone(),
        },
        SessionIo {
            commands: command_rx,
            cancel,
            events: event_tx,
            redactions,
        },
        event_rx,
    )
}

/// A compiled streaming request: the [`PreparedRequest`] (URL, headers and
/// auth resolved, its URL already passed through [`wire_url`]) and the
/// kind's settings.
#[derive(Clone, Debug)]
pub struct StreamRequest {
    /// The compiled request.
    pub prepared: PreparedRequest,
    /// What to connect with.
    pub settings: ProtocolSettings,
}

/// Start `request` on `runtime`. Returns the handle to drive it and the
/// receiver of its live events; the session ends with a
/// [`SessionState::Closed`] or [`SessionState::Failed`] event.
pub fn start_session(
    runtime: &tokio::runtime::Handle,
    request: StreamRequest,
) -> Result<(SessionHandle, mpsc::UnboundedReceiver<StreamEvent>), ProtocolError> {
    let StreamRequest { prepared, settings } = request;
    if !settings.protocol().available() {
        return Err(ProtocolError::Disabled(settings.protocol().name()));
    }
    let (handle, io, events) = session_pair(prepared.redactions.clone());
    match settings {
        ProtocolSettings::Sse(settings) => {
            runtime.spawn(sse::run(prepared, settings, io));
        }
        #[cfg(feature = "websocket")]
        ProtocolSettings::WebSocket(settings) => {
            runtime.spawn(websocket::run(prepared, settings, io));
        }
        #[cfg(feature = "grpc")]
        ProtocolSettings::Grpc(settings) => {
            runtime.spawn(grpc::run(prepared, settings, io));
        }
        #[allow(unreachable_patterns)]
        other => return Err(ProtocolError::Disabled(other.protocol().name())),
    }
    Ok((handle, events))
}

/// How long connecting (DNS, TCP, TLS, handshake) may take.
pub(crate) fn connect_timeout(prepared: &PreparedRequest) -> Duration {
    Duration::from_millis(prepared.settings.timeout_ms.clamp(1_000, 300_000))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CollectionId, HttpMethod, RequestId};

    fn request() -> SavedRequest {
        SavedRequest {
            id: RequestId::new(),
            collection_id: CollectionId::new(),
            folder_id: None,
            name: "socket".into(),
            method: HttpMethod::get(),
            url: "wss://example.test/ws".into(),
            params: Vec::new(),
            headers: Vec::new(),
            auth: Default::default(),
            body: Default::default(),
            variables: Vec::new(),
            scripts: Default::default(),
            settings: Default::default(),
            extensions: Default::default(),
            sort_key: 0,
        }
    }

    #[test]
    fn protocol_settings_round_trip_through_extensions() {
        let mut saved = request();
        assert_eq!(saved.protocol(), RequestProtocol::Http);
        let settings = ProtocolSettings::WebSocket(WebSocketSettings {
            subprotocols: vec!["graphql-ws".into()],
            message: "{}".into(),
            format: MessageFormat::Json,
            snippets: vec![MessageSnippet {
                name: "hello".into(),
                format: MessageFormat::Text,
                body: "hi".into(),
            }],
        });
        saved.set_protocol_settings(Some(&settings));
        let json = serde_json::to_string(&saved).unwrap();
        let back: SavedRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.protocol(), RequestProtocol::WebSocket);
        assert_eq!(back.protocol_settings(), Some(settings));
        let safe = crate::persistence_safe_saved_request(&back).unwrap();
        assert_eq!(safe.protocol(), RequestProtocol::WebSocket);
        saved.set_protocol_settings(None);
        assert!(saved.extensions.is_empty());
    }

    #[test]
    fn sse_settings_fill_defaults() {
        let settings: ProtocolSettings = serde_json::from_str(r#"{"kind":"sse"}"#).unwrap();
        assert_eq!(settings, ProtocolSettings::Sse(SseSettings::default()));
    }

    #[test]
    fn urls_map_to_http_for_compiling_and_back() {
        assert_eq!(
            compile_url(RequestProtocol::WebSocket, "wss://h/x"),
            "https://h/x"
        );
        assert_eq!(
            compile_url(RequestProtocol::WebSocket, "WS://h/x"),
            "http://h/x"
        );
        assert_eq!(
            compile_url(RequestProtocol::WebSocket, "/relative"),
            "/relative"
        );
        assert_eq!(
            compile_url(RequestProtocol::Grpc, "grpcs://h:443"),
            "https://h:443"
        );
        assert_eq!(
            compile_url(RequestProtocol::Grpc, "grpc://h:50051"),
            "http://h:50051"
        );
        assert_eq!(
            compile_url(RequestProtocol::Sse, "https://h/e"),
            "https://h/e"
        );
        assert_eq!(
            wire_url(RequestProtocol::WebSocket, "https://h/x?a=1"),
            "wss://h/x?a=1"
        );
        assert_eq!(
            wire_url(RequestProtocol::WebSocket, "http://h/x"),
            "ws://h/x"
        );
        assert_eq!(
            wire_url(RequestProtocol::Grpc, "http://h:1/"),
            "http://h:1/"
        );
    }

    #[test]
    fn messages_encode_by_format() {
        assert_eq!(
            encode_message(MessageFormat::Text, "hi").unwrap(),
            OutgoingPayload::Text("hi".into())
        );
        assert!(encode_message(MessageFormat::Json, "{nope").is_err());
        assert_eq!(
            encode_message(MessageFormat::Hex, "0x de ad\nbe ef").unwrap(),
            OutgoingPayload::Binary(vec![0xde, 0xad, 0xbe, 0xef])
        );
        assert!(encode_message(MessageFormat::Hex, "abc").is_err());
        assert_eq!(
            encode_message(MessageFormat::Base64, "AAEC").unwrap(),
            OutgoingPayload::Binary(vec![0, 1, 2])
        );
    }

    #[test]
    fn log_text_is_clipped_and_binary_shown_as_hex() {
        let long = "é".repeat(MAX_LOG_TEXT_BYTES);
        let entry = LogEntry::now(Direction::Received, EntryKind::Text, "", &long, long.len());
        assert!(entry.text.len() <= MAX_LOG_TEXT_BYTES + 3);
        assert_eq!(entry.size, long.len());
        let entry = LogEntry::binary(Direction::Sent, "", &[0, 255, 16]);
        assert_eq!(entry.text, "00 ff 10");
        assert_eq!(entry.size, 3);
    }

    #[test]
    fn session_events_redact_secrets() {
        let (_handle, io, mut events) = session_pair(vec!["s3cr3t-token".into()]);
        io.log(LogEntry::info("got s3cr3t-token back"));
        io.state(SessionState::Failed("bad s3cr3t-token".into()));
        let mut texts = Vec::new();
        while let Ok(event) = events.try_recv() {
            match event {
                StreamEvent::Log(entry) => texts.push(entry.text),
                StreamEvent::State(SessionState::Failed(error)) => texts.push(error),
                _ => {}
            }
        }
        assert!(
            texts.iter().all(|text| !text.contains("s3cr3t")),
            "{texts:?}"
        );
        assert_eq!(texts.len(), 3);
    }
}
