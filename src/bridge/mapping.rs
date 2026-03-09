use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use tracing::warn;
use uuid::Uuid;

use crate::{BridgeRequest, ResponsesToolCallKind};

pub(crate) fn map_chat_to_responses_request(request: &Value, stream: bool) -> Result<Value> {
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

pub(crate) fn normalize_responses_tools(tools: Vec<Value>) -> Vec<Value> {
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

pub(crate) fn normalize_responses_tool_choice(choice: Value) -> Value {
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

pub(crate) fn chat_message_content_to_input_items(content: Option<&Value>) -> Vec<Value> {
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

pub(crate) fn responses_tool_call_kind_by_name(request: &Value) -> HashMap<String, ResponsesToolCallKind> {
    request
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    let tool_type = tool.get("type").and_then(Value::as_str)?;
                    let kind = match tool_type {
                        "function" => ResponsesToolCallKind::Function,
                        "custom" => ResponsesToolCallKind::Custom,
                        _ => return None,
                    };
                    let name = tool
                        .get("function")
                        .and_then(Value::as_object)
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .or_else(|| tool.get("name").and_then(Value::as_str))?
                        .trim()
                        .to_string();
                    if name.is_empty() {
                        return None;
                    }
                    Some((name, kind))
                })
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn responses_tool_call_item(
    name: &str,
    arguments: &str,
    call_id: &str,
    tool_call_kinds_by_name: &HashMap<String, ResponsesToolCallKind>,
) -> Value {
    match tool_call_kinds_by_name.get(name) {
        // Custom tools are freeform inputs in Responses API. If upstream chat
        // produced JSON-wrapped arguments (e.g. "..." or {"input":"..."}),
        // unwrap them back to raw text.
        Some(ResponsesToolCallKind::Custom) => json!({
            "type": "custom_tool_call",
            "name": name,
            "input": custom_tool_input_from_arguments(arguments),
            "call_id": call_id,
        }),
        _ => json!({
            "type": "function_call",
            "name": name,
            "arguments": arguments,
            "call_id": call_id,
        }),
    }
}

fn custom_tool_input_from_arguments(arguments: &str) -> String {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::String(s)) => s,
        Ok(Value::Object(obj)) => obj
            .get("input")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| arguments.to_string()),
        _ => arguments.to_string(),
    }
}

pub(crate) fn chat_json_to_responses_json(
    chat: Value,
    response_id: String,
    tool_call_kinds_by_name: &HashMap<String, ResponsesToolCallKind>,
    enable_provider_specific_fields: bool,
) -> Value {
    let mut output_items = Vec::new();
    let mut status = "completed";

    if let Some(choice) = chat
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
    {
        status = map_chat_finish_reason_to_responses_status(
            choice.get("finish_reason").and_then(Value::as_str),
        );
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

                    let mut item = responses_tool_call_item(
                        name,
                        &arguments,
                        &call_id,
                        tool_call_kinds_by_name,
                    );
                    if enable_provider_specific_fields {
                        if let Some(provider_specific_fields) = tc
                            .get("provider_specific_fields")
                            .cloned()
                            .or_else(|| function.get("provider_specific_fields").cloned())
                            && let Some(obj) = item.as_object_mut()
                        {
                            obj.insert(
                                "provider_specific_fields".to_string(),
                                provider_specific_fields,
                            );
                        }
                    }

                    output_items.push(item);
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

    let mut response = json!({
        "id": response_id,
        "object": "response",
        "status": status,
        "output": output_items,
        "usage": usage_json,
    });
    if enable_provider_specific_fields {
        if let Some(provider_specific_fields) = chat.get("provider_specific_fields").cloned()
            && let Some(obj) = response.as_object_mut()
        {
            obj.insert(
                "provider_specific_fields".to_string(),
                provider_specific_fields,
            );
        }
    }
    response
}

fn map_chat_finish_reason_to_responses_status(finish_reason: Option<&str>) -> &'static str {
    match finish_reason {
        Some("length" | "content_filter") => "incomplete",
        _ => "completed",
    }
}

