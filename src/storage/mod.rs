mod indexed;
mod segments;
#[cfg(test)]
mod validation;

use serde::{Deserialize, Serialize};
use std::{io, path::Path};

use indexed::{RedbStore, SqliteStore};
use segments::SegmentStore;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub sequence: i64,
    pub qpc: i64,
    pub process_id: u32,
    pub kind: String,
    pub data: Vec<u8>,
}

pub struct Window {
    pub qpc_start: i64,
    pub qpc_end: i64,
    pub process_id: Option<u32>,
    pub after: Option<(i64, i64)>,
    pub limit: usize,
}

/// All stores acknowledge a batch only after their durable write boundary.
pub trait Store {
    fn append(&mut self, rows: &[Row]) -> io::Result<()>;
    fn query(&self, window: &Window) -> io::Result<Vec<Row>>;
    fn seal(self: Box<Self>) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Sqlite,
    Segments,
    Redb,
}

impl Backend {
    pub fn filename(self) -> &'static str {
        match self {
            Self::Sqlite => "events.sqlite",
            Self::Segments => "events.segments",
            Self::Redb => "events.redb",
        }
    }

    pub fn create(self, path: &Path) -> io::Result<Box<dyn Store>> {
        match self {
            Self::Sqlite => Ok(Box::new(SqliteStore::create(path)?)),
            Self::Segments => Ok(Box::new(SegmentStore::create(path)?)),
            Self::Redb => Ok(Box::new(RedbStore::create(path)?)),
        }
    }

    pub fn open(self, path: &Path) -> io::Result<Box<dyn Store>> {
        match self {
            Self::Sqlite => Ok(Box::new(SqliteStore::open_read(path)?)),
            Self::Segments => {
                let store = SegmentStore::open(path)?;
                if store.has_incomplete_tail() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "sealed segment contains an incomplete tail",
                    ));
                }
                Ok(Box::new(store))
            }
            Self::Redb => Ok(Box::new(RedbStore::open_read(path)?)),
        }
    }

    pub fn validate_sealed(self, path: &Path) -> io::Result<()> {
        if let Self::Sqlite = self {
            SqliteStore::validate_sealed(path)?;
        }
        Ok(())
    }
}
