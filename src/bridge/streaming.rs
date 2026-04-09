use anyhow::Result;
use async_stream::stream;
use axum::body::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use tracing::{debug, warn};

use crate::{
    ChatChunk, ResponsesToolCallKind, SseParser, StreamAccumulator, responses_tool_call_item,
};

pub(crate) fn passthrough_responses_stream<S>(
    upstream_stream: S,
    router_name: String,
    verbose_logging: bool,
) -> impl Stream<Item = Result<Bytes, std::convert::Infallible>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    stream! {
        let mut upstream_stream = Box::pin(upstream_stream);
        while let Some(chunk_result) = upstream_stream.next().await {
            match chunk_result {
                Ok(chunk) => {
                    if verbose_logging {
                        debug!(
                            "upstream response payload stream chunk (router={}, responses): {}",
                            router_name,
                            String::from_utf8_lossy(&chunk)
                        );
                    }
                    yield Ok(chunk)
                },
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

pub(crate) fn translate_chat_stream_to_anthropic<S>(
    upstream_stream: S,
    router_name: String,
    verbose_logging: bool,
    model: String,
) -> impl Stream<Item = Result<Bytes, std::convert::Infallible>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    stream! {
        let mut upstream_stream = Box::pin(upstream_stream);
        let mut parser = SseParser::default();
        let message_id = format!("msg_{}", uuid::Uuid::now_v7());
        let mut next_index = 0usize;
        let mut thinking_index = None;
        let mut text_index = None;
        let mut tool_blocks: BTreeMap<usize, AnthropicToolBlockState> = BTreeMap::new();
        let mut think_parser = ThinkTagParser::default();
        let mut heuristic_tool_parser = HeuristicToolParser::default();
        let mut saw_done_marker = false;
        let mut saw_terminal_finish_reason = false;
        let mut finish_reason = "end_turn".to_string();
        let mut usage = json!({
            "input_tokens": 0,
            "output_tokens": 0,
        });

        yield Ok(anthropic_sse_event(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": message_id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": model,
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": usage,
                }
            }),
        ));

        let mut process_event = |data: String| -> Vec<Bytes> {
            let mut emitted = Vec::new();

            if data == "[DONE]" {
                saw_done_marker = true;
                return emitted;
            }

            let Ok(chat_chunk) = serde_json::from_str::<ChatChunk>(&data) else {
                warn!("failed to decode upstream chat chunk for anthropic stream");
                return emitted;
            };

            if let Some(chat_usage) = chat_chunk.usage {
                usage = json!({
                    "input_tokens": chat_usage.prompt_tokens,
                    "output_tokens": chat_usage.completion_tokens,
                });
            }

            for choice in chat_chunk.choices {
                if choice.finish_reason.as_deref().is_some_and(|reason| !reason.trim().is_empty()) {
                    saw_terminal_finish_reason = true;
                }
                if let Some(reason) = choice.finish_reason.as_deref() {
                    finish_reason = map_chat_finish_reason_to_anthropic_stop_reason(reason).to_string();
                }

                let Some(delta) = choice.delta else {
                    continue;
                };

                if let Some(thinking_blocks) = delta.thinking_blocks.as_ref() {
                    if let Some(blocks) = thinking_blocks.as_array() {
                        for block in blocks {
                            match block.get("type").and_then(Value::as_str).unwrap_or_default() {
                                "thinking" => {
                                    let thinking = block
                                        .get("thinking")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default();
                                    if !thinking.is_empty() {
                                        emit_thinking_segment(
                                            &mut emitted,
                                            &mut next_index,
                                            &mut thinking_index,
                                            &mut text_index,
                                            thinking,
                                        );
                                    }
                                    if let Some(signature) = block.get("signature").and_then(Value::as_str)
                                        && !signature.is_empty()
                                    {
                                        emit_thinking_signature(
                                            &mut emitted,
                                            &mut next_index,
                                            &mut thinking_index,
                                            &mut text_index,
                                            signature,
                                        );
                                    }
                                }
                                "redacted_thinking" => {
                                    if let Some(index) = thinking_index.take() {
                                        emitted.push(anthropic_content_block_stop(index));
                                    }
                                    if let Some(index) = text_index.take() {
                                        emitted.push(anthropic_content_block_stop(index));
                                    }
                                    let block_index = next_index;
                                    next_index += 1;
                                    emitted.push(anthropic_content_block_start(
                                        block_index,
                                        "redacted_thinking",
                                        json!({
                                            "type": "redacted_thinking",
                                        }),
                                    ));
                                    emitted.push(anthropic_content_block_stop(block_index));
                                }
                                _ => {}
                            }
                        }
                    }
                } else {
                    if let Some(reasoning) = delta.reasoning_content
                        && !reasoning.is_empty()
                    {
                        emit_thinking_segment(
                            &mut emitted,
                            &mut next_index,
                            &mut thinking_index,
                            &mut text_index,
                            &reasoning,
                        );
                    }

                    if let Some(details) = delta.reasoning_details.as_ref() {
                        for reasoning in reasoning_details_texts(details) {
                            emit_thinking_segment(
                                &mut emitted,
                                &mut next_index,
                                &mut thinking_index,
                                &mut text_index,
                                &reasoning,
                            );
                        }
                    }

                    if let Some(signature) = delta.signature.as_deref()
                        && !signature.is_empty()
                    {
                        emit_thinking_signature(
                            &mut emitted,
                            &mut next_index,
                            &mut thinking_index,
                            &mut text_index,
                            signature,
                        );
                    }
                }

                if let Some(content) = delta.content
                    && !content.is_empty()
                {
                    for parsed in think_parser.feed(&content) {
                        match parsed.kind {
                            ThinkContentKind::Text => {
                                emit_text_segment(
                                    &mut emitted,
                                    &mut next_index,
                                    &mut thinking_index,
                                    &mut text_index,
                                    &mut heuristic_tool_parser,
                                    &parsed.content,
                                );
                            }
                            ThinkContentKind::Thinking => {
                                emit_thinking_segment(
                                    &mut emitted,
                                    &mut next_index,
                                    &mut thinking_index,
                                    &mut text_index,
                                    &parsed.content,
                                );
                            }
                        }
                    }
                }

                if let Some(tool_calls) = delta.tool_calls {
                    if let Some(index) = thinking_index.take() {
                        emitted.push(anthropic_content_block_stop(index));
                    }
                    if let Some(index) = text_index.take() {
                        emitted.push(anthropic_content_block_stop(index));
                    }

                    for tool_call in tool_calls {
                        let tool_index = tool_call.index.unwrap_or(tool_blocks.len());
                        let state = tool_blocks.entry(tool_index).or_default();
                        if let Some(id) = tool_call.id {
                            state.id = id;
                        }
                        if let Some(function) = tool_call.function {
                            if let Some(name) = function.name {
                                state.name = name;
                            }
                            if let Some(arguments) = function.arguments
                                && !arguments.is_empty()
                            {
                                state.arguments.push_str(&arguments);
                            }
                        }
                        if !state.started {
                            state.started = true;
                            state.block_index = next_index;
                            next_index += 1;
                            emitted.push(anthropic_content_block_start(
                                state.block_index,
                                "tool_use",
                                json!({
                                    "type": "tool_use",
                                    "id": state.id,
                                    "name": state.name,
                                    "input": {},
                                }),
                            ));
                        }
                        if !state.arguments.is_empty() {
                            emitted.push(anthropic_content_block_delta(
                                state.block_index,
                                "input_json_delta",
                                &state.arguments,
                            ));
                            state.arguments.clear();
                        }
                    }
                }
            }

            emitted
        };

        while let Some(chunk_result) = upstream_stream.next().await {
            let chunk = match chunk_result {
                Ok(chunk) => chunk,
                Err(err) => {
                    yield Ok(anthropic_sse_event(
                        "error",
                        &json!({
                            "type": "error",
                            "error": {
                                "type": "api_error",
                                "message": err.to_string(),
                            }
                        }),
                    ));
                    return;
                }
            };

            if verbose_logging {
                debug!(
                    "upstream response payload stream chunk (router={}, chat->anthropic): {}",
                    router_name,
                    String::from_utf8_lossy(&chunk)
                );
            }

            let text = String::from_utf8_lossy(&chunk);
            let events = parser.feed(&text);
            for data in events {
                for event in process_event(data) {
                    yield Ok(event);
                }
            }
        }

        if let Some(data) = parser.finish() {
            for event in process_event(data) {
                yield Ok(event);
            }
        }

        if let Some(remaining) = think_parser.flush() {
            let mut emitted = Vec::new();
            match remaining.kind {
                ThinkContentKind::Text => {
                    emit_text_segment(
                        &mut emitted,
                        &mut next_index,
                        &mut thinking_index,
                        &mut text_index,
                        &mut heuristic_tool_parser,
                        &remaining.content,
                    );
                }
                ThinkContentKind::Thinking => {
                    emit_thinking_segment(
                        &mut emitted,
                        &mut next_index,
                        &mut thinking_index,
                        &mut text_index,
                        &remaining.content,
                    );
                }
            }
            for event in emitted {
                yield Ok(event);
            }
        }

        let (heuristic_flush_text, heuristic_flush_calls) = heuristic_tool_parser.flush();
        if !heuristic_flush_text.is_empty() {
            if let Some(index) = thinking_index.take() {
                yield Ok(anthropic_content_block_stop(index));
            }
            let index = *text_index.get_or_insert_with(|| {
                let index = next_index;
                next_index += 1;
                index
            });
            if text_index == Some(index) && index + 1 == next_index {
                yield Ok(anthropic_content_block_start(
                    index,
                    "text",
                    json!({"type":"text","text":""}),
                ));
            }
            yield Ok(anthropic_content_block_delta(
                index,
                "text_delta",
                &heuristic_flush_text,
            ));
        }
        for heuristic_call in heuristic_flush_calls {
            let mut emitted = Vec::new();
            emit_heuristic_tool_call(
                &mut emitted,
                &mut next_index,
                &mut thinking_index,
                &mut text_index,
                heuristic_call,
            );
            for event in emitted {
                yield Ok(event);
            }
        }

        if !saw_done_marker && !saw_terminal_finish_reason {
            yield Ok(anthropic_sse_event(
                "error",
                &json!({
                    "type": "error",
                    "error": {
                        "type": "api_error",
                        "message": "upstream stream ended before terminal marker",
                    }
                }),
            ));
            return;
        }

        if let Some(index) = thinking_index {
            yield Ok(anthropic_content_block_stop(index));
        }
        if let Some(index) = text_index {
            yield Ok(anthropic_content_block_stop(index));
        }
        for (_, state) in tool_blocks {
            if state.started {
                yield Ok(anthropic_content_block_stop(state.block_index));
            }
        }

        yield Ok(anthropic_sse_event(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": finish_reason,
                    "stop_sequence": null,
                },
                "usage": usage,
            }),
        ));
        yield Ok(anthropic_sse_event(
            "message_stop",
            &json!({
                "type": "message_stop",
            }),
        ));
    }
}

