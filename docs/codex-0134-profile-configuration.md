# Codex 0.134.0 Profile Configuration

Codex 0.134.0 makes `--profile` the primary selector. Keep the global Codex config small and put bridge-specific settings in a separate profile file.

## Files

- Global config: `~/.codex/config.toml`
- Bridge profile: `~/.codex/chat-bridge.config.toml`
- Bridge runtime config: `~/.config/codex-chat-bridge/conf.toml`

Do not use this old style with Codex 0.134.0+:

```toml
profile = "chat-bridge"

[profiles.chat-bridge]
model = "openai/gpt-oss-120b"
```

Use this instead:

```bash
codex --profile chat-bridge
codex exec --profile chat-bridge "Summarize this repo."
```

## 1. Install Codex And Bridge

```bash
npm install -g @openai/codex@0.134.0
npm install -g @heungtae/codex-chat-bridge
```

Codex links:

- https://github.com/openai/codex
- https://developers.openai.com/codex/config-reference/

## 2. Create Bridge Runtime Config

Create `~/.config/codex-chat-bridge/conf.toml`:

```toml
api_key_env = "OPENROUTER_API_KEY"
verbose_logging = false

[features]
enable_previous_response_id = true
enable_tool_argument_stream_events = true
enable_extended_stream_events = true
enable_reasoning_stream_events = true
enable_provider_specific_fields = true
enable_extended_input_types = true
tool_transform_mode = "legacy_convert"

[routers.chat_bridge]
incoming_url = "http://127.0.0.1:8787/chat-bridge/v1/responses"
upstream_url = "https://openrouter.ai/api/v1/chat/completions"
upstream_wire = "chat"
upstream_model = "openai/gpt-oss-120b"
drop_request_fields = ["prompt_cache_key"]
```

Run it:

```bash
export OPENROUTER_API_KEY=sk-...
codex-chat-bridge --config ~/.config/codex-chat-bridge/conf.toml
```

## 3. Keep Global Codex Config Minimal

Create or edit `~/.codex/config.toml`:

```toml
model = "openai/gpt-oss-120b"
model_provider = "openai"
approval_policy = "on-request"
sandbox_mode = "workspace-write"
```

Do not set `profile = "chat-bridge"` here.

## 4. Create Separate Codex Profile File

Create `~/.codex/chat-bridge.config.toml`:

```toml
model = "openai/gpt-oss-120b"
model_provider = "chat-bridge"
approval_policy = "on-request"
sandbox_mode = "workspace-write"
model_reasoning_effort = "medium"
model_reasoning_summary = "auto"

[model_providers.chat-bridge]
name = "codex-chat-bridge"
base_url = "http://127.0.0.1:8787/chat-bridge/v1"
env_key = "OPENROUTER_API_KEY"
wire_api = "responses"
requires_openai_auth = false
request_max_retries = 3
stream_max_retries = 3
stream_idle_timeout_ms = 300000
```

Important:

- `base_url` is `http://127.0.0.1:8787/chat-bridge/v1`.
- Do not add `/responses`; Codex appends it because `wire_api = "responses"`.
- The bridge router must listen on `http://127.0.0.1:8787/chat-bridge/v1/responses`.
- Keep `wire_api = "responses"` for Codex. The bridge converts to chat upstreams when needed.

## 5. Run Codex Through The Bridge

```bash
export OPENROUTER_API_KEY=sk-...
codex --profile chat-bridge
```

Non-interactive:

```bash
export OPENROUTER_API_KEY=sk-...
codex exec --profile chat-bridge "Explain the project."
```

## 6. Claude Code Route

Add a Claude Code router when you need Anthropic `/v1/messages` input:

```toml
[routers.claude_code]
incoming_url = "http://127.0.0.1:8787/claude/v1/messages"
upstream_url = "https://openrouter.ai/api/v1/chat/completions"
upstream_wire = "chat"
upstream_model_sonnet = "anthropic/claude-sonnet-4.5"
anthropic_preserve_thinking = false
anthropic_enable_openrouter_reasoning = false
```

Point Claude Code at:

```text
http://127.0.0.1:8787/claude/v1/messages
```

## 7. Troubleshooting

- 404 from bridge: `incoming_url` does not match the actual client path.
- 400 from upstream about `namespace`: use `tool_transform_mode = "legacy_convert"`.
- Codex starts with the wrong provider: you forgot `--profile chat-bridge`.
- Codex rejects config: remove legacy `profile = "..."` and `[profiles.*]` from `~/.codex/config.toml`.
- API key missing: export the env var named by both `api_key_env` and `env_key`.

References:

- https://developers.openai.com/codex/config-reference/
- https://github.com/openai/codex
- https://github.com/heungtae/codex-chat-bridge/blob/main/conf.toml.example
