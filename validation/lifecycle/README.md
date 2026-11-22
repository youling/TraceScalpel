# R6 生命周期离线合成验证

此 crate 用有限的合成回执序列验证 Stop 请求、匹配确认、排空和封存的顺序。同一个 Stop 只生效一次。不同请求、错序回执和其他会话被记录为无效或不相关。缺失或降级的确认记入轨迹，流程保持等待匹配确认。进程退出见证与封存独立，明确见证保留退出码，观察超时记为 `Unknown`，仍在观察时记为 `Pending`。

在本目录运行。构建输出写入仓库外的临时目录。

```powershell
$env:CARGO_TARGET_DIR = Join-Path $env:TEMP 'r6-lifecycle-cargo-target'
cargo test --offline --locked
cargo fmt --check
```

测试输入均为合成回执，结果只验证回执顺序和状态判定。
