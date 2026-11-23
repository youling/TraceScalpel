//! Bounded summary of attributed ETW headers. Payload bytes are only counted.

use serde::Serialize;
use serde_json::{Value, json};
use std::io;

const MAX_KINDS: usize = 64;
const MAX_FIELDS: usize = 64;
const MAX_FIELD_NAME_BYTES: usize = 128;
const MAX_BUCKETS: usize = 1200;

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Kind {
    pub source: &'static str,
    pub event_id: u16,
    pub opcode: u8,
    pub version: u8,
}

#[derive(Serialize)]
pub struct Field {
    pub name: String,
    pub in_type: u16,
    pub out_type: u16,
}

#[derive(Clone, Copy, Default, Serialize)]
struct Counts {
    events: u64,
    user_data_bytes: u64,
}

impl Counts {
    fn add(&mut self, bytes: u16) {
        self.events = self.events.saturating_add(1);
        self.user_data_bytes = self.user_data_bytes.saturating_add(u64::from(bytes));
    }
}

#[derive(Serialize)]
struct Summary {
    #[serde(flatten)]
    kind: Kind,
    #[serde(flatten)]
    counts: Counts,
    min_user_data_bytes: u16,
    max_user_data_bytes: u16,
    fields: Option<Vec<Field>>,
}

pub struct Survey {
    qpc_start: i64,
    qpc_end: i64,
    qpc_frequency: i64,
    bucket_ticks: i64,
    bins: Box<[Counts]>,
    kinds: Vec<Summary>,
    total: Counts,
    ungrouped: Counts,
    outside: Counts,
}

impl Survey {
    pub fn new(qpc_start: i64, qpc_end: i64, qpc_frequency: i64) -> io::Result<Self> {
        let duration = qpc_end.checked_sub(qpc_start).filter(|n| *n > 0);
        let bucket_ticks = qpc_frequency.checked_add(9).filter(|_| qpc_frequency > 0);
        let (Some(duration), Some(width)) = (duration, bucket_ticks) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid survey clock",
            ));
        };
        let bucket_ticks = width / 10;
        let bucket_count = (duration - 1) / bucket_ticks + 1;
        if bucket_count > MAX_BUCKETS as i64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "survey window exceeds the bucket limit",
            ));
        }
        Ok(Self {
            qpc_start,
            qpc_end,
            qpc_frequency,
            bucket_ticks,
            bins: vec![Counts::default(); bucket_count as usize].into_boxed_slice(),
            kinds: Vec::with_capacity(MAX_KINDS),
            total: Counts::default(),
            ungrouped: Counts::default(),
            outside: Counts::default(),
        })
    }

    /// True marks the first observation of a tracked kind, so the adapter can sample its schema.
    pub fn observe(&mut self, kind: Kind, qpc: i64, payload_bytes: u16) -> Option<bool> {
        if !(self.qpc_start..self.qpc_end).contains(&qpc) {
            self.outside.add(payload_bytes);
            return None;
        }
        self.total.add(payload_bytes);
        let index = ((qpc - self.qpc_start) / self.bucket_ticks) as usize;
        self.bins[index].add(payload_bytes);
        if let Some(summary) = self.kinds.iter_mut().find(|summary| summary.kind == kind) {
            summary.counts.add(payload_bytes);
            summary.min_user_data_bytes = summary.min_user_data_bytes.min(payload_bytes);
            summary.max_user_data_bytes = summary.max_user_data_bytes.max(payload_bytes);
            return Some(false);
        }
        if self.kinds.len() == MAX_KINDS {
            self.ungrouped.add(payload_bytes);
            return None;
        }
        let mut counts = Counts::default();
        counts.add(payload_bytes);
        self.kinds.push(Summary {
            kind,
            counts,
            min_user_data_bytes: payload_bytes,
            max_user_data_bytes: payload_bytes,
            fields: None,
        });
        Some(true)
    }

    pub fn set_fields(&mut self, kind: Kind, fields: Vec<Field>) {
        if fields.len() > MAX_FIELDS
            || fields
                .iter()
                .any(|field| field.name.len() > MAX_FIELD_NAME_BYTES)
        {
            return;
        }
        if let Some(summary) = self.kinds.iter_mut().find(|summary| summary.kind == kind) {
            summary.fields = Some(fields);
        }
    }

    pub fn report(&self) -> Value {
        json!({
            "qpc_start": self.qpc_start.to_string(),
            "qpc_end": self.qpc_end.to_string(),
            "qpc_frequency_hz": self.qpc_frequency.to_string(),
            "bucket_width_ticks": self.bucket_ticks.to_string(),
            "buckets": self.bins.len(),
            "nonempty_buckets": self.bins.iter().filter(|bin| bin.events > 0).count(),
            "peak_events_per_bucket": self.bins.iter().map(|bin| bin.events).max(),
            "peak_user_data_bytes_per_bucket": self.bins.iter().map(|bin| bin.user_data_bytes).max(),
            "total": self.total,
            "ungrouped": self.ungrouped,
            "outside_window": self.outside,
            "event_types": self.kinds
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(opcode: u8, version: u8) -> Kind {
        Kind {
            source: "fixture",
            event_id: 0,
            opcode,
            version,
        }
    }

    #[test]
    fn report_counts_bytes_and_half_open_windows_without_payloads() {
        let mut survey = Survey::new(100, 350, 1_000).unwrap();
        assert_eq!(survey.observe(kind(64, 3), 100, 50), Some(true));
        assert_eq!(survey.observe(kind(64, 3), 199, 25), Some(false));
        assert_eq!(survey.observe(kind(64, 2), 200, 90), Some(true));
        assert_eq!(survey.observe(kind(32, 3), 300, 12), Some(true));
        assert_eq!(survey.observe(kind(64, 3), 350, 200), None);
        survey.set_fields(
            kind(64, 3),
            vec![Field {
                name: "Name".into(),
                in_type: 1,
                out_type: 1,
            }],
        );
        let report = survey.report();
        assert_eq!(report["total"], json!({"events":4,"user_data_bytes":177}));
        assert_eq!(report["outside_window"]["events"], 1);
        assert_eq!(report["peak_events_per_bucket"], 2);
        assert_eq!(report["peak_user_data_bytes_per_bucket"], 90);
        assert_eq!(report["event_types"][0]["fields"][0]["name"], "Name");
        assert_eq!(report["event_types"][0]["min_user_data_bytes"], 25);
        assert_eq!(report["event_types"][0]["max_user_data_bytes"], 50);
        assert_eq!(report["event_types"][1]["fields"], Value::Null);
        assert_eq!(report["event_types"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn capacity_keeps_totals_and_surfaces_kinds_that_cannot_be_grouped() {
        assert!(Survey::new(0, 125_000, 1_000).is_err());
        let mut survey = Survey::new(0, 20, 1_000).unwrap();
        for opcode in 0..=64 {
            survey.observe(kind(opcode, 1), 8, 2);
        }
        survey.set_fields(
            kind(0, 1),
            vec![Field {
                name: "x".repeat(129),
                in_type: 1,
                out_type: 1,
            }],
        );
        let report = survey.report();
        assert_eq!(report["total"]["events"], 65);
        assert_eq!(report["ungrouped"], json!({"events":1,"user_data_bytes":2}));
        assert_eq!(report["event_types"].as_array().unwrap().len(), 64);
        assert_eq!(report["event_types"][0]["fields"], Value::Null);
    }
}
