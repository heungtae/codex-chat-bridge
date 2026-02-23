use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use async_stream::stream;
use axum::Router;
use axum::body::Body;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderName;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header::CACHE_CONTROL;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use clap::Parser;
use clap::ValueEnum;
use futures::Stream;
use futures::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fs::File;
use std::fs::{self};
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;
use tracing::warn;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Deserialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum WireApi {
    Chat,
    Responses,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IncomingApi {
    Responses,
    Chat,
}

const FORWARDED_UPSTREAM_HEADERS: [&str; 5] = [
    "openai-organization",
    "openai-project",
    "x-openai-subagent",
    "x-codex-turn-state",
    "x-codex-turn-metadata",
];

#[derive(Debug, Clone, Parser)]
#[command(
    name = "codex-chat-bridge",
    about = "Responses-to-Chat completions bridge",
    version
)]
struct Args {
    #[arg(
        long,
        value_name = "FILE",
        help = "config file path (default: ~/.config/codex-chat-bridge/conf.toml)"
    )]
    config: Option<PathBuf>,

    #[arg(long)]
    host: Option<String>,

    #[arg(long)]
    port: Option<u16>,

    #[arg(long)]
    upstream_url: Option<String>,

    #[arg(long, value_enum)]
    upstream_wire: Option<WireApi>,

    #[arg(long)]
    api_key_env: Option<String>,

    #[arg(long, value_name = "FILE")]
    server_info: Option<PathBuf>,

    #[arg(long)]
    http_shutdown: bool,

    #[arg(long, help = "enable verbose bridge logs (request/response payloads)")]
    verbose_logging: bool,

    #[arg(
        long = "drop-tool-type",
        value_name = "TYPE",
        action = clap::ArgAction::Append,
        help = "drop tool entries whose `type` matches this value; can be repeated"
    )]
    drop_tool_types: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct FileConfig {
    host: Option<String>,
    port: Option<u16>,
    upstream_url: Option<String>,
    upstream_wire: Option<WireApi>,
    api_key_env: Option<String>,
    server_info: Option<PathBuf>,
    http_shutdown: Option<bool>,
    verbose_logging: Option<bool>,
    drop_tool_types: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
struct ResolvedConfig {
    host: String,
    port: Option<u16>,
    upstream_url: String,
    upstream_wire: WireApi,
    api_key_env: String,
    server_info: Option<PathBuf>,
    http_shutdown: bool,
    verbose_logging: bool,
    drop_tool_types: Vec<String>,
}

const DEFAULT_CONFIG_TEMPLATE: &str = r#"# codex-chat-bridge runtime configuration
#
# Priority: CLI flags > config file > built-in defaults

# host = "127.0.0.1"
# port = 8787
# upstream_url = "https://api.openai.com/v1/chat/completions"
# upstream_wire = "chat" # chat | responses
# api_key_env = "OPENAI_API_KEY"
# server_info = "/tmp/codex-chat-bridge-info.json"
# http_shutdown = false
# verbose_logging = false
# drop_tool_types = ["web_search", "web_search_preview"]
"#;

#[derive(Clone)]
struct AppState {
    client: Client,
    upstream_url: String,
    upstream_wire: WireApi,
    api_key: String,
    http_shutdown: bool,
    verbose_logging: bool,
    drop_tool_types: HashSet<String>,
}

#[derive(Serialize)]
struct ServerInfo {
    port: u16,
    pid: u32,
}

#[derive(Debug)]
struct BridgeRequest {
    chat_request: Value,
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
    let config_path = resolve_config_path(args.config.clone())?;
    ensure_default_config_file(&config_path)?;
    let file_config = load_file_config(&config_path)?;
    let config = resolve_config(args, file_config);

    let api_key = std::env::var(&config.api_key_env)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| anyhow!("missing or empty env var: {}", config.api_key_env))?;

    let client = Client::builder()
        .build()
        .context("building reqwest client")?;

    let state = Arc::new(AppState {
        client,
        upstream_url: config.upstream_url.clone(),
        upstream_wire: config.upstream_wire,
        api_key,
        http_shutdown: config.http_shutdown,
        verbose_logging: config.verbose_logging,
        drop_tool_types: config.drop_tool_types.into_iter().collect(),
    });

    let app = Router::new()
        .route("/v1/responses", post(handle_responses))
        .route("/v1/chat/completions", post(handle_chat_completions))
        .route("/healthz", get(healthz))
        .route("/shutdown", get(shutdown))
        .with_state(state);

    let bind_addr = format!("{}:{}", config.host, config.port.unwrap_or(0));
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("binding {bind_addr}"))?;
    let local_addr = listener.local_addr().context("reading local_addr")?;

    if let Some(path) = config.server_info.as_ref() {
        write_server_info(path, local_addr.port())?;
    }

    info!("codex-chat-bridge listening on {}", local_addr);
    axum::serve(listener, app)
        .await
        .context("serving axum app")?;
    Ok(())
}

