use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle, RawHandle};
use std::sync::atomic::{AtomicU64, Ordering, fence};
use std::time::{Duration, Instant};

use bytes::BytesMut;
use dari_media::RgbaFrame;
use dari_proto::{
    AppToHelper, FRAME_SECTION_MAGIC, FRAME_SECTION_VERSION, FrameLayout, FrameSlot,
    HelperPipeName, HelperToApp, InputDesktop, LOCAL_FRAME_LIMIT, MessageCodec, PIPE_CLIENT_RIGHTS,
    PIPE_RANDOM_BYTES, Refusal, SERVICE_PIPE, ServiceReply, ServiceRequest,
};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer, PipeMode, ServerOptions};
use tokio_util::codec::{Encoder, FramedRead, FramedWrite};
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
use windows::Win32::System::Memory::{
    FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, UnmapViewOfFile,
};
use windows::Win32::System::Pipes::{ImpersonateNamedPipeClient, WaitNamedPipeW};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken,
};
use windows::core::{PCWSTR, PWSTR};

use super::handover::{Handover, HandoverError, MappedSection, SectionMapper};
use super::{LinkCommand, LinkDriver, SecureDesktopLink};

const LOCAL_SYSTEM: &str = "S-1-5-18";
const SERVICE_TIME: Duration = Duration::from_secs(5);
const HELPER_TIME: Duration = Duration::from_secs(10);
const STOP_TIME: Duration = Duration::from_secs(5);

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
    Handover(#[from] HandoverError),
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

async fn run(input: bool, driver: &mut LinkDriver) -> Result<(), LinkError> {
    let (messages, first) = tokio::select! {
        connected = connect(input) => connected?,
        () = session_gone(driver) => return Ok(()),
    };
    relay(messages, first, driver).await
}

/// Plays what a connected helper says into `driver` until the session drops the link or the
/// helper's pipe ends, then stops the helper.
async fn relay(
    messages: HelperMessages,
    first: InputDesktop,
    driver: &mut LinkDriver,
) -> Result<(), LinkError> {
    driver.desktop_changed(first);
    let (mut messages, mut replies) = split(messages);
    let relayed: Result<(), LinkError> = async {
        if let Some(display) = driver.selected_display() {
            replies.send(&AppToHelper::SelectDisplay(display)).await?;
        }
        let mut handover = Handover::new(ReadOnlySections);
        loop {
            tokio::select! {
                command = driver.command() => match command {
                    Some(LinkCommand::SelectDisplay(display)) => {
                        replies.send(&AppToHelper::SelectDisplay(display)).await?;
                    }
                    None => return Ok(()),
                },
                message = messages.next() => {
                    let message = message.ok_or(LinkError::HelperClosed)??;
                    if let Some(reply) = handover.handle(message, driver).await? {
                        replies.send(&reply).await?;
                    }
                }
            }
        }
    }
    .await;
    stop_helper(&mut messages, &mut replies).await;
    relayed
}

/// Stops the helper and reads until it closes its pipe, for up to `STOP_TIME`. Until then the
/// helper may duplicate a section handle into this process that no message read so far names;
/// each `FrameSection` read here is closed unmapped. A message that doesn't decode ends the read,
/// since nothing after it can be found.
async fn stop_helper(messages: &mut HelperReader, replies: &mut HelperReplies) {
    let drained = async {
        let _sent = replies.send(&AppToHelper::Stop).await;
        while let Some(Ok(message)) = messages.next().await {
            if let HelperToApp::FrameSection { handle, .. } = message {
                drop(section_handle(handle));
            }
        }
    };
    let _drained = tokio::time::timeout(STOP_TIME, drained).await;
}

type HelperReader = FramedRead<ReadHalf<NamedPipeServer>, MessageCodec<HelperToApp>>;
type HelperReplies = FramedWrite<WriteHalf<NamedPipeServer>, MessageCodec<AppToHelper>>;

fn split(messages: HelperMessages) -> (HelperReader, HelperReplies) {
    let parts = messages.into_parts();
    let (reader, writer) = tokio::io::split(parts.io);
    let mut messages = FramedRead::new(reader, parts.codec);
    *messages.read_buffer_mut() = parts.read_buf;
    let replies = FramedWrite::new(writer, MessageCodec::<AppToHelper>::new(LOCAL_FRAME_LIMIT));
    (messages, replies)
}

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

struct ReadOnlySections;

impl SectionMapper for ReadOnlySections {
    type Section = ReadOnlySection;

    fn map(&mut self, handle: u64, layout: FrameLayout) -> io::Result<ReadOnlySection> {
        ReadOnlySection::map(handle, layout)
    }
}

fn section_handle(handle: u64) -> io::Result<OwnedHandle> {
    let raw = usize::try_from(handle).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
    // SAFETY: the helper, which passed the LocalSystem check, duplicated this handle into this
    // process for the app alone, so nothing else owns it; it is closed when the result drops.
    Ok(unsafe { OwnedHandle::from_raw_handle(std::ptr::with_exposed_provenance_mut(raw)) })
}

struct ReadOnlySection {
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    layout: FrameLayout,
}

// SAFETY: the view is only read, through raw pointers and atomic loads, and stays mapped until
// the section is dropped.
unsafe impl Send for ReadOnlySection {}
// SAFETY: as for `Send`; nothing writes through the view.
unsafe impl Sync for ReadOnlySection {}

// Rust allows an atomic load of read-only memory only when it is relaxed and no wider than the
// target's limit, 8 bytes on x86_64 and aarch64. On 32-bit targets the limit is 4 bytes, and a
// 64-bit atomic load may write.
const _: () = assert!(
    cfg!(any(target_arch = "x86_64", target_arch = "aarch64")),
    "the frame section's 64-bit sequence words need 8-byte read-only atomic loads"
);

impl ReadOnlySection {
    fn map(handle: u64, layout: FrameLayout) -> io::Result<Self> {
        let handle = section_handle(handle)?;
        // SAFETY: mapping a section read-only creates a new view and touches no memory Rust owns.
        let view = unsafe {
            MapViewOfFile(
                HANDLE(handle.as_raw_handle()),
                FILE_MAP_READ,
                0,
                0,
                layout.total_len(),
            )
        };
        if view.Value.is_null() {
            return Err(io::Error::last_os_error());
        }
        drop(handle);
        let section = Self { view, layout };
        let header = [
            (FrameLayout::MAGIC_OFFSET, FRAME_SECTION_MAGIC),
            (FrameLayout::VERSION_OFFSET, FRAME_SECTION_VERSION),
            (FrameLayout::WIDTH_OFFSET, layout.width()),
            (FrameLayout::HEIGHT_OFFSET, layout.height()),
        ];
        for (offset, expected) in header {
            let found = section.header_word(offset);
            if found != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("header word at {offset} is {found:#x}, not {expected:#x}"),
                ));
            }
        }
        Ok(section)
    }

    fn base(&self) -> *const c_void {
        self.view.Value.cast_const()
    }

    fn header_word(&self, offset: usize) -> u32 {
        // SAFETY: header offsets are 4-byte aligned and inside the header page, and the helper
        // wrote the header before it sent the handle and never writes it again.
        u32::from_le(unsafe { self.base().byte_add(offset).cast::<u32>().read_volatile() })
    }

    fn sequence(&self, slot: FrameSlot) -> &AtomicU64 {
        // SAFETY: the sequence word is 8-byte aligned inside the header page, which lives as
        // long as `self`, and the helper writes it only atomically. The view is mapped
        // read-only, which allows only relaxed loads of at most 8 bytes on the targets asserted
        // above, so every load of the word is relaxed and the copy is ordered by fences.
        unsafe {
            AtomicU64::from_ptr(
                self.base()
                    .byte_add(FrameLayout::sequence_offset(slot))
                    .cast::<u64>()
                    .cast_mut(),
            )
        }
    }
}

