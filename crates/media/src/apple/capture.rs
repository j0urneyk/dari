//! Display capture with ScreenCaptureKit.
//!
//! ScreenCaptureKit delivers frames on its own clock, at most `max_fps` and only when the screen
//! changes, already scaled to the stream size and converted to NV12 on the GPU. The capture
//! thread waits for the newest frame; frames it did not get to in time are simply replaced.

use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_graphics::{
    CGDisplayCopyDisplayMode, CGDisplayMode, kCGDisplayStreamYCbCrMatrix_ITU_R_601_4,
};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType,
};
use tracing::{debug, warn};

use super::{NATIVE_PIXEL_FORMAT, NativeFrame};
use crate::display::CaptureError;
use crate::frame::CapturedFrame;
use crate::scale::fit_within;
use crate::stream::ScreenCapturer;

/// How long ScreenCaptureKit may take to list displays or start and stop a stream.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Frames ScreenCaptureKit may have in flight. The stream holds at most three at once (the
/// newest, the one being encoded, and the last one kept for keyframe requests).
const QUEUE_DEPTH: isize = 5;
/// `SCStreamErrorUserDeclined`: Screen Recording permission is missing.
const USER_DECLINED: isize = -3801;

/// Captures one display with ScreenCaptureKit.
pub(crate) struct ScreenCaptureKitCapturer {
    display: u32,
    max_long_edge: u32,
    max_fps: u32,
    running: RunningStream,
    shared: Arc<Shared>,
    /// The sequence number of the last frame handed out.
    delivered: u64,
    /// The stream stopped on its own and must be restarted before capturing again.
    stopped: bool,
}

impl std::fmt::Debug for ScreenCaptureKitCapturer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreenCaptureKitCapturer")
            .field("display", &self.display)
            .field("max_long_edge", &self.max_long_edge)
            .field("max_fps", &self.max_fps)
            .finish_non_exhaustive()
    }
}

impl ScreenCaptureKitCapturer {
    /// Starts capturing display `display` (a `CGDirectDisplayID`) at up to `max_fps`, scaled to
    /// fit `max_long_edge`.
    pub(crate) fn open(
        display: u32,
        max_long_edge: u32,
        max_fps: u32,
    ) -> Result<Self, CaptureError> {
        let shared = Arc::new(Shared::default());
        let running = start_stream(display, max_long_edge, max_fps, &shared)?;
        Ok(Self {
            display,
            max_long_edge,
            max_fps,
            running,
            shared,
            delivered: 0,
            stopped: false,
        })
    }

    fn restart(&mut self) -> Result<(), CaptureError> {
        stop_stream(&self.running.stream);
        self.running = start_stream(self.display, self.max_long_edge, self.max_fps, &self.shared)?;
        self.stopped = false;
        Ok(())
    }
}

