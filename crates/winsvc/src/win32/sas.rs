use std::io;

use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::core::{BOOL, PCWSTR, s};

use super::wide;

type SendSas = unsafe extern "system" fn(as_user: BOOL);

pub(crate) fn send_sas() -> io::Result<()> {
    let name = wide("sas.dll");
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let module =
        unsafe { LoadLibraryExW(PCWSTR(name.as_ptr()), None, LOAD_LIBRARY_SEARCH_SYSTEM32)? };
    let library = Library(module);
    // SAFETY: the module stays loaded while `library` lives, and the name is a C string.
    let Some(address) = (unsafe { GetProcAddress(library.0, s!("SendSAS")) }) else {
        return Err(io::Error::last_os_error());
    };
    // SAFETY: sas.dll exports `SendSAS` as `VOID WINAPI SendSAS(BOOL AsUser)`.
    let send =
        unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, SendSas>(address) };
    // SAFETY: the function takes no pointers, and its module stays loaded until `library` drops.
    unsafe { send(false.into()) };
    Ok(())
}

struct Library(HMODULE);

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: the module came from `LoadLibraryExW` and is freed once.
        let _freed = unsafe { FreeLibrary(self.0) };
    }
}