impl MappedSection for ReadOnlySection {
    fn copy(&self, slot: FrameSlot, sequence: u64) -> Option<RgbaFrame> {
        let word = self.sequence(slot);
        if word.load(Ordering::Relaxed) != sequence {
            return None;
        }
        fence(Ordering::Acquire);
        let len = self.layout.slot_len();
        let mut pixels = Vec::<u8>::with_capacity(len);
        // SAFETY: the slot lies inside the view per `FrameLayout`, `pixels` has room for `len`
        // bytes, and the two don't overlap. No reference to the shared bytes is made: the
        // helper doesn't write a slot the app owns, and if it does anyway, the check below
        // discards the copy.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.base()
                    .byte_add(self.layout.slot_offset(slot))
                    .cast::<u8>(),
                pixels.as_mut_ptr(),
                len,
            );
            pixels.set_len(len);
        }
        fence(Ordering::Acquire);
        if word.load(Ordering::Relaxed) != sequence {
            return None;
        }
        RgbaFrame::new(self.layout.width(), self.layout.height(), pixels)
    }
}

impl Drop for ReadOnlySection {
    fn drop(&mut self) {
        // SAFETY: the view came from `MapViewOfFile`, and nothing reads it after this.
        let _unmapped = unsafe { UnmapViewOfFile(self.view) };
    }
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
    use std::io::{Read, Write};