fn load_file_config(path: &Path) -> Result<Option<FileConfig>> {
    if !path.exists() {
        return Ok(None);
    }

    let raw = fs::read_to_string(path)
        .with_context(|| format!("reading config file {}", path.display()))?;
    let parsed: FileConfig = toml::from_str(&raw)
        .with_context(|| format!("parsing config file {}", path.display()))?;
    info!("loaded config file {}", path.display());
    Ok(Some(parsed))
}

fn resolve_config_path(cli_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = cli_path {
        return Ok(path);
    }

    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow!("HOME environment variable is not set"))?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("codex-chat-bridge")
        .join("conf.toml"))
}

fn ensure_default_config_file(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating config directory {}", parent.display()))?;
    }

    fs::write(path, DEFAULT_CONFIG_TEMPLATE)
        .with_context(|| format!("creating default config file {}", path.display()))?;
    info!("created default config file {}", path.display());
    Ok(())
}

fn resolve_config(args: Args, file_config: Option<FileConfig>) -> ResolvedConfig {
    let file_config = file_config.unwrap_or_default();
    let mut drop_tool_types = file_config.drop_tool_types.unwrap_or_default();
    drop_tool_types.extend(args.drop_tool_types);
    drop_tool_types.retain(|v| !v.trim().is_empty());
    let upstream_wire = args
        .upstream_wire
        .or(file_config.upstream_wire)
        .unwrap_or(WireApi::Chat);

    ResolvedConfig {
        host: args
            .host
            .or(file_config.host)
            .unwrap_or_else(|| "127.0.0.1".to_string()),
        port: args.port.or(file_config.port),
        upstream_url: args
            .upstream_url
            .or(file_config.upstream_url)
            .unwrap_or_else(|| default_upstream_url(upstream_wire)),
        upstream_wire,
        api_key_env: args
            .api_key_env
            .or(file_config.api_key_env)
            .unwrap_or_else(|| "OPENAI_API_KEY".to_string()),
        server_info: args.server_info.or(file_config.server_info),
        http_shutdown: args.http_shutdown || file_config.http_shutdown.unwrap_or(false),
        verbose_logging: args.verbose_logging || file_config.verbose_logging.unwrap_or(false),
        drop_tool_types,
    }
}

fn default_upstream_url(upstream_wire: WireApi) -> String {
    match upstream_wire {
        WireApi::Chat => "https://api.openai.com/v1/chat/completions".to_string(),
        WireApi::Responses => "https://api.openai.com/v1/responses".to_string(),
    }
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

async fn handle_responses(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    handle_incoming(state, headers, body, IncomingApi::Responses).await
}

async fn handle_chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    handle_incoming(state, headers, body, IncomingApi::Chat).await
}

