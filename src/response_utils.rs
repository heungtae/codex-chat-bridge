use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header::CACHE_CONTROL;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::response::Response;
use serde_json::Value;
use serde_json::json;
use uuid::Uuid;

use crate::bridge::streaming::sse_event;
use crate::bridge::streaming::anthropic_sse_event;
use crate::model::IncomingApi;

#[derive(Debug)]
pub(crate) struct NormalizedUpstreamError {
    pub(crate) code: String,
    pub(crate) message: String,
}

pub(crate) fn normalize_upstream_error_payload(
    status: StatusCode,
    body: &str,
) -> NormalizedUpstreamError {
    let default_message = format!("upstream returned {status}: {body}");
    let mut upstream_code = String::new();
    let mut upstream_message = String::new();

    if let Ok(parsed) = serde_json::from_str::<Value>(body) {
        if let Some(error) = parsed.get("error") {
            if let Some(code) = error.get("code").and_then(Value::as_str) {
                upstream_code = code.to_string();
            }
            if let Some(message) = error.get("message").and_then(Value::as_str) {
                upstream_message = message.to_string();
            }
        } else if let Some(code) = parsed.get("code").and_then(Value::as_str) {
            upstream_code = code.to_string();
            if let Some(message) = parsed.get("message").and_then(Value::as_str) {
                upstream_message = message.to_string();
            }
        }
    }

    let normalized_code = match upstream_code.as_str() {
        "context_length_exceeded" => "context_window_exceeded",
        "insufficient_quota" => "quota_exceeded",
        "rate_limit_exceeded" => "rate_limit_exceeded",
        "invalid_request_error" | "invalid_request" => "invalid_request",
        _ => {
            if status == StatusCode::TOO_MANY_REQUESTS {
                "rate_limit_exceeded"
            } else if status == StatusCode::BAD_REQUEST {
                "invalid_request"
            } else {
                "upstream_error"
            }
        }
    };
    let normalized_message = if upstream_message.is_empty() {
        default_message
    } else {
        upstream_message
    };

    NormalizedUpstreamError {
        code: normalized_code.to_string(),
        message: normalized_message,
    }
}

pub(crate) fn sse_error_response(code: &str, message: &str) -> Response {
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

pub(crate) fn anthropic_sse_error_response(code: &str, message: &str) -> Response {
    let body = anthropic_sse_event(
        "error",
        &json!({
            "type": "error",
            "error": {
                "type": code,
                "message": message,
            }
        }),
    );

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

pub(crate) fn error_response_for_api(
    incoming_api: IncomingApi,
    stream: bool,
    code: &str,
    message: &str,
) -> Response {
    match incoming_api {
        IncomingApi::Anthropic => {
            if stream {
                anthropic_sse_error_response(code, message)
            } else {
                anthropic_json_error_response(code, message)
            }
        }
        _ => {
            if stream {
                sse_error_response(code, message)
            } else {
                json_error_response(code, message)
            }
        }
    }
}

pub(crate) fn json_success_response(payload: Value) -> Response {
    let body = serde_json::to_vec(&payload).unwrap_or_else(|_| b"{}".to_vec());
    (
        StatusCode::OK,
        [(CONTENT_TYPE, HeaderValue::from_static("application/json"))],
        body,
    )
        .into_response()
}

pub(crate) fn json_error_response(code: &str, message: &str) -> Response {
    json_success_response(json!({
        "error": {
            "type": code,
            "message": message,
        }
    }))
}

pub(crate) fn anthropic_json_error_response(code: &str, message: &str) -> Response {
    json_success_response(json!({
        "type": "error",
        "error": {
            "type": code,
            "message": message,
        }
    }))
}
