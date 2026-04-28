use anyhow::Result;
use async_stream::stream;
use axum::body::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
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
    allowed_tool_names: HashSet<String>,
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
        let mut thinking_started = false;
        let mut text_started = false;
        let mut tool_blocks: BTreeMap<usize, AnthropicToolBlockState> = BTreeMap::new();
        let mut think_parser = ThinkTagParser::default();
        let mut heuristic_tool_parser = HeuristicToolParser::new(allowed_tool_names);
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
                                            &mut thinking_started,
                                            &mut text_index,
                                            &mut text_started,
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
                                            &mut thinking_started,
                                            &mut text_index,
                                            &mut text_started,
                                            signature,
                                        );
                                    }
                                }
                                "redacted_thinking" => {
                                    if let Some(index) = thinking_index.take() {
                                        emitted.push(anthropic_content_block_stop(index));
                                        thinking_started = false;
                                    }
                                    if let Some(index) = text_index.take() {
                                        emitted.push(anthropic_content_block_stop(index));
                                        text_started = false;
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
                            &mut thinking_started,
                            &mut text_index,
                            &mut text_started,
                            &reasoning,
                        );
                    }

                    if let Some(details) = delta.reasoning_details.as_ref() {
                        for reasoning in reasoning_details_texts(details) {
                            emit_thinking_segment(
                                &mut emitted,
                                &mut next_index,
                                &mut thinking_index,
                                &mut thinking_started,
                                &mut text_index,
                                &mut text_started,
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
                            &mut thinking_started,
                            &mut text_index,
                            &mut text_started,
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
                                if emit_text_segment(
                                    &mut emitted,
                                    &mut next_index,
                                    &mut thinking_index,
                                    &mut thinking_started,
                                    &mut text_index,
                                    &mut text_started,
                                    &mut heuristic_tool_parser,
                                    &parsed.content,
                                ) {
                                    finish_reason = "tool_use".to_string();
                                }
                            }
                            ThinkContentKind::Thinking => {
                                emit_thinking_segment(
                                    &mut emitted,
                                    &mut next_index,
                                    &mut thinking_index,
                                    &mut thinking_started,
                                    &mut text_index,
                                    &mut text_started,
                                    &parsed.content,
                                );
                            }
                        }
                    }
                }

                if let Some(tool_calls) = delta.tool_calls {
                    finish_reason = "tool_use".to_string();
                    if let Some(index) = thinking_index.take() {
                        emitted.push(anthropic_content_block_stop(index));
                        thinking_started = false;
                    }
                    if let Some(index) = text_index.take() {
                        emitted.push(anthropic_content_block_stop(index));
                        text_started = false;
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
                    if emit_text_segment(
                        &mut emitted,
                        &mut next_index,
                        &mut thinking_index,
                        &mut thinking_started,
                        &mut text_index,
                        &mut text_started,
                        &mut heuristic_tool_parser,
                        &remaining.content,
                    ) {
                        finish_reason = "tool_use".to_string();
                    }
                }
                ThinkContentKind::Thinking => {
                    emit_thinking_segment(
                        &mut emitted,
                        &mut next_index,
                        &mut thinking_index,
                        &mut thinking_started,
                        &mut text_index,
                        &mut text_started,
                        &remaining.content,
                    );
                }
            }
            for event in emitted {
                yield Ok(event);
            }
        }

        let mut emitted = Vec::new();
        if emit_heuristic_fragments(
            &mut emitted,
            &mut next_index,
            &mut thinking_index,
            &mut thinking_started,
            &mut text_index,
            &mut text_started,
            heuristic_tool_parser.flush(),
        ) {
            finish_reason = "tool_use".to_string();
        }
        for event in emitted {
            yield Ok(event);
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
            self.buffer
                .drain(..close_pos + think_close_len(&self.buffer[close_pos..]));
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
    thinking_started: &mut bool,
    text_index: &mut Option<usize>,
    text_started: &mut bool,
    heuristic_tool_parser: &mut HeuristicToolParser,
    segment: &str,
) -> bool {
    if segment.is_empty() {
        return false;
    }
    if let Some(index) = thinking_index.take() {
        emitted.push(anthropic_content_block_stop(index));
        *thinking_started = false;
    }
    let fragments = heuristic_tool_parser.feed(segment);
    if fragments.is_empty() {
        return false;
    }
    emit_heuristic_fragments(
        emitted,
        next_index,
        thinking_index,
        thinking_started,
        text_index,
        text_started,
        fragments,
    )
}