async fn handle_incoming(
    state: Arc<AppState>,
    headers: HeaderMap,
    body: String,
    incoming_api: IncomingApi,
) -> Response {
    if state.verbose_logging {
        info!("incoming request body ({incoming_api:?}): {body}");
    }

    let mut request_value: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(err) => {
            return error_response_for_stream(
                stream_default_for_api(incoming_api),
                "invalid_request",
                &format!("failed to parse request JSON: {err}"),
            )
        }
    };

    let wants_stream = stream_flag_for_request(incoming_api, &request_value);
    apply_request_filters(incoming_api, &mut request_value, &state.drop_tool_types);

    let response_id = format!("resp_bridge_{}", Uuid::now_v7());
    let upstream_payload =
        match build_upstream_payload(&request_value, incoming_api, state.upstream_wire, wants_stream)
        {
            Ok(v) => v,
            Err(err) => return error_response_for_stream(wants_stream, "invalid_request", &err.to_string()),
        };

    if state.verbose_logging {
        if let Some(messages) = upstream_messages_for_logging(state.upstream_wire, &upstream_payload) {
            info!(
                "upstream messages ({:?}->{:?}): {}",
                incoming_api, state.upstream_wire, messages
            );
        }

        info!(
            "upstream headers ({:?}->{:?}): {}",
            incoming_api,
            state.upstream_wire,
            upstream_headers_for_logging(&headers, &state.api_key)
        );

        info!(
            "upstream payload ({:?}->{:?}): {}",
            incoming_api, state.upstream_wire, upstream_payload
        );
    }

    let mut upstream_request = state
        .client
        .post(&state.upstream_url)
        .bearer_auth(&state.api_key)
        .header(CONTENT_TYPE, "application/json")
        .json(&upstream_payload);

    for header_name in FORWARDED_UPSTREAM_HEADERS {
        if let Some(value) = headers.get(header_name) {
            upstream_request = upstream_request.header(header_name, value.clone());
        }
    }

    let upstream_response = match upstream_request.send().await {
        Ok(response) => response,
        Err(err) => {
            return error_response_for_stream(
                wants_stream,
                "upstream_transport_error",
                &format!("failed to call upstream endpoint: {err}"),
            )
        }
    };

    if state.verbose_logging {
        info!(
            "upstream response status: {} {}",
            upstream_response.status().as_u16(),
            upstream_response.status()
        );
    }

    if !upstream_response.status().is_success() {
        let status = upstream_response.status();
        let body = upstream_response
            .text()
            .await
            .unwrap_or_else(|_| "<failed to read error body>".to_string());
        if state.verbose_logging {
            info!("upstream error body: {body}");
        }
        let message = format!("upstream returned {status}: {body}");
        return error_response_for_stream(wants_stream, "upstream_error", &message);
    }

    if wants_stream {
        let body = match state.upstream_wire {
            WireApi::Chat => Body::from_stream(translate_chat_stream(
                upstream_response.bytes_stream(),
                response_id,
            )),
            WireApi::Responses => {
                Body::from_stream(passthrough_responses_stream(upstream_response.bytes_stream()))
            }
        };

        return (
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
            .into_response();
    }

    let upstream_json = match upstream_response.json::<Value>().await {
        Ok(v) => v,
        Err(err) => {
            return json_error_response(
                "upstream_decode_error",
                &format!("failed to decode upstream JSON: {err}"),
            )
        }
    };

    let response_json = match state.upstream_wire {
        WireApi::Chat => chat_json_to_responses_json(upstream_json, response_id),
        WireApi::Responses => upstream_json,
    };

    json_success_response(response_json)
}

fn stream_default_for_api(incoming_api: IncomingApi) -> bool {
    match incoming_api {
        IncomingApi::Responses => true,
        IncomingApi::Chat => false,
    }
}

fn stream_flag_for_request(incoming_api: IncomingApi, request: &Value) -> bool {
    request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| stream_default_for_api(incoming_api))
}

fn apply_request_filters(
    incoming_api: IncomingApi,
    request: &mut Value,
    drop_tool_types: &HashSet<String>,
) {
    let Some(obj) = request.as_object_mut() else {
        return;
    };

    let Some(tools) = obj.get_mut("tools") else {
        return;
    };

    let Some(items) = tools.as_array_mut() else {
        return;
    };

    items.retain(|tool| {
        let tool_type = match incoming_api {
            IncomingApi::Responses => tool.get("type").and_then(Value::as_str),
            IncomingApi::Chat => tool.get("type").and_then(Value::as_str),
        };
        !tool_type.is_some_and(|t| drop_tool_types.contains(t))
    });

    if items.is_empty() {
        obj.remove("tools");
        obj.remove("tool_choice");
    }
}

