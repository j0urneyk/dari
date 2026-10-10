//! The host's capture → scale → encode loop on a dedicated thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::codec::{CodecError, EncodedFrame, EncoderSettings, FrameDelivery, VideoEncoder};
use crate::display::CaptureError;
use crate::frame::CapturedFrame;
use crate::scale::FrameScaler;

/// A source of screen frames.
pub trait ScreenCapturer {
    /// Captures the screen.
    ///
    /// A polled source captures whenever it is called. A source that delivers frames on its own
    /// clock ([`ScreenCapturer::paces_itself`]) waits up to `timeout` for a frame newer than the
    /// last one it returned, and returns `None` if the screen has not changed.
    fn capture(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError>;

    /// Whether frames arrive on the source's own clock, already limited to the stream's frame
    /// rate, so the stream must not pace captures itself.
    fn paces_itself(&self) -> bool {
        false
    }

    /// Whether the last [`ScreenCapturer::capture`] moved to a different picture source (the
    /// Windows secure desktop replacing the user's, or back), so the stream must not resend a
    /// frame from before it and must encode the next frame as a keyframe. The stream asks after
    /// every capture, whatever it returned, and asking clears the change.
    fn take_source_change(&mut self) -> bool {
        false
    }
}

impl<T: ScreenCapturer + ?Sized> ScreenCapturer for Box<T> {
    fn capture(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
        (**self).capture(timeout)
    }

    fn paces_itself(&self) -> bool {
        (**self).paces_itself()
    }

    fn take_source_change(&mut self) -> bool {
        (**self).take_source_change()
    }
}

/// Stream parameters the host may change while streaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamSettings {
    /// Longest edge of the encoded video, in pixels.
    pub max_long_edge: u32,
    pub max_fps: u32,
    pub bitrate_bps: u32,
    /// Encode with the platform's hardware encoder where there is one (VideoToolbox on macOS, a
    /// Media Foundation hardware encoder on Windows), falling back to OpenH264.
    pub hardware_encoder: bool,
    pub still_refinement: StillRefinement,
}

impl StreamSettings {
    #[expect(clippy::cast_precision_loss, reason = "frame rates are small integers")]
    fn encoder_settings(self) -> EncoderSettings {
        EncoderSettings {
            bitrate_bps: self.bitrate_bps,
            max_fps: self.max_fps.max(1) as f32,
            hardware: self.hardware_encoder,
        }
    }
}

impl Default for StreamSettings {
    fn default() -> Self {
        Self {
            max_long_edge: 1920,
            max_fps: 30,
            bitrate_bps: 4_000_000,
            hardware_encoder: true,
            still_refinement: StillRefinement::default(),
        }
    }
}

/// How the stream sharpens a screen that has stopped changing.
///
/// The frame that ends a change is encoded under the bitrate budget of a moving screen, and a
/// self-paced source then delivers nothing while the screen is still, so the viewer would keep
/// that coarse picture until the next change. Instead, once the screen has been still for
/// `delay`, the stream re-encodes the last frame `frames` times, `interval` apart: the encoder
/// spends the bits a still screen saves on refining the picture it already shows, and then the
/// stream goes quiet again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StillRefinement {
    /// How long the screen must have been still before refinement starts, so a dropped or late
    /// frame of a moving screen does not start one.
    pub delay: Duration,
    /// Time between refinement frames.
    pub interval: Duration,
    /// How many times the last frame is re-encoded after each change.
    pub frames: u32,
}

impl Default for StillRefinement {
    fn default() -> Self {
        Self {
            delay: Duration::from_millis(100),
            interval: Duration::from_millis(33),
            frames: 12,
        }
    }
}

impl StillRefinement {
    /// When the refinement frame after `sent` of them is due, counted from the moment the screen
    /// went still; `None` once all of them have been sent.
    pub fn due_after(self, sent: u32) -> Option<Duration> {
        (sent < self.frames).then(|| self.delay + self.interval * sent)
    }
}

#[derive(Debug, Error)]
pub enum StreamError {
    #[error(transparent)]
    Capture(#[from] CaptureError),
    #[error(transparent)]
    Codec(#[from] CodecError),
}

/// Counters for diagnostics and tests.
#[derive(Debug, Default)]
pub struct StreamStats {
    pub frames_encoded: AtomicU64,
    /// Frames dropped before encoding because the consumer had not taken the previous one yet.
    pub frames_skipped: AtomicU64,
    /// Frames re-encoded to refine a still screen ([`StillRefinement`]).
    pub still_refinements: AtomicU64,
    /// Time from capturing each encoded frame to its delivery (scaling and encoding latency),
    /// in microseconds. Frames overlap in a hardware encoder, so this is not the time the
    /// thread was busy.
    pub encode_micros: AtomicU64,
    /// Whether the frames so far came from a hardware encoder.
    pub hardware_encoding: AtomicBool,
}

#[derive(Debug, Default)]
struct StreamControl {
    stop: AtomicBool,
    keyframe_requested: AtomicBool,
    /// Frames submitted to the encoder and not yet delivered or dropped.
    in_flight: AtomicUsize,
    stats: StreamStats,
}

/// Handle to a running capture thread. Dropping it stops the thread.
#[derive(Debug)]
pub struct CaptureStream {
    control: Arc<StreamControl>,
    thread: Option<JoinHandle<()>>,
}

impl CaptureStream {
    /// Makes the next frame a keyframe, e.g. after the viewer lost decoder state.
    pub fn request_keyframe(&self) {
        self.control
            .keyframe_requested
            .store(true, Ordering::Relaxed);
    }

    pub fn stats(&self) -> &StreamStats {
        &self.control.stats
    }

    /// Stops capturing and waits for the thread to exit.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.control.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            warn!("capture thread panicked");
        }
    }
}

