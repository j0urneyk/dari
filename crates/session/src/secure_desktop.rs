mod capturer;
#[cfg(windows)]
#[allow(unsafe_code)]
mod win;

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use dari_media::RgbaFrame;
use dari_proto::InputDesktop;
use tokio::sync::mpsc;

pub(crate) use capturer::TwoSourceCapturer;
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

/// What the link does on the session's behalf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkCommand {
    /// Capture the display with this ID on the secure desktop.
    SelectDisplay(u32),
}

/// A live link to the helper, which lives as long as the session holds it. Dropping it closes
/// the helper's pipe, and the helper exits.
#[derive(Debug)]
pub struct SecureDesktopLink {
    events: mpsc::UnboundedReceiver<SecureDesktopEvent>,
    commands: mpsc::UnboundedSender<LinkCommand>,
    shared: Arc<Shared>,
}

impl SecureDesktopLink {
    /// A link and the driver that runs it: the pipe task keeps the driver and plays what the
    /// helper says into it, and a test can play the helper the same way.
    pub fn pair() -> (SecureDesktopLink, LinkDriver) {
        let (event_sender, events) = mpsc::unbounded_channel();
        let (commands, command_receiver) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared::default());
        let link = SecureDesktopLink {
            events,
            commands,
            shared: shared.clone(),
        };
        let driver = LinkDriver {
            events: event_sender,
            commands: command_receiver,
            shared,
        };
        (link, driver)
    }

    /// The next event, or `None` if the link stopped without saying why.
    pub(crate) async fn next(&mut self) -> Option<SecureDesktopEvent> {
        self.events.recv().await
    }

    /// Captures `display` from now on. The shared state changes before this returns, so a
    /// capture thread started after it never sees a frame of the previous display.
    pub(crate) fn select_display(&self, display: u32) {
        self.shared.update(|state| state.select(display));
        let _sent = self.commands.send(LinkCommand::SelectDisplay(display));
    }

    /// What a capture thread reads the helper's frames through.
    pub(crate) fn view(&self) -> SecureDesktopView {
        SecureDesktopView(self.shared.clone())
    }
}

/// The helper's end of a link, and the only writer of its state. Dropping it ends the link
/// without a reason, so a capturer never waits on a helper that is gone.
#[derive(Debug)]
pub struct LinkDriver {
    events: mpsc::UnboundedSender<SecureDesktopEvent>,
    commands: mpsc::UnboundedReceiver<LinkCommand>,
    shared: Arc<Shared>,
}

impl LinkDriver {
    pub fn desktop_changed(&self, desktop: InputDesktop) {
        self.shared
            .update(|state| state.desktop_changed(desktop.clone()));
        let _sent = self
            .events
            .send(SecureDesktopEvent::DesktopChanged(desktop));
    }

    /// Whether a frame of `display` would be kept now, so the link can skip copying one it
    /// would drop.
    pub fn wants(&self, display: u32) -> bool {
        self.shared.lock().accepts(display)
    }

    /// The helper published `image` of `display`, captured on the current desktop.
    pub fn frame(&self, display: u32, image: RgbaFrame) {
        self.shared.update(|state| state.frame(display, image));
    }

    /// The helper can't capture `display` right now.
    pub fn screen_unavailable(&self, display: u32) {
        self.shared.update(|state| state.unavailable(display));
    }

    /// The display the session selected last, for a helper that just connected.
    pub fn selected_display(&self) -> Option<u32> {
        self.shared.lock().selected
    }

    /// The session's next command, or `None` once the session dropped the link.
    pub async fn command(&mut self) -> Option<LinkCommand> {
        self.commands.recv().await
    }

    /// Ends the link and tells the session why.
    pub fn end(self, reason: String) {
        self.shared.update(LinkState::end);
        let _sent = self.events.send(SecureDesktopEvent::Ended(reason));
    }
}

impl Drop for LinkDriver {
    fn drop(&mut self) {
        self.shared.update(LinkState::end);
    }
}

/// A capture thread's read-only handle on a link. It outlives the link harmlessly: an ended
/// link routes to the user's desktop.
#[derive(Debug, Clone)]
pub(crate) struct SecureDesktopView(Arc<Shared>);

