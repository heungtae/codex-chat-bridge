use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use axum::Router;
use axum::body::Body;
use axum::http::HeaderMap;
use axum::http::HeaderName;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header::CACHE_CONTROL;
use axum::http::header::CONTENT_TYPE;
use axum::http::header::HOST;
use axum::response::IntoResponse;
use axum::response::Response;
use clap::Parser;
use reqwest::Client;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::fs::File;
use std::fs::{self};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::debug;
use tracing::info;
use tracing::warn;
use uuid::Uuid;

mod bridge;
mod bridge_types;
mod config;
mod http_handlers;
mod logging_utils;
mod model;
mod pipeline;
mod response_utils;
mod routing;
mod session;
mod state;
use bridge::mapping::*;
use bridge::streaming::*;
use bridge_types::*;
use config::*;
use http_handlers::build_app;
use logging_utils::*;
use model::*;
use pipeline::*;
use response_utils::*;
use routing::*;
use session::*;
use state::AppState;
use state::UpstreamSuccessLogSnapshot;

#[derive(Serialize)]
struct ServerInfo {
    port: u16,
    ports: Vec<u16>,
    pid: u32,
}

fn init_tracing(verbose_logging: bool) {
    let default_filter = if verbose_logging { "debug" } else { "info" };
    let mut env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter));
    let crate_directive = if verbose_logging {
        "codex_chat_bridge=debug"
    } else {
        "codex_chat_bridge=info"
    };
    if let Ok(directive) = crate_directive.parse() {
        env_filter = env_filter.add_directive(directive);
    }

    tracing_subscriber::fmt().with_env_filter(env_filter).init();
}

fn load_runtime_config(args: &Args) -> Result<(ResolvedConfig, BTreeMap<String, RouterConfig>)> {
    let config_path = resolve_config_path(args.config.clone())?;
    ensure_default_config_file(&config_path)?;
    let file_config = load_file_config(&config_path)?;
    let config = resolve_config(args.clone(), file_config.clone())?;
    let routers = file_config
        .as_ref()
        .and_then(|fc| fc.routers.clone())
        .unwrap_or_default();
    Ok((config, routers))
}

fn handle_list_routers(args: &Args, routers: &BTreeMap<String, RouterConfig>) -> bool {
    if !args.list_routers {
        return false;
    }
    if routers.is_empty() {
        println!("No routers defined in config file.");
    } else {
        println!("Available routers:");
        for name in routers.keys() {
            println!("  - {}", name);
        }
    }
    true
}

fn build_router_manager(
    config: &ResolvedConfig,
    routers: BTreeMap<String, RouterConfig>,
) -> Result<RouterManager> {
    RouterManager::new(
        routers,
        config.upstream_url.clone(),
        config.upstream_wire,
        config.upstream_http_headers.clone(),
        config.forward_incoming_headers.clone(),
        config.drop_tool_types.clone(),
        config.drop_request_fields.clone(),
        config.feature_flags,
    )
}

