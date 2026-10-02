# Phase 7 — 패키징과 배포 문서

## 목표

사용자가 macOS와 Windows 11에 설치해 쓸 수 있는 산출물을 만든다.

## 설계

- `cargo-packager`로 macOS `.app`/`.dmg`, Windows NSIS 설치 파일 생성
  - macOS: `Info.plist`에 `NSScreenCaptureUsageDescription` 등 사용 목적 문자열, 앱 아이콘,
    번들 ID. 화면 기록/손쉬운 사용 권한은 번들 ID 단위로 부여되므로 안정적인 번들 ID가 중요하다.
  - Windows: 매니페스트로 Per-Monitor V2 DPI 인식, 방화벽 안내
- 태그(`v*`) 푸시 시 GitHub Actions에서 두 OS 산출물을 빌드해 릴리스 초안에 첨부
- 코드 서명/공증은 인증서가 필요하므로 워크플로에 선택적 단계(시크릿이 있을 때만)로 둔다
- README: 설치, 권한 설정, 직접 접속/LAN/릴레이 사용법, 보안 모델, 알려진 한계
- `CHANGELOG.md` 정리 후 `v0.1.0` 릴리스

## 완료 조건

- 릴리스 워크플로가 두 OS 산출물을 만든다
- 설치한 앱으로 macOS↔Windows 접속을 수동 확인

## 구현 노트

- `cargo-packager`로 macOS `.app`과 Windows NSIS 설치 파일을 만든다. macOS DMG는 cargo-packager의
  `create-dmg`가 Finder AppleScript로 창 배치를 하느라 자동화 권한이 없는 환경에서 실패하므로 `hdiutil`로 만든다.
- 아이콘은 `crates/app/assets/icon.svg`를 원본으로 PNG와 `.icns`(`iconutil`)를 만든다.
- macOS `Info.plist`에 로컬 네트워크 사용 설명과 Bonjour 서비스(`_open-desk._udp`)를 넣었다. macOS 15부터
  이것이 없으면 LAN 접속과 mDNS 검색이 차단된다. 화면 기록/손쉬운 사용은 별도의 사용 설명 키가 없다.
- Windows Per-Monitor V2 DPI 인식은 매니페스트 대신 시작 시 `SetProcessDpiAwarenessContext`로 켠다(phase 3).
- 릴리스 워크플로: `v*` 태그에서 macOS dmg, Windows 설치 파일, Linux 릴레이를 빌드하고 CHANGELOG의 해당
  버전 섹션을 본문으로 초안 릴리스를 만든다. `workflow_dispatch`로 게시 없이 패키징만 검증할 수 있다.
  Apple 서명·공증은 시크릿이 있을 때만 수행한다.
