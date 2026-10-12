//! What is in front on a Windows desktop. The Start menu and its search sit above always-on-top
//! windows, so while one is open it takes the clicks and keys meant for the input window.

use std::path::Path;

use enigo::{Direction, Enigo, Key, Keyboard as _, Settings};
use windows::Win32::Foundation::{CloseHandle, HWND, POINT};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetCursorPos, GetForegroundWindow, GetWindowTextW,
    GetWindowThreadProcessId, WindowFromPoint,
};
use windows::core::PWSTR;

/// The programs that draw the Start menu and its search.
const START_MENU: [&str; 2] = ["StartMenuExperienceHost.exe", "SearchHost.exe"];

/// Closes the Start menu with Escape, as a person would, if it has the keyboard. Returns whether
/// it did.
pub(crate) fn close_start_menu() -> bool {
    // SAFETY: takes no arguments.
    let foreground = unsafe { GetForegroundWindow() };
    if !program(foreground).is_some_and(|name| START_MENU.contains(&name.as_str())) {
        return false;
    }
    Enigo::new(&Settings::default())
        .is_ok_and(|mut keys| keys.key(Key::Escape, Direction::Click).is_ok())
}

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
