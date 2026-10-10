use std::io;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::time::Duration;

use dari_proto::PIPE_CLIENT_RIGHTS;
use windows::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED, HANDLE, HLOCAL, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, SECURITY_IDENTIFICATION,
    SECURITY_SQOS_PRESENT, WriteFile,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
    GetNamedPipeClientSessionId, GetNamedPipeServerProcessId, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::core::PCWSTR;

use super::io::{Event, Outcome, overlapped};
use super::{owned, raw, wide};

const BUFFER_SIZE: u32 = 4096;

#[derive(Debug)]
pub(crate) struct Pipe(OwnedHandle);

impl Pipe {
    /// Creates every instance the pipe at `path` will have, failing if any process created that
    /// name first. Each instance gets the security descriptor `sddl`.
    pub(crate) fn create_instances(path: &str, sddl: &str, count: u32) -> io::Result<Vec<Self>> {
        (0..count)
            .map(|index| {
                let first = if index == 0 {
                    FILE_FLAG_FIRST_PIPE_INSTANCE
                } else {
                    FILE_FLAGS_AND_ATTRIBUTES(0)
                };
                Self::create_instance(path, sddl, first, count)
            })
            .collect()
    }

    fn create_instance(
        path: &str,
        sddl: &str,
        first: FILE_FLAGS_AND_ATTRIBUTES,
        count: u32,
    ) -> io::Result<Self> {
        let descriptor = SecurityDescriptor::parse(sddl)?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
            lpSecurityDescriptor: descriptor.0.0,
            bInheritHandle: false.into(),
        };
        let path = wide(path);
        // SAFETY: `path` is NUL-terminated and `attributes` points at `descriptor`, and both
        // outlive the call.
        let pipe = unsafe {
            CreateNamedPipeW(
                PCWSTR(path.as_ptr()),
                PIPE_ACCESS_DUPLEX | first | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                count,
                BUFFER_SIZE,
                BUFFER_SIZE,
                0,
                Some(&raw const attributes),
            )
        };
        if pipe.is_invalid() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `CreateNamedPipeW` just returned this handle to us.
        Ok(Self(unsafe { owned(pipe) }))
    }

    /// Opens the pipe at `path` as a client. The server may identify this process but not
    /// impersonate it.
    pub(crate) fn open(path: &str) -> io::Result<Self> {
        let path = wide(path);
        // SAFETY: `path` is NUL-terminated and outlives the call.
        let pipe = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                PIPE_CLIENT_RIGHTS,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                None,
            )?
        };
        // SAFETY: `CreateFileW` just returned this handle to us.
        Ok(Self(unsafe { owned(pipe) }))
    }

    /// Waits for a client. Returns false if `stop` was set first.
    pub(crate) fn connect(&self, stop: &Event) -> io::Result<bool> {
        let pipe = self.handle();
        let outcome = overlapped(
            pipe,
            |operation| {
                // SAFETY: `overlapped` keeps the `OVERLAPPED` alive until the operation ends.
                unsafe { ConnectNamedPipe(pipe, Some(operation)) }
            },
            None,
            Some(stop),
        )?;
        Ok(matches!(outcome, Outcome::Done(_)))
    }

    /// Drops the connected client, discarding anything it hasn't read.
    pub(crate) fn disconnect(&self) {
        // SAFETY: the handle is a pipe server for as long as `self` lives.
        let _disconnected = unsafe { DisconnectNamedPipe(self.handle()) };
    }

    pub(crate) fn client_process_id(&self) -> io::Result<u32> {
        let mut id = 0;
        // SAFETY: `id` outlives the call that writes it.
        unsafe { GetNamedPipeClientProcessId(self.handle(), &raw mut id)? };
        Ok(id)
    }

    pub(crate) fn client_session_id(&self) -> io::Result<u32> {
        let mut id = 0;
        // SAFETY: `id` outlives the call that writes it.
        unsafe { GetNamedPipeClientSessionId(self.handle(), &raw mut id)? };
        Ok(id)
    }

    pub(crate) fn server_process_id(&self) -> io::Result<u32> {
        let mut id = 0;
        // SAFETY: `id` outlives the call that writes it.
        unsafe { GetNamedPipeServerProcessId(self.handle(), &raw mut id)? };
        Ok(id)
    }

    /// Reads what has arrived, up to `buffer.len()` bytes. Returns 0 once the other end closed,
    /// and a `TimedOut` error after `timeout`.
    pub(crate) fn read(&self, buffer: &mut [u8], timeout: Option<Duration>) -> io::Result<usize> {
        let pipe = self.handle();
        let outcome = overlapped(
            pipe,
            |operation| {
                // SAFETY: `buffer` outlives the operation, which `overlapped` finishes before
                // returning.
                unsafe { ReadFile(pipe, Some(buffer), None, Some(operation)) }
            },
            timeout,
            None,
        );
        match outcome {
            Ok(Outcome::Done(read)) => Ok(read as usize),
            Ok(Outcome::TimedOut | Outcome::Stopped) => Err(io::ErrorKind::TimedOut.into()),
            // The other end closed its handle, or a server disconnected this client. Errors from
            // the `windows` crate carry the HRESULT as their OS error.
            Err(error)
                if [ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED]
                    .iter()
                    .any(|closed| error.raw_os_error() == Some(closed.to_hresult().0)) =>
            {
                Ok(0)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn write_all(&self, mut bytes: &[u8], timeout: Option<Duration>) -> io::Result<()> {
        let pipe = self.handle();
        while !bytes.is_empty() {
            let outcome = overlapped(
                pipe,
                |operation| {
                    // SAFETY: `bytes` outlives the operation, which `overlapped` finishes before
                    // returning.
                    unsafe { WriteFile(pipe, Some(bytes), None, Some(operation)) }
                },
                timeout,
                None,
            )?;
            match outcome {
                Outcome::Done(0) => return Err(io::ErrorKind::WriteZero.into()),
                Outcome::Done(written) => bytes = &bytes[(written as usize).min(bytes.len())..],
                Outcome::TimedOut | Outcome::Stopped => return Err(io::ErrorKind::TimedOut.into()),
            }
        }
        Ok(())
    }

    fn handle(&self) -> HANDLE {
        raw(&self.0)
    }
}

impl AsRawHandle for Pipe {
    fn as_raw_handle(&self) -> std::os::windows::io::RawHandle {
        self.0.as_raw_handle()
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
    use std::thread;

    use windows::Win32::Storage::FileSystem::{FILE_GENERIC_READ, FILE_WRITE_DATA};

    use super::*;
    use crate::test_support::test_pipe;

    #[test]
    fn the_shared_client_rights_are_read_and_write_data() {
        assert_eq!(PIPE_CLIENT_RIGHTS, FILE_GENERIC_READ.0 | FILE_WRITE_DATA.0);
    }

    #[test]
    fn a_read_after_the_other_end_closes_finds_the_end() {
        let (server, path) = test_pipe();
        let client = Pipe::open(&path).unwrap();
        drop(client);
        let mut buffer = [0u8; 16];
        assert_eq!(
            server
                .read(&mut buffer, Some(Duration::from_secs(5)))
                .unwrap(),
            0
        );

        let (server, path) = test_pipe();
        let client = Pipe::open(&path).unwrap();
        server.disconnect();
        assert_eq!(
            client
                .read(&mut buffer, Some(Duration::from_secs(5)))
                .unwrap(),
            0
        );
    }

    #[test]
    fn a_read_that_times_out_loses_nothing_that_arrives_later() {
        let (server, path) = test_pipe();
        let client = Pipe::open(&path).unwrap();
        let mut buffer = [0u8; 16];
        let timed_out = server
            .read(&mut buffer, Some(Duration::from_millis(50)))
            .unwrap_err();
        assert_eq!(timed_out.kind(), io::ErrorKind::TimedOut);

        thread::scope(|scope| {
            scope.spawn(|| client.write_all(b"after", None).unwrap());
            let read = server
                .read(&mut buffer, Some(Duration::from_secs(5)))
                .unwrap();
            assert_eq!(&buffer[..read], b"after");
        });
    }
}
