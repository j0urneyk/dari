//! Password-authenticated key exchange bound to the TLS session.
//!
//! Both peers run SPAKE2 over the handshake stream with the one-time password and then prove
//! they derived the same key by exchanging HMAC confirmations over the TLS exporter and the
//! hello transcript. A man in the middle terminates two different TLS sessions and therefore
//! sees two different exporters, so its confirmations can never verify even if it relays every
//! message. An eavesdropper learns nothing it could use to test password guesses offline.

use dari_proto::{
    AuthOutcome, ClientHello, CodecError, HANDSHAKE_FRAME_LIMIT, HandshakeMessage,
    KEY_CONFIRMATION_LEN, MessageCodec, PROTOCOL_VERSION, RejectReason, ServerHello,
};
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use subtle::ConstantTimeEq;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{FramedRead, FramedWrite};
use zeroize::Zeroizing;

use crate::password::AccessPassword;

/// Length of the TLS exporter value the confirmations are bound to.
pub(crate) const EXPORTER_LEN: usize = 32;
pub(crate) const EXPORTER_LABEL: &[u8] = b"EXPORTER-dari-auth-v1";

const VIEWER_IDENTITY: &[u8] = b"dari viewer";
const HOST_IDENTITY: &[u8] = b"dari host";
const CONFIRMATION_LABEL: &[u8] = b"dari key confirmation v1";