fn emit_heuristic_fragments(
    emitted: &mut Vec<Bytes>,
    next_index: &mut usize,
    thinking_index: &mut Option<usize>,
    thinking_started: &mut bool,
    text_index: &mut Option<usize>,
    text_started: &mut bool,
    fragments: Vec<HeuristicFragment>,
) -> bool {
    let mut emitted_tool_call = false;
    for fragment in fragments {
        match fragment {
            HeuristicFragment::Text(text) => {
                if text.is_empty() {
                    continue;
                }
                let index = *text_index.get_or_insert_with(|| {
                    let index = *next_index;
                    *next_index += 1;
                    index
                });
                if !*text_started {
                    emitted.push(anthropic_content_block_start(
                        index,
                        "text",
                        json!({"type":"text","text":""}),
                    ));
                    *text_started = true;
                }
                emitted.push(anthropic_content_block_delta(index, "text_delta", &text));
            }
            HeuristicFragment::ToolCall(call) => {
                emitted_tool_call = true;
                emit_heuristic_tool_call(
                    emitted,
                    next_index,
                    thinking_index,
                    thinking_started,
                    text_index,
                    text_started,
                    call,
                );
            }
        }
    }
    emitted_tool_call
}

fn emit_thinking_segment(
    emitted: &mut Vec<Bytes>,
    next_index: &mut usize,
    thinking_index: &mut Option<usize>,
    thinking_started: &mut bool,
    text_index: &mut Option<usize>,
    text_started: &mut bool,
    segment: &str,
) {
    if segment.is_empty() {
        return;
    }
    if let Some(index) = text_index.take() {
        emitted.push(anthropic_content_block_stop(index));
        *text_started = false;
    }
    let index = *thinking_index.get_or_insert_with(|| {
        let index = *next_index;
        *next_index += 1;
        index
    });
    if !*thinking_started {
        emitted.push(anthropic_content_block_start(
            index,
            "thinking",
            json!({"type":"thinking","thinking":""}),
        ));
        *thinking_started = true;
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
    thinking_started: &mut bool,
    text_index: &mut Option<usize>,
    text_started: &mut bool,
    signature: &str,
) {
    if signature.is_empty() {
        return;
    }
    if let Some(index) = text_index.take() {
        emitted.push(anthropic_content_block_stop(index));
        *text_started = false;
    }
    let index = *thinking_index.get_or_insert_with(|| {
        let index = *next_index;
        *next_index += 1;
        index
    });
    if !*thinking_started {
        emitted.push(anthropic_content_block_start(
            index,
            "thinking",
            json!({"type":"thinking","thinking":""}),
        ));
        *thinking_started = true;
    }
    emitted.push(anthropic_content_block_delta(
        index,
        "signature_delta",
        signature,
    ));
}

struct HeuristicToolParser {
    buffer: String,
    allowed_tool_names: HashSet<String>,
}

#[derive(Debug, Clone)]
struct HeuristicToolCall {
    id: String,
    name: String,
    input: Value,
}

enum HeuristicFragment {
    Text(String),
    ToolCall(HeuristicToolCall),
}

impl HeuristicToolParser {
    const CONTROL_TOKEN_START: &'static str = "<|";
    const CONTROL_TOKEN_END: &'static str = "|>";
    const SPECIAL_WRAPPER_NAMES: [&'static str; 2] = ["request_user_input", "AskUserQuestion"];

    fn new(allowed_tool_names: HashSet<String>) -> Self {
        Self {
            buffer: String::new(),
            allowed_tool_names,
        }
    }

    fn feed(&mut self, text: &str) -> Vec<HeuristicFragment> {
        self.buffer.push_str(text);
        self.strip_control_tokens();
        let mut emitted = Vec::new();

        loop {
            if self.buffer.is_empty() {
                break;
            }

            if let Some(candidate_pos) = self.find_candidate_start() {
                if candidate_pos > 0 {
                    let prefix = self.buffer[..candidate_pos].to_string();
                    if !prefix.is_empty() {
                        emitted.push(HeuristicFragment::Text(prefix));
                    }
                    self.buffer.drain(..candidate_pos);
                    continue;
                }

                if let Some((calls, consumed)) =
                    try_parse_python_tool_call_prefix(&self.buffer, &self.allowed_tool_names)
                {
                    emitted.extend(calls.into_iter().map(HeuristicFragment::ToolCall));
                    self.buffer.drain(..consumed);
                    continue;
                }

                if self.looks_like_bare_tool_prefix() {
                    break;
                }
            } else {
                if self.looks_like_bare_tool_prefix() {
                    break;
                }
                if !self.buffer.is_empty() {
                    emitted.push(HeuristicFragment::Text(std::mem::take(&mut self.buffer)));
                }
                break;
            }

            if let Some(first_char) = self.buffer.chars().next() {
                emitted.push(HeuristicFragment::Text(first_char.to_string()));
                self.buffer.drain(..first_char.len_utf8());
            }
        }

        emitted
    }

    fn flush(&mut self) -> Vec<HeuristicFragment> {
        self.strip_control_tokens();
        let mut emitted = Vec::new();

        while !self.buffer.is_empty() {
            if let Some(candidate_pos) = self.find_candidate_start() {
                if candidate_pos > 0 {
                    let prefix = self.buffer[..candidate_pos].to_string();
                    if !prefix.is_empty() {
                        emitted.push(HeuristicFragment::Text(prefix));
                    }
                    self.buffer.drain(..candidate_pos);
                    continue;
                }

                if let Some((calls, consumed)) =
                    try_parse_python_tool_call_prefix(&self.buffer, &self.allowed_tool_names)
                {
                    emitted.extend(calls.into_iter().map(HeuristicFragment::ToolCall));
                    self.buffer.drain(..consumed);
                    continue;
                }
            }

            if let Some(first_char) = self.buffer.chars().next() {
                emitted.push(HeuristicFragment::Text(first_char.to_string()));
                self.buffer.drain(..first_char.len_utf8());
            }
        }

        emitted
    }

    fn strip_control_tokens(&mut self) {
        let mut output = String::with_capacity(self.buffer.len());
        let mut rest = self.buffer.as_str();

        while let Some(start) = rest.find(Self::CONTROL_TOKEN_START) {
            output.push_str(&rest[..start]);
            let tail = &rest[start + Self::CONTROL_TOKEN_START.len()..];
            if let Some(end) = tail.find(Self::CONTROL_TOKEN_END) {
                let token_body = &tail[..end];
                if !token_body.is_empty()
                    && token_body.len() <= 80
                    && !token_body.contains('|')
                    && !token_body.contains('>')
                {
                    rest = &tail[end + Self::CONTROL_TOKEN_END.len()..];
                } else {
                    output.push_str(Self::CONTROL_TOKEN_START);
                    rest = tail;
                }
            } else {
                output.push_str(&rest[start..]);
                rest = "";
            }
        }

        output.push_str(rest);
        self.buffer = output;
    }

    fn find_candidate_start(&self) -> Option<usize> {
        let mut candidate = None;
        candidate = min_option(candidate, self.find_special_wrapper_start());
        candidate = min_option(candidate, self.find_allowed_xml_wrapper_start());
        candidate = min_option(candidate, self.find_harmony_tool_call_start());
        candidate = min_option(candidate, self.find_wrapped_python_call_start());

        for name in &self.allowed_tool_names {
            if let Some(pos) = find_function_name_start(&self.buffer, name) {
                candidate = min_option(candidate, Some(pos));
            }
        }

        candidate
    }

    fn find_wrapped_python_call_start(&self) -> Option<usize> {
        let mut start = 0;
        while let Some(idx) = self.buffer[start..].find('[') {
            let pos = start + idx;
            let inner = self.buffer[pos + 1..].trim_start_matches(char::is_whitespace);
            if self
                .allowed_tool_names
                .iter()
                .any(|name| inner.starts_with(&format!("{name}(")))
            {
                return Some(pos);
            }
            start = pos + 1;
        }
        None
    }

    fn find_special_wrapper_start(&self) -> Option<usize> {
        Self::SPECIAL_WRAPPER_NAMES
            .iter()
            .filter_map(|name| self.buffer.find(&format!("<{name}>")))
            .min()
    }

    fn find_allowed_xml_wrapper_start(&self) -> Option<usize> {
        self.allowed_tool_names
            .iter()
            .filter_map(|name| self.buffer.find(&format!("<{name}>")))
            .min()
    }

    fn find_harmony_tool_call_start(&self) -> Option<usize> {
        let mut start = 0;
        while let Some(idx) = self.buffer[start..].find("to=functions.") {
            let pos = start + idx;
            let rest = &self.buffer[pos + "to=functions.".len()..];
            if self.allowed_tool_names.iter().any(|name| {
                rest.starts_with(name) && looks_like_harmony_tool_tail(&rest[name.len()..])
            }) {
                return Some(pos);
            }
            start = pos + 1;
        }
        None
    }

    fn looks_like_bare_tool_prefix(&self) -> bool {
        let trimmed = self.buffer.trim_start_matches(char::is_whitespace);

        if Self::SPECIAL_WRAPPER_NAMES
            .iter()
            .any(|name| trimmed.starts_with(&format!("<{name}>")))
        {
            return true;
        }

        if self
            .allowed_tool_names
            .iter()
            .any(|name| trimmed.starts_with(&format!("<{name}>")))
        {
            return true;
        }

        if let Some(rest) = trimmed.strip_prefix("to=functions.")
            && self.allowed_tool_names.iter().any(|name| {
                rest.starts_with(name) && looks_like_harmony_tool_tail(&rest[name.len()..])
            })
        {
            return true;
        }

        if trimmed.starts_with('[') {
            let inner = trimmed[1..].trim_start_matches(char::is_whitespace);
            return self
                .allowed_tool_names
                .iter()
                .any(|name| inner.starts_with(&format!("{name}(")));
        }

        self.allowed_tool_names
            .iter()
            .any(|name| trimmed.starts_with(&format!("{name}(")))
    }
}

fn looks_like_harmony_tool_tail(tail: &str) -> bool {
    tail.is_empty()
        || tail.starts_with("json")
        || tail.starts_with('{')
        || tail.starts_with('[')
        || tail.starts_with(char::is_whitespace)
        || tail.starts_with(HeuristicToolParser::CONTROL_TOKEN_START)
}

fn min_option(current: Option<usize>, candidate: Option<usize>) -> Option<usize> {
    match (current, candidate) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn find_function_name_start(text: &str, name: &str) -> Option<usize> {
    let pattern = format!("{name}(");
    let mut start = 0;
    while let Some(idx) = text[start..].find(&pattern) {
        let pos = start + idx;
        let before = text[..pos].chars().next_back();
        if before.is_none_or(|ch| ch.is_whitespace() || ch == '[' || ch == ',' || ch == '(') {
            return Some(pos);
        }
        start = pos + 1;
    }
    None
}

fn try_parse_python_tool_call_prefix(
    text: &str,
    allowed_tool_names: &HashSet<String>,
) -> Option<(Vec<HeuristicToolCall>, usize)> {
    if let Some(result) = try_parse_special_wrapper(text) {
        return Some(result);
    }
    if let Some(result) = try_parse_allowed_xml_wrapper(text, allowed_tool_names) {
        return Some(result);
    }
    if let Some(result) = try_parse_harmony_tool_call(text, allowed_tool_names) {
        return Some(result);
    }

    let trimmed = text.trim_start_matches(char::is_whitespace);
    let consumed_prefix = text.len() - trimmed.len();
    let (calls, consumed) = parse_python_tool_calls_prefix(trimmed, allowed_tool_names)?;
    let heuristic_calls = calls
        .into_iter()
        .map(|(name, input)| HeuristicToolCall {
            id: format!("toolu_heuristic_{}", uuid::Uuid::now_v7()),
            name,
            input,
        })
        .collect();
    Some((heuristic_calls, consumed_prefix + consumed))
}

fn try_parse_special_wrapper(text: &str) -> Option<(Vec<HeuristicToolCall>, usize)> {
    let trimmed = text.trim_start_matches(char::is_whitespace);
    let consumed_prefix = text.len() - trimmed.len();

    for wrapper_name in HeuristicToolParser::SPECIAL_WRAPPER_NAMES {
        let open_tag = format!("<{wrapper_name}>");
        if !trimmed.starts_with(&open_tag) {
            continue;
        }

        let mut rest = trimmed[open_tag.len()..].trim_start_matches(char::is_whitespace);
        let (raw_input, consumed_json) = parse_json_prefix(rest)?;
        rest = &rest[consumed_json..];
        let rest_trimmed = rest.trim_start_matches(char::is_whitespace);
        let consumed_ws = rest.len() - rest_trimmed.len();
        let close_tag = format!("</{wrapper_name}>");
        let consumed_close = if rest_trimmed.starts_with(&close_tag) {
            close_tag.len()
        } else {
            0
        };

        let call = HeuristicToolCall {
            id: format!("toolu_heuristic_{}", uuid::Uuid::now_v7()),
            name: wrapper_name.to_string(),
            input: raw_input,
        };

        return Some((
            vec![call],
            consumed_prefix + open_tag.len() + consumed_json + consumed_ws + consumed_close,
        ));
    }

    None
}

fn try_parse_allowed_xml_wrapper(
    text: &str,
    allowed_tool_names: &HashSet<String>,
) -> Option<(Vec<HeuristicToolCall>, usize)> {
    let trimmed = text.trim_start_matches(char::is_whitespace);
    let consumed_prefix = text.len() - trimmed.len();

    for name in allowed_tool_names {
        let open_tag = format!("<{name}>");
        if !trimmed.starts_with(&open_tag) {
            continue;
        }

        let mut rest = trimmed[open_tag.len()..].trim_start_matches(char::is_whitespace);
        let (raw_input, consumed_json) = parse_json_prefix(rest)?;
        rest = &rest[consumed_json..];
        let rest_trimmed = rest.trim_start_matches(char::is_whitespace);
        let consumed_ws = rest.len() - rest_trimmed.len();
        let close_tag = format!("</{name}>");
        let consumed_close = if rest_trimmed.starts_with(&close_tag) {
            close_tag.len()
        } else {
            0
        };

        return Some((
            vec![HeuristicToolCall {
                id: format!("toolu_heuristic_{}", uuid::Uuid::now_v7()),
                name: name.to_string(),
                input: raw_input,
            }],
            consumed_prefix + open_tag.len() + consumed_json + consumed_ws + consumed_close,
        ));
    }

    None
}

fn try_parse_harmony_tool_call(
    text: &str,
    allowed_tool_names: &HashSet<String>,
) -> Option<(Vec<HeuristicToolCall>, usize)> {
    let trimmed = text.trim_start_matches(char::is_whitespace);
    let consumed_prefix = text.len() - trimmed.len();
    let rest = trimmed.strip_prefix("to=functions.")?;

    let name = allowed_tool_names
        .iter()
        .filter(|name| rest.starts_with(name.as_str()))
        .max_by_key(|name| name.len())?;
    let mut tail = rest[name.len()..].trim_start_matches(char::is_whitespace);
    if let Some(json_tail) = tail.strip_prefix("json") {
        tail = json_tail.trim_start_matches(char::is_whitespace);
    }

    let (raw_input, consumed_json) = parse_json_prefix(tail)?;
    let consumed_before_json = trimmed.len() - tail.len();
    Some((
        vec![HeuristicToolCall {
            id: format!("toolu_heuristic_{}", uuid::Uuid::now_v7()),
            name: name.to_string(),
            input: raw_input,
        }],
        consumed_prefix + consumed_before_json + consumed_json,
    ))
}

fn parse_json_prefix(text: &str) -> Option<(Value, usize)> {
    let trimmed = text.trim_start_matches(char::is_whitespace);
    let consumed_prefix = text.len() - trimmed.len();
    let start_ch = trimmed.chars().next()?;
    if !matches!(start_ch, '{' | '[') {
        return None;
    }

    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (idx, ch) in trimmed.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                if depth == 0 {
                    let end = idx + ch.len_utf8();
                    let value = serde_json::from_str::<Value>(&trimmed[..end]).ok()?;
                    return Some((value, consumed_prefix + end));
                }
            }
            _ => {}
        }
    }

    None
}

