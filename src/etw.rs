//! Windows ETL baseline for a private session with one fixture-owned provider.

use std::fs;
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::null;

use serde::{Serialize, Serializer};
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, FILETIME};
use windows_sys::Win32::System::Com::CoCreateGuid;
use windows_sys::Win32::System::Diagnostics::Etw::{
    CONTROLTRACE_HANDLE, CloseTrace, ControlTraceW, EVENT_CONTROL_CODE_DISABLE_PROVIDER,
    EVENT_CONTROL_CODE_ENABLE_PROVIDER, EVENT_DATA_DESCRIPTOR, EVENT_DESCRIPTOR, EVENT_RECORD,
    EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_FILE_MODE_SEQUENTIAL, EVENT_TRACE_LOGFILEW,
    EVENT_TRACE_LOGFILEW_0, EVENT_TRACE_LOGFILEW_1, EVENT_TRACE_PRIVATE_IN_PROC,
    EVENT_TRACE_PRIVATE_LOGGER_MODE, EVENT_TRACE_PROPERTIES, EnableTraceEx2, EventRegister,
    EventUnregister, EventWrite, OpenTraceW, PROCESS_TRACE_MODE_EVENT_RECORD,
    PROCESS_TRACE_MODE_RAW_TIMESTAMP, PROCESSTRACE_HANDLE, ProcessTrace, REGHANDLE, StartTraceW,
    TRACE_LEVEL_INFORMATION, WNODE_FLAG_TRACED_GUID,
};
use windows_sys::core::GUID;

use crate::timebase::{frequency, qpc};

const EVENT_COUNT: usize = 64;
const ETL_MAX_MIB: u32 = 1;
const MAX_NAME_CHARS: usize = 1024;
const MAX_FILE_CHARS: usize = 1024;
const LOGGER_MODE: u32 = EVENT_TRACE_PRIVATE_LOGGER_MODE
    | EVENT_TRACE_PRIVATE_IN_PROC
    | EVENT_TRACE_FILE_MODE_SEQUENTIAL;
const EVENT: EVENT_DESCRIPTOR = EVENT_DESCRIPTOR {
    Id: 1,
    Version: 1,
    Channel: 0,
    Level: TRACE_LEVEL_INFORMATION as u8,
    Opcode: 0,
    Task: 0,
    Keyword: 0,
};

#[derive(Debug, Serialize)]
pub struct EtwFixture {
    pub schema_version: &'static str,
    pub mode: &'static str,
    pub provider_guid: String,
    #[serde(serialize_with = "serialize_ticks")]
    pub qpc_frequency_hz: i64,
    pub etl_bytes: u64,
    pub events_lost: u32,
    pub log_buffers_lost: u32,
    pub etl_events_lost: u32,
    pub etl_buffers_lost: u32,
    pub events: Vec<EtwEvent>,
}

#[derive(Debug, Serialize)]
pub struct EtwEvent {
    pub sequence: u32,
    pub process_id: u32,
    #[serde(serialize_with = "serialize_ticks")]
    pub qpc: i64,
    #[serde(serialize_with = "serialize_ticks")]
    pub qpc_before: i64,
    #[serde(serialize_with = "serialize_ticks")]
    pub qpc_after: i64,
}

fn serialize_ticks<S: Serializer>(ticks: &i64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(ticks)
}

fn win_status(code: u32) -> io::Result<()> {
    if code == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code as i32))
    }
}

fn new_guid() -> io::Result<GUID> {
    let mut guid = GUID::default();
    // SAFETY: The out-parameter points to initialized, writable GUID storage.
    let status = unsafe { CoCreateGuid(&mut guid) };
    if status < 0 {
        return Err(io::Error::other(format!(
            "CoCreateGuid failed with HRESULT {status:#010x}"
        )));
    }
    Ok(guid)
}

fn session_name(guid: &GUID) -> Vec<u16> {
    format!("TraceScalpel-{}", guid_string(guid))
        .encode_utf16()
        .chain([0])
        .collect()
}