fn log_runtime_startup(
    config: &ResolvedConfig,
    router_manager: &RouterManager,
    listen_addrs: &[String],
) {
    let router_defaults = router_manager.get_default_log_snapshot();
    info!(
        "startup: listen_addrs={:?} router_count={}",
        listen_addrs,
        router_manager.get_router_names().len()
    );
    info!(
        "router defaults: upstream_url={}, upstream_wire={:?}, upstream_http_headers={:?}, forward_incoming_headers={:?}, drop_tool_types={:?}, drop_request_fields={:?}",
        router_defaults.upstream_url,
        router_defaults.upstream_wire,
        router_defaults.upstream_http_headers,
        router_defaults.forward_incoming_headers,
        router_defaults.drop_tool_types,
        router_defaults.drop_request_fields
    );
    info!(
        "runtime config: api_key_env={}, server_info={:?}, http_shutdown={}, verbose_logging={}, feature_flags={:?}",
        config.api_key_env,
        config.server_info,
        config.http_shutdown,
        config.verbose_logging,
        config.feature_flags
    );
    for snapshot in router_manager.get_router_delta_log_snapshots() {
        let RouterDeltaLogSnapshot {
            name,
            active,
            incoming_url,
            non_local_incoming_host,
            upstream_wire,
            override_upstream_url,
            override_upstream_wire,
            override_upstream_model,
            override_upstream_model_opus,
            override_upstream_model_sonnet,
            override_upstream_model_haiku,
            override_upstream_http_headers,
            override_forward_incoming_headers,
            override_drop_tool_types,
            override_drop_request_fields,
            override_anthropic_preserve_thinking,
            override_anthropic_enable_openrouter_reasoning,
        } = snapshot;

        let mut overrides = Vec::new();
        if let Some(v) = override_upstream_url {
            overrides.push(format!("upstream_url={v}"));
        }
        if let Some(v) = override_upstream_wire {
            overrides.push(format!("upstream_wire={v:?}"));
        }
        if let Some(v) = override_upstream_model {
            overrides.push(format!("upstream_model={v}"));
        }
        if let Some(v) = override_upstream_model_opus {
            overrides.push(format!("upstream_model_opus={v}"));
        }
        if let Some(v) = override_upstream_model_sonnet {
            overrides.push(format!("upstream_model_sonnet={v}"));
        }
        if let Some(v) = override_upstream_model_haiku {
            overrides.push(format!("upstream_model_haiku={v}"));
        }
        if let Some(v) = override_upstream_http_headers {
            overrides.push(format!("upstream_http_headers={v:?}"));
        }
        if let Some(v) = override_forward_incoming_headers {
            overrides.push(format!("forward_incoming_headers={v:?}"));
        }
        if let Some(v) = override_drop_tool_types {
            overrides.push(format!("drop_tool_types={v:?}"));
        }
        if let Some(v) = override_drop_request_fields {
            overrides.push(format!("drop_request_fields={v:?}"));
        }
        if override_anthropic_preserve_thinking == Some(true) {
            overrides.push("anthropic_preserve_thinking=true".to_string());
        }
        if override_anthropic_enable_openrouter_reasoning == Some(true) {
            overrides.push("anthropic_enable_openrouter_reasoning=true".to_string());
        }
        let override_summary = if overrides.is_empty() {
            "none".to_string()
        } else {
            overrides.join(", ")
        };

        info!(
            "router: name={}, active={}, incoming_url={:?}, upstream_wire={:?}, overrides={}",
            name, active, incoming_url, upstream_wire, override_summary
        );
        if let Some(host) = non_local_incoming_host {
            warn!(
                "router `{}` incoming_url host `{}` is not loopback/local. codex-chat-bridge is typically intended for local-only use; consider binding to localhost/127.0.0.1 or adding network controls.",
                name, host
            );
        }
    }
}

async fn run_server(
    app: Router,
    listen_addrs: &[String],
    server_info: Option<&Path>,
) -> Result<()> {
    let mut bound_ports = Vec::new();
    let mut join_set = tokio::task::JoinSet::new();
    for bind_addr in listen_addrs {
        let listener = tokio::net::TcpListener::bind(bind_addr)
            .await
            .with_context(|| format!("binding {bind_addr}"))?;
        let local_addr = listener.local_addr().context("reading local_addr")?;
        bound_ports.push(local_addr.port());
        info!("codex-chat-bridge listening on {}", local_addr);

        let app_clone = app.clone();
        let local_addr_for_task = local_addr;
        join_set.spawn(async move {
            axum::serve(listener, app_clone)
                .await
                .with_context(|| format!("serving axum app on {}", local_addr_for_task))
        });
    }

    if let Some(path) = server_info {
        write_server_info(path, &bound_ports)?;
    }

    match join_set.join_next().await {
        Some(result) => {
            result.context("listener task failed to join")??;
            Err(anyhow!("listener exited unexpectedly"))
        }
        None => Err(anyhow!("no listener task started")),
    }
}

pub async fn run() -> Result<()> {
    let args = Args::parse();
    let (config, routers) = load_runtime_config(&args)?;
    init_tracing(config.verbose_logging);

    if handle_list_routers(&args, &routers) {
        return Ok(());
    }

    let api_key = std::env::var(&config.api_key_env)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| anyhow!("missing or empty env var: {}", config.api_key_env))?;

    let client = Client::builder()
        .build()
        .context("building reqwest client")?;

    let router_manager = build_router_manager(&config, routers)?;
    let listen_addrs = router_manager.get_listen_addrs();
    if listen_addrs.is_empty() {
        return Err(anyhow!(
            "no listenable incoming_url found. configure at least one absolute URL like `http://<host>:<port>/<path>` in [routers.*].incoming_url"
        ));
    }
    log_runtime_startup(&config, &router_manager, &listen_addrs);

    let state = Arc::new(AppState {
        client,
        api_key,
        http_shutdown: config.http_shutdown,
        verbose_logging: config.verbose_logging,
        routers: Arc::new(RwLock::new(router_manager)),
        sessions: Arc::new(RwLock::new(SessionStore::default())),
        last_successful_upstream_log: Arc::new(RwLock::new(None)),
    });

    let app = build_app(state.clone());
    run_server(app, &listen_addrs, config.server_info.as_deref()).await
}

