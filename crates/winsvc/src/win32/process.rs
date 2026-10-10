use std::ffi::c_void;
use std::io;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::time::Duration;

use windows::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Security::{
    CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, DuplicateTokenEx, SecurityImpersonation,
    TOKEN_ALL_ACCESS, TOKEN_DUPLICATE, TOKEN_QUERY, TokenPrimary,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows::Win32::System::LibraryLoader::{
    LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories,
};
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTS_CONNECTSTATE_CLASS, WTSActive, WTSConnectState, WTSFreeMemory,
    WTSQuerySessionInformationW,
};
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::Win32::System::Threading::{
    CREATE_SUSPENDED, CreateProcessAsUserW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetProcessId,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcess, OpenProcessToken,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_DUP_HANDLE, PROCESS_INFORMATION, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW,
    ResumeThread, STARTUPINFOEXW, STARTUPINFOW, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

use super::token::{LOCAL_SYSTEM, token_user};
use super::{from_wide, owned, raw, same_path, wide};

/// Loads DLLs only from System32. Call it before anything else loads one.
pub(crate) fn restrict_dll_search() -> io::Result<()> {
    // SAFETY: changes only this process's DLL search path.
    unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32)? };
    Ok(())
}

/// Opens a pipe client's process with the rights the service and the helper use later: to check
/// its image, to duplicate handles into it, and to wait for it to exit. While the handle is open,
/// Windows can't give the process ID to another process.
pub(crate) fn open_client_process(pid: u32) -> io::Result<OwnedHandle> {
    // SAFETY: `OpenProcess` takes no pointers.
    let process = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_DUP_HANDLE | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )?
    };
    // SAFETY: `OpenProcess` just returned this handle to us.
    Ok(unsafe { owned(process) })
}

pub(crate) fn image_path(process: &impl AsRawHandle) -> io::Result<PathBuf> {
    let mut buffer = vec![0u16; 32 * 1024];
    let mut len = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
    // SAFETY: `buffer` holds `len` units and outlives the call, which writes at most that many.
    unsafe {
        QueryFullProcessImageNameW(
            raw(process),
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &raw mut len,
        )?;
    }
    Ok(PathBuf::from(String::from_utf16_lossy(
        &buffer[..(len as usize).min(buffer.len())],
    )))
}

pub(crate) fn process_id(process: &impl AsRawHandle) -> u32 {
    // SAFETY: `GetProcessId` only reads the handle and returns 0 for a handle that isn't a
    // process.
    unsafe { GetProcessId(raw(process)) }
}

pub(crate) fn has_exited(process: &impl AsRawHandle) -> bool {
    // SAFETY: a zero timeout only polls the handle's state.
    unsafe { WaitForSingleObject(raw(process), 0) == WAIT_OBJECT_0 }
}

/// Whether `session` is an active session (signed in and connected, locked or not).
pub(crate) fn session_is_active(session: u32) -> io::Result<bool> {
    let mut buffer = PWSTR::null();
    let mut bytes = 0u32;
    // SAFETY: on success the call allocates `buffer`, which holds a `WTS_CONNECTSTATE_CLASS`
    // and is freed below.
    unsafe {
        WTSQuerySessionInformationW(
            None,
            session,
            WTSConnectState,
            &raw mut buffer,
            &raw mut bytes,
        )?;
        let state = if bytes as usize >= size_of::<WTS_CONNECTSTATE_CLASS>() {
            Some(buffer.0.cast::<WTS_CONNECTSTATE_CLASS>().read_unaligned())
        } else {
            None
        };
        WTSFreeMemory(buffer.0.cast());
        Ok(state == Some(WTSActive))
    }
}

#[derive(Debug)]
pub(crate) struct InheritedProcess(HANDLE);

impl InheritedProcess {
    pub(crate) fn adopt(value: usize) -> io::Result<Self> {
        let handle = HANDLE(value as *mut c_void);
        // SAFETY: `GetProcessId` only reads the handle and fails for anything but a process.
        if unsafe { GetProcessId(handle) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(handle))
    }
}

impl AsRawHandle for InheritedProcess {
    fn as_raw_handle(&self) -> std::os::windows::io::RawHandle {
        self.0.0
    }
}

#[derive(Debug)]
pub(crate) struct Job(OwnedHandle);

impl Job {
    pub(crate) fn kill_on_close() -> io::Result<Self> {
        // SAFETY: an unnamed job with default security; the handle is ours to own.
        let job = unsafe { owned(CreateJobObjectW(None, PCWSTR::null())?) };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `limits` is the structure the information class names, and outlives the call.
        unsafe {
            SetInformationJobObject(
                raw(&job),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                u32::try_from(size_of_val(&limits)).unwrap_or(0),
            )?;
        }
        Ok(Self(job))
    }
}

