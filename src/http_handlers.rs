use axum::Router;
use axum::extract::Path as AxumPath;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use serde_json::json;
use std::sync::Arc;

use crate::model::IncomingApi;
use crate::response_utils::json_success_response;
use crate::routing::normalize_request_path;
use crate::state::AppState;

pub(crate) fn build_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/messages", post(handle_anthropic_messages))
        .route("/v1/responses", post(handle_responses))
        .route("/v1/chat/completions", post(handle_chat_completions))
        .route("/healthz", get(healthz))
        .route("/shutdown", get(shutdown))
        .route("/routers", get(list_routers))
        .route("/{*incoming_path}", post(handle_routed_incoming))
        .with_state(state)
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

async fn list_routers(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let routers = state.routers.read().await;
    let router_names = routers.get_router_names();

    json_success_response(json!({
        "routers": router_names,
    }))
}

async fn handle_responses(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    crate::handle_incoming(
        state,
        headers,
        body,
        Some(IncomingApi::Responses),
        Some("/v1/responses".to_string()),
    )
    .await
}

async fn handle_anthropic_messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    crate::handle_incoming(
        state,
        headers,
        body,
        Some(IncomingApi::Anthropic),
        Some("/v1/messages".to_string()),
    )
    .await
}

async fn handle_chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    crate::handle_incoming(
        state,
        headers,
        body,
        Some(IncomingApi::Chat),
        Some("/v1/chat/completions".to_string()),
    )
    .await
}

async fn handle_routed_incoming(
    State(state): State<Arc<AppState>>,
    AxumPath(incoming_path): AxumPath<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    crate::handle_incoming(
        state,
        headers,
        body,
        None,
        Some(normalize_request_path(&incoming_path)),
    )
    .await
}
