//! gRPC calls with dynamic messages: services and methods come from server
//! reflection or from `.proto` files compiled in process (protox), request
//! bodies are JSON converted with prost-reflect, and every call kind
//! (unary, server, client and bidirectional streaming) goes over one tonic
//! streaming call.

use std::str::FromStr as _;
use std::time::Duration;

use prost::Message as _;
use prost_reflect::{
    DescriptorPool, DynamicMessage, MessageDescriptor, MethodDescriptor, ReflectMessage as _,
};
use tokio::sync::mpsc;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::codegen::http::uri::PathAndQuery;
use tonic::metadata::{MetadataKey, MetadataMap, MetadataValue};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

use super::{
    DescriptorSource, Direction, EntryKind, GrpcSettings, GrpcTransport, LogEntry, OutgoingPayload,
    ProtocolError, SessionCommand, SessionIo, SessionState, StreamEvent, connect_timeout,
};
use super::{MethodInfo, ServiceInfo};
use crate::PreparedRequest;

/// The services `settings` can call: asked of the server by reflection, or
/// compiled from the configured `.proto` files. Reflection services
/// themselves are left out.
pub async fn discover(
    prepared: &PreparedRequest,
    settings: &GrpcSettings,
) -> Result<Vec<ServiceInfo>, ProtocolError> {
    let pool = descriptor_pool(prepared, settings).await?;
    Ok(services(&pool))
}

/// The services in `pool`, minus the reflection service.
pub fn services(pool: &DescriptorPool) -> Vec<ServiceInfo> {
    pool.services()
        .filter(|service| !service.full_name().starts_with("grpc.reflection."))
        .map(|service| ServiceInfo {
            name: service.full_name().to_owned(),
            methods: service
                .methods()
                .map(|method| MethodInfo {
                    name: method.name().to_owned(),
                    client_streaming: method.is_client_streaming(),
                    server_streaming: method.is_server_streaming(),
                    input_type: method.input().full_name().to_owned(),
                    output_type: method.output().full_name().to_owned(),
                    template: message_template(&method.input()),
                })
                .collect(),
        })
        .collect()
}

/// A JSON object with every field of `message` at its default.
pub fn message_template(message: &MessageDescriptor) -> String {
    let empty = DynamicMessage::new(message.clone());
    let options = prost_reflect::SerializeOptions::new()
        .skip_default_fields(false)
        .stringify_64_bit_integers(false);
    let mut out = Vec::new();
    let mut serializer = serde_json::Serializer::pretty(&mut out);
    match empty.serialize_with_options(&mut serializer, &options) {
        Ok(()) => String::from_utf8(out).unwrap_or_else(|_| "{}".into()),
        Err(_) => "{}".into(),
    }
}

/// Compile `.proto` files into a descriptor pool.
pub fn compile_proto_files(
    files: &[String],
    include_dirs: &[String],
) -> Result<DescriptorPool, ProtocolError> {
    if files.is_empty() {
        return Err(ProtocolError::Invalid(
            "Add at least one .proto file, or use server reflection.".into(),
        ));
    }
    let mut includes = include_dirs
        .iter()
        .filter(|dir| !dir.trim().is_empty())
        .map(std::path::PathBuf::from)
        .collect::<Vec<_>>();
    for file in files {
        if let Some(parent) = std::path::Path::new(file).parent()
            && !includes.iter().any(|dir| dir == parent)
        {
            includes.push(parent.to_path_buf());
        }
    }
    let set = protox::compile(files, &includes).map_err(|error| {
        ProtocolError::Invalid(format!("Could not compile the .proto files: {error}"))
    })?;
    DescriptorPool::from_file_descriptor_set(set)
        .map_err(|error| ProtocolError::Invalid(format!("Invalid descriptors: {error}")))
}

async fn descriptor_pool(
    prepared: &PreparedRequest,
    settings: &GrpcSettings,
) -> Result<DescriptorPool, ProtocolError> {
    match &settings.descriptors {
        DescriptorSource::ProtoFiles {
            files,
            include_dirs,
        } => {
            let files = files.clone();
            let include_dirs = include_dirs.clone();
            tokio::task::spawn_blocking(move || compile_proto_files(&files, &include_dirs))
                .await
                .map_err(|error| ProtocolError::Invalid(format!("compiling failed: {error}")))?
        }
        DescriptorSource::Reflection => {
            let channel = channel(prepared, settings).await?;
            reflection::fetch_pool(channel, metadata(prepared)?).await
        }
    }
}

