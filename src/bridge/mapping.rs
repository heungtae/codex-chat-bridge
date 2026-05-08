use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use tracing::warn;
use uuid::Uuid;

use crate::bridge::apply_patch::normalize_apply_patch_input_with_repairs;
use crate::{BridgeRequest, ResponsesToolCallKind, ToolTransformMode};

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
    for (message_index, message) in messages.iter().enumerate() {
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

        if role == "assistant" {
            if let Some(reasoning) = chat_message_reasoning_text(message) {
                input.push(json!({
                    "type": "reasoning",
                    "summary": [{
                        "type": "summary_text",
                        "text": reasoning,
                    }],
                }));
            }

            let content = chat_message_content_to_input_items(message.get("content"));
            if !content.is_empty() {
                input.push(json!({
                    "type": "message",
                    "role": role,
                    "content": content,
                }));
            }

            if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
                for (tool_index, tool_call) in tool_calls.iter().enumerate() {
                    if let Some(function_call) =
                        chat_tool_call_to_responses_input_item(tool_call, message_index, tool_index)
                    {
                        input.push(function_call);
                    }
                }
            }

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

    if let Some(response_format) = request.get("response_format").cloned()
        && !response_format.is_null()
        && let Some(obj) = out.as_object_mut()
    {
        obj.insert(
            "text".to_string(),
            json!({
                "format": response_format,
            }),
        );
    }

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

pub(crate) fn map_anthropic_messages_to_chat_request(
    request: &Value,
    preserve_thinking: bool,
) -> Result<Value> {
    let model = request
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing `model`"))?;
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing `messages` array"))?;

    let mut chat_messages = Vec::new();

    if let Some(system_message) = anthropic_system_to_chat_message(request.get("system")) {
        chat_messages.push(system_message);
    }

    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        match role {
            "assistant" => {
                if let Some(chat_message) = anthropic_assistant_message_to_chat_message(
                    message.get("content"),
                    preserve_thinking,
                ) {
                    chat_messages.push(chat_message);
                }
            }
            "user" => {
                chat_messages.extend(anthropic_user_message_to_chat_messages(
                    message.get("content"),
                ));
            }
            _ => {}
        }
    }

    let mut out = json!({
        "model": model,
        "messages": chat_messages,
    });

    let Some(obj) = out.as_object_mut() else {
        return Ok(out);
    };

    for field in ["max_tokens", "temperature", "top_p", "metadata"] {
        if let Some(value) = request.get(field).cloned()
            && !value.is_null()
        {
            obj.insert(field.to_string(), value);
        }
    }

    if let Some(stop_sequences) = request.get("stop_sequences").cloned()
        && !stop_sequences.is_null()
    {
        obj.insert("stop".to_string(), stop_sequences);
    }

    if let Some(stream) = request.get("stream").cloned()
        && !stream.is_null()
    {
        obj.insert("stream".to_string(), stream);
    }

    if let Some(tools) = request.get("tools").and_then(Value::as_array) {
        let chat_tools = tools
            .iter()
            .filter_map(|tool| {
                let name = tool.get("name").and_then(Value::as_str)?;
                let description = tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let parameters = tool
                    .get("input_schema")
                    .cloned()
                    .unwrap_or_else(default_function_tool_parameters);
                let parameters = normalize_anthropic_tool_parameters(name, parameters);
                Some(json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": description,
                        "parameters": parameters,
                    }
                }))
            })
            .collect::<Vec<_>>();
        if !chat_tools.is_empty() {
            obj.insert("tools".to_string(), Value::Array(chat_tools));
        }
    }

    if let Some(tool_choice) = anthropic_tool_choice_to_chat_tool_choice(request.get("tool_choice"))
    {
        obj.insert("tool_choice".to_string(), tool_choice);
    }

    Ok(out)
}

fn anthropic_system_to_chat_message(system: Option<&Value>) -> Option<Value> {
    let content = match system {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    };
    if content.is_empty() {
        None
    } else {
        Some(json!({
            "role": "system",
            "content": content,
        }))
    }
}

