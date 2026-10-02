//! End-to-end tests over real QUIC on the loopback interface.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers may panic"
)]

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use open_desk_net::{
    AccessPassword, ConnectError, DeviceIdentity, HandshakeError, HostEndpoint, HostSettings,
    connect,
};
use open_desk_proto::{ControlMessage, RejectReason, VideoPacket};

fn host() -> HostEndpoint {
    let identity = DeviceIdentity::generate().unwrap();
    HostEndpoint::bind(
        HostSettings {
            bind_address: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            host_name: "test-host".into(),
        },
        &identity,
    )
    .unwrap()
}

fn rejection(result: Result<impl std::fmt::Debug, ConnectError>) -> RejectReason {
    match result {
        Err(ConnectError::Handshake(HandshakeError::Rejected(reason))) => reason,
        other => panic!("expected a rejection, got {other:?}"),
    }
}

#[tokio::test]
async fn authenticated_session_exchanges_control_and_video() {
    let mut host = host();
    let password = AccessPassword::generate().unwrap();
    host.set_password(Some(password.clone()));
    let address = host.local_address().unwrap();

    let (viewer, hosted) = tokio::join!(
        connect(address, &password, "test-viewer".into()),
        host.accept()
    );
    let viewer = viewer.unwrap();
    let hosted = hosted.unwrap();
    assert_eq!(viewer.peer().name, "test-host");
    assert!(viewer.peer().fingerprint.is_some());
    assert_eq!(hosted.peer().name, "test-viewer");

    let (viewer_link, mut viewer_tx, mut viewer_rx) = viewer.split();
    let (host_link, mut host_tx, mut host_rx) = hosted.split();

    viewer_tx
        .send(&ControlMessage::Ping { token: 42 })
        .await
        .unwrap();
    assert_eq!(
        host_rx.next().await.unwrap().unwrap(),
        ControlMessage::Ping { token: 42 }
    );
    host_tx
        .send(&ControlMessage::Pong { token: 42 })
        .await
        .unwrap();
    assert_eq!(
        viewer_rx.next().await.unwrap().unwrap(),
        ControlMessage::Pong { token: 42 }
    );

    let packet = VideoPacket {
        sequence: 0,
        timestamp_us: 0,
        keyframe: true,
        width: 64,
        height: 48,
        data: vec![1; 50_000],
    };
    let mut video_tx = host_link.open_video_sender().await.unwrap();
    video_tx.send(&packet).await.unwrap();
    let mut video_rx = viewer_link.accept_video_receiver().await.unwrap();
    assert_eq!(video_rx.next().await.unwrap().unwrap(), packet);
}

#[tokio::test]
async fn wrong_password_is_rejected() {
    let mut host = host();
    host.set_password(Some(AccessPassword::generate().unwrap()));
    let address = host.local_address().unwrap();
    let guess = AccessPassword::generate().unwrap();

    let result = connect(address, &guess, "viewer".into()).await;
    assert_eq!(rejection(result), RejectReason::AuthenticationFailed);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), host.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn host_without_password_is_not_accepting() {
    let host = host();
    let address = host.local_address().unwrap();
    let result = connect(
        address,
        &AccessPassword::generate().unwrap(),
        "viewer".into(),
    )
    .await;
    assert_eq!(rejection(result), RejectReason::NotAccepting);
}

#[tokio::test]
async fn password_is_consumed_and_second_viewer_is_busy() {
    let mut host = host();
    let password = AccessPassword::generate().unwrap();
    host.set_password(Some(password.clone()));
    let address = host.local_address().unwrap();

    let (first, hosted) = tokio::join!(connect(address, &password, "first".into()), host.accept());
    let _first = first.unwrap();
    let hosted = hosted.unwrap();
    assert!(
        !host.is_accepting(),
        "a successful login consumes the one-time password"
    );

    // Reusing the consumed password fails even though no session slot is involved yet.
    let reuse = connect(address, &password, "replay".into()).await;
    assert_eq!(rejection(reuse), RejectReason::Busy);

    // Even with a fresh password, the active session keeps the host busy.
    let fresh = AccessPassword::generate().unwrap();
    host.set_password(Some(fresh.clone()));
    let second = connect(address, &fresh, "second".into()).await;
    assert_eq!(rejection(second), RejectReason::Busy);

    // Ending the session frees the slot for the fresh password.
    drop(hosted);
    let (third, hosted) = tokio::join!(connect(address, &fresh, "third".into()), host.accept());
    assert!(third.is_ok());
    assert!(hosted.is_some());
}

#[tokio::test]
async fn consumed_password_is_rejected_after_session_ends() {
    let mut host = host();
    let password = AccessPassword::generate().unwrap();
    host.set_password(Some(password.clone()));
    let address = host.local_address().unwrap();

    let (first, hosted) = tokio::join!(connect(address, &password, "first".into()), host.accept());
    drop(first.unwrap());
    drop(hosted.unwrap());
    let replay = connect(address, &password, "replay".into()).await;
    assert_eq!(rejection(replay), RejectReason::NotAccepting);
}

#[tokio::test]
async fn repeated_failures_lock_out_the_source() {
    let mut host = host();
    let password = AccessPassword::generate().unwrap();
    host.set_password(Some(password.clone()));
    let address = host.local_address().unwrap();

    for _ in 0..4 {
        let guess = AccessPassword::generate().unwrap();
        let result = connect(address, &guess, "attacker".into()).await;
        assert_eq!(rejection(result), RejectReason::AuthenticationFailed);
    }
    // Locked out now: even the right password is refused before any handshake.
    let locked = connect(address, &password, "attacker".into()).await;
    assert!(
        matches!(locked, Err(ConnectError::Connection(_))),
        "got {locked:?}"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(200), host.accept())
            .await
            .is_err()
    );
}
