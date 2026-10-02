# 와이어 프로토콜

호스트와 뷰어는 QUIC(TLS 1.3, ALPN `open-desk/1`) 연결 하나 위에서 이야기합니다. 메시지 타입은 모두
`open-desk-proto`(`crates/proto`)에 있고, 이 문서는 그 형식과 순서를 정리합니다. 현재 프로토콜 버전은
**1.0**(`PROTOCOL_VERSION`)입니다.

## 버전 호환

핸드셰이크의 `ClientHello`와 `ServerHello`가 `ProtocolVersion { major, minor }`를 주고받습니다. `major`가
같으면 호환됩니다. `minor` 증가는 메시지 추가만 뜻하며, 상대가 지원을 알리지 않은 새 메시지는 보내지
않습니다. 호스트는 `major`가 다르면 `Rejected(IncompatibleVersion)`으로 거절합니다.

## 프레이밍

모든 스트림은 같은 `MessageCodec<T>`를 씁니다.

```text
┌──────────────────────┬────────────────────────────┐
│ length: u32 (BE)     │ postcard(T), length bytes  │
└──────────────────────┴────────────────────────────┘
```

길이 헤더가 채널 상한을 넘으면 본문을 버퍼링하기 전에 오류로 연결을 끊습니다. 디코딩된 메시지는
`Validate`를 통과해야만 호출자에게 전달됩니다.

| 채널 | 메시지 타입 | 프레임 상한 | 스트림 |
| --- | --- | --- | --- |
| 핸드셰이크 | `HandshakeMessage` | 4 KiB | 뷰어가 여는 양방향 스트림 |
| 제어 | `ControlMessage` | 2 MiB | 인증이 끝난 핸드셰이크 스트림을 그대로 사용 |
| 비디오 | `VideoPacket` | 16 MiB | 호스트가 여는 단방향 스트림 |
| 릴레이 제어 | `RelayRequest` / `RelayResponse` | 2 MiB | 릴레이와의 양방향 스트림(ALPN `open-desk-relay/1`) |

제어 채널 상한은 계획(64 KiB)보다 큰 2 MiB입니다. 최대 1 MiB의 클립보드 텍스트가 같은 채널을 쓰기
때문입니다. 인증이 끝나면 핸드셰이크 스트림의 코덱만 바꿔(`map_decoder`/`map_encoder`) 제어 스트림으로
쓰므로, 핸드셰이크 직후에 도착해 이미 버퍼에 있는 제어 메시지도 잃지 않습니다. QUIC 전송 설정이 상대가 열 수
있는 스트림 수를 제한합니다. 뷰어는 양방향 스트림 하나(핸드셰이크 후 제어)만 열 수 있고 단방향 스트림은 열 수
없으며, 미디어용 단방향 스트림은 호스트만 엽니다.

## 핸드셰이크

```text
viewer                                         host
  │── ClientHello {version, name, os} ─────────►│  버전 확인, 사전 검사(busy/throttle/not accepting)
  │◄──────────── ServerHello {version, name, os}│  (또는 Outcome(Rejected))
  │── Pake(SPAKE2 A) ──────────────────────────►│
  │◄─────────────────────────── Pake(SPAKE2 B) ─│
  │── Confirmation(MAC_viewer) ────────────────►│  상수 시간 비교, 세션 슬롯 확보·비밀번호 소모
  │◄──────────────────── Confirmation(MAC_host) ─│
  │◄────────────────────── Outcome(Accepted) ────│
```

- SPAKE2는 Ed25519 그룹을 쓰며 신원 문자열은 뷰어 `open-desk viewer`, 호스트 `open-desk host`입니다.
  비밀번호는 정규화된 형식(대문자 10자, 구분자 없음)의 ASCII 바이트입니다. 화면에는 `K7MXQ-3PTWA`처럼
  표시하고, 입력할 때는 대소문자와 공백·`-`를 무시합니다.
- `exporter`는 TLS `export_keying_material(32, "EXPORTER-open-desk-auth-v1")`입니다.
- `transcript`는 두 hello의 postcard 인코딩을 각각 4바이트 길이 접두사와 함께 SHA-256으로 해시한 값입니다.
- 확인값은 `HMAC-SHA256(K, "open-desk key confirmation v1" ‖ role ‖ exporter ‖ transcript)`이며 `role`은
  `"viewer"` 또는 `"host\0\0"`입니다.
