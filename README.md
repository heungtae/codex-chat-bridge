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
- Supports selectable upstream wire via `--upstream-wire chat|responses`
- Uses `incoming_url` path matching for `POST /{*incoming_path}`
- Returns Responses-style output (`stream=true` -> SSE, `stream=false` -> JSON)
- For chat upstream streaming, emits Responses-style SSE events:
  - `response.created`
  - `response.output_item.added` (assistant text starts; only emitted when text delta exists)
  - `response.output_text.delta`
  - `response.output_item.done` (assistant message and function calls)
  - `response.completed`
  - `response.failed` (for upstream/network errors)

## Recent Updates (v0.2.3)

- For `responses -> chat` upstream forwarding, `tools[].type="custom"` is normalized to `function` only when sending to chat upstream.
- For chat upstream responses converted back to Responses format:
  - incoming `custom_tool_call` is returned as `custom_tool_call_output`
  - incoming `function_call` is returned as `function_call_output`
  - `call_id` is preserved in both flows
- Added explicit `custom_tool_call` input handling in the mapper and preserved output mapping behavior for both `custom_tool_call_output` and `function_call_output`.

## Routers

The bridge supports multiple routers for different upstream configurations. Define routers in `conf.toml`:

```toml
[routers.openrouter]
incoming_url = "http://127.0.0.1:8787/openrouter/v1/responses"
upstream_url = "https://openrouter.ai/api/v1/chat/completions"
upstream_wire = "chat"

[routers.research]
incoming_url = "http://127.0.0.1:8787/research/v1/responses"
upstream_url = "https://api.openai.com/v1/responses"
upstream_wire = "responses"

[routers.local]
incoming_url = "http://127.0.0.1:8787/local/v1/responses"
upstream_url = "http://localhost:8080/v1/chat/completions"
upstream_wire = "chat"
```

Available router options:
- `upstream_url`: Override upstream URL for this router
- `upstream_wire`: Optional. If omitted, inferred from `upstream_url` (`.../v1/chat/completions` -> `chat`, `.../v1/responses` -> `responses`)
  - If `upstream_url` and `upstream_wire` are both set but inconsistent, startup fails with a configuration error.
- `upstream_http_headers`: Static headers for this router
- `forward_incoming_headers`: Forwarded headers for this router
- `drop_tool_types`: Tool types to drop for this router
- `drop_request_fields`: Top-level request fields to drop before forwarding (for example `["prompt_cache_key"]`)
- `incoming_url`: Incoming URL/path bound to this router (for example `http://127.0.0.1:8787/research/v1/responses`)
  - Absolute `http://host:port/path` entries define listener addresses. The bridge listens on every unique `host:port` found in `routers.*.incoming_url`.
  - `incoming_url` is required for every router entry.
  - At least one absolute `incoming_url` is required to start the server.

### CLI Options

- `--list-routers`: List available routers and exit
- Compatibility alias: `--list-profiles`

When the bridge starts, it logs startup summary, defaults, and per-router overrides:

```
INFO codex_chat_bridge: startup: listen_addrs=["127.0.0.1:8787"] router_count=3
INFO codex_chat_bridge: router defaults: upstream_url=http://localhost:8080/v1/chat/completions upstream_wire=Chat upstream_http_headers=[] forward_incoming_headers=[...] drop_tool_types=[...] drop_request_fields=[...]
INFO codex_chat_bridge: runtime config: api_key_env=OPENROUTER_API_KEY server_info=Some("/tmp/codex-chat-bridge-info.json") http_shutdown=false verbose_logging=false
INFO codex_chat_bridge: router: name=openrouter active=true incoming_url=Some("http://127.0.0.1:8787/openrouter/v1/responses") overrides=none
INFO codex_chat_bridge: request routed: router=research, incoming_route=/research/v1/responses, upstream_url=..., upstream_wire=Responses
```

### Runtime Router Management

Route requests by `incoming_url` path:

```bash
# List all routers
curl http://127.0.0.1:8787/routers

# Send request using incoming_url routing
curl -X POST http://127.0.0.1:8787/research/v1/responses \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-4.1","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]}'
```

Routing behavior:
- `POST /{*incoming_path}`: use `routers.*.incoming_url` path match (for example `/openrouter/v1/responses`, `/research/v1/responses`)
- `POST /v1/responses`, `POST /v1/chat/completions`: not used for routing

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
upstream_wire = "responses"
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

- `POST /{*incoming_path}`: Process by `routers.*.incoming_url` path match
- `GET /healthz`: Health check
- `GET /shutdown`: Shutdown bridge process (available when `--http-shutdown` is enabled)
- `GET /routers`: List routers
- `GET /profiles`: Compatibility alias of `GET /routers`
