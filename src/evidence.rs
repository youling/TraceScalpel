use crate::etw;
use crate::fixture;
use crate::storage::{Backend, Row, Window};
use crate::timebase::{self, Clock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

const SCHEMA_VERSION: u32 = 1;
const MAX_ARTIFACT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 32 * 1024;
const MAX_PAGE_ROWS: usize = 128;
const ETW_RECEIPT_FILE: &str = "etw-receipt.json";

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Source {
    #[default]
    FixtureSelfReport,
    EtwOwned,
}

impl Source {
    fn filename(self) -> &'static str {
        match self {
            Self::FixtureSelfReport => "source.json",
            Self::EtwOwned => "source.etl",
        }
    }

    fn profile(self) -> &'static str {
        match self {
            Self::FixtureSelfReport => "fixture-self-report.v1",
            Self::EtwOwned => "owned-etw-marker.v1",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordRequest {
    backend: Backend,
    #[serde(default)]
    source: Source,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureRequest {
    capture_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryRequest {
    capture_id: String,
    qpc_start: Option<String>,
    qpc_end: Option<String>,
    process_id: Option<u32>,
    limit: Option<usize>,
    cursor: Option<Cursor>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    query_id: String,
    qpc: String,
    sequence: String,
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
struct Artifact {
    filename: String,
    sha256: String,
    bytes: u64,
}

#[derive(Deserialize, Serialize)]
struct Manifest {
    version: u32,
    capture_id: String,
    backend: Backend,
    source: Source,
    analysis_profile: String,
    qpc_clock_domain: String,
    frequency_hz: String,
    utc_anchors: Vec<Clock>,
    qpc_start: String,
    qpc_end: String,
    coverage_complete: bool,
    native_loss: Option<NativeLoss>,
    rows: usize,
    raw: Artifact,
    index: Artifact,
    etw_receipt: Option<Artifact>,
}

#[derive(Deserialize, Serialize)]
struct NativeLoss {
    session_events_lost: u32,
    log_buffers_lost: u32,
    etl_events_lost: u32,
    etl_buffers_lost: u32,
}

fn input_error(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn evidence_root() -> io::Result<PathBuf> {
    let root = std::env::var_os("TRACESCALPEL_EVIDENCE_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "set TRACESCALPEL_EVIDENCE_DIR to a private scratch directory",
            )
        })?;
    if !fs::metadata(&root)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "evidence root must name an existing directory",
        ));
    }
    Ok(root)
}

fn capture_path(capture_id: &str) -> io::Result<PathBuf> {
    if capture_id.len() > 96
        || !(capture_id.starts_with("fixture-") || capture_id.starts_with("etw-"))
        || capture_id
            .bytes()
            .any(|ch| !ch.is_ascii_alphanumeric() && ch != b'-')
    {
        return Err(input_error("invalid capture ID"));
    }
    Ok(evidence_root()?.join(capture_id))
}

fn plain_file(path: &Path, max_bytes: u64) -> io::Result<File> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.len() > max_bytes
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid or oversized evidence artifact",
        ));
    }
    File::open(path)
}

fn save_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let mut file = File::create_new(path)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()
}

fn digest_artifact(dir: &Path, filename: &str) -> io::Result<Artifact> {
    let file = plain_file(&dir.join(filename), MAX_ARTIFACT_BYTES)?;
    let mut reader = file.take(MAX_ARTIFACT_BYTES + 1);
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 16384];
    let mut bytes = 0u64;
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        hash.update(&buffer[..n]);
    }
    if bytes > MAX_ARTIFACT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "evidence artifact grew beyond its size limit",
        ));
    }
    Ok(Artifact {
        filename: filename.to_owned(),
        sha256: format!("{:x}", hash.finalize()),
        bytes,
    })
}

struct Collected {
    rows: Vec<Row>,
    frequency_hz: String,
    utc_anchors: Vec<Clock>,
    coverage_qpc: (i64, i64),
    native_loss: Option<NativeLoss>,
}

