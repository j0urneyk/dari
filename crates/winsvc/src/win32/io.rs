use std::io;
use std::os::windows::io::OwnedHandle;
use std::time::Duration;

use windows::Win32::Foundation::{
    ERROR_IO_PENDING, ERROR_OPERATION_ABORTED, ERROR_PIPE_CONNECTED, HANDLE, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{
    CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};
use windows::core::PCWSTR;

use super::{owned, raw};

#[derive(Debug)]
pub(crate) struct Event(OwnedHandle);

impl Event {
    pub(crate) fn new() -> io::Result<Self> {
        // SAFETY: no name and no security attributes; the returned handle is ours to own.
        let event = unsafe { CreateEventW(None, true, false, PCWSTR::null())? };
        // SAFETY: `CreateEventW` just returned this handle to us.
        Ok(Self(unsafe { owned(event) }))
    }

    pub(crate) fn set(&self) {
        // SAFETY: the handle is an open event for as long as `self` lives.
        let _set = unsafe { SetEvent(self.handle()) };
    }

    /// Waits up to `timeout` for the event. Returns whether it was set.
    pub(crate) fn wait(&self, timeout: Duration) -> bool {
        let milliseconds = u32::try_from(timeout.as_millis()).unwrap_or(INFINITE - 1);
        // SAFETY: the handle is an open event for as long as `self` lives.
        unsafe { WaitForSingleObject(self.handle(), milliseconds) == WAIT_OBJECT_0 }
    }

    pub(super) fn handle(&self) -> HANDLE {
        raw(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    Done(u32),
    TimedOut,
    Stopped,
}

/// Starts one overlapped operation on `file` with `start` and waits until it completes, `timeout`
/// passes, or `stop` is set. A timed-out or stopped operation is cancelled and drained before
/// this returns, so the `OVERLAPPED` on this stack frame is never used after it ends.
pub(super) fn overlapped(
    file: HANDLE,
    start: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>,
    timeout: Option<Duration>,
    stop: Option<&Event>,
) -> io::Result<Outcome> {
    let done = Event::new()?;
    let mut operation = OVERLAPPED {
        hEvent: done.handle(),
        ..OVERLAPPED::default()
    };
    match start(&raw mut operation) {
        Ok(()) => {}
        Err(error) if error.code() == ERROR_IO_PENDING.to_hresult() => {}
        // `ConnectNamedPipe` reports a client that connected before the call this way, and no
        // operation is pending.
        Err(error) if error.code() == ERROR_PIPE_CONNECTED.to_hresult() => {
            return Ok(Outcome::Done(0));
        }
        Err(error) => return Err(error.into()),
    }
    let mut handles = vec![done.handle()];
    if let Some(stop) = stop {
        handles.push(stop.handle());
    }
    let milliseconds = timeout.map_or(INFINITE, |timeout| {
        u32::try_from(timeout.as_millis()).unwrap_or(INFINITE - 1)
    });
    // SAFETY: every handle in `handles` is an open event for the duration of the call.
    let waited = unsafe { WaitForMultipleObjects(&handles, false, milliseconds) };
    let wait_failed =
        waited != WAIT_OBJECT_0 && waited != WAIT_TIMEOUT && waited.0 != WAIT_OBJECT_0.0 + 1;
    let wait_error = wait_failed.then(io::Error::last_os_error);
    let interrupted = if waited == WAIT_OBJECT_0 {
        None
    } else if waited == WAIT_TIMEOUT {
        Some(Outcome::TimedOut)
    } else {
        Some(Outcome::Stopped)
    };
    if interrupted.is_some() {
        // SAFETY: `operation` is the pending operation on `file`.
        let _cancelled = unsafe { CancelIoEx(file, Some(&raw const operation)) };
    }
    let mut transferred = 0u32;
    // SAFETY: waits for the operation started on `file` with `operation`, which is still alive,
    // to finish or finish cancelling.
    let result =
        unsafe { GetOverlappedResult(file, &raw const operation, &raw mut transferred, true) };
    if let Some(error) = wait_error {
        return Err(error);
    }
    match (result, interrupted) {
        (Ok(()), _) => Ok(Outcome::Done(transferred)),
        (Err(error), Some(outcome)) if error.code() == ERROR_OPERATION_ABORTED.to_hresult() => {
            Ok(outcome)
        }
        (Err(error), _) => Err(error.into()),
    }
}
