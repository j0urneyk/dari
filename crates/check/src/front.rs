//! What is in front on a Windows desktop.

use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, HWND, POINT};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetCursorPos, GetWindowTextW, GetWindowThreadProcessId, WindowFromPoint,
};
use windows::core::PWSTR;

/// The program and title of the top-level window under the pointer, as in
/// `SearchHost.exe "Search"`.
pub(crate) fn window_under_pointer() -> Option<String> {
    let mut point = POINT::default();
    // SAFETY: `point` outlives the call, which writes it.
    unsafe { GetCursorPos(&raw mut point) }.ok()?;
    // SAFETY: neither call takes pointers.
    let window = unsafe { GetAncestor(WindowFromPoint(point), GA_ROOT) };
    let mut title = [0u16; 256];
    // SAFETY: `title` outlives the call, which writes at most its length.
    let len = unsafe { GetWindowTextW(window, &mut title) };
    let title = String::from_utf16_lossy(&title[..usize::try_from(len).unwrap_or(0)]);
    Some(format!("{} {title:?}", program(window)?))
}

/// The file name of the program that owns `window`.
fn program(window: HWND) -> Option<String> {
    let mut pid = 0;
    // SAFETY: `pid` outlives the call, which writes it.
    unsafe { GetWindowThreadProcessId(window, Some(&raw mut pid)) };
    // SAFETY: takes no pointers.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut path = [0u16; 1024];
    let mut len = u32::try_from(path.len()).unwrap_or(u32::MAX);
    // SAFETY: `path` holds `len` units and outlives the call, which writes at most that many.
    let queried = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &raw mut len,
        )
    };
    // SAFETY: `OpenProcess` returned this handle above; it is closed once, here.
    let _closed = unsafe { CloseHandle(process) };
    queried.ok()?;
    let path = String::from_utf16_lossy(&path[..(len as usize).min(path.len())]);
    Some(Path::new(&path).file_name()?.to_string_lossy().into_owned())
}