fn write_server_info(path: &Path, ports: &[u16]) -> Result<()> {
    if ports.is_empty() {
        return Err(anyhow!("no listening ports available"));
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let unique_ports = ports.iter().copied().collect::<BTreeSet<_>>();
    let ordered_ports = unique_ports.into_iter().collect::<Vec<_>>();
    let info = ServerInfo {
        port: ordered_ports[0],
        ports: ordered_ports,
        pid: std::process::id(),
    };
    let mut data = serde_json::to_string(&info)?;
    data.push('\n');
    let mut f = File::create(path)?;
    f.write_all(data.as_bytes())?;
    Ok(())
}

fn resolve_incoming_route(
    incoming_api_hint: Option<IncomingApi>,
    incoming_path: Option<&str>,
) -> String {
    incoming_path
        .map(str::to_string)
        .unwrap_or_else(|| match incoming_api_hint {
            Some(IncomingApi::Chat) => "/v1/chat/completions".to_string(),
            Some(IncomingApi::Responses) => "/v1/responses".to_string(),
            Some(IncomingApi::Anthropic) => "/v1/messages".to_string(),
            None => "/v1/responses".to_string(),
        })
}

fn infer_incoming_api_from_hint_or_path(
    incoming_api_hint: Option<IncomingApi>,
    incoming_path: Option<&str>,
    request_value: &Value,
) -> IncomingApi {
    if let Some(api) = incoming_api_hint {
        return api;
    }

    if let Some(path) = incoming_path {
        let normalized_path = normalize_request_path(path);
        if normalized_path.ends_with("/v1/messages/count_tokens") {
            return IncomingApi::Anthropic;
        }
        if normalized_path.ends_with("/v1/messages") {
            return IncomingApi::Anthropic;
        }
        if normalized_path.ends_with("/v1/chat/completions") {
            return IncomingApi::Chat;
        }
        if normalized_path.ends_with("/v1/responses") {
            return IncomingApi::Responses;
        }
    }

    infer_incoming_api(request_value)
}

fn select_upstream_model_override<'a>(
    requested_model: Option<&str>,
    route_target: &'a RouteTarget,
) -> Option<&'a str> {
    let requested_model = requested_model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or_default()
        .to_ascii_lowercase();

    let family_override = if requested_model.contains("opus") {
        route_target.upstream_model_opus.as_deref()
    } else if requested_model.contains("sonnet") {
        route_target.upstream_model_sonnet.as_deref()
    } else if requested_model.contains("haiku") {
        route_target.upstream_model_haiku.as_deref()
    } else {
        None
    };

    family_override.or(route_target.upstream_model.as_deref())
}

fn apply_upstream_model_override(payload: &mut Value, route_target: &RouteTarget) {
    let requested_model = payload.get("model").and_then(Value::as_str);
    let Some(model) = select_upstream_model_override(requested_model, route_target) else {
        return;
    };
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("model".to_string(), Value::String(model.to_string()));
    }
}

fn inject_openrouter_reasoning(payload: &mut Value) {
    let Some(obj) = payload.as_object_mut() else {
        return;
    };

    match obj.get_mut("reasoning") {
        Some(Value::Object(reasoning)) => {
            reasoning
                .entry("enabled".to_string())
                .or_insert(Value::Bool(true));
        }
        Some(_) => {}
        None => {
            obj.insert(
                "reasoning".to_string(),
                serde_json::json!({"enabled": true}),
            );
        }
    }
}

fn strip_anthropic_reasoning_fields(payload: &mut Value) {
    let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };

    for message in messages {
        let Some(obj) = message.as_object_mut() else {
            continue;
        };
        obj.remove("reasoning_content");
        obj.remove("thinking");
    }
}