pub(crate) fn translate_chat_stream<S>(
    upstream_stream: S,
    response_id: String,
    router_name: String,
    verbose_logging: bool,
    tool_call_kinds_by_name: HashMap<String, ResponsesToolCallKind>,
    feature_flags: crate::FeatureFlags,
) -> impl Stream<Item = Result<Bytes, std::convert::Infallible>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    stream! {
        let mut upstream_stream = Box::pin(upstream_stream);
        let mut parser = SseParser::default();
        let mut acc = StreamAccumulator::default();
        let mut assistant_item_added = false;
        let mut assistant_content_part_added = false;
        let mut reasoning_item_added = false;
        let mut saw_done_marker = false;
        let mut saw_terminal_finish_reason = false;

        yield Ok(sse_event(
            "response.created",
            &json!({
                "type": "response.created",
                "response": {
                    "id": response_id.clone(),
                }
            }),
        ));
        if feature_flags.enable_extended_stream_events {
            yield Ok(sse_event(
                "response.in_progress",
                &json!({
                    "type": "response.in_progress",
                    "response": {
                        "id": response_id.clone(),
                    }
                }),
            ));
        }

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

            if verbose_logging {
                debug!(
                    "upstream response payload stream chunk (router={}, chat): {}",
                    router_name,
                    String::from_utf8_lossy(&chunk)
                );
            }

            let text = String::from_utf8_lossy(&chunk);
            let events = parser.feed(&text);
            for data in events {
                if data == "[DONE]" {
                    saw_done_marker = true;
                    continue;
                }

                match serde_json::from_str::<ChatChunk>(&data) {
                    Ok(chat_chunk) => {
                        if let Some(usage) = chat_chunk.usage.clone() {
                            acc.usage = Some(usage);
                        }

                        for choice in chat_chunk.choices {
                            if choice.finish_reason.as_deref().is_some_and(|reason| !reason.trim().is_empty()) {
                                saw_terminal_finish_reason = true;
                            }

                            if let Some(delta) = choice.delta {
                                if feature_flags.enable_reasoning_stream_events
                                    && let Some(reasoning) = delta.reasoning_content
                                    && !reasoning.is_empty()
                                {
                                    if !reasoning_item_added {
                                        yield Ok(sse_event(
                                            "response.output_item.added",
                                            &json!({
                                                "type": "response.output_item.added",
                                                "output_index": 0,
                                                "item": {
                                                    "type": "reasoning",
                                                    "id": reasoning_item_id(&response_id),
                                                    "summary": [
                                                        {
                                                            "type": "summary_text",
                                                            "text": "",
                                                        }
                                                    ]
                                                }
                                            }),
                                        ));
                                        reasoning_item_added = true;
                                    }

                                    acc.reasoning_text.push_str(&reasoning);
                                    yield Ok(sse_event(
                                        "response.reasoning_summary_text.delta",
                                        &json!({
                                            "type": "response.reasoning_summary_text.delta",
                                            "item_id": reasoning_item_id(&response_id),
                                            "output_index": 0,
                                            "summary_index": 0,
                                            "delta": reasoning,
                                        }),
                                    ));
                                }

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
                                    if feature_flags.enable_extended_stream_events
                                        && !assistant_content_part_added
                                    {
                                        yield Ok(sse_event(
                                            "response.content_part.added",
                                            &json!({
                                                "type": "response.content_part.added",
                                                "item_id": response_id.clone(),
                                                "output_index": 0,
                                                "content_index": 0,
                                                "part": {
                                                    "type": "output_text",
                                                    "text": "",
                                                }
                                            }),
                                        ));
                                        assistant_content_part_added = true;
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
                                        let (
                                            call_id,
                                            name,
                                            all_arguments,
                                            delta_arguments,
                                            emit_added,
                                        ) = {
                                            let entry = acc.tool_calls.entry(index).or_default();

                                            if let Some(id) = tool_call.id {
                                                entry.id = Some(id);
                                            }

                                            let mut delta_arguments = None;
                                            if let Some(function) = tool_call.function {
                                                if let Some(name) = function.name {
                                                    entry.name = Some(name);
                                                }
                                                if let Some(arguments) = function.arguments {
                                                    if !arguments.is_empty() {
                                                        entry.arguments.push_str(&arguments);
                                                        delta_arguments = Some(arguments);
                                                    }
                                                }
                                            }

                                            let emit_added = !entry.added_emitted;
                                            if emit_added {
                                                entry.added_emitted = true;
                                            }

                                            (
                                                entry.id.clone().unwrap_or_else(|| {
                                                    deterministic_tool_call_id(&response_id, index)
                                                }),
                                                entry
                                                    .name
                                                    .clone()
                                                    .unwrap_or_else(|| "unknown_function".to_string()),
                                                entry.arguments.clone(),
                                                delta_arguments,
                                                emit_added,
                                            )
                                        };

                                        if emit_added {
                                            let item = responses_tool_call_item(
                                                &name,
                                                &all_arguments,
                                                &call_id,
                                                &tool_call_kinds_by_name,
                                            );
                                            yield Ok(sse_event(
                                                "response.output_item.added",
                                                &json!({
                                                    "type": "response.output_item.added",
                                                    "output_index": tool_output_index(index),
                                                    "item": item,
                                                }),
                                            ));
                                        }

                                        if feature_flags.enable_tool_argument_stream_events
                                            && let Some(delta_arguments) = delta_arguments
                                        {
                                            yield Ok(sse_event(
                                                "response.function_call_arguments.delta",
                                                &json!({
                                                    "type": "response.function_call_arguments.delta",
                                                    "item_id": call_id,
                                                    "output_index": tool_output_index(index),
                                                    "delta": delta_arguments,
                                                }),
                                            ));
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

        if let Some(data) = parser.finish() {
            if data == "[DONE]" {
                saw_done_marker = true;
            } else {
                warn!("bridge received trailing SSE payload: {data}");
            }
        }

        if !acc.assistant_text.is_empty() {
            if feature_flags.enable_extended_stream_events {
                yield Ok(sse_event(
                    "response.output_text.done",
                    &json!({
                        "type": "response.output_text.done",
                        "item_id": response_id.clone(),
                        "output_index": 0,
                        "content_index": 0,
                        "text": acc.assistant_text,
                    }),
                ));

                yield Ok(sse_event(
                    "response.content_part.done",
                    &json!({
                        "type": "response.content_part.done",
                        "item_id": response_id.clone(),
                        "output_index": 0,
                        "content_index": 0,
                        "part": {
                            "type": "output_text",
                            "text": acc.assistant_text,
                        }
                    }),
                ));
            }

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

        if feature_flags.enable_reasoning_stream_events && !acc.reasoning_text.is_empty() {
            yield Ok(sse_event(
                "response.reasoning_summary_text.done",
                &json!({
                    "type": "response.reasoning_summary_text.done",
                    "item_id": reasoning_item_id(&response_id),
                    "output_index": 0,
                    "summary_index": 0,
                    "text": acc.reasoning_text,
                }),
            ));
            yield Ok(sse_event(
                "response.output_item.done",
                &json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": {
                        "type": "reasoning",
                        "id": reasoning_item_id(&response_id),
                        "summary": [{
                            "type": "summary_text",
                            "text": acc.reasoning_text,
                        }]
                    }
                }),
            ));
        }

        for (index, tool_call) in acc.tool_calls {
            let call_id = tool_call.id.unwrap_or_else(|| {
                deterministic_tool_call_id(&response_id, index)
            });
            let name = tool_call
                .name
                .unwrap_or_else(|| "unknown_function".to_string());
            let item = responses_tool_call_item(
                &name,
                &tool_call.arguments,
                &call_id,
                &tool_call_kinds_by_name,
            );
            let item_for_done = item.clone();

            if !tool_call.added_emitted {
                yield Ok(sse_event(
                    "response.output_item.added",
                    &json!({
                        "type": "response.output_item.added",
                        "output_index": tool_output_index(index),
                        "item": item,
                    }),
                ));
            }

            if feature_flags.enable_tool_argument_stream_events {
                yield Ok(sse_event(
                    "response.function_call_arguments.done",
                    &json!({
                        "type": "response.function_call_arguments.done",
                        "item_id": call_id,
                        "output_index": tool_output_index(index),
                        "arguments": tool_call.arguments,
                    }),
                ));
            }

            yield Ok(sse_event(
                "response.output_item.done",
                &json!({
                    "type": "response.output_item.done",
                    "output_index": tool_output_index(index),
                    "item": item_for_done,
                }),
            ));
        }

        if !saw_done_marker && !saw_terminal_finish_reason {
            yield Ok(sse_event(
                "response.failed",
                &json!({
                    "type": "response.failed",
                    "response": {
                        "id": response_id.clone(),
                        "error": {
                            "code": "upstream_stream_incomplete",
                            "message": "upstream stream ended before terminal marker",
                        }
                    }
                }),
            ));
            return;
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

fn deterministic_tool_call_id(response_id: &str, index: usize) -> String {
    format!("call_{}_{}", response_id, index)
}

fn reasoning_item_id(response_id: &str) -> String {
    format!("rs_{}", response_id)
}

fn tool_output_index(index: usize) -> usize {
    index + 1
}

pub(crate) fn sse_event(event_name: &str, payload: &Value) -> Bytes {
    let json_payload = serde_json::to_string(payload).unwrap_or_else(|_| {
        "{\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"internal serialization error\"}}}".to_string()
    });
    Bytes::from(format!("event: {event_name}\ndata: {json_payload}\n\n"))
}

pub(crate) fn anthropic_sse_event(event_name: &str, payload: &Value) -> Bytes {
    let json_payload = serde_json::to_string(payload).unwrap_or_else(|_| {
        "{\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"internal serialization error\"}}".to_string()
    });
    Bytes::from(format!("event: {event_name}\ndata: {json_payload}\n\n"))
}

fn anthropic_content_block_start(index: usize, _block_type: &str, content_block: Value) -> Bytes {
    anthropic_sse_event(
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": index,
            "content_block": content_block,
        }),
    )
}

fn anthropic_content_block_delta(index: usize, delta_type: &str, value: &str) -> Bytes {
    let delta = match delta_type {
        "thinking_delta" => json!({"type":"thinking_delta","thinking": value}),
        "signature_delta" => json!({"type":"signature_delta","signature": value}),
        "input_json_delta" => json!({"type":"input_json_delta","partial_json": value}),
        _ => json!({"type":"text_delta","text": value}),
    };
    anthropic_sse_event(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": index,
            "delta": delta,
        }),
    )
}

fn anthropic_content_block_stop(index: usize) -> Bytes {
    anthropic_sse_event(
        "content_block_stop",
        &json!({
            "type": "content_block_stop",
            "index": index,
        }),
    )
}

#[derive(Default)]
struct AnthropicToolBlockState {
    block_index: usize,
    id: String,
    name: String,
    arguments: String,
    started: bool,
}

fn map_chat_finish_reason_to_anthropic_stop_reason(reason: &str) -> &'static str {
    match reason {
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        _ => "end_turn",
    }
}

#[allow(dead_code)]
fn split_think_tagged_text_segments(text: &str) -> Option<Vec<(bool, String)>> {
    if !text.contains("<think>")
        && !text.contains("</think>")
        && !text.contains("<thinking>")
        && !text.contains("</thinking>")
    {
        return None;
    }

    let mut segments = Vec::new();
    let mut rest = text;

    loop {
        let open_pos = match next_think_open(rest) {
            Some(pos) => pos,
            None => {
                if !rest.is_empty() {
                    segments.push((false, rest.to_string()));
                }
                break;
            }
        };

        let (prefix, after_open) = rest.split_at(open_pos);
        if !prefix.is_empty() {
            segments.push((false, prefix.to_string()));
        }

        let after_open = &after_open[think_open_len(after_open)..];
        let Some(close_pos) = next_think_close(after_open) else {
            if !after_open.is_empty() {
                segments.push((false, after_open.to_string()));
            }
            break;
        };

        let (thinking, after_close) = after_open.split_at(close_pos);
        if !thinking.is_empty() {
            segments.push((true, thinking.to_string()));
        }
        rest = &after_close[think_close_len(after_close)..];
    }

    Some(segments)
}

fn reasoning_details_texts(value: &Value) -> Vec<String> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };

    items
        .iter()
        .filter_map(|item| {
            item.get("text")
                .and_then(Value::as_str)
                .or_else(|| item.get("content").and_then(Value::as_str))
        })
        .filter(|text| !text.trim().is_empty())
        .map(ToString::to_string)
        .collect()
}

