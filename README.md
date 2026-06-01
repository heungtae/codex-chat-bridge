# codex-chat-bridge

Tiny local bridge for running Codex and Claude Code against OpenAI-compatible upstreams without patching either client.

## What It Is

- Codex: OpenAI's local coding agent. It is configured through Codex profile config files and can call a custom `model_provider`.
- Claude Code: Anthropic's coding agent. It sends Anthropic `/v1/messages` traffic.
- codex-chat-bridge: A local adapter that accepts Responses, Chat Completions, and Anthropic Messages requests, then forwards them to a configured upstream as `/v1/responses` or `/v1/chat/completions`.

## Install

```bash
npm install -g @heungtae/codex-chat-bridge
```

The npm package builds a Rust binary during install. You need Node.js 20+, npm 10+, and a Rust toolchain.

## Quick Start

Create the bridge config:

```bash
mkdir -p ~/.config/codex-chat-bridge
cp conf.toml.example ~/.config/codex-chat-bridge/conf.toml
```

Run the bridge:

```bash
export OPENROUTER_API_KEY=sk-...
codex-chat-bridge --config ~/.config/codex-chat-bridge/conf.toml
```

Use Codex 0.134.0+ with a separate profile file. Do not put legacy `[profiles.*]` blocks or `profile = "..."` in `~/.codex/config.toml`.

```bash
codex --profile chat-bridge
```

Full setup:

- [Codex 0.134.0 profile configuration](https://github.com/heungtae/codex-chat-bridge/blob/main/docs/codex-0134-profile-configuration.md)
- [Example bridge config](https://github.com/heungtae/codex-chat-bridge/blob/main/conf.toml.example)
- [OpenAI Codex config reference](https://developers.openai.com/codex/config-reference/)
- [OpenAI Codex repository](https://github.com/openai/codex)

## Bridge Behavior

- `responses -> chat`: maps Codex Responses traffic to Chat Completions upstreams.
- `chat -> responses`: maps Chat Completions clients to Responses upstreams.
- `anthropic -> chat`: maps Claude Code `/v1/messages` traffic to Chat Completions upstreams.
- `namespace`, `custom`, `mcp`, and `web_search*` tools can be converted into chat `function` tools when `tool_transform_mode = "legacy_convert"`.

## Minimal Router

```toml
[routers.chat_bridge]
incoming_url = "http://127.0.0.1:8787/chat-bridge/v1/responses"
upstream_url = "https://openrouter.ai/api/v1/chat/completions"
upstream_wire = "chat"
```

Codex provider `base_url` must stop at `/v1`; Codex appends `/responses` itself:

```toml
base_url = "http://127.0.0.1:8787/chat-bridge/v1"
wire_api = "responses"
```

## License

[Apache-2.0](https://github.com/heungtae/codex-chat-bridge/blob/main/LICENSE)
