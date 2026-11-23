//! Append-only event frames. Each committed batch writes one independently
//! checksummed frame. Readers can replay the verified prefix of an interrupted
//! file and detect the unfinished tail without modifying the file.

use super::{Row, Store, Window};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Mutex;

const FILE_MAGIC: &[u8; 8] = b"TSCSEG\x01\0";
const FRAME_MAGIC: &[u8; 4] = b"TSFR";
const FRAME_HEADER_BYTES: usize = 36;
const ROW_HEADER_BYTES: usize = 24;
const MAX_ROWS: usize = 128;
const MAX_KIND_BYTES: usize = 256;
const MAX_DATA_BYTES: usize = 16 * 1024;
const MAX_FRAME_BYTES: usize = MAX_ROWS * (ROW_HEADER_BYTES + MAX_KIND_BYTES + MAX_DATA_BYTES);

#[derive(Clone, Copy)]
struct FrameHeader {
    bytes: usize,
    rows: usize,
    min_qpc: i64,
    max_qpc: i64,
    payload_crc: u32,
}

impl FrameHeader {
    // The header is little-endian. Its checksum covers the frame marker, sizes,
    // QPC bounds and payload checksum so a corrupt size cannot hide a bad frame.
    fn encode(self) -> [u8; FRAME_HEADER_BYTES] {
        let mut bytes = [0; FRAME_HEADER_BYTES];
        bytes[..4].copy_from_slice(FRAME_MAGIC);
        bytes[4..8].copy_from_slice(&(self.bytes as u32).to_le_bytes());
        bytes[8..12].copy_from_slice(&(self.rows as u32).to_le_bytes());
        bytes[12..20].copy_from_slice(&self.min_qpc.to_le_bytes());
        bytes[20..28].copy_from_slice(&self.max_qpc.to_le_bytes());
        bytes[28..32].copy_from_slice(&self.payload_crc.to_le_bytes());
        let crc = crc32fast::hash(&bytes[..32]);
        bytes[32..].copy_from_slice(&crc.to_le_bytes());
        bytes
    }

    fn read(file: &mut File) -> io::Result<Self> {
        let mut bytes = [0; FRAME_HEADER_BYTES];
        file.read_exact(&mut bytes)?;
        let mut remaining = bytes.as_slice();
        let magic = fixed(&mut remaining)?;
        let bytes_len = u32::from_le_bytes(fixed(&mut remaining)?) as usize;
        let rows = u32::from_le_bytes(fixed(&mut remaining)?) as usize;
        let min_qpc = i64::from_le_bytes(fixed(&mut remaining)?);
        let max_qpc = i64::from_le_bytes(fixed(&mut remaining)?);
        let payload_crc = u32::from_le_bytes(fixed(&mut remaining)?);
        let header_crc = u32::from_le_bytes(fixed(&mut remaining)?);
        if &magic != FRAME_MAGIC || crc32fast::hash(&bytes[..32]) != header_crc {
            return Err(invalid_data(
                "segment frame header checksum or marker mismatch",
            ));
        }
        if rows == 0
            || rows > MAX_ROWS
            || !(rows * ROW_HEADER_BYTES..=MAX_FRAME_BYTES).contains(&bytes_len)
            || min_qpc > max_qpc
        {
            return Err(invalid_data("invalid segment frame bounds"));
        }
        Ok(Self {
            bytes: bytes_len,
            rows,
            min_qpc,
            max_qpc,
            payload_crc,
        })
    }

    fn read_payload(self, file: &mut File, buffer: &mut Vec<u8>) -> io::Result<()> {
        buffer.resize(self.bytes, 0);
        file.read_exact(buffer)?;
        if crc32fast::hash(buffer) != self.payload_crc {
            return Err(invalid_data("segment frame payload checksum mismatch"));
        }
        Ok(())
    }
}

struct RowRef<'a> {
    sequence: i64,
    qpc: i64,
    process_id: u32,
    kind: &'a str,
    data: &'a [u8],
}