fn anthropic_request_enables_thinking(request: &Value) -> bool {
    match request.get("thinking") {
        Some(Value::Bool(enabled)) => *enabled,
        Some(Value::String(mode)) => {
            mode.eq_ignore_ascii_case("enabled") || mode.eq_ignore_ascii_case("adaptive")
        }
        Some(Value::Object(thinking)) => match thinking.get("type").and_then(Value::as_str) {
            Some(kind) if kind.eq_ignore_ascii_case("disabled") => false,
            Some(kind)
                if kind.eq_ignore_ascii_case("enabled")
                    || kind.eq_ignore_ascii_case("adaptive") =>
            {
                true
            }
            Some(_) => false,
            None => thinking
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        _ => false,
    }
}

fn is_anthropic_count_tokens_path(incoming_path: Option<&str>) -> bool {
    incoming_path
        .map(normalize_request_path)
        .is_some_and(|path| path.ends_with("/v1/messages/count_tokens"))
}

fn estimate_anthropic_count_tokens(request: &Value) -> i64 {
    let mut total = 0usize;

    total += count_anthropic_token_chars(request.get("system"));

    if let Some(messages) = request.get("messages").and_then(Value::as_array) {
        for message in messages {
            total += count_anthropic_message_token_chars(message);
        }
    }

    if let Some(tools) = request.get("tools").and_then(Value::as_array) {
        for tool in tools {
            total += count_anthropic_token_chars(tool.get("name"));
            total += count_anthropic_token_chars(tool.get("description"));
            total += count_anthropic_token_chars(tool.get("input_schema"));
        }
    }

    let tokens = total / 4;
    if tokens == 0 && request.get("messages").is_some() {
        1
    } else {
        tokens as i64
    }
}

fn count_anthropic_message_token_chars(message: &Value) -> usize {
    let mut total = 0usize;
    total += count_anthropic_token_chars(message.get("role"));
    total += count_anthropic_visible_content_chars(message.get("content"));
    total += count_anthropic_token_chars(message.get("name"));
    total += count_anthropic_token_chars(message.get("tool_use_id"));
    total += count_anthropic_token_chars(message.get("tool_call_id"));

    total
}

fn count_anthropic_visible_content_chars(value: Option<&Value>) -> usize {
    let Some(value) = value else {
        return 0;
    };

    match value {
        Value::Array(items) => items.iter().map(count_anthropic_content_block_chars).sum(),
        Value::Object(_) => count_anthropic_content_block_chars(value),
        _ => count_anthropic_token_chars(Some(value)),
    }
}

fn count_anthropic_content_block_chars(block: &Value) -> usize {
    let Some(obj) = block.as_object() else {
        return count_anthropic_token_chars(Some(block));
    };

    let block_type = obj
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();

    if matches!(
        block_type.as_str(),
        "thinking" | "redacted_thinking" | "signature"
    ) {
        return 0;
    }

    let mut total = 0usize;
    if !block_type.is_empty() {
        total += block_type.len();
    }

    total += count_anthropic_token_chars(obj.get("text"));
    total += count_anthropic_token_chars(obj.get("content"));
    total += count_anthropic_token_chars(obj.get("input"));
    total += count_anthropic_token_chars(obj.get("name"));
    total += count_anthropic_token_chars(obj.get("id"));
    total += count_anthropic_token_chars(obj.get("tool_use_id"));
    total += count_anthropic_token_chars(obj.get("data"));

    total
}

fn count_anthropic_token_chars(value: Option<&Value>) -> usize {
    let Some(value) = value else {
        return 0;
    };

    match value {
        Value::Null => 0,
        Value::Bool(v) => {
            if *v {
                4
            } else {
                5
            }
        }
        Value::Number(n) => n.to_string().len(),
        Value::String(s) => s.len(),
        Value::Array(items) => items.iter().map(count_anthropic_content_block_chars).sum(),
        Value::Object(_) => serde_json::to_string(value).map(|s| s.len()).unwrap_or(0),
    }
}

async fn resolve_route_target(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    incoming_route: &str,
    incoming_path: Option<&str>,
) -> std::result::Result<RouteTarget, Response> {
    let routers = state.routers.read().await;
    let host_header = headers.get(HOST).and_then(|h| h.to_str().ok());
    let route_target = match incoming_path {
        Some(path) => match routers.get_target_for_incoming_route(path, host_header) {
            Ok(Some(target)) => target,
            Ok(None) => {
                warn!(
                    "router miss: host={:?}, incoming_route={}, router_names={:?}",
                    host_header,
                    incoming_route,
                    routers.get_router_names()
                );
                return Err((StatusCode::NOT_FOUND, "not found").into_response());
            }
            Err(err) => {
                warn!(
                    "router resolution failed: host={:?}, incoming_route={}, error={}",
                    host_header, incoming_route, err
                );
                return Err(json_error_response("router_error", &err.to_string()));
            }
        },
        None => {
            warn!(
                "router resolution failed: host={:?}, incoming_route={}, error=missing incoming path",
                host_header, incoming_route
            );
            return Err(json_error_response(
                "router_error",
                "routing requires POST /{incoming_path} that matches routers.*.incoming_url",
            ));
        }
    };
    Ok(route_target)
}

fn parse_and_prepare_request(
    body: &str,
    incoming_api_hint: Option<IncomingApi>,
    incoming_path: Option<&str>,
    route_target: &RouteTarget,
    verbose_logging: bool,
) -> std::result::Result<
    (
        IncomingApi,
        bool,
        Value,
        HashMap<String, ResponsesToolCallKind>,
    ),
    Response,
> {
    let fallback_incoming_api = incoming_api_hint.unwrap_or(IncomingApi::Responses);
    let mut request_value: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(err) => {
            warn!(
                "request parse failed: router={}, incoming_path={:?}, error={}",
                route_target.router_name, incoming_path, err
            );
            return Err(error_response_for_api(
                fallback_incoming_api,
                stream_default_for_api(fallback_incoming_api),
                "invalid_request",
                &format!("failed to parse request JSON: {err}"),
            ));
        }
    };

    if verbose_logging {
        debug!(
            "incoming tool types (router={}): {}",
            route_target.router_name,
            tool_types_for_logging(&request_value)
        );
        debug!(
            "incoming request fields (router={}): {}",
            route_target.router_name,
            request_fields_for_logging(&request_value)
        );
    }

    let incoming_api =
        infer_incoming_api_from_hint_or_path(incoming_api_hint, incoming_path, &request_value);
    let wants_stream = stream_flag_for_request(incoming_api, &request_value);
    apply_request_filters(
        incoming_api,
        &mut request_value,
        &route_target.drop_tool_types,
        &route_target.drop_request_fields,
    );
    if let Err(err) = validate_capability_gate(
        incoming_api,
        route_target.upstream_wire,
        route_target.feature_flags.enable_extended_input_types,
        &request_value,
    ) {
        warn!(
            "request capability gate rejected: router={}, incoming_api={:?}, upstream_wire={:?}, error={}",
            route_target.router_name, incoming_api, route_target.upstream_wire, err
        );
        return Err(error_response_for_api(
            incoming_api,
            wants_stream,
            "unsupported_feature",
            &err.to_string(),
        ));
    }
    let tool_call_kinds_by_name = if incoming_api == IncomingApi::Responses {
        responses_tool_call_kind_by_name(&request_value)
    } else {
        HashMap::new()
    };

    Ok((
        incoming_api,
        wants_stream,
        request_value,
        tool_call_kinds_by_name,
    ))
}

