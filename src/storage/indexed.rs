use std::fs::{self, OpenOptions};
use std::io;
use std::path::Path;

use redb::{
    Database, Durability, ReadOnlyDatabase, ReadableDatabase, ReadableTable, TableDefinition,
};
use rusqlite::{Connection, OpenFlags, Params, TransactionBehavior, params};

use super::{Row, Store, Window};

const CACHE_BYTES: usize = 8 * 1024 * 1024;
const SQLITE_APP_ID: i32 = 0x5453_5131;
const SQLITE_FORMAT_VERSION: i32 = 1;
const SQLITE_SCHEMA: &str = "
    CREATE TABLE events(
        sequence INTEGER PRIMARY KEY,
        qpc INTEGER NOT NULL,
        process_id INTEGER NOT NULL CHECK(process_id BETWEEN 0 AND 4294967295),
        kind TEXT NOT NULL,
        data BLOB NOT NULL
    ) STRICT;
    CREATE INDEX events_qpc ON events(qpc, sequence);
    CREATE INDEX events_process_qpc ON events(process_id, qpc, sequence);
";

fn reserve_file(path: &Path) -> io::Result<()> {
    OpenOptions::new().write(true).create_new(true).open(path)?;
    Ok(())
}

fn invalid_store(detail: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, detail)
}

fn check_sequence(previous: Option<i64>, rows: &[Row]) -> io::Result<()> {
    if let Some(first) = rows.first() {
        let ordered = rows
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence);
        if previous.is_some_and(|last| first.sequence <= last) || !ordered {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "event sequence must increase across batches",
            ));
        }
    }
    Ok(())
}

fn query_bounds(window: &Window) -> Option<((i64, i64), (i64, i64))> {
    if window.limit == 0 {
        return None;
    }
    let first = (window.qpc_start, i64::MIN);
    let last = (window.qpc_end, i64::MIN);
    let lower = match window.after {
        Some((qpc, sequence)) if qpc >= window.qpc_start => match sequence.checked_add(1) {
            Some(next) => (qpc, next),
            None => (qpc.checked_add(1)?, i64::MIN),
        },
        _ => first,
    };
    (lower < last).then_some((lower, last))
}

pub struct SqliteStore {
    connection: Connection,
}

impl SqliteStore {
    pub fn create(path: &Path) -> io::Result<Self> {
        reserve_file(path)?;
        let mut connection = Self::read_write(path)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(io::Error::other)?;
        Self::configure(&connection)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(io::Error::other)?;
        transaction
            .execute_batch(SQLITE_SCHEMA)
            .map_err(io::Error::other)?;
        transaction
            .pragma_update(None, "application_id", SQLITE_APP_ID)
            .map_err(io::Error::other)?;
        transaction
            .pragma_update(None, "user_version", SQLITE_FORMAT_VERSION)
            .map_err(io::Error::other)?;
        transaction.commit().map_err(io::Error::other)?;
        Ok(Self { connection })
    }

    #[cfg(test)]
    fn open(path: &Path) -> io::Result<Self> {
        let connection = Self::read_write(path)?;
        Self::verify_format(&connection)?;
        Self::configure(&connection)?;
        Ok(Self { connection })
    }

    /// Open a sealed store for queries without a writable handle to the database.
    pub fn open_read(path: &Path) -> io::Result<Self> {
        Self::validate_sealed(path)?;
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(io::Error::other)?;
        Self::verify_format(&connection)?;
        Self::configure_cache(&connection)?;
        Ok(Self { connection })
    }

