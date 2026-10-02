# 릴레이 서버 운영

서로 다른 네트워크(각자 공유기/NAT 뒤)에 있는 기기는 직접 접속할 수 없습니다. 이때 공인 IP가 있는
서버에서 `dari-relay`를 실행하면, 호스트는 9자리 ID를 받고 뷰어는 ID와 일회용 비밀번호로
접속할 수 있습니다.

## 동작 방식과 보안

1. 호스트는 기기 인증서(TLS 클라이언트 인증서)로 릴레이에 등록하고, 릴레이는 인증서 지문마다
   고정된 9자리 ID를 발급해 `ids.toml`에 저장합니다.
2. 뷰어가 ID로 접속을 요청하면 릴레이는 호스트용·뷰어용 UDP 포트를 하나씩 열고, 각 쪽에 서로 다른
   16바이트 토큰을 줍니다. 각 쪽은 토큰을 담은 데이터그램으로 자기 주소를 등록합니다.
3. 이후 릴레이는 두 주소 사이의 UDP 데이터그램을 그대로 전달할 뿐이고, 호스트와 뷰어는 직접 접속과
   똑같은 QUIC(TLS 1.3) + SPAKE2 세션을 종단 간으로 맺습니다.

따라서 릴레이 운영자도 화면, 입력, 클립보드, 비밀번호를 볼 수 없습니다. 릴레이가 아는 것은 어떤 기기가
온라인인지와 접속 시각, 트래픽 양, 양쪽의 공인 IP 정도입니다. 비밀번호 검사·시도 제한·접속 승인은 모두
호스트가 합니다. 메시지 형식은 [프로토콜](protocol.md#릴레이), 신뢰 관계는 [보안 모델](security.md#릴레이)에
있습니다.

## 실행

```bash
cargo build --release -p dari-relay
./target/release/dari-relay --listen 0.0.0.0:47822 --data-dir /var/lib/dari-relay
```

| 옵션 | 기본값 | 설명 |
| --- | --- | --- |
| `--listen` | `[::]:47822` | 제어용 UDP 주소. 전달용 포트도 같은 IP에서 엽니다 |
| `--data-dir` | `relay-data` | 릴레이 인증서와 ID 표(`ids.toml`) 저장 위치 |
| `--max-allocations` | `256` | 동시에 전달하는 연결 수 상한 |

방화벽에서 `--listen` 포트와 임시 포트 범위(UDP)를 모두 열어야 합니다. 전달용 포트는 운영체제가
고르는 임시 포트입니다.

`--data-dir`에는 릴레이 자신의 인증서(`identity-cert.der`, `identity-key.der`)와 ID 표(`ids.toml`)가 저장됩니다.
`ids.toml`을 잃으면 모든 호스트가 새 ID를 받으므로 백업해 두세요. 로그 수준은 `RUST_LOG` 환경 변수로
조절합니다(기본 `info`). 릴리스에는 Linux x86_64 바이너리(`dari-relay_<버전>_linux_x86_64.tar.gz`)가
포함됩니다.

### Docker

```dockerfile
FROM rust:1.99 AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p dari-relay

FROM debian:stable-slim
COPY --from=build /src/target/release/dari-relay /usr/local/bin/
VOLUME /data
ENTRYPOINT ["dari-relay", "--listen", "0.0.0.0:47822", "--data-dir", "/data"]
```

```bash
docker run -d --network host -v dari-relay:/data dari-relay
```

전달 포트가 임시 포트이므로 `--network host`로 실행하는 것이 가장 간단합니다.

## 앱에서 사용

- 호스트: "이 기기" 카드의 **릴레이 서버**에 `서버주소` 또는 `서버주소:포트`를 입력하고 Enter.
  등록되면 **내 ID**가 표시됩니다.
- 뷰어: 같은 릴레이 서버를 설정한 뒤 주소 칸에 상대의 9자리 ID를 입력하고 접속합니다.
- CLI: `dari host --relay 서버주소`, `dari connect 123456789 --relay 서버주소`.

## 남용 방지

- 출발지 IP마다 1분에 30건까지만 등록·접속 요청을 받습니다(ID 대입 방지).
- 할당은 30초 안에 양쪽이 등록하지 않거나 120초 동안 트래픽이 없으면 해제됩니다.
- 한 호스트에 동시에 대기할 수 있는 접속 요청은 4개, 전체 할당 수는 `--max-allocations`로 제한됩니다.
