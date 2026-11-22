# manager 模块综述

## 在 systema 中的位置

`manager` 是**控制面客户端**。System A 在 `control.socket` 上提供一对多的控制
总线，SysW（D-Bus 桥）、SysI 与运维工具都通过它下发 `manager.*` 请求，并接收
System A 主动推送的单元与 job 事件。

本模块只负责“会话与信封”：

- 建立连接并完成 `ipc.version` → `manager.hello` 两级握手；
- 把 `manager.*` 请求编码成信封，按请求编号等待应答；
- 把服务端推送的信封分类为 [`ControlEvent`](./ControlEvent.md)，交给上层维护镜像。

## 职责边界

负责：

- 连接、版本协商、hello 身份声明；
- RPC 请求/应答关联，`manager.error` 到 `anyhow::Error` 的映射；
- 事件分类（`unit.*`、`job.*`）。

不负责：

- 单元状态镜像、D-Bus 对象与信号——那是 SysW flavor crate 的职责；
- 重连策略——会话关闭后由调用方自行重连并重建镜像；
- 服务端方法与调度——实现在 `systema-sysa`。

## 会话生命周期

```text
connect(socket_path)
  │ ipc.version / ipc.version.result
  │ manager.hello / manager.hello.result
  ▼
ControlClient ──call("manager.*")──► System A
  ▲                                   │
  └──── ControlEvent（unit.* / job.*）─┘
  │
  └─ 断线：事件流关闭、在途 RPC 全部失败 → 调用方重连
```

## 本目录文档

- [`ControlClient`](./ControlClient.md)：连接、RPC 与事件流。
- [`ControlEvent`](./ControlEvent.md)：事件分类与 `ControlEventKind`。
- [`calls`](./calls.md)：常用 `manager.*` 的便捷封装。

线协议细节见 [ipc](../ipc/README.md)，错误码见
[error_code](../error_code/README.md)。