    pub fn validate_sealed(path: &Path) -> io::Result<()> {
        let mut wal = path.as_os_str().to_owned();
        wal.push("-wal");
        match fs::symlink_metadata(&wal) {
            Ok(meta) if meta.is_file() && meta.len() == 0 => Ok(()),
            Ok(_) => Err(invalid_store(
                "sealed SQLite database has unexpected WAL data",
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn verify_format(connection: &Connection) -> io::Result<()> {
        let app_id: i32 = connection
            .pragma_query_value(None, "application_id", |row| row.get(0))
            .map_err(io::Error::other)?;
        let version: i32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(io::Error::other)?;
        if app_id != SQLITE_APP_ID || version != SQLITE_FORMAT_VERSION {
            return Err(invalid_store("unrecognized SQLite evidence format"));
        }
        Ok(())
    }

    fn read_write(path: &Path) -> io::Result<Connection> {
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(io::Error::other)
    }

    fn configure(connection: &Connection) -> io::Result<()> {
        let mode: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .map_err(io::Error::other)?;
        if mode != "wal" {
            return Err(invalid_store("SQLite evidence requires WAL mode"));
        }
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(io::Error::other)?;
        Self::configure_cache(connection)
    }

    fn configure_cache(connection: &Connection) -> io::Result<()> {
        connection
            .pragma_update(None, "cache_size", -((CACHE_BYTES / 1024) as i64))
            .map_err(io::Error::other)
    }

    fn query_rows(&self, sql: &str, params: impl Params) -> io::Result<Vec<Row>> {
        let mut statement = self
            .connection
            .prepare_cached(sql)
            .map_err(io::Error::other)?;
        let rows = statement
            .query_map(params, |row| {
                Ok(Row {
                    sequence: row.get(0)?,
                    qpc: row.get(1)?,
                    process_id: row.get(2)?,
                    kind: row.get(3)?,
                    data: row.get(4)?,
                })
            })
            .map_err(io::Error::other)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(io::Error::other)
    }
}

impl Store for SqliteStore {
    fn seal(self: Box<Self>) -> io::Result<()> {
        let (busy, remaining): (i64, i64) = self
            .connection
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .map_err(io::Error::other)?;
        if busy != 0 || remaining != 0 {
            return Err(io::Error::other(
                "SQLite seal checkpoint did not drain the WAL",
            ));
        }
        self.connection
            .close()
            .map_err(|(_, error)| io::Error::other(error))
    }

    fn append(&mut self, rows: &[Row]) -> io::Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(io::Error::other)?;
        let last_sequence = transaction
            .query_row("SELECT max(sequence) FROM events", [], |row| row.get(0))
            .map_err(io::Error::other)?;
        check_sequence(last_sequence, rows)?;
        {
            let mut insert = transaction
                .prepare_cached("INSERT INTO events VALUES (?1, ?2, ?3, ?4, ?5)")
                .map_err(io::Error::other)?;
            for row in rows {
                insert
                    .execute(params![
                        row.sequence,
                        row.qpc,
                        row.process_id,
                        row.kind,
                        row.data
                    ])
                    .map_err(io::Error::other)?;
            }
        }
        transaction.commit().map_err(io::Error::other)
    }

    fn query(&self, window: &Window) -> io::Result<Vec<Row>> {
        let Some(((start, sequence), (end, _))) = query_bounds(window) else {
            return Ok(Vec::new());
        };
        let limit = i64::try_from(window.limit).map_err(io::Error::other)?;
        match window.process_id {
            Some(pid) => self.query_rows(
                "SELECT sequence, qpc, process_id, kind, data FROM events
                 WHERE process_id = ?1 AND (qpc, sequence) >= (?2, ?3)
                   AND qpc < ?4 ORDER BY qpc, sequence LIMIT ?5",
                params![pid, start, sequence, end, limit],
            ),
            None => self.query_rows(
                "SELECT sequence, qpc, process_id, kind, data FROM events
                 WHERE (qpc, sequence) >= (?1, ?2)
                   AND qpc < ?3 ORDER BY qpc, sequence LIMIT ?4",
                params![start, sequence, end, limit],
            ),
        }
    }
}

type EventKey = (i64, i64);
type EventValue = (u32, &'static str, &'static [u8]);
const EVENTS: TableDefinition<EventKey, EventValue> = TableDefinition::new("events_v1");
const PROCESS_EVENTS: TableDefinition<(u32, i64, i64), ()> =
    TableDefinition::new("process_events_v1");
const SEQUENCE: TableDefinition<u8, i64> = TableDefinition::new("sequence_v1");
const LAST_SEQUENCE: u8 = 0;

enum RedbHandle {
    ReadWrite(Database),
    ReadOnly(ReadOnlyDatabase),
}

pub struct RedbStore {
    database: RedbHandle,
}

impl RedbStore {
    pub fn create(path: &Path) -> io::Result<Self> {
        reserve_file(path)?;
        let database = Database::builder()
            .set_cache_size(CACHE_BYTES)
            .create(path)
            .map_err(io::Error::other)?;
        let transaction = database.begin_write().map_err(io::Error::other)?;
        transaction.open_table(EVENTS).map_err(io::Error::other)?;
        transaction
            .open_table(PROCESS_EVENTS)
            .map_err(io::Error::other)?;
        transaction.open_table(SEQUENCE).map_err(io::Error::other)?;
        transaction.commit().map_err(io::Error::other)?;
        Ok(Self {
            database: RedbHandle::ReadWrite(database),
        })
    }

    #[cfg(test)]
    fn open(path: &Path) -> io::Result<Self> {
        let database = Database::builder()
            .set_cache_size(CACHE_BYTES)
            .set_check_integrity_on_open(true)
            .open(path)
            .map_err(io::Error::other)?;
        Self::verify_format(&database)?;
        Ok(Self {
            database: RedbHandle::ReadWrite(database),
        })
    }

    /// Open a cleanly sealed, digest-verified artifact without changing the file.
    pub fn open_read(path: &Path) -> io::Result<Self> {
        let database = Database::builder()
            .set_cache_size(CACHE_BYTES)
            .open_read_only(path)
            .map_err(io::Error::other)?;
        Self::verify_format(&database)?;
        Ok(Self {
            database: RedbHandle::ReadOnly(database),
        })
    }

    fn verify_format(database: &impl ReadableDatabase) -> io::Result<()> {
        let transaction = database.begin_read().map_err(io::Error::other)?;
        transaction.open_table(EVENTS).map_err(io::Error::other)?;
        transaction
            .open_table(PROCESS_EVENTS)
            .map_err(io::Error::other)?;
        transaction.open_table(SEQUENCE).map_err(io::Error::other)?;
        Ok(())
    }
}

fn owned_row((qpc, sequence): EventKey, (process_id, kind, data): (u32, &str, &[u8])) -> Row {
    Row {
        sequence,
        qpc,
        process_id,
        kind: kind.to_owned(),
        data: data.to_owned(),
    }
}

impl Store for RedbStore {
    fn append(&mut self, rows: &[Row]) -> io::Result<()> {
        let Some(last_row) = rows.last() else {
            return Ok(());
        };
        let RedbHandle::ReadWrite(database) = &self.database else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "redb store is read-only",
            ));
        };
        let mut transaction = database.begin_write().map_err(io::Error::other)?;
        transaction
            .set_durability(Durability::Immediate)
            .map_err(io::Error::other)?;
        {
            let mut cursor = transaction.open_table(SEQUENCE).map_err(io::Error::other)?;
            let last_sequence = cursor
                .get(LAST_SEQUENCE)
                .map_err(io::Error::other)?
                .map(|guard| guard.value());
            check_sequence(last_sequence, rows)?;
            let mut events = transaction.open_table(EVENTS).map_err(io::Error::other)?;
            let mut by_process = transaction
                .open_table(PROCESS_EVENTS)
                .map_err(io::Error::other)?;
            for row in rows {
                events
                    .insert(
                        (row.qpc, row.sequence),
                        (row.process_id, row.kind.as_str(), row.data.as_slice()),
                    )
                    .map_err(io::Error::other)?;
                by_process
                    .insert((row.process_id, row.qpc, row.sequence), ())
                    .map_err(io::Error::other)?;
            }
            cursor
                .insert(LAST_SEQUENCE, last_row.sequence)
                .map_err(io::Error::other)?;
        }
        transaction.commit().map_err(io::Error::other)
    }

