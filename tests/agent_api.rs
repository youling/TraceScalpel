#![cfg(windows)]

use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        fs::create_dir_all(&root).unwrap();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = root.join(format!("camera-agent-test-{}-{unique}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn decimal(value: &Value) -> u128 {
    let s = value.as_str().expect("decimal string");
    assert!(
        !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()),
        "invalid decimal: {s}"
    );
    s.parse().expect("decimal within u128")
}

#[test]
fn stdio_protocol_and_owned_fixture_end_to_end() {
    let scratch = Scratch::new();
    let mut child = OwnedChild(
        Command::new(env!("CARGO_BIN_EXE_tracescalpel"))
            .env("TRACESCALPEL_FIXTURE_DIR", &scratch.0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn camera"),
    );
    let stdout = child.0.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut input = child.0.stdin.take().unwrap();
    writeln!(input, "{{bad json").unwrap();
    writeln!(input, "{{\"jsonrpc\":\"2.0\",\"id\":1}} ").unwrap();
    writeln!(input, "{}", "x".repeat(65537)).unwrap();
    let bad_id = r#"{"jsonrpc":"2.0","id":{"$serde_json::private::Number":"7"},"method":"camera.capabilities"}"#;
    writeln!(input, "{bad_id}").unwrap();
    let notification = r#"{"jsonrpc":"2.0","method":"camera.capabilities"}"#;
    for _ in 0..2 {
        writeln!(input, "{notification}").unwrap();
    }
    let capabilities = r#"{"jsonrpc":"2.0","id":9007199254740993123456789,"method":"camera.capabilities","params":{ }}"#;
    let pad = " ".repeat(65536 - capabilities.len());
    writeln!(input, "{capabilities}{pad}").unwrap();
    writeln!(input, "{capabilities}").unwrap();
    writeln!(input, "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"camera.run_fixture\",\"params\":{{\"unexpected\":true}}}}").unwrap();
    writeln!(
        input,
        "{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"camera.run_fixture\",\"params\":{{}}}}"
    )
    .unwrap();
    drop(input);
    let mut rows = Vec::new();
    loop {
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(line) => rows.push(serde_json::from_str::<Value>(&line.unwrap()).unwrap()),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(e) => panic!("camera output timed out: {e}"),
        }
    }
    assert!(child.0.wait().unwrap().success());
    assert_eq!(rows.len(), 8, "notifications must not respond: {rows:?}");
    for (i, code) in [-32700, -32600, -32600, -32600].into_iter().enumerate() {
        assert_eq!(rows[i]["error"]["code"], code);
        assert!(rows[i]["id"].is_null());
    }
    assert_eq!(rows[4], rows[5], "repeated IDs identify separate calls");
    assert_eq!(rows[4]["id"].to_string(), "9007199254740993123456789");
    assert!(
        rows[4]["result"]["methods"]
            .as_array()
            .unwrap()
            .contains(&json!("camera.run_fixture"))
    );
    assert_eq!(rows[6]["id"], 2);
    assert_eq!(rows[6]["error"]["code"], -32602);
    assert_eq!(rows[7]["id"], 3);
    assert!(
        rows[7].get("error").is_none(),
        "fixture failed: {}",
        rows[7]
    );
    let result = &rows[7]["result"];
    assert!(!result["capture_id"].as_str().unwrap().is_empty());
    assert!(!result["schema_version"].as_str().unwrap().is_empty());
    assert!(decimal(&result["clock"]["frequency_hz"]) > 0);
    let cal = &result["clock"]["utc_calibration"];
    assert!(decimal(&cal["qpc_before"]) <= decimal(&cal["qpc_after"]));
    assert!(decimal(&cal["filetime_100ns_since_1601_utc"]) > 0);
    let events = result["events"].as_array().unwrap();
    assert_eq!(events.len(), 5);
    let mut kinds: Vec<_> = events
        .iter()
        .map(|e| {
            assert!(decimal(&e["qpc_start"]) <= decimal(&e["qpc_end"]));
            assert_eq!(e["source"], "fixture_self_report");
            assert_eq!(e["process_id"], child.0.id());
            e["kind"].as_str().unwrap()
        })
        .collect();
    kinds.sort_unstable();
    assert_eq!(
        kinds,
        [
            "anonymous_pipe_exchange",
            "dynamic_marker",
            "executable_fingerprint",
            "loopback_tcp_exchange",
            "temporary_file_exchange"
        ]
    );
    assert_eq!(
        fs::read_dir(&scratch.0).unwrap().count(),
        0,
        "fixture must clean its owned files"
    );
}

struct EvidenceClient {
    _child: OwnedChild,
    input: ChildStdin,
    output: mpsc::Receiver<String>,
}

impl EvidenceClient {
    fn start(root: &Scratch) -> Self {
        let mut child = OwnedChild(
            Command::new(env!("CARGO_BIN_EXE_tracescalpel"))
                .env("TRACESCALPEL_EVIDENCE_DIR", &root.0)
                .env("TRACESCALPEL_FIXTURE_DIR", &root.0)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let input = child.0.stdin.take().unwrap();
        let stdout = child.0.stdout.take().unwrap();
        let (sender, output) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            _child: child,
            input,
            output,
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        writeln!(
            self.input,
            "{}",
            json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })
        )
        .unwrap();
        let line = self.output.recv_timeout(Duration::from_secs(20)).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["id"], 1);
        reply
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let reply = self.call(method, params);
        assert!(reply.get("error").is_none(), "{method}: {reply}");
        reply["result"].clone()
    }
}

