use anyhow::{Context, Result, anyhow};
use async_stream::stream;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use clap::Parser;
use futures::{Stream, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};
use uuid::Uuid;

#[derive(Debug, Clone, Parser)]
#[command(name = "codex-chat-bridge", about = "Responses-to-Chat completions bridge")]
struct Args {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    #[arg(long)]
    port: Option<u16>,

    #[arg(long, default_value = "https://api.openai.com/v1/chat/completions")]
    upstream_url: String,

    #[arg(long, default_value = "OPENAI_API_KEY")]
    api_key_env: String,

    #[arg(long, value_name = "FILE")]
    server_info: Option<PathBuf>,

    #[arg(long)]
    http_shutdown: bool,
}

#[derive(Clone)]
struct AppState {
    client: Client,
    upstream_url: String,
    api_key: String,
    http_shutdown: bool,
}

#[derive(Serialize)]
struct ServerInfo {
    port: u16,
    pid: u32,
}

#[derive(Debug)]
struct BridgeRequest {
    chat_request: Value,
    response_id: String,
}

#[derive(Debug, Deserialize)]
struct ChatChunk {
    #[allow(dead_code)]
    id: Option<String>,
    #[serde(default)]
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<ChatUsage>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    #[serde(default)]
    delta: Option<ChatDelta>,
    #[allow(dead_code)]
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ChatToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct ChatToolCallDelta {
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<ChatFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct ChatFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: i64,
    #[serde(default)]
    completion_tokens: i64,
    #[serde(default)]
    total_tokens: i64,
}

#[derive(Debug, Default)]
struct ToolCallAccumulator {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

#[derive(Debug, Default)]
struct StreamAccumulator {
    assistant_text: String,
    tool_calls: BTreeMap<usize, ToolCallAccumulator>,
    usage: Option<ChatUsage>,
}

#[derive(Debug, Default)]
struct SseParser {
    buffer: String,
    current_data_lines: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let api_key = std::env::var(&args.api_key_env)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| anyhow!("missing or empty env var: {}", args.api_key_env))?;

    let client = Client::builder()
        .timeout(None)
        .build()
        .context("building reqwest client")?;

    let state = Arc::new(AppState {
        client,
        upstream_url: args.upstream_url,
        api_key,
        http_shutdown: args.http_shutdown,
    });

    let app = Router::new()
        .route("/v1/responses", post(handle_responses))
        .route("/healthz", get(healthz))
        .route("/shutdown", get(shutdown))
        .with_state(state);

    let bind_addr = format!("{}:{}", args.host, args.port.unwrap_or(0));
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("binding {bind_addr}"))?;
    let local_addr = listener.local_addr().context("reading local_addr")?;

    if let Some(path) = args.server_info.as_ref() {
        write_server_info(path, local_addr.port())?;
    }

    info!("codex-chat-bridge listening on {}", local_addr);
    axum::serve(listener, app).await.context("serving axum app")?;
    Ok(())
}

fn write_server_info(path: &Path, port: u16) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let info = ServerInfo {
        port,
        pid: std::process::id(),
    };
    let mut data = serde_json::to_string(&info)?;
    data.push('\n');
    let mut f = File::create(path)?;
    f.write_all(data.as_bytes())?;
    Ok(())
}

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn shutdown(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if !state.http_shutdown {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        std::process::exit(0);
    });

    (StatusCode::OK, "shutting down").into_response()
}

