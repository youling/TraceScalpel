//! Aggregate-only live control, restricted to a disposable elevated CI VM.

use std::sync::{Arc, Mutex, MutexGuard};

use super::*;
use crate::survey::{Kind, Survey};
use windows_sys::Win32::System::Diagnostics::Etw::{
    EVENT_TRACE_CONTROL_FLUSH, EVENT_TRACE_REAL_TIME_MODE, PROCESS_TRACE_MODE_REAL_TIME,
};

const LIVE_DEADLINE: Duration = Duration::from_secs(8);
const PHASE_DEADLINE: Duration = Duration::from_secs(3);
const MEASURE_SECONDS: u32 = 2;
const BURSTS: u32 = 5;
const OPENS_PER_BURST: u32 = 64;
const BURST_SPACING: Duration = Duration::from_millis(350);

fn live_properties(guid: GUID) -> TraceProperties {
    let mut props = properties(guid);
    props.properties.LogFileMode = EVENT_TRACE_SYSTEM_LOGGER_MODE | EVENT_TRACE_REAL_TIME_MODE;
    props.properties.LogFileNameOffset = 0;
    props.properties.MaximumFileSize = 0;
    props.properties.FlushTimer = 1;
    props
}

fn start_realtime(guid: GUID) -> io::Result<SystemSession> {
    let mut props = live_properties(guid);
    let name = session_name(&guid);
    let mut handle = CONTROLTRACE_HANDLE::default();
    // SAFETY: The unique terminated name and entire properties allocation live through the call.
    // The real-time-only flags and zero file-name offset supply no writable log path.
    win_status(unsafe { StartTraceW(&mut handle, name.as_ptr(), (&raw mut props).cast()) })?;
    Ok(SystemSession {
        handle,
        guid,
        name,
        active: true,
    })
}

fn flush_realtime(handle: CONTROLTRACE_HANDLE, guid: GUID) -> io::Result<()> {
    let mut props = live_properties(guid);
    // SAFETY: This flush identifies only our live session and supplies no destination file.
    win_status(unsafe {
        ControlTraceW(
            handle,
            null(),
            (&raw mut props).cast(),
            EVENT_TRACE_CONTROL_FLUSH,
        )
    })
}

struct Phase {
    child_pid: u32,
    filename: Vec<u16>,
    spans: [Span; 2],
    hits: [Hits; 2],
    expected: [u32; 2],
    survey: Option<Survey>,
}

impl Phase {
    fn ready(&self) -> bool {
        self.hits
            .iter()
            .zip(self.expected)
            .all(|(hits, expected)| hits.in_window >= expected)
    }

    fn report(&self) -> Value {
        json!({
            "process_start": self.hits[0].report(self.spans[0], self.expected[0], false),
            "file_create_or_open": self.hits[1].report(self.spans[1], self.expected[1], false)
        })
    }
}

fn locked<T>(state: &Mutex<T>) -> io::Result<MutexGuard<'_, T>> {
    state
        .lock()
        .map_err(|_| io::Error::other("live fixture state lock poisoned"))
}

struct Consumer {
    pid: u32,
    tid: u32,
    state: Arc<Mutex<Option<Phase>>>,
    notices: mpsc::SyncSender<()>,
}

impl Consumer {
    fn match_index(&self, event: &EVENT_RECORD, phase: &Phase) -> Option<usize> {
        let header = &event.EventHeader;
        if equal_guid(&header.ProviderId, &PROCESS_PROVIDER)
            && header.EventDescriptor.Id == PROCESS_START
            && phase.spans[0].contains(header.TimeStamp)
            && property_u32(event, w!("ParentProcessID")) == Some(self.pid)
            && property_u32(event, w!("ProcessID")) == Some(phase.child_pid)
        {
            return Some(0);
        }
        if equal_guid(&header.ProviderId, &FILE_PROVIDER)
            && header.EventDescriptor.Opcode == FILE_CREATE_OPCODE
            && phase.spans[1].contains(header.TimeStamp)
            && (header.ProcessId == self.pid || classic_thread_id(event) == Some(self.tid))
        {
            let mut name = [0; 4096];
            if let Ok(size) = property(event, w!("OpenPath"), &mut name)
                && matches_filename(&name[..size], &phase.filename)
            {
                return Some(1);
            }
        }
        None
    }

