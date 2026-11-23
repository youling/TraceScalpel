mod etw;
mod evidence;
mod fixture;
mod startup;
mod storage;
#[cfg(test)]
mod survey;
mod timebase;

use serde::de::DeserializeOwned;
use serde_json::{Value, json, value::RawValue};
use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};

pub use startup::capture_preflight;

// Bound memory use even when a peer sends a line without a newline.
const MAX_LINE_BYTES: usize = 64 * 1024;

enum Line {
    Data(Vec<u8>),
    TooLong,
    Eof,
}

fn read_line<R: BufRead>(reader: &mut R) -> io::Result<Line> {
    let mut line = Vec::new();
    let mut too_long = false;
    let mut saw_bytes = false;
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok(if !saw_bytes {
                Line::Eof
            } else if too_long {
                Line::TooLong
            } else {
                Line::Data(line)
            });
        }
        let end = buf.iter().position(|&b| b == b'\n');
        let count = end.unwrap_or(buf.len());
        saw_bytes = true;
        if !too_long {
            if line.len() + count > MAX_LINE_BYTES {
                too_long = true;
                line.clear();
            } else {
                line.extend_from_slice(&buf[..count]);
            }
        }
        let consumed = count + usize::from(end.is_some());
        reader.consume(consumed);
        if end.is_some() {
            return Ok(if too_long {
                Line::TooLong
            } else {
                Line::Data(line)
            });
        }
    }
}

fn error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "error":{"code":code, "message":message}})
}

fn evidence_response<T: DeserializeOwned>(
    id: Value,
    params: Option<&&RawValue>,
    call: impl FnOnce(T) -> io::Result<Value>,
) -> Value {
    let Some(request) = params.and_then(|value| serde_json::from_str::<T>(value.get()).ok()) else {
        return error(id, -32602, "Invalid evidence parameters");
    };
    operation_response(id, call(request))
}

fn operation_response(id: Value, outcome: io::Result<Value>) -> Value {
    match outcome {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(failure) => {
            let code = if failure.kind() == io::ErrorKind::InvalidInput {
                -32602
            } else {
                -32010
            };
            json!({
                "jsonrpc": "2.0", "id": id, "error": {
                    "code": code,
                    "message": if code == -32602 { "Invalid evidence parameters" } else { "Evidence operation failed" },
                    "data": {
                        "kind": format!("{:?}", failure.kind()),
                        "os_error": failure.raw_os_error(),
                        "detail": failure.to_string()
                    }
                }
            })
        }
    }
}

fn handle(line: &[u8]) -> Option<Value> {
    let request: &RawValue = match serde_json::from_slice(line) {
        Ok(value) => value,
        Err(_) => return Some(error(Value::Null, -32700, "Parse error")),
    };
    let Ok(object) = serde_json::from_str::<BTreeMap<String, &RawValue>>(request.get()) else {
        return Some(error(Value::Null, -32600, "Invalid Request"));
    };
    let Some(method) = object
        .get("method")
        .and_then(|method| serde_json::from_str::<String>(method.get()).ok())
    else {
        return Some(error(Value::Null, -32600, "Invalid Request"));
    };
    let id = object.get("id");
    let version = object
        .get("jsonrpc")
        .and_then(|version| serde_json::from_str::<String>(version.get()).ok());
    // Validate the wire type before converting to Value. With arbitrary_precision,
    // Value also recognizes its internal number representation in JSON objects.
    if version.as_deref() != Some("2.0")
        || id.is_some_and(|id| {
            !matches!(
                id.get().as_bytes().first(),
                Some(b'n' | b'"' | b'-' | b'0'..=b'9')
            )
        })
    {
        return Some(error(Value::Null, -32600, "Invalid Request"));
    }
    let respond = id.is_some();
    let id = match id.map(|id| serde_json::from_str::<Value>(id.get())) {
        None => Value::Null,
        Some(Ok(id)) => id,
        Some(Err(_)) => return Some(error(Value::Null, -32600, "Invalid Request")),
    };
    let valid_params = object.get("params").is_none_or(|params| {
        let params = params.get();
        params
            .strip_prefix('{')
            .and_then(|contents| contents.strip_suffix('}'))
            .is_some_and(|contents| contents.trim().is_empty())
    });
    let response = match (method.as_str(), valid_params) {
        ("camera.capabilities", true) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "version": env!("CARGO_PKG_VERSION"),
                "methods": [
                    "camera.capabilities", "camera.run_fixture", "camera.record_fixture",
                    "camera.verify_capture", "camera.query_events", "camera.inspect_storage"
                ],
                "storage_backends": ["sqlite", "segments", "redb"],
                "fixture_sources": ["fixture_self_report", "etw_owned"],
                "evidence_root_env": "TRACESCALPEL_EVIDENCE_DIR",
                "transport": "JSON-RPC 2.0 over stdio, one request per line",
                "max_request_bytes": MAX_LINE_BYTES,
                "limitations": [
                    "Single requests only; batch requests are unsupported",
                    "Fixture only; live camera capture is unsupported",
                    "No arbitrary capture targets, shell execution, or socket transport"
                ],
            }
        }),
        ("camera.run_fixture", true) => match fixture::run() {
            Ok(capture) => json!({"jsonrpc": "2.0", "id": id, "result": capture}),
            Err(failure) => {
                eprintln!("camera fixture [{}]: {}", failure.stage, failure.cause);
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32603,
                        "message": "Fixture failed",
                        "data": {
                            "stage": failure.stage,
                            "kind": format!("{:?}", failure.cause.kind()),
                            "os_error": failure.cause.raw_os_error(),
                        },
                    },
                })
            }
        },
        ("camera.capabilities" | "camera.run_fixture", false) => error(
            id,
            -32602,
            "Invalid params. Omit params or provide an empty object.",
        ),
        ("camera.record_fixture", _) => {
            evidence_response(id, object.get("params"), evidence::record)
        }
        ("camera.verify_capture", _) => {
            evidence_response(id, object.get("params"), evidence::verify)
        }
        ("camera.query_events", _) => evidence_response(id, object.get("params"), evidence::query),
        ("camera.inspect_storage", true) => operation_response(
            id,
            startup::inspect().and_then(|status| Ok(serde_json::to_value(status)?)),
        ),
        ("camera.inspect_storage", false) => error(id, -32602, "Invalid params"),
        _ => error(id, -32601, "Method not found"),
    };
    respond.then_some(response)
}

pub fn serve() -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    loop {
        let response = match read_line(&mut input)? {
            Line::Eof => break,
            Line::TooLong => Some(error(
                Value::Null,
                -32600,
                "Request exceeds maximum line length",
            )),
            Line::Data(line) => handle(&line),
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut output, &response)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
    Ok(())
}
