use std::collections::HashMap;
use std::io;
use std::os::windows::io::OwnedHandle;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use dari_proto::{
    HelperPipeName, PIPE_CLIENT_RIGHTS, Refusal, SERVICE_PIPE, SecureDesktopControl, ServiceReply,
    ServiceRequest,
};
use windows::Win32::Storage::FileSystem::{FILE_GENERIC_READ, FILE_GENERIC_WRITE};

use crate::command::HelperArgs;
use crate::frames::{MessageReader, write_message};
use crate::limiter::RefusalLimiter;
use crate::sas::{self, NotRaised, Raised};
use crate::slot::{self, Start};
use crate::win32::{
    Event, EventLog, Job, Pipe, console_session, end_process, has_exited, image_path,
    launch_helper, open_client_process, own_user, process_id, same_path, session_is_active,
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
    sas_limiter: Mutex<RefusalLimiter>,
    policy: fn() -> SecureDesktopControl,
    console: fn() -> Option<u32>,
    raise_sas: fn() -> Result<Raised, NotRaised>,
}

/// A helper and the handle to the app it serves, which keeps the app's process ID from being
/// reused while the helper runs.
#[derive(Debug)]
struct Helper {
    process: OwnedHandle,
    app: OwnedHandle,
    input: bool,
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
            sas_limiter: Mutex::new(RefusalLimiter::new(REFUSALS_PER_WINDOW, REFUSAL_WINDOW)),
            policy: SecureDesktopControl::read,
            console: console_session,
            raise_sas: sas::raise,
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
                if count_refusal(&self.limiter, refused.refusal, &refused.detail, log) {
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
        let starts_helper = matches!(request, ServiceRequest::StartHelper { .. });
        let reply = self.handle(client, request, log);
        if starts_helper
            && let ServiceReply::Refused(refusal) = reply
            && !count_refusal(&self.limiter, refusal, &detail, log)
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

    fn handle(
        &self,
        client: Client,
        request: ServiceRequest,
        log: Option<&EventLog>,
    ) -> ServiceReply {
        match admit(request, (self.policy)()) {
            Ok(ServiceRequest::StartHelper { pipe, input }) => {
                self.start_helper(client, pipe, input, log)
            }
            Ok(ServiceRequest::SendSas) => self.send_sas(&client, log),
            Err(refusal) => self.refuse_sas(&client, refusal, None, log),
        }
    }

    fn start_helper(
        &self,
        client: Client,
        pipe: HelperPipeName,
        input: bool,
        log: Option<&EventLog>,
    ) -> ServiceReply {
        let session = client.session;
        let mut helpers = self.helpers();
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
                    let message = format!(
                        "started a helper in session {session}: pid {}, input {input}, for {}",
                        launched.pid, client.detail
                    );
                    if input {
                        log.input_helper_started(&message);
                    } else {
                        log.info(&message);
                    }
                }
                helpers.insert(
                    session,
                    Helper {
                        process: launched.process,
                        app: client.process,
                        input,
                    },
                );
                ServiceReply::HelperStarted {
                    secure_attention: self.sas_grant(input, session).is_ok(),
                }
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

    fn sas_grant(&self, input: bool, session: u32) -> Result<(), Refusal> {
        if !input {
            return Err(Refusal::NoInput);
        }
        if (self.console)() != Some(session) {
            return Err(Refusal::NotConsole);
        }
        Ok(())
    }

    fn send_sas(&self, client: &Client, log: Option<&EventLog>) -> ServiceReply {
        let session = client.session;
        let Some(input) = self.own_helper_input(client) else {
            return self.refuse_sas(client, Refusal::NoHelper, None, log);
        };
        if let Err(refusal) = self.sas_grant(input, session) {
            return self.refuse_sas(client, refusal, None, log);
        }
        match (self.raise_sas)() {
            Ok(Raised {
                generation,
                cleanup_error,
            }) => {
                if let Some(log) = log {
                    log.sas_sent(&format!(
                        "raised Ctrl+Alt+Del in session {session} for {}; {generation}",
                        client.detail
                    ));
                    if let Some(cleanup_error) = cleanup_error {
                        log.error(&format!(
                            "after Ctrl+Alt+Del in session {session}: {cleanup_error}"
                        ));
                    }
                }
                ServiceReply::SasSent
            }
            Err(NotRaised::Disallowed(generation)) => self.refuse_sas(
                client,
                Refusal::SasDisallowed,
                Some(format!(
                    "{generation}, which keeps services from raising it"
                )),
                log,
            ),
            Err(NotRaised::Failed(error)) => {
                self.refuse_sas(client, Refusal::Failed, Some(error), log)
            }
        }
    }

    fn refuse_sas(
        &self,
        client: &Client,
        refusal: Refusal,
        why: Option<String>,
        log: Option<&EventLog>,
    ) -> ServiceReply {
        if lock(&self.sas_limiter).allow(Instant::now())
            && let Some(log) = log
        {
            let why = why.map(|why| format!(": {why}")).unwrap_or_default();
            let message = format!(
                "refused Ctrl+Alt+Del for a client ({}): {refusal:?}{why}",
                client.detail
            );
            if refusal == Refusal::Failed {
                log.error(&message);
            } else {
                log.info(&message);
            }
        }
        ServiceReply::Refused(refusal)
    }

    fn own_helper_input(&self, client: &Client) -> Option<bool> {
        let app = process_id(&client.process);
        let helpers = self.helpers();
        let helper = helpers.get(&client.session)?;
        (process_id(&helper.app) == app).then_some(helper.input)
    }

    fn helpers(&self) -> MutexGuard<'_, HashMap<u32, Helper>> {
        lock(&self.helpers)
    }
}