#[derive(Debug, Default)]
struct ThinkTagParser {
    buffer: String,
    in_think_tag: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThinkContentKind {
    Text,
    Thinking,
}

#[derive(Debug, Clone)]
struct ThinkContentChunk {
    kind: ThinkContentKind,
    content: String,
}

impl ThinkTagParser {
    const OPEN_TAG: &'static str = "<think>";
    const CLOSE_TAG: &'static str = "</think>";
    const ALT_OPEN_TAG: &'static str = "<thinking>";
    const ALT_CLOSE_TAG: &'static str = "</thinking>";

    fn feed(&mut self, content: &str) -> Vec<ThinkContentChunk> {
        self.buffer.push_str(content);
        let mut chunks = Vec::new();

        loop {
            let prev_len = self.buffer.len();
            let chunk = if self.in_think_tag {
                self.parse_inside_think()
            } else {
                self.parse_outside_think()
            };

            if let Some(chunk) = chunk {
                chunks.push(chunk);
            } else if self.buffer.len() == prev_len {
                break;
            }
        }

        chunks
    }

    fn parse_outside_think(&mut self) -> Option<ThinkContentChunk> {
        let think_start = next_think_open(&self.buffer);
        let orphan_close = next_think_close(&self.buffer);

        if let Some(close_pos) = orphan_close
            && think_start.map_or(true, |start| close_pos < start)
        {
            let prefix = self.buffer[..close_pos].to_string();
            self.buffer.drain(..close_pos + think_close_len(&self.buffer[close_pos..]));
            if prefix.is_empty() {
                return None;
            }
            return Some(ThinkContentChunk {
                kind: ThinkContentKind::Text,
                content: prefix,
            });
        }

        if let Some(open_pos) = think_start {
            let prefix = self.buffer[..open_pos].to_string();
            let open_len = think_open_len(&self.buffer[open_pos..]);
            self.buffer.drain(..open_pos + open_len);
            self.in_think_tag = true;
            if prefix.is_empty() {
                None
            } else {
                Some(ThinkContentChunk {
                    kind: ThinkContentKind::Text,
                    content: prefix,
                })
            }
        } else if let Some(last_bracket) = self.buffer.rfind('<') {
            let potential = &self.buffer[last_bracket..];
            if Self::OPEN_TAG.starts_with(potential) && potential.len() < Self::OPEN_TAG.len()
                || Self::ALT_OPEN_TAG.starts_with(potential)
                    && potential.len() < Self::ALT_OPEN_TAG.len()
                || Self::CLOSE_TAG.starts_with(potential) && potential.len() < Self::CLOSE_TAG.len()
                || Self::ALT_CLOSE_TAG.starts_with(potential)
                    && potential.len() < Self::ALT_CLOSE_TAG.len()
            {
                let prefix = self.buffer[..last_bracket].to_string();
                self.buffer.drain(..last_bracket);
                if prefix.is_empty() {
                    return None;
                }
                return Some(ThinkContentChunk {
                    kind: ThinkContentKind::Text,
                    content: prefix,
                });
            }
            self.flush_text_like(ThinkContentKind::Text)
        } else {
            self.flush_text_like(ThinkContentKind::Text)
        }
    }

