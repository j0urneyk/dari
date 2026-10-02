# Phase 1 — 프로토콜과 보안 전송

## 목표

두 프로세스가 QUIC으로 연결하고, 일회용 비밀번호로 상호 인증한 뒤, 길이 제한이 있는
타입 메시지를 주고받을 수 있게 한다. UI와 미디어 없이 테스트로 완전히 검증 가능한 단계다.

## 설계

### `open-desk-proto`

- `PROTOCOL_VERSION` 상수와 호환성 규칙(메이저 불일치 시 거부)
- 메시지 (serde + postcard):
  - 핸드셰이크: `ClientHello { version, client_name, client_os }`,
    `ServerHello { version, host_name, host_os }`, `PakeMessage(bytes)`,
    `KeyConfirmation([u8; 32])`, `AuthResult { Ok | Rejected(reason) }`
  - 제어: `ControlMessage` (phase 3 이후 입력·클립보드·설정 메시지가 추가된다)
  - 비디오: `VideoPacket { seq, timestamp_us, keyframe, width, height, data }`
- 프레이밍: 4바이트 길이 접두사 + postcard 본문. 채널별 최대 길이
  (핸드셰이크 4 KiB, 제어 64 KiB, 비디오 16 MiB)를 넘으면 즉시 연결 오류.
  `tokio-util`의 `LengthDelimitedCodec`을 감싼 `FramedMessages<T>`를 제공한다.
- 문자열 필드는 길이 상한을 검증한다 (이름 64자 등).

### `open-desk-net`

- 인증서: `rcgen`으로 자체 서명 인증서를 만들고, 설정 디렉터리에 저장 (Unix에서 개인키 파일 0600).
  인증서 SHA-256 지문을 기기 식별자로 쓴다.
- QUIC 설정: ALPN `open-desk/1`, idle timeout 30초, keep-alive 5초,
  동시 bidi/uni 스트림 수 제한.
- 서버 인증서 검증기: CA 체인 검증 대신 지문을 기록하고, TLS 1.3 서명 검증은 rustls 암호
  provider로 반드시 수행한다. (인증 자체는 아래 PAKE가 담당)
- 인증 핸드셰이크 (제어 스트림 위에서, 전체 10초 타임아웃):
  1. 뷰어 → `ClientHello`, 호스트 → `ServerHello` (버전 확인)
  2. SPAKE2(Ed25519 그룹, 신원 `open-desk-viewer` / `open-desk-host`) 메시지 교환
  3. `exporter = TLS export_keying_material(32, "EXPORTER-open-desk-auth-v1")`
  4. 확인값 `HMAC-SHA256(K, role || exporter)` 교환 — 뷰어가 먼저 보내고 호스트가 상수 시간
     비교로 검증한 뒤 자기 확인값을 보낸다. 뷰어도 검증한다.
  5. 호스트는 `AuthResult`로 결과를 알린다. 실패 시 이유는 일반화된 메시지만 보낸다.
- 비밀번호: 혼동 문자를 뺀 32자 알파벳에서 10자(약 50비트), `zeroize`로 메모리에서 지운다.
- 시도 제한: IP별 연속 실패 시 지수 백오프(최대 5분), 전역 분당 실패 상한.
  임계값을 넘으면 핸드셰이크를 시작하기 전에 연결을 닫는다.
- 단일 세션: 이미 인증된 세션이 있으면 새 연결은 `Busy`로 거부한다.
- API: `HostEndpoint::bind(addr, identity)`, `HostEndpoint::accept() -> AuthenticatedConnection`,
  `connect(addr, password) -> AuthenticatedConnection`. 인증된 연결은 제어 스트림과
  비디오 스트림을 여는 메서드를 제공한다.

## 작업 항목

- `crates/proto`, `crates/net` 생성 및 위 설계 구현
- 단위 테스트: 프레이밍 상한 초과 거부, 메시지 round-trip, 비밀번호 생성 분포/알파벳
- 통합 테스트(loopback):
  - 올바른 비밀번호로 인증 성공 후 메시지 왕복
  - 틀린 비밀번호 거부, 상대에게 확인값이 누설되지 않음
  - 버전 불일치 거부
  - 반복 실패 시 백오프 적용
  - 인증 전 큰 프레임 전송 시 연결 종료
  - 두 번째 동시 세션 `Busy`

## 완료 조건

- 위 테스트가 macOS·Windows CI에서 통과
- 인증 전에는 어떤 제어/비디오 스트림도 열리지 않음이 테스트로 보장됨
