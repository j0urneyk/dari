//! Follows the input desktop and keeps a duplication of the selected display on any desktop but
//! `Default`. Every decision is here, in pure code: `win32::DxgiWorld` only makes the calls.

use std::fmt;
use std::time::{Duration, Instant};

use dari_proto::{FrameLayout, InputDesktop};

use crate::dxgi_result::{
    DuplicateFailure, FrameFailure, Hresult, duplicate_failure, frame_failure,
};
use crate::pointer::{Pointer, PointerPosition, PointerShape, RawPointerShape};
use crate::tracker::{DesktopSource, DesktopTracker, Observation};

pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Longest single wait in `AcquireNextFrame`, so commands from the app (a returned credit, a new
/// display) are picked up within one slice even on a still screen.
pub(crate) const ACQUIRE_SLICE: Duration = Duration::from_millis(33);
pub(crate) const DENIED_LIMIT: Duration = Duration::from_secs(5);
/// A duplication that shows no image this long is replaced: a still screen whose first frame was
/// pointer-only would otherwise stay blank.
pub(crate) const NO_IMAGE_LIMIT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Size {
    pub(crate) width: u32,
    pub(crate) height: u32,
}

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

/// The OS as the screen thread sees it. Its methods mirror the Win32 and DXGI calls one to one.
pub(crate) trait DesktopWorld: DesktopSource {
    type Duplication: Duplication;
    /// Attaches this thread to the current input desktop and names the desktop it attached to,
    /// which may differ from the last poll if a switch happened in between.
    fn attach(&mut self) -> Result<Observation, Hresult>;
    /// Finds the output whose `HMONITOR`'s low 32 bits are `display` on any adapter, creates a
    /// Direct3D 11 device on that adapter, and duplicates the output.
    fn duplicate(&mut self, display: u32) -> Result<Self::Duplication, Duplicate>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Duplicate {
    NoSuchDisplay,
    Failed(Hresult),
}

/// One `IDXGIOutputDuplication` with its device.
pub(crate) trait Duplication {
    fn size(&self) -> Size;
    fn acquire(&mut self, timeout: Duration) -> Result<AcquiredFrame, Hresult>;
    /// Copies the acquired image into `into`, tightly packed RGBA of `size()`.
    fn copy_image(&mut self, into: &mut [u8]) -> Result<(), Hresult>;
    /// `GetFramePointerShape`, called only when the acquired frame says the shape changed.
    fn pointer_shape(&mut self) -> Result<RawPointerShape, Hresult>;
    fn release(&mut self) -> Result<(), Hresult>;
}

/// The parts of `DXGI_OUTDUPL_FRAME_INFO` the machine uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AcquiredFrame {
    /// 0 for a pointer-only update, whose image may be black (Phase 0).
    pub(crate) accumulated_frames: u32,
    pub(crate) last_present_time: i64,
    /// `Some` when `LastMouseUpdateTime` isn't 0.
    pub(crate) pointer: Option<PointerPosition>,
    /// `PointerShapeBufferSize` isn't 0.
    pub(crate) shape_changed: bool,
}

/// What the screen thread passes on, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScreenEvent {
    DesktopChanged(InputDesktop),
    Unavailable {
        display: u32,
    },
    /// For the event log only.
    Note(Note),
}

