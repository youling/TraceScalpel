//! Explicitly enabled, elevated CI control for Windows system process and file events.
//! The bounded ETL is transient. Reports expose only fixture matches and aggregate counters.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::*;
use serde_json::{Value, json};
use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
use windows_sys::Win32::System::Diagnostics::Etw::{
    ENABLE_TRACE_PARAMETERS, ENABLE_TRACE_PARAMETERS_VERSION_2,
    EVENT_ENABLE_PROPERTY_IGNORE_KEYWORD_0, EVENT_FILTER_DESCRIPTOR, EVENT_FILTER_TYPE_EVENT_ID,
    EVENT_TRACE_FLAG_DISK_FILE_IO, EVENT_TRACE_FLAG_DISK_IO, EVENT_TRACE_FLAG_FILE_IO_INIT,
    EVENT_TRACE_FLAG_NO_SYSCONFIG, EVENT_TRACE_SYSTEM_LOGGER_MODE, PROPERTY_DATA_DESCRIPTOR,
    TdhGetProperty, TdhGetPropertySize,
};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::core::{PCWSTR, w};

mod realtime;

const ETL_MIB: u32 = 4;
const CAPTURE_DEADLINE: Duration = Duration::from_secs(4);
const PROCESS_PROVIDER: GUID = GUID::from_u128(0x22fb2cd6_0e7b_422b_a0c7_2fad1fd0e716);
const FILE_PROVIDER: GUID = GUID::from_u128(0x90cbdc39_4a3e_11d1_84f4_0000f80464e3);
// Process uses the manifest event ID. Classic FileIo uses the MOF event opcode.
const PROCESS_START: u16 = 1;
const FILE_CREATE_OPCODE: u8 = 64;
const PROCESS_KEYWORD: u64 = 0x10;
const FILE_FLAGS: u32 = EVENT_TRACE_FLAG_DISK_IO
    | EVENT_TRACE_FLAG_DISK_FILE_IO
    | EVENT_TRACE_FLAG_FILE_IO_INIT
    | EVENT_TRACE_FLAG_NO_SYSCONFIG;

struct Scratch {
    dir: PathBuf,
    paths: [PathBuf; 3],
}

impl Drop for Scratch {
    fn drop(&mut self) {
        for path in &self.paths {
            // These three paths belong to a fresh, uniquely named fixture directory.
            let _ = fs::remove_file(path);
        }
        let _ = fs::remove_dir(&self.dir);
    }
}

struct SystemSession {
    handle: CONTROLTRACE_HANDLE,
    guid: GUID,
    name: Vec<u16>,
    active: bool,
}

fn properties(guid: GUID) -> TraceProperties {
    let mut p = TraceProperties::new(guid);
    // A uniquely named system logger with the file-create initiation stream.
    p.properties.LogFileMode = EVENT_TRACE_FILE_MODE_SEQUENTIAL | EVENT_TRACE_SYSTEM_LOGGER_MODE;
    p.properties.EnableFlags = FILE_FLAGS;
    p.properties.MaximumFileSize = ETL_MIB;
    p
}

impl SystemSession {
    fn start(guid: GUID, path: &[u16]) -> io::Result<Self> {
        let mut p = properties(guid);
        p.path[..path.len()].copy_from_slice(path);
        let name = session_name(&guid);
        let mut handle = CONTROLTRACE_HANDLE::default();
        // SAFETY: Both terminated names and the full properties allocation are valid.
        win_status(unsafe { StartTraceW(&mut handle, name.as_ptr(), (&raw mut p).cast()) })?;
        Ok(Self {
            handle,
            guid,
            name,
            active: true,
        })
    }

    fn stop(&mut self) -> io::Result<EVENT_TRACE_PROPERTIES> {
        let mut p = properties(self.guid);
        // SAFETY: The acquired handle and unique name belong exclusively to this session.
        win_status(unsafe {
            ControlTraceW(
                self.handle,
                self.name.as_ptr(),
                (&raw mut p).cast(),
                EVENT_TRACE_CONTROL_STOP,
            )
        })?;
        self.active = false;
        Ok(p.properties)
    }
}

