# IPC API 协议版本稳定化计划（组件边界全公开契约, v1）

状态：**草案**（2026-10-17；2026-10-18 三次修订：worker 面升格为公开契约；
admin 面与外部兼容面纳入公开契约；**兼容面泛化为 SysW 适配器族**）。
§2 决策记录中 3 项待拍板；定稿前不动 `proto/`。

## 目标

把 System A 的**全部组件边界**稳定为 **v1 契约**。设计判据（既定原则）：

> **组件拆分点即公开契约**——组件既然被拆分出来，就必须可替换；
> 可替换性要求其边界协议公开且稳定，否则第三方无法独立实现/替换该组件。

据此，四个边界面均为公开 API，分别服务不同受众：

| 面 | 受众 |
|---|---|
| **A 控制面**（`manager.*`） | 消费者：其他项目的控制客户端 |
| **B Worker 面**（worker IPC + fdpass + finder） | 实现者：第三方 worker 作者 |
| **C Admin 面**（`admin.*`/`staging.*`/`unitstate.*`） | 运维工具作者 |
| **D 外部兼容面（SysW 适配器族）** | 外部生态工具：systemctl（systemd）、telinit/sysvinit 工具（`/dev/initctl`）及其他未来生态 |

- 协议演进有机械化的兼容性规则，并由 CI 门禁强制；
- 版本协商与能力发现成为握手的一部分，不再静默通过；
- 发布可交付物：`.proto` 契约文件、client crate、worker 脚手架 crate、
  多语言参考实现、conformance 测试套件、契约文档。

## 1. 现状盘点（资产与缺口）

### 1.1 A 面：控制面资产

| 项 | 现状 | 位置 |
|---|---|---|
| 信封 | `Envelope{request_id, source, target, method, payload}`，请求-应答按 `request_id` 关联，事件复用同一会话（单向事件 `request_id=0`） | `proto/common.proto` |
| 分帧 | 4 字节大端长度前缀（tokio `LengthDelimitedCodec`），上限 16 MiB | `libsysa/src/ipc.rs` |
| 序列化 | protobuf（prost），三个契约文件 `proto/{common,control,workload}.proto`，当前均为 `package ipc`（无版本、未按面分包） | `proto/` |
| 方法面 | ~25 个 `manager.*` 方法（字符串分发）、事件（`JobEvent`、`UnitRemovedEvent`）、流式应答（`UnitStateEof`） | `ipc/control.rs` 分发表 |
| 握手 | 首帧必须 `manager.hello`（`ManagerHelloRequest{flavor, version, worker_id}`）；`admin.*` 为同 socket 旁路入口 | `ipc/control.rs` |
| 错误出口 | 通用 `manager.error` 方法 | `ipc/control.rs` |
| 鉴权 | socket 文件权限 + uid 检查（root 或本 uid）；`--user` 实例走 `$SYSTEMA_RUNSTATEDIR` | `libsysa/src/paths.rs` |

### 1.2 B 面：Worker 面资产（第三方实现者的现有接口）

| 项 | 现状 | 位置 |
|---|---|---|
| Worker IPC | Envelope 同构复用；`worker.register` 注册，`worker.ready`/`worker.ack`/`worker.exit`/`worker.slice` 事件；`resource.prepare`/`resource.prepare_result` 任务对；`job.new`/`job.completed` 通知 | `libsysa/src/worker_ipc.rs`（`WorkerClient`/`WorkerHub`） |
| 能力旗标 | **已有雏形**：`supports_unit_define`/`supports_resource_prepare` 构建期能力声明 | `libsysa/src/worker_ipc.rs` |
| fdpass | 独立 unix socket（`systema_fdpass_sock`），按 `worker_id` 关联通道（`FdPassMap`），SCM_RIGHTS 传 fd | `ipc/server.rs` |
| Finder 注册 | 单元类型注册会话：`finder.register_units` → `finder.commit_units` → `finder.ack` | `libsysa/src/finder.rs` |
| 生命周期/监督 | 注册 → ready → 任务 → exit/reaped；**worker 死亡 → sysi abort + drain**（行为契约，未成文） | `crates/backend/systema-sysi/src/supervise.rs` |

### 1.3 C 面：Admin 面资产

