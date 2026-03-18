# codex-chat-bridge

`codex-chat-bridge` accepts Responses/Chat requests, applies request filtering, and forwards them to an OpenAI-compatible upstream (`/v1/chat/completions` or `/v1/responses`).

This is intended for "no core source change" integration: run this bridge locally, then override `model_provider` to point Codex at the bridge.

## Prerequisites (before `npm install`)

`npm install` runs a `postinstall` step that compiles the Rust binary locally (`cargo build --release`).
Prepare the following first:

- Node.js `>=20.0.0` (from `package.json` engines)
- npm `>=10.0.0`
- Rust toolchain (`rustup`, `rustc`, `cargo`) on `PATH`
  - `rustc >=1.85.0` (`edition = "2024"` minimum)
  - `cargo >=1.85.0`
- Network access to npm and crates.io (or your internal mirrors/registries)

Quick checks:

```bash
node --version
npm --version
rustc --version
cargo --version
```

Runtime-only setup (not required for install): set your upstream API key environment variable before running the bridge, for example:

```bash
export OPENAI_API_KEY=<your-api-key>
```

## Install with npm

```bash
npm install @heungtae/codex-chat-bridge
```

Install globally if you want the `codex-chat-bridge` command on PATH:

```bash
npm install -g @heungtae/codex-chat-bridge
```

## What it does

- Filters request payloads (`drop_tool_types`, `drop_request_fields`) before upstream forwarding
- Adds static upstream headers via `--upstream-http-header NAME=VALUE` or config map (`upstream_http_headers`, alias: `http_headers`)
- Forwards configured incoming headers (defaults include OpenAI metadata headers) to upstream with `--forward-incoming-header NAME` or `forward_incoming_headers`
- If the same header key exists in both `forward_incoming_headers` and `upstream_http_headers`, the value from `upstream_http_headers` is sent
- Automatically maps upstream API type from `upstream_url` (`.../v1/chat/completions` or `.../v1/responses`)
- Uses `incoming_url` path matching for `POST /{*incoming_path}`
- Supports feature flags globally (`[features]`) and per-router (`[routers.<name>.features]`)
- Supports host-aware routing when routers use absolute `incoming_url` values
- Returns Responses-style output (`stream=true` -> SSE, `stream=false` -> JSON)
- Supports Anthropic `POST /v1/messages` input for Claude Code compatible routing
- For chat upstream streaming, emits Responses-style SSE events:
  - `response.created`
  - `response.in_progress` (when `enable_extended_stream_events=true`)
  - `response.output_item.added` (assistant text starts; only emitted when text delta exists)
  - `response.content_part.added/done` (when `enable_extended_stream_events=true`)
  - `response.output_text.delta`
  - `response.output_text.done` (when `enable_extended_stream_events=true`)
  - `response.function_call_arguments.delta/done` (when `enable_tool_argument_stream_events=true`)
  - `response.reasoning_summary_text.delta/done` (when `enable_reasoning_stream_events=true`)
  - `response.output_item.done` (assistant message and function calls)
  - `response.completed`
  - `response.failed` (for upstream/network errors)

## Routers

The bridge supports multiple routers for different upstream configurations. Routing key is:

- absolute `incoming_url` in config: `host:port + path`
- path-only `incoming_url` in config: `path` only

Define routers in `conf.toml`:

```toml
[routers.openrouter]
incoming_url = "http://127.0.0.1:8787/openrouter/v1/responses"
upstream_url = "https://openrouter.ai/api/v1/chat/completions"

[routers.research]
incoming_url = "http://127.0.0.1:8787/research/v1/responses"
upstream_url = "https://api.openai.com/v1/responses"

[routers.local]
incoming_url = "http://127.0.0.1:8787/local/v1/responses"
upstream_url = "http://localhost:8080/v1/chat/completions"
```

Router resolution and overrides:
- Every router must define `incoming_url`.
- `incoming_url` can be absolute URL (`http://host:port/path`) or path-only (`/path`).
- At least one router must use an absolute `incoming_url` so the process can bind/listen.
- Duplicate route keys are rejected at startup.
- For each request, router config is merged onto defaults.
  - `upstream_http_headers`: merged (router keys overwrite same header names)
  - `forward_incoming_headers`: replaced by router list when set
  - If the same header key appears in both forwarded and static upstream headers, static `upstream_http_headers` wins
  - `drop_tool_types`, `drop_request_fields`: appended/unioned with defaults
  - `features`: per-router override on top of global feature flags

