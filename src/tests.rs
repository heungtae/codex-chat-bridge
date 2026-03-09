    use super::*;
    use axum::body::Bytes;
    use futures::stream;
    use futures::StreamExt;
    use serde_json::json;
    use std::collections::HashSet;
    use std::path::PathBuf;

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

        let req = map_responses_to_chat_request_with_stream(&input, &HashSet::new(), true, true).expect("should map");
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
    fn sse_parser_collects_data_events() {
        let mut parser = SseParser::default();
        let chunk = "event: message\ndata: {\"a\":1}\n\n";
        let events = parser.feed(chunk);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0], "{\"a\":1}");
    }

    #[test]
    fn normalize_tool_choice_wraps_function_name() {
        let choice = json!({"type":"function", "name":"f"});
        let normalized = normalize_tool_choice(choice);
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
        let out = normalize_chat_tools(tools, &HashSet::new());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "function");
        assert_eq!(out[0]["function"]["name"], "web_search_preview");
    }

    #[test]
    fn normalize_chat_tools_converts_mcp_to_function() {
        let tools = vec![json!({"type": "mcp", "server_label": "shell"})];
        let out = normalize_chat_tools(tools, &HashSet::new());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "function");
        assert_eq!(out[0]["function"]["name"], "mcp__shell");
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

        let req = map_responses_to_chat_request_with_stream(&input, &HashSet::new(), true, true).expect("should map");
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

        let req = map_responses_to_chat_request_with_stream(&input, &HashSet::new(), true, true).expect("should map");
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

        let req = map_responses_to_chat_request_with_stream(&input, &HashSet::new(), true, true).expect("should map");
        let messages = req
            .chat_request
            .get("messages")
            .and_then(Value::as_array)
            .expect("messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["tool_calls"][0]["id"], "call_web_1");
        assert_eq!(messages[0]["tool_calls"][0]["function"]["name"], "web_search");
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

        let req = map_responses_to_chat_request_with_stream(&input, &HashSet::new(), true, true).expect("should map");
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

        let req = map_responses_to_chat_request_with_stream(&input, &HashSet::new(), true, true).expect("should map");
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

        let req =
            map_responses_to_chat_request_with_stream(&input, &HashSet::new(), true, true).expect("should map");
        assert_eq!(req.chat_request["max_tokens"], 321);
        assert_eq!(req.chat_request["metadata"]["trace_id"], "t-1");
        assert_eq!(req.chat_request["reasoning"]["effort"], "medium");
        assert_eq!(req.chat_request["service_tier"], "default");
        assert_eq!(req.chat_request["include"][0], "reasoning.encrypted_content");
        assert_eq!(req.chat_request["response_format"]["type"], "json_object");
        assert_eq!(req.chat_request["text"]["format"]["type"], "json_object");
    }

    #[test]
    fn map_supports_reasoning_input_item_summary_text() {
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

        let req =
            map_responses_to_chat_request_with_stream(&input, &HashSet::new(), false, false).expect("should map");
        let messages = req.chat_request["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["content"], "[reasoning_summary] step 1\nstep 2");
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

        let req =
            map_responses_to_chat_request_with_stream(&input, &HashSet::new(), false, false).expect("should map");
        let messages = req.chat_request["messages"].as_array().expect("messages");
        assert!(messages.is_empty());
    }

    #[test]
    fn map_supports_reasoning_input_item_text_fallback() {
        let input = json!({
            "model": "gpt-4.1",
            "input": [
                {
                    "type": "reasoning",
                    "text": "fallback reasoning text"
                }
            ]
        });

        let req =
            map_responses_to_chat_request_with_stream(&input, &HashSet::new(), false, false).expect("should map");
        let messages = req.chat_request["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["content"], "[reasoning_summary] fallback reasoning text");
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

        let req =
            map_responses_to_chat_request_with_stream(&input, &HashSet::new(), false, true).expect("should map");
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

        let req =
            map_responses_to_chat_request_with_stream(&input, &HashSet::new(), false, true).expect("should map");
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

        let err = map_responses_to_chat_request_with_stream(&input, &HashSet::new(), false, true)
            .expect_err("must fail");
        assert!(err
            .to_string()
            .contains("does not match pending tool calls"));
    }

    #[test]
    fn map_defaults_tool_choice_when_invalid() {
        let input = json!({
            "model": "gpt-4.1",
            "input": [{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],
            "tools": [{"type":"function","name":"f","parameters":{"type":"object"}}],
            "tool_choice": 123
        });

        let req = map_responses_to_chat_request_with_stream(&input, &HashSet::new(), true, true).expect("should map");
        assert_eq!(req.chat_request["tool_choice"], "auto");
    }

    #[test]
    fn map_requires_input_array() {
        let input = json!({"model":"gpt-4.1"});
        let err = map_responses_to_chat_request_with_stream(&input, &HashSet::new(), true, true).expect_err("must fail");
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
        let req = map_responses_to_chat_request_with_stream(&input, &HashSet::new(), true, true).expect("ok");
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
    fn normalize_chat_tools_keeps_function_already_wrapped() {
        let tools = vec![json!({
            "type": "function",
            "function": {"name":"f", "parameters": {"type":"object"}}
        })];
        let out = normalize_chat_tools(tools.clone(), &HashSet::new());
        assert_eq!(out, tools);
    }

    #[test]
    fn normalize_chat_tools_converts_custom_tool_to_function() {
        let tools = vec![json!({
            "type": "custom",
            "name": "shell",
            "description": "run shell",
            "input_schema": {"type":"object","properties":{"cmd":{"type":"string"}}}
        })];
        let out = normalize_chat_tools(tools, &HashSet::new());
        assert_eq!(
            out[0],
            json!({
                "type": "function",
                "function": {
                    "name": "shell",
                    "description": "run shell",
                    "parameters": {"type":"object","properties":{"cmd":{"type":"string"}}}
                }
            })
        );
    }

    #[test]
    fn normalize_tool_choice_preserves_wrapped_choice() {
        let choice = json!({"type":"function", "function":{"name":"do_it"}});
        assert_eq!(normalize_tool_choice(choice.clone()), choice);
    }

    #[test]
    fn normalize_tool_choice_converts_custom_name() {
        let choice = json!({"type":"custom", "name":"shell"});
        assert_eq!(
            normalize_tool_choice(choice),
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
        let req = map_responses_to_chat_request_with_stream(&input, &HashSet::new(), true, true).expect("ok");
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
        let out = normalize_chat_tools(tools, &drop);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "function");
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

        let function_item = responses_tool_call_item(
            "get_weather",
            "{\"city\":\"seoul\"}",
            "call_fn_1",
            &kinds,
        );
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
        assert_eq!(out["provider_specific_fields"]["mcp_list_tools"][0]["name"], "shell");
        assert_eq!(
            out["output"][0]["provider_specific_fields"]["source"],
            "provider"
        );
    }

    #[test]
    fn resolve_config_prefers_cli_over_file_and_defaults() {
        let args = Args {
            config: None,
            upstream_url: None,
            upstream_wire: None,
            upstream_http_headers: vec![parse_upstream_http_header_arg("x-trace-id=cli")
                .expect("valid header")],
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
        assert_eq!(resolved.server_info, Some(PathBuf::from("/tmp/server.json")));
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
        let parsed: FileConfig = toml::from_str(
            "[profiles.gpt_oss]\nincoming_url = \"http://localhost:8080/gpt-oss\"",
        )
        .expect("ok");
        assert!(parsed.routers.is_none());
    }

    #[test]
    fn normalize_incoming_url_to_path_supports_full_url_and_path() {
        assert_eq!(
            normalize_incoming_url_to_path("http://localhost:8080/gpt-oss/")
                .expect("normalized"),
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
        assert_eq!(target.upstream_url, "http://upstream.local/v1/chat/completions");
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
    fn map_chat_to_responses_request_converts_messages() {
        let chat = json!({
            "model": "gpt-4.1",
            "messages": [
                {"role":"user","content":"hello"}
            ],
            "stream": false
        });

        let out = map_chat_to_responses_request(&chat, false).expect("ok");
        assert_eq!(out["model"], "gpt-4.1");
        assert_eq!(out["stream"], false);
        assert_eq!(out["input"][0]["type"], "message");
        assert_eq!(out["input"][0]["content"][0]["text"], "hello");
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
    fn upstream_headers_for_logging_includes_forwarded_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("openai-organization", HeaderValue::from_static("org_123"));
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
            "x-openai-subagent".to_string(),
        ];

        let out = upstream_headers_for_logging(
            &headers,
            "sk-test",
            &configured_headers,
            &forwarded_headers,
        );
        assert_eq!(out["authorization"], "<redacted>");
        assert_eq!(out["content-type"], "application/json");
        assert_eq!(out["openai-organization"], "org_123");
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
    }