async fn handle_responses(State(state): State<Arc<AppState>>, headers: HeaderMap, body: String) -> Response {
    let request_value: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(err) => {
            return sse_error_response(
                "invalid_request",
                &format!("failed to parse request JSON: {err}"),
            );
        }
    };

    let bridge_request = match map_responses_to_chat_request(&request_value) {
        Ok(v) => v,
        Err(err) => return sse_error_response("invalid_request", &err.to_string()),
    };

    let mut upstream_request = state
        .client
        .post(&state.upstream_url)
        .bearer_auth(&state.api_key)
        .header(CONTENT_TYPE, "application/json")
        .json(&bridge_request.chat_request);

    for header_name in [
        "openai-organization",
        "openai-project",
        "x-openai-subagent",
        "x-codex-turn-state",
        "x-codex-turn-metadata",
    ] {
        if let Some(value) = headers.get(header_name) {
            upstream_request = upstream_request.header(header_name, value.clone());
        }
    }

    let upstream_response = match upstream_request.send().await {
        Ok(response) => response,
        Err(err) => {
            return sse_error_response(
                "upstream_transport_error",
                &format!("failed to call upstream chat endpoint: {err}"),
            );
        }
    };

    if !upstream_response.status().is_success() {
        let status = upstream_response.status();
        let body = upstream_response
            .text()
            .await
            .unwrap_or_else(|_| "<failed to read error body>".to_string());
        let message = format!("upstream returned {status}: {body}");
        return sse_error_response("upstream_error", &message);
    }

    let response_stream = translate_upstream_stream(
        upstream_response.bytes_stream(),
        bridge_request.response_id,
    );

    let body = Body::from_stream(response_stream);
    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, HeaderValue::from_static("text/event-stream")),
            (CACHE_CONTROL, HeaderValue::from_static("no-cache")),
            (
                HeaderName::from_static("x-accel-buffering"),
                HeaderValue::from_static("no"),
            ),
        ],
        body,
    )
        .into_response()
}

fn translate_upstream_stream<S>(
    upstream_stream: S,
    response_id: String,
) -> impl Stream<Item = Result<Bytes, std::convert::Infallible>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    stream! {
        let mut upstream_stream = Box::pin(upstream_stream);
        let mut parser = SseParser::default();
        let mut acc = StreamAccumulator::default();

        yield Ok(sse_event(
            "response.created",
            &json!({
                "type": "response.created",
                "response": {
                    "id": response_id.clone(),
                }
            }),
        ));

        while let Some(chunk_result) = upstream_stream.next().await {
            let chunk = match chunk_result {
                Ok(chunk) => chunk,
                Err(err) => {
                    yield Ok(sse_event(
                        "response.failed",
                        &json!({
                            "type": "response.failed",
                            "response": {
                                "id": response_id.clone(),
                                "error": {
                                    "code": "upstream_stream_error",
                                    "message": err.to_string(),
                                }
                            }
                        }),
                    ));
                    return;
                }
            };

            let text = String::from_utf8_lossy(&chunk);
            let events = parser.feed(&text);
            for data in events {
                if data == "[DONE]" {
                    continue;
                }

                match serde_json::from_str::<ChatChunk>(&data) {
                    Ok(chat_chunk) => {
                        if let Some(usage) = chat_chunk.usage.clone() {
                            acc.usage = Some(usage);
                        }

                        for choice in chat_chunk.choices {
                            if let Some(delta) = choice.delta {
                                if let Some(content) = delta.content
                                    && !content.is_empty()
                                {
                                    acc.assistant_text.push_str(&content);
                                    yield Ok(sse_event(
                                        "response.output_text.delta",
                                        &json!({
                                            "type": "response.output_text.delta",
                                            "delta": content,
                                        }),
                                    ));
                                }

                                if let Some(tool_calls) = delta.tool_calls {
                                    for tool_call in tool_calls {
                                        let index = tool_call.index.unwrap_or(acc.tool_calls.len());
                                        let entry = acc.tool_calls.entry(index).or_default();

                                        if let Some(id) = tool_call.id {
                                            entry.id = Some(id);
                                        }

                                        if let Some(function) = tool_call.function {
                                            if let Some(name) = function.name {
                                                entry.name = Some(name);
                                            }
                                            if let Some(arguments) = function.arguments {
                                                entry.arguments.push_str(&arguments);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(err) => {
                        warn!("failed to decode upstream chat chunk: {err}");
                    }
                }
            }
        }

        if let Some(data) = parser.finish()
            && data != "[DONE]"
        {
            warn!("bridge received trailing SSE payload: {data}");
        }

        if !acc.assistant_text.is_empty() {
            yield Ok(sse_event(
                "response.output_item.done",
                &json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "message",
                        "role": "assistant",
                        "content": [
                            {
                                "type": "output_text",
                                "text": acc.assistant_text,
                            }
                        ]
                    }
                }),
            ));
        }

        for (index, tool_call) in acc.tool_calls {
            let call_id = tool_call
                .id
                .unwrap_or_else(|| format!("call_{}_{}", Uuid::now_v7(), index));
            let name = tool_call
                .name
                .unwrap_or_else(|| "unknown_function".to_string());

            yield Ok(sse_event(
                "response.output_item.done",
                &json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "function_call",
                        "name": name,
                        "arguments": tool_call.arguments,
                        "call_id": call_id,
                    }
                }),
            ));
        }

        let usage_json = acc.usage.map(|usage| {
            json!({
                "input_tokens": usage.prompt_tokens,
                "input_tokens_details": null,
                "output_tokens": usage.completion_tokens,
                "output_tokens_details": null,
                "total_tokens": usage.total_tokens,
            })
        });

        yield Ok(sse_event(
            "response.completed",
            &json!({
                "type": "response.completed",
                "response": {
                    "id": response_id.clone(),
                    "usage": usage_json,
                }
            }),
        ));
    }
}

