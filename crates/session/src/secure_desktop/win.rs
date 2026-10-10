use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle, RawHandle};
use std::time::{Duration, Instant};

use bytes::BytesMut;
use dari_proto::{
    HelperPipeName, HelperToApp, InputDesktop, LOCAL_FRAME_LIMIT, MessageCodec, PIPE_CLIENT_RIGHTS,
    PIPE_RANDOM_BYTES, Refusal, SERVICE_PIPE, ServiceReply, ServiceRequest,
};
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;
use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer, PipeMode, ServerOptions};
use tokio_util::codec::{Encoder, FramedRead};
use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_SEM_TIMEOUT, HANDLE, HLOCAL, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, PSID, RevertToSelf, SECURITY_ATTRIBUTES,
    TOKEN_GROUPS, TOKEN_INFORMATION_CLASS, TOKEN_QUERY, TOKEN_USER, TokenLogonSid, TokenUser,
};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_OVERLAPPED, SECURITY_IDENTIFICATION};
use windows::Win32::System::Pipes::{ImpersonateNamedPipeClient, WaitNamedPipeW};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken,
};
use windows::core::{PCWSTR, PWSTR};

use super::{LinkCommand, LinkDriver, SecureDesktopLink};

const LOCAL_SYSTEM: &str = "S-1-5-18";
const SERVICE_TIME: Duration = Duration::from_secs(5);
const HELPER_TIME: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub(crate) enum LinkError {
    #[error("DariService isn't running")]
    ServiceMissing,
    #[error("DariService refused: {0:?}")]
    Refused(Refusal),
    #[error("the helper didn't connect in time")]
    HelperTimedOut,
    #[error("the helper's pipe client is {0}, not LocalSystem")]
    NotLocalSystem(String),
    #[error("the helper sent {0:?} before naming the input desktop")]
    UnexpectedFirstMessage(HelperToApp),
    #[error("the helper closed its pipe")]
    HelperClosed,
    #[error("{0}")]
    Codec(#[from] dari_proto::CodecError),
    #[error("{0}")]
    Io(#[from] io::Error),
}

/// Starts the link in the background and returns it at once. Called from the session's
/// runtime.
pub(crate) fn open(input: bool) -> SecureDesktopLink {
    let (link, mut driver) = SecureDesktopLink::pair();
    tokio::spawn(async move {
        if let Err(error) = run(input, &mut driver).await {
            driver.end(error.to_string());
        }
    });
    link
}

/// Runs the link until it fails, or until the session drops it (`Ok`).
async fn run(input: bool, driver: &mut LinkDriver) -> Result<(), LinkError> {
    let (mut messages, first) = tokio::select! {
        connected = connect(input) => connected?,
        () = session_gone(driver) => return Ok(()),
    };
    driver.desktop_changed(first);
    loop {
        tokio::select! {
            command = driver.command() => match command {
                Some(LinkCommand::SelectDisplay(_)) => {}
                None => return Ok(()),
            },
            message = messages.next() => match message {
                Some(Ok(HelperToApp::DesktopChanged(desktop))) => driver.desktop_changed(desktop),
                Some(Ok(
                    HelperToApp::FrameSection { .. }
                    | HelperToApp::Frame { .. }
                    | HelperToApp::ScreenUnavailable { .. },
                )) => {}
                Some(Err(error)) => return Err(error.into()),
                None => return Err(LinkError::HelperClosed),
            },
        }
    }
}

/// Has the service start the helper, and waits for it to connect and name its desktop.
async fn connect(input: bool) -> Result<(HelperMessages, InputDesktop), LinkError> {
    let pipe = random_pipe_name()?;
    let server = create_helper_pipe(&pipe)?;
    match ask_service(&ServiceRequest::StartHelper { pipe, input }).await? {
        ServiceReply::HelperStarted => accept_helper(server).await,
        ServiceReply::Refused(refusal) => Err(LinkError::Refused(refusal)),
    }
}

/// Returns once the session dropped the link. Commands that arrive meanwhile are already in the
/// link's state, which the link reads once the helper connects.
async fn session_gone(driver: &mut LinkDriver) {
    while driver.command().await.is_some() {}
}

fn random_pipe_name() -> io::Result<HelperPipeName> {
    let mut bytes = [0u8; PIPE_RANDOM_BYTES];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(HelperPipeName::from_random(bytes))
}

/// Creates the helper's pipe: one instance, local clients only. Its DACL admits SYSTEM, which
/// the helper is, and this process's logon session, whose processes [`accept_helper`] then
/// refuses. The name is no secret: any local process can list pipe names.
pub(crate) fn create_helper_pipe(pipe: &HelperPipeName) -> io::Result<NamedPipeServer> {
    let logon = logon_sid()?;
    let descriptor = SecurityDescriptor::parse(&format!(
        "D:P(A;;{PIPE_CLIENT_RIGHTS:#x};;;SY)(A;;{PIPE_CLIENT_RIGHTS:#x};;;{logon})"
    ))?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
        lpSecurityDescriptor: descriptor.0.0,
        bInheritHandle: false.into(),
    };
    // SAFETY: `attributes` points at `descriptor`, and both outlive the call, which copies the
    // descriptor into the new pipe.
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .max_instances(1)
            .pipe_mode(PipeMode::Byte)
            .create_with_security_attributes_raw(pipe.path(), (&raw mut attributes).cast())
    }
}