#[derive(Debug)]
pub(crate) struct Launched {
    pub(crate) process: OwnedHandle,
    pub(crate) pid: u32,
}

/// Starts `exe` in `session` as SYSTEM with the session's `winlogon.exe` token stripped of every
/// privilege but `SeChangeNotifyPrivilege`, inside `job`. The new process inherits exactly one
/// handle, a duplicate of `app`; `arguments` turns its value into the command line's arguments.
pub(crate) fn launch_helper(
    session: u32,
    exe: &Path,
    app: &impl AsRawHandle,
    arguments: impl FnOnce(usize) -> String,
    job: &Job,
) -> io::Result<Launched> {
    let token = restricted_winlogon_token(session)?;
    let inheritable = inheritable_duplicate(app)?;
    let mut attributes = HandleList::new(raw(&inheritable))?;

    let exe_text = exe.to_string_lossy();
    let application = wide(&exe_text);
    let mut command_line = wide(&format!(
        "\"{exe_text}\" {}",
        arguments(raw(&inheritable).0 as usize)
    ));
    let mut desktop = wide(r"winsta0\default");
    let startup = STARTUPINFOEXW {
        StartupInfo: STARTUPINFOW {
            cb: u32::try_from(size_of::<STARTUPINFOEXW>()).unwrap_or(0),
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..STARTUPINFOW::default()
        },
        lpAttributeList: attributes.list(),
    };
    let mut created = PROCESS_INFORMATION::default();
    // SAFETY: every pointer refers to a NUL-terminated buffer or structure above that outlives
    // the call, and `startup` is a `STARTUPINFOEXW` as `EXTENDED_STARTUPINFO_PRESENT` says.
    unsafe {
        CreateProcessAsUserW(
            Some(raw(&token)),
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            true,
            CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT,
            None,
            PCWSTR::null(),
            (&raw const startup).cast(),
            &raw mut created,
        )?;
    }
    // SAFETY: `CreateProcessAsUserW` returned these handles to us.
    let (process, thread) = unsafe { (owned(created.hProcess), owned(created.hThread)) };
    // SAFETY: both handles are open; a suspended process that can't join the job never runs.
    unsafe {
        if let Err(error) = AssignProcessToJobObject(raw(&job.0), raw(&process)) {
            let _ended = TerminateProcess(raw(&process), 1);
            return Err(error.into());
        }
        if ResumeThread(raw(&thread)) == u32::MAX {
            let error = io::Error::last_os_error();
            let _ended = TerminateProcess(raw(&process), 1);
            return Err(error);
        }
    }
    Ok(Launched {
        process,
        pid: created.dwProcessId,
    })
}

fn restricted_winlogon_token(session: u32) -> io::Result<OwnedHandle> {
    let token = winlogon_token(session)?;
    // SAFETY: each call writes one handle that is wrapped as owned right after it returns.
    unsafe {
        let mut primary = HANDLE::default();
        DuplicateTokenEx(
            raw(&token),
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &raw mut primary,
        )?;
        let primary = owned(primary);
        let mut restricted = HANDLE::default();
        CreateRestrictedToken(
            raw(&primary),
            DISABLE_MAX_PRIVILEGE,
            None,
            None,
            None,
            &raw mut restricted,
        )?;
        Ok(owned(restricted))
    }
}

/// The token of `session`'s `winlogon.exe`: a process of that name in the session whose image is
/// `winlogon.exe` in the system directory and whose token's user is `LocalSystem`. A process that
/// only shares the name is passed over.
fn winlogon_token(session: u32) -> io::Result<OwnedHandle> {
    let expected = system_directory()?.join("winlogon.exe");
    // SAFETY: the snapshot handle is wrapped as owned right away, and `entry` is a
    // `PROCESSENTRY32W` with `dwSize` set, which both enumeration calls fill in.
    unsafe {
        let snapshot = owned(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?);
        let mut entry = PROCESSENTRY32W {
            dwSize: u32::try_from(size_of::<PROCESSENTRY32W>()).unwrap_or(0),
            ..PROCESSENTRY32W::default()
        };
        let mut listed = Process32FirstW(raw(&snapshot), &raw mut entry);
        while listed.is_ok() {
            let mut entry_session = u32::MAX;
            if from_wide(&entry.szExeFile).eq_ignore_ascii_case("winlogon.exe")
                && ProcessIdToSessionId(entry.th32ProcessID, &raw mut entry_session).is_ok()
                && entry_session == session
                && let Ok(Some(token)) = system_token_of(entry.th32ProcessID, &expected)
            {
                return Ok(token);
            }
            listed = Process32NextW(raw(&snapshot), &raw mut entry);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "no {} running as LocalSystem in session {session}",
            expected.display()
        ),
    ))
}

