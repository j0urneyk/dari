use std::io;
use std::os::windows::io::AsRawHandle;
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread::{self, ScopedJoinHandle};
use std::time::{Duration, Instant};

use dari_proto::{AppToHelper, HelperToApp};

use crate::channel::{AppChannel, Offered, Outbox, SectionFactory};
use crate::command::HelperArgs;
use crate::frames::{MessageReader, write_message};
use crate::screen::{DesktopWorld, ScreenEvent, ScreenMachine};
use crate::win32::{
    AppSections, DxgiWorld, EventLog, InheritedProcess, Pipe, has_exited, own_identity, process_id,
    restrict_dll_search,
};

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const WRITE_TIME: Duration = Duration::from_secs(5);
/// The app is untrusted: a full queue stops the reader, so the pipe pushes back on the app
/// instead of the helper's memory growing.
const QUEUED_COMMANDS: usize = 8;
const SCREEN_STOPPED: &str = "the screen thread stopped";

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
    converse(&pipe, &app, DxgiWorld::default)
}

/// Talks to the app until either side stops. This thread is the pipe's only reader; a screen
/// thread is its only writer, so the pipe's order is the order things happened.
fn converse<W: DesktopWorld>(
    pipe: &Pipe,
    app: &InheritedProcess,
    world: impl FnOnce() -> W + Send,
) -> Result<&'static str, String> {
    thread::scope(|scope| {
        let (commands, received) = mpsc::sync_channel(QUEUED_COMMANDS);
        let screen = scope.spawn(move || {
            let log = EventLog::open();
            let machine = ScreenMachine::new(world(), Instant::now());
            let channel = AppChannel::new(PipeOutbox(pipe), AppSections::new(app));
            run_screen(machine, channel, &received, log.as_ref())
        });
        let reason = read_app(pipe, app, &commands, &screen);
        drop(commands);
        let screen_reason = screen
            .join()
            .map_err(|_| "the screen thread panicked".to_owned())?;
        Ok(if reason == SCREEN_STOPPED {
            screen_reason
        } else {
            reason
        })
    })
}

#[derive(Debug)]
enum ScreenCommand {
    SelectDisplay(u32),
    RequestFrame,
}

fn read_app(
    pipe: &Pipe,
    app: &InheritedProcess,
    commands: &mpsc::SyncSender<ScreenCommand>,
    screen: &ScopedJoinHandle<'_, &'static str>,
) -> &'static str {
    let mut messages = MessageReader::<AppToHelper>::new();
    loop {
        let command = match messages.read(pipe, Some(Instant::now() + POLL_INTERVAL)) {
            Ok(Some(AppToHelper::SelectDisplay(display))) => {
                Some(ScreenCommand::SelectDisplay(display))
            }
            Ok(Some(AppToHelper::RequestFrame)) => Some(ScreenCommand::RequestFrame),
            Ok(Some(AppToHelper::Input(_))) => None,
            Ok(Some(AppToHelper::Stop)) => return "the app ended the link",
            Ok(None) => return "the app closed its pipe",
            Err(error) if error.kind() == io::ErrorKind::TimedOut => None,
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                return "the app sent an invalid message";
            }
            Err(_) => return "reading the app's pipe failed",
        };
        if let Some(command) = command
            && commands.send(command).is_err()
        {
            return SCREEN_STOPPED;
        }
        if has_exited(app) {
            return "the app exited";
        }
        if screen.is_finished() {
            return SCREEN_STOPPED;
        }
    }
}

fn run_screen<W: DesktopWorld, F: SectionFactory>(
    mut machine: ScreenMachine<W>,
    mut channel: AppChannel<PipeOutbox<'_>, F>,
    commands: &mpsc::Receiver<ScreenCommand>,
    log: Option<&EventLog>,
) -> &'static str {
    const WRITE_FAILED: &str = "writing to the app failed";
    loop {
        if deliver(machine.step(Instant::now()), &mut channel, log).is_err() {
            return WRITE_FAILED;
        }
        if let Some(frame) = machine.dirty_frame() {
            match channel.offer(frame.layout(), frame.display, |slot| frame.write_into(slot)) {
                Ok(Offered::Published | Offered::Drafted) => machine.mark_offered(),
                Ok(Offered::Deferred) => {}
                Err(error) => {
                    if let Some(log) = log {
                        log.error(&format!("helper: cannot offer a frame: {error}"));
                    }
                    return WRITE_FAILED;
                }
            }
        }
        let wait = machine.idle_until().map_or(Duration::ZERO, |until| {
            until.saturating_duration_since(Instant::now())
        });
        let mut next = commands.recv_timeout(wait);
        loop {
            let command = match next {
                Ok(command) => command,
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => return "the reader stopped",
            };
            let handled = match command {
                ScreenCommand::SelectDisplay(display) => deliver(
                    machine.select_display(display, Instant::now()),
                    &mut channel,
                    log,
                ),
                ScreenCommand::RequestFrame => channel.request_frame(),
            };
            if handled.is_err() {
                return WRITE_FAILED;
            }
            next = commands.try_recv().map_err(|error| match error {
                mpsc::TryRecvError::Empty => mpsc::RecvTimeoutError::Timeout,
                mpsc::TryRecvError::Disconnected => mpsc::RecvTimeoutError::Disconnected,
            });
        }
    }
}

