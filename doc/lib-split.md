# 库拆分计划（libsysa 瘦身与 worker 门面库）

状态：**草案二稿**（2026-10-18）。一稿四层结构按 **VFS 驱动模型**简化：
对外仅两库，worker 作者只见"一个 crate、一个 trait、一个 `run()`"。
**同日执行决定：M1 以单库 `libsysa-ipc` 落地，暂不拆 api/wire 细层
（见 §3.4）。**
本计划是 `doc/ipc-api-stability.md` §7 交付物的**物理前提**；契约语义
（错误码、协商、capabilities）在该文档定义。crate 命名/目录为建议值。

## 目标

**System Worker 按外部项目对待，接入方式对齐 Linux VFS 驱动模型**：

> 驱动作者实现 `struct file_operations`（`{read, write, flush, …}`），
> 其余由 VFS 核心提供；worker 作者实现 `trait Worker`
> （`{status, start, stop, prepare, …}`），其余由库提供。

由此：

1. worker 接入 = 一个 crate 依赖 + 一个 trait 实现 + 一个 `run()` 调用；
2. 分帧/握手/协商/fdpass/事件泵/关停全部是**库的内部实现**，不进
   worker 作者的心智模型；
3. 消灭多套客户端实现并存，建立**统一标准**（同一 schema、同一 trait
   语义、同一 conformance），为未来其他语言封装铺路；
4. libsysa 瘦身为"内部共享 + 二进制基建"。

## 1. 现状实证（三个病灶）

1. **依赖重量**：`libsysa`（package 名 `sysa`）依赖 `gettext-rs`（构建期
   需 gettext C 工具链）、`colored`、`tracing-subscriber`、`libc` 等
   二进制基建。外部项目只想接 worker，却被迫拖进全部。
2. **三套客户端实现并存**：`libsysa::ipc` 原语 + 临时客户端
   （`staging_admin`/`unitstate_admin`/`finder`）；`sysw-common::
   ControlClient`（完整会话实现：握手/RPC 多路/事件流/重连）；
   `sysactl.py`（协议第三套实现）。
3. **双端混居**：服务端资产（`WorkerHub`、`event_bus`、`notify`、
   fdpass 监听）与客户端资产同库，无法向外部承诺任何依赖边界。

**影响面**：23 个 crate 依赖 `libsysa`（11 backend + 2 frontend +
`sysa-pager` + 深层子 crate 含 `sysw-common`）——依赖图上不存在
"外部视角"入口。

## 2. 决策记录

| 议题 | 决策 |
|------|------|
| 对外结构 | **两库**：`systema-worker`（worker 门面：trait + `run()`）、`systema-client`（工具面：类型化控制/admin 客户端） |
| 内部结构 | **一库**：`systema-ipc`（proto 生成 + 分帧/关联/错误码，两 SDK 共用，仅为 DRY）；**不对外承诺其 Rust API 稳定** |
| M1 执行形态（2026-10-18） | **单库 `libsysa-ipc`**（package 名 `sysa-ipc`，沿"目录 `libsysa` ↔ 包名 `sysa`"惯例）：一次性把 IPC 内核提为独立库，**暂不拆 api/wire 细层**（用户决定）；对外 SDK 与 worker trait 化仍属 M2+。落点见 §3.4 |
| worker 门面模型 | VFS 类比：`trait Worker` = ops 表（方法集由 B 面协议盘点一一映射）；未实现操作 = trait 默认实现返回 `UNSUPPORTED`（= NULL ops）；`run()` = 注册设备 + 内核核心 |
| 稳定性对象 | 稳定 = `.proto` + framing 规范 + **trait 语义**（additive：新操作=带默认实现的方法）+ conformance；内部 crate 签名与门面以外的类型不承诺 |
| 依赖方向 | `systema-*` 永不依赖 `sysa`(libsysa)；libsysa 反向依赖 SDK；CI 依赖方向门禁 |
| 库不设策略 | SDK 无日志 subscriber、无 gettext、无 CLI 输出、无信号处理；`tracing` 仅 facade；**错误 = 结构化码**，l10n/文案归二进制层 |
| 机制/策略边界 | 机制（分帧、关联、握手、注册、fdpass）进 `systema-ipc` + 门面；策略（`mounts`、监督）留 libsysa，直到有外部需求 |
| Finder 归属 | `finder` 注册会话 = 单元类型提供者接口 → `systema-worker`（由 `run()` 自动执行，`UNIT_TYPES` 常量驱动） |
| 收编第二实现 | `sysw-common::ControlClient` 会话层收编进 `systema-client`，sysw 只留 flavor 逻辑——统一标准第一案 |
| 多语言路线 | A = 从 proto+framing 规范+**trait 语义文档**移植（python 先例）；B = C ABI（cdylib + `.h`，opaque handle、整型错误码）。两路线过同一 conformance |
| proto 代码生成 | 随 `systema-ipc`（build.rs + `OUT_DIR` include）；protoc 级面隔离保持；descriptor diff CI 挂该 crate |

