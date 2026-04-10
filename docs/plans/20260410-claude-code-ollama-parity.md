# Claude Code Ollama 패리티

## 문제 정의
- 목적: Claude Code가 보내는 Anthropic `/v1/messages` 요청을, 외부에서 관찰되는 동작 기준으로 `../ollama`와 최대한 일치시키고, 브리지 전용 예외를 Claude 경로에서 제거한다.
- 범위: `/v1/messages`, `/v1/messages/count_tokens`, Anthropic error envelope, streaming SSE, thinking/reasoning 처리, router 문서와 테스트.
- 성공 기준:
  - Claude route의 request/response shape가 Ollama와 같아진다.
  - `count_tokens`는 로컬 응답이지만 Ollama의 best-effort 추정 규칙과 동일하게 동작한다.
  - bridge-only thinking/reasoning 보정은 Claude route에서 비활성화된다.

## 요구사항 구조화
- 기능 요구사항:
  - Anthropic request 변환에서 `system`, `messages`, `tool_use`, `tool_result`, `thinking`, `server_tool_use`, `web_search_tool_result`를 Ollama 기준으로 처리한다.
  - Anthropic response/error envelope에 Ollama 스타일 `request_id`를 포함한다.
  - streaming event 순서와 stop reason을 Ollama 기준으로 맞춘다.
  - `/v1/messages/count_tokens`는 upstream 전달 없이 로컬 추정치로 응답한다.
- 비기능 요구사항:
  - Claude route만 strict mode로 다루고 다른 router는 기존 동작을 유지한다.
  - 내부 helper 구조는 유지하되 wire-visible contract를 우선한다.
- 우선순위:
  - P0: request/response parity와 error envelope
  - P1: count_tokens 및 identifier parity
  - P2: 문서와 회귀 테스트 정비

## 제약 조건
- 일정/리소스: 단일 저장소 내에서 해결한다.
- 기술 스택/환경:
  - Rust HTTP bridge.
  - Ollama 저장소를 동작 기준(oracle)으로 삼는다.
- 기타:
  - Claude route에서 `anthropic_preserve_thinking`와 `anthropic_enable_openrouter_reasoning`는 strict parity와 충돌하지 않도록 비활성화한다.
  - 외부에서 보이는 contract만 일치시키고, upstream provider별 내부 최적화는 유지한다.

## 아키텍처/설계 방향
- 핵심 설계:
  - Claude request는 `build_upstream_payload` 단계에서 Ollama 규칙으로 정규화한다.
  - Anthropic error path는 `request_id`를 포함한 Ollama 스타일 envelope로 통일한다.
  - `count_tokens`는 기존 local estimate 경로를 유지하되 Ollama와 동일한 계산 규칙을 기준으로 검증한다.
- 대안 및 trade-off:
  - bridge-only thinking/reasoning 보정을 유지하는 안은 기존 유연성이 높지만 parity drift가 남는다.
  - Claude strict mode는 유연성은 줄지만 Ollama와의 행동 일치도가 높다.
- 리스크:
  - thinking/reasoning 제거가 일부 upstream model의 품질에 영향을 줄 수 있다.
  - SSE/event envelope 변경은 회귀 테스트로만 안전하게 통과시켜야 한다.

## 작업 계획
1. Claude route에 대한 strict parity helper를 추가하고, request normalization에서 bridge-only thinking/reasoning 주입을 끊는다.
2. Anthropic error envelope를 Ollama 스타일로 맞춘다.
3. `count_tokens`, ID prefix, streaming stop reason을 회귀 테스트로 고정한다.
4. README와 example config에 Claude strict parity 기준을 반영한다.
5. 전체 테스트를 돌려 Claude route 외 동작이 깨지지 않았는지 확인한다.
