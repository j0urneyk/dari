//! Display capture with Windows.Graphics.Capture.
//!
//! Windows delivers a frame into a free-threaded frame pool only when the screen changes, and
//! the pool's callback keeps just the newest one. The capture thread takes it at most `max_fps`
//! times a second, and shaders scale it to the stream size and convert it to NV12 on the GPU
//! (see `convert`), ready for the hardware encoder.
//!
//! While Windows shows its secure desktop (a UAC prompt, the lock screen, Ctrl+Alt+Del) the
//! session simply delivers nothing, so a screen that has been still for a while is checked
//! against the input desktop to tell the two apart.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tracing::{debug, warn};
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HANDLE, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, GetUserObjectInformationW,
    OpenInputDesktop, UOI_NAME,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{BOOL, IInspectable, Interface, Ref};

use super::NativeFrame;
use super::convert::Converter;
use super::device::{create_device, enter_apartment};
use super::frame::texture_desc;
use crate::display::CaptureError;
use crate::frame::CapturedFrame;
use crate::scale::fit_within;
use crate::stream::ScreenCapturer;

/// Buffers in the frame pool: one Windows draws into, one waiting for the capture thread, and
/// one the capture thread is converting.
const POOL_BUFFERS: i32 = 3;
const CAPTURE_FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;
/// How long the screen stays still before the input desktop is checked, and how often after.
const SECURE_DESKTOP_PROBE_INTERVAL: Duration = Duration::from_millis(300);

/// Captures one display with Windows.Graphics.Capture.
pub(crate) struct GraphicsCaptureCapturer {
    display: u32,
    max_long_edge: u32,
    max_fps: u32,
    shared: Arc<Shared>,
    /// `None` after the capture stopped on its own; the next capture starts it again.
    running: Option<RunningCapture>,
    converter: Option<Converter>,
    /// When the last frame was handed out. Windows versions before 11 24H2 ignore the session's
    /// minimum update interval, so the frame rate limit is also kept here.
    last_delivered: Option<Instant>,
    /// When a still screen is next checked for the secure desktop.
    next_probe: Instant,
}

impl std::fmt::Debug for GraphicsCaptureCapturer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphicsCaptureCapturer")
            .field("display", &self.display)
            .field("max_long_edge", &self.max_long_edge)
            .field("max_fps", &self.max_fps)
            .finish_non_exhaustive()
    }
}

