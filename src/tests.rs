use super::*;
use crate::bridge::apply_patch::normalize_apply_patch_input_with_repairs;
use crate::bridge_types::ChatDelta;
use axum::Json;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::State as AxumState;
use axum::http::Request;
use axum::routing::post;
use futures::StreamExt;
use futures::stream;
use serde_json::json;
use std::collections::HashSet;
use std::path::PathBuf;
use tower::ServiceExt;

#[test]
fn maps_responses_request_to_chat_request_with_function_tool() {
    let input = json!({
        "model": "gpt-4.1",
        "instructions": "You are helpful",
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]
            }
        ],
        "tools": [
            {
                "type": "function",
                "name": "get_weather",
                "description": "Get weather",
                "parameters": {"type":"object","properties":{"city":{"type":"string"}}}
            }
        ],
        "tool_choice": "auto",
        "parallel_tool_calls": true
    });

    let req = map_responses_to_chat_request_with_stream(
        &input,
        &HashSet::new(),
        true,
        true,
        ToolTransformMode::LegacyConvert,
    )
    .expect("should map");
    let messages = req
        .chat_request
        .get("messages")
        .and_then(Value::as_array)
        .expect("messages array");
    assert_eq!(messages.len(), 2);

    let tools = req
        .chat_request
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools array");
    assert_eq!(
        tools[0]
            .get("function")
            .and_then(Value::as_object)
            .is_some(),
        true
    );
}

#[test]
fn maps_responses_request_normalizes_null_required() {
    let input = json!({
        "model": "gpt-4.1",
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]
            }
        ],
        "tools": [
            {
                "type": "function",
                "name": "shell",
                "description": "Run shell",
                "parameters": {
                    "type": "object",
                    "properties": {},
                    "required": null
                }
            }
        ]
    });

    let req = map_responses_to_chat_request_with_stream(
        &input,
        &HashSet::new(),
        true,
        true,
        ToolTransformMode::LegacyConvert,
    )
    .expect("should map");

    assert_eq!(
        req.chat_request["tools"][0]["function"]["parameters"]["required"],
        json!([])
    );
}

#[test]
fn maps_responses_request_defaults_missing_parameters() {
    let input = json!({
        "model": "gpt-4.1",
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]
            }
        ],
        "tools": [
            {
                "type": "function",
                "name": "spawn_agent",
                "description": "Spawn a sub-agent"
            }
        ]
    });

    let req = map_responses_to_chat_request_with_stream(
        &input,
        &HashSet::new(),
        true,
        true,
        ToolTransformMode::LegacyConvert,
    )
    .expect("should map");

    assert_eq!(
        req.chat_request["tools"][0]["function"]["parameters"],
        json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        })
    );
}

#[test]
fn maps_responses_request_wraps_missing_type_named_tool() {
    let input = json!({
        "model": "gpt-4.1",
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]
            }
        ],
        "tools": [
            {
                "name": "spawn_agent",
                "description": "Spawn a sub-agent"
            }
        ]
    });

    let req = map_responses_to_chat_request_with_stream(
        &input,
        &HashSet::new(),
        true,
        true,
        ToolTransformMode::LegacyConvert,
    )
    .expect("should map");

    assert_eq!(
        req.chat_request["tools"][0],
        json!({
            "type": "function",
            "function": {
                "name": "spawn_agent",
                "description": "Spawn a sub-agent",
                "parameters": {
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }
            }
        })
    );
}

#[test]
fn sse_parser_collects_data_events() {
    let mut parser = SseParser::default();
    let chunk = "event: message\ndata: {\"a\":1}\n\n";
    let events = parser.feed(chunk);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0], "{\"a\":1}");
}

#[test]
fn chat_delta_treats_empty_tool_calls_as_absent() {
    let delta: ChatDelta =
        serde_json::from_value(json!({"tool_calls": []})).expect("delta should deserialize");
    assert!(delta.tool_calls.is_none());
}

#[test]
fn normalize_tool_choice_wraps_function_name() {
    let choice = json!({"type":"function", "name":"f"});
    let normalized = normalize_tool_choice(choice, ToolTransformMode::LegacyConvert);
    assert_eq!(
        normalized,
        json!({"type":"function", "function": {"name":"f"}})
    );
}

#[test]
fn function_output_text_handles_array_items() {
    let value = json!([
        {"type": "input_text", "text": "line1"},
        {"type": "output_text", "text": "line2"}
    ]);
    assert_eq!(function_output_to_text(&value), "line1\nline2");
}

#[test]
fn normalize_chat_tools_converts_web_search_preview_to_function() {
    let tools = vec![json!({"type": "web_search_preview"})];
    let out = normalize_chat_tools(tools, &HashSet::new(), ToolTransformMode::LegacyConvert);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["type"], "function");
    assert_eq!(out[0]["function"]["name"], "web_search_preview");
}

#[test]
fn normalize_chat_tools_converts_mcp_to_function() {
    let tools = vec![json!({"type": "mcp", "server_label": "shell"})];
    let out = normalize_chat_tools(tools, &HashSet::new(), ToolTransformMode::LegacyConvert);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["type"], "function");
    assert_eq!(out[0]["function"]["name"], "mcp__shell");
}

#[test]
fn normalize_chat_tools_converts_empty_additional_properties_to_false() {
    let tools = vec![json!({
        "type": "function",
        "name": "ExitPlanMode",
        "parameters": {
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": {}
        }
    })];
    let out = normalize_chat_tools(tools, &HashSet::new(), ToolTransformMode::LegacyConvert);
    assert_eq!(out[0]["function"]["parameters"]["required"], json!([]));
    assert_eq!(
        out[0]["function"]["parameters"]["additionalProperties"],
        false
    );
}

#[test]
fn normalize_chat_tools_removes_parameter_constraint_fields() {
    let tools = vec![json!({
        "type": "function",
        "name": "format_output",
        "parameters": {
            "type": "object",
            "properties": {},
            "required": [],
            "structural_tag": null,
            "disable_any_whitespace": false,
            "disable_additional_properties": false,
            "whitespace_pattern": null,
            "regex": null,
            "choice": null,
            "grammar": null,
            "json_object": null
        }
    })];
    let out = normalize_chat_tools(tools, &HashSet::new(), ToolTransformMode::LegacyConvert);
    let parameters = out[0]["function"]["parameters"]
        .as_object()
        .expect("parameters");

    for field in [
        "structural_tag",
        "disable_any_whitespace",
        "disable_additional_properties",
        "whitespace_pattern",
        "regex",
        "choice",
        "grammar",
        "json_object",
    ] {
        assert!(!parameters.contains_key(field), "{field} should be removed");
    }
}

#[test]
fn normalize_chat_tools_converts_null_required_to_empty_array() {
    let tools = vec![json!({
        "type": "function",
        "name": "shell",
        "parameters": {
            "type": "object",
            "properties": {},
            "required": null
        }
    })];
    let out = normalize_chat_tools(tools, &HashSet::new(), ToolTransformMode::LegacyConvert);
    assert_eq!(out[0]["function"]["parameters"]["required"], json!([]));
}

#[test]
fn flatten_content_items_filters_non_text() {
    let items = vec![
        json!({"type":"input_text","text":"a"}),
        json!({"type":"input_image","image_url":"x"}),
        json!({"type":"output_text","text":"b"}),
    ];
    assert_eq!(flatten_content_items(&items, true), "a\n[input_image] x\nb");
}

#[test]
fn map_supports_function_call_output_to_tool_message() {
    let input = json!({
        "model": "gpt-4.1",
        "input": [
            {
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "{\"ok\":true}"
            }
        ],
        "tools": []
    });

    let req = map_responses_to_chat_request_with_stream(
        &input,
        &HashSet::new(),
        true,
        true,
        ToolTransformMode::LegacyConvert,
    )
    .expect("should map");
    let messages = req
        .chat_request
        .get("messages")
        .and_then(Value::as_array)
        .expect("messages");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "tool");
    assert_eq!(messages[0]["tool_call_id"], "call_1");
}

#[test]
fn map_supports_tool_result_to_tool_message() {
    let input = json!({
        "model": "gpt-4.1",
        "input": [
            {
                "type": "tool_result",
                "call_id": "call_2",
                "result": "{\"ok\":true}"
            }
        ],
        "tools": []
    });

    let req = map_responses_to_chat_request_with_stream(
        &input,
        &HashSet::new(),
        true,
        true,
        ToolTransformMode::LegacyConvert,
    )
    .expect("should map");
    let messages = req
        .chat_request
        .get("messages")
        .and_then(Value::as_array)
        .expect("messages");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "tool");
    assert_eq!(messages[0]["tool_call_id"], "call_2");
    assert_eq!(messages[0]["content"], "{\"ok\":true}");
}

#[test]
fn map_supports_web_search_call_to_assistant_tool_call_message() {
    let input = json!({
        "model": "gpt-4.1",
        "input": [
            {
                "type": "web_search_call",
                "call_id": "call_web_1",
                "name": "web_search",
                "arguments": "{\"query\":\"rust\"}"
            }
        ],
        "tools": []
    });

    let req = map_responses_to_chat_request_with_stream(
        &input,
        &HashSet::new(),
        true,
        true,
        ToolTransformMode::LegacyConvert,
    )
    .expect("should map");
    let messages = req
        .chat_request
        .get("messages")
        .and_then(Value::as_array)
        .expect("messages");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "assistant");
    assert_eq!(messages[0]["tool_calls"][0]["id"], "call_web_1");
    assert_eq!(
        messages[0]["tool_calls"][0]["function"]["name"],
        "web_search"
    );
}

#[test]
fn map_supports_function_call_to_assistant_tool_call() {
    let input = json!({
        "model": "gpt-4.1",
        "input": [
            {
                "type": "function_call",
                "call_id": "call_1",
                "name": "get_weather",
                "arguments": "{\"city\":\"seoul\"}"
            }
        ],
        "tools": []
    });

    let req = map_responses_to_chat_request_with_stream(
        &input,
        &HashSet::new(),
        true,
        true,
        ToolTransformMode::LegacyConvert,
    )
    .expect("should map");
    let messages = req
        .chat_request
        .get("messages")
        .and_then(Value::as_array)
        .expect("messages");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "assistant");
    assert_eq!(messages[0]["tool_calls"][0]["id"], "call_1");
    assert_eq!(messages[0]["tool_calls"][0]["type"], "function");
    assert_eq!(
        messages[0]["tool_calls"][0]["function"]["name"],
        "get_weather"
    );
}

#[test]
fn map_supports_custom_tool_call_to_assistant_tool_call() {
    let input = json!({
        "model": "gpt-4.1",
        "input": [
            {
                "type": "custom_tool_call",
                "call_id": "call_custom_1",
                "name": "shell",
                "input": "echo hello"
            }
        ],
        "tools": []
    });

    let req = map_responses_to_chat_request_with_stream(
        &input,
        &HashSet::new(),
        true,
        true,
        ToolTransformMode::LegacyConvert,
    )
    .expect("should map");
    let messages = req
        .chat_request
        .get("messages")
        .and_then(Value::as_array)
        .expect("messages");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "assistant");
    assert_eq!(messages[0]["tool_calls"][0]["id"], "call_custom_1");
    assert_eq!(messages[0]["tool_calls"][0]["type"], "function");
    assert_eq!(messages[0]["tool_calls"][0]["function"]["name"], "shell");
    assert_eq!(
        messages[0]["tool_calls"][0]["function"]["arguments"],
        "echo hello"
    );
}

#[test]
fn map_applies_responses_extensions_to_chat_payload() {
    let input = json!({
        "model": "gpt-4.1",
        "input": [{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],
        "max_output_tokens": 321,
        "metadata": {"trace_id":"t-1"},
        "reasoning": {"effort":"medium"},
        "service_tier": "default",
        "include": ["reasoning.encrypted_content"],
        "text": {"format":{"type":"json_object"}}
    });

    let req = map_responses_to_chat_request_with_stream(
        &input,
        &HashSet::new(),
        true,
        true,
        ToolTransformMode::LegacyConvert,
    )
    .expect("should map");
    assert_eq!(req.chat_request["max_completion_tokens"], 321);
    assert_eq!(req.chat_request["metadata"]["trace_id"], "t-1");
    assert_eq!(req.chat_request["reasoning_effort"], "medium");
    assert_eq!(req.chat_request["service_tier"], "default");
    assert!(req.chat_request.get("include").is_none());
    assert!(req.chat_request.get("reasoning").is_none());
    assert!(req.chat_request.get("max_tokens").is_none());
    assert_eq!(req.chat_request["response_format"]["type"], "json_object");
    assert!(req.chat_request.get("text").is_none());
}

#[test]
fn map_drops_reasoning_input_item_summary_text() {
    let input = json!({
        "model": "gpt-4.1",
        "input": [
            {
                "type": "reasoning",
                "summary": [
                    {"type":"summary_text","text":"step 1"},
                    {"type":"summary_text","text":"step 2"}
                ]
            }
        ]
    });

    let req = map_responses_to_chat_request_with_stream(
        &input,
        &HashSet::new(),
        false,
        false,
        ToolTransformMode::LegacyConvert,
    )
    .expect("should map");
    let messages = req.chat_request["messages"].as_array().expect("messages");
    assert!(messages.is_empty());
}

#[test]
fn map_ignores_reasoning_input_item_with_empty_summary() {
    let input = json!({
        "model": "gpt-4.1",
        "input": [
            {
                "type": "reasoning",
                "summary": [
                    {"type":"summary_text","text":""}
                ]
            }
        ]
    });

    let req = map_responses_to_chat_request_with_stream(
        &input,
        &HashSet::new(),
        false,
        false,
        ToolTransformMode::LegacyConvert,
    )
    .expect("should map");
    let messages = req.chat_request["messages"].as_array().expect("messages");
    assert!(messages.is_empty());
}

#[test]
fn map_drops_reasoning_input_item_text_fallback() {
    let input = json!({
        "model": "gpt-4.1",
        "input": [
            {
                "type": "reasoning",
                "text": "fallback reasoning text"
            }
        ]
    });

    let req = map_responses_to_chat_request_with_stream(
        &input,
        &HashSet::new(),
        false,
        false,
        ToolTransformMode::LegacyConvert,
    )
    .expect("should map");
    let messages = req.chat_request["messages"].as_array().expect("messages");
    assert!(messages.is_empty());
}

#[test]
fn map_preserves_user_messages_while_dropping_reasoning_items() {
    let input = json!({
        "model": "gpt-4.1",
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [
                    {"type":"input_text","text":"hello"}
                ]
            },
            {
                "type": "reasoning",
                "summary": [
                    {"type":"summary_text","text":"internal step"}
                ]
            }
        ]
    });

    let req = map_responses_to_chat_request_with_stream(
        &input,
        &HashSet::new(),
        false,
        false,
        ToolTransformMode::LegacyConvert,
    )
    .expect("should map");
    let messages = req.chat_request["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "hello");
}

