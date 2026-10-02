# 아키텍처

Dari는 앱 하나가 호스트(내 화면 공유)와 뷰어(원격 화면 제어)를 모두 맡습니다. 코드는 계층별
크레이트로 나뉘고 의존은 한 방향으로만 흐릅니다. 그래서 세션 로직 전체를 UI 없이 테스트할 수 있고,
GUI가 잠시 멈춰도 네트워크와 미디어 처리는 계속 돕니다.

```text
dari (app) ──► dari-session ──► dari-net ───┐
                                      ├──► dari-media  ├──► dari-proto
                                      └──► dari-input ─┘
dari-relay ──► dari-net, dari-proto
```

## 크레이트

| 크레이트 | 경로 | 책임 | 주요 의존성 |
| --- | --- | --- | --- |
| `dari-proto` | `crates/proto` | 메시지 타입, 프로토콜 버전, 길이 제한 프레이밍, 메시지 검증. I/O 없음 | serde, postcard, tokio-util |
| `dari-net` | `crates/net` | 기기 인증서, 일회용 비밀번호, SPAKE2 핸드셰이크, 시도 제한, QUIC 엔드포인트, mDNS 검색, 릴레이 클라이언트 | quinn, rustls(ring), rcgen, spake2, mdns-sd |
| `dari-media` | `crates/media` | 디스플레이 열거·캡처, 축소, H.264 인코드/디코드, 페이싱된 캡처 스레드 | xcap, fast_image_resize, openh264 |
| `dari-input` | `crates/input` | 입력 주입, 눌린 키 추적, ⌘↔Ctrl 매핑, Windows DPI/커서 처리 | enigo, windows |
| `dari-session` | `crates/session` | 호스트 서비스, 호스트 세션(승인·캡처·입력·클립보드), 뷰어 세션 | tokio, arboard |
| `dari-relay` | `crates/relay` | 랑데부(ID 발급)와 UDP 전달 서버 바이너리 | quinn, tokio |
| `dari` | `crates/app` | gpui-kit 데스크톱 앱과 헤드리스 CLI(`host`, `connect`) | gpui-kit, clap, directories, toml |

### 외부 의존성

바퀴를 다시 만들지 않는다는 원칙으로 각 영역에 널리 쓰이는 크레이트를 골랐습니다.

| 영역 | 크레이트 |
| --- | --- |
| UI | `gpui-kit` 0.7(GPUI 스냅샷을 `=` 버전으로 고정해 다시 내보냄) |
| 비동기·네트워크 | `tokio`, `tokio-util`, `quinn`, `rustls`(ring provider), `rcgen` |
| 인증·암호 | `spake2`, `hmac`, `sha2`, `subtle`, `zeroize`, `getrandom` |
| 직렬화 | `serde`, `postcard` |
| 화면 캡처·비디오 | `xcap`, `openh264`(Cisco OpenH264 소스 빌드), `fast_image_resize` |
| 입력 주입 | `enigo`, Windows는 `windows` 크레이트 |
| 클립보드·LAN 검색 | `arboard`, `mdns-sd` |
| 설정·로깅·오류·CLI | `directories`, `toml`, `tracing`, `tracing-subscriber`, `thiserror`, `anyhow`, `clap`, `sys-locale` |
| 패키징 | `cargo-packager` |

## 스레드와 실행기

앱은 두 실행기를 함께 씁니다. GPUI가 메인 스레드에서 UI를 돌리고, tokio 멀티스레드 런타임이 네트워크와
세션을 돌립니다. tokio 런타임은 GPUI 전역(`runtime::TokioRuntime`)으로 설치되고, UI 쪽은 GPUI 태스크에서
tokio 채널이나 `JoinHandle`을 await해 결과를 받습니다. tokio 동기화 도구는 어느 실행기에서 await해도
동작하므로 두 실행기 사이에 별도 다리가 필요 없습니다.

CPU를 많이 쓰거나 블로킹 API를 쓰는 작업은 전용 OS 스레드에서 돕니다.

- **캡처 스레드**(호스트): 목표 FPS에 맞춰 캡처 → 축소 → H.264 인코딩. 플랫폼 캡처 핸들은 `Send`가 아니므로
  스레드 안에서 팩토리 클로저로 엽니다.
- **입력 스레드**(호스트): 받은 입력 이벤트를 enigo로 주입합니다. enigo 백엔드도 스레드에 묶어 둡니다.
- **디코드 스레드**(뷰어): H.264를 BGRA로 디코딩해 최신 프레임만 `watch` 채널에 올립니다.

## 호스트 쪽 흐름

`dari_session::start_host`가 `HostEndpoint`(QUIC 서버)를 열고 호스트 서비스 태스크를 띄웁니다. UI는
`HostHandle`로 명령(비밀번호 재발급, 수락 on/off, 승인·클립보드 정책 변경, 세션 종료)을 보내고
`HostEvent` 채널로 상태를 받습니다(`PasswordChanged`, `ApprovalRequested`, `SessionStarted`,
`SessionStatus`, `SessionEnded`, `Relay`).

