use std::collections::HashMap;
use std::io;
use std::os::windows::io::OwnedHandle;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use dari_proto::{PIPE_CLIENT_RIGHTS, Refusal, SERVICE_PIPE, ServiceReply, ServiceRequest};
use windows::Win32::Storage::FileSystem::{FILE_GENERIC_READ, FILE_GENERIC_WRITE};

use crate::command::HelperArgs;
use crate::frames::{MessageReader, write_message};
use crate::limiter::RefusalLimiter;
use crate::slot::{self, Start};
use crate::win32::{
    Event, EventLog, Job, Pipe, end_process, has_exited, image_path, launch_helper,
    open_client_process, own_user, process_id, same_path, session_is_active,
};

const INSTANCES: u32 = 4;
const CLIENT_TIME: Duration = Duration::from_secs(2);
const REPLACE_TIME: Duration = Duration::from_secs(1);
const REFUSALS_PER_WINDOW: u32 = 10;
const REFUSAL_WINDOW: Duration = Duration::from_secs(60);

/// SYSTEM and interactive users may connect, as clients only. `creator`, the service's own user,
/// may also create the pipe's other instances. Windows checks the first instance's DACL for that
/// even for the process that created it, and asks for generic read and write, whose write half
/// includes `FILE_CREATE_PIPE_INSTANCE`.
fn service_pipe_sddl(creator: &str) -> String {
    let creating = FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0;
    format!(
        "D:P(A;;{creating:#x};;;{creator})(A;;{PIPE_CLIENT_RIGHTS:#x};;;SY)(A;;{PIPE_CLIENT_RIGHTS:#x};;;IU)"
    )
}

#[derive(Debug)]
pub(crate) struct StartError {
    pub(crate) doing: &'static str,
    pub(crate) error: io::Error,
}

#[derive(Debug)]
pub(crate) struct Server {
    pipes: Vec<Pipe>,
    job: Job,
    exe: PathBuf,
    expected_app: PathBuf,
    helpers: Mutex<HashMap<u32, Helper>>,
    limiter: Mutex<RefusalLimiter>,
}

/// A helper and the handle to the app it serves, which keeps the app's process ID from being
/// reused while the helper runs.
#[derive(Debug)]
struct Helper {
    process: OwnedHandle,
    app: OwnedHandle,
}

#[derive(Debug)]
pub(crate) struct Client {
    session: u32,
    process: OwnedHandle,
    detail: String,
}

#[derive(Debug)]
pub(crate) struct Refused {
    pub(crate) refusal: Refusal,
    detail: String,
}

impl Server {
    /// The pipe fails if another process created its name first.
    pub(crate) fn start(path: &str) -> Result<Self, StartError> {
        let exe = std::env::current_exe().map_err(|error| StartError {
            doing: "find dari-service.exe's path",
            error,
        })?;
        let job = Job::kill_on_close().map_err(|error| StartError {
            doing: "create the helpers' job object",
            error,
        })?;
        let creator = own_user().map_err(|error| StartError {
            doing: "read dari-service.exe's user",
            error,
        })?;
        let pipes = Pipe::create_instances(path, &service_pipe_sddl(&creator), INSTANCES)
            .map_err(|error| StartError {
                doing: r"create \\.\pipe\dari-service (if another process created it first, that process isn't DariService)",
                error,
            })?;
        Ok(Self {
            pipes,
            job,
            expected_app: exe.with_file_name("dari.exe"),
            exe,
            helpers: Mutex::default(),
            limiter: Mutex::new(RefusalLimiter::new(REFUSALS_PER_WINDOW, REFUSAL_WINDOW)),
        })
    }

    pub(crate) fn run(&self, stop: &Event, open_log: fn() -> Option<EventLog>) {
        thread::scope(|scope| {
            for pipe in &self.pipes {
                scope.spawn(move || self.run_instance(pipe, stop, open_log));
            }
        });
    }