fn build_upstream_payload(
    request: &Value,
    incoming_api: IncomingApi,
    upstream_wire: WireApi,
    stream: bool,
) -> Result<Value> {
    let mut payload = match (incoming_api, upstream_wire) {
        (IncomingApi::Responses, WireApi::Responses) => request.clone(),
        (IncomingApi::Responses, WireApi::Chat) => {
            map_responses_to_chat_request_with_stream(request, &HashSet::new(), stream)?.chat_request
        }
        (IncomingApi::Chat, WireApi::Chat) => request.clone(),
        (IncomingApi::Chat, WireApi::Responses) => map_chat_to_responses_request(request, stream)?,
    };
    set_stream_flag(&mut payload, stream);
    Ok(payload)
}

fn set_stream_flag(payload: &mut Value, stream: bool) {
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("stream".to_string(), Value::Bool(stream));
    }
}

fn upstream_messages_for_logging(upstream_wire: WireApi, payload: &Value) -> Option<Value> {
    match upstream_wire {
        WireApi::Chat => payload.get("messages").cloned(),
        WireApi::Responses => payload
            .get("input")
            .and_then(Value::as_array)
            .map(|items| {
                let messages = items
                    .iter()
                    .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
                    .cloned()
                    .collect();
                Value::Array(messages)
            }),
    }
}

fn upstream_headers_for_logging(headers: &HeaderMap, api_key: &str) -> Value {
    let mut out = serde_json::Map::new();
    out.insert(
        "authorization".to_string(),
        Value::String(format!("Bearer {}", redact_for_logging(api_key))),
    );
    out.insert(
        CONTENT_TYPE.as_str().to_string(),
        Value::String("application/json".to_string()),
    );

    for header_name in FORWARDED_UPSTREAM_HEADERS {
        if let Some(value) = headers.get(header_name) {
            let header_value = value
                .to_str()
                .map(str::to_string)
                .unwrap_or_else(|_| "<non-utf8>".to_string());
            out.insert(header_name.to_string(), Value::String(header_value));
        }
    }

    Value::Object(out)
}

fn redact_for_logging(secret: &str) -> &'static str {
    if secret.is_empty() {
        "<empty>"
    } else {
        "<redacted>"
    }
}

fn passthrough_responses_stream<S>(
    upstream_stream: S,
) -> impl Stream<Item = Result<Bytes, std::convert::Infallible>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    stream! {
        let mut upstream_stream = Box::pin(upstream_stream);
        while let Some(chunk_result) = upstream_stream.next().await {
            match chunk_result {
                Ok(chunk) => yield Ok(chunk),
                Err(err) => {
                    yield Ok(sse_event(
                        "response.failed",
                        &json!({
                            "type": "response.failed",
                            "response": {
                                "error": {
                                    "code": "upstream_stream_error",
                                    "message": err.to_string(),
                                }
                            }
                        }),
                    ));
                    return;
                }
            }
        }
    }
}

