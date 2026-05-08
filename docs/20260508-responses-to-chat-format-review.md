# Responses to Chat Format Review

Date: 2026-05-08

This review compares the bridge's `responses -> chat` request mapping with the current OpenAI API reference for Chat Completions and Responses.

References:

- Chat Completions create: https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create
- Responses create: https://developers.openai.com/api/reference/resources/responses/methods/create

## Summary

The bridge already converts most Responses-only message content into Chat-safe text:

- `input_text`, `output_text`, and `summary_text` are flattened into string content.
- `input_image` and `input_file` are converted to textual placeholders when extended input types are enabled.
- Responses `text.format` is converted to Chat `response_format`.
- `reasoning` input items are ignored instead of being serialized into Chat messages.

The review found several places where Responses fields could still be forwarded to a Chat Completions upstream in a shape that the official Chat API does not accept.

## Findings

- `include` is a Responses request field and is not part of the Chat Completions create request body. Forwarding it to Chat can produce an invalid request.
- Responses `reasoning: { "effort": ... }` is not the Chat request shape. Chat uses `reasoning_effort`.
- Responses `max_output_tokens` should map to Chat `max_completion_tokens`; `max_tokens` is deprecated and is not compatible with newer reasoning models.
- `tool_transform_mode = "passthrough"` can intentionally preserve non-Chat tool shapes such as Responses-style `mcp` or `web_search*`. That mode is provider-specific and should not be treated as official OpenAI Chat compatibility.
- A `tool` role message is only Chat-valid when it answers a preceding assistant `tool_calls` entry. The bridge can create such a message from `function_call_output`; this is valid after `previous_response_id` history is merged, but can be invalid if sent without a matching prior tool call.

## Changes Applied

- `max_output_tokens` now maps to `max_completion_tokens`.
- `reasoning.effort` now maps to `reasoning_effort`.
- `include` is no longer copied into Chat Completions requests.

## Remaining Notes

- Keep `tool_transform_mode = "legacy_convert"` for OpenAI-compatible Chat upstreams.
- Use `tool_transform_mode = "passthrough"` only for upstreams that explicitly accept Responses-like tool types on a Chat endpoint.
- Tool-output-only inputs still depend on merged prior messages to be valid Chat conversations.