#[test]
fn map_responses_developer_message_to_chat_system_message() {
    let input = json!({
        "model": "gpt-4.1",
        "input": [
            {
                "type": "message",
                "role": "developer",
                "content": [
                    {"type":"input_text","text":"<permissions instructions>..."}
                ]
            },
            {
                "type": "message",
                "role": "user",
                "content": [
                    {"type":"input_text","text":"hi"}
                ]
            }
        ]
    });

    let req = map_responses_to_chat_request_with_stream(
        &input,
        &HashSet::new(),
        false,
        false,
        ToolTransformMode::LegacyConvert,
    )
    .expect("should map");
    let messages = req.chat_request["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "<permissions instructions>...");
    assert_eq!(messages[1]["role"], "user");
}

#[test]
fn map_recovers_missing_call_id_from_pending_tool_call() {
    let input = json!({
        "model": "gpt-4.1",
        "input": [
            {"type":"function_call","call_id":"call_1","name":"tool_a","arguments":"{}"},
            {"type":"function_call_output","output":"ok"}
        ]
    });

    let req = map_responses_to_chat_request_with_stream(
        &input,
        &HashSet::new(),
        false,
        true,
        ToolTransformMode::LegacyConvert,
    )
    .expect("should map");
    let messages = req.chat_request["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1]["role"], "tool");
    assert_eq!(messages[1]["tool_call_id"], "call_1");
}

#[test]
fn map_recovers_mismatched_call_id_when_single_pending_exists() {
    let input = json!({
        "model": "gpt-4.1",
        "input": [
            {"type":"function_call","call_id":"call_1","name":"tool_a","arguments":"{}"},
            {"type":"function_call_output","call_id":"wrong_id","output":"ok"}
        ]
    });

    let req = map_responses_to_chat_request_with_stream(
        &input,
        &HashSet::new(),
        false,
        true,
        ToolTransformMode::LegacyConvert,
    )
    .expect("should map");
    let messages = req.chat_request["messages"].as_array().expect("messages");
    assert_eq!(messages[1]["tool_call_id"], "call_1");
}

#[test]
fn map_rejects_mismatched_call_id_when_multiple_pending_exist() {
    let input = json!({
        "model": "gpt-4.1",
        "input": [
            {"type":"function_call","call_id":"call_1","name":"tool_a","arguments":"{}"},
            {"type":"function_call","call_id":"call_2","name":"tool_b","arguments":"{}"},
            {"type":"function_call_output","call_id":"wrong_id","output":"ok"}
        ]
    });

    let err = map_responses_to_chat_request_with_stream(
        &input,
        &HashSet::new(),
        false,
        true,
        ToolTransformMode::LegacyConvert,
    )
    .expect_err("must fail");
    assert!(
        err.to_string()
            .contains("does not match pending tool calls")
    );
}

#[test]
fn map_defaults_tool_choice_when_invalid() {
    let input = json!({
        "model": "gpt-4.1",
        "input": [{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],
        "tools": [{"type":"function","name":"f","parameters":{"type":"object"}}],
        "tool_choice": 123
    });

    let req = map_responses_to_chat_request_with_stream(
        &input,
        &HashSet::new(),
        true,
        true,
        ToolTransformMode::LegacyConvert,
    )
    .expect("should map");
    assert_eq!(req.chat_request["tool_choice"], "auto");
}

#[test]
fn map_requires_input_array() {
    let input = json!({"model":"gpt-4.1"});
    let err = map_responses_to_chat_request_with_stream(
        &input,
        &HashSet::new(),
        true,
        true,
        ToolTransformMode::LegacyConvert,
    )
    .expect_err("must fail");
    assert!(err.to_string().contains("missing `input` array"));
}

#[test]
fn parser_handles_split_chunks() {
    let mut parser = SseParser::default();
    let first = parser.feed("data: {\"a\":");
    assert!(first.is_empty());
    let second = parser.feed("1}\n\n");
    assert_eq!(second, vec!["{\"a\":1}".to_string()]);
}

#[test]
fn map_removes_tool_fields_when_tools_empty() {
    let input = json!({
        "model": "gpt-4.1",
        "input": [{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],
        "tools": []
    });
    let req = map_responses_to_chat_request_with_stream(
        &input,
        &HashSet::new(),
        true,
        true,
        ToolTransformMode::LegacyConvert,
    )
    .expect("ok");
    let obj = req.chat_request.as_object().expect("object");
    assert!(!obj.contains_key("tools"));
    assert!(!obj.contains_key("tool_choice"));
}

#[test]
fn sse_error_response_contains_failed_event() {
    let response = sse_error_response("x", "y");
    assert_eq!(response.status(), StatusCode::OK);
}

#[test]
fn parser_ignores_non_data_lines() {
    let mut parser = SseParser::default();
    let out = parser.feed(": ping\nevent: hello\ndata: {\"z\":1}\n\n");
    assert_eq!(out, vec!["{\"z\":1}".to_string()]);
}

#[test]
fn parser_collects_event_names() {
    let mut parser = SseParser::default();
    let out = parser.feed_with_event_names("event: hello\ndata: {\"z\":1}\n\n");
    assert_eq!(
        out,
        vec![(Some("hello".to_string()), "{\"z\":1}".to_string())]
    );
}

#[test]
fn normalize_chat_tools_keeps_function_already_wrapped() {
    let tools = vec![json!({
        "type": "function",
        "function": {"name":"f", "parameters": {"type":"object"}}
    })];
    let out = normalize_chat_tools(
        tools.clone(),
        &HashSet::new(),
        ToolTransformMode::LegacyConvert,
    );
    assert_eq!(out[0]["function"]["name"], "f");
    assert_eq!(out[0]["function"]["parameters"]["required"], json!([]));
}

#[test]
fn normalize_chat_tools_normalizes_wrapped_function_null_required() {
    let tools = vec![json!({
        "type": "function",
        "function": {
            "name": "f",
            "parameters": {"type":"object","required":null}
        }
    })];
    let out = normalize_chat_tools(tools, &HashSet::new(), ToolTransformMode::LegacyConvert);
    assert_eq!(out[0]["function"]["parameters"]["required"], json!([]));
}

#[test]
fn normalize_chat_tools_removes_wrapped_output_schema_constraint_fields() {
    let tools = vec![json!({
        "type": "function",
        "function": {
            "name": "format_output",
            "parameters": {"type":"object"},
            "output_schema": {
                "type": "object",
                "properties": {},
                "required": [],
                "structural_tag": null,
                "disable_any_whitespace": false,
                "disable_additional_properties": false,
                "whitespace_pattern": null,
                "regex": null,
                "choice": null,
                "grammar": null,
                "json_object": null
            }
        }
    })];
    let out = normalize_chat_tools(tools, &HashSet::new(), ToolTransformMode::LegacyConvert);
    let output_schema = out[0]["function"]["output_schema"]
        .as_object()
        .expect("output_schema");

    assert_eq!(
        output_schema.get("type").and_then(Value::as_str),
        Some("object")
    );
    assert_eq!(output_schema.get("required"), Some(&json!([])));
    for field in [
        "structural_tag",
        "disable_any_whitespace",
        "disable_additional_properties",
        "whitespace_pattern",
        "regex",
        "choice",
        "grammar",
        "json_object",
    ] {
        assert!(
            !output_schema.contains_key(field),
            "{field} should be removed"
        );
    }
}

#[test]
fn normalize_chat_tools_adds_default_parameters_for_wrapped_function() {
    let tools = vec![json!({
        "type": "function",
        "function": {
            "name": "spawn_agent",
            "description": "Spawn a sub-agent"
        }
    })];
    let out = normalize_chat_tools(tools, &HashSet::new(), ToolTransformMode::LegacyConvert);
    assert_eq!(
        out[0]["function"]["parameters"],
        json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        })
    );
}

#[test]
fn map_anthropic_messages_to_chat_request_supports_tools_and_thinking() {
    let input = json!({
        "model": "claude-sonnet",
        "system": [{"type":"text","text":"system prompt"}],
        "messages": [
            {
                "role": "assistant",
                "content": [
                    {"type":"thinking","thinking":"internal"},
                    {"type":"text","text":"hello"},
                    {"type":"tool_use","id":"tool_1","name":"shell","input":{"cmd":"pwd"}}
                ]
            },
            {
                "role": "user",
                "content": [
                    {"type":"text","text":"run it"},
                    {"type":"tool_result","tool_use_id":"tool_1","content":"done"}
                ]
            }
        ],
        "tools": [
            {"name":"shell","description":"Run shell","input_schema":{"type":"object"}}
        ],
        "tool_choice": {"type":"tool","name":"shell"}
    });

    let out = map_anthropic_messages_to_chat_request(&input, false).expect("ok");
    let messages = out["messages"].as_array().expect("messages");
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"], "hello");
    assert_eq!(messages[1]["reasoning_content"], "internal");
    assert_eq!(messages[1]["tool_calls"][0]["function"]["name"], "shell");
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(out["tools"][0]["function"]["name"], "shell");
    assert_eq!(out["tool_choice"]["function"]["name"], "shell");
}

#[test]
fn map_anthropic_messages_to_chat_request_normalizes_empty_additional_properties() {
    let input = json!({
        "model": "claude-sonnet",
        "messages": [{"role":"user","content":[{"type":"text","text":"hi"}]}],
        "tools": [
            {
                "name": "ExitPlanMode",
                "description": "exit plan mode",
                "input_schema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": {}
                }
            }
        ]
    });

    let out = map_anthropic_messages_to_chat_request(&input, false).expect("ok");
    let parameters = &out["tools"][0]["function"]["parameters"];
    assert_eq!(parameters["required"], json!([]));
    assert_eq!(parameters["additionalProperties"], false);
}

#[test]
fn map_anthropic_messages_to_chat_request_normalizes_null_required() {
    let input = json!({
        "model": "claude-sonnet",
        "messages": [{"role":"user","content":[{"type":"text","text":"hi"}]}],
        "tools": [
            {
                "name": "shell",
                "description": "run shell",
                "input_schema": {
                    "type": "object",
                    "properties": {},
                    "required": null
                }
            }
        ]
    });

    let out = map_anthropic_messages_to_chat_request(&input, false).expect("ok");
    assert_eq!(
        out["tools"][0]["function"]["parameters"]["required"],
        json!([])
    );
}

#[test]
fn map_anthropic_messages_to_chat_request_preserves_thinking_in_content_when_enabled() {
    let input = json!({
        "model": "claude-sonnet",
        "messages": [
            {
                "role": "assistant",
                "content": [
                    {"type":"thinking","thinking":"internal chain"},
                    {"type":"text","text":"hello"}
                ]
            }
        ]
    });

    let out = map_anthropic_messages_to_chat_request(&input, true).expect("ok");
    let messages = out["messages"].as_array().expect("messages");
    assert_eq!(messages[0]["reasoning_content"], "internal chain");
    assert_eq!(
        messages[0]["content"],
        "<thinking>\ninternal chain\n</thinking>\n\nhello"
    );
}

#[test]
fn map_anthropic_messages_to_chat_request_preserves_thinking_without_text_for_tool_calls() {
    let input = json!({
        "model": "claude-sonnet",
        "messages": [
            {
                "role": "assistant",
                "content": [
                    {"type":"thinking","thinking":"internal chain"},
                    {"type":"tool_use","id":"tool_1","name":"shell","input":{"cmd":"pwd"}}
                ]
            }
        ]
    });

    let out = map_anthropic_messages_to_chat_request(&input, true).expect("ok");
    let messages = out["messages"].as_array().expect("messages");
    assert_eq!(
        messages[0]["content"],
        "<thinking>\ninternal chain\n</thinking>"
    );
    assert_eq!(messages[0]["tool_calls"][0]["id"], "tool_1");
    assert_eq!(messages[0]["tool_calls"][0]["function"]["name"], "shell");
}

#[test]
fn chat_json_to_anthropic_json_maps_tool_calls() {
    let input = json!({
        "id": "chatcmpl_1",
        "model": "gpt-4.1",
        "choices": [{
            "finish_reason": "tool_calls",
            "message": {
                "content": "hello",
                "reasoning_content": "thinking",
                "tool_calls": [{
                    "id": "call_1",
                    "function": {
                        "name": "shell",
                        "arguments": "{\"cmd\":\"pwd\"}"
                    }
                }]
            }
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 5
        }
    });

    let out = chat_json_to_anthropic_json(input, "claude-bridge");
    assert_eq!(out["type"], "message");
    assert_eq!(out["stop_reason"], "tool_use");
    assert_eq!(out["content"][0]["type"], "thinking");
    assert_eq!(out["content"][1]["type"], "text");
    assert_eq!(out["content"][2]["type"], "tool_use");
    assert_eq!(out["content"][2]["input"]["cmd"], "pwd");
}

#[test]
fn chat_json_to_anthropic_json_keeps_think_tags_as_text() {
    let input = json!({
        "choices": [{
            "message": {
                "content": "before <think>internal</think> after"
            }
        }]
    });

    let out = chat_json_to_anthropic_json(input, "claude-bridge");
    assert_eq!(out["content"][0]["type"], "text");
    assert_eq!(
        out["content"][0]["text"],
        "before <think>internal</think> after"
    );
    assert!(out.get("stop_sequence").is_none());
}

#[test]
fn chat_json_to_anthropic_json_ignores_provider_specific_thinking_blocks() {
    let input = json!({
        "choices": [{
            "message": {
                "thinking_blocks": [
                    {
                        "type": "thinking",
                        "thinking": "step one",
                        "signature": "sig_1"
                    },
                    {
                        "type": "redacted_thinking",
                        "data": "redacted_payload"
                    }
                ]
            }
        }]
    });

    let out = chat_json_to_anthropic_json(input, "claude-bridge");
    assert_eq!(out["content"].as_array().unwrap().len(), 0);
    assert!(out.get("stop_reason").is_none());
    assert!(out.get("stop_sequence").is_none());
}

#[test]
fn chat_json_to_anthropic_json_maps_unknown_finish_reason_to_stop_sequence() {
    let input = json!({
        "choices": [{
            "finish_reason": "content_filter",
            "message": {"content": "filtered"}
        }]
    });

    let out = chat_json_to_anthropic_json(input, "claude-bridge");
    assert_eq!(out["stop_reason"], "stop_sequence");
    assert!(out.get("stop_sequence").is_none());
}

#[test]
fn chat_json_to_anthropic_json_omits_empty_finish_reason() {
    let input = json!({
        "choices": [{
            "finish_reason": "",
            "message": {"content": "hello"}
        }]
    });

    let out = chat_json_to_anthropic_json(input, "claude-bridge");
    assert!(out.get("stop_reason").is_none());
    assert!(out.get("stop_sequence").is_none());
}

#[test]
fn normalize_chat_tools_converts_custom_tool_to_function() {
    let tools = vec![json!({
        "type": "custom",
        "name": "shell",
        "description": "run shell",
        "input_schema": {"type":"object","properties":{"cmd":{"type":"string"}}}
    })];
    let out = normalize_chat_tools(tools, &HashSet::new(), ToolTransformMode::LegacyConvert);
    assert_eq!(
        out[0],
        json!({
            "type": "function",
            "function": {
                "name": "shell",
                "description": "run shell",
                "parameters": {"type":"object","properties":{"cmd":{"type":"string"}},"required":[]}
            }
        })
    );
}

#[test]
fn normalize_chat_tools_custom_without_schema_requires_input_string() {
    let tools = vec![json!({
        "type": "custom",
        "name": "apply_patch",
        "description": "patch files"
    })];
    let out = normalize_chat_tools(tools, &HashSet::new(), ToolTransformMode::LegacyConvert);
    assert_eq!(out[0]["type"], "function");
    assert_eq!(out[0]["function"]["parameters"]["type"], "object");
    assert_eq!(
        out[0]["function"]["parameters"]["required"],
        json!(["input"])
    );
    assert_eq!(
        out[0]["function"]["parameters"]["properties"]["input"]["type"],
        "string"
    );
}

#[test]
fn normalize_chat_tools_custom_string_schema_wraps_as_input_object() {
    let tools = vec![json!({
        "type": "custom",
        "name": "apply_patch",
        "input_schema": {"type":"string"}
    })];
    let out = normalize_chat_tools(tools, &HashSet::new(), ToolTransformMode::LegacyConvert);
    assert_eq!(out[0]["type"], "function");
    assert_eq!(out[0]["function"]["parameters"]["type"], "object");
    assert_eq!(
        out[0]["function"]["parameters"]["properties"]["input"]["type"],
        "string"
    );
}

#[test]
fn normalize_tool_choice_preserves_wrapped_choice() {
    let choice = json!({"type":"function", "function":{"name":"do_it"}});
    assert_eq!(
        normalize_tool_choice(choice.clone(), ToolTransformMode::LegacyConvert),
        choice
    );
}

#[test]
fn normalize_tool_choice_converts_custom_name() {
    let choice = json!({"type":"custom", "name":"shell"});
    assert_eq!(
        normalize_tool_choice(choice, ToolTransformMode::LegacyConvert),
        json!({"type":"function","function":{"name":"shell"}})
    );
}

#[test]
fn function_output_to_text_for_object_uses_json() {
    let value = json!({"ok": true});
    assert_eq!(function_output_to_text(&value), "{\"ok\":true}");
}

#[test]
fn map_includes_system_message_from_instructions() {
    let input = json!({
        "model": "gpt-4.1",
        "instructions": "sys",
        "input": [{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],
        "tools": []
    });
    let req = map_responses_to_chat_request_with_stream(
        &input,
        &HashSet::new(),
        true,
        true,
        ToolTransformMode::LegacyConvert,
    )
    .expect("ok");
    let messages = req.chat_request["messages"].as_array().expect("array");
    assert_eq!(messages[0]["role"], "system");
}

#[test]
fn normalize_chat_tools_drops_configured_tool_types() {
    let tools = vec![
        json!({"type": "web_search_preview"}),
        json!({"type": "function", "name": "f", "parameters": {"type":"object"}}),
    ];
    let mut drop = HashSet::new();
    drop.insert("web_search_preview".to_string());
    let out = normalize_chat_tools(tools, &drop, ToolTransformMode::LegacyConvert);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["type"], "function");
}

#[test]
fn normalize_chat_tools_passthrough_keeps_custom_tool() {
    let tools = vec![json!({
        "type": "custom",
        "name": "apply_patch",
        "description": "patch files",
        "input_schema": {"type":"string"}
    })];
    let out = normalize_chat_tools(
        tools.clone(),
        &HashSet::new(),
        ToolTransformMode::Passthrough,
    );
    assert_eq!(out, tools);
}

#[test]
fn normalize_tool_choice_passthrough_keeps_custom_choice() {
    let choice = json!({"type":"custom", "name":"apply_patch"});
    assert_eq!(
        normalize_tool_choice(choice.clone(), ToolTransformMode::Passthrough),
        choice
    );
}

#[test]
fn responses_tool_call_item_maps_custom_and_function_types() {
    let mut kinds = HashMap::new();
    kinds.insert("shell".to_string(), ResponsesToolCallKind::Custom);
    kinds.insert("get_weather".to_string(), ResponsesToolCallKind::Function);

    let custom_item = responses_tool_call_item("shell", "ls -al", "call_custom_1", &kinds);
    assert_eq!(custom_item["type"], "custom_tool_call");
    assert_eq!(custom_item["call_id"], "call_custom_1");
    assert_eq!(custom_item["input"], "ls -al");

    let function_item =
        responses_tool_call_item("get_weather", "{\"city\":\"seoul\"}", "call_fn_1", &kinds);
    assert_eq!(function_item["type"], "function_call");
    assert_eq!(function_item["call_id"], "call_fn_1");
    assert_eq!(function_item["arguments"], "{\"city\":\"seoul\"}");
}

#[tokio::test]
async fn stream_emits_output_item_added_before_text_delta() {
    let upstream = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n\
             data: [DONE]\n\n",
    ))]);
    let mut output = Box::pin(translate_chat_stream(
        upstream,
        "resp_1".to_string(),
        "test_router".to_string(),
        false,
        HashMap::new(),
        FeatureFlags::default(),
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    let added_idx = payload
        .find("event: response.output_item.added")
        .expect("added event");
    let delta_idx = payload
        .find("event: response.output_text.delta")
        .expect("delta event");
    assert!(added_idx < delta_idx);
    assert!(payload.contains("event: response.in_progress"));
    assert!(payload.contains("event: response.content_part.added"));
    assert!(payload.contains("event: response.output_text.done"));
    assert!(payload.contains("event: response.content_part.done"));
}

#[tokio::test]
async fn stream_maps_custom_tool_call_and_preserves_call_id() {
    let upstream = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_custom_1\",\"function\":{\"name\":\"shell\",\"arguments\":\"echo hello\"}}]}}]}\n\n\
             data: [DONE]\n\n",
    ))]);
    let mut kinds = HashMap::new();
    kinds.insert("shell".to_string(), ResponsesToolCallKind::Custom);
    let mut output = Box::pin(translate_chat_stream(
        upstream,
        "resp_1".to_string(),
        "test_router".to_string(),
        false,
        kinds,
        FeatureFlags::default(),
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    assert!(payload.contains("\"type\":\"custom_tool_call\""));
    assert!(payload.contains("\"call_id\":\"call_custom_1\""));
    assert!(payload.contains("\"input\":\"echo hello\""));
}

#[test]
fn chat_json_to_responses_json_maps_custom_tool_call_type() {
    let chat = json!({
        "choices": [{
            "message": {
                "tool_calls": [{
                    "id": "call_custom_1",
                    "function": {
                        "name": "shell",
                        "arguments": "echo hello"
                    }
                }]
            }
        }]
    });
    let mut kinds = HashMap::new();
    kinds.insert("shell".to_string(), ResponsesToolCallKind::Custom);
    let out = chat_json_to_responses_json(chat, "resp_1".to_string(), &kinds, false);
    let output = out["output"].as_array().expect("output array");
    assert_eq!(output[0]["type"], "custom_tool_call");
    assert_eq!(output[0]["call_id"], "call_custom_1");
    assert_eq!(output[0]["input"], "echo hello");
}

#[test]
fn chat_json_to_responses_json_unwraps_custom_tool_call_json_string_arguments() {
    let chat = json!({
        "choices": [{
            "message": {
                "tool_calls": [{
                    "id": "call_custom_2",
                    "function": {
                        "name": "apply_patch",
                        "arguments": "\"*** Begin Patch\\n*** End Patch\""
                    }
                }]
            }
        }]
    });
    let mut kinds = HashMap::new();
    kinds.insert("apply_patch".to_string(), ResponsesToolCallKind::Custom);
    let out = chat_json_to_responses_json(chat, "resp_2".to_string(), &kinds, false);
    let output = out["output"].as_array().expect("output");
    assert_eq!(output[0]["type"], "custom_tool_call");
    assert_eq!(output[0]["input"], "*** Begin Patch\n*** End Patch");
}

#[test]
fn chat_json_to_responses_json_unwraps_custom_tool_call_input_field_arguments() {
    let chat = json!({
        "choices": [{
            "message": {
                "tool_calls": [{
                    "id": "call_custom_3",
                    "function": {
                        "name": "apply_patch",
                        "arguments": "{\"input\":\"*** Begin Patch\\n*** End Patch\"}"
                    }
                }]
            }
        }]
    });
    let mut kinds = HashMap::new();
    kinds.insert("apply_patch".to_string(), ResponsesToolCallKind::Custom);
    let out = chat_json_to_responses_json(chat, "resp_3".to_string(), &kinds, false);
    let output = out["output"].as_array().expect("output");
    assert_eq!(output[0]["type"], "custom_tool_call");
    assert_eq!(output[0]["input"], "*** Begin Patch\n*** End Patch");
}

#[test]
fn chat_json_to_responses_json_unwraps_custom_tool_call_patch_field_arguments() {
    let chat = json!({
        "choices": [{
            "message": {
                "tool_calls": [{
                    "id": "call_custom_4",
                    "function": {
                        "name": "apply_patch",
                        "arguments": "{\"patch\":\"*** Begin Patch\\n*** End Patch\"}"
                    }
                }]
            }
        }]
    });
    let mut kinds = HashMap::new();
    kinds.insert("apply_patch".to_string(), ResponsesToolCallKind::Custom);
    let out = chat_json_to_responses_json(chat, "resp_4".to_string(), &kinds, false);
    let output = out["output"].as_array().expect("output");
    assert_eq!(output[0]["type"], "custom_tool_call");
    assert_eq!(output[0]["input"], "*** Begin Patch\n*** End Patch");
}

#[test]
fn normalize_apply_patch_input_strips_code_fence_and_text() {
    let raw = "Here is the patch:\n```patch\n*** Begin Patch\n*** Update File: a.txt\n@@\n-old   \n+new   \n*** End Patch\n```\n";
    assert_eq!(
        normalize_apply_patch_input_with_repairs(raw).normalized,
        "*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** End Patch"
    );
}

#[test]
fn normalize_apply_patch_input_repairs_add_file_lines_without_prefix() {
    let raw = "*** Begin Patch\n*** Add File: hello.txt\nhello\n\nworld\n*** End Patch";
    assert_eq!(
        normalize_apply_patch_input_with_repairs(raw).normalized,
        "*** Begin Patch\n*** Add File: hello.txt\n+hello\n+\n+world\n*** End Patch"
    );
}

#[test]
fn normalize_apply_patch_input_repairs_mixed_add_file_lines() {
    let raw = "*** Begin Patch\n*** Add File: hello.txt\n+hello\n world\n*** Update File: a.txt\n@@\n-old\n+new\n*** End Patch";
    assert_eq!(
        normalize_apply_patch_input_with_repairs(raw).normalized,
        "*** Begin Patch\n*** Add File: hello.txt\n+hello\n+ world\n*** Update File: a.txt\n@@\n-old\n+new\n*** End Patch"
    );
}

#[test]
fn normalize_apply_patch_input_reports_repairs() {
    let raw = "*** Begin Patch\n*** Add File: hello.txt\nhello\n\n*** End Patch";
    let normalized = normalize_apply_patch_input_with_repairs(raw);
    assert_eq!(
        normalized.normalized,
        "*** Begin Patch\n*** Add File: hello.txt\n+hello\n+\n*** End Patch"
    );
    assert_eq!(
        normalized.repairs,
        vec![
            "added missing '+' prefix for add-file content line (hello.txt:1)",
            "added missing '+' prefix for blank add-file line (hello.txt:2)",
        ]
    );
}

#[test]
fn chat_json_to_responses_json_normalizes_apply_patch_arguments() {
    let chat = json!({
        "choices": [{
            "message": {
                "tool_calls": [{
                    "id": "call_custom_5",
                    "function": {
                        "name": "apply_patch",
                        "arguments": "{\"input\":\"Patch follows\\n```patch\\n*** Begin Patch\\n*** Update File: a.txt\\n@@\\n-old   \\n+new   \\n*** End Patch\\n```\"}"
                    }
                }]
            }
        }]
    });
    let mut kinds = HashMap::new();
    kinds.insert("apply_patch".to_string(), ResponsesToolCallKind::Custom);
    let out = chat_json_to_responses_json(chat, "resp_5".to_string(), &kinds, false);
    let output = out["output"].as_array().expect("output");
    assert_eq!(
        output[0]["input"],
        "*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** End Patch"
    );
}

#[test]
fn chat_json_to_responses_json_keeps_non_apply_patch_custom_input_unchanged() {
    let chat = json!({
        "choices": [{
            "message": {
                "tool_calls": [{
                    "id": "call_custom_6",
                    "function": {
                        "name": "shell",
                        "arguments": "{\"input\":\"```bash\\necho hello\\n```\"}"
                    }
                }]
            }
        }]
    });
    let mut kinds = HashMap::new();
    kinds.insert("shell".to_string(), ResponsesToolCallKind::Custom);
    let out = chat_json_to_responses_json(chat, "resp_6".to_string(), &kinds, false);
    let output = out["output"].as_array().expect("output");
    assert_eq!(output[0]["input"], "```bash\necho hello\n```");
}

#[test]
fn chat_json_to_responses_json_maps_length_finish_reason_to_incomplete() {
    let chat = json!({
        "choices": [{
            "finish_reason": "length",
            "message": {
                "content": "partial output"
            }
        }]
    });
    let out = chat_json_to_responses_json(chat, "resp_2".to_string(), &HashMap::new(), false);
    assert_eq!(out["status"], "incomplete");
}

#[test]
fn chat_json_to_responses_json_preserves_provider_specific_fields() {
    let chat = json!({
        "provider_specific_fields": {
            "mcp_list_tools": [{"name":"shell"}]
        },
        "choices": [{
            "message": {
                "tool_calls": [{
                    "id": "call_fn_1",
                    "provider_specific_fields": {"source":"provider"},
                    "function": {
                        "name": "get_weather",
                        "arguments": "{\"city\":\"seoul\"}"
                    }
                }]
            }
        }]
    });
    let out = chat_json_to_responses_json(chat, "resp_3".to_string(), &HashMap::new(), true);
    assert_eq!(
        out["provider_specific_fields"]["mcp_list_tools"][0]["name"],
        "shell"
    );
    assert_eq!(
        out["output"][0]["provider_specific_fields"]["source"],
        "provider"
    );
}

#[test]
fn responses_json_to_chat_json_maps_text_tool_calls_and_usage() {
    let response = json!({
        "id": "resp_1",
        "model": "gpt-4.1",
        "status": "completed",
        "output": [
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type":"output_text","text":"hello"}]
            },
            {
                "type": "function_call",
                "call_id": "call_1",
                "name": "get_weather",
                "arguments": "{\"city\":\"seoul\"}"
            }
        ],
        "usage": {
            "input_tokens": 3,
            "output_tokens": 5,
            "total_tokens": 8
        }
    });

    let out = responses_json_to_chat_json(response, "fallback-model");

    assert_eq!(out["object"], "chat.completion");
    assert_eq!(out["id"], "resp_1");
    assert_eq!(out["model"], "gpt-4.1");
    assert_eq!(out["choices"][0]["message"]["role"], "assistant");
    assert_eq!(out["choices"][0]["message"]["content"], "hello");
    assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        out["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "get_weather"
    );
    assert_eq!(
        out["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
        "{\"city\":\"seoul\"}"
    );
    assert_eq!(out["usage"]["prompt_tokens"], 3);
    assert_eq!(out["usage"]["completion_tokens"], 5);
    assert_eq!(out["usage"]["total_tokens"], 8);
}

#[tokio::test]
async fn responses_stream_to_chat_stream_emits_data_only_chunks_and_done() {
    let upstream = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
        "event: response.output_text.delta\n\
         data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n\
         event: response.output_item.added\n\
         data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"shell\",\"arguments\":\"\"}}\n\n\
         event: response.function_call_arguments.delta\n\
         data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"call_1\",\"output_index\":1,\"delta\":\"{\\\"cmd\\\":\"}\n\n\
         event: response.function_call_arguments.delta\n\
         data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"call_1\",\"output_index\":1,\"delta\":\"\\\"ls\\\"}\"}\n\n\
         event: response.completed\n\
         data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2,\"total_tokens\":3}}}\n\n",
    ))]);
    let mut output = Box::pin(translate_responses_stream_to_chat(
        upstream,
        "test_router".to_string(),
        false,
        "gpt-4.1".to_string(),
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    assert!(!payload.contains("event:"));
    assert!(payload.contains("\"object\":\"chat.completion.chunk\""));
    assert!(payload.contains("\"content\":\"Hi\""));
    assert!(payload.contains("\"tool_calls\""));
    assert!(payload.contains("\"arguments\":\"{\\\"cmd\\\":\""));
    assert!(payload.contains("\"arguments\":\"\\\"ls\\\"}\""));
    assert!(payload.contains("\"finish_reason\":\"tool_calls\""));
    assert!(payload.contains("\"prompt_tokens\":1"));
    assert!(payload.ends_with("data: [DONE]\n\n"));
}

#[test]
fn resolve_config_prefers_cli_over_file_and_defaults() {
    let args = Args {
        config: None,
        upstream_url: None,
        upstream_wire: None,
        upstream_http_headers: vec![
            parse_upstream_http_header_arg("x-trace-id=cli").expect("valid header"),
        ],
        forward_incoming_headers: vec![],
        api_key_env: Some("CLI_API_KEY".to_string()),
        server_info: None,
        http_shutdown: true,
        verbose_logging: false,
        drop_tool_types: vec![],
        router: None,
        list_routers: false,
    };
    let file = FileConfig {
        upstream_url: Some("https://example.com/v1/chat/completions".to_string()),
        upstream_wire: None,
        upstream_http_headers: Some(BTreeMap::from([(
            "x-trace-id".to_string(),
            "file".to_string(),
        )])),
        forward_incoming_headers: None,
        api_key_env: Some("FILE_API_KEY".to_string()),
        server_info: Some(PathBuf::from("/tmp/server.json")),
        http_shutdown: Some(false),
        verbose_logging: Some(true),
        drop_tool_types: None,
        drop_request_fields: None,
        features: None,
        routers: None,
    };

    let resolved = resolve_config(args, Some(file)).expect("ok");
    assert_eq!(
        resolved.upstream_url,
        "https://example.com/v1/chat/completions"
    );
    assert_eq!(resolved.upstream_http_headers.len(), 1);
    assert_eq!(resolved.upstream_http_headers[0].name, "x-trace-id");
    assert_eq!(resolved.upstream_http_headers[0].value, "cli");
    assert_eq!(resolved.api_key_env, "CLI_API_KEY");
    assert_eq!(
        resolved.server_info,
        Some(PathBuf::from("/tmp/server.json"))
    );
    assert!(resolved.http_shutdown);
    assert!(resolved.verbose_logging);
}

#[test]
fn resolve_config_uses_defaults_when_missing() {
    let args = Args {
        config: None,
        upstream_url: None,
        upstream_wire: None,
        upstream_http_headers: vec![],
        forward_incoming_headers: vec![],
        api_key_env: None,
        server_info: None,
        http_shutdown: false,
        verbose_logging: false,
        drop_tool_types: vec![],
        router: None,
        list_routers: false,
    };

    let resolved = resolve_config(args, None).expect("ok");
    assert_eq!(
        resolved.upstream_url,
        "https://api.openai.com/v1/chat/completions"
    );
    assert_eq!(resolved.api_key_env, "OPENAI_API_KEY");
    assert_eq!(resolved.server_info, None);
    assert!(!resolved.http_shutdown);
    assert!(!resolved.verbose_logging);
}

#[test]
fn resolve_config_path_prefers_cli_value() {
    let path = resolve_config_path(Some(PathBuf::from("/tmp/custom.toml"))).expect("ok");
    assert_eq!(path, PathBuf::from("/tmp/custom.toml"));
}

#[test]
fn resolve_config_defaults_upstream_url_by_wire() {
    let args = Args {
        config: None,
        upstream_url: None,
        upstream_wire: Some(WireApi::Responses),
        upstream_http_headers: vec![],
        forward_incoming_headers: vec![],
        api_key_env: None,
        server_info: None,
        http_shutdown: false,
        verbose_logging: false,
        drop_tool_types: vec![],
        router: None,
        list_routers: false,
    };

    let resolved = resolve_config(args, None).expect("ok");
    assert_eq!(resolved.upstream_wire, WireApi::Responses);
    assert_eq!(resolved.upstream_url, "https://api.openai.com/v1/responses");
}

#[test]
fn resolve_config_infers_upstream_wire_from_upstream_url() {
    let args = Args {
        config: None,
        upstream_url: None,
        upstream_wire: None,
        upstream_http_headers: vec![],
        forward_incoming_headers: vec![],
        api_key_env: None,
        server_info: None,
        http_shutdown: false,
        verbose_logging: false,
        drop_tool_types: vec![],
        router: None,
        list_routers: false,
    };
    let file = FileConfig {
        upstream_url: Some("https://api.openai.com/v1/responses".to_string()),
        ..Default::default()
    };

    let resolved = resolve_config(args, Some(file)).expect("ok");
    assert_eq!(resolved.upstream_wire, WireApi::Responses);
}

#[test]
fn resolve_config_rejects_mismatched_upstream_wire_and_url() {
    let args = Args {
        config: None,
        upstream_url: None,
        upstream_wire: Some(WireApi::Chat),
        upstream_http_headers: vec![],
        forward_incoming_headers: vec![],
        api_key_env: None,
        server_info: None,
        http_shutdown: false,
        verbose_logging: false,
        drop_tool_types: vec![],
        router: None,
        list_routers: false,
    };
    let file = FileConfig {
        upstream_url: Some("https://api.openai.com/v1/responses".to_string()),
        ..Default::default()
    };

    let err = resolve_config(args, Some(file)).expect_err("must fail");
    assert!(err.to_string().contains("configuration mismatch"));
}

#[test]
fn parse_upstream_http_header_arg_rejects_invalid_input() {
    let err = parse_upstream_http_header_arg("not-valid").expect_err("must fail");
    assert!(err.contains("NAME=VALUE"));
}

#[test]
fn file_config_accepts_http_headers_alias() {
    let parsed: FileConfig = toml::from_str("http_headers = { \"x-test\" = \"1\" }").expect("ok");
    let headers = parsed.upstream_http_headers.expect("headers");
    assert_eq!(headers.get("x-test"), Some(&"1".to_string()));
}

#[test]
fn file_config_does_not_accept_profiles_alias_for_routers() {
    let parsed: FileConfig =
        toml::from_str("[profiles.gpt_oss]\nincoming_url = \"http://localhost:8080/gpt-oss\"")
            .expect("ok");
    assert!(parsed.routers.is_none());
}

#[test]
fn normalize_incoming_url_to_path_supports_full_url_and_path() {
    assert_eq!(
        normalize_incoming_url_to_path("http://localhost:8080/gpt-oss/").expect("normalized"),
        "/gpt-oss"
    );
    assert_eq!(
        normalize_incoming_url_to_path("/gpt-oss/").expect("normalized"),
        "/gpt-oss"
    );
}

#[test]
fn router_manager_routes_by_incoming_url_path() {
    let mut routers = BTreeMap::new();
    routers.insert(
        "gpt_oss".to_string(),
        RouterConfig {
            incoming_url: Some("http://localhost:8080/gpt-oss".to_string()),
            upstream_url: Some("http://upstream.local/v1/chat/completions".to_string()),
            ..Default::default()
        },
    );

    let manager = RouterManager::new(
        routers,
        "https://api.openai.com/v1/chat/completions".to_string(),
        WireApi::Chat,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        FeatureFlags::default(),
    )
    .expect("manager");
    let target = manager
        .get_target_for_incoming_route("/gpt-oss", Some("localhost:8080"))
        .expect("route lookup")
        .expect("target");

    assert_eq!(target.router_name, "gpt_oss");
    assert_eq!(
        target.upstream_url,
        "http://upstream.local/v1/chat/completions"
    );
}

#[test]
fn router_manager_routes_prefix_paths_to_the_best_match() {
    let mut routers = BTreeMap::new();
    routers.insert(
        "claude_root".to_string(),
        RouterConfig {
            incoming_url: Some("http://localhost:8080/claude".to_string()),
            upstream_url: Some("http://upstream.local/v1/chat/completions".to_string()),
            ..Default::default()
        },
    );
    routers.insert(
        "claude_messages".to_string(),
        RouterConfig {
            incoming_url: Some("http://localhost:8080/claude/v1/messages".to_string()),
            upstream_url: Some("http://upstream.local/v1/chat/completions".to_string()),
            ..Default::default()
        },
    );

    let manager = RouterManager::new(
        routers,
        "https://api.openai.com/v1/chat/completions".to_string(),
        WireApi::Chat,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        FeatureFlags::default(),
    )
    .expect("manager");

    let exact_target = manager
        .get_target_for_incoming_route("/claude/v1/messages", Some("localhost:8080"))
        .expect("route lookup")
        .expect("target");
    assert_eq!(exact_target.router_name, "claude_messages");

    let prefix_target = manager
        .get_target_for_incoming_route("/claude/v1/messages/count_tokens", Some("localhost:8080"))
        .expect("route lookup")
        .expect("target");
    assert_eq!(prefix_target.router_name, "claude_messages");
}

#[test]
fn router_snapshot_marks_registered_route_as_active() {
    let mut routers = BTreeMap::new();
    routers.insert(
        "gpt_oss".to_string(),
        RouterConfig {
            incoming_url: Some("http://localhost:8080/gpt-oss".to_string()),
            ..Default::default()
        },
    );

    let manager = RouterManager::new(
        routers,
        "https://api.openai.com/v1/chat/completions".to_string(),
        WireApi::Chat,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        FeatureFlags::default(),
    )
    .expect("manager");

    let snapshots = manager.get_router_delta_log_snapshots();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].name, "gpt_oss");
    assert!(snapshots[0].active);
    assert_eq!(snapshots[0].non_local_incoming_host, None);
}

