//! The host service: listening, password management, and serving one viewer at a time.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use dari_media::StreamSettings;
use dari_net::{
    AccessPassword, DeviceIdentity, EndpointError, HostEndpoint, HostSettings, PasswordError,
    PeerInfo, RelayRegistration, RelayedAcceptor, bind_to_allocation,
};
use dari_proto::{DEFAULT_RELAY_PORT, DeviceId, HostStatus};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::SessionEndReason;
use crate::host_session::{SessionOptions, serve_viewer};
use crate::platform::HostPlatform;

#[derive(Debug, Clone)]
pub struct HostConfig {
    pub bind_address: SocketAddr,
    pub host_name: String,
    pub stream: StreamSettings,
    /// Ask the host user before a viewer that knows the password may see the screen.
    pub require_approval: bool,
    /// Share clipboard text with viewers allowed to control this device.
    pub clipboard: bool,
    /// Relay to register with (`host` or `host:port`) so viewers can reach this device by ID.
    pub relay: Option<String>,
}

/// This host's standing with its relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayStatus {
    Connecting,
    /// Reachable under this ID.
    Registered(DeviceId),
    /// Not reachable through the relay right now; retrying.
    Unavailable(String),
}

/// The host user's answer to a viewer waiting for approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// The viewer may see the screen and control the keyboard and mouse.
    AllowControl,
    /// The viewer may only see the screen.
    ViewOnly,
    Deny,
}

/// A pending approval. Answer it once with [`ApprovalRequest::respond`]; an unanswered request
/// is declined after 30 seconds or when the session ends.
#[derive(Clone)]
pub struct ApprovalRequest(Arc<Mutex<Option<oneshot::Sender<ApprovalDecision>>>>);

impl ApprovalRequest {
    pub(crate) fn new(respond: oneshot::Sender<ApprovalDecision>) -> Self {
        Self(Arc::new(Mutex::new(Some(respond))))
    }

    /// Answers the request; later answers are ignored.
    pub fn respond(&self, decision: ApprovalDecision) {
        let pending = self.0.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(respond) = pending {
            let _sent = respond.send(decision);
        }
    }
}

impl std::fmt::Debug for ApprovalRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalRequest").finish_non_exhaustive()
    }
}

#[derive(Debug, Error)]
pub enum HostError {
    #[error(transparent)]
    Endpoint(#[from] EndpointError),
    #[error(transparent)]
    Password(#[from] PasswordError),
}

/// What the host service reports to the UI.
#[derive(Debug, Clone)]
pub enum HostEvent {
    /// The access password viewers need now; `None` while not accepting viewers.
    PasswordChanged(Option<AccessPassword>),
    SessionStarted(PeerInfo),
    /// A viewer proved the password and waits for the host user to allow it.
    ApprovalRequested {
        peer: PeerInfo,
        request: ApprovalRequest,
    },
    /// Screen or input availability for the running session changed.
    SessionStatus(HostStatus),
    SessionEnded {
        peer: PeerInfo,
        reason: SessionEndReason,
    },
    Relay(RelayStatus),
}

enum HostCommand {
    RegeneratePassword,
    SetAccepting(bool),
    SetPolicy {
        require_approval: bool,
        clipboard: bool,
    },
    EndSession,
}

/// Controls a running host service. Dropping it stops the service and ends any session.
#[derive(Debug)]
pub struct HostHandle {
    commands: mpsc::UnboundedSender<HostCommand>,
    local_address: SocketAddr,
    task: JoinHandle<()>,
}

impl HostHandle {
    pub fn local_address(&self) -> SocketAddr {
        self.local_address
    }

    /// Replaces the access password with a fresh one.
    pub fn regenerate_password(&self) {
        let _sent = self.commands.send(HostCommand::RegeneratePassword);
    }

    /// Starts or stops accepting new viewers. A running session is not affected.
    pub fn set_accepting(&self, accepting: bool) {
        let _sent = self.commands.send(HostCommand::SetAccepting(accepting));
    }

    /// Changes approval and clipboard policy for the next session.
    pub fn set_policy(&self, require_approval: bool, clipboard: bool) {
        let _sent = self.commands.send(HostCommand::SetPolicy {
            require_approval,
            clipboard,
        });
    }

    /// Ends the running session, if any.
    pub fn end_session(&self) {
        let _sent = self.commands.send(HostCommand::EndSession);
    }
}

impl Drop for HostHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl std::fmt::Debug for HostCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            HostCommand::RegeneratePassword => "RegeneratePassword",
            HostCommand::SetAccepting(_) => "SetAccepting",
            HostCommand::SetPolicy { .. } => "SetPolicy",
            HostCommand::EndSession => "EndSession",
        })
    }
}

