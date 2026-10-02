# 개발 가이드

## 준비

| 항목 | 내용 |
| --- | --- |
| Rust | 1.99.0. `rust-toolchain.toml`이 버전과 `rustfmt`/`clippy` 컴포넌트를 고정하므로 rustup이 알아서 설치합니다 |
| macOS | Xcode Command Line Tools. GUI 테스트에는 Metal이 필요합니다 |
| Windows 11 | Visual Studio Build Tools(MSVC) |
| 도구 | [cargo-deny](https://github.com/EmbarkStudios/cargo-deny), 패키징에는 [cargo-packager](https://github.com/crabnebula-dev/cargo-packager) 0.11.8 |

asdf 등 다른 버전 관리자가 PATH 앞쪽에 오래된 `cargo`를 두면 `rust-toolchain.toml`이 무시될 수 있습니다.
`cargo --version`이 1.99.0인지 먼저 확인하세요.

gpui-kit는 GPUI 스냅샷을 `=` 버전으로 고정해 다시 내보냅니다. gpui-kit 업그레이드는 GPUI 전체를 바꾸므로 다른
변경과 섞지 말고 별도 PR로만 합니다.

워크스페이스는 resolver 3, edition 2024, `rust-version = "1.99"`이며 공용 의존성 버전은 루트
`Cargo.toml`의 `[workspace.dependencies]`에서 관리합니다. 의존성은 dev 프로필에서도 `opt-level = 2`로
빌드합니다. GPUI와 코덱은 최적화 없이 쓸 수 없을 만큼 느리기 때문이고, 우리 크레이트는 디버깅할 수 있게
최적화하지 않습니다. release 프로필은 thin LTO에 `codegen-units = 1`입니다.

## 품질 게이트

PR마다 아래가 모두 통과해야 합니다. CI도 같은 명령을 돌립니다.

```bash
cargo fmt --all --check
```

```bash
cargo lint
```

```bash
cargo test --workspace --locked
```

```bash
cargo test -p open-desk --test gui
```

```bash
cargo deny check
```

- **formatter**: `rustfmt.toml`(stable 옵션만).
- **linter**: `cargo lint`는 `.cargo/config.toml`의 별칭으로
  `clippy --workspace --all-targets --locked -- -D warnings`입니다. 규칙은 루트 `Cargo.toml`의
  `[workspace.lints]`에 있습니다. `unsafe_code = "deny"`, clippy `all`과 `pedantic`, 그리고
  `unwrap_used`, `expect_used`, `print_stdout`, `print_stderr`, `dbg_macro`, `todo`, `unimplemented`를
  경고로 켭니다. `wildcard_imports`(gpui-kit 프렐류드)와 `similar_names`, 문서 관련 소음 lint는 허용합니다.
  테스트에서는 `clippy.toml`이 `unwrap`/`expect`를 허용합니다.
- **cargo-deny**(`deny.toml`): 보안 권고는 전체 의존성 트리에, `unmaintained` 권고는 직접 의존성에만
  적용합니다. GPUI가 전이적으로 가져오는 크레이트는 우리가 바꿀 수 없기 때문입니다. 허용 라이선스 목록,
  yanked 금지, 와일드카드 버전 금지, 알 수 없는 레지스트리·깃 소스 금지를 검사합니다.

`unsafe`는 `crates/input/src/backend.rs`의 OS API 호출 세 곳에만 있고, 그 항목에서만 `allow(unsafe_code)`로
lint를 풉니다. macOS의 `AXIsProcessTrusted`(손쉬운 사용 권한 확인), Windows의 `SetCursorPos`(보조 모니터를
포함한 가상 데스크톱 좌표로 포인터 이동)와 `SetProcessDpiAwarenessContext`(Per-Monitor V2 DPI 인식)입니다.
Windows 전용 코드는 macOS에서도 검사할 수 있습니다.

```bash
cargo clippy -p open-desk-input --target x86_64-pc-windows-msvc -- -D warnings
```

## 테스트

| 종류 | 위치 | 내용 |
| --- | --- | --- |
| 단위 테스트 | 각 크레이트 `src/` | 코덱 상한, 메시지 검증, 비밀번호 생성·파싱, 시도 제한, 핸드셰이크(MITM, 버전 불일치), 축소, 인코드/디코드, 키 매핑, 레터박스 좌표, 릴레이 포워더 |
| 전송 E2E | `crates/net/tests/loopback.rs` | 실제 QUIC 루프백: 성공, 틀린 비밀번호, 비밀번호 소모, Busy, 시도 제한, 인증 전 큰 프레임, 뷰어의 단방향 스트림 금지 |
| 세션 E2E | `crates/session/tests/loopback.rs` | 합성 화면 + 기록 입력으로 호스트·뷰어 전체 경로: 프레임 수신, 입력 주입, 키 해제, 권한 상태 보고, 승인 허용·거부·보기 전용, 디스플레이 전환, 양방향 클립보드, 릴레이 경유 접속 |
| 릴레이 E2E | `crates/relay/tests/relay.rs` | ID로 접속, 틀린 비밀번호는 호스트가 거부, 없는 ID, 재시작 후 같은 ID |
| GUI | `crates/app/tests/gui.rs` | 헤드리스 Metal 렌더러로 실제 창을 그리고 입력을 주입(아래) |
| mDNS | `crates/net/src/discovery.rs` | 로컬 네트워크 멀티캐스트가 필요해 기본으로 건너뜀. `cargo test -p open-desk-net -- --ignored`로 실행 |

실제 화면 캡처와 인코딩 성능은 예제로 잽니다. 주 디스플레이를 몇 초간 캡처·인코딩해 처리량을 출력합니다.
macOS에서 화면 기록 권한이 없어도 바탕화면만 담긴 원본 크기 프레임이 나오므로 비용 측정에는 충분합니다.

```bash
cargo run --release -p open-desk-media --example capture_bench -- 5 1920
```

세션 계층은 `HostPlatform` 트레이트로 화면과 입력을 바꿔 끼울 수 있어서, CI 러너처럼 화면 권한이 없는
환경에서도 QUIC과 H.264를 포함한 실제 경로를 검증합니다. 시간 의존 테스트(할당 만료 등)는 tokio의 일시정지
시계를 씁니다.

### GUI 테스트

GPUI의 macOS 플랫폼은 메인 스레드에서만 만들 수 있어 표준 테스트 하네스를 쓸 수 없습니다. 그래서
`crates/app/tests/gui.rs`는 `harness = false`인 자체 `main`을 가진 테스트이고, 앱 크레이트는 라이브러리를
노출합니다(`open_desk::test_support`). 각 테스트는 `HeadlessAppContext`로 창을 그리고 결과를
`target/gui-snapshots/*.png`로 저장하므로 화면을 눈으로 검토할 수 있습니다.

- 홈 창에 주소와 비밀번호가 표시된다.
- 뷰어 창이 합성 호스트에 실제 QUIC으로 접속해 화면을 그리고, GPUI 입력 이벤트(마우스, 키, Tab, ⌘C)가 호스트
  입력 백엔드까지 도착한다. 같은 테스트가 스트리밍 중 메모리 증가를 잽니다. 한도는 표시한 프레임 수에
  비례합니다(20 MB + 프레임당 0.4 MB, 최소 20프레임). 고정 프레임 수를 요구했더니 느린 CI 러너에서
  48프레임만 그려져 실패했기 때문입니다.
- 접속 폼에 주소(또는 릴레이 ID)와 비밀번호를 입력해 접속하면 승인 카드가 뜨고, "제어 허용"을 누르면 세션이
  성립한다.

## CI

`.github/workflows/ci.yml`은 PR과 `main` 푸시에서 돕니다.

| job | 러너 | 내용 |
| --- | --- | --- |
| `rustfmt` | ubuntu | `cargo fmt --all --check` |
| `cargo-deny` | ubuntu | `cargo deny check` |
| `clippy + test` | macos-latest, windows-latest | `cargo lint`, `cargo test --workspace --locked`, macOS에서는 GUI 테스트 |

액션은 커밋 SHA로 고정하고 토큰은 읽기 전용입니다. 같은 브랜치의 이전 실행은 취소합니다. 디버그 정보
(Windows PDB)가 콜드 빌드 시간을 크게 늘려 `CARGO_PROFILE_DEV_DEBUG=0`으로 끕니다. 캐시는 `main` 푸시에서만
저장하므로 PR 브랜치의 첫 빌드는 대부분 콜드 빌드입니다. GPUI 때문에 Windows 러너에서 clippy 약 15분, 테스트
약 20분이 걸리고, 비공개 저장소에서 macOS 러너 시간은 10배로 과금됩니다. CI를 여러 번 돌리게 되는 긴 PR
체인은 피하는 편이 좋습니다.

## 작업 흐름

1. `main`에서 작업 브랜치를 만듭니다.
2. 구현하고 위 품질 게이트를 로컬에서 통과시킵니다.
3. PR 템플릿(`.github/pull_request_template.md`)의 Summary, Verification, Release notes를 채워 PR을 엽니다.
4. `/code-reviewer`와 `/security-review`를 발견 사항이 없을 때까지 반복합니다. 고친 결함에는 회귀 테스트를
   붙입니다.
5. 사용자에게 보이는 변경은 `CHANGELOG.md`의 `## [Unreleased]`에 적습니다. 설계가 바뀌면 이 디렉터리의 해당
   문서(아키텍처, 프로토콜, 보안 모델 등)를 같은 PR에서 고칩니다.

## 패키징과 릴리스

패키징 설정은 `crates/app/Cargo.toml`의 `[package.metadata.packager]`에 있습니다(번들 ID
`dev.open-desk.app`, macOS 최소 12.0, Windows NSIS 사용자 단위 설치). 아이콘은 `crates/app/assets/icon.svg`가
원본이고 PNG와 `.icns`(`iconutil`)를 만들어 둡니다. `assets/Info.plist`에는 로컬 네트워크 사용 설명
(`NSLocalNetworkUsageDescription`)과 Bonjour 서비스(`_open-desk._udp`)가 있습니다. macOS 15부터 이것이 없으면
LAN 접속과 mDNS가 차단됩니다. macOS의 화면 기록·손쉬운 사용 권한은 번들 ID 단위로 부여되므로 번들 ID를 바꾸면
사용자가 권한을 다시 줘야 합니다.

macOS에서 로컬로 패키징하려면 다음을 실행합니다. DMG는 `hdiutil`로 만듭니다. cargo-packager의 dmg 형식은
Finder AppleScript로 창을 배치하느라 자동화 권한이 없는 환경에서 실패합니다.

```bash
cd crates/app && cargo packager --release --formats app
```

```bash
cd target/release && hdiutil create -volname open-desk -srcfolder open-desk.app -ov -format UDZO open-desk.dmg
```

릴리스 절차:

1. `CHANGELOG.md`의 `## [Unreleased]` 항목을 `## [x.y.z] - YYYY-MM-DD` 섹션으로 옮기고, 빈 `## [Unreleased]`와
   비교·태그 링크를 남깁니다(Keep a Changelog). 루트 `Cargo.toml`의 버전도 맞춥니다.
2. PR로 머지한 뒤 `vx.y.z` 태그를 푸시합니다.
3. `.github/workflows/release.yml`이 macOS dmg, Windows 설치 파일, Linux 릴레이 tar.gz를 빌드하고, CHANGELOG의
   해당 버전 섹션을 본문으로 초안 릴리스를 만듭니다. 본문 추출은 다음 `## [` 제목이나 `[x]: ` 링크 정의 줄에서
   멈춥니다. 같은 태그의 릴리스가 이미 있으면 산출물만 추가합니다(`--clobber`).
4. 초안 본문이 CHANGELOG 섹션과 일치하는지 확인하고 게시합니다.

저장소 시크릿에 Apple 인증서(`APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`)와
공증 정보(`APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID`)가 있을 때만 서명·공증합니다. 없으면 서명되지 않은
앱이 나옵니다. 브랜치를 기준으로 `workflow_dispatch`를 돌리면 게시 단계 없이 패키징만 검증합니다. 이미 게시된
태그에 빠진 산출물을 붙이려면 태그를 기준으로 워크플로를 다시 실행합니다. 이때는 게시 단계가 기존 릴리스에
산출물을 추가합니다.

```bash
gh workflow run release.yml --ref v0.1.0
```
