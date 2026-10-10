use std::time::{Duration, Instant};

use dari_media::{CaptureError, CapturedFrame, RgbaFrame, ScreenCapturer};

use super::{Route, SecureDesktopView, Wait};

type OpenDefault = Box<dyn FnMut() -> Result<Box<dyn ScreenCapturer>, CaptureError>>;

/// Reads the helper's frames of one desktop epoch from a link. Frames of any other epoch, such
/// as the previous visit to the secure desktop, are never returned.
pub(crate) struct SecureDesktopCapturer {
    view: SecureDesktopView,
    epoch: u64,
    last: Option<u64>,
}

impl SecureDesktopCapturer {
    pub(crate) fn new(view: SecureDesktopView, epoch: u64) -> Self {
        Self {
            view,
            epoch,
            last: None,
        }
    }
}

impl ScreenCapturer for SecureDesktopCapturer {
    fn capture(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
        match self.view.wait_frame(self.epoch, self.last, timeout) {
            Wait::Frame { number, image } => {
                self.last = Some(number);
                Ok(Some(CapturedFrame::Rgba(RgbaFrame::clone(&image))))
            }
            Wait::Nothing | Wait::Moved => Ok(None),
            Wait::Unavailable => Err(CaptureError::SecureDesktop),
        }
    }

    fn paces_itself(&self) -> bool {
        true
    }
}

/// The capturer a session runs while it holds a helper link: the platform's capturer on the
/// user's desktop, and the helper's frames on any other desktop while the helper is connected.
/// A switch changes only where frames come from, so the stream keeps its encoder, and the
/// first frame from the new source asks for a keyframe.
pub(crate) struct TwoSourceCapturer {
    open_default: OpenDefault,
    view: SecureDesktopView,
    epoch: u64,
    source: Source,
    max_fps: u32,
    switched: bool,
    keyframe_requested: bool,
}

enum Source {
    /// Opened at the first capture on the user's desktop, and again after every switch back,
    /// so a frame Windows.Graphics.Capture queued before the switch (the dimmed desktop behind
    /// a prompt) is never shown.
    Default(Option<Box<dyn ScreenCapturer>>),
    Secure(SecureDesktopCapturer),
}

impl Source {
    fn for_route(route: Route, view: &SecureDesktopView, epoch: u64) -> Self {
        match route {
            Route::Default { .. } => Source::Default(None),
            Route::Secure => Source::Secure(SecureDesktopCapturer::new(view.clone(), epoch)),
        }
    }
}

impl TwoSourceCapturer {
    pub(crate) fn new(
        open_default: impl FnMut() -> Result<Box<dyn ScreenCapturer>, CaptureError> + 'static,
        view: SecureDesktopView,
        max_fps: u32,
    ) -> Self {
        let (epoch, route) = view.route();
        Self {
            open_default: Box::new(open_default),
            source: Source::for_route(route, &view, epoch),
            view,
            epoch,
            max_fps,
            switched: false,
            keyframe_requested: false,
        }
    }
}

impl ScreenCapturer for TwoSourceCapturer {
    fn capture(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
        let (epoch, route) = self.view.route();
        if epoch != self.epoch {
            self.epoch = epoch;
            self.source = Source::for_route(route, &self.view, epoch);
            self.switched = true;
        }
        let captured = match &mut self.source {
            Source::Secure(source) => source.capture(timeout),
            Source::Default(slot) => {
                let source = match slot {
                    Some(source) => source,
                    None => slot.insert(paced((self.open_default)()?, self.max_fps)),
                };
                match source.capture(timeout) {
                    // The helper reports every switch, and the source's own check can lag it
                    // either way; the screen must stay available across switches.
                    Err(CaptureError::SecureDesktop)
                        if route == (Route::Default { helper_live: true }) =>
                    {
                        Ok(None)
                    }
                    other => other,
                }
            }
        };
        if matches!(captured, Ok(Some(_))) && std::mem::take(&mut self.switched) {
            self.keyframe_requested = true;
        }
        captured
    }

    fn paces_itself(&self) -> bool {
        true
    }

    fn take_keyframe_request(&mut self) -> bool {
        std::mem::take(&mut self.keyframe_requested)
    }
}

fn paced(source: Box<dyn ScreenCapturer>, max_fps: u32) -> Box<dyn ScreenCapturer> {
    if source.paces_itself() {
        return source;
    }
    Box::new(Paced {
        inner: source,
        interval: Duration::from_secs(1) / max_fps.max(1),
        next: Instant::now(),
    })
}

struct Paced {
    inner: Box<dyn ScreenCapturer>,
    interval: Duration,
    next: Instant,
}