fn deliver<F: SectionFactory>(
    events: Vec<ScreenEvent>,
    channel: &mut AppChannel<PipeOutbox<'_>, F>,
    log: Option<&EventLog>,
) -> io::Result<()> {
    for event in events {
        match event {
            ScreenEvent::DesktopChanged(desktop) => channel.desktop_changed(desktop)?,
            ScreenEvent::Unavailable { display } => channel.screen_unavailable(display)?,
            ScreenEvent::CaptureEnded => channel.capture_ended(),
            ScreenEvent::Note(note) => {
                if let Some(log) = log {
                    log.info(&format!("helper: {note}"));
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
struct PipeOutbox<'a>(&'a Pipe);

impl Outbox for PipeOutbox<'_> {
    fn send(&mut self, message: HelperToApp) -> io::Result<()> {
        write_message(self.0, &message, Some(Instant::now() + WRITE_TIME))
    }
}

pub(crate) fn server_is_app(pipe: &Pipe, app: &impl AsRawHandle) -> io::Result<bool> {
    Ok(pipe.server_process_id()? == process_id(app))
}

#[cfg(test)]
mod tests {
    use dari_proto::InputDesktop;

    use super::*;
    use crate::dxgi_result::Hresult;
    use crate::pointer::RawPointerShape;
    use crate::screen::{AcquiredFrame, Duplicate, Duplication, Size};
    use crate::test_support::test_pipe;
    use crate::tracker::{DesktopSource, Observation};
    use crate::win32::open_client_process;

    #[derive(Debug)]
    struct NoOutputs;

    #[derive(Debug)]
    enum NoDuplication {}

    impl DesktopSource for NoOutputs {
        fn poll(&mut self) -> Observation {
            Observation::Named("Winlogon".into())
        }
    }

    impl DesktopWorld for NoOutputs {
        type Duplication = NoDuplication;

        fn attach(&mut self) -> Result<Observation, Hresult> {
            Ok(self.poll())
        }

        fn duplicate(&mut self, _display: u32) -> Result<NoDuplication, Duplicate> {
            Err(Duplicate::NoSuchDisplay)
        }
    }

    impl Duplication for NoDuplication {
        fn size(&self) -> Size {
            match *self {}
        }
        fn acquire(&mut self, _timeout: Duration) -> Result<AcquiredFrame, Hresult> {
            match *self {}
        }
        fn copy_image(&mut self, _into: &mut [u8]) -> Result<(), Hresult> {
            match *self {}
        }
        fn pointer_shape(&mut self) -> Result<RawPointerShape, Hresult> {
            match *self {}
        }
        fn release(&mut self) -> Result<(), Hresult> {
            match *self {}
        }
    }

    #[test]
    fn the_screen_thread_answers_through_the_pipe_until_the_app_hangs_up() {
        let (server, path) = test_pipe();
        let client = Pipe::open(&path).unwrap();
        let this = open_client_process(std::process::id()).unwrap();
        let app = InheritedProcess::adopt(this.as_raw_handle() as usize).unwrap();
        thread::scope(|scope| {
            let helper = scope.spawn(|| converse(&client, &app, || NoOutputs));
            let deadline = Some(Instant::now() + Duration::from_secs(10));
            write_message(&server, &AppToHelper::SelectDisplay(5), deadline).unwrap();
            let mut replies = MessageReader::<HelperToApp>::new();
            assert_eq!(
                replies.read(&server, deadline).unwrap(),
                Some(HelperToApp::DesktopChanged(InputDesktop::Winlogon))
            );
            assert_eq!(
                replies.read(&server, deadline).unwrap(),
                Some(HelperToApp::ScreenUnavailable { display: 5 })
            );
            drop(server);
            assert_eq!(helper.join().unwrap(), Ok("the app closed its pipe"));
        });
    }

    #[test]
    fn the_helper_stops_when_the_app_ends_the_link_without_closing_its_pipe() {
        let (server, path) = test_pipe();
        let client = Pipe::open(&path).unwrap();
        let this = open_client_process(std::process::id()).unwrap();
        let app = InheritedProcess::adopt(this.as_raw_handle() as usize).unwrap();
        thread::scope(|scope| {
            let helper = scope.spawn(|| converse(&client, &app, || NoOutputs));
            let deadline = Some(Instant::now() + Duration::from_secs(10));
            write_message(&server, &AppToHelper::Stop, deadline).unwrap();
            assert_eq!(helper.join().unwrap(), Ok("the app ended the link"));
        });
    }

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
