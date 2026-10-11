use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};

use dari_proto::SecureDesktopControl;
use windows::Win32::Foundation::{E_UNEXPECTED, ERROR_CANCELLED, HANDLE};
use windows::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows::core::PCWSTR;

use super::wide;
use crate::secure_desktop::PolicyChange;

/// The machine's policy, or `None` when `dari-service.exe` isn't installed beside the app, so
/// there is no helper for it to govern.
pub fn secure_desktop_control() -> Option<SecureDesktopControl> {
    service_exe()?;
    Some(SecureDesktopControl::read())
}

/// Runs `dari-service.exe policy on|off` elevated, which shows a UAC prompt, and waits for it.
/// Blocks until the prompt is answered and the command exits.
pub fn change_secure_desktop_control(to: SecureDesktopControl) -> PolicyChange {
    let Some(service) = service_exe() else {
        return PolicyChange::Failed("dari-service.exe isn't installed beside Dari".into());
    };
    let arguments = match to {
        SecureDesktopControl::On => "policy on",
        SecureDesktopControl::Off => "policy off",
    };
    match run_elevated(&service, arguments) {
        Err(error) if error.code() == ERROR_CANCELLED.to_hresult() => PolicyChange::Declined,
        Err(error) => PolicyChange::Failed(error.message()),
        Ok(_) if SecureDesktopControl::read() == to => PolicyChange::Changed(to),
        Ok(code) => {
            PolicyChange::Failed(format!("dari-service.exe {arguments} exited with {code}"))
        }
    }
}

fn service_exe() -> Option<PathBuf> {
    let path = std::env::current_exe()
        .ok()?
        .with_file_name("dari-service.exe");
    path.is_file().then_some(path)
}

fn run_elevated(file: &Path, arguments: &str) -> windows::core::Result<u32> {
    let file = wide(&file.to_string_lossy());
    let arguments = wide(arguments);
    let verb = wide("runas");
    let mut info = SHELLEXECUTEINFOW {
        cbSize: u32::try_from(size_of::<SHELLEXECUTEINFOW>()).unwrap_or(0),
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(arguments.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    // SAFETY: `info` is initialized with its size, and the strings it points to are
    // NUL-terminated and outlive the call.
    unsafe { ShellExecuteExW(&raw mut info)? };
    if info.hProcess.is_invalid() {
        return Err(windows::core::Error::new(
            E_UNEXPECTED,
            "runas started no process",
        ));
    }
    // SAFETY: `SEE_MASK_NOCLOSEPROCESS` hands the new process's handle to the caller, which
    // closes it when `process` drops.
    let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess.0) };
    let handle = HANDLE(process.as_raw_handle());
    let mut code = 0u32;
    // SAFETY: `handle` is an open process handle, and `code` outlives the call that writes it.
    unsafe {
        WaitForSingleObject(handle, INFINITE);
        GetExitCodeProcess(handle, &raw mut code)?;
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests may panic")]

    use super::*;

    #[test]
    fn without_dari_service_beside_the_app_there_is_no_policy_to_show() {
        assert!(
            !std::env::current_exe()
                .unwrap()
                .with_file_name("dari-service.exe")
                .exists()
        );
        assert_eq!(secure_desktop_control(), None);
        assert!(matches!(
            change_secure_desktop_control(SecureDesktopControl::Off),
            PolicyChange::Failed(_)
        ));
    }

    #[test]
    #[ignore = "shows a UAC prompt unless the test runs elevated"]
    fn an_elevated_command_reports_its_exit_code() {
        let cmd = Path::new(&std::env::var("SystemRoot").unwrap()).join(r"System32\cmd.exe");
        assert_eq!(run_elevated(&cmd, "/c exit 7").unwrap(), 7);
    }
}
