# 验证计划

固定窗在源时钟上采用 `[start, deadline)`。迟到的窗内事件仍可纳入，时钟域无法对齐的事件标为未知。STOP 请求、实际停采确认、排空、封存和退出分别记录，重复 STOP 以同一请求身份核对。确认缺失时保留证据缺口。READY 需要已知答案事件通过完整链路。

## 已迁入的离线用例

| 用例 | 命令 | 判据 |
| --- | --- | --- |
| Agent 子进程和自有操作 | `cargo test --locked` | 有界输入、错误恢复、时钟采样和五类自报告事件 |
| 固定源时钟窗 | `python validation/verify_window.py validation/fixed_window.json` | 窗口两端、迟到事件、未知时钟和大整数精度 |
| STOP、排空、封存和退出 | `cargo test --locked --manifest-path validation/lifecycle/Cargo.toml` | 重复停止、乱序和跨会话回执，退出见证独立于封存 |

## 0.2 原生 ETW

- 用一个自有子进程生成有编号的 marker 和唯一临时文件。同一窗口运行另一自有进程写不同文件作为归属负控。记录 Windows build、采集配置、进程启动证据和原件定位。
- 明示 ETW 会话时钟与消费时钟，保留原始计数和 QPC/UTC 校准区间。用多次采样报告分布和窗口误差。收到的事件范围取决于 provider 的语义，先核对操作和事件对应关系。
- 分开报告来源发出数、消费数、导出数、时间窗循环覆盖、ETW 会话丢失及消费者缺口。无法取到的计数标识为未知。权限失败保留原始错误和未覆盖面。
- 先确认 CI 实际可用的管理员权限与 provider，再决定可运行的端到端测试。本机和 CI 分别报告结果。比较采集开关两组的 fixture 延迟、CPU、峰值内存和输出吞吐。

[ETW 会话计数](https://learn.microsoft.com/en-us/windows/win32/api/evntrace/ns-evntrace-event_trace_properties) · [原始时间戳](https://learn.microsoft.com/en-us/windows/win32/api/evntrace/ns-evntrace-event_trace_logfilew) · [QPC](https://learn.microsoft.com/en-us/windows/win32/sysinfo/acquiring-high-resolution-time-stamps) · [ETW 基线工单](https://github.com/youling/TraceScalpel/issues/3)