    fn parse_inside_think(&mut self) -> Option<ThinkContentChunk> {
        if let Some(close_pos) = next_think_close(&self.buffer) {
            let close_len = think_close_len(&self.buffer[close_pos..]);
            let thinking = self.buffer[..close_pos].to_string();
            self.buffer.drain(..close_pos + close_len);
            self.in_think_tag = false;
            if thinking.is_empty() {
                None
            } else {
                Some(ThinkContentChunk {
                    kind: ThinkContentKind::Thinking,
                    content: thinking,
                })
            }
        } else if let Some(last_bracket) = self.buffer.rfind('<') {
            let potential = &self.buffer[last_bracket..];
            if Self::CLOSE_TAG.starts_with(potential) && potential.len() < Self::CLOSE_TAG.len()
                || Self::ALT_CLOSE_TAG.starts_with(potential)
                    && potential.len() < Self::ALT_CLOSE_TAG.len()
            {
                let emit = self.buffer[..last_bracket].to_string();
                self.buffer.drain(..last_bracket);
                if emit.is_empty() {
                    return None;
                }
                return Some(ThinkContentChunk {
                    kind: ThinkContentKind::Thinking,
                    content: emit,
                });
            }
            self.flush_text_like(ThinkContentKind::Thinking)
        } else {
            self.flush_text_like(ThinkContentKind::Thinking)
        }
    }

