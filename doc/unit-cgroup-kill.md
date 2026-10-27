# 停止路径的 cgroup 化：SysS 可选查询 unit cgroup（架构设计）

状态：**已实现**（2026-10-27）。§12 的落地顺序 1–4 均已入库，协议形状与
本文一致；§9 中需要真实 cgroupfs / root 的端到端用例未做（理由见 §9）。

## 1. 问题

今天 System S 的 `stop` 只对 **main pid** 发信号（`crates/backend/systema-syss/src/process.rs::
stop_service`）。systemd 的默认语义是 `KillMode=control-group`：停止时打的是
**整个 unit 的 cgroup**，因此 `setsid()`、二次 fork、守护化后主动脱离进程组的
子进程都会被一并收走；我们只杀 main pid，会留下孤儿进程，`RemainAfterExit=no`
的服务"停了但没死"。

正确的收尾范围由 System R 掌握（cgroup 由它创建/托管），而**杀进程**是
System S 的职责（它持有 pid、负责生命周期）。两者需要一个单向、可选的
信息通道，且不能破坏已有的组件解耦：

- System S 只认识 System A：它不能知道 System R 的存在、worker id 或 cgroup 布局；
- System A 只做中继：它不解析 cgroup 路径、不 stat 文件系统；
- System R 只回答"这个 unit 的 cgroup 在哪"：它不发任何信号；
- System R **可选**：没有它时，System S 退回按进程组杀，行为不比现在差。

## 2. 组件职责

| 组件 | 新增职责 | 明确不做 |
|---|---|---|
| **System S**（`systema-syss`） | 停止前查询 unit cgroup；按 cgroup 发信号；查询失败/超时按进程组杀 | 不直连 System R；不解释 cgroup 层级；不假设 `/sys/fs/cgroup` 挂载点（路径当不透明字符串） |
| **System A**（`systema-sysa`） | 把 `unit.cgroup` 转成 `resource.cgroup` 问 System R，再把答案原样回给 System S；`slice` 从 UnitIR 取 | 不拼接/校验/规范化路径；不读盘；不缓存单位置（避免第二份真相） |
| **System R**（`systema-sysr`） | 回答"该 unit 的 cgroup 绝对路径"（registry 命中优先，否则按 `(unit_name, slice)` 纯函数推导 + 存在性检查） | 不杀进程；不关心提问者是谁 |

调用链恒定单向：**SysS → SysA → SysR**，回程 **SysR → SysA → SysS**。
SysS 与 SysR 之间没有会话、没有共同 socket、没有共享 crate。

## 3. 协议（workload 面）

两跳复用同一对消息，因此整个特性只增加 **2 条消息 + 2 组方法名**，
不新增 plane、socket、注册能力位或控制面方法。

```proto
// proto/workload.proto

// System S 询问某个 unit 的 cgroup；System A 收到后填好 `slice`
// （来自 UnitIR，System A 本来就持有），原样转发给 resource worker。
message UnitCgroupQuery {
  string unit_name = 1;
  // [Unit] Slice= 的值；SysS 留空，由 System A 用 IR 填充。
  string slice = 2;
}

// 对 `unit.cgroup` / `resource.cgroup` 的应答，按 request_id 回显。
message UnitCgroupResult {
  // 本机存在可用的 cgroup 后端（System R 有 backend）。
  bool available = 1;
  // cgroup_path 是本 unit 真实存在的 cgroup 目录。
  bool found = 2;
  // 绝对 cgroupfs 路径，例如 "/sys/fs/cgroup/system.slice/app.service"。
  string cgroup_path = 3;
  // 诊断信息（l10n），available/found 为 false 时才有意义。
  string error = 4;
}
```

| 跳 | 请求方法 | 应答方法 | 载荷 |
|---|---|---|---|
| SysS → SysA | `unit.cgroup` | `unit.cgroup_result` | `UnitCgroupQuery` / `UnitCgroupResult` |
| SysA → SysR | `resource.cgroup` | `resource.cgroup_result` | 同上（`slice` 已填） |

约定：

- 请求/应答的命名沿既有 `resource.prepare` → `resource.prepare_result`、
  `unit.define` → `unit.define_result` 的模式。