    fn query(&self, window: &Window) -> io::Result<Vec<Row>> {
        match &self.database {
            RedbHandle::ReadWrite(database) => query_redb(database, window),
            RedbHandle::ReadOnly(database) => query_redb(database, window),
        }
    }
}

fn query_redb(database: &impl ReadableDatabase, window: &Window) -> io::Result<Vec<Row>> {
    let Some((lower, upper)) = query_bounds(window) else {
        return Ok(Vec::new());
    };
    let transaction = database.begin_read().map_err(io::Error::other)?;
    let events = transaction.open_table(EVENTS).map_err(io::Error::other)?;
    let mut rows = Vec::new();
    if let Some(pid) = window.process_id {
        let by_process = transaction
            .open_table(PROCESS_EVENTS)
            .map_err(io::Error::other)?;
        let range = (pid, lower.0, lower.1)..(pid, upper.0, upper.1);
        for entry in by_process
            .range(range)
            .map_err(io::Error::other)?
            .take(window.limit)
        {
            let (key, _) = entry.map_err(io::Error::other)?;
            let (_, qpc, sequence) = key.value();
            let event_key = (qpc, sequence);
            let value = events
                .get(event_key)
                .map_err(io::Error::other)?
                .ok_or_else(|| invalid_store("missing indexed redb event"))?;
            let value = value.value();
            if value.0 != pid {
                return Err(invalid_store("redb event process does not match index"));
            }
            rows.push(owned_row(event_key, value));
        }
    } else {
        for entry in events
            .range(lower..upper)
            .map_err(io::Error::other)?
            .take(window.limit)
        {
            let (key, value) = entry.map_err(io::Error::other)?;
            rows.push(owned_row(key.value(), value.value()));
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            let parent = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("indexed-tests");
            fs::create_dir_all(&parent).unwrap();
            let path = parent.join(format!(
                "tracescalpel-indexed-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn row(sequence: i64, qpc: i64, process_id: u32) -> Row {
        Row {
            sequence,
            qpc,
            process_id,
            kind: "file_marker".to_owned(),
            data: vec![0, 255, 42],
        }
    }

    fn exercise<S: Store>(
        create: fn(&Path) -> io::Result<S>,
        open: fn(&Path) -> io::Result<S>,
        open_read: fn(&Path) -> io::Result<S>,
    ) {
        let scratch = Scratch::new();
        let path = scratch.0.join("capture");
        assert!(open(&path).is_err());
        let mut store = create(&path).unwrap();
        assert!(create(&path).is_err());
        let first = row(1, 55, 34);
        let second = row(2, 50, 34);
        let third = row(3, 50, 55);
        let fourth = row(4, 56, 34);
        store
            .append(&[first.clone(), second.clone(), third.clone(), fourth.clone()])
            .unwrap();
        // Duplicate or regressing sequence halfway through rejects the entire batch.
        assert!(store.append(&[row(6, 52, 34), row(4, 53, 34)]).is_err());
        drop(store);
        let mut store = open(&path).unwrap();
        assert!(store.append(&[row(4, 52, 34)]).is_err());
        let fifth = row(5, 57, 55);
        store.append(std::slice::from_ref(&fifth)).unwrap();

        let mut window = Window {
            qpc_start: 50,
            qpc_end: 57,
            process_id: None,
            after: None,
            limit: 2,
        };
        assert_eq!(store.query(&window).unwrap(), [second.clone(), third]);
        window.after = Some((50, 3));
        assert_eq!(
            store.query(&window).unwrap(),
            [first.clone(), fourth.clone()]
        );
        window.process_id = Some(34);
        window.after = None;
        assert_eq!(store.query(&window).unwrap(), [second, first]);
        window.after = Some((i64::MAX, i64::MAX));
        assert!(store.query(&window).unwrap().is_empty());
        drop(store);

        window.qpc_start = 56;
        window.qpc_end = 58;
        window.process_id = None;
        window.after = None;
        let sealed = fs::read(&path).unwrap();
        let mut store = open_read(&path).unwrap();
        assert_eq!(store.query(&window).unwrap(), [fourth, fifth]);
        assert!(store.append(&[row(6, 57, 34)]).is_err());
        drop(store);
        assert_eq!(fs::read(&path).unwrap(), sealed);

        let corrupt = scratch.0.join("corrupt");
        fs::write(&corrupt, b"unrecognized evidence file").unwrap();
        assert!(open(&corrupt).is_err());
        assert!(open_read(&corrupt).is_err());
    }

    #[test]
    fn sqlite_wal_full_indexed_contract() {
        assert_eq!(rusqlite::version(), "3.53.2");
        exercise(
            SqliteStore::create,
            SqliteStore::open,
            SqliteStore::open_read,
        );
    }

    #[test]
    fn redb_immediate_indexed_contract() {
        exercise(RedbStore::create, RedbStore::open, RedbStore::open_read);
    }
}
