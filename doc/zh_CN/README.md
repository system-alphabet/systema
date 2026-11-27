# System Alphabet 项目总览

本文介绍 **System Alphabet** 项目的定位、组成与协作方式，重点说明 **System
Allocator（调度器）**、**System Worker** 与 **Controller** 之间的关系。

> **命名区分**
>
> - **System Alphabet**：项目名，指本仓库描述的整套系统资源管理器，以及它的
>   组件集合（System A / S / K / … 组成的“字母表”）。
> - **System Allocator**（也简称 **System A**，二进制 `systema-sysa`）：项目内的
>   **调度器**组件。它只持有期望状态、解析依赖并派发任务，不直接管理进程。
>
> 下文出现“System A”时，指的都是这个调度器，而不是整个项目。

## 项目定位

System Alphabet 是一个**跨平台、事件驱动的系统资源管理器**。类似于 systemd
PID1 ，但它以 **non-pid1 的 systemd shim** 的形态工作（不过它在设计哲学上也
考虑作为其他服务管理器的shim层）：

- 把“引擎”拆成一组**单一职责、可替换的 System Worker**，每个 worker 只负责
  一类单元（服务、套接字、定时器、挂载……）；
- 把“控制器”拆成一个**单一职责、可替换的 System Wrapper**，包装器会把
  systema的控制接口转换为其他系统资源管理器
  （如 systemd `systemctl`,  SysVInit `service`）可以操作的接口形式。
- 把“调度”的逻辑提取为一个各个服务管理器共识性的调度器，它根据依赖拓扑结构，给服务的启动排序以确保引擎的执行结果符合人类系统管理员期望的语义。
- 既可以由 `systema-sysi` 作为 init 拉起整棵进程树，也可以让 System A 和各
  worker 作为普通进程分别启动（这体现了 System Alphabet 组件的可替换性）；
  例如，系统会话用`systema-sysi`启动，而用户会话另有一套 `systema-sysu`
  实例，而它们启动的systema二进制都是同样的。

## 组成结构

### 进程与连接视图

这里以 systemd 系列前端，和 SystemA 自己的直接执行器

```text
          systemctl / busctl / 桌面组件 / 其他管理工具
                              │  控制接口（如systemd 的 D-Bus org.freedesktop.systemd1）
                              ▼
                 ┌─────────────────────────────┐
                 │ systema-sysw.systemd (SysW) │ 兼容层：只做协议适配
                 └──────────────┬──────────────┘
                                │ control.socket（控制面：manager.* / admin.*）
                                ▼
   ┌────────────────────┐   ┌───────────────────────────────┐
   │ systema-sysi       │   │ systema-sysa                  │
   │ SysAInit           │──►│ System A / System Allocator   │
   │ 拉起 · 监督 · 退出 │   │ 期望状态 · 依赖 · 调度 · 派发 │
   └────────────────────┘   └──────────────┬────────────────┘
                                           │ allocator.sock（工作负载面）
                    ┌──────────────────────┼──────────────────────┐
                    ▼                     ▼                     ▼
              ┌───────────┐          ┌───────────┐          ┌──────────┐
              │ System S  │          │ System K  │   ……     │ System F │
              │ service   │          │ socket    │          │ Finder   │
              └───────────┘          └───────────┘          └──────────┘
每个 worker 实现 Controller 操作表（类似于 Linux VFS），并把实际状态推回 System Allocator
```

可以用 Linux 或者 BSD 的 VFS 来类比：

- 暴露给用户的`write`, `read`, `stat`, `close`这样的调用是 System Wrapper (`systema-sysw.*`)
- Linux 或者 BSD 内核是System Allocator (`systema-sysa`)
- 而文件系统驱动(`ext4`,  `f2fs`, `btrfs` )则是

### Allocator、Worker 与 Controller 的协作

```text
   Finder ──统一 IR（staging → commit）──► System A
                                            │
                        ① 依赖解析 + 事务调度（只计算期望状态）
                                            │
                        ② 按单元类型路由任务（method.call / job 事件）
                                            ▼
                          Worker（实现 Controller 操作表）
                                            │
                        ③ 执行：spawn / stop / 监听 / 定时 / 挂载……
                                            │
                        ④ 实际状态事件（unit.state_update）
                                            ▼
                          System A 状态缓存（只缓存，不产生事实）
                                            │
                        ⑤ control.socket 推送
                                            ▼
                          SysW ──► D-Bus 属性 / 信号 ──► systemctl
```

1. **Finder 生产模型**：只有 Finder 解析单元文件，产出统一 IR 并提交到暂存区；
   System A 消费解析结果，从不直接读单元文件。
2. **Allocator 做调度**：System A 维护依赖图与事务，决定“现在该启动/停止谁”，
   按单元类型把任务派发给注册了该类型的 worker。
3. **Worker 执行**：每个 worker 实现一张 **Controller 操作表**（状态查询、启动、
   停止、重启、重载、状态同步），是 System A 眼中该单元类型的全部能力。
4. **状态回推**：worker 在执行过程中产生真实状态（PID、退出码、子状态），以事件
   推回 System A；System A 只缓存，不推断进程事实。
5. **对外呈现**：SysW 把缓存映射成 systemd 的 D-Bus 属性和信号，兼容既有生态。

这套分工的等价类比是 Linux VFS：Controller 是驱动的操作表，worker 是驱动实现，
System A 是核心；新增或替换一种单元类型不需要改调度器。