impl Drop for SystemSession {
    fn drop(&mut self) {
        if self.active {
            let _ = self.stop();
        }
    }
}

fn enable(
    handle: CONTROLTRACE_HANDLE,
    provider: &GUID,
    events: &[u16],
    keyword: u64,
) -> io::Result<()> {
    // EVENT_FILTER_EVENT_ID has a trailing array and the descriptor requires eight-byte alignment.
    #[repr(C, align(8))]
    struct EventIdFilter {
        filter_in: u8,
        reserved: u8,
        count: u16,
        events: [u16; 2],
    }
    let mut id_filter = EventIdFilter {
        filter_in: 1,
        reserved: 0,
        count: events.len() as u16,
        events: [0; 2],
    };
    id_filter.events[..events.len()].copy_from_slice(events);
    let mut filter = EVENT_FILTER_DESCRIPTOR {
        Ptr: (&raw const id_filter) as u64,
        Size: (offset_of!(EventIdFilter, events) + size_of_val(events)) as u32,
        Type: EVENT_FILTER_TYPE_EVENT_ID,
    };
    let parameters = ENABLE_TRACE_PARAMETERS {
        Version: ENABLE_TRACE_PARAMETERS_VERSION_2,
        EnableProperty: EVENT_ENABLE_PROPERTY_IGNORE_KEYWORD_0,
        EnableFilterDesc: &mut filter,
        FilterDescCount: 1,
        ..Default::default()
    };
    // SAFETY: The provider, parameters, descriptor and aligned filter live through the call.
    win_status(unsafe {
        EnableTraceEx2(
            handle,
            provider,
            EVENT_CONTROL_CODE_ENABLE_PROVIDER,
            TRACE_LEVEL_INFORMATION as u8,
            keyword,
            0,
            1_000,
            &parameters,
        )
    })
}

#[derive(Clone, Copy)]
struct Span {
    before: i64,
    after: i64,
}

impl Span {
    fn contains(self, tick: i64) -> bool {
        (self.before..=self.after).contains(&tick)
    }

    fn report(self) -> Value {
        json!({"qpc_before": self.before.to_string(), "qpc_after": self.after.to_string()})
    }
}

fn bracket<T>(op: impl FnOnce() -> io::Result<T>) -> io::Result<(T, Span)> {
    let before = qpc()?;
    let result = op()?;
    Ok((
        result,
        Span {
            before,
            after: qpc()?,
        },
    ))
}

struct FixtureChild(Child);

impl Drop for FixtureChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn system_cmd() -> io::Result<PathBuf> {
    let mut buffer = [0_u16; 1024];
    // SAFETY: buffer is writable for the supplied number of wide characters.
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    if length >= buffer.len() {
        return Err(io::Error::other(
            "system directory exceeds fixture path buffer",
        ));
    }
    Ok(PathBuf::from(OsString::from_wide(&buffer[..length])).join("cmd.exe"))
}

fn start_child(cmd: &Path) -> io::Result<FixtureChild> {
    Command::new(cmd)
        .args(["/d", "/q", "/c", "exit", "0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(FixtureChild)
}

fn wait_child(child: &mut FixtureChild, deadline: Instant) -> io::Result<()> {
    while child.0.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "fixture child did not exit",
            ));
        }
        thread::sleep(Duration::from_millis(1));
    }
    if !child.0.wait()?.success() {
        return Err(io::Error::other("fixture child failed"));
    }
    Ok(())
}

fn new_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

struct Expected {
    pids: [u32; 2],
    filenames: [Vec<u16>; 2],
    spans: [Span; 4],
}

