# worker_ipc 模块综述

## 在 systema 中的位置

每个 System Worker 都是一个独立进程：System S 管服务进程，System K 管套接字，
System C 管定时器……它们与 System A 之间只通过一条工作负载连接通信。

`worker_ipc` 就是 worker 二进制的“内核”：worker 作者只实现
[Controller 操作表](../controller/README.md)，然后调用一次运行循环，连接、注册、
任务分发、状态上报、重连和退出握手全部由本模块完成。这相当于 Linux VFS 模型里
“驱动只写 ops 表，其余由核心提供”。

## 它替 worker 做了什么

- **连接与注册**：连接 `allocator.sock`，用唯一的 `worker_id` 注册自己管理的
  单元类型，并报告就绪；System I 串行启动 worker，正是等待这条就绪通知。
- **自动重连**：System A 重启或连接断开时按退避重连；每次成功连接后调用
  `on_reconnect` 钩子，便于清理只对单条连接有效的缓存。
- **任务分发**：把 System A 的方法调用和 job 事件交给 Controller 操作表执行。
  耗时操作（如 `Type=notify` 的启动要等 READY）在独立任务中运行，不会卡住
  接收循环。
- **状态与事件上报**：提供状态推送通道（`unit.state_update` 等），带 ACK 与
  重试；也支持按单元订阅，只收关心的推送。定时器触发等主动事件走同一条通道。
- **能力声明**：通过构建期开关声明是否支持 `unit.define`、`resource.prepare`
  等可选能力。未声明的能力 System A 不会派发，worker 因此可以只实现最小集合。
- **fd 传递**：socket 激活需要把监听 fd 交给服务进程；本模块是 fdpass 通道的
  客户端，按需向 System A 请求 fd。
- **优雅退出**：接收循环**不监听任何进程信号**。需要 SIGTERM/SIGINT 语义的
  worker 由调用方把信号转发成退出请求（`ShutdownHandle`）。循环在边界处执行
  本地清理（连接仍保持）→ 发送 `worker.exit` → 等待 System A 关闭连接 → 退出；
  等待有超时上限，绝不会拖过 System I 的关停宽限期。

## 与 System I 的契约

worker 是被监督进程：System I 等它报告就绪后才启动下一个 worker；一旦某个
常驻 worker 退出，System I 会转入整体收敛退出。因此 worker 不应自行 `exit`，
而应通过退出握手下线。

## 与其它模块的关系

- [controller](../controller/README.md)：分发的目标操作表。
- [ipc](../ipc/README.md)：信封、分帧与版本握手的线协议实现。
- [error_code](../error_code/README.md)：任务失败时使用的结构化错误。
- [ir](../ir/README.md)：Finder 用它提交单元，worker 用 `UnitConfig` 接收
  “本类型执行说明”。

## 本目录文档

- [`WorkerIpc`](./WorkerIpc.md)：运行循环与配置钩子。
- [`ShutdownHandle`](./ShutdownHandle.md)：跨任务的退出请求。
- [`EventPublisher`](./EventPublisher.md)：状态、事件与查询的发送端。
- [`UnitFailure`](./UnitFailure.md)：区分“单元自身失败”与“任务级故障”。