fn translate_chat_stream<S>(
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
        let mut assistant_item_added = false;

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
                                    if !assistant_item_added {
                                        yield Ok(sse_event(
                                            "response.output_item.added",
                                            &json!({
                                                "type": "response.output_item.added",
                                                "item": {
                                                    "type": "message",
                                                    "role": "assistant",
                                                    "content": [
                                                        {
                                                            "type": "output_text",
                                                            "text": "",
                                                        }
                                                    ]
                                                }
                                            }),
                                        ));
                                        assistant_item_added = true;
                                    }
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

fn error_response_for_stream(stream: bool, code: &str, message: &str) -> Response {
    if stream {
        sse_error_response(code, message)
    } else {
        json_error_response(code, message)
    }
}

fn json_success_response(payload: Value) -> Response {
    let body = serde_json::to_vec(&payload).unwrap_or_else(|_| b"{}".to_vec());
    (
        StatusCode::OK,
        [(CONTENT_TYPE, HeaderValue::from_static("application/json"))],
        body,
    )
        .into_response()
}

fn json_error_response(code: &str, message: &str) -> Response {
    json_success_response(json!({
        "error": {
            "type": code,
            "message": message,
        }
    }))
}

fn map_chat_to_responses_request(request: &Value, stream: bool) -> Result<Value> {
    let model = request
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing `model`"))?;
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing `messages` array"))?;

    let mut input = Vec::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");

        if role == "tool" {
            let call_id = message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let output = message
                .get("content")
                .map(function_output_to_text)
                .unwrap_or_default();
            input.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": output,
            }));
            continue;
        }

        let content = chat_message_content_to_input_items(message.get("content"));
        if content.is_empty() {
            continue;
        }

        input.push(json!({
            "type": "message",
            "role": role,
            "content": content,
        }));
    }

    let tools = request
        .get("tools")
        .and_then(Value::as_array)
        .map(|v| normalize_responses_tools(v.clone()))
        .unwrap_or_default();
    let tool_choice = request
        .get("tool_choice")
        .cloned()
        .map(normalize_responses_tool_choice)
        .unwrap_or_else(|| Value::String("auto".to_string()));
    let parallel_tool_calls = request
        .get("parallel_tool_calls")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let mut out = json!({
        "model": model,
        "input": input,
        "stream": stream,
        "tools": tools,
        "tool_choice": tool_choice,
        "parallel_tool_calls": parallel_tool_calls,
    });

    if out
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        if let Some(obj) = out.as_object_mut() {
            obj.remove("tools");
            obj.remove("tool_choice");
        }
    }

    Ok(out)
}

fn normalize_responses_tools(tools: Vec<Value>) -> Vec<Value> {
    tools
        .into_iter()
        .filter_map(|tool| {
            if tool.get("type").and_then(Value::as_str) != Some("function") {
                return Some(tool);
            }

            if tool.get("function").is_none() {
                return Some(tool);
            }

            let function = tool.get("function")?;
            let name = function.get("name").and_then(Value::as_str)?.to_string();
            let description = function
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let parameters = function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type":"object","properties":{}}));

            Some(json!({
                "type": "function",
                "name": name,
                "description": description,
                "parameters": parameters,
            }))
        })
        .collect()
}

fn normalize_responses_tool_choice(choice: Value) -> Value {
    if let Some(obj) = choice.as_object()
        && obj.get("type").and_then(Value::as_str) == Some("function")
        && let Some(name) = obj
            .get("function")
            .and_then(Value::as_object)
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
    {
        return json!({
            "type": "function",
            "name": name,
        });
    }
    choice
}

fn chat_message_content_to_input_items(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(text)) => {
            if text.trim().is_empty() {
                Vec::new()
            } else {
                vec![json!({"type":"input_text","text":text})]
            }
        }
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    return Some(json!({"type":"input_text","text":text}));
                }
                item.get("content")
                    .and_then(Value::as_str)
                    .map(|text| json!({"type":"input_text","text":text}))
            })
            .collect(),
        Some(other) => {
            let as_text = other.to_string();
            if as_text.trim().is_empty() {
                Vec::new()
            } else {
                vec![json!({"type":"input_text","text":as_text})]
            }
        }
        None => Vec::new(),
    }
}

