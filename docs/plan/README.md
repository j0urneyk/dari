# open-desk 구현 계획

open-desk는 AnyDesk·RustDesk·Windows 원격 데스크톱(RDP)과 같은 원격 제어 프로그램이다.
macOS와 Windows 11이 서로 접속(양방향 호스트/뷰어)할 수 있어야 하며, Rust와
[gpui-kit](https://github.com/longbridge/gpui-kit)로 만든다.

이 디렉터리의 phase 문서가 구현의 기준이다. 각 phase는 독립적으로 리뷰·머지 가능한
PR 하나로 끝나며, 다음 phase는 이전 phase가 `main`에 머지된 뒤 시작한다.

## 목표와 비목표

목표:

- 하나의 데스크톱 앱이 호스트(내 화면을 공유)와 뷰어(원격 화면을 제어) 역할을 모두 한다.
- macOS ↔ Windows 11, macOS ↔ macOS, Windows ↔ Windows 조합이 모두 동작한다.
- 연결은 종단 간 암호화되고, 일회용 접속 비밀번호를 모르는 상대는 화면을 볼 수도 입력을 보낼 수도 없다.
- 같은 네트워크(직접 IP 접속, LAN 자동 검색)와 인터넷(릴레이 서버 경유) 모두 지원한다.

비목표(현재 범위 밖, 문서화된 한계로 남김):

- 하드웨어 비디오 인코더(VideoToolbox/Media Foundation), 오디오 전송, 파일 전송
- Windows 보안 데스크톱(UAC 프롬프트, 잠금 화면) 캡처 — 시스템 서비스가 필요하다
- 무인 접속(영구 비밀번호), 다중 동시 세션, 모바일 클라이언트

## 아키텍처

```text
crates/
├── proto     open-desk-proto    메시지 타입, 버전, 길이 제한 프레이밍 코덱 (I/O 없음)
├── net       open-desk-net      QUIC 엔드포인트, 인증서, SPAKE2 인증 핸드셰이크, 시도 제한
├── media     open-desk-media    화면 캡처(xcap), 스케일링, H.264 인코드/디코드(openh264)
├── input     open-desk-input    원격 입력 이벤트 → OS 입력 주입(enigo), 키 매핑
├── session   open-desk-session  호스트/뷰어 세션 오케스트레이션 (tokio), UI와는 채널로 통신
├── relay     open-desk-relay    인터넷 접속용 랑데부 + UDP 릴레이 서버 (phase 6)
└── app       open-desk          gpui-kit 데스크톱 앱 (호스트 + 뷰어 UI)
```

의존 방향은 `app → session → {net, media, input} → proto` 한 방향이다. UI 스레드(gpui)와
네트워크/미디어 스레드(tokio 런타임, 캡처·인코딩 전용 스레드)는 채널로만 대화하므로
UI가 막혀도 세션이 멈추지 않고, 세션 로직은 UI 없이 테스트할 수 있다.

### 연결과 보안 모델

1. 호스트는 최초 실행 시 자체 서명 인증서(rcgen, Ed25519/ECDSA)를 만들어 설정 디렉터리에 저장하고
   QUIC(quinn + rustls, TLS 1.3) 서버를 연다.
2. 호스트 화면에는 접속 주소와 **일회용 접속 비밀번호**가 표시된다. 세션이 끝나거나 사용자가
   요청하면 비밀번호를 새로 만든다.
3. 뷰어는 QUIC으로 접속한다. 인증서는 CA로 검증할 수 없으므로 TLS 서명 검증만 수행하고, 실제
   인증은 **SPAKE2 PAKE**로 한다. 양측은 PAKE 키와 TLS exporter 값을 HMAC으로 묶은 확인값을
   교환한다. 따라서 비밀번호를 모르는 중간자는 세션을 가로챌 수 없고, 도청자는 비밀번호를
   오프라인으로 대입해 볼 수도 없다.
4. 호스트는 실패한 인증 시도를 IP별·전역으로 제한(지수 백오프)하고, 한 번에 하나의 세션만 허용한다.
5. 인증 후 제어 스트림(입력, 클립보드, 설정)과 비디오 스트림(H.264 프레임)이 열린다. 모든
   메시지는 길이 상한이 있는 프레이밍으로 읽어 메모리 고갈 공격을 막는다.

### 핵심 의존성 (바퀴를 다시 만들지 않는다)

| 영역 | 크레이트 |
| --- | --- |
| UI | `gpui-kit` 0.7 (GPUI 스냅샷을 정확히 고정해 재노출) |
| 비동기/네트워크 | `tokio`, `tokio-util`, `quinn`, `rustls`, `rcgen` |
| 인증/암호 | `spake2`, `hmac`, `sha2`, `subtle`, `zeroize`, `rand` |
| 직렬화 | `serde`, `postcard` |
| 화면 캡처 | `xcap` |
| 비디오 코덱 | `openh264` (Cisco OpenH264 소스 빌드), `fast_image_resize` |
| 입력 주입 | `enigo` |
| 클립보드 / LAN 검색 | `arboard`, `mdns-sd` |
| 설정/로깅/에러 | `directories`, `toml`, `tracing`, `tracing-subscriber`, `thiserror`, `anyhow`, `clap` |
| 패키징 | `cargo-packager` |

### 품질 게이트

- formatter: `rustfmt` (`rustfmt.toml`), CI에서 `cargo fmt --check`
- linter: `clippy` (워크스페이스 공통 lint 설정, CI에서 `-D warnings`), `cargo-deny`
  (보안 권고, 라이선스, 중복/출처 검사)
- 테스트: `cargo test --workspace`
- CI: GitHub Actions에서 macOS와 Windows 러너로 위 게이트를 모두 실행

## Phase 목록

| Phase | 문서 | 결과물 |
| --- | --- | --- |
| 0 | [phase-00-bootstrap.md](phase-00-bootstrap.md) | 워크스페이스, 툴체인 고정, fmt/clippy/deny, CI, 앱 골격 |
| 1 | [phase-01-secure-transport.md](phase-01-secure-transport.md) | 프로토콜, QUIC 전송, SPAKE2 인증, 시도 제한 |
| 2 | [phase-02-media-pipeline.md](phase-02-media-pipeline.md) | 화면 캡처, 스케일링, H.264 인코드/디코드 |
| 3 | [phase-03-input-and-host-session.md](phase-03-input-and-host-session.md) | 입력 주입, 호스트/뷰어 세션, 헤드리스 CLI |
| 4 | [phase-04-desktop-ui.md](phase-04-desktop-ui.md) | gpui-kit 홈/접속/뷰어 화면, 권한 안내 |
| 5 | [phase-05-collaboration-features.md](phase-05-collaboration-features.md) | 접속 승인, 클립보드, LAN 검색, 모니터·품질 선택 |
| 6 | [phase-06-relay.md](phase-06-relay.md) | 인터넷 접속용 ID 랑데부 + UDP 릴레이 서버 |
| 7 | [phase-07-packaging.md](phase-07-packaging.md) | macOS .app/.dmg, Windows 설치 파일, 릴리스 문서 |

## 작업 방식

각 phase는 다음 순서로 진행한다.

1. `main`에서 `phase-NN-<slug>` 브랜치를 만든다.
2. phase 문서의 작업 항목을 구현하고, 문서의 "완료 조건"을 로컬에서 확인한다
   (`cargo fmt --check`, `cargo lint`, `cargo test --workspace`, `cargo deny check`).
3. PR을 열고 macOS/Windows CI가 통과하는지 확인한다.
4. `/code-reviewer`와 `/security-review`를 실행하고, 발견 사항을 수정한 뒤 두 리뷰 모두
   발견 사항이 없을 때까지 반복한다.
5. 계획과 달라진 점이 있으면 phase 문서의 "구현 노트"에 기록하고 `CHANGELOG.md`를 갱신한 뒤 머지한다.
