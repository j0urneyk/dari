# open-desk

macOS와 Windows 11이 서로 접속할 수 있는 원격 데스크톱 프로그램입니다. AnyDesk, RustDesk,
Windows 원격 데스크톱과 같은 용도로, 하나의 앱이 **내 화면 공유(호스트)**와 **원격 기기 제어(뷰어)**를
모두 합니다. Rust와 [gpui-kit](https://github.com/longbridge/gpui-kit)로 만들었습니다.

- 같은 네트워크: IP 주소 또는 "근처 기기" 목록으로 바로 접속
- 다른 네트워크: 직접 운영하는 [릴레이 서버](docs/relay.md)를 거쳐 9자리 ID로 접속
- 일회용 비밀번호 + 호스트 사용자의 접속 승인(제어 허용 / 보기만 허용 / 거부)
- 화면 스트리밍(H.264), 키보드·마우스·휠 제어, 텍스트 클립보드 공유, 모니터 전환, 화질 프리셋
- ⌘ ↔ Ctrl 단축키 자동 변환, 한국어/영어 UI

## 설치

[Releases](https://github.com/j0urneyk/open-desk/releases)에서 받습니다.

| 플랫폼 | 파일 |
| --- | --- |
| macOS (Apple silicon) | `open-desk_<버전>_macos_aarch64.dmg` |
| Windows 11 (x64) | `open-desk_<버전>_x64-setup.exe` |
| 릴레이 서버 (Linux x64) | `open-desk-relay_<버전>_linux_x86_64.tar.gz` |

코드 서명 인증서 없이 빌드된 경우 macOS에서는 처음 실행할 때 Finder에서 앱을 우클릭 → **열기**를,
Windows에서는 SmartScreen 경고에서 **추가 정보 → 실행**을 선택해야 합니다.

### macOS 권한

내 화면을 공유하려면(호스트) 시스템 설정 → 개인정보 보호 및 보안에서 두 권한을 허용해야 합니다.
앱의 "이 기기" 카드에 빠진 권한과 **권한 요청 / 시스템 설정 열기** 버튼이 표시됩니다.

- **화면 및 시스템 오디오 녹음**: 상대가 화면을 보려면 필요합니다. 허용 후 앱을 다시 시작하세요.
- **손쉬운 사용**: 상대가 키보드와 마우스를 제어하려면 필요합니다.
- **로컬 네트워크**: 처음 실행 시 묻는 창에서 허용해야 같은 네트워크의 기기와 연결됩니다.

Windows에서는 처음 실행할 때 방화벽 창에서 개인 네트워크 접근을 허용하세요.

## 사용법

**내 화면 공유하기**: 앱을 켜면 "이 기기" 카드에 접속 주소와 일회용 비밀번호가 표시됩니다. 상대에게
알려 주고, 접속 요청이 오면 **제어 허용**, **보기만 허용**, **거부** 중 하나를 고릅니다(30초 안에
응답하지 않으면 거부). 세션 중에는 **연결 끊기**로 언제든 끝낼 수 있고, 세션이 끝나면 비밀번호가 새로
바뀝니다.

**원격 기기 제어하기**: "원격 기기 제어" 카드에 상대의 주소(IP, IP:포트, 또는 릴레이를 쓰면 9자리 ID)와
비밀번호를 입력하고 **접속**합니다. 같은 네트워크의 기기는 "근처 기기"에서 고를 수 있습니다. 뷰어 창
툴바에서 모니터 전환, 화질(속도/균형/화질), 연결 끊기를 할 수 있습니다.

**다른 네트워크의 기기**: 공인 IP가 있는 서버에서 `open-desk-relay`를 실행하고([운영 문서](docs/relay.md)),
양쪽 앱의 **릴레이 서버** 칸에 그 주소를 입력합니다. 호스트에 표시되는 **내 ID**로 접속합니다.

**명령줄(개발·서버용)**:

```bash
open-desk host [--port 47821] [--relay 서버주소]
open-desk connect <주소 또는 ID> [--relay 서버주소]
```

헤드리스 호스트는 승인할 사람이 없으므로 비밀번호를 아는 뷰어에게 바로 제어를 허용합니다.

## 보안 모델

자세한 위협 모델과 근거는 [보안 모델](docs/security.md)에 있습니다.

- 연결은 QUIC(TLS 1.3)로 암호화되고, 인증은 일회용 비밀번호를 쓰는 **SPAKE2** PAKE로 합니다. 확인값이
  TLS 세션(exporter)에 묶여 있어 중간자는 세션을 가로챌 수 없고, 도청자는 비밀번호를 오프라인으로 대입해
  볼 수 없습니다. 비밀번호는 10자(약 50비트)이며 한 번 쓰면 폐기됩니다.
- 실패한 시도는 출발지별·전역으로 지수 백오프 제한되고, 한 번에 한 세션만 허용됩니다.
- 접속 승인이 켜져 있으면(기본값) 호스트 사용자가 허용하기 전에는 화면·입력·클립보드 어느 것도 공유되지
  않습니다. 보기 전용 세션은 입력과 클립보드를 아예 받지 않습니다.
- 릴레이 서버는 암호화된 UDP 패킷만 전달하므로 화면, 입력, 비밀번호를 볼 수 없습니다.
- LAN 검색 정보는 인증되지 않은 표시용 정보일 뿐이며, 접속 시 항상 비밀번호 인증을 거칩니다.

## 알려진 한계

- Windows의 보안 데스크톱(UAC 확인 창, 잠금 화면, Ctrl+Alt+Del)은 일반 앱에서 캡처·제어할 수 없습니다.
- 소프트웨어 H.264 인코딩만 지원합니다(하드웨어 인코더 미사용). 오디오와 파일 전송은 없습니다.
- macOS Apple silicon과 Windows x64 빌드만 제공합니다.
- 릴레이 경유 세션 중 클라이언트의 공인 주소가 바뀌면(NAT 재바인딩) 다시 접속해야 합니다.

## 개발

- Rust 1.99.0 (`rust-toolchain.toml`이 버전과 rustfmt/clippy 컴포넌트를 고정)
- macOS: Xcode Command Line Tools / Windows 11: Visual Studio Build Tools (MSVC)
- [cargo-deny](https://github.com/EmbarkStudios/cargo-deny), 패키징에는 [cargo-packager](https://github.com/crabnebula-dev/cargo-packager)

```bash
cargo run -p open-desk                     # 앱 실행
cargo fmt --all --check                    # 포매터
cargo lint                                 # clippy (-D warnings, 별칭)
cargo test --workspace --locked            # 테스트
cargo test -p open-desk --test gui         # macOS 헤드리스 GUI 테스트 (Metal)
cargo deny check                           # 의존성 보안·라이선스 검사
```

크레이트 구성:

| 크레이트 | 역할 |
| --- | --- |
| `open-desk-proto` | 메시지, 버전, 길이 제한 프레이밍 |
| `open-desk-net` | QUIC, 기기 인증서, SPAKE2 인증, 시도 제한, LAN 검색, 릴레이 클라이언트 |
| `open-desk-media` | 화면 캡처, 스케일링, H.264 인코드/디코드 |
| `open-desk-input` | 원격 입력 주입, 키 매핑 |
| `open-desk-session` | 호스트/뷰어 세션, 승인, 클립보드 |
| `open-desk-relay` | 릴레이 서버 |
| `open-desk` | gpui-kit 데스크톱 앱과 CLI |

자세한 문서는 [docs](docs/README.md)에 있습니다: [사용 가이드](docs/user-guide.md), [아키텍처](docs/architecture.md),
[프로토콜](docs/protocol.md), [보안 모델](docs/security.md), [개발 가이드](docs/development.md). 변경 이력은 [CHANGELOG](CHANGELOG.md)에 있습니다.

### 릴리스

`CHANGELOG.md`의 `Unreleased` 항목을 새 버전 섹션으로 옮기고 `v<버전>` 태그를 푸시하면, Release 워크플로가
macOS dmg, Windows 설치 파일, 릴레이 바이너리를 빌드해 초안 릴리스에 첨부합니다. 저장소 시크릿에 Apple
인증서(`APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`)와 공증 정보
(`APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID`)가 있으면 macOS 앱에 서명·공증합니다.
