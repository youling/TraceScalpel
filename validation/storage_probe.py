"""Synthetic Windows local storage probe. Run against a new scratch directory."""

import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import json
import os
from pathlib import Path
from queue import Empty, Queue
from statistics import median, quantiles
from struct import Struct
from threading import Event
from time import perf_counter

import apsw
import duckdb


TARGET = 1000
SCHEMA = """
CREATE TABLE events (
    seq INTEGER PRIMARY KEY, qpc INTEGER NOT NULL, process_id INTEGER NOT NULL,
    op TEXT NOT NULL, path TEXT NOT NULL, byte_count INTEGER NOT NULL
);
CREATE INDEX process_window ON events(process_id, qpc, seq);
"""
INSERT = "INSERT INTO events VALUES (?, ?, ?, ?, ?, ?)"
WINDOW = """
SELECT count(*) FROM events WHERE process_id = ? AND qpc >= ? AND qpc < ?
"""


def connect(path):
    db = apsw.Connection(str(path))
    db.execute("PRAGMA journal_mode=WAL")
    db.execute("PRAGMA synchronous=FULL")
    db.execute("PRAGMA cache_size=-16384")
    db.execute("PRAGMA wal_autocheckpoint=1000")
    db.execute(SCHEMA)
    return db


def row(seq):
    return (
        seq, 10_000_000_000 + seq * 100, TARGET + seq % 8,
        ("create", "write", "rename")[seq % 3],
        f"/synthetic/workspace-{seq % 17:02d}/cache/entry-{seq % 6001:04d}.bin",
        (seq % 31) * 4096,
    )


def file_bytes(path):
    try:
        return path.stat().st_size
    except FileNotFoundError:
        return 0


def load_sqlite(path, total, batch_size, hold_reader):
    wal = Path(str(path) + "-wal")
    db = connect(path)
    reader = None
    if hold_reader:
        reader = apsw.Connection(str(path), flags=apsw.SQLITE_OPEN_READONLY)
        reader.execute("BEGIN")
        assert reader.execute("SELECT count(*) FROM events").get == 0
    started = perf_counter()
    durations = []
    peak_wal = 0
    for base in range(0, total, batch_size):
        batch = [row(i) for i in range(base, min(base + batch_size, total))]
        begin = perf_counter()
        with db:
            db.executemany(INSERT, batch)
        durations.append((perf_counter() - begin) * 1000)
        peak_wal = max(peak_wal, file_bytes(wal))
    elapsed = perf_counter() - started
    pinned_checkpoint = None
    if reader:
        pinned_checkpoint = db.execute("PRAGMA wal_checkpoint(TRUNCATE)").get
        assert pinned_checkpoint[0] == 1
        reader.execute("ROLLBACK")
        reader.close()
    timings, count = timed_windows(db, WINDOW, total)
    checkpoint = db.execute("PRAGMA wal_checkpoint(TRUNCATE)").get
    assert checkpoint == (0, 0, 0) and file_bytes(wal) == 0
    assert db.execute("SELECT count(*) FROM events").get == total
    db.close()
    return {
        "events_per_s": round(total / elapsed),
        "batch_p95_ms": round(quantiles(durations, n=20)[18], 2),
        "peak_wal_bytes_sampled": peak_wal,
        "checkpoint_while_reader_pinned": pinned_checkpoint,
        "database_bytes": file_bytes(path),
        "warm_window_median_ms": timings,
        "rows_counted_across_windows": count,
    }


def timed_windows(conn, sql, total):
    times = []
    rows = 0
    for i in range(20):
        start = (i * 7919) % (total - 1000)
        params = (TARGET, row(start)[1], row(start + 1000)[1])
        begin = perf_counter()
        result = conn.execute(sql, params).fetchone()
        times.append((perf_counter() - begin) * 1000)
        assert result == (125,)
        rows += result[0]
    return round(median(times), 3), rows


