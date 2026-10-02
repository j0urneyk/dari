# 개발 기록

open-desk v0.1.0은 [구현 계획](plan/README.md)의 phase 0부터 7까지를 phase마다 PR 하나로 쌓아 만들었습니다.
이 문서는 각 PR이 무엇을 했는지, 리뷰가 무엇을 찾아 어떻게 고쳤는지, 어떤 검증이 되었고 무엇이 남았는지를
기록합니다. 계획 대비 설계 변경의 세부 내용은 각 phase 문서의 "구현 노트"에 있습니다.

## 진행 방식

phase마다 `main`에서 브랜치를 만들어 구현하고, 로컬 품질 게이트(fmt, `cargo lint`, 테스트, cargo-deny)를
통과시킨 뒤 PR을 열었습니다. 머지 전에는 `/code-reviewer`와 `/security-review`를 **두 리뷰 모두 발견 사항이 없을
때까지** 반복했습니다. 리뷰 기준에 못 미치는 지적도 수정 비용이 작고 실제로 의미가 있으면 고쳤고, 고친 결함에는
회귀 테스트를 붙였습니다.

## PR 목록

| PR | 내용 | 머지 |
| --- | --- | --- |
| [#1](https://github.com/j0urneyk/open-desk/pull/1) | Phase 0: 워크스페이스, 품질 게이트, CI | 2026-10-02 |
| [#2](https://github.com/j0urneyk/open-desk/pull/2) | Phase 1: 와이어 프로토콜과 비밀번호 인증 QUIC 전송 | 2026-10-02 |
| [#3](https://github.com/j0urneyk/open-desk/pull/3) | Phase 2: 화면 캡처와 H.264 파이프라인 | 2026-10-02 |
| [#4](https://github.com/j0urneyk/open-desk/pull/4) | Phase 3: 원격 입력, 호스트/뷰어 세션, 헤드리스 CLI | 2026-10-02 |
| [#5](https://github.com/j0urneyk/open-desk/pull/5) | Phase 4: gpui-kit 데스크톱 앱 | 2026-10-02 |
| [#6](https://github.com/j0urneyk/open-desk/pull/6) | Phase 5: 접속 승인, 클립보드, LAN 검색, 디스플레이, 화질 | 2026-10-02 |
| [#7](https://github.com/j0urneyk/open-desk/pull/7) | Phase 6: 릴레이를 통한 ID 접속 | 2026-10-02 |
| [#8](https://github.com/j0urneyk/open-desk/pull/8) | Phase 7: 패키징, 릴리스 워크플로, README | 2026-10-02 |
| [#9](https://github.com/j0urneyk/open-desk/pull/9) | v0.1.0 릴리스(CHANGELOG 확정) | 2026-10-02 |
| [#10](https://github.com/j0urneyk/open-desk/pull/10) | 릴리스 노트 추출이 CHANGELOG 링크 정의에서 멈추게 수정 | 2026-10-02 |

머지 시각은 UTC 기준이며, 한국 시간으로는 2026-10-03 새벽입니다.

## Phase별 요약

### Phase 0: 저장소와 품질 게이트 (#1)

Cargo 워크스페이스와 gpui-kit 앱 골격, rustfmt, clippy(workspace lints, `-D warnings`), cargo-deny, macOS·Windows
CI를 갖췄습니다. gpui-kit가 프렐류드(`use gpui_kit::*`) 사용을 전제로 하므로 `wildcard_imports`를 허용했고,
GPUI가 전이적으로 가져오는 크레이트의 `unmaintained` 권고는 직접 의존성 범위로 좁혔습니다(보안 권고는 전체
트리에 그대로 적용).

### Phase 1: 프로토콜과 보안 전송 (#2)

`open-desk-proto`(버전, 검증, 길이 제한 프레이밍)와 `open-desk-net`(기기 인증서, 일회용 비밀번호, TLS exporter에
묶인 SPAKE2 핸드셰이크, 시도 제한, 단일 세션 호스트 엔드포인트)을 만들었습니다.

- **code-reviewer P2**: 호스트가 자기 확인값을 보내기 전에 비밀번호를 소모했기 때문에, 그 전송이 실패하거나
  시간 초과되면 호스트가 비밀번호 없이 모든 뷰어를 조용히 거절하는 상태에 빠졌습니다. 비밀번호 변경을 세대
  번호로 추적해 세션이 시작되지 않았고 그사이 아무것도 바뀌지 않았을 때만 되돌리도록 고쳤습니다.
- **security-review**: 발견 없음(인증 우회, MITM, 반사, 오프라인 대입, 비밀번호 경쟁, 인증 전 역직렬화 검토).
- 거절 메시지를 보낸 직후 연결을 닫으면 QUIC이 아직 보내지 않은 데이터를 버려 거절 이유가 사라졌습니다.
  스트림을 `finish()`하고 잠시 기다린 뒤 닫도록 했습니다.
- 첫 CI 실행은 Windows 콜드 빌드가 너무 오래 걸려 취소되었습니다. 디버그 정보(PDB)가 빌드 시간의 큰 부분이라
  `CARGO_PROFILE_DEV_DEBUG=0`을 더한 뒤 다시 돌려 통과했습니다.

### Phase 2: 미디어 파이프라인 (#3)

xcap 캡처(macOS 화면 기록 사전 검사 포함), 짝수 크기 축소, 화면 콘텐츠용 OpenH264 인코더, BGRA 디코더, 인코딩
전에 프레임을 버리는 페이싱 캡처 스레드를 만들었습니다.

- **code-reviewer P2 두 건**: 일시적 캡처 실패(보안 데스크톱, 디스플레이 모드 변경) 한 번에 스트림이 영영
  끝났습니다. 이제 30초까지 재시도합니다. 마지막 오류를 `blocking_send`로 보내 스레드를 join하는 소유자와
  교착할 수 있었습니다. 이제 미리 예약한 자리로 보냅니다.
- 실측(2560×1440 모니터, release): 1080p 출력 34 fps, 1440p 원본 28 fps. 계획의 목표(1080p 20 fps 이상)를
  넘었습니다.

### Phase 3: 입력과 세션 (#4)

입력 프로토콜, `open-desk-input`(눌린 키 추적, enigo, Windows `SetCursorPos`와 DPI 인식, ⌘↔Ctrl 매핑),
`open-desk-session`(호스트 서비스, 캡처·비디오 펌프·입력 스레드, 최신 프레임만 넘기는 뷰어), CLI `host`/`connect`를
만들었습니다.

- **code-reviewer P3**: 네트워크가 멈춘 동안 포인터 이동이 뷰어 입력 큐를 채워 뒤이은 키 떼기가 버려지고
  호스트에 키가 눌린 채로 남았습니다. 포인터 이동은 여유 공간이 있을 때만 큐를 쓰게 했습니다.
- **security-review 강화**: 세션이 끝난 뒤에도 큐에 남은 원격 입력이 주입되었습니다. 큐가 사라지는 즉시 주입을
  멈추고 눌린 키만 뗍니다.
- 뷰어가 비디오 스트림의 정상 종료를 세션 종료로 처리하던 문제와, 양쪽이 `Disconnect`를 보내자마자 닫아
  메시지를 잃던 문제를 고쳤습니다.

### Phase 4: 데스크톱 UI (#5)

gpui-kit 홈 창(주소, 비밀번호, 권한 안내, 접속 폼)과 뷰어 창(캔버스 그리기, 입력 변환)을 만들었습니다. 앱을
라이브러리와 얇은 바이너리로 나눠 헤드리스 Metal GUI 테스트를 가능하게 했습니다.

- 리뷰 준비 중 발견: gpui-kit `Root`가 Tab, Shift-Tab, ⌘C/Ctrl+C를 먼저 가로채 원격으로 가지 않았습니다. 원격
  화면 키 컨텍스트에서 `NoAction`으로 풀었고 GUI 테스트가 이를 검증합니다.
- **code-reviewer P3**: 비밀번호 형식 오류가 UI 언어로 표시되지 않았습니다.
- **security-review 강화**: 상대 이름의 양방향 덮어쓰기·폭 없는 문자를 거부합니다.
- 프레임마다 `drop_image`로 GPU 아틀라스를 비우지 않으면 6초 스트리밍에 메모리가 약 283 MB 늘어나는 것을
  확인하고, 메모리 증가 상한을 GUI 테스트로 고정했습니다. 이 테스트는 처음에 최소 60프레임을 요구했는데 macOS
  CI 러너가 48프레임만 그려 실패했습니다. 상한을 실제로 그린 프레임 수에 비례하게(최소 20프레임,
  20 MB + 프레임당 0.4 MB) 바꿔 느린 러너에서도 의미 있는 검사가 되게 했습니다.
- Ctrl+Alt+Del 전송은 Windows SAS를 일반 프로세스가 보낼 수 없어 계획에서 뺐습니다.

### Phase 5: 협업 기능 (#6)

접속 승인(제어 허용 / 보기만 / 거부, 30초), 텍스트 클립보드 동기화, mDNS LAN 검색, 디스플레이 전환과 화질
프리셋을 더했습니다.

- **code-reviewer P3 두 건**: 길거나 한글인 기기 이름이 mDNS 인스턴스 이름의 63바이트 DNS 레이블 한도를 넘어
  광고가 조용히 실패했습니다. 뷰어는 입력 주입이 동작할 때만 클립보드를 공유했는데, 호스트는 제어가 허용되면
  항상 공유했습니다.
- **security-review 강화**: 호스트가 나중에 보기 전용을 알리면 뷰어도 클립보드 공유를 멈춥니다.
- 비밀번호가 소모된 세션 동안 호스트 화면에 이미 쓸모없는 비밀번호가 보이던 문제와, 승인 전부터 "연결됨"으로
  표시되던 문제를 UI에서 고쳤습니다.

### Phase 6: 릴레이 (#7)

`open-desk-relay`(인증서에 묶인 9자리 ID, 토큰으로 묶인 UDP 전달), 릴레이 등록과 ID 접속, 앱의 릴레이 설정과
"내 ID"를 만들었습니다. 릴레이 경유 세션도 종단 간 QUIC + SPAKE2입니다.

- **code-reviewer**: 발견 없음. **security-review**: 발견 없음.
- 기능 결함 수정: 포워더가 1500바이트 버퍼를 써서 큰 데이터그램을 잘랐습니다. 64 KiB 버퍼로 바꿨습니다.
  첫 수정 커밋이 버퍼 분할을 잘못해 후속 커밋으로 바로잡았습니다.
- 이 PR부터 GitHub Actions가 계정 결제·지출 한도 문제로 job을 시작하지 않았습니다(아래 참조).

### Phase 7: 패키징 (#8), 릴리스 (#9, #10)

cargo-packager 설정, SVG 원본 아이콘, macOS 로컬 네트워크 선언, 태그로 동작하는 릴리스 워크플로, README를
만들었습니다. 리뷰는 둘 다 발견 없음이었습니다.

- cargo-packager의 dmg 형식은 Finder AppleScript가 자동화 권한을 요구해 실패했고, `.icns` 생성도 실패해
  `hdiutil`과 `iconutil`로 대신했습니다.
- #9에서 CHANGELOG를 `## [0.1.0] - 2026-10-03`으로 확정했습니다. 릴리스 노트를 확인하다가, 마지막 버전
  섹션에서는 `awk` 추출이 파일 끝의 링크 정의(`[0.1.0]: …`)까지 포함한다는 것을 발견해 #10에서 고쳤습니다.
  태그 푸시로 시작된 릴리스 워크플로도 Actions 중단으로 돌지 못해, macOS dmg와 Linux 릴레이(Docker에서 빌드)는
  로컬에서 만들어 고친 규칙으로 추출한 노트와 함께 게시했습니다. 게시된 v0.1.0 노트는 CHANGELOG의 `[0.1.0]`
  섹션과 일치합니다.

## CI 중단과 대응

PR #7을 진행하던 중 GitHub Actions가 "recent account payments have failed or your spending limit needs to be
increased"라는 이유로 모든 job 시작을 거부했습니다. GPUI 콜드 빌드가 길고(Windows에서 clippy 약 15분, 테스트
약 20분) 비공개 저장소의 macOS 러너는 10배로 과금되는데, PR 브랜치마다 캐시 없이 빌드한 것이 겹쳤습니다.

결제 설정은 저장소 소유자가 결정할 일이라 바꾸지 않았습니다. #7부터 #10은 로컬 검증(fmt, `cargo lint`, 워크스페이스
테스트, Metal GUI 테스트, cargo-deny, `open-desk-input`의 Windows 대상 clippy)으로 머지하고, 그 사실을 각 PR에
코멘트로 남겼습니다. 같은 이유로 v0.1.0의 Windows 설치 파일은 아직 빌드되지 않았습니다.

## 검증 현황 (v0.1.0)

| 항목 | 상태 |
| --- | --- |
| 단위·E2E 테스트(전송, 세션, 릴레이), cargo-deny | 로컬 통과. #1–#6은 macOS·Windows CI도 통과 |
| GUI 테스트(홈, 뷰어 스트리밍과 입력, 메모리, 승인 클릭, 릴레이 ID 접속) | macOS 로컬 Metal에서 통과 |
| CLI `host` ↔ `connect` 실제 실행(macOS 루프백) | 인증, 권한 상태 전달, 정상 종료, 비밀번호 교체 확인 |
| Linux 릴레이(Docker) + macOS CLI 호스트·뷰어 | ID 접속 확인 |
| macOS `.app`과 dmg | 로컬 빌드, 실행, 호스팅 시작 확인(11 MB) |
| 실제 화면 캡처 성능 | `capture_bench`로 측정. 단 개발 환경 프로세스에 화면 기록 권한이 없어 바탕화면만 캡처됨 |
| 권한을 받은 상태의 실제 화면 공유·입력 주입 | **미확인**(개발 환경에 권한 없음) |
| macOS ↔ Windows 11 실제 기기 간 접속 | **미확인**(Windows 기기 없음). 설치 후 수동 확인 필요 |
| Windows NSIS 설치 파일 | **미빌드**(Actions 중단). 결제 문제 해결 후 `gh workflow run release.yml --ref v0.1.0` |
| mDNS 왕복 테스트 | 로컬에서 `--ignored`로 통과(CI에서는 건너뜀) |

## 교훈

- **QUIC close는 보내지 않은 데이터를 버립니다.** 거절 이유나 `Disconnect`처럼 마지막에 보내는 메시지는 스트림을
  끝낸 뒤 상대가 닫기를 잠깐 기다려야 도착합니다. 이 문제는 phase 1과 3에서 각각 다른 모습으로 나타났습니다.
- **소모하는 자원은 되돌리는 경로까지 설계해야 합니다.** 비밀번호를 "먼저 소모하고 실패하면 끝"으로 두면
  호스트가 조용히 멈춥니다. 세대 번호로 경쟁 없이 되돌렸습니다.
- **백프레셔는 인코딩 전에.** H.264에서 프레임을 인코딩한 뒤 버리면 참조 체인이 깨집니다.
- **UI 프레임워크의 기본 키 바인딩을 확인해야 합니다.** 원격 화면처럼 모든 키를 넘겨야 하는 영역에서는 상위
  컨텍스트의 바인딩이 키를 가로챕니다.
- **고정 수치 대신 관측량에 비례하는 테스트 한도.** 러너 속도에 따라 그리는 프레임 수가 달라지므로 메모리 상한을
  프레임 수에 비례시켰습니다.
- **비공개 저장소의 GPUI CI는 비쌉니다.** 캐시 전략과 PR 체인 길이를 미리 생각해야 하고, CI가 멈췄을 때 무엇을
  로컬에서 검증했는지 PR에 남겨야 합니다.

## 다음 작업 후보

- Windows 설치 파일 빌드와 v0.1.0 릴리스 첨부(결제 문제 해결 후)
- 실제 macOS ↔ Windows 11 기기 간 양방향 접속, 클립보드 왕복, 다중 모니터·고DPI 수동 검증
- 계획의 비목표: 하드웨어 인코더(VideoToolbox, Media Foundation), 오디오, 파일 전송, 무인 접속, 보안 데스크톱
  지원(Windows 서비스), Intel Mac·ARM Windows 빌드
- 릴레이 경유 세션의 NAT 재바인딩 대응, 릴레이 경유 시도 제한을 출발지별로 나누는 방법
