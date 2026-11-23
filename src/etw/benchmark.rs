use super::*;
use std::hint::black_box;
use std::time::Instant;
use windows_sys::Win32::System::Diagnostics::Etw::EventEnabled;
use windows_sys::Win32::System::Performance::QueryPerformanceCounter;
use windows_sys::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;

struct Stats {
    min: f64,
    median: f64,
    p95: f64,
    max: f64,
    total: f64,
}

impl Stats {
    fn from_values(values: &[f64]) -> Self {
        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        Self {
            min: sorted[0],
            median: sorted[sorted.len() / 2],
            p95: sorted[(sorted.len() * 95).div_ceil(100) - 1],
            max: sorted[sorted.len() - 1],
            total: sorted.iter().sum(),
        }
    }

    fn report(&self, label: &str) {
        println!(
            "{label}: min={:.1}, median={:.1}, p95={:.1}, max={:.1}, sum={:.3}",
            self.min, self.median, self.p95, self.max, self.total
        );
    }
}

#[test]
#[ignore = "opt-in single-host native ETW overhead experiment"]
fn measure_owned_etw_and_clock_calls() {
    let wall = Instant::now();
    let frequency = frequency().unwrap();
    let ns_per_tick = 1_000_000_000.0 / frequency as f64;

    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!(
            "overhead-{}",
            String::from_utf16_lossy(&session_name(&new_guid().unwrap())).trim_end_matches('\0')
        ));
    fs::create_dir(&dir).unwrap();
    let artifact = dir.join("source.etl");
    let mut path = path_wide(&artifact).unwrap();
    const PAIRS: usize = 31;
    let mut samples = [Vec::new(), Vec::new()];
    let mut deltas = Vec::new();
    let mut start_ms = Vec::new();
    let mut stop_ms = Vec::new();
    let mut replay_ms = Vec::new();
    let mut bytes = Vec::new();
    let mut counter_sums = [0u64; 4];
    for pair in 0..=PAIRS {
        let guid = new_guid().unwrap();
        let provider = Provider::register(&guid).unwrap();
        let mut measurements = [0.0; 2];
        let cases = if pair % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        };
        for enabled in cases {
            let setup_start = Instant::now();
            let mut session = enabled.then(|| {
                let session = Session::start(guid, &path).unwrap();
                session
                    .set_enabled(EVENT_CONTROL_CODE_ENABLE_PROVIDER)
                    .unwrap();
                session
            });
            let setup_elapsed = setup_start.elapsed();
            // SAFETY: The registration, descriptor and event payload belong to this fixture.
            assert_eq!(unsafe { EventEnabled(provider.0, &EVENT) }, enabled);

            let intervals = emit_bracketed(&provider).unwrap();
            let ticks = intervals[EVENT_COUNT - 1].1 - intervals[0].0;
            let per_marker = ticks as f64 * ns_per_tick / EVENT_COUNT as f64;
            measurements[usize::from(enabled)] = per_marker;
            if pair > 0 {
                samples[usize::from(enabled)].push(per_marker);
            }

            if let Some(session) = &mut session {
                let stopping = Instant::now();
                session
                    .set_enabled(EVENT_CONTROL_CODE_DISABLE_PROVIDER)
                    .unwrap();
                let stats = session.stop().unwrap();
                let stop_elapsed = stopping.elapsed();
                let bytes_on_disk = fs::metadata(&artifact).unwrap().len();
                let mut state = ReaderState {
                    guid,
                    process_id: std::process::id(),
                    timestamps: [None; EVENT_COUNT],
                    invalid_record: false,
                };
                let replaying = Instant::now();
                let (etl_frequency, etl_events_lost, etl_buffers_lost) =
                    replay(&mut path, &mut state).unwrap();
                let replay_elapsed = replaying.elapsed();
                fs::remove_file(&artifact).unwrap();
                assert_eq!(etl_frequency, frequency);
                assert!(!state.invalid_record);
                for (ts, (before, after)) in state.timestamps.into_iter().zip(intervals) {
                    assert!((before..=after).contains(&ts.unwrap()));
                }
                if pair > 0 {
                    start_ms.push(setup_elapsed.as_secs_f64() * 1_000.0);
                    stop_ms.push(stop_elapsed.as_secs_f64() * 1_000.0);
                    replay_ms.push(replay_elapsed.as_secs_f64() * 1_000.0);
                    bytes.push(bytes_on_disk as f64);
                    for (sum, item) in counter_sums.iter_mut().zip([
                        stats.EventsLost,
                        stats.LogBuffersLost,
                        etl_events_lost,
                        etl_buffers_lost,
                    ]) {
                        *sum += u64::from(item);
                    }
                }
            }
        }
        if pair > 0 {
            deltas.push(measurements[1] - measurements[0]);
        }
    }
    fs::remove_dir(&dir).unwrap();
    println!(
        "pairs={PAIRS}, events_per_case={}, qpc_hz={frequency}",
        PAIRS * EVENT_COUNT
    );
    Stats::from_values(&samples[0]).report("disabled ns per numbered EventWrite incl qpc");
    Stats::from_values(&samples[1]).report("enabled ns per numbered EventWrite incl qpc");
    Stats::from_values(&deltas).report("paired enabled-minus-disabled ns per numbered EventWrite");
    Stats::from_values(&start_ms).report("ETL setup + enable ms");
    Stats::from_values(&stop_ms).report("ETL disable + stop/seal ms");
    Stats::from_values(&replay_ms).report("ETL OpenTrace + ProcessTrace + CloseTrace ms");
    Stats::from_values(&bytes).report("ETL bytes per enabled case");
    println!(
        "loss sums [session events, session log buffers, ETL events, ETL buffers]={counter_sums:?}"
    );

    const CALLS: u32 = 200_000;
    const CLOCK_TRIALS: usize = 15;
    let mut clocks = [Vec::new(), Vec::new(), Vec::new()];
    for trial in 0..=CLOCK_TRIALS {
        let cases = [trial % 3, (trial + 1) % 3, (trial + 2) % 3];
        for case in cases {
            let start = qpc().unwrap();
            match case {
                0 => {
                    for _ in 0..CALLS {
                        black_box(0_i64);
                    }
                }
                1 => {
                    let mut ticks = 0;
                    for _ in 0..CALLS {
                        // SAFETY: ticks remains valid and the API writes an i64.
                        let result = unsafe { QueryPerformanceCounter(&mut ticks) };
                        black_box((result, ticks));
                    }
                }
                2 => {
                    let mut filetime = FILETIME::default();
                    for _ in 0..CALLS {
                        // SAFETY: filetime remains valid and the API writes one FILETIME.
                        unsafe { GetSystemTimePreciseAsFileTime(&mut filetime) };
                        black_box(filetime);
                    }
                }
                _ => unreachable!(),
            }
            let elapsed = qpc().unwrap() - start;
            if trial > 0 {
                clocks[case].push(elapsed as f64 * ns_per_tick / f64::from(CALLS));
            }
        }
    }
    println!("clock_trials={CLOCK_TRIALS}, calls_per_trial={CALLS}");
    Stats::from_values(&clocks[0]).report("loop black_box baseline ns/op");
    Stats::from_values(&clocks[1]).report("QueryPerformanceCounter ns/call incl loop");
    Stats::from_values(&clocks[2]).report("GetSystemTimePreciseAsFileTime ns/call incl loop");
    println!(
        "total bench wall seconds={:.3}",
        wall.elapsed().as_secs_f64()
    );
}