impl ScreenCapturer for ScreenCaptureKitCapturer {
    fn capture(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
        if self.stopped {
            self.restart()?;
        }
        let deadline = Instant::now() + timeout;
        let mut latest = self.shared.lock();
        loop {
            if let Some(reason) = latest.stopped.take() {
                drop(latest);
                self.stopped = true;
                return Err(CaptureError::Backend(reason));
            }
            if latest.sequence != self.delivered {
                self.delivered = latest.sequence;
                return Ok(latest.frame.clone().map(CapturedFrame::Native));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            latest = self
                .shared
                .ready
                .wait_timeout(latest, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn paces_itself(&self) -> bool {
        true
    }
}

impl Drop for ScreenCaptureKitCapturer {
    fn drop(&mut self) {
        stop_stream(&self.running.stream);
    }
}

/// A started stream and what it calls back into.
struct RunningStream {
    stream: Retained<SCStream>,
    /// The stream's output and delegate, kept alive for as long as the stream.
    _output: Retained<FrameOutput>,
    /// The queue ScreenCaptureKit calls the output on.
    _queue: DispatchRetained<DispatchQueue>,
}

/// The newest frame, shared between ScreenCaptureKit's queue and the capture thread.
#[derive(Default)]
struct Shared {
    latest: Mutex<Latest>,
    ready: Condvar,
}

#[derive(Default)]
struct Latest {
    frame: Option<NativeFrame>,
    /// Counts delivered frames, so the capture thread can tell a new frame from the last one.
    sequence: u64,
    /// Why the stream stopped on its own, until the capture thread has seen it.
    stopped: Option<String>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Latest> {
        self.latest.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn receive(&self, sample: &CMSampleBuffer) {
        if frame_status(sample) != Some(SCFrameStatus::Complete) {
            return;
        }
        // SAFETY: The sample buffer is valid for the duration of the callback.
        let Some(frame) = (unsafe { sample.image_buffer() }).and_then(NativeFrame::new) else {
            return;
        };
        let mut latest = self.lock();
        latest.frame = Some(frame);
        latest.sequence += 1;
        drop(latest);
        self.ready.notify_all();
    }

    fn stop(&self, reason: String) {
        self.lock().stopped = Some(reason);
        self.ready.notify_all();
    }
}

/// The `SCStreamFrameInfoStatus` attachment of a ScreenCaptureKit sample.
fn frame_status(sample: &CMSampleBuffer) -> Option<SCFrameStatus> {
    // SAFETY: The sample buffer is valid; not creating the array avoids mutating it.
    let attachments = unsafe { sample.sample_attachments_array(false) }?;
    // SAFETY: Sample attachment arrays hold CFDictionary values with CFString keys.
    let attachments: CFRetained<CFArray<CFDictionary<CFString, CFType>>> =
        unsafe { CFRetained::cast_unchecked(attachments) };
    // SAFETY: Reading an immutable static ScreenCaptureKit constant. `NSString` is toll-free
    // bridged to `CFString`.
    let key: &CFString = unsafe { &*std::ptr::from_ref(SCStreamFrameInfoStatus).cast() };
    let status = attachments.get(0)?.get(key)?.downcast::<CFNumber>().ok()?;
    status.as_isize().map(SCFrameStatus)
}

define_class!(
    /// Receives ScreenCaptureKit's frames and stop notifications.
    #[unsafe(super(NSObject))]
    #[name = "DariFrameOutput"]
    #[ivars = Arc<Shared>]
    struct FrameOutput;

    unsafe impl NSObjectProtocol for FrameOutput {}

    unsafe impl SCStreamOutput for FrameOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_did_output_sample_buffer(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind == SCStreamOutputType::Screen {
                self.ivars().receive(sample);
            }
        }
    }

    unsafe impl SCStreamDelegate for FrameOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn stream_did_stop_with_error(&self, _stream: &SCStream, error: &NSError) {
            warn!(error = %error.localizedDescription(), "screen capture stream stopped");
            self.ivars().stop(error.localizedDescription().to_string());
        }
    }
);

impl FrameOutput {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(shared);
        // SAFETY: `NSObject`'s designated initializer.
        unsafe { msg_send![super(this), init] }
    }
}

/// ScreenCaptureKit objects created on one thread and used on another.
struct Handoff<T>(T);

// SAFETY: ScreenCaptureKit's content, filter, and error objects are immutable snapshots that
// Apple documents as usable from any thread; they only cross from a completion handler to the
// thread waiting for it.
unsafe impl<T> Send for Handoff<T> {}

fn start_stream(
    display: u32,
    max_long_edge: u32,
    max_fps: u32,
    shared: &Arc<Shared>,
) -> Result<RunningStream, CaptureError> {
    let target = find_display(display)?;
    // SAFETY: Creating a filter for one display, excluding no windows.
    let filter = unsafe {
        SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            &target,
            &NSArray::new(),
        )
    };
    let config = configuration(display, &target, max_long_edge, max_fps);
    let output = FrameOutput::new(shared.clone());
    let delegate = ProtocolObject::from_ref(&*output);
    // SAFETY: The filter, configuration, and delegate are valid; the delegate is kept alive by
    // the capturer for the stream's lifetime.
    let stream = unsafe {
        SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &config,
            Some(delegate),
        )
    };
    let queue = DispatchQueue::new("dev.dari.capture", None);
    // SAFETY: The output stays alive as long as the stream (both are owned by the capturer).
    unsafe {
        stream.addStreamOutput_type_sampleHandlerQueue_error(
            ProtocolObject::from_ref(&*output),
            SCStreamOutputType::Screen,
            Some(&queue),
        )
    }
    .map_err(|error| capture_error(&error))?;

