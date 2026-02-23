# codex-chat-bridge 사용 및 설정 가이드

## 0. npm 배포/설치 빠른 시작

패키지명: `@heungtae/codex-chat-bridge`

```bash
npm install @heungtae/codex-chat-bridge
npx @heungtae/codex-chat-bridge --help
```

공개 npm 배포(현재 기본값):

```bash
npm publish --access public
```

private registry 배포(선택):

```bash
npm publish --registry <private-registry> --access restricted
```

## 1. 개요

`codex-chat-bridge`는 `responses`/`chat` 요청을 받아 request filtering 후 업스트림(`chat/completions` 또는 `responses`)으로 전달하고, 결과를 `responses` 형식으로 반환하는 로컬 브리지입니다.

핵심 동작:
- 클라이언트 -> `POST /v1/responses` 또는 `POST /v1/chat/completions` (브리지)
- 브리지 -> 필터 적용(`drop_tool_types`) 후 업스트림 호출
- 브리지 -> `responses` 형식으로 반환 (`stream=true`면 SSE, `stream=false`면 JSON)

## 2. 언제 필요한가

다음 상황에서 사용합니다.

- 현재 환경/벤더가 `responses`보다 `chat/completions` 호환성이 더 좋을 때
- Codex 본체 소스 수정 없이 연결 방식을 바꾸고 싶을 때
- 기존 Codex 워크플로(도구 호출 포함)를 유지하고 싶을 때

## 3. 사전 준비

필수:
- Node.js 20+
- npm
- Rust/Cargo 설치
- `OPENAI_API_KEY`(또는 원하는 이름의 API 키 환경변수)
- Codex CLI 사용 가능 상태

확인:

```bash
npm --version
cargo --version
codex --version
echo "${OPENAI_API_KEY:+set}"
```

## 4. 브리지 실행

프로젝트 루트(`chat-bridge`) 기준:

```bash
npx @heungtae/codex-chat-bridge -- \
  --port 8787 \
  --api-key-env OPENAI_API_KEY
```

기본적으로 `~/.config/codex-chat-bridge/conf.toml`을 사용합니다.
파일이 없으면 기본 템플릿으로 자동 생성됩니다.
우선순위는 `CLI 옵션 > 설정 파일 > 내장 기본값`입니다.

또는 Cargo 직접 실행:

```bash
cargo run --bin codex-chat-bridge -- \
  --port 8787 \
  --api-key-env OPENAI_API_KEY
```

다른 설정 파일을 쓰려면:

```bash
npx @heungtae/codex-chat-bridge --config /path/to/conf.toml
```

옵션 설명:
- `--config <FILE>`: 설정 파일 경로 (기본값: `~/.config/codex-chat-bridge/conf.toml`)
- `--port`: 브리지 포트 (기본: 랜덤 포트)
- `--api-key-env`: 업스트림 호출에 쓸 API 키 환경변수 이름
- `--upstream-url`: 업스트림 endpoint URL
- `--upstream-wire <chat|responses>`: 업스트림 wire 선택 (기본: `chat`)
- `--upstream-http-header <NAME=VALUE>`: 업스트림 고정 헤더 추가 (반복 가능)
- `--server-info <FILE>`: 시작 시 포트/프로세스 정보 JSON 저장
- `--http-shutdown`: `GET /shutdown` 허용

헬스체크:

```bash
curl --fail --silent --show-error http://127.0.0.1:8787/healthz
```

## 5. Codex를 브리지로 연결 (권장: 세션 오버라이드)

`exec` 예시:

```bash
codex exec \
  -c 'model_providers.chat-bridge={name="Chat Bridge",base_url="http://127.0.0.1:8787/v1",env_key="OPENAI_API_KEY",wire_api="responses"}' \
  -c 'model_provider="chat-bridge"' \
  '이 코드베이스에서 TODO를 찾아 수정해줘'
```

대화형(`codex`) 실행 시도 동일하게 가능:

```bash
codex \
  -c 'model_providers.chat-bridge={name="Chat Bridge",base_url="http://127.0.0.1:8787/v1",env_key="OPENAI_API_KEY",wire_api="responses"}' \
  -c 'model_provider="chat-bridge"'
```

중요:
- 브리지를 켜두기만 하면 자동 전환되지 않습니다.
- 반드시 `model_provider`가 브리지를 가리켜야 합니다.

## 6. 대안 연결 방식

### 6.1 `OPENAI_BASE_URL` 방식

기존 `openai` provider를 그대로 쓰고 base URL만 브리지로 바꾸는 방식입니다.

```bash
export OPENAI_BASE_URL=http://127.0.0.1:8787/v1
codex exec '간단한 테스트를 해줘'
```

### 6.2 래퍼 스크립트 사용

브리지 기동/종료 + Codex 실행을 한 번에 처리:

```bash
scripts/run_codex_with_bridge.sh "이 저장소 구조를 설명해줘"
```

