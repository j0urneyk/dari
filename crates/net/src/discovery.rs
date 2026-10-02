//! Finding hosts on the local network with mDNS/DNS-SD.
//!
//! Advertisements are unauthenticated: anyone on the network can announce anything. They only
//! help fill in an address; the SPAKE2 handshake still decides who is who.

use std::collections::HashMap;
use std::net::IpAddr;

use dari_proto::{MAX_DEVICE_NAME_CHARS, Os, sanitize_display_text};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use thiserror::Error;

use crate::identity::Fingerprint;

const SERVICE_TYPE: &str = "_dari._udp.local.";
/// Addresses kept per discovered device.
const MAX_ADDRESSES: usize = 8;
/// Devices kept by one browser; the network is small, anything beyond this is noise.
const MAX_DEVICES: usize = 64;

#[derive(Debug, Error)]
#[error("local network discovery failed: {0}")]
pub struct DiscoveryError(#[from] mdns_sd::Error);

/// Announces this host on the local network until dropped.
pub struct Advertisement {
    daemon: ServiceDaemon,
    fullname: String,
}

impl std::fmt::Debug for Advertisement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Advertisement")
            .field("fullname", &self.fullname)
            .finish_non_exhaustive()
    }
}

impl Advertisement {
    pub fn start(name: &str, port: u16, fingerprint: &Fingerprint) -> Result<Self, DiscoveryError> {
        let daemon = ServiceDaemon::new()?;
        // The instance name must be unique on the network; the fingerprint prefix makes it so
        // even when two devices share a name.
        let fingerprint_hex = fingerprint_hint(fingerprint);
        let display_name = sanitize_display_text(name, MAX_DEVICE_NAME_CHARS);
        let instance = instance_name(&display_name, &fingerprint_hex);
        let host_name = format!("dari-{fingerprint_hex}.local.");
        let properties = [
            ("name", display_name.as_str()),
            ("os", os_tag(Os::current())),
            ("fp", fingerprint_hex.as_str()),
        ];
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            &instance,
            &host_name,
            "",
            port,
            &properties[..],
        )?
        .enable_addr_auto();
        let fullname = info.get_fullname().to_owned();
        daemon.register(info)?;
        Ok(Self { daemon, fullname })
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        let _unregistered = self.daemon.unregister(&self.fullname);
        let _stopped = self.daemon.shutdown();
    }
}

/// DNS labels hold at most this many bytes.
const MAX_LABEL_BYTES: usize = 63;

/// `"{name} {hint}"`, with the name shortened on a character boundary so the whole label fits
/// in a DNS label. The hint stays intact to keep instance names unique.
fn instance_name(name: &str, hint: &str) -> String {
    let budget = MAX_LABEL_BYTES.saturating_sub(hint.len() + 1);
    let mut end = 0;
    for (index, character) in name.char_indices() {
        if index + character.len_utf8() > budget {
            break;
        }
        end = index + character.len_utf8();
    }
    format!("{} {hint}", &name[..end])
}

/// A host seen on the local network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NearbyDevice {
    /// Stable key for this advertisement.
    pub id: String,
    pub name: String,
    pub os: Option<Os>,
    pub addresses: Vec<IpAddr>,
    pub port: u16,
    /// First bytes of the advertised certificate fingerprint, as hex. Unauthenticated; only
    /// useful for recognizing this device's own advertisement.
    pub fingerprint_hint: String,
}

/// The hint [`Advertisement`] publishes for `fingerprint`.
pub fn fingerprint_hint(fingerprint: &Fingerprint) -> String {
    use std::fmt::Write as _;
    fingerprint.as_bytes()[..4]
        .iter()
        .fold(String::with_capacity(8), |mut hex, byte| {
            let _written = write!(hex, "{byte:02x}");
            hex
        })
}

/// What changed on the local network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryEvent {
    Found(NearbyDevice),
    Lost { id: String },
}

/// Watches the local network for hosts until dropped.
pub struct Browser {
    daemon: ServiceDaemon,
    events: mdns_sd::Receiver<ServiceEvent>,
    known: HashMap<String, NearbyDevice>,
    /// Loopback addresses are useless to other devices; tests on one machine keep them.
    include_loopback: bool,
}

impl std::fmt::Debug for Browser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Browser")
            .field("known", &self.known.len())
            .finish_non_exhaustive()
    }
}

impl Browser {
    pub fn start() -> Result<Self, DiscoveryError> {
        Self::start_with(false)
    }

