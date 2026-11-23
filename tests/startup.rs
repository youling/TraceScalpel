#![cfg(windows)]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::process::{Command, Stdio};
use std::ptr::{null_mut, without_provenance};
use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{
    FindResourceW, LOAD_LIBRARY_AS_DATAFILE, LOAD_LIBRARY_AS_IMAGE_RESOURCE, LoadLibraryExW,
    LoadResource, LockResource, SizeofResource,
};

struct MappedImage(HMODULE);

impl Drop for MappedImage {
    fn drop(&mut self) {
        // SAFETY: This is the owned data-file mapping returned by LoadLibraryExW.
        unsafe { FreeLibrary(self.0) };
    }
}

fn embedded_manifest(exe: &str) -> String {
    let path: Vec<u16> = OsStr::new(exe).encode_wide().chain([0]).collect();
    // SAFETY: The path is NUL-terminated. DATAFILE and IMAGE_RESOURCE avoid executing the file.
    let image = unsafe {
        LoadLibraryExW(
            path.as_ptr(),
            null_mut(),
            LOAD_LIBRARY_AS_DATAFILE | LOAD_LIBRARY_AS_IMAGE_RESOURCE,
        )
    };
    assert!(!image.is_null(), "map executable for resource inspection");
    let image = MappedImage(image);
    // SAFETY: Integer resource ID 1 and type RT_MANIFEST (24) are valid Win32 resource selectors.
    let resource = unsafe { FindResourceW(image.0, without_provenance(1), without_provenance(24)) };
    assert!(
        !resource.is_null(),
        "expected embedded application manifest"
    );
    // SAFETY: Resource and image handles remain valid while inspecting the resource.
    let size = unsafe { SizeofResource(image.0, resource) } as usize;
    let handle = unsafe { LoadResource(image.0, resource) };
    assert!(!handle.is_null() && (1..32768).contains(&size));
    let data = unsafe { LockResource(handle) };
    assert!(!data.is_null());
    // SAFETY: The mapped resource remains live and is readable for SizeofResource bytes.
    let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size) };
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn capture_startup_requests_administrator_at_process_creation() {
    let capture = embedded_manifest(env!("CARGO_BIN_EXE_tracescalpel-capture"));
    assert!(capture.contains("requestedExecutionLevel"));
    assert!(capture.contains("requireAdministrator"), "{capture}");
    let evidence = embedded_manifest(env!("CARGO_BIN_EXE_tracescalpel"));
    assert!(evidence.contains("asInvoker"), "{evidence}");
    assert!(!evidence.contains("requireAdministrator"));
}

#[test]
fn direct_launch_respects_the_current_windows_token() {
    let result = Command::new(env!("CARGO_BIN_EXE_tracescalpel-capture"))
        .env("TRACESCALPEL_EVIDENCE_DIR", env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::null())
        .output();
    match result {
        Ok(output) => assert!(
            output.status.success(),
            "elevated startup failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(error) => assert_eq!(error.raw_os_error(), Some(740), "{error}"),
    }
}
