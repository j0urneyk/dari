#[cfg(windows)]
#[allow(unsafe_code)]
mod win;

use dari_proto::InputDesktop;
use tokio::sync::mpsc;

#[cfg(windows)]
pub(crate) use win::open;

/// What the helper reports to the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecureDesktopEvent {
    /// The input desktop changed. The first event names the desktop the helper started on.
    DesktopChanged(InputDesktop),
    /// The link ended and won't report again this session: the service is missing or refused,
    /// the helper never connected or failed its check, or its pipe closed.
    Ended(String),
}

/// A live link to the helper, which lives as long as the session holds it. Dropping it closes
/// the helper's pipe, and the helper exits.
#[derive(Debug)]
pub struct SecureDesktopLink {
    events: mpsc::UnboundedReceiver<SecureDesktopEvent>,
}

impl SecureDesktopLink {
    /// A link that reports what is sent on the other end of `events`. Whatever runs the link
    /// learns that the session dropped it when `events` closes.
    pub fn new(events: mpsc::UnboundedReceiver<SecureDesktopEvent>) -> Self {
        Self { events }
    }

    /// The next event, or `None` if the link stopped without saying why.
    pub(crate) async fn next(&mut self) -> Option<SecureDesktopEvent> {
        self.events.recv().await
    }
}