/// Connect to the request's origin (the path is not part of a gRPC target).
async fn channel(
    prepared: &PreparedRequest,
    settings: &GrpcSettings,
) -> Result<Channel, ProtocolError> {
    let url = url::Url::parse(&prepared.url)
        .map_err(|error| ProtocolError::Invalid(format!("invalid URL: {error}")))?;
    let tls = match settings.transport {
        GrpcTransport::FromUrl => url.scheme() == "https",
        GrpcTransport::Plaintext => false,
        GrpcTransport::Tls => true,
    };
    let allow_private = prepared.settings.allow_private_network;
    crate::http::validate_http_step_url(url.as_str(), allow_private)
        .map_err(ProtocolError::Invalid)?;
    let host = url
        .host_str()
        .ok_or_else(|| ProtocolError::Invalid("the URL has no host".into()))?
        .to_owned();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| ProtocolError::Invalid("the URL has no port".into()))?;
    // Check the policy against what the name resolves to now.
    crate::http::resolve_http_step_addresses(&url, allow_private)
        .await
        .map_err(ProtocolError::Connection)?;
    let origin = format!("{}://{host}:{port}", if tls { "https" } else { "http" });
    let timeout = connect_timeout(prepared);
    let mut endpoint = Endpoint::from_shared(origin)
        .map_err(|error| ProtocolError::Invalid(format!("invalid gRPC target: {error}")))?
        .connect_timeout(timeout)
        .user_agent(concat!("Switchyard/", env!("CARGO_PKG_VERSION")))
        .map_err(|error| ProtocolError::Invalid(error.to_string()))?;
    if tls {
        let domain = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        endpoint = endpoint
            .tls_config(
                ClientTlsConfig::new()
                    .with_native_roots()
                    .domain_name(domain),
            )
            .map_err(|error| ProtocolError::Invalid(format!("TLS setup failed: {error}")))?;
    }
    endpoint.connect().await.map_err(|error| {
        ProtocolError::Connection(format!("Connecting failed: {}", error_chain(&error)))
    })
}

/// `error` and its sources on one line (tonic's top-level message is
/// "transport error").
fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// The compiled headers as gRPC metadata. Transport headers HTTP/2 or gRPC
/// own are skipped; `-bin` keys take base64 values.
fn metadata(prepared: &PreparedRequest) -> Result<MetadataMap, ProtocolError> {
    use base64::Engine as _;
    let mut map = MetadataMap::new();
    for (name, value) in &prepared.headers {
        let name = name.trim().to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "content-type"
                | "te"
                | "host"
                | "connection"
                | "grpc-timeout"
                | "content-length"
                | "user-agent"
        ) {
            continue;
        }
        if name.ends_with("-bin") {
            let key = MetadataKey::from_bytes(name.as_bytes())
                .map_err(|_| ProtocolError::Invalid(format!("invalid metadata key {name:?}")))?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(value.trim())
                .map_err(|_| ProtocolError::Invalid(format!("metadata {name} must be base64")))?;
            map.append_bin(key, MetadataValue::from_bytes(&bytes));
        } else {
            let key = MetadataKey::from_bytes(name.as_bytes())
                .map_err(|_| ProtocolError::Invalid(format!("invalid metadata key {name:?}")))?;
            let value = MetadataValue::try_from(value.as_str()).map_err(|_| {
                ProtocolError::Invalid(format!("invalid value for metadata {name}"))
            })?;
            map.append(key, value);
        }
    }
    Ok(map)
}