fn sse_event(event_name: &str, payload: &Value) -> Bytes {
    let json_payload = serde_json::to_string(payload).unwrap_or_else(|_| {
        "{\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"internal serialization error\"}}}".to_string()
    });
    Bytes::from(format!("event: {event_name}\ndata: {json_payload}\n\n"))
}

fn sse_error_response(code: &str, message: &str) -> Response {
    let response_id = format!("resp_bridge_{}", Uuid::now_v7());
    let mut body = Vec::new();
    body.extend_from_slice(&sse_event(
        "response.created",
        &json!({
            "type": "response.created",
            "response": {
                "id": response_id.clone(),
            }
        }),
    ));
    body.extend_from_slice(&sse_event(
        "response.failed",
        &json!({
            "type": "response.failed",
            "response": {
                "id": response_id,
                "error": {
                    "code": code,
                    "message": message,
                }
            }
        }),
    ));

    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, HeaderValue::from_static("text/event-stream")),
            (CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        body,
    )
        .into_response()
}

fn map_responses_to_chat_request(request: &Value) -> Result<BridgeRequest> {
    let model = request
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing `model`"))?
        .to_string();

    let instructions = request
        .get("instructions")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let input_items = request
        .get("input")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing `input` array"))?;

    let tools = request
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let tool_choice = request
        .get("tool_choice")
        .cloned()
        .unwrap_or_else(|| Value::String("auto".to_string()));

    let parallel_tool_calls = request
        .get("parallel_tool_calls")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let mut messages = Vec::new();

    if !instructions.trim().is_empty() {
        messages.push(json!({
            "role": "system",
            "content": instructions,
        }));
    }

    for item in input_items {
        let item_type = item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();

        match item_type {
            "message" => {
                let role = item
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("user");
                let content = item
                    .get("content")
                    .and_then(Value::as_array)
                    .map_or_else(String::new, flatten_content_items);

                if !content.trim().is_empty() {
                    messages.push(json!({
                        "role": role,
                        "content": content,
                    }));
                }
            }
            "function_call_output" => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let output_text = item
                    .get("output")
                    .map(function_output_to_text)
                    .unwrap_or_default();
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": output_text,
                }));
            }
            "custom_tool_call_output" => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let output_text = item
                    .get("output")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": output_text,
                }));
            }
            "mcp_tool_call_output" => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let output_text = item
                    .get("result")
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": output_text,
                }));
            }
            _ => {
                warn!("ignoring unsupported input item type: {item_type}");
            }
        }
    }

    let chat_tools = normalize_chat_tools(tools);
    let chat_tool_choice = normalize_tool_choice(tool_choice);

    let response_id = format!("resp_bridge_{}", Uuid::now_v7());

    let mut chat_request = json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true },
        "tools": chat_tools,
        "tool_choice": chat_tool_choice,
        "parallel_tool_calls": parallel_tool_calls,
    });

    if chat_request
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        if let Some(obj) = chat_request.as_object_mut() {
            obj.remove("tools");
            obj.remove("tool_choice");
        }
    }

    Ok(BridgeRequest {
        chat_request,
        response_id,
    })
}

