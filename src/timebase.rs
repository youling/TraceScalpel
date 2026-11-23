//! Capture-local monotonic time and a bounded sample of the UTC wall clock.

use serde::{Deserialize, Serialize};
use std::io;
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;

#[derive(Clone, Deserialize, Serialize)]
pub struct Clock {
    source: String,
    pub frequency_hz: String,
    pub utc_calibration: UtcCalibration,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct UtcCalibration {
    source: String,
    pub qpc_before: String,
    qpc_after: String,
    filetime_100ns_since_1601_utc: String,
}

pub fn qpc() -> io::Result<i64> {
    let mut ticks = 0;
    // SAFETY: The destination is valid for one i64.
    if unsafe { QueryPerformanceCounter(&mut ticks) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ticks)
}

pub fn frequency() -> io::Result<i64> {
    let mut frequency = 0;
    // SAFETY: The destination is valid for one i64.
    if unsafe { QueryPerformanceFrequency(&mut frequency) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if frequency <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid QPC frequency",
        ));
    }
    Ok(frequency)
}

pub fn sample() -> io::Result<Clock> {
    let frequency = frequency()?;
    let before = qpc()?;
    let mut utc = FILETIME::default();
    // SAFETY: The destination is valid for one FILETIME.
    unsafe { GetSystemTimePreciseAsFileTime(&mut utc) };
    let after = qpc()?;
    let filetime = (u64::from(utc.dwHighDateTime) << 32) | u64::from(utc.dwLowDateTime);
    Ok(Clock {
        source: "QueryPerformanceCounter".to_owned(),
        frequency_hz: frequency.to_string(),
        utc_calibration: UtcCalibration {
            source: "GetSystemTimePreciseAsFileTime".to_owned(),
            qpc_before: before.to_string(),
            qpc_after: after.to_string(),
            filetime_100ns_since_1601_utc: filetime.to_string(),
        },
    })
}