/// Starts the host service on the current Tokio runtime.
pub fn start_host(
    config: HostConfig,
    identity: Arc<DeviceIdentity>,
    platform: Arc<dyn HostPlatform>,
) -> Result<(HostHandle, mpsc::UnboundedReceiver<HostEvent>), HostError> {
    let endpoint = HostEndpoint::bind(
        HostSettings {
            bind_address: config.bind_address,
            host_name: config.host_name,
        },
        &identity,
    )?;
    let local_address = endpoint.local_address().map_err(EndpointError::from)?;
    let (commands, command_receiver) = mpsc::unbounded_channel();
    let (events, event_receiver) = mpsc::unbounded_channel();
    let relay = config.relay.map(|relay| {
        AbortOnDrop(tokio::spawn(stay_registered(
            relay,
            identity,
            endpoint.relayed_acceptor(),
            events.clone(),
        )))
    });
    let service = HostService {
        endpoint,
        _relay: relay,
        events,
        platform,
        options: SessionOptions {
            stream: config.stream,
            require_approval: config.require_approval,
            clipboard: config.clipboard,
        },
        accepting: true,
    };
    service.issue_password()?;
    let task = tokio::spawn(service.run(command_receiver));
    Ok((
        HostHandle {
            commands,
            local_address,
            task,
        },
        event_receiver,
    ))
}

struct RunningSession {
    peer: PeerInfo,
    end: Option<oneshot::Sender<()>>,
    task: JoinHandle<SessionEndReason>,
}

struct HostService {
    endpoint: HostEndpoint,
    /// Keeps this host registered with its relay while the service runs.
    _relay: Option<AbortOnDrop>,
    events: mpsc::UnboundedSender<HostEvent>,
    platform: Arc<dyn HostPlatform>,
    options: SessionOptions,
    accepting: bool,
}

impl HostService {
    /// Sets a fresh password (or none when not accepting) and tells the UI.
    fn issue_password(&self) -> Result<(), PasswordError> {
        let password = self.accepting.then(AccessPassword::generate).transpose()?;
        self.endpoint.set_password(password.clone());
        let _sent = self.events.send(HostEvent::PasswordChanged(password));
        Ok(())
    }

    fn reissue_password(&self) {
        if let Err(error) = self.issue_password() {
            warn!(%error, "could not create a new password; not accepting viewers");
            self.endpoint.set_password(None);
            let _sent = self.events.send(HostEvent::PasswordChanged(None));
        }
    }

    async fn run(mut self, mut commands: mpsc::UnboundedReceiver<HostCommand>) {
        let mut session: Option<RunningSession> = None;
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    None => break,
                    Some(HostCommand::RegeneratePassword) => self.reissue_password(),
                    Some(HostCommand::SetAccepting(accepting)) => {
                        self.accepting = accepting;
                        // Mid-session the password stays consumed; a new one is issued when
                        // the session ends.
                        if session.is_none() || !accepting {
                            self.reissue_password();
                        }
                    }
                    Some(HostCommand::SetPolicy { require_approval, clipboard }) => {
                        self.options.require_approval = require_approval;
                        self.options.clipboard = clipboard;
                    }
                    Some(HostCommand::EndSession) => {
                        if let Some(end) = session.as_mut().and_then(|running| running.end.take()) {
                            let _sent = end.send(());
                        }
                    }
                },
                connection = self.endpoint.accept(), if session.is_none() => {
                    let Some(connection) = connection else { break };
                    let peer = connection.peer().clone();
                    info!(peer = %peer.name, address = %peer.address, "session started");
                    let _sent = self.events.send(HostEvent::SessionStarted(peer.clone()));
                    let (end, end_receiver) = oneshot::channel();
                    let task = tokio::spawn(serve_viewer(
                        connection,
                        self.platform.clone(),
                        self.options,
                        end_receiver,
                        self.events.clone(),
                    ));
                    session = Some(RunningSession { peer, end: Some(end), task });
                }
                reason = async {
                    match session.as_mut() {
                        Some(running) => (&mut running.task).await,
                        None => std::future::pending().await,
                    }
                } => {
                    let reason = reason.unwrap_or_else(|error| {
                        SessionEndReason::ConnectionLost(format!("session task failed: {error}"))
                    });
                    if let Some(running) = session.take() {
                        info!(peer = %running.peer.name, %reason, "session ended");
                        let _sent = self.events.send(HostEvent::SessionEnded { peer: running.peer, reason });
                    }
                    // The previous password was consumed by the session; issue a new one.
                    self.reissue_password();
                }
            }
        }
        if let Some(running) = session {
            running.task.abort();
        }
    }
}