impl GraphicsCaptureCapturer {
    /// Starts capturing display `display` (the monitor handle's ID, as xcap reports it) at up to
    /// `max_fps`, scaled to fit `max_long_edge`.
    pub(crate) fn open(
        display: u32,
        max_long_edge: u32,
        max_fps: u32,
    ) -> Result<Self, CaptureError> {
        enter_apartment();
        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            return Err(CaptureError::Backend(
                "Windows.Graphics.Capture is not available".into(),
            ));
        }
        let max_fps = max_fps.max(1);
        let shared = Arc::new(Shared::default());
        let running = RunningCapture::start(display, interval(max_fps), &shared)?;
        Ok(Self {
            display,
            max_long_edge,
            max_fps,
            shared,
            running: Some(running),
            converter: None,
            last_delivered: None,
            next_probe: Instant::now() + SECURE_DESKTOP_PROBE_INTERVAL,
        })
    }

    /// Waits until the newest frame is due, or `timeout` passes.
    fn next_frame(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<Direct3D11CaptureFrame>, CaptureError> {
        let deadline = Instant::now() + timeout;
        let mut latest = self.shared.lock();
        loop {
            if let Some(reason) = latest.stopped.take() {
                return Err(CaptureError::Backend(reason));
            }
            let now = Instant::now();
            let due = self
                .last_delivered
                .map_or(now, |last| last + interval(self.max_fps));
            if latest.frame.is_some() && now >= due {
                return Ok(latest.frame.take());
            }
            let wake = if latest.frame.is_some() {
                due.min(deadline)
            } else {
                deadline
            };
            if now >= wake {
                return Ok(None);
            }
            latest = self
                .shared
                .ready
                .wait_timeout(latest, wake - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn convert(&mut self, frame: &Direct3D11CaptureFrame) -> Result<CapturedFrame, CaptureError> {
        let running = self.running.as_mut().ok_or(CaptureError::NoDisplay)?;
        let content = frame.ContentSize().map_err(backend)?;
        let access: IDirect3DDxgiInterfaceAccess = frame
            .Surface()
            .and_then(|surface| surface.cast())
            .map_err(backend)?;
        // SAFETY: A capture surface wraps a Direct3D 11 texture.
        let texture: ID3D11Texture2D = unsafe { access.GetInterface() }.map_err(backend)?;
        let desc = texture_desc(&texture);
        // The pool's buffers keep their size until it is recreated, so a display that grew
        // fills only part of them.
        let size = (
            u32::try_from(content.Width).unwrap_or(0).min(desc.Width),
            u32::try_from(content.Height).unwrap_or(0).min(desc.Height),
        );
        if size.0 == 0 || size.1 == 0 {
            return Err(CaptureError::Backend("captured an empty frame".into()));
        }
        let output = fit_within(size.0, size.1, self.max_long_edge);

        let converter = match &mut self.converter {
            Some(converter) if converter.fits((desc.Width, desc.Height), size, output) => converter,
            slot => slot.insert(
                Converter::new(&running.device, (desc.Width, desc.Height), size, output)
                    .map_err(backend)?,
            ),
        };
        let converted = converter.convert(&texture).map_err(backend)?;
        let frame = NativeFrame::new(converted).ok_or_else(|| {
            CaptureError::Backend("the GPU conversion made an unusable frame".into())
        })?;
        running.resize_pool(content)?;
        Ok(CapturedFrame::Native(frame))
    }
}

impl ScreenCapturer for GraphicsCaptureCapturer {
    fn capture(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
        if self.running.is_none() {
            self.running = Some(RunningCapture::start(
                self.display,
                interval(self.max_fps),
                &self.shared,
            )?);
        }
        let frame = match self.next_frame(timeout) {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                let now = Instant::now();
                if now >= self.next_probe {
                    self.next_probe = now + SECURE_DESKTOP_PROBE_INTERVAL;
                    if input_desktop_is_secure() {
                        return Err(CaptureError::SecureDesktop);
                    }
                }
                return Ok(None);
            }
            Err(error) => {
                self.stop();
                return Err(error);
            }
        };
        let now = Instant::now();
        self.last_delivered = Some(now);
        self.next_probe = now + SECURE_DESKTOP_PROBE_INTERVAL;
        let converted = self.convert(&frame);
        // Hands the buffer back to the pool now rather than whenever the last reference goes.
        let _closed = frame.Close();
        if converted.is_err() {
            // A lost device or a changed display: start over with a new device next time.
            self.stop();
        }
        converted.map(Some)
    }

    fn paces_itself(&self) -> bool {
        true
    }
}

impl GraphicsCaptureCapturer {
    fn stop(&mut self) {
        self.running = None;
        self.converter = None;
        let mut latest = self.shared.lock();
        latest.stopped = None;
        if let Some(frame) = latest.frame.take() {
            let _closed = frame.Close();
        }
    }
}

fn interval(max_fps: u32) -> Duration {
    Duration::from_secs(1) / max_fps.max(1)
}

#[expect(clippy::needless_pass_by_value, reason = "used as a `map_err` adapter")]
fn backend(error: windows::core::Error) -> CaptureError {
    CaptureError::Backend(error.message())
}

/// The newest frame, shared between the frame pool's callback and the capture thread.
#[derive(Default)]
struct Shared {
    latest: Mutex<Latest>,
    ready: Condvar,
}

#[derive(Default)]
struct Latest {
    frame: Option<Direct3D11CaptureFrame>,
    /// Why the capture stopped on its own, until the capture thread has seen it.
    stopped: Option<String>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Latest> {
        self.latest.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn receive(&self, frame: Direct3D11CaptureFrame) {
        let replaced = self.lock().frame.replace(frame);
        if let Some(replaced) = replaced {
            let _closed = replaced.Close();
        }
        self.ready.notify_all();
    }

    fn stop(&self, reason: String) {
        self.lock().stopped = Some(reason);
        self.ready.notify_all();
    }
}

/// A started capture session and everything it calls back into.
struct RunningCapture {
    device: ID3D11Device,
    winrt_device: IDirect3DDevice,
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    pool_size: SizeInt32,
    session: GraphicsCaptureSession,
    frame_arrived: i64,
    closed: i64,
}

impl RunningCapture {
    fn start(display: u32, interval: Duration, shared: &Arc<Shared>) -> Result<Self, CaptureError> {
        let monitor = find_monitor(display).ok_or(CaptureError::DisplayNotFound(display))?;
        let device = create_device().map_err(backend)?;
        let dxgi: IDXGIDevice = device.cast().map_err(backend)?;
        // SAFETY: `dxgi` is the DXGI side of a valid Direct3D 11 device.
        let winrt_device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .and_then(|device| device.cast())
            .map_err(backend)?;
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .map_err(backend)?;
        // SAFETY: `monitor` is a handle Windows just enumerated.
        let item: GraphicsCaptureItem =
            unsafe { interop.CreateForMonitor(monitor) }.map_err(backend)?;
        let pool_size = item.Size().map_err(backend)?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            CAPTURE_FORMAT,
            POOL_BUFFERS,
            pool_size,
        )
        .map_err(backend)?;
        let frame_arrived = pool
            .FrameArrived(&TypedEventHandler::new({
                let shared = shared.clone();
                move |pool: Ref<Direct3D11CaptureFramePool>, _: Ref<IInspectable>| {
                    if let Some(pool) = pool.as_ref()
                        && let Ok(frame) = pool.TryGetNextFrame()
                    {
                        shared.receive(frame);
                    }
                    Ok(())
                }
            }))
            .map_err(backend)?;
        let closed = item
            .Closed(&TypedEventHandler::new({
                let shared = shared.clone();
                move |_: Ref<GraphicsCaptureItem>, _: Ref<IInspectable>| {
                    warn!("the captured display went away");
                    shared.stop("the display was disconnected".into());
                    Ok(())
                }
            }))
            .map_err(backend)?;
        let session = pool.CreateCaptureSession(&item).map_err(backend)?;
        // The viewer draws its own pointer over the picture.
        if let Err(error) = session.SetIsCursorCaptureEnabled(false) {
            debug!(%error, "could not hide the pointer from the capture");
        }
        // Windows 11 can leave out the yellow border around a captured display.
        if let Err(error) = session.SetIsBorderRequired(false) {
            debug!(%error, "the capture border stays");
        }
        // Windows 11 24H2 and later stop drawing frames faster than this.
        if let Err(error) = session.SetMinUpdateInterval(TimeSpan::from(interval)) {
            debug!(%error, "no minimum update interval; pacing frames on the capture thread");
        }
        session.StartCapture().map_err(backend)?;
        let display_id = display;
        debug!(display_id, ?interval, "screen capture started");
        Ok(Self {
            device,
            winrt_device,
            item,
            pool,
            pool_size,
            session,
            frame_arrived,
            closed,
        })
    }

    /// Resizes the pool's buffers when the display's size changed, after the frame that showed
    /// it was converted.
    fn resize_pool(&mut self, content: SizeInt32) -> Result<(), CaptureError> {
        if content != self.pool_size {
            debug!(
                width = content.Width,
                height = content.Height,
                "display size changed"
            );
            self.pool
                .Recreate(&self.winrt_device, CAPTURE_FORMAT, POOL_BUFFERS, content)
                .map_err(backend)?;
            self.pool_size = content;
        }
        Ok(())
    }
}

impl Drop for RunningCapture {
    fn drop(&mut self) {
        let _removed = self.pool.RemoveFrameArrived(self.frame_arrived);
        let _removed = self.item.RemoveClosed(self.closed);
        let _closed = self.session.Close();
        let _closed = self.pool.Close();
    }
}

/// Whether the desktop receiving input is one a user process cannot capture: Winlogon's secure
/// desktop, which shows UAC prompts, the lock screen, and Ctrl+Alt+Del. Winlogon's desktop
/// refuses to be opened by a user process; any other desktop is recognized by its name.
fn input_desktop_is_secure() -> bool {
    // SAFETY: Win32 calls with valid arguments. The desktop handle is closed before returning,
    // and `name` outlives the call that writes at most its size into it.
    unsafe {
        let Ok(desktop) = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS)
        else {
            return true;
        };
        let mut name = [0u16; 64];
        let mut needed = 0u32;
        let read = GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            u32::try_from(std::mem::size_of_val(&name)).unwrap_or(0),
            Some(&raw mut needed),
        );
        let _closed = CloseDesktop(desktop);
        if read.is_err() {
            return false;
        }
        let len = name
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(name.len());
        !String::from_utf16_lossy(&name[..len]).eq_ignore_ascii_case("default")
    }
}

/// Finds the monitor whose handle has the low 32 bits `display`, which is how xcap names
/// displays on Windows.
fn find_monitor(display: u32) -> Option<HMONITOR> {
    unsafe extern "system" fn collect(
        monitor: HMONITOR,
        _context: HDC,
        _bounds: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        // SAFETY: `data` is the address of the vector below, which outlives the enumeration.
        let monitors = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
        monitors.push(monitor);
        true.into()
    }
    let mut monitors: Vec<HMONITOR> = Vec::new();
    // SAFETY: `collect` only pushes onto `monitors`, which lives until the call returns.
    let listed = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(&raw mut monitors as isize),
        )
    };
    if !listed.as_bool() {
        return None;
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "xcap's display IDs are the handle's low 32 bits"
    )]
    monitors
        .into_iter()
        .find(|monitor| monitor.0 as usize as u32 == display)
}