fn chat_json_to_responses_json(chat: Value, response_id: String) -> Value {
    let mut output_items = Vec::new();

    if let Some(choice) = chat
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
    {
        if let Some(message) = choice.get("message") {
            let text = message
                .get("content")
                .map(function_output_to_text)
                .unwrap_or_default();
            if !text.trim().is_empty() {
                output_items.push(json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type":"output_text","text": text}],
                }));
            }

            if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
                for tc in tool_calls {
                    let call_id = tc
                        .get("id")
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                        .unwrap_or_else(|| format!("call_{}", Uuid::now_v7()));
                    let function = tc.get("function").cloned().unwrap_or_else(|| json!({}));
                    let name = function
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown_function");
                    let arguments = function
                        .get("arguments")
                        .map(function_arguments_to_text)
                        .unwrap_or_else(|| "{}".to_string());

                    output_items.push(json!({
                        "type":"function_call",
                        "name": name,
                        "arguments": arguments,
                        "call_id": call_id,
                    }));
                }
            }
        }
    }

    let usage_json = chat.get("usage").map(|usage| {
        json!({
            "input_tokens": usage.get("prompt_tokens").and_then(Value::as_i64).unwrap_or(0),
            "input_tokens_details": null,
            "output_tokens": usage.get("completion_tokens").and_then(Value::as_i64).unwrap_or(0),
            "output_tokens_details": null,
            "total_tokens": usage.get("total_tokens").and_then(Value::as_i64).unwrap_or(0),
        })
    });

    json!({
        "id": response_id,
        "object": "response",
        "status": "completed",
        "output": output_items,
        "usage": usage_json,
    })
}

fn map_responses_to_chat_request(
    request: &Value,
    drop_tool_types: &HashSet<String>,
) -> Result<BridgeRequest> {
    map_responses_to_chat_request_with_stream(request, drop_tool_types, true)
}

fn map_responses_to_chat_request_with_stream(
    request: &Value,
    drop_tool_types: &HashSet<String>,
    stream: bool,
) -> Result<BridgeRequest> {
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
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();

        match item_type {
            "message" => {
                let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                let content = item
                    .get("content")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .map_or_else(String::new, flatten_content_items);

                if !content.trim().is_empty() {
                    messages.push(json!({
                        "role": role,
                        "content": content,
                    }));
                }
            }
            "function_call" => {
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if name.is_empty() {
                    warn!("ignoring function_call item with empty name");
                    continue;
                }

                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .filter(|v| !v.trim().is_empty())
                    .map(ToString::to_string)
                    .unwrap_or_else(|| format!("call_{}", Uuid::now_v7()));
                let arguments = item
                    .get("arguments")
                    .map(function_arguments_to_text)
                    .unwrap_or_else(|| "{}".to_string());

                messages.push(json!({
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": arguments,
                        }
                    }]
                }));
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

    let chat_tools = normalize_chat_tools(tools, drop_tool_types);
    let chat_tool_choice = normalize_tool_choice(tool_choice);

    let mut chat_request = json!({
        "model": model,
        "messages": messages,
        "stream": stream,
        "stream_options": { "include_usage": true },
        "tools": chat_tools,
        "tool_choice": chat_tool_choice,
        "parallel_tool_calls": parallel_tool_calls,
    });

    if !stream
        && let Some(obj) = chat_request.as_object_mut()
    {
        obj.remove("stream_options");
    }

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

    Ok(BridgeRequest { chat_request })
}