impl SecureDesktopView {
    /// Which source to capture from now, and the desktop epoch it belongs to.
    pub(crate) fn route(&self) -> (u64, Route) {
        let state = self.0.lock();
        (state.epoch, state.route())
    }

    /// Waits up to `timeout` for a frame of `epoch` other than frame `after`.
    pub(crate) fn wait_frame(&self, epoch: u64, after: Option<u64>, timeout: Duration) -> Wait {
        let deadline = Instant::now() + timeout;
        let mut state = self.0.lock();
        loop {
            if state.epoch != epoch {
                return Wait::Moved;
            }
            if let Picture::Frame { number, image } = &state.picture
                && Some(*number) != after
            {
                return Wait::Frame {
                    number: *number,
                    image: image.clone(),
                };
            }
            let now = Instant::now();
            if now >= deadline {
                return match state.picture {
                    Picture::Unavailable => Wait::Unavailable,
                    Picture::Waiting | Picture::Frame { .. } => Wait::Nothing,
                };
            }
            state = self
                .0
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    /// The user's desktop. `helper_live`: the helper is connected and reports every switch, so
    /// the default source's own secure-desktop check only lags it.
    Default { helper_live: bool },
    /// Any other desktop while the helper is connected: frames come from the helper.
    Secure,
}

#[derive(Debug, Clone)]
pub(crate) enum Wait {
    Frame {
        number: u64,
        image: Arc<RgbaFrame>,
    },
    Nothing,
    /// The helper can't capture the selected display.
    Unavailable,
    /// The desktop changed: ask for the route again.
    Moved,
}

#[derive(Debug, Default)]
struct Shared {
    state: Mutex<LinkState>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, LinkState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn update(&self, change: impl FnOnce(&mut LinkState)) {
        change(&mut self.lock());
        self.changed.notify_all();
    }
}

#[derive(Debug)]
struct LinkState {
    phase: Phase,
    desktop: InputDesktop,
    /// Bumped whenever `desktop` changes, and when the link ends off the user's desktop. A
    /// capturer switches source at every bump, even for Default, Winlogon, Default seen late as
    /// Default, Default: the default source may hold a frame from before the switch.
    epoch: u64,
    selected: Option<u32>,
    /// For the current epoch and selected display only.
    picture: Picture,
    next_number: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Waiting for the helper to name its first desktop.
    Connecting,
    Live,
    Ended,
}

#[derive(Debug)]
enum Picture {
    Waiting,
    Frame { number: u64, image: Arc<RgbaFrame> },
    Unavailable,
}

impl Default for LinkState {
    fn default() -> Self {
        Self {
            phase: Phase::Connecting,
            desktop: InputDesktop::Default,
            epoch: 0,
            selected: None,
            picture: Picture::Waiting,
            next_number: 0,
        }
    }
}

impl LinkState {
    fn desktop_changed(&mut self, desktop: InputDesktop) {
        if self.phase == Phase::Ended {
            return;
        }
        self.phase = Phase::Live;
        if desktop != self.desktop {
            self.desktop = desktop;
            self.epoch += 1;
            self.picture = Picture::Waiting;
        }
    }

    fn select(&mut self, display: u32) {
        if self.selected != Some(display) {
            self.selected = Some(display);
            self.picture = Picture::Waiting;
        }
    }

    fn accepts(&self, display: u32) -> bool {
        self.phase == Phase::Live
            && self.desktop != InputDesktop::Default
            && self.selected == Some(display)
    }

    fn frame(&mut self, display: u32, image: RgbaFrame) {
        if self.accepts(display) {
            self.picture = Picture::Frame {
                number: self.next_number,
                image: Arc::new(image),
            };
            self.next_number += 1;
        }
    }

    fn unavailable(&mut self, display: u32) {
        if self.accepts(display) {
            self.picture = Picture::Unavailable;
        }
    }

    fn end(&mut self) {
        if self.phase == Phase::Ended {
            return;
        }
        if self.desktop != InputDesktop::Default {
            self.epoch += 1;
        }
        self.phase = Phase::Ended;
        self.picture = Picture::Waiting;
    }

    fn route(&self) -> Route {
        match self.phase {
            Phase::Live if self.desktop != InputDesktop::Default => Route::Secure,
            Phase::Live => Route::Default { helper_live: true },
            Phase::Connecting | Phase::Ended => Route::Default { helper_live: false },
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests may panic")]

    use super::*;

    fn image(red: u8) -> RgbaFrame {
        RgbaFrame::new(2, 1, vec![red, 0, 0, 255, red, 0, 0, 255]).unwrap()
    }

    fn secure(display: u32) -> LinkState {
        let mut state = LinkState::default();
        state.select(display);
        state.desktop_changed(InputDesktop::Winlogon);
        state
    }

    fn shown(state: &LinkState) -> Option<u8> {
        match &state.picture {
            Picture::Frame { image, .. } => Some(image.pixels()[0]),
            Picture::Waiting | Picture::Unavailable => None,
        }
    }

    #[test]
    fn a_frame_of_the_selected_display_on_the_secure_desktop_is_kept() {
        let mut state = secure(7);
        state.frame(7, image(1));
        assert_eq!(shown(&state), Some(1));
        state.frame(7, image(2));
        assert_eq!(shown(&state), Some(2));
    }

    #[test]
    fn a_frame_of_another_display_is_dropped() {
        let mut state = secure(7);
        state.frame(8, image(1));
        assert_eq!(shown(&state), None);
        assert!(!state.accepts(8));
    }

    #[test]
    fn frames_before_the_helper_connects_or_on_the_users_desktop_are_dropped() {
        let mut state = LinkState::default();
        state.select(7);
        state.frame(7, image(1));
        assert_eq!(shown(&state), None);
        state.desktop_changed(InputDesktop::Default);
        state.frame(7, image(1));
        assert_eq!(shown(&state), None);
        assert!(!state.accepts(7));
    }

    #[test]
    fn selecting_another_display_clears_the_frame_and_selecting_the_same_one_does_not() {
        let mut state = secure(7);
        state.frame(7, image(1));
        state.select(7);
        assert_eq!(shown(&state), Some(1));
        state.select(8);
        assert_eq!(shown(&state), None);
        state.frame(7, image(2));
        assert_eq!(shown(&state), None);
    }

    #[test]
    fn a_desktop_change_clears_the_frame_and_bumps_the_epoch_and_the_same_desktop_does_not() {
        let mut state = secure(7);
        let epoch = state.epoch;
        state.frame(7, image(1));
        state.desktop_changed(InputDesktop::Winlogon);
        assert_eq!((state.epoch, shown(&state)), (epoch, Some(1)));
        state.desktop_changed(InputDesktop::from_name("Screen-saver"));
        assert_eq!((state.epoch, shown(&state)), (epoch + 1, None));
        state.desktop_changed(InputDesktop::Default);
        assert_eq!(state.epoch, epoch + 2);
        assert_eq!(state.route(), Route::Default { helper_live: true });
    }

    #[test]
    fn the_helper_starting_on_the_users_desktop_does_not_bump_the_epoch() {
        let mut state = LinkState::default();
        assert_eq!(state.route(), Route::Default { helper_live: false });
        state.desktop_changed(InputDesktop::Default);
        assert_eq!(state.epoch, 0);
        assert_eq!(state.route(), Route::Default { helper_live: true });
        state.desktop_changed(InputDesktop::Winlogon);
        assert_eq!((state.epoch, state.route()), (1, Route::Secure));
    }

    #[test]
    fn an_unavailable_screen_replaces_the_frame_until_the_next_one() {
        let mut state = secure(7);
        state.frame(7, image(1));
        state.unavailable(8);
        assert_eq!(shown(&state), Some(1));
        state.unavailable(7);
        assert!(matches!(state.picture, Picture::Unavailable));
        state.frame(7, image(2));
        assert_eq!(shown(&state), Some(2));
        state.unavailable(7);
        state.desktop_changed(InputDesktop::from_name("Screen-saver"));
        assert!(matches!(state.picture, Picture::Waiting));
    }

    #[test]
    fn ending_on_the_secure_desktop_bumps_the_epoch_and_routes_to_the_users_desktop() {
        let mut state = secure(7);
        state.frame(7, image(1));
        let epoch = state.epoch;
        state.end();
        assert_eq!(state.epoch, epoch + 1);
        assert_eq!(state.route(), Route::Default { helper_live: false });
        assert_eq!(shown(&state), None);
        state.end();
        assert_eq!(state.epoch, epoch + 1);
    }

    #[test]
    fn ending_on_the_users_desktop_keeps_the_epoch() {
        let mut state = LinkState::default();
        state.desktop_changed(InputDesktop::Default);
        state.end();
        assert_eq!(state.epoch, 0);
        assert_eq!(state.route(), Route::Default { helper_live: false });
    }

    #[test]
    fn an_ended_link_stays_ended() {
        let mut state = secure(7);
        state.end();
        let epoch = state.epoch;
        state.desktop_changed(InputDesktop::from_name("Screen-saver"));
        state.frame(7, image(1));
        assert_eq!(state.route(), Route::Default { helper_live: false });
        assert_eq!((state.epoch, shown(&state)), (epoch, None));
    }

    #[test]
    fn dropping_the_driver_ends_the_link_without_a_reason() {
        let (mut link, driver) = SecureDesktopLink::pair();
        let view = link.view();
        driver.desktop_changed(InputDesktop::Winlogon);
        assert_eq!(view.route(), (1, Route::Secure));
        drop(driver);
        assert_eq!(view.route(), (2, Route::Default { helper_live: false }));
        assert_eq!(
            link.events.try_recv().unwrap(),
            SecureDesktopEvent::DesktopChanged(InputDesktop::Winlogon)
        );
        assert!(link.events.try_recv().is_err());
    }

    #[test]
    fn ending_the_driver_tells_the_session_why() {
        let (mut link, driver) = SecureDesktopLink::pair();
        driver.end("gone".into());
        assert_eq!(
            link.events.try_recv().unwrap(),
            SecureDesktopEvent::Ended("gone".into())
        );
        assert_eq!(
            link.view().route(),
            (0, Route::Default { helper_live: false })
        );
    }

    #[tokio::test]
    async fn selecting_a_display_updates_the_state_and_tells_the_driver() {
        let (link, mut driver) = SecureDesktopLink::pair();
        assert_eq!(driver.selected_display(), None);
        link.select_display(8);
        assert_eq!(driver.selected_display(), Some(8));
        assert_eq!(driver.command().await, Some(LinkCommand::SelectDisplay(8)));
        drop(link);
        assert_eq!(driver.command().await, None);
    }

    #[test]
    fn a_waiting_view_wakes_for_a_frame_and_for_a_desktop_change() {
        let (link, driver) = SecureDesktopLink::pair();
        link.select_display(7);
        driver.desktop_changed(InputDesktop::Winlogon);
        let view = link.view();
        let waiting = std::thread::spawn(move || view.wait_frame(1, None, Duration::from_secs(10)));
        driver.frame(7, image(3));
        let Wait::Frame { number, image } = waiting.join().unwrap() else {
            panic!("the view didn't get the frame");
        };
        assert_eq!(image.pixels()[0], 3);

        let view = link.view();
        assert!(matches!(
            view.wait_frame(1, Some(number), Duration::from_millis(10)),
            Wait::Nothing
        ));
        let waiting =
            std::thread::spawn(move || view.wait_frame(1, Some(number), Duration::from_secs(10)));
        driver.desktop_changed(InputDesktop::Default);
        assert!(matches!(waiting.join().unwrap(), Wait::Moved));
    }

    #[test]
    fn an_unavailable_screen_is_reported_only_after_the_wait() {
        let (link, driver) = SecureDesktopLink::pair();
        link.select_display(7);
        driver.desktop_changed(InputDesktop::Winlogon);
        driver.screen_unavailable(7);
        let started = Instant::now();
        assert!(matches!(
            link.view().wait_frame(1, None, Duration::from_millis(30)),
            Wait::Unavailable
        ));
        // The stream captures again at once after this error, so returning early would spin.
        assert!(started.elapsed() >= Duration::from_millis(30));
    }
}
