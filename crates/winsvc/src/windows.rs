use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use windows_service::service::{
    Service, ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
    ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

const SERVICE_NAME: &str = "DariService";
const DISPLAY_NAME: &str = "Dari Service";
const DESCRIPTION: &str = "Lets Dari show and answer the Windows secure desktop, such as UAC prompts and the lock screen.";

const WAIT_LIMIT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(250);

const ERROR_SERVICE_ALREADY_RUNNING: i32 = 1056;
const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
const ERROR_SERVICE_CANNOT_ACCEPT_CTRL: i32 = 1061;
const ERROR_SERVICE_NOT_ACTIVE: i32 = 1062;
const ERROR_SERVICE_MARKED_FOR_DELETE: i32 = 1072;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Service,
    Install,
    Uninstall,
}

impl Command {
    fn parse(arguments: &[OsString]) -> Option<Self> {
        let [command] = arguments else { return None };
        match command.to_str()? {
            "service" => Some(Self::Service),
            "install" => Some(Self::Install),
            "uninstall" => Some(Self::Uninstall),
            _ => None,
        }
    }
}

#[derive(Debug)]
enum Failure {
    Call {
        doing: &'static str,
        error: windows_service::Error,
    },
    Timeout {
        waiting_for: &'static str,
    },
    ExecutablePath(io::Error),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // The crate's own message for this variant leaves out the OS error.
            Self::Call {
                doing,
                error: windows_service::Error::Winapi(error),
            } => write!(f, "cannot {doing}: {error}"),
            Self::Call { doing, error } => write!(f, "cannot {doing}: {error}"),
            Self::Timeout { waiting_for } => write!(
                f,
                "gave up after {} s waiting for {waiting_for}",
                WAIT_LIMIT.as_secs()
            ),
            Self::ExecutablePath(error) => write!(f, "cannot find this executable's path: {error}"),
        }
    }
}

fn while_doing(doing: &'static str) -> impl FnOnce(windows_service::Error) -> Failure {
    move |error| Failure::Call { doing, error }
}

fn os_code(error: &windows_service::Error) -> Option<i32> {
    match error {
        windows_service::Error::Winapi(error) => error.raw_os_error(),
        _ => None,
    }
}

fn unless_already(
    result: windows_service::Result<()>,
    already: &[i32],
    doing: &'static str,
) -> Result<(), Failure> {
    match result {
        Err(error) if !os_code(&error).is_some_and(|code| already.contains(&code)) => {
            Err(Failure::Call { doing, error })
        }
        _ => Ok(()),
    }
}

pub(crate) fn main() -> ExitCode {
    let arguments: Vec<OsString> = std::env::args_os().skip(1).collect();
    let Some(command) = Command::parse(&arguments) else {
        return fail("usage: dari-service <install|uninstall|service>");
    };
    let result = match command {
        Command::Service => run_service(),
        Command::Install => install(),
        Command::Uninstall => uninstall(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => fail(&failure.to_string()),
    }
}

fn fail(message: &str) -> ExitCode {
    let _ = writeln!(io::stderr(), "dari-service: {message}");
    ExitCode::FAILURE
}

define_windows_service!(ffi_service_main, service_main);

fn run_service() -> Result<(), Failure> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(while_doing(
        "run as DariService (only the service control manager starts this command)",
    ))
}

fn service_main(_arguments: Vec<OsString>) {
    let (stop_sender, stop_receiver) = mpsc::channel();
    let handler = move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = stop_sender.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let Ok(status) = service_control_handler::register(SERVICE_NAME, handler) else {
        return;
    };
    let exit_code = match status.set_service_status(service_status(ServiceState::Running)) {
        Ok(()) => {
            let _ = stop_receiver.recv();
            ServiceExitCode::NO_ERROR
        }
        Err(_) => ServiceExitCode::ServiceSpecific(1),
    };
    let _ = status.set_service_status(ServiceStatus {
        exit_code,
        ..service_status(ServiceState::Stopped)
    });
}

fn service_status(state: ServiceState) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: if state == ServiceState::Running {
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
        } else {
            ServiceControlAccept::empty()
        },
        exit_code: ServiceExitCode::NO_ERROR,
        checkpoint: 0,
        wait_hint: Duration::ZERO,
        process_id: None,
    }
}