#[test]
fn router_manager_infers_router_wire_from_upstream_url() {
    let mut routers = BTreeMap::new();
    routers.insert(
        "gpt_oss".to_string(),
        RouterConfig {
            incoming_url: Some("http://localhost:8080/gpt-oss".to_string()),
            upstream_url: Some("http://upstream.local/v1/responses".to_string()),
            ..Default::default()
        },
    );

    let manager = RouterManager::new(
        routers,
        "https://api.openai.com/v1/chat/completions".to_string(),
        WireApi::Chat,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        FeatureFlags::default(),
    )
    .expect("manager");
    let target = manager
        .get_target_for_incoming_route("/gpt-oss", Some("localhost:8080"))
        .expect("route lookup")
        .expect("target");

    assert_eq!(target.upstream_wire, WireApi::Responses);
}

#[test]
fn router_manager_resolves_router_upstream_model_override() {
    let mut routers = BTreeMap::new();
    routers.insert(
        "claude".to_string(),
        RouterConfig {
            incoming_url: Some("http://localhost:8080/claude/v1/messages".to_string()),
            upstream_model: Some("openrouter/provider-model".to_string()),
            ..Default::default()
        },
    );

    let manager = RouterManager::new(
        routers,
        "https://api.openai.com/v1/chat/completions".to_string(),
        WireApi::Chat,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        FeatureFlags::default(),
    )
    .expect("manager");
    let target = manager
        .get_target_for_incoming_route("/claude/v1/messages", Some("localhost:8080"))
        .expect("route lookup")
        .expect("target");

    assert_eq!(
        target.upstream_model,
        Some("openrouter/provider-model".to_string())
    );

    let snapshots = manager.get_router_delta_log_snapshots();
    assert_eq!(
        snapshots[0].override_upstream_model,
        Some("openrouter/provider-model".to_string())
    );
}