#[derive(Debug, Error)]
pub enum HandshakeError {
    #[error("the host refused the session: {0}")]
    Rejected(RejectReason),
    #[error("the remote device could not prove it knows the password")]
    PeerAuthenticationFailed,
    #[error("the peer violated the handshake protocol: {0}")]
    Protocol(&'static str),
    #[error("the peer closed the connection during the handshake")]
    Closed,
    #[error(transparent)]
    Codec(#[from] CodecError),
}

/// The handshake stream, framed for [`HandshakeMessage`]s.
pub(crate) struct HandshakeChannel<R, W> {
    pub(crate) reader: FramedRead<R, MessageCodec<HandshakeMessage>>,
    pub(crate) writer: FramedWrite<W, MessageCodec<HandshakeMessage>>,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> HandshakeChannel<R, W> {
    pub(crate) fn new(reader: R, writer: W) -> Self {
        Self {
            reader: FramedRead::new(reader, MessageCodec::new(HANDSHAKE_FRAME_LIMIT)),
            writer: FramedWrite::new(writer, MessageCodec::new(HANDSHAKE_FRAME_LIMIT)),
        }
    }

    async fn send(&mut self, message: &HandshakeMessage) -> Result<(), HandshakeError> {
        self.writer.send(message).await?;
        Ok(())
    }

    async fn receive(&mut self) -> Result<HandshakeMessage, HandshakeError> {
        match self.reader.next().await {
            Some(message) => Ok(message?),
            None => Err(HandshakeError::Closed),
        }
    }
}

#[derive(Clone, Copy)]
enum Role {
    Viewer,
    Host,
}

/// Runs the viewer side. Returns the host's hello once both sides have proven the password.
pub(crate) async fn authenticate_to_host<R, W>(
    channel: &mut HandshakeChannel<R, W>,
    password: &AccessPassword,
    exporter: &[u8; EXPORTER_LEN],
    hello: ClientHello,
) -> Result<ServerHello, HandshakeError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    channel
        .send(&HandshakeMessage::ClientHello(hello.clone()))
        .await?;
    let server_hello = match channel.receive().await? {
        HandshakeMessage::ServerHello(server_hello) => server_hello,
        message => return Err(unexpected(&message)),
    };
    if !PROTOCOL_VERSION.is_compatible_with(server_hello.version) {
        return Err(HandshakeError::Rejected(RejectReason::IncompatibleVersion));
    }
    let transcript = transcript_hash(&hello, &server_hello)?;

    let (spake, outbound) = Spake2::<Ed25519Group>::start_a(
        &Password::new(password.as_bytes()),
        &Identity::new(VIEWER_IDENTITY),
        &Identity::new(HOST_IDENTITY),
    );
    channel.send(&HandshakeMessage::Pake(outbound)).await?;
    let inbound = match channel.receive().await? {
        HandshakeMessage::Pake(inbound) => inbound,
        message => return Err(unexpected(&message)),
    };
    let key = Zeroizing::new(
        spake
            .finish(&inbound)
            .map_err(|_| HandshakeError::Protocol("invalid key exchange message"))?,
    );

    let own = confirmation(&key, Role::Viewer, exporter, &transcript);
    channel.send(&HandshakeMessage::Confirmation(own)).await?;
    let theirs = match channel.receive().await? {
        HandshakeMessage::Confirmation(theirs) => theirs,
        message => return Err(unexpected(&message)),
    };
    let expected = confirmation(&key, Role::Host, exporter, &transcript);
    if !bool::from(expected.ct_eq(&theirs)) {
        return Err(HandshakeError::PeerAuthenticationFailed);
    }
    match channel.receive().await? {
        HandshakeMessage::Outcome(AuthOutcome::Accepted) => Ok(server_hello),
        message => Err(unexpected(&message)),
    }
}

/// What the host decided about an incoming connection before reading its hello.
pub(crate) struct HostAdmission<'a> {
    /// `Err` if the host refuses before any password check (busy, throttled, not accepting).
    pub(crate) precheck: Result<&'a AccessPassword, RejectReason>,
    pub(crate) server_hello: ServerHello,
    pub(crate) exporter: [u8; EXPORTER_LEN],
}

/// Runs the host side.
///
/// `password_attempted` is set once the viewer could have made a password guess, so the caller
/// can count failures (including timeouts) against the source's attempt budget. `finalize` is
/// called after the viewer proved the password and before the host reveals its confirmation;
/// it claims the session slot and may still refuse.
pub(crate) async fn authenticate_viewer<R, W, G>(
    channel: &mut HandshakeChannel<R, W>,
    admission: HostAdmission<'_>,
    password_attempted: &mut bool,
    finalize: impl FnOnce() -> Result<G, RejectReason>,
) -> Result<(ClientHello, G), HandshakeError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let client_hello = match channel.receive().await? {
        HandshakeMessage::ClientHello(client_hello) => client_hello,
        message => return Err(unexpected(&message)),
    };
    if !PROTOCOL_VERSION.is_compatible_with(client_hello.version) {
        return reject(channel, RejectReason::IncompatibleVersion).await;
    }
    let password = match admission.precheck {
        Ok(password) => password,
        Err(reason) => return reject(channel, reason).await,
    };
    let transcript = transcript_hash(&client_hello, &admission.server_hello)?;
    channel
        .send(&HandshakeMessage::ServerHello(admission.server_hello))
        .await?;

    *password_attempted = true;
    let inbound = match channel.receive().await? {
        HandshakeMessage::Pake(inbound) => inbound,
        message => return Err(unexpected(&message)),
    };
    let (spake, outbound) = Spake2::<Ed25519Group>::start_b(
        &Password::new(password.as_bytes()),
        &Identity::new(VIEWER_IDENTITY),
        &Identity::new(HOST_IDENTITY),
    );
    channel.send(&HandshakeMessage::Pake(outbound)).await?;
    let key = Zeroizing::new(
        spake
            .finish(&inbound)
            .map_err(|_| HandshakeError::Protocol("invalid key exchange message"))?,
    );

    let theirs = match channel.receive().await? {
        HandshakeMessage::Confirmation(theirs) => theirs,
        message => return Err(unexpected(&message)),
    };
    let expected = confirmation(&key, Role::Viewer, &admission.exporter, &transcript);
    if !bool::from(expected.ct_eq(&theirs)) {
        // Best effort: the viewer learns the outcome, but the failure is ours to report.
        let _notified: Result<(), _> = reject(channel, RejectReason::AuthenticationFailed).await;
        return Err(HandshakeError::PeerAuthenticationFailed);
    }
    let guard = match finalize() {
        Ok(guard) => guard,
        Err(reason) => return reject(channel, reason).await,
    };
    let own = confirmation(&key, Role::Host, &admission.exporter, &transcript);
    channel.send(&HandshakeMessage::Confirmation(own)).await?;
    channel
        .send(&HandshakeMessage::Outcome(AuthOutcome::Accepted))
        .await?;
    Ok((client_hello, guard))
}