/// A line for the event log. A duplication logs its start, its first image, and its end with a
/// summary, never each frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Note {
    Duplicated {
        display: u32,
        desktop: InputDesktop,
        size: Size,
    },
    FirstImage {
        accumulated_frames: u32,
        last_present_time: i64,
        /// Pointer-only frames before it, each skipped.
        skipped: u32,
        after: Duration,
    },
    Ended {
        why: Ended,
        stats: Stats,
        lasted: Duration,
    },
    /// The first failure, and each change of code, while trying to duplicate.
    CannotDuplicate {
        display: u32,
        code: Hresult,
    },
    Unavailable {
        display: u32,
        why: Unavailable,
    },
    /// A pointer shape that didn't parse; the previous one stays.
    BadPointerShape {
        kind: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ended {
    DesktopChanged,
    DisplaySelected,
    Failed(Hresult),
    NoImage,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Stats {
    pub(crate) images: u32,
    pub(crate) pointer_only: u32,
    /// Frames handed to the app's channel, published or drafted.
    pub(crate) offered: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unavailable {
    NoSuchDisplay,
    Unsupported(Hresult),
    /// Still failing after `DENIED_LIMIT`, with the last code if there was one.
    StillFailing(Option<Hresult>),
    TooLarge(Size),
}

impl fmt::Display for Note {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duplicated {
                display,
                desktop,
                size,
            } => write!(f, "duplicating display {display} on {desktop}, {size}"),
            Self::FirstImage {
                accumulated_frames,
                last_present_time,
                skipped,
                after,
            } => write!(
                f,
                "first image after {after:?}: AccumulatedFrames {accumulated_frames}, \
                 LastPresentTime {last_present_time}, {skipped} pointer-only frames skipped"
            ),
            Self::Ended { why, stats, lasted } => {
                let why = match why {
                    Ended::DesktopChanged => "the desktop changed".to_owned(),
                    Ended::DisplaySelected => "another display was selected".to_owned(),
                    Ended::Failed(code) => format!("DXGI failed with {code}"),
                    Ended::NoImage => format!("no image within {NO_IMAGE_LIMIT:?}"),
                };
                write!(
                    f,
                    "duplication ended after {lasted:?}, {why}: {} images, {} pointer-only \
                     frames, {} offered to the app",
                    stats.images, stats.pointer_only, stats.offered
                )
            }
            Self::CannotDuplicate { display, code } => {
                write!(f, "cannot duplicate display {display} yet: {code}")
            }
            Self::Unavailable { display, why } => {
                write!(f, "display {display} is unavailable: ")?;
                match why {
                    Unavailable::NoSuchDisplay => f.write_str("no output matches it"),
                    Unavailable::Unsupported(code) => {
                        write!(f, "DuplicateOutput failed with {code}")
                    }
                    Unavailable::StillFailing(Some(code)) => {
                        write!(f, "still failing after {DENIED_LIMIT:?} with {code}")
                    }
                    Unavailable::StillFailing(None) => {
                        write!(
                            f,
                            "the desktop still can't be attached after {DENIED_LIMIT:?}"
                        )
                    }
                    Unavailable::TooLarge(size) => write!(f, "{size} is too large"),
                }
            }
            Self::BadPointerShape { kind } => write!(f, "ignored a pointer shape of type {kind}"),
        }
    }
}

#[derive(Debug)]
enum Capture<D> {
    /// `Default`, the desktop not read yet, or no display selected: nothing to capture.
    Idle,
    Starting(Starting),
    Running(Running<D>),
    /// Until the desktop or display changes.
    Unavailable,
}

#[derive(Debug, Clone, Copy)]
struct Starting {
    retry_at: Instant,
    give_up: GiveUp,
    last_failure: Option<Hresult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GiveUp {
    /// Report the screen unavailable if no duplication has started by then.
    At(Instant),
    Reported,
}

impl Starting {
    fn new(now: Instant) -> Self {
        Self {
            retry_at: now,
            give_up: GiveUp::At(now + DENIED_LIMIT),
            last_failure: None,
        }
    }
}

#[derive(Debug)]
struct Running<D> {
    duplication: D,
    layout: FrameLayout,
    started: Instant,
    /// `None` until this duplication's first frame with an image, and dropped with it, so nothing
    /// shows a previous desktop's or display's pixels (issue #46, first comment).
    image: Option<Vec<u8>>,
    /// The image or the pointer changed since the app was last offered a frame.
    dirty: bool,
    stats: Stats,
}

enum Attempt {
    /// Attach found another desktop than the last check: check again at once.
    Moved,
    Retry(Option<Hresult>),
    Unavailable(Unavailable),
}

#[derive(Debug)]
pub(crate) struct ScreenMachine<W: DesktopWorld> {
    world: W,
    tracker: DesktopTracker,
    desktop: Option<InputDesktop>,
    display: Option<u32>,
    capture: Capture<W::Duplication>,
    next_poll: Instant,
    /// Survives duplications of one display, so a pointer-only first frame still places it.
    pointer: Pointer,
}

impl<W: DesktopWorld> ScreenMachine<W> {
    pub(crate) fn new(world: W, now: Instant) -> Self {
        Self {
            world,
            tracker: DesktopTracker::default(),
            desktop: None,
            display: None,
            capture: Capture::Idle,
            next_poll: now,
            pointer: Pointer::default(),
        }
    }

    /// The app's `SelectDisplay`. The same display again changes nothing.
    pub(crate) fn select_display(&mut self, display: u32, now: Instant) -> Vec<ScreenEvent> {
        if self.display == Some(display) {
            return Vec::new();
        }
        self.display = Some(display);
        self.pointer = Pointer::default();
        self.restart(Ended::DisplaySelected, now)
            .into_iter()
            .collect()
    }

    /// One bounded step: checks the desktop when due, then advances the capture, blocking at most
    /// `ACQUIRE_SLICE` in `AcquireNextFrame`.
    pub(crate) fn step(&mut self, now: Instant) -> Vec<ScreenEvent> {
        let mut events = Vec::new();
        if now >= self.next_poll {
            self.next_poll = now + POLL_INTERVAL;
            if let Some(desktop) = self.tracker.poll(&mut self.world) {
                self.desktop = Some(desktop.clone());
                events.push(ScreenEvent::DesktopChanged(desktop));
                events.extend(self.restart(Ended::DesktopChanged, now));
            }
        }
        self.capture = match std::mem::replace(&mut self.capture, Capture::Idle) {
            Capture::Starting(starting) if now >= starting.retry_at => {
                self.start(starting, now, &mut events)
            }
            Capture::Running(running) => self.run(running, now, &mut events),
            waiting @ (Capture::Idle | Capture::Starting(_) | Capture::Unavailable) => waiting,
        };
        events
    }

    /// Until when the screen thread may wait for commands instead of stepping. `None` while a
    /// duplication runs: the next step waits in `AcquireNextFrame` instead.
    pub(crate) fn idle_until(&self) -> Option<Instant> {
        match &self.capture {
            Capture::Running(_) => None,
            Capture::Starting(starting) => Some(self.next_poll.min(starting.retry_at)),
            Capture::Idle | Capture::Unavailable => Some(self.next_poll),
        }
    }

    /// The frame to offer the app, if the image or the pointer changed since the last offer.
    pub(crate) fn dirty_frame(&self) -> Option<Composed<'_>> {
        let (Capture::Running(running), Some(display)) = (&self.capture, self.display) else {
            return None;
        };
        if !running.dirty {
            return None;
        }
        Some(Composed {
            display,
            layout: running.layout,
            image: running.image.as_deref()?,
            pointer: &self.pointer,
        })
    }

    pub(crate) fn mark_offered(&mut self) {
        if let Capture::Running(running) = &mut self.capture {
            running.dirty = false;
            running.stats.offered += 1;
        }
    }

    fn wants_capture(&self) -> bool {
        self.display.is_some()
            && self
                .desktop
                .as_ref()
                .is_some_and(|desktop| *desktop != InputDesktop::Default)
    }

    /// Drops any duplication and starts over for the current desktop and display.
    fn restart(&mut self, why: Ended, now: Instant) -> Option<ScreenEvent> {
        let next = if self.wants_capture() {
            Capture::Starting(Starting::new(now))
        } else {
            Capture::Idle
        };
        match std::mem::replace(&mut self.capture, next) {
            Capture::Running(running) => Some(ended(&running, why, now)),
            Capture::Idle | Capture::Starting(_) | Capture::Unavailable => None,
        }
    }

    fn start(
        &mut self,
        mut starting: Starting,
        now: Instant,
        events: &mut Vec<ScreenEvent>,
    ) -> Capture<W::Duplication> {
        let Some(display) = self.display else {
            return Capture::Idle;
        };
        let unavailable = |why, events: &mut Vec<ScreenEvent>| {
            events.push(ScreenEvent::Note(Note::Unavailable { display, why }));
            events.push(ScreenEvent::Unavailable { display });
            Capture::Unavailable
        };
        match self.attempt(display) {
            Ok(duplication) => {
                let size = duplication.size();
                let Ok(layout) = FrameLayout::new(size.width, size.height) else {
                    return unavailable(Unavailable::TooLarge(size), events);
                };
                events.push(ScreenEvent::Note(Note::Duplicated {
                    display,
                    desktop: self.desktop.clone().unwrap_or(InputDesktop::Default),
                    size,
                }));
                Capture::Running(Running {
                    duplication,
                    layout,
                    started: now,
                    image: None,
                    dirty: false,
                    stats: Stats::default(),
                })
            }
            Err(Attempt::Moved) => {
                self.next_poll = now;
                starting.retry_at = now + POLL_INTERVAL;
                Capture::Starting(starting)
            }
            Err(Attempt::Retry(code)) => {
                if code.is_some() && code != starting.last_failure {
                    starting.last_failure = code;
                    if let Some(code) = code {
                        events.push(ScreenEvent::Note(Note::CannotDuplicate { display, code }));
                    }
                }
                starting.retry_at = self.next_poll;
                if let GiveUp::At(deadline) = starting.give_up
                    && now >= deadline
                {
                    starting.give_up = GiveUp::Reported;
                    unavailable(Unavailable::StillFailing(starting.last_failure), events);
                }
                Capture::Starting(starting)
            }
            Err(Attempt::Unavailable(why)) => unavailable(why, events),
        }
    }

    fn attempt(&mut self, display: u32) -> Result<W::Duplication, Attempt> {
        match self.world.attach() {
            Ok(Observation::Named(name))
                if self.desktop.as_ref() == Some(&InputDesktop::from_name(&name)) => {}
            Ok(Observation::Named(_)) => return Err(Attempt::Moved),
            Ok(Observation::Unreadable) => return Err(Attempt::Retry(None)),
            Err(code) => return Err(Attempt::Retry(Some(code))),
        }
        self.world
            .duplicate(display)
            .map_err(|failure| match failure {
                Duplicate::NoSuchDisplay => Attempt::Unavailable(Unavailable::NoSuchDisplay),
                Duplicate::Failed(code) => match duplicate_failure(code) {
                    DuplicateFailure::Transient => Attempt::Retry(Some(code)),
                    DuplicateFailure::Unsupported => {
                        Attempt::Unavailable(Unavailable::Unsupported(code))
                    }
                },
            })
    }

    fn run(
        &mut self,
        mut running: Running<W::Duplication>,
        now: Instant,
        events: &mut Vec<ScreenEvent>,
    ) -> Capture<W::Duplication> {
        let timeout = ACQUIRE_SLICE.min(self.next_poll.saturating_duration_since(now));
        let outcome = match running.duplication.acquire(timeout) {
            Ok(frame) => self.take(&mut running, frame, now, events),
            Err(code) => Err(code),
        };
        if let Err(code) = outcome
            && frame_failure(code) == FrameFailure::Switched
        {
            return self.lose(running, Ended::Failed(code), now, events);
        }
        if running.image.is_none() && now.duration_since(running.started) >= NO_IMAGE_LIMIT {
            return self.lose(running, Ended::NoImage, now, events);
        }
        Capture::Running(running)
    }

    /// Copies what an acquired frame changed and releases it. A frame that fails anywhere
    /// between acquire and release is discarded whole.
    fn take(
        &mut self,
        running: &mut Running<W::Duplication>,
        frame: AcquiredFrame,
        now: Instant,
        events: &mut Vec<ScreenEvent>,
    ) -> Result<(), Hresult> {
        let has_image = frame.accumulated_frames != 0;
        let copied = if has_image {
            let slot_len = running.layout.slot_len();
            let image = running.image.get_or_insert_with(|| vec![0; slot_len]);
            running.duplication.copy_image(image)
        } else {
            Ok(())
        };
        let shape = if frame.shape_changed && copied.is_ok() {
            Some(running.duplication.pointer_shape())
        } else {
            None
        };
        let released = running.duplication.release();
        copied?;
        let shape = shape.transpose()?;
        released?;

        if has_image {
            running.stats.images += 1;
            running.dirty = true;
            if running.stats.images == 1 {
                events.push(ScreenEvent::Note(Note::FirstImage {
                    accumulated_frames: frame.accumulated_frames,
                    last_present_time: frame.last_present_time,
                    skipped: running.stats.pointer_only,
                    after: now.duration_since(running.started),
                }));
            }
        } else {
            running.stats.pointer_only += 1;
        }
        if let Some(position) = frame.pointer {
            self.pointer.moved(position);
            running.dirty |= running.image.is_some();
        }
        if let Some(raw) = shape {
            let kind = raw.kind;
            match PointerShape::parse(raw) {
                Some(shape) => {
                    self.pointer.reshaped(shape);
                    running.dirty |= running.image.is_some();
                }
                None => events.push(ScreenEvent::Note(Note::BadPointerShape { kind })),
            }
        }
        Ok(())
    }

    /// Drops a duplication that failed and checks the desktop at once before duplicating again.
    fn lose(
        &mut self,
        running: Running<W::Duplication>,
        why: Ended,
        now: Instant,
        events: &mut Vec<ScreenEvent>,
    ) -> Capture<W::Duplication> {
        events.push(ended(&running, why, now));
        drop(running);
        self.next_poll = now;
        Capture::Starting(Starting::new(now))
    }
}

fn ended<D>(running: &Running<D>, why: Ended, now: Instant) -> ScreenEvent {
    ScreenEvent::Note(Note::Ended {
        why,
        stats: running.stats,
        lasted: now.duration_since(running.started),
    })
}

/// Copies rows of BGRA pixels, each starting `pitch` bytes after the previous one in `source`,
/// into `into` as tightly packed RGBA rows `width` pixels wide, with opaque alpha.
pub(crate) fn rgba_from_bgra_rows(source: &[u8], pitch: usize, width: usize, into: &mut [u8]) {
    for (row, source_row) in into.chunks_exact_mut(width * 4).zip(source.chunks(pitch)) {
        let (pixels, _) = row.as_chunks_mut::<4>();
        let (source_pixels, _) = source_row.as_chunks::<4>();
        for (pixel, [blue, green, red, _]) in pixels.iter_mut().zip(source_pixels) {
            *pixel = [*red, *green, *blue, 0xFF];
        }
    }
}

/// An image with the pointer, ready to be written into a section slot.
#[derive(Debug)]
pub(crate) struct Composed<'a> {
    pub(crate) display: u32,
    layout: FrameLayout,
    image: &'a [u8],
    pointer: &'a Pointer,
}