## 组件一览

| 字母 | 系统 | 二进制 | 职责 |
| :---: | :---: | :---: | :---: |
| I | **SysAInit** | `systema-sysi` | init 形态的监督者：挂载 API 文件系统、拉起 System A 与各 worker、处理电源动作 |
| A | **System Allocator** | `systema-sysa` | 调度器：单元模型、依赖解析、事务调度、任务派发、期望状态 |
| S | **System S**（**S**ervice） | `systema-syss` | 服务进程的 fork/exec、生命周期状态机、孤儿回收、就绪等待 |
| K | **System K**（Soc**k**et） | `systema-sysk` | 套接字激活：监听、fd 传递、按连接拉起服务 |
| C | **System C**（**C**ron） | `systema-sysc` | 定时器：单调时钟与日历调度，触发 `timer.fired` |
| N | **System N**（**N**otify） | `systema-sysn` | 路径单元：文件系统条件监视 |
| D | **System D**（**D**evice） | `systema-sysd` | 设备单元：扫描 `/dev` 与 sysfs，注入设备单元 |
| E | **System E**（**E**xternal） | `systema-syse` | 作用域：包装外部创建的进程（`PIDs=`） |
| M | **System M**（**M**ount） | `systema-sysm.*` | 挂载与自动挂载 |
| R | **System R**（**R**esource） | `systema-sysr.linux` | cgroup v2 层级与资源限额，动态注册 slice |
| T | **System T**（**T**arget） | `systema-syst` | 目标单元：只做激活与状态跟踪 |
| P | **System P**（**P**laceholder） | `systema-placeholder` | 手动启动的占位 worker：用 `--manage` 指定管理的单元类型；SysAInit 不会启动它 |
| F | **System F**（**F**inder） | `systema-sysf` + finder 插件 | 解析单元文件并提交统一 IR |
| W | **System W** (**W**arper) | `systema-sysw.*` | 一套 |
| U | **System U**（**U**ser） | `systema-sysu` | 用户会话实例的启动与监督 |
| H | **System H**（**H**ello） | `systema-sysh.console` | 控制台/启动画面辅助 |

字母 **B** 未使用。System P 是占位 worker，与电源动作无关；电源动作仍由 SysAInit
在进程内直接处理。
手动启动时必须传入 `--manage=XXX:YYY` 指定要接管的单元类型（对应 `.XXX`、`.YYY`；
类型前的点可省略）。`--state` 默认为 `follow`：start 后 active、stop 后 inactive；
设为 `active` 或 `inactive` 时，两种操作后都报告该状态。`--default` 控制尚未发生
start/stop 时报告的状态，默认为 `inactive`。System P 不在 SysAInit 的启动列表中。

库的分层是：`libsystema-shared` 提供 IPC 契约与 worker 运行时，`libsystema-sysa`
只追加服务端握手逻辑，`libsysa` 再把两者重导出给全部二进制（保留历史路径）。

## 传输与会话

| 路径 | 服务端 | 客户端 | 用途 |
| --- | --- | --- | --- |
| `allocator.sock` | System A | 各 worker、Finder | 工作负载面：注册、方法调用、状态事件 |
| `control.socket` | System A | SysW、SysAInit、运维 CLI | 控制/运维面：`manager.*`、`admin.*`、作业与单元事件 |
| `notify/*.sock` | SysAInit 等监听者 | System A 广播 | `key=value` 就绪通知（`MANAGER_READY`、`WORKER_READY`） |
| `fdpass.sock` | System A | System S、System K | 裸 `SCM_RIGHTS` fd 传递（套接字激活） |

两个业务平面在 protobuf 层强制隔离：各自的消息只能引用公共模型，不能互相引用。

## 启动与监督

1. SysAInit 挂载 API 文件系统，拉起 System A 并等待 `MANAGER_READY`；
2. 串行拉起各 worker，每个都必须先报告 `WORKER_READY`；
3. 进入控制阶段：握手 → `daemon_reload`（System A 按需拉起 Finder）→ 注册电源
   动作 → 列出单元 → 启动默认目标（`default.target`）；
4. 转入稳态监督；退出时按宽限期停止子进程、回收孤儿并完成握手。

所有二进制都是**单线程异步**（`tokio` current-thread），没有 thread-per-service。

## 设计原则

1. **控制面与执行面分离**：System A 只持期望状态，真实状态由 worker 产生并回推。
2. **两平面 IPC**：工作负载面与控制/运维面各用一条 socket，各自独立演进。
3. **兼容性留在边缘**：systemd 的 D-Bus 面只在 SysW 实现，System A 与 worker
   不引入 `zbus` 依赖。
4. **单线程异步**：统一 `tokio` current-thread 运行时。
5. **平台能力降级而非编译失败**：平台相关实现拆到 `*-common` + 平台 crate，
   非 Linux 目标退化为 `NoopController` 之类的实现。

## 文档地图

- API 手册与模块综述：`doc/libsystema-shared/zh_CN/README.md`；
- 顶层设计/决策记录：`doc/*.md`（中文，背景阅读）；
- 单元文件搜索路径、构建依赖与运行方式：仓库根目录 `README.md`。

## 快速开始

```bash
cargo build --workspace

sudo mkdir -p /run/systema
sudo ./target/debug/systema-sysi          # 一次拉起 System A 与全部 worker

systemctl --system status sshd.service    # 用现有工具驱动
```

许可证见仓库根目录的 `LICENSE`。