    use windows::Win32::Foundation::{
        CompareObjectHandles, DUPLICATE_SAME_ACCESS, DuplicateHandle, INVALID_HANDLE_VALUE,
    };
    use windows::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_WRITE, PAGE_READWRITE};

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

    struct HelperSection {
        mapping: OwnedHandle,
        view: MEMORY_MAPPED_VIEW_ADDRESS,
    }

    impl HelperSection {
        fn new(len: usize) -> Self {
            let len = u64::try_from(len).unwrap();
            // SAFETY: an unnamed, pagefile-backed section; the handle is wrapped as owned, and
            // the view is unmapped on drop.
            unsafe {
                let mapping = CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    None,
                    PAGE_READWRITE,
                    u32::try_from(len >> 32).unwrap(),
                    u32::try_from(len & 0xffff_ffff).unwrap(),
                    PCWSTR::null(),
                )
                .unwrap();
                let view = MapViewOfFile(mapping, FILE_MAP_WRITE, 0, 0, 0);
                assert!(!view.Value.is_null());
                Self {
                    mapping: OwnedHandle::from_raw_handle(mapping.0),
                    view,
                }
            }
        }

        fn write(&self, offset: usize, bytes: &[u8]) {
            // SAFETY: every caller writes inside the section, which nothing else maps yet.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    self.view.Value.cast::<u8>().add(offset),
                    bytes.len(),
                );
            }
        }

        fn header(&self, width: u32, height: u32) {
            self.write(
                FrameLayout::MAGIC_OFFSET,
                &FRAME_SECTION_MAGIC.to_le_bytes(),
            );
            self.write(
                FrameLayout::VERSION_OFFSET,
                &FRAME_SECTION_VERSION.to_le_bytes(),
            );
            self.write(FrameLayout::WIDTH_OFFSET, &width.to_le_bytes());
            self.write(FrameLayout::HEIGHT_OFFSET, &height.to_le_bytes());
        }

        fn handle(&self) -> u64 {
            let mut duplicate = HANDLE::default();
            // SAFETY: duplicates an open handle within this process; `map` takes ownership.
            unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    HANDLE(self.mapping.as_raw_handle()),
                    GetCurrentProcess(),
                    &raw mut duplicate,
                    0,
                    false,
                    DUPLICATE_SAME_ACCESS,
                )
                .unwrap();
            }
            u64::try_from(duplicate.0.expose_provenance()).unwrap()
        }
    }

    impl Drop for HelperSection {
        fn drop(&mut self) {
            // SAFETY: the view came from `MapViewOfFile` and isn't used after this.
            let _unmapped = unsafe { UnmapViewOfFile(self.view) };
        }
    }

    #[test]
    fn a_mapped_section_copies_a_slot_only_while_its_sequence_matches() {
        let layout = FrameLayout::new(3, 2).unwrap();
        let section = HelperSection::new(layout.total_len());
        section.header(3, 2);
        let pixels: Vec<u8> = (0..24).collect();
        section.write(layout.slot_offset(FrameSlot::Second), &pixels);
        section.write(
            FrameLayout::sequence_offset(FrameSlot::Second),
            &9u64.to_le_bytes(),
        );

        let mapped = ReadOnlySection::map(section.handle(), layout).unwrap();
        let copied = mapped.copy(FrameSlot::Second, 9).unwrap();
        assert_eq!((copied.width(), copied.height()), (3, 2));
        assert_eq!(copied.pixels(), pixels);
        assert!(mapped.copy(FrameSlot::Second, 8).is_none());
        assert!(mapped.copy(FrameSlot::First, 9).is_none());
    }

    #[test]
    fn a_section_whose_header_disagrees_with_its_message_is_refused() {
        let layout = FrameLayout::new(3, 2).unwrap();
        let section = HelperSection::new(layout.total_len());
        section.header(3, 3);
        assert!(ReadOnlySection::map(section.handle(), layout).is_err());
        section.header(3, 2);
        section.write(FrameLayout::VERSION_OFFSET, &2u32.to_le_bytes());
        assert!(ReadOnlySection::map(section.handle(), layout).is_err());
        section.header(3, 2);
        assert!(ReadOnlySection::map(section.handle(), layout).is_ok());
    }

    #[test]
    fn a_section_smaller_than_its_message_says_is_refused() {
        let small = FrameLayout::new(64, 64).unwrap();
        let section = HelperSection::new(small.total_len());
        section.header(1024, 1024);
        let claimed = FrameLayout::new(1024, 1024).unwrap();
        assert!(ReadOnlySection::map(section.handle(), claimed).is_err());
    }

    #[tokio::test]
    async fn a_section_the_helper_sends_as_the_session_ends_is_closed() {
        let pipe = random_pipe_name().unwrap();
        let server = create_helper_pipe(&pipe).unwrap();
        let mut client = client_of(&pipe);
        server.connect().await.unwrap();
        let section = HelperSection::new(FrameLayout::new(1, 1).unwrap().total_len());
        section.header(1, 1);
        let handle = section.handle();
        let helper = tokio::task::spawn_blocking(move || {
            let _read = client.read(&mut [0u8; 64]);
            let _sent = client.write_all(&frame(&HelperToApp::FrameSection {
                handle,
                width: 1,
                height: 1,
            }));
        });

        let (link, mut driver) = SecureDesktopLink::pair();
        drop(link);
        let messages = FramedRead::new(server, MessageCodec::<HelperToApp>::new(LOCAL_FRAME_LIMIT));
        relay(messages, InputDesktop::Winlogon, &mut driver)
            .await
            .unwrap();
        helper.await.unwrap();

        let sent = HANDLE(std::ptr::with_exposed_provenance_mut(
            usize::try_from(handle).unwrap(),
        ));
        // SAFETY: only compares the objects two handle values name; an invalid one names none.
        let leaked = unsafe { CompareObjectHandles(sent, HANDLE(section.mapping.as_raw_handle())) };
        assert!(
            !leaked.as_bool(),
            "the app kept the helper's section handle"
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
