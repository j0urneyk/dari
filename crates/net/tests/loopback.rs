//! End-to-end tests over real QUIC on the loopback interface.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers may panic"
)]

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use dari_net::{
    AccessPassword, ConnectError, DeviceIdentity, HandshakeError, HostEndpoint, HostSettings,
    IncomingStream, connect,
};
use dari_proto::{AudioPacket, ControlMessage, RejectReason, TransferId, VideoPacket};
use futures_util::{SinkExt, StreamExt};

fn host() -> HostEndpoint {
    host_on(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
}

fn host_on(bind_address: SocketAddr) -> HostEndpoint {
    let identity = DeviceIdentity::generate().unwrap();
    HostEndpoint::bind(
        HostSettings {
            bind_address,
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
    let mut video_tx = host_link.streams().open_video_sender().await.unwrap();
    video_tx.send(&packet).await.unwrap();
    let Ok(IncomingStream::Video(mut video_rx)) = viewer_link.streams().accept().await else {
        panic!("expected the video stream");
    };
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

#[tokio::test]
async fn host_on_the_ipv6_wildcard_accepts_ipv4_viewers() {
    // The app hosts on [::]. Windows makes IPv6 sockets IPv6-only by default, which turned away
    // every viewer that connected by IPv4 address.
    let mut host = host_on(SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)));
    let password = AccessPassword::generate().unwrap();
    host.set_password(Some(password.clone()));
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, host.local_address().unwrap().port()));

    let (viewer, hosted) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            connect(address, &password, "ipv4-viewer".into()),
            host.accept()
        )
    })
    .await
    .expect("an IPv4 viewer reaches a host on [::]");
    viewer.unwrap();
    assert_eq!(hosted.unwrap().peer().name, "ipv4-viewer");
}

async fn read_to_end(
    stream: &mut dari_net::FileReceiver,
) -> Result<Vec<u8>, dari_net::StreamError> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 256];
    while let Some(read) = stream.read(&mut buffer).await? {
        bytes.extend_from_slice(&buffer[..read]);
    }
    Ok(bytes)
}

async fn session_pair() -> (
    HostEndpoint,
    dari_net::AuthenticatedConnection,
    dari_net::AuthenticatedConnection,
) {
    let mut host = host();
    let password = AccessPassword::generate().unwrap();
    host.set_password(Some(password.clone()));
    let address = host.local_address().unwrap();
    let (viewer, hosted) = tokio::join!(
        connect(address, &password, "test-viewer".into()),
        host.accept()
    );
    (host, viewer.unwrap(), hosted.unwrap())
}

#[tokio::test]
async fn viewer_file_streams_need_credit_and_carry_their_id() {
    let (_host, viewer, hosted) = session_pair().await;
    let (viewer_link, _viewer_tx, _viewer_rx) = viewer.split();
    let (host_link, _host_tx, _host_rx) = hosted.split();

    let blocked = tokio::time::timeout(
        Duration::from_millis(300),
        viewer_link.streams().open_file_sender(TransferId(1)),
    )
    .await;
    assert!(blocked.is_err(), "viewers get no stream credit by default");

    host_link.streams().allow_peer_streams(2);
    let mut sender = viewer_link
        .streams()
        .open_file_sender(TransferId(7))
        .await
        .unwrap();
    sender.write_all(b"hello file").await.unwrap();
    sender.finish().unwrap();

    let Ok(IncomingStream::File { id, mut stream }) = host_link.streams().accept().await else {
        panic!("expected a file stream");
    };
    assert_eq!(id, TransferId(7));
    assert_eq!(read_to_end(&mut stream).await.unwrap(), b"hello file");
}

#[tokio::test]
async fn a_dropped_file_sender_resets_instead_of_finishing() {
    let (_host, viewer, hosted) = session_pair().await;
    let (viewer_link, _viewer_tx, _viewer_rx) = viewer.split();
    let (host_link, _host_tx, _host_rx) = hosted.split();

    let mut sender = host_link
        .streams()
        .open_file_sender(TransferId(2))
        .await
        .unwrap();
    sender.write_all(b"partial").await.unwrap();
    let Ok(IncomingStream::File { id, mut stream }) = viewer_link.streams().accept().await else {
        panic!("expected a file stream");
    };
    assert_eq!(id, TransferId(2));
    drop(sender);
    assert!(read_to_end(&mut stream).await.is_err());
}

#[tokio::test]
async fn a_stream_reset_before_its_header_is_skipped() {
    let (_host, viewer, hosted) = session_pair().await;
    let (viewer_link, _viewer_tx, _viewer_rx) = viewer.split();
    let (host_link, _host_tx, _host_rx) = hosted.split();

    let cancelled = host_link
        .streams()
        .open_file_sender(TransferId(2))
        .await
        .unwrap();
    drop(cancelled);
    let mut next = host_link
        .streams()
        .open_file_sender(TransferId(4))
        .await
        .unwrap();
    next.write_all(b"x").await.unwrap();
    next.finish().unwrap();

    // The reset stream may or may not have delivered its header; either way the session's
    // next stream still arrives.
    let mut ids = Vec::new();
    while !ids.contains(&TransferId(4)) {
        match tokio::time::timeout(Duration::from_secs(5), viewer_link.streams().accept())
            .await
            .unwrap()
        {
            Ok(IncomingStream::File { id, .. }) => ids.push(id),
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[tokio::test]
async fn audio_datagrams_flow_only_from_host_to_viewer() {
    let (_host, viewer, hosted) = session_pair().await;
    let (viewer_link, _viewer_tx, _viewer_rx) = viewer.split();
    let (host_link, _host_tx, _host_rx) = hosted.split();

    let packet = AudioPacket {
        sequence: 9,
        data: vec![0xfc; 300],
    };
    host_link.streams().send_audio(&packet).unwrap();
    let received = tokio::time::timeout(
        Duration::from_secs(5),
        viewer_link.streams().receive_audio(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(received, packet);

    // Hosts accept no datagrams, so a viewer can't push audio (or anything else) that way.
    assert!(viewer_link.streams().send_audio(&packet).is_err());
}
