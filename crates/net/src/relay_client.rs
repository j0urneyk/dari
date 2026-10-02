//! Reaching hosts through a relay when no direct path exists.
//!
//! A host keeps a registration open with the relay. When a viewer asks for the host's ID, the
//! relay allocates a UDP port per side; each side binds its address to its port with a token,
//! and from then on the relay forwards datagrams between them. Host and viewer then run the
//! usual QUIC + SPAKE2 session through those ports, end to end.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use open_desk_proto::{
    Allocation, CONTROL_FRAME_LIMIT, CodecError, DeviceId, MessageCodec, RelayError, RelayRequest,
    RelayResponse,
};
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
                RelayResponse::Incoming(_) | RelayResponse::Allocated(_) => {
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
    pub async fn next_allocation(&mut self) -> Result<Allocation, RelayClientError> {
        match next_response(&mut self.receiver).await? {
            RelayResponse::Incoming(allocation) => Ok(allocation),
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

/// Opens a UDP socket and binds its address to `allocation` on the relay, retrying until the
/// relay confirms. Blocking; run it off the async workers.
pub fn bind_to_allocation(
    relay_ip: IpAddr,
    allocation: &Allocation,
) -> Result<UdpSocket, RelayClientError> {
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
                        return Ok(socket);
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
            RelayResponse::Registered { .. } | RelayResponse::Incoming(_) => {
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
    let socket = tokio::task::spawn_blocking(move || bind_to_allocation(relay.ip(), &bound))
        .await
        .map_err(|error| ConnectError::RelayUnavailable(error.to_string()))?
        .map_err(to_connect_error)?;
    let session_endpoint = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        None,
        socket,
        std::sync::Arc::new(quinn::TokioRuntime),
    )
    .map_err(EndpointError::from)?;
    connect_with(
        session_endpoint,
        SocketAddr::new(relay.ip(), allocation.port),
        password,
        client_name,
    )
    .await
}