## 3. 简化结构

### 3.1 对外视图（外部项目只见这两行）

```
   systema-worker   worker 作者：trait Worker + run(Ctx)     ← 第三方 worker
   systema-client   工具作者：类型化 manager.*/admin.* 客户端  ← 控制工具
```

### 3.2 内部视图（实现细节，不进文档、不承诺签名）

```
   systema-ipc（内部共享）
     ├── proto/   proto 生成物（build.rs 迁入，面隔离保持）
     └── wire/    Envelope 分帧、request_id 关联、握手协商、
                  错误码、能力串、socket 发现（env 矩阵）、SCM_RIGHTS 原语
        ↑ 两 SDK 依赖它；libsysa 也依赖它（同一实现，不复制）

   libsysa（瘦身后）
     服务端：WorkerHub / event_bus / notify / fdpass 监听 / paths(server)
     基建：logging / l10n / signals / version
     策略域：mounts
```

worker 依赖闭包目标：`systema-worker` + 底层的
`prost/bytes/tokio(+util)/futures/tracing(facade)` —— 无
`gettext-rs`/`colored`/`tracing-subscriber`/`libc`。

### 3.3 worker 门面（VFS 模型草图，方法集由 P0 协议盘点定稿）

```rust
use systema_worker::{Worker, Ctx};

struct MyWorker;
impl Worker for MyWorker {
    const UNIT_TYPES: &'static [&'static str] = &["mynode"]; // 注册元数据
    const CAPS: &'static [Cap] = &[Cap::ResourcePrepare];    // 能力声明（升格 supports_*）

    // ── ops 表：B 面协议方法一一映射（worker.* / resource.* / job.* 盘点） ──
    async fn status(&self, id: &str) -> Result<UnitStatus, WireError> { ... }
    async fn start(&self, job: Job, ctx: &Ctx) -> Result<(), WireError> { ... }
    async fn stop(&self, id: &str, ctx: &Ctx) -> Result<(), WireError> { ... }
    async fn prepare(&self, req: Prepare, ctx: &Ctx) -> Result<PrepareReply, WireError> { ... }

    // 未覆盖的操作 → 默认返回 UNSUPPORTED，run() 不派发（= VFS NULL ops）
}

#[tokio::main]
async fn main() -> Result<()> {
    systema_worker::run(MyWorker).await // 连接/发现/注册/握手/fdpass/事件泵/关停全包
}
```

| Linux VFS | systema-worker |
|---|---|
| `struct file_operations`（`.read/.write/…`） | `trait Worker`（ops 表） |
| 驱动 `.write = NULL` | trait 默认实现 → `UNSUPPORTED` |
| `register_chrdev()` | `run()` 内的连接 + 注册 + 握手 |
| VFS 核心（dentry/fd/页缓存） | 分帧、`request_id` 关联、协商、fdpass、事件泵、关停 |
| 驱动拿到的 `struct file *` | `Ctx`（发状态更新、收 fd、facade 日志） |
| 驱动 `module_init` | `#[tokio::main] main` |

`run()` 职责清单（全部隐藏在门面后）：socket 路径发现 → 连接 →
`worker.register`/`finder.register_units`（读 `UNIT_TYPES`）→ 握手
协商（wire_min/max + capabilities）→ 事件泵（`job.new` 等派发到 trait）
→ 应答/ack 关联 → fdpass 收发 → 优雅关停（SIGTERM 归一）。

### 3.4 第一刀执行落点（`libsysa-ipc`，2026-10-18，已执行）

用户决定：M1 以**单库**落地，暂不拆 api/wire 细层。新建
`crates/libsysa-ipc/`（package 名 `sysa-ipc`，沿"目录 `libsysa` ↔
包名 `sysa`"惯例），一刀切分：

- **迁入**：`ipc.rs`（分帧/连接原语）、`worker_ipc.rs`（双端），
  `controller.rs`（`UnitController` trait + `UnitStatus`/`UnitConfig`
  模型）、`finder.rs`、`staging_admin.rs`、`unitstate_admin.rs`、
  `paths.rs`、`signals.rs`（`run()` 关停归一是 IPC 会话生命周期的一
  部分）、`l10n/`（IPC 错误文案即时翻译依赖；M2 结构化错误码落地后
  再剥离 gettext）、`proto` 生成。
- **build.rs 四职责拆分**：proto 生成、`generate_paths()`、
  `compile_mo_files()`（`l10n_debug` feature 随迁）→ libsysa-ipc；
  `emit_build_info()`（`version.rs` 的构建记录）→ 留 libsysa。