    fn flush_text_like(&mut self, kind: ThinkContentKind) -> Option<ThinkContentChunk> {
        if self.buffer.is_empty() {
            return None;
        }
        Some(ThinkContentChunk {
            kind,
            content: std::mem::take(&mut self.buffer),
        })
    }

    fn flush(&mut self) -> Option<ThinkContentChunk> {
        if self.buffer.is_empty() {
            return None;
        }
        let kind = if self.in_think_tag {
            ThinkContentKind::Thinking
        } else {
            ThinkContentKind::Text
        };
        self.in_think_tag = false;
        Some(ThinkContentChunk {
            kind,
            content: std::mem::take(&mut self.buffer),
        })
    }
}

fn emit_text_segment(
    emitted: &mut Vec<Bytes>,
    next_index: &mut usize,
    thinking_index: &mut Option<usize>,
    text_index: &mut Option<usize>,
    heuristic_tool_parser: &mut HeuristicToolParser,
    segment: &str,
) {
    if segment.is_empty() {
        return;
    }
    if let Some(index) = thinking_index.take() {
        emitted.push(anthropic_content_block_stop(index));
    }
    let index = *text_index.get_or_insert_with(|| {
        let index = *next_index;
        *next_index += 1;
        index
    });
    if matches!(*text_index, Some(current) if current == index) && index + 1 == *next_index {
        emitted.push(anthropic_content_block_start(
            index,
            "text",
            json!({"type":"text","text":""}),
        ));
    }
    let (filtered_text, tool_calls) = heuristic_tool_parser.feed(segment);
    if !filtered_text.is_empty() {
        emitted.push(anthropic_content_block_delta(
            index,
            "text_delta",
            &filtered_text,
        ));
    }
    for heuristic_call in tool_calls {
        emit_heuristic_tool_call(
            emitted,
            next_index,
            thinking_index,
            text_index,
            heuristic_call,
        );
    }
}

fn emit_thinking_segment(
    emitted: &mut Vec<Bytes>,
    next_index: &mut usize,
    thinking_index: &mut Option<usize>,
    text_index: &mut Option<usize>,
    segment: &str,
) {
    if segment.is_empty() {
        return;
    }
    if let Some(index) = text_index.take() {
        emitted.push(anthropic_content_block_stop(index));
    }
    let index = *thinking_index.get_or_insert_with(|| {
        let index = *next_index;
        *next_index += 1;
        index
    });
    if matches!(*thinking_index, Some(current) if current == index) && index + 1 == *next_index {
        emitted.push(anthropic_content_block_start(
            index,
            "thinking",
            json!({"type":"thinking","thinking":""}),
        ));
    }
    emitted.push(anthropic_content_block_delta(
        index,
        "thinking_delta",
        segment,
    ));
}

fn emit_thinking_signature(
    emitted: &mut Vec<Bytes>,
    next_index: &mut usize,
    thinking_index: &mut Option<usize>,
    text_index: &mut Option<usize>,
    signature: &str,
) {
    if signature.is_empty() {
        return;
    }
    if let Some(index) = text_index.take() {
        emitted.push(anthropic_content_block_stop(index));
    }
    let index = *thinking_index.get_or_insert_with(|| {
        let index = *next_index;
        *next_index += 1;
        index
    });
    if matches!(*thinking_index, Some(current) if current == index) && index + 1 == *next_index {
        emitted.push(anthropic_content_block_start(
            index,
            "thinking",
            json!({"type":"thinking","thinking":""}),
        ));
    }
    emitted.push(anthropic_content_block_delta(
        index,
        "signature_delta",
        signature,
    ));
}

#[derive(Debug, Default)]
struct HeuristicToolParser {
    state: HeuristicToolParserState,
    buffer: String,
    current_tool_id: Option<String>,
    current_function_name: Option<String>,
    current_parameters: BTreeMap<String, String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum HeuristicToolParserState {
    #[default]
    Text,
    MatchingFunction,
    ParsingParameters,
}

#[derive(Debug, Clone)]
struct HeuristicToolCall {
    id: String,
    name: String,
    input: Value,
}

impl HeuristicToolParser {
    fn feed(&mut self, text: &str) -> (String, Vec<HeuristicToolCall>) {
        self.buffer.push_str(text);
        let mut emitted_text = String::new();
        let mut detected = Vec::new();

        loop {
            match self.state {
                HeuristicToolParserState::Text => {
                    let Some(bullet_pos) = self.buffer.find('●') else {
                        emitted_text.push_str(&self.buffer);
                        self.buffer.clear();
                        break;
                    };

                    emitted_text.push_str(&self.buffer[..bullet_pos]);
                    self.buffer.drain(..bullet_pos);
                    self.state = HeuristicToolParserState::MatchingFunction;
                }
                HeuristicToolParserState::MatchingFunction => {
                    let Some(function_pos) = self.buffer.find("<function=") else {
                        if self.buffer.len() > 96 {
                            self.buffer.drain(..'●'.len_utf8());
                            self.state = HeuristicToolParserState::Text;
                        }
                        break;
                    };

                    if function_pos > 0 {
                        self.buffer.drain(..function_pos);
                        continue;
                    }

                    let Some(close_pos) = self.buffer.find('>') else {
                        break;
                    };
                    if close_pos <= "<function=".len() {
                        break;
                    }

                    let name = self.buffer["<function=".len()..close_pos].trim();
                    if name.is_empty() {
                        emitted_text.push_str(&self.buffer[..close_pos + 1]);
                        self.buffer.drain(..close_pos + 1);
                        self.state = HeuristicToolParserState::Text;
                        continue;
                    }

                    self.current_tool_id = Some(format!("toolu_heuristic_{}", uuid::Uuid::now_v7()));
                    self.current_function_name = Some(name.to_string());
                    self.current_parameters.clear();
                    self.buffer.drain(..close_pos + 1);
                    self.state = HeuristicToolParserState::ParsingParameters;
                }
                HeuristicToolParserState::ParsingParameters => {
                    while let Some(param_start) = self.buffer.find("<parameter=") {
                        if param_start > 0 {
                            emitted_text.push_str(&self.buffer[..param_start]);
                            self.buffer.drain(..param_start);
                        }

                        let Some(param_close) = self.buffer.find('>') else {
                            break;
                        };
                        if param_close <= "<parameter=".len() {
                            break;
                        }
                        let key = self.buffer["<parameter=".len()..param_close].trim();
                        if key.is_empty() {
                            break;
                        }

                        let end_tag = "</parameter>";
                        let Some(param_end) = self.buffer[param_close + 1..].find(end_tag) else {
                            break;
                        };

                        let value_start = param_close + 1;
                        let value_end = value_start + param_end;
                        let value = self.buffer[value_start..value_end].to_string();
                        self.current_parameters.insert(key.to_string(), value);
                        self.buffer.drain(..value_end + end_tag.len());
                    }

                    if self.current_function_name.is_none() {
                        break;
                    }

                    let should_emit = self.buffer.starts_with('●')
                        || (!self.buffer.is_empty()
                            && !self.buffer.starts_with("<parameter=")
                            && !self.buffer.starts_with("</parameter>"));

                    if should_emit {
                        detected.push(self.finish_tool_call());
                        self.state = HeuristicToolParserState::Text;
                        break;
                    }

                    break;
                }
            }
        }

        (emitted_text, detected)
    }