    fn run_instance(&self, pipe: &Pipe, stop: &Event, open_log: fn() -> Option<EventLog>) {
        let log = open_log();
        let log = log.as_ref();
        loop {
            match pipe.connect(stop) {
                Ok(true) => self.serve(pipe, log),
                Ok(false) => return,
                Err(error) => {
                    if let Some(log) = log {
                        log.error(&format!(
                            "cannot wait for a client on {SERVICE_PIPE}: {error}"
                        ));
                    }
                    if stop.wait(Duration::from_secs(1)) {
                        return;
                    }
                }
            }
            pipe.disconnect();
        }
    }

    fn serve(&self, pipe: &Pipe, log: Option<&EventLog>) {
        self.helpers()
            .retain(|_, helper| !has_exited(&helper.process));
        let request_deadline = Instant::now() + CLIENT_TIME;
        let client = match vet(pipe, &self.expected_app) {
            Ok(client) => client,
            Err(refused) => {
                if self.count_refusal(refused.refusal, &refused.detail, log) {
                    let _written = write_message(
                        pipe,
                        &ServiceReply::Refused(refused.refusal),
                        Some(request_deadline),
                    );
                }
                return;
            }
        };
        let Ok(Some(request)) =
            MessageReader::<ServiceRequest>::new().read(pipe, Some(request_deadline))
        else {
            return;
        };
        let detail = client.detail.clone();
        let reply = self.handle(client, request, log);
        if let ServiceReply::Refused(refusal) = reply
            && !self.count_refusal(refusal, &detail, log)
        {
            return;
        }
        let reply_deadline = Instant::now() + CLIENT_TIME;
        if write_message(pipe, &reply, Some(reply_deadline)).is_ok() {
            // Disconnecting discards what the client hasn't read yet, so wait for it to hang up.
            let mut rest = MessageReader::<ServiceRequest>::new();
            while let Ok(Some(_)) = rest.read(pipe, Some(reply_deadline)) {}
        }
    }

    fn count_refusal(&self, refusal: Refusal, detail: &str, log: Option<&EventLog>) -> bool {
        let answer = lock(&self.limiter).allow(Instant::now());
        if answer && let Some(log) = log {
            log.info(&format!("refused a client ({detail}): {refusal:?}"));
        }
        answer
    }

    fn handle(
        &self,
        client: Client,
        request: ServiceRequest,
        log: Option<&EventLog>,
    ) -> ServiceReply {
        let session = client.session;
        let mut helpers = self.helpers();
        match request {
            ServiceRequest::StartHelper { pipe, input } => {
                let owner = helpers.get(&session).map(|helper| process_id(&helper.app));
                match slot::start(owner, process_id(&client.process)) {
                    Start::Refuse => return ServiceReply::Refused(Refusal::HelperRunning),
                    Start::Replace => {
                        if let Some(old) = helpers.remove(&session) {
                            if !end_process(&old.process, REPLACE_TIME) {
                                helpers.insert(session, old);
                                return ServiceReply::Refused(Refusal::HelperRunning);
                            }
                            if let Some(log) = log {
                                log.info(&format!(
                                    "ended helper pid {} in session {session}: its app asked for another",
                                    process_id(&old.process)
                                ));
                            }
                        }
                    }
                    Start::New => {}
                }
                let arguments = |app| HelperArgs { pipe, input, app }.to_arguments();
                match launch_helper(session, &self.exe, &client.process, arguments, &self.job) {
                    Ok(launched) => {
                        if let Some(log) = log {
                            log.info(&format!(
                                "started a helper in session {session}: pid {}, input {input}, for {}",
                                launched.pid, client.detail
                            ));
                        }
                        helpers.insert(
                            session,
                            Helper {
                                process: launched.process,
                                app: client.process,
                            },
                        );
                        ServiceReply::HelperStarted
                    }
                    Err(error) => {
                        if let Some(log) = log {
                            log.error(&format!(
                                "cannot start a helper in session {session}: {error}"
                            ));
                        }
                        ServiceReply::Refused(Refusal::Failed)
                    }
                }
            }
            ServiceRequest::SendSas if !helpers.contains_key(&session) => {
                ServiceReply::Refused(Refusal::NoHelper)
            }
            ServiceRequest::SendSas => ServiceReply::Refused(Refusal::Failed),
        }
    }

