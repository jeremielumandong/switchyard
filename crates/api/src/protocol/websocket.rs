//! WebSocket sessions over tokio-tungstenite. The TCP connection goes to the
//! addresses the private-network policy validated, and `wss://` uses the
//! same rustls configuration (OS trust store, verification on) as HTTP sends.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::protocol::frame::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};

use super::{
    Direction, EntryKind, LogEntry, OutgoingPayload, SessionCommand, SessionIo, SessionState,
    StreamEvent, WebSocketSettings, connect_timeout,
};
use crate::PreparedRequest;

/// How long to wait for the server's close reply after sending a close frame.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Largest message accepted from the server.
const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// Connect, then relay commands and messages until either side closes.
pub(crate) async fn run(prepared: PreparedRequest, settings: WebSocketSettings, io: SessionIo) {
    io.state(SessionState::Connecting);
    let connected = tokio::select! {
        () = io.cancel.cancelled() => {
            io.state(SessionState::Closed { code: None, reason: "Cancelled".into() });
            return;
        }
        connected = tokio::time::timeout(connect_timeout(&prepared), connect(&prepared, &settings)) => connected,
    };
    match connected {
        Err(_) => io.state(SessionState::Failed("Connecting timed out".into())),
        Ok(Err(error)) => io.state(SessionState::Failed(error)),
        Ok(Ok(Connection::Plain(socket, response))) => session(*socket, response, io).await,
        Ok(Ok(Connection::Tls(socket, response))) => session(*socket, response, io).await,
    }
}

/// The handshake reply worth showing.
struct Handshake {
    status: u16,
    headers: Vec<(String, String)>,
    subprotocol: Option<String>,
}

enum Connection {
    Plain(Box<WebSocketStream<tokio::net::TcpStream>>, Handshake),
    Tls(
        Box<WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>>,
        Handshake,
    ),
}

async fn connect(
    prepared: &PreparedRequest,
    settings: &WebSocketSettings,
) -> Result<Connection, String> {
    let url = url::Url::parse(&prepared.url).map_err(|error| format!("invalid URL: {error}"))?;
    let secure = match url.scheme() {
        "wss" => true,
        "ws" => false,
        other => {
            return Err(format!(
                "a WebSocket URL starts with ws:// or wss://, not {other}://"
            ));
        }
    };
    // The private-network policy is written for http(s) URLs.
    let mut policy_url = url.clone();
    policy_url
        .set_scheme(if secure { "https" } else { "http" })
        .map_err(|()| "invalid WebSocket URL".to_string())?;
    let allow_private = prepared.settings.allow_private_network;
    crate::http::validate_http_step_url(policy_url.as_str(), allow_private)?;
    let addresses = crate::http::resolve_http_step_addresses(&policy_url, allow_private).await?;
    let tcp = connect_any(&addresses).await?;
    let host = url
        .host_str()
        .ok_or_else(|| "the WebSocket URL has no host".to_string())?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let request = handshake_request(prepared, settings)?;
    let config = Some(
        WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_MESSAGE_BYTES)),
    );
    if secure {
        let mut tls = crate::tls::client_config()?;
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        let name = rustls::pki_types::ServerName::try_from(host)
            .map_err(|error| format!("invalid TLS server name: {error}"))?;
        let stream = tokio_rustls::TlsConnector::from(Arc::new(tls))
            .connect(name, tcp)
            .await
            .map_err(|error| format!("TLS handshake failed: {error}"))?;
        let (socket, response) =
            tokio_tungstenite::client_async_with_config(request, stream, config)
                .await
                .map_err(handshake_error)?;
        Ok(Connection::Tls(Box::new(socket), handshake(&response)))
    } else {
        let (socket, response) = tokio_tungstenite::client_async_with_config(request, tcp, config)
            .await
            .map_err(handshake_error)?;
        Ok(Connection::Plain(Box::new(socket), handshake(&response)))
    }
}

async fn connect_any(addresses: &[SocketAddr]) -> Result<tokio::net::TcpStream, String> {
    let mut last = None;
    for address in addresses {
        match tokio::net::TcpStream::connect(address).await {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            Err(error) => last = Some(error),
        }
    }
    Err(match last {
        Some(error) => format!("Connecting failed: {error}"),
        None => "the host resolved to no addresses".into(),
    })
}

