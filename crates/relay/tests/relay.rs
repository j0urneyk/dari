//! Host and viewer reaching each other through a local relay.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers may panic"
)]

use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::time::Duration;

use dari_net::{
    AccessPassword, ConnectError, DeviceIdentity, HostEndpoint, HostSettings, RelayRegistration,
    bind_to_allocation, connect_via_relay,
};
use dari_proto::{ControlMessage, DeviceId, RelayError};
use dari_relay::{RelayConfig, RelayServer};
use futures_util::{SinkExt, StreamExt};

fn relay(data: &Path) -> RelayServer {
    RelayServer::start(&RelayConfig {
        listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        data_directory: data.to_owned(),
        max_allocations: 8,
    })
    .unwrap()
}

/// A host registered with the relay that serves every allocation it is offered.
async fn relayed_host(
    relay: SocketAddr,
    identity: &DeviceIdentity,
) -> (HostEndpoint, DeviceId, AccessPassword) {
    let host = HostEndpoint::bind(
        HostSettings {
            bind_address: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            host_name: "relayed-host".into(),
        },
        identity,
    )
    .unwrap();
    let password = AccessPassword::generate().unwrap();
    host.set_password(Some(password.clone()));
    let mut registration = RelayRegistration::register(relay, identity).await.unwrap();
    let id = registration.id();
    let acceptor = host.relayed_acceptor();
    tokio::spawn(async move {
        while let Ok(allocation) = registration.next_allocation().await {
            let socket =
                tokio::task::spawn_blocking(move || bind_to_allocation(relay.ip(), &allocation))
                    .await
                    .unwrap()
                    .unwrap();
            acceptor.accept_on(socket).unwrap();
        }
    });
    (host, id, password)
}

#[tokio::test(flavor = "multi_thread")]
async fn viewer_reaches_a_host_by_id_through_the_relay() {
    let data = tempfile::tempdir().unwrap();
    let relay = relay(data.path());
    let relay_address = relay.local_address().unwrap();
    let identity = DeviceIdentity::generate().unwrap();
    let (mut host, id, password) = relayed_host(relay_address, &identity).await;

    let (viewer, hosted) = tokio::join!(
        connect_via_relay(relay_address, id, &password, "relayed-viewer".into()),
        tokio::time::timeout(Duration::from_secs(15), host.accept()),
    );
    let viewer = viewer.unwrap();
    let hosted = hosted.unwrap().unwrap();
    assert_eq!(viewer.peer().name, "relayed-host");
    assert_eq!(viewer.peer().fingerprint, Some(identity.fingerprint()));
    assert_eq!(hosted.peer().name, "relayed-viewer");

    // The end-to-end session works through the forwarded ports.
    let (_viewer_link, mut viewer_tx, _viewer_rx) = viewer.split();
    let (_host_link, _host_tx, mut host_rx) = hosted.split();
    viewer_tx
        .send(&ControlMessage::Ping { token: 9 })
        .await
        .unwrap();
    assert_eq!(
        host_rx.next().await.unwrap().unwrap(),
        ControlMessage::Ping { token: 9 }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_password_through_the_relay_is_rejected_end_to_end() {
    let data = tempfile::tempdir().unwrap();
    let relay = relay(data.path());
    let relay_address = relay.local_address().unwrap();
    let identity = DeviceIdentity::generate().unwrap();
    let (_host, id, _password) = relayed_host(relay_address, &identity).await;
    let guess = AccessPassword::generate().unwrap();
    let result = connect_via_relay(relay_address, id, &guess, "attacker".into()).await;
    assert!(
        matches!(result, Err(ConnectError::Handshake(_))),
        "the host, not the relay, decides: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_ids_are_reported() {
    let data = tempfile::tempdir().unwrap();
    let relay = relay(data.path());
    let unknown = DeviceId::new(123_456_789).unwrap();
    let password = AccessPassword::generate().unwrap();
    let result = connect_via_relay(
        relay.local_address().unwrap(),
        unknown,
        &password,
        "viewer".into(),
    )
    .await;
    assert!(
        matches!(result, Err(ConnectError::Relay(RelayError::NotFound))),
        "{result:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_keeps_its_id_across_relay_restarts() {
    let data = tempfile::tempdir().unwrap();
    let identity = DeviceIdentity::generate().unwrap();
    let first = {
        let relay = relay(data.path());
        RelayRegistration::register(relay.local_address().unwrap(), &identity)
            .await
            .unwrap()
            .id()
    };
    let relay = relay(data.path());
    let second = RelayRegistration::register(relay.local_address().unwrap(), &identity)
        .await
        .unwrap()
        .id();
    assert_eq!(first, second);
    let other = DeviceIdentity::generate().unwrap();
    let third = RelayRegistration::register(relay.local_address().unwrap(), &other)
        .await
        .unwrap()
        .id();
    assert_ne!(first, third);
}
