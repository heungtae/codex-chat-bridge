# codex-chat-bridge

`codex-chat-bridge` lets Codex keep using the Responses wire API while forwarding requests to an OpenAI-compatible `/v1/chat/completions` upstream.

This is intended for "no core source change" integration: run this bridge locally, then override `model_provider` to point Codex at the bridge.

Detailed guide: `USAGE.md`

## What it does

- Accepts `POST /v1/responses`
- Translates request payload into `POST /v1/chat/completions`
- Streams upstream Chat Completions chunks back as Responses-style SSE events:
  - `response.created`
  - `response.output_text.delta`
  - `response.output_item.done` (assistant message and function calls)
  - `response.completed`
  - `response.failed` (for upstream/network errors)

## Run

```bash
cargo run -p codex-chat-bridge -- --port 8787 --api-key-env OPENAI_API_KEY
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
chat-bridge/scripts/run_codex_with_bridge.sh "Summarize this repo."
```

## Endpoints

- `POST /v1/responses`
- `GET /healthz`
- `GET /shutdown` (only when `--http-shutdown` is enabled)
