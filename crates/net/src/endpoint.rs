//! Host listener and viewer connector.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use dari_proto::{ClientHello, Os, PROTOCOL_VERSION, RejectReason, ServerHello};
use thiserror::Error;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::handshake::{
    EXPORTER_LABEL, EXPORTER_LEN, HandshakeChannel, HandshakeError, HostAdmission,
    authenticate_to_host, authenticate_viewer,
};
use crate::identity::{CERTIFICATE_SUBJECT, DeviceIdentity, Fingerprint};
use crate::limiter::AttemptLimiter;
use crate::password::AccessPassword;
use crate::relay_client::RelayBinding;
use crate::session::{AuthenticatedConnection, PeerInfo, SessionLink, SessionSlot};
use crate::tls::{TlsConfigError, client_config, server_config};

/// Upper bound for TLS plus the password handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Handshakes the host runs at once; extra connection attempts are refused.
const MAX_PENDING_HANDSHAKES: usize = 8;
/// How long a rejected viewer gets to read the rejection before the host closes.
const REJECTION_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Error)]
pub enum EndpointError {
    #[error(transparent)]
    Tls(#[from] TlsConfigError),
    #[error("failed to open a UDP socket: {0}")]
    Bind(#[from] std::io::Error),
}

#[derive(Debug, Error)]
pub enum ConnectError {
    #[error(transparent)]
    Endpoint(#[from] EndpointError),
    #[error("could not start the connection: {0}")]
    Start(#[from] quinn::ConnectError),
    #[error("the connection failed: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error(transparent)]
    Handshake(#[from] HandshakeError),
    #[error("the remote device did not answer in time")]
    TimedOut,
    #[error("the relay refused: {0}")]
    Relay(dari_proto::RelayError),
    #[error("the relay is unavailable: {0}")]
    RelayUnavailable(String),
}

/// Host-side settings that do not change while listening.
#[derive(Debug, Clone)]
pub struct HostSettings {
    pub bind_address: SocketAddr,
    pub host_name: String,
}

/// The host's current one-time password; `None` while not accepting viewers.
///
/// Every change bumps `generation`, so a handshake can tell whether the password it consumed is
/// still the latest state before putting it back.
#[derive(Default)]
struct PasswordSlot {
    current: Option<AccessPassword>,
    generation: u64,
}

impl PasswordSlot {
    fn set(&mut self, password: Option<AccessPassword>) {
        self.current = password;
        self.generation += 1;
    }

    /// Takes the password if it is still `used`; returns the generation after consuming it.
    fn consume(&mut self, used: &AccessPassword) -> Option<u64> {
        if self.current.as_ref() != Some(used) {
            return None;
        }
        self.set(None);
        Some(self.generation)
    }

    /// Puts back a consumed password unless the slot changed after `consumed_generation`.
    fn restore(&mut self, password: AccessPassword, consumed_generation: u64) {
        if self.generation == consumed_generation {
            self.set(Some(password));
        }
    }
}

struct HostShared {
    server_hello: ServerHello,
    password: Mutex<PasswordSlot>,
    limiter: Mutex<AttemptLimiter>,
    session_active: Arc<AtomicBool>,
}

impl HostShared {
    fn password(&self) -> MutexGuard<'_, PasswordSlot> {
        self.password.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn limiter(&self) -> MutexGuard<'_, AttemptLimiter> {
        self.limiter.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Listens for viewers and yields connections that passed authentication.
///
/// A successful authentication consumes the one-time password: the host stops accepting new
/// viewers until [`HostEndpoint::set_password`] provides a fresh one.
pub struct HostEndpoint {
    endpoint: quinn::Endpoint,
    shared: Arc<HostShared>,
    server_config: quinn::ServerConfig,
    session_sender: mpsc::Sender<AuthenticatedConnection>,
    sessions: mpsc::Receiver<AuthenticatedConnection>,
    accept_task: JoinHandle<()>,
}

/// How long a relayed socket waits for the viewer's QUIC handshake to begin.
const RELAYED_ACCEPT_TIMEOUT: Duration = Duration::from_secs(20);

/// Accepts viewers that arrive through a relay allocation, through the same authentication,
/// throttling, and single-session rules as direct connections. Cheap to clone.
#[derive(Clone)]
pub struct RelayedAcceptor {
    shared: Arc<HostShared>,
    server_config: quinn::ServerConfig,
    sessions: mpsc::Sender<AuthenticatedConnection>,
}

impl std::fmt::Debug for RelayedAcceptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayedAcceptor").finish_non_exhaustive()
    }
}

impl RelayedAcceptor {
    /// Accepts one viewer on `binding`, a socket already bound to a relay allocation.
    /// `viewer` is the viewer's address as the relay reported it; failed attempts are throttled
    /// against it rather than against the relay. Must be called within a Tokio runtime.
    pub fn accept_on(&self, binding: RelayBinding, viewer: IpAddr) -> Result<(), EndpointError> {
        let (socket, keepalive) = binding.activate()?;
        let endpoint = quinn::Endpoint::new(
            quinn::EndpointConfig::default(),
            Some(self.server_config.clone()),
            socket,
            Arc::new(quinn::TokioRuntime),
        )?;
        let shared = self.shared.clone();
        let sessions = self.sessions.clone();
        tokio::spawn(async move {
            let Ok(Some(incoming)) =
                tokio::time::timeout(RELAYED_ACCEPT_TIMEOUT, endpoint.accept()).await
            else {
                debug!("no viewer arrived on the relay allocation");
                endpoint.close(0u32.into(), b"unused");
                return;
            };
            match handle_incoming(incoming, &shared, viewer).await {
                Ok(mut connection) => {
                    info!(peer = %connection.peer().name, "viewer authenticated through the relay");
                    // The endpoint drives this connection's socket; keep it, and the binding
                    // refresh, with the session.
                    connection.link.endpoint = Some(endpoint);
                    connection.link.relay_keepalive = Some(keepalive);
                    let _delivered = sessions.send(connection).await;
                }
                Err(error) => {
                    debug!(%error, "relayed connection failed");
                    endpoint.close(0u32.into(), b"handshake failed");
                }
            }
        });
        Ok(())
    }
}

impl std::fmt::Debug for HostEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostEndpoint")
            .field("local_address", &self.local_address().ok())
            .finish()
    }
}

impl HostEndpoint {
    /// Starts listening. Must be called within a Tokio runtime.
    pub fn bind(settings: HostSettings, identity: &DeviceIdentity) -> Result<Self, EndpointError> {
        let server_config = server_config(identity)?;
        let endpoint = quinn::Endpoint::server(server_config.clone(), settings.bind_address)?;
        let shared = Arc::new(HostShared {
            server_hello: ServerHello {
                version: PROTOCOL_VERSION,
                host_name: settings.host_name,
                host_os: Os::current(),
            },
            password: Mutex::new(PasswordSlot::default()),
            limiter: Mutex::new(AttemptLimiter::default()),
            session_active: Arc::new(AtomicBool::new(false)),
        });
        let (session_sender, sessions) = mpsc::channel(1);
        let accept_task = tokio::spawn(accept_loop(
            endpoint.clone(),
            shared.clone(),
            session_sender.clone(),
        ));
        info!(address = ?endpoint.local_addr().ok(), "host endpoint listening");
        Ok(Self {
            endpoint,
            shared,
            server_config,
            session_sender,
            sessions,
            accept_task,
        })
    }

    /// A handle for accepting viewers that arrive through a relay.
    pub fn relayed_acceptor(&self) -> RelayedAcceptor {
        RelayedAcceptor {
            shared: self.shared.clone(),
            server_config: self.server_config.clone(),
            sessions: self.session_sender.clone(),
        }
    }

    pub fn local_address(&self) -> std::io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    /// Replaces the access password; `None` stops accepting new viewers.
    pub fn set_password(&self, password: Option<AccessPassword>) {
        self.shared.password().set(password);
    }

    /// Whether a password is set, i.e. whether a new viewer could authenticate now.
    pub fn is_accepting(&self) -> bool {
        self.shared.password().current.is_some()
    }

    /// Waits for the next authenticated viewer. Returns `None` once the endpoint is closed.
    pub async fn accept(&mut self) -> Option<AuthenticatedConnection> {
        self.sessions.recv().await
    }
}

impl Drop for HostEndpoint {
    fn drop(&mut self) {
        self.accept_task.abort();
        self.endpoint.close(0u32.into(), b"host stopped");
    }
}

async fn accept_loop(
    endpoint: quinn::Endpoint,
    shared: Arc<HostShared>,
    sessions: mpsc::Sender<AuthenticatedConnection>,
) {
    let pending = Arc::new(Semaphore::new(MAX_PENDING_HANDSHAKES));
    while let Some(incoming) = endpoint.accept().await {
        let address = incoming.remote_address();
        if !shared.limiter().allows(address.ip(), Instant::now()) {
            debug!(%address, "refusing throttled source");
            incoming.refuse();
            continue;
        }
        let Ok(permit) = pending.clone().try_acquire_owned() else {
            debug!(%address, "refusing connection: too many pending handshakes");
            incoming.refuse();
            continue;
        };
        let shared = shared.clone();
        let sessions = sessions.clone();
        tokio::spawn(async move {
            let _permit = permit;
            match handle_incoming(incoming, &shared, address.ip()).await {
                Ok(connection) => {
                    info!(peer = %connection.peer().name, %address, "viewer authenticated");
                    // If nobody is accepting, dropping the connection closes it.
                    let _delivered = sessions.send(connection).await;
                }
                Err(error) => debug!(%address, %error, "incoming connection failed"),
            }
        });
    }
}

/// Authenticates one incoming viewer. `origin` is the address failed attempts count against:
/// the connection's source for direct viewers, the viewer's reported address for relayed ones.
async fn handle_incoming(
    incoming: quinn::Incoming,
    shared: &HostShared,
    origin: IpAddr,
) -> Result<AuthenticatedConnection, ConnectError> {
    let deadline = tokio::time::Instant::now() + HANDSHAKE_TIMEOUT;
    let address = incoming.remote_address();
    let connection = tokio::time::timeout_at(deadline, incoming)
        .await
        .map_err(|_| ConnectError::TimedOut)??;
    let (send, receive) = tokio::time::timeout_at(deadline, connection.accept_bi())
        .await
        .map_err(|_| ConnectError::TimedOut)??;
    let mut channel = HandshakeChannel::new(receive, send);
    let exporter = exporter(&connection)?;

    let password = shared.password().current.clone();
    let precheck = if !shared.limiter().allows(origin, Instant::now()) {
        Err(RejectReason::TooManyAttempts)
    } else if shared
        .session_active
        .load(std::sync::atomic::Ordering::Acquire)
    {
        Err(RejectReason::Busy)
    } else {
        password.as_ref().ok_or(RejectReason::NotAccepting)
    };
    let admission = HostAdmission {
        precheck,
        server_hello: shared.server_hello.clone(),
        exporter,
    };

    let mut password_attempted = false;
    let mut consumed_generation = None;
    let finalize = || {
        let mut slot_state = shared.password();
        let used = password.as_ref().ok_or(RejectReason::NotAccepting)?;
        // Claim the session before consuming so a busy host keeps its password.
        let slot = SessionSlot::try_claim(&shared.session_active).ok_or(RejectReason::Busy)?;
        // The password may have been replaced or consumed while this handshake ran.
        let generation = slot_state
            .consume(used)
            .ok_or(RejectReason::AuthenticationFailed)?;
        consumed_generation = Some(generation);
        Ok(slot)
    };
    let result = tokio::time::timeout_at(
        deadline,
        authenticate_viewer(&mut channel, admission, &mut password_attempted, finalize),
    )
    .await;

    let outcome = match result {
        Ok(Ok((hello, slot))) => Ok((hello, slot)),
        Ok(Err(error)) => Err(ConnectError::Handshake(error)),
        Err(_) => Err(ConnectError::TimedOut),
    };
    match outcome {
        Ok((hello, slot)) => {
            shared.limiter().record_success(origin);
            let link = SessionLink {
                connection,
                peer: peer_from_hello(hello, address),
                slot: Some(slot),
                endpoint: None,
                relay_keepalive: None,
            };
            Ok(AuthenticatedConnection::new(link, channel))
        }
        Err(error) => {
            if password_attempted {
                warn!(%address, %origin, %error, "failed authentication attempt");
                shared.limiter().record_failure(origin, Instant::now());
            }
            if let (Some(generation), Some(password)) = (consumed_generation, password) {
                // The viewer proved the password but the session never got established; keep
                // the host reachable unless the password was changed meanwhile.
                shared.password().restore(password, generation);
            }
            // Closing immediately would discard a rejection still in flight. Finish the stream
            // and give the viewer a moment to read it and hang up first.
            let _finished = channel.writer.get_mut().finish();
            let _closed = tokio::time::timeout(REJECTION_GRACE, connection.closed()).await;
            connection.close(1u32.into(), b"handshake failed");
            Err(error)
        }
    }
}

fn peer_from_hello(hello: ClientHello, address: SocketAddr) -> PeerInfo {
    PeerInfo {
        name: hello.client_name,
        os: hello.client_os,
        address,
        fingerprint: None,
    }
}

fn exporter(connection: &quinn::Connection) -> Result<[u8; EXPORTER_LEN], HandshakeError> {
    let mut exporter = [0u8; EXPORTER_LEN];
    connection
        .export_keying_material(&mut exporter, EXPORTER_LABEL, b"")
        .map_err(|_| HandshakeError::Protocol("TLS exporter unavailable"))?;
    Ok(exporter)
}

/// Connects to a host and authenticates with its one-time access password.
pub async fn connect(
    address: SocketAddr,
    password: &AccessPassword,
    client_name: String,
) -> Result<AuthenticatedConnection, ConnectError> {
    let unspecified: IpAddr = match address {
        SocketAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
        SocketAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
    };
    let endpoint =
        quinn::Endpoint::client(SocketAddr::new(unspecified, 0)).map_err(EndpointError::from)?;
    connect_with(endpoint, address, password, client_name).await
}

/// Authenticates to the host at `address` through `endpoint` (direct or relay-bound socket).
pub(crate) async fn connect_with(
    mut endpoint: quinn::Endpoint,
    address: SocketAddr,
    password: &AccessPassword,
    client_name: String,
) -> Result<AuthenticatedConnection, ConnectError> {
    endpoint.set_default_client_config(client_config().map_err(EndpointError::from)?);

    let hello = ClientHello {
        version: PROTOCOL_VERSION,
        client_name,
        client_os: Os::current(),
    };
    let handshake = async {
        let connection = endpoint.connect(address, CERTIFICATE_SUBJECT)?.await?;
        let (send, receive) = connection.open_bi().await?;
        let mut channel = HandshakeChannel::new(receive, send);
        let exporter = exporter(&connection)?;
        let server_hello = authenticate_to_host(&mut channel, password, &exporter, hello).await?;
        Ok::<_, ConnectError>((connection, channel, server_hello))
    };
    let (connection, channel, server_hello) = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake)
        .await
        .map_err(|_| ConnectError::TimedOut)??;

    let fingerprint = connection
        .peer_identity()
        .and_then(|identity| {
            identity
                .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
                .ok()
        })
        .and_then(|chain| {
            chain
                .first()
                .map(|certificate| Fingerprint::of_certificate(certificate))
        });
    let link = SessionLink {
        connection,
        peer: PeerInfo {
            name: server_hello.host_name,
            os: server_hello.host_os,
            address,
            fingerprint,
        },
        slot: None,
        endpoint: Some(endpoint),
        relay_keepalive: None,
    };
    Ok(AuthenticatedConnection::new(link, channel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_slot_consumes_only_the_current_password() {
        let password = AccessPassword::generate().unwrap();
        let mut slot = PasswordSlot::default();
        slot.set(Some(password.clone()));
        assert!(slot.consume(&AccessPassword::generate().unwrap()).is_none());
        assert!(slot.consume(&password).is_some());
        assert!(slot.current.is_none());
        assert!(
            slot.consume(&password).is_none(),
            "a password is consumed once"
        );
    }

    #[test]
    fn password_slot_restores_only_if_unchanged() {
        let password = AccessPassword::generate().unwrap();
        let mut slot = PasswordSlot::default();
        slot.set(Some(password.clone()));
        let generation = slot.consume(&password).unwrap();
        slot.restore(password.clone(), generation);
        assert_eq!(slot.current.as_ref(), Some(&password));

        // The host stopped accepting while the failed handshake was finishing.
        let generation = slot.consume(&password).unwrap();
        slot.set(None);
        slot.restore(password.clone(), generation);
        assert!(slot.current.is_none());

        // The host rotated to a new password meanwhile.
        let fresh = AccessPassword::generate().unwrap();
        slot.set(Some(password.clone()));
        let generation = slot.consume(&password).unwrap();
        slot.set(Some(fresh.clone()));
        slot.restore(password, generation);
        assert_eq!(slot.current.as_ref(), Some(&fresh));
    }

    fn host() -> HostEndpoint {
        let host = HostEndpoint::bind(
            HostSettings {
                bind_address: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
                host_name: "host".into(),
            },
            &DeviceIdentity::generate().unwrap(),
        )
        .unwrap();
        host.set_password(Some(AccessPassword::generate().unwrap()));
        host
    }

    async fn raw_connection(host: &HostEndpoint) -> (quinn::Endpoint, quinn::Connection) {
        let mut endpoint = quinn::Endpoint::client((Ipv4Addr::LOCALHOST, 0).into()).unwrap();
        endpoint.set_default_client_config(client_config().unwrap());
        let connection = endpoint
            .connect(host.local_address().unwrap(), CERTIFICATE_SUBJECT)
            .unwrap()
            .await
            .unwrap();
        (endpoint, connection)
    }

    #[tokio::test]
    async fn oversized_frame_before_authentication_closes_the_connection() {
        let mut host = host();
        let (_endpoint, connection) = raw_connection(&host).await;
        let (mut send, _receive) = connection.open_bi().await.unwrap();
        // Announce a 1 GiB handshake frame.
        send.write_all(&(1u32 << 30).to_be_bytes()).await.unwrap();
        let closed = tokio::time::timeout(Duration::from_secs(5), connection.closed()).await;
        assert!(
            closed.is_ok(),
            "host must drop a connection sending oversized frames"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), host.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn viewers_cannot_open_unidirectional_streams() {
        let host = host();
        let (_endpoint, connection) = raw_connection(&host).await;
        // The host grants viewers no unidirectional stream credit, so this never completes.
        let opened = tokio::time::timeout(Duration::from_millis(300), connection.open_uni()).await;
        assert!(opened.is_err());
    }
}