#[test]
fn native_etl_evidence_reopens_and_pages_across_all_backends() {
    let scratch = Scratch::new();
    let mut recorder = EvidenceClient::start(&scratch);
    let storage = recorder.ok("camera.inspect_storage", json!({}));
    assert!(storage["token_elevated"].is_boolean());
    let volume = &storage["volume"];
    assert_eq!(volume["drive_type"], "fixed");
    assert!(
        volume["filesystem"]
            .as_str()
            .is_some_and(|name| !name.is_empty())
    );
    assert!(
        volume["allocation_unit_bytes"]
            .as_u64()
            .is_some_and(|size| size > 0)
    );
    let sealed: Vec<Value> = ["sqlite", "segments", "redb"]
        .into_iter()
        .map(|backend| {
            let manifest = recorder.ok(
                "camera.record_fixture",
                json!({ "backend": backend, "source": "etw_owned" }),
            );
            assert_eq!(manifest["backend"], backend);
            assert_eq!(manifest["rows"], 64);
            assert_eq!(manifest["source"], "etw_owned");
            assert_eq!(manifest["raw"]["filename"], "source.etl");
            assert_eq!(manifest["coverage_complete"], true);
            let anchors = manifest["utc_anchors"].as_array().unwrap();
            assert_eq!(anchors.len(), 2);
            for anchor in anchors {
                assert_eq!(anchor["frequency_hz"], manifest["frequency_hz"]);
                let sample = &anchor["utc_calibration"];
                assert!(decimal(&sample["qpc_before"]) <= decimal(&sample["qpc_after"]));
                assert!(decimal(&sample["filetime_100ns_since_1601_utc"]) > 0);
            }
            manifest
        })
        .collect();
    drop(recorder);

    let mut reader = EvidenceClient::start(&scratch);
    for manifest in sealed {
        let id = manifest["capture_id"].as_str().unwrap();
        let params = json!({ "capture_id": id });
        assert_eq!(reader.ok("camera.verify_capture", params.clone()), manifest);
        let mut cursor = Value::Null;
        let mut seen = Vec::new();
        for _ in 0..4 {
            let page = reader.ok(
                "camera.query_events",
                json!({ "capture_id": id, "limit": 17, "cursor": cursor }),
            );
            assert_eq!(page["coverage"]["status"], "complete");
            assert_eq!(page["coverage"]["native_loss"]["session_events_lost"], 0);
            for event in page["events"].as_array().unwrap() {
                assert_eq!(event["locator"]["sha256"], manifest["raw"]["sha256"]);
                let qpc = decimal(&event["qpc"]);
                assert!(decimal(&event["details"]["emission_qpc_before"]) <= qpc);
                assert!(qpc <= decimal(&event["details"]["emission_qpc_after"]));
                seen.push(decimal(&event["sequence"]));
            }
            cursor = page["next_cursor"].clone();
        }
        assert!(cursor.is_null());
        assert_eq!(seen, (1..=64).collect::<Vec<_>>());

        let empty = reader.ok(
            "camera.query_events",
            json!({ "capture_id": id, "process_id": 0 }),
        );
        assert_eq!(empty["events"], json!([]));
        assert_eq!(empty["coverage"]["status"], "complete");
        let early = (decimal(&manifest["qpc_start"]) - 1).to_string();
        let partial = reader.ok(
            "camera.query_events",
            json!({ "capture_id": id, "process_id": 0, "qpc_start": early }),
        );
        assert_eq!(partial["events"], json!([]));
        assert_eq!(partial["coverage"]["status"], "partial");

        let first = reader.ok(
            "camera.query_events",
            json!({ "capture_id": id, "limit": 1 }),
        );
        let stale = reader.call(
            "camera.query_events",
            json!({ "capture_id": id, "process_id": 0, "cursor": first["next_cursor"] }),
        );
        assert_eq!(stale["error"]["code"], -32602);
        assert_eq!(reader.ok("camera.verify_capture", params.clone()), manifest);
        if manifest["backend"] == "sqlite" {
            let wal = scratch.0.join(id).join("events.sqlite-wal");
            fs::write(&wal, b"unexpected WAL").unwrap();
            let unexpected_wal = reader.call("camera.query_events", params.clone());
            assert_eq!(unexpected_wal["error"]["data"]["kind"], "InvalidData");
            fs::remove_file(wal).unwrap();
        }

        let etl = scratch.0.join(id).join("source.etl");
        fs::OpenOptions::new()
            .append(true)
            .open(etl)
            .unwrap()
            .write_all(b"changed")
            .unwrap();
        let unverified = reader.call("camera.verify_capture", params);
        assert_eq!(unverified["error"]["data"]["kind"], "InvalidData");
        let unqueryable = reader.call("camera.query_events", json!({ "capture_id": id }));
        assert_eq!(unqueryable["error"]["data"]["kind"], "InvalidData");
    }
}
