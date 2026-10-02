# Phase 0 — 저장소와 품질 게이트

## 목표

이후 모든 phase가 같은 규칙으로 검사되도록 Cargo 워크스페이스, 툴체인, formatter/linter,
CI를 먼저 갖춘다. gpui-kit 창 하나를 띄우는 앱 골격으로 GUI 빌드가 macOS와 Windows에서
모두 되는지 이 단계에서 확인한다.

## 작업 항목

- Cargo 워크스페이스 (`resolver = "3"`, edition 2024, `rust-version = "1.99"`)
  - `crates/app` (`open-desk` 바이너리): gpui-kit로 빈 홈 창을 연다.
  - 공용 의존성은 `[workspace.dependencies]`에서 버전을 한 곳에서 관리한다.
- `rust-toolchain.toml`: `1.99.0`, `rustfmt`/`clippy` 컴포넌트 고정
- formatter: `rustfmt.toml` (edition 2024, import 그룹 정리 등 stable 옵션만)
- linter
  - `[workspace.lints]`: `unsafe_code = "deny"`, `clippy::all`/`pedantic` 경고 +
    소음이 큰 lint만 명시적으로 허용, `unwrap_used`/`expect_used`/`dbg_macro`/`todo` 경고
  - `clippy.toml`: 테스트에서는 `unwrap`/`expect` 허용
  - `deny.toml`: RustSec 권고, 허용 라이선스 목록, 알 수 없는 레지스트리/깃 소스 금지
- `.cargo/config.toml` 별칭: `cargo lint` (= clippy all-targets, `-D warnings`)
- `.editorconfig`, `.gitignore`
- GitHub Actions `ci.yml`
  - 트리거: PR, `main` push
  - `fmt` (ubuntu, 빠름), `deny` (ubuntu, cargo-deny-action)
  - `build-test` 매트릭스: `macos-latest`, `windows-latest` — `cargo lint`, `cargo test --workspace`
  - `Swatinem/rust-cache`로 캐시, 같은 브랜치의 이전 실행은 취소(concurrency)
- `.github/pull_request_template.md`, `CHANGELOG.md`(Keep a Changelog), `README.md`

## 완료 조건

- `cargo fmt --check`, `cargo lint`, `cargo test --workspace`, `cargo deny check`가 로컬에서 통과
- `cargo run -p open-desk`로 macOS에서 gpui-kit 창이 열린다
- PR의 CI가 macOS·Windows 모두 통과

## 위험과 대응

- GPUI는 큰 의존성 트리를 가져와 첫 빌드와 CI가 느리다 → rust-cache, 변경 없는 job 최소화
- gpui-kit가 GPUI 스냅샷을 `=` 버전으로 고정하므로 gpui-kit 업그레이드는 별도 PR로만 한다

## 구현 노트

- gpui-kit는 `use gpui_kit::*` 프렐류드 사용을 전제로 설계되어 있어 `clippy::wildcard_imports`는
  워크스페이스 전체에서 허용했다.
- cargo-deny의 `unmaintained` 검사는 `workspace`(직접 의존성) 범위로 한정했다. GPUI가 전이적으로
  가져오는 `instant`, `paste`, `rustls-pemfile`, `rustybuzz`, `ttf-parser`는 우리가 교체할 수 없다.
  취약점(vulnerability) 권고는 전체 의존성 트리에 그대로 적용된다.
- 의존성은 dev 프로필에서도 `opt-level = 2`로 빌드한다. GPUI와 코덱이 최적화 없이 너무 느리기 때문이다.
