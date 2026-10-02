//! The relay's control endpoint: host registrations and viewer connection requests.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use open_desk_net::{DeviceIdentity, Fingerprint};
use open_desk_proto::{
    Allocation, CONTROL_FRAME_LIMIT, DeviceId, MessageCodec, RelayError, RelayRequest,
    RelayResponse,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::codec::{FramedRead, FramedWrite};
use tracing::{debug, info, warn};

use crate::forward::{AllocationSlot, allocate};
use crate::ids::IdStore;

/// How long a device has to finish TLS and send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Requests (registrations and connects) one address may make per window.
const REQUESTS_PER_WINDOW: usize = 30;
const REQUEST_WINDOW: Duration = Duration::from_secs(60);
/// Bound on addresses tracked by the request limiter.
const MAX_TRACKED_ADDRESSES: usize = 10_000;
/// Viewers that may be waiting for one host at once.
const PENDING_PER_HOST: usize = 4;

#[derive(Debug, Clone)]
pub struct RelayConfig {
    pub listen: SocketAddr,
    /// Where the relay keeps its own certificate and the device ID table.
    pub data_directory: PathBuf,
    /// Most port pairs forwarded at once.
    pub max_allocations: usize,
}

struct RegisteredHost {
    generation: u64,
    incoming: mpsc::Sender<Allocation>,
}

struct State {
    hosts: Mutex<HashMap<DeviceId, RegisteredHost>>,
    ids: Mutex<IdStore>,
    requests: Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
    allocations: Arc<AtomicUsize>,
    max_allocations: usize,
    bind_ip: IpAddr,
    generations: AtomicU64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl State {
    /// Sliding-window limit per source address, so IDs cannot be scanned quickly.
    fn allow_request(&self, address: IpAddr) -> bool {
        let now = Instant::now();
        let mut requests = lock(&self.requests);
        if requests.len() >= MAX_TRACKED_ADDRESSES && !requests.contains_key(&address) {
            requests.retain(|_, times| {
                times
                    .back()
                    .is_some_and(|last| now.duration_since(*last) < REQUEST_WINDOW)
            });
            if requests.len() >= MAX_TRACKED_ADDRESSES {
                return false;
            }
        }
        let times = requests.entry(address).or_default();
        while times
            .front()
            .is_some_and(|first| now.duration_since(*first) >= REQUEST_WINDOW)
        {
            times.pop_front();
        }
        if times.len() >= REQUESTS_PER_WINDOW {
            return false;
        }
        times.push_back(now);
        true
    }
}

/// A running relay. Dropping it stops the relay.
pub struct RelayServer {
    endpoint: quinn::Endpoint,
    task: JoinHandle<()>,
}

impl std::fmt::Debug for RelayServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayServer")
            .field("address", &self.endpoint.local_addr().ok())
            .finish_non_exhaustive()
    }
}

impl RelayServer {
    /// Starts the relay on the current Tokio runtime.
    pub fn start(config: &RelayConfig) -> anyhow::Result<Self> {
        let identity = DeviceIdentity::load_or_generate(&config.data_directory)?;
        let server_config = open_desk_net::relay_tls::server_config(&identity)?;
        let endpoint = quinn::Endpoint::server(server_config, config.listen)?;
        let state = Arc::new(State {
            hosts: Mutex::new(HashMap::new()),
            ids: Mutex::new(IdStore::load(&config.data_directory.join("ids.toml"))?),
            requests: Mutex::new(HashMap::new()),
            allocations: Arc::new(AtomicUsize::new(0)),
            max_allocations: config.max_allocations,
            bind_ip: config.listen.ip(),
            generations: AtomicU64::new(0),
        });
        info!(address = ?endpoint.local_addr().ok(), "relay listening");
        let task = tokio::spawn(accept_loop(endpoint.clone(), state));
        Ok(Self { endpoint, task })
    }

    pub fn local_address(&self) -> std::io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }
}

impl Drop for RelayServer {
    fn drop(&mut self) {
        self.task.abort();
        self.endpoint.close(0u32.into(), b"relay stopped");
    }
}