async fn ask_service(request: &ServiceRequest) -> Result<ServiceReply, LinkError> {
    let deadline = Instant::now() + SERVICE_TIME;
    let conversation = async {
        let mut client = connect_service(deadline).await?;
        let mut frame = BytesMut::new();
        MessageCodec::<ServiceRequest>::new(LOCAL_FRAME_LIMIT).encode(request, &mut frame)?;
        client.write_all(&frame).await?;
        let mut replies =
            FramedRead::new(client, MessageCodec::<ServiceReply>::new(LOCAL_FRAME_LIMIT));
        match replies.next().await {
            Some(reply) => Ok(reply?),
            None => Err(LinkError::Io(io::ErrorKind::UnexpectedEof.into())),
        }
    };
    tokio::time::timeout(SERVICE_TIME, conversation)
        .await
        .unwrap_or(Err(LinkError::Io(io::ErrorKind::TimedOut.into())))
}

async fn connect_service(deadline: Instant) -> Result<NamedPipeClient, LinkError> {
    let file = tokio::task::spawn_blocking(move || open_service_pipe(deadline))
        .await
        .map_err(io::Error::other)??;
    let handle = file.into_raw_handle();
    // SAFETY: `handle` is an open pipe client opened for overlapped I/O, and the new client takes
    // ownership of it.
    Ok(unsafe { NamedPipeClient::from_raw_handle(handle)? })
}

fn open_service_pipe(deadline: Instant) -> Result<File, LinkError> {
    let path = wide(SERVICE_PIPE);
    loop {
        let opened = OpenOptions::new()
            .access_mode(PIPE_CLIENT_RIGHTS)
            .custom_flags(FILE_FLAG_OVERLAPPED.0)
            .security_qos_flags(SECURITY_IDENTIFICATION.0)
            .open(SERVICE_PIPE);
        let error = match opened {
            Ok(file) => return Ok(file),
            Err(error) if is_os_error(&error, ERROR_PIPE_BUSY.0) => {
                let left = deadline
                    .checked_duration_since(Instant::now())
                    .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))?;
                // 0 would mean the pipe's default wait, and `u32::MAX` forever.
                let milliseconds = u32::try_from(left.as_millis())
                    .unwrap_or(u32::MAX - 1)
                    .max(1);
                // SAFETY: `path` is NUL-terminated and outlives the call.
                if unsafe { WaitNamedPipeW(PCWSTR(path.as_ptr()), milliseconds) }.as_bool() {
                    continue;
                }
                io::Error::last_os_error()
            }
            Err(error) => error,
        };
        return Err(if is_os_error(&error, ERROR_FILE_NOT_FOUND.0) {
            LinkError::ServiceMissing
        } else if is_os_error(&error, ERROR_SEM_TIMEOUT.0) {
            io::Error::from(io::ErrorKind::TimedOut).into()
        } else {
            error.into()
        });
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn is_os_error(error: &io::Error, code: u32) -> bool {
    error.raw_os_error() == i32::try_from(code).ok()
}