#[test]
fn router_manager_resolves_router_claude_family_model_overrides() {
    let mut routers = BTreeMap::new();
    routers.insert(
        "claude".to_string(),
        RouterConfig {
            incoming_url: Some("http://localhost:8080/claude/v1/messages".to_string()),
            upstream_model_sonnet: Some("openrouter/sonnet".to_string()),
            upstream_model_haiku: Some("openrouter/haiku".to_string()),
            anthropic_preserve_thinking: Some(true),
            anthropic_enable_openrouter_reasoning: Some(true),
            ..Default::default()
        },
    );

    let manager = RouterManager::new(
        routers,
        "https://api.openai.com/v1/chat/completions".to_string(),
        WireApi::Chat,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        FeatureFlags::default(),
    )
    .expect("manager");
    let target = manager
        .get_target_for_incoming_route("/claude/v1/messages", Some("localhost:8080"))
        .expect("route lookup")
        .expect("target");

    assert_eq!(
        target.upstream_model_sonnet,
        Some("openrouter/sonnet".to_string())
    );
    assert_eq!(
        target.upstream_model_haiku,
        Some("openrouter/haiku".to_string())
    );
    assert!(target.anthropic_preserve_thinking);
    assert!(target.anthropic_enable_openrouter_reasoning);

    let snapshots = manager.get_router_delta_log_snapshots();
    assert_eq!(
        snapshots[0].override_upstream_model_sonnet,
        Some("openrouter/sonnet".to_string())
    );
    assert_eq!(
        snapshots[0].override_upstream_model_haiku,
        Some("openrouter/haiku".to_string())
    );
    assert_eq!(
        snapshots[0].override_anthropic_preserve_thinking,
        Some(true)
    );
    assert_eq!(
        snapshots[0].override_anthropic_enable_openrouter_reasoning,
        Some(true)
    );
}

#[test]
fn router_manager_rejects_mismatched_router_wire_and_upstream_url() {
    let mut routers = BTreeMap::new();
    routers.insert(
        "gpt_oss".to_string(),
        RouterConfig {
            incoming_url: Some("http://localhost:8080/gpt-oss".to_string()),
            upstream_url: Some("http://upstream.local/v1/responses".to_string()),
            upstream_wire: Some(WireApi::Chat),
            ..Default::default()
        },
    );

    let result = RouterManager::new(
        routers,
        "https://api.openai.com/v1/chat/completions".to_string(),
        WireApi::Chat,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        FeatureFlags::default(),
    );
    match result {
        Ok(_) => panic!("must fail"),
        Err(err) => assert!(err.to_string().contains("configuration mismatch")),
    }
}