async fn build_upstream_payload_with_session(
    state: &Arc<AppState>,
    request_value: &Value,
    incoming_api: IncomingApi,
    route_target: &RouteTarget,
    wants_stream: bool,
) -> std::result::Result<(String, Value), Response> {
    let response_id = format!("resp_bridge_{}", Uuid::now_v7());
    let mut upstream_payload = match build_upstream_payload(
        request_value,
        incoming_api,
        route_target.upstream_wire,
        wants_stream,
        route_target.feature_flags.enable_extended_input_types,
        route_target.feature_flags.tool_transform_mode,
        route_target.anthropic_preserve_thinking,
    ) {
        Ok(v) => v,
        Err(err) => {
            warn!(
                "request mapping failed: router={}, incoming_api={:?}, upstream_wire={:?}, error={}",
                route_target.router_name, incoming_api, route_target.upstream_wire, err
            );
            return Err(error_response_for_api(
                incoming_api,
                wants_stream,
                "invalid_request",
                &err.to_string(),
            ));
        }
    };
    apply_upstream_model_override(&mut upstream_payload, route_target);
    if incoming_api == IncomingApi::Anthropic {
        strip_anthropic_reasoning_fields(&mut upstream_payload);
    }
    if route_target.anthropic_enable_openrouter_reasoning
        && anthropic_request_enables_thinking(request_value)
    {
        inject_openrouter_reasoning(&mut upstream_payload);
    }

    let should_store_previous_response_messages =
        route_target.feature_flags.enable_previous_response_id
            && incoming_api == IncomingApi::Responses
            && route_target.upstream_wire == WireApi::Chat;
    if should_store_previous_response_messages {
        let previous_messages = {
            let sessions = state.sessions.read().await;
            match resolve_previous_messages_for_request(request_value, &sessions) {
                Ok(v) => v,
                Err(err) => {
                    warn!(
                        "previous response lookup failed: router={}, response_id={}, error={}",
                        route_target.router_name, response_id, err
                    );
                    return Err(error_response_for_api(
                        incoming_api,
                        wants_stream,
                        "invalid_request",
                        &err.to_string(),
                    ));
                }
            }
        };
        if let Some(messages) = previous_messages
            && let Err(err) = merge_previous_messages(&mut upstream_payload, messages)
        {
            warn!(
                "previous response merge failed: router={}, response_id={}, error={}",
                route_target.router_name, response_id, err
            );
            return Err(error_response_for_api(
                incoming_api,
                wants_stream,
                "invalid_request",
                &err.to_string(),
            ));
        }
    }

    if route_target.upstream_wire == WireApi::Chat {
        normalize_unsupported_chat_message_roles(&mut upstream_payload);
    }

    if should_store_previous_response_messages {
        let chat_messages = upstream_payload
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut sessions = state.sessions.write().await;
        sessions.insert_messages(response_id.clone(), chat_messages);
    }

    Ok((response_id, upstream_payload))
}