- `enable` 语义同 `resource.prepare`：`request_id` 由发起方分配，应答必须回显；
  单次请求无副作用，可安全重试（本设计不重试，失败即降级）。
- **能力发现复用现有 `WorkerRegistration.supports_resource_prepare`**：
  System A 已经用它找到"resource worker"（`scheduler/mod.rs` 里
  `find(|w| w.supports_resource_prepare)`），同一个 worker 就是 `resource.cgroup`
  的应答者。因此**不加新 flag**，只把该字段的注释扩为"resource worker：
  实现 `resource.prepare` 与 `resource.cgroup`"。
- 没有 SysR 连接时，System A **立刻**回 `available=false`（不走超时、不排队），
  这条路径对 SysS 就是"可选依赖不存在"。

## 4. System R：如何作答

`handle_cgroup_query`：

1. `controller.available() == false`（非 Linux、无 cgroup v2、容器内无挂载）→
   `{available:false, found:false, error:t_("no cgroup backend")}`。这是
   "全局不可用"，SysS 可据此在整个连接生命周期内跳过后续查询。
2. 计算路径：`ResourceWorker::cgroup_path(unit_name, slice)`。它已是**纯函数**
   （`slice` 为空按 `system.slice`，`.slice` 单位指向自身），不需要磁盘、不依赖
   registry 是否已释放。
3. 判断 `found`：registry 命中 → `true`；否则用 `controller.exists(path)`
   （`sysr-common` 增补一个只读方法，v2 后端即 `rel(path).is_dir()`；
   `NoopController` 返回 `false`）。**必须区分"目录存在但当前没有进程"
   与"目录不存在"**：启动瞬间、停止瞬间的 cgroup 可能为空，仍必须答 `found`。
4. 返回 registry 中的**unit 根 cgroup**（不是 Delegate 的 `DelegateSubgroup=`
   子目录）：杀进程要覆盖整棵子树，`ResourcePrepareResult.placement_path`
   是"进程放置点"，两者不是同一语义。

不做：不检查 unit 是否 active、不看提问者、不发信号、不写 `cgroup.kill`。

## 5. System A：纯中继

在 worker 会话循环（`ipc/server.rs`）新增两条分支：

- 收 `unit.cgroup`：decode → 取 `state.units[unit_name].unit.slice` 填进查询 →
  找 `supports_resource_prepare` 的 worker → 分配新的 `request_id`，把
  `oneshot::Sender<UnitCgroupResult>` 存进 `state.cgroup_query_txs`（与
  `resource_prepare_txs` 并列）→ 发 `resource.cgroup`。
  - 没有 SysR / 发送失败 / 超过 `CGROUP_QUERY_TIMEOUT`（建议 500 ms）→
    回 `available:false`（超时要先从 map 里摘掉 sender）。
  - 整个处理在 `tokio::spawn` 里做：worker 会话循环绝不等待另一个 worker。
- 收 `resource.cgroup_result`：按 `request_id` 从 `cgroup_query_txs` 取 sender，
  原样转发给发起查询的 worker，目标用 `env.source`（本会话已在注册时校验过），
  回显原 `request_id`。

`UnitCgroupResult` 在 System A 里**只被搬动，不被解释**：路径不进缓存、不进
`CachedUnitState`，避免出现第二份真相（System R 的 registry 才是权威）。

## 6. System S：停止时序

```
stop(unit):
  inst = registry[unit]; pid = inst.main_pid            # None → 直接完成
  path = inst.cgroup_path                               # 首次停止时为空
      ?? query_cgroup(unit).await                       # 200 ms 预算，失败即 None
  inst.state = Stopping
  signal(SIGTERM):
      命中 cgroup → <path>/**/cgroup.procs 里的每个 pid
      总是       → killpg(pid)（与 cgroup 结果取并集、去重）
  wait(deadline = TimeoutStopSec):
      主进程消失 且（无 cgroup 或子树已空：<path>/cgroup.events 的
      populated 为 0，内核过旧时退回轮询 cgroup.procs）
  signal(SIGKILL):
      命中 cgroup → 若 <path>/cgroup.kill 可写（内核 ≥ 5.14）写 "1"，
                    否则对重读到的每个 pid 发 SIGKILL
      总是       → killpg(pid) 再补一次
  reap / publish_state
```