#[test]
fn apply_upstream_model_override_replaces_request_model() {
    let mut payload = json!({
        "model": "claude-sonnet-4-6",
        "messages": [{"role":"user","content":"hi"}]
    });

    let route_target = RouteTarget {
        router_name: "claude".to_string(),
        upstream_url: "http://localhost:8080/v1/chat/completions".to_string(),
        upstream_wire: WireApi::Chat,
        upstream_model: Some("openrouter/default-model".to_string()),
        upstream_model_opus: Some("openrouter/opus-model".to_string()),
        upstream_model_sonnet: Some("openrouter/sonnet-model".to_string()),
        upstream_model_haiku: Some("openrouter/haiku-model".to_string()),
        upstream_http_headers: Vec::new(),
        forward_incoming_headers: Vec::new(),
        drop_tool_types: HashSet::new(),
        drop_request_fields: HashSet::new(),
        feature_flags: FeatureFlags::default(),
        anthropic_preserve_thinking: false,
        anthropic_enable_openrouter_reasoning: false,
    };

    apply_upstream_model_override(&mut payload, &route_target);

    assert_eq!(payload["model"], "openrouter/sonnet-model");
}

#[test]
fn apply_upstream_model_override_falls_back_to_generic_model() {
    let mut payload = json!({
        "model": "claude-sonnet-4-6",
        "messages": [{"role":"user","content":"hi"}]
    });

    let route_target = RouteTarget {
        router_name: "claude".to_string(),
        upstream_url: "http://localhost:8080/v1/chat/completions".to_string(),
        upstream_wire: WireApi::Chat,
        upstream_model: Some("openrouter/default-model".to_string()),
        upstream_model_opus: Some("openrouter/opus-model".to_string()),
        upstream_model_sonnet: None,
        upstream_model_haiku: None,
        upstream_http_headers: Vec::new(),
        forward_incoming_headers: Vec::new(),
        drop_tool_types: HashSet::new(),
        drop_request_fields: HashSet::new(),
        feature_flags: FeatureFlags::default(),
        anthropic_preserve_thinking: false,
        anthropic_enable_openrouter_reasoning: false,
    };

    apply_upstream_model_override(&mut payload, &route_target);

    assert_eq!(payload["model"], "openrouter/default-model");
}

#[test]
fn apply_upstream_model_override_ignores_family_overrides_for_non_claude_models() {
    let mut payload = json!({
        "model": "gpt-4.1",
        "messages": [{"role":"user","content":"hi"}]
    });

    let route_target = RouteTarget {
        router_name: "default".to_string(),
        upstream_url: "http://localhost:8080/v1/chat/completions".to_string(),
        upstream_wire: WireApi::Chat,
        upstream_model: Some("openrouter/default-model".to_string()),
        upstream_model_opus: Some("openrouter/opus-model".to_string()),
        upstream_model_sonnet: Some("openrouter/sonnet-model".to_string()),
        upstream_model_haiku: Some("openrouter/haiku-model".to_string()),
        upstream_http_headers: Vec::new(),
        forward_incoming_headers: Vec::new(),
        drop_tool_types: HashSet::new(),
        drop_request_fields: HashSet::new(),
        feature_flags: FeatureFlags::default(),
        anthropic_preserve_thinking: false,
        anthropic_enable_openrouter_reasoning: false,
    };

    apply_upstream_model_override(&mut payload, &route_target);

    assert_eq!(payload["model"], "openrouter/default-model");
}

#[test]
fn normalize_unsupported_chat_message_roles_rewrites_provider_rejected_roles() {
    let mut payload = json!({
        "model": "gpt-4.1",
        "messages": [
            {"role":"user","content":"first user"},
            {"role":"developer","content":"developer instructions"},
            {"role":"tool","tool_call_id":"call_1","content":"tool output"},
            {"role":"function","name":"legacy_function","content":"function output"},
            {"role":"system","content":"later system"},
            {"role":"user","content":"hi"}
        ]
    });

    normalize_unsupported_chat_message_roles(&mut payload);

    let messages = payload["messages"].as_array().expect("messages");
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(
        messages[0]["content"],
        "developer instructions\n\nlater system"
    );
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "first user");
    assert_eq!(messages[2]["role"], "user");
    assert!(messages[2].get("tool_call_id").is_none());
    assert_eq!(messages[3]["role"], "user");
    assert!(messages[3].get("name").is_none());
    assert_eq!(messages[4]["role"], "user");
}

#[test]
fn inject_openrouter_reasoning_sets_enabled_without_clobbering_other_fields() {
    let mut payload = json!({
        "model": "claude-sonnet-4-6",
        "reasoning": {"effort": "medium"}
    });

    inject_openrouter_reasoning(&mut payload);

    assert_eq!(payload["reasoning"]["effort"], "medium");
    assert_eq!(payload["reasoning"]["enabled"], true);
}

#[test]
fn inject_openrouter_reasoning_creates_reasoning_field_when_missing() {
    let mut payload = json!({
        "model": "claude-sonnet-4-6"
    });

    inject_openrouter_reasoning(&mut payload);

    assert_eq!(payload["reasoning"]["enabled"], true);
}

#[test]
fn anthropic_request_enables_thinking_detects_enabled_payloads() {
    assert!(anthropic_request_enables_thinking(&json!({
        "thinking": {"type": "enabled", "budget_tokens": 2048}
    })));
    assert!(anthropic_request_enables_thinking(&json!({
        "thinking": {"type": "adaptive", "budget_tokens": 2048}
    })));
    assert!(anthropic_request_enables_thinking(&json!({
        "thinking": {"enabled": true}
    })));
    assert!(anthropic_request_enables_thinking(&json!({
        "thinking": true
    })));
}

#[test]
fn anthropic_request_enables_thinking_rejects_disabled_payloads() {
    assert!(!anthropic_request_enables_thinking(&json!({
        "thinking": {"type": "disabled"}
    })));
    assert!(!anthropic_request_enables_thinking(&json!({
        "thinking": {"enabled": false}
    })));
    assert!(!anthropic_request_enables_thinking(&json!({})));
}

#[test]
fn estimate_anthropic_count_tokens_ignores_assistant_thinking_blocks() {
    let base = json!({
        "messages": [
            {"role": "assistant", "content": []},
            {"role": "user", "content": "hello there"}
        ]
    });
    let with_thinking = json!({
        "messages": [
            {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "internal chain"}
                ]
            },
            {"role": "user", "content": "hello there"}
        ]
    });

    assert_eq!(
        estimate_anthropic_count_tokens(&base),
        estimate_anthropic_count_tokens(&with_thinking)
    );
}

#[tokio::test]
async fn anthropic_json_error_response_includes_request_id() {
    let response = anthropic_json_error_response("invalid_request_error", "boom");
    let body = response.into_body();
    let bytes = to_bytes(body, usize::MAX).await.expect("body");
    let json: Value = serde_json::from_slice(&bytes).expect("json");

    assert_eq!(json["type"], "error");
    assert_eq!(json["error"]["type"], "invalid_request_error");
    assert_eq!(json["error"]["message"], "boom");
    assert!(
        json["request_id"]
            .as_str()
            .is_some_and(|v| v.starts_with("req_"))
    );
}

#[test]
fn router_manager_collects_multiple_listen_ports_from_incoming_urls() {
    let mut routers = BTreeMap::new();
    routers.insert(
        "one".to_string(),
        RouterConfig {
            incoming_url: Some("http://127.0.0.1:8787/one".to_string()),
            ..Default::default()
        },
    );
    routers.insert(
        "two".to_string(),
        RouterConfig {
            incoming_url: Some("http://127.0.0.1:8788/two".to_string()),
            ..Default::default()
        },
    );

    let manager = RouterManager::new(
        routers,
        "https://api.openai.com/v1/chat/completions".to_string(),
        WireApi::Chat,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        FeatureFlags::default(),
    )
    .expect("manager");

    assert_eq!(
        manager.get_listen_addrs(),
        vec!["127.0.0.1:8787".to_string(), "127.0.0.1:8788".to_string()]
    );
}

#[test]
fn router_snapshot_flags_non_local_incoming_host() {
    let mut routers = BTreeMap::new();
    routers.insert(
        "shared".to_string(),
        RouterConfig {
            incoming_url: Some("http://0.0.0.0:8787/shared".to_string()),
            ..Default::default()
        },
    );

    let manager = RouterManager::new(
        routers,
        "https://api.openai.com/v1/chat/completions".to_string(),
        WireApi::Chat,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        FeatureFlags::default(),
    )
    .expect("manager");

    let snapshots = manager.get_router_delta_log_snapshots();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(
        snapshots[0].non_local_incoming_host,
        Some("0.0.0.0".to_string())
    );
}

#[test]
fn router_manager_rejects_router_without_incoming_url() {
    let mut routers = BTreeMap::new();
    routers.insert(
        "missing_route".to_string(),
        RouterConfig {
            upstream_url: Some("http://upstream.local/v1/chat/completions".to_string()),
            ..Default::default()
        },
    );

    let result = RouterManager::new(
        routers,
        "https://api.openai.com/v1/chat/completions".to_string(),
        WireApi::Chat,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        FeatureFlags::default(),
    );

    match result {
        Ok(_) => panic!("must fail when incoming_url is missing"),
        Err(err) => assert!(
            err.to_string()
                .contains("missing required incoming_url for [routers.missing_route]")
        ),
    }
}

#[test]
fn resolve_config_rejects_invalid_file_upstream_http_header_name() {
    let args = Args {
        config: None,
        upstream_url: None,
        upstream_wire: None,
        upstream_http_headers: vec![],
        forward_incoming_headers: vec![],
        api_key_env: None,
        server_info: None,
        http_shutdown: false,
        verbose_logging: false,
        drop_tool_types: vec![],
        router: None,
        list_routers: false,
    };
    let file = FileConfig {
        upstream_url: None,
        upstream_wire: None,
        upstream_http_headers: Some(BTreeMap::from([(
            "bad header".to_string(),
            "value".to_string(),
        )])),
        forward_incoming_headers: None,
        api_key_env: None,
        server_info: None,
        http_shutdown: None,
        verbose_logging: None,
        drop_tool_types: None,
        drop_request_fields: None,
        features: None,
        routers: None,
    };

    let err = resolve_config(args, Some(file)).expect_err("must fail");
    assert!(err.to_string().contains("invalid upstream header name"));
}

#[test]
fn apply_request_filters_drops_tool_type_for_chat_input() {
    let mut request = json!({
        "model": "gpt-4.1",
        "tools": [
            {"type":"web_search_preview"},
            {"type":"function","function":{"name":"f","parameters":{"type":"object"}}}
        ],
        "tool_choice": "auto"
    });
    let mut drop = HashSet::new();
    drop.insert("web_search_preview".to_string());

    apply_request_filters(IncomingApi::Chat, &mut request, &drop, &HashSet::new());
    let tools = request["tools"].as_array().expect("tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], "function");
}

#[test]
fn infer_incoming_api_from_prefixed_messages_path_returns_anthropic() {
    let request = json!({
        "model": "claude-sonnet-4-6",
        "messages": [{"role":"user","content":[{"type":"text","text":"hi"}]}],
        "stream": true
    });

    let incoming_api =
        infer_incoming_api_from_hint_or_path(None, Some("/claude/v1/messages"), &request);

    assert_eq!(incoming_api, IncomingApi::Anthropic);
}

#[test]
fn prefixed_anthropic_messages_path_maps_request_to_chat_payload() {
    let request = json!({
        "model": "claude-sonnet-4-6",
        "messages": [
            {"role":"user","content":[{"type":"text","text":"hi"}]}
        ],
        "system": [{"type":"text","text":"system prompt"}],
        "tools": [
            {
                "name": "shell",
                "description": "Run shell",
                "input_schema": {"type":"object","properties":{"cmd":{"type":"string"}}}
            }
        ],
        "stream": true,
        "max_tokens": 128
    });

    let incoming_api =
        infer_incoming_api_from_hint_or_path(None, Some("/claude/v1/messages"), &request);
    let out = build_upstream_payload(
        &request,
        incoming_api,
        WireApi::Chat,
        true,
        true,
        ToolTransformMode::LegacyConvert,
        false,
    )
    .expect("should map anthropic request");

    let messages = out
        .get("messages")
        .and_then(Value::as_array)
        .expect("messages array");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "system prompt");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "hi");
    assert!(out.get("system").is_none());

    let tools = out
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["function"]["name"], "shell");
}

#[test]
fn prefixed_anthropic_messages_path_preserves_thinking_content_when_enabled() {
    let request = json!({
        "model": "claude-sonnet-4-6",
        "messages": [
            {
                "role":"assistant",
                "content":[
                    {"type":"thinking","thinking":"internal chain"},
                    {"type":"text","text":"hello"}
                ]
            }
        ],
        "stream": false
    });

    let incoming_api =
        infer_incoming_api_from_hint_or_path(None, Some("/claude/v1/messages"), &request);
    let out = build_upstream_payload(
        &request,
        incoming_api,
        WireApi::Chat,
        false,
        true,
        ToolTransformMode::LegacyConvert,
        true,
    )
    .expect("should map anthropic request");

    let messages = out
        .get("messages")
        .and_then(Value::as_array)
        .expect("messages array");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "assistant");
    assert_eq!(
        messages[0]["content"],
        "<thinking>\ninternal chain\n</thinking>\n\nhello"
    );
    assert_eq!(messages[0].get("reasoning_content"), None);
}

#[test]
fn map_chat_to_responses_request_converts_messages() {
    let chat = json!({
        "model": "gpt-4.1",
        "messages": [
            {"role":"user","content":"hello"}
        ],
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
    assert_eq!(out["model"], "gpt-4.1");
    assert_eq!(out["stream"], false);
    assert_eq!(out["input"][0]["type"], "message");
    assert_eq!(out["input"][0]["content"][0]["text"], "hello");
    assert_eq!(out["text"]["format"]["type"], "json_schema");
    assert_eq!(out["text"]["format"]["json_schema"]["name"], "answer");
}

#[test]
fn map_chat_to_responses_request_normalizes_missing_type_named_tool() {
    let chat = json!({
        "model": "gpt-4.1",
        "messages": [
            {"role":"user","content":"hello"}
        ],
        "tools": [
            {
                "name": "spawn_agent",
                "description": "Spawn a sub-agent"
            }
        ]
    });

    let out = map_chat_to_responses_request(&chat, false).expect("ok");
    assert_eq!(
        out["tools"][0],
        json!({
            "type": "function",
            "name": "spawn_agent",
            "description": "Spawn a sub-agent",
            "parameters": {
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }
        })
    );
}

#[test]
fn map_chat_to_responses_request_normalizes_top_level_function_tool() {
    let chat = json!({
        "model": "gpt-4.1",
        "messages": [
            {"role":"user","content":"hello"}
        ],
        "tools": [
            {
                "type": "function",
                "name": "spawn_agent",
                "description": "Spawn a sub-agent"
            }
        ]
    });

    let out = map_chat_to_responses_request(&chat, false).expect("ok");
    assert_eq!(out["tools"][0]["type"], "function");
    assert_eq!(out["tools"][0]["name"], "spawn_agent");
    assert!(out["tools"][0].get("function").is_none());
    assert_eq!(out["tools"][0]["parameters"]["required"], json!([]));
}

fn responses_input_items(out: &Value) -> &[Value] {
    out.get("input")
        .and_then(Value::as_array)
        .expect("input array")
}

fn responses_input_item<'a>(items: &'a [Value], item_type: &str) -> Option<&'a Value> {
    items
        .iter()
        .find(|item| item.get("type").and_then(Value::as_str) == Some(item_type))
}