fn guid_string(guid: &GUID) -> String {
    let suffix = u64::from_be_bytes(guid.data4);
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        guid.data1,
        guid.data2,
        guid.data3,
        suffix >> 48,
        suffix & 0x0000_ffff_ffff_ffff
    )
}

fn path_wide(path: &Path) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.len() >= MAX_FILE_CHARS || wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ETL path too long or invalid",
        ));
    }
    wide.push(0);
    Ok(wide)
}

// ETW expects the properties and both NUL-terminated strings in one writable allocation.
#[repr(C)]
struct TraceProperties {
    properties: EVENT_TRACE_PROPERTIES,
    name: [u16; MAX_NAME_CHARS],
    path: [u16; MAX_FILE_CHARS],
}

impl TraceProperties {
    fn new(guid: GUID) -> Self {
        let mut properties = EVENT_TRACE_PROPERTIES::default();
        properties.Wnode.BufferSize = size_of::<Self>() as u32;
        properties.Wnode.Guid = guid;
        properties.Wnode.ClientContext = 1; // QueryPerformanceCounter.
        properties.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        properties.LoggerNameOffset = offset_of!(Self, name) as u32;
        properties.LogFileNameOffset = offset_of!(Self, path) as u32;
        properties.LogFileMode = LOGGER_MODE;
        properties.BufferSize = 16; // KiB.
        properties.MinimumBuffers = 2;
        properties.MaximumBuffers = 8;
        properties.MaximumFileSize = ETL_MAX_MIB;
        Self {
            properties,
            name: [0; MAX_NAME_CHARS],
            path: [0; MAX_FILE_CHARS],
        }
    }
}

struct Provider(REGHANDLE);

impl Provider {
    fn register(guid: &GUID) -> io::Result<Self> {
        let mut registration = 0;
        // SAFETY: Both argument buffers are live during the call. No callback is registered.
        win_status(unsafe { EventRegister(guid, None, null(), &mut registration) })?;
        Ok(Self(registration))
    }

    fn emit(&self, sequence: u32) -> io::Result<()> {
        let data = sequence.to_le_bytes();
        let descriptor = EVENT_DATA_DESCRIPTOR {
            Ptr: data.as_ptr() as u64,
            Size: data.len() as u32,
            ..Default::default()
        };
        // SAFETY: Both descriptors and the payload remain live through the synchronous call.
        win_status(unsafe { EventWrite(self.0, &EVENT, 1, &descriptor) })
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        // SAFETY: This registration is owned and has no registered callback.
        unsafe { EventUnregister(self.0) };
    }
}

struct Session {
    handle: CONTROLTRACE_HANDLE,
    guid: GUID,
    name: Vec<u16>,
    stopped: bool,
}

impl Session {
    fn start(guid: GUID, path: &[u16]) -> io::Result<Self> {
        let mut props = TraceProperties::new(guid);
        props.path[..path.len()].copy_from_slice(path);
        let name = session_name(&guid);
        let mut handle = CONTROLTRACE_HANDLE::default();
        // SAFETY: props holds both name buffers described by its offsets. The separate name
        // passed to StartTraceW is NUL-terminated and is copied by the system.
        win_status(unsafe { StartTraceW(&mut handle, name.as_ptr(), (&raw mut props).cast()) })?;
        Ok(Self {
            handle,
            guid,
            name,
            stopped: false,
        })
    }

    fn set_enabled(&self, action: u32) -> io::Result<()> {
        // SAFETY: The handle identifies our session. The GUID was registered by this process.
        win_status(unsafe {
            EnableTraceEx2(
                self.handle,
                &self.guid,
                action,
                EVENT.Level,
                0,
                0,
                1_000,
                null(),
            )
        })
    }