pub(crate) fn map_responses_to_chat_request_with_stream(
    request: &Value,
    drop_tool_types: &HashSet<String>,
    stream: bool,
    enable_extended_input_types: bool,
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
    let mut pending_tool_call_ids = VecDeque::new();

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
                    .map_or_else(String::new, |items| {
                        flatten_content_items(items, enable_extended_input_types)
                    });

                if !content.trim().is_empty() {
                    messages.push(json!({
                        "role": role,
                        "content": content,
                    }));
                }
            }
            "reasoning" => {
                if let Some(content) = reasoning_item_to_text(item)
                    && !content.trim().is_empty()
                {
                    messages.push(json!({
                        "role": "assistant",
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
                pending_tool_call_ids.push_back(call_id);
            }
            "custom_tool_call" => {
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if name.is_empty() {
                    warn!("ignoring custom_tool_call item with empty name");
                    continue;
                }

                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .filter(|v| !v.trim().is_empty())
                    .map(ToString::to_string)
                    .unwrap_or_else(|| format!("call_{}", Uuid::now_v7()));
                let arguments = item
                    .get("input")
                    .or_else(|| item.get("arguments"))
                    .map(function_arguments_to_text)
                    .unwrap_or_default();

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
                pending_tool_call_ids.push_back(call_id);
            }
            "function_call_output" => {
                let call_id = resolve_tool_output_call_id(item, &mut pending_tool_call_ids)?;
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
            "tool_result" => {
                if !enable_extended_input_types {
                    warn!("ignoring unsupported input item type: {item_type}");
                    continue;
                }
                let call_id = resolve_tool_output_call_id(item, &mut pending_tool_call_ids)?;
                let output_text = item
                    .get("output")
                    .or_else(|| item.get("result"))
                    .map(function_output_to_text)
                    .unwrap_or_default();
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": output_text,
                }));
            }
            "custom_tool_call_output" => {
                let call_id = resolve_tool_output_call_id(item, &mut pending_tool_call_ids)?;
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
            "web_search_call" => {
                if !enable_extended_input_types {
                    warn!("ignoring unsupported input item type: {item_type}");
                    continue;
                }
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .filter(|v| !v.trim().is_empty())
                    .map(ToString::to_string)
                    .unwrap_or_else(|| format!("call_{}", Uuid::now_v7()));
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or("web_search");
                let arguments = item
                    .get("arguments")
                    .or_else(|| item.get("query"))
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
                pending_tool_call_ids.push_back(call_id);
            }
            "mcp_tool_call_output" => {
                let call_id = resolve_tool_output_call_id(item, &mut pending_tool_call_ids)?;
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

    apply_responses_request_extensions(request, &mut chat_request)?;

    Ok(BridgeRequest { chat_request })
}

fn resolve_tool_output_call_id(item: &Value, pending_tool_call_ids: &mut VecDeque<String>) -> Result<String> {
    let incoming_call_id = item
        .get("call_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToString::to_string);

    match incoming_call_id {
        Some(call_id) => {
            if let Some(pos) = pending_tool_call_ids.iter().position(|id| id == &call_id) {
                pending_tool_call_ids.remove(pos);
                return Ok(call_id);
            }
            if pending_tool_call_ids.len() == 1 {
                let recovered = pending_tool_call_ids
                    .pop_front()
                    .expect("single pending id must exist");
                warn!(
                    "recovering tool output call_id mismatch: incoming={} recovered={}",
                    call_id, recovered
                );
                return Ok(recovered);
            }
            if pending_tool_call_ids.is_empty() {
                return Ok(call_id);
            }
            Err(anyhow!(
                "tool output call_id `{}` does not match pending tool calls {:?}",
                call_id,
                pending_tool_call_ids
            ))
        }
        None => {
            let Some(recovered) = pending_tool_call_ids.pop_front() else {
                return Err(anyhow!(
                    "tool output is missing `call_id` and no pending tool call exists"
                ));
            };
            warn!("recovering missing tool output call_id with pending call_id={recovered}");
            Ok(recovered)
        }
    }
}

fn apply_responses_request_extensions(request: &Value, chat_request: &mut Value) -> Result<()> {
    let Some(chat_obj) = chat_request.as_object_mut() else {
        return Err(anyhow!("chat request payload must be an object"));
    };

    if let Some(max_output_tokens) = request.get("max_output_tokens") {
        chat_obj.insert("max_tokens".to_string(), max_output_tokens.clone());
    }
    if let Some(metadata) = request.get("metadata") {
        chat_obj.insert("metadata".to_string(), metadata.clone());
    }
    if let Some(reasoning) = request.get("reasoning") {
        chat_obj.insert("reasoning".to_string(), reasoning.clone());
    }
    if let Some(service_tier) = request.get("service_tier") {
        chat_obj.insert("service_tier".to_string(), service_tier.clone());
    }
    if let Some(include) = request.get("include") {
        chat_obj.insert("include".to_string(), include.clone());
    }
    if let Some(text) = request.get("text") {
        if let Some(format) = text.get("format") {
            chat_obj.insert("response_format".to_string(), format.clone());
        }
        chat_obj.insert("text".to_string(), text.clone());
    }

    Ok(())
}

fn reasoning_item_to_text(item: &Value) -> Option<String> {
    if let Some(summary_items) = item.get("summary").and_then(Value::as_array) {
        let summary_text = flatten_content_items(summary_items, true);
        if !summary_text.trim().is_empty() {
            return Some(format!("[reasoning_summary] {summary_text}"));
        }
    }

    if let Some(content_items) = item.get("content").and_then(Value::as_array) {
        let content_text = flatten_content_items(content_items, true);
        if !content_text.trim().is_empty() {
            return Some(format!("[reasoning_summary] {content_text}"));
        }
    }

    let text = item.get("text").and_then(Value::as_str).unwrap_or_default();
    if text.trim().is_empty() {
        return None;
    }
    Some(format!("[reasoning_summary] {text}"))
}

pub(crate) fn flatten_content_items(items: &[Value], enable_extended_input_types: bool) -> String {
    let mut parts = Vec::new();
    for item in items {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        if matches!(item_type, "input_text" | "output_text" | "summary_text")
            && let Some(text) = item.get("text").and_then(Value::as_str)
            && !text.is_empty()
        {
            parts.push(text.to_string());
            continue;
        }

        if enable_extended_input_types && item_type == "input_image" {
            if let Some(url) = item.get("image_url").and_then(Value::as_str) {
                parts.push(format!("[input_image] {url}"));
            } else {
                parts.push("[input_image]".to_string());
            }
            continue;
        }

        if enable_extended_input_types && item_type == "input_file" {
            if let Some(file_id) = item.get("file_id").and_then(Value::as_str) {
                parts.push(format!("[input_file] file_id={file_id}"));
            } else if let Some(file_data) = item.get("file_data").and_then(Value::as_str) {
                parts.push(format!("[input_file] file_data={file_data}"));
            } else {
                parts.push("[input_file]".to_string());
            }
        }
    }

    parts.join("\n")
}

pub(crate) fn function_output_to_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => flatten_content_items(items, true),
        other => other.to_string(),
    }
}

pub(crate) fn function_arguments_to_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub(crate) fn normalize_chat_tools(tools: Vec<Value>, drop_tool_types: &HashSet<String>) -> Vec<Value> {
    tools
        .into_iter()
        .filter_map(|tool| {
            let tool_type = tool.get("type").and_then(Value::as_str);
            if tool_type.is_some_and(|t| drop_tool_types.contains(t)) {
                return None;
            }

            if tool_type == Some("function") {
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

                return Some(json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": description,
                        "parameters": parameters,
                    }
                }));
            }

            if tool_type == Some("custom") {
                if let Some(function) = tool.get("function").cloned() {
                    let mut converted = tool;
                    if let Some(obj) = converted.as_object_mut() {
                        obj.insert("type".to_string(), Value::String("function".to_string()));
                        obj.insert("function".to_string(), function);
                    }
                    return Some(converted);
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
                    .or_else(|| tool.get("input_schema").cloned())
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}}));

                return Some(json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": description,
                        "parameters": parameters,
                    }
                }));
            }

            if tool_type == Some("mcp") {
                let server_label = tool
                    .get("server_label")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .unwrap_or("mcp_server");
                let function_name = format!("mcp__{server_label}");
                let description = tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("MCP tool proxy");
                let parameters = tool
                    .get("parameters")
                    .cloned()
                    .or_else(|| tool.get("input_schema").cloned())
                    .unwrap_or_else(|| {
                        json!({
                            "type": "object",
                            "properties": {
                                "arguments": {
                                    "type": "object",
                                    "additionalProperties": true
                                }
                            },
                            "additionalProperties": true
                        })
                    });
                return Some(json!({
                    "type": "function",
                    "function": {
                        "name": function_name,
                        "description": description,
                        "parameters": parameters,
                    }
                }));
            }

            if matches!(tool_type, Some("web_search") | Some("web_search_preview")) {
                let name = tool
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .unwrap_or(tool_type.unwrap_or("web_search"));
                let description = tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("Web search tool");
                let parameters = tool
                    .get("parameters")
                    .cloned()
                    .or_else(|| tool.get("input_schema").cloned())
                    .unwrap_or_else(|| {
                        json!({
                            "type": "object",
                            "properties": {
                                "query": {"type": "string"}
                            },
                            "required": ["query"],
                            "additionalProperties": true
                        })
                    });
                return Some(json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": description,
                        "parameters": parameters,
                    }
                }));
            }

            Some(tool)
        })
        .collect()
}

pub(crate) fn normalize_tool_choice(tool_choice: Value) -> Value {
    if let Some(s) = tool_choice.as_str() {
        return Value::String(s.to_string());
    }

    let Some(obj) = tool_choice.as_object() else {
        return Value::String("auto".to_string());
    };

    let tool_type = obj.get("type").and_then(Value::as_str);

    if tool_type == Some("custom")
        && let Some(name) = obj
            .get("function")
            .and_then(Value::as_object)
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
    {
        return json!({
            "type": "function",
            "function": {
                "name": name,
            }
        });
    }

    if tool_type == Some("custom")
        && let Some(name) = obj.get("name").and_then(Value::as_str)
    {
        return json!({
            "type": "function",
            "function": {
                "name": name,
            }
        });
    }

    if obj.get("function").is_some() {
        return tool_choice;
    }

    if tool_type == Some("function")
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
