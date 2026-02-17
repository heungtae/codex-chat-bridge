# codex-chat-bridge

`codex-chat-bridge` lets Codex keep using the Responses wire API while forwarding requests to an OpenAI-compatible `/v1/chat/completions` upstream.

This is intended for "no core source change" integration: run this bridge locally, then override `model_provider` to point Codex at the bridge.

Detailed guide: `USAGE.md`

## Install with npm

Node.js 20+ and Rust/Cargo are required because npm installation compiles the Rust binary locally.

```bash
npm install @heungtae/codex-chat-bridge
```

Install globally if you want the `codex-chat-bridge` command on PATH:

```bash
npm install -g @heungtae/codex-chat-bridge
```

## What it does

- Accepts `POST /v1/responses`
- Translates request payload into `POST /v1/chat/completions`
- Streams upstream Chat Completions chunks back as Responses-style SSE events:
  - `response.created`
  - `response.output_item.added` (assistant text starts; only emitted when text delta exists)
  - `response.output_text.delta`
  - `response.output_item.done` (assistant message and function calls)
  - `response.completed`
  - `response.failed` (for upstream/network errors)

## Run

```bash
npx @heungtae/codex-chat-bridge --port 8787 --api-key-env OPENAI_API_KEY
```

By default, the bridge uses `~/.config/codex-chat-bridge/conf.toml`.
If the file does not exist, it is created automatically with commented defaults.
CLI flags override file values.

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
- The script does not force `model`; pass it as extra args when needed (for example: `--model gpt-4.1`).

## Package Scripts

```bash
npm run build:bridge
npm run pack:check
```

## Endpoints

- `POST /v1/responses`
- `GET /healthz`
- `GET /shutdown` (only when `--http-shutdown` is enabled)
