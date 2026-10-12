//! gRPC against an in-process tonic server with reflection: discovery,
//! unary, server streaming, client streaming, errors with trailers, and
//! `.proto` import.

use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::Stream;
use tonic::codegen::{BoxFuture, Service, http};
use tonic::server::NamedService;
use tonic::{Request, Response, Status, Streaming};

use super::*;
use crate::protocol::{ProtocolSettings, StreamRequest, start_session};
use crate::{HttpMethod, PreparedBody, RequestId};

const PROTO: &str = r#"
syntax = "proto3";
package test;

message Ping {
  string text = 1;
  int32 count = 2;
}

message Pong {
  string text = 1;
  int32 index = 2;
}

service Echo {
  rpc Unary(Ping) returns (Pong);
  rpc Repeat(Ping) returns (stream Pong);
  rpc Collect(stream Ping) returns (Pong);
  rpc Fail(Ping) returns (Pong);
}
"#;

/// The test `.proto` compiled with protox, and the file's path.
fn compiled() -> (tempfile::TempDir, String, DescriptorPool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("echo.proto");
    std::fs::write(&path, PROTO).unwrap();
    let path = path.to_string_lossy().into_owned();
    let pool = compile_proto_files(std::slice::from_ref(&path), &[]).unwrap();
    (dir, path, pool)
}

fn text_of(message: &DynamicMessage) -> String {
    message
        .get_field_by_name("text")
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn count_of(message: &DynamicMessage) -> i32 {
    message
        .get_field_by_name("count")
        .and_then(|value| value.as_i32())
        .unwrap_or_default()
}

fn pong(pool: &DescriptorPool, text: String, index: i32) -> DynamicMessage {
    let mut message = DynamicMessage::new(pool.get_message_by_name("test.Pong").unwrap());
    message.set_field_by_name("text", prost_reflect::Value::String(text));
    message.set_field_by_name("index", prost_reflect::Value::I32(index));
    message
}

#[derive(Clone)]
struct Echo {
    pool: DescriptorPool,
}

impl NamedService for Echo {
    const NAME: &'static str = "test.Echo";
}

struct Unary(DescriptorPool, bool);

impl tonic::server::UnaryService<DynamicMessage> for Unary {
    type Response = DynamicMessage;
    type Future = BoxFuture<Response<DynamicMessage>, Status>;

    fn call(&mut self, request: Request<DynamicMessage>) -> Self::Future {
        let pool = self.0.clone();
        let fail = self.1;
        Box::pin(async move {
            if fail {
                let mut status = Status::permission_denied("not for you");
                status
                    .metadata_mut()
                    .insert("x-reason", "test".parse().unwrap());
                return Err(status);
            }
            let auth = request
                .metadata()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            let text = format!("{} ({auth})", text_of(request.get_ref()));
            let mut response = Response::new(pong(&pool, text, 0));
            response
                .metadata_mut()
                .insert("x-served-by", "echo".parse().unwrap());
            Ok(response)
        })
    }
}

struct Repeat(DescriptorPool);

type PongStream = Pin<Box<dyn Stream<Item = Result<DynamicMessage, Status>> + Send>>;

impl tonic::server::ServerStreamingService<DynamicMessage> for Repeat {
    type Response = DynamicMessage;
    type ResponseStream = PongStream;
    type Future = BoxFuture<Response<PongStream>, Status>;

    fn call(&mut self, request: Request<DynamicMessage>) -> Self::Future {
        let pool = self.0.clone();
        Box::pin(async move {
            let text = text_of(request.get_ref());
            let count = count_of(request.get_ref());
            let items = (0..count)
                .map(move |index| Ok(pong(&pool, text.clone(), index)))
                .collect::<Vec<_>>();
            Ok(Response::new(
                Box::pin(futures::stream::iter(items)) as PongStream
            ))
        })
    }
}

struct Collect(DescriptorPool);

impl tonic::server::ClientStreamingService<DynamicMessage> for Collect {
    type Response = DynamicMessage;
    type Future = BoxFuture<Response<DynamicMessage>, Status>;

    fn call(&mut self, request: Request<Streaming<DynamicMessage>>) -> Self::Future {
        let pool = self.0.clone();
        Box::pin(async move {
            let mut stream = request.into_inner();
            let mut texts = Vec::new();
            while let Some(message) = stream.message().await? {
                texts.push(text_of(&message));
            }
            let count = texts.len() as i32;
            Ok(Response::new(pong(&pool, texts.join("+"), count)))
        })
    }
}

impl<B> Service<http::Request<B>> for Echo
where
    B: tonic::codegen::Body + Send + 'static,
    B::Error: Into<tonic::codegen::StdError> + Send + 'static,
{
    type Response = http::Response<tonic::body::Body>;
    type Error = Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        let pool = self.pool.clone();
        let ping = pool.get_message_by_name("test.Ping").unwrap();
        let path = request.uri().path().to_owned();
        Box::pin(async move {
            let mut grpc = tonic::server::Grpc::new(DynamicCodec::new(ping));
            Ok(match path.as_str() {
                "/test.Echo/Unary" => grpc.unary(Unary(pool, false), request).await,
                "/test.Echo/Fail" => grpc.unary(Unary(pool, true), request).await,
                "/test.Echo/Repeat" => grpc.server_streaming(Repeat(pool), request).await,
                "/test.Echo/Collect" => grpc.client_streaming(Collect(pool), request).await,
                _ => Status::unimplemented("no such method").into_http(),
            })
        })
    }
}