fn anthropic_assistant_message_to_chat_message(
    content: Option<&Value>,
    preserve_thinking: bool,
) -> Option<Value> {
    let mut content_parts = Vec::new();
    let mut reasoning_parts = Vec::new();
    let mut tool_calls = Vec::new();

    match content {
        Some(Value::String(text)) => {
            if split_think_tagged_text(text).is_some() {
                // Split into blocks below.
            } else if !text.trim().is_empty() {
                content_parts.push(text.to_string());
            }
        }
        Some(Value::Array(items)) => {
            for item in items {
                match item.get("type").and_then(Value::as_str).unwrap_or_default() {
                    "text" => {
                        if let Some(text) = item.get("text").and_then(Value::as_str)
                            && !text.trim().is_empty()
                        {
                            content_parts.push(text.to_string());
                        }
                    }
                    "thinking" => {
                        if let Some(thinking) = item.get("thinking").and_then(Value::as_str)
                            && !thinking.trim().is_empty()
                        {
                            reasoning_parts.push(thinking.to_string());
                        }
                    }
                    "tool_use" => {
                        let name = item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown_function");
                        let arguments = item
                            .get("input")
                            .cloned()
                            .unwrap_or_else(|| json!({}))
                            .to_string();
                        tool_calls.push(json!({
                            "id": item.get("id").and_then(Value::as_str).unwrap_or_default(),
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": arguments,
                            }
                        }));
                    }
                    _ => {}
                }
            }
        }
        Some(other) => {
            let text = other.to_string();
            if !text.trim().is_empty() {
                content_parts.push(text);
            }
        }
        None => {}
    }

    if let Some(reconstructed) = content
        .and_then(|value| value.as_str())
        .and_then(split_think_tagged_text)
    {
        for block in reconstructed {
            match block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
            {
                "text" => {
                    if let Some(text) = block.get("text").and_then(Value::as_str)
                        && !text.trim().is_empty()
                    {
                        content_parts.push(text.to_string());
                    }
                }
                "thinking" => {
                    if let Some(thinking) = block.get("thinking").and_then(Value::as_str)
                        && !thinking.trim().is_empty()
                    {
                        reasoning_parts.push(thinking.to_string());
                    }
                }
                _ => {}
            }
        }
    }

    if content_parts.is_empty() && reasoning_parts.is_empty() && tool_calls.is_empty() {
        return None;
    }

    let mut assistant_content = if content_parts.is_empty() {
        String::new()
    } else {
        content_parts.join("\n\n")
    };
    if preserve_thinking && !reasoning_parts.is_empty() {
        assistant_content = append_preserved_thinking_to_chat_content(
            &assistant_content,
            &reasoning_parts.join("\n\n"),
        );
    }

    let mut message = json!({
        "role": "assistant",
        "content": assistant_content,
    });
    if let Some(obj) = message.as_object_mut() {
        if !tool_calls.is_empty() {
            obj.insert("tool_calls".to_string(), Value::Array(tool_calls));
        }
        if !reasoning_parts.is_empty() {
            obj.insert(
                "reasoning_content".to_string(),
                Value::String(reasoning_parts.join("\n\n")),
            );
        }
    }
    Some(message)
}

fn split_think_tagged_text(text: &str) -> Option<Vec<Value>> {
    if !text.contains("<think>")
        && !text.contains("</think>")
        && !text.contains("<thinking>")
        && !text.contains("</thinking>")
    {
        return None;
    }

    let mut blocks = Vec::new();
    let mut rest = text;

    loop {
        let open_pos = match next_think_open(rest) {
            Some(pos) => pos,
            None => {
                if !rest.is_empty() {
                    blocks.push(json!({
                        "type": "text",
                        "text": rest,
                    }));
                }
                break;
            }
        };

        let (prefix, after_open) = rest.split_at(open_pos);
        if !prefix.is_empty() {
            blocks.push(json!({
                "type": "text",
                "text": prefix,
            }));
        }

        let after_open = &after_open[think_open_len(after_open)..];
        let Some(close_pos) = next_think_close(after_open) else {
            if !after_open.is_empty() {
                blocks.push(json!({
                    "type": "text",
                    "text": after_open,
                }));
            }
            break;
        };

        let (thinking, after_close) = after_open.split_at(close_pos);
        if !thinking.is_empty() {
            blocks.push(json!({
                "type": "thinking",
                "thinking": thinking,
            }));
        }
        rest = &after_close[think_close_len(after_close)..];
    }

    Some(blocks)
}