    fn observe(&self, event: &EVENT_RECORD) {
        let header = &event.EventHeader;
        let descriptor = header.EventDescriptor;
        if !(equal_guid(&header.ProviderId, &PROCESS_PROVIDER) && descriptor.Id == PROCESS_START
            || equal_guid(&header.ProviderId, &FILE_PROVIDER)
                && descriptor.Opcode == FILE_CREATE_OPCODE)
        {
            return;
        }
        let Ok(mut state) = locked(&self.state) else {
            return;
        };
        let Some(phase) = state.as_mut() else { return };
        let Some(index) = self.match_index(event, phase) else {
            return;
        };
        phase.hits[index].observe(descriptor, header.TimeStamp, phase.spans[index]);
        if let Some(survey) = &mut phase.survey {
            let kind = Kind {
                source: if index == 0 {
                    "Microsoft-Windows-Kernel-Process"
                } else {
                    "SystemTraceProvider/FileIo"
                },
                event_id: descriptor.Id,
                opcode: descriptor.Opcode,
                version: descriptor.Version,
            };
            if survey.observe(kind, header.TimeStamp, event.UserDataLength) == Some(true)
                && let Ok(fields) = crate::etw::schema::fields(event)
            {
                survey.set_fields(kind, fields);
            }
        }
        if phase.hits[index].in_window == phase.expected[index] {
            let _ = self.notices.try_send(());
        }
    }
}

unsafe extern "system" fn realtime_event(raw: *mut EVENT_RECORD) {
    // SAFETY: The ETW record is borrowed during this callback. Context points to our
    // boxed Consumer, kept live until ProcessTrace exits.
    let Some(event) = (unsafe { raw.as_ref() }) else {
        return;
    };
    let Some(consumer) = (unsafe { event.UserContext.cast::<Consumer>().as_ref() }) else {
        return;
    };
    consumer.observe(event);
}

fn emit_phase(
    cmd: &Path,
    path: &Path,
    state: &Mutex<Option<Phase>>,
    measured_frequency: Option<i64>,
    deadline: Instant,
) -> io::Result<()> {
    let filename = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing fixture basename"))?
        .encode_wide()
        .collect();
    let started = Instant::now();
    let mut child = {
        // This is a test-only registration lock, released before waiting for any delivery.
        // The callback cannot attribute a child to a phase until its exact PID and bracket exist.
        let mut slot = locked(state)?;
        let (child, process_span) = bracket(|| start_child(cmd))?;
        let (_, mut file_span) = bracket(|| new_file(path))?;
        let survey = measured_frequency
            .map(|freq| {
                let end = freq
                    .checked_mul(i64::from(MEASURE_SECONDS))
                    .and_then(|duration| process_span.before.checked_add(duration))
                    .ok_or_else(|| io::Error::other("QPC window overflow"))?;
                file_span.after = end - 1;
                Survey::new(process_span.before, end, freq)
            })
            .transpose()?;
        let file_operations = 1 + measured_frequency.map_or(0, |_| BURSTS * OPENS_PER_BURST);
        *slot = Some(Phase {
            child_pid: child.0.id(),
            filename,
            spans: [process_span, file_span],
            hits: std::array::from_fn(|_| Hits::default()),
            expected: [1, file_operations],
            survey,
        });
        child
    };
    wait_child(&mut child, deadline)?;
    if measured_frequency.is_some() {
        run_file_bursts(path, started, deadline)?;
    }
    Ok(())
}

