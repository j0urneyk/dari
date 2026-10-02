//! Blind UDP forwarding between the two sides of an allocation.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use open_desk_proto::{Allocation, RELAY_TOKEN_LEN};
use subtle::ConstantTimeEq;
use tokio::net::UdpSocket;
use tokio::time::Instant;
use tracing::debug;

/// How long both sides have to bind after the allocation is made.
const BIND_TIMEOUT: Duration = Duration::from_secs(30);
/// An allocation is released after this long without traffic.
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// Largest UDP payload; anything a peer can send is forwarded whole, never truncated.
const MAX_DATAGRAM: usize = 65_535;

/// Counts live allocations against the relay's limit.
#[derive(Debug)]
pub(crate) struct AllocationSlot(Arc<AtomicUsize>);

impl AllocationSlot {
    pub(crate) fn try_take(active: &Arc<AtomicUsize>, max: usize) -> Option<Self> {
        active
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < max).then_some(current + 1)
            })
            .ok()
            .map(|_| Self(active.clone()))
    }
}

impl Drop for AllocationSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn random_token() -> std::io::Result<[u8; RELAY_TOKEN_LEN]> {
    let mut token = [0u8; RELAY_TOKEN_LEN];
    getrandom::fill(&mut token).map_err(std::io::Error::other)?;
    Ok(token)
}

/// Opens the host and viewer ports and starts forwarding. Returns what each side is told.
pub(crate) async fn allocate(
    bind_ip: IpAddr,
    slot: AllocationSlot,
) -> std::io::Result<(Allocation, Allocation)> {
    let host_socket = UdpSocket::bind(SocketAddr::new(bind_ip, 0)).await?;
    let viewer_socket = UdpSocket::bind(SocketAddr::new(bind_ip, 0)).await?;
    let host = Allocation {
        port: host_socket.local_addr()?.port(),
        token: random_token()?,
    };
    let viewer = Allocation {
        port: viewer_socket.local_addr()?.port(),
        token: random_token()?,
    };
    let sides = [
        Side::new(host_socket, &host),
        Side::new(viewer_socket, &viewer),
    ];
    tokio::spawn(async move {
        let _slot = slot;
        forward(sides).await;
    });
    Ok((host, viewer))
}

struct Side {
    socket: UdpSocket,
    binding: Vec<u8>,
    ack: Vec<u8>,
    peer: Option<SocketAddr>,
}

impl Side {
    fn new(socket: UdpSocket, allocation: &Allocation) -> Self {
        Self {
            socket,
            binding: allocation.binding_datagram().to_vec(),
            ack: allocation.ack_datagram().to_vec(),
            peer: None,
        }
    }
}

async fn forward(mut sides: [Side; 2]) {
    let created = Instant::now();
    let mut last_traffic = created;
    let mut buffers = [vec![0u8; MAX_DATAGRAM], vec![0u8; MAX_DATAGRAM]];
    loop {
        let both_bound = sides.iter().all(|side| side.peer.is_some());
        let deadline = if both_bound {
            last_traffic + IDLE_TIMEOUT
        } else {
            created + BIND_TIMEOUT
        };
        let [first, second] = &mut buffers;
        let (index, received) = tokio::select! {
            received = sides[0].socket.recv_from(first) => (0, received),
            received = sides[1].socket.recv_from(second) => (1, received),
            () = tokio::time::sleep_until(deadline) => {
                debug!(both_bound, "allocation released");
                return;
            }
        };
        let Ok((length, source)) = received else {
            continue;
        };
        let datagram = &buffers[index][..length];
        let side = &mut sides[index];

        // Binding: the first datagram with the right token claims this side's address.
        let is_binding = length == side.binding.len() && bool::from(datagram.ct_eq(&side.binding));
        if is_binding {
            if side.peer.is_none() || side.peer == Some(source) {
                side.peer = Some(source);
                let _acked = side.socket.send_to(&side.ack, source).await;
            }
            continue;
        }
        // Everything else must come from the bound address and go to a bound peer.
        if side.peer != Some(source) {
            continue;
        }
        let other = &sides[1 - index];
        if let Some(destination) = other.peer {
            let _forwarded = other.socket.send_to(datagram, destination).await;
            last_traffic = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    async fn bind_side(allocation: &Allocation) -> UdpSocket {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let relay = SocketAddr::from((Ipv4Addr::LOCALHOST, allocation.port));
        socket
            .send_to(&allocation.binding_datagram(), relay)
            .await
            .unwrap();
        let mut ack = [0u8; 64];
        let (length, _) = tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut ack))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&ack[..length], allocation.ack_datagram());
        socket
    }

    #[tokio::test]
    async fn bound_sides_exchange_datagrams_and_strangers_are_ignored() {
        let active = Arc::new(AtomicUsize::new(0));
        let slot = AllocationSlot::try_take(&active, 4).unwrap();
        let (host, viewer) = allocate(Ipv4Addr::LOCALHOST.into(), slot).await.unwrap();
        let host_socket = bind_side(&host).await;
        let viewer_socket = bind_side(&viewer).await;

        viewer_socket
            .send_to(b"quic-initial", (Ipv4Addr::LOCALHOST, viewer.port))
            .await
            .unwrap();
        let mut buffer = [0u8; 64];
        let (length, _) = host_socket.recv_from(&mut buffer).await.unwrap();
        assert_eq!(&buffer[..length], b"quic-initial");
        host_socket
            .send_to(b"quic-handshake", (Ipv4Addr::LOCALHOST, host.port))
            .await
            .unwrap();
        let (length, _) = viewer_socket.recv_from(&mut buffer).await.unwrap();
        assert_eq!(&buffer[..length], b"quic-handshake");

        // A third party can neither inject traffic nor claim a side with a guessed token.
        let stranger = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        stranger
            .send_to(b"injected", (Ipv4Addr::LOCALHOST, viewer.port))
            .await
            .unwrap();
        let forged = Allocation {
            port: viewer.port,
            token: [0; RELAY_TOKEN_LEN],
        };
        stranger
            .send_to(
                &forged.binding_datagram(),
                (Ipv4Addr::LOCALHOST, viewer.port),
            )
            .await
            .unwrap();
        let nothing = tokio::time::timeout(
            Duration::from_millis(200),
            host_socket.recv_from(&mut buffer),
        )
        .await;
        assert!(nothing.is_err(), "stranger traffic must not be forwarded");
        let no_ack =
            tokio::time::timeout(Duration::from_millis(200), stranger.recv_from(&mut buffer)).await;
        assert!(no_ack.is_err(), "forged bindings are not acknowledged");
        assert_eq!(active.load(Ordering::Acquire), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn unbound_allocations_expire_and_free_their_slot() {
        let active = Arc::new(AtomicUsize::new(0));
        let slot = AllocationSlot::try_take(&active, 4).unwrap();
        let (_host, _viewer) = allocate(Ipv4Addr::LOCALHOST.into(), slot).await.unwrap();
        assert_eq!(active.load(Ordering::Acquire), 1);
        tokio::time::sleep(BIND_TIMEOUT + Duration::from_secs(1)).await;
        // Let the forwarder task observe its deadline.
        tokio::task::yield_now().await;
        assert_eq!(active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn allocation_limit_is_enforced() {
        let active = Arc::new(AtomicUsize::new(0));
        let first = AllocationSlot::try_take(&active, 1).unwrap();
        assert!(AllocationSlot::try_take(&active, 1).is_none());
        drop(first);
        assert!(AllocationSlot::try_take(&active, 1).is_some());
    }
}