Available router fields:
- `upstream_url`: Override upstream URL for this router
  - Upstream API type is inferred automatically from URL suffix (`.../v1/chat/completions` -> chat, `.../v1/responses` -> responses)
- `upstream_http_headers`: Static headers for this router
- `forward_incoming_headers`: Forwarded headers for this router
- `drop_tool_types`: Tool types to drop for this router
- `drop_request_fields`: Top-level request fields to drop before forwarding (for example `["prompt_cache_key"]`)
- `features`: Router-specific feature flag overrides (for example `[routers.research.features]`)
- `incoming_url`: Incoming URL/path bound to this router (for example `http://127.0.0.1:8787/research/v1/responses`)
  - Absolute URL entries define listener addresses. The bridge listens on every unique `host:port` found in `routers.*.incoming_url`.
  - Path-only entries participate in route matching but do not create listeners.
  - `incoming_url` must include the final request path, not just the base URL. For Claude Code this means `/v1/messages`, not only `/v1`.

## Feature Flags (Global + Router Override)

Boolean feature flags default to `true`.

Priority:
- Built-in default (`true`)
- Global override (`[features]`)
- Router override (`[routers.<name>.features]`)

Example:

```toml
[features]
enable_previous_response_id = true
enable_tool_argument_stream_events = true
enable_extended_stream_events = true
enable_reasoning_stream_events = true
enable_provider_specific_fields = true
enable_extended_input_types = true
tool_transform_mode = "legacy_convert" # passthrough | legacy_convert

[routers.research.features]
enable_reasoning_stream_events = false
enable_tool_argument_stream_events = false
tool_transform_mode = "legacy_convert"
```

Available flags:
- `enable_previous_response_id`: Enables `previous_response_id` session chaining for `responses -> chat`.
- `enable_tool_argument_stream_events`: Emits tool argument SSE events (`response.function_call_arguments.delta/done`).
- `enable_extended_stream_events`: Emits extended SSE lifecycle events (`response.in_progress`, content part add/done, `response.output_text.done`).
- `enable_reasoning_stream_events`: Emits reasoning SSE events (`response.reasoning_summary_text.*`) and reasoning output items.
- `enable_provider_specific_fields`: Preserves/passes `provider_specific_fields` in mapped responses.
- `enable_extended_input_types`: Allows extended input/tool types (`input_image`, `input_file`, `mcp`, `web_search`, `web_search_preview`) in `responses -> chat` bridge path.
- `tool_transform_mode`: Controls `responses -> chat` tool conversion.
  - `legacy_convert` (default): Convert `custom`/`mcp`/`web_search*` tools into chat `function` tools.
  - `passthrough`: Keep non-`function` tool types as-is (LiteLLM-like behavior).

`responses -> chat` mapping accepts top-level `input` items with `type: "reasoning"` for compatibility, but does not serialize them into assistant message text. Live reasoning visibility is provided through Responses-style reasoning stream events/items.

### CLI Options

- `--list-routers`: List available routers and exit

When the bridge starts, it logs startup summary, defaults, and per-router overrides:

```
INFO codex_chat_bridge: startup: listen_addrs=["127.0.0.1:8787"] router_count=3
INFO codex_chat_bridge: router defaults: upstream_url=http://localhost:8080/v1/chat/completions, upstream_wire=Chat, upstream_http_headers=[], forward_incoming_headers=[...], drop_tool_types=[...], drop_request_fields=[...]
INFO codex_chat_bridge: runtime config: api_key_env=OPENROUTER_API_KEY, server_info=Some("/tmp/codex-chat-bridge-info.json"), http_shutdown=false, verbose_logging=false, feature_flags=FeatureFlags { enable_provider_specific_fields: false, enable_extended_input_types: false, tool_transform_mode: LegacyConvert }
INFO codex_chat_bridge: router: name=openrouter, active=true, incoming_url=Some("http://127.0.0.1:8787/openrouter/v1/responses"), upstream_wire=Responses, overrides=upstream_url=http://openrouter.ai/api/v1/responses, upstream_wire=Responses
```