impl Composed<'_> {
    pub(crate) fn layout(&self) -> FrameLayout {
        self.layout
    }

    /// Copies the image into `slot`, `layout().slot_len()` bytes, and draws the pointer.
    pub(crate) fn write_into(&self, slot: &mut [u8]) {
        slot.copy_from_slice(self.image);
        self.pointer.draw(slot, self.layout.width());
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use super::*;

    const SIZE: Size = Size {
        width: 4,
        height: 2,
    };
    const POINTER: [u8; 4] = [1, 2, 3, 255];

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Attach,
        Duplicate(u32),
        Acquire(usize),
        Copy(usize),
        Shape(usize),
        Release(usize),
    }

    type Calls = Rc<RefCell<Vec<Call>>>;

    /// One acquire's script: the frame info, or the failure.
    #[derive(Debug, Clone)]
    enum Acquire {
        Frame {
            frame: AcquiredFrame,
            copy: Result<u8, Hresult>,
            release: Result<(), Hresult>,
        },
        Fail(Hresult),
    }

    #[derive(Debug, Default)]
    struct World {
        current: Option<Observation>,
        desktops: VecDeque<Observation>,
        attaches: VecDeque<Result<Observation, Hresult>>,
        duplicates: VecDeque<Result<Vec<Acquire>, Duplicate>>,
        calls: Calls,
        next_id: usize,
    }

    impl World {
        fn on(desktop: &str) -> Self {
            Self {
                current: Some(named(desktop)),
                ..Self::default()
            }
        }

        fn switch_to(&mut self, desktop: &str) {
            self.current = Some(named(desktop));
        }

        fn duplicates(mut self, script: Vec<Result<Vec<Acquire>, Duplicate>>) -> Self {
            self.duplicates = script.into();
            self
        }
    }

    impl DesktopSource for World {
        fn poll(&mut self) -> Observation {
            if let Some(next) = self.desktops.pop_front() {
                return next;
            }
            self.current.clone().unwrap_or(Observation::Unreadable)
        }
    }

    impl DesktopWorld for World {
        type Duplication = Scripted;

        fn attach(&mut self) -> Result<Observation, Hresult> {
            self.calls.borrow_mut().push(Call::Attach);
            self.attaches
                .pop_front()
                .unwrap_or_else(|| Ok(self.current.clone().unwrap_or(Observation::Unreadable)))
        }

        fn duplicate(&mut self, display: u32) -> Result<Scripted, Duplicate> {
            self.calls.borrow_mut().push(Call::Duplicate(display));
            let script = self
                .duplicates
                .pop_front()
                .unwrap_or(Err(Duplicate::Failed(Hresult::E_ACCESSDENIED)))?;
            self.next_id += 1;
            Ok(Scripted {
                id: self.next_id,
                script: script.into(),
                held: None,
                calls: self.calls.clone(),
            })
        }
    }

    /// A duplication that plays its script, then times out forever. It panics on any call DXGI
    /// would refuse, such as acquiring before releasing.
    #[derive(Debug)]
    struct Scripted {
        id: usize,
        script: VecDeque<Acquire>,
        held: Option<(Result<u8, Hresult>, Result<(), Hresult>)>,
        calls: Calls,
    }

    impl Duplication for Scripted {
        fn size(&self) -> Size {
            SIZE
        }

        fn acquire(&mut self, _timeout: Duration) -> Result<AcquiredFrame, Hresult> {
            self.calls.borrow_mut().push(Call::Acquire(self.id));
            assert!(self.held.is_none(), "AcquireNextFrame before ReleaseFrame");
            match self
                .script
                .pop_front()
                .unwrap_or(Acquire::Fail(Hresult::WAIT_TIMEOUT))
            {
                Acquire::Frame {
                    frame,
                    copy,
                    release,
                } => {
                    self.held = Some((copy, release));
                    Ok(frame)
                }
                Acquire::Fail(code) => Err(code),
            }
        }

        fn copy_image(&mut self, into: &mut [u8]) -> Result<(), Hresult> {
            self.calls.borrow_mut().push(Call::Copy(self.id));
            let (copy, _) = self.held.expect("a copy outside a held frame");
            assert_eq!(into.len(), (SIZE.width * SIZE.height * 4) as usize);
            into.fill(copy?);
            Ok(())
        }

        fn pointer_shape(&mut self) -> Result<RawPointerShape, Hresult> {
            self.calls.borrow_mut().push(Call::Shape(self.id));
            assert!(self.held.is_some(), "a pointer shape outside a held frame");
            Ok(RawPointerShape {
                kind: 2,
                width: 1,
                height: 1,
                pitch: 4,
                buffer: vec![POINTER[2], POINTER[1], POINTER[0], 255],
            })
        }

        fn release(&mut self) -> Result<(), Hresult> {
            self.calls.borrow_mut().push(Call::Release(self.id));
            let (_, release) = self.held.take().expect("ReleaseFrame without a frame");
            release
        }
    }

    fn named(name: &str) -> Observation {
        Observation::Named(name.into())
    }

    fn image(fill: u8) -> Acquire {
        Acquire::Frame {
            frame: AcquiredFrame {
                accumulated_frames: 1,
                last_present_time: 42,
                pointer: None,
                shape_changed: false,
            },
            copy: Ok(fill),
            release: Ok(()),
        }
    }

    fn released_with(acquire: Acquire, code: Hresult) -> Acquire {
        match acquire {
            Acquire::Frame { frame, copy, .. } => Acquire::Frame {
                frame,
                copy,
                release: Err(code),
            },
            Acquire::Fail(_) => acquire,
        }
    }

    /// A pointer-only update whose image is black, placing a new 1x1 pointer at `(x, y)`.
    fn pointer_only(x: i32, y: i32) -> Acquire {
        Acquire::Frame {
            frame: AcquiredFrame {
                accumulated_frames: 0,
                last_present_time: 0,
                pointer: Some(PointerPosition {
                    visible: true,
                    x,
                    y,
                }),
                shape_changed: true,
            },
            copy: Ok(0),
            release: Ok(()),
        }
    }

    struct Harness {
        machine: ScreenMachine<World>,
        now: Instant,
        events: Vec<ScreenEvent>,
    }

    impl Harness {
        fn new(world: World, display: u32) -> Self {
            let now = Instant::now();
            let mut machine = ScreenMachine::new(world, now);
            let events = machine.select_display(display, now);
            Self {
                machine,
                now,
                events,
            }
        }

        /// Steps `count` times, `every` apart.
        fn run(&mut self, count: u32, every: Duration) {
            for _ in 0..count {
                let events = self.machine.step(self.now);
                self.events.extend(events);
                self.now += every;
            }
        }

        fn step(&mut self) {
            self.run(1, Duration::from_millis(1));
        }

        fn calls(&self) -> Vec<Call> {
            self.machine.world.calls.borrow().clone()
        }

        fn duplicated(&self) -> usize {
            self.calls()
                .iter()
                .filter(|call| matches!(call, Call::Duplicate(_)))
                .count()
        }

        /// The dirty frame's pixels, then marks it offered.
        fn offer(&mut self) -> Option<Vec<[u8; 4]>> {
            let frame = self.machine.dirty_frame()?;
            let mut slot = vec![0; frame.layout().slot_len()];
            frame.write_into(&mut slot);
            self.machine.mark_offered();
            Some(
                slot.chunks(4)
                    .map(|pixel| pixel.try_into().unwrap())
                    .collect(),
            )
        }

        fn app_events(&self) -> Vec<ScreenEvent> {
            self.events
                .iter()
                .filter(|event| !matches!(event, ScreenEvent::Note(_)))
                .cloned()
                .collect()
        }

        fn notes(&self) -> Vec<Note> {
            self.events
                .iter()
                .filter_map(|event| match event {
                    ScreenEvent::Note(note) => Some(note.clone()),
                    _ => None,
                })
                .collect()
        }

        fn ends(&self) -> Vec<Ended> {
            self.notes()
                .into_iter()
                .filter_map(|note| match note {
                    Note::Ended { why, .. } => Some(why),
                    _ => None,
                })
                .collect()
        }
    }

    fn solid(fill: u8) -> Vec<[u8; 4]> {
        vec![[fill; 4]; (SIZE.width * SIZE.height) as usize]
    }

    fn with_pointer_at(mut pixels: Vec<[u8; 4]>, x: usize, y: usize) -> Vec<[u8; 4]> {
        let at = y * SIZE.width as usize + x;
        pixels[at] = [POINTER[0], POINTER[1], POINTER[2], pixels[at][3]];
        pixels
    }

    #[test]
    fn bgra_rows_with_padding_become_packed_opaque_rgba() {
        // Two rows of two pixels, each row padded to 12 bytes; the last row isn't padded.
        let source = [
            1, 2, 3, 0, 4, 5, 6, 0, 99, 99, 99, 99, //
            7, 8, 9, 0, 10, 11, 12, 0,
        ];
        let mut into = [0; 16];
        rgba_from_bgra_rows(&source, 12, 2, &mut into);
        assert_eq!(
            into,
            [3, 2, 1, 255, 6, 5, 4, 255, 9, 8, 7, 255, 12, 11, 10, 255]
        );
    }

    #[test]
    fn nothing_is_duplicated_on_default() {
        let mut harness = Harness::new(World::on("Default"), 1);
        harness.run(50, POLL_INTERVAL);
        assert_eq!(harness.duplicated(), 0);
        assert_eq!(
            harness.app_events(),
            [ScreenEvent::DesktopChanged(InputDesktop::Default)]
        );
        assert_eq!(
            harness.machine.idle_until(),
            Some(harness.machine.next_poll)
        );
    }

    #[test]
    fn nothing_is_duplicated_before_a_display_is_selected() {
        let now = Instant::now();
        let mut machine = ScreenMachine::new(World::on("Winlogon"), now);
        machine.step(now);
        assert!(machine.world.calls.borrow().is_empty());
        machine.select_display(3, now);
        machine.step(now);
        assert_eq!(
            *machine.world.calls.borrow(),
            [Call::Attach, Call::Duplicate(3)]
        );
    }

    #[test]
    fn a_duplication_that_loses_access_mid_frame_recovers() {
        let lost = Acquire::Frame {
            frame: AcquiredFrame {
                accumulated_frames: 1,
                last_present_time: 43,
                pointer: None,
                shape_changed: false,
            },
            copy: Err(Hresult::ACCESS_LOST),
            release: Ok(()),
        };
        let world = World::on("Winlogon").duplicates(vec![
            Ok(vec![image(10), lost]),
            Ok(vec![pointer_only(1, 1), image(20)]),
        ]);
        let mut harness = Harness::new(world, 1);
        harness.step();
        harness.step();
        assert_eq!(harness.offer(), Some(solid(10)));

        harness.step();
        assert_eq!(harness.offer(), None, "the frame that failed is discarded");
        assert_eq!(harness.ends(), [Ended::Failed(Hresult::ACCESS_LOST)]);
        let calls = harness.calls();
        assert_eq!(
            calls[calls.len() - 3..],
            [Call::Acquire(1), Call::Copy(1), Call::Release(1)],
            "the failed frame is still released"
        );

        harness.step();
        assert_eq!(harness.duplicated(), 2, "duplicated again at once");
        harness.step();
        assert_eq!(
            harness.offer(),
            None,
            "a pointer-only first frame publishes nothing"
        );
        harness.step();
        assert_eq!(harness.offer(), Some(with_pointer_at(solid(20), 1, 1)));
        assert!(harness.notes().contains(&Note::FirstImage {
            accumulated_frames: 1,
            last_present_time: 42,
            skipped: 1,
            after: Duration::from_millis(2),
        }));
        assert_eq!(
            harness.app_events(),
            [ScreenEvent::DesktopChanged(InputDesktop::Winlogon)]
        );
    }

    #[test]
    fn release_frame_failures_are_switches() {
        let world = World::on("Winlogon").duplicates(vec![
            Ok(vec![released_with(image(1), Hresult::INVALID_CALL)]),
            Ok(vec![released_with(image(2), Hresult::ACCESS_LOST)]),
            Ok(vec![image(3)]),
        ]);
        let mut harness = Harness::new(world, 1);
        harness.run(6, Duration::from_millis(1));
        assert_eq!(
            harness.ends(),
            [
                Ended::Failed(Hresult::INVALID_CALL),
                Ended::Failed(Hresult::ACCESS_LOST)
            ]
        );
        assert_eq!(harness.offer(), Some(solid(3)));
    }

    #[test]
    fn access_lost_on_the_way_back_to_default_reports_default_and_stops_capturing() {
        let world = World::on("Winlogon").duplicates(vec![Ok(vec![
            image(1),
            Acquire::Fail(Hresult::WAIT_TIMEOUT),
            Acquire::Fail(Hresult::INVALID_CALL),
        ])]);
        let mut harness = Harness::new(world, 1);
        harness.run(3, Duration::from_millis(1));
        assert!(harness.offer().is_some());
        harness.machine.world.switch_to("Default");

        harness.step();
        assert_eq!(harness.ends(), [Ended::Failed(Hresult::INVALID_CALL)]);
        harness.step();
        assert_eq!(
            harness.app_events(),
            [
                ScreenEvent::DesktopChanged(InputDesktop::Winlogon),
                ScreenEvent::DesktopChanged(InputDesktop::Default),
            ]
        );
        harness.run(50, POLL_INTERVAL);
        assert_eq!(harness.duplicated(), 1);
        assert_eq!(harness.offer(), None);
    }

    #[test]
    fn access_denied_is_retried_after_each_desktop_check_and_reported_after_five_seconds() {
        let mut harness = Harness::new(World::on("Winlogon"), 2);
        harness.run(510, Duration::from_millis(10));
        assert_eq!(
            harness.app_events(),
            [
                ScreenEvent::DesktopChanged(InputDesktop::Winlogon),
                ScreenEvent::Unavailable { display: 2 },
            ]
        );
        assert!(
            (50..=52).contains(&harness.duplicated()),
            "{}",
            harness.duplicated()
        );
        assert!(harness.notes().contains(&Note::Unavailable {
            display: 2,
            why: Unavailable::StillFailing(Some(Hresult::E_ACCESSDENIED)),
        }));

        harness.run(200, Duration::from_millis(10));
        assert!(
            (70..=72).contains(&harness.duplicated()),
            "{}",
            harness.duplicated()
        );
        assert_eq!(
            harness.app_events().len(),
            2,
            "reported once, still retrying"
        );
        assert_eq!(
            harness
                .notes()
                .iter()
                .filter(|note| matches!(note, Note::CannotDuplicate { .. }))
                .count(),
            1,
            "the same code is logged once"
        );
    }

    #[test]
    fn the_five_second_limit_counts_from_when_duplicating_began() {
        let mut harness = Harness::new(World::on("Winlogon"), 2);
        harness.run(499, Duration::from_millis(10));
        assert_eq!(harness.app_events().len(), 1, "not yet at 4.99 s");
        harness.run(2, Duration::from_millis(10));
        assert_eq!(harness.app_events().len(), 2);
    }

    #[test]
    fn unsupported_is_reported_until_the_next_desktop_change() {
        let world = World::on("Winlogon").duplicates(vec![
            Err(Duplicate::Failed(Hresult::UNSUPPORTED)),
            Ok(vec![image(5)]),
        ]);
        let mut harness = Harness::new(world, 1);
        harness.run(30, POLL_INTERVAL);
        assert_eq!(harness.duplicated(), 1);
        assert_eq!(
            harness.app_events(),
            [
                ScreenEvent::DesktopChanged(InputDesktop::Winlogon),
                ScreenEvent::Unavailable { display: 1 },
            ]
        );

        harness.machine.world.switch_to("Screen-saver");
        harness.run(3, POLL_INTERVAL);
        assert_eq!(harness.duplicated(), 2);
        assert_eq!(harness.offer(), Some(solid(5)));
    }

    #[test]
    fn an_unknown_display_is_reported_unavailable() {
        let world = World::on("Winlogon").duplicates(vec![Err(Duplicate::NoSuchDisplay)]);
        let mut harness = Harness::new(world, 9);
        harness.run(30, POLL_INTERVAL);
        assert_eq!(harness.duplicated(), 1);
        assert_eq!(
            harness.app_events()[1..],
            [ScreenEvent::Unavailable { display: 9 }]
        );
        assert!(harness.notes().contains(&Note::Unavailable {
            display: 9,
            why: Unavailable::NoSuchDisplay
        }));
    }

    #[test]
    fn a_switch_between_the_check_and_the_attach_rechecks_the_desktop() {
        let mut world = World::on("Winlogon");
        world.attaches.push_back(Ok(named("Default")));
        world.current = Some(named("Default"));
        world.desktops.push_back(named("Winlogon"));
        let mut harness = Harness::new(world, 1);
        harness.step();
        harness.step();
        assert_eq!(
            harness.app_events(),
            [
                ScreenEvent::DesktopChanged(InputDesktop::Winlogon),
                ScreenEvent::DesktopChanged(InputDesktop::Default),
            ],
            "checked again on the next step, not the next interval"
        );
        assert_eq!(harness.calls(), [Call::Attach]);
    }

    #[test]
    fn selecting_another_display_drops_the_image_and_duplicates_again() {
        let world = World::on("Winlogon").duplicates(vec![Ok(vec![image(1)]), Ok(vec![])]);
        let mut harness = Harness::new(world, 1);
        harness.run(2, Duration::from_millis(1));
        assert!(harness.machine.dirty_frame().is_some());

        let now = harness.now;
        assert_eq!(harness.machine.select_display(1, now), []);
        let events = harness.machine.select_display(2, now);
        assert!(matches!(
            events[..],
            [ScreenEvent::Note(Note::Ended {
                why: Ended::DisplaySelected,
                stats: Stats { images: 1, .. },
                ..
            })]
        ));
        assert_eq!(harness.offer(), None);
        harness.step();
        assert_eq!(harness.calls().last(), Some(&Call::Duplicate(2)));
    }

    #[test]
    fn a_pointer_only_update_redraws_the_pointer_on_the_last_image() {
        let world = World::on("Winlogon").duplicates(vec![Ok(vec![
            image(7),
            pointer_only(3, 0),
            Acquire::Fail(Hresult::WAIT_TIMEOUT),
        ])]);
        let mut harness = Harness::new(world, 1);
        harness.run(2, Duration::from_millis(1));
        assert_eq!(harness.offer(), Some(solid(7)));
        harness.step();
        assert_eq!(harness.offer(), Some(with_pointer_at(solid(7), 3, 0)));
        harness.step();
        assert_eq!(harness.offer(), None, "a still screen offers nothing new");
    }

    #[test]
    fn a_duplication_with_no_image_within_a_second_is_replaced() {
        let world = World::on("Winlogon")
            .duplicates(vec![Ok(vec![pointer_only(0, 0)]), Ok(vec![image(3)])]);
        let mut harness = Harness::new(world, 1);
        harness.run(9, Duration::from_millis(100));
        assert_eq!(harness.duplicated(), 1);
        assert_eq!(harness.offer(), None);
        harness.run(2, Duration::from_millis(100));
        assert_eq!(harness.ends(), [Ended::NoImage]);
        harness.run(2, Duration::from_millis(1));
        assert_eq!(harness.duplicated(), 2);
        assert_eq!(harness.offer(), Some(with_pointer_at(solid(3), 0, 0)));
    }

    #[test]
    fn a_duplication_too_large_for_a_section_is_unavailable() {
        struct Huge(World);
        // Reuses the scripted world but reports a size past MAX_FRAME_DIMENSION.
        impl DesktopSource for Huge {
            fn poll(&mut self) -> Observation {
                self.0.poll()
            }
        }
        impl DesktopWorld for Huge {
            type Duplication = HugeDuplication;
            fn attach(&mut self) -> Result<Observation, Hresult> {
                self.0.attach()
            }
            fn duplicate(&mut self, display: u32) -> Result<HugeDuplication, Duplicate> {
                self.0.duplicate(display).map(HugeDuplication)
            }
        }
        struct HugeDuplication(Scripted);
        impl Duplication for HugeDuplication {
            fn size(&self) -> Size {
                Size {
                    width: 10_240,
                    height: 4_320,
                }
            }
            fn acquire(&mut self, timeout: Duration) -> Result<AcquiredFrame, Hresult> {
                self.0.acquire(timeout)
            }
            fn copy_image(&mut self, into: &mut [u8]) -> Result<(), Hresult> {
                self.0.copy_image(into)
            }
            fn pointer_shape(&mut self) -> Result<RawPointerShape, Hresult> {
                self.0.pointer_shape()
            }
            fn release(&mut self) -> Result<(), Hresult> {
                self.0.release()
            }
        }

        let now = Instant::now();
        let world = Huge(World::on("Winlogon").duplicates(vec![Ok(vec![image(1)])]));
        let mut machine = ScreenMachine::new(world, now);
        machine.select_display(1, now);
        let events = machine.step(now);
        assert_eq!(
            events.last(),
            Some(&ScreenEvent::Unavailable { display: 1 })
        );
        assert_eq!(machine.step(now + POLL_INTERVAL), []);
    }
}