fn produce(cmd: &Path, scratch: &Scratch, deadline: Instant) -> io::Result<Expected> {
    let (mut positive, positive_process) = bracket(|| start_child(cmd))?;
    let (mut noise, noise_process) = bracket(|| start_child(cmd))?;
    let pids = [positive.0.id(), noise.0.id()];
    let (_, positive_file) = bracket(|| new_file(&scratch.paths[1]))?;
    let (_, noise_file) = bracket(|| new_file(&scratch.paths[2]))?;
    wait_child(&mut positive, deadline)?;
    wait_child(&mut noise, deadline)?;
    Ok(Expected {
        pids,
        filenames: std::array::from_fn(|index| {
            scratch.paths[index + 1]
                .file_name()
                .unwrap()
                .encode_wide()
                .collect()
        }),
        spans: [positive_process, noise_process, positive_file, noise_file],
    })
}

fn property(event: &EVENT_RECORD, name: PCWSTR, buffer: &mut [u8]) -> Result<usize, u32> {
    let descriptor = PROPERTY_DATA_DESCRIPTOR {
        PropertyName: name as u64,
        ArrayIndex: u32::MAX,
        Reserved: 0,
    };
    let mut size = 0;
    // SAFETY: event and its TDH-discoverable payload are callback-scoped. The supplied name,
    // descriptor, size and destination remain valid for the synchronous TDH calls.
    let result = unsafe { TdhGetPropertySize(event, 0, null(), 1, &descriptor, &mut size) };
    if result != ERROR_SUCCESS {
        return Err(result);
    }
    if size as usize > buffer.len() {
        return Err(ERROR_INSUFFICIENT_BUFFER);
    }
    let result =
        unsafe { TdhGetProperty(event, 0, null(), 1, &descriptor, size, buffer.as_mut_ptr()) };
    if result != ERROR_SUCCESS {
        return Err(result);
    }
    Ok(size as usize)
}

fn property_u32(event: &EVENT_RECORD, name: PCWSTR) -> Option<u32> {
    let mut buffer = [0; 4];
    (property(event, name, &mut buffer).ok()? == buffer.len()).then(|| u32::from_le_bytes(buffer))
}

fn classic_thread_id(event: &EVENT_RECORD) -> Option<u32> {
    let mut buffer = [0; 8];
    match property(event, w!("TTID"), &mut buffer).ok()? {
        4 => Some(u32::from_le_bytes(buffer[..4].try_into().ok()?)),
        8 => u32::try_from(u64::from_le_bytes(buffer)).ok(),
        _ => None,
    }
}

fn matches_filename(raw: &[u8], expected: &[u16]) -> bool {
    let (units, remainder) = raw.as_chunks::<2>();
    if !remainder.is_empty() {
        return false;
    }
    let mut chars = units
        .iter()
        .rev()
        .map(|&pair| u16::from_le_bytes(pair))
        .skip_while(|&unit| unit == 0);
    for &expected in expected.iter().rev() {
        let Some(c) = chars.next() else { return false };
        // Fixture basenames are ASCII.
        if c > 0x7f || !(c as u8).eq_ignore_ascii_case(&(expected as u8)) {
            return false;
        }
    }
    chars
        .next()
        .is_none_or(|c| c == u16::from(b'\\') || c == u16::from(b'/'))
}

#[derive(Default)]
struct Hits {
    in_window: u32,
    out_of_window: u32,
    first: Option<(i64, EVENT_DESCRIPTOR)>,
}

impl Hits {
    fn observe(&mut self, descriptor: EVENT_DESCRIPTOR, tick: i64, span: Span) {
        if span.contains(tick) {
            self.in_window = self.in_window.saturating_add(1);
            self.first.get_or_insert((tick, descriptor));
        } else {
            self.out_of_window = self.out_of_window.saturating_add(1);
        }
    }

