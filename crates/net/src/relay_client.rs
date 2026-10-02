//! Reaching hosts through a relay when no direct path exists.
//!
//! A host keeps a registration open with the relay. When a viewer asks for the host's ID, the
//! relay allocates a UDP port per side; each side binds its address to its port with a token,
//! and from then on the relay forwards datagrams between them. Host and viewer then run the
//! usual QUIC + SPAKE2 session through those ports, end to end.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use dari_proto::{
    Allocation, CONTROL_FRAME_LIMIT, CodecError, DeviceId, MessageCodec, RELAY_BIND_MAGIC,
    RELAY_TOKEN_LEN, RelayError, RelayRequest, RelayResponse,
};
use futures_util::{SinkExt, StreamExt};
use subtle::ConstantTimeEq;
use thiserror::Error;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::endpoint::{ConnectError, EndpointError, connect_with};
use crate::identity::{CERTIFICATE_SUBJECT, DeviceIdentity};
use crate::password::AccessPassword;
use crate::session::AuthenticatedConnection;
use crate::tls::{TlsConfigError, relay_client_config};

/// How long to wait for the relay to answer a request.
const RELAY_TIMEOUT: Duration = Duration::from_secs(10);
/// Binding attempts before giving up, and how long each waits for the relay's ack.
const BIND_ATTEMPTS: u32 = 6;
const BIND_ACK_TIMEOUT: Duration = Duration::from_millis(400);
/// How often a bound side repeats its binding datagram. If a NAT moves the device to a new
/// public address, the relay follows within this interval, well inside the QUIC idle timeout.
const REBIND_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug, Error)]
pub enum RelayClientError {
    #[error(transparent)]
    Tls(#[from] TlsConfigError),
    #[error("cannot open a socket: {0}")]
    Io(#[from] std::io::Error),
    #[error("cannot reach the relay: {0}")]
    Connect(#[from] quinn::ConnectError),
    #[error("the relay connection failed: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("the relay sent an invalid message: {0}")]
    Codec(#[from] CodecError),
    #[error("the relay refused: {0}")]
    Refused(RelayError),
    #[error("the relay broke the protocol")]
    Protocol,
    #[error("the relay did not answer in time")]
    TimedOut,
    #[error("the relay did not confirm the port binding")]
    BindingNotConfirmed,
}

type RelayReceiver = FramedRead<quinn::RecvStream, MessageCodec<RelayResponse>>;
type RelaySender = FramedWrite<quinn::SendStream, MessageCodec<RelayRequest>>;

fn client_endpoint(
    relay: SocketAddr,
    identity: Option<&DeviceIdentity>,
) -> Result<quinn::Endpoint, RelayClientError> {
    let unspecified: IpAddr = match relay {
        SocketAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
        SocketAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
    };
    let mut endpoint = quinn::Endpoint::client(SocketAddr::new(unspecified, 0))?;
    endpoint.set_default_client_config(relay_client_config(identity)?);
    Ok(endpoint)
}

/// Opens a control stream to the relay and sends `request`.
async fn request(
    endpoint: &quinn::Endpoint,
    relay: SocketAddr,
    request: RelayRequest,
) -> Result<(quinn::Connection, RelaySender, RelayReceiver), RelayClientError> {
    let connection = endpoint.connect(relay, CERTIFICATE_SUBJECT)?.await?;
    let (send, receive) = connection.open_bi().await?;
    let mut sender = FramedWrite::new(send, MessageCodec::new(CONTROL_FRAME_LIMIT));
    let receiver = FramedRead::new(receive, MessageCodec::new(CONTROL_FRAME_LIMIT));
    sender.send(&request).await?;
    Ok((connection, sender, receiver))
}

async fn next_response(receiver: &mut RelayReceiver) -> Result<RelayResponse, RelayClientError> {
    match receiver.next().await {
        Some(response) => Ok(response?),
        None => Err(RelayClientError::Protocol),
    }
}

/// A host's open registration with a relay. Dropping it takes the host offline there.
pub struct RelayRegistration {
    id: DeviceId,
    relay: SocketAddr,
    receiver: RelayReceiver,
    _sender: RelaySender,
    connection: quinn::Connection,
    _endpoint: quinn::Endpoint,
}

impl std::fmt::Debug for RelayRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayRegistration")
            .field("id", &self.id)
            .field("relay", &self.relay)
            .finish_non_exhaustive()
    }
}

impl RelayRegistration {
    /// Registers this device with the relay, proving its identity with its certificate.
    pub async fn register(
        relay: SocketAddr,
        identity: &DeviceIdentity,
    ) -> Result<Self, RelayClientError> {
        let endpoint = client_endpoint(relay, Some(identity))?;
        let registered = async {
            let (connection, sender, mut receiver) =
                request(&endpoint, relay, RelayRequest::Register).await?;
            match next_response(&mut receiver).await? {
                RelayResponse::Registered { id } => Ok((connection, sender, receiver, id)),
                RelayResponse::Refused(error) => Err(RelayClientError::Refused(error)),
                RelayResponse::Incoming { .. } | RelayResponse::Allocated(_) => {
                    Err(RelayClientError::Protocol)
                }
            }
        };
        let (connection, sender, receiver, id) = tokio::time::timeout(RELAY_TIMEOUT, registered)
            .await
            .map_err(|_| RelayClientError::TimedOut)??;
        Ok(Self {
            id,
            relay,
            receiver,
            _sender: sender,
            connection,
            _endpoint: endpoint,
        })
    }

    /// The ID viewers use to reach this host.
    pub fn id(&self) -> DeviceId {
        self.id
    }

    pub fn relay(&self) -> SocketAddr {
        self.relay
    }

    /// Waits for the next viewer the relay routes to this host.
    pub async fn next_allocation(&mut self) -> Result<RelayIncoming, RelayClientError> {
        match next_response(&mut self.receiver).await? {
            RelayResponse::Incoming { allocation, viewer } => {
                Ok(RelayIncoming { allocation, viewer })
            }
            RelayResponse::Registered { .. }
            | RelayResponse::Allocated(_)
            | RelayResponse::Refused(_) => Err(RelayClientError::Protocol),
        }
    }
}

impl Drop for RelayRegistration {
    fn drop(&mut self) {
        self.connection.close(0u32.into(), b"going offline");
    }
}

/// A viewer the relay is routing to this host.
#[derive(Debug, Clone)]
pub struct RelayIncoming {
    /// The host's side of the allocation.
    pub allocation: Allocation,
    /// The viewer's address as the relay reports it; used to throttle failed attempts per
    /// viewer. Unauthenticated: a lying relay can only spread or merge throttling buckets.
    pub viewer: IpAddr,
}

/// A UDP socket whose address is bound to one side of a relay allocation.
#[derive(Debug)]
pub struct RelayBinding {
    socket: UdpSocket,
    relay_port: SocketAddr,
    datagram: [u8; RELAY_BIND_MAGIC.len() + RELAY_TOKEN_LEN],
}

impl RelayBinding {
    /// Hands the socket to QUIC and keeps the binding fresh until the returned guard drops, so
    /// the relay follows this side if its NAT mapping changes. Must be called within a Tokio
    /// runtime.
    pub(crate) fn activate(self) -> Result<(UdpSocket, BindingKeepalive), std::io::Error> {
        self.activate_every(REBIND_INTERVAL)
    }

    fn activate_every(
        self,
        interval: Duration,
    ) -> Result<(UdpSocket, BindingKeepalive), std::io::Error> {
        let refresher = self.socket.try_clone()?;
        let (relay_port, datagram) = (self.relay_port, self.datagram);
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            interval.tick().await;
            loop {
                interval.tick().await;
                // The socket is non-blocking once QUIC owns it; a dropped refresh is retried.
                let _sent = refresher.send_to(&datagram, relay_port);
            }
        });
        Ok((self.socket, BindingKeepalive(task)))
    }
}