| 项 | 现状 | 位置 |
|---|---|---|
| 方法族 | `admin.staging`/`admin.staging.result`、`admin.unitstate`/`admin.unitstate.entry`/`admin.unitstate.eof`（流式）、`staging.query`/`staging.query_result` | `ipc/control.rs` ~L158 旁路入口 |
| 实现 | staging 查询、unitstate 全量导出（entry 流 + eof 终止） | `libsysa/src/staging_admin.rs`、`unitstate_admin.rs` |
| 鉴权 | 与 `manager.*` 同 socket，规则待盘点成文（应更严：root/管理域） | — |

### 1.4 D 面：外部兼容面资产（SysW 适配器族）

SysW 的定位是**外部生态适配器族**：每个外部生态协议 = 一个 flavor
适配器，共享 flavor 无关基础（control-port client），经 B 面接入。

| 适配器 | 状态 | 版本锚 | 位置/说明 |
|---|---|---|---|
| **systemd**（`org.freedesktop.systemd1`，D-Bus） | 已实现 | systemd 上游版本（建议基线 v257） | `systema-sysw.systemd/src/dbus/{manager,service_obj,scope_obj,mount_obj,properties,activator}.rs`；会话桥 `SYSTEMCTL_FORCE_BUS` 路径见 `doc/user-session.md`；已知差异（如 `set-environment` 挂死）为差异清单素材 |
| **initctl**（`/dev/initctl`，sysvinit FIFO 协议） | **未来** | sysvinit `initmsg.h` 协议定义（上游已冻结：`INIT_MAGIC`/版本/命令码/`struct init_request`） | 无现有代码；适配器形态见 §4.4 |
| 其他（upstart/OpenRC/BSD rc.d/……） | 占位 | 各协议自有锚 | 按同一"基线+矩阵+差异"模型扩展 |
| 共享基础 | 已有 | — | `systema-sysw-common`：flavor 无关的 control-port client（代码注释即定义为 "bridge flavors" 架构） |

### 1.5 缺口（稳定化的靶子）

1. **A/C 面 hello 的 `version` 字段完全未校验**：只打日志，无协商、
   无不兼容拒绝。
2. **无兼容性强制机制**：字段号复用、类型变更没有任何 CI 拦截。
3. **错误模型半结构化**：各方法自带 `success/message`，无稳定错误码枚举。
4. **proto 未按面分版本**：共用 `package ipc`，A/B/C 面无独立演进空间。
5. **B 面无契约文档、无 SDK**：第三方实现者只能读 `worker_ipc.rs` 源码；
   fdpass 关联机制、监督行为契约均未对外成文。
6. **C 面未成文**：方法族存在但无文档、鉴权规则未声明。
7. **D 面无统一模型**：适配器族已有架构雏形（flavor 机制），但无
   "每适配器 = 基线 + 支持矩阵 + 差异清单 + 快速失败"的契约模型；
   systemd 适配器无支持矩阵、无差异清单；未支持方法以挂死而非错误
   呈现（实测教训）。
8. **无客户端/实现者交付物**。

## 2. 决策记录

| 议题 | 决策 |
|------|------|
| 冻结范围 | **四公开面**：A 控制面（`manager.*`+事件流）、B Worker 面（worker IPC+fdpass+finder+监督行为）、C Admin 面（`admin.*`/`staging.*`/`unitstate.*`）、D 外部兼容面（SysW 适配器族）。判据：组件拆分点即契约 |
| B/C/D 与组件替换的关系 | sysw 本身是 worker，其整体替换性由 B 面覆盖；D 面是各适配器**服务**的外部契约（面向生态工具），按适配器独立版本化 |
| D 面契约模型 | **每适配器实例 = 一个契约面**：{传输/编码，命令面，锚（该协议上游定义），支持矩阵，差异清单}。v1 冻结的是**模型与规则**；systemd 适配器随 v1 交付矩阵/差异 1.0；initctl 等为后续适配器（模型占位，additive） |
| 版本模型 | 三层：wire 版本（分帧+信封，A/B/C 共享）/ API 版本（按面分包挂版本）/ capabilities（升格现有 `supports_*` 旗标） |
| D 面版本锚 | **各适配器独立锚定**：systemd 适配器锚上游版本；initctl 适配器锚 sysvinit `initmsg.h` 协议（已冻结）；不由 systema semver 牵动 |
| proto 包组织 | 按面分包：`systema.control.v1`（A+C 同 socket 同发布节奏，方法族独立稳定性清单）/ `systema.worker.v1`（B）；共享信封留 `ipc.common`。D 面非 protobuf（D-Bus/FIFO 各异），以支持矩阵文档为契约 |
| 握手演进 | A/B/C 注册握手均**加字段**（protobuf 天然兼容，老调用方无感）；不兼容时回结构化错误并关闭连接 |
| 错误模型 | 统一稳定错误码枚举（A/B/C），经 `manager.error` + `request_id` 回传；错误码是契约的一部分 |
| 方法/事件字符串 | **重命名=破坏性变更**（字符串是路由键），A/B/C 面一致；弃用=保留旧名+文档标注，删除进 v2 |
| fdpass 契约 | fd 集合、顺序、`worker_id` 通道关联机制均为契约面；fd 集合变更=破坏性（或以新能力串开新形状） |
| 生命周期契约 | worker 死亡 → sysi abort+drain 等监督行为属**行为契约**，写入实现指南并受版本承诺约束（改=破坏性） |
| D 面偏离策略 | 对生态工具的**可观测行为**是契约：已支持命令改语义/移除=破坏性；**未支持命令必须按该协议语义快速失败**（D-Bus 回明确错误；initctl 这类无错误回包语义的协议须定义忽略/日志行为，不得无限挂起调用方）；新支持=additive |
| 大版本策略 | **待拍板**：破坏性变更时 v1/v2 并存服务 vs 单轨切换（各面独立适用） |
| 本轮交付物范围 | **待拍板**：仅协议层，还是含 client crate + worker 脚手架 + conformance 套件 |
| CI 破坏检查工具 | **待拍板**：`buf breaking`（外部依赖、功能全）vs 自研 descriptor-set 对比脚本（零依赖、只拦硬破坏） |

