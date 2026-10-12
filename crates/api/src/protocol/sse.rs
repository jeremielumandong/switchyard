//! Server-Sent Events: the `text/event-stream` parser (WHATWG HTML §9.2) and
//! a streaming client that reconnects like `EventSource`.

use std::time::Duration;

use futures::StreamExt as _;

use super::{
    Direction, EntryKind, LogEntry, SessionIo, SessionState, SseSettings, StreamEvent,
    connect_timeout,
};
use crate::{PreparedBody, PreparedRequest};

/// One dispatched event.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SseEvent {
    /// The `event:` type; `message` when the server named none.
    pub event: String,
    /// The `data:` lines joined with `\n`.
    pub data: String,
    /// The last event id in force when the event was dispatched.
    pub id: Option<String>,
}

/// An incremental `text/event-stream` parser. Feed it bytes as they arrive;
/// it returns the events each chunk completes.
#[derive(Debug, Default)]
pub struct SseParser {
    line: Vec<u8>,
    /// A chunk ended in CR: a LF at the start of the next one belongs to it.
    after_cr: bool,
    started: bool,
    event: String,
    data: String,
    has_data: bool,
    last_id: Option<String>,
    retry_ms: Option<u64>,
}

impl SseParser {
    /// A parser at the start of a stream.
    pub fn new() -> Self {
        Self::default()
    }

    /// A parser for a reconnected stream: the last event id carries over.
    pub fn resume(last_id: Option<String>) -> Self {
        Self {
            last_id,
            ..Self::default()
        }
    }

    /// The last event id seen (sent as `Last-Event-ID` on reconnect).
    pub fn last_event_id(&self) -> Option<&str> {
        self.last_id.as_deref()
    }

    /// The reconnection time the server asked for with `retry:`.
    pub fn retry_ms(&self) -> Option<u64> {
        self.retry_ms
    }

    /// Parse `chunk`; returns the events it completed.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        let mut events = Vec::new();
        let mut bytes = chunk;
        if self.after_cr {
            self.after_cr = false;
            if let Some(rest) = bytes.strip_prefix(b"\n") {
                bytes = rest;
            }
        }
        let mut start = 0;
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'\n' | b'\r' => {
                    self.line.extend_from_slice(&bytes[start..index]);
                    let line = std::mem::take(&mut self.line);
                    self.process_line(&line, &mut events);
                    if bytes[index] == b'\r' {
                        if index + 1 == bytes.len() {
                            self.after_cr = true;
                        } else if bytes[index + 1] == b'\n' {
                            index += 1;
                        }
                    }
                    start = index + 1;
                }
                _ => {}
            }
            index += 1;
        }
        self.line.extend_from_slice(&bytes[start..]);
        events
    }

    fn process_line(&mut self, raw: &[u8], events: &mut Vec<SseEvent>) {
        let mut line = String::from_utf8_lossy(raw);
        if !self.started {
            self.started = true;
            if let Some(stripped) = line.strip_prefix('\u{feff}') {
                line = stripped.to_owned().into();
            }
        }
        if line.is_empty() {
            self.dispatch(events);
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line.as_ref(), ""),
        };
        match field {
            "event" => self.event = value.to_owned(),
            "data" => {
                self.data.push_str(value);
                self.data.push('\n');
                self.has_data = true;
            }
            "id" if !value.contains('\0') => self.last_id = Some(value.to_owned()),
            "retry" if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) => {
                self.retry_ms = value.parse().ok();
            }
            _ => {}
        }
    }

    fn dispatch(&mut self, events: &mut Vec<SseEvent>) {
        let event = std::mem::take(&mut self.event);
        if !self.has_data {
            return;
        }
        self.has_data = false;
        let mut data = std::mem::take(&mut self.data);
        if data.ends_with('\n') {
            data.pop();
        }
        events.push(SseEvent {
            event: if event.is_empty() {
                "message".to_owned()
            } else {
                event
            },
            data,
            id: self.last_id.clone(),
        });
    }
}

