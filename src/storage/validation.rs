use super::{Backend, Row, Store, Window};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BACKENDS: [Backend; 3] = [Backend::Sqlite, Backend::Segments, Backend::Redb];
const BATCH_ROWS: usize = 128;
const QPC_ORIGIN: i64 = 9_007_199_254_740_993;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"));
        let root = target.join("storage-validation");
        fs::create_dir_all(&root).unwrap();
        let tick = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!("{}-{tick}-{serial}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn bytes(&self) -> u64 {
        fs::read_dir(&self.0)
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap())
            .filter(|metadata| metadata.is_file())
            .map(|metadata| metadata.len())
            .sum()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Ok(entries) = fs::read_dir(&self.0) {
            for entry in entries.flatten() {
                let _ = fs::remove_file(entry.path());
            }
        }
        let _ = fs::remove_dir(&self.0);
    }
}

fn make_rows(
    count: usize,
    payload_bytes: usize,
    clock_offset: impl Fn(usize) -> usize,
) -> Vec<Row> {
    (0..count)
        .map(|index| Row {
            sequence: index as i64 + 1,
            qpc: QPC_ORIGIN + clock_offset(index) as i64,
            process_id: 100 + (index % 4) as u32,
            kind: ["open", "read", "write", "close"][index % 4].into(),
            data: (0..payload_bytes)
                .map(|offset| (index.wrapping_mul(29) ^ offset) as u8)
                .collect(),
        })
        .collect()
}

fn expected_rows(rows: &[Row], window: &Window) -> Vec<Row> {
    let mut matches: Vec<_> = rows
        .iter()
        .filter(|row| {
            (window.qpc_start..window.qpc_end).contains(&row.qpc)
                && window.process_id.is_none_or(|pid| row.process_id == pid)
                && window
                    .after
                    .is_none_or(|cursor| (row.qpc, row.sequence) > cursor)
        })
        .collect();
    matches.sort_unstable_by_key(|row| (row.qpc, row.sequence));
    matches.into_iter().take(window.limit).cloned().collect()
}

fn assert_pagination(store: &dyn Store, input: &[Row], mut window: Window) {
    let complete = Window {
        limit: input.len(),
        ..window
    };
    let expected = expected_rows(input, &complete);
    for page in expected.chunks(window.limit) {
        assert_eq!(store.query(&window).unwrap(), page);
        let last = page.last().unwrap();
        window.after = Some((last.qpc, last.sequence));
    }
    assert!(store.query(&window).unwrap().is_empty());
}

fn verify_queries(store: &dyn Store, input: &[Row]) {
    assert_pagination(
        store,
        input,
        Window {
            qpc_start: QPC_ORIGIN,
            qpc_end: QPC_ORIGIN + 68,
            process_id: None,
            after: None,
            limit: 7,
        },
    );
    assert_pagination(
        store,
        input,
        Window {
            qpc_start: QPC_ORIGIN + 1,
            qpc_end: QPC_ORIGIN + 19,
            process_id: Some(102),
            after: None,
            limit: 3,
        },
    );
    assert_pagination(
        store,
        input,
        Window {
            qpc_start: QPC_ORIGIN,
            qpc_end: QPC_ORIGIN + 30,
            process_id: Some(999),
            after: None,
            limit: 3,
        },
    );
}

#[test]
fn same_input_conforms_across_backends() {
    let count = BATCH_ROWS * 2 + 11;
    let rows = make_rows(count, 32, |index| index * 131 % (count / 4 + 1));
    for backend in BACKENDS {
        let scratch = Scratch::new();
        let path = scratch.0.join(backend.filename());
        let mut store = backend.create(&path).unwrap();
        for batch in rows.chunks(BATCH_ROWS) {
            store.append(batch).unwrap();
        }
        verify_queries(store.as_ref(), &rows);
        store.seal().unwrap();
        let reopened = backend.open(&path).unwrap();
        verify_queries(reopened.as_ref(), &rows);
    }
}

fn p95_ms(mut samples: Vec<Duration>) -> f64 {
    samples.sort_unstable();
    let index = (samples.len() * 95).div_ceil(100) - 1;
    samples[index].as_secs_f64() * 1_000.0
}

/// This measures local store operations with an uncontrolled host cache. OS
/// tracing and decode costs are excluded. Logical lengths include sidecars.
#[test]
#[ignore = "opt-in release benchmark"]
fn compare_storage_backends() {
    const COUNT: usize = 32_768;
    const DATA_BYTES: usize = 96;
    let rows = make_rows(COUNT, DATA_BYTES, |index| {
        // Nearby events arrive out of timestamp order, while each batch has
        // a local time range so a sparse per-batch index can prune useful work.
        let batch = index / BATCH_ROWS;
        let within_batch = index % BATCH_ROWS;
        batch * (BATCH_ROWS / 4) + (within_batch * 83 % BATCH_ROWS) / 4
    });
    let queries: Vec<_> = (0..48)
        .map(|index| {
            let qpc_start = QPC_ORIGIN + (index * 193 % 7_900);
            let window = Window {
                qpc_start,
                qpc_end: qpc_start + 128,
                process_id: Some(100 + (index % 4) as u32),
                after: None,
                limit: 64,
            };
            let expected = expected_rows(&rows, &window);
            (window, expected)
        })
        .collect();
    for backend in BACKENDS {
        let scratch = Scratch::new();
        let path = scratch.0.join(backend.filename());
        let mut store = backend.create(&path).unwrap();
        let mut commits = Vec::with_capacity(COUNT.div_ceil(BATCH_ROWS));
        let write_start = Instant::now();
        for batch in rows.chunks(BATCH_ROWS) {
            let batch_start = Instant::now();
            store.append(batch).unwrap();
            commits.push(batch_start.elapsed());
        }
        let write_elapsed = write_start.elapsed();
        let live_file_bytes = scratch.bytes();
        let close_start = Instant::now();
        store.seal().unwrap();
        let close_elapsed = close_start.elapsed();
        let sealed_file_bytes = scratch.bytes();
        let open_start = Instant::now();
        let store = backend.open(&path).unwrap();
        let open_elapsed = open_start.elapsed();
        assert_eq!(store.query(&queries[0].0).unwrap(), queries[0].1);
        let mut reads = Vec::with_capacity(queries.len());
        for (window, expected) in &queries {
            let query_start = Instant::now();
            let seen = store.query(window).unwrap();
            reads.push(query_start.elapsed());
            assert_eq!(&seen, expected);
        }
        println!(
            "{}",
            serde_json::json!({
                "backend": backend,
                "rows": COUNT,
                "data_bytes_per_row": DATA_BYTES,
                "batch_rows": BATCH_ROWS,
                "clock_pattern": "32_ticks_per_batch_shuffled_within_batch",
                "commits": commits.len(),
                "write_elapsed_ms": write_elapsed.as_secs_f64() * 1_000.0,
                "write_rows_per_second": COUNT as f64 / write_elapsed.as_secs_f64(),
                "commit_p95_ms": p95_ms(commits),
                "live_file_bytes": live_file_bytes,
                "sealed_file_bytes": sealed_file_bytes,
                "close_elapsed_ms": close_elapsed.as_secs_f64() * 1_000.0,
                "open_elapsed_ms": open_elapsed.as_secs_f64() * 1_000.0,
                "queries": reads.len(),
                "query_p95_ms": p95_ms(reads),
            })
        );
    }
}