## 3. 边界：四公开面

| 层 | 内容 | 版本锚 | 服务对象 |
|---|---|---|---|
| **Public A：控制面** | `control.socket` 的 `manager.*` + 事件/流式应答 | systema semver（`systema.control.v1`） | 消费者（客户端） |
| **Public B：Worker 面** | worker IPC、fdpass、finder 注册、监督行为契约 | systema semver（`systema.worker.v1`） | 实现者（第三方 worker） |
| **Public C：Admin 面** | 同 socket 的 `admin.*`/`staging.*`/`unitstate.*` 方法族 | 跟随 A 面同轨，方法族独立清单 | 运维工具作者 |
| **Public D：外部兼容面** | SysW 适配器族：systemd1（D-Bus）、`/dev/initctl`（FIFO）、未来其他 | **各适配器独立锚**（上游协议定义）+ 差异清单 | 外部生态工具 |
| Internal | 仅 socket/进程内部实现细节（无独立协议面）；服务侧 notify 走 sd_notify 语义（systemd 锚定，随 D 面文档一并声明） | — | — |

判据：**组件拆分点即公开契约**。A/B/C 共享 wire 层与发布节奏，但作为
独立方法族分别冻结、分别演进；D 是**一族**契约（每适配器一个实例），
契约形态统一为"外部协议语义子集 + 支持矩阵 + 差异清单 + 快速失败"。

## 4. 版本模型

### 4.1 三层

1. **wire_version**：分帧格式 + `Envelope` 语义。A/B/C 共享，极稳定，
   v1 冻结后只有破坏性变更才会 bump。
2. **API 版本**：按面分包挂版本（`systema.control.v1`（含 A+C）/
   `systema.worker.v1`（B））。各面 v1 内只增不改；破坏性变更开对应面
   v2。**D 面独立锚定**（每适配器）：版本声明 = 兼容基线（该协议上游
   版本/定义）+ 支持矩阵修订，不随 systema semver。
3. **capabilities**：握手应答携带能力字符串列表，调用方按能力探测而非
   版本嗅探。新增可选功能只加能力串。B 面现有
   `supports_unit_define`/`supports_resource_prepare` 旗标升格为正式
   capabilities 字段（映射 `unit.define`、`resource.prepare`）。

### 4.2 A/C 面握手演进（protobuf 草案，加字段即兼容）

```protobuf
message ManagerHelloRequest {
  string flavor = 1;      // 既有
  string version = 2;     // 既有：客户端自身版本字符串（语义不变）
  string worker_id = 3;   // 既有
  uint32 wire_min = 4;    // 新：客户端支持的最低 wire 版本
  uint32 wire_max = 5;    // 新：最高
  repeated string capabilities = 6; // 新：客户端所需特性
}
message ManagerHelloResult {
  bool success = 1;
  string message = 2;
  uint32 wire = 3;                   // 新：服务端选定的 wire 版本（0=不协商，老服务端）
  string server_version = 4;         // 新
  repeated string capabilities = 5;  // 新：服务端能力全集
}
```

