//! COM and Media Foundation setup, and the Direct3D 11 device that capture, conversion, and
//! encoding share.

use std::sync::OnceLock;

use windows::Win32::Foundation::{E_POINTER, HMODULE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device, ID3D11Multithread,
};
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_NOSOCKET, MFStartup};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::{HRESULT, Interface};

/// Joins this thread to the multithreaded apartment, which Windows Runtime capture and Media
/// Foundation need. A thread already in an apartment stays in it, and the thread never leaves: it is only
/// ever a capture or encoding thread, or a test's.
pub(super) fn enter_apartment() {
    // SAFETY: No reserved argument is passed. A failure (the thread is already in a
    // single-threaded apartment) leaves the thread as it was, which COM objects created in it
    // still work with.
    let _result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

/// Joins this thread to the multithreaded apartment and starts Media Foundation, once for the
/// life of the process.
pub(super) fn start_media_foundation() -> Result<(), HRESULT> {
    static STARTED: OnceLock<Result<(), HRESULT>> = OnceLock::new();
    enter_apartment();
    *STARTED.get_or_init(|| {
        // SAFETY: Called once; Media Foundation stays started until the process exits.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }.map_err(|error| error.code())
    })
}

/// Creates a Direct3D 11 device on the default adapter, safe to use from several threads, with
/// the video support a hardware encoder needs where the driver has it (the basic display driver
/// in VMs and CI runners does not).
pub(super) fn create_device() -> windows::core::Result<ID3D11Device> {
    let create = |flags: D3D11_CREATE_DEVICE_FLAG| {
        let mut device = None;
        // SAFETY: `device` is a valid out pointer, and every other argument is a plain value.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                flags,
                None,
                D3D11_SDK_VERSION,
                Some(&raw mut device),
                None,
                None,
            )
        }?;
        device.ok_or_else(|| windows::core::Error::from(E_POINTER))
    };
    let device = create(D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT)
        .or_else(|_| create(D3D11_CREATE_DEVICE_BGRA_SUPPORT))?;
    // Windows.Graphics.Capture and the Media Foundation encoder call into the device from their
    // own threads while the capture thread uses its immediate context.
    // SAFETY: Turning on the device's own locking has no preconditions.
    let _was_protected = unsafe {
        device
            .cast::<ID3D11Multithread>()?
            .SetMultithreadProtected(true)
    };
    Ok(device)
}