fn normalize_unsupported_chat_message_roles(payload: &mut Value) {
    let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };

    let original_messages = std::mem::take(messages);
    let mut system_contents = Vec::new();
    let mut normalized_messages = Vec::new();

    for mut message in original_messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        match role.as_str() {
            "developer" | "system" => {
                if let Some(content) = chat_message_content_to_system_text(&message) {
                    system_contents.push(content);
                }
            }
            "tool" | "function" => {
                let Some(obj) = message.as_object_mut() else {
                    normalized_messages.push(message);
                    continue;
                };
                obj.insert("role".to_string(), Value::String("user".to_string()));
                obj.remove("tool_call_id");
                obj.remove("name");
                normalized_messages.push(message);
            }
            _ => normalized_messages.push(message),
        }
    }

    if !system_contents.is_empty() {
        messages.push(serde_json::json!({
            "role": "system",
            "content": system_contents.join("\n\n"),
        }));
    }
    messages.extend(normalized_messages);
}

fn chat_message_content_to_system_text(message: &Value) -> Option<String> {
    let content = message.get("content")?;
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Null => return None,
        other => other.to_string(),
    };
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

fn build_upstream_request(
    state: &Arc<AppState>,
    route_target: &RouteTarget,
    headers: &HeaderMap,
    upstream_payload: &Value,
) -> reqwest::RequestBuilder {
    let mut merged_headers = HeaderMap::new();
    for header_name in &route_target.forward_incoming_headers {
        if let Some(value) = headers.get(header_name) {
            if let Ok(name) = HeaderName::from_bytes(header_name.as_bytes()) {
                merged_headers.insert(name, value.clone());
            }
        }
    }
    for header in &route_target.upstream_http_headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(header.name.as_bytes()),
            HeaderValue::from_str(&header.value),
        ) {
            merged_headers.insert(name, value);
        }
    }

    let mut upstream_request = state
        .client
        .post(&route_target.upstream_url)
        .bearer_auth(&state.api_key)
        .header(CONTENT_TYPE, "application/json")
        .json(upstream_payload);

    if !merged_headers.is_empty() {
        upstream_request = upstream_request.headers(merged_headers);
    }
    upstream_request
}