- **留守 libsysa**：`event_bus`、`notify`、`mounts`、`unit_name`、
  `logging`、`version`。
- **零改动保证**：libsysa 在 crate 根 `pub use sysa_ipc::{…}` 重导出
  全部迁出模块——`sysa::ipc::*`/`sysa::l10n`/`sysa::worker_ipc::*` 等
  旧路径不变，**23 个消费者与其 Cargo.toml 均不动**。
- 本节取代 §3.2 中 `systema-ipc` 的当前形态；§4 表为 M2+ 目标态。


## 4. 逐模块归属表

| libsysa 模块 | 去向 | 理由 |
|---|---|---|
| `proto`（生成物 + build.rs） | **systema-ipc**（内部 `proto/`） | 契约源头；descriptor diff CI 挂这里 |
| `ipc`：分帧/编解码/`connect_control`/`make_envelope` | **systema-ipc**（内部 `wire/`） | 线格式唯一实现；`connect_control` 的跨域文案改结构化错误，由调用方渲染 |
| `worker_ipc`：`WorkerClient`、run 循环、fdpass 客户端、`supports_*` | **systema-worker**（门面内核） | trait 派发器 + 协议机械层 |
| `worker_ipc`：`WorkerHub`、`publish_unit_state_update` | 留 libsysa | sysa 服务端侧 |
| `finder`（注册三段会话 + staging query） | **systema-worker**（由 `run()` 自动执行） | 单元类型提供者接口 |
| `staging_admin`、`unitstate_admin` | **systema-client**（admin 模块） | C 面客户端 |
| `controller`：`decode_unit_config` + `UnitStatus` | **systema-ipc**（内部模型） | 契约模型，双端共用 |
| `paths`：socket 发现 + env 覆盖矩阵 | **systema-ipc**（内部 `wire/`） | 客户端按同一规则算路径（`doc/user-session.md` §3.1 已成文） |
| `paths`：监听/创建/日志路径 | 留 libsysa | 服务端 |
| `event_bus`、`notify` | 留 libsysa | 服务端分发 |
| `mounts` | 留 libsysa | 策略，暂无外部需求 |
| `unit_name` | **systema-ipc**（内部模型） | 任何解析单元名的客户端都需要 |
| `logging`、`l10n`、`signals`、`version` | 留 libsysa | 二进制基建，SDK 不碰 |

## 5. 统一标准（跨语言的关键）

1. **Schema 源唯一**：`proto/*.proto`（按面分包，见
   `ipc-api-stability.md` §4）—— 所有语言从同一源生成。
2. **线格式规范成文**：framing / 关联语义 / 握手协商 / 错误码 / 能力串 /
   鉴权 → `doc/ipc-api.md`。
3. **trait 语义成文**：`Worker` ops 表 = worker 契约的 Rust 表达
   （`doc/worker-impl.md`）；**additive 规则同 proto**：新操作 = 带
   默认实现的方法，现有实现零改动。
4. **规范实现**：Rust `systema-worker`/`systema-client` = normative；
   `sysactl.py` = 第二语言参考。
5. **Conformance = 执行机制**：黑盒测试对活系统，任何语言的客户端
   **与任何 toy worker** 跑同一套——统一标准靠门禁执行。

多语言封装双路线：

- **路线 A · 规范移植**：proto + framing 规范 + trait 语义 → 各语言
  薄封装（python 客户端已实证 ~100 行；worker 侧封装即"把 ops 表
  接到该语言的回调模型"）。
- **路线 B · C ABI**：`systema-worker` 编 cdylib + `systema_worker.h`
  （ops 表 = C 函数指针表——与 VFS `file_operations` 的 C 形态天然
  同构；opaque handle、整型错误码）→ 天然支持 C/C++/Go/Python worker。
- 两路线验收 = 通过同一 conformance。

## 6. 迁移步骤（移动不复制，每步门禁护航）

- **M0 · 归属与规则定稿**：本文档评审通过。不动代码。
- **M1 · IPC 内核成库（已执行，2026-10-18，单库变体）**：新建
  `libsysa-ipc`（package `sysa-ipc`，暂不拆 api/wire 细层——用户决定），
  **移动** proto 生成、分帧、worker 双端、controller/模型、finder、
  admin 客户端、paths、signals、l10n（build.rs 三职责随迁，
  `emit_build_info` 留守）；libsysa 根 `pub use` 重导出保持全部旧路径，
  **23 个消费者零改动**。门禁：0e/0w + rustfmt 基线 + 交叉矩阵。