`admin.*` 首帧入口沿用同一协商（admin 客户端同样先声明版本范围）。

### 4.3 B 面握手（同一规则）

`worker.register`/`finder.register_units` 两个注册入口同样加协商字段
（`wire_min/max`、capabilities），sysa 侧回选定版本与能力全集；形态在
P0 盘点现有 payload 后补全草案。规则与 §4.2 一致：老 worker 不填 →
按 v1 服务；无交集 → 结构化错误 + 关闭。

### 4.4 D 面声明方式（每适配器一个契约实例）

D 面以文档为契约，目录化：`doc/compat/<protocol>.md`，每个适配器统一
五段结构——

1. **传输/编码**：如 systemd = 会话/系统总线 D-Bus；
   initctl = `/dev/initctl` FIFO + `struct init_request`
   （`INIT_MAGIC`/版本/命令码，sysvinit `initmsg.h` 语义）；
2. **命令面**：支持的命令/方法/属性/信号清单；
3. **锚**：所追踪的上游协议定义（systemd 建议基线 v257；initctl 为
   sysvinit 冻结协议）；
4. **支持矩阵**：full / partial / 未支持→快速失败方式；
5. **差异清单**：与上游语义的已知偏离（systemd 适配器含 2026-10-17
   调试实录：`set-environment` 等未支持方法曾以挂死呈现，冻结前必须
   改为明确 D-Bus 错误）。

适配器实现形态（非契约但相关）：每适配器一个 flavor 子 crate
（`sysw.<protocol>`，复用 `sysw-common` 的 control-port client 与
B 面接入），与契约实例一一对应、可独立替换。

**v1 范围**：模型与规则随 v1 冻结；systemd 适配器的矩阵/差异 1.0 随
v1 交付；**initctl 适配器为后续阶段交付**（本文档仅占位模型，其实现
不构成对 v1 其他面的破坏性变更——additive）。

## 5. 兼容性规则（机械规则，写入文档 + CI）

1. **proto 规则（A/B/C）**：字段号永不复用；不改字段类型/基数；删除
   字段必须 `reserved`；枚举只增不改语义（prost 开放枚举 + 未知值容忍）。
2. **方法/事件字符串即 API**：`manager.start_units`、`resource.prepare`、
   `finder.ack`、`admin.staging` 等字符串是路由键，改名=破坏性变更；
   新增不受限。
3. **未知方法必须回错误** `METHOD_UNKNOWN`（调用方据此安全做能力降级）；
   **未知字段忽略**（prost 默认行为，测试钉死）。
4. **事件与流式应答同受约束**：事件名、`UnitStateEof`/`admin.unitstate.eof`
   等流终止语义均为契约面。
5. **fdpass 契约**：fd 集合与顺序、按 `worker_id` 通道的关联机制、
   SCM_RIGHTS 语义成文；fd 集合变更=破坏性（新增 fd 走新能力串开新形状）。
6. **行为契约同受版本承诺**：worker 死亡 → sysi abort+drain、prepare
   失败 → 完整失败机制等生命周期语义写入 worker 实现指南；改变可观测
   行为=破坏性（文档 + conformance 钉死）。
7. **错误码稳定枚举**（A/B/C 契约的一部分）：`NOT_FOUND / INVALID_ARGUMENT /
   PERMISSION_DENIED / FAILED_PRECONDITION / TIMEOUT / METHOD_UNKNOWN /
   INCOMPATIBLE_VERSION / INTERNAL`。文案可变（且走 l10n），码不可变。
8. **D 面规则（每适配器）**：已支持命令的可观测行为改变=破坏性；
   未支持命令必须按该协议语义快速失败——有错误通道的（D-Bus）回明确
   错误；无错误回包语义的（initctl FIFO 类）须在矩阵中定义其行为
   （忽略 + 日志，且不得使调用方无限阻塞）；新支持=additive。

## 6. 契约管理与 CI 门禁

1. `proto/` 目录即 A/B/C 面公开契约，路径固定（本仓库根 `proto/`）。
2. build 时嵌入 **FILE_DESCRIPTOR_SET + schema 指纹**（prost-build 产出
   `descriptor_set`），A/B/C 握手均可携带指纹供对端自检。
3. **CI 破坏性变更拦截**：与上一 release 的 descriptor set 对比（A/B/C）；
   工具选型见 §2 待拍板项；与现有门禁（0e/0w、交叉矩阵、rust-test.sh）
   同级执行。