async fn finalize_upstream_response(
    state: &Arc<AppState>,
    upstream_response: reqwest::Response,
    route_target: &RouteTarget,
    incoming_api: IncomingApi,
    wants_stream: bool,
    upstream_model: String,
    anthropic_input_tokens: i64,
    response_id: String,
    tool_call_kinds_by_name: HashMap<String, ResponsesToolCallKind>,
    verbose_logging: bool,
) -> Response {
    if !upstream_response.status().is_success() {
        if let Some(snapshot) = state.last_successful_upstream_log.read().await.clone() {
            warn!(
                "last successful upstream response status: router={}, incoming_api={:?}, upstream_wire={:?}, status={}",
                snapshot.router_name,
                snapshot.incoming_api,
                snapshot.upstream_wire,
                snapshot.status
            );
            warn!(
                "last successful upstream response headers: router={}, incoming_api={:?}, upstream_wire={:?}, headers={}",
                snapshot.router_name,
                snapshot.incoming_api,
                snapshot.upstream_wire,
                snapshot.headers
            );
        }
        let status = upstream_response.status();
        let headers = headers_for_logging(upstream_response.headers());
        let body = upstream_response
            .text()
            .await
            .unwrap_or_else(|_| "<failed to read error body>".to_string());
        warn!(
            "upstream error response headers: router={}, incoming_api={:?}, upstream_wire={:?}, headers={}",
            route_target.router_name, incoming_api, route_target.upstream_wire, headers
        );
        warn_large_log(
            &format!(
                "upstream error response body: router={}, incoming_api={:?}, upstream_wire={:?}, status={}",
                route_target.router_name, incoming_api, route_target.upstream_wire, status
            ),
            &body,
        );
        if verbose_logging {
            debug_large_log(
                &format!(
                    "upstream response payload error (router={}, {:?}<-{:?})",
                    route_target.router_name, incoming_api, route_target.upstream_wire
                ),
                &body,
            );
        }
        let normalized = normalize_upstream_error_payload(status, &body);
        warn!(
            "upstream error: router={}, incoming_api={:?}, upstream_wire={:?}, status={}, code={}, message={}",
            route_target.router_name,
            incoming_api,
            route_target.upstream_wire,
            status,
            normalized.code,
            normalized.message
        );
        return error_response_for_api(
            incoming_api,
            wants_stream,
            &normalized.code,
            &normalized.message,
        );
    }

    if wants_stream {
        if incoming_api == IncomingApi::Anthropic
            && route_target.upstream_wire == WireApi::Responses
        {
            return error_response_for_api(
                incoming_api,
                true,
                "unsupported_feature",
                "anthropic `/v1/messages` streaming currently requires `upstream_wire = \"chat\"`",
            );
        }
        let body = match route_target.upstream_wire {
            WireApi::Chat => {
                if incoming_api == IncomingApi::Anthropic {
                    Body::from_stream(translate_chat_stream_to_anthropic(
                        upstream_response.bytes_stream(),
                        route_target.router_name.clone(),
                        verbose_logging,
                        upstream_model.clone(),
                        anthropic_input_tokens,
                    ))
                } else {
                    Body::from_stream(translate_chat_stream(
                        upstream_response.bytes_stream(),
                        response_id,
                        route_target.router_name.clone(),
                        verbose_logging,
                        tool_call_kinds_by_name,
                        route_target.feature_flags,
                    ))
                }
            }
            WireApi::Responses => Body::from_stream(passthrough_responses_stream(
                upstream_response.bytes_stream(),
                route_target.router_name.clone(),
                verbose_logging,
            )),
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
            return error_response_for_api(
                incoming_api,
                false,
                "upstream_decode_error",
                &format!("failed to decode upstream JSON: {err}"),
            );
        }
    };
    if verbose_logging {
        debug_large_log(
            &format!(
                "upstream response payload (router={}, {:?}<-{:?})",
                route_target.router_name, incoming_api, route_target.upstream_wire
            ),
            &upstream_json.to_string(),
        );
    }

    let response_json = match route_target.upstream_wire {
        WireApi::Chat => {
            if incoming_api == IncomingApi::Anthropic {
                chat_json_to_anthropic_json(upstream_json, &upstream_model)
            } else {
                chat_json_to_responses_json(
                    upstream_json,
                    response_id,
                    &tool_call_kinds_by_name,
                    route_target.feature_flags.enable_provider_specific_fields,
                )
            }
        }
        WireApi::Responses => {
            if incoming_api == IncomingApi::Anthropic {
                responses_json_to_anthropic_json(upstream_json, &upstream_model)
            } else {
                upstream_json
            }
        }
    };

    json_success_response(response_json)
}

fn upstream_payload_model(payload: &Value) -> String {
    payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("claude-bridge")
        .to_string()
}