async fn accept_loop(endpoint: quinn::Endpoint, state: Arc<State>) {
    while let Some(incoming) = endpoint.accept().await {
        let address = incoming.remote_address().ip();
        if !state.allow_request(address) {
            incoming.refuse();
            continue;
        }
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(error) = serve(incoming, &state).await {
                debug!(%address, %error, "relay request ended");
            }
        });
    }
}

type Sender = FramedWrite<quinn::SendStream, MessageCodec<RelayResponse>>;

async fn serve(incoming: quinn::Incoming, state: &State) -> anyhow::Result<()> {
    let (connection, mut sender, request) = tokio::time::timeout(REQUEST_TIMEOUT, async {
        let connection = incoming.await?;
        let (send, receive) = connection.accept_bi().await?;
        let mut receiver = FramedRead::new(
            receive,
            MessageCodec::<RelayRequest>::new(CONTROL_FRAME_LIMIT),
        );
        let sender = FramedWrite::new(
            send,
            MessageCodec::<RelayResponse>::new(CONTROL_FRAME_LIMIT),
        );
        let request = receiver
            .next()
            .await
            .ok_or_else(|| anyhow::anyhow!("no request"))??;
        anyhow::Ok((connection, sender, request))
    })
    .await??;

    match request {
        RelayRequest::Register => register(&connection, &mut sender, state).await,
        RelayRequest::Connect { id } => connect(&connection, &mut sender, state, id).await,
    }
}

async fn register(
    connection: &quinn::Connection,
    sender: &mut Sender,
    state: &State,
) -> anyhow::Result<()> {
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
    let Some(fingerprint) = fingerprint else {
        sender
            .send(&RelayResponse::Refused(RelayError::CertificateRequired))
            .await?;
        return Ok(());
    };
    let id = lock(&state.ids).get_or_assign(&fingerprint)?;
    let generation = state.generations.fetch_add(1, Ordering::Relaxed);
    let (incoming, mut allocations) = mpsc::channel(PENDING_PER_HOST);
    // A newer registration of the same device replaces an older one.
    lock(&state.hosts).insert(
        id,
        RegisteredHost {
            generation,
            incoming,
        },
    );
    info!(%id, "host registered");
    sender.send(&RelayResponse::Registered { id }).await?;

    let result = loop {
        tokio::select! {
            allocation = allocations.recv() => {
                let Some(allocation) = allocation else { break Ok(()) };
                if let Err(error) = sender.send(&RelayResponse::Incoming(allocation)).await {
                    break Err(error.into());
                }
            }
            _ = connection.closed() => break Ok(()),
        }
    };
    let mut hosts = lock(&state.hosts);
    if hosts
        .get(&id)
        .is_some_and(|host| host.generation == generation)
    {
        hosts.remove(&id);
        info!(%id, "host went offline");
    }
    result
}

async fn connect(
    connection: &quinn::Connection,
    sender: &mut Sender,
    state: &State,
    id: DeviceId,
) -> anyhow::Result<()> {
    let host = lock(&state.hosts)
        .get(&id)
        .map(|host| host.incoming.clone());
    let response = match host {
        None => RelayResponse::Refused(RelayError::NotFound),
        Some(host) => match AllocationSlot::try_take(&state.allocations, state.max_allocations) {
            None => {
                warn!("allocation limit reached");
                RelayResponse::Refused(RelayError::Unavailable)
            }
            Some(slot) => {
                let (host_side, viewer_side) = allocate(state.bind_ip, slot).await?;
                if host.try_send(host_side).is_ok() {
                    RelayResponse::Allocated(viewer_side)
                } else {
                    // The host has too many viewers waiting; the unused allocation expires.
                    RelayResponse::Refused(RelayError::TooManyRequests)
                }
            }
        },
    };
    sender.send(&response).await?;
    let _finished = sender.close().await;
    // Let the viewer read the answer before the connection goes away.
    let _closed = tokio::time::timeout(REQUEST_TIMEOUT, connection.closed()).await;
    Ok(())
}