降级矩阵：

| SysR 状态 | 查询结果 | SIGTERM 目标 | SIGKILL 目标 |
|---|---|---|---|
| 未连接 | `available=false` | main 的进程组 | main 的进程组 |
| 已连接 | `found=false` | main 的进程组 | main 的进程组 |
| 已连接 | `found=true` | cgroup 全子树 ∪ main 进程组 | `cgroup.kill` + 残留 ∪ main 进程组 |
| 超时/协议错误 | — | main 的进程组 | main 的进程组 |

细节：

- **并集而非二选一**：System R 把 pid 放进 cgroup 是在"active"事件之后，
  启动后极短窗口内 cgroup 可能还不含 main pid；只按 cgroup 杀会漏掉它。
- 查到的路径缓存在 `ServiceInstance`（同一 unit 的路径稳定），下次 stop/restart
  不再查询；失败不缓存。
- `available=false` 在连接生命周期内缓存为"本机无 cgroup 杀法"，避免每次
  stop 都问一次；worker 重连后清零。
- SysS 不引入 `systema-sysr*` 依赖：读 `cgroup.procs` / `cgroup.events`、写
  `cgroup.kill` 只是文件 I/O，
  放在 `crates/backend/systema-syss/src/cgroup.rs`，`cfg(any(target_os="linux",
  target_os="android"))`；其它平台整块编译掉，走进程组路径（符合"平台差异只降级、
  不编译失败"）。
- 日志：`available=false`/`found=false` 是正常降级，用 `debug!`；超时、解码失败
  用 `warn!`。

## 7. 必须先解决的两个既有缺陷

这两条不是"顺手改"，是本设计的**前置条件**（实现时已一并修掉）：

1. **`stop` 目前在 WorkerIpc 读者循环里内联执行**（`worker_ipc.rs::run_method`
   的 `deferred` 只包含 `start|restart|reload`）。SysS 在 `stop()` 里等待
   `unit.cgroup_result` 时，应答只能由同一个读者循环读入 → **自死锁**。
   修法：把 `"stop"` 加入 `deferred`，让它在 spawn 出去的任务里执行。
   附带收益：现在一次 30–90 s 的停止会阻塞整个 worker 会话（state ack、
   其它 unit 的 status 都被卡住），这本来就是 bug。
2. **服务进程没有自己的进程组**：`setsid()` 只出现在 TTY 分支
   （`process.rs::apply_tty`），普通服务与 SysS 同组。此时 `killpg(pid)`
   会把 SysS 自己（以及其它 unit）杀掉。修法：unix 上 spawn 时加
   `CommandExt::process_group(0)`（等价于子进程 `setpgid(0,0)`）。
   **注意两者互斥**：`setsid()` 在调用方已是进程组组长时返回 `EPERM`，
   而 `pre_exec` 闭包在 `setpgid` 之后运行，所以带 TTY 的 unit 必须
   跳过 `process_group(0)`（由 `setsid()` 顺带产生新进程组），
   否则服务根本启不起来；两条路径都以 `pgid == pid` 结束。

## 8. 时序与竞态

- **查询必须在状态上报之前**：SysS 一进入 `Stopping` 就会向 SysA 上报
  `deactivating`，SysA 随即把该状态投影给 SysR，SysR 会释放 registry 条目。
  先查后报可以让两者都还描述"运行中的 unit"。即便发生竞争，第 4 节的
  路径推导是纯函数，只有 `found` 会退化成 `false` → 降级，不会误杀。
- **不做跨 unit 误杀**：cgroup 路径来自 SysR，按 `unit_name + slice` 决定，
  与 pid 无关；进程组路径必须先校验 `getpgid(pid) == pid` 再 `killpg`。
- **pid 复用**：cgroup 路径下的 pid 在发信号前重新读一次 `cgroup.procs`；
  主进程路径先 `is_alive` + 进程组校验。与 systemd 的做法一致，不能完全消除，
  但窗口极小。