/// Serve `Echo` with reflection on a loopback port.
async fn serve(pool: &DescriptorPool) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let encoded = pool.encode_to_vec();
    let reflection = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(Box::leak(encoded.into_boxed_slice()))
        .build_v1()
        .unwrap();
    let incoming = futures::stream::unfold(listener, |listener| async move {
        let accepted = listener.accept().await.map(|(socket, _)| socket);
        Some((accepted, listener))
    });
    let echo = Echo { pool: pool.clone() };
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(echo)
            .add_service(reflection)
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    address
}

fn prepared(address: std::net::SocketAddr) -> PreparedRequest {
    PreparedRequest {
        request_id: RequestId::new(),
        method: HttpMethod::get(),
        url: format!("http://{address}/"),
        headers: vec![("Authorization".into(), "Bearer grpc-secret-1".into())],
        body: PreparedBody::None,
        settings: Default::default(),
        aws_sigv4: None,
        redactions: vec!["grpc-secret-1".into()],
    }
}

/// Run a call to the end: received texts, the closing state, headers and
/// trailers.
async fn call(
    address: std::net::SocketAddr,
    settings: GrpcSettings,
    then: impl FnOnce(&crate::protocol::SessionHandle),
) -> (
    Vec<String>,
    SessionState,
    Vec<(String, String)>,
    Vec<(String, String)>,
) {
    let (handle, mut events) = start_session(
        &tokio::runtime::Handle::current(),
        StreamRequest {
            prepared: prepared(address),
            settings: ProtocolSettings::Grpc(settings),
        },
    )
    .unwrap();
    let mut then = Some(then);
    let (mut received, mut headers, mut trailers) = (Vec::new(), Vec::new(), Vec::new());
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .unwrap()
            .unwrap();
        match event {
            StreamEvent::State(SessionState::Open { .. }) => {
                if let Some(then) = then.take() {
                    then(&handle);
                }
            }
            StreamEvent::State(state) if state.is_finished() => {
                return (received, state, headers, trailers);
            }
            StreamEvent::Log(entry) if entry.direction == Direction::Received => {
                received.push(entry.text);
            }
            StreamEvent::ResponseHeaders(rows) => headers = rows,
            StreamEvent::Trailers(rows) => trailers = rows,
            _ => {}
        }
    }
}

fn settings(method: &str, message: &str) -> GrpcSettings {
    GrpcSettings {
        service: "test.Echo".into(),
        method: method.into(),
        message: message.into(),
        ..GrpcSettings::default()
    }
}

#[tokio::test]
async fn discovers_services_by_reflection() {
    let (_dir, _path, pool) = compiled();
    let address = serve(&pool).await;
    let services = discover(&prepared(address), &GrpcSettings::default())
        .await
        .unwrap();
    assert_eq!(services.len(), 1, "{services:?}");
    assert_eq!(services[0].name, "test.Echo");
    let methods = services[0]
        .methods
        .iter()
        .map(|method| (method.name.as_str(), method.kind_label()))
        .collect::<Vec<_>>();
    assert_eq!(
        methods,
        vec![
            ("Unary", "unary"),
            ("Repeat", "server streaming"),
            ("Collect", "client streaming"),
            ("Fail", "unary"),
        ]
    );
    let template: serde_json::Value =
        serde_json::from_str(&services[0].methods[0].template).unwrap();
    assert_eq!(template, serde_json::json!({"text": "", "count": 0}));
}

