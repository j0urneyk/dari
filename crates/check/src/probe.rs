//! A window on the host that records what the session's input did once an app received it: the
//! text typed into it (including what an input method composed), clicks, the wheel, and copy
//! shortcuts. The injection-level checks only show that events reached the OS; this shows they
//! had their effect.

use std::sync::{Arc, Mutex, PoisonError};

use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, ModifiersState};
use winit::window::{Window, WindowId, WindowLevel};

/// Text the viewer types as plain keys.
pub(crate) const TYPED_TEXT: &str = "dari7";
/// The keys the viewer types after switching a Korean input method to Hangul: "gks" composes "한".
pub(crate) const HANGUL_KEYS: &str = "gks";
pub(crate) const HANGUL_TEXT: &str = "한";
/// Wheel lines the viewer scrolls down.
pub(crate) const WHEEL_LINES: i16 = 3;

/// Tells the window's event loop that the host is done.
#[derive(Debug)]
pub(crate) struct Finished;

/// What the window received.
#[derive(Debug, Default, Clone)]
pub(crate) struct ProbeLog {
    /// Why the window couldn't open, if it didn't.
    pub(crate) error: Option<String>,
    pub(crate) opened: bool,
    pub(crate) focused: bool,
    /// The window's inner size, in physical pixels.
    pub(crate) size: (u32, u32),
    pub(crate) text: String,
    /// Where left clicks landed, in physical pixels from the window's top-left corner.
    pub(crate) left_clicks: Vec<(f64, f64)>,
    /// Vertical wheel movement; positive scrolls down (towards later content).
    pub(crate) wheel_down: f64,
    /// The modifier held for each C pressed with a shortcut modifier: "control" or "super".
    pub(crate) copy_shortcuts: Vec<&'static str>,
}

impl ProbeLog {
    /// Whether a left click landed in the middle third of the window, where the viewer aims.
    pub(crate) fn clicked_middle(&self) -> bool {
        let (width, height) = (f64::from(self.size.0), f64::from(self.size.1));
        self.left_clicks.iter().any(|&(x, y)| {
            (width / 3.0..=width * 2.0 / 3.0).contains(&x)
                && (height / 3.0..=height * 2.0 / 3.0).contains(&y)
        })
    }
}

pub(crate) type SharedProbe = Arc<Mutex<ProbeLog>>;