fn flatten_content_items(items: &[Value]) -> String {
    let mut parts = Vec::new();
    for item in items {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
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

fn function_arguments_to_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn normalize_chat_tools(tools: Vec<Value>, drop_tool_types: &HashSet<String>) -> Vec<Value> {
    tools
        .into_iter()
        .filter_map(|tool| {
            let tool_type = tool.get("type").and_then(Value::as_str);
            if tool_type.is_some_and(|t| drop_tool_types.contains(t)) {
                return None;
            }

            if tool_type != Some("function") {
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
    use futures::stream;

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

        let req = map_responses_to_chat_request(&input, &HashSet::new()).expect("should map");
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
        assert_eq!(
            tools[0]
                .get("function")
                .and_then(Value::as_object)
                .is_some(),
            true
        );
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
        let out = normalize_chat_tools(tools, &HashSet::new());
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

        let req = map_responses_to_chat_request(&input, &HashSet::new()).expect("should map");
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
    fn map_supports_function_call_to_assistant_tool_call() {
        let input = json!({
            "model": "gpt-4.1",
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "get_weather",
                    "arguments": "{\"city\":\"seoul\"}"
                }
            ],
            "tools": []
        });

        let req = map_responses_to_chat_request(&input, &HashSet::new()).expect("should map");
        let messages = req
            .chat_request
            .get("messages")
            .and_then(Value::as_array)
            .expect("messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(messages[0]["tool_calls"][0]["type"], "function");
        assert_eq!(
            messages[0]["tool_calls"][0]["function"]["name"],
            "get_weather"
        );
    }

    #[test]
    fn map_defaults_tool_choice_when_invalid() {
        let input = json!({
            "model": "gpt-4.1",
            "input": [{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],
            "tools": [{"type":"function","name":"f","parameters":{"type":"object"}}],
            "tool_choice": 123
        });

        let req = map_responses_to_chat_request(&input, &HashSet::new()).expect("should map");
        assert_eq!(req.chat_request["tool_choice"], "auto");
    }

    #[test]
    fn map_requires_input_array() {
        let input = json!({"model":"gpt-4.1"});
        let err = map_responses_to_chat_request(&input, &HashSet::new()).expect_err("must fail");
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
        let req = map_responses_to_chat_request(&input, &HashSet::new()).expect("ok");
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
        let out = normalize_chat_tools(tools.clone(), &HashSet::new());
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
        let req = map_responses_to_chat_request(&input, &HashSet::new()).expect("ok");
        let messages = req.chat_request["messages"].as_array().expect("array");
        assert_eq!(messages[0]["role"], "system");
    }

    #[test]
    fn normalize_chat_tools_drops_configured_tool_types() {
        let tools = vec![
            json!({"type": "web_search_preview"}),
            json!({"type": "function", "name": "f", "parameters": {"type":"object"}}),
        ];
        let mut drop = HashSet::new();
        drop.insert("web_search_preview".to_string());
        let out = normalize_chat_tools(tools, &drop);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "function");
    }

    #[tokio::test]
    async fn stream_emits_output_item_added_before_text_delta() {
        let upstream = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n\
             data: [DONE]\n\n",
        ))]);
        let mut output = Box::pin(translate_chat_stream(upstream, "resp_1".to_string()));
        let mut payload = String::new();

        while let Some(event) = output.next().await {
            payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
        }

        let added_idx = payload
            .find("event: response.output_item.added")
            .expect("added event");
        let delta_idx = payload
            .find("event: response.output_text.delta")
            .expect("delta event");
        assert!(added_idx < delta_idx);
    }

    #[test]
    fn resolve_config_prefers_cli_over_file_and_defaults() {
        let args = Args {
            config: None,
            host: Some("0.0.0.0".to_string()),
            port: Some(9999),
            upstream_url: None,
            upstream_wire: None,
            api_key_env: Some("CLI_API_KEY".to_string()),
            server_info: None,
            http_shutdown: true,
            verbose_logging: false,
            drop_tool_types: vec![],
        };
        let file = FileConfig {
            host: Some("127.0.0.1".to_string()),
            port: Some(8787),
            upstream_url: Some("https://example.com/v1/chat/completions".to_string()),
            upstream_wire: None,
            api_key_env: Some("FILE_API_KEY".to_string()),
            server_info: Some(PathBuf::from("/tmp/server.json")),
            http_shutdown: Some(false),
            verbose_logging: Some(true),
            drop_tool_types: None,
        };

        let resolved = resolve_config(args, Some(file));
        assert_eq!(resolved.host, "0.0.0.0");
        assert_eq!(resolved.port, Some(9999));
        assert_eq!(
            resolved.upstream_url,
            "https://example.com/v1/chat/completions"
        );
        assert_eq!(resolved.api_key_env, "CLI_API_KEY");
        assert_eq!(resolved.server_info, Some(PathBuf::from("/tmp/server.json")));
        assert!(resolved.http_shutdown);
        assert!(resolved.verbose_logging);
    }

    #[test]
    fn resolve_config_uses_defaults_when_missing() {
        let args = Args {
            config: None,
            host: None,
            port: None,
            upstream_url: None,
            upstream_wire: None,
            api_key_env: None,
            server_info: None,
            http_shutdown: false,
            verbose_logging: false,
            drop_tool_types: vec![],
        };

        let resolved = resolve_config(args, None);
        assert_eq!(resolved.host, "127.0.0.1");
        assert_eq!(resolved.port, None);
        assert_eq!(
            resolved.upstream_url,
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(resolved.api_key_env, "OPENAI_API_KEY");
        assert_eq!(resolved.server_info, None);
        assert!(!resolved.http_shutdown);
        assert!(!resolved.verbose_logging);
    }

    #[test]
    fn resolve_config_path_prefers_cli_value() {
        let path = resolve_config_path(Some(PathBuf::from("/tmp/custom.toml"))).expect("ok");
        assert_eq!(path, PathBuf::from("/tmp/custom.toml"));
    }

    #[test]
    fn resolve_config_defaults_upstream_url_by_wire() {
        let args = Args {
            config: None,
            host: None,
            port: None,
            upstream_url: None,
            upstream_wire: Some(WireApi::Responses),
            api_key_env: None,
            server_info: None,
            http_shutdown: false,
            verbose_logging: false,
            drop_tool_types: vec![],
        };

        let resolved = resolve_config(args, None);
        assert_eq!(resolved.upstream_wire, WireApi::Responses);
        assert_eq!(resolved.upstream_url, "https://api.openai.com/v1/responses");
    }

    #[test]
    fn apply_request_filters_drops_tool_type_for_chat_input() {
        let mut request = json!({
            "model": "gpt-4.1",
            "tools": [
                {"type":"web_search_preview"},
                {"type":"function","function":{"name":"f","parameters":{"type":"object"}}}
            ],
            "tool_choice": "auto"
        });
        let mut drop = HashSet::new();
        drop.insert("web_search_preview".to_string());

        apply_request_filters(IncomingApi::Chat, &mut request, &drop);
        let tools = request["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
    }

    #[test]
    fn map_chat_to_responses_request_converts_messages() {
        let chat = json!({
            "model": "gpt-4.1",
            "messages": [
                {"role":"user","content":"hello"}
            ],
            "stream": false
        });

        let out = map_chat_to_responses_request(&chat, false).expect("ok");
        assert_eq!(out["model"], "gpt-4.1");
        assert_eq!(out["stream"], false);
        assert_eq!(out["input"][0]["type"], "message");
        assert_eq!(out["input"][0]["content"][0]["text"], "hello");
    }

    #[test]
    fn upstream_messages_for_logging_reads_chat_messages() {
        let payload = json!({
            "model": "gpt-4.1",
            "messages": [
                {"role":"user","content":"hi"},
                {"role":"assistant","content":"hello"}
            ]
        });

        let out = upstream_messages_for_logging(WireApi::Chat, &payload).expect("messages");
        let messages = out.as_array().expect("array");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "user");
    }

    #[test]
    fn upstream_messages_for_logging_filters_responses_input_items() {
        let payload = json!({
            "model": "gpt-4.1",
            "input": [
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
                {"type":"function_call_output","call_id":"call_1","output":"ok"}
            ]
        });

        let out = upstream_messages_for_logging(WireApi::Responses, &payload).expect("messages");
        let messages = out.as_array().expect("array");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["type"], "message");
        assert_eq!(messages[0]["role"], "user");
    }

    #[test]
    fn upstream_headers_for_logging_includes_forwarded_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("openai-organization", HeaderValue::from_static("org_123"));
        headers.insert("x-openai-subagent", HeaderValue::from_static("subagent_1"));

        let out = upstream_headers_for_logging(&headers, "sk-test");
        assert_eq!(out["authorization"], "Bearer <redacted>");
        assert_eq!(out["content-type"], "application/json");
        assert_eq!(out["openai-organization"], "org_123");
        assert_eq!(out["x-openai-subagent"], "subagent_1");
    }

    #[test]
    fn upstream_headers_for_logging_marks_empty_api_key() {
        let headers = HeaderMap::new();

        let out = upstream_headers_for_logging(&headers, "");
        assert_eq!(out["authorization"], "Bearer <empty>");
        assert_eq!(out["content-type"], "application/json");
    }
}