/// Stops refreshing a relay binding when dropped.
#[derive(Debug)]
pub(crate) struct BindingKeepalive(tokio::task::JoinHandle<()>);

impl Drop for BindingKeepalive {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Opens a UDP socket and binds its address to `allocation` on the relay, retrying until the
/// relay confirms. Blocking; run it off the async workers.
pub fn bind_to_allocation(
    relay_ip: IpAddr,
    allocation: &Allocation,
) -> Result<RelayBinding, RelayClientError> {
    let unspecified: IpAddr = match relay_ip {
        IpAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
        IpAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
    };
    let socket = UdpSocket::bind(SocketAddr::new(unspecified, 0))?;
    let relay_port = SocketAddr::new(relay_ip, allocation.port);
    socket.set_read_timeout(Some(BIND_ACK_TIMEOUT))?;
    let expected_ack = allocation.ack_datagram();
    let mut buffer = [0u8; 64];
    for _ in 0..BIND_ATTEMPTS {
        socket.send_to(&allocation.binding_datagram(), relay_port)?;
        let deadline = std::time::Instant::now() + BIND_ACK_TIMEOUT;
        while std::time::Instant::now() < deadline {
            match socket.recv_from(&mut buffer) {
                Ok((length, from)) => {
                    let acked = from == relay_port
                        && length == expected_ack.len()
                        && bool::from(buffer[..length].ct_eq(&expected_ack));
                    if acked {
                        socket.set_read_timeout(None)?;
                        return Ok(RelayBinding {
                            socket,
                            relay_port,
                            datagram: allocation.binding_datagram(),
                        });
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    Err(RelayClientError::BindingNotConfirmed)
}

/// Asks the relay for the host with `id` and authenticates to it through the relay.
pub async fn connect_via_relay(
    relay: SocketAddr,
    id: DeviceId,
    password: &AccessPassword,
    client_name: String,
) -> Result<AuthenticatedConnection, ConnectError> {
    let to_connect_error = |error: RelayClientError| match error {
        RelayClientError::Refused(reason) => ConnectError::Relay(reason),
        other => ConnectError::RelayUnavailable(other.to_string()),
    };
    let endpoint = client_endpoint(relay, None).map_err(to_connect_error)?;
    let allocated = async {
        let (connection, _sender, mut receiver) =
            request(&endpoint, relay, RelayRequest::Connect { id }).await?;
        let response = next_response(&mut receiver).await?;
        connection.close(0u32.into(), b"allocated");
        match response {
            RelayResponse::Allocated(allocation) => Ok(allocation),
            RelayResponse::Refused(error) => Err(RelayClientError::Refused(error)),
            RelayResponse::Registered { .. } | RelayResponse::Incoming { .. } => {
                Err(RelayClientError::Protocol)
            }
        }
    };
    let allocation = tokio::time::timeout(RELAY_TIMEOUT, allocated)
        .await
        .map_err(|_| ConnectError::TimedOut)?
        .map_err(to_connect_error)?;
    drop(endpoint);

    let bound = allocation.clone();
    let binding = tokio::task::spawn_blocking(move || bind_to_allocation(relay.ip(), &bound))
        .await
        .map_err(|error| ConnectError::RelayUnavailable(error.to_string()))?
        .map_err(to_connect_error)?;
    let (socket, keepalive) = binding.activate().map_err(EndpointError::from)?;
    let session_endpoint = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        None,
        socket,
        std::sync::Arc::new(quinn::TokioRuntime),
    )
    .map_err(EndpointError::from)?;
    let mut connection = connect_with(
        session_endpoint,
        SocketAddr::new(relay.ip(), allocation.port),
        password,
        client_name,
    )
    .await?;
    connection.link.relay_keepalive = Some(keepalive);
    Ok(connection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_bound_side_refreshes_its_binding_until_the_session_ends() {
        let relay = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let allocation = Allocation {
            port: relay.local_addr().unwrap().port(),
            token: [5; RELAY_TOKEN_LEN],
        };
        let relay_ip = IpAddr::from(Ipv4Addr::LOCALHOST);
        let bound = allocation.clone();
        let binding = tokio::task::spawn_blocking(move || bind_to_allocation(relay_ip, &bound));
        let mut buffer = [0u8; 64];
        let (length, client) = relay.recv_from(&mut buffer).await.unwrap();
        assert_eq!(&buffer[..length], allocation.binding_datagram());
        relay
            .send_to(&allocation.ack_datagram(), client)
            .await
            .unwrap();
        let binding = binding.await.unwrap().unwrap();

        let (socket, keepalive) = binding.activate_every(Duration::from_millis(50)).unwrap();
        for _ in 0..2 {
            let (length, from) =
                tokio::time::timeout(Duration::from_secs(2), relay.recv_from(&mut buffer))
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(from, client, "refreshes come from the session's own socket");
            assert_eq!(&buffer[..length], allocation.binding_datagram());
        }

        drop(keepalive);
        tokio::time::sleep(Duration::from_millis(100)).await;
        while tokio::time::timeout(Duration::from_millis(10), relay.recv_from(&mut buffer))
            .await
            .is_ok()
        {}
        let quiet =
            tokio::time::timeout(Duration::from_millis(200), relay.recv_from(&mut buffer)).await;
        assert!(quiet.is_err(), "refreshes stop with the session");
        drop(socket);
    }
}