impl Drop for CaptureStream {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// How long to wait before capturing again after a transient failure.
const CAPTURE_RETRY_DELAY: Duration = Duration::from_millis(250);
/// How long capture may keep failing before the stream gives up.
const CAPTURE_FAILURE_LIMIT: Duration = Duration::from_secs(30);
/// Longest wait for a self-paced source, so a stop request is noticed promptly.
const SOURCE_WAIT: Duration = Duration::from_millis(50);

/// How many frames the encoder may work on at once. VideoToolbox takes a new frame while the
/// previous one is still being encoded, so a frame that takes longer than the frame interval
/// (about 8 ms at 2560×1662) does not cap the frame rate. A sink needs room for this many frames
/// for the stream to benefit.
pub const FRAMES_IN_FLIGHT: usize = 2;

/// Starts capturing on a new thread.
///
/// `open_capturer` runs on that thread, so platform capture handles never cross threads.
/// Encoded frames are delivered to `sink`, each as soon as it is encoded; a fatal error is
/// delivered as the last item. A frame is only encoded while no encoded frame is waiting in
/// `sink` and fewer than [`FRAMES_IN_FLIGHT`] are being encoded, with room reserved for each, so
/// a slow network drops whole frames *before* encoding and the H.264 reference chain stays
/// intact. Transient capture failures (a display mode change) are retried for up to 30 seconds.
/// A screen hidden behind a secure desktop is not a failure: [`CaptureError::SecureDesktop`] is
/// delivered once, frames stop, and the next frame marks the screen visible again.
pub fn spawn_capture_stream<C, F>(
    open_capturer: F,
    settings: StreamSettings,
    sink: mpsc::Sender<Result<EncodedFrame, StreamError>>,
) -> std::io::Result<CaptureStream>
where
    C: ScreenCapturer,
    F: FnOnce() -> Result<C, CaptureError> + Send + 'static,
{
    let control = Arc::new(StreamControl::default());
    let thread_control = control.clone();
    let thread = std::thread::Builder::new()
        .name("dari-capture".into())
        .spawn(move || {
            if let Err(error) = run(open_capturer, settings, &sink, &thread_control) {
                warn!(%error, "capture stream failed to start");
                // Nothing was sent yet, so the channel has room; never block on the consumer.
                let _delivered = sink.try_send(Err(error));
            }
        })?;
    Ok(CaptureStream {
        control,
        thread: Some(thread),
    })
}

fn run<C, F>(
    open_capturer: F,
    settings: StreamSettings,
    sink: &mpsc::Sender<Result<EncodedFrame, StreamError>>,
    control: &Arc<StreamControl>,
) -> Result<(), StreamError>
where
    C: ScreenCapturer,
    F: FnOnce() -> Result<C, CaptureError>,
{
    let mut capturer = open_capturer()?;
    let paced_by_source = capturer.paces_itself();
    let mut scaler = FrameScaler::default();
    let mut encoder = VideoEncoder::new(settings.encoder_settings())?;

    let interval = Duration::from_secs(1) / settings.max_fps.max(1);
    let refinement = settings.still_refinement;
    let mut next_frame = Instant::now();
    let mut failing_since: Option<Instant> = None;
    let mut hidden_reported = false;
    let mut still: Option<StillScreen> = None;
    while !control.stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if !paced_by_source {
            if now < next_frame {
                // Sleep in short slices so a stop request is noticed promptly.
                std::thread::sleep((next_frame - now).min(Duration::from_millis(50)));
                continue;
            }
            next_frame = (next_frame + interval).max(now);
            // Capturing is a polled source's expensive step; skip it while the consumer is behind.
            if consumer_is_behind(sink, control) {
                control.stats.frames_skipped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        }

        let wait = match &still {
            Some(screen) if !consumer_is_behind(sink, control) => {
                screen.capture_timeout(refinement, now)
            }
            _ => SOURCE_WAIT,
        };
        let captured = capturer.capture(wait);
        if capturer.take_source_change() {
            still = None;
            // Sticky, so a frame skipped for a full sink passes it on to the next one.
            control.keyframe_requested.store(true, Ordering::Relaxed);
        }
        let (captured, resend) = match captured.inspect(|_| hidden_reported = false) {
            Ok(Some(frame)) => {
                failing_since = None;
                if paced_by_source {
                    still = Some(StillScreen::new(frame.clone()));
                }
                (frame, None)
            }
            Ok(None) => {
                let keyframe_wanted =
                    control.keyframe_requested.load(Ordering::Relaxed) || encoder.has_lost_frames();
                match still
                    .as_ref()
                    .and_then(|screen| screen.resend(refinement, keyframe_wanted))
                {
                    Some((frame, resend)) => (frame, Some(resend)),
                    None => continue,
                }
            }
            Err(CaptureError::SecureDesktop) => {
                // The last frame may show a screen that is gone when the stretch ends (the
                // helper's secure desktop, after the helper died), so it is never resent.
                still = None;
                if !std::mem::replace(&mut hidden_reported, true) {
                    deliver_error(sink, control, CaptureError::SecureDesktop.into());
                }
                continue;
            }
            Err(error @ CaptureError::Backend(_))
                if now.duration_since(*failing_since.get_or_insert(now))
                    < CAPTURE_FAILURE_LIMIT =>
            {
                debug!(%error, "capture failed; retrying");
                next_frame = now + CAPTURE_RETRY_DELAY;
                if paced_by_source {
                    std::thread::sleep(CAPTURE_RETRY_DELAY);
                }
                continue;
            }
            Err(error) => {
                warn!(%error, "capture stream stopped");
                deliver_error(sink, control, error.into());
                return Ok(());
            }
        };

        match encode(
            captured,
            &mut scaler,
            &mut encoder,
            settings.max_long_edge,
            sink,
            control,
        ) {
            Ok(Encoded::Sent) => {
                if let Some((screen, resend)) = still.as_mut().zip(resend) {
                    screen.sent(resend, &control.stats);
                }
            }
            Ok(Encoded::Skipped) => {}
            Ok(Encoded::SinkClosed) => return Ok(()),
            Err(error) => {
                warn!(%error, "capture stream stopped");
                deliver_error(sink, control, error.into());
                return Ok(());
            }
        }
    }
    Ok(())
}

enum Encoded {
    Sent,
    Skipped,
    SinkClosed,
}

fn encode(
    captured: CapturedFrame,
    scaler: &mut FrameScaler,
    encoder: &mut VideoEncoder,
    max_long_edge: u32,
    sink: &mpsc::Sender<Result<EncodedFrame, StreamError>>,
    control: &Arc<StreamControl>,
) -> Result<Encoded, CodecError> {
    let permit = match reserve_room(sink, control) {
        Room::Reserved(permit) => permit,
        Room::Full => {
            control.stats.frames_skipped.fetch_add(1, Ordering::Relaxed);
            return Ok(Encoded::Skipped);
        }
        Room::Closed => {
            debug!("capture sink closed");
            return Ok(Encoded::SinkClosed);
        }
    };
    let deliver = delivery(permit, control.clone());
    let frame = fit(captured, scaler, max_long_edge);
    if control.keyframe_requested.swap(false, Ordering::Relaxed) {
        encoder.request_keyframe();
    }
    let submitted = encoder.submit(&frame, deliver);
    control
        .stats
        .hardware_encoding
        .store(encoder.is_hardware(), Ordering::Relaxed);
    submitted.map(|()| Encoded::Sent)
}

#[derive(Debug, Clone, Copy)]
enum Resend {
    Keyframe,
    Refinement,
}

struct StillScreen {
    frame: CapturedFrame,
    coarse_at: Instant,
    refined: u32,
}

impl StillScreen {
    fn new(frame: CapturedFrame) -> Self {
        Self {
            frame,
            coarse_at: Instant::now(),
            refined: 0,
        }
    }

