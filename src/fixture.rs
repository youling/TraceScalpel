//! Owned fixture that emits source records on a common Windows clock.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::windows::io::FromRawHandle;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;

const TIMEOUT: Duration = Duration::from_secs(2);
const MAX_EXE_BYTES: u64 = 128 * 1024 * 1024;
const PAYLOAD: &[u8] = b"camera-fixture-v1";

#[derive(Serialize)]
pub struct Capture {
    schema_version: &'static str,
    capture_id: String,
    clock: Clock,
    events: [Event; 5],
}

#[derive(Serialize)]
struct Clock {
    source: &'static str,
    frequency_hz: String,
    utc_calibration: UtcCalibration,
}

#[derive(Serialize)]
struct UtcCalibration {
    source: &'static str,
    qpc_before: String,
    qpc_after: String,
    filetime_100ns_since_1601_utc: String,
}

#[derive(Serialize)]
struct Event {
    kind: &'static str,
    qpc_start: String,
    qpc_end: String,
    process_id: u32,
    source: &'static str,
    details: Value,
}

pub struct FixtureError {
    pub stage: &'static str,
    pub cause: io::Error,
}

fn qpc() -> io::Result<i64> {
    let mut ticks = 0;
    if unsafe { QueryPerformanceCounter(&mut ticks) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ticks)
}

fn measure(
    kind: &'static str,
    probe: impl FnOnce() -> io::Result<Value>,
) -> Result<Event, FixtureError> {
    let fail = |cause| FixtureError { stage: kind, cause };
    let start = qpc().map_err(fail)?;
    let details = probe().map_err(fail)?;
    let end = qpc().map_err(fail)?;
    Ok(Event {
        kind,
        qpc_start: start.to_string(),
        qpc_end: end.to_string(),
        process_id: std::process::id(),
        source: "fixture_self_report",
        details,
    })
}

fn executable_hash() -> io::Result<Value> {
    let mut file = File::open(std::env::current_exe()?)?;
    let size = file.metadata()?.len();
    if size > MAX_EXE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "fixture executable exceeds hash limit",
        ));
    }
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 16384];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > MAX_EXE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fixture executable grew beyond hash limit",
            ));
        }
        hasher.update(&buf[..n]);
    }
    Ok(
        json!({"artifact": "current_exe_file", "sha256": format!("{:x}", hasher.finalize()), "bytes": total}),
    )
}

fn pipe_exchange() -> io::Result<Value> {
    let mut read_handle = std::ptr::null_mut();
    let mut write_handle = std::ptr::null_mut();
    if unsafe { CreatePipe(&mut read_handle, &mut write_handle, std::ptr::null(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // File owns and closes each handle, including on an early error.
    let mut reader = unsafe { File::from_raw_handle(read_handle) };
    let mut writer = unsafe { File::from_raw_handle(write_handle) };
    writer.write_all(PAYLOAD)?;
    drop(writer);
    let mut received = [0u8; PAYLOAD.len()];
    reader.read_exact(&mut received)?;
    if received != PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pipe payload mismatch",
        ));
    }
    Ok(json!({"transport": "windows anonymous pipe", "bytes": received.len(), "matched": true}))
}

fn tcp_exchange() -> io::Result<Value> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    listener.set_nonblocking(true)?;
    let addr: SocketAddr = listener.local_addr()?;
    let mut client = TcpStream::connect_timeout(&addr, TIMEOUT)?;
    client.set_read_timeout(Some(TIMEOUT))?;
    client.set_write_timeout(Some(TIMEOUT))?;
    let client_addr = client.local_addr()?;
    let deadline = Instant::now() + TIMEOUT;
    let mut server = loop {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "fixture loopback accept timed out",
            ));
        }
        match listener.accept() {
            Ok((stream, peer)) if peer == client_addr => break stream,
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) => return Err(e),
        }
    };
    server.set_read_timeout(Some(TIMEOUT))?;
    server.set_write_timeout(Some(TIMEOUT))?;
    client.write_all(PAYLOAD)?;
    let mut received = [0u8; PAYLOAD.len()];
    server.read_exact(&mut received)?;
    if received != PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "TCP payload mismatch",
        ));
    }
    server.write_all(&received)?;
    let mut reply = [0u8; PAYLOAD.len()];
    client.read_exact(&mut reply)?;
    if reply != PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "TCP reply mismatch",
        ));
    }
    Ok(
        json!({"transport": "IPv4 loopback TCP", "port": addr.port(), "bytes_each_direction": reply.len(), "matched": true}),
    )
}

struct RemoveOnDrop(Option<PathBuf>);

impl RemoveOnDrop {
    fn finish(mut self) -> io::Result<()> {
        if let Some(path) = &self.0 {
            fs::remove_file(path)?;
        }
        self.0 = None;
        Ok(())
    }
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

fn file_exchange() -> io::Result<Value> {
    let dir = std::env::var_os("TRACESCALPEL_FIXTURE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    if !fs::metadata(&dir)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "fixture path must name a directory",
        ));
    }
    for attempt in 0..16 {
        let path = dir.join(format!(
            "camera-fixture-{}-{}-{}.tmp",
            std::process::id(),
            qpc()?,
            attempt
        ));
        let opened = match OpenOptions::new()
            .write(true)
            .read(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let cleanup = RemoveOnDrop(Some(path));
        let mut file = opened;
        file.write_all(PAYLOAD)?;
        file.flush()?;
        file.seek(SeekFrom::Start(0))?;
        let mut received = Vec::new();
        Read::by_ref(&mut file)
            .take((PAYLOAD.len() + 1) as u64)
            .read_to_end(&mut received)?;
        drop(file);
        if received != PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file payload mismatch",
            ));
        }
        cleanup.finish()?;
        return Ok(json!({"bytes": received.len(), "matched": true, "cleanup": "removed"}));
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create fixture file",
    ))
}

fn sample_clock() -> io::Result<Clock> {
    let mut frequency = 0i64;
    if unsafe { QueryPerformanceFrequency(&mut frequency) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if frequency <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid QPC frequency",
        ));
    }
    let before = qpc()?;
    let mut utc = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    unsafe { GetSystemTimePreciseAsFileTime(&mut utc) };
    let after = qpc()?;
    let filetime = ((utc.dwHighDateTime as u64) << 32) | utc.dwLowDateTime as u64;
    Ok(Clock {
        source: "QueryPerformanceCounter",
        frequency_hz: frequency.to_string(),
        utc_calibration: UtcCalibration {
            source: "GetSystemTimePreciseAsFileTime",
            qpc_before: before.to_string(),
            qpc_after: after.to_string(),
            filetime_100ns_since_1601_utc: filetime.to_string(),
        },
    })
}

/// Perform owned, local fixture operations and return their QPC intervals.
pub fn run() -> Result<Capture, FixtureError> {
    let clock = sample_clock().map_err(|cause| FixtureError {
        stage: "clock",
        cause,
    })?;
    let capture_id = format!(
        "fixture-{}-{}",
        std::process::id(),
        clock.utc_calibration.qpc_before
    );
    let events = [
        measure("executable_fingerprint", executable_hash)?,
        measure("dynamic_marker", || {
            let marker = std::hint::black_box("camera-fixture-marker-v1");
            Ok(json!({"marker": marker}))
        })?,
        measure("anonymous_pipe_exchange", pipe_exchange)?,
        measure("loopback_tcp_exchange", tcp_exchange)?,
        measure("temporary_file_exchange", file_exchange)?,
    ];
    Ok(Capture {
        schema_version: "camera-fixture.v1",
        capture_id,
        clock,
        events,
    })
}
