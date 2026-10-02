//! QUIC/TLS configuration for hosts and viewers.

use std::sync::Arc;
use std::time::Duration;

use open_desk_proto::RELAY_ALPN;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{IdleTimeout, TransportConfig, VarInt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use thiserror::Error;

use crate::identity::DeviceIdentity;

pub(crate) const ALPN: &[u8] = b"open-desk/1";
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const KEEP_ALIVE: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum TlsConfigError {
    #[error("TLS configuration failed: {0}")]
    Rustls(#[from] rustls::Error),
    #[error("QUIC does not support this TLS configuration")]
    UnsupportedCipherSuite,
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Transport limits. Viewers open exactly one bidirectional stream (handshake, then control);
/// hosts open unidirectional streams for media.
fn transport(peer_bidi_streams: u32, peer_uni_streams: u32) -> Arc<TransportConfig> {
    let mut transport = TransportConfig::default();
    transport
        .max_idle_timeout(Some(IdleTimeout::from(VarInt::from_u32(
            u32::try_from(IDLE_TIMEOUT.as_millis()).unwrap_or(u32::MAX),
        ))))
        .keep_alive_interval(Some(KEEP_ALIVE))
        .max_concurrent_bidi_streams(VarInt::from_u32(peer_bidi_streams))
        .max_concurrent_uni_streams(VarInt::from_u32(peer_uni_streams));
    Arc::new(transport)
}

pub(crate) fn server_config(
    identity: &DeviceIdentity,
) -> Result<quinn::ServerConfig, TlsConfigError> {
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![identity.certificate()], identity.private_key())?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    // 0-RTT data could be replayed; the handshake never needs it.
    tls.max_early_data_size = 0;
    let crypto =
        QuicServerConfig::try_from(tls).map_err(|_| TlsConfigError::UnsupportedCipherSuite)?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    config.transport_config(transport(1, 0));
    Ok(config)
}

pub(crate) fn client_config() -> Result<quinn::ClientConfig, TlsConfigError> {
    let provider = provider();
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PasswordAuthenticatedServer { provider }))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto =
        QuicClientConfig::try_from(tls).map_err(|_| TlsConfigError::UnsupportedCipherSuite)?;
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    config.transport_config(transport(0, 4));
    Ok(config)
}

/// TLS for a relay's control endpoint. Devices may present their certificate as a client
/// certificate; the relay identifies registering hosts by it.
pub fn relay_server_config(
    identity: &DeviceIdentity,
) -> Result<quinn::ServerConfig, TlsConfigError> {
    let provider = provider();
    let mut tls = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(SelfSignedClient { provider }))
        .with_single_cert(vec![identity.certificate()], identity.private_key())?;
    tls.alpn_protocols = vec![RELAY_ALPN.to_vec()];
    tls.max_early_data_size = 0;
    let crypto =
        QuicServerConfig::try_from(tls).map_err(|_| TlsConfigError::UnsupportedCipherSuite)?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    config.transport_config(transport(1, 0));
    Ok(config)
}

/// TLS for talking to a relay, presenting `identity` as a client certificate when given.
///
/// The relay's own certificate is not verified: the relay is not trusted with anything. It
/// only learns which devices are online and forwards ciphertext; sessions through it are
/// authenticated end to end by SPAKE2.
pub(crate) fn relay_client_config(
    identity: Option<&DeviceIdentity>,
) -> Result<quinn::ClientConfig, TlsConfigError> {
    let provider = provider();
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PasswordAuthenticatedServer { provider }));
    let mut tls = match identity {
        Some(identity) => {
            builder.with_client_auth_cert(vec![identity.certificate()], identity.private_key())?
        }
        None => builder.with_no_client_auth(),
    };
    tls.alpn_protocols = vec![RELAY_ALPN.to_vec()];
    let crypto =
        QuicClientConfig::try_from(tls).map_err(|_| TlsConfigError::UnsupportedCipherSuite)?;
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    config.transport_config(transport(0, 0));
    Ok(config)
}

/// Accepts any self-signed client certificate, but only after the client proved possession of
/// its key with a valid TLS signature, so the certificate fingerprint identifies the client.
#[derive(Debug)]
struct SelfSignedClient {
    provider: Arc<CryptoProvider>,
}

impl ClientCertVerifier for SelfSignedClient {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        // Viewers asking for a connection have no reason to identify themselves.
        false
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            certificate,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            certificate,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Accepts any host certificate, because hosts are self-signed and there is no CA to vouch
/// for them. The host is instead authenticated by the SPAKE2 handshake, whose confirmations are
/// bound to this TLS session's exporter: a substituted certificate means a different session
/// and a failed handshake. Handshake signatures are still verified so the exporter is tied to
/// the presented key.
#[derive(Debug)]
struct PasswordAuthenticatedServer {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PasswordAuthenticatedServer {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            certificate,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            certificate,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
