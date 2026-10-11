//! Headless host and viewer for testing without the GUI.

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "this is the CLI's output"
)]

use std::io::{IsTerminal as _, Write as _};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use dari_media::StreamSettings;
use dari_net::{AccessPassword, DeviceIdentity};
use dari_session::{
    ApprovalDecision, HostConfig, HostEvent, HostPolicy, RelayStatus, SystemPlatform, ViewerConfig,
    ViewerEvent, connect_viewer, start_host,
};

use crate::config::{
    auto_frame_rate, data_directory, device_name, local_addresses, resolve_target,
};

pub(crate) async fn host(port: u16, relay: Option<String>, view_only: bool) -> anyhow::Result<()> {
    let decision = if view_only {
        ApprovalDecision::ViewOnly
    } else {
        ApprovalDecision::AllowControl
    };
    let identity = Arc::new(DeviceIdentity::load_or_generate(&data_directory()?)?);
    let (handle, mut events) = start_host(
        HostConfig {
            bind_address: (std::net::Ipv6Addr::UNSPECIFIED, port).into(),
            host_name: device_name(),
            stream: StreamSettings::default(),
            policy: HostPolicy {
                // The headless host has no one to ask, so it answers each request itself:
                // anyone with the password gets control, or with --view-only only the screen.
                // Only an approval can grant less than control.
                require_approval: view_only,
                clipboard: false,
                // Files would land on this machine without anyone choosing to accept them.
                file_transfer: false,
                audio: true,
            },
            downloads: None,
            relay,
        },
        identity.clone(),
        Arc::new(SystemPlatform),
    )
    .context("cannot start hosting")?;
    let port = handle.local_address().port();
    println!(
        "Hosting as \"{}\" (device fingerprint {})",
        device_name(),
        identity.fingerprint()
    );
    for address in local_addresses() {
        match address {
            std::net::IpAddr::V4(v4) => println!("  address: {v4}:{port}"),
            std::net::IpAddr::V6(v6) => println!("  address: [{v6}]:{port}"),
        }
    }
    if view_only {
        println!("Viewers can only see the screen.");
    }
    println!("Press Ctrl+C to stop.");
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            event = events.recv() => match event {
                None => break,
                Some(HostEvent::PasswordChanged(Some(password))) => {
                    println!("Access password: {}", password.display_text().as_str());
                }
                Some(HostEvent::PasswordChanged(None)) => println!("Not accepting viewers."),
                Some(HostEvent::SessionStarted(peer)) => {
                    println!("{} ({:?}) connected from {}", peer.name, peer.os, peer.address);
                }
                Some(HostEvent::Relay(status)) => match status {
                    RelayStatus::Registered(id) => println!("Relay ID: {id}"),
                    RelayStatus::Connecting => println!("Connecting to the relay…"),
                    RelayStatus::Unavailable(error) => println!("Relay unavailable: {error}"),
                },
                Some(HostEvent::ApprovalRequested { request, .. }) => request.respond(decision),
                Some(HostEvent::SessionStatus(status)) => {
                    println!("screen: {:?}, input: {:?}", status.screen, status.input);
                }
                Some(HostEvent::SessionEnded { peer, reason }) => {
                    println!("{} left: {reason}", peer.name);
                }
                Some(HostEvent::Transfer(_)) => {}
            }
        }
    }
    drop(handle);
    Ok(())
}

pub(crate) async fn connect(address: &str, relay: &str, fps: Option<u16>) -> anyhow::Result<()> {
    let target = resolve_target(address, relay).await?;
    let typed = tokio::task::spawn_blocking(|| {
        if std::io::stdin().is_terminal() {
            rpassword::prompt_password("Access password: ")
        } else {
            // Scripted use: read the password from piped stdin.
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).map(|_| line)
        }
    })
    .await
    .context("password prompt failed")??;
    let password = AccessPassword::parse(&typed)?;
    let (viewer, mut events) = connect_viewer(
        ViewerConfig {
            target,
            client_name: device_name(),
            map_shortcut_modifier: true,
            clipboard: None,
            frame_rate: fps.unwrap_or_else(auto_frame_rate),
            downloads: None,
            audio: None,
            play_audio: false,
        },
        &password,
    )
    .await
    .context("cannot connect")?;
    println!(
        "Connected to {} ({:?}). Press Ctrl+C to disconnect.",
        viewer.peer().name,
        viewer.peer().os
    );

    let mut report = tokio::time::interval(Duration::from_secs(1));
    let (mut last_frames, mut last_bytes, mut last_report) = (0, 0, Instant::now());
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                viewer.disconnect();
            }
            event = events.recv() => match event {
                Some(ViewerEvent::AwaitingApproval) => println!("Waiting for the host to allow the session…"),
                Some(ViewerEvent::Displays { displays, active }) => {
                    println!("host displays: {} (showing {active})", displays.len());
                }
                Some(ViewerEvent::FrameRate(rate)) => println!("host streams at up to {rate} fps"),
                Some(ViewerEvent::HostStatus(status)) => {
                    println!("host screen: {:?}, host input: {:?}", status.screen, status.input);
                }
                Some(ViewerEvent::Transfer(_)) => {}
                Some(ViewerEvent::Ended(reason)) => {
                    println!("Session ended: {reason}");
                    break;
                }
                None => break,
            },
            _ = report.tick() => {
                let frames = viewer.stats().frames_decoded.load(Ordering::Relaxed);
                let bytes = viewer.stats().bytes_received.load(Ordering::Relaxed);
                let elapsed = last_report.elapsed().as_secs_f64().max(0.001);
                #[expect(clippy::cast_precision_loss, reason = "display only")]
                let (fps, mbps) = (
                    (frames - last_frames) as f64 / elapsed,
                    (bytes - last_bytes) as f64 * 8.0 / elapsed / 1e6,
                );
                let size = viewer.frames().borrow().as_ref().map(|frame| (frame.width, frame.height));
                print!("\r{fps:5.1} fps  {mbps:6.2} Mbit/s  rtt {:4} ms  frame {size:?}   ",
                    viewer.rtt().as_millis());
                let _flushed = std::io::stdout().flush();
                (last_frames, last_bytes, last_report) = (frames, bytes, Instant::now());
            }
        }
    }
    println!();
    Ok(())
}
