# TraceScalpel

High-precision Windows tracing for AI agents.

## Windows prototype

TraceScalpel starts as a headless executable. Its current fixture measures five
operations it performs itself: SHA-256 hashing of the executable file, an
application marker, an anonymous pipe exchange, a localhost TCP exchange, and a
temporary file write, read, and cleanup. Each event has a process ID, a
`fixture_self_report` source, and start and end ticks on one
`QueryPerformanceCounter` clock. The result also contains the counter
frequency and a QPC bracket around a precise UTC FILETIME sample. Time values
are decimal strings so agents can decode them without floating-point rounding.
These intervals measure the operations in the fixture. Native ETW observation
and timestamp alignment are tracked in [issue #3](https://github.com/youling/TraceScalpel/issues/3).

Install [Rust with rustup](https://rustup.rs/) and the Windows MSVC build tools,
then run the following in PowerShell:

```powershell
cargo build --locked

@(
  '{"jsonrpc":"2.0","id":1,"method":"camera.capabilities"}'
  '{"jsonrpc":"2.0","id":2,"method":"camera.run_fixture"}'
) | .\target\debug\tracescalpel.exe
```

Send one UTF-8 JSON-RPC 2.0 request per line. The executable returns one
newline-delimited response per request with an ID and keeps diagnostics on
stderr. Requests are limited to 65,536 bytes before the line feed. Omit
`params` or pass an empty object. Batches are unsupported. For a fixture run
whose result you need, include an ID and wait for its response before ending
the process. A successful result contains `schema_version`, `capture_id`,
`clock`, and `events`. A fixture failure returns error `-32603` with
`data.stage`, `data.kind`, and `data.os_error`.

The fixture uses the account's temporary directory by default. Set
`TRACESCALPEL_FIXTURE_DIR` to an existing writable directory when launching the
process to choose its scratch location.

Run the Windows subprocess integration tests with `cargo test --locked`.

## Design and verification

- [Architecture and evidence flow](docs/ARCHITECTURE.md)
- [Tracing tool and licensing research](docs/RESEARCH.md)
- [Windows capture, clocks and agent protocol research](docs/WINDOWS-FRONTIER.md)
- [Validation plan and synthetic checks](docs/VALIDATION.md)
- [Milestones](https://github.com/youling/TraceScalpel/milestones)

## Licensing

TraceScalpel's original project material is available under the
[PolyForm Noncommercial License 1.0.0](LICENSE). Commercial use is available
under a separate written license from the applicable copyright holders.
Third-party components remain under their respective upstream licenses.

Required Notice: Copyright 2026 youling and the TraceScalpel contributors.
