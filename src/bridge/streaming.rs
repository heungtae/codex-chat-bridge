use anyhow::Result;
use async_stream::stream;
use axum::body::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use tracing::{debug, warn};
use uuid::Uuid;

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

pub(crate) fn translate_chat_stream<S>(
    upstream_stream: S,
    response_id: String,
    router_name: String,
    verbose_logging: bool,
    tool_call_kinds_by_name: HashMap<String, ResponsesToolCallKind>,
) -> impl Stream<Item = Result<Bytes, std::convert::Infallible>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    stream! {
        let mut upstream_stream = Box::pin(upstream_stream);
        let mut parser = SseParser::default();
        let mut acc = StreamAccumulator::default();
        let mut assistant_item_added = false;

        yield Ok(sse_event(
            "response.created",
            &json!({
                "type": "response.created",
                "response": {
                    "id": response_id.clone(),
                }
            }),
        ));

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
                    continue;
                }

                match serde_json::from_str::<ChatChunk>(&data) {
                    Ok(chat_chunk) => {
                        if let Some(usage) = chat_chunk.usage.clone() {
                            acc.usage = Some(usage);
                        }

                        for choice in chat_chunk.choices {
                            if let Some(delta) = choice.delta {
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
                                        let entry = acc.tool_calls.entry(index).or_default();

                                        if let Some(id) = tool_call.id {
                                            entry.id = Some(id);
                                        }

                                        if let Some(function) = tool_call.function {
                                            if let Some(name) = function.name {
                                                entry.name = Some(name);
                                            }
                                            if let Some(arguments) = function.arguments {
                                                entry.arguments.push_str(&arguments);
                                            }
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

        if let Some(data) = parser.finish()
            && data != "[DONE]"
        {
            warn!("bridge received trailing SSE payload: {data}");
        }

        if !acc.assistant_text.is_empty() {
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

        for (index, tool_call) in acc.tool_calls {
            let call_id = tool_call
                .id
                .unwrap_or_else(|| format!("call_{}_{}", Uuid::now_v7(), index));
            let name = tool_call
                .name
                .unwrap_or_else(|| "unknown_function".to_string());
            let item = responses_tool_call_item(
                &name,
                &tool_call.arguments,
                &call_id,
                &tool_call_kinds_by_name,
            );

            yield Ok(sse_event(
                "response.output_item.done",
                &json!({
                    "type": "response.output_item.done",
                    "item": item,
                }),
            ));
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

pub(crate) fn sse_event(event_name: &str, payload: &Value) -> Bytes {
    let json_payload = serde_json::to_string(payload).unwrap_or_else(|_| {
        "{\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"internal serialization error\"}}}".to_string()
    });
    Bytes::from(format!("event: {event_name}\ndata: {json_payload}\n\n"))
}

impl SseParser {
    pub(crate) fn feed(&mut self, chunk: &str) -> Vec<String> {
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
                    events.push(self.current_data_lines.join("\n"));
                    self.current_data_lines.clear();
                }
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
        if self.current_data_lines.is_empty() {
            None
        } else {
            Some(self.current_data_lines.join("\n"))
        }
    }
}