fn next_think_open(text: &str) -> Option<usize> {
    match (text.find("<think>"), text.find("<thinking>")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn think_open_len(text: &str) -> usize {
    if text.starts_with("<thinking>") {
        "<thinking>".len()
    } else {
        "<think>".len()
    }
}

fn next_think_close(text: &str) -> Option<usize> {
    match (text.find("</think>"), text.find("</thinking>")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn think_close_len(text: &str) -> usize {
    if text.starts_with("</thinking>") {
        "</thinking>".len()
    } else {
        "</think>".len()
    }
}

fn append_preserved_thinking_to_chat_content(content: &str, thinking: &str) -> String {
    let thinking = thinking.trim();
    if thinking.is_empty() {
        return content.to_string();
    }
    if content.trim().is_empty() {
        return format!("<thinking>\n{thinking}\n</thinking>");
    }
    format!("<thinking>\n{thinking}\n</thinking>\n\n{content}")
}

fn anthropic_user_message_to_chat_messages(content: Option<&Value>) -> Vec<Value> {
    let mut messages = Vec::new();
    let mut content_parts = Vec::new();
    let mut has_image = false;

    let flush_content =
        |messages: &mut Vec<Value>, content_parts: &mut Vec<Value>, has_image: &mut bool| {
            if content_parts.is_empty() {
                return;
            }

            if *has_image {
                messages.push(json!({
                    "role": "user",
                    "content": content_parts.clone(),
                }));
            } else {
                let text_parts = content_parts
                    .iter()
                    .filter_map(|item| item.get("text").and_then(Value::as_str))
                    .map(ToString::to_string)
                    .collect::<Vec<_>>();
                if text_parts.len() == 1 {
                    messages.push(json!({
                        "role": "user",
                        "content": text_parts[0].clone(),
                    }));
                } else {
                    messages.push(json!({
                        "role": "user",
                        "content": text_parts.join("\n"),
                    }));
                }
            }

            content_parts.clear();
            *has_image = false;
        };

    match content {
        Some(Value::String(text)) => {
            if !text.trim().is_empty() {
                messages.push(json!({
                    "role": "user",
                    "content": text,
                }));
            }
        }
        Some(Value::Array(items)) => {
            for item in items {
                match item.get("type").and_then(Value::as_str).unwrap_or_default() {
                    "text" => {
                        if let Some(text) = item.get("text").and_then(Value::as_str)
                            && !text.trim().is_empty()
                        {
                            content_parts.push(json!({
                                "type": "text",
                                "text": text,
                            }));
                        }
                    }
                    "tool_result" => {
                        flush_content(&mut messages, &mut content_parts, &mut has_image);
                        let content = anthropic_tool_result_content_to_text(item.get("content"));
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": item.get("tool_use_id").and_then(Value::as_str).unwrap_or_default(),
                            "content": content,
                        }));
                    }
                    "image" => {
                        if let Some(image_item) = anthropic_image_to_chat_content_item(item) {
                            has_image = true;
                            content_parts.push(image_item);
                        }
                    }
                    _ => {}
                }
            }
            flush_content(&mut messages, &mut content_parts, &mut has_image);
        }
        Some(other) => {
            let text = other.to_string();
            if !text.trim().is_empty() {
                messages.push(json!({
                    "role": "user",
                    "content": text,
                }));
            }
        }
        None => {}
    }

    messages
}

fn anthropic_image_to_chat_content_item(item: &Value) -> Option<Value> {
    let source = item.get("source")?;
    let source_type = source.get("type").and_then(Value::as_str)?;

    let image_url = match source_type {
        "url" => source
            .get("url")
            .and_then(Value::as_str)?
            .trim()
            .to_string(),
        "base64" => {
            let data = source.get("data").and_then(Value::as_str)?.trim();
            if data.is_empty() {
                return None;
            }
            let media_type = source
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or("image/png")
                .trim();
            if media_type.is_empty() {
                return None;
            }
            format!("data:{};base64,{}", media_type, data)
        }
        "file" => {
            warn!("ignoring anthropic image block with unsupported file source");
            return None;
        }
        _ => {
            warn!("ignoring anthropic image block with unsupported source type: {source_type}");
            return None;
        }
    };

    if image_url.is_empty() {
        return None;
    }

    Some(json!({
        "type": "image_url",
        "image_url": {
            "url": image_url,
        }
    }))
}

fn anthropic_tool_result_content_to_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.to_string(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.get("text")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
                    .unwrap_or_else(|| item.to_string())
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Object(_)) => content.cloned().unwrap_or(Value::Null).to_string(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn anthropic_tool_choice_to_chat_tool_choice(tool_choice: Option<&Value>) -> Option<Value> {
    let tool_choice = tool_choice?.as_object()?;
    match tool_choice.get("type").and_then(Value::as_str)? {
        "auto" => Some(Value::String("auto".to_string())),
        "any" => Some(Value::String("required".to_string())),
        "tool" => tool_choice.get("name").and_then(Value::as_str).map(|name| {
            json!({
                "type": "function",
                "function": {
                    "name": name,
                }
            })
        }),
        _ => None,
    }
}

pub(crate) fn normalize_responses_tools(tools: Vec<Value>) -> Vec<Value> {
    tools
        .into_iter()
        .filter_map(normalize_responses_tool_shape)
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
            .filter_map(chat_content_item_to_input_item)
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

pub(crate) fn responses_tool_call_kind_by_name(
    request: &Value,
) -> HashMap<String, ResponsesToolCallKind> {
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
            "input": custom_tool_input_from_arguments(name, arguments),
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

fn custom_tool_input_from_arguments(name: &str, arguments: &str) -> String {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let input = match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::String(s)) => s,
        Ok(Value::Object(obj)) => obj
            .get("input")
            .or_else(|| obj.get("patch"))
            .or_else(|| obj.get("content"))
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| arguments.to_string()),
        _ => arguments.to_string(),
    };

    if name == "apply_patch" {
        let normalized = normalize_apply_patch_input_with_repairs(&input);
        if !normalized.repairs.is_empty() {
            warn!(
                repairs = ?normalized.repairs,
                before = %input,
                after = %normalized.normalized,
                "repaired apply_patch input"
            );
        }
        normalized.normalized
    } else {
        input
    }
}

fn default_custom_tool_function_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "input": { "type": "string" }
        },
        "required": ["input"],
        "additionalProperties": true
    })
}

