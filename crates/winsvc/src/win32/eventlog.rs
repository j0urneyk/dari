use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::EventLog::{
    DeregisterEventSource, EVENTLOG_ERROR_TYPE, EVENTLOG_INFORMATION_TYPE, REPORT_EVENT_TYPE,
    RegisterEventSourceW, ReportEventW,
};
use windows::core::PCWSTR;

use super::wide;

const SOURCE: &str = "DariService";

#[derive(Debug)]
pub(crate) struct EventLog(HANDLE);

impl EventLog {
    pub(crate) fn open() -> Option<Self> {
        let source = wide(SOURCE);
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