def archive_probe(scratch, total, batch_size):
    path = scratch / "events.ndjson"
    fields = ("seq", "qpc", "process_id", "op", "path", "byte_count")
    t = perf_counter()
    with path.open("wb") as f:
        for base in range(0, total, batch_size):
            lines = (
                json.dumps(dict(zip(fields, row(i))), separators=(",", ":")) + "\n"
                for i in range(base, min(base + batch_size, total))
            )
            f.write("".join(lines).encode("utf-8"))
            f.flush()
            os.fsync(f.fileno())
    elapsed = perf_counter() - t
    conn = duckdb.connect(config={"memory_limit": "128MB", "threads": "1"})
    parquet = scratch / "sealed.parquet"
    start = perf_counter()
    source = str(path).replace("'", "''")
    target = str(parquet).replace("'", "''")
    conn.execute(
        f"""
        COPY (SELECT * FROM read_ndjson_auto('{source}'))
        TO '{target}' (FORMAT PARQUET, COMPRESSION ZSTD, ROW_GROUP_SIZE 16384)
        """
    )
    export_seconds = round(perf_counter() - start, 3)
    timings, count = timed_windows(
        conn, WINDOW.replace("FROM events", f"FROM read_parquet('{target}')"), total
    )
    assert conn.execute(f"SELECT count(*) FROM read_parquet('{target}')").fetchone() == (total,)
    conn.close()
    return {
        "ndjson_events_per_s_with_fsync": round(total / elapsed),
        "ndjson_bytes": file_bytes(path),
        "parquet_export_seconds": export_seconds,
        "parquet_bytes": file_bytes(parquet),
        "warm_parquet_window_median_ms": timings,
        "rows_counted_across_windows": count,
    }


def stalled_writer(path, filter_before_admission):
    slots, batch_size, attempted_batches = 8, 512, 24
    record = Struct("<QQIHH")
    pool = Queue(maxsize=slots)
    pending = Queue(maxsize=slots + 1)
    for _ in range(slots):
        pool.put(bytearray(record.size * batch_size))
    resume = Event()

    def write():
        with closing(connect(path)) as db:
            assert resume.wait(timeout=10)
            while (item := pending.get(timeout=10)) is not None:
                block, used = item
                payload = memoryview(block)[:used * record.size]
                selected = [
                    row(seq)
                    for seq, _, pid, _, _ in record.iter_unpack(payload)
                    if pid == TARGET
                ]
                with db:
                    db.executemany(INSERT, selected)
                pool.put(block, timeout=10)
            return db.execute("SELECT count(*) FROM events").get

    attempted = attempted_batches * batch_size
    admitted = rejected = dropped = 0
    block = None
    used = 0
    with ThreadPoolExecutor(max_workers=1) as executor:
        writer = executor.submit(write)
        for seq in range(attempted):
            _, qpc, pid, *_ = row(seq)
            if filter_before_admission and pid != TARGET:
                rejected += 1
                continue
            if block is None:
                try:
                    block = pool.get_nowait()
                except Empty:
                    dropped += 1
                    continue
            record.pack_into(block, used * record.size, seq, qpc, pid, 0, 0)
            admitted += 1
            used += 1
            if used == batch_size:
                pending.put_nowait((block, used))
                block = None
                used = 0
        if block is not None:
            pending.put_nowait((block, used))
        pending.put_nowait(None)
        resume.set()
        stored = writer.result(timeout=15)
    assert pool.qsize() == slots and attempted == admitted + rejected + dropped
    assert stored == (attempted if filter_before_admission else admitted) // 8
    assert dropped == (0 if filter_before_admission else attempted - slots * batch_size)
    return {
        "fixed_input_payload_bytes": slots * batch_size * record.size,
        "attempted": attempted,
        "filtered_out_before_admission": rejected,
        "admitted": admitted,
        "filtered_out_after_admission": admitted - stored,
        "dropped_at_admission": dropped,
        "stored": stored,
        "returned_buffers": pool.qsize(),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("scratch", type=Path, help="directory to create for generated datasets")
    parser.add_argument(
        "--sqlite-scale",
        action="store_true",
        help="run only the indexed SQLite workload with 2.5 million generated rows",
    )
    args = parser.parse_args()
    args.scratch.mkdir()
    total = 2_500_000 if args.sqlite_scale else 250_000
    batch_size = 2_048
    result = {
        "sqlite_version": apsw.sqlitelibversion(),
        "total_rows": total,
        "batch_rows": batch_size,
        "sqlite_full": load_sqlite(args.scratch / "active.db", total, batch_size, False),
    }
    if not args.sqlite_scale:
        result.update(
            duckdb_version=duckdb.__version__,
            sqlite_full_reader_pinned=load_sqlite(
                args.scratch / "pinned.db", total, batch_size, True
            ),
            sealed_archive=archive_probe(args.scratch, total, batch_size),
            stalled_writer_late_filter=stalled_writer(args.scratch / "late.db", False),
            stalled_writer_early_filter=stalled_writer(args.scratch / "early.db", True),
        )
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