- 호스트는 뷰어의 확인값을 검증한 **뒤에만** 자기 확인값을 보냅니다. 비밀번호를 모르는 뷰어는 호스트로부터
  비밀번호를 검증할 단서를 하나도 얻지 못합니다.
- 핸드셰이크 전체는 10초 안에 끝나야 합니다.

거절 이유(`RejectReason`)는 일부러 거칠게 나눕니다: `IncompatibleVersion`, `AuthenticationFailed`, `Busy`,
`TooManyAttempts`, `NotAccepting`. 거절할 때는 스트림을 `finish()`하고 최대 2초 동안 뷰어가 닫기를 기다린
뒤 연결을 닫습니다. QUIC의 즉시 close는 아직 보내지 않은 데이터를 버리기 때문입니다.

## 제어 메시지

| 메시지 | 방향 | 의미 |
| --- | --- | --- |
| `Ping { token }` / `Pong { token }` | 양방향 | 왕복 시간 측정 |
| `Disconnect` | 양방향 | 정상 종료. 보낸 쪽은 상대가 닫기를 최대 1초 기다림 |
| `Input(InputEvent)` | 뷰어 → 호스트 | 키보드·포인터 입력(아래 참조) |
| `RequestKeyframe` | 뷰어 → 호스트 | 디코더 상태를 잃었으니 키프레임을 보내 달라 |
| `HostStatus { screen, input }` | 호스트 → 뷰어 | 세션 시작 시와 변할 때마다 보내는 기능 상태 |
| `AwaitingApproval` | 호스트 → 뷰어 | 호스트 사용자에게 승인을 묻는 중 |
| `Declined` | 호스트 → 뷰어 | 호스트 사용자가 거부함. 곧 연결이 닫힘 |
| `Displays { displays, active }` | 호스트 → 뷰어 | 보여 줄 수 있는 디스플레이(최대 16개)와 현재 디스플레이 |
| `SelectDisplay(id)` | 뷰어 → 호스트 | 다른 디스플레이로 전환 |
| `SetQuality(preset)` | 뷰어 → 호스트 | `Speed` / `Balanced` / `Quality` |
| `Clipboard(text)` | 양방향 | 클립보드 텍스트 변경(최대 1 MiB, NUL 금지) |

`Availability`는 `Available`, `PermissionDenied`(macOS 권한 없음), `Unavailable`, `NotAllowed`(보기 전용
세션의 입력) 중 하나입니다.

## 입력 이벤트

| 이벤트 | 내용과 제한 |
| --- | --- |
| `PointerMove(PointerPosition { x, y })` | 캡처한 디스플레이 기준 정규화 좌표. `0`이 왼쪽/위, `u16::MAX`가 오른쪽/아래. 양쪽의 해상도나 DPI와 무관 |
| `PointerButton { button, pressed }` | `Left`, `Right`, `Middle`, `Back`, `Forward` |
| `Scroll { dx, dy }` | 휠 줄 단위, 축마다 ±100. 양수 `dy`는 아래, 양수 `dx`는 오른쪽 |
| `Key { key, pressed }` | `KeyCode::Character(c)` 또는 `KeyCode::Named(NamedKey)`. 제어 문자 금지, `Function(n)`은 1..=20 |
| `Text(String)` | 키로 표현할 수 없는 텍스트, 1..=256자, 제어·보이지 않는 서식 문자 금지 |

`KeyCode::Character`는 "US 배열에서 수정자 없이 그 문자를 내는 키"입니다. 최종 문자는 호스트의 자판 배열과
IME가 결정하므로 물리 키보드와 똑같이 동작하고, 원격 한글 조합도 그대로 됩니다. `NamedKey`에는 방향키,
편집 키, F1–F20, 수정자(`Shift`, `Control`, `Alt`=Option, `Meta`=⌘/Windows 키), `CapsLock`, `PrintScreen`,
`Pause`, `NumLock`, `HangulMode`(한/영), `HanjaMode`(한자)가 있습니다. `Insert`, `PrintScreen`, `Pause`,
`NumLock`, 한/영, 한자 키는 enigo가 Windows에서만 제공하므로 macOS 호스트에서는 무시됩니다.