pub(crate) fn snapshot(probe: &SharedProbe) -> ProbeLog {
    probe.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

fn record(probe: &SharedProbe, change: impl FnOnce(&mut ProbeLog)) {
    change(&mut probe.lock().unwrap_or_else(PoisonError::into_inner));
}

/// The window. Its event loop runs on the main thread while the host runs on another.
#[derive(Debug)]
pub(crate) struct Probe {
    log: SharedProbe,
    window: Option<Window>,
    cursor: Option<PhysicalPosition<f64>>,
    modifiers: ModifiersState,
    #[cfg(target_os = "macos")]
    input_source: Option<input_source::AsciiInput>,
}

impl Probe {
    pub(crate) fn new(log: SharedProbe) -> Self {
        Self {
            log,
            window: None,
            cursor: None,
            modifiers: ModifiersState::empty(),
            #[cfg(target_os = "macos")]
            input_source: None,
        }
    }
}

impl ApplicationHandler<Finished> for Probe {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // Centred on the primary display, so the viewer's click at the display's centre lands in
        // it; on top, so nothing else on the host takes the click.
        let mut attributes = Window::default_attributes()
            .with_title("dari-check")
            .with_window_level(WindowLevel::AlwaysOnTop);
        if let Some(monitor) = event_loop
            .primary_monitor()
            .or_else(|| event_loop.available_monitors().next())
        {
            let area = monitor.size();
            let size = PhysicalSize::new(area.width * 2 / 5, area.height * 2 / 5);
            let offset = |whole: u32, part: u32| i32::try_from((whole - part) / 2).unwrap_or(0);
            let origin = monitor.position();
            attributes = attributes
                .with_inner_size(size)
                .with_position(PhysicalPosition::new(
                    origin.x + offset(area.width, size.width),
                    origin.y + offset(area.height, size.height),
                ));
        }
        match event_loop.create_window(attributes) {
            Ok(window) => {
                window.set_ime_allowed(true);
                window.focus_window();
                #[cfg(target_os = "macos")]
                {
                    self.input_source = input_source::AsciiInput::select();
                }
                let size = window.inner_size();
                record(&self.log, |log| {
                    log.opened = true;
                    log.size = (size.width, size.height);
                });
                self.window = Some(window);
            }
            Err(error) => record(&self.log, |log| log.error = Some(error.to_string())),
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, Finished: Finished) {
        // Gives the host's user their input source back, here on the main thread.
        #[cfg(target_os = "macos")]
        drop(self.input_source.take());
        event_loop.exit();
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::Resized(size) => {
                record(&self.log, |log| log.size = (size.width, size.height));
            }
            WindowEvent::Focused(focused) => {
                record(&self.log, |log| log.focused |= focused);
            }
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::CursorMoved { position, .. } => self.cursor = Some(position),
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                if let Some(position) = self.cursor {
                    record(&self.log, |log| {
                        log.left_clicks.push((position.x, position.y));
                    });
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // winit's positive y moves content down, which is scrolling up.
                let down = match delta {
                    MouseScrollDelta::LineDelta(_, y) => -f64::from(y),
                    MouseScrollDelta::PixelDelta(position) => -position.y,
                };
                record(&self.log, |log| log.wheel_down += down);
            }
            WindowEvent::Ime(Ime::Commit(text)) => {
                record(&self.log, |log| log.text.push_str(&text));
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let shortcut = if self.modifiers.control_key() {
                    Some("control")
                } else if self.modifiers.super_key() {
                    Some("super")
                } else {
                    None
                };
                let is_c =
                    matches!(&event.logical_key, Key::Character(c) if c.eq_ignore_ascii_case("c"));
                record(&self.log, |log| {
                    if let Some(modifier) = shortcut.filter(|_| is_c) {
                        log.copy_shortcuts.push(modifier);
                    } else if let Some(text) = &event.text
                        && shortcut.is_none()
                    {
                        log.text.extend(text.chars().filter(|c| !c.is_control()));
                    }
                });
            }
            _ => {}
        }
    }
}

/// While the window is open on macOS, types with an ASCII keyboard input source, so the viewer's
/// keys produce [`TYPED_TEXT`] whatever input method the host's user had selected.
#[cfg(target_os = "macos")]
#[allow(
    unsafe_code,
    reason = "three HIToolbox calls, made on the main thread they require"
)]
mod input_source {
    use std::ffi::c_void;

    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn TISCopyCurrentKeyboardInputSource() -> *mut c_void;
        fn TISCopyCurrentASCIICapableKeyboardInputSource() -> *mut c_void;
        fn TISSelectInputSource(source: *mut c_void) -> i32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(object: *const c_void);
    }

    /// The input source to go back to. Created and dropped on the main thread.
    #[derive(Debug)]
    pub(crate) struct AsciiInput(*mut c_void);

    impl AsciiInput {
        pub(crate) fn select() -> Option<Self> {
            // SAFETY: both copies return an owned reference or null, and this runs on the main
            // thread, where HIToolbox's input source functions must be called.
            unsafe {
                let previous = TISCopyCurrentKeyboardInputSource();
                let ascii = TISCopyCurrentASCIICapableKeyboardInputSource();
                if !ascii.is_null() {
                    TISSelectInputSource(ascii);
                    CFRelease(ascii);
                }
                (!previous.is_null()).then_some(Self(previous))
            }
        }
    }

    impl Drop for AsciiInput {
        fn drop(&mut self) {
            // SAFETY: `self.0` is the reference copied in `select`, released exactly once here.
            unsafe {
                TISSelectInputSource(self.0);
                CFRelease(self.0);
            }
        }
    }
}