fn default_function_tool_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "required": [],
        "additionalProperties": false
    })
}

fn normalize_custom_tool_parameters(parameters: Option<Value>) -> Value {
    match parameters {
        Some(Value::Object(obj)) => {
            if obj.get("type").and_then(Value::as_str) == Some("string") {
                return default_custom_tool_function_parameters();
            }
            normalize_tool_parameters(Value::Object(obj))
        }
        Some(other) => normalize_tool_parameters(other),
        None => default_custom_tool_function_parameters(),
    }
}

fn normalize_tool_parameters(mut parameters: Value) -> Value {
    let Some(obj) = parameters.as_object_mut() else {
        return parameters;
    };

    if !obj.get("required").is_some_and(Value::is_array) {
        obj.insert("required".to_string(), Value::Array(Vec::new()));
    }

    if obj
        .get("additionalProperties")
        .and_then(Value::as_object)
        .is_some_and(serde_json::Map::is_empty)
    {
        obj.insert("additionalProperties".to_string(), Value::Bool(false));
    }

    parameters
}

fn normalize_anthropic_tool_parameters(tool_name: &str, mut parameters: Value) -> Value {
    let Some(obj) = parameters.as_object_mut() else {
        return parameters;
    };

    obj.entry("type".to_string())
        .or_insert_with(|| json!("object"));
    obj.entry("properties".to_string())
        .or_insert_with(|| json!({}));

    if tool_name == "ExitPlanMode" {
        obj.insert("additionalProperties".to_string(), Value::Bool(false));
    } else if obj
        .get("additionalProperties")
        .and_then(Value::as_object)
        .is_some_and(serde_json::Map::is_empty)
    {
        obj.insert("additionalProperties".to_string(), Value::Bool(false));
    }

    normalize_tool_parameters(parameters)
}

fn normalize_wrapped_function_tool_parameters(mut tool: Value) -> Value {
    if let Some(function) = tool.get_mut("function").and_then(Value::as_object_mut) {
        let parameters = function
            .remove("parameters")
            .unwrap_or_else(default_function_tool_parameters);
        function.insert(
            "parameters".to_string(),
            normalize_tool_parameters(parameters),
        );
    }

    tool
}

fn normalize_responses_tool_shape(tool: Value) -> Option<Value> {
    let tool_type = tool.get("type").and_then(Value::as_str);
    if tool_type.is_some_and(|value| value != "function") {
        return Some(tool);
    }

    let function = tool.get("function").and_then(Value::as_object);
    let name = function
        .and_then(|obj| obj.get("name"))
        .or_else(|| tool.get("name"))?
        .as_str()?
        .trim();
    if name.is_empty() {
        return None;
    }

    let description = function
        .and_then(|obj| obj.get("description"))
        .or_else(|| tool.get("description"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let parameters = function
        .and_then(|obj| obj.get("parameters"))
        .or_else(|| tool.get("parameters"))
        .or_else(|| tool.get("input_schema"))
        .cloned()
        .unwrap_or_else(default_function_tool_parameters);
    let parameters = normalize_tool_parameters(parameters);

    Some(json!({
        "type": "function",
        "name": name,
        "description": description,
        "parameters": parameters,
    }))
}

fn normalize_chat_function_tool_shape(tool: Value) -> Option<Value> {
    let tool_type = tool.get("type").and_then(Value::as_str);
    if tool_type.is_some_and(|value| value != "function") {
        return Some(tool);
    }

    if tool.get("function").is_some() {
        return Some(normalize_wrapped_function_tool_parameters(tool));
    }

    let name = tool.get("name")?.as_str()?.trim();
    if name.is_empty() {
        return None;
    }

    let description = tool
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let parameters = tool
        .get("parameters")
        .or_else(|| tool.get("input_schema"))
        .cloned()
        .unwrap_or_else(default_function_tool_parameters);
    let parameters = normalize_tool_parameters(parameters);

    Some(json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters,
        }
    }))
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