fn metadata_rows(map: &MetadataMap) -> Vec<(String, String)> {
    map.clone()
        .into_headers()
        .iter()
        .map(|(name, value)| {
            (
                name.to_string(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect()
}

/// Encodes and decodes [`DynamicMessage`]s of known descriptors.
#[derive(Clone, Debug)]
pub struct DynamicCodec {
    decode: MessageDescriptor,
}

impl DynamicCodec {
    /// A codec that decodes `decode` messages.
    pub fn new(decode: MessageDescriptor) -> Self {
        Self { decode }
    }
}

impl Codec for DynamicCodec {
    type Encode = DynamicMessage;
    type Decode = DynamicMessage;
    type Encoder = DynamicEncoder;
    type Decoder = DynamicDecoder;

    fn encoder(&mut self) -> Self::Encoder {
        DynamicEncoder
    }

    fn decoder(&mut self) -> Self::Decoder {
        DynamicDecoder(self.decode.clone())
    }
}

/// The encoding half of [`DynamicCodec`].
#[derive(Debug)]
pub struct DynamicEncoder;

impl Encoder for DynamicEncoder {
    type Item = DynamicMessage;
    type Error = tonic::Status;

    fn encode(&mut self, item: Self::Item, dst: &mut EncodeBuf<'_>) -> Result<(), Self::Error> {
        item.encode(dst)
            .map_err(|error| tonic::Status::internal(format!("encoding failed: {error}")))
    }
}

/// The decoding half of [`DynamicCodec`].
#[derive(Debug)]
pub struct DynamicDecoder(MessageDescriptor);

impl Decoder for DynamicDecoder {
    type Item = DynamicMessage;
    type Error = tonic::Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Self::Error> {
        DynamicMessage::decode(self.0.clone(), src)
            .map(Some)
            .map_err(|error| tonic::Status::internal(format!("decoding failed: {error}")))
    }
}

/// Parse a JSON request message.
pub fn message_from_json(
    descriptor: &MessageDescriptor,
    json: &str,
) -> Result<DynamicMessage, ProtocolError> {
    let json = if json.trim().is_empty() { "{}" } else { json };
    let mut deserializer = serde_json::Deserializer::from_str(json);
    let message =
        DynamicMessage::deserialize(descriptor.clone(), &mut deserializer).map_err(|error| {
            ProtocolError::Invalid(format!(
                "The message does not match {}: {error}",
                descriptor.full_name()
            ))
        })?;
    deserializer.end().map_err(|error| {
        ProtocolError::Invalid(format!("Unexpected text after the message: {error}"))
    })?;
    Ok(message)
}

/// A response message as pretty JSON.
fn message_to_json(message: &DynamicMessage) -> String {
    let mut out = Vec::new();
    let mut serializer = serde_json::Serializer::pretty(&mut out);
    let options = prost_reflect::SerializeOptions::new().skip_default_fields(false);
    match message.serialize_with_options(&mut serializer, &options) {
        Ok(()) => String::from_utf8(out).unwrap_or_default(),
        Err(error) => format!("<could not show the message: {error}>"),
    }
}

/// The gRPC status code's name.
pub fn code_name(code: tonic::Code) -> &'static str {
    use tonic::Code;
    match code {
        Code::Ok => "OK",
        Code::Cancelled => "CANCELLED",
        Code::Unknown => "UNKNOWN",
        Code::InvalidArgument => "INVALID_ARGUMENT",
        Code::DeadlineExceeded => "DEADLINE_EXCEEDED",
        Code::NotFound => "NOT_FOUND",
        Code::AlreadyExists => "ALREADY_EXISTS",
        Code::PermissionDenied => "PERMISSION_DENIED",
        Code::ResourceExhausted => "RESOURCE_EXHAUSTED",
        Code::FailedPrecondition => "FAILED_PRECONDITION",
        Code::Aborted => "ABORTED",
        Code::OutOfRange => "OUT_OF_RANGE",
        Code::Unimplemented => "UNIMPLEMENTED",
        Code::Internal => "INTERNAL",
        Code::Unavailable => "UNAVAILABLE",
        Code::DataLoss => "DATA_LOSS",
        Code::Unauthenticated => "UNAUTHENTICATED",
    }
}

/// Run one call and report its messages, headers, trailers and status.
pub(crate) async fn run(prepared: PreparedRequest, settings: GrpcSettings, mut io: SessionIo) {
    io.state(SessionState::Connecting);
    let setup = tokio::select! {
        () = io.cancel.cancelled() => {
            io.state(SessionState::Closed { code: Some(1), reason: "CANCELLED".into() });
            return;
        }
        setup = setup_call(&prepared, &settings) => setup,
    };
    let (channel, method) = match setup {
        Ok(setup) => setup,
        Err(error) => {
            io.state(SessionState::Failed(error.to_string()));
            return;
        }
    };
    let first = match message_from_json(&method.input(), &settings.message) {
        Ok(message) => message,
        Err(error) => {
            io.state(SessionState::Failed(error.to_string()));
            return;
        }
    };
    let metadata = match metadata(&prepared) {
        Ok(metadata) => metadata,
        Err(error) => {
            io.state(SessionState::Failed(error.to_string()));
            return;
        }
    };
    // Outgoing messages: the first now; for client streaming, more as the
    // viewer sends them until "End stream".
    let (outgoing, outgoing_rx) = mpsc::unbounded_channel::<DynamicMessage>();
    log_sent(&io, &first);
    let _ = outgoing.send(first);
    let mut outgoing = method.is_client_streaming().then_some(outgoing);
    let requests = futures::stream::unfold(outgoing_rx, |mut rx| async move {
        rx.recv().await.map(|message| (message, rx))
    });
    let mut request = tonic::Request::new(requests);
    *request.metadata_mut() = metadata;
    if let Some(deadline) = settings.deadline_ms.filter(|ms| *ms > 0) {
        request.set_timeout(Duration::from_millis(deadline));
    }
    let path = match PathAndQuery::from_str(&format!(
        "/{}/{}",
        method.parent_service().full_name(),
        method.name()
    )) {
        Ok(path) => path,
        Err(error) => {
            io.state(SessionState::Failed(format!(
                "invalid method path: {error}"
            )));
            return;
        }
    };
    let mut client = tonic::client::Grpc::new(channel);
    let ready = tokio::select! {
        () = io.cancel.cancelled() => {
            io.state(SessionState::Closed { code: Some(1), reason: "CANCELLED".into() });
            return;
        }
        ready = client.ready() => ready,
    };
    if let Err(error) = ready {
        io.state(SessionState::Failed(format!(
            "Connecting failed: {}",
            error_chain(&error)
        )));
        return;
    }
    // The call is in flight: a client stream can send from here on (its
    // server answers only once the stream ends).
    io.state(SessionState::Open {
        status: 200,
        subprotocol: None,
    });
    let call = client.streaming(request, path, DynamicCodec::new(method.output()));
    tokio::pin!(call);
    let response = loop {
        tokio::select! {
            () = io.cancel.cancelled() => {
                io.state(SessionState::Closed { code: Some(1), reason: "CANCELLED".into() });
                return;
            }
            command = io.commands.recv(), if outgoing.is_some() => {
                client_command(command, &method, &io, &mut outgoing);
            }
            response = &mut call => break response,
        }
    };
    let mut stream = match response {
        Ok(response) => {
            io.emit(StreamEvent::ResponseHeaders(metadata_rows(
                response.metadata(),
            )));
            response.into_inner()
        }
        Err(status) => {
            finish_with_status(&io, &status);
            return;
        }
    };
    loop {
        tokio::select! {
            () = io.cancel.cancelled() => {
                io.state(SessionState::Closed { code: Some(1), reason: "CANCELLED".into() });
                return;
            }
            command = io.commands.recv(), if outgoing.is_some() => {
                client_command(command, &method, &io, &mut outgoing);
            }
            message = stream.message() => match message {
                Ok(Some(message)) => {
                    let json = message_to_json(&message);
                    io.log(LogEntry::now(
                        Direction::Received,
                        EntryKind::Text,
                        method.output().name(),
                        &json,
                        message.encoded_len(),
                    ));
                }
                Ok(None) => {
                    let trailers = stream.trailers().await.ok().flatten();
                    if let Some(trailers) = trailers {
                        io.emit(StreamEvent::Trailers(metadata_rows(&trailers)));
                    }
                    io.state(SessionState::Closed { code: Some(0), reason: "OK".into() });
                    return;
                }
                Err(status) => {
                    finish_with_status(&io, &status);
                    return;
                }
            }
        }
    }
}

/// Handle one viewer command during a client-streaming call.
fn client_command(
    command: Option<SessionCommand>,
    method: &MethodDescriptor,
    io: &SessionIo,
    outgoing: &mut Option<mpsc::UnboundedSender<DynamicMessage>>,
) {
    match command {
        Some(SessionCommand::Send(OutgoingPayload::Text(json))) => {
            match message_from_json(&method.input(), &json) {
                Ok(message) => {
                    log_sent(io, &message);
                    if let Some(sender) = outgoing.as_ref() {
                        let _ = sender.send(message);
                    }
                }
                Err(error) => io.log(LogEntry::error(error.to_string())),
            }
        }
        Some(SessionCommand::CloseSend) | None => {
            *outgoing = None;
            io.log(LogEntry::info("Client stream ended"));
        }
        Some(_) => io.log(LogEntry::error("A gRPC message is JSON text.")),
    }
}

async fn setup_call(
    prepared: &PreparedRequest,
    settings: &GrpcSettings,
) -> Result<(Channel, MethodDescriptor), ProtocolError> {
    if settings.service.trim().is_empty() || settings.method.trim().is_empty() {
        return Err(ProtocolError::Invalid(
            "Choose a service and method first (Load services).".into(),
        ));
    }
    let pool = descriptor_pool(prepared, settings).await?;
    let service = pool
        .get_service_by_name(settings.service.trim())
        .ok_or_else(|| {
            ProtocolError::Invalid(format!("The server has no service {}", settings.service))
        })?;
    let method = service
        .methods()
        .find(|method| method.name() == settings.method.trim())
        .ok_or_else(|| {
            ProtocolError::Invalid(format!(
                "{} has no method {}",
                settings.service, settings.method
            ))
        })?;
    let channel = channel(prepared, settings).await?;
    Ok((channel, method))
}

fn log_sent(io: &SessionIo, message: &DynamicMessage) {
    let json = message_to_json(message);
    io.log(LogEntry::now(
        Direction::Sent,
        EntryKind::Text,
        message.descriptor().name(),
        &json,
        message.encoded_len(),
    ));
}

fn finish_with_status(io: &SessionIo, status: &tonic::Status) {
    let trailers = metadata_rows(status.metadata());
    if !trailers.is_empty() {
        io.emit(StreamEvent::Trailers(trailers));
    }
    let code = status.code();
    let reason = if status.message().is_empty() {
        code_name(code).to_owned()
    } else {
        format!("{} · {}", code_name(code), status.message())
    };
    io.state(SessionState::Closed {
        code: Some(code as u32),
        reason,
    });
}

/// The gRPC server reflection client (v1, falling back to v1alpha).
mod reflection {
    use std::collections::{BTreeMap, BTreeSet};

    use prost_reflect::DescriptorPool;
    use prost_types::FileDescriptorProto;
    use tonic::metadata::MetadataMap;
    use tonic::transport::Channel;

    use super::{ProtocolError, error_chain};

    macro_rules! reflection_client {
        ($name:ident, $module:path) => {
            mod $name {
                use super::*;
                use pb::server_reflection_client::ServerReflectionClient;
                use pb::server_reflection_request::MessageRequest;
                use pb::server_reflection_response::MessageResponse;
                use $module as pb;

                /// Ask one question; `Ok(None)` when the server lacks this
                /// reflection version.
                async fn ask(
                    channel: &Channel,
                    metadata: &MetadataMap,
                    request: MessageRequest,
                ) -> Result<Option<MessageResponse>, ProtocolError> {
                    let mut client = ServerReflectionClient::new(channel.clone());
                    let mut call =
                        tonic::Request::new(futures::stream::iter([pb::ServerReflectionRequest {
                            host: String::new(),
                            message_request: Some(request),
                        }]));
                    *call.metadata_mut() = metadata.clone();
                    let response = match client.server_reflection_info(call).await {
                        Ok(response) => response,
                        Err(status) if status.code() == tonic::Code::Unimplemented => {
                            return Ok(None);
                        }
                        Err(status) => {
                            return Err(ProtocolError::Connection(format!(
                                "Server reflection failed: {} {}",
                                super::super::code_name(status.code()),
                                status.message()
                            )));
                        }
                    };
                    let mut stream = response.into_inner();
                    match stream.message().await {
                        Ok(Some(reply)) => match reply.message_response {
                            Some(MessageResponse::ErrorResponse(error)) => {
                                Err(ProtocolError::Connection(format!(
                                    "Server reflection: {}",
                                    error.error_message
                                )))
                            }
                            other => Ok(other),
                        },
                        Ok(None) => Err(ProtocolError::Connection(
                            "Server reflection sent no answer".into(),
                        )),
                        Err(status) if status.code() == tonic::Code::Unimplemented => Ok(None),
                        Err(status) => Err(ProtocolError::Connection(format!(
                            "Server reflection failed: {}",
                            status.message()
                        ))),
                    }
                }

                fn files(response: Option<MessageResponse>) -> Vec<Vec<u8>> {
                    match response {
                        Some(MessageResponse::FileDescriptorResponse(files)) => {
                            files.file_descriptor_proto
                        }
                        _ => Vec::new(),
                    }
                }

                /// Every file the server's services need; `None` when this
                /// reflection version is not served.
                pub(super) async fn fetch(
                    channel: &Channel,
                    metadata: &MetadataMap,
                ) -> Result<Option<Vec<Vec<u8>>>, ProtocolError> {
                    let Some(listed) = ask(
                        channel,
                        metadata,
                        MessageRequest::ListServices(String::new()),
                    )
                    .await?
                    else {
                        return Ok(None);
                    };
                    let services = match listed {
                        MessageResponse::ListServicesResponse(list) => list
                            .service
                            .into_iter()
                            .map(|service| service.name)
                            .collect::<Vec<_>>(),
                        _ => Vec::new(),
                    };
                    let mut encoded = Vec::new();
                    for service in services {
                        encoded.extend(files(
                            ask(
                                channel,
                                metadata,
                                MessageRequest::FileContainingSymbol(service),
                            )
                            .await?,
                        ));
                    }
                    Ok(Some(encoded))
                }

                /// One file by name, for a dependency the first answers
                /// did not include.
                pub(super) async fn file(
                    channel: &Channel,
                    metadata: &MetadataMap,
                    name: String,
                ) -> Result<Vec<Vec<u8>>, ProtocolError> {
                    Ok(files(
                        ask(channel, metadata, MessageRequest::FileByFilename(name)).await?,
                    ))
                }
            }
        };
    }

    reflection_client!(v1, tonic_reflection::pb::v1);
    reflection_client!(v1alpha, tonic_reflection::pb::v1alpha);

    /// Build a pool from what the server reflects.
    pub(super) async fn fetch_pool(
        channel: Channel,
        metadata: MetadataMap,
    ) -> Result<DescriptorPool, ProtocolError> {
        let (encoded, alpha) = match v1::fetch(&channel, &metadata).await? {
            Some(files) => (files, false),
            None => match v1alpha::fetch(&channel, &metadata).await? {
                Some(files) => (files, true),
                None => {
                    return Err(ProtocolError::Connection(
                        "The server does not offer reflection; import its .proto files instead."
                            .into(),
                    ));
                }
            },
        };
        let mut files = BTreeMap::new();
        add_files(&mut files, encoded)?;
        // Fetch dependencies the answers referred to but did not include.
        let mut asked = BTreeSet::new();
        loop {
            let missing = files
                .values()
                .flat_map(|file: &FileDescriptorProto| file.dependency.iter().cloned())
                .filter(|name| !files.contains_key(name) && !asked.contains(name))
                .collect::<BTreeSet<_>>();
            if missing.is_empty() {
                break;
            }
            for name in missing {
                asked.insert(name.clone());
                let encoded = if alpha {
                    v1alpha::file(&channel, &metadata, name).await?
                } else {
                    v1::file(&channel, &metadata, name).await?
                };
                add_files(&mut files, encoded)?;
            }
        }
        let mut pool = DescriptorPool::new();
        pool.add_file_descriptor_protos(files.into_values())
            .map_err(|error| {
                ProtocolError::Invalid(format!(
                    "The reflected descriptors are incomplete: {}",
                    error_chain(&error)
                ))
            })?;
        Ok(pool)
    }

    fn add_files(
        files: &mut BTreeMap<String, FileDescriptorProto>,
        encoded: Vec<Vec<u8>>,
    ) -> Result<(), ProtocolError> {
        use prost::Message as _;
        for bytes in encoded {
            let file = FileDescriptorProto::decode(bytes.as_slice()).map_err(|error| {
                ProtocolError::Invalid(format!("invalid reflected descriptor: {error}"))
            })?;
            files.insert(file.name().to_owned(), file);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