With `verbose_logging=true`, request-level routing logs are also emitted at `DEBUG` level:

```
DEBUG codex_chat_bridge: request routed: router=research, incoming_route=/research/v1/responses, upstream_url=http://localhost:8080/v1/chat/completions, upstream_wire=Chat
```

## Run

```bash
npx @heungtae/codex-chat-bridge --api-key-env OPENAI_API_KEY
```

By default, the bridge uses `~/.config/codex-chat-bridge/conf.toml`.
If the file does not exist, it is created automatically with commented defaults.
CLI flags override file values.

Or run the binary directly via Cargo:

```bash
cargo run --bin codex-chat-bridge -- --api-key-env OPENAI_API_KEY
```

Use a custom config file path:

```bash
npx @heungtae/codex-chat-bridge --config /path/to/conf.toml
```

## Codex Guide (Multi Routing)

To use Codex with this bridge, set each router `incoming_url` to the exact request path Codex will call.
Do not use bridge root `/v1` as Codex `base_url`.

For `wire_api="responses"` providers, Codex uses `<base_url>/responses`.
So if provider `base_url` is `http://127.0.0.1:8787/research/v1`, set:

```toml
[routers.research]
incoming_url = "http://127.0.0.1:8787/research/v1/responses"
upstream_url = "https://api.openai.com/v1/responses"
```

Multi-router Codex provider example:

```bash
codex exec \
  -c 'model_providers.openrouter-bridge={name="OpenRouter Bridge",base_url="http://127.0.0.1:8787/openrouter/v1",env_key="OPENROUTER_API_KEY",wire_api="responses"}' \
  -c 'model_providers.research-bridge={name="Research Bridge",base_url="http://127.0.0.1:8787/research/v1",env_key="OPENAI_API_KEY",wire_api="responses"}' \
  -c 'model_provider="research-bridge"' \
  'Say hello and call tools when needed.'
```

To switch routes, change `model_provider` to another bridge provider name.

## Claude Code Guide

Claude Code uses Anthropic `POST /v1/messages`. If you set:

```bash
ANTHROPIC_BASE_URL=http://127.0.0.1:8787/claude
```

Claude Code will call:

```text
http://127.0.0.1:8787/claude/v1/messages
```

So the router `incoming_url` must include that exact final path:

```toml
[routers.claude]
incoming_url = "http://127.0.0.1:8787/claude/v1/messages"
upstream_url = "http://localhost:8080/v1/chat/completions"
upstream_wire = "chat"
```

Important points:
- Do not set `incoming_url` to only `http://127.0.0.1:8787/claude` or `.../v1`.
- For Claude Code streaming, use `upstream_wire = "chat"`.
- Anthropic `/v1/messages` to upstream `responses` streaming is not supported yet.

Example:

```bash
ANTHROPIC_BASE_URL="http://127.0.0.1:8787/claude" \
ANTHROPIC_AUTH_TOKEN="dummy" \
claude
```

## Wrapper script

Use `scripts/run_codex_with_bridge.sh` to run the bridge and `codex exec` together:

```bash
scripts/run_codex_with_bridge.sh "Summarize this repo."
```

Defaults:
- `API_KEY_ENV=OPENAI_API_KEY`
- `UPSTREAM_URL=https://api.openai.com/v1/chat/completions`
- `ROUTER_BASE_PATH=/codex/v1` (Codex provider `base_url` becomes `http://127.0.0.1:${BRIDGE_PORT}${ROUTER_BASE_PATH}`)
- The script does not force `model`; pass it as extra args when needed (for example: `--model gpt-4.1`).

## Package Scripts

```bash
npm run build:bridge
npm run pack:check
```

## Endpoints

- `POST /{*incoming_path}`: Process by `routers.*.incoming_url` match
- `POST /v1/messages`: Anthropic/Claude Code compatible routed entrypoint
- `POST /v1/responses`: Routed request entrypoint (requires matching router `incoming_url`)
- `POST /v1/chat/completions`: Routed request entrypoint (requires matching router `incoming_url`)
- `GET /healthz`: Health check
- `GET /shutdown`: Shutdown bridge process (available when `--http-shutdown` is enabled)
- `GET /routers`: List routers