    fn refinement_due(&self, refinement: StillRefinement) -> Option<Instant> {
        refinement
            .due_after(self.refined)
            .map(|offset| self.coarse_at + offset)
    }

    fn capture_timeout(&self, refinement: StillRefinement, now: Instant) -> Duration {
        self.refinement_due(refinement).map_or(SOURCE_WAIT, |due| {
            due.saturating_duration_since(now).min(SOURCE_WAIT)
        })
    }

    fn resend(
        &self,
        refinement: StillRefinement,
        keyframe_wanted: bool,
    ) -> Option<(CapturedFrame, Resend)> {
        let resend = if keyframe_wanted {
            Resend::Keyframe
        } else if self
            .refinement_due(refinement)
            .is_some_and(|due| due <= Instant::now())
        {
            Resend::Refinement
        } else {
            return None;
        };
        Some((self.frame.clone(), resend))
    }

    fn sent(&mut self, resend: Resend, stats: &StreamStats) {
        match resend {
            Resend::Refinement => {
                self.refined += 1;
                stats.still_refinements.fetch_add(1, Ordering::Relaxed);
            }
            Resend::Keyframe => {
                self.coarse_at = Instant::now();
                self.refined = 0;
            }
        }
    }
}

enum Room {
    Reserved(mpsc::OwnedPermit<Result<EncodedFrame, StreamError>>),
    Full,
    Closed,
}

fn reserve_room(
    sink: &mpsc::Sender<Result<EncodedFrame, StreamError>>,
    control: &StreamControl,
) -> Room {
    if consumer_is_behind(sink, control) {
        return Room::Full;
    }
    match sink.clone().try_reserve_owned() {
        Ok(permit) => Room::Reserved(permit),
        Err(mpsc::error::TrySendError::Full(_)) => Room::Full,
        Err(mpsc::error::TrySendError::Closed(_)) => Room::Closed,
    }
}

fn fit(captured: CapturedFrame, scaler: &mut FrameScaler, max_long_edge: u32) -> CapturedFrame {
    match captured {
        CapturedFrame::Rgba(frame) => CapturedFrame::Rgba(scaler.fit(frame, max_long_edge)),
        #[cfg(any(target_os = "macos", windows))]
        native @ CapturedFrame::Native(_) => native,
    }
}

/// Sends one encoded frame into the room reserved for it, and counts it.
fn delivery(
    permit: mpsc::OwnedPermit<Result<EncodedFrame, StreamError>>,
    control: Arc<StreamControl>,
) -> FrameDelivery {
    let in_flight = InFlight::start(control);
    let started = Instant::now();
    Box::new(move |encoded| {
        let stats = &in_flight.0.stats;
        stats.frames_encoded.fetch_add(1, Ordering::Relaxed);
        stats.encode_micros.fetch_add(
            u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        permit.send(Ok(encoded));
        // Counted until after the send, so `consumer_is_behind` may briefly miss a waiting frame
        // but never mistakes a frame in flight for one.
        drop(in_flight);
    })
}

/// Whether a new frame would only wait: an encoded frame the consumer has not taken yet is
/// waiting in `sink`, or the encoder already has [`FRAMES_IN_FLIGHT`] frames.
fn consumer_is_behind(
    sink: &mpsc::Sender<Result<EncodedFrame, StreamError>>,
    control: &StreamControl,
) -> bool {
    let in_flight = control.in_flight.load(Ordering::Relaxed);
    // Slots in use are reserved for frames in flight or hold frames waiting for the consumer.
    let waiting = (sink.max_capacity() - sink.capacity()).saturating_sub(in_flight);
    waiting > 0 || in_flight >= FRAMES_IN_FLIGHT
}

/// Counts one frame in flight until it is delivered or dropped.
struct InFlight(Arc<StreamControl>);

impl InFlight {
    fn start(control: Arc<StreamControl>) -> Self {
        control.in_flight.fetch_add(1, Ordering::Relaxed);
        Self(control)
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

fn deliver_error(
    sink: &mpsc::Sender<Result<EncodedFrame, StreamError>>,
    control: &StreamControl,
    error: StreamError,
) {
    let mut item = Err(error);
    loop {
        match sink.try_send(item) {
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => return,
            Err(mpsc::error::TrySendError::Full(returned)) => {
                if control.stop.load(Ordering::Relaxed) {
                    return;
                }
                item = returned;
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthetic::SyntheticCapturer;

    fn settings() -> StreamSettings {
        StreamSettings {
            max_long_edge: 1920,
            max_fps: 200,
            bitrate_bps: 1_000_000,
            hardware_encoder: false,
            ..StreamSettings::default()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stream_delivers_a_keyframe_first() {
        let (sender, mut receiver) = mpsc::channel(4);
        let stream =
            spawn_capture_stream(|| Ok(SyntheticCapturer::new(160, 120)), settings(), sender)
                .unwrap();
        let first = receiver.recv().await.unwrap().unwrap();
        assert!(first.keyframe);
        assert_eq!((first.width, first.height), (160, 120));
        let second = receiver.recv().await.unwrap().unwrap();
        assert!(!second.keyframe);
        stream.stop();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn slow_consumer_skips_frames_before_encoding() {
        let (sender, mut receiver) = mpsc::channel(1);
        let stream =
            spawn_capture_stream(|| Ok(SyntheticCapturer::new(64, 64)), settings(), sender)
                .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(stream.stats().frames_encoded.load(Ordering::Relaxed), 1);
        assert!(stream.stats().frames_skipped.load(Ordering::Relaxed) > 0);
        // Everything received still decodes as one unbroken stream.
        let mut decoder = crate::codec::VideoDecoder::new().unwrap();
        for _ in 0..3 {
            let frame = receiver.recv().await.unwrap().unwrap();
            assert!(decoder.decode(&frame.data).unwrap().is_some());
        }
        stream.stop();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn keyframe_request_is_honored() {
        let (sender, mut receiver) = mpsc::channel(1);
        let stream =
            spawn_capture_stream(|| Ok(SyntheticCapturer::new(64, 64)), settings(), sender)
                .unwrap();
        assert!(receiver.recv().await.unwrap().unwrap().keyframe);
        assert!(!receiver.recv().await.unwrap().unwrap().keyframe);
        stream.request_keyframe();
        // At most one frame was already encoded before the request was seen.
        let next = receiver.recv().await.unwrap().unwrap();
        let after = receiver.recv().await.unwrap().unwrap();
        assert!(next.keyframe || after.keyframe);
        stream.stop();
    }

    struct SourceSwitcher {
        inner: SyntheticCapturer,
        captured: u32,
        changes_on: u32,
    }

    impl ScreenCapturer for SourceSwitcher {
        fn capture(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
            self.captured += 1;
            self.inner.capture(timeout)
        }

        fn take_source_change(&mut self) -> bool {
            self.captured == self.changes_on
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_source_change_makes_the_next_frame_a_keyframe() {
        let (sender, mut receiver) = mpsc::channel(1);
        let stream = spawn_capture_stream(
            || {
                Ok(SourceSwitcher {
                    inner: SyntheticCapturer::new(64, 64),
                    captured: 0,
                    changes_on: 5,
                })
            },
            settings(),
            sender,
        )
        .unwrap();
        let mut decoder = crate::codec::VideoDecoder::new().unwrap();
        let mut keyframes = Vec::new();
        for index in 0..10 {
            let frame = receiver.recv().await.unwrap().unwrap();
            if frame.keyframe {
                keyframes.push(index);
            }
            assert!(
                decoder.decode(&frame.data).unwrap().is_some(),
                "frame {index}"
            );
        }
        stream.stop();
        assert_eq!(keyframes, [0, 4]);
    }

    /// A source on its own clock that shows one frame, moves to another source at the next
    /// capture, which shows nothing until `shows_from`, and then a moving screen.
    struct SwitchesWithoutAFrame {
        inner: SyntheticCapturer,
        captured: u32,
        shows_from: Instant,
    }

    impl ScreenCapturer for SwitchesWithoutAFrame {
        fn capture(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
            self.captured += 1;
            if self.captured > 1 && Instant::now() < self.shows_from {
                std::thread::sleep(timeout);
                return Ok(None);
            }
            self.inner.capture(timeout)
        }

        fn paces_itself(&self) -> bool {
            true
        }

        fn take_source_change(&mut self) -> bool {
            self.captured == 2
        }
    }

    /// Runs [`SwitchesWithoutAFrame`] and returns the first frame after the switch, failing if it
    /// came before the new source showed anything.
    async fn first_frame_after_a_frameless_switch(
        settings: StreamSettings,
        ask_for_keyframes: bool,
    ) -> EncodedFrame {
        let shows_from = Instant::now() + Duration::from_millis(800);
        let (sender, mut receiver) = mpsc::channel(4);
        let stream = spawn_capture_stream(
            move || {
                Ok(SwitchesWithoutAFrame {
                    inner: SyntheticCapturer::new(64, 64),
                    captured: 0,
                    shows_from,
                })
            },
            settings,
            sender,
        )
        .unwrap();
        assert!(receiver.recv().await.unwrap().unwrap().keyframe);
        let mut asks = tokio::time::interval(Duration::from_millis(50));
        let next = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    _ = asks.tick(), if ask_for_keyframes => stream.request_keyframe(),
                    received = receiver.recv() => break received,
                }
            }
        })
        .await
        .expect("frames must resume once the new source shows one");
        let early = shows_from.saturating_duration_since(Instant::now());
        assert!(
            early.is_zero(),
            "a frame was sent {early:?} before the new source showed one"
        );
        stream.stop();
        next.unwrap().unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_frame_from_before_a_source_change_is_never_resent() {
        let next = first_frame_after_a_frameless_switch(settings(), true).await;
        assert!(next.keyframe);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_source_change_without_a_frame_makes_the_next_frame_a_keyframe() {
        let settings = StreamSettings {
            still_refinement: StillRefinement {
                frames: 0,
                ..REFINEMENT
            },
            ..settings()
        };
        let next = first_frame_after_a_frameless_switch(settings, false).await;
        assert!(next.keyframe, "the new source's first frame");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn capture_failure_is_reported_and_ends_the_stream() {
        let (sender, mut receiver) = mpsc::channel(1);
        let _stream = spawn_capture_stream(
            || Err::<SyntheticCapturer, _>(CaptureError::PermissionDenied),
            settings(),
            sender,
        )
        .unwrap();
        assert!(matches!(
            receiver.recv().await,
            Some(Err(StreamError::Capture(CaptureError::PermissionDenied)))
        ));
        assert!(receiver.recv().await.is_none());
    }

    /// Fails `failures` times with a transient error, then fails with `then` (or succeeds).
    struct FlakyCapturer {
        inner: SyntheticCapturer,
        failures: u32,
        then: Option<fn() -> CaptureError>,
    }

    impl ScreenCapturer for FlakyCapturer {
        fn capture(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
            if self.failures > 0 {
                self.failures -= 1;
                return Err(CaptureError::Backend("display is changing modes".into()));
            }
            match self.then.take() {
                Some(error) => Err(error()),
                None => self.inner.capture(timeout),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn transient_capture_failures_are_retried() {
        let (sender, mut receiver) = mpsc::channel(1);
        let stream = spawn_capture_stream(
            || {
                Ok(FlakyCapturer {
                    inner: SyntheticCapturer::new(64, 64),
                    failures: 3,
                    then: None,
                })
            },
            settings(),
            sender,
        )
        .unwrap();
        let first = tokio::time::timeout(Duration::from_secs(5), receiver.recv()).await;
        assert!(first.unwrap().unwrap().unwrap().keyframe);
        stream.stop();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stopping_while_the_consumer_is_behind_does_not_hang() {
        let (sender, mut receiver) = mpsc::channel(1);
        let stream = spawn_capture_stream(
            || {
                Ok(FlakyCapturer {
                    inner: SyntheticCapturer::new(64, 64),
                    failures: 0,
                    then: None,
                })
            },
            settings(),
            sender,
        )
        .unwrap();
        // Leave a frame unread so the channel is full while the stream is stopped.
        assert!(receiver.recv().await.unwrap().is_ok());
        tokio::time::sleep(Duration::from_millis(100)).await;
        let stopped = tokio::task::spawn_blocking(move || stream.stop());
        tokio::time::timeout(Duration::from_secs(5), stopped)
            .await
            .expect("stop must not wait for the consumer")
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn permanent_capture_failure_is_reported_once_room_frees_up() {
        let (sender, mut receiver) = mpsc::channel(1);
        let _stream = spawn_capture_stream(
            || {
                Ok(FlakyCapturer {
                    inner: SyntheticCapturer::new(64, 64),
                    failures: 0,
                    then: Some(|| CaptureError::PermissionDenied),
                })
            },
            settings(),
            sender,
        )
        .unwrap();
        assert!(matches!(
            receiver.recv().await,
            Some(Err(StreamError::Capture(CaptureError::PermissionDenied)))
        ));
        assert!(receiver.recv().await.is_none());
    }

    #[derive(Clone, Copy)]
    enum Shows {
        SecureDesktop,
        StillScreen,
    }

    /// A source on its own clock that shows one frame, then each scripted stretch in turn, then
    /// a moving screen.
    struct ScriptedScreen {
        inner: SyntheticCapturer,
        script: Vec<(Shows, Duration)>,
        started: Option<Instant>,
        hidden_captures: Arc<AtomicU64>,
    }

    impl ScriptedScreen {
        fn new(script: Vec<(Shows, Duration)>, hidden_captures: Arc<AtomicU64>) -> Self {
            Self {
                inner: SyntheticCapturer::new(64, 64),
                script,
                started: None,
                hidden_captures,
            }
        }
    }

    impl ScreenCapturer for ScriptedScreen {
        fn capture(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
            let Some(started) = self.started else {
                self.started = Some(Instant::now());
                return self.inner.capture(timeout);
            };
            let mut elapsed = started.elapsed();
            let shows = self.script.iter().find_map(|&(shows, lasts)| {
                if elapsed < lasts {
                    return Some(shows);
                }
                elapsed -= lasts;
                None
            });
            match shows {
                Some(Shows::SecureDesktop) => {
                    self.hidden_captures.fetch_add(1, Ordering::Relaxed);
                    std::thread::sleep(timeout);
                    Err(CaptureError::SecureDesktop)
                }
                Some(Shows::StillScreen) => {
                    std::thread::sleep(timeout);
                    Ok(None)
                }
                None => self.inner.capture(timeout),
            }
        }

        fn paces_itself(&self) -> bool {
            true
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_secure_desktop_is_reported_once_and_the_stream_goes_on() {
        let hidden_for = Duration::from_millis(600);
        let hidden_captures = Arc::new(AtomicU64::new(0));
        let counted = hidden_captures.clone();
        let (sender, mut receiver) = mpsc::channel(4);
        let stream = spawn_capture_stream(
            move || {
                Ok(ScriptedScreen::new(
                    vec![(Shows::SecureDesktop, hidden_for)],
                    counted,
                ))
            },
            settings(),
            sender,
        )
        .unwrap();
        assert!(receiver.recv().await.unwrap().unwrap().keyframe);
        let mut notices = 0;
        loop {
            match tokio::time::timeout(Duration::from_secs(5), receiver.recv())
                .await
                .expect("frames must resume after the secure desktop")
            {
                Some(Err(StreamError::Capture(CaptureError::SecureDesktop))) => notices += 1,
                Some(Ok(_)) => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(notices, 1, "one notice for the whole hidden stretch");
        // The first frame's refinement falls due while the screen is hidden; waiting for it must
        // not turn into polling the capturer without a timeout.
        let polls = hidden_captures.load(Ordering::Relaxed);
        let at_source_wait =
            u64::try_from(hidden_for.as_millis() / SOURCE_WAIT.as_millis()).unwrap();
        assert!(polls <= 2 * at_source_wait, "{polls} captures while hidden");
        stream.stop();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_secure_desktop_after_a_still_screen_is_reported_again() {
        let hidden_for = Duration::from_millis(300);
        let script = vec![
            (Shows::SecureDesktop, hidden_for),
            (Shows::StillScreen, Duration::from_millis(400)),
            (Shows::SecureDesktop, hidden_for),
        ];
        let script_ends = Instant::now()
            + Duration::from_secs(1)
            + script.iter().map(|&(_, lasts)| lasts).sum::<Duration>();
        let (sender, mut receiver) = mpsc::channel(4);
        let stream = spawn_capture_stream(
            move || Ok(ScriptedScreen::new(script, Arc::default())),
            settings(),
            sender,
        )
        .unwrap();
        assert!(receiver.recv().await.unwrap().unwrap().keyframe);
        let mut notices = 0;
        loop {
            match tokio::time::timeout(Duration::from_secs(5), receiver.recv())
                .await
                .expect("frames must resume after the second secure desktop")
            {
                Some(Err(StreamError::Capture(CaptureError::SecureDesktop))) => notices += 1,
                Some(Ok(_)) if notices == 2 || Instant::now() >= script_ends => break,
                Some(Ok(_)) => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(notices, 2, "one notice for each hidden stretch");
        stream.stop();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_frame_from_before_a_secure_desktop_is_not_resent_after_it() {
        let hidden_for = Duration::from_millis(300);
        let still_for = Duration::from_millis(1000);
        let moves_from = Instant::now() + hidden_for + still_for;
        let (sender, mut receiver) = mpsc::channel(4);
        let stream = spawn_capture_stream(
            move || {
                Ok(ScriptedScreen::new(
                    vec![
                        (Shows::SecureDesktop, hidden_for),
                        (Shows::StillScreen, still_for),
                    ],
                    Arc::default(),
                ))
            },
            settings(),
            sender,
        )
        .unwrap();
        assert!(receiver.recv().await.unwrap().unwrap().keyframe);
        assert!(matches!(
            receiver.recv().await,
            Some(Err(StreamError::Capture(CaptureError::SecureDesktop)))
        ));
        let mut asks = tokio::time::interval(Duration::from_millis(100));
        let resumed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    _ = asks.tick() => stream.request_keyframe(),
                    received = receiver.recv() => break received,
                }
            }
        })
        .await
        .expect("frames must resume once the screen moves");
        assert!(matches!(resumed, Some(Ok(_))), "{resumed:?}");
        let early = moves_from.saturating_duration_since(Instant::now());
        assert!(
            early.is_zero(),
            "a frame from before the secure desktop was sent {early:?} before the screen moved"
        );
        stream.stop();
    }

    /// A source on its own clock that shows one frame and then a still screen.
    struct StillSource {
        inner: SyntheticCapturer,
        shown: bool,
    }

    impl ScreenCapturer for StillSource {
        fn capture(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
            if std::mem::replace(&mut self.shown, true) {
                std::thread::sleep(timeout);
                return Ok(None);
            }
            self.inner.capture(timeout)
        }

        fn paces_itself(&self) -> bool {
            true
        }
    }

    fn still_screen() -> StillSource {
        StillSource {
            inner: SyntheticCapturer::new(64, 64),
            shown: false,
        }
    }

    const REFINEMENT: StillRefinement = StillRefinement {
        delay: Duration::from_millis(150),
        interval: Duration::from_millis(20),
        frames: 3,
    };

    async fn drain(
        receiver: &mut mpsc::Receiver<Result<EncodedFrame, StreamError>>,
        quiet: Duration,
    ) -> Vec<EncodedFrame> {
        let mut frames = Vec::new();
        while let Ok(Some(frame)) = tokio::time::timeout(quiet, receiver.recv()).await {
            frames.push(frame.unwrap());
        }
        frames
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_still_screen_is_refined_a_bounded_number_of_times_and_then_sends_nothing() {
        let (sender, mut receiver) = mpsc::channel(FRAMES_IN_FLIGHT);
        let settings = StreamSettings {
            still_refinement: REFINEMENT,
            ..settings()
        };
        let stream = spawn_capture_stream(|| Ok(still_screen()), settings, sender).unwrap();
        assert!(receiver.recv().await.unwrap().unwrap().keyframe);
        let stats = stream.stats();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            stats.still_refinements.load(Ordering::Relaxed),
            0,
            "refinement starts only once the screen has been still for the delay"
        );
        let refinements = drain(&mut receiver, Duration::from_millis(400)).await;
        assert!(
            refinements.iter().all(|frame| !frame.keyframe),
            "refinement frames predict from the picture the viewer has"
        );
        assert_eq!(stats.still_refinements.load(Ordering::Relaxed), 3);
        assert!(refinements.len() <= 3, "{} frames", refinements.len());
        assert!(
            drain(&mut receiver, Duration::from_millis(300))
                .await
                .is_empty(),
            "a refined still screen sends nothing more"
        );
        assert_eq!(stats.still_refinements.load(Ordering::Relaxed), 3);
        stream.stop();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_keyframe_request_on_a_still_screen_is_answered_and_refined_again() {
        let (sender, mut receiver) = mpsc::channel(FRAMES_IN_FLIGHT);
        let settings = StreamSettings {
            still_refinement: REFINEMENT,
            ..settings()
        };
        let stream = spawn_capture_stream(|| Ok(still_screen()), settings, sender).unwrap();
        assert!(receiver.recv().await.unwrap().unwrap().keyframe);
        drain(&mut receiver, Duration::from_millis(400)).await;
        assert_eq!(stream.stats().still_refinements.load(Ordering::Relaxed), 3);

        stream.request_keyframe();
        let resent = tokio::time::timeout(Duration::from_secs(5), receiver.recv()).await;
        assert!(resent.unwrap().unwrap().unwrap().keyframe);
        drain(&mut receiver, Duration::from_millis(400)).await;
        assert_eq!(stream.stats().still_refinements.load(Ordering::Relaxed), 6);
        stream.stop();
    }

    /// A source on its own clock that shows prepared frames at a fixed rate, like a display
    /// with something moving on it.
    #[cfg(target_os = "macos")]
    struct ClockedSource {
        frames: Vec<CapturedFrame>,
        interval: Duration,
        next: Instant,
        shown: usize,
    }

    #[cfg(target_os = "macos")]
    impl ScreenCapturer for ClockedSource {
        fn capture(&mut self, _timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
            std::thread::sleep(self.next.saturating_duration_since(Instant::now()));
            self.next = (self.next + self.interval).max(Instant::now());
            self.shown += 1;
            Ok(Some(self.frames[self.shown % self.frames.len()].clone()))
        }

        fn paces_itself(&self) -> bool {
            true
        }
    }

    /// Measures the hardware stream at 144 fps without depending on what the screen shows:
    /// `cargo test --release -p dari-media -- --ignored --nocapture hardware_stream_keeps_up`.
    #[cfg(target_os = "macos")]
    #[ignore = "a release-mode throughput measurement"]
    #[tokio::test(flavor = "multi_thread")]
    async fn hardware_stream_keeps_up_with_144_fps() {
        const FPS: u32 = 144;
        let (width, height) = (1920, 1246);
        let mut capturer = SyntheticCapturer::new(width, height);
        let frames: Vec<CapturedFrame> = (0..32)
            .map(|_| {
                let rgba = capturer.render();
                let native =
                    crate::apple::NativeFrame::from_i420(&crate::codec::rgba_to_i420(&rgba));
                CapturedFrame::Native(native.unwrap())
            })
            .collect();
        let (sender, mut receiver) = mpsc::channel(FRAMES_IN_FLIGHT);
        let stream = spawn_capture_stream(
            move || {
                Ok(ClockedSource {
                    frames,
                    interval: Duration::from_secs(1) / FPS,
                    next: Instant::now(),
                    shown: 0,
                })
            },
            StreamSettings {
                max_long_edge: width,
                max_fps: FPS,
                bitrate_bps: 4_000_000,
                hardware_encoder: true,
                ..StreamSettings::default()
            },
            sender,
        )
        .unwrap();
        // Count once the encoder has settled: VideoToolbox in real-time mode slowed down after
        // about three seconds.
        let warm_up = Instant::now();
        while warm_up.elapsed() < Duration::from_secs(4) {
            receiver.recv().await.unwrap().unwrap();
        }
        let encoded_before = stream.stats().frames_encoded.load(Ordering::Relaxed);
        let micros_before = stream.stats().encode_micros.load(Ordering::Relaxed);
        let started = Instant::now();
        let mut received = 0u32;
        while started.elapsed() < Duration::from_secs(5) {
            receiver.recv().await.unwrap().unwrap();
            received += 1;
        }
        let fps = f64::from(received) / started.elapsed().as_secs_f64();
        let stats = stream.stats();
        #[expect(clippy::cast_precision_loss, reason = "report only")]
        let latency_ms = (stats.encode_micros.load(Ordering::Relaxed) - micros_before) as f64
            / (stats.frames_encoded.load(Ordering::Relaxed) - encoded_before).max(1) as f64
            / 1000.0;
        println!(
            "{width}x{height} at {FPS} fps: {fps:.1} fps delivered, {latency_ms:.2} ms latency, {} skipped",
            stats.frames_skipped.load(Ordering::Relaxed)
        );
        assert!(stats.hardware_encoding.load(Ordering::Relaxed));
        stream.stop();
        assert!(fps > f64::from(FPS) * 0.95, "{fps:.1} fps");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_the_receiver_ends_the_thread() {
        let (sender, receiver) = mpsc::channel(1);
        let stream =
            spawn_capture_stream(|| Ok(SyntheticCapturer::new(64, 64)), settings(), sender)
                .unwrap();
        drop(receiver);
        // `stop` joins the thread; it must return rather than hang.
        stream.stop();
    }
}