/// Why one connection of the stream ended.
enum StreamEnd {
    /// Stopped by the viewer.
    Cancelled,
    /// The server ended the stream, or the connection dropped.
    Ended { error: Option<String>, events: u64 },
    /// The server refused the stream; reconnecting would not help.
    Fatal(String),
    /// 204 No Content: the server asks the client to stop.
    NoContent,
}

/// Run the stream until it ends for good, reconnecting per `settings`.
pub(crate) async fn run(prepared: PreparedRequest, settings: SseSettings, io: SessionIo) {
    let mut last_id: Option<String> = None;
    let mut retry_ms = settings.retry_ms.max(100);
    let mut failures = 0u32;
    loop {
        io.state(SessionState::Connecting);
        let mut parser = SseParser::resume(last_id.clone());
        let end = connect_once(&prepared, &settings, &mut parser, &io).await;
        last_id = parser.last_event_id().map(str::to_owned);
        if let Some(retry) = parser.retry_ms() {
            retry_ms = retry.max(100);
        }
        let reason = match end {
            StreamEnd::Cancelled => {
                io.state(SessionState::Closed {
                    code: None,
                    reason: "Stopped".into(),
                });
                return;
            }
            StreamEnd::Fatal(error) => {
                io.state(SessionState::Failed(error));
                return;
            }
            StreamEnd::NoContent => {
                io.state(SessionState::Closed {
                    code: Some(204),
                    reason: "The server ended the stream (204 No Content)".into(),
                });
                return;
            }
            StreamEnd::Ended { error, events } => {
                if events > 0 {
                    failures = 0;
                }
                error
            }
        };
        if !settings.reconnect || failures >= settings.max_reconnects {
            match reason {
                Some(error) => io.state(SessionState::Failed(error)),
                None => io.state(SessionState::Closed {
                    code: None,
                    reason: "The server ended the stream".into(),
                }),
            }
            return;
        }
        if let Some(error) = reason {
            io.log(LogEntry::error(error));
        }
        failures += 1;
        io.state(SessionState::Reconnecting {
            attempt: failures,
            delay_ms: retry_ms,
        });
        tokio::select! {
            () = io.cancel.cancelled() => {
                io.state(SessionState::Closed { code: None, reason: "Stopped".into() });
                return;
            }
            () = tokio::time::sleep(Duration::from_millis(retry_ms)) => {}
        }
    }
}

