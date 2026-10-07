//! A scripted stand-in for the OpenAI Responses API (what Codex talks to), so the real Codex
//! CLI can be driven without an account: each request answers with the next scripted tool
//! call, then a final message. Every request body is kept for assertions.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A running mock: its port and the request bodies it received.
pub struct MockModel {
    pub port: u16,
    pub requests: Arc<Mutex<Vec<Value>>>,
}

/// One scripted step: a tool in namespace `ns` with JSON arguments.
pub struct Call {
    pub ns: &'static str,
    pub name: &'static str,
    pub args: Value,
}

async fn read_request(s: &mut tokio::net::TcpStream) -> Option<Value> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        let n = s.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_lowercase();
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    while buf.len() < head_end + len {
        let n = s.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    serde_json::from_slice(&buf[head_end..]).ok()
}

fn event(v: &Value) -> String {
    format!(
        "event: {}\ndata: {v}\n\n",
        v["type"].as_str().unwrap_or_default()
    )
}

/// Start the mock; `final_text` is the message after the last call.
pub async fn start(script: Vec<Call>, final_text: &'static str) -> MockModel {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests: Arc<Mutex<Vec<Value>>> = Arc::default();
    let script = Arc::new(script);
    let seen = requests.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            let (script, seen) = (script.clone(), seen.clone());
            tokio::spawn(async move {
                let Some(body) = read_request(&mut s).await else {
                    return;
                };
                let done = body["input"].as_array().map_or(0, |i| {
                    i.iter()
                        .filter(|x| x["type"] == "function_call_output")
                        .count()
                });
                seen.lock().unwrap().push(body);
                let item = match script.get(done) {
                    Some(c) => json!({
                        "type": "function_call", "id": format!("fc_{done}"),
                        "call_id": format!("call_{done}"), "namespace": c.ns,
                        "name": c.name, "arguments": c.args.to_string(),
                    }),
                    None => json!({
                        "type": "message", "role": "assistant", "id": "msg_final",
                        "content": [{"type": "output_text", "text": final_text}],
                    }),
                };
                let mut out = String::from(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                );
                out += &event(
                    &json!({"type": "response.created", "response": {"id": format!("r{done}")}}),
                );
                out += &event(
                    &json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
                );
                out += &event(
                    &json!({"type": "response.completed", "response": {"id": format!("r{done}"),
                    "usage": {"input_tokens": 10, "input_tokens_details": {"cached_tokens": 0},
                              "output_tokens": 5, "output_tokens_details": {"reasoning_tokens": 0},
                              "total_tokens": 15}}}),
                );
                let _ = s.write_all(out.as_bytes()).await;
                let _ = s.shutdown().await;
            });
        }
    });
    MockModel { port, requests }
}

impl MockModel {
    /// Codex `-c` overrides that point it at this mock.
    pub fn codex_args(&self) -> Vec<String> {
        [
            "model_provider=\"mock\"".to_owned(),
            "model_providers.mock.name=\"mock\"".to_owned(),
            format!(
                "model_providers.mock.base_url=\"http://127.0.0.1:{}/v1\"",
                self.port
            ),
            "model_providers.mock.wire_api=\"responses\"".to_owned(),
            "model=\"mock-model\"".to_owned(),
        ]
        .into_iter()
        .flat_map(|c| ["-c".to_owned(), c])
        .collect()
    }
}

/// The same, as the Gemini API (`…:streamGenerateContent?alt=sse`): each request answers
/// with the next scripted function call (by its Gemini name), then `final_text`.
pub async fn start_gemini(
    script: Vec<(&'static str, Value)>,
    final_text: &'static str,
) -> MockModel {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests: Arc<Mutex<Vec<Value>>> = Arc::default();
    let script = Arc::new(script);
    let seen = requests.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            let (script, seen) = (script.clone(), seen.clone());
            tokio::spawn(async move {
                // The request line tells streaming from helper calls.
                let mut peek = [0u8; 512];
                let n = s.peek(&mut peek).await.unwrap_or(0);
                let line = String::from_utf8_lossy(&peek[..n]).into_owned();
                let Some(body) = read_request(&mut s).await else {
                    return;
                };
                let done = body["contents"].as_array().map_or(0, |c| {
                    c.iter()
                        .flat_map(|m| m["parts"].as_array().cloned().unwrap_or_default())
                        .filter(|p| p.get("functionResponse").is_some())
                        .count()
                });
                let part = match script.get(done) {
                    Some((name, args)) => json!({"functionCall": {"name": name, "args": args}}),
                    None => json!({"text": final_text}),
                };
                let reply = json!({
                    "candidates": [{"content": {"role": "model", "parts": [part]},
                                    "finishReason": "STOP", "index": 0}],
                    "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5,
                                      "totalTokenCount": 15}
                });
                let out = if line.contains(":streamGenerateContent") {
                    seen.lock().unwrap().push(body);
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {reply}\r\n\r\n"
                    )
                } else {
                    let r = if line.contains(":countTokens") {
                        json!({"totalTokens": 10})
                    } else {
                        json!({"candidates": [{"content": {"role": "model", "parts": [{"text":
                            "{\"reasoning\":\"done\",\"next_speaker\":\"user\",\"model_choice\":\"flash\"}"}]},
                            "finishReason": "STOP"}],
                            "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2}})
                    };
                    let body = r.to_string();
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                let _ = s.write_all(out.as_bytes()).await;
                let _ = s.shutdown().await;
            });
        }
    });
    MockModel { port, requests }
}
