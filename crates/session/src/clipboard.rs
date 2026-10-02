//! Text clipboard synchronization between host and viewer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use dari_proto::MAX_CLIPBOARD_BYTES;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// How often the local clipboard is checked for changes.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Reads and writes the local clipboard's text.
pub trait ClipboardAccess: Send {
    fn read_text(&mut self) -> Option<String>;
    fn write_text(&mut self, text: &str) -> bool;
}

/// Creates clipboard access on the clipboard thread; `None` if there is no clipboard.
pub type ClipboardFactory = Arc<dyn Fn() -> Option<Box<dyn ClipboardAccess>> + Send + Sync>;

/// The OS clipboard.
pub struct SystemClipboard(arboard::Clipboard);

impl std::fmt::Debug for SystemClipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemClipboard").finish_non_exhaustive()
    }
}

impl SystemClipboard {
    pub fn open() -> Option<Self> {
        match arboard::Clipboard::new() {
            Ok(clipboard) => Some(Self(clipboard)),
            Err(error) => {
                warn!(%error, "clipboard unavailable");
                None
            }
        }
    }

    /// A factory for [`ClipboardFactory`] slots.
    pub fn factory() -> ClipboardFactory {
        Arc::new(|| Self::open().map(|clipboard| Box::new(clipboard) as Box<dyn ClipboardAccess>))
    }
}

impl ClipboardAccess for SystemClipboard {
    fn read_text(&mut self) -> Option<String> {
        self.0.get_text().ok()
    }

    fn write_text(&mut self, text: &str) -> bool {
        self.0.set_text(text).is_ok()
    }
}

/// A running clipboard sync. Dropping it stops the thread.
pub(crate) struct ClipboardSync {
    incoming: std::sync::mpsc::Sender<String>,
    stop: Arc<AtomicBool>,
}

impl ClipboardSync {
    /// Starts watching the local clipboard on its own thread. Local changes go to `outgoing`;
    /// text that already was on the clipboard when the session started is never sent.
    pub(crate) fn start(factory: ClipboardFactory, outgoing: mpsc::Sender<String>) -> Option<Self> {
        let (incoming, remote_changes) = std::sync::mpsc::channel::<String>();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let spawned = std::thread::Builder::new()
            .name("dari-clipboard".into())
            .spawn(move || {
                let Some(mut clipboard) = factory() else {
                    return;
                };
                let mut last = clipboard.read_text();
                while !thread_stop.load(Ordering::Acquire) {
                    for text in remote_changes.try_iter() {
                        if clipboard.write_text(&text) {
                            // Remember it so the poll below does not echo it back.
                            last = Some(text);
                        }
                    }
                    if let Some(text) = clipboard.read_text()
                        && last.as_ref() != Some(&text)
                    {
                        if text.len() <= MAX_CLIPBOARD_BYTES && !text.contains('\0') {
                            if outgoing.try_send(text.clone()).is_err() {
                                debug!("clipboard update dropped");
                            }
                        } else {
                            debug!("local clipboard text is too large to share");
                        }
                        last = Some(text);
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
            });
        match spawned {
            Ok(_thread) => Some(Self { incoming, stop }),
            Err(error) => {
                warn!(%error, "cannot start clipboard sync");
                None
            }
        }
    }

    /// Puts text from the peer on the local clipboard.
    pub(crate) fn apply_remote(&self, text: String) {
        let _sent = self.incoming.send(text);
    }
}

impl Drop for ClipboardSync {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use super::*;

    /// A clipboard shared between test threads.
    #[derive(Clone, Default)]
    pub(crate) struct MemoryClipboard(pub(crate) Arc<Mutex<Option<String>>>);

    impl ClipboardAccess for MemoryClipboard {
        fn read_text(&mut self) -> Option<String> {
            self.0.lock().ok()?.clone()
        }

        fn write_text(&mut self, text: &str) -> bool {
            self.0
                .lock()
                .map(|mut slot| *slot = Some(text.to_owned()))
                .is_ok()
        }
    }

    impl MemoryClipboard {
        pub(crate) fn factory(&self) -> ClipboardFactory {
            let clipboard = self.clone();
            Arc::new(move || Some(Box::new(clipboard.clone()) as Box<dyn ClipboardAccess>))
        }

        pub(crate) fn set(&self, text: &str) {
            *self.0.lock().unwrap() = Some(text.into());
        }

        pub(crate) fn get(&self) -> Option<String> {
            self.0.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn local_changes_are_sent_but_not_the_initial_content() {
        let clipboard = MemoryClipboard::default();
        clipboard.set("secret already on the clipboard");
        let (outgoing, mut changes) = mpsc::channel(4);
        let _sync = ClipboardSync::start(clipboard.factory(), outgoing).unwrap();
        tokio::time::sleep(POLL_INTERVAL * 2).await;
        assert!(changes.try_recv().is_err());
        clipboard.set("copied text");
        let sent = tokio::time::timeout(Duration::from_secs(2), changes.recv())
            .await
            .unwrap();
        assert_eq!(sent.as_deref(), Some("copied text"));
    }

    #[tokio::test]
    async fn remote_text_is_applied_without_echo() {
        let clipboard = MemoryClipboard::default();
        let (outgoing, mut changes) = mpsc::channel(4);
        let sync = ClipboardSync::start(clipboard.factory(), outgoing).unwrap();
        sync.apply_remote("from the other side".into());
        tokio::time::sleep(POLL_INTERVAL * 3).await;
        assert_eq!(clipboard.get().as_deref(), Some("from the other side"));
        assert!(
            changes.try_recv().is_err(),
            "remote text must not be sent back"
        );
    }
}
