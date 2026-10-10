use std::io;
use std::os::windows::io::OwnedHandle;

use windows::Win32::Foundation::{HANDLE, HLOCAL, LUID, LocalFree};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{
    GetTokenInformation, LookupPrivilegeNameW, PSID, SE_PRIVILEGE_ENABLED, TOKEN_INFORMATION_CLASS,
    TOKEN_MANDATORY_LABEL, TOKEN_PRIVILEGES, TOKEN_QUERY, TOKEN_USER, TokenIntegrityLevel,
    TokenPrivileges, TokenSessionId, TokenUser,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{PCWSTR, PWSTR};

use super::{from_wide, owned, raw};

pub(super) const LOCAL_SYSTEM: &str = "S-1-5-18";

pub(crate) fn own_identity() -> io::Result<String> {
    let token = own_token()?;
    let user = token_user(&token)?;
    let integrity = information(&token, TokenIntegrityLevel)?;
    let session = information(&token, TokenSessionId)?;
    let privileges = information(&token, TokenPrivileges)?;
    // SAFETY: each buffer holds the structure its information class names, written by
    // `GetTokenInformation`, and the SIDs they point to live inside the same buffers.
    // `GetTokenInformation` wrote `PrivilegeCount` entries after the header, so the slice stays
    // inside `privileges`.
    unsafe {
        let integrity = sid_string(
            integrity
                .as_ptr()
                .cast::<TOKEN_MANDATORY_LABEL>()
                .read()
                .Label
                .Sid,
        )?;
        let session = session.as_ptr().cast::<u32>().read();
        let list = privileges.as_ptr().cast::<TOKEN_PRIVILEGES>();
        let count = (*list).PrivilegeCount as usize;
        let entries = std::slice::from_raw_parts(
            (&raw const (*list).Privileges).cast::<windows::Win32::Security::LUID_AND_ATTRIBUTES>(),
            count,
        );
        let privileges: Vec<String> = entries
            .iter()
            .map(|entry| {
                let name = privilege_name(entry.Luid);
                if entry.Attributes.contains(SE_PRIVILEGE_ENABLED) {
                    format!("{name} (enabled)")
                } else {
                    name
                }
            })
            .collect();
        Ok(format!(
            "user {user}, integrity {integrity} ({}), session {session}, {count} privileges: {}",
            integrity_name(&integrity),
            privileges.join(", ")
        ))
    }
}

pub(crate) fn own_user() -> io::Result<String> {
    token_user(&own_token()?)
}

fn own_token() -> io::Result<OwnedHandle> {
    let mut token = HANDLE::default();
    // SAFETY: the pseudo-handle needs no closing, and the token handle is wrapped as owned.
    unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token)?;
        Ok(owned(token))
    }
}

pub(super) fn token_user(token: &OwnedHandle) -> io::Result<String> {
    let user = information(token, TokenUser)?;
    // SAFETY: the buffer holds a `TOKEN_USER` whose SID lives in the same buffer.
    unsafe { sid_string(user.as_ptr().cast::<TOKEN_USER>().read().User.Sid) }
}

fn integrity_name(sid: &str) -> &'static str {
    match sid {
        "S-1-16-0" => "Untrusted",
        "S-1-16-4096" => "Low",
        "S-1-16-8192" => "Medium",
        "S-1-16-12288" => "High",
        "S-1-16-16384" => "System",
        _ => "other",
    }
}

/// A token information buffer, aligned for the structures it holds.
fn information(token: &OwnedHandle, class: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<u64>> {
    let mut needed = 0u32;
    // SAFETY: a size query; it fails with ERROR_INSUFFICIENT_BUFFER and sets `needed`.
    let _sized = unsafe { GetTokenInformation(raw(token), class, None, 0, &raw mut needed) };
    let mut buffer = vec![0u64; (needed as usize).div_ceil(size_of::<u64>())];
    // SAFETY: `buffer` holds at least `needed` bytes and outlives the call.
    unsafe {
        GetTokenInformation(
            raw(token),
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
        let _freed = LocalFree(Some(HLOCAL(text.0.cast())));
        Ok(string)
    }
}

fn privilege_name(luid: LUID) -> String {
    let mut name = [0u16; 128];
    let mut len = u32::try_from(name.len()).unwrap_or(0);
    // SAFETY: `name` holds `len` units and outlives the call.
    let found = unsafe {
        LookupPrivilegeNameW(
            PCWSTR::null(),
            &raw const luid,
            Some(PWSTR(name.as_mut_ptr())),
            &raw mut len,
        )
    };
    match found {
        Ok(()) => from_wide(&name),
        Err(_) => format!("LUID {}:{}", luid.HighPart, luid.LowPart),
    }
}