fn run_file_bursts(path: &Path, start: Instant, phase_deadline: Instant) -> io::Result<()> {
    let deadline = (start + Duration::from_secs(u64::from(MEASURE_SECONDS))).min(phase_deadline);
    for burst in 0..BURSTS {
        let due = start + BURST_SPACING * burst;
        thread::sleep(due.min(deadline).saturating_duration_since(Instant::now()));
        for _ in 0..OPENS_PER_BURST {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "fixture file opens exceeded the observation window",
                ));
            }
            drop(File::open(path)?);
        }
    }
    thread::sleep(deadline.saturating_duration_since(Instant::now()));
    Ok(())
}

fn wait_delivery(
    state: &Mutex<Option<Phase>>,
    notices: &mpsc::Receiver<()>,
    deadline: Instant,
) -> io::Result<()> {
    loop {
        if locked(state)?.as_ref().is_some_and(Phase::ready) {
            return Ok(());
        }
        notices
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "live provider controls not delivered",
                )
            })?;
    }
}

#[derive(Clone, Copy)]
struct Stop {
    timed_out: bool,
    events_lost: u32,
    realtime_buffers_lost: u32,
    log_buffers_lost: u32,
    buffer_kib: u32,
    buffers: u32,
    qpc_after: i64,
}

fn exercise(
    cmd: &Path,
    scratch: &Scratch,
    state: &Mutex<Option<Phase>>,
    notices: &mpsc::Receiver<()>,
    identity: (CONTROLTRACE_HANDLE, GUID),
    qpc_frequency: i64,
    deadline: Instant,
) -> Result<Value, ProbeError> {
    let (handle, guid) = identity;
    let phase_deadline = || (Instant::now() + PHASE_DEADLINE).min(deadline);
    stage(
        "enable_kernel_process",
        enable(handle, &PROCESS_PROVIDER, &[PROCESS_START], PROCESS_KEYWORD),
    )?;
    stage(
        "warm_fixture",
        emit_phase(cmd, &scratch.paths[1], state, None, phase_deadline()),
    )?;
    stage("flush_warm_fixture", flush_realtime(handle, guid))?;
    let warm = stage(
        "wait_live_ready",
        wait_delivery(state, notices, phase_deadline()),
    );
    if warm.is_err()
        && let Some(phase) = stage("inspect_ready", locked(state))?.as_ref()
    {
        println!(
            "{}",
            json!({
                "status": "incomplete",
                "stage": "wait_live_ready",
                "controls": phase.report()
            })
        );
    }
    warm?;
    let ready_qpc = stage("sample_ready_qpc", qpc())?;
    let warm_controls = stage("read_warm_controls", locked(state))?
        .as_ref()
        .expect("warm phase registered")
        .report();

    stage(
        "measure_fixture",
        emit_phase(
            cmd,
            &scratch.paths[2],
            state,
            Some(qpc_frequency),
            phase_deadline(),
        ),
    )?;
    stage("flush_measured_fixture", flush_realtime(handle, guid))?;
    let measured = stage(
        "wait_measured_controls",
        wait_delivery(state, notices, phase_deadline()),
    );
    if measured.is_err()
        && let Some(phase) = stage("inspect_measurement", locked(state))?.as_ref()
    {
        println!(
            "{}",
            json!({
                "status": "incomplete",
                "stage": "wait_measured_controls",
                "controls": phase.report()
            })
        );
    }
    measured?;
    Ok(json!({"qpc_after_ready": ready_qpc.to_string(), "matched_controls": warm_controls}))
}

