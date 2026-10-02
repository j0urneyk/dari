# open-desk

macOS와 Windows 11이 서로 접속할 수 있는 원격 데스크톱 프로그램입니다 (AnyDesk, RustDesk,
Windows 원격 데스크톱과 같은 용도). Rust와 [gpui-kit](https://github.com/longbridge/gpui-kit)로
만듭니다.

> 개발 중입니다. 진행 상황과 설계는 [구현 계획](docs/plan/README.md)을 참고하세요.

## 개발 환경

- Rust 1.99.0 (`rust-toolchain.toml`이 버전과 rustfmt/clippy 컴포넌트를 고정합니다)
- macOS: Xcode Command Line Tools
- Windows 11: Visual Studio Build Tools (MSVC, "Desktop development with C++")
- [cargo-deny](https://github.com/EmbarkStudios/cargo-deny): `cargo install --locked cargo-deny`

```bash
cargo run -p open-desk
```

## 품질 게이트

PR을 올리기 전에 CI와 같은 검사를 로컬에서 실행합니다.

```bash
cargo fmt --all --check
cargo lint
cargo test --workspace --locked
cargo deny check
```

`cargo lint`는 `.cargo/config.toml`에 정의된 별칭으로, 모든 타깃에 clippy를 `-D warnings`로
실행합니다. lint 규칙은 루트 `Cargo.toml`의 `[workspace.lints]`에서 관리합니다.