impl<'a> RowRef<'a> {
    fn read(remaining: &mut &'a [u8]) -> io::Result<Self> {
        let sequence = i64::from_le_bytes(fixed(remaining)?);
        let qpc = i64::from_le_bytes(fixed(remaining)?);
        let process_id = u32::from_le_bytes(fixed(remaining)?);
        let kind_bytes = u16::from_le_bytes(fixed(remaining)?) as usize;
        let data_bytes = u16::from_le_bytes(fixed(remaining)?) as usize;
        if kind_bytes > MAX_KIND_BYTES || data_bytes > MAX_DATA_BYTES {
            return Err(invalid_data("invalid segment row bounds"));
        }
        let kind = std::str::from_utf8(take(remaining, kind_bytes)?)
            .map_err(|_| invalid_data("invalid segment event kind UTF-8"))?;
        let data = take(remaining, data_bytes)?;
        Ok(Self {
            sequence,
            qpc,
            process_id,
            kind,
            data,
        })
    }

    fn into_row(self) -> Row {
        Row {
            sequence: self.sequence,
            qpc: self.qpc,
            process_id: self.process_id,
            kind: self.kind.to_owned(),
            data: self.data.to_vec(),
        }
    }
}

struct QueryRow(Row);

impl QueryRow {
    fn key(&self) -> (i64, i64) {
        (self.0.qpc, self.0.sequence)
    }
}

impl PartialEq for QueryRow {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for QueryRow {}

impl PartialOrd for QueryRow {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for QueryRow {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key().cmp(&other.key())
    }
}

pub struct SegmentStore {
    file: Mutex<File>,
    verified_end: u64,
    last_sequence: Option<i64>,
    writable: bool,
    incomplete_tail: bool,
    encoded: Vec<u8>,
}

impl SegmentStore {
    pub fn create(path: &Path) -> io::Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        file.write_all(FILE_MAGIC)?;
        file.sync_all()?;
        Ok(Self {
            file: Mutex::new(file),
            verified_end: FILE_MAGIC.len() as u64,
            last_sequence: None,
            writable: true,
            incomplete_tail: false,
            encoded: Vec::new(),
        })
    }

    /// Opens an existing segment read-only. A partial final header or payload
    /// is exposed separately from the verified prefix. A corrupt complete
    /// frame is an error even when it is the final frame.
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        let mut magic = [0; FILE_MAGIC.len()];
        file.read_exact(&mut magic)?;
        if &magic != FILE_MAGIC {
            return Err(invalid_data("unsupported segment file header"));
        }
        let mut verified_end = FILE_MAGIC.len() as u64;
        let mut last_sequence = None;
        let mut buffer = Vec::new();
        while len - verified_end >= FRAME_HEADER_BYTES as u64 {
            let header = FrameHeader::read(&mut file)?;
            let frame_len = (FRAME_HEADER_BYTES + header.bytes) as u64;
            if frame_len > len - verified_end {
                break;
            }
            header.read_payload(&mut file, &mut buffer)?;
            let mut remaining = buffer.as_slice();
            let mut min_qpc = i64::MAX;
            let mut max_qpc = i64::MIN;
            for _ in 0..header.rows {
                let row = RowRef::read(&mut remaining)?;
                if last_sequence.is_some_and(|prev| row.sequence <= prev) {
                    return Err(invalid_data("segment sequence is not strictly increasing"));
                }
                last_sequence = Some(row.sequence);
                min_qpc = min_qpc.min(row.qpc);
                max_qpc = max_qpc.max(row.qpc);
            }
            if !remaining.is_empty() || min_qpc != header.min_qpc || max_qpc != header.max_qpc {
                return Err(invalid_data(
                    "segment frame metadata does not match its rows",
                ));
            }
            verified_end += frame_len;
        }
        Ok(Self {
            file: Mutex::new(file),
            verified_end,
            last_sequence,
            writable: false,
            incomplete_tail: verified_end != len,
            encoded: Vec::new(),
        })
    }

    pub fn has_incomplete_tail(&self) -> bool {
        self.incomplete_tail
    }
}

