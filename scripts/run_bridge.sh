#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

API_KEY_ENV="${API_KEY_ENV:-OPENROUTER_API_KEY}"

if [[ -z "${!API_KEY_ENV:-}" ]]; then
  echo "error: missing required env var ${API_KEY_ENV}" >&2
  echo "example: export ${API_KEY_ENV}=<your_api_key>" >&2
  exit 1
fi

exec cargo run --bin codex-chat-bridge -- "$@"
