mod volume;

use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;

use serde::Serialize;
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

#[derive(Serialize)]
pub struct StartupStatus {
    token_elevated: bool,
    volume: volume::VolumeInfo,
}

pub fn evidence_directory() -> io::Result<PathBuf> {
    let root = std::env::var_os("TRACESCALPEL_EVIDENCE_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "set TRACESCALPEL_EVIDENCE_DIR to an existing private evidence directory",
            )
        })?;
    let root = std::path::absolute(root)?;
    if !std::fs::metadata(&root)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "evidence directory must be an existing directory",
        ));
    }
    Ok(root)
}

pub(crate) fn token_elevated() -> io::Result<bool> {
    let mut token = std::ptr::null_mut();
    // SAFETY: The pseudo-process handle is current and the output points to one HANDLE.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenProcessToken returned a new, owned token handle.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut elevation = TOKEN_ELEVATION::default();
    let mut written = 0;
    // SAFETY: The token stays live and both output pointers point to writable storage.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut written,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(elevation.TokenIsElevated != 0)
}

pub fn inspect() -> io::Result<StartupStatus> {
    Ok(StartupStatus {
        token_elevated: token_elevated()?,
        volume: volume::inspect(&evidence_directory()?)?,
    })
}

pub fn capture_preflight() -> io::Result<()> {
    if !token_elevated()? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "live capture entry requires an elevated Windows token at startup",
        ));
    }
    volume::inspect(&evidence_directory()?)?;
    Ok(())
}
