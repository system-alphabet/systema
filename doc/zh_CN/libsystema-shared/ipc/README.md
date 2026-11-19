# ipc 模块综述

## 在 systema 中的位置

systema 的每个组件边界都是一条 Unix socket 上的 protobuf 会话：System A 与
worker 之间走工作负载面（`allocator.sock`），System A 与 SysW、SysI、运维 CLI
之间走控制面（`control.socket`），socket 激活再用一条独立的 fd 传递通道。

`ipc` 模块是这些会话的**线协议层**，也是整个工程里**唯一构建和解析帧的地方**。
它只关心“字节怎么变成信封、信封怎么发出去”，不解释任何业务方法。

## 线格式的几件事

1. **信封**：每条消息都带请求编号（0 表示单向事件）、来源、目标、方法名和
   protobuf 载荷。请求与应答按请求编号关联，事件复用同一条会话。
2. **分帧**：4 字节大端长度前缀 + 载荷，单帧上限 16 MiB。超过上限或长度非法
   的帧直接拒绝，避免内存被异常长度拖垮。
3. **版本协商**：新会话的**第一帧**必须是版本请求，服务端用自己的固定版本判定
   兼容性并回复版本结果；不兼容就返回结构化错误并关闭会话，而不是静默继续。
4. **两平面隔离**：控制面与工作负载面在 proto 层就已经分开，消息类型不能跨面
   引用；本模块只负责把各自平面的信封送出去。
5. **fd 传递**：SCM_RIGHTS 走一条独立的裸连接，不做信封分帧——一条消息只带
   一个 fd。它服务于 socket 激活：服务进程启动时向 System A 要监听 fd。
6. **会话收尾**：控制面客户端在关闭连接前会把剩余帧读干净（有超时上限），
   避免服务端因为对端突然关闭而把正常应答当成协议错误。

## 不负责什么

- 不决定方法语义、不做方法路由——那是各 worker 的运行循环（见
  [worker_ipc](../worker_ipc/README.md)）与 System A 的职责。
- 不渲染用户文案——失败以结构化错误码表达（见
  [error_code](../error_code/README.md)）。

## 与其它模块的关系

- [worker_ipc](../worker_ipc/README.md)：用本模块建立连接、发送/接收信封。
- [error_code](../error_code/README.md)：本模块所有失败的结构化表达。
- [ir](../ir/README.md)：信封载荷里的单元模型以 JSON 字节承载。

## 本目录文档

- [`EnvelopeFramed`](./EnvelopeFramed.md)、[`frame_stream`](./frame_stream.md)、
  [`make_envelope`](./make_envelope.md)、[`send_envelope`](./send_envelope.md)、
  [`recv_envelope`](./recv_envelope.md)：信封与分帧。
- [`connect_control`](./connect_control.md)：控制面连接。
- [`exchange_version`](./exchange_version.md)、[`IPC_VERSION`](./IPC_VERSION.md)、
  [`METHOD_VERSION`](./METHOD_VERSION.md)、
  [`METHOD_VERSION_RESULT`](./METHOD_VERSION_RESULT.md)：版本协商。
- [`send_fd`](./send_fd.md)、[`recv_fd`](./recv_fd.md)：SCM_RIGHTS 传递。
- [`finish_control_session`](./finish_control_session.md)、
  [`CONTROL_DRAIN_TIMEOUT`](./CONTROL_DRAIN_TIMEOUT.md)：会话收尾。
