use std::io;
use std::os::windows::io::AsRawHandle;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use dari_proto::{AppToHelper, HelperToApp};

use crate::command::HelperArgs;
use crate::frames::{MessageReader, write_message};
use crate::tracker::DesktopTracker;
use crate::win32::{
    EventLog, InheritedProcess, InputDesktopSource, Pipe, has_exited, own_identity, process_id,
    restrict_dll_search,
};

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const WRITE_TIME: Duration = Duration::from_secs(5);

pub(crate) fn run(args: &HelperArgs) -> ExitCode {
    // Before anything else can load a DLL.
    let dll_search = restrict_dll_search();
    let log = EventLog::open();
    let result = dll_search.map_err(|error| format!("cannot restrict DLL loading: {error}"));
    match result.and_then(|()| serve(args, log.as_ref())) {
        Ok(reason) => {
            if let Some(log) = &log {
                log.info(&format!("helper exiting: {reason}"));
            }
            ExitCode::SUCCESS
        }
        Err(reason) => {
            if let Some(log) = &log {
                log.error(&format!("helper exiting: {reason}"));
            }
            ExitCode::FAILURE
        }
    }
}

fn serve(args: &HelperArgs, log: Option<&EventLog>) -> Result<&'static str, String> {
    let identity = own_identity().map_err(|error| format!("cannot read its own token: {error}"))?;
    if let Some(log) = log {
        log.info(&format!("helper started, input {}: {identity}", args.input));
    }
    let app = InheritedProcess::adopt(args.app)
        .map_err(|error| format!("the app's process handle is invalid: {error}"))?;
    let pipe = Pipe::open(&args.pipe.path())
        .map_err(|error| format!("cannot connect to the app's pipe: {error}"))?;
    if !server_is_app(&pipe, &app)
        .map_err(|error| format!("cannot identify the pipe's server: {error}"))?
    {
        return Err("the pipe's server isn't the app the service vetted".into());
    }

    let mut tracker = DesktopTracker::default();
    let mut source = InputDesktopSource;
    let mut messages = MessageReader::<AppToHelper>::new();
    let mut next_poll = Instant::now();
    loop {
        if Instant::now() >= next_poll {
            if let Some(desktop) = tracker.poll(&mut source) {
                let deadline = Instant::now() + WRITE_TIME;
                if write_message(&pipe, &HelperToApp::DesktopChanged(desktop), Some(deadline))
                    .is_err()
                {
                    return Ok("writing to the app's pipe failed");
                }
            }
            if has_exited(&app) {
                return Ok("the app exited");
            }
            next_poll = Instant::now() + POLL_INTERVAL;
        }
        match messages.read(&pipe, Some(next_poll)) {
            Ok(Some(_ignored)) => {}
            Ok(None) => return Ok("the app closed its pipe"),
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                return Ok("the app sent an invalid message");
            }
            Err(_) => return Ok("reading the app's pipe failed"),
        }
    }
}

pub(crate) fn server_is_app(pipe: &Pipe, app: &impl AsRawHandle) -> io::Result<bool> {
    Ok(pipe.server_process_id()? == process_id(app))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_pipe;
    use crate::win32::open_client_process;

    #[test]
    fn the_helper_refuses_a_server_that_isnt_the_app() {
        let (_server, path) = test_pipe();
        let client = Pipe::open(&path).unwrap();
        let mut other = std::process::Command::new("cmd.exe")
            .args(["/c", "exit"])
            .spawn()
            .unwrap();
        assert!(!server_is_app(&client, &other).unwrap());
        other.wait().unwrap();

        let server_process = open_client_process(std::process::id()).unwrap();
        assert!(server_is_app(&client, &server_process).unwrap());
    }
}