pub(crate) fn chat_json_to_anthropic_json(chat: Value, fallback_model: &str) -> Value {
    let mut content = Vec::new();
    let mut stop_reason = None::<String>;

    if let Some(choice) = chat
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
    {
        if let Some(message) = choice.get("message") {
            let has_tool_calls = message
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|tool_calls| !tool_calls.is_empty());
            stop_reason = if has_tool_calls {
                Some("tool_use".to_string())
            } else {
                match choice.get("finish_reason").and_then(Value::as_str) {
                    Some("stop") => Some("end_turn".to_string()),
                    Some("length") => Some("max_tokens".to_string()),
                    Some("tool_calls") => Some("tool_use".to_string()),
                    Some(reason) if !reason.trim().is_empty() => Some("stop_sequence".to_string()),
                    _ => None,
                }
            };

            if let Some(reasoning) = message.get("reasoning_content").and_then(Value::as_str)
                && !reasoning.trim().is_empty()
            {
                content.push(json!({
                    "type": "thinking",
                    "thinking": reasoning,
                }));
                if let Some(signature) = message.get("signature").and_then(Value::as_str)
                    && !signature.trim().is_empty()
                    && let Some(last) = content.last_mut()
                    && let Some(obj) = last.as_object_mut()
                {
                    obj.insert(
                        "signature".to_string(),
                        Value::String(signature.to_string()),
                    );
                }
            } else if let Some(signature) = message.get("signature").and_then(Value::as_str)
                && !signature.trim().is_empty()
            {
                content.push(json!({
                    "type": "thinking",
                    "thinking": "",
                    "signature": signature,
                }));
            }

            if let Some(text) = message.get("content").and_then(Value::as_str) {
                if !text.trim().is_empty() {
                    content.push(json!({
                        "type": "text",
                        "text": text,
                    }));
                }
            } else {
                let text = message
                    .get("content")
                    .map(function_output_to_text)
                    .unwrap_or_default();
                if !text.trim().is_empty() {
                    content.push(json!({
                        "type": "text",
                        "text": text,
                    }));
                }
            }

            if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
                for tc in tool_calls {
                    let arguments = tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    content.push(json!({
                        "type": "tool_use",
                        "id": tc.get("id").and_then(Value::as_str).unwrap_or_default(),
                        "name": tc.get("function").and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or("unknown_function"),
                        "input": parse_json_or_string(arguments),
                    }));
                }
            }
        }
    }

    let mut response = json!({
        "id": chat.get("id").and_then(Value::as_str).map(ToString::to_string).unwrap_or_else(|| format!("msg_{}", Uuid::now_v7())),
        "type": "message",
        "role": "assistant",
        "model": chat.get("model").and_then(Value::as_str).unwrap_or(fallback_model),
        "content": content,
        "usage": {
            "input_tokens": chat.get("usage").and_then(|u| u.get("prompt_tokens")).and_then(Value::as_i64).unwrap_or(0),
            "output_tokens": chat.get("usage").and_then(|u| u.get("completion_tokens")).and_then(Value::as_i64).unwrap_or(0),
        }
    });
    if let Some(stop_reason) = stop_reason
        && let Some(obj) = response.as_object_mut()
    {
        obj.insert("stop_reason".to_string(), Value::String(stop_reason));
    }
    response
}