#[test]
fn map_chat_to_responses_request_preserves_assistant_tool_calls() {
    let chat = json!({
        "model": "gpt-4.1",
        "messages": [
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"city\":\"seoul\"}"
                        }
                    }
                ]
            }
        ],
        "stream": false
    });

    let out = map_chat_to_responses_request(&chat, false).expect("ok");
    let items = responses_input_items(&out);
    let item = responses_input_item(items, "function_call").expect("function_call item");
    assert_eq!(item["call_id"], "call_1");
    assert_eq!(item["name"], "get_weather");
    assert_eq!(item["arguments"], "{\"city\":\"seoul\"}");
}

#[test]
fn map_chat_to_responses_request_generates_unique_tool_call_ids_when_missing() {
    let chat = json!({
        "model": "gpt-4.1",
        "messages": [
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "type": "function",
                        "function": {
                            "name": "tool_a",
                            "arguments": "{}"
                        }
                    },
                    {
                        "type": "function",
                        "function": {
                            "name": "tool_b",
                            "arguments": "{\"x\":1}"
                        }
                    }
                ]
            }
        ],
        "stream": false
    });

    let out = map_chat_to_responses_request(&chat, false).expect("ok");
    let items = responses_input_items(&out);
    let first = responses_input_item(items, "function_call").expect("first function_call item");
    assert_eq!(first["name"], "tool_a");
    assert_eq!(first["call_id"], "call_m0_t0");

    let second = items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
        .nth(1)
        .expect("second function_call item");
    assert_eq!(second["name"], "tool_b");
    assert_eq!(second["call_id"], "call_m0_t1");
    assert_ne!(first["call_id"], second["call_id"]);
}

#[test]
fn map_chat_to_responses_request_preserves_assistant_reasoning() {
    let chat = json!({
        "model": "gpt-4.1",
        "messages": [
            {
                "role": "assistant",
                "content": "final answer",
                "reasoning_content": "internal chain"
            }
        ],
        "stream": false
    });

    let out = map_chat_to_responses_request(&chat, false).expect("ok");
    let items = responses_input_items(&out);
    let item = responses_input_item(items, "reasoning").expect("reasoning item");
    assert_eq!(item["summary"][0]["type"], "summary_text");
    assert_eq!(item["summary"][0]["text"], "internal chain");
}

#[test]
fn map_chat_to_responses_request_preserves_multimodal_content() {
    let chat = json!({
        "model": "gpt-4.1",
        "messages": [
            {
                "role": "user",
                "content": [
                    {"type":"text","text":"hello"},
                    {"type":"image_url","image_url":{"url":"https://example.com/cat.png"}},
                    {"type":"input_audio","input_audio":{"data":"aGVsbG8="}}
                ]
            }
        ],
        "stream": false
    });

    let out = map_chat_to_responses_request(&chat, false).expect("ok");
    let items = responses_input_items(&out);
    let message = responses_input_item(items, "message").expect("message item");
    let content = message["content"].as_array().expect("content array");
    assert_eq!(content[0]["type"], "input_text");
    assert_eq!(content[0]["text"], "hello");
    assert_eq!(content[1]["type"], "input_image");
    assert_eq!(content[1]["image_url"], "https://example.com/cat.png");
    assert_eq!(content[2]["type"], "input_audio");
    assert_eq!(content[2]["input_audio"]["data"], "aGVsbG8=");
}

#[test]
fn map_chat_to_responses_request_maps_tool_messages_to_function_call_output() {
    let chat = json!({
        "model": "gpt-4.1",
        "messages": [
            {
                "role": "tool",
                "tool_call_id": "call_2",
                "content": "{\"ok\":true}"
            }
        ],
        "stream": false
    });

    let out = map_chat_to_responses_request(&chat, false).expect("ok");
    let items = responses_input_items(&out);
    let item =
        responses_input_item(items, "function_call_output").expect("function_call_output item");
    assert_eq!(item["call_id"], "call_2");
    assert_eq!(item["output"], "{\"ok\":true}");
}

#[test]
fn map_chat_to_responses_request_preserves_mixed_assistant_content_and_tool_calls() {
    let chat = json!({
        "model": "gpt-4.1",
        "messages": [
            {
                "role": "assistant",
                "content": "I will check that now.",
                "tool_calls": [
                    {
                        "id": "call_3",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"city\":\"seoul\"}"
                        }
                    }
                ]
            }
        ],
        "stream": false
    });

    let out = map_chat_to_responses_request(&chat, false).expect("ok");
    let items = responses_input_items(&out);
    let message = responses_input_item(items, "message").expect("assistant message");
    assert_eq!(message["role"], "assistant");
    assert_eq!(message["content"][0]["text"], "I will check that now.");

    let tool_call = responses_input_item(items, "function_call").expect("function_call item");
    assert_eq!(tool_call["call_id"], "call_3");
    assert_eq!(tool_call["name"], "get_weather");
}

#[test]
fn previous_response_id_for_request_ignores_blank_values() {
    let request = json!({
        "previous_response_id": "   "
    });
    assert_eq!(previous_response_id_for_request(&request), None);

    let request = json!({
        "previous_response_id": "resp_123"
    });
    assert_eq!(previous_response_id_for_request(&request), Some("resp_123"));
}

#[test]
fn resolve_previous_messages_for_request_errors_when_not_found() {
    let request = json!({
        "previous_response_id": "resp_missing"
    });
    let sessions = SessionStore::default();
    let err = resolve_previous_messages_for_request(&request, &sessions).expect_err("must fail");
    assert!(err.to_string().contains("unknown `previous_response_id`"));
}

#[test]
fn merge_previous_messages_prepends_history_messages() {
    let mut payload = json!({
        "messages": [
            {"role":"user","content":"new"}
        ]
    });
    let previous = vec![
        json!({"role":"system","content":"old-system"}),
        json!({"role":"user","content":"old-user"}),
    ];

    merge_previous_messages(&mut payload, previous).expect("ok");

    let messages = payload["messages"].as_array().expect("array");
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["content"], "old-system");
    assert_eq!(messages[1]["content"], "old-user");
    assert_eq!(messages[2]["content"], "new");
}

#[test]
fn upstream_messages_for_logging_reads_chat_messages() {
    let payload = json!({
        "model": "gpt-4.1",
        "messages": [
            {"role":"user","content":"hi"},
            {"role":"assistant","content":"hello"}
        ]
    });

    let out = upstream_messages_for_logging(WireApi::Chat, &payload).expect("messages");
    let messages = out.as_array().expect("array");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "user");
}

#[test]
fn upstream_messages_for_logging_filters_responses_input_items() {
    let payload = json!({
        "model": "gpt-4.1",
        "input": [
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
            {"type":"function_call_output","call_id":"call_1","output":"ok"}
        ]
    });

    let out = upstream_messages_for_logging(WireApi::Responses, &payload).expect("messages");
    let messages = out.as_array().expect("array");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["type"], "message");
    assert_eq!(messages[0]["role"], "user");
}

#[test]
fn tool_types_for_logging_includes_type_and_name_labels() {
    let payload = json!({
        "tools": [
            {"type":"function","name":"f1"},
            {"type":"function","function":{"name":"f2"}},
            {"type":"custom","name":"shell"},
            {"type":"web_search_preview"},
            {"name":"missing_type"},
            {}
        ]
    });

    let out = tool_types_for_logging(&payload);
    assert_eq!(
        out,
        json!([
            "function(f1)",
            "function(f2)",
            "custom(shell)",
            "web_search_preview",
            "<missing_type>(missing_type)",
            "<missing_type>"
        ])
    );
}

#[test]
fn tool_definitions_for_logging_defaults_required_and_normalizes_null() {
    let payload = json!({
        "tools": [
            {"type":"function","function":{"name":"missing","parameters":{"type":"object"}}},
            {"type":"function","function":{"name":"null","parameters":{"type":"object","required":null}}},
            {"type":"function","function":{"name":"spawn_agent"}}
        ]
    });

    let out = tool_definitions_for_logging(&payload);
    assert_eq!(out[0]["required"], json!([]));
    assert_eq!(out[1]["required"], json!([]));
    assert_eq!(
        out[2],
        json!({
            "name": "spawn_agent",
            "required": [],
            "additionalProperties": false
        })
    );
}

#[test]
fn request_fields_for_logging_sorts_top_level_keys() {
    let payload = json!({
        "stream": true,
        "model": "gpt-4.1",
        "input": []
    });

    let out = request_fields_for_logging(&payload);
    assert_eq!(out, json!(["input", "model", "stream"]));
}

#[test]
fn split_log_chunks_splits_large_ascii_payloads() {
    let chunks = split_log_chunks_by_bytes("abcdef", 2);

    assert_eq!(chunks, vec!["ab", "cd", "ef"]);
}

#[test]
fn split_log_chunks_keeps_utf8_boundaries() {
    let input = "가나다";
    let chunks = split_log_chunks_by_bytes(input, 4);

    assert_eq!(chunks, vec!["가", "나", "다"]);
    assert_eq!(chunks.join(""), input);
}

#[test]
fn upstream_headers_for_logging_includes_forwarded_headers() {
    let mut headers = HeaderMap::new();
    headers.insert("openai-organization", HeaderValue::from_static("org_123"));
    headers.insert("x-client-request-id", HeaderValue::from_static("trace_123"));
    headers.insert("x-openai-subagent", HeaderValue::from_static("subagent_1"));
    let configured_headers = vec![
        UpstreamHeader {
            name: "x-custom-header".to_string(),
            value: "hello".to_string(),
        },
        UpstreamHeader {
            name: "authorization".to_string(),
            value: "Bearer secret".to_string(),
        },
    ];
    let forwarded_headers = vec![
        "openai-organization".to_string(),
        "x-client-request-id".to_string(),
        "x-openai-subagent".to_string(),
    ];

    let out =
        upstream_headers_for_logging(&headers, "sk-test", &configured_headers, &forwarded_headers);
    assert_eq!(out["authorization"], "<redacted>");
    assert_eq!(out["content-type"], "application/json");
    assert_eq!(out["openai-organization"], "org_123");
    assert_eq!(out["x-client-request-id"], "trace_123");
    assert_eq!(out["x-openai-subagent"], "subagent_1");
    assert_eq!(out["x-custom-header"], "hello");
}

#[test]
fn upstream_headers_for_logging_marks_empty_api_key() {
    let headers = HeaderMap::new();

    let out = upstream_headers_for_logging(&headers, "", &[], &[]);
    assert_eq!(out["authorization"], "Bearer <empty>");
    assert_eq!(out["content-type"], "application/json");
}

#[test]
fn headers_for_logging_redacts_sensitive_headers() {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", HeaderValue::from_static("Bearer sk-test"));
    headers.insert("cookie", HeaderValue::from_static("sid=abc"));
    headers.insert("x-trace-id", HeaderValue::from_static("trace-1"));

    let out = headers_for_logging(&headers);
    assert_eq!(out["authorization"], "<redacted>");
    assert_eq!(out["cookie"], "<redacted>");
    assert_eq!(out["x-trace-id"], "trace-1");
}

#[test]
fn capability_gate_allows_extended_responses_fields() {
    let request = json!({
        "model": "gpt-4.1",
        "input": [],
        "reasoning": {"effort":"medium"},
        "include": ["reasoning.encrypted_content"],
        "text": {"format":{"type":"json_object"}},
        "service_tier": "default"
    });

    let out = validate_capability_gate(IncomingApi::Responses, WireApi::Chat, true, &request);
    assert!(out.is_ok());
}

#[test]
fn capability_gate_rejects_invalid_max_output_tokens_type() {
    let request = json!({
        "model": "gpt-4.1",
        "input": [],
        "max_output_tokens": "bad"
    });

    let err = validate_capability_gate(IncomingApi::Responses, WireApi::Chat, true, &request)
        .expect_err("must fail");
    assert!(err.to_string().contains("max_output_tokens"));
}

#[test]
fn capability_gate_rejects_invalid_metadata_type() {
    let request = json!({
        "model": "gpt-4.1",
        "input": [],
        "metadata": ["bad"]
    });

    let err = validate_capability_gate(IncomingApi::Responses, WireApi::Chat, true, &request)
        .expect_err("must fail");
    assert!(err.to_string().contains("metadata"));
}