- **M2 · 两 SDK 成库 + worker 门面**：`WorkerClient`/fdpass 客户端/
  `finder` → `systema-worker`，**trait 化**（ops 表 + 默认
  `UNSUPPORTED` + `run()` + `Ctx`）；`staging_admin`/`unitstate_admin`
  → `systema-client`。试金石：`stagingctl`/`unitstatectl` 改依赖后
  功能不变；toy worker（样例）只依赖 `systema-worker` 即注册成功。
- **M3 · 收编第二实现（统一标准第一案）**：`sysw-common` 改薄 flavor
  逻辑，会话层依赖 `systema-client`。**风险中**：保持既有语义
  （pending RPC fail-closed、事件流、flavor 负责重连）——单独测试 +
  `systemctl` 冒烟回归。
- **M4 · 瘦身与门禁**：libsysa 完成瘦身；CI 上线依赖方向门禁
  （`systema-* → sysa` = 红）+ 对外依赖白名单校验；`cargo tree` 验证
  worker 闭包无 gettext/colored/subscriber/libc。
- **M5 · conformance 与多语言（后续阶段）**：随 `ipc-api-stability.md`
  §2 交付物拍板执行（conformance 先行；路线 B 的 C ABI 最后——其
  ops 表形态见 §3.3）。

## 7. 与稳定性计划的关系

| 本文步骤 | 对应 ipc-api-stability 阶段 |
|---|---|
| M0 | P0（模块归属 + 依赖规则盘点） |
| M1–M2 | P1–P2（物理载体先于/伴随契约冻结；trait 方法集=P0 协议盘点的输出） |
| M3 | P2（client crate 交付物前置） |
| M4 | P2（CI 门禁扩展依赖方向检查） |
| M5 | P2 交付物 + 后续阶段 |

## 8. 验证清单

- [ ] M1 后 23 个 crate 全部 0e/0w；re-export 旧路径下树内调用零改动
- [ ] **toy worker 只 import `systema_worker`（一个 trait + 一行 `run()`）
      即注册成功并可被 sysa 派发任务**
- [ ] worker 依赖闭包（`cargo tree`）不出现 `gettext-rs`/`colored`/
      `tracing-subscriber`/`libc`
- [ ] 未实现的 ops 收到派发前即被能力/默认实现拦截，返回 `UNSUPPORTED`
      （= NULL ops 语义，测试钉死）
- [ ] `stagingctl`/`unitstatectl` 只依赖 `systema-client` 功能不变
- [ ] `sysw-common` 闭包内只剩一套会话实现；收编后 systemctl 冒烟
      （list-units/status/start/stop/is-active）全绿
- [ ] 依赖方向门禁 CI 上线：`systema-* → sysa` 报红
- [ ] 每步 rustfmt 基线零新增偏差；交叉矩阵 0e
- [ ] conformance（M5）对新部署栈全绿

## 附：关键代码位置

| 内容 | 位置 |
|---|---|
| libsysa 本体（package 名 `sysa`） | `crates/libsysa/`（依赖见 `Cargo.toml`：gettext-rs/colored/tracing-subscriber/libc 等） |
| proto 代码生成（→ systema-ipc） | `crates/libsysa/build.rs`（prost-build）+ `lib.rs` 的 `pub mod proto`（`include!(OUT_DIR/ipc.rs)`，protoc 级面隔离注释在内） |
| 分帧/连接原语（→ systema-ipc） | `crates/libsysa/src/ipc.rs`（`EnvelopeFramed`/`connect_control`/`frame_stream`/`send/recv_envelope`） |
| 门面内核前身（WorkerClient + Hub，双端在此拆开） | `crates/libsysa/src/worker_ipc.rs`（`WorkerClient`/`WorkerHub`/`supports_*` 旗标） |
| 第二套会话客户端（待收编） | `crates/backend/systema-sysw/systema-sysw-common/src/client.rs`（`ControlClient`：握手/RPC 多路/事件流/重连语义） |
| Admin 客户端（→ systema-client） | `crates/libsysa/src/staging_admin.rs`、`unitstate_admin.rs` |
| Finder 注册（→ systema-worker，run() 自动执行） | `crates/libsysa/src/finder.rs` |
| 路径矩阵（客户端半 → systema-ipc） | `crates/libsysa/src/paths.rs`（`include!(OUT_DIR/paths.rs)` 编译期默认 + 运行时 env 覆盖） |
| B 面方法清单（= trait ops 表输入） | `worker.{register,ready,ack,exit,slice}`、`resource.{prepare,prepare_result}`、`job.{new,completed}`（P0 盘点定稿） |
| 换依赖试金石 | `crates/frontend/systema-stagingctl/`、`crates/frontend/systema-unitstatectl/` |
| 依赖者清单（23） | `grep -rln libsysa crates --include=Cargo.toml` |
| 多语言先例 | `sysactl.py`（调试用纯 Python 控制面客户端） |