    fn stop(&mut self) -> io::Result<EVENT_TRACE_PROPERTIES> {
        let mut props = TraceProperties::new(self.guid);
        // SAFETY: The handle and exact name identify only the session successfully created here.
        win_status(unsafe {
            ControlTraceW(
                self.handle,
                self.name.as_ptr(),
                (&raw mut props).cast(),
                EVENT_TRACE_CONTROL_STOP,
            )
        })?;
        self.stopped = true;
        Ok(props.properties)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.stopped {
            let _ = self.set_enabled(EVENT_CONTROL_CODE_DISABLE_PROVIDER);
            let _ = self.stop();
        }
    }
}

struct ReaderState {
    guid: GUID,
    process_id: u32,
    timestamps: [Option<i64>; EVENT_COUNT],
    invalid_record: bool,
}

fn equal_guid(left: &GUID, right: &GUID) -> bool {
    left.data1 == right.data1
        && left.data2 == right.data2
        && left.data3 == right.data3
        && left.data4 == right.data4
}

unsafe extern "system" fn event_callback(raw: *mut EVENT_RECORD) {
    // SAFETY: ETW provides a callback-scoped record and our user context from OpenTraceW.
    let Some(record) = (unsafe { raw.as_ref() }) else {
        return;
    };
    // SAFETY: ProcessTrace runs this callback serially. Its caller keeps the mutable state live.
    let Some(state) = (unsafe { record.UserContext.cast::<ReaderState>().as_mut() }) else {
        return;
    };
    let header = &record.EventHeader;
    if !equal_guid(&header.ProviderId, &state.guid) {
        return;
    }
    if header.ProcessId != state.process_id
        || header.EventDescriptor.Id != EVENT.Id
        || header.EventDescriptor.Version != EVENT.Version
        || record.UserDataLength as usize != size_of::<u32>()
        || record.UserData.is_null()
    {
        state.invalid_record = true;
        return;
    }
    // SAFETY: UserDataLength establishes four readable bytes; the pointer need not be aligned.
    let sequence = u32::from_le(unsafe { record.UserData.cast::<u32>().read_unaligned() });
    match state
        .timestamps
        .get_mut(sequence.saturating_sub(1) as usize)
    {
        Some(slot) if sequence != 0 && slot.is_none() => *slot = Some(header.TimeStamp),
        _ => state.invalid_record = true,
    }
}

struct TraceReader(PROCESSTRACE_HANDLE);

impl Drop for TraceReader {
    fn drop(&mut self) {
        // SAFETY: The handle is owned and no ProcessTrace call is active.
        unsafe { CloseTrace(self.0) };
    }
}

fn replay(path: &mut [u16], state: &mut ReaderState) -> io::Result<(i64, u32, u32)> {
    let mut logfile = EVENT_TRACE_LOGFILEW {
        LogFileName: path.as_mut_ptr(),
        Anonymous1: EVENT_TRACE_LOGFILEW_0 {
            ProcessTraceMode: PROCESS_TRACE_MODE_EVENT_RECORD | PROCESS_TRACE_MODE_RAW_TIMESTAMP,
        },
        Anonymous2: EVENT_TRACE_LOGFILEW_1 {
            EventRecordCallback: Some(event_callback),
        },
        Context: (state as *mut ReaderState).cast(),
        ..Default::default()
    };
    // SAFETY: logfile and all pointers it holds are kept live until the reader is closed.
    let handle = unsafe { OpenTraceW(&mut logfile) };
    if handle.Value == usize::MAX as u64 {
        return Err(io::Error::last_os_error());
    }
    let reader = TraceReader(handle);
    // SAFETY: ProcessTrace borrows one valid handle and invokes callbacks synchronously on this
    // thread. Null time pointers select the whole bounded fixture file.
    win_status(unsafe { ProcessTrace(&reader.0, 1, null::<FILETIME>(), null::<FILETIME>()) })?;
    let header = logfile.LogfileHeader;
    // SAFETY: This is the documented EventsLost member of an ETL file's header.
    let etl_events_lost = unsafe { header.Anonymous2.Anonymous.EventsLost };
    Ok((header.PerfFreq, etl_events_lost, header.BuffersLost))
}