async fn connect_once(
    prepared: &PreparedRequest,
    settings: &SseSettings,
    parser: &mut SseParser,
    io: &SessionIo,
) -> StreamEnd {
    let last_id = last_event_id_for(settings, parser);
    let response = tokio::select! {
        () = io.cancel.cancelled() => return StreamEnd::Cancelled,
        response = open(prepared, last_id.as_deref()) => response,
    };
    let response = match response {
        Ok(response) => response,
        Err(OpenError::Retry(error)) => {
            return StreamEnd::Ended {
                error: Some(error),
                events: 0,
            };
        }
        Err(OpenError::Fatal(error)) => return StreamEnd::Fatal(error),
    };
    let status = response.status().as_u16();
    io.emit(StreamEvent::ResponseHeaders(crate::http::response_headers(
        response.headers(),
    )));
    if status == 204 {
        return StreamEnd::NoContent;
    }
    if status != 200 {
        let body = response.text().await.unwrap_or_default();
        let body = body.chars().take(500).collect::<String>();
        return StreamEnd::Fatal(format!("The server answered {status}: {body}"));
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !content_type.starts_with("text/event-stream") {
        return StreamEnd::Fatal(format!(
            "The response is not an event stream (Content-Type: {})",
            if content_type.is_empty() {
                "none"
            } else {
                &content_type
            }
        ));
    }
    io.state(SessionState::Open {
        status,
        subprotocol: None,
    });
    let mut body = response.bytes_stream();
    let mut events = 0u64;
    loop {
        let chunk = tokio::select! {
            () = io.cancel.cancelled() => return StreamEnd::Cancelled,
            chunk = body.next() => chunk,
        };
        match chunk {
            Some(Ok(bytes)) => {
                for event in parser.feed(&bytes) {
                    events += 1;
                    let mut entry = LogEntry::now(
                        Direction::Received,
                        EntryKind::Text,
                        event.event,
                        &event.data,
                        event.data.len(),
                    );
                    entry.event_id = event.id;
                    io.log(entry);
                }
            }
            Some(Err(error)) => {
                return StreamEnd::Ended {
                    error: Some(format!("The stream broke: {}", error.without_url())),
                    events,
                };
            }
            None => {
                return StreamEnd::Ended {
                    error: None,
                    events,
                };
            }
        }
    }
}

enum OpenError {
    Retry(String),
    Fatal(String),
}

async fn open(
    prepared: &PreparedRequest,
    last_id: Option<&str>,
) -> Result<reqwest::Response, OpenError> {
    let allow_private = prepared.settings.allow_private_network;
    crate::http::validate_http_step_url(&prepared.url, allow_private).map_err(OpenError::Fatal)?;
    let url = reqwest::Url::parse(&prepared.url)
        .map_err(|error| OpenError::Fatal(format!("invalid URL: {error}")))?;
    let timeout = connect_timeout(prepared);
    let addresses = tokio::time::timeout(
        timeout,
        crate::http::resolve_http_step_addresses(&url, allow_private),
    )
    .await
    .map_err(|_| OpenError::Retry("DNS lookup timed out".into()))?
    .map_err(OpenError::Retry)?;
    let client = crate::http::pinned_streaming_client(&url, &addresses, timeout, "event stream")
        .map_err(OpenError::Fatal)?;
    let method = reqwest::Method::from_bytes(prepared.method.as_str().as_bytes())
        .map_err(|error| OpenError::Fatal(format!("invalid method: {error}")))?;
    let mut request = client.request(method, url);
    let mut has_accept = false;
    for (name, value) in &prepared.headers {
        has_accept |= name.eq_ignore_ascii_case("accept");
        request = request.header(name.as_str(), value.as_str());
    }
    if !has_accept {
        request = request.header(reqwest::header::ACCEPT, "text/event-stream");
    }
    request = request.header(reqwest::header::CACHE_CONTROL, "no-cache");
    if let Some(id) = last_id {
        request = request.header("Last-Event-ID", id);
    }
    match &prepared.body {
        PreparedBody::None => {}
        PreparedBody::Bytes {
            content_type,
            bytes,
        } => {
            if !prepared
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            {
                request = request.header(reqwest::header::CONTENT_TYPE, content_type.as_str());
            }
            request = request.body(bytes.clone());
        }
        PreparedBody::Multipart(_) | PreparedBody::File(_) => {
            return Err(OpenError::Fatal(
                "An event stream request can send a raw body only (JSON, XML, text or form)."
                    .into(),
            ));
        }
    }
    tokio::time::timeout(timeout, request.send())
        .await
        .map_err(|_| OpenError::Retry("Connecting timed out".into()))?
        .map_err(|error| OpenError::Retry(format!("Connecting failed: {}", error.without_url())))
}

/// Whether to send `Last-Event-ID`: only when the settings say so.
pub(crate) fn last_event_id_for(settings: &SseSettings, parser: &SseParser) -> Option<String> {
    settings
        .send_last_event_id
        .then(|| parser.last_event_id().map(str::to_owned))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(chunks: &[&[u8]]) -> Vec<SseEvent> {
        let mut parser = SseParser::new();
        chunks.iter().flat_map(|chunk| parser.feed(chunk)).collect()
    }

    #[test]
    fn parses_fields_and_dispatches_on_blank_lines() {
        let parsed =
            events(&[b"event: tick\nid: 7\ndata: one\ndata:two\n\n: comment\ndata: x\n\n"]);
        assert_eq!(
            parsed,
            vec![
                SseEvent {
                    event: "tick".into(),
                    data: "one\ntwo".into(),
                    id: Some("7".into()),
                },
                SseEvent {
                    event: "message".into(),
                    data: "x".into(),
                    id: Some("7".into()),
                },
            ]
        );
    }

    #[test]
    fn handles_crlf_cr_and_split_chunks() {
        let parsed = events(&[b"data: a\r", b"\n\r", b"\ndata: b\r\rdat", b"a: c\n", b"\n"]);
        let data: Vec<_> = parsed.iter().map(|event| event.data.as_str()).collect();
        assert_eq!(data, vec!["a", "b", "c"]);
    }

    #[test]
    fn ignores_events_without_data_and_strips_bom() {
        let parsed = events(&["\u{feff}event: empty\n\nid: 3\n\ndata\n\n".as_bytes()]);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].data, "");
        assert_eq!(parsed[0].id.as_deref(), Some("3"));
    }

    #[test]
    fn reads_retry_and_ignores_bad_values() {
        let mut parser = SseParser::new();
        parser.feed(b"retry: 1500\n\nretry: soon\n\n");
        assert_eq!(parser.retry_ms(), Some(1500));
        let mut parser = SseParser::new();
        parser.feed(b"id: a\0b\n\n");
        assert_eq!(parser.last_event_id(), None);
    }

    #[test]
    fn a_trailing_event_without_blank_line_is_not_dispatched() {
        assert!(events(&[b"data: partial"]).is_empty());
    }

    #[test]
    fn resume_keeps_the_last_id_and_settings_gate_it() {
        let parser = SseParser::resume(Some("41".into()));
        let mut settings = SseSettings::default();
        assert_eq!(last_event_id_for(&settings, &parser).as_deref(), Some("41"));
        settings.send_last_event_id = false;
        assert_eq!(last_event_id_for(&settings, &parser), None);
    }

    #[tokio::test]
    async fn streams_events_and_reconnects_with_last_event_id() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut seen = Vec::new();
            for round in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0u8; 4096];
                let read = socket.read(&mut request).await.unwrap();
                seen.push(String::from_utf8_lossy(&request[..read]).to_string());
                let body = match round {
                    0 => "retry: 100\nid: 1\nevent: greet\ndata: hello\n\n",
                    1 => "id: 2\ndata: again\n\n",
                    _ => {
                        socket
                            .write_all(b"HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n")
                            .await
                            .unwrap();
                        continue;
                    }
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            }
            seen
        });
        let prepared = PreparedRequest {
            request_id: crate::RequestId::new(),
            method: crate::HttpMethod::get(),
            url: format!("http://{address}/events"),
            headers: vec![("X-Token".into(), "abcd-secret".into())],
            body: PreparedBody::None,
            settings: Default::default(),
            aws_sigv4: None,
            redactions: vec!["abcd-secret".into()],
        };
        let settings = SseSettings {
            max_reconnects: 1,
            ..SseSettings::default()
        };
        let (handle, mut events) = super::super::start_session(
            &tokio::runtime::Handle::current(),
            super::super::StreamRequest {
                prepared,
                settings: super::super::ProtocolSettings::Sse(settings),
            },
        )
        .unwrap();
        let mut received = Vec::new();
        let mut finished = None;
        while let Some(event) = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .unwrap()
        {
            match event {
                StreamEvent::Log(entry) if entry.direction == Direction::Received => {
                    received.push((entry.label, entry.text, entry.event_id));
                }
                StreamEvent::State(state) if state.is_finished() => {
                    finished = Some(state);
                    break;
                }
                _ => {}
            }
        }
        drop(handle);
        assert_eq!(
            received,
            vec![
                ("greet".into(), "hello".into(), Some("1".into())),
                ("message".into(), "again".into(), Some("2".into())),
            ]
        );
        assert!(
            matches!(
                finished,
                Some(SessionState::Closed {
                    code: Some(204),
                    ..
                })
            ),
            "{finished:?}"
        );
        let seen = server.await.unwrap();
        assert!(
            seen[0]
                .to_ascii_lowercase()
                .contains("accept: text/event-stream")
        );
        assert!(!seen[0].to_ascii_lowercase().contains("last-event-id"));
        assert!(seen[1].to_ascii_lowercase().contains("last-event-id: 1"));
        assert!(seen[2].to_ascii_lowercase().contains("last-event-id: 2"));
    }
}
