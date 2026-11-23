//! Read storage geometry from the volume that backs an existing output directory.

use serde::Serialize;
use std::{
    fs::OpenOptions,
    io,
    mem::size_of,
    os::windows::{
        ffi::OsStrExt,
        fs::OpenOptionsExt,
        io::{AsHandle, AsRawHandle, BorrowedHandle},
    },
    path::Path,
    ptr::null_mut,
};
use windows_sys::Win32::{
    Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_STORAGE_INFO,
        FileStorageInfo, GetDiskFreeSpaceExW, GetDiskFreeSpaceW, GetDriveTypeW,
        GetFileInformationByHandleEx, GetFinalPathNameByHandleW, GetVolumeInformationByHandleW,
        GetVolumePathNameW, VOLUME_NAME_DOS,
    },
    System::WindowsProgramming::{
        DRIVE_CDROM, DRIVE_FIXED, DRIVE_RAMDISK, DRIVE_REMOTE, DRIVE_REMOVABLE,
    },
};

const MAX_PATH_UNITS: usize = 32_768;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DriveType {
    Fixed,
    Removable,
    Remote,
    Optical,
    RamDisk,
    Unknown,
}

#[derive(Debug, Serialize)]
pub struct VolumeInfo {
    pub filesystem: Option<String>,
    pub drive_type: DriveType,
    pub allocation_unit_bytes: Option<u64>,
    pub logical_sector_bytes: Option<u32>,
    pub physical_sector_atomicity_bytes: Option<u32>,
    pub physical_sector_performance_bytes: Option<u32>,
    pub filesystem_effective_atomicity_bytes: Option<u32>,
    pub sector_alignment_offset_bytes: Option<u32>,
    pub partition_alignment_offset_bytes: Option<u32>,
    pub free_bytes_available: Option<u64>,
}

pub fn inspect(path: &Path) -> io::Result<VolumeInfo> {
    let directory = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    if !directory.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected an output directory",
        ));
    }

    let handle = directory.as_handle();
    // Use an absolute path even when handle-based normalization is unavailable.
    // GetVolumePathNameW follows local mount points in the path.
    let resolved = final_path(handle).or_else(|| {
        std::path::absolute(path)
            .ok()
            .map(|path| path.as_os_str().encode_wide().chain([0]).collect())
    });
    let root = resolved.as_deref().and_then(volume_root);
    let geometry = root.as_deref().and_then(cluster_geometry);
    let storage = storage_info(handle);

    Ok(VolumeInfo {
        filesystem: filesystem(handle),
        drive_type: root.as_deref().map_or(DriveType::Unknown, drive_type),
        allocation_unit_bytes: geometry
            .map(|(sectors, bytes)| u64::from(sectors) * u64::from(bytes)),
        logical_sector_bytes: storage
            .and_then(|info| nonzero(info.LogicalBytesPerSector))
            .or_else(|| geometry.map(|(_, bytes)| bytes)),
        physical_sector_atomicity_bytes: storage
            .and_then(|info| nonzero(info.PhysicalBytesPerSectorForAtomicity)),
        physical_sector_performance_bytes: storage
            .and_then(|info| nonzero(info.PhysicalBytesPerSectorForPerformance)),
        filesystem_effective_atomicity_bytes: storage
            .and_then(|info| nonzero(info.FileSystemEffectivePhysicalBytesPerSectorForAtomicity)),
        sector_alignment_offset_bytes: storage
            .and_then(|info| known_offset(info.ByteOffsetForSectorAlignment)),
        partition_alignment_offset_bytes: storage
            .and_then(|info| known_offset(info.ByteOffsetForPartitionAlignment)),
        free_bytes_available: resolved.as_deref().and_then(free_bytes),
    })
}

fn final_path(handle: BorrowedHandle<'_>) -> Option<Vec<u16>> {
    let mut buffer = vec![0; MAX_PATH_UNITS];
    // SAFETY: Handle is live and the output buffer can hold the declared number of WCHARs.
    let written = unsafe {
        GetFinalPathNameByHandleW(
            handle.as_raw_handle(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    } as usize;
    if written == 0 || written >= buffer.len() {
        return None;
    }
    buffer.truncate(written + 1);
    Some(buffer)
}

fn volume_root(path: &[u16]) -> Option<Vec<u16>> {
    let mut root = vec![0; MAX_PATH_UNITS];
    // SAFETY: The path is NUL-terminated and the output buffer matches the declared capacity.
    let ok = unsafe { GetVolumePathNameW(path.as_ptr(), root.as_mut_ptr(), root.len() as u32) };
    (ok != 0).then_some(())?;
    let end = root.iter().position(|&unit| unit == 0)?;
    root.truncate(end + 1);
    (end != 0).then_some(root)
}

fn filesystem(handle: BorrowedHandle<'_>) -> Option<String> {
    let mut name = [0; 261];
    // SAFETY: Handle is live, optional unused outputs are NULL, and the name buffer is writable.
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            handle.as_raw_handle(),
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            null_mut(),
            name.as_mut_ptr(),
            name.len() as u32,
        )
    };
    (ok != 0).then_some(())?;
    let end = name.iter().position(|&unit| unit == 0)?;
    (end != 0).then(|| String::from_utf16_lossy(&name[..end]))
}

fn drive_type(root: &[u16]) -> DriveType {
    // SAFETY: The supplied volume root is NUL-terminated.
    match unsafe { GetDriveTypeW(root.as_ptr()) } {
        DRIVE_FIXED => DriveType::Fixed,
        DRIVE_REMOVABLE => DriveType::Removable,
        DRIVE_REMOTE => DriveType::Remote,
        DRIVE_CDROM => DriveType::Optical,
        DRIVE_RAMDISK => DriveType::RamDisk,
        _ => DriveType::Unknown,
    }
}

fn cluster_geometry(root: &[u16]) -> Option<(u32, u32)> {
    let (mut sectors, mut bytes, mut free_clusters, mut total_clusters) = (0, 0, 0, 0);
    // SAFETY: Root is NUL-terminated and each output refers to a distinct valid DWORD.
    let ok = unsafe {
        GetDiskFreeSpaceW(
            root.as_ptr(),
            &mut sectors,
            &mut bytes,
            &mut free_clusters,
            &mut total_clusters,
        )
    };
    (ok != 0 && sectors != 0 && bytes != 0).then_some((sectors, bytes))
}

fn storage_info(handle: BorrowedHandle<'_>) -> Option<FILE_STORAGE_INFO> {
    let mut info = FILE_STORAGE_INFO::default();
    // SAFETY: The FileStorageInfo class expects exactly this writable struct.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle.as_raw_handle(),
            FileStorageInfo,
            (&mut info as *mut FILE_STORAGE_INFO).cast(),
            size_of::<FILE_STORAGE_INFO>() as u32,
        )
    };
    (ok != 0).then_some(info)
}

fn free_bytes(path: &[u16]) -> Option<u64> {
    let mut bytes = 0;
    // SAFETY: Path is NUL-terminated, bytes is a writable ULARGE_INTEGER, and other outputs are optional.
    let ok = unsafe { GetDiskFreeSpaceExW(path.as_ptr(), &mut bytes, null_mut(), null_mut()) };
    (ok != 0).then_some(bytes)
}

fn nonzero(value: u32) -> Option<u32> {
    (value != 0).then_some(value)
}

fn known_offset(value: u32) -> Option<u32> {
    // FILE_STORAGE_INFO uses STORAGE_INFO_OFFSET_UNKNOWN for unreported offsets.
    (value != u32::MAX).then_some(value)
}
