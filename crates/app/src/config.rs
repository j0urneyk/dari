//! Locations, defaults, and addressing shared by the GUI and the CLI.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use anyhow::{Context as _, bail};
use dari_proto::{DeviceId, MAX_DEVICE_NAME_CHARS, sanitize_display_text};
use dari_session::ViewerTarget;

/// UDP port hosts listen on unless configured otherwise.
pub(crate) const DEFAULT_PORT: u16 = 47821;

/// The frame rates the viewer offers. "Auto" picks from this display's refresh rate instead.
pub(crate) const FRAME_RATE_CHOICES: [u16; 5] = [30, 60, 90, 120, 144];
/// What "Auto" asks for when no display reports its refresh rate.
const FALLBACK_FRAME_RATE: u16 = 60;

/// Per-user directory for the device identity and settings.
pub(crate) fn data_directory() -> anyhow::Result<PathBuf> {
    directories::ProjectDirs::from("dev", "dari", "dari")
        .map(|directories| directories.data_local_dir().to_owned())
        .context("cannot determine the user's application data directory")
}

/// This computer's name as shown to the other side, made safe for the protocol's limits.
pub(crate) fn device_name() -> String {
    let raw = gethostname::gethostname().to_string_lossy().into_owned();
    let raw = raw.strip_suffix(".local").unwrap_or(&raw);
    let name = sanitize_display_text(raw, MAX_DEVICE_NAME_CHARS);
    if name.trim().is_empty() {
        "Dari".into()
    } else {
        name
    }
}

/// Resolves what a person typed as the remote address: `IP`, `IP:port`, `host`, or `host:port`.
pub(crate) async fn resolve_address(input: &str) -> anyhow::Result<SocketAddr> {
    let input = input.trim();
    if input.is_empty() {
        bail!("enter the remote device's address");
    }
    if let Ok(address) = input.parse::<SocketAddr>() {
        return Ok(address);
    }
    if let Ok(ip) = input
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
    {
        return Ok(SocketAddr::new(ip, DEFAULT_PORT));
    }
    let with_port = if input.contains(':') {
        input.to_owned()
    } else {
        format!("{input}:{DEFAULT_PORT}")
    };
    tokio::net::lookup_host(&with_port)
        .await
        .with_context(|| format!("cannot resolve {input}"))?
        .next()
        .with_context(|| format!("{input} has no address"))
}

/// What the connect form's input names: a relay ID (with a relay configured) or an address.
pub(crate) async fn resolve_target(input: &str, relay: &str) -> anyhow::Result<ViewerTarget> {
    if let Some(id) = DeviceId::parse(input) {
        if relay.trim().is_empty() {
            bail!("{}", crate::text::text().relay_required);
        }
        return Ok(ViewerTarget::Relay {
            relay: relay.trim().to_owned(),
            id,
        });
    }
    Ok(ViewerTarget::Direct(resolve_address(input).await?))
}

/// The frame rate "Auto" asks the host for: the fastest refresh rate among this machine's
/// displays, since the viewer window may move to any of them, up to the highest choice offered.
/// The host lowers it further to its own display's refresh rate.
pub(crate) fn auto_frame_rate() -> u16 {
    let refresh = dari_media::list_displays()
        .unwrap_or_default()
        .iter()
        .map(|display| display.refresh_rate)
        .max()
        .unwrap_or(0);
    auto_frame_rate_for(refresh)
}

fn auto_frame_rate_for(refresh_rate: u32) -> u16 {
    let highest = FRAME_RATE_CHOICES[FRAME_RATE_CHOICES.len() - 1];
    match u16::try_from(refresh_rate) {
        Ok(0) => FALLBACK_FRAME_RATE,
        Ok(refresh) => refresh.min(highest),
        Err(_) => highest,
    }
}

/// Addresses on this machine that other devices on the network can reach, best first.
pub(crate) fn local_addresses() -> Vec<IpAddr> {
    let mut addresses: Vec<IpAddr> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|interface| !interface.is_loopback())
        .map(|interface| interface.ip())
        .filter(|ip| match ip {
            IpAddr::V4(v4) => !v4.is_link_local(),
            // Link-local IPv6 addresses need a zone id to be usable; skip them.
            IpAddr::V6(v6) => !v6.is_unicast_link_local(),
        })
        .collect();
    addresses.sort_by_key(|ip| (ip.is_ipv6(), *ip));
    addresses.dedup();
    addresses
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn addresses_accept_common_forms() {
        assert_eq!(
            resolve_address("192.168.0.10").await.unwrap(),
            "192.168.0.10:47821".parse().unwrap()
        );
        assert_eq!(
            resolve_address(" 192.168.0.10:5000 ").await.unwrap(),
            "192.168.0.10:5000".parse().unwrap()
        );
        assert_eq!(
            resolve_address("[::1]").await.unwrap(),
            "[::1]:47821".parse().unwrap()
        );
        assert_eq!(resolve_address("localhost:9").await.unwrap().port(), 9);
        assert!(resolve_address("").await.is_err());
    }

    #[test]
    fn auto_frame_rate_follows_the_display_up_to_the_highest_choice() {
        assert_eq!(auto_frame_rate_for(0), FALLBACK_FRAME_RATE);
        assert_eq!(auto_frame_rate_for(60), 60);
        assert_eq!(auto_frame_rate_for(120), 120);
        assert_eq!(auto_frame_rate_for(240), 144);
        assert_eq!(auto_frame_rate_for(u32::MAX), 144);
    }

    #[test]
    fn device_name_fits_the_protocol() {
        let name = device_name();
        assert!(name.chars().count() <= MAX_DEVICE_NAME_CHARS);
        assert!(!name.chars().any(char::is_control));
    }
}