    fn report(&self, span: Span, expected_at_least: u32, outside_checked: bool) -> Value {
        json!({
            "expected_at_least": expected_at_least,
            "within_fixture_qpc_bracket": self.in_window,
            "matching_identity_outside_bracket": outside_checked.then_some(self.out_of_window),
            "fixture_bracket": span.report(),
            "first_event": self.first.map(|(tick, descriptor)| json!({
                "qpc": tick.to_string(),
                "id": descriptor.Id,
                "opcode": descriptor.Opcode,
                "version": descriptor.Version
            }))
        })
    }
}

struct ReplayState {
    expected: Expected,
    pid: u32,
    tid: u32,
    hits: [Hits; 4],
    process_candidates: u32,
    file_candidates: u32,
    process_decode_failures: u32,
    file_decode_failures: u32,
    other_provider_events: u32,
}

impl ReplayState {
    fn select(&mut self, event: &EVENT_RECORD) -> Option<usize> {
        let header = &event.EventHeader;
        let descriptor = header.EventDescriptor;
        if equal_guid(&header.ProviderId, &PROCESS_PROVIDER) {
            if descriptor.Id != PROCESS_START {
                self.other_provider_events = self.other_provider_events.saturating_add(1);
                return None;
            }
            self.process_candidates = self.process_candidates.saturating_add(1);
            match property_u32(event, w!("ParentProcessID")) {
                Some(parent) if parent != self.pid => return None,
                Some(_) => {}
                None => {
                    self.process_decode_failures = self.process_decode_failures.saturating_add(1);
                    return None;
                }
            }
            if let Some(pid) = property_u32(event, w!("ProcessID")) {
                return self
                    .expected
                    .pids
                    .iter()
                    .position(|&expected| expected == pid);
            }
            self.process_decode_failures = self.process_decode_failures.saturating_add(1);
        } else if equal_guid(&header.ProviderId, &FILE_PROVIDER) {
            if descriptor.Opcode != FILE_CREATE_OPCODE {
                self.other_provider_events = self.other_provider_events.saturating_add(1);
                return None;
            }
            self.file_candidates = self.file_candidates.saturating_add(1);
            if header.ProcessId != self.pid && classic_thread_id(event) != Some(self.tid) {
                return None;
            }
            let mut filename = [0; 4096];
            if let Ok(size) = property(event, w!("OpenPath"), &mut filename) {
                return self
                    .expected
                    .filenames
                    .iter()
                    .position(|expected| matches_filename(&filename[..size], expected))
                    .map(|index| index + 2);
            }
            self.file_decode_failures = self.file_decode_failures.saturating_add(1);
        }
        None
    }
}

unsafe extern "system" fn on_event(raw: *mut EVENT_RECORD) {
    // SAFETY: ProcessTrace provides a callback-scoped record with our live serial replay state.
    let Some(record) = (unsafe { raw.as_ref() }) else {
        return;
    };
    let Some(state) = (unsafe { record.UserContext.cast::<ReplayState>().as_mut() }) else {
        return;
    };
    if let Some(index) = state.select(record) {
        state.hits[index].observe(
            record.EventHeader.EventDescriptor,
            record.EventHeader.TimeStamp,
            state.expected.spans[index],
        );
    }
}

fn read_file(path: &mut [u16], state: &mut ReplayState) -> io::Result<(i64, u32, u32)> {
    let mut log = EVENT_TRACE_LOGFILEW {
        LogFileName: path.as_mut_ptr(),
        Anonymous1: EVENT_TRACE_LOGFILEW_0 {
            ProcessTraceMode: PROCESS_TRACE_MODE_EVENT_RECORD | PROCESS_TRACE_MODE_RAW_TIMESTAMP,
        },
        Anonymous2: EVENT_TRACE_LOGFILEW_1 {
            EventRecordCallback: Some(on_event),
        },
        Context: (state as *mut ReplayState).cast(),
        ..Default::default()
    };
    // SAFETY: All logfile input buffers and our callback state remain live past ProcessTrace.
    let handle = unsafe { OpenTraceW(&mut log) };
    if handle.Value == usize::MAX as u64 {
        return Err(io::Error::last_os_error());
    }
    let reader = TraceReader(handle);
    // SAFETY: This replays one bounded stopped local ETL synchronously.
    win_status(unsafe { ProcessTrace(&reader.0, 1, null::<FILETIME>(), null::<FILETIME>()) })?;
    let header = log.LogfileHeader;
    // SAFETY: This is the documented event-loss union member of an ETL file's header.
    let loss = unsafe { header.Anonymous2.Anonymous.EventsLost };
    Ok((header.PerfFreq, loss, header.BuffersLost))
}