fn run() -> Result<Value, ProbeError> {
    let cmd = stage("system_cmd", system_cmd())?;
    let guid = stage("create_session_identity", new_guid())?;
    let id = guid_string(&guid);
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
    stage("prepare_scratch_root", fs::create_dir_all(&root))?;
    let dir = root.join(format!("sys-etw-live-{id}"));
    stage("create_scratch", fs::create_dir(&dir))?;
    let scratch = Scratch {
        paths: [
            dir.join("unused.etl"),
            dir.join(format!("ready-{id}.dat")),
            dir.join(format!("measure-{id}.dat")),
        ],
        dir,
    };

    let qpc_frequency = stage("sample_qpc_frequency", frequency())?;
    let qpc_before = stage("sample_start_qpc", qpc())?;
    let mut session = stage("start_realtime_session", start_realtime(guid))?;
    let handle = session.handle;
    let deadline = Instant::now() + LIVE_DEADLINE;
    let mut name = session.name.clone();
    let state = Arc::new(Mutex::new(None));
    let (notice_send, notices) = mpsc::sync_channel(1);
    let mut consumer = Box::new(Consumer {
        pid: std::process::id(),
        // SAFETY: The current thread ID API has no pointer inputs.
        tid: unsafe { GetCurrentThreadId() },
        state: state.clone(),
        notices: notice_send,
    });
    let mut log = EVENT_TRACE_LOGFILEW {
        LoggerName: name.as_mut_ptr(),
        Anonymous1: EVENT_TRACE_LOGFILEW_0 {
            ProcessTraceMode: PROCESS_TRACE_MODE_EVENT_RECORD
                | PROCESS_TRACE_MODE_RAW_TIMESTAMP
                | PROCESS_TRACE_MODE_REAL_TIME,
        },
        Anonymous2: EVENT_TRACE_LOGFILEW_1 {
            EventRecordCallback: Some(realtime_event),
        },
        Context: (&raw mut *consumer).cast(),
        ..Default::default()
    };

    let (readiness, stop, trace_frequency) = thread::scope(|scope| {
        let (finish, requested) = mpsc::channel();
        let watch = scope.spawn(move || {
            let missing = requested
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .is_err();
            let timed_out = missing || Instant::now() >= deadline;
            let stats = stage("stop_realtime_session", session.stop())?;
            Ok(Stop {
                timed_out,
                events_lost: stats.EventsLost,
                realtime_buffers_lost: stats.RealTimeBuffersLost,
                log_buffers_lost: stats.LogBuffersLost,
                buffer_kib: stats.BufferSize,
                buffers: stats.NumberOfBuffers,
                qpc_after: stage("sample_stop_qpc", qpc())?,
            })
        });
        // SAFETY: The terminated logger name, log structure and callback state stay live
        // for the whole scoped acquisition.
        let read_handle = unsafe { OpenTraceW(&mut log) };
        if read_handle.Value == usize::MAX as u64 {
            let failed = io::Error::last_os_error();
            let _ = finish.send(());
            let _ = watch.join();
            return stage("open_realtime_consumer", Err(failed));
        }
        let header_frequency = log.LogfileHeader.PerfFreq;
        let trace_frequency = (header_frequency > 0).then_some(header_frequency);
        let mut reader = Some(TraceReader(read_handle));
        let process = scope.spawn(move || {
            // SAFETY: This is our single real-time consumer. Only this scoped thread processes
            // the handle, and its callback uses the still-live boxed state.
            win_status(unsafe {
                ProcessTrace(&read_handle, 1, null::<FILETIME>(), null::<FILETIME>())
            })
        });
        let measured = exercise(
            &cmd,
            &scratch,
            &state,
            &notices,
            (handle, guid),
            qpc_frequency,
            deadline,
        );
        let _ = finish.send(());
        let stopped: Result<Stop, ProbeError> =
            watch.join().expect("real-time stop watchdog panicked");
        if stopped.is_err() {
            // In real-time mode CloseTrace may be issued while ProcessTrace drains, releasing
            // this handle even when ControlTrace failed. Taking the RAII guard prevents reuse.
            drop(reader.take());
        }
        let drained = process.join().expect("real-time consumer panicked");
        stage("drain_realtime_consumer", drained)?;
        Ok((measured?, stopped?, trace_frequency))
    })?;
    let state = stage("read_final_survey", locked(&state))?;
    let phase = state.as_ref().expect("measured phase registered");
    let survey = phase
        .survey
        .as_ref()
        .expect("measurement survey registered")
        .report();
    let trace_artifact_created = stage("check_no_trace_artifact", scratch.paths[0].try_exists())?;
    let passed = !stop.timed_out
        && stop.events_lost == 0
        && stop.realtime_buffers_lost == 0
        && stop.log_buffers_lost == 0
        && trace_frequency.is_none_or(|observed| observed == qpc_frequency)
        && !trace_artifact_created
        && phase.ready();
    Ok(json!({
        "status": if passed { "live_owned_controls_observed" } else { "incomplete" },
        "token_elevated": true,
        "capture_mode": "named_system_logger_realtime",
        "scope": "owned_process_identity_and_fixture_file_postfilter",
        "trace_file_configured": false,
        "trace_artifact_created": trace_artifact_created,
        "database_configured": false,
        "qpc_frequency_hz": qpc_frequency.to_string(),
        "trace_header_frequency_hz": trace_frequency.map(|value| value.to_string()),
        "session_bracket": Span { before: qpc_before, after: stop.qpc_after }.report(),
        "readiness": readiness,
        "measurements": phase.report(),
        "fixture_workload": {"window_seconds": MEASURE_SECONDS, "file_bursts": BURSTS, "opens_per_burst": OPENS_PER_BURST},
        "outside_window_scope": "aggregator_input_after_qpc_and_identity_filter",
        "survey": survey,
        "stop": if stop.timed_out { "watchdog_stop_succeeded" } else { "requested_stop_succeeded" },
        "drain": "process_trace_returned_success",
        "limits": {
            "capture_deadline_ms": LIVE_DEADLINE.as_millis(),
            "effective_buffer_kib": stop.buffer_kib, "effective_buffers": stop.buffers,
        },
        "loss": {
            "session_events": stop.events_lost,
            "session_realtime_buffers": stop.realtime_buffers_lost,
            "session_log_buffers": stop.log_buffers_lost,
        },
    }))
}