fn emit_bracketed(provider: &Provider) -> io::Result<[(i64, i64); EVENT_COUNT]> {
    let mut intervals = [(0, 0); EVENT_COUNT];
    for (index, interval) in intervals.iter_mut().enumerate() {
        let before = qpc()?;
        provider.emit((index + 1) as u32)?;
        *interval = (before, qpc()?);
    }
    Ok(intervals)
}

/// Record and replay only this process's synthetic provider in a caller-owned directory.
/// The native evidence file is preserved as `source.etl` on success.
pub fn run_owned_in(dir: &Path) -> io::Result<EtwFixture> {
    let path = std::path::absolute(dir)?.join("source.etl");
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "source.etl already exists",
            ));
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => (),
        Err(err) => return Err(err),
    }
    let mut wide_path = path_wide(&path)?;
    let guid = new_guid()?;
    let provider = Provider::register(&guid)?;
    let mut session = Session::start(guid, &wide_path)?;
    session.set_enabled(EVENT_CONTROL_CODE_ENABLE_PROVIDER)?;

    let intervals = emit_bracketed(&provider)?;
    session.set_enabled(EVENT_CONTROL_CODE_DISABLE_PROVIDER)?;
    let stats = session.stop()?;
    let size = fs::metadata(&path)?.len();
    if size == 0 || size > u64::from(ETL_MAX_MIB) * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ETL length outside fixture bounds",
        ));
    }

    let mut state = ReaderState {
        guid,
        process_id: std::process::id(),
        timestamps: [None; EVENT_COUNT],
        invalid_record: false,
    };
    let (etl_frequency, etl_events_lost, etl_buffers_lost) = replay(&mut wide_path, &mut state)?;
    let frequency = frequency()?;
    if frequency != etl_frequency || state.invalid_record {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid ETL clock or fixture record",
        ));
    }

    let mut events = Vec::with_capacity(EVENT_COUNT);
    for (index, (timestamp, (before, after))) in
        state.timestamps.into_iter().zip(intervals).enumerate()
    {
        let sequence = (index + 1) as u32;
        let qpc = timestamp.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("missing ETW fixture event {sequence}"),
            )
        })?;
        if qpc < before || qpc > after {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "ETW fixture event {sequence} outside QPC bracket: {qpc}, expected {before}..={after}"
                ),
            ));
        }
        events.push(EtwEvent {
            sequence,
            process_id: state.process_id,
            qpc,
            qpc_before: before,
            qpc_after: after,
        });
    }
    Ok(EtwFixture {
        schema_version: "tracescalpel.etw-fixture.v1",
        mode: "private_in_process_etl",
        provider_guid: guid_string(&guid),
        qpc_frequency_hz: frequency,
        etl_bytes: size,
        events_lost: stats.EventsLost,
        log_buffers_lost: stats.LogBuffersLost,
        etl_events_lost,
        etl_buffers_lost,
        events,
    })
}

#[cfg(test)]
mod benchmark;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_etl_round_trip() {
        let guid = new_guid().unwrap();
        let suffix = u64::from_be_bytes(guid.data4);
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        fs::create_dir_all(&root).unwrap();
        let directory = root.join(format!("tracescalpel-test-{suffix:016x}"));
        fs::create_dir(&directory).unwrap();
        let report = run_owned_in(&directory);
        let duplicate = run_owned_in(&directory);
        let cleanup_file = fs::remove_file(directory.join("source.etl"));
        let cleanup_dir = fs::remove_dir(&directory);
        let report = report.unwrap();
        assert_eq!(report.events.len(), EVENT_COUNT);
        assert_eq!(report.events_lost, 0);
        assert_eq!(report.log_buffers_lost, 0);
        assert_eq!(report.etl_events_lost, 0);
        assert_eq!(report.etl_buffers_lost, 0);
        assert_eq!(duplicate.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        let serialized = serde_json::to_value(&report).unwrap();
        assert!(serialized["events"][0]["qpc"].is_string());
        cleanup_file.unwrap();
        cleanup_dir.unwrap();
    }
}