1. **인증**: `HostEndpoint`가 들어오는 연결마다 핸드셰이크를 동시에(최대 8개, 각 10초) 돌립니다. 시도 제한에
   걸린 출발지는 TLS 전에 거절합니다. 인증에 성공하면 세션 슬롯을 차지하고 비밀번호를 소모합니다.
   자세한 절차는 [보안 모델](security.md)과 [프로토콜](protocol.md#핸드셰이크)에 있습니다.
2. **승인**: 승인이 켜져 있으면 `AwaitingApproval`을 보내고 호스트 사용자의 결정(제어 허용 / 보기만 허용 /
   거부, 30초 제한)을 기다립니다. 이 동안에는 캡처·입력·클립보드 중 아무것도 만들지 않고 들어온 입력은
   버립니다.
3. **세션 시작**: 디스플레이 목록과 `HostStatus`를 보내고 캡처 스트림을 엽니다. 비디오 펌프 태스크는 세션 내내
   살아 있으면서 캡처 스트림에서 받은 패킷을 단방향 비디오 스트림으로 씁니다. 제어가 허용되면 입력 스레드와
   클립보드 동기화를 시작합니다.
4. **세션 중**: 제어 스트림 메시지를 처리합니다. `SelectDisplay`와 `SetQuality`는 캡처 스트림만 다시 열고
   (새 인코더가 키프레임부터 보냄) 비디오 펌프와 입력 좌표계는 새 디스플레이로 이어 갑니다.
   `RequestKeyframe`은 인코더에 키프레임을 요청합니다.
5. **종료**: `Disconnect`를 보내고 상대가 연결을 닫기를 최대 1초 기다립니다. 입력 큐에 남은 이벤트는 버리고,
   눌린 채로 남은 키·버튼은 해제합니다. 서비스는 새 비밀번호를 발급합니다.

화면과 입력은 `HostPlatform` 트레이트(`displays`, `open_capturer`, `open_input`, `clipboard`) 뒤에
있습니다. 실제 앱은 `SystemPlatform`을, 테스트는 합성 화면과 기록 입력을 씁니다. 덕분에 QUIC과 H.264를
포함한 실제 경로 전체를 CI에서 E2E로 검증할 수 있습니다.

캡처가 실패해도(권한 없음, 보안 데스크톱 등) 세션은 끝나지 않습니다. 일시적 실패는 30초까지 재시도하고,
그래도 안 되면 `HostStatus`로 `PermissionDenied`나 `Unavailable`을 알립니다.

### 캡처와 백프레셔

캡처 스레드는 채널에 자리가 있을 때만(`try_reserve`) 캡처와 인코딩을 합니다. 네트워크가 밀리면 프레임을
**인코딩하기 전에** 건너뜁니다. 인코딩한 뒤 버리면 다음 P-프레임의 참조가 깨지기 때문입니다. 축소는 긴 변
기준이고, 가로·세로를 짝수로 맞추며 OpenH264 한도(3840×2160) 안으로 제한합니다.

인코더는 OpenH264의 `ScreenContentRealTime` 모드를 씁니다. 이 모드에서는 프레임 건너뛰기를 켜야 목표
비트레이트를 지킵니다. 건너뛴 프레임은 출력되지 않을 뿐이라 참조 체인에는 영향이 없습니다. 화면 콘텐츠에서
지원하지 않는 적응형 양자화와 배경 감지는 끕니다. 캡처 해상도가 바뀌면 인코더를 새로 만들어 키프레임부터
보냅니다. 캡처는 `ScreenCapturer` 트레이트 뒤에 있고, 실제 구현은 `XcapCapturer`, 테스트용은 움직이는 패턴을
만드는 `SyntheticCapturer`입니다.

| 품질 프리셋 | 긴 변 최대 | 비트레이트 |
| --- | --- | --- |
| 속도 | 1280px | 1.5 Mbps |
| 균형(기본) | 1920px | 4 Mbps |
| 화질 | 2560px | 10 Mbps |

FPS 상한은 30입니다. 프리셋은 뷰어가 요청하고 호스트가 자기 상한으로 매핑합니다(`host_session.rs`).

### 입력 좌표와 DPI

뷰어는 포인터 위치를 캡처한 디스플레이 기준 정규화 좌표(0..=65535)로 보내고, 호스트가 대상 디스플레이의 OS
좌표로 바꿉니다. macOS는 포인트, Windows는 물리 픽셀 단위입니다. Windows에서 enigo의 절대 좌표 이동은 주
모니터 기준이라 보조 모니터에서 위치가 틀어집니다. 그래서 포인터는 가상 데스크톱 물리 좌표를 받는
`SetCursorPos`로 옮기고, 프로세스 시작 시 `SetProcessDpiAwarenessContext`로 Per-Monitor V2 DPI 인식을
켭니다(매니페스트 대신 실행 시 설정). 디스플레이를 바꾸면 입력 좌표계도 새 디스플레이를 따릅니다.

## 뷰어 쪽 흐름

`connect_viewer`가 대상(`ViewerTarget::Direct(주소)` 또는 `Relay { relay, id }`)에 접속하고 인증한 뒤
`ViewerHandle`과 `ViewerEvent` 채널을 돌려줍니다.

- 비디오 스트림 → 디코드 스레드(큐 4) → `watch::Sender<Option<Arc<DecodedFrame>>>`. UI는 항상 최신 프레임만
  봅니다. 디코딩에 실패하면 `RequestKeyframe`을 보냅니다.
- 입력은 큐(512)로 보냅니다. 그중 128칸은 키·버튼 이벤트용으로 남겨 두어, 네트워크가 밀려도 포인터 이동이
  큐를 채워 키 떼기가 버려지는 일이 없게 합니다.
- 비디오 스트림이 정상 종료되어도 세션은 끝내지 않습니다. 세션의 끝은 제어 스트림이 정합니다.
- 클립보드 공유는 호스트가 제어를 허용했을 때만 켜지고, 호스트가 나중에 보기 전용을 알리면 꺼집니다.

## 데스크톱 앱

`crates/app`은 라이브러리와 얇은 바이너리(`main.rs`)로 나뉩니다. GPUI의 macOS 플랫폼은 메인 스레드에서만
만들 수 있어서, GUI 테스트(`tests/gui.rs`, `harness = false`)가 라이브러리를 직접 띄울 수 있어야 하기
때문입니다.

| 모듈 | 역할 |
| --- | --- |
| `lib.rs` | 로깅 초기화, 인자 파싱. 하위 명령이 없으면 GUI, 있으면 CLI |
| `home.rs` | 홈 창: "이 기기" 카드(주소, 비밀번호, 릴레이 ID, 승인 카드, 권한 안내, 설정)와 "원격 기기 제어" 카드(주소·비밀번호, 최근 주소, 근처 기기) |
| `viewer.rs` | 뷰어 창: 프레임을 `canvas`에 `paint_image`로 그리고, 입력을 프로토콜 이벤트로 변환. 툴바(디스플레이, 화질, 초당 프레임·왕복 지연, 연결 끊기) |
| `video_layout.rs` | 레터박스 계산과 창 좌표 → 정규화 좌표 변환 |
| `keymap.rs` | GPUI `Keystroke` → 프로토콜 `KeyCode` |
| `state.rs`, `runtime.rs` | 앱 상태(기기 인증서, 설정)와 tokio 런타임을 GPUI 전역으로 보관 |
| `settings.rs`, `config.rs` | `settings.toml` 저장/로드, 데이터 디렉터리, 주소·ID 해석, 로컬 주소 목록 |
| `permissions.rs` | macOS 화면 기록·손쉬운 사용 권한 확인과 요청(3초마다 갱신) |
| `text.rs` | 한국어/영어 UI 문자열(시스템 로캘로 선택) |
| `cli.rs` | 헤드리스 `host`/`connect` |

뷰어 창은 프레임마다 새 `RenderImage`를 만들고 이전 이미지는 `window.drop_image`로 GPU 아틀라스에서
해제합니다. 이것을 빼면 6초 스트리밍만으로 메모리가 약 280 MB 늘어나는 것을 GUI 테스트로 확인했습니다.

gpui-kit의 `Root`는 Tab, Shift-Tab, ⌘C/Ctrl+C를 포커스 이동과 복사에 씁니다. 원격 화면의 키 컨텍스트
(`RemoteScreen`)에서는 이 키들을 `NoAction`으로 풀어서 원격 기기로 전달되게 합니다. 창이 포커스를 잃으면
눌린 키·버튼·수정자를 모두 떼어 원격에 키가 눌린 채로 남지 않게 합니다.

## 릴레이

릴레이는 세션 내용을 모릅니다. 호스트는 기기 인증서를 TLS 클라이언트 인증서로 제시해 등록하고 9자리 ID를
받습니다. 뷰어가 ID로 접속을 요청하면 릴레이가 UDP 포트 한 쌍을 할당하고, 두 쪽은 그 포트를 통해 직접
접속과 똑같은 QUIC + SPAKE2 세션을 맺습니다. 호스트 쪽 릴레이 접속은 `RelayedAcceptor`를 거쳐
`HostEndpoint`의 인증 경로로 그대로 들어가므로 비밀번호, 시도 제한, 단일 세션, 승인이 직접 접속과 똑같이
적용됩니다. 등록이 끊기면 호스트 서비스는 2초에서 60초까지 지수 백오프로 다시 등록합니다.

운영 방법은 [릴레이 서버 운영](relay.md), 메시지 형식은 [프로토콜](protocol.md#릴레이)에 있습니다.

## LAN 검색

호스트는 mDNS로 `_dari._udp.local.` 서비스를 광고합니다(이름, OS, 인증서 지문 앞 4바이트). 뷰어는
이를 "근처 기기" 목록에 보여 줄 뿐이고, 접속할 때는 항상 비밀번호 인증을 거칩니다. 인스턴스 이름은 DNS
레이블 한도(63바이트)에 맞춰 자르고, 자기 자신의 광고는 지문 힌트로 걸러 냅니다. 루프백 주소는 다른 기기에
쓸모가 없어 결과에서 뺍니다(같은 기기 안의 테스트에서만 포함).
