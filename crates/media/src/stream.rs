//! The host's capture → scale → encode loop on a dedicated thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::codec::{CodecError, EncodedFrame, EncoderSettings, VideoEncoder};
use crate::display::CaptureError;
use crate::frame::RgbaFrame;
use crate::scale::FrameScaler;

/// A source of screen frames.
pub trait ScreenCapturer {
    fn capture(&mut self) -> Result<RgbaFrame, CaptureError>;
}

impl<T: ScreenCapturer + ?Sized> ScreenCapturer for Box<T> {
    fn capture(&mut self) -> Result<RgbaFrame, CaptureError> {
        (**self).capture()
    }
}

/// Stream parameters the host may change while streaming.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StreamSettings {
    /// Longest edge of the encoded video, in pixels.
    pub max_long_edge: u32,
    pub max_fps: u32,
    pub bitrate_bps: u32,
}

impl Default for StreamSettings {
    fn default() -> Self {
        Self {
            max_long_edge: 1920,
            max_fps: 30,
            bitrate_bps: 4_000_000,
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
    /// Frames not captured because the consumer had not taken the previous one yet.
    pub frames_skipped: AtomicU64,
}

#[derive(Debug, Default)]
struct StreamControl {
    stop: AtomicBool,
    keyframe_requested: AtomicBool,
    max_fps: AtomicU32,
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

    /// Changes the capture rate without restarting the stream.
    pub fn set_max_fps(&self, max_fps: u32) {
        self.control
            .max_fps
            .store(max_fps.max(1), Ordering::Relaxed);
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

/// Starts capturing on a new thread.
///
/// `open_capturer` runs on that thread, so platform capture handles never cross threads.
/// Encoded frames are delivered to `sink`; a fatal error is delivered as the last item. The
/// thread only captures when `sink` has room, so a slow network drops whole frames *before*
/// encoding and the H.264 reference chain stays intact. Transient capture failures (a secure
/// desktop on Windows, a display mode change) are retried for up to 30 seconds.
pub fn spawn_capture_stream<C, F>(
    open_capturer: F,
    settings: StreamSettings,
    sink: mpsc::Sender<Result<EncodedFrame, StreamError>>,
) -> std::io::Result<CaptureStream>
where
    C: ScreenCapturer,
    F: FnOnce() -> Result<C, CaptureError> + Send + 'static,
{
    let control = Arc::new(StreamControl {
        max_fps: AtomicU32::new(settings.max_fps.max(1)),
        ..StreamControl::default()
    });
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
    control: &StreamControl,
) -> Result<(), StreamError>
where
    C: ScreenCapturer,
    F: FnOnce() -> Result<C, CaptureError>,
{
    let mut capturer = open_capturer()?;
    let mut scaler = FrameScaler::default();
    #[expect(clippy::cast_precision_loss, reason = "frame rates are small integers")]
    let mut encoder = VideoEncoder::new(EncoderSettings {
        bitrate_bps: settings.bitrate_bps,
        max_fps: settings.max_fps.max(1) as f32,
    })?;

    let mut next_frame = Instant::now();
    let mut failing_since: Option<Instant> = None;
    while !control.stop.load(Ordering::Relaxed) {
        let interval = Duration::from_secs(1) / control.max_fps.load(Ordering::Relaxed).max(1);
        let now = Instant::now();
        if now < next_frame {
            // Sleep in short slices so a stop request is noticed promptly.
            std::thread::sleep((next_frame - now).min(Duration::from_millis(50)));
            continue;
        }
        next_frame = (next_frame + interval).max(now);

        let permit = match sink.try_reserve() {
            Ok(permit) => permit,
            Err(mpsc::error::TrySendError::Full(())) => {
                control.stats.frames_skipped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            Err(mpsc::error::TrySendError::Closed(())) => {
                debug!("capture sink closed");
                return Ok(());
            }
        };
        // Errors from here on travel through the reserved permit, so reporting them can never
        // block on a consumer that is itself waiting for this thread to stop.
        let captured = match capturer.capture() {
            Ok(frame) => {
                failing_since = None;
                frame
            }
            Err(error @ CaptureError::Backend(_))
                if now.duration_since(*failing_since.get_or_insert(now))
                    < CAPTURE_FAILURE_LIMIT =>
            {
                debug!(%error, "capture failed; retrying");
                next_frame = now + CAPTURE_RETRY_DELAY;
                continue;
            }
            Err(error) => {
                warn!(%error, "capture stream stopped");
                permit.send(Err(error.into()));
                return Ok(());
            }
        };
        let frame = scaler.fit(captured, settings.max_long_edge);
        if control.keyframe_requested.swap(false, Ordering::Relaxed) {
            encoder.request_keyframe();
        }
        match encoder.encode(&frame) {
            Ok(Some(encoded)) => {
                control.stats.frames_encoded.fetch_add(1, Ordering::Relaxed);
                permit.send(Ok(encoded));
            }
            Ok(None) => {}
            Err(error) => {
                warn!(%error, "capture stream stopped");
                permit.send(Err(error.into()));
                return Ok(());
            }
        }
    }
    Ok(())
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
        fn capture(&mut self) -> Result<RgbaFrame, CaptureError> {
            if self.failures > 0 {
                self.failures -= 1;
                return Err(CaptureError::Backend("display is changing modes".into()));
            }
            match self.then.take() {
                Some(error) => Err(error()),
                None => self.inner.capture(),
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
