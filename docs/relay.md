# Relay operations

Devices on different networks (each behind its own router or NAT) can't connect directly. Run `dari-relay` on a
server with a public IP, and hosts get a nine-digit ID that viewers can connect to with the one-time password.

## How it works and why it's safe

1. The host registers with the relay using its device certificate as a TLS client certificate. The relay issues
   a stable nine-digit ID per certificate fingerprint and stores it in `ids.toml`.
2. When a viewer asks to connect to an ID, the relay opens one UDP port for the host and one for the viewer, and
   gives each side its own 16-byte token. Each side registers its address by sending a datagram that carries its
   token.
3. From then on the relay only forwards UDP datagrams between the two addresses. The host and viewer run the same
   end-to-end QUIC (TLS 1.3) + SPAKE2 session as a direct connection.

So even the relay operator can't see the screen, input, clipboard, or password. The relay knows which devices are
online, when connections happen, how much traffic flows, and both sides' public IPs. Password checks, attempt
throttling, and connection approval all happen on the host. The message format is in the
[protocol](protocol.md#relay) document and the trust relationships are in the [security model](security.md#relay).

## Running it

```bash
cargo build --release -p dari-relay
./target/release/dari-relay --listen 0.0.0.0:47822 --data-dir /var/lib/dari-relay
```

| Option | Default | Description |
| --- | --- | --- |
| `--listen` | `[::]:47822` | UDP address for the control endpoint. Forwarding ports open on the same IP |
| `--data-dir` | `relay-data` | Where the relay certificate and the ID table (`ids.toml`) are stored |
| `--max-allocations` | `256` | Most connections forwarded at once |

Open both the `--listen` port and the ephemeral port range (UDP) in the firewall. Forwarding ports are ephemeral
ports chosen by the operating system.

`--data-dir` holds the relay's own certificate (`identity-cert.der`, `identity-key.der`) and the ID table
(`ids.toml`). If `ids.toml` is lost, every host gets a new ID, so back it up. Set the log level with the `RUST_LOG`
environment variable (default `info`). Releases include a Linux x86_64 binary
(`dari-relay_<version>_linux_x86_64.tar.gz`).

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

Forwarding ports are ephemeral, so running with `--network host` is the simplest setup.

## Using it from the app

- Host: on the "This device" page, enter `server` or `server:port` in **Relay server** and press Enter. Once
  registered, **My ID** appears.
- Viewer: set the same relay server, then type the host's nine-digit ID in the address field and connect.
- CLI: `dari host --relay server`, `dari connect 123456789 --relay server`.

## Abuse limits

- Each source IP may make at most 30 registration or connect requests per minute (prevents ID scanning).
- An allocation is freed if both sides haven't bound within 30 seconds, or after 120 seconds without traffic.
- A host can have at most 4 pending connect requests, and `--max-allocations` caps the total.
- With each allocation the relay tells the host the viewer's IP, so the host throttles failed password attempts per
  viewer instead of blocking everyone who connects through the relay.

Clients refresh their port binding every 10 seconds, so a session survives a NAT that assigns a new public address
or port mid-session. Relays and devices must run the same relay protocol (`dari-relay/2`); update the relay together
with the apps.
