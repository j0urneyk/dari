mod desktop;
mod dxgi;
mod eventlog;
mod io;
mod pipe;
mod process;
mod section;
mod token;

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;

use windows::Win32::Foundation::HANDLE;

pub(crate) use desktop::InputThreadDesktop;
pub(crate) use dxgi::DxgiWorld;
pub(crate) use eventlog::{EVENT_SOURCE, EventLog};
pub(crate) use io::Event;
pub(crate) use pipe::Pipe;
pub(crate) use process::{
    InheritedProcess, Job, end_process, has_exited, image_path, launch_helper, open_client_process,
    process_id, restrict_dll_search, session_is_active,
};
pub(crate) use section::AppSections;
pub(crate) use token::{own_identity, own_user};

fn raw(handle: &impl AsRawHandle) -> HANDLE {
    HANDLE(handle.as_raw_handle())
}

/// # Safety
///
/// `handle` must be a valid, open handle that this process owns and that nothing else closes.
unsafe fn owned(handle: HANDLE) -> OwnedHandle {
    // SAFETY: the caller guarantees ownership of an open handle.
    unsafe { OwnedHandle::from_raw_handle(handle.0) }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(buffer: &[u16]) -> String {
    let len = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}

/// Whether two paths name the same file as Windows compares names, without case.
pub(crate) fn same_path(left: &Path, right: &Path) -> bool {
    left.to_string_lossy().to_lowercase() == right.to_string_lossy().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_compare_without_case() {
        assert!(same_path(
            Path::new(r"C:\Program Files\Dari\dari.exe"),
            Path::new(r"c:\program files\dari\DARI.EXE")
        ));
        assert!(!same_path(
            Path::new(r"C:\Program Files\Dari\dari.exe"),
            Path::new(r"C:\Users\me\Downloads\dari.exe")
        ));
    }
}
