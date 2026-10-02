# Phase 2 — 화면 캡처와 비디오 파이프라인

## 목표

호스트 화면을 주기적으로 캡처해 H.264로 인코딩하고, 뷰어 쪽에서 디코딩해 BGRA 프레임
(gpui `RenderImage`가 요구하는 형식)으로 돌려주는 파이프라인을 만든다.

## 설계 (`open-desk-media`)

- `DisplayInfo { id, name, x, y, width, height, scale_factor, is_primary }` — xcap `Monitor`에서 얻는다.
- `ScreenCapturer` 트레이트 + `XcapCapturer` 구현. 테스트용 `SyntheticCapturer`(움직이는 패턴).
- 스케일링: 최대 해상도(기본 긴 변 1920px) 초과 시 `fast_image_resize`로 축소.
  H.264 요구에 맞게 가로·세로를 짝수로 맞춘다.
- 인코더: openh264 `Encoder` (실시간 화면 공유 프로파일, 목표 비트레이트·최대 FPS 설정,
  키프레임 강제 요청 지원). RGBA → YUV420 변환은 openh264 내장 변환을 쓴다.
- 디코더: openh264 `Decoder` → RGBA → BGRA 변환 후 `DecodedFrame { width, height, bgra }`.
- 프레임 페이싱: 캡처 루프는 전용 OS 스레드에서 돌며, 목표 FPS(기본 30)에 맞춰 잠들고,
  송신 쪽이 바쁘면(채널 가득) 인코딩하지 않고 프레임을 버린다 — P-프레임 참조가 깨지지
  않도록 "버리기"는 반드시 인코딩 전에 일어난다.
- 해상도가 바뀌면 인코더를 다시 만들고 키프레임부터 보낸다.
- macOS 화면 기록 권한: 캡처 실패를 `CaptureError::PermissionDenied`로 구분해 UI가 안내할 수 있게 한다.

## 작업 항목

- `crates/media` 생성, 위 구성요소 구현
- 테스트: 합성 프레임 인코드→디코드 round-trip(크기 보존, 픽셀 오차 허용 범위),
  홀수 해상도 처리, 해상도 변경 후 키프레임, 프레임 드롭 정책
- 벤치용 예제(`examples/capture_bench.rs`): 실제 모니터 캡처+인코딩 FPS 출력 (수동 검증)

## 완료 조건

- CI에서 합성 프레임 테스트 통과 (CI 러너에는 실제 화면 권한이 없으므로 실제 캡처는 수동 검증)
- 로컬 macOS에서 `capture_bench`가 1080p 기준 20fps 이상