/// The token of process `pid`, if its image is `image` and its user is `LocalSystem`.
fn system_token_of(pid: u32, image: &Path) -> io::Result<Option<OwnedHandle>> {
    // SAFETY: `OpenProcess` takes no pointers; the handle is wrapped as owned.
    let process = unsafe { owned(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)?) };
    if !same_path(&image_path(&process)?, image) {
        return Ok(None);
    }
    let mut token = HANDLE::default();
    // SAFETY: `token` outlives the call that writes it, and is wrapped as owned.
    let token = unsafe {
        OpenProcessToken(raw(&process), TOKEN_DUPLICATE | TOKEN_QUERY, &raw mut token)?;
        owned(token)
    };
    Ok((token_user(&token)? == LOCAL_SYSTEM).then_some(token))
}

fn system_directory() -> io::Result<PathBuf> {
    let mut buffer = [0u16; 260];
    // SAFETY: the call writes at most `buffer.len()` units into `buffer`.
    let len = unsafe { GetSystemDirectoryW(Some(&mut buffer)) } as usize;
    if len == 0 || len >= buffer.len() {
        return Err(io::Error::last_os_error());
    }
    Ok(PathBuf::from(String::from_utf16_lossy(&buffer[..len])))
}

/// Ends `process` and waits up to `wait` for it to exit. Returns whether it exited.
pub(crate) fn end_process(process: &impl AsRawHandle, wait: Duration) -> bool {
    let milliseconds = u32::try_from(wait.as_millis()).unwrap_or(u32::MAX - 1);
    // SAFETY: both calls only use the handle, which `process` keeps open.
    unsafe {
        let _ended = TerminateProcess(raw(process), 1);
        WaitForSingleObject(raw(process), milliseconds) == WAIT_OBJECT_0
    }
}

fn inheritable_duplicate(process: &impl AsRawHandle) -> io::Result<OwnedHandle> {
    let mut duplicate = HANDLE::default();
    // SAFETY: duplicates within this process; the new handle is wrapped as owned.
    unsafe {
        let this = GetCurrentProcess();
        DuplicateHandle(
            this,
            raw(process),
            this,
            &raw mut duplicate,
            0,
            true,
            DUPLICATE_SAME_ACCESS,
        )?;
        Ok(owned(duplicate))
    }
}

/// A `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` naming one handle, the only one a new process inherits.
struct HandleList {
    // `usize` keeps the opaque list pointer-aligned.
    buffer: Vec<usize>,
    // Boxed so the address the attribute holds survives moves of `HandleList`.
    handles: Box<[HANDLE; 1]>,
}

impl HandleList {
    fn new(handle: HANDLE) -> io::Result<Self> {
        let mut size = 0usize;
        // SAFETY: a size query with no list fails with ERROR_INSUFFICIENT_BUFFER and sets `size`.
        let _sized = unsafe { InitializeProcThreadAttributeList(None, 1, None, &raw mut size) };
        let mut list = Self {
            buffer: vec![0; size.div_ceil(size_of::<usize>())],
            handles: Box::new([handle]),
        };
        // SAFETY: `buffer` holds at least `size` bytes; `handles` is boxed, so the attribute's
        // pointer stays valid until `drop` deletes the list.
        unsafe {
            if let Err(error) =
                InitializeProcThreadAttributeList(Some(list.list()), 1, None, &raw mut size)
            {
                list.buffer.clear();
                return Err(error.into());
            }
            if let Err(error) = UpdateProcThreadAttribute(
                list.list(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(list.handles.as_ptr().cast()),
                size_of::<[HANDLE; 1]>(),
                None,
                None,
            ) {
                DeleteProcThreadAttributeList(list.list());
                list.buffer.clear();
                return Err(error.into());
            }
        }
        Ok(list)
    }

    fn list(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        LPPROC_THREAD_ATTRIBUTE_LIST(self.buffer.as_mut_ptr().cast())
    }
}

impl Drop for HandleList {
    fn drop(&mut self) {
        if !self.buffer.is_empty() {
            // SAFETY: the list was initialized in `new` and is deleted once.
            unsafe { DeleteProcThreadAttributeList(self.list()) };
        }
    }
}