async fn reject<R, W, T>(
    channel: &mut HandshakeChannel<R, W>,
    reason: RejectReason,
) -> Result<T, HandshakeError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    channel
        .send(&HandshakeMessage::Outcome(AuthOutcome::Rejected(reason)))
        .await?;
    Err(HandshakeError::Rejected(reason))
}

fn unexpected(message: &HandshakeMessage) -> HandshakeError {
    match message {
        HandshakeMessage::Outcome(AuthOutcome::Rejected(reason)) => {
            HandshakeError::Rejected(*reason)
        }
        _ => HandshakeError::Protocol("unexpected handshake message"),
    }
}

/// Hash of both hellos, so neither side's announced version, name, or OS can be altered.
fn transcript_hash(client: &ClientHello, server: &ServerHello) -> Result<[u8; 32], HandshakeError> {
    let mut hasher = Sha256::new();
    for encoded in [postcard::to_allocvec(client), postcard::to_allocvec(server)] {
        let encoded = encoded.map_err(CodecError::from)?;
        hasher.update(
            u32::try_from(encoded.len())
                .unwrap_or(u32::MAX)
                .to_be_bytes(),
        );
        hasher.update(&encoded);
    }
    Ok(hasher.finalize().into())
}

fn confirmation(
    key: &[u8],
    role: Role,
    exporter: &[u8; EXPORTER_LEN],
    transcript: &[u8; 32],
) -> [u8; KEY_CONFIRMATION_LEN] {
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(key) else {
        unreachable!("HMAC accepts keys of any length");
    };
    mac.update(CONFIRMATION_LABEL);
    mac.update(match role {
        Role::Viewer => b"viewer",
        Role::Host => b"host\0\0",
    });
    mac.update(exporter);
    mac.update(transcript);
    mac.finalize().into_bytes().into()
}

#[cfg(test)]
mod tests {
    use dari_proto::Os;
    use tokio::io::{DuplexStream, ReadHalf, WriteHalf, duplex, split};

    use super::*;

    type Channel = HandshakeChannel<ReadHalf<DuplexStream>, WriteHalf<DuplexStream>>;

    fn channel_pair() -> (Channel, Channel) {
        let (left, right) = duplex(64 * 1024);
        let (left_read, left_write) = split(left);
        let (right_read, right_write) = split(right);
        (
            HandshakeChannel::new(left_read, left_write),
            HandshakeChannel::new(right_read, right_write),
        )
    }

    fn client_hello() -> ClientHello {
        ClientHello {
            version: PROTOCOL_VERSION,
            client_name: "viewer".into(),
            client_os: Os::MacOs,
        }
    }

    fn server_hello() -> ServerHello {
        ServerHello {
            version: PROTOCOL_VERSION,
            host_name: "host".into(),
            host_os: Os::Windows,
        }
    }

    struct Outcome {
        viewer: Result<ServerHello, HandshakeError>,
        host: Result<ClientHello, HandshakeError>,
        attempted: bool,
    }

    async fn run(
        viewer_password: &AccessPassword,
        host_password: Result<&AccessPassword, RejectReason>,
        viewer_exporter: [u8; EXPORTER_LEN],
        host_exporter: [u8; EXPORTER_LEN],
    ) -> Outcome {
        let (mut viewer_channel, mut host_channel) = channel_pair();
        let mut attempted = false;
        let viewer = authenticate_to_host(
            &mut viewer_channel,
            viewer_password,
            &viewer_exporter,
            client_hello(),
        );
        let host = authenticate_viewer(
            &mut host_channel,
            HostAdmission {
                precheck: host_password,
                server_hello: server_hello(),
                exporter: host_exporter,
            },
            &mut attempted,
            || Ok(()),
        );
        let (viewer, host) = tokio::join!(viewer, host);
        Outcome {
            viewer,
            host: host.map(|(hello, ())| hello),
            attempted,
        }
    }

