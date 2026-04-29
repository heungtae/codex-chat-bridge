use axum::http::HeaderMap;
use axum::http::header::CONTENT_TYPE;
use serde_json::Value;

use crate::model::UpstreamHeader;
use crate::model::WireApi;

pub(crate) fn upstream_messages_for_logging(
    upstream_wire: WireApi,
    payload: &Value,
) -> Option<Value> {
    match upstream_wire {
        WireApi::Chat => payload.get("messages").cloned(),
        WireApi::Responses => payload.get("input").and_then(Value::as_array).map(|items| {
            let messages = items
                .iter()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
                .cloned()
                .collect();
            Value::Array(messages)
        }),
    }
}

pub(crate) fn tool_types_for_logging(payload: &Value) -> Value {
    let tool_labels = payload
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .map(tool_type_label_for_logging)
                .map(Value::String)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Value::Array(tool_labels)
}

pub(crate) fn tool_definitions_for_logging(payload: &Value) -> Value {
    let definitions = payload
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .map(tool_definition_for_logging)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Value::Array(definitions)
}

pub(crate) fn request_fields_for_logging(payload: &Value) -> Value {
    let Some(obj) = payload.as_object() else {
        return Value::Array(Vec::new());
    };

    let mut fields = obj.keys().cloned().collect::<Vec<_>>();
    fields.sort();
    Value::Array(fields.into_iter().map(Value::String).collect())
}

pub(crate) fn tool_type_label_for_logging(tool: &Value) -> String {
    let Some(obj) = tool.as_object() else {
        return "<invalid_tool>".to_string();
    };

    let tool_type = obj
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty());

    let function_name = obj
        .get("function")
        .and_then(Value::as_object)
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty());

    let top_level_name = obj
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty());

    let tool_name = function_name.or(top_level_name);

    match (tool_type, tool_name) {
        (Some(t), Some(n)) => format!("{t}({n})"),
        (Some(t), None) => t.to_string(),
        (None, Some(n)) => format!("<missing_type>({n})"),
        (None, None) => "<missing_type>".to_string(),
    }
}

fn tool_definition_for_logging(tool: &Value) -> Value {
    let function = tool.get("function").unwrap_or(tool);
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let parameters = function.get("parameters").unwrap_or(&Value::Null);

    serde_json::json!({
        "name": name,
        "required": parameters.get("required").cloned().unwrap_or(Value::Null),
        "additionalProperties": parameters
            .get("additionalProperties")
            .cloned()
            .unwrap_or(Value::Null),
    })
}

pub(crate) fn upstream_headers_for_logging(
    headers: &HeaderMap,
    api_key: &str,
    upstream_http_headers: &[UpstreamHeader],
    forwarded_headers: &[String],
) -> Value {
    let mut out = serde_json::Map::new();
    out.insert(
        "authorization".to_string(),
        Value::String(format!("Bearer {}", redact_for_logging(api_key))),
    );
    out.insert(
        CONTENT_TYPE.as_str().to_string(),
        Value::String("application/json".to_string()),
    );

    for header_name in forwarded_headers {
        if let Some(value) = headers.get(header_name) {
            let header_value = value
                .to_str()
                .map(str::to_string)
                .unwrap_or_else(|_| "<non-utf8>".to_string());
            out.insert(header_name.to_string(), Value::String(header_value));
        }
    }
    for header in upstream_http_headers {
        let header_name = header.name.to_ascii_lowercase();
        let header_value = if is_sensitive_upstream_header(&header_name) {
            redact_for_logging(&header.value).to_string()
        } else {
            header.value.clone()
        };
        out.insert(header_name, Value::String(header_value));
    }

    Value::Object(out)
}

pub(crate) fn headers_for_logging(headers: &HeaderMap) -> Value {
    let mut out = serde_json::Map::new();
    for (name, value) in headers {
        let header_name = name.as_str().to_ascii_lowercase();
        let header_value = if is_sensitive_upstream_header(&header_name)
            || header_name.eq_ignore_ascii_case("cookie")
            || header_name.eq_ignore_ascii_case("set-cookie")
        {
            redact_for_logging(value.to_str().ok().filter(|v| !v.is_empty()).unwrap_or("x"))
                .to_string()
        } else {
            value
                .to_str()
                .map(str::to_string)
                .unwrap_or_else(|_| "<non-utf8>".to_string())
        };
        out.insert(header_name, Value::String(header_value));
    }
    Value::Object(out)
}

fn is_sensitive_upstream_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("authorization")
        || name.eq_ignore_ascii_case("proxy-authorization")
        || name.eq_ignore_ascii_case("x-api-key")
        || name.eq_ignore_ascii_case("api-key")
}

fn redact_for_logging(secret: &str) -> &'static str {
    if secret.is_empty() {
        "<empty>"
    } else {
        "<redacted>"
    }
}