4. **文档生成**：
   - `doc/ipc-api.md`（A+C 面：分帧、鉴权（含 admin 规则）、socket 发现、
     错误码表、事件订阅、超时常量、并发模型）；
   - `doc/worker-impl.md`（B 面：注册/任务/事件流程、fdpass 关联、
     生命周期与监督契约、环境变量契约、脚手架用法）；
   - `doc/compat/systemd.md`、`doc/compat/initctl.md`、……（D 面，每
     适配器五段结构：§4.4）。
   人工维护部分以"生成文件 + 手写附录"存在，避免双份真相。
5. **版本 bump 规则**：additive → minor；服务端修复 → patch；破坏性 →
   major + 对应面 v2（D 面随各适配器基线声明）。

## 7. 客户端与实现者生态

1. **`systema-client` Rust crate**（A+C 面）：从 workspace 导出，复用
   prost 生成物与 `libsysa` 的连接/编解码（`connect_control`、
   `frame_stream`、`send/recv_envelope`）；admin 方法族同 crate 暴露。
2. **`systema-worker` 脚手架 crate**（B 面）：封装连接/注册/事件循环/
   ack/fdpass 处理的公共实现，第三方实现者只写 do-one-thing 业务逻辑；
   升格 `supports_*` 旗标为 capabilities API。
3. **参考实现**：树内示例 worker（玩具单元类型）作 B 面参考；纯 Python
   控制面客户端（调试用 `sysactl.py`，~100 行）收编为多语言参考实现，
   发布 framing 规范 + `.proto` 即可衍生其他语言版本；D 面 initctl 类
   FIFO 协议同样可用小脚本做协议级参考客户端（`struct` 打包，
   `sysactl.py` 先例同型）。
4. **conformance 套件**（四面）：
   - A 面：握手协商、启停、事件订阅、流式 EOF、错误码路径；
   - C 面：admin 握手、staging 查询、unitstate 流式导出 + eof；
   - B 面：注册 → ready → prepare/spawn 任务对 → fdpass 闭环 →
     exit/reaped 全生命周期；监督行为（杀 worker → abort）可观测验证；
   - D 面（每适配器）：systemd 适配器**以真实 systemctl 为 conformance
     工具**（list-units / status / start / stop / is-active，已有冒烟
     先例）+ 支持矩阵逐项测试 + 未支持方法"快速明确错误"专项（防挂死
     回归）；initctl 适配器交付时以 telinit/sysvinit 工具或协议级
     脚本同样逐项覆盖。
5. **兼容矩阵测试**：A/B/C 旧 × 新（forward compat 必测）；新 × 旧
   （backward compat，仅 v1/v2 并存策略下需要）。

## 8. 迁移路径（P0–P3）

- **P0 盘点**：
  - A/C 面方法清单（`manager.*`；`admin.*`/`staging.*`/`unitstate.*`），
    逐个标注 stable / experimental；admin 鉴权规则成文素材；
  - B 面清单（`worker.*`/`resource.*`/`job.*`/`finder.*`）；fdpass fd
    集合与关联机制成文素材；payload proto 归属；
  - D 面：`doc/compat/` 骨架 + systemd 适配器方法覆盖盘点 → 支持矩阵
    初版 + 差异清单初版（含挂死案例改为明确错误的整改项）；initctl
    适配器五段结构占位文档（范围声明，无实现承诺）。
- **P1 无损升级**：A/B/C 握手协商字段 + capabilities + 统一错误码 +
  未知方法错误化；D 面（systemd 适配器）未支持方法改快速明确错误。
  现有调用方（sysw 桥、树内全部 worker、测试脚本、生态工具）零破坏。
- **P2 门禁与文档**：descriptor diff CI + 契约文档（`ipc-api.md` /
  `worker-impl.md` / `compat/*`）+（视拍板）client crate + worker 脚手架
  + 四面 conformance 套件。
- **P3 声明 v1.0 冻结**：A/B/C 同步宣告（semver 轨），D 面宣告模型 +
  systemd 适配器矩阵 1.0；此后各面按规则演进。
- **后续阶段（v1 后，additive）**：initctl 适配器实现 + 其 compat 文档
  1.0 + conformance；再后续适配器按同一模型推进。

## 9. 明确不做

1. **不换 gRPC**：本地 unix socket 场景 prost + 长度分帧足够；
   仅借鉴其 status 错误模型。
2. **不动鉴权模型**：保持 uid 规则（root 或本 uid）；C 面规则只成文、
   不新设；polkit/ACL 为后续独立议题。
