# Windows 突发采集与本地持久化调研

2026-11-23。研究阶段保留多条路线。GitHub [工单 #5](https://github.com/youling/TraceScalpel/issues/5) 跟踪持久化和爆发验证，[工单 #3](https://github.com/youling/TraceScalpel/issues/3) 跟踪原生 ETW 验收。

## 输入与容量

数量级假设为 30–60 分钟内约 1 亿条 IPC 观测记录，对应平均约 2.8 万–5.6 万条/秒。每条保留记录若占 100–200 B，1 亿条需要 10–20 GB，另计索引、会话缓冲、日志、原件和派生物。现场记录数、事件体积和峰值仍须按各传感器分别量测。压力测试把 IPC、文件/磁盘和网络同时纳入，并单独统计每路负载。

## 爆发处理

1. **源端缩小范围。** ETW 先用实际支持的 provider、level、keyword 和作用域。用非目标进程、目录和高频无关操作检验生效层。限定 ETW 缓冲并读取实际生效配置和丢失计数。采集回调优先使用已解码的头部字段做廉价筛选，深度归属和路径解析交给工作线程。 [ETW 过滤](https://learn.microsoft.com/en-us/windows/win32/api/evntprov/ns-evntprov-event_filter_descriptor) · [会话缓冲和丢失](https://learn.microsoft.com/en-us/windows/win32/api/evntrace/ns-evntrace-event_trace_properties)
2. **用预分配块吸收短峰值。** 回调复制最少所需的自有字节，提交尝试使用有界队列。解析、符号、数据库事务和磁盘等待放到回调外。按实际生产者拓扑比较 SPSC 和 MPMC。对需要延后判断的记录限制解码预算，并为关键标记留独立容量。记录每段队列高水位和积压时间。 [消费接口](https://learn.microsoft.com/en-us/windows/win32/etw/consuming-events) · [rtrb](https://docs.rs/rtrb/latest/rtrb/) · [ArrayQueue](https://docs.rs/crossbeam-queue/latest/crossbeam_queue/struct.ArrayQueue.html)
3. **分层写盘和回收。** 写入批次按条数、字节和最大等待时间共同限制。异步写的缓冲区持有到完成通知。提交、系统缓存中的写入和显式刷盘分别记录。会话上限包含 ETW、应用队列、数据库缓存和 WAL，磁盘预算还留出封存和派生空间。限制在线读事务生命周期。 [IOCP](https://learn.microsoft.com/en-us/windows/win32/fileio/i-o-completion-ports) · [Windows 文件缓存](https://learn.microsoft.com/en-us/windows/win32/fileio/file-caching)
4. **持续过载有明确终态。** 分开记录源损失、入队丢弃、按条件过滤、环形覆盖、持久化失败和已提交行，保留序号缺口与实际保留窗口。预算用尽时按选定策略停止或覆盖旧块。控制与丢失计数放在拥塞数据队列之外。

恒定短峰的应用缓冲下界为 `max(0, 入队字节率 − 实际排空字节率) × 峰值秒数`，再加现有积压、元数据和安全余量。例子为 200 MB/s 入队、80 MB/s 排空，持续 0.5 秒会多出约 60 MB。长时间入队快于排空时，有限内存最终会用尽。

CPU 先做 PID、事件编号等小字段的低延迟筛选。对于本地离线压缩或规则足够复杂且可整批处理的数据，再测 GPU。比较口径包含批次等待、数据打包、往返传输、持久化与设备内存。 [CUDA 数据传输建议](https://docs.nvidia.com/cuda/cuda-c-best-practices-guide/index.html#data-transfer-between-host-and-device)

## 存储与证据路线

| 路线 | 可能适合 | 选择前必须测 |
| --- | --- | --- |
| SQLite WAL 和索引 | 单写者合批，边采边查短时间窗。原件与归一化索引保留独立定位。SQLite 可交付源码是公有领域。 [WAL](https://www.sqlite.org/wal.html) · [许可](https://www.sqlite.org/copyright.html) | 长读持有快照、checkpoint 尾延迟、写放大、分段滚动和突然退出恢复。`journal_size_limit` 约束重置时的保留大小，活跃 WAL 还需单独限额。 [PRAGMA](https://sqlite.org/pragma.html#pragma_journal_size_limit) |
| 二进制追加段和稀疏索引 | 批量顺序写入，按段回收。可将字典、索引和压缩排到后续阶段。格式、校验、封存与截断恢复由项目负责。 | 用同等同步、检索与恢复口径对照 SQLite。比较是否减少端到端丢失和扰动，测文件句柄、索引重建及尾段故障。 |
| redb / RocksDB / ESENT | redb 是纯 Rust 有序事务键值候选。RocksDB 用 LSM 支持高写入，ESENT 是 Windows 原生事务存储对照。 [redb](https://github.com/cberner/redb) · [RocksDB](https://github.com/meta/rocksdb) · [ESENT](https://learn.microsoft.com/en-us/windows/win32/extensible-storage-engine/extensible-storage-engine) | 索引维护、读快照和磁盘预算。RocksDB 还要核对 WAL 同步、memtable 和 compaction 尾延迟。ESENT 测 FFI、文件及恢复维护成本。 [RocksDB WAL](https://github.com/meta/rocksdb/wiki/WAL-Performance) |
| 原生 ETL | WPR/ETW 复用原生录制与环形缓冲，WPA/PerfView 供复核。可先留有界原件，后置解码与查询。 [WPR 模式](https://learn.microsoft.com/en-us/windows-hardware/test/wpt/logging-mode) · [TraceEvent](https://github.com/microsoft/perfview/blob/main/documentation/TraceEvent/TraceEventProgrammersGuide.md) | profile 的有效范围、时钟、保存成本、原生丢失计数和可恢复性。 |
| DuckDB + Parquet 或 Perfetto | 对封存后的列式数据做查询与可移植交换。Perfetto 保持时间关联分析地位。 [Parquet 结构](https://parquet.apache.org/docs/file-format/) · [DuckDB 2.0.1](https://github.com/duckdb/duckdb/releases/tag/v2.0.1) · [Perfetto 缓冲语义](https://perfetto.dev/docs/concepts/buffers) | 压缩、读写额外空间、全窗查询、行组裁剪及损失语义保真。 |

归一化数据保留录制 ID、来源、序号、原始整数 QPC、时钟域及原件定位。热库只建试验实际需要的索引，先验证目标进程加时间窗。滚动保留比较按段删除与逐行删除的成本。原始 ETL、归一化库和分析导出分别核算数据量与保留期限。SQLite 和 DuckDB 暂用于实验，各组件及依赖仍按各自上游许可发布。

## 实测第一轮

Windows build 26300，ReFS，Python 3.14.4，SQLite 3.54.1（APSW 3.54.1.0），DuckDB 2.0.1。数据是合成文件事件，固定近似路径结构和 8 个进程。SQLite 使用 WAL、`synchronous=FULL`、一个进程时间窗索引、每批 2,048 行。

| 场景 | 结果 |
| --- | --- |
| 25 万行 SQLite，重复两轮 | 38.3–38.6 万行/秒，批事务 p95 10.31–10.79 ms，主库 23.7 MiB，采样到的最大 WAL 4.2 MiB |
| 250 万行 SQLite，单轮 | 37.2 万行/秒，批事务 p95 11.26 ms，主库 237.8 MiB，采样到的最大 WAL 4.3 MiB |
| 25 万行且读事务固定，重复两轮 | WAL 达 39.9 MiB。持有读事务时 TRUNCATE checkpoint 返回 busy。关闭读事务后 WAL 截断到零 |
| 25 万行封存导出，重复两轮 | NDJSON 为 32.7 MiB，Parquet ZSTD 为 0.78 MiB。数据重复度有利于压缩，结果只代表本样本 |
| 写入暂停，12,288 条输入，96 KiB 固定记录池 | 入队后按 PID 筛选保留 512 条目标事件，入队前按 PID 筛选保留全部 1,536 条目标事件。两轮中每条记录都有去向计数，八块缓冲均回池 |

这些数字测量数据库和合成记录池。原生 ETW、实际 IPC/文件/网络负载、源端过滤成本、总内存、满盘和断电恢复仍待测。SQLite 行速率包含行生成与批提交，窗口查询为热缓存计数查询。NDJSON `fsync` 与 SQLite 事务的恢复语义不同。Parquet 结果来自后置转换。

```powershell
python -m venv .venv
.\.venv\Scripts\python -m pip install apsw==3.54.1.0 duckdb==2.0.1
.\.venv\Scripts\python validation/storage_probe.py storage-run
.\.venv\Scripts\python validation/storage_probe.py storage-scale --sqlite-scale
```

## 下一轮可淘汰候选的验证

| 验证 | 对照与淘汰条件 |
| --- | --- |
| **Windows 文件与事件归属** | 让两个自有进程生成编号、创建、读写、重命名和删除。比较 ETW 实时与 ETL 回放，配合 Procmon 对账。用 `ReadDirectoryChangesW` 验目录通知与溢出，支持的卷上用 USN 验变更补扫。用读操作与跨目录重命名揭示各来源的盲区。 [目录通知](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-readdirectorychangesw) · [USN](https://learn.microsoft.com/en-us/windows/win32/fileio/change-journals) · [Procmon](https://learn.microsoft.com/en-us/sysinternals/downloads/procmon) |
| **Rust 入口与拓扑** | 同一批自有 trace 比较 `windows-sys` 直连和 ferrisetw，保留原始 QPC、TDH 解码覆盖和丢失计数。按实际线程拓扑测 SPSC 与 MPMC。回调 p99、源端损失、内存、CPU 和清退都计入。丢失无法解释或饱和时回调等待磁盘的方案出局。 [windows-rs](https://github.com/microsoft/windows-rs) · [ferrisetw](https://github.com/n4r1b/ferrisetw) |
| **多源持续压测与短峰** | 初始试验点设为合计 10 万条/秒持续 30 分钟，并注入 50 万和 100 万条/秒的有界短峰。改变 IPC、网络、文件比例、记录大小与目标命中率。加入慢读、写盘停顿和有限容量测试卷。对齐发出、采集、筛选、入队、提交与恢复的序号和计数；同时量吞吐、尾延迟、CPU、峰值内存和磁盘增量。数字是探索用的输入档位。 |
| **存储与写入** | 比较有界 ETL 后置索引、SQLite 直接入库、带校验与稀疏索引的二进制段。相同刷盘频率、查询和保留窗口下对照单写者同步 I/O、overlapped/IOCP。只有在目标系统支持且写提交成为瓶颈时再比 I/O Ring。测试进程中止与隔离环境的恢复、句柄回收和空间封存。 [I/O Ring 能力查询](https://learn.microsoft.com/en-us/windows/win32/api/ioringapi/nf-ioringapi-queryioringcapabilities) |

还可验证低成本摘要环和短窗细节采集的组合。用同一 QPC 时钟衡量历史保留长度、触发延迟与细节空窗。滚动及触发设计参考 Stuttometer，GPL 源码保持独立。 [Stuttometer](https://github.com/xdr0p/stuttometer)
