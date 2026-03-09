use anyhow::Result;
use anyhow::anyhow;
use serde_json::Value;
use std::collections::HashSet;

use crate::bridge::mapping::map_chat_to_responses_request;
use crate::bridge::mapping::map_responses_to_chat_request_with_stream;
use crate::model::IncomingApi;
use crate::model::ToolTransformMode;
use crate::model::WireApi;

pub(crate) fn infer_incoming_api(request: &Value) -> IncomingApi {
    if request.get("messages").is_some() {
        IncomingApi::Chat
    } else {
        IncomingApi::Responses
    }
}

pub(crate) fn stream_default_for_api(incoming_api: IncomingApi) -> bool {
    match incoming_api {
        IncomingApi::Responses => true,
        IncomingApi::Chat => false,
    }
}

pub(crate) fn stream_flag_for_request(incoming_api: IncomingApi, request: &Value) -> bool {
    request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| stream_default_for_api(incoming_api))
}

pub(crate) fn apply_request_filters(
    incoming_api: IncomingApi,
    request: &mut Value,
    drop_tool_types: &HashSet<String>,
    drop_request_fields: &HashSet<String>,
) {
    let Some(obj) = request.as_object_mut() else {
        return;
    };

    for field in drop_request_fields {
        obj.remove(field);
    }

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

pub(crate) fn validate_capability_gate(
    incoming_api: IncomingApi,
    upstream_wire: WireApi,
    enable_extended_input_types: bool,
    request: &Value,
) -> Result<()> {
    if incoming_api != IncomingApi::Responses || upstream_wire != WireApi::Chat {
        return Ok(());
    }

    if let Some(value) = request.get("max_output_tokens")
        && !value.is_null()
        && !value.is_number()
    {
        return Err(anyhow!("`max_output_tokens` must be a number when provided"));
    }

    if let Some(value) = request.get("metadata")
        && !value.is_null()
        && !value.is_object()
    {
        return Err(anyhow!("`metadata` must be an object when provided"));
    }

    if let Some(tools) = request.get("tools").and_then(Value::as_array) {
        for tool in tools {
            let tool_type = tool.get("type").and_then(Value::as_str).unwrap_or_default();
            let is_supported = if enable_extended_input_types {
                matches!(
                    tool_type,
                    "function" | "custom" | "mcp" | "web_search" | "web_search_preview"
                )
            } else {
                matches!(tool_type, "function" | "custom")
            };
            if !is_supported {
                return Err(anyhow!(
                    "responses->chat bridge does not support tool type `{tool_type}`"
                ));
            }
        }
    }

    Ok(())
}

pub(crate) fn build_upstream_payload(
    request: &Value,
    incoming_api: IncomingApi,
    upstream_wire: WireApi,
    stream: bool,
    enable_extended_input_types: bool,
    tool_transform_mode: ToolTransformMode,
) -> Result<Value> {
    let mut payload = match (incoming_api, upstream_wire) {
        (IncomingApi::Responses, WireApi::Responses) => request.clone(),
        (IncomingApi::Responses, WireApi::Chat) => {
            map_responses_to_chat_request_with_stream(
                request,
                &HashSet::new(),
                stream,
                enable_extended_input_types,
                tool_transform_mode,
            )?
            .chat_request
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