impl ScreenCapturer for Paced {
    fn capture(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
        let now = Instant::now();
        if now < self.next {
            std::thread::sleep((self.next - now).min(timeout));
            if Instant::now() < self.next {
                return Ok(None);
            }
        }
        self.next = (self.next + self.interval).max(Instant::now());
        self.inner.capture(timeout)
    }

    fn paces_itself(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests may panic")]

    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use dari_proto::InputDesktop;

    use super::*;
    use crate::secure_desktop::{LinkDriver, SecureDesktopLink};

    const DISPLAY: u32 = 7;
    const WAIT: Duration = Duration::from_millis(10);
    const SECURE: u8 = 200;

    #[derive(Clone, Default)]
    struct DefaultScreen {
        opens: Arc<AtomicUsize>,
        hidden: Arc<AtomicBool>,
    }

    struct Opened {
        number: u8,
        hidden: Arc<AtomicBool>,
    }

    impl ScreenCapturer for Opened {
        fn capture(&mut self, _timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
            if self.hidden.load(Ordering::SeqCst) {
                return Err(CaptureError::SecureDesktop);
            }
            Ok(Some(image(self.number).into()))
        }

        fn paces_itself(&self) -> bool {
            true
        }
    }

    impl DefaultScreen {
        fn opens(&self) -> usize {
            self.opens.load(Ordering::SeqCst)
        }
    }

    fn image(red: u8) -> RgbaFrame {
        RgbaFrame::new(1, 1, vec![red, 0, 0, 255]).unwrap()
    }

    struct Setup {
        capturer: TwoSourceCapturer,
        screen: DefaultScreen,
        driver: LinkDriver,
        _link: SecureDesktopLink,
    }

    fn setup(desktop: Option<InputDesktop>) -> Setup {
        let (link, driver) = SecureDesktopLink::pair();
        link.select_display(DISPLAY);
        if let Some(desktop) = desktop {
            driver.desktop_changed(desktop);
        }
        let screen = DefaultScreen::default();
        let opener = screen.clone();
        let capturer = TwoSourceCapturer::new(
            move || {
                let number = opener.opens.fetch_add(1, Ordering::SeqCst) + 1;
                Ok(Box::new(Opened {
                    number: u8::try_from(number).unwrap(),
                    hidden: opener.hidden.clone(),
                }) as Box<dyn ScreenCapturer>)
            },
            link.view(),
            30,
        );
        Setup {
            capturer,
            screen,
            driver,
            _link: link,
        }
    }

    fn shown(capturer: &mut TwoSourceCapturer) -> Result<Option<u8>, CaptureError> {
        Ok(capturer.capture(WAIT)?.map(|frame| match frame {
            CapturedFrame::Rgba(frame) => frame.pixels()[0],
            #[cfg(any(target_os = "macos", windows))]
            CapturedFrame::Native(_) => panic!("the test sources make RGBA frames"),
        }))
    }

    #[test]
    fn the_first_frame_after_each_switch_asks_for_a_keyframe_and_no_other_does() {
        let Setup {
            mut capturer,
            driver,
            ..
        } = setup(Some(InputDesktop::Default));
        let mut keyframes = Vec::new();
        let mut capture = |capturer: &mut TwoSourceCapturer| {
            let shown = shown(capturer).unwrap();
            keyframes.push(capturer.take_keyframe_request());
            shown
        };

        assert_eq!(capture(&mut capturer), Some(1));
        assert_eq!(capture(&mut capturer), Some(1));
        driver.desktop_changed(InputDesktop::Winlogon);
        assert_eq!(capture(&mut capturer), None);
        driver.frame(DISPLAY, image(SECURE));
        assert_eq!(capture(&mut capturer), Some(SECURE));
        driver.frame(DISPLAY, image(SECURE + 1));
        assert_eq!(capture(&mut capturer), Some(SECURE + 1));
        driver.desktop_changed(InputDesktop::Default);
        assert_eq!(capture(&mut capturer), Some(2));
        assert_eq!(capture(&mut capturer), Some(2));

        assert_eq!(keyframes, [false, false, false, true, false, true, false]);
        assert!(!capturer.take_keyframe_request());
    }

    #[test]
    fn a_frame_from_before_a_switch_is_never_returned() {
        let Setup {
            mut capturer,
            driver,
            ..
        } = setup(Some(InputDesktop::Winlogon));
        driver.frame(DISPLAY, image(SECURE));
        assert_eq!(shown(&mut capturer).unwrap(), Some(SECURE));
        driver.desktop_changed(InputDesktop::Default);
        assert_eq!(shown(&mut capturer).unwrap(), Some(1));
        driver.desktop_changed(InputDesktop::Winlogon);
        assert_eq!(shown(&mut capturer).unwrap(), None);
        driver.frame(DISPLAY, image(SECURE + 1));
        assert_eq!(shown(&mut capturer).unwrap(), Some(SECURE + 1));
        assert_eq!(shown(&mut capturer).unwrap(), None);
    }

    #[test]
    fn a_switch_seen_late_still_reopens_the_default_source() {
        let Setup {
            mut capturer,
            driver,
            screen,
            ..
        } = setup(Some(InputDesktop::Default));
        assert_eq!(shown(&mut capturer).unwrap(), Some(1));
        driver.desktop_changed(InputDesktop::Winlogon);
        driver.desktop_changed(InputDesktop::Default);
        assert_eq!(shown(&mut capturer).unwrap(), Some(2));
        assert!(capturer.take_keyframe_request());
        assert_eq!(screen.opens(), 2);
    }

    #[test]
    fn the_default_source_opens_only_on_the_users_desktop_and_reopens_after_the_secure_one() {
        let Setup {
            mut capturer,
            driver,
            screen,
            ..
        } = setup(Some(InputDesktop::Winlogon));
        assert_eq!(shown(&mut capturer).unwrap(), None);
        assert_eq!(screen.opens(), 0);
        driver.desktop_changed(InputDesktop::Default);
        assert_eq!(shown(&mut capturer).unwrap(), Some(1));
        driver.desktop_changed(InputDesktop::Winlogon);
        assert_eq!(shown(&mut capturer).unwrap(), None);
        assert_eq!(screen.opens(), 1);
        driver.desktop_changed(InputDesktop::Default);
        assert_eq!(shown(&mut capturer).unwrap(), Some(2));
    }

    #[test]
    fn the_default_sources_secure_desktop_error_is_hidden_only_while_the_helper_is_connected() {
        let Setup {
            mut capturer,
            driver,
            screen,
            ..
        } = setup(None);
        screen.hidden.store(true, Ordering::SeqCst);
        assert!(matches!(
            shown(&mut capturer),
            Err(CaptureError::SecureDesktop)
        ));
        driver.desktop_changed(InputDesktop::Default);
        assert_eq!(shown(&mut capturer).unwrap(), None);
        drop(driver);
        assert!(matches!(
            shown(&mut capturer),
            Err(CaptureError::SecureDesktop)
        ));
        screen.hidden.store(false, Ordering::SeqCst);
        assert_eq!(shown(&mut capturer).unwrap(), Some(1));
    }

    #[test]
    fn an_unavailable_screen_shows_the_notice_until_the_next_frame() {
        let Setup {
            mut capturer,
            driver,
            ..
        } = setup(Some(InputDesktop::Winlogon));
        driver.frame(DISPLAY, image(SECURE));
        assert_eq!(shown(&mut capturer).unwrap(), Some(SECURE));
        driver.screen_unavailable(DISPLAY);
        assert!(matches!(
            shown(&mut capturer),
            Err(CaptureError::SecureDesktop)
        ));
        assert!(matches!(
            shown(&mut capturer),
            Err(CaptureError::SecureDesktop)
        ));
        driver.frame(DISPLAY, image(SECURE + 1));
        assert_eq!(shown(&mut capturer).unwrap(), Some(SECURE + 1));
    }

    #[test]
    fn a_helper_that_ends_on_the_secure_desktop_falls_back_to_the_default_source() {
        let Setup {
            mut capturer,
            driver,
            screen,
            ..
        } = setup(Some(InputDesktop::Winlogon));
        driver.frame(DISPLAY, image(SECURE));
        assert_eq!(shown(&mut capturer).unwrap(), Some(SECURE));
        capturer.take_keyframe_request();
        screen.hidden.store(true, Ordering::SeqCst);
        driver.end("the helper closed its pipe".into());
        assert!(matches!(
            shown(&mut capturer),
            Err(CaptureError::SecureDesktop)
        ));
        screen.hidden.store(false, Ordering::SeqCst);
        assert_eq!(shown(&mut capturer).unwrap(), Some(1));
        assert!(capturer.take_keyframe_request());
    }

    struct Polled(Arc<AtomicUsize>);

    impl ScreenCapturer for Polled {
        fn capture(&mut self, _timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Some(image(1).into()))
        }
    }

    #[test]
    fn a_polled_source_captures_no_faster_than_the_frame_rate() {
        let captures = Arc::new(AtomicUsize::new(0));
        let mut source = paced(Box::new(Polled(captures.clone())), 20);
        assert!(source.paces_itself());
        let started = Instant::now();
        let mut frames = 0;
        while frames < 3 {
            if source.capture(WAIT).unwrap().is_some() {
                frames += 1;
            }
        }
        assert!(started.elapsed() >= Duration::from_millis(100));
        assert_eq!(captures.load(Ordering::SeqCst), 3);
    }
}
