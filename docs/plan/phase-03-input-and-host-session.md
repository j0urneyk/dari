# Phase 3 — 입력 주입과 세션

## 목표

인증된 연결 위에서 호스트는 화면을 스트리밍하고 원격 입력을 주입하며, 뷰어는 프레임을
받고 입력을 보낸다. UI 없이 헤드리스 CLI로 두 기기 간 실제 원격 제어가 되게 만든다.

## 설계

### 입력 프로토콜 (`open-desk-proto` 확장)

- `InputEvent`:
  - `MouseMove { x, y }` — 캡처한 모니터 기준 정규화 좌표(0..=65535)
  - `MouseButton { button, pressed }`, `Scroll { dx, dy }` (픽셀 단위, 상한 적용)
  - `Key { key: KeyCode, pressed }` — OS 중립 키 코드(문자/숫자/기능키/방향키/수정자 등)
  - `Text(String)` — IME 조합 결과처럼 키 코드로 표현할 수 없는 텍스트 (길이 상한)
- 뷰어는 원격 OS가 다르면 수정자 매핑(macOS Cmd ↔ Windows Ctrl)을 적용한다. 기본값은 켜짐.

### `open-desk-input`

- `InputInjector` 트레이트 + `EnigoInjector` 구현, 테스트용 `RecordingInjector`.
- 정규화 좌표 → 대상 모니터의 OS 좌표 변환 (macOS: 포인트, Windows: 물리 픽셀 — DPI 인식 확인)
- 세션 종료 시 눌린 채로 남은 키/버튼을 모두 해제 (stuck key 방지)
- macOS 손쉬운 사용 권한이 없으면 `InjectError::PermissionDenied`

### `open-desk-session`

- tokio 기반. UI와는 `SessionCommand`(입력) / `SessionEvent`(상태, 프레임) 채널로 통신.
- `HostService`: 엔드포인트를 열고, 비밀번호를 관리하며, 인증된 세션마다
  캡처 스레드 → 인코더 → 비디오 스트림, 제어 스트림 → 입력 주입을 연결한다.
  세션 종료 시 비밀번호를 교체한다.
- `ViewerSession`: 접속, 비디오 수신 → 디코딩 스레드 → 최신 프레임만 UI로 전달(오래된 프레임
  덮어쓰기), 입력 송신, 연결 상태/지연 이벤트.
- 키프레임 요청: 뷰어가 디코딩 오류를 만나면 `RequestKeyframe`을 보낸다.

### 헤드리스 CLI (`open-desk` 바이너리의 하위 명령)

- `open-desk host [--port]` — 주소와 비밀번호를 출력하고 대기
- `open-desk connect <addr>` — 비밀번호를 입력받아 접속하고 수신 FPS/비트레이트를 출력
  (화면 표시는 phase 4에서)

## 작업 항목

- 입력 프로토콜, `crates/input`, `crates/session` 구현, CLI 하위 명령(clap)
- 테스트: 합성 캡처러 + 기록 인젝터로 loopback 세션 E2E (프레임 수신, 입력 전달,
  좌표 변환, 세션 종료 시 키 해제, 키프레임 요청 처리)

## 완료 조건

- loopback E2E 테스트가 CI에서 통과
- 로컬에서 `host`/`connect`로 실제 화면 프레임이 수신됨을 확인

## 구현 노트

- 입력 프로토콜: 포인터 좌표는 캡처 디스플레이 기준 0..=65535 정규화 값이다. 문자 키는 "US 배열에서
  수정자 없이 그 문자를 내는 키"(`KeyCode::Character`)로 보내고, 최종 입력 문자는 호스트의 자판
  배열·IME가 결정한다(물리 키보드와 같은 방식이라 원격 한글 IME 조합이 그대로 동작한다).
  한/영, 한자 키는 Windows 호스트에서만 지원한다.
- Windows: enigo의 절대 좌표 이동은 주 모니터 기준이라 보조 모니터에서 틀어진다. 포인터 위치는
  `SetCursorPos`(가상 데스크톱 물리 좌표)로 옮기고, 시작 시 `SetProcessDpiAwarenessContext`로
  Per-Monitor V2 DPI 인식을 켠다. 이 두 Win32 호출만 `unsafe`를 쓴다(`crates/input/src/backend.rs`).
  Windows 전용 코드는 `cargo clippy --target x86_64-pc-windows-msvc`로 로컬에서도 검사했다.
- 세션 계층은 `HostPlatform` 트레이트로 화면/입력을 추상화해 합성 화면 + 기록 입력으로 실제
  QUIC·H.264 경로 전체를 E2E 테스트한다.
- 호스트 캡처가 실패(권한 없음 등)해도 세션은 유지하고 `HostStatus`로 이유를 알린다. 뷰어는 비디오
  스트림이 정상 종료되어도 세션을 끝내지 않는다.
- 연결을 닫기 전에 `Disconnect`가 실제로 전달되도록 양쪽 모두 상대가 연결을 닫기를 최대 1초 기다린다
  (QUIC close는 전송 전 데이터를 버린다).
- 실제 CLI 수동 검증(macOS, loopback): 접속·인증·권한 상태 전파·정상 종료·비밀번호 교체 확인.
  이 개발 환경 프로세스에는 화면 기록/손쉬운 사용 권한이 없어 실제 화면 프레임과 입력 주입은
  권한이 있는 환경(phase 4 GUI 앱)에서 확인한다.