⌘↔Ctrl 매핑은 뷰어가 적용합니다(`ModifierMapping`). 양쪽의 단축키 수정자(macOS는 `Meta`, 그 밖에는
`Control`)가 다르고 설정이 켜져 있으면 두 키를 서로 맞바꿉니다. 그래서 macOS 뷰어의 ⌘C는 Windows 호스트에
Ctrl+C로, Windows 뷰어의 Ctrl+C는 macOS 호스트에 ⌘C로 도착합니다.

## 비디오

```text
VideoPacket { sequence: u64, timestamp_us: u64, keyframe: bool, width: u32, height: u32, data: Vec<u8> }
```

`data`는 H.264 Annex-B 비트스트림입니다. 가로·세로는 1..=8192여야 하고 `data`는 비어 있으면 안 됩니다.
디코더는 3840×2160을 넘는 출력 프레임을 거부합니다. 인코더는 세션 시작, 해상도 변경, 디스플레이·화질
전환, `RequestKeyframe` 때 키프레임을 냅니다.

## 문자열 검증

이름 같은 표시용 문자열(`client_name`, `host_name`, 디스플레이 이름)은 64자 이하이고, 제어 문자와 보이지
않는 서식 문자(U+200B–U+200F, U+202A–U+202E, U+2060–U+206F, U+FEFF)를 포함하면 거부합니다. 양방향 덮어쓰기
문자로 이름을 다른 것처럼 보이게 하는 공격을 막기 위해서입니다. 자기 이름을 보낼 때는
`sanitize_display_text`로 같은 규칙에 맞춰 정리합니다.

## 릴레이

릴레이 제어 연결은 별도 ALPN(`open-desk-relay/1`)의 QUIC 연결입니다. 기본 포트는 UDP 47822입니다.

| 메시지 | 방향 | 의미 |
| --- | --- | --- |
| `RelayRequest::Register` | 호스트 → 릴레이 | 클라이언트 인증서 지문에 묶인 ID로 등록 |
| `RelayRequest::Connect { id }` | 뷰어 → 릴레이 | 이 ID의 호스트에 접속 요청 |
| `RelayResponse::Registered { id }` | 릴레이 → 호스트 | 등록된 9자리 ID |
| `RelayResponse::Incoming(Allocation)` | 릴레이 → 호스트 | 뷰어가 왔으니 이 할당에 바인딩하고 QUIC을 받아라 |
| `RelayResponse::Allocated(Allocation)` | 릴레이 → 뷰어 | 이 할당을 통해 접속하라 |
| `RelayResponse::Refused(RelayError)` | 릴레이 → 기기 | `NotFound`, `TooManyRequests`, `CertificateRequired`, `Unavailable` |

`Allocation { port, token }`은 한쪽이 보낼 릴레이 UDP 포트와 16바이트 토큰입니다. 각 쪽은 자기 소켓에서
`"ODRB" ‖ token` 데이터그램을 그 포트로 보내 주소를 바인딩하고, 릴레이는 `"ODRA" ‖ token`으로 확인합니다.
클라이언트는 확인이 올 때까지 400ms 간격으로 최대 6번 다시 보냅니다. 두 쪽이 모두 바인딩되면 릴레이는 두
주소 사이의 데이터그램(최대 65,535바이트)을 내용을 보지 않고 전달하며, 그 위에서 위의 QUIC 세션이 그대로
진행됩니다. `DeviceId`는 100000000..=999999999 범위이며 `123 456 789`로 표시하고, 입력할 때는 공백과 `-`를
무시합니다.

## 주요 상수

| 상수 | 값 | 위치 |
| --- | --- | --- |
| 기본 호스트 포트 | UDP 47821 | `crates/app/src/config.rs` |
| 기본 릴레이 포트 | UDP 47822 | `crates/proto/src/relay.rs` |
| QUIC idle timeout / keep-alive | 30초 / 5초 | `crates/net/src/tls.rs` |
| 핸드셰이크 제한 시간, 동시 핸드셰이크 | 10초, 8개 | `crates/net/src/endpoint.rs` |
| 승인 대기 | 30초 | `crates/session/src/host_session.rs` |
| 클립보드 폴링 간격 | 250ms | `crates/session/src/clipboard.rs` |
| 뷰어 입력 큐 / 키용 예약분 | 512 / 128 | `crates/session/src/viewer.rs` |
| mDNS 서비스 | `_open-desk._udp.local.` | `crates/net/src/discovery.rs` |