/// The upgrade request: the URL, the compiled headers (auth included) and
/// the offered subprotocols.
fn handshake_request(
    prepared: &PreparedRequest,
    settings: &WebSocketSettings,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, String> {
    let mut request = prepared
        .url
        .as_str()
        .into_client_request()
        .map_err(|error| format!("invalid WebSocket request: {error}"))?;
    let headers = request.headers_mut();
    for (name, value) in &prepared.headers {
        let name = HeaderName::from_bytes(name.trim().as_bytes())
            .map_err(|error| format!("invalid header name: {error}"))?;
        // The handshake owns these.
        if matches!(
            name.as_str(),
            "host"
                | "connection"
                | "upgrade"
                | "sec-websocket-key"
                | "sec-websocket-version"
                | "sec-websocket-extensions"
        ) {
            continue;
        }
        let value = HeaderValue::from_str(value)
            .map_err(|error| format!("invalid header value: {error}"))?;
        headers.append(name, value);
    }
    let protocols = settings
        .subprotocols
        .iter()
        .map(|protocol| protocol.trim())
        .filter(|protocol| !protocol.is_empty())
        .collect::<Vec<_>>();
    if !protocols.is_empty() {
        headers.insert(
            "sec-websocket-protocol",
            HeaderValue::from_str(&protocols.join(", "))
                .map_err(|error| format!("invalid subprotocol: {error}"))?,
        );
    }
    Ok(request)
}

fn handshake(response: &tokio_tungstenite::tungstenite::handshake::client::Response) -> Handshake {
    let headers = response
        .headers()
        .iter()
        .filter(|(name, _)| name.as_str() != "set-cookie")
        .map(|(name, value)| {
            (
                name.to_string(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    Handshake {
        status: response.status().as_u16(),
        subprotocol: response
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        headers,
    }
}

fn handshake_error(error: tokio_tungstenite::tungstenite::Error) -> String {
    use tokio_tungstenite::tungstenite::Error;
    match error {
        Error::Http(response) => {
            let body = response
                .body()
                .as_deref()
                .map(|body| String::from_utf8_lossy(&body[..body.len().min(500)]).into_owned())
                .unwrap_or_default();
            format!(
                "The server refused the upgrade ({}){}",
                response.status(),
                if body.is_empty() {
                    String::new()
                } else {
                    format!(": {body}")
                }
            )
        }
        other => format!("WebSocket handshake failed: {other}"),
    }
}

async fn session<S>(mut socket: WebSocketStream<S>, handshake: Handshake, mut io: SessionIo)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    io.emit(StreamEvent::ResponseHeaders(handshake.headers));
    io.state(SessionState::Open {
        status: handshake.status,
        subprotocol: handshake.subprotocol,
    });
    let mut closing: Option<tokio::time::Instant> = None;
    loop {
        let close_deadline = async {
            match closing {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            () = io.cancel.cancelled() => {
                let _ = tokio::time::timeout(Duration::from_secs(1), socket.close(None)).await;
                io.state(SessionState::Closed { code: None, reason: "Cancelled".into() });
                return;
            }
            () = close_deadline => {
                io.state(SessionState::Closed {
                    code: None,
                    reason: "The server did not answer the close frame".into(),
                });
                return;
            }
            command = io.commands.recv(), if closing.is_none() => {
                let Some(command) = command else {
                    // The handle is gone: nobody is watching.
                    let _ = socket.close(None).await;
                    return;
                };
                match send_command(&mut socket, command, &io).await {
                    Ok(CommandOutcome::Continue) => {}
                    Ok(CommandOutcome::Closing) => {
                        closing = Some(tokio::time::Instant::now() + CLOSE_TIMEOUT);
                    }
                    Err(error) => {
                        io.state(SessionState::Failed(error));
                        return;
                    }
                }
            }
            message = socket.next() => match message {
                Some(Ok(message)) => {
                    if let Some(closed) = received(message, &io) {
                        io.state(closed);
                        return;
                    }
                }
                Some(Err(error)) => {
                    io.state(SessionState::Failed(format!("The connection broke: {error}")));
                    return;
                }
                None => {
                    io.state(SessionState::Closed {
                        code: None,
                        reason: "The connection ended without a close frame".into(),
                    });
                    return;
                }
            }
        }
    }
}

enum CommandOutcome {
    Continue,
    Closing,
}

async fn send_command<S>(
    socket: &mut WebSocketStream<S>,
    command: SessionCommand,
    io: &SessionIo,
) -> Result<CommandOutcome, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (message, entry, outcome) = match command {
        SessionCommand::Send(OutgoingPayload::Text(text)) => {
            let entry = LogEntry::now(Direction::Sent, EntryKind::Text, "", &text, text.len());
            (Message::text(text), entry, CommandOutcome::Continue)
        }
        SessionCommand::Send(OutgoingPayload::Binary(bytes)) => {
            let entry = LogEntry::binary(Direction::Sent, "", &bytes);
            (Message::binary(bytes), entry, CommandOutcome::Continue)
        }
        SessionCommand::Ping(payload) => {
            if payload.len() > 125 {
                io.log(LogEntry::error("A ping payload is at most 125 bytes."));
                return Ok(CommandOutcome::Continue);
            }
            let text = String::from_utf8_lossy(&payload).into_owned();
            let entry = LogEntry::now(
                Direction::Sent,
                EntryKind::Ping,
                "ping",
                &text,
                payload.len(),
            );
            (
                Message::Ping(payload.into()),
                entry,
                CommandOutcome::Continue,
            )
        }
        SessionCommand::Close { code, reason } => {
            let reason = truncate_reason(&reason);
            let entry = LogEntry::now(
                Direction::Sent,
                EntryKind::Status,
                format!("close {code}"),
                &reason,
                reason.len() + 2,
            );
            let frame = CloseFrame {
                code: CloseCode::from(code),
                reason: reason.into(),
            };
            (Message::Close(Some(frame)), entry, CommandOutcome::Closing)
        }
        SessionCommand::CloseSend => return Ok(CommandOutcome::Continue),
    };
    socket
        .send(message)
        .await
        .map_err(|error| format!("Sending failed: {error}"))?;
    io.log(entry);
    Ok(outcome)
}

/// A close reason fits a control frame: 123 bytes after the code.
fn truncate_reason(reason: &str) -> String {
    let mut end = reason.len().min(123);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason[..end].to_owned()
}

/// Log one received message; a close frame ends the session.
fn received(message: Message, io: &SessionIo) -> Option<SessionState> {
    match message {
        Message::Text(text) => {
            io.log(LogEntry::now(
                Direction::Received,
                EntryKind::Text,
                "",
                text.as_str(),
                text.len(),
            ));
        }
        Message::Binary(bytes) => io.log(LogEntry::binary(Direction::Received, "", &bytes)),
        Message::Ping(payload) => io.log(LogEntry::now(
            Direction::Received,
            EntryKind::Ping,
            "ping",
            &String::from_utf8_lossy(&payload),
            payload.len(),
        )),
        Message::Pong(payload) => io.log(LogEntry::now(
            Direction::Received,
            EntryKind::Pong,
            "pong",
            &String::from_utf8_lossy(&payload),
            payload.len(),
        )),
        Message::Close(frame) => {
            let (code, reason) = frame
                .map(|frame| {
                    (
                        Some(u32::from(u16::from(frame.code))),
                        frame.reason.to_string(),
                    )
                })
                .unwrap_or((None, String::new()));
            return Some(SessionState::Closed { code, reason });
        }
        Message::Frame(_) => {}
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        MessageFormat, ProtocolSettings, StreamRequest, encode_message, start_session,
    };
    use crate::{HttpMethod, PreparedBody, RequestId};
    use tokio::sync::mpsc::UnboundedReceiver;

    #[allow(clippy::result_large_err)] // tungstenite's handshake callback signature
    /// An echo server: echoes text and binary, records the handshake's
    /// headers, and closes with the client's code.
    async fn echo_server() -> (SocketAddr, tokio::task::JoinHandle<Vec<(String, String)>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let record = seen.clone();
            let callback = move |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                                 mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                let mut headers = record.lock().unwrap();
                for (name, value) in request.headers() {
                    headers.push((name.to_string(), value.to_str().unwrap_or_default().to_string()));
                }
                if let Some(offered) = request.headers().get("sec-websocket-protocol") {
                    let first = offered.to_str().unwrap().split(',').next().unwrap().trim().to_string();
                    response
                        .headers_mut()
                        .insert("sec-websocket-protocol", HeaderValue::from_str(&first).unwrap());
                }
                Ok(response)
            };
            let mut socket = tokio_tungstenite::accept_hdr_async(stream, callback)
                .await
                .unwrap();
            while let Some(Ok(message)) = socket.next().await {
                match message {
                    Message::Text(_) | Message::Binary(_) => socket.send(message).await.unwrap(),
                    // tungstenite answers the close itself; reading on
                    // flushes that reply and ends the stream.
                    _ => {}
                }
            }
            seen.lock().unwrap().clone()
        });
        (address, task)
    }

    fn prepared(url: String) -> PreparedRequest {
        PreparedRequest {
            request_id: RequestId::new(),
            method: HttpMethod::get(),
            url,
            headers: vec![("Authorization".into(), "Bearer tok-1234".into())],
            body: PreparedBody::None,
            settings: Default::default(),
            aws_sigv4: None,
            redactions: vec!["tok-1234".into()],
        }
    }

    async fn next_state(events: &mut UnboundedReceiver<StreamEvent>) -> SessionState {
        loop {
            match tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .unwrap()
            {
                Some(StreamEvent::State(SessionState::Connecting)) => {}
                Some(StreamEvent::State(state)) => return state,
                Some(_) => {}
                None => panic!("session ended without a state"),
            }
        }
    }

    async fn next_received(events: &mut UnboundedReceiver<StreamEvent>) -> LogEntry {
        loop {
            match tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .unwrap()
            {
                Some(StreamEvent::Log(entry)) if entry.direction == Direction::Received => {
                    return entry;
                }
                Some(_) => {}
                None => panic!("session ended"),
            }
        }
    }

    #[tokio::test]
    async fn echoes_text_and_binary_then_closes_with_a_code() {
        let (address, server) = echo_server().await;
        let settings = WebSocketSettings {
            subprotocols: vec!["chat.v2".into(), "chat.v1".into()],
            ..WebSocketSettings::default()
        };
        let (handle, mut events) = start_session(
            &tokio::runtime::Handle::current(),
            StreamRequest {
                prepared: prepared(format!("ws://{address}/socket")),
                settings: ProtocolSettings::WebSocket(settings),
            },
        )
        .unwrap();
        assert_eq!(
            next_state(&mut events).await,
            SessionState::Open {
                status: 101,
                subprotocol: Some("chat.v2".into()),
            }
        );
        handle
            .send(SessionCommand::Send(
                encode_message(MessageFormat::Json, r#"{"hello":1}"#).unwrap(),
            ))
            .unwrap();
        let echoed = next_received(&mut events).await;
        assert_eq!(
            (echoed.kind, echoed.text.as_str()),
            (EntryKind::Text, r#"{"hello":1}"#)
        );
        handle
            .send(SessionCommand::Send(
                encode_message(MessageFormat::Hex, "00ff").unwrap(),
            ))
            .unwrap();
        let echoed = next_received(&mut events).await;
        assert_eq!(
            (echoed.kind, echoed.text.as_str(), echoed.size),
            (EntryKind::Binary, "00 ff", 2)
        );
        handle
            .send(SessionCommand::Close {
                code: 4001,
                reason: "done".into(),
            })
            .unwrap();
        assert_eq!(
            next_state(&mut events).await,
            SessionState::Closed {
                code: Some(4001),
                reason: "done".into(),
            }
        );
        let headers = server.await.unwrap();
        assert!(
            headers
                .iter()
                .any(|(name, value)| name == "authorization" && value == "Bearer tok-1234")
        );
        assert!(
            headers.iter().any(
                |(name, value)| name == "sec-websocket-protocol" && value == "chat.v2, chat.v1"
            )
        );
    }

    #[tokio::test]
    async fn cancelling_closes_the_session() {
        let (address, _server) = echo_server().await;
        let (handle, mut events) = start_session(
            &tokio::runtime::Handle::current(),
            StreamRequest {
                prepared: prepared(format!("ws://{address}/")),
                settings: ProtocolSettings::WebSocket(WebSocketSettings::default()),
            },
        )
        .unwrap();
        assert!(matches!(
            next_state(&mut events).await,
            SessionState::Open { .. }
        ));
        handle.cancel();
        assert!(matches!(
            next_state(&mut events).await,
            SessionState::Closed { code: None, .. }
        ));
    }

    #[tokio::test]
    async fn a_refused_upgrade_fails_with_the_status_and_redacts_secrets() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0u8; 4096];
            let _ = socket.read(&mut buffer).await;
            let _ = socket
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 17\r\n\r\nbad token tok-1234",
                )
                .await;
        });
        let (_handle, mut events) = start_session(
            &tokio::runtime::Handle::current(),
            StreamRequest {
                prepared: prepared(format!("ws://{address}/")),
                settings: ProtocolSettings::WebSocket(WebSocketSettings::default()),
            },
        )
        .unwrap();
        match next_state(&mut events).await {
            SessionState::Failed(error) => {
                assert!(error.contains("401"), "{error}");
                assert!(!error.contains("tok-1234"), "{error}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn close_reasons_fit_a_control_frame() {
        assert_eq!(truncate_reason("ok"), "ok");
        assert!(truncate_reason(&"é".repeat(100)).len() <= 123);
    }

    #[test]
    fn handshake_requests_carry_headers_but_not_reserved_ones() {
        let mut prepared = prepared("wss://example.test/x".into());
        prepared.headers.push(("Upgrade".into(), "nope".into()));
        let request = handshake_request(&prepared, &WebSocketSettings::default()).unwrap();
        assert_eq!(request.headers()["authorization"], "Bearer tok-1234");
        assert_eq!(request.headers()["upgrade"], "websocket");
    }
}
