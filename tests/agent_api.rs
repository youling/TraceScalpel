#![cfg(windows)]

use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
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
