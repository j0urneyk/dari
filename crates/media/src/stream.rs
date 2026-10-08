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
}

impl<T: ScreenCapturer + ?Sized> ScreenCapturer for Box<T> {
    fn capture(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
        (**self).capture(timeout)
    }

    fn paces_itself(&self) -> bool {
        (**self).paces_itself()
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
}

impl Default for StreamSettings {
    fn default() -> Self {
        Self {
            max_long_edge: 1920,
            max_fps: 30,
            bitrate_bps: 4_000_000,
            hardware_encoder: true,
        }
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
/// intact. Transient capture failures (a secure desktop on Windows, a display mode change) are
/// retried for up to 30 seconds.
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
    #[expect(clippy::cast_precision_loss, reason = "frame rates are small integers")]
    let mut encoder = VideoEncoder::new(EncoderSettings {
        bitrate_bps: settings.bitrate_bps,
        max_fps: settings.max_fps.max(1) as f32,
        hardware: settings.hardware_encoder,
    })?;

    let interval = Duration::from_secs(1) / settings.max_fps.max(1);
    let mut next_frame = Instant::now();
    let mut failing_since: Option<Instant> = None;
    // A self-paced source sends nothing while the screen is still, so its last frame is kept
    // to answer a keyframe request.
    let mut last_frame: Option<CapturedFrame> = None;
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

        let captured = match capturer.capture(SOURCE_WAIT) {
            Ok(Some(frame)) => {
                failing_since = None;
                if paced_by_source {
                    last_frame = Some(frame.clone());
                }
                frame
            }
            Ok(None) => match &last_frame {
                Some(frame)
                    if control.keyframe_requested.load(Ordering::Relaxed)
                        || encoder.has_lost_frames() =>
                {
                    frame.clone()
                }
                _ => continue,
            },
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

        if consumer_is_behind(sink, control) {
            control.stats.frames_skipped.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let permit = match sink.clone().try_reserve_owned() {
            Ok(permit) => permit,
            Err(mpsc::error::TrySendError::Full(_)) => {
                control.stats.frames_skipped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                debug!("capture sink closed");
                return Ok(());
            }
        };
        let deliver = delivery(permit, control.clone());
        let frame = match captured {
            CapturedFrame::Rgba(frame) => {
                CapturedFrame::Rgba(scaler.fit(frame, settings.max_long_edge))
            }
            #[cfg(any(target_os = "macos", windows))]
            native @ CapturedFrame::Native(_) => native,
        };
        if control.keyframe_requested.swap(false, Ordering::Relaxed) {
            encoder.request_keyframe();
        }
        let submitted = encoder.submit(&frame, deliver);
        control
            .stats
            .hardware_encoding
            .store(encoder.is_hardware(), Ordering::Relaxed);
        if let Err(error) = submitted {
            warn!(%error, "capture stream stopped");
            deliver_error(sink, control, error.into());
            return Ok(());
        }
    }
    Ok(())
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

/// Delivers the stream's last item, an error, without blocking on a consumer that may itself be
/// waiting for this thread to stop.
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

    /// A source on its own clock whose screen goes behind a secure desktop for `hidden` calls
    /// after the first frame, like Windows during a UAC prompt.
    struct HiddenScreen {
        inner: SyntheticCapturer,
        shown: bool,
        hidden: u32,
    }

    impl ScreenCapturer for HiddenScreen {
        fn capture(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
            if std::mem::replace(&mut self.shown, true) && self.hidden > 0 {
                self.hidden -= 1;
                std::thread::sleep(timeout);
                return Err(CaptureError::SecureDesktop);
            }
            self.inner.capture(timeout)
        }

        fn paces_itself(&self) -> bool {
            true
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_secure_desktop_is_reported_once_and_the_stream_goes_on() {
        let (sender, mut receiver) = mpsc::channel(4);
        let stream = spawn_capture_stream(
            || {
                Ok(HiddenScreen {
                    inner: SyntheticCapturer::new(64, 64),
                    shown: false,
                    hidden: 8,
                })
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
        assert_eq!(notices, 1, "eight hidden captures make one notice");
        stream.stop();
    }

    /// A source on its own clock that shows one frame and then a still screen.
    struct StillScreen {
        inner: SyntheticCapturer,
        shown: bool,
    }

    impl ScreenCapturer for StillScreen {
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

    #[tokio::test(flavor = "multi_thread")]
    async fn a_still_screen_sends_nothing_until_a_keyframe_is_requested() {
        let (sender, mut receiver) = mpsc::channel(1);
        let stream = spawn_capture_stream(
            || {
                Ok(StillScreen {
                    inner: SyntheticCapturer::new(64, 64),
                    shown: false,
                })
            },
            settings(),
            sender,
        )
        .unwrap();
        assert!(receiver.recv().await.unwrap().unwrap().keyframe);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            receiver.try_recv().is_err(),
            "a still screen is not re-sent"
        );
        assert_eq!(stream.stats().frames_encoded.load(Ordering::Relaxed), 1);

        // A viewer that lost its decoder state still gets a picture.
        stream.request_keyframe();
        let resent = tokio::time::timeout(Duration::from_secs(5), receiver.recv()).await;
        assert!(resent.unwrap().unwrap().unwrap().keyframe);
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