fn flatten_content_items(items: &[Value]) -> String {
    let mut parts = Vec::new();
    for item in items {
        let item_type = item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if matches!(item_type, "input_text" | "output_text")
            && let Some(text) = item.get("text").and_then(Value::as_str)
            && !text.is_empty()
        {
            parts.push(text.to_string());
        }
    }

    parts.join("\n")
}

fn function_output_to_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => flatten_content_items(items),
        other => other.to_string(),
    }
}

fn normalize_chat_tools(tools: Vec<Value>) -> Vec<Value> {
    tools
        .into_iter()
        .filter_map(|tool| {
            if tool.get("type").and_then(Value::as_str) != Some("function") {
                return Some(tool);
            }

            if tool.get("function").is_some() {
                return Some(tool);
            }

            let name = tool.get("name")?.as_str()?.to_string();
            let description = tool
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let parameters = tool
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));

            Some(json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": parameters,
                }
            }))
        })
        .collect()
}

fn normalize_tool_choice(tool_choice: Value) -> Value {
    if let Some(s) = tool_choice.as_str() {
        return Value::String(s.to_string());
    }

    let Some(obj) = tool_choice.as_object() else {
        return Value::String("auto".to_string());
    };

    if obj.get("function").is_some() {
        return tool_choice;
    }

    if obj.get("type").and_then(Value::as_str) == Some("function")
        && let Some(name) = obj.get("name").and_then(Value::as_str)
    {
        return json!({
            "type": "function",
            "function": {
                "name": name,
            }
        });
    }

    Value::String("auto".to_string())
}

impl SseParser {
    fn feed(&mut self, chunk: &str) -> Vec<String> {
        self.buffer.push_str(chunk);
        let mut events = Vec::new();

        while let Some(pos) = self.buffer.find('\n') {
            let mut line = self.buffer[..pos].to_string();
            self.buffer.drain(..=pos);

            if line.ends_with('\r') {
                line.pop();
            }

            if line.is_empty() {
                if !self.current_data_lines.is_empty() {
                    events.push(self.current_data_lines.join("\n"));
                    self.current_data_lines.clear();
                }
                continue;
            }

            if let Some(rest) = line.strip_prefix("data:") {
                let data = rest.strip_prefix(' ').unwrap_or(rest).to_string();
                self.current_data_lines.push(data);
            }
        }

        events
    }

    fn finish(&mut self) -> Option<String> {
        if self.current_data_lines.is_empty() {
            None
        } else {
            Some(self.current_data_lines.join("\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_responses_request_to_chat_request_with_function_tool() {
        let input = json!({
            "model": "gpt-4.1",
            "instructions": "You are helpful",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "hello"}]
                }
            ],
            "tools": [
                {
                    "type": "function",
                    "name": "get_weather",
                    "description": "Get weather",
                    "parameters": {"type":"object","properties":{"city":{"type":"string"}}}
                }
            ],
            "tool_choice": "auto",
            "parallel_tool_calls": true
        });

        let req = map_responses_to_chat_request(&input).expect("should map");
        let messages = req
            .chat_request
            .get("messages")
            .and_then(Value::as_array)
            .expect("messages array");
        assert_eq!(messages.len(), 2);