fn parse_python_tool_calls_prefix(
    content: &str,
    allowed_tool_names: &HashSet<String>,
) -> Option<(Vec<(String, Value)>, usize)> {
    let original = content;
    let trimmed = content.trim_start_matches(char::is_whitespace);
    let leading_ws = original.len() - trimmed.len();
    let mut content = trimmed;
    let mut consumed = 0;
    let mut wrapped = false;

    if content.starts_with('[') {
        wrapped = true;
        content = &content[1..];
        consumed += 1;
    }

    let mut calls = Vec::new();
    let mut remaining = content;

    loop {
        remaining = remaining.trim_start_matches(char::is_whitespace);
        if wrapped && remaining.starts_with(']') {
            consumed += 1;
            break;
        }
        if remaining.is_empty() {
            break;
        }
        if remaining.starts_with(',') {
            remaining = remaining[1..].trim_start_matches(char::is_whitespace);
            consumed += 1;
            if remaining.is_empty() {
                break;
            }
        }

        let paren_idx = remaining.find('(')?;
        let func_name = remaining[..paren_idx].trim();
        if func_name.is_empty() {
            return None;
        }
        if !allowed_tool_names.is_empty() && !allowed_tool_names.contains(func_name) {
            return None;
        }

        let close_idx = find_matching_paren(remaining, paren_idx)?;
        let args_str = &remaining[paren_idx + 1..close_idx];
        let arguments = parse_python_args(args_str)?;
        let json_map = arguments
            .into_iter()
            .collect::<serde_json::Map<String, Value>>();
        calls.push((func_name.to_string(), Value::Object(json_map)));

        consumed += remaining[..close_idx + 1].len();
        remaining = &remaining[close_idx + 1..];

        if !wrapped {
            break;
        }
    }

    if calls.is_empty() {
        return None;
    }

    Some((calls, leading_ws + consumed))
}