type HelperMessages = FramedRead<NamedPipeServer, MessageCodec<HelperToApp>>;

/// Waits for the helper to connect and name its first desktop, then checks that the client is
/// `LocalSystem`. Nothing the client sent counts until it passes.
pub(crate) async fn accept_helper(
    server: NamedPipeServer,
) -> Result<(HelperMessages, InputDesktop), LinkError> {
    let accepting = async {
        server.connect().await?;
        let mut messages =
            FramedRead::new(server, MessageCodec::<HelperToApp>::new(LOCAL_FRAME_LIMIT));
        // Impersonating the client fails until the server has read from the pipe.
        let first = match messages.next().await {
            Some(first) => first?,
            None => return Err(LinkError::HelperClosed),
        };
        let user = pipe_client_user(messages.get_ref().as_raw_handle())?;
        if user != LOCAL_SYSTEM {
            return Err(LinkError::NotLocalSystem(user));
        }
        match first {
            HelperToApp::DesktopChanged(desktop) => Ok((messages, desktop)),
            other => Err(LinkError::UnexpectedFirstMessage(other)),
        }
    };
    tokio::time::timeout(HELPER_TIME, accepting)
        .await
        .unwrap_or(Err(LinkError::HelperTimedOut))
}

/// The SID of the user on the other end of `pipe`, read by impersonating it. Impersonation is
/// per thread, so nothing between impersonating and reverting may yield.
fn pipe_client_user(pipe: RawHandle) -> io::Result<String> {
    let mut token = HANDLE::default();
    // SAFETY: `pipe` is an open, connected pipe server that has read from its client, and
    // `token` outlives the call that writes it.
    let opened = unsafe {
        ImpersonateNamedPipeClient(HANDLE(pipe))?;
        // `OpenAsSelf`: an identification-level client token can't grant opening its own token.
        let opened = OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &raw mut token);
        if RevertToSelf().is_err() {
            // A runtime worker thread must never keep running as another user.
            std::process::abort();
        }
        opened
    };
    opened?;
    // SAFETY: `OpenThreadToken` returned this handle to us.
    let token = unsafe { OwnedHandle::from_raw_handle(token.0) };
    let user = information(&token, TokenUser)?;
    // SAFETY: the buffer holds a `TOKEN_USER` whose SID lives in the same buffer.
    unsafe { sid_string(user.as_ptr().cast::<TOKEN_USER>().read().User.Sid) }
}

/// This process's logon SID, which every process of the signed-in user's logon session has.
fn logon_sid() -> io::Result<String> {
    let mut token = HANDLE::default();
    // SAFETY: the pseudo-handle needs no closing, and the token handle is wrapped as owned.
    let token = unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token)?;
        OwnedHandle::from_raw_handle(token.0)
    };
    let groups = information(&token, TokenLogonSid)?;
    // SAFETY: `information` returned a `TOKEN_GROUPS` for `TokenLogonSid`; `Groups[0]` is read
    // only after `GroupCount` is checked, and its SID lives in the same buffer.
    unsafe {
        let groups = groups.as_ptr().cast::<TOKEN_GROUPS>();
        if (*groups).GroupCount == 0 {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no logon SID"));
        }
        sid_string((*groups).Groups[0].Sid)
    }
}