fn admit(
    request: ServiceRequest,
    control: SecureDesktopControl,
) -> Result<ServiceRequest, Refusal> {
    match request {
        ServiceRequest::StartHelper { pipe, input } => Ok(ServiceRequest::StartHelper {
            pipe,
            input: control.helper_input(input),
        }),
        ServiceRequest::SendSas if control == SecureDesktopControl::Off => Err(Refusal::PolicyOff),
        ServiceRequest::SendSas => Ok(ServiceRequest::SendSas),
    }
}

fn count_refusal(
    limiter: &Mutex<RefusalLimiter>,
    refusal: Refusal,
    detail: &str,
    log: Option<&EventLog>,
) -> bool {
    let answer = lock(limiter).allow(Instant::now());
    if answer && let Some(log) = log {
        log.info(&format!("refused a client ({detail}): {refusal:?}"));
    }
    answer
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
    use std::sync::atomic::{AtomicBool, Ordering};

    use dari_proto::{HelperPipeName, PIPE_RANDOM_BYTES};
    use windows::Win32::Foundation::ERROR_PIPE_BUSY;

    use super::*;
    use crate::sas::Generation;
    use crate::test_support::{test_pipe, test_pipe_path};

    #[test]
    fn the_policy_off_takes_input_from_a_helper_and_refuses_the_secure_attention_sequence() {
        use SecureDesktopControl::{Off, On};

        let pipe = HelperPipeName::from_random([7; PIPE_RANDOM_BYTES]);
        let start = |input| ServiceRequest::StartHelper {
            pipe: pipe.clone(),
            input,
        };
        for (control, requested, granted) in [
            (On, true, true),
            (On, false, false),
            (Off, true, false),
            (Off, false, false),
        ] {
            assert_eq!(admit(start(requested), control), Ok(start(granted)));
        }
        assert_eq!(
            admit(ServiceRequest::SendSas, On),
            Ok(ServiceRequest::SendSas)
        );
        assert_eq!(admit(ServiceRequest::SendSas, Off), Err(Refusal::PolicyOff));
    }

    static POLICY_ON: AtomicBool = AtomicBool::new(false);

    fn test_policy() -> SecureDesktopControl {
        if POLICY_ON.load(Ordering::Relaxed) {
            SecureDesktopControl::On
        } else {
            SecureDesktopControl::Off
        }
    }

    #[test]
    fn the_service_reads_the_policy_for_each_request() {
        let mut server = Server::start(&test_pipe_path()).unwrap();
        server.policy = test_policy;
        let client = || Client {
            session: u32::MAX,
            process: open_client_process(std::process::id()).unwrap(),
            detail: "this test".into(),
        };
        let reply = |server: &Server| server.handle(client(), ServiceRequest::SendSas, None);

        POLICY_ON.store(false, Ordering::Relaxed);
        assert_eq!(reply(&server), ServiceReply::Refused(Refusal::PolicyOff));
        POLICY_ON.store(true, Ordering::Relaxed);
        assert_eq!(reply(&server), ServiceReply::Refused(Refusal::NoHelper));
        POLICY_ON.store(false, Ordering::Relaxed);
        assert_eq!(reply(&server), ServiceReply::Refused(Refusal::PolicyOff));
    }

    #[test]
    fn only_the_app_whose_helper_runs_with_input_on_the_console_may_raise_the_sequence() {
        let mut server = Server::start(&test_pipe_path()).unwrap();
        server.policy = || SecureDesktopControl::On;
        server.console = || Some(7);
        server.raise_sas = || {
            Ok(Raised {
                generation: Generation::Allowed(1),
                cleanup_error: None,
            })
        };
        let this_process = || open_client_process(std::process::id()).unwrap();
        let client = |session| Client {
            session,
            process: this_process(),
            detail: "this test".into(),
        };
        let send_sas = |server: &Server, session| {
            server.handle(client(session), ServiceRequest::SendSas, None)
        };
        let helper = |app, input| Helper {
            process: this_process(),
            app,
            input,
        };
        assert_eq!(
            send_sas(&server, 7),
            ServiceReply::Refused(Refusal::NoHelper)
        );

        let mut other_app = std::process::Command::new("cmd.exe")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        server.helpers().insert(
            7,
            helper(open_client_process(other_app.id()).unwrap(), true),
        );
        assert_eq!(
            send_sas(&server, 7),
            ServiceReply::Refused(Refusal::NoHelper)
        );
        other_app.kill().unwrap();
        other_app.wait().unwrap();

        server.helpers().insert(7, helper(this_process(), false));
        assert_eq!(
            send_sas(&server, 7),
            ServiceReply::Refused(Refusal::NoInput)
        );

        server.helpers().insert(7, helper(this_process(), true));
        assert_eq!(send_sas(&server, 7), ServiceReply::SasSent);
        assert_eq!(
            send_sas(&server, 8),
            ServiceReply::Refused(Refusal::NoHelper)
        );
        server.helpers().insert(8, helper(this_process(), true));
        assert_eq!(
            send_sas(&server, 8),
            ServiceReply::Refused(Refusal::NotConsole)
        );
        server.console = || None;
        assert_eq!(
            send_sas(&server, 7),
            ServiceReply::Refused(Refusal::NotConsole)
        );
        server.console = || Some(7);

        server.raise_sas = || Err(NotRaised::Disallowed(Generation::Disallowed(2)));
        assert_eq!(
            send_sas(&server, 7),
            ServiceReply::Refused(Refusal::SasDisallowed)
        );
        server.raise_sas = || Err(NotRaised::Failed("no sas.dll".into()));
        assert_eq!(send_sas(&server, 7), ServiceReply::Refused(Refusal::Failed));
        server.raise_sas = || {
            Ok(Raised {
                generation: Generation::Unset,
                cleanup_error: Some("cannot delete SoftwareSASGeneration".into()),
            })
        };
        assert_eq!(send_sas(&server, 7), ServiceReply::SasSent);
        server.policy = || SecureDesktopControl::Off;
        assert_eq!(
            send_sas(&server, 7),
            ServiceReply::Refused(Refusal::PolicyOff)
        );
    }

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