fn prepare_capture(root: &Path, source: Source) -> io::Result<(String, PathBuf, Collected)> {
    match source {
        Source::FixtureSelfReport => {
            let capture = fixture::run().map_err(|failure| failure.cause)?;
            let capture_id = capture.id().to_owned();
            let dir = root.join(&capture_id);
            fs::create_dir(&dir)?;
            save_json(&dir.join(source.filename()), &capture)?;
            let rows = capture.rows()?;
            let coverage_qpc = (rows[0].qpc, capture.coverage_end()?);
            Ok((
                capture_id,
                dir,
                Collected {
                    rows,
                    frequency_hz: capture.clock().frequency_hz.clone(),
                    utc_anchors: vec![capture.clock().clone()],
                    coverage_qpc,
                    native_loss: None,
                },
            ))
        }
        Source::EtwOwned => {
            let capture_id = fixture::new_etw_capture_id()?;
            let dir = root.join(&capture_id);
            fs::create_dir(&dir)?;
            let before = timebase::sample()?;
            let trace = etw::run_owned_in(&dir)?;
            let after = timebase::sample()?;
            let coverage_qpc = (
                trace.events.iter().map(|event| event.qpc_before).min(),
                trace.events.iter().map(|event| event.qpc_after).max(),
            );
            let (Some(start), Some(end)) = coverage_qpc else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "empty owned ETL",
                ));
            };
            let end = end
                .checked_add(1)
                .ok_or_else(|| input_error("ETW interval overflow"))?;
            let rows = trace
                .events
                .iter()
                .map(|event| {
                    Ok(Row {
                        sequence: i64::from(event.sequence),
                        qpc: event.qpc,
                        process_id: event.process_id,
                        kind: "etw_owned_marker".to_owned(),
                        data: serde_json::to_vec(&json!({
                            "emission_qpc_before": event.qpc_before.to_string(),
                            "emission_qpc_after": event.qpc_after.to_string()
                        }))?,
                    })
                })
                .collect::<io::Result<Vec<_>>>()?;
            save_json(&dir.join(ETW_RECEIPT_FILE), &trace)?;
            Ok((
                capture_id,
                dir,
                Collected {
                    rows,
                    frequency_hz: trace.qpc_frequency_hz.to_string(),
                    utc_anchors: vec![before, after],
                    coverage_qpc: (start, end),
                    native_loss: Some(NativeLoss {
                        session_events_lost: trace.events_lost,
                        log_buffers_lost: trace.log_buffers_lost,
                        etl_events_lost: trace.etl_events_lost,
                        etl_buffers_lost: trace.etl_buffers_lost,
                    }),
                },
            ))
        }
    }
}

pub fn record(request: RecordRequest) -> io::Result<Value> {
    let (capture_id, dir, collected) = prepare_capture(&evidence_root()?, request.source)?;
    let index_file = request.backend.filename();
    let raw = digest_artifact(&dir, request.source.filename())?;
    {
        let mut store = request.backend.create(&dir.join(index_file))?;
        for batch in collected.rows.chunks(MAX_PAGE_ROWS) {
            store.append(batch)?;
        }
        store.seal()?;
    }
    request.backend.validate_sealed(&dir.join(index_file))?;
    let index = digest_artifact(&dir, index_file)?;
    let coverage_complete = collected.native_loss.as_ref().is_none_or(|loss| {
        loss.session_events_lost == 0
            && loss.log_buffers_lost == 0
            && loss.etl_events_lost == 0
            && loss.etl_buffers_lost == 0
    });
    let etw_receipt = collected
        .native_loss
        .as_ref()
        .map(|_| digest_artifact(&dir, ETW_RECEIPT_FILE))
        .transpose()?;
    let manifest = Manifest {
        version: SCHEMA_VERSION,
        qpc_clock_domain: format!("{capture_id}:qpc"),
        capture_id,
        backend: request.backend,
        source: request.source,
        analysis_profile: request.source.profile().to_owned(),
        frequency_hz: collected.frequency_hz,
        utc_anchors: collected.utc_anchors,
        qpc_start: collected.coverage_qpc.0.to_string(),
        qpc_end: collected.coverage_qpc.1.to_string(),
        coverage_complete,
        native_loss: collected.native_loss,
        rows: collected.rows.len(),
        raw,
        index,
        etw_receipt,
    };
    let staged = dir.join("manifest.pending");
    save_json(&staged, &manifest)?;
    fs::rename(staged, dir.join("manifest.json"))?;
    Ok(serde_json::to_value(manifest)?)
}

fn load_capture(capture_id: &str) -> io::Result<(PathBuf, Manifest)> {
    let dir = capture_path(capture_id)?;
    let metadata = fs::symlink_metadata(&dir)?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid evidence directory",
        ));
    }
    let manifest: Manifest = serde_json::from_reader(
        plain_file(&dir.join("manifest.json"), MAX_MANIFEST_BYTES)?.take(MAX_MANIFEST_BYTES + 1),
    )?;
    if manifest.version != SCHEMA_VERSION
        || manifest.capture_id != capture_id
        || manifest.qpc_clock_domain != format!("{capture_id}:qpc")
        || manifest.analysis_profile != manifest.source.profile()
        || manifest.raw.filename != manifest.source.filename()
        || manifest.index.filename != manifest.backend.filename()
        || manifest
            .etw_receipt
            .as_ref()
            .map(|artifact| artifact.filename.as_str())
            != match manifest.source {
                Source::FixtureSelfReport => None,
                Source::EtwOwned => Some(ETW_RECEIPT_FILE),
            }
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid evidence manifest",
        ));
    }
    Ok((dir, manifest))
}