/// A token information buffer, aligned for the structures it holds.
fn information(token: &OwnedHandle, class: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<u64>> {
    let token = HANDLE(token.as_raw_handle());
    let mut needed = 0u32;
    // SAFETY: a size query; it fails with ERROR_INSUFFICIENT_BUFFER and sets `needed`.
    let _sized = unsafe { GetTokenInformation(token, class, None, 0, &raw mut needed) };
    let mut buffer = vec![0u64; (needed as usize).div_ceil(size_of::<u64>())];
    // SAFETY: `buffer` holds at least `needed` bytes and outlives the call.
    unsafe {
        GetTokenInformation(
            token,
            class,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &raw mut needed,
        )?;
    }
    Ok(buffer)
}

/// # Safety
///
/// `sid` must point to a valid SID.
unsafe fn sid_string(sid: PSID) -> io::Result<String> {
    let mut text = PWSTR::null();
    // SAFETY: the caller guarantees `sid`; the string is allocated with `LocalAlloc` and freed
    // after it is copied.
    unsafe {
        ConvertSidToStringSidW(sid, &raw mut text)?;
        let string = text.to_string().unwrap_or_default();
        let _freed = LocalFree(Some(HLOCAL(text.0.cast::<c_void>())));
        Ok(string)
    }
}

struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    fn parse(sddl: &str) -> io::Result<Self> {
        let sddl = wide(sddl);
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `sddl` is NUL-terminated; the descriptor is allocated with `LocalAlloc` and
        // freed in `drop`.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &raw mut descriptor,
                None,
            )?;
        }
        Ok(Self(descriptor))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: the descriptor came from `LocalAlloc` and nothing uses it after this.
        let _freed = unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::secure_desktop::SecureDesktopEvent;

    fn client_of(pipe: &HelperPipeName) -> std::fs::File {
        OpenOptions::new()
            .access_mode(PIPE_CLIENT_RIGHTS)
            .security_qos_flags(SECURITY_IDENTIFICATION.0)
            .open(pipe.path())
            .unwrap()
    }

    fn frame(message: &HelperToApp) -> BytesMut {
        let mut frame = BytesMut::new();
        MessageCodec::<HelperToApp>::new(LOCAL_FRAME_LIMIT)
            .encode(message, &mut frame)
            .unwrap();
        frame
    }

    #[tokio::test]
    async fn the_app_refuses_a_helper_pipe_client_that_isnt_local_system() {
        let pipe = random_pipe_name().unwrap();
        let server = create_helper_pipe(&pipe).unwrap();
        let mut client = client_of(&pipe);
        client
            .write_all(&frame(&HelperToApp::DesktopChanged(InputDesktop::Winlogon)))
            .unwrap();
        match accept_helper(server).await {
            Err(LinkError::NotLocalSystem(user)) => {
                let own = own_user();
                assert_eq!(user, own);
                assert_ne!(user, LOCAL_SYSTEM);
            }
            Err(other) => panic!("expected NotLocalSystem, got {other}"),
            Ok(_) => panic!("a client that isn't LocalSystem was accepted"),
        }
    }

    #[tokio::test]
    async fn the_helper_pipe_has_one_instance() {
        let pipe = random_pipe_name().unwrap();
        let _server = create_helper_pipe(&pipe).unwrap();
        assert!(create_helper_pipe(&pipe).is_err());
        assert!(
            ServerOptions::new()
                .pipe_mode(PipeMode::Byte)
                .create(pipe.path())
                .is_err()
        );
    }

    /// Whether `\\.\pipe\dari-service` exists, found without connecting to it.
    fn service_pipe_exists() -> bool {
        let path = wide(SERVICE_PIPE);
        // SAFETY: `path` is NUL-terminated and outlives the call.
        let found = unsafe { WaitNamedPipeW(PCWSTR(path.as_ptr()), 1) }.as_bool();
        found || !is_os_error(&io::Error::last_os_error(), ERROR_FILE_NOT_FOUND.0)
    }

    #[tokio::test]
    async fn the_link_ends_at_once_without_dari_service() {
        if service_pipe_exists() {
            eprintln!("skipped: DariService runs on this machine");
            return;
        }
        let mut link = open(true);
        let event = tokio::time::timeout(SERVICE_TIME, link.next())
            .await
            .expect("the link didn't end in time");
        assert_eq!(
            event,
            Some(SecureDesktopEvent::Ended(
                LinkError::ServiceMissing.to_string()
            ))
        );
    }

    fn own_user() -> String {
        let mut token = HANDLE::default();
        // SAFETY: as in `logon_sid`.
        let token = unsafe {
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token).unwrap();
            OwnedHandle::from_raw_handle(token.0)
        };
        let user = information(&token, TokenUser).unwrap();
        // SAFETY: as in `pipe_client_user`.
        unsafe { sid_string(user.as_ptr().cast::<TOKEN_USER>().read().User.Sid).unwrap() }
    }
}