impl Store for SegmentStore {
    fn append(&mut self, rows: &[Row]) -> io::Result<()> {
        if !self.writable {
            return Err(io::Error::other(
                "segment is read-only or its writer failed",
            ));
        }
        if rows.is_empty() {
            return Ok(());
        }
        if rows.len() > MAX_ROWS {
            return Err(invalid_input("segment batch exceeds frame row limit"));
        }
        self.encoded.clear();
        let mut min_qpc = i64::MAX;
        let mut max_qpc = i64::MIN;
        let mut last_sequence = self.last_sequence;
        for row in rows {
            if last_sequence.is_some_and(|prev| row.sequence <= prev) {
                return Err(invalid_input("segment sequence is not strictly increasing"));
            }
            let kind = row.kind.as_bytes();
            if kind.len() > MAX_KIND_BYTES || row.data.len() > MAX_DATA_BYTES {
                return Err(invalid_input("segment event kind or data exceeds limit"));
            }
            self.encoded.extend_from_slice(&row.sequence.to_le_bytes());
            self.encoded.extend_from_slice(&row.qpc.to_le_bytes());
            self.encoded
                .extend_from_slice(&row.process_id.to_le_bytes());
            self.encoded
                .extend_from_slice(&(kind.len() as u16).to_le_bytes());
            self.encoded
                .extend_from_slice(&(row.data.len() as u16).to_le_bytes());
            self.encoded.extend_from_slice(kind);
            self.encoded.extend_from_slice(&row.data);
            min_qpc = min_qpc.min(row.qpc);
            max_qpc = max_qpc.max(row.qpc);
            last_sequence = Some(row.sequence);
        }
        let header = FrameHeader {
            bytes: self.encoded.len(),
            rows: rows.len(),
            min_qpc,
            max_qpc,
            payload_crc: crc32fast::hash(&self.encoded),
        };
        let end = self
            .verified_end
            .checked_add((FRAME_HEADER_BYTES + header.bytes) as u64)
            .ok_or_else(|| invalid_input("segment file offset overflow"))?;
        let file = self
            .file
            .get_mut()
            .map_err(|_| io::Error::other("segment file lock poisoned"))?;
        file.seek(SeekFrom::Start(self.verified_end))?;
        // After an uncertain write or flush, only a fresh read-only open may
        // determine which complete frames survived.
        self.writable = false;
        file.write_all(&header.encode())?;
        file.write_all(&self.encoded)?;
        file.sync_all()?;
        self.verified_end = end;
        self.last_sequence = last_sequence;
        self.writable = true;
        Ok(())
    }

    fn query(&self, window: &Window) -> io::Result<Vec<Row>> {
        if window.limit == 0 || window.qpc_start >= window.qpc_end {
            return Ok(Vec::new());
        }
        let mut file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("segment file lock poisoned"))?;
        let mut position = FILE_MAGIC.len() as u64;
        file.seek(SeekFrom::Start(position))?;
        let mut result: BinaryHeap<QueryRow> = BinaryHeap::new();
        let mut buffer = Vec::new();
        while position < self.verified_end {
            let header = FrameHeader::read(&mut file)?;
            let frame_end = position + (FRAME_HEADER_BYTES + header.bytes) as u64;
            if frame_end > self.verified_end {
                return Err(invalid_data("segment frame crosses verified boundary"));
            }
            position = frame_end;
            if header.max_qpc < window.qpc_start || header.min_qpc >= window.qpc_end {
                file.seek(SeekFrom::Start(position))?;
                continue;
            }
            header.read_payload(&mut file, &mut buffer)?;
            let mut remaining = buffer.as_slice();
            for _ in 0..header.rows {
                let row = RowRef::read(&mut remaining)?;
                let key = (row.qpc, row.sequence);
                if row.qpc < window.qpc_start
                    || row.qpc >= window.qpc_end
                    || window.process_id.is_some_and(|pid| row.process_id != pid)
                    || window.after.is_some_and(|after| key <= after)
                {
                    continue;
                }
                if result.len() == window.limit {
                    if result.peek().is_some_and(|largest| key >= largest.key()) {
                        continue;
                    }
                    result.pop();
                }
                result.push(QueryRow(row.into_row()));
            }
            if !remaining.is_empty() {
                return Err(invalid_data("extra bytes in segment frame payload"));
            }
        }
        Ok(result
            .into_sorted_vec()
            .into_iter()
            .map(|row| row.0)
            .collect())
    }
}

fn fixed<const N: usize>(input: &mut &[u8]) -> io::Result<[u8; N]> {
    let (value, rest) = input
        .split_first_chunk()
        .ok_or_else(|| invalid_data("truncated segment field"))?;
    *input = rest;
    Ok(*value)
}

fn take<'a>(input: &mut &'a [u8], len: usize) -> io::Result<&'a [u8]> {
    let (value, rest) = input
        .split_at_checked(len)
        .ok_or_else(|| invalid_data("truncated segment field"))?;
    *input = rest;
    Ok(value)
}

fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn invalid_input(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