    fn start_with(include_loopback: bool) -> Result<Self, DiscoveryError> {
        let daemon = ServiceDaemon::new()?;
        let events = daemon.browse(SERVICE_TYPE)?;
        Ok(Self {
            daemon,
            events,
            known: HashMap::new(),
            include_loopback,
        })
    }

    /// The next change, or `None` once the browser stopped.
    pub async fn next(&mut self) -> Option<DiscoveryEvent> {
        loop {
            match self.events.recv_async().await.ok()? {
                ServiceEvent::ServiceResolved(service) => {
                    let id = service.get_fullname().to_owned();
                    if !self.known.contains_key(&id) && self.known.len() >= MAX_DEVICES {
                        continue;
                    }
                    let advertised = service.get_property_val_str("name").unwrap_or_default();
                    let name = sanitize_display_text(advertised, MAX_DEVICE_NAME_CHARS);
                    let mut addresses: Vec<IpAddr> = service
                        .get_addresses()
                        .iter()
                        .map(mdns_sd::ScopedIp::to_ip_addr)
                        .filter(|address| {
                            (self.include_loopback || !address.is_loopback())
                                && match address {
                                    IpAddr::V4(v4) => !v4.is_link_local(),
                                    IpAddr::V6(v6) => !v6.is_unicast_link_local(),
                                }
                        })
                        .collect();
                    addresses.sort_by_key(|address| (address.is_ipv6(), *address));
                    addresses.truncate(MAX_ADDRESSES);
                    if name.trim().is_empty() || addresses.is_empty() || service.get_port() == 0 {
                        continue;
                    }
                    let device = NearbyDevice {
                        id: id.clone(),
                        name,
                        os: service.get_property_val_str("os").and_then(parse_os_tag),
                        addresses,
                        port: service.get_port(),
                        fingerprint_hint: service
                            .get_property_val_str("fp")
                            .unwrap_or_default()
                            .chars()
                            .filter(char::is_ascii_hexdigit)
                            .take(8)
                            .collect(),
                    };
                    if self.known.get(&id) == Some(&device) {
                        continue;
                    }
                    self.known.insert(id, device.clone());
                    return Some(DiscoveryEvent::Found(device));
                }
                ServiceEvent::ServiceRemoved(_, id) if self.known.remove(&id).is_some() => {
                    return Some(DiscoveryEvent::Lost { id });
                }
                _ => {}
            }
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _stopped = self.daemon.stop_browse(SERVICE_TYPE);
        let _shut_down = self.daemon.shutdown();
    }
}

fn os_tag(os: Os) -> &'static str {
    match os {
        Os::MacOs => "macos",
        Os::Windows => "windows",
        Os::Linux => "linux",
        Os::Other => "other",
    }
}

fn parse_os_tag(tag: &str) -> Option<Os> {
    Some(match tag {
        "macos" => Os::MacOs,
        "windows" => Os::Windows,
        "linux" => Os::Linux,
        "other" => Os::Other,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_names_fit_a_dns_label() {
        assert_eq!(instance_name("studio", "0a1b2c3d"), "studio 0a1b2c3d");
        let korean = instance_name(&"맥".repeat(40), "0a1b2c3d");
        assert!(korean.len() <= MAX_LABEL_BYTES, "{} bytes", korean.len());
        assert!(korean.ends_with(" 0a1b2c3d"));
        let ascii = instance_name(&"x".repeat(64), "0a1b2c3d");
        assert_eq!(ascii.len(), MAX_LABEL_BYTES);
    }

    #[test]
    fn os_tags_round_trip() {
        for os in [Os::MacOs, Os::Windows, Os::Linux, Os::Other] {
            assert_eq!(parse_os_tag(os_tag(os)), Some(os));
        }
        assert_eq!(parse_os_tag("plan9"), None);
    }
}

#[cfg(test)]
mod network_tests {
    use std::time::Duration;

    use super::*;
    use crate::identity::DeviceIdentity;

    #[tokio::test]
    #[ignore = "needs multicast on the local network; run with --ignored"]
    async fn advertised_hosts_are_discovered() {
        let identity = DeviceIdentity::generate().unwrap();
        let name = format!("discovery-test-{}", std::process::id());
        let _advertisement = Advertisement::start(&name, 47_999, &identity.fingerprint()).unwrap();
        let mut browser = Browser::start_with(true).unwrap();
        let found = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(DiscoveryEvent::Found(device)) = browser.next().await
                    && device.name == name
                {
                    return device;
                }
            }
        })
        .await
        .expect("the advertisement was not discovered");
        assert_eq!(found.port, 47_999);
        assert_eq!(found.os, Some(Os::current()));
        assert_ne!(found.addresses, Vec::<IpAddr>::new());
    }
}