3. **不定义 worker 间直连**：组件只与 sysa 对话（经 B 面）。
4. **本轮不实现任何新适配器**：initctl 只出模型占位文档；logind 等
   systemd 生态扩展同为后续适配器。

## 10. 验证清单

- [ ] 老调用方（sysw 桥、树内全部 worker、`sysactl.py`、真实 systemctl）
      在 P1 改动后零修改可用
- [ ] 新客户端/新 worker/新 admin 工具 hello（wire_min/max）与老调用方
      均通过协商（A/B/C）
- [ ] 无交集版本 → `INCOMPATIBLE_VERSION` + 连接关闭（A/B/C）
- [ ] 未知方法 → `METHOD_UNKNOWN`；未知字段 → 静默忽略（测试钉死）
- [ ] C 面：admin 握手 + staging/unitstate 流式导出 conformance 通过；
      admin 鉴权规则成文且测试覆盖
- [ ] 第三方视角的参考 worker 全生命周期通过
      （注册 → ready → prepare/spawn → fdpass → exit）
- [ ] fdpass fd 集合/顺序/关联机制 conformance 通过
- [ ] 监督行为可观测验证：杀 worker → sysi abort+drain（行为契约钉死）
- [ ] D 面：`doc/compat/systemd.md` 矩阵初版 + 差异清单成文；未支持
      方法快速明确错误（挂死案例整改）；systemctl conformance 全绿
- [ ] D 面：`doc/compat/initctl.md` 五段结构占位（范围声明，无实现）
- [ ] descriptor diff CI 拦截注入的模拟破坏（字段复用、类型变更各一例）
- [ ] 契约文档生成成功且与 proto 同步（CI 校验无漂移）
- [ ] conformance 套件在干净机器上对新部署栈全绿
- [ ] 各面独立 bump 互不影响（A/C 同轨例外——同 socket 需同步宣告）

## 附：关键代码位置

| 内容 | 位置 |
|---|---|
| 契约文件（现均 `package ipc`，待按面分包挂版本） | `proto/common.proto`（Envelope/UnitStatus）、`proto/control.proto`（A 面 ~67 message）、`proto/workload.proto` |
| A 面帧编解码/连接 | `crates/libsysa/src/ipc.rs`（`connect_control`/`frame_stream`/`send_envelope`/`recv_envelope`） |
| hello 处理（现仅打日志，`version` 未校验） | `crates/backend/systema-sysa/src/ipc/control.rs` ~L169–230 |
| A 面方法分发表 | `crates/backend/systema-sysa/src/ipc/control.rs` ~L524 起 `match method` |
| 通用错误出口 `manager.error` | `crates/backend/systema-sysa/src/ipc/control.rs` ~L270 |
| C 面旁路入口 + 方法族 | `crates/backend/systema-sysa/src/ipc/control.rs` ~L158；`admin.{staging,staging.result,unitstate,unitstate.entry,unitstate.eof}`、`staging.{query,query_result}` |
| C 面实现 | `crates/libsysa/src/staging_admin.rs`、`crates/libsysa/src/unitstate_admin.rs` |
| B 面 Worker IPC（`WorkerClient`/`WorkerHub`、`supports_*` 旗标） | `crates/libsysa/src/worker_ipc.rs` |
| B 面方法/事件字符串 | `worker.{register,ready,ack,exit,slice}`、`resource.{prepare,prepare_result}`、`job.{new,completed}` |
| B 面 fdpass（`FdPassMap`，`worker_id` → UnixStream，SCM_RIGHTS） | `crates/backend/systema-sysa/src/ipc/server.rs` |
| B 面 Finder 注册（`register_units`/`commit_units`/`finder.ack`） | `crates/libsysa/src/finder.rs` |
| B 面监督行为契约（worker 死亡 → abort+drain） | `crates/backend/systema-sysi/src/supervise.rs` |
| D 面 flavor 无关基础（bridge flavors 架构） | `crates/backend/systema-sysw/systema-sysw-common/` |
| D 面 systemd 适配器（systemd1 对象层） | `crates/backend/systema-sysw/systema-sysw.systemd/src/dbus/{manager,service_obj,scope_obj,mount_obj,properties,activator}.rs` |
| D 面 initctl 适配器（未来） | 无现有代码；锚 = sysvinit `initmsg.h`（`INIT_MAGIC`/`struct init_request`） |
| socket 路径/环境矩阵 | `crates/libsysa/src/paths.rs`、`doc/user-session.md` §3.1 |
| 多语言客户端先例 | `sysactl.py`（调试用纯 Python 控制面客户端） |