fn install() -> Result<(), Failure> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(while_doing("connect to the service control manager"))?;
    let info = service_info()?;
    let deadline = Instant::now() + WAIT_LIMIT;
    let service = loop {
        match create_or_update(&manager, &info) {
            Err(error)
                if os_code(&error) == Some(ERROR_SERVICE_MARKED_FOR_DELETE)
                    && Instant::now() < deadline =>
            {
                thread::sleep(POLL_INTERVAL);
            }
            result => break result.map_err(while_doing("create or update DariService"))?,
        }
    };
    service
        .set_description(DESCRIPTION)
        .map_err(while_doing("set DariService's description"))?;
    service
        .update_failure_actions(restart_on_failure())
        .map_err(while_doing("set DariService's failure actions"))?;
    converge(&service, "DariService to run", |state| match state {
        ServiceState::Running => Ok(true),
        ServiceState::Stopped => unless_already(
            service.start::<&OsStr>(&[]),
            &[ERROR_SERVICE_ALREADY_RUNNING],
            "start DariService",
        )
        .map(|()| false),
        _ => Ok(false),
    })
}

fn create_or_update(
    manager: &ServiceManager,
    info: &ServiceInfo,
) -> windows_service::Result<Service> {
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::START | ServiceAccess::CHANGE_CONFIG;
    match manager.open_service(SERVICE_NAME, access) {
        Ok(service) => service.change_config(info).map(|()| service),
        Err(error) if os_code(&error) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => {
            manager.create_service(info, access)
        }
        Err(error) => Err(error),
    }
}

fn service_info() -> Result<ServiceInfo, Failure> {
    Ok(ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: DISPLAY_NAME.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        // The crate quotes a path that contains spaces, so `C:\Program Files\...` can't be read as
        // `C:\Program` with arguments.
        executable_path: std::env::current_exe().map_err(Failure::ExecutablePath)?,
        launch_arguments: vec!["service".into()],
        dependencies: Vec::new(),
        account_name: None,
        account_password: None,
    })
}

fn restart_on_failure() -> ServiceFailureActions {
    let restart = ServiceAction {
        action_type: ServiceActionType::Restart,
        delay: Duration::from_secs(5),
    };
    ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_hours(24)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![restart; 3]),
    }
}

fn uninstall() -> Result<(), Failure> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(while_doing("connect to the service control manager"))?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    let service = match manager.open_service(SERVICE_NAME, access) {
        Ok(service) => service,
        Err(error) if os_code(&error) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => return Ok(()),
        Err(error) => {
            return Err(Failure::Call {
                doing: "open DariService",
                error,
            });
        }
    };
    converge(&service, "DariService to stop", |state| match state {
        ServiceState::Stopped => Ok(true),
        ServiceState::Running | ServiceState::Paused => unless_already(
            service.stop().map(drop),
            &[ERROR_SERVICE_NOT_ACTIVE, ERROR_SERVICE_CANNOT_ACCEPT_CTRL],
            "stop DariService",
        )
        .map(|()| false),
        _ => Ok(false),
    })?;
    unless_already(
        service.delete(),
        &[ERROR_SERVICE_MARKED_FOR_DELETE],
        "delete DariService",
    )
}

fn converge(
    service: &Service,
    goal: &'static str,
    mut step: impl FnMut(ServiceState) -> Result<bool, Failure>,
) -> Result<(), Failure> {
    let deadline = Instant::now() + WAIT_LIMIT;
    loop {
        let state = service
            .query_status()
            .map_err(while_doing("query DariService's status"))?
            .current_state;
        if step(state)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Failure::Timeout { waiting_for: goal });
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> Option<Command> {
        let arguments: Vec<OsString> = arguments.iter().map(OsString::from).collect();
        Command::parse(&arguments)
    }

    #[test]
    fn parses_exactly_one_known_command() {
        assert_eq!(parse(&["service"]), Some(Command::Service));
        assert_eq!(parse(&["install"]), Some(Command::Install));
        assert_eq!(parse(&["uninstall"]), Some(Command::Uninstall));
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&["Install"]), None);
        assert_eq!(parse(&["install", "service"]), None);
    }
}