pub(crate) async fn handle_incoming(
    state: Arc<AppState>,
    headers: HeaderMap,
    body: String,
    incoming_api_hint: Option<IncomingApi>,
    incoming_path: Option<String>,
) -> Response {
    let verbose_logging = state.verbose_logging;
    let incoming_route = resolve_incoming_route(incoming_api_hint, incoming_path.as_deref());
    let host_header = headers.get(HOST).and_then(|h| h.to_str().ok());

    if verbose_logging {
        debug!(
            "incoming request received: hinted_api={:?}, incoming_route={}, host={:?}, body_bytes={}",
            incoming_api_hint,
            incoming_route,
            host_header,
            body.len()
        );
    }

    let route_target =
        match resolve_route_target(&state, &headers, &incoming_route, incoming_path.as_deref())
            .await
        {
            Ok(target) => target,
            Err(response) => return response,
        };

    debug!(
        "request routed: router={}, incoming_route={}, upstream_url={}, upstream_wire={:?}",
        route_target.router_name,
        incoming_route,
        route_target.upstream_url,
        route_target.upstream_wire
    );

    if verbose_logging {
        debug!(
            "incoming headers (router={}): {}",
            route_target.router_name,
            headers_for_logging(&headers)
        );
        debug_large_log(
            &format!(
                "incoming request body (router={})",
                route_target.router_name
            ),
            &body,
        );
    }

    let (incoming_api, wants_stream, request_value, tool_call_kinds_by_name) =
        match parse_and_prepare_request(
            &body,
            incoming_api_hint,
            incoming_path.as_deref(),
            &route_target,
            verbose_logging,
        ) {
            Ok(v) => v,
            Err(response) => return response,
        };

    if incoming_api == IncomingApi::Anthropic
        && is_anthropic_count_tokens_path(incoming_path.as_deref())
    {
        let input_tokens = estimate_anthropic_count_tokens(&request_value);
        if verbose_logging {
            debug!(
                "anthropic count_tokens handled locally (router={}, estimated_input_tokens={})",
                route_target.router_name, input_tokens
            );
        }
        return json_success_response(serde_json::json!({
            "input_tokens": input_tokens,
        }));
    }

    let (response_id, upstream_payload) = match build_upstream_payload_with_session(
        &state,
        &request_value,
        incoming_api,
        &route_target,
        wants_stream,
    )
    .await
    {
        Ok(v) => v,
        Err(response) => return response,
    };

    if verbose_logging {
        if let Some(messages) =
            upstream_messages_for_logging(route_target.upstream_wire, &upstream_payload)
        {
            debug_large_log(
                &format!(
                    "upstream messages (router={}, {:?}->{:?})",
                    route_target.router_name, incoming_api, route_target.upstream_wire
                ),
                &messages.to_string(),
            );
        }

        debug!(
            "upstream headers (router={}, {:?}->{:?}): {}",
            route_target.router_name,
            incoming_api,
            route_target.upstream_wire,
            upstream_headers_for_logging(
                &headers,
                &state.api_key,
                &route_target.upstream_http_headers,
                &route_target.forward_incoming_headers,
            )
        );

        debug_large_log(
            &format!(
                "upstream payload (router={}, {:?}->{:?})",
                route_target.router_name, incoming_api, route_target.upstream_wire
            ),
            &upstream_payload.to_string(),
        );
        debug!(
            "upstream tool types (router={}, {:?}->{:?}): {}",
            route_target.router_name,
            incoming_api,
            route_target.upstream_wire,
            tool_types_for_logging(&upstream_payload)
        );
        debug!(
            "upstream tool definitions (router={}, {:?}->{:?}): {}",
            route_target.router_name,
            incoming_api,
            route_target.upstream_wire,
            tool_definitions_for_logging(&upstream_payload)
        );
        debug!(
            "upstream request fields (router={}, {:?}->{:?}): {}",
            route_target.router_name,
            incoming_api,
            route_target.upstream_wire,
            request_fields_for_logging(&upstream_payload)
        );
    }

    let upstream_request =
        build_upstream_request(&state, &route_target, &headers, &upstream_payload);
    let upstream_model = upstream_payload_model(&upstream_payload);
    let anthropic_input_tokens = if incoming_api == IncomingApi::Anthropic {
        estimate_anthropic_count_tokens(&request_value)
    } else {
        0
    };
    let upstream_response = match upstream_request.send().await {
        Ok(response) => response,
        Err(err) => {
            warn!(
                "upstream transport failed: router={}, incoming_route={}, upstream_url={}, error={}",
                route_target.router_name, incoming_route, route_target.upstream_url, err
            );
            return error_response_for_api(
                incoming_api,
                wants_stream,
                "upstream_transport_error",
                &format!("failed to call upstream endpoint: {err}"),
            );
        }
    };

    if verbose_logging {
        debug!(
            "upstream response status (router={}): {} {}",
            route_target.router_name,
            upstream_response.status().as_u16(),
            upstream_response.status()
        );
        debug!(
            "upstream response headers (router={}, {:?}<-{:?}): {}",
            route_target.router_name,
            incoming_api,
            route_target.upstream_wire,
            headers_for_logging(upstream_response.headers())
        );
    }

    {
        let mut snapshot = state.last_successful_upstream_log.write().await;
        *snapshot = Some(UpstreamSuccessLogSnapshot {
            router_name: route_target.router_name.clone(),
            incoming_api,
            upstream_wire: route_target.upstream_wire,
            status: upstream_response.status(),
            headers: headers_for_logging(upstream_response.headers()),
        });
    }

    finalize_upstream_response(
        &state,
        upstream_response,
        &route_target,
        incoming_api,
        wants_stream,
        upstream_model,
        anthropic_input_tokens,
        response_id,
        tool_call_kinds_by_name,
        verbose_logging,
    )
    .await
}

#[cfg(test)]
mod tests;
