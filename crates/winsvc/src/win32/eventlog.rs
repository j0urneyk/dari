use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::EventLog::{
    DeregisterEventSource, EVENTLOG_ERROR_TYPE, EVENTLOG_INFORMATION_TYPE, EVENTLOG_WARNING_TYPE,
    REPORT_EVENT_TYPE, RegisterEventSourceW, ReportEventW,
};
use windows::core::PCWSTR;

use super::wide;

pub(crate) const EVENT_SOURCE: &str = "DariService";

#[derive(Debug)]
pub(crate) struct EventLog(HANDLE);

impl EventLog {
    pub(crate) fn open() -> Option<Self> {
        let source = wide(EVENT_SOURCE);
        // SAFETY: `source` is NUL-terminated and outlives the call.
        unsafe { RegisterEventSourceW(PCWSTR::null(), PCWSTR(source.as_ptr())) }
            .ok()
            .map(Self)
    }

    pub(crate) fn info(&self, message: &str) {
        self.report(EVENTLOG_INFORMATION_TYPE, 1, message);
    }

    pub(crate) fn error(&self, message: &str) {
        self.report(EVENTLOG_ERROR_TYPE, 2, message);
    }

    pub(crate) fn input_helper_started(&self, message: &str) {
        self.report(EVENTLOG_WARNING_TYPE, 3, message);
    }

    pub(crate) fn policy_changed(&self, message: &str) {
        self.report(EVENTLOG_INFORMATION_TYPE, 4, message);
    }

    /// `id` stays within 1 to 1000, the IDs whose message `EventCreate.exe`, the registered
    /// message file, renders.
    fn report(&self, kind: REPORT_EVENT_TYPE, id: u32, message: &str) {
        let message = wide(message);
        let strings = [PCWSTR(message.as_ptr())];
        // SAFETY: the handle is open for as long as `self` lives, and `strings` points at one
        // NUL-terminated string that outlives the call.
        let _reported = unsafe { ReportEventW(self.0, kind, 0, id, None, 0, Some(&strings), None) };
    }
}

impl Drop for EventLog {
    fn drop(&mut self) {
        // SAFETY: the handle came from `RegisterEventSourceW` and is closed once.
        let _closed = unsafe { DeregisterEventSource(self.0) };
    }
}