#[tokio::test]
async fn unary_call_sends_metadata_and_shows_headers_and_status() {
    let (_dir, _path, pool) = compiled();
    let address = serve(&pool).await;
    let (received, state, headers, _) =
        call(address, settings("Unary", r#"{"text":"hi"}"#), |_| {}).await;
    assert_eq!(
        state,
        SessionState::Closed {
            code: Some(0),
            reason: "OK".into()
        }
    );
    assert_eq!(received.len(), 1);
    let reply: serde_json::Value = serde_json::from_str(&received[0]).unwrap();
    // The bearer token reached the server, and the log shows it redacted.
    assert_eq!(reply["text"], "hi (Bearer <redacted>)");
    assert!(
        headers
            .iter()
            .any(|(name, value)| name == "x-served-by" && value == "echo")
    );
}

#[tokio::test]
async fn server_streaming_call_receives_every_message() {
    let (_dir, _path, pool) = compiled();
    let address = serve(&pool).await;
    let (received, state, _, _) = call(
        address,
        settings("Repeat", r#"{"text":"x","count":3}"#),
        |_| {},
    )
    .await;
    assert_eq!(
        state,
        SessionState::Closed {
            code: Some(0),
            reason: "OK".into()
        }
    );
    let indices = received
        .iter()
        .map(|json| serde_json::from_str::<serde_json::Value>(json).unwrap()["index"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        indices,
        vec![
            serde_json::json!(0),
            serde_json::json!(1),
            serde_json::json!(2)
        ]
    );
}

#[tokio::test]
async fn client_streaming_call_sends_until_end_of_stream() {
    let (_dir, path, pool) = compiled();
    let address = serve(&pool).await;
    let mut settings = settings("Collect", r#"{"text":"a"}"#);
    // Imported .proto files instead of reflection.
    settings.descriptors = DescriptorSource::ProtoFiles {
        files: vec![path],
        include_dirs: Vec::new(),
    };
    let (received, state, _, _) = call(address, settings, |handle| {
        handle
            .send(SessionCommand::Send(OutgoingPayload::Text(
                r#"{"text":"b"}"#.into(),
            )))
            .unwrap();
        handle.send(SessionCommand::CloseSend).unwrap();
    })
    .await;
    assert_eq!(
        state,
        SessionState::Closed {
            code: Some(0),
            reason: "OK".into()
        }
    );
    let reply: serde_json::Value = serde_json::from_str(&received[0]).unwrap();
    assert_eq!(reply["text"], "a+b");
    assert_eq!(reply["index"], 2);
}

#[tokio::test]
async fn errors_report_the_status_code_and_trailers() {
    let (_dir, _path, pool) = compiled();
    let address = serve(&pool).await;
    let (_, state, _, trailers) = call(address, settings("Fail", "{}"), |_| {}).await;
    assert_eq!(
        state,
        SessionState::Closed {
            code: Some(7),
            reason: "PERMISSION_DENIED · not for you".into(),
        }
    );
    assert!(
        trailers
            .iter()
            .any(|(name, value)| name == "x-reason" && value == "test")
    );
}

#[tokio::test]
async fn a_bad_message_fails_before_calling() {
    let (_dir, _path, pool) = compiled();
    let address = serve(&pool).await;
    let (_, state, _, _) = call(address, settings("Unary", r#"{"nope":1}"#), |_| {}).await;
    assert!(
        matches!(state, SessionState::Failed(ref error) if error.contains("test.Ping")),
        "{state:?}"
    );
}

#[test]
fn metadata_skips_transport_headers_and_decodes_binary_keys() {
    let mut request = prepared("127.0.0.1:1".parse().unwrap());
    request
        .headers
        .push(("Content-Type".into(), "application/json".into()));
    request.headers.push(("trace-bin".into(), "AAE=".into()));
    let map = metadata(&request).unwrap();
    assert!(map.get("content-type").is_none());
    assert_eq!(map.get("authorization").unwrap(), "Bearer grpc-secret-1");
    assert_eq!(
        map.get_bin("trace-bin")
            .unwrap()
            .to_bytes()
            .unwrap()
            .as_ref(),
        &[0, 1]
    );
}
