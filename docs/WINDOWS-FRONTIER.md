# Windows 软件高速摄影机选型调研

2026-11-23 补充研究。TraceScalpel 需要在一个有界窗口内同时取得可核对的静态身份、运行事件、IPC、网络和文件证据。其他 Agent 通过可版本化接口调用。Linux 和 macOS 后续按平台独立开发。真实 provider 覆盖、运行开销、退出行为和宿主兼容性仍需验证。 [综合调研](RESEARCH.md)

## 共同的取证模型

一次取证应声明问题、目标进程实例、选定证据面、允许窗口和资源预算。结果保留每个观测面的时钟、原始入口、定位符、覆盖、丢失与未知。主因果图只消费明确关联的片段，原始 ETL、PML 和应用自报各自保留。盲区使某个问题不可裁决时，在报告中直接指出缺失面。

| 证据面 | 首选候选 | 需要证明的边界 |
| --- | --- | --- |
| 静态身份 | PE 文件 SHA-256、版本和签名，CodeView 标识/PDB，配合 ETW image load/unload/rundown | 静态分析可以在录制前后离线完成，靠实例、模块装载区间、路径与文件身份对齐。在文件可被替换时，事后路径哈希只能证明读到的那份文件。PID 需要进程开始时间及主机/boot 范围。PDB 用 signature/age 类标识验证匹配。 [PE 规范](https://learn.microsoft.com/en-us/windows/win32/debug/pe-format) · [ETW Image_Load](https://learn.microsoft.com/en-us/windows/win32/etw/image-load) · [DIA PDB 验证](https://learn.microsoft.com/en-us/visualstudio/debugger/debug-interface-access/idiadatasource-loaddatafromcodeviewinfo?view=visualstudio) |
| 动态层 | ETW 系统/用户 provider，必要时短窗 stackwalk，自有应用用 TraceLogging/Activity ID 标记 | 进程、线程、映像、调度和采样栈可回答资源与执行归属。具体函数参数、业务语义只由具备这类观察能力的源给出。WinDbg TTD 可作单进程、可接受开销的按需显微镜。DynamoRIO/Frida 保留为更深的适配器候选，开启前需独立确认样本、用途、扰动和停止行为。 [ETW](https://learn.microsoft.com/en-us/windows/win32/etw/about-event-tracing) · [TraceLogging](https://learn.microsoft.com/en-us/windows/win32/tracelogging/trace-logging-portal) · [TTD](https://learn.microsoft.com/en-us/windows-hardware/drivers/debuggercmds/time-travel-debugging-overview) · [运行时候选复用矩阵](RESEARCH.md) |
| IPC | 命名管道与自身 provider 分别观察。RPC/COM、ALPC、共享内存按目标的实际传输和可验证来源拆开 | 同名管道可有多个实例。RPC 支持本机 RPC、命名管道和 TCP 等传输。管道名、COM 接口或 PID 无法单独识别一次请求。无法证实的 IPC 面登记 UNKNOWN。 [命名管道](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipes) · [RPC 协议序列](https://learn.microsoft.com/en-us/windows/win32/rpc/protocol-sequence-constants) |
| 网络 | ETW 连接生命周期与进程作为元数据主候选。Pktmon 用于需要组件路径或包/丢失原因的独立窗口 | 五元组、时间与同一进程实例是关联线索。回环、代理代发、端口复用、断续连接和多组件重复需要负控。Pktmon 启动选项明确区分报文长度与日志模式。保留组件、方向、原 ETL 和计数。加密链路的应用内容单独按已授权来源处理。常驻驱动、全量包及内容采集暂列候选。 [Windows 网络 tracing 架构](https://learn.microsoft.com/en-us/windows/win32/ndf/network-tracing-in-windows-7-architecture) · [Pktmon 选项](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/pktmon-start) |
| 文件、注册表和 SQLite | ETW 文件/注册表/磁盘事件，Procmon 用于有界独立对照，SQLite 来源提供已授权的事务语义 | 文件写、WAL 文件变长、WAL 有效 commit frame、SQL API 返回和业务提交是不同证据。WAL 需验证字节、校验、salt 与代次。Procmon 保留 PML 原件，比较过滤范围、栈与扰动等配置。 [Procmon](https://learn.microsoft.com/en-us/sysinternals/downloads/procmon) · [SQLite WAL 格式](https://www.sqlite.org/walformat.html) |
| 分析与交换 | 原始证据旁建立带来源的事件信封，派生 Perfetto/SQL；已有业务 span 可作为另一个源 | 源时钟和映射误差显式记录。对无法映射或区间重叠的事件保留顺序未知。Perfetto 导出附工具、配置和原始 locator。导入成功的证据不能自动提升原采集覆盖。 [Perfetto 时钟](https://perfetto.dev/docs/concepts/clock-sync) · [TrackEvent](https://perfetto.dev/docs/instrumentation/track-events) · [丢失统计](https://perfetto.dev/docs/analysis/sql-stats) |

## 时间精度和本地收集

1. **同机原始量。** QPC 用于计算本机时间间隔。ETW 会话选择 `Wnode.ClientContext=1` 时关联 QPC。消费者启用 `PROCESS_TRACE_MODE_RAW_TIMESTAMP` 并检查 `TRACE_LOGFILE_HEADER.PerfFreq`，因为默认消费路径可得到已转换时间。记录 Windows build、boot、频率、原始 tick、线程/CPU、provider 及事件版本。序列化用整数或十进制字符串。微软文档提醒不同线程相差约一个 tick 时排序存在歧义。 [QPC](https://learn.microsoft.com/en-us/windows/win32/sysinfo/acquiring-high-resolution-time-stamps) · [ETW 选钟](https://learn.microsoft.com/en-us/windows/win32/etw/wnode-header) · [原始消费](https://learn.microsoft.com/en-us/windows/win32/api/evntrace/ns-evntrace-event_trace_logfilew)
2. **墙钟锚。** 记 `QPC_before → PreciseFileTime → QPC_after`，把前后界、所用 API、频率和重复采样留在原件。括号宽度只量化采样边界。UTC 同步误差、时钟调整、休眠和外部设备时间需要另记。原始 QPC 用来关联同机事件，UTC 用来定位到日历时间。 [PreciseFileTime](https://learn.microsoft.com/en-us/windows/win32/api/sysinfoapi/nf-sysinfoapi-getsystemtimepreciseasfiletime)
3. **量测真正发生的地方。** 区分源事件时间、消费者到达、解码/导出时刻和业务语义完成。用独立 fixture 的源序号、开始/结束标记分别核对。报告可归因的事件数、p50/p95/p99 偏差、不可对齐与不可排序比例。高精度 timestamp 的位数仅说明表示能力。
4. **覆盖与停采。** 在本机按 OS build、provider/event/schema、权限和配置逐项实测。PID scope 对 provider 适用性有限，已启动与新启动实例、内核 provider 都需验证。生产环境对不能在来源限制的事件，须明确可接受的采集范围和归属过滤。 [ETW 过滤描述](https://learn.microsoft.com/en-us/windows/win32/api/evntprov/ns-evntprov-event_filter_descriptor)
5. **丢失与回收。** 分开记 ETW `EventsLost`、`LogBuffersLost`、`RealTimeBuffersLost`、循环保留覆盖、消费者丢弃、解码缺口和未启动面。只 stop 自有会话，分别留请求、确认、实际停采、flush/drain、封存和退出见证。测 R1/R6 的重复 Stop、卡住消费、超时、死进程恢复。 [会话属性和计数](https://learn.microsoft.com/en-us/windows/win32/etw/event-trace-properties) · [ControlTrace](https://learn.microsoft.com/en-us/windows/win32/api/evntrace/nf-evntrace-controltracew) · [WPR 内存/文件模式](https://learn.microsoft.com/en-us/windows-hardware/test/wpt/logging-mode)

## 管线与主语言候选

| 候选 | 成立条件 | 未决成本 |
| --- | --- | --- |
| **A．WPR/ETW 文件 + 薄控制器 + 离线关联** | 原生 profile 能覆盖目标问题，能清楚取得实际窗口、时钟、丢失和 ETL。优先用这条路线测原生基线。WPA 用于查看原件，Procmon 针对自己的覆盖做独立旁证。 | 细粒度实时触发、源端窄过滤和精确排空能否由 WPR 控制，要看 profile 和 Windows 实测。 [WPR 命令](https://learn.microsoft.com/en-us/windows-hardware/test/wpt/wpr-command-line-options) |
| **B．owned ETW consumer + 原 ETL + 结构化时间轴** | A 缺实时选择、定点触发或可验丢失时适用。先做最小 provider 和双重复核，逐步扩控制。 | 会话 ownership、回调异常、消费者反压、文件索引、异常退出和专用 decoder 都要验证。 [微软 ETW 消费与 TDH](https://learn.microsoft.com/en-us/windows/desktop/ETW/consuming-events) |
| **C．原生底座 + 受限的定点运行时探针** | 只有前两条留下决定性的业务语义空白时启用。与原生窗口同钟定位，使用独立正负控量化泄漏、退出和盲区。 | 深层探针的扰动、盲区和权限可能超过观测收益。Stuttometer 提供滚动与触发架构参照，GPL 代码沿原裁决独立。 [架构参照与适配器来源](RESEARCH.md) |

| 主语言候选 | 优势 | 用相同样本比较 |
| --- | --- | --- |
| **Rust** | 当前首选候选。控制、记录整理、Agent API 可用一套语言。windows-rs 提供 Win32 的高层与底层绑定。句柄与线程边界易集中审查。 | 直接 Win32/TDH 与 ferrisetw 包装分别核 raw timestamps、schema 完整度、loss、回调/句柄回收、部署体积与维护成本。Rust 绑定层仍需要 FFI 正确性。 [windows-rs](https://github.com/microsoft/windows-rs) · [ferrisetw](https://github.com/n4r1b/ferrisetw) |
| **C# / TraceEvent** | 成熟的 ETW 控制与解析，可迅速得到独立参照，也可作为交付主程序的备选。PerfView 源码包含原始时间戳消费路径。 | 测 Windows 版本/架构、所需 provider 和 ETL 保真、发布尺寸、冷启动、回调延迟和资源预算。 [TraceEvent 指南](https://github.com/microsoft/perfview/blob/main/documentation/TraceEvent/TraceEventLibrary.md) · [原始时间消费源码](https://github.com/microsoft/perfview/blob/ea4e78e0e363a0838c963338619221f3e211ddb5/src/TraceEvent/ETWTraceEventSource.cs) |
| **C++ / TDH 或 krabsetw** | 与 Windows SDK 和已有诊断工具的直接对照，有现成 session/provider 封装。 | 核架构支持、异常传播、阻塞关闭、内存安全和维护；以 native 实测作负载/解析参照。 [krabsetw](https://github.com/microsoft/krabsetw) · [TDH](https://learn.microsoft.com/en-us/windows/desktop/ETW/consuming-events) |
| **Go / Python** | 可把原生 WPR 作为外部进程，方便做编排、离线校验、脚本和数据分析。 | 若提议进入实时消费路径，使用与其他候选相同的 provider、事件率、CPU/RSS、丢失和停止测试。胶水程序的快慢与 ETW provider 覆盖无直接对应。 |

本轮首选 Rust 进行小型可重做的比较。若两条路线得到同等数据，选生命周期、部署和维护成本更可靠的一条。语言、缓冲策略与 API 连接方式可以由各自的测量结果决定。

## 其他 Agent 的调用接口

先定义调用语义，再选传输。候选接口包含能力查询、创建有界窗口、查询状态、带同一请求 ID 的幂等停止、读取封存摘要和按引用浏览原件。把 `ACCEPTED`、已就绪、停止请求、确认停止、已封存、partial 和失败分别返回。断连、超时或重试只说明该调用通道状态，采集终态须按 capture ID 查询。大量原始数据按稳定 locator 读取，短摘要返回 Agent。

| 传输候选 | 适用情形 | 冻结前验证 |
| --- | --- | --- |
| **stdio JSON-RPC** | 本机 Agent 可拥有子进程，接口小、容易重放。先验证本机调用与子进程生命周期。 | 版本发现、请求 ID、输入限长、慢读、背压、取消和 EOF 后的会话收尾。一次运行搭配一个宿主时优先。 [JSON-RPC 2.0](https://www.jsonrpc.org/specification) |
| **MCP** | Agent 宿主原生发现工具，适合在成熟 capture API 外加本地 adapter。 | 验证宿主实际支持的初始化、版本、工具清单与调用。stdio JSON-RPC 仅是底层契约。共享式 HTTP 需要额外验证会话管理和本机绑定。 [MCP 生命周期](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle) · [传输](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports) |
| **Windows 命名管道** | 多 Agent 复用同一常驻会话、需要本机身份隔离时比较。 | 显式 DACL、调用者身份、拒绝远端、名称碰撞、重连和服务回收。微软文档的默认 DACL 涵盖比单用户更宽的主体。 [命名管道访问权限](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights) |
| **本机 HTTP/gRPC** | 多客户端、跨语言流式读取与长任务查询；是否采用依赖宿主需要。 | 监听范围、身份、本机调用保护、流量反压与版本；设置 deadline，验证取消被服务端执行，确保 retried Stop 使用同一操作身份。 [gRPC deadline](https://grpc.io/docs/guides/deadlines/) · [取消](https://grpc.io/docs/guides/cancellation/) · [重试](https://grpc.io/docs/guides/retry/) |

## 冻结前的比较实验

| 验证 | 独立对照 | 会改变的决策 |
| --- | --- | --- |
| 覆盖、吞吐与扰动 | 自有 fixture 的独立序号、目标身份和无录制基线，在同机同负载轮换 A/B，条件需要时复验 C。按重复窗记录每来源覆盖、loss、CPU、RSS、磁盘写入和处理延迟。 | A 通过时仅补薄控制；需要实时细粒度时进入 B；只有具体语义缺口才增加 C。 |
| 精度、复用和方向性 | 同名不同 PE/PDB、PID/模块/端口多次复用，独立 IPC 种类、不同线程近邻 tick、延迟 marker、UTC 跳变、并发无关写、SQLite 回滚/提交/WAL 换代。 | 哪些关系能绑定到目标，哪些只有时间共现，何时可以表达顺序或因果。 |
| 执行失败与退出 | 无权限、其他会话已经运行、强制溢出、受控暂停读取、跨会话 Stop、EOF、独立退出观察超时、封存后继续写。 | 哪些失败要阻止 READY，停采如何确认，partial 如何可复核，哪种传输最稳。 |
| 可调用与可部署 | 至少用一个目标 Agent 宿主和一个普通客户端，测能力发现、重试幂等、取消、并发与读取摘要，Windows 实际工具版本和配置随回执。 | stdio 是否足够，是否需要 MCP 适配或常驻服务，候选语言的交付成本。 |

已迁入固定窗口和生命周期的离线合成测试。原生 ETW 覆盖、独立时钟对齐和多语言同条件对比仍需实测。 [验证计划](VALIDATION.md) · [ETW 基线工单](https://github.com/youling/TraceScalpel/issues/3)