pub(crate) fn responses_json_to_anthropic_json(response: Value, fallback_model: &str) -> Value {
    let mut content = Vec::new();
    let mut stop_reason = "end_turn";

    if response.get("status").and_then(Value::as_str) == Some("incomplete") {
        stop_reason = "max_tokens";
    }

    if let Some(output) = response.get("output").and_then(Value::as_array) {
        for item in output {
            match item.get("type").and_then(Value::as_str).unwrap_or_default() {
                "reasoning" => {
                    let text = item
                        .get("summary")
                        .and_then(Value::as_array)
                        .and_then(|items| items.first())
                        .and_then(|item| item.get("text"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if !text.trim().is_empty() {
                        content.push(json!({
                            "type": "thinking",
                            "thinking": text,
                        }));
                    }
                }
                "message" => {
                    if let Some(parts) = item.get("content").and_then(Value::as_array) {
                        let text = parts
                            .iter()
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !text.trim().is_empty() {
                            content.push(json!({
                                "type": "text",
                                "text": text,
                            }));
                        }
                    }
                }
                "function_call" | "custom_tool_call" => {
                    stop_reason = "tool_use";
                    let input_value = item
                        .get("arguments")
                        .cloned()
                        .or_else(|| item.get("input").cloned())
                        .unwrap_or_else(|| json!({}));
                    content.push(json!({
                        "type": "tool_use",
                        "id": item.get("call_id").and_then(Value::as_str).or_else(|| item.get("id").and_then(Value::as_str)).unwrap_or_default(),
                        "name": item.get("name").and_then(Value::as_str).unwrap_or("unknown_function"),
                        "input": parse_json_or_string(input_value),
                    }));
                }
                _ => {}
            }
        }
    }

    json!({
        "id": response.get("id").and_then(Value::as_str).map(ToString::to_string).unwrap_or_else(|| format!("msg_{}", Uuid::now_v7())),
        "type": "message",
        "role": "assistant",
        "model": fallback_model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": response.get("usage").and_then(|u| u.get("input_tokens")).and_then(Value::as_i64).unwrap_or(0),
            "output_tokens": response.get("usage").and_then(|u| u.get("output_tokens")).and_then(Value::as_i64).unwrap_or(0),
        }
    })
}

fn parse_json_or_string(value: Value) -> Value {
    match value {
        Value::String(text) => serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text)),
        other => other,
    }
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
    tool_transform_mode: ToolTransformMode,
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
                let role = item
                    .get("role")
                    .and_then(Value::as_str)
                    .map(responses_message_role_to_chat_role)
                    .unwrap_or("user");
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
            "reasoning" => {}
            "function_call" => {
                let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
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
                let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
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

    let chat_tools = normalize_chat_tools(tools, drop_tool_types, tool_transform_mode);
    let chat_tool_choice = normalize_tool_choice(tool_choice, tool_transform_mode);

    let mut chat_request = json!({
        "model": model,
        "messages": messages,
        "stream": stream,
        "stream_options": { "include_usage": true },
        "tools": chat_tools,
        "tool_choice": chat_tool_choice,
        "parallel_tool_calls": parallel_tool_calls,
    });

    if !stream && let Some(obj) = chat_request.as_object_mut() {
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

fn responses_message_role_to_chat_role(role: &str) -> &str {
    match role {
        "developer" => "system",
        role => role,
    }
}

fn resolve_tool_output_call_id(
    item: &Value,
    pending_tool_call_ids: &mut VecDeque<String>,
) -> Result<String> {
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

fn chat_message_reasoning_text(message: &Value) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(thinking) = message.get("thinking").and_then(Value::as_str)
        && !thinking.trim().is_empty()
    {
        parts.push(thinking.trim().to_string());
    }
    if let Some(reasoning) = message.get("reasoning_content").and_then(Value::as_str)
        && !reasoning.trim().is_empty()
    {
        parts.push(reasoning.trim().to_string());
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

fn chat_tool_call_to_responses_input_item(
    tool_call: &Value,
    message_index: usize,
    tool_index: usize,
) -> Option<Value> {
    let function = tool_call.get("function")?;
    let name = function.get("name").and_then(Value::as_str)?.trim();
    if name.is_empty() {
        return None;
    }

    let call_id = tool_call
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("call_m{message_index}_t{tool_index}"));
    let arguments = function
        .get("arguments")
        .map(function_arguments_to_text)
        .unwrap_or_else(|| "{}".to_string());

    Some(json!({
        "type": "function_call",
        "call_id": call_id,
        "name": name,
        "arguments": arguments,
    }))
}

fn chat_content_item_to_input_item(item: &Value) -> Option<Value> {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();

    if matches!(item_type, "text" | "input_text" | "output_text")
        && let Some(text) = item.get("text").and_then(Value::as_str)
        && !text.trim().is_empty()
    {
        return Some(json!({
            "type": "input_text",
            "text": text,
        }));
    }

    if item_type == "image_url" {
        let image_url = item
            .get("image_url")
            .and_then(|value| {
                value.as_str().map(ToString::to_string).or_else(|| {
                    value
                        .get("url")
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                })
            })
            .or_else(|| {
                item.get("url")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            })
            .unwrap_or_default();
        if image_url.is_empty() {
            return None;
        }
        return Some(json!({
            "type": "input_image",
            "image_url": image_url,
        }));
    }

    if item_type == "input_audio" {
        let input_audio = item.get("input_audio").unwrap_or(item);
        let data = input_audio
            .get("data")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if data.is_empty() {
            return None;
        }
        return Some(json!({
            "type": "input_audio",
            "input_audio": {
                "data": data,
            }
        }));
    }

    item.get("content")
        .and_then(Value::as_str)
        .and_then(|text| {
            if text.trim().is_empty() {
                None
            } else {
                Some(json!({
                    "type": "input_text",
                    "text": text,
                }))
            }
        })
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
    if let Some(text) = request.get("text")
        && let Some(response_format) = text
            .get("format")
            .and_then(responses_text_format_to_chat_response_format)
    {
        chat_obj.insert("response_format".to_string(), response_format);
    }
    Ok(())
}

fn responses_text_format_to_chat_response_format(format: &Value) -> Option<Value> {
    let format_obj = format.as_object()?;
    let format_type = format_obj.get("type").and_then(Value::as_str)?;

    match format_type {
        "json_schema" if format_obj.contains_key("json_schema") => Some(json!({
            "type": "json_schema",
            "json_schema": format_obj.get("json_schema")?.clone(),
        })),
        "json_schema" => {
            let schema = format_obj.get("schema")?.clone();
            let name = format_obj.get("name")?.clone();
            let mut json_schema = serde_json::Map::new();
            json_schema.insert("name".to_string(), name);
            json_schema.insert("schema".to_string(), schema);

            for field in ["description", "strict"] {
                if let Some(value) = format_obj.get(field) {
                    json_schema.insert(field.to_string(), value.clone());
                }
            }

            Some(json!({
                "type": "json_schema",
                "json_schema": Value::Object(json_schema),
            }))
        }
        "json_object" => Some(json!({"type": "json_object"})),
        _ => None,
    }
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

pub(crate) fn normalize_chat_tools(
    tools: Vec<Value>,
    drop_tool_types: &HashSet<String>,
    tool_transform_mode: ToolTransformMode,
) -> Vec<Value> {
    tools
        .into_iter()
        .filter_map(|tool| {
            let tool_type = tool.get("type").and_then(Value::as_str);
            if tool_type.is_some_and(|t| drop_tool_types.contains(t)) {
                return None;
            }

            if tool_type == Some("function") {
                return normalize_chat_function_tool_shape(tool);
            }

            if tool_type.is_none() && tool.get("name").is_some() {
                return normalize_chat_function_tool_shape(tool);
            }

            if tool_type == Some("custom") {
                if tool_transform_mode == ToolTransformMode::Passthrough {
                    return Some(tool);
                }
                if let Some(function) = tool.get("function").cloned() {
                    let mut converted = tool;
                    if let Some(obj) = converted.as_object_mut() {
                        obj.insert("type".to_string(), Value::String("function".to_string()));
                        obj.insert("function".to_string(), function);
                    }
                    return Some(normalize_wrapped_function_tool_parameters(converted));
                }

                let name = tool.get("name")?.as_str()?.to_string();
                let description = tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let parameters = normalize_custom_tool_parameters(
                    tool.get("parameters")
                        .cloned()
                        .or_else(|| tool.get("input_schema").cloned()),
                );

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
                if tool_transform_mode == ToolTransformMode::Passthrough {
                    return Some(tool);
                }
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
                let parameters = normalize_tool_parameters(parameters);
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
                if tool_transform_mode == ToolTransformMode::Passthrough {
                    return Some(tool);
                }
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
                let parameters = normalize_tool_parameters(parameters);
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

pub(crate) fn normalize_tool_choice(
    tool_choice: Value,
    tool_transform_mode: ToolTransformMode,
) -> Value {
    if let Some(s) = tool_choice.as_str() {
        return Value::String(s.to_string());
    }

    let Some(obj) = tool_choice.as_object() else {
        return Value::String("auto".to_string());
    };

    let tool_type = obj.get("type").and_then(Value::as_str);

    if tool_transform_mode == ToolTransformMode::LegacyConvert
        && tool_type == Some("custom")
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

    if tool_transform_mode == ToolTransformMode::LegacyConvert
        && tool_type == Some("custom")
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

    if tool_transform_mode == ToolTransformMode::Passthrough {
        return tool_choice;
    }

    Value::String("auto".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_user_image_blocks_are_preserved() {
        let input = json!({
            "model": "claude-opus-4-7",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type":"text","text":"before"},
                        {
                            "type":"image",
                            "source":{
                                "type":"url",
                                "url":"https://example.com/cat.png"
                            }
                        },
                        {"type":"text","text":"after"}
                    ]
                }
            ]
        });

        let out = map_anthropic_messages_to_chat_request(&input, false).expect("ok");
        let messages = out["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["type"], "text");
        assert_eq!(messages[0]["content"][0]["text"], "before");
        assert_eq!(messages[0]["content"][1]["type"], "image_url");
        assert_eq!(
            messages[0]["content"][1]["image_url"]["url"],
            "https://example.com/cat.png"
        );
        assert_eq!(messages[0]["content"][2]["type"], "text");
        assert_eq!(messages[0]["content"][2]["text"], "after");
    }

    #[test]
    fn chat_response_format_maps_to_responses_text_format() {
        let chat = json!({
            "model": "gpt-4.1",
            "messages": [],
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "answer",
                    "schema": {"type":"object"}
                }
            },
            "stream": false
        });

        let out = map_chat_to_responses_request(&chat, false).expect("ok");
        assert_eq!(out["text"]["format"]["type"], "json_schema");
        assert_eq!(out["text"]["format"]["json_schema"]["name"], "answer");
        assert!(out.get("response_format").is_none());
    }

    #[test]
    fn responses_text_format_maps_to_chat_response_format_without_text_field() {
        let request = json!({
            "model": "gpt-4.1",
            "input": [],
            "text": {
                "format": {
                    "type": "json_object",
                    "json": {"type": "object"},
                    "regex": null,
                    "choice": null,
                    "grammar": null,
                    "disable_any_whitespace": false
                }
            }
        });

        let out = map_responses_to_chat_request_with_stream(
            &request,
            &HashSet::new(),
            false,
            false,
            ToolTransformMode::LegacyConvert,
        )
        .expect("ok");

        assert_eq!(out.chat_request["response_format"]["type"], "json_object");
        assert_eq!(
            out.chat_request["response_format"],
            json!({"type": "json_object"})
        );
        assert!(out.chat_request.get("text").is_none());
    }

    #[test]
    fn responses_json_schema_text_format_maps_to_chat_response_format() {
        let request = json!({
            "model": "gpt-4.1",
            "input": [],
            "text": {
                "format": {
                    "type": "json_schema",
                    "name": "codex_output_schema",
                    "schema": {
                        "type": "object",
                        "properties": {
                            "answer": {"type": "string"}
                        },
                        "required": ["answer"],
                        "additionalProperties": false
                    },
                    "strict": true
                }
            }
        });

        let out = map_responses_to_chat_request_with_stream(
            &request,
            &HashSet::new(),
            false,
            false,
            ToolTransformMode::LegacyConvert,
        )
        .expect("ok");

        assert_eq!(out.chat_request["response_format"]["type"], "json_schema");
        assert_eq!(
            out.chat_request["response_format"]["json_schema"]["name"],
            "codex_output_schema"
        );
        assert_eq!(
            out.chat_request["response_format"]["json_schema"]["schema"]["type"],
            "object"
        );
        assert_eq!(
            out.chat_request["response_format"]["json_schema"]["strict"],
            true
        );
        assert!(out.chat_request["response_format"].get("schema").is_none());
    }

    #[test]
    fn responses_nested_json_schema_text_format_drops_other_constraints() {
        let request = json!({
            "model": "gpt-4.1",
            "input": [],
            "text": {
                "format": {
                    "type": "json_schema",
                    "json_schema": {
                        "name": "codex_output_schema",
                        "schema": {"type": "object"},
                        "strict": true
                    },
                    "json": {"type": "object"},
                    "regex": null,
                    "choice": null,
                    "grammar": null,
                    "json_object": null,
                    "disable_any_whitespace": false,
                    "disable_additional_properties": false
                }
            }
        });

        let out = map_responses_to_chat_request_with_stream(
            &request,
            &HashSet::new(),
            false,
            false,
            ToolTransformMode::LegacyConvert,
        )
        .expect("ok");

        assert_eq!(
            out.chat_request["response_format"],
            json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "codex_output_schema",
                    "schema": {"type": "object"},
                    "strict": true
                }
            })
        );
    }
}