struct ProbeError {
    stage: &'static str,
    error: io::Error,
}

struct CaptureStats {
    events_lost: u32,
    log_buffers_lost: u32,
    buffer_kib: u32,
    buffer_count: u32,
}

fn stage<T>(name: &'static str, result: io::Result<T>) -> Result<T, ProbeError> {
    result.map_err(|error| ProbeError { stage: name, error })
}

fn capture() -> Result<Value, ProbeError> {
    let cmd = stage("system_cmd", system_cmd())?;
    let guid = stage("create_session_identity", new_guid())?;
    let id = guid_string(&guid);
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
    stage("prepare_scratch_root", fs::create_dir_all(&root))?;
    let dir = root.join(format!("sys-etw-{id}"));
    stage("create_scratch", fs::create_dir(&dir))?;
    let scratch = Scratch {
        paths: [
            dir.join("source.etl"),
            dir.join(format!("target-{id}.dat")),
            dir.join(format!("noise-{id}.dat")),
        ],
        dir,
    };
    let mut path = stage("prepare_etl_path", path_wide(&scratch.paths[0]))?;
    let before = stage("sample_start_qpc", qpc())?;
    let mut session = stage("start_session", SystemSession::start(guid, &path))?;
    let handle = session.handle;
    let deadline = Instant::now() + CAPTURE_DEADLINE;
    let (expected, stats, after, timed_out) = thread::scope(|scope| {
        let (finish, requested) = mpsc::channel();
        let watch = scope.spawn(move || {
            let stop_signal_missing = requested
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .is_err();
            let timed_out = stop_signal_missing || Instant::now() >= deadline;
            let native = stage("stop_session", session.stop())?;
            let after = stage("sample_stop_qpc", qpc())?;
            Ok((
                CaptureStats {
                    events_lost: native.EventsLost,
                    log_buffers_lost: native.LogBuffersLost,
                    buffer_kib: native.BufferSize,
                    buffer_count: native.NumberOfBuffers,
                },
                after,
                timed_out,
            ))
        });
        let observed = (|| {
            stage(
                "enable_kernel_process",
                enable(handle, &PROCESS_PROVIDER, &[PROCESS_START], PROCESS_KEYWORD),
            )?;
            stage("emit_owned_controls", produce(&cmd, &scratch, deadline))
        })();
        let _ = finish.send(());
        let (stats, after, timed_out) = watch.join().expect("native ETW stop watchdog panicked")?;
        Ok((observed?, stats, after, timed_out))
    })?;

    let etl_len = stage("inspect_etl_size", fs::metadata(&scratch.paths[0]))?.len();
    stage(
        "check_etl_cap",
        if (1..=u64::from(ETL_MIB) * 1024 * 1024).contains(&etl_len) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ETL exceeded fixture size bounds",
            ))
        },
    )?;
    let mut state = ReplayState {
        expected,
        pid: std::process::id(),
        // SAFETY: The current thread pseudo-API has no inputs.
        tid: unsafe { GetCurrentThreadId() },
        hits: std::array::from_fn(|_| Hits::default()),
        process_candidates: 0,
        file_candidates: 0,
        process_decode_failures: 0,
        file_decode_failures: 0,
        other_provider_events: 0,
    };
    let (frequency_from_etl, etl_event_loss, etl_buffer_loss) =
        stage("decode_etl", read_file(&mut path, &mut state))?;
    let qpc_frequency = stage("sample_qpc_frequency", frequency())?;
    let loss_free = stats.events_lost == 0
        && stats.log_buffers_lost == 0
        && etl_event_loss == 0
        && etl_buffer_loss == 0;
    let succeeded = !timed_out
        && frequency_from_etl == qpc_frequency
        && loss_free
        && state.hits.iter().all(|hits| hits.in_window > 0);
    let [positive_process, noise_process, positive_file, noise_file] = state.hits;
    let [
        target_proc_span,
        noise_proc_span,
        target_file_span,
        noise_file_span,
    ] = state.expected.spans;
    Ok(json!({
        "status": if succeeded { "owned_controls_observed" } else { "incomplete" },
        "token_elevated": true,
        "capture_mode": "named_system_logger_etl",
        "clock": {
            "kind": "raw_qpc", "frequency_hz": qpc_frequency.to_string(),
            "etl_frequency_hz": frequency_from_etl.to_string(),
            "session_bracket": Span { before, after }.report()
        },
        "stop": if timed_out { "watchdog_stop_succeeded" } else { "requested_stop_succeeded" },
        "limits": {
            "capture_deadline_ms": CAPTURE_DEADLINE.as_millis(),
            "etl_max_bytes": u64::from(ETL_MIB) * 1024 * 1024,
            "etl_observed_bytes": etl_len,
            "effective_buffer_kib": stats.buffer_kib,
            "effective_buffers": stats.buffer_count
        },
        "loss": {
            "session_events": stats.events_lost, "session_log_buffers": stats.log_buffers_lost,
            "etl_events": etl_event_loss, "etl_buffers": etl_buffer_loss
        },
        "scope": {
            "process_enable_filters": "provider_keyword_and_event_id",
            "file_enable_filters": "system_file_io_init_flags",
            "identity": "owned_process_and_exact_fixture_file_postfilter",
            "other_events_from_enabled_providers": state.other_provider_events
        },
        "process": {
            "provider": "Microsoft-Windows-Kernel-Process",
            "guid": guid_string(&PROCESS_PROVIDER),
            "event_id": PROCESS_START, "keyword": PROCESS_KEYWORD,
            "candidates": state.process_candidates,
            "decode_failures": state.process_decode_failures,
            "target": positive_process.report(target_proc_span, 1, true),
            "negative_control": noise_process.report(noise_proc_span, 1, true)
        },
        "file": {
            "provider": "SystemTraceProvider/FileIo",
            "guid": guid_string(&FILE_PROVIDER),
            "opcode": FILE_CREATE_OPCODE, "kernel_enable_flags": FILE_FLAGS,
            "candidates": state.file_candidates,
            "decode_failures": state.file_decode_failures,
            "target": positive_file.report(target_file_span, 1, true),
            "negative_control": noise_file.report(noise_file_span, 1, true)
        }
    }))
}

#[test]
#[ignore = "requires isolated elevated Windows CI and explicit opt-in"]
fn owned_system_process_file_control() {
    assert_eq!(
        std::env::var("TRACESCALPEL_SYSTEM_ETW_CONTROL")
            .ok()
            .as_deref(),
        Some("1"),
        "explicit opt-in required for a bounded standard ETW control"
    );
    if !crate::startup::token_elevated().expect("read token elevation") {
        println!(
            "{}",
            json!({"status": "unavailable", "token_elevated": false, "stage": "elevation_gate"})
        );
        panic!("system ETW control requires an already elevated token");
    }
    match capture() {
        Ok(report) => {
            println!("{report}");
            assert_eq!(report["status"], "owned_controls_observed");
        }
        Err(failure) => {
            println!(
                "{}",
                json!({
                    "status": "unavailable",
                    "token_elevated": true,
                    "stage": failure.stage,
                    "win32_error": failure.error.raw_os_error(),
                    "error_kind": format!("{:?}", failure.error.kind())
                })
            );
            panic!("owned kernel ETL probe failed in {}", failure.stage);
        }
    }
}