    fn flush(&mut self) -> (String, Vec<HeuristicToolCall>) {
        let mut emitted_text = String::new();
        let mut detected = Vec::new();
        match self.state {
            HeuristicToolParserState::Text => {
                emitted_text.push_str(&self.buffer);
            }
            HeuristicToolParserState::MatchingFunction => {
                emitted_text.push_str(&self.buffer);
            }
            HeuristicToolParserState::ParsingParameters => {
                if self.current_function_name.is_some() {
                    detected.push(self.finish_tool_call());
                } else {
                    emitted_text.push_str(&self.buffer);
                }
            }
        }
        self.buffer.clear();
        self.state = HeuristicToolParserState::Text;
        (emitted_text, detected)
    }

    fn finish_tool_call(&mut self) -> HeuristicToolCall {
        HeuristicToolCall {
            id: self.current_tool_id.take().unwrap_or_else(|| {
                format!("toolu_heuristic_{}", uuid::Uuid::now_v7())
            }),
            name: self
                .current_function_name
                .take()
                .unwrap_or_else(|| "unknown_function".to_string()),
            input: serde_json::to_value(&self.current_parameters).unwrap_or_else(|_| json!({})),
        }
    }
}

fn emit_heuristic_tool_call(
    emitted: &mut Vec<Bytes>,
    next_index: &mut usize,
    thinking_index: &mut Option<usize>,
    text_index: &mut Option<usize>,
    call: HeuristicToolCall,
) {
    if let Some(index) = thinking_index.take() {
        emitted.push(anthropic_content_block_stop(index));
    }
    if let Some(index) = text_index.take() {
        emitted.push(anthropic_content_block_stop(index));
    }

    let block_index = *next_index;
    *next_index += 1;
    emitted.push(anthropic_content_block_start(
        block_index,
        "tool_use",
        json!({
            "type": "tool_use",
            "id": call.id,
            "name": call.name,
            "input": {},
        }),
    ));
    emitted.push(anthropic_content_block_delta(
        block_index,
        "input_json_delta",
        &serde_json::to_string(&call.input).unwrap_or_else(|_| "{}".to_string()),
    ));
    emitted.push(anthropic_content_block_stop(block_index));
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

impl SseParser {
    pub(crate) fn feed(&mut self, chunk: &str) -> Vec<String> {
        self.feed_with_event_names(chunk)
            .into_iter()
            .map(|(_, data)| data)
            .collect()
    }

    pub(crate) fn feed_with_event_names(&mut self, chunk: &str) -> Vec<(Option<String>, String)> {
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
                    events.push((
                        self.current_event_name.take(),
                        self.current_data_lines.join("\n"),
                    ));
                    self.current_data_lines.clear();
                }
                continue;
            }

            if let Some(rest) = line.strip_prefix("event:") {
                self.current_event_name = Some(rest.trim().to_string());
                continue;
            }

            if let Some(rest) = line.strip_prefix("data:") {
                let data = rest.strip_prefix(' ').unwrap_or(rest).to_string();
                self.current_data_lines.push(data);
            }
        }

        events
    }

    pub(crate) fn finish(&mut self) -> Option<String> {
        if !self.buffer.is_empty() {
            let mut line = std::mem::take(&mut self.buffer);

            if line.ends_with('\r') {
                line.pop();
            }

            if let Some(rest) = line.strip_prefix("event:") {
                self.current_event_name = Some(rest.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("data:") {
                let data = rest.strip_prefix(' ').unwrap_or(rest).to_string();
                self.current_data_lines.push(data);
            }
        }

        if self.current_data_lines.is_empty() {
            None
        } else {
            self.current_event_name.take();
            Some(std::mem::take(&mut self.current_data_lines).join("\n"))
        }
    }
}
