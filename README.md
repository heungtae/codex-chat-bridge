# codex-chat-bridge

`codex-chat-bridge` accepts Responses/Chat requests, applies request filtering, and forwards them to an OpenAI-compatible upstream (`/v1/chat/completions` or `/v1/responses`).

This is intended for "no core source change" integration: run this bridge locally, then override `model_provider` to point Codex at the bridge.

Detailed guide: `USAGE.md`

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

- Accepts `POST /v1/responses` and `POST /v1/chat/completions`
- Filters request payloads (`drop_tool_types`) before upstream forwarding
- Adds static upstream headers via `--upstream-http-header NAME=VALUE` or config map (`upstream_http_headers`, alias: `http_headers`)
- Forwards configured incoming headers (defaults include OpenAI metadata headers) to upstream with `--forward-incoming-header NAME` or `forward_incoming_headers`
- Supports selectable upstream wire via `--upstream-wire chat|responses`
- Returns Responses-style output (`stream=true` -> SSE, `stream=false` -> JSON)
- For chat upstream streaming, emits Responses-style SSE events:
  - `response.created`
  - `response.output_item.added` (assistant text starts; only emitted when text delta exists)
  - `response.output_text.delta`
  - `response.output_item.done` (assistant message and function calls)
  - `response.completed`
  - `response.failed` (for upstream/network errors)

## Profiles

The bridge supports multiple profiles for different upstream configurations. Define profiles in `conf.toml`:

```toml
[profiles.default]
upstream_url = "https://api.openai.com/v1/chat/completions"
upstream_wire = "chat"

[profiles.prod]
upstream_url = "https://api.openai.com/v1/chat/completions"
upstream_wire = "responses"

[profiles.dev]
upstream_url = "http://localhost:8080/v1/chat/completions"
upstream_wire = "chat"
```

Available profile options:
- `upstream_url`: Override upstream URL for this profile
- `upstream_wire`: Set wire API (`chat` or `responses`)
- `upstream_http_headers`: Static headers for this profile
- `forward_incoming_headers`: Forwarded headers for this profile
- `drop_tool_types`: Tool types to drop for this profile

### CLI Options

- `--profile <name>`: Start with a specific profile
- `--list-profiles`: List available profiles and exit

When the bridge starts, it logs the active configuration at info level:

```
INFO codex_chat_bridge: using profile: default
INFO codex_chat_bridge: config: host=127.0.0.1, port=8787, upstream_url=..., upstream_wire=Chat, ...
```

### Runtime Profile Management

Switch profiles at runtime via HTTP API:

```bash
# Get current profile
curl http://127.0.0.1:8787/profile

# List all profiles
curl http://127.0.0.1:8787/profiles

# Switch profile
curl -X POST http://127.0.0.1:8787/profile \
  -H "Content-Type: application/json" \
  -d '{"name": "prod"}'
```

## Run

```bash
npx @heungtae/codex-chat-bridge --port 8787 --api-key-env OPENAI_API_KEY
```

By default, the bridge uses `~/.config/codex-chat-bridge/conf.toml`.
If the file does not exist, it is created automatically with commented defaults.
CLI flags override file values.

Start with a specific profile:

```bash
codex-chat-bridge --profile prod
```

Or run the binary directly via Cargo:

```bash
cargo run --bin codex-chat-bridge -- --port 8787 --api-key-env OPENAI_API_KEY
```

Use a custom config file path:

```bash
npx @heungtae/codex-chat-bridge --config /path/to/conf.toml
```

## Codex override example

```bash
codex exec \
  -c 'model_providers.chat-bridge={name="Chat Bridge",base_url="http://127.0.0.1:8787/v1",env_key="OPENAI_API_KEY",wire_api="responses"}' \
  -c 'model_provider="chat-bridge"' \
  'Say hello and call tools when needed.'
```

You can also set `OPENAI_BASE_URL=http://127.0.0.1:8787/v1` and keep `model_provider=openai`.

## Wrapper script

Use `scripts/run_codex_with_bridge.sh` to run the bridge and `codex exec` together:

```bash
scripts/run_codex_with_bridge.sh "Summarize this repo."
```

Defaults:
- `API_KEY_ENV=OPENAI_API_KEY`
- `UPSTREAM_URL=https://api.openai.com/v1/chat/completions`
- `UPSTREAM_WIRE=chat`
- The script does not force `model`; pass it as extra args when needed (for example: `--model gpt-4.1`).

## Package Scripts

```bash
npm run build:bridge
npm run pack:check
```

## Endpoints

- `POST /v1/responses`
- `POST /v1/chat/completions`
- `GET /healthz`
- `GET /shutdown` (only when `--http-shutdown` is enabled)
- `GET /profile` - Get current profile info
- `POST /profile` - Switch profile (`{"name": "profile_name"}`)
- `GET /profiles` - List all available profiles