기본값:
- `API_KEY_ENV=OPENAI_API_KEY`
- `UPSTREAM_URL=https://api.openai.com/v1/chat/completions`
- 래퍼 스크립트는 `model`을 강제하지 않음 (필요 시 추가 인자로 전달)

추가 인자 전달:

```bash
scripts/run_codex_with_bridge.sh \
  "테스트 코드 생성해줘" \
  --model gpt-4.1
```

### 6.3 npm 스크립트 사용

```bash
npm run build:bridge
npm run pack:check
```

## 7. 영구 설정 (선택)

반복 사용 시 `~/.codex/config.toml`에 provider를 추가할 수 있습니다.

```toml
[model_providers.chat-bridge]
name = "Chat Bridge"
base_url = "http://127.0.0.1:8787/v1"
env_key = "OPENAI_API_KEY"
wire_api = "responses"

model_provider = "chat-bridge"
```

주의:
- Codex는 여전히 `responses` wire를 사용합니다.
- 브리지는 `/v1/chat/completions` 입력도 수용하지만 출력은 `responses` 형식으로 통일됩니다.

## 8. 동작 검증 방법

## 8.1 브리지가 받는 요청 확인

브리지 로그를 터미널에서 보고, Codex 요청 시 에러가 없는지 확인합니다.

## 8.2 실제 chat/completions 경유 여부 확인

`--upstream-url`을 명시적으로 지정해 예상 엔드포인트로만 트래픽이 가게 만듭니다.

```bash
cargo run --bin codex-chat-bridge -- \
  --port 8787 \
  --api-key-env OPENAI_API_KEY \
  --upstream-url https://api.openai.com/v1/chat/completions
```

## 8.3 툴 호출 동작 확인

도구 호출이 필요한 프롬프트를 주고, 브리지가 `response.output_item.done`(function_call)을 내보내는지 확인합니다.

## 9. 트러블슈팅

`missing or empty env var`:
- `OPENAI_API_KEY`가 비어 있거나 설정되지 않음
- `--api-key-env` 값과 실제 환경변수 이름이 다름

`upstream returned 401/403`:
- API 키 권한/조직/프로젝트 헤더 확인
- 필요 시 `openai-organization`, `openai-project` 헤더 전달 상태 확인

`upstream returned 404`:
- `--upstream-url` 경로 확인 (`/v1/chat/completions`)

브리지 실행은 되지만 Codex가 여전히 기본 경로를 탐:
- `-c 'model_provider="chat-bridge"'` 누락 여부 확인
- `-c` 인용부호 깨짐 여부 확인 (특히 쉘에서 `'`/`"` 혼용)

스트리밍 중단:
- 네트워크 끊김 시 `response.failed` 이벤트로 종료될 수 있음
- 장시간 작업은 재시도/재실행 전략 권장

## 10. 보안/운영 권장 사항

- 브리지는 기본적으로 로컬 바인딩(`127.0.0.1`)만 사용하세요.
- 공유 서버에서는 `--http-shutdown` 사용을 최소화하세요.
- API 키는 쉘 히스토리에 직접 남기지 말고 환경변수로 주입하세요.
- 운영 환경에서는 systemd/supervisor로 브리지 생명주기 관리 권장

## 11. 현재 제한사항

- 완전한 Responses 기능 1:1 재현이 아니라, Codex 동작에 필요한 핵심 이벤트 중심 변환입니다.
- 모델/벤더별 Chat delta 형식 차이가 있으면 추가 매핑이 필요할 수 있습니다.
- `responses` 전용 고급 필드는 일부 축약 또는 무시될 수 있습니다.

## 12. 빠른 실행 요약

1. 브리지 실행

```bash
cargo run --bin codex-chat-bridge -- --port 8787 --api-key-env OPENAI_API_KEY
```

2. Codex를 브리지 provider로 실행

```bash
codex exec \
  -c 'model_providers.chat-bridge={name="Chat Bridge",base_url="http://127.0.0.1:8787/v1",env_key="OPENAI_API_KEY",wire_api="responses"}' \
  -c 'model_provider="chat-bridge"' \
  '작업을 수행해줘'
```

## 13. Ollama `gpt-oss:20b` 기준 Configuration (전체)

이 섹션은 `Ollama + gpt-oss:20b`를 `codex-chat-bridge`로 연결할 때 필요한 설정을 한 번에 정리한 것입니다.

전제:
- Ollama OpenAI 호환 엔드포인트 사용: `http://127.0.0.1:11434/v1/chat/completions`
- 모델: `gpt-oss:20b`
- Codex는 브리지에 `responses`로 요청하고, 브리지가 Ollama `chat/completions`로 변환

### 13.1 Ollama 준비

```bash
ollama pull gpt-oss:20b
ollama serve
```

확인:

```bash
curl --silent http://127.0.0.1:11434/api/tags | jq '.models[].name' | grep 'gpt-oss:20b'
```

### 13.2 환경변수 설정

