//! Privacy permission checks that macOS has no public API for.

use std::ffi::{c_char, c_int, c_void};
use std::sync::OnceLock;

use objc2_core_foundation::CFString;

/// `TCCAccessPreflight(service, options)`: 0 allowed, 1 refused, 2 not asked yet. Never prompts.
type Preflight = unsafe extern "C" fn(service: *const CFString, options: *const c_void) -> c_int;

const RTLD_LAZY: c_int = 1;

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

/// The private TCC framework's preflight, looked up at run time so a macOS without it only
/// loses the check instead of failing to start.
fn preflight() -> Option<Preflight> {
    static PREFLIGHT: OnceLock<Option<Preflight>> = OnceLock::new();
    *PREFLIGHT.get_or_init(|| {
        // SAFETY: Both strings are NUL-terminated; a missing library or symbol returns null.
        let symbol = unsafe {
            let handle = dlopen(
                c"/System/Library/PrivateFrameworks/TCC.framework/TCC".as_ptr(),
                RTLD_LAZY,
            );
            if handle.is_null() {
                return None;
            }
            dlsym(handle, c"TCCAccessPreflight".as_ptr())
        };
        // SAFETY: `TCCAccessPreflight` has this signature on every macOS that has it; the
        // library stays loaded for the life of the process.
        (!symbol.is_null())
            .then(|| unsafe { std::mem::transmute::<*mut c_void, Preflight>(symbol) })
    })
}

/// What macOS answers about this app recording system audio (macOS 14.4 and later), without
/// asking the user; `None` if it can't tell.
///
/// A refused app can still open a process tap: it records nothing but silence, which looks the
/// same as a quiet machine, so this is the only way to know.
pub(crate) fn system_audio_preflight() -> Option<i32> {
    let preflight = preflight()?;
    let service = CFString::from_static_str("kTCCServiceAudioCapture");
    // SAFETY: `service` is a valid CFString for the call, and the options may be null.
    Some(unsafe { preflight(&raw const *service, std::ptr::null()) })
}