- **user 域实例**（`systema-sysu`）：路径形如
  `/sys/fs/cgroup/user.slice/...`，SysS 不解构、不假设前缀，原样使用。

## 9. 测试

已实现的用例（层内位置：`crates/backend/systema-syss/src/cgroup.rs`、
`crates/backend/systema-syss/src/process.rs::stop_tests`、
`crates/backend/systema-sysr/src/worker.rs`、`crates/backend/systema-sysa/src/scheduler/mod.rs`、
`crates/libsysa-ipc/src/worker_ipc.rs`）：

| 层 | 用例 |
|---|---|
| SysR 单测 | registry 命中 → 返回 registry 路径；未命中但 `(unit,slice)` 有效 → 推导路径；`NoopController` → `available=false`；空 slice 按 `system.slice`；`.slice` 单位指向自身 |
| SysA 单测 | 无 SysR → 立即 `available=false`；SysR 应答 → 路径与会话 `request_id` 原样回给发起者；SysR 超时 → `available=false` 且 sender 已清理；`slice` 来自 IR；路径不被改写 |
| SysS 单测 | 查询超时/失败 → 只走进程组；`found=true`（临时目录伪造 `cgroup.procs`）→ 逐个 pid 发信号；`available=false` 后不再查询 |
| 集成 | **未做**：真机验证需要 root 与真实 cgroupfs（`/sys/fs/cgroup` 挂载、
SysR 建目录），仓库内的测试不触碰系统状态；SysS 的停止路径改用临时目录伪造
`cgroup.procs` / `cgroup.events` / `cgroup.kill` 覆盖（见 `stop_tests`），
内容与真实接口同形 |

## 10. 明确不做

- 不解析 `KillMode=`：`ServiceConfig` 里没有这个字段，行为等价于
  "能查到 cgroup 时 control-group，否则 mixed"。将来要支持再扩 `ServiceConfig`。
- 不让 SysR 代发信号（那会把生命周期职责搬进 System R）。
- 不动 `UnitConfig.cgroup_path`（它只服务于 Delegate 的放置点，语义不同）。
- 不动控制面/`manager.*`、admin 面、`unit.state_update`、`UnitSnapshot`。
- 不覆盖 SysS 自身的 shutdown 路径（`main.rs` 的分阶段 SIGTERM/SIGKILL），
  那里同样按进程组即可，后续可选复用同一个 helper。
- 不给其它 worker（SysE scope、SysI 等）开放该协议，`unit.cgroup` 目前只有 SysS 调用。

## 11. 备选方案与取舍（已否决）

| 方案 | 否决原因 |
|---|---|
| SysA 在 start 时把路径塞进 `UnitConfig.cgroup_path` | 对普通服务，cgroup 在"active"事件后才被 System R 创建，start 时还没有路径；若为每个 start 都向 SysR 要路径，就给所有启动加了一跳依赖和延迟 |
| SysA 把路径拼在 stop 的 `MethodCall.args` 里下发 | 每次 stop 都要先做一次 SysR 往返（连"本来就不需要杀"的停止也要付），且改了 `MethodCall.args` 对 "status/stop 恒为空" 的既有契约 |
| SysA 缓存 System R 每秒推送的 `CgroupMetricsUpdate.control_group` | 那是相对的 control group，不是 fs 路径；SysA 需要知道挂载点才能用，破坏"System A 不碰 cgroup"的边界 |
| SysS 直接连 SysR | 破坏单向解耦，SysS 必须感知 worker 拓扑 |
| 新增 `supports_cgroup_query` 注册位 | 与 `supports_resource_prepare` 完全同源（同一 worker），增加协议面却无新信息 |

## 12. 落地顺序

1. ✅ `refactor(ipc): Defer stop calls out of the worker reader loop.`
2. ✅ `feat(sysr): Answer unit cgroup questions from the resource worker.`（含 proto 两消息、`exists()`、SysA 中继）
3. ✅ `feat(syss): Run every service in its own process group.`（TTY 分支走 `setsid()`，两者互斥）
4. ✅ `feat(syss): Prefer the unit cgroup when stopping a service.`
5. ✅ 契约条目补登 `doc/ipc-api-stability.md`（§9 的端到端集成测试除外，见上）。
