# controller 模块综述

## 在 systema 中的位置

systema 按单元类型拆分 worker：System S 管 `service`，System K 管 `socket`，
System C 管 `timer`……每个 worker 都要回答同一个问题：**“这类单元怎么启动、
停止和汇报状态？”**

`controller` 模块把这个答案定义成一张**操作表**（`UnitController`），再加两个
跨进程共享的模型：单元状态与单元配置。System A 只认识这张表，不认识具体单元
类型，因此 worker 可以独立替换、增删，而 System A 无需改动。

## 三个角色

1. **操作表**：每个 worker 实现 `UnitController`，提供 `status`、`start`、
   `stop`、`restart`、`reload` 以及可选的 `sync_state`。System A 收到单元任务后，
   按“谁注册了该单元类型”把任务路由过去，并以泛型方式调用这些操作。
2. **状态模型**：`UnitStatus` 是 worker 向 System A 汇报运行状态的统一格式
   （active/sub 状态、主 PID、invocation id 等）。System A 只缓存 worker 推送的
   状态，从不自行推断进程事实；SysW 再把缓存映射成 systemd 的 D-Bus 属性。
3. **配置载体**：`UnitConfig` 是 System A 派发任务时携带的“执行说明”——单元名、
   扁平指令字典（类型专属 section）、需要请求的 socket 单元、以及可选的 cgroup
   路径。worker 用它解码出本类型的指令，再执行。

## 失败语义

worker 的失败分两类，边界在 Controller 上：

- **单元自身失败**：例如服务主进程正常退出但退出码非 0。这属于“job 失败、
  单元进入 failed”，System A 不应把它当成 worker 故障告警。
- **任务级故障**：worker 内部错误、协议错误、超时等。System A 会按任务失败
  记录并告警。

worker 通过共享的失败标记把第一类与第二类区分开，使 System A 的日志和 job 结果
各自准确。

## 与其它模块的关系

- [worker_ipc](../worker_ipc/README.md)：把收到的任务分发给本模块的操作表，
  并把结果、状态和事件送回 System A。
- [ipc](../ipc/README.md)：负责任务与状态在网络上的编码、分帧和传递。
- [ir](../ir/README.md)：Finder 侧使用的单元模型；System A 把其中的类型专属
  部分转成 `UnitConfig` 的扁平指令字典后交给 worker。
- [error_code](../error_code/README.md)：操作失败时跨边界传递的结构化错误。

## 本目录文档

- [`UnitController`](./UnitController.md)：worker 操作表。
- [`UnitStatus`](./UnitStatus.md)：运行状态模型。
- [`decode_unit_config`](./decode_unit_config.md)：把任务携带的字节解码成
  `UnitConfig`。