    let (done, started) = mpsc::channel();
    let handler = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: ScreenCaptureKit passes a valid error or null.
        let error = unsafe { Retained::retain(error) }.map(Handoff);
        let _sent = done.send(error);
    });
    // SAFETY: The completion handler is a valid block that outlives the call.
    unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };
    match started.recv_timeout(CALL_TIMEOUT) {
        Ok(None) => {
            let display_id = display;
            debug!(
                display_id,
                max_long_edge, max_fps, "screen capture stream started"
            );
            Ok(RunningStream {
                stream,
                _output: output,
                _queue: queue,
            })
        }
        Ok(Some(Handoff(error))) => Err(capture_error(&error)),
        Err(_) => Err(CaptureError::Backend(
            "screen capture did not start in time".into(),
        )),
    }
}

/// Stops a stream and waits briefly, so a stream reopened right after does not overlap it.
fn stop_stream(stream: &SCStream) {
    let (done, stopped) = mpsc::channel();
    let handler = RcBlock::new(move |_error: *mut NSError| {
        let _sent = done.send(());
    });
    // SAFETY: The completion handler is a valid block that outlives the call.
    unsafe { stream.stopCaptureWithCompletionHandler(Some(&handler)) };
    let _stopped = stopped.recv_timeout(CALL_TIMEOUT);
}

fn find_display(display: u32) -> Result<Retained<SCDisplay>, CaptureError> {
    let (done, listed) = mpsc::channel();
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: ScreenCaptureKit passes valid objects or null.
            let result = match unsafe { Retained::retain(content) } {
                Some(content) => Ok(Handoff(content)),
                None => Err(unsafe { Retained::retain(error) }.map(Handoff)),
            };
            let _sent = done.send(result);
        },
    );
    // SAFETY: The completion handler is a valid block that outlives the call.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
    let content = match listed.recv_timeout(CALL_TIMEOUT) {
        Ok(Ok(Handoff(content))) => content,
        Ok(Err(Some(Handoff(error)))) => return Err(capture_error(&error)),
        Ok(Err(None)) => return Err(CaptureError::Backend("no shareable content".into())),
        Err(_) => {
            return Err(CaptureError::Backend("listing displays timed out".into()));
        }
    };
    // SAFETY: Reading the snapshot's display list.
    let displays = unsafe { content.displays() };
    displays
        .iter()
        // SAFETY: Reading a display's ID.
        .find(|candidate| unsafe { candidate.displayID() } == display)
        .ok_or(CaptureError::DisplayNotFound(display))
}

fn configuration(
    display: u32,
    target: &SCDisplay,
    max_long_edge: u32,
    max_fps: u32,
) -> Retained<SCStreamConfiguration> {
    // The display mode's pixel size is the panel's real resolution, even in a scaled mode.
    let mode = CGDisplayCopyDisplayMode(display);
    let mut native = (
        CGDisplayMode::pixel_width(mode.as_deref()),
        CGDisplayMode::pixel_height(mode.as_deref()),
    );
    if native.0 == 0 || native.1 == 0 {
        // SAFETY: Reading the display's size in points.
        native = unsafe {
            (
                usize::try_from(target.width()).unwrap_or(0),
                usize::try_from(target.height()).unwrap_or(0),
            )
        };
    }
    let (width, height) = fit_within(
        u32::try_from(native.0).unwrap_or(u32::MAX),
        u32::try_from(native.1).unwrap_or(u32::MAX),
        max_long_edge,
    );
    // SAFETY: Creating a configuration and setting plain properties on it.
    unsafe {
        let config = SCStreamConfiguration::new();
        config.setWidth(width as usize);
        config.setHeight(height as usize);
        config.setMinimumFrameInterval(CMTime::new(1, i32::try_from(max_fps.max(1)).unwrap_or(1)));
        config.setPixelFormat(NATIVE_PIXEL_FORMAT);
        config.setColorMatrix(kCGDisplayStreamYCbCrMatrix_ITU_R_601_4);
        // The viewer draws its own pointer over the picture.
        config.setShowsCursor(false);
        config.setQueueDepth(QUEUE_DEPTH);
        config
    }
}

fn capture_error(error: &NSError) -> CaptureError {
    if error.code() == USER_DECLINED {
        CaptureError::PermissionDenied
    } else {
        CaptureError::Backend(error.localizedDescription().to_string())
    }
}