fn find_matching_paren(s: &str, open_idx: usize) -> Option<usize> {
    let mut depth = 1;
    let mut i = open_idx + 1;
    let bytes = s.as_bytes();
    while i < s.len() && depth > 0 {
        match bytes[i] as char {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            '\'' | '"' => {
                let quote = bytes[i] as char;
                i += 1;
                while i < s.len() {
                    let ch = bytes[i] as char;
                    if ch == '\\' && i + 1 < s.len() {
                        i += 2;
                        continue;
                    }
                    if ch == quote {
                        break;
                    }
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn parse_python_args(args: &str) -> Option<BTreeMap<String, Value>> {
    let mut out = BTreeMap::new();
    let bytes = args.as_bytes();
    let mut i = 0;
    while i < args.len() {
        while i < args.len() && ((bytes[i] as char) == ',' || (bytes[i] as char).is_whitespace()) {
            i += 1;
        }
        if i >= args.len() {
            break;
        }
        let key_start = i;
        while i < args.len() && (bytes[i] as char) != '=' && (bytes[i] as char) != ',' {
            i += 1;
        }
        if i >= args.len() || (bytes[i] as char) != '=' {
            return None;
        }
        let key = args[key_start..i].trim();
        if key.is_empty() {
            return None;
        }
        i += 1;
        while i < args.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        let (value, next) = parse_python_arg_value(args, i)?;
        out.insert(key.to_string(), value);
        i = next;
        if i < args.len() && (bytes[i] as char) == ',' {
            i += 1;
        }
    }
    Some(out)
}

fn parse_python_arg_value(s: &str, mut i: usize) -> Option<(Value, usize)> {
    if i >= s.len() {
        return None;
    }
    let bytes = s.as_bytes();
    let current = bytes[i] as char;
    if current == '\'' || current == '"' {
        let quote = current;
        i += 1;
        let start = i;
        while i < s.len() {
            let ch = bytes[i] as char;
            if ch == '\\' && i + 1 < s.len() {
                i += 2;
                continue;
            }
            if ch == quote {
                return Some((Value::String(s[start..i].to_string()), i + 1));
            }
            i += 1;
        }
        return None;
    }

    let start = i;
    let mut depth_paren = 0;
    let mut depth_square = 0;
    let mut depth_curly = 0;
    let mut in_string = false;
    let mut quote = '\0';
    let mut escaped = false;

    while i < s.len() {
        let ch = bytes[i] as char;
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                in_string = false;
            }
            i += 1;
            continue;
        }

        match ch {
            '\'' | '"' => {
                in_string = true;
                quote = ch;
            }
            '(' => depth_paren += 1,
            ')' => {
                if depth_paren > 0 {
                    depth_paren -= 1;
                }
            }
            '[' => depth_square += 1,
            ']' => {
                if depth_square > 0 {
                    depth_square -= 1;
                }
            }
            '{' => depth_curly += 1,
            '}' => {
                if depth_curly > 0 {
                    depth_curly -= 1;
                }
            }
            ',' if depth_paren == 0 && depth_square == 0 && depth_curly == 0 => {
                let token = s[start..i].trim();
                return parse_python_literal(token).map(|value| (value, i));
            }
            _ => {}
        }
        i += 1;
    }

    let token = s[start..i].trim();
    parse_python_literal(token).map(|value| (value, i))
}

fn parse_python_literal(token: &str) -> Option<Value> {
    match token {
        "" => return Some(Value::String(String::new())),
        "true" | "True" => return Some(Value::Bool(true)),
        "false" | "False" => return Some(Value::Bool(false)),
        "null" | "None" => return Some(Value::Null),
        _ => {}
    }

    if let Ok(v) = token.parse::<i64>() {
        return Some(json!(v));
    }
    if let Ok(v) = token.parse::<f64>() {
        return Some(json!(v));
    }

    if token.starts_with('[') || token.starts_with('{') {
        if let Ok(value) = serde_json::from_str::<Value>(token) {
            return Some(value);
        }
        if let Some(converted) = python_literal_to_json(token)
            && let Ok(value) = serde_json::from_str::<Value>(&converted)
        {
            return Some(value);
        }
    }

    Some(Value::String(token.to_string()))
}

fn python_literal_to_json(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len() + s.len() / 8);
    let mut in_string = false;
    let mut quote = '\0';
    let mut escaped = false;
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];
        if in_string {
            if escaped {
                out.push(ch);
                escaped = false;
            } else if ch == '\\' {
                out.push(ch);
                escaped = true;
            } else if ch == quote {
                out.push('"');
                in_string = false;
            } else {
                out.push(ch);
            }
            i += 1;
            continue;
        }

        match ch {
            '\'' | '"' => {
                in_string = true;
                quote = ch;
                out.push('"');
            }
            'T' if chars[i..].starts_with(&['T', 'r', 'u', 'e']) => {
                out.push_str("true");
                i += 3;
            }
            'F' if chars[i..].starts_with(&['F', 'a', 'l', 's', 'e']) => {
                out.push_str("false");
                i += 4;
            }
            'N' if chars[i..].starts_with(&['N', 'o', 'n', 'e']) => {
                out.push_str("null");
                i += 3;
            }
            _ => out.push(ch),
        }
        i += 1;
    }

    Some(out)
}

fn emit_heuristic_tool_call(
    emitted: &mut Vec<Bytes>,
    next_index: &mut usize,
    thinking_index: &mut Option<usize>,
    thinking_started: &mut bool,
    text_index: &mut Option<usize>,
    text_started: &mut bool,
    call: HeuristicToolCall,
) {
    if let Some(index) = thinking_index.take() {
        emitted.push(anthropic_content_block_stop(index));
        *thinking_started = false;
    }
    if let Some(index) = text_index.take() {
        emitted.push(anthropic_content_block_stop(index));
        *text_started = false;
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
