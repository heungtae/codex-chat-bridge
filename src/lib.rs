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

#[derive(Serialize)]
struct ServerInfo {
    port: u16,
    ports: Vec<u16>,
    pid: u32,
}

fn init_tracing(verbose_logging: bool) {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if verbose_logging {
            tracing_subscriber::EnvFilter::new("debug")
        } else {
            tracing_subscriber::EnvFilter::new("info")
        }
    });

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .init();
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

fn log_runtime_startup(config: &ResolvedConfig, router_manager: &RouterManager, listen_addrs: &[String]) {
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
            upstream_wire,
            override_upstream_url,
            override_upstream_wire,
            override_upstream_http_headers,
            override_forward_incoming_headers,
            override_drop_tool_types,
            override_drop_request_fields,
        } = snapshot;

        let mut overrides = Vec::new();
        if let Some(v) = override_upstream_url {
            overrides.push(format!("upstream_url={v}"));
        }
        if let Some(v) = override_upstream_wire {
            overrides.push(format!("upstream_wire={v:?}"));
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
        let override_summary = if overrides.is_empty() {
            "none".to_string()
        } else {
            overrides.join(", ")
        };

        info!(
            "router: name={}, active={}, incoming_url={:?}, upstream_wire={:?}, overrides={}",
            name, active, incoming_url, upstream_wire, override_summary
        );
    }
}

