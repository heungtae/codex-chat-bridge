#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: $0 '<prompt>' [extra codex args...]" >&2
  exit 1
fi

PROMPT="$1"
shift || true

API_KEY_ENV="${API_KEY_ENV:-OPENAI_API_KEY}"
UPSTREAM_URL="${UPSTREAM_URL:-https://api.openai.com/v1/chat/completions}"
BRIDGE_PORT="${BRIDGE_PORT:-8787}"
SERVER_INFO="${SERVER_INFO:-/tmp/codex-chat-bridge-info.json}"

if [[ -z "${!API_KEY_ENV:-}" ]]; then
  echo "error: missing required env var ${API_KEY_ENV}" >&2
  exit 1
fi

cleanup() {
  curl --silent --show-error --fail "http://127.0.0.1:${BRIDGE_PORT}/shutdown" >/dev/null 2>&1 || true
}
trap cleanup EXIT

cargo run -p codex-chat-bridge -- \
  --port "${BRIDGE_PORT}" \
  --api-key-env "${API_KEY_ENV}" \
  --upstream-url "${UPSTREAM_URL}" \
  --server-info "${SERVER_INFO}" \
  --http-shutdown >/tmp/codex-chat-bridge.log 2>&1 &

for _ in $(seq 1 40); do
  if curl --silent --show-error --fail "http://127.0.0.1:${BRIDGE_PORT}/healthz" >/dev/null 2>&1; then
    break
  fi
  sleep 0.25
done

codex exec \
  -c "model_providers.chat-bridge={name='Chat Bridge',base_url='http://127.0.0.1:${BRIDGE_PORT}/v1',env_key='${API_KEY_ENV}',wire_api='responses'}" \
  -c 'model_provider="chat-bridge"' \
  "$PROMPT" "$@"