#[test]
fn capability_gate_allows_basic_responses_request() {
    let request = json!({
        "model": "gpt-4.1",
        "input": [{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],
        "tools": [{"type":"function","name":"f","parameters":{"type":"object"}}]
    });

    let out = validate_capability_gate(IncomingApi::Responses, WireApi::Chat, true, &request);
    assert!(out.is_ok());
}

#[test]
fn capability_gate_allows_prompt_cache_key() {
    let request = json!({
        "model": "gpt-4.1",
        "input": [{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],
        "prompt_cache_key": "session-key-1"
    });

    let out = validate_capability_gate(IncomingApi::Responses, WireApi::Chat, true, &request);
    assert!(out.is_ok());
}

#[test]
fn capability_gate_allows_mcp_and_web_search_tool_types() {
    let request = json!({
        "model": "gpt-4.1",
        "input": [{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],
        "tools": [
            {"type":"mcp","server_label":"s","server_url":"http://localhost/mcp"},
            {"type":"web_search"},
            {"type":"web_search_preview"}
        ]
    });

    let out = validate_capability_gate(IncomingApi::Responses, WireApi::Chat, true, &request);
    assert!(out.is_ok());
}

#[test]
fn normalize_upstream_error_maps_known_codes() {
    let payload = r#"{"error":{"code":"context_length_exceeded","message":"too long"}}"#;
    let err = normalize_upstream_error_payload(StatusCode::BAD_REQUEST, payload);
    assert_eq!(err.code, "context_window_exceeded");
    assert_eq!(err.message, "too long");
}

#[test]
fn build_upstream_request_prefers_static_header_on_duplicate_key() {
    let router_manager = RouterManager::new(
        BTreeMap::new(),
        "http://localhost:8080/v1/chat/completions".to_string(),
        WireApi::Chat,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        FeatureFlags::default(),
    )
    .expect("router manager");
    let state = Arc::new(AppState {
        client: Client::new(),
        api_key: "test-key".to_string(),
        http_shutdown: false,
        verbose_logging: false,
        routers: Arc::new(tokio::sync::RwLock::new(router_manager)),
        sessions: Arc::new(tokio::sync::RwLock::new(SessionStore::default())),
        last_successful_upstream_log: Arc::new(tokio::sync::RwLock::new(None)),
    });
    let route_target = RouteTarget {
        router_name: "default".to_string(),
        upstream_url: "http://localhost:8080/v1/chat/completions".to_string(),
        upstream_wire: WireApi::Chat,
        upstream_model: None,
        upstream_model_opus: None,
        upstream_model_sonnet: None,
        upstream_model_haiku: None,
        upstream_http_headers: vec![UpstreamHeader {
            name: "x-request-id".to_string(),
            value: "from-upstream-http-headers".to_string(),
        }],
        forward_incoming_headers: vec![
            "x-request-id".to_string(),
            "x-client-request-id".to_string(),
        ],
        drop_tool_types: HashSet::new(),
        drop_request_fields: HashSet::new(),
        feature_flags: FeatureFlags::default(),
        anthropic_preserve_thinking: false,
        anthropic_enable_openrouter_reasoning: false,
    };
    let mut incoming_headers = HeaderMap::new();
    incoming_headers.insert(
        "x-request-id",
        HeaderValue::from_static("from-forward-incoming-headers"),
    );
    incoming_headers.insert(
        "x-client-request-id",
        HeaderValue::from_static("client-trace-123"),
    );

    let request = build_upstream_request(
        &state,
        &route_target,
        &incoming_headers,
        &json!({"model":"gpt-4.1","messages":[]}),
    )
    .build()
    .expect("request");
    let values = request.headers().get_all("x-request-id");
    let collected = values.iter().collect::<Vec<_>>();

    assert_eq!(collected.len(), 1);
    assert_eq!(collected[0], "from-upstream-http-headers");
    assert_eq!(
        request
            .headers()
            .get("x-client-request-id")
            .and_then(|value| value.to_str().ok()),
        Some("client-trace-123")
    );
}

fn test_state_with_router(
    incoming_url: &str,
    upstream_url: &str,
    upstream_wire: WireApi,
) -> Arc<AppState> {
    let mut routers = BTreeMap::new();
    routers.insert(
        "default".to_string(),
        RouterConfig {
            incoming_url: Some(incoming_url.to_string()),
            upstream_url: Some(upstream_url.to_string()),
            upstream_wire: Some(upstream_wire),
            ..Default::default()
        },
    );
    let router_manager = RouterManager::new(
        routers,
        "https://api.openai.com/v1/chat/completions".to_string(),
        WireApi::Chat,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        FeatureFlags::default(),
    )
    .expect("router manager");

    Arc::new(AppState {
        client: Client::new(),
        api_key: "test-key".to_string(),
        http_shutdown: false,
        verbose_logging: true,
        routers: Arc::new(tokio::sync::RwLock::new(router_manager)),
        sessions: Arc::new(tokio::sync::RwLock::new(SessionStore::default())),
        last_successful_upstream_log: Arc::new(tokio::sync::RwLock::new(None)),
    })
}

#[derive(Clone)]
struct MockJsonUpstreamState {
    response: Value,
    captured_request: Arc<tokio::sync::Mutex<Option<Value>>>,
}

async fn mock_json_upstream_handler(
    AxumState(state): AxumState<MockJsonUpstreamState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    *state.captured_request.lock().await = Some(body);
    Json(state.response)
}

async fn spawn_mock_json_upstream(
    path: &str,
    response: Value,
) -> (
    String,
    tokio::task::JoinHandle<()>,
    Arc<tokio::sync::Mutex<Option<Value>>>,
) {
    let captured_request = Arc::new(tokio::sync::Mutex::new(None));
    let state = MockJsonUpstreamState {
        response,
        captured_request: captured_request.clone(),
    };
    let app = axum::Router::new()
        .route(path, post(mock_json_upstream_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let addr = listener.local_addr().expect("mock upstream address");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}{path}"), handle, captured_request)
}

#[tokio::test]
async fn direct_anthropic_endpoint_routes_using_request_path() {
    let app = build_app(test_state_with_router(
        "http://127.0.0.1:8787/v1/messages",
        "http://127.0.0.1:9/v1/chat/completions",
        WireApi::Chat,
    ));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("host", "127.0.0.1:8787")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"claude-sonnet","messages":[{"role":"user","content":"hi"}],"stream":false}"#,
                ))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: Value = serde_json::from_slice(&body).expect("json");

    assert_eq!(json["type"], "error");
    assert_eq!(json["error"]["type"], "upstream_transport_error");
}

#[tokio::test]
async fn anthropic_count_tokens_is_handled_locally_for_subpaths() {
    let app = build_app(test_state_with_router(
        "http://127.0.0.1:8787/claude/v1/messages",
        "http://127.0.0.1:9/v1/chat/completions",
        WireApi::Chat,
    ));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/claude/v1/messages/count_tokens")
                .header("host", "127.0.0.1:8787")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"claude-sonnet","messages":[{"role":"user","content":"hello there"}]}"#,
                ))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: Value = serde_json::from_slice(&body).expect("json");

    assert_eq!(json["input_tokens"].as_i64().is_some(), true);
    assert_eq!(json.get("error"), None);
}

#[tokio::test]
async fn direct_responses_endpoint_routes_using_request_path() {
    let app = build_app(test_state_with_router(
        "http://127.0.0.1:8787/v1/responses",
        "http://127.0.0.1:9/v1/responses",
        WireApi::Responses,
    ));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("host", "127.0.0.1:8787")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-4.1","stream":false,"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#,
                ))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: Value = serde_json::from_slice(&body).expect("json");

    assert_eq!(json["error"]["type"], "upstream_transport_error");
}

#[tokio::test]
#[ignore = "requires binding a local TCP listener"]
async fn responses_request_to_chat_upstream_returns_responses_json() {
    let (upstream_url, upstream_handle, captured_request) = spawn_mock_json_upstream(
        "/v1/chat/completions",
        json!({
            "id": "chatcmpl_1",
            "model": "gpt-4.1",
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": "hello"
                }
            }],
            "usage": {
                "prompt_tokens": 1,
                "completion_tokens": 2,
                "total_tokens": 3
            }
        }),
    )
    .await;
    let app = build_app(test_state_with_router(
        "http://127.0.0.1:8787/v1/responses",
        &upstream_url,
        WireApi::Chat,
    ));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("host", "127.0.0.1:8787")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-4.1","stream":false,"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#,
                ))
                .expect("request"),
        )
        .await
        .expect("response");

    upstream_handle.abort();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: Value = serde_json::from_slice(&body).expect("json");
    let captured = captured_request
        .lock()
        .await
        .clone()
        .expect("captured request");

    assert_eq!(captured["messages"][0]["role"], "user");
    assert_eq!(json["object"], "response");
    assert_eq!(json["output"][0]["content"][0]["text"], "hello");
    assert_eq!(json["usage"]["input_tokens"], 1);
}

#[tokio::test]
#[ignore = "requires binding a local TCP listener"]
async fn chat_request_to_responses_upstream_returns_chat_json() {
    let (upstream_url, upstream_handle, captured_request) = spawn_mock_json_upstream(
        "/v1/responses",
        json!({
            "id": "resp_1",
            "model": "gpt-4.1",
            "status": "completed",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type":"output_text","text":"hello chat"}]
            }],
            "usage": {
                "input_tokens": 4,
                "output_tokens": 5,
                "total_tokens": 9
            }
        }),
    )
    .await;
    let app = build_app(test_state_with_router(
        "http://127.0.0.1:8787/v1/chat/completions",
        &upstream_url,
        WireApi::Responses,
    ));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("host", "127.0.0.1:8787")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-4.1","messages":[{"role":"user","content":"hi"}],"stream":false}"#,
                ))
                .expect("request"),
        )
        .await
        .expect("response");

    upstream_handle.abort();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: Value = serde_json::from_slice(&body).expect("json");
    let captured = captured_request
        .lock()
        .await
        .clone()
        .expect("captured request");

    assert_eq!(captured["input"][0]["type"], "message");
    assert_eq!(json["object"], "chat.completion");
    assert_eq!(json["choices"][0]["message"]["content"], "hello chat");
    assert_eq!(json["usage"]["prompt_tokens"], 4);
}

#[tokio::test]
async fn stream_emits_failed_when_done_marker_missing() {
    let upstream = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
    ))]);
    let mut output = Box::pin(translate_chat_stream(
        upstream,
        "resp_1".to_string(),
        "test_router".to_string(),
        false,
        HashMap::new(),
        FeatureFlags::default(),
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    assert!(payload.contains("\"type\":\"response.failed\""));
    assert!(!payload.contains("event: response.completed"));
}

#[tokio::test]
async fn stream_emits_added_and_done_for_tool_calls() {
    let upstream = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"shell\",\"arguments\":\"echo hi\"}}]}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n\
             data: [DONE]\n\n",
    ))]);
    let mut kinds = HashMap::new();
    kinds.insert("shell".to_string(), ResponsesToolCallKind::Custom);
    let mut output = Box::pin(translate_chat_stream(
        upstream,
        "resp_1".to_string(),
        "test_router".to_string(),
        false,
        kinds,
        FeatureFlags::default(),
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    let added_idx = payload
        .find("event: response.output_item.added")
        .expect("added event");
    let done_idx = payload
        .rfind("event: response.output_item.done")
        .expect("done event");
    assert!(added_idx < done_idx);
    assert!(payload.contains("\"call_id\":\"call_resp_1_0\""));
    assert!(payload.contains("event: response.function_call_arguments.delta"));
    assert!(payload.contains("event: response.function_call_arguments.done"));
    assert!(payload.contains("\"arguments\":\"echo hi\""));
}

#[tokio::test]
async fn stream_emits_reasoning_summary_events() {
    let upstream = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think \"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"step\"}}]}\n\n\
             data: [DONE]\n\n",
    ))]);
    let mut output = Box::pin(translate_chat_stream(
        upstream,
        "resp_1".to_string(),
        "test_router".to_string(),
        false,
        HashMap::new(),
        FeatureFlags::default(),
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    assert!(payload.contains("event: response.reasoning_summary_text.delta"));
    assert!(payload.contains("event: response.reasoning_summary_text.done"));
    assert!(payload.contains("\"text\":\"think step\""));
    assert!(!payload.contains("[reasoning_summary]"));
}

#[tokio::test]
async fn anthropic_stream_message_start_uses_model_and_input_tokens_without_null_stop_fields() {
    let upstream = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"},\"finish_reason\":\"stop\"}]}\n\n\
             data: [DONE]\n\n",
    ))]);
    let mut output = Box::pin(translate_chat_stream_to_anthropic(
        upstream,
        "test_router".to_string(),
        false,
        "gpt-4.1".to_string(),
        17,
    ));
    let first = output
        .next()
        .await
        .expect("message_start event")
        .expect("stream event");
    let payload = String::from_utf8_lossy(&first);

    assert!(payload.contains("event: message_start"));
    assert!(payload.contains("\"model\":\"gpt-4.1\""));
    assert!(payload.contains("\"input_tokens\":17"));
    assert!(!payload.contains("\"stop_reason\":null"));
    assert!(!payload.contains("\"stop_sequence\":null"));
}

