# codex-chat-bridge 사용 및 설정 가이드

## 1. 개요

`codex-chat-bridge`는 Codex가 내부적으로 `responses` API를 쓰는 구조를 유지하면서, 실제 업스트림 호출은 `chat/completions`로 보내도록 중간 변환해주는 로컬 브리지입니다.

핵심 동작:
- Codex -> `POST /v1/responses` (브리지)
- 브리지 -> `POST /v1/chat/completions` (업스트림)
- 브리지 -> Responses 스타일 SSE 이벤트로 재변환 후 Codex에 전달

## 2. 언제 필요한가

다음 상황에서 사용합니다.

- 현재 환경/벤더가 `responses`보다 `chat/completions` 호환성이 더 좋을 때
- Codex 본체 소스 수정 없이 연결 방식을 바꾸고 싶을 때
- 기존 Codex 워크플로(도구 호출 포함)를 유지하고 싶을 때

## 3. 사전 준비

필수:
- Rust/Cargo 설치
- `OPENAI_API_KEY`(또는 원하는 이름의 API 키 환경변수)
- Codex CLI 사용 가능 상태

확인:

```bash
cargo --version
codex --version
echo "${OPENAI_API_KEY:+set}"
```

## 4. 브리지 실행

프로젝트 루트(`codex/codex-rs`) 기준:

```bash
cargo run -p codex-chat-bridge -- \
  --port 8787 \
  --api-key-env OPENAI_API_KEY
```

옵션 설명:
- `--port`: 브리지 포트 (기본: 랜덤 포트)
- `--api-key-env`: 업스트림 호출에 쓸 API 키 환경변수 이름
- `--upstream-url`: 기본값 `https://api.openai.com/v1/chat/completions`
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
codex-rs/chat-bridge/scripts/run_codex_with_bridge.sh "이 저장소 구조를 설명해줘"
```

추가 인자 전달:

```bash
codex-rs/chat-bridge/scripts/run_codex_with_bridge.sh \
  "테스트 코드 생성해줘" \
  --model gpt-4.1
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
- `wire_api = "chat"`는 지원되지 않습니다.

## 8. 동작 검증 방법

## 8.1 브리지가 받는 요청 확인

브리지 로그를 터미널에서 보고, Codex 요청 시 에러가 없는지 확인합니다.

## 8.2 실제 chat/completions 경유 여부 확인

`--upstream-url`을 명시적으로 지정해 예상 엔드포인트로만 트래픽이 가게 만듭니다.

```bash
cargo run -p codex-chat-bridge -- \
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
cargo run -p codex-chat-bridge -- --port 8787 --api-key-env OPENAI_API_KEY
```

2. Codex를 브리지 provider로 실행

```bash
codex exec \
  -c 'model_providers.chat-bridge={name="Chat Bridge",base_url="http://127.0.0.1:8787/v1",env_key="OPENAI_API_KEY",wire_api="responses"}' \
  -c 'model_provider="chat-bridge"' \
  '작업을 수행해줘'
```