fn verify_artifact(dir: &Path, artifact: &Artifact) -> io::Result<()> {
    if digest_artifact(dir, &artifact.filename)? != *artifact {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "evidence digest mismatch",
        ));
    }
    Ok(())
}

fn verify_sealed(dir: &Path, manifest: &Manifest) -> io::Result<()> {
    manifest
        .backend
        .validate_sealed(&dir.join(&manifest.index.filename))?;
    verify_artifact(dir, &manifest.raw)?;
    verify_artifact(dir, &manifest.index)?;
    if let Some(receipt) = &manifest.etw_receipt {
        verify_artifact(dir, receipt)?;
    }
    Ok(())
}

pub fn verify(request: CaptureRequest) -> io::Result<Value> {
    let (dir, manifest) = load_capture(&request.capture_id)?;
    verify_sealed(&dir, &manifest)?;
    Ok(serde_json::to_value(manifest)?)
}

fn ticks(value: &str) -> io::Result<i64> {
    value
        .parse()
        .map_err(|_| input_error("invalid decimal QPC or sequence value"))
}

pub fn query(request: QueryRequest) -> io::Result<Value> {
    let (dir, manifest) = load_capture(&request.capture_id)?;
    let start = ticks(&manifest.qpc_start)?;
    let end = ticks(&manifest.qpc_end)?;
    let lo = request
        .qpc_start
        .as_deref()
        .map(ticks)
        .transpose()?
        .unwrap_or(start);
    let hi = request
        .qpc_end
        .as_deref()
        .map(ticks)
        .transpose()?
        .unwrap_or(end);
    let limit = request.limit.unwrap_or(MAX_PAGE_ROWS);
    if lo >= hi || !(1..=MAX_PAGE_ROWS).contains(&limit) {
        return Err(input_error("invalid query window or page limit"));
    }
    let query_id = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(
            &manifest,
            lo,
            hi,
            request.process_id,
        ))?)
    );
    let after = if let Some(cursor) = request.cursor {
        let qpc = ticks(&cursor.qpc)?;
        let sequence = ticks(&cursor.sequence)?;
        if cursor.query_id != query_id || !(lo..hi).contains(&qpc) || sequence < 0 {
            return Err(input_error("cursor does not match the sealed query"));
        }
        Some((qpc, sequence))
    } else {
        None
    };
    verify_sealed(&dir, &manifest)?;
    let store = manifest.backend.open(&dir.join(&manifest.index.filename))?;
    let mut rows = store.query(&Window {
        qpc_start: lo,
        qpc_end: hi,
        process_id: request.process_id,
        after,
        limit: limit + 1,
    })?;
    let has_next = rows.len() > limit;
    rows.truncate(limit);
    let cursor = rows.last().filter(|_| has_next).map(|row| Cursor {
        query_id: query_id.clone(),
        qpc: row.qpc.to_string(),
        sequence: row.sequence.to_string(),
    });
    let events = rows
        .into_iter()
        .map(|row| {
            let details: Value = serde_json::from_slice(&row.data)?;
            Ok(json!({
                "sequence": row.sequence.to_string(), "qpc": row.qpc.to_string(),
                "process_id": row.process_id, "kind": row.kind, "details": details,
                "locator": { "sha256": manifest.raw.sha256, "sequence": row.sequence.to_string() }
            }))
        })
        .collect::<io::Result<Vec<_>>>()?;
    let status = if hi <= start || lo >= end {
        "outside_capture"
    } else if manifest.coverage_complete && lo >= start && hi <= end {
        "complete"
    } else {
        "partial"
    };
    Ok(json!({
        "capture_id": manifest.capture_id,
        "source": manifest.source,
        "analysis_profile": manifest.analysis_profile,
        "qpc_clock_domain": manifest.qpc_clock_domain,
        "qpc_frequency_hz": manifest.frequency_hz,
        "coverage": {
            "status": status,
            "capture_qpc_start": manifest.qpc_start,
            "capture_qpc_end": manifest.qpc_end,
            "native_loss": manifest.native_loss
        },
        "query_id": query_id,
        "events": events,
        "next_cursor": cursor
    }))
}