#[tokio::test]
async fn anthropic_stream_maps_native_tool_call_delta() {
    let upstream = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"shell\",\"arguments\":\"{\\\"cmd\\\":\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"pwd\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n\
             data: [DONE]\n\n",
    ))]);
    let mut output = Box::pin(translate_chat_stream_to_anthropic(
        upstream,
        "test_router".to_string(),
        false,
        "gpt-4.1".to_string(),
        0,
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    assert!(payload.contains("\"type\":\"tool_use\""));
    assert!(payload.contains("\"id\":\"call_1\""));
    assert!(payload.contains("\"name\":\"shell\""));
    assert!(payload.contains("\"partial_json\":\"{\\\"cmd\\\":\""));
    assert!(payload.contains("\"partial_json\":\"\\\"pwd\\\"}\""));
    assert!(payload.contains("\"stop_reason\":\"tool_use\""));
}

#[tokio::test]
async fn anthropic_stream_handles_split_think_tags_across_chunks() {
    let upstream = stream::iter(vec![
        Ok::<Bytes, reqwest::Error>(Bytes::from(
            "data: {\"choices\":[{\"delta\":{\"content\":\"before <thi\"}}]}\n\n",
        )),
        Ok::<Bytes, reqwest::Error>(Bytes::from(
            "data: {\"choices\":[{\"delta\":{\"content\":\"nk>internal</thi\"}}]}\n\n",
        )),
        Ok::<Bytes, reqwest::Error>(Bytes::from(
            "data: {\"choices\":[{\"delta\":{\"content\":\"nk> after\"},\"finish_reason\":\"stop\"}]}\n\n\
             data: [DONE]\n\n",
        )),
    ]);
    let mut output = Box::pin(translate_chat_stream_to_anthropic(
        upstream,
        "test_router".to_string(),
        false,
        "claude-bridge".to_string(),
        0,
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    assert!(payload.contains("\"text\":\"before <thi\""));
    assert!(payload.contains("\"text\":\"nk>internal</thi\""));
    assert!(payload.contains("\"text\":\"nk> after\""));
    assert!(!payload.contains("\"type\":\"thinking_delta\""));
}

#[tokio::test]
async fn anthropic_stream_ignores_signature_delta_from_thinking_blocks() {
    let upstream = stream::iter(vec![
        Ok::<Bytes, reqwest::Error>(Bytes::from(
            "data: {\"choices\":[{\"delta\":{\"thinking_blocks\":[{\"type\":\"thinking\",\"thinking\":\"step one\",\"signature\":\"sig_1\"}]}}]}\n\n",
        )),
        Ok::<Bytes, reqwest::Error>(Bytes::from("data: [DONE]\n\n")),
    ]);
    let mut output = Box::pin(translate_chat_stream_to_anthropic(
        upstream,
        "test_router".to_string(),
        false,
        "claude-bridge".to_string(),
        0,
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    assert!(!payload.contains("\"type\":\"thinking_delta\""));
    assert!(!payload.contains("\"type\":\"signature_delta\""));
    assert_eq!(payload.matches("event: content_block_start").count(), 0);
    assert!(payload.contains("event: message_stop"));
}

#[tokio::test]
async fn anthropic_stream_ignores_redacted_thinking_blocks() {
    let upstream = stream::iter(vec![
        Ok::<Bytes, reqwest::Error>(Bytes::from(
            "data: {\"choices\":[{\"delta\":{\"thinking_blocks\":[{\"type\":\"redacted_thinking\",\"data\":\"secret\"}]}}]}\n\n",
        )),
        Ok::<Bytes, reqwest::Error>(Bytes::from("data: [DONE]\n\n")),
    ]);
    let mut output = Box::pin(translate_chat_stream_to_anthropic(
        upstream,
        "test_router".to_string(),
        false,
        "claude-bridge".to_string(),
        0,
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    assert!(!payload.contains("\"type\":\"redacted_thinking\""));
    assert_eq!(payload.matches("event: content_block_start").count(), 0);
    assert!(!payload.contains("response.failed"));
}

#[tokio::test]
async fn anthropic_stream_accepts_terminal_finish_reason_without_done_marker() {
    let upstream = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    ))]);
    let mut output = Box::pin(translate_chat_stream_to_anthropic(
        upstream,
        "test_router".to_string(),
        false,
        "claude-bridge".to_string(),
        0,
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    assert!(payload.contains("event: message_start"));
    assert_eq!(payload.matches("event: content_block_start").count(), 1);
    assert!(payload.contains("event: content_block_delta"));
    assert!(payload.contains("\"text\":\"Hi\""));
    assert_eq!(payload.matches("event: content_block_stop").count(), 1);
    assert!(payload.contains("event: message_delta"));
    assert!(payload.contains("event: message_stop"));
    assert!(!payload.contains("event: error"));
}

#[tokio::test]
async fn anthropic_stream_splits_think_tags_in_content() {
    let upstream = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
        "data: {\"choices\":[{\"delta\":{\"content\":\"before <think>internal</think> after\"}}]}\n\n\
             data: [DONE]\n\n",
    ))]);
    let mut output = Box::pin(translate_chat_stream_to_anthropic(
        upstream,
        "test_router".to_string(),
        false,
        "claude-bridge".to_string(),
        0,
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    assert!(payload.contains("\"type\":\"text_delta\""));
    assert!(payload.contains("\"text\":\"before <think>internal</think> after\""));
    assert!(!payload.contains("\"type\":\"thinking_delta\""));
    assert_eq!(payload.matches("event: content_block_start").count(), 1);
    assert_eq!(payload.matches("event: content_block_stop").count(), 1);
}

#[tokio::test]
async fn anthropic_stream_ignores_reasoning_details() {
    let upstream = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
        "data: {\"choices\":[{\"delta\":{\"reasoning_details\":[{\"text\":\"step one\"}]}}]}\n\n\
             data: [DONE]\n\n",
    ))]);
    let mut output = Box::pin(translate_chat_stream_to_anthropic(
        upstream,
        "test_router".to_string(),
        false,
        "claude-bridge".to_string(),
        0,
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    assert!(!payload.contains("\"thinking\":\"step one\""));
    assert_eq!(payload.matches("event: content_block_start").count(), 0);
    assert_eq!(payload.matches("event: content_block_stop").count(), 0);
    assert!(payload.contains("event: message_stop"));
}

#[tokio::test]
async fn anthropic_stream_keeps_heuristic_tool_call_text_as_text() {
    let upstream = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
        "data: {\"choices\":[{\"delta\":{\"content\":\"before shell(command='pwd') after\"}}]}\n\n\
             data: [DONE]\n\n",
    ))]);
    let mut output = Box::pin(translate_chat_stream_to_anthropic(
        upstream,
        "test_router".to_string(),
        false,
        "claude-bridge".to_string(),
        0,
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    assert!(payload.contains("\"text\":\"before shell(command='pwd') after\""));
    assert!(!payload.contains("\"type\":\"tool_use\""));
    assert!(payload.contains("\"stop_reason\":\"end_turn\""));
}

#[tokio::test]
async fn anthropic_stream_keeps_exit_plan_mode_python_call_as_text() {
    let upstream = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
        "data: {\"choices\":[{\"delta\":{\"content\":\"ExitPlanMode()\"}}]}\n\n\
             data: [DONE]\n\n",
    ))]);
    let mut output = Box::pin(translate_chat_stream_to_anthropic(
        upstream,
        "test_router".to_string(),
        false,
        "claude-bridge".to_string(),
        0,
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    assert!(payload.contains("\"text\":\"ExitPlanMode()\""));
    assert!(!payload.contains("\"type\":\"tool_use\""));
    assert!(payload.contains("\"stop_reason\":\"end_turn\""));
}

#[tokio::test]
async fn anthropic_stream_keeps_exit_plan_mode_harmony_call_as_text() {
    let upstream = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
        "data: {\"choices\":[{\"delta\":{\"content\":\"to=functions.ExitPlanMode<|constrain|>json<|message|>{}\"}}]}\n\n\
             data: [DONE]\n\n",
    ))]);
    let mut output = Box::pin(translate_chat_stream_to_anthropic(
        upstream,
        "test_router".to_string(),
        false,
        "claude-bridge".to_string(),
        0,
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    assert!(payload.contains("to=functions.ExitPlanMode"));
    assert!(payload.contains("<|constrain|>"));
    assert!(payload.contains("<|message|>"));
    assert!(!payload.contains("\"type\":\"tool_use\""));
    assert!(payload.contains("\"stop_reason\":\"end_turn\""));
}

#[tokio::test]
async fn anthropic_stream_keeps_unallowed_harmony_call_as_text() {
    let upstream = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
        "data: {\"choices\":[{\"delta\":{\"content\":\"to=functions.ExitPlanMode<|constrain|>json<|message|>{}\"},\"finish_reason\":\"stop\"}]}\n\n\
             data: [DONE]\n\n",
    ))]);
    let mut output = Box::pin(translate_chat_stream_to_anthropic(
        upstream,
        "test_router".to_string(),
        false,
        "claude-bridge".to_string(),
        0,
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    assert!(payload.contains("to=functions.ExitPlanMode"));
    assert!(payload.contains("<|constrain|>"));
    assert!(payload.contains("<|message|>"));
    assert!(!payload.contains("\"type\":\"tool_use\""));
    assert!(payload.contains("\"stop_reason\":\"end_turn\""));
}

#[tokio::test]
async fn anthropic_stream_preserves_control_tokens_in_text_content() {
    let upstream = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
        "data: {\"choices\":[{\"delta\":{\"content\":\"alpha<|message|>beta<|constrain|>gamma\"}}]}\n\n\
             data: [DONE]\n\n",
    ))]);
    let mut output = Box::pin(translate_chat_stream_to_anthropic(
        upstream,
        "test_router".to_string(),
        false,
        "claude-bridge".to_string(),
        0,
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    assert!(payload.contains("alpha<|message|>beta<|constrain|>gamma"));
    assert!(!payload.contains("\"type\":\"tool_use\""));
}

#[tokio::test]
async fn anthropic_stream_keeps_non_tool_thinking_trace_as_text() {
    let upstream = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
        "data: {\"choices\":[{\"delta\":{\"content\":\"● <thinking<|message|>Create next tasks.\\n\\n● <thinking<|message|>Create remaining tasks similarly.<thinking\\n\\nto=functions.TaskCreate<|constrain|>json<|message|>{\\\"description\\\":\\\"Create a guide\\\"}\"}}]}\n\n\
             data: [DONE]\n\n",
    ))]);
    let mut output = Box::pin(translate_chat_stream_to_anthropic(
        upstream,
        "test_router".to_string(),
        false,
        "claude-bridge".to_string(),
        0,
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    assert!(payload.contains("● <thinking"));
    assert!(payload.contains("Create next tasks."));
    assert!(payload.contains("Create remaining tasks similarly."));
    assert!(payload.contains("to=functions.TaskCreate"));
    assert!(payload.contains("<|mes"));
    assert!(payload.contains("sage|>"));
    assert!(payload.contains("<|con"));
    assert!(payload.contains("strain|>"));
    assert!(!payload.contains("\"type\":\"tool_use\""));
}

#[tokio::test]
async fn anthropic_stream_keeps_split_non_tool_thinking_trace_as_text() {
    let upstream = stream::iter(vec![
        Ok::<Bytes, reqwest::Error>(Bytes::from(
            "data: {\"choices\":[{\"delta\":{\"content\":\"● <thinking<|mes\"}}]}\n\n",
        )),
        Ok::<Bytes, reqwest::Error>(Bytes::from(
            "data: {\"choices\":[{\"delta\":{\"content\":\"sage|>Create next tasks. to=functions.TaskCreate<|con\"}}]}\n\n",
        )),
        Ok::<Bytes, reqwest::Error>(Bytes::from(
            "data: {\"choices\":[{\"delta\":{\"content\":\"strain|>json\"}}]}\n\n\
             data: [DONE]\n\n",
        )),
    ]);
    let mut output = Box::pin(translate_chat_stream_to_anthropic(
        upstream,
        "test_router".to_string(),
        false,
        "claude-bridge".to_string(),
        0,
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    assert!(payload.contains("\"text\":\"● <thinking"));
    assert!(payload.contains("Create next tasks."));
    assert!(payload.contains("to=functions.TaskCreate"));
    assert!(payload.contains("<|mes"));
    assert!(payload.contains("sage|>"));
    assert!(payload.contains("<|con"));
    assert!(payload.contains("strain|>"));
    assert!(!payload.contains("\"type\":\"tool_use\""));
}

#[tokio::test]
async fn anthropic_stream_keeps_bare_python_style_ask_user_question_as_text() {
    let upstream = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
        "data: {\"choices\":[{\"delta\":{\"content\":\"[AskUserQuestion(question=\\\"What's up?\\\", headers=['Hello!'], options=['A','B'], multiSelect=False, metadata={'priority': 1, 'active': True})]\"},\"finish_reason\":\"stop\"}]}\n\n\
             data: [DONE]\n\n",
    ))]);
    let mut output = Box::pin(translate_chat_stream_to_anthropic(
        upstream,
        "test_router".to_string(),
        false,
        "claude-bridge".to_string(),
        0,
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    assert!(payload.contains("AskUserQuestion(question="));
    assert!(!payload.contains("\"type\":\"tool_use\""));
    assert!(payload.contains("\"type\":\"text_delta\""));
}

#[tokio::test]
async fn anthropic_stream_keeps_ask_user_question_xml_wrapper_as_text() {
    let upstream = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
        "data: {\"choices\":[{\"delta\":{\"content\":\"<AskUserQuestion>{\\\"questions\\\":[{\\\"question\\\":\\\"작업 분할 계획을 저장할 파일 경로를 선택해 주세요.\\\",\\\"multiSelect\\\":false,\\\"options\\\":[{\\\"label\\\":\\\"docs/plans/unit-test-execution-plan.md (추천)\\\",\\\"description\\\":\\\"문서 전용 폴더에 마크다운 파일로 보관\\\"},{\\\"label\\\":\\\"README.md에 추가\\\",\\\"description\\\":\\\"프로젝트 루트 README에 삽입\\\"},{\\\"label\\\":\\\"src/main/resources/unit-test-execution-plan.md\\\",\\\"description\\\":\\\"리소스 경로에 저장해 빌드에 포함\\\"}]}]}\"},\"finish_reason\":\"stop\"}]}\n\n\
             data: [DONE]\n\n",
    ))]);
    let mut output = Box::pin(translate_chat_stream_to_anthropic(
        upstream,
        "test_router".to_string(),
        false,
        "claude-bridge".to_string(),
        0,
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    assert!(payload.contains("<AskUserQuestion>"));
    assert!(payload.contains("작업 분할 계획을 저장할 파일 경로를 선택해 주세요."));
    assert!(!payload.contains("\"type\":\"tool_use\""));
}

#[tokio::test]
async fn anthropic_stream_keeps_unknown_bare_python_style_call_as_text() {
    let upstream = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
        "data: {\"choices\":[{\"delta\":{\"content\":\"AskUserQuestion(question=\\\"What's up?\\\")\"},\"finish_reason\":\"stop\"}]}\n\n\
             data: [DONE]\n\n",
    ))]);
    let mut output = Box::pin(translate_chat_stream_to_anthropic(
        upstream,
        "test_router".to_string(),
        false,
        "claude-bridge".to_string(),
        0,
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    assert!(payload.contains("\"text\":\"AskUserQuestion(question=\\\"What's up?\\\")\""));
    assert!(!payload.contains("\"type\":\"tool_use\""));
}

#[tokio::test]
async fn anthropic_stream_handles_trailing_done_without_newline() {
    let upstream = stream::iter(vec![
        Ok::<Bytes, reqwest::Error>(Bytes::from(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
        )),
        Ok::<Bytes, reqwest::Error>(Bytes::from("data: [DONE]")),
    ]);
    let mut output = Box::pin(translate_chat_stream_to_anthropic(
        upstream,
        "test_router".to_string(),
        false,
        "claude-bridge".to_string(),
        0,
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    assert!(payload.contains("\"text\":\"Hi\""));
    assert_eq!(payload.matches("event: content_block_start").count(), 1);
    assert_eq!(payload.matches("event: content_block_stop").count(), 1);
    assert!(payload.contains("event: message_stop"));
    assert!(!payload.contains("event: error"));
}

#[tokio::test]
async fn anthropic_stream_opens_text_block_only_once_across_multiple_deltas() {
    let upstream = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
        "data: {\"choices\":[{\"delta\":{\"content\":\"He\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
             data: [DONE]\n\n",
    ))]);
    let mut output = Box::pin(translate_chat_stream_to_anthropic(
        upstream,
        "test_router".to_string(),
        false,
        "claude-bridge".to_string(),
        0,
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    assert_eq!(payload.matches("event: content_block_start").count(), 1);
    assert_eq!(payload.matches("\"type\":\"text_delta\"").count(), 2);
    assert_eq!(payload.matches("event: content_block_stop").count(), 1);
    assert!(payload.contains("\"text\":\"He\""));
    assert!(payload.contains("\"text\":\"llo\""));
}

#[tokio::test]
async fn anthropic_stream_ignores_empty_tool_calls_when_streaming_text() {
    let upstream = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
        "data: {\"choices\":[{\"delta\":{\"content\":\"He\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"llo\",\"tool_calls\":[]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"!\"},\"finish_reason\":\"stop\"}]}\n\n\
             data: [DONE]\n\n",
    ))]);
    let mut output = Box::pin(translate_chat_stream_to_anthropic(
        upstream,
        "test_router".to_string(),
        false,
        "claude-bridge".to_string(),
        0,
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    assert_eq!(payload.matches("event: content_block_start").count(), 1);
    assert_eq!(payload.matches("event: content_block_stop").count(), 1);
    assert_eq!(payload.matches("\"type\":\"text_delta\"").count(), 3);
    assert!(payload.contains("\"text\":\"He\""));
    assert!(payload.contains("\"text\":\"llo\""));
    assert!(payload.contains("\"text\":\"!\""));
}

#[tokio::test]
async fn anthropic_stream_opens_thinking_block_only_once_across_multiple_deltas() {
    let upstream = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from(
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"step \"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"one\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
             data: [DONE]\n\n",
    ))]);
    let mut output = Box::pin(translate_chat_stream_to_anthropic(
        upstream,
        "test_router".to_string(),
        false,
        "claude-bridge".to_string(),
        0,
    ));
    let mut payload = String::new();

    while let Some(event) = output.next().await {
        payload.push_str(&String::from_utf8_lossy(&event.expect("stream event")));
    }

    assert_eq!(payload.matches("event: content_block_start").count(), 1);
    assert_eq!(payload.matches("\"type\":\"thinking_delta\"").count(), 2);
    assert_eq!(payload.matches("event: content_block_stop").count(), 1);
    assert!(payload.contains("\"thinking\":\"step \""));
    assert!(payload.contains("\"thinking\":\"one\""));
}