`codex-chat-bridge`는 API 키 문자열이 비어 있지 않아야 하므로, Ollama에서는 더미 값을 사용합니다.

```bash
export OLLAMA_API_KEY=ollama
```

### 13.3 브리지 실행 설정 (전체 옵션 예시)

```bash
cargo run --bin codex-chat-bridge -- \
  --host 127.0.0.1 \
  --port 8787 \
  --api-key-env OLLAMA_API_KEY \
  --upstream-url http://127.0.0.1:11434/v1/chat/completions \
  --server-info /tmp/codex-chat-bridge-info.json \
  --http-shutdown
```

옵션 설명:

| 옵션 | 예시 값 | 설명 |
|---|---|---|
| `--host` | `127.0.0.1` | 브리지 바인딩 주소 |
| `--port` | `8787` | 브리지 포트 |
| `--api-key-env` | `OLLAMA_API_KEY` | 브리지가 읽을 API 키 환경변수 이름 (Ollama에서는 더미 가능) |
| `--upstream-url` | `http://127.0.0.1:11434/v1/chat/completions` | 실제 업스트림 chat endpoint |
| `--server-info` | `/tmp/codex-chat-bridge-info.json` | 실행 포트/프로세스 정보 파일 |
| `--http-shutdown` | enabled | `GET /shutdown` 허용 |

### 13.4 Codex 실행 설정 (CLI 오버라이드 방식)

`model`은 `gpt-oss:20b`로 지정해야 합니다.

```bash
export OLLAMA_API_KEY=ollama

codex exec \
  --model gpt-oss:20b \
  -c 'model_providers.chat-bridge-ollama={name="Chat Bridge Ollama",base_url="http://127.0.0.1:8787/v1",env_key="OLLAMA_API_KEY",wire_api="responses"}' \
  -c 'model_provider="chat-bridge-ollama"' \
  '현재 디렉터리 프로젝트 구조를 요약해줘'
```

### 13.5 Codex 영구 설정 (`~/.codex/config.toml`)

매번 `-c`를 넣기 싫다면 아래처럼 고정할 수 있습니다.

```toml
[model_providers.chat-bridge-ollama]
name = "Chat Bridge Ollama"
base_url = "http://127.0.0.1:8787/v1"
env_key = "OLLAMA_API_KEY"
wire_api = "responses"

model_provider = "chat-bridge-ollama"
model = "gpt-oss:20b"
```

이후 실행:

```bash
export OLLAMA_API_KEY=ollama
codex
```

### 13.6 Configuration 전체 항목 설명

#### A) 브리지(`codex-chat-bridge`) 설정 항목

| 항목 | 필요 여부 | 설명 |
|---|---|---|
| `host` | 선택 | 브리지 수신 IP |
| `port` | 선택 | 브리지 수신 포트 (미지정 시 랜덤) |
| `upstream_url` | 필수(실질) | 변환 후 요청을 보낼 chat endpoint |
| `upstream_http_headers` | 선택 | 업스트림 요청에 항상 추가할 헤더 맵 (`{ "x-key" = "value" }`, alias: `http_headers`) |
| `forward_incoming_headers` | 선택 | 업스트림 요청에 복사할 들어오는 헤더 이름 리스트 (기본: `openai-organization`, `openai-project`, `x-openai-subagent`, `x-codex-turn-state`, `x-codex-turn-metadata`) |
| `api_key_env` | 필수 | 브리지가 읽는 토큰 환경변수 이름 |
| `server_info` | 선택 | 포트/PID 파일 출력 |
| `http_shutdown` | 선택 | HTTP 종료 엔드포인트 허용 |

#### B) Codex provider 설정 항목 (`model_providers.<id>`)

| 항목 | 필요 여부 | 설명 |
|---|---|---|
| `name` | 권장 | 표시용 provider 이름 |
| `base_url` | 필수 | Codex가 호출할 Responses base URL (`http://127.0.0.1:8787/v1`) |
| `env_key` | 필수 | Codex가 provider 인증 토큰을 읽을 환경변수 |
| `wire_api` | 필수 | 반드시 `"responses"` |

#### C) Codex 런타임 선택 항목

| 항목 | 필요 여부 | 설명 |
|---|---|---|
| `model_provider` | 필수 | 위에서 정의한 provider id 선택 |
| `model` | 필수(실질) | Ollama 모델명 (`gpt-oss:20b`) |

### 13.7 연결 검증 체크리스트

1. 브리지 헬스체크:

```bash
curl --fail --silent --show-error http://127.0.0.1:8787/healthz
```

2. Codex 실행 시 provider/model 확인:
- provider: `chat-bridge-ollama`
- model: `gpt-oss:20b`

3. 실패 시 우선 점검:
- Ollama가 `11434`에서 실행 중인지
- 브리지 `--upstream-url` 경로가 정확한지
- `OLLAMA_API_KEY`가 빈 문자열이 아닌지