#[test]
#[ignore = "requires isolated elevated Windows CI and explicit opt-in"]
fn owned_realtime_stats_control() {
    assert_eq!(
        std::env::var("TRACESCALPEL_SYSTEM_ETW_CONTROL")
            .ok()
            .as_deref(),
        Some("1"),
        "explicit opt-in required for a bounded live ETW control"
    );
    if !crate::startup::token_elevated().expect("read token elevation") {
        println!(
            "{}",
            json!({"status": "unavailable", "token_elevated": false, "stage": "elevation_gate"})
        );
        panic!("live ETW control requires an already elevated token");
    }
    match run() {
        Ok(report) => {
            println!("{report}");
            assert_eq!(report["status"], "live_owned_controls_observed");
            let ticks = |value: &Value| {
                value
                    .as_str()
                    .expect("QPC string")
                    .parse::<i64>()
                    .expect("valid QPC")
            };
            assert!(
                ticks(&report["readiness"]["qpc_after_ready"])
                    <= ticks(&report["survey"]["qpc_start"])
            );
            let types = report["survey"]["event_types"]
                .as_array()
                .expect("event types");
            assert_eq!(types.len(), 2);
            assert_eq!(
                report["survey"]["total"]["events"],
                2 + BURSTS * OPENS_PER_BURST
            );
            assert!(
                report["survey"]["nonempty_buckets"]
                    .as_u64()
                    .is_some_and(|count| count >= u64::from(BURSTS))
            );
            for event in types {
                assert!(
                    event["fields"]
                        .as_array()
                        .is_some_and(|fields| !fields.is_empty()),
                    "{event}"
                );
                assert!(
                    event["user_data_bytes"]
                        .as_u64()
                        .is_some_and(|bytes| bytes > 0)
                );
            }
        }
        Err(failure) => {
            println!(
                "{}",
                json!({
                    "status": "unavailable", "token_elevated": true, "stage": failure.stage,
                    "win32_error": failure.error.raw_os_error(),
                    "error_kind": format!("{:?}", failure.error.kind())
                })
            );
            panic!("owned live ETW control failed in {}", failure.stage);
        }
    }
}