    fn helpers(&self) -> MutexGuard<'_, HashMap<u32, Helper>> {
        lock(&self.helpers)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Checks the client on `pipe` before reading a byte from it: its process must be
/// `expected_app` and its session active. The process stays open from then on, so its ID can't
/// be reused under the check.
pub(crate) fn vet(pipe: &Pipe, expected_app: &Path) -> Result<Client, Refused> {
    let refused = |refusal, detail| Refused { refusal, detail };
    let (Ok(pid), Ok(session)) = (pipe.client_process_id(), pipe.client_session_id()) else {
        return Err(refused(Refusal::NotDari, "an unidentified client".into()));
    };
    let detail = format!("pid {pid}, session {session}");
    let Ok(process) = open_client_process(pid) else {
        return Err(refused(Refusal::NotDari, detail));
    };
    let image = match image_path(&process) {
        Ok(image) if same_path(&image, expected_app) => image,
        Ok(image) => {
            return Err(refused(
                Refusal::NotDari,
                format!("{detail}, {}", image.display()),
            ));
        }
        Err(_) => return Err(refused(Refusal::NotDari, detail)),
    };
    if !session_is_active(session).unwrap_or(false) {
        return Err(refused(Refusal::InactiveSession, detail));
    }
    Ok(Client {
        session,
        process,
        detail: format!("{detail}, {}", image.display()),
    })
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::ERROR_PIPE_BUSY;

    use super::*;
    use crate::test_support::{test_pipe, test_pipe_path};

    fn connected_pipe() -> (Pipe, Pipe) {
        let (server, path) = test_pipe();
        let client = Pipe::open(&path).unwrap();
        assert!(server.connect(&Event::new().unwrap()).unwrap());
        (server, client)
    }

    #[test]
    fn vetting_refuses_a_client_with_another_image() {
        let (server, _client) = connected_pipe();
        let refused = vet(&server, Path::new(r"C:\Program Files\Dari\dari.exe")).unwrap_err();
        assert_eq!(refused.refusal, Refusal::NotDari);
        assert!(
            refused
                .detail
                .contains(&format!("pid {}", std::process::id())),
            "{}",
            refused.detail
        );

        // CI runs in session 0, where the session check refuses.
        let own = std::env::current_exe().unwrap();
        let shouted = PathBuf::from(own.to_string_lossy().to_uppercase());
        for expected in [own, shouted] {
            match vet(&server, &expected) {
                Ok(_) => {}
                Err(refused) => assert_eq!(refused.refusal, Refusal::InactiveSession),
            }
        }
    }

    #[test]
    fn a_pipe_name_another_process_created_first_fails_to_create() {
        let (_first, path) = test_pipe();
        let creator = own_user().unwrap();
        assert!(Pipe::create_instances(&path, &service_pipe_sddl(&creator), INSTANCES).is_err());
    }

    fn open_within(path: &str, limit: Duration) -> Pipe {
        let deadline = Instant::now() + limit;
        loop {
            match Pipe::open(path) {
                Ok(pipe) => return pipe,
                Err(error) if Instant::now() < deadline => {
                    let busy = ERROR_PIPE_BUSY.to_hresult().0;
                    assert_eq!(error.raw_os_error(), Some(busy), "{error}");
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("no instance came free in {limit:?}: {error}"),
            }
        }
    }

    #[test]
    fn refused_clients_that_never_hang_up_dont_hold_the_pipe() {
        let path = test_pipe_path();
        let server = Server::start(&path).unwrap();
        let stop = Event::new().unwrap();
        thread::scope(|scope| {
            scope.spawn(|| server.run(&stop, || None));
            let clients = scope.spawn(|| {
                let mut held = Vec::new();
                for _ in 0..2 * INSTANCES {
                    let client = open_within(&path, Duration::from_millis(500));
                    let deadline = Instant::now() + Duration::from_millis(500);
                    match MessageReader::<ServiceReply>::new().read(&client, Some(deadline)) {
                        Ok(None | Some(ServiceReply::Refused(Refusal::NotDari))) => {}
                        other => panic!("expected a refusal or a closed pipe, got {other:?}"),
                    }
                    held.push(client);
                }
            });
            let finished = clients.join();
            stop.set();
            if let Err(panic) = finished {
                std::panic::resume_unwind(panic);
            }
        });
    }
}