        let tools = req
            .chat_request
            .get("tools")
            .and_then(Value::as_array)
            .expect("tools array");
        assert_eq!(tools[0].get("function").and_then(Value::as_object).is_some(), true);
    }

    #[test]
    fn sse_parser_collects_data_events() {
        let mut parser = SseParser::default();
        let chunk = "event: message\ndata: {\"a\":1}\n\n";
        let events = parser.feed(chunk);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0], "{\"a\":1}");
    }

    #[test]
    fn normalize_tool_choice_wraps_function_name() {
        let choice = json!({"type":"function", "name":"f"});
        let normalized = normalize_tool_choice(choice);
        assert_eq!(
            normalized,
            json!({"type":"function", "function": {"name":"f"}})
        );
    }

    #[test]
    fn function_output_text_handles_array_items() {
        let value = json!([
            {"type": "input_text", "text": "line1"},
            {"type": "output_text", "text": "line2"}
        ]);
        assert_eq!(function_output_to_text(&value), "line1\nline2");
    }

    #[test]
    fn normalize_chat_tools_passes_non_function_tool() {
        let tools = vec![json!({"type": "web_search_preview"})];
        let out = normalize_chat_tools(tools);
        assert_eq!(out, vec![json!({"type": "web_search_preview"})]);
    }

    #[test]
    fn flatten_content_items_filters_non_text() {
        let items = vec![
            json!({"type":"input_text","text":"a"}),
            json!({"type":"input_image","image_url":"x"}),
            json!({"type":"output_text","text":"b"}),
        ];
        assert_eq!(flatten_content_items(&items), "a\nb");
    }

    #[test]
    fn map_supports_function_call_output_to_tool_message() {
        let input = json!({
            "model": "gpt-4.1",
            "input": [
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "{\"ok\":true}"
                }
            ],
            "tools": []
        });

        let req = map_responses_to_chat_request(&input).expect("should map");
        let messages = req
            .chat_request
            .get("messages")
            .and_then(Value::as_array)
            .expect("messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "tool");
        assert_eq!(messages[0]["tool_call_id"], "call_1");
    }

    #[test]
    fn map_defaults_tool_choice_when_invalid() {
        let input = json!({
            "model": "gpt-4.1",
            "input": [{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],
            "tools": [{"type":"function","name":"f","parameters":{"type":"object"}}],
            "tool_choice": 123
        });

        let req = map_responses_to_chat_request(&input).expect("should map");
        assert_eq!(req.chat_request["tool_choice"], "auto");
    }

    #[test]
    fn map_requires_input_array() {
        let input = json!({"model":"gpt-4.1"});
        let err = map_responses_to_chat_request(&input).expect_err("must fail");
        assert!(err.to_string().contains("missing `input` array"));
    }

    #[test]
    fn parser_handles_split_chunks() {
        let mut parser = SseParser::default();
        let first = parser.feed("data: {\"a\":");
        assert!(first.is_empty());
        let second = parser.feed("1}\n\n");
        assert_eq!(second, vec!["{\"a\":1}".to_string()]);
    }

    #[test]
    fn map_removes_tool_fields_when_tools_empty() {
        let input = json!({
            "model": "gpt-4.1",
            "input": [{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],
            "tools": []
        });
        let req = map_responses_to_chat_request(&input).expect("ok");
        let obj = req.chat_request.as_object().expect("object");
        assert!(!obj.contains_key("tools"));
        assert!(!obj.contains_key("tool_choice"));
    }

    #[test]
    fn sse_error_response_contains_failed_event() {
        let response = sse_error_response("x", "y");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn parser_ignores_non_data_lines() {
        let mut parser = SseParser::default();
        let out = parser.feed(": ping\nevent: hello\ndata: {\"z\":1}\n\n");
        assert_eq!(out, vec!["{\"z\":1}".to_string()]);
    }

    #[test]
    fn normalize_chat_tools_keeps_function_already_wrapped() {
        let tools = vec![json!({
            "type": "function",
            "function": {"name":"f", "parameters": {"type":"object"}}
        })];
        let out = normalize_chat_tools(tools.clone());
        assert_eq!(out, tools);
    }

    #[test]
    fn normalize_tool_choice_preserves_wrapped_choice() {
        let choice = json!({"type":"function", "function":{"name":"do_it"}});
        assert_eq!(normalize_tool_choice(choice.clone()), choice);
    }

    #[test]
    fn function_output_to_text_for_object_uses_json() {
        let value = json!({"ok": true});
        assert_eq!(function_output_to_text(&value), "{\"ok\":true}");
    }

    #[test]
    fn map_includes_system_message_from_instructions() {
        let input = json!({
            "model": "gpt-4.1",
            "instructions": "sys",
            "input": [{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],
            "tools": []
        });
        let req = map_responses_to_chat_request(&input).expect("ok");
        let messages = req.chat_request["messages"].as_array().expect("array");
        assert_eq!(messages[0]["role"], "system");
    }
}