/// Aborts a task when dropped, tying it to its owner's lifetime.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

const RELAY_RETRY_MIN: Duration = Duration::from_secs(2);
const RELAY_RETRY_MAX: Duration = Duration::from_secs(60);

/// Keeps the host registered with its relay, re-registering with backoff after failures, and
/// hands every viewer the relay routes here to the normal authentication path.
async fn stay_registered(
    relay: String,
    identity: Arc<DeviceIdentity>,
    acceptor: RelayedAcceptor,
    events: mpsc::UnboundedSender<HostEvent>,
) {
    let mut retry = RELAY_RETRY_MIN;
    loop {
        let _sent = events.send(HostEvent::Relay(RelayStatus::Connecting));
        let failure = match resolve_relay(&relay).await {
            Err(error) => error,
            Ok(address) => match RelayRegistration::register(address, &identity).await {
                Err(error) => error.to_string(),
                Ok(mut registration) => {
                    retry = RELAY_RETRY_MIN;
                    let _sent =
                        events.send(HostEvent::Relay(RelayStatus::Registered(registration.id())));
                    serve_allocations(&mut registration, address, &acceptor).await
                }
            },
        };
        warn!(%failure, "relay unavailable");
        let _sent = events.send(HostEvent::Relay(RelayStatus::Unavailable(failure)));
        tokio::time::sleep(retry).await;
        retry = (retry * 2).min(RELAY_RETRY_MAX);
    }
}

/// Binds each allocation the relay offers and accepts the viewer on it, until the
/// registration fails; returns why.
async fn serve_allocations(
    registration: &mut RelayRegistration,
    relay: SocketAddr,
    acceptor: &RelayedAcceptor,
) -> String {
    loop {
        let incoming = match registration.next_allocation().await {
            Ok(incoming) => incoming,
            Err(error) => return error.to_string(),
        };
        let relay_ip = relay.ip();
        let allocation = incoming.allocation;
        match tokio::task::spawn_blocking(move || bind_to_allocation(relay_ip, &allocation)).await {
            Ok(Ok(binding)) => {
                if let Err(error) = acceptor.accept_on(binding, incoming.viewer) {
                    warn!(%error, "cannot accept a relayed viewer");
                }
            }
            Ok(Err(error)) => warn!(%error, "cannot bind a relay allocation"),
            Err(error) => warn!(%error, "relay binding task failed"),
        }
    }
}

/// Resolves `host` or `host:port`, defaulting to the standard relay port.
pub(crate) async fn resolve_relay(relay: &str) -> Result<SocketAddr, String> {
    let relay = relay.trim();
    if let Ok(address) = relay.parse::<SocketAddr>() {
        return Ok(address);
    }
    let bare = relay.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        return Ok(SocketAddr::new(ip, DEFAULT_RELAY_PORT));
    }
    let with_port = if relay.contains(':') {
        relay.to_owned()
    } else {
        format!("{relay}:{DEFAULT_RELAY_PORT}")
    };
    tokio::net::lookup_host(&with_port)
        .await
        .map_err(|error| format!("cannot resolve {relay}: {error}"))?
        .next()
        .ok_or_else(|| format!("{relay} has no address"))
}