async fn run_server(app: Router, listen_addrs: &[String], server_info: Option<&Path>) -> Result<()> {
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

fn resolve_incoming_route(incoming_api_hint: Option<IncomingApi>, incoming_path: Option<String>) -> String {
    incoming_path.unwrap_or_else(|| match incoming_api_hint {
        Some(IncomingApi::Chat) => "/v1/chat/completions".to_string(),
        Some(IncomingApi::Responses) => "/v1/responses".to_string(),
        None => "/v1/responses".to_string(),
    })
}

async fn resolve_route_target(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    incoming_path: Option<&str>,
) -> std::result::Result<RouteTarget, Response> {
    let routers = state.routers.read().await;
    let host_header = headers.get(HOST).and_then(|h| h.to_str().ok());
    let route_target = match incoming_path {
        Some(path) => match routers.get_target_for_incoming_route(path, host_header) {
            Ok(Some(target)) => target,
            Ok(None) => return Err((StatusCode::NOT_FOUND, "not found").into_response()),
            Err(err) => return Err(json_error_response("router_error", &err.to_string())),
        },
        None => {
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
    route_target: &RouteTarget,
    verbose_logging: bool,
) -> std::result::Result<(IncomingApi, bool, Value, HashMap<String, ResponsesToolCallKind>), Response> {
    let fallback_incoming_api = incoming_api_hint.unwrap_or(IncomingApi::Responses);
    let mut request_value: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(err) => {
            return Err(error_response_for_stream(
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

    let incoming_api = incoming_api_hint.unwrap_or_else(|| infer_incoming_api(&request_value));
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
        return Err(error_response_for_stream(
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

    Ok((incoming_api, wants_stream, request_value, tool_call_kinds_by_name))
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
    ) {
        Ok(v) => v,
        Err(err) => {
            return Err(error_response_for_stream(
                wants_stream,
                "invalid_request",
                &err.to_string(),
            ));
        }
    };

    if route_target.feature_flags.enable_previous_response_id
        && incoming_api == IncomingApi::Responses
        && route_target.upstream_wire == WireApi::Chat
    {
        let previous_messages = {
            let sessions = state.sessions.read().await;
            match resolve_previous_messages_for_request(request_value, &sessions) {
                Ok(v) => v,
                Err(err) => {
                    return Err(error_response_for_stream(
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
            return Err(error_response_for_stream(
                wants_stream,
                "invalid_request",
                &err.to_string(),
            ));
        }

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

fn build_upstream_request(
    state: &Arc<AppState>,
    route_target: &RouteTarget,
    headers: &HeaderMap,
    upstream_payload: &Value,
) -> reqwest::RequestBuilder {
    let mut upstream_request = state
        .client
        .post(&route_target.upstream_url)
        .bearer_auth(&state.api_key)
        .header(CONTENT_TYPE, "application/json")
        .json(upstream_payload);

    for header_name in &route_target.forward_incoming_headers {
        if let Some(value) = headers.get(header_name) {
            upstream_request = upstream_request.header(header_name, value.clone());
        }
    }
    for header in &route_target.upstream_http_headers {
        upstream_request = upstream_request.header(&header.name, &header.value);
    }
    upstream_request
}

async fn finalize_upstream_response(
    upstream_response: reqwest::Response,
    route_target: &RouteTarget,
    incoming_api: IncomingApi,
    wants_stream: bool,
    response_id: String,
    tool_call_kinds_by_name: HashMap<String, ResponsesToolCallKind>,
    verbose_logging: bool,
) -> Response {
    if !upstream_response.status().is_success() {
        let status = upstream_response.status();
        let body = upstream_response
            .text()
            .await
            .unwrap_or_else(|_| "<failed to read error body>".to_string());
        if verbose_logging {
            debug!(
                "upstream response payload error (router={}, {:?}<-{:?}): {body}",
                route_target.router_name,
                incoming_api,
                route_target.upstream_wire
            );
        }
        let normalized = normalize_upstream_error_payload(status, &body);
        return error_response_for_stream(wants_stream, &normalized.code, &normalized.message);
    }

    if wants_stream {
        let body = match route_target.upstream_wire {
            WireApi::Chat => Body::from_stream(translate_chat_stream(
                upstream_response.bytes_stream(),
                response_id,
                route_target.router_name.clone(),
                verbose_logging,
                tool_call_kinds_by_name,
                route_target.feature_flags,
            )),
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
            return json_error_response(
                "upstream_decode_error",
                &format!("failed to decode upstream JSON: {err}"),
            );
        }
    };
    if verbose_logging {
        debug!(
            "upstream response payload (router={}, {:?}<-{:?}): {}",
            route_target.router_name,
            incoming_api,
            route_target.upstream_wire,
            upstream_json
        );
    }

    let response_json = match route_target.upstream_wire {
        WireApi::Chat => chat_json_to_responses_json(
            upstream_json,
            response_id,
            &tool_call_kinds_by_name,
            route_target.feature_flags.enable_provider_specific_fields,
        ),
        WireApi::Responses => upstream_json,
    };

    json_success_response(response_json)
}

pub(crate) async fn handle_incoming(
    state: Arc<AppState>,
    headers: HeaderMap,
    body: String,
    incoming_api_hint: Option<IncomingApi>,
    incoming_path: Option<String>,
) -> Response {
    let route_target = match resolve_route_target(&state, &headers, incoming_path.as_deref()).await {
        Ok(target) => target,
        Err(response) => return response,
    };
    let verbose_logging = state.verbose_logging;
    let incoming_route = resolve_incoming_route(incoming_api_hint, incoming_path);

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
        debug!(
            "incoming request body (router={}): {body}",
            route_target.router_name
        );
    }

    let (incoming_api, wants_stream, request_value, tool_call_kinds_by_name) =
        match parse_and_prepare_request(&body, incoming_api_hint, &route_target, verbose_logging) {
            Ok(v) => v,
            Err(response) => return response,
        };
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
            debug!(
                "upstream messages (router={}, {:?}->{:?}): {}",
                route_target.router_name, incoming_api, route_target.upstream_wire, messages
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

        debug!(
            "upstream payload (router={}, {:?}->{:?}): {}",
            route_target.router_name, incoming_api, route_target.upstream_wire, upstream_payload
        );
        debug!(
            "upstream tool types (router={}, {:?}->{:?}): {}",
            route_target.router_name,
            incoming_api,
            route_target.upstream_wire,
            tool_types_for_logging(&upstream_payload)
        );
        debug!(
            "upstream request fields (router={}, {:?}->{:?}): {}",
            route_target.router_name,
            incoming_api,
            route_target.upstream_wire,
            request_fields_for_logging(&upstream_payload)
        );
    }

    let upstream_request = build_upstream_request(&state, &route_target, &headers, &upstream_payload);
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

    finalize_upstream_response(
        upstream_response,
        &route_target,
        incoming_api,
        wants_stream,
        response_id,
        tool_call_kinds_by_name,
        verbose_logging,
    )
    .await
}

#[cfg(test)]
mod tests;