    #[tokio::test]
    async fn matching_passwords_authenticate_both_sides() {
        let password = AccessPassword::generate().unwrap();
        let outcome = run(&password, Ok(&password), [7; 32], [7; 32]).await;
        assert_eq!(outcome.viewer.unwrap(), server_hello());
        assert_eq!(outcome.host.unwrap(), client_hello());
        assert!(outcome.attempted);
    }

    #[tokio::test]
    async fn wrong_password_is_rejected() {
        let host_password = AccessPassword::generate().unwrap();
        let guess = AccessPassword::generate().unwrap();
        let outcome = run(&guess, Ok(&host_password), [7; 32], [7; 32]).await;
        assert!(matches!(
            outcome.viewer,
            Err(HandshakeError::Rejected(RejectReason::AuthenticationFailed))
        ));
        assert!(matches!(
            outcome.host,
            Err(HandshakeError::PeerAuthenticationFailed)
        ));
        assert!(outcome.attempted);
    }

    #[tokio::test]
    async fn different_tls_sessions_cannot_authenticate() {
        // A relaying man in the middle sees different exporters on its two legs.
        let password = AccessPassword::generate().unwrap();
        let outcome = run(&password, Ok(&password), [1; 32], [2; 32]).await;
        assert!(outcome.viewer.is_err());
        assert!(matches!(
            outcome.host,
            Err(HandshakeError::PeerAuthenticationFailed)
        ));
    }

    #[tokio::test]
    async fn precheck_rejection_happens_before_any_password_use() {
        let password = AccessPassword::generate().unwrap();
        let outcome = run(&password, Err(RejectReason::Busy), [7; 32], [7; 32]).await;
        assert!(matches!(
            outcome.viewer,
            Err(HandshakeError::Rejected(RejectReason::Busy))
        ));
        assert!(matches!(
            outcome.host,
            Err(HandshakeError::Rejected(RejectReason::Busy))
        ));
        assert!(!outcome.attempted);
    }

    #[tokio::test]
    async fn incompatible_viewer_version_is_rejected() {
        let password = AccessPassword::generate().unwrap();
        let (mut viewer_channel, mut host_channel) = channel_pair();
        let mut attempted = false;
        let mut hello = client_hello();
        hello.version.major += 1;
        let viewer = async {
            viewer_channel
                .send(&HandshakeMessage::ClientHello(hello))
                .await
                .unwrap();
            viewer_channel.receive().await.unwrap()
        };
        let host = authenticate_viewer(
            &mut host_channel,
            HostAdmission {
                precheck: Ok(&password),
                server_hello: server_hello(),
                exporter: [0; 32],
            },
            &mut attempted,
            || Ok(()),
        );
        let (reply, result) = tokio::join!(viewer, host);
        assert_eq!(
            reply,
            HandshakeMessage::Outcome(AuthOutcome::Rejected(RejectReason::IncompatibleVersion))
        );
        assert!(result.is_err());
        assert!(!attempted);
    }

    #[tokio::test]
    async fn host_keeps_its_confirmation_when_finalize_refuses() {
        let password = AccessPassword::generate().unwrap();
        let (mut viewer_channel, mut host_channel) = channel_pair();
        let mut attempted = false;
        let viewer = authenticate_to_host(&mut viewer_channel, &password, &[3; 32], client_hello());
        let host = authenticate_viewer(
            &mut host_channel,
            HostAdmission {
                precheck: Ok(&password),
                server_hello: server_hello(),
                exporter: [3; 32],
            },
            &mut attempted,
            || Err::<(), _>(RejectReason::Busy),
        );
        let (viewer, host) = tokio::join!(viewer, host);
        assert!(matches!(
            viewer,
            Err(HandshakeError::Rejected(RejectReason::Busy))
        ));
        assert!(matches!(
            host,
            Err(HandshakeError::Rejected(RejectReason::Busy))
        ));
    }
}
