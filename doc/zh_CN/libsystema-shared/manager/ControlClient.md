# ControlClient

所属模块：`libsystema_shared::manager`

## 简介

一条已建立的**控制面会话**。每个桥进程持有一个实例：构造时完成
`ipc.version` 与 `manager.hello` 握手，随后可用 [`call`](#方法) 下发
`manager.*` 请求，并通过 [`events`](#方法) 消费 System A 推送的生命周期事件。

`ControlClient` 不负责重连：会话断开后事件流关闭、在途请求全部失败，调用方
重新 `connect` 并重建镜像即可。

## 定义

```rust
pub struct ControlClient {
    // 私有字段
}

pub type ControlEventStream = mpsc::UnboundedReceiver<ControlEvent>;
```

## 方法

| 签名 | 说明 |
| --- | --- |
| `connect(socket_path: &str, flavor: &str, worker_id: Option<&str>) -> Result<ControlClient>` | 连接控制 socket，依次完成版本协商与 `manager.hello`。`flavor` 是桥身份（如 `"systemd"`）；`worker_id` 供受监督的桥声明身份（如 `"system-w-1"`）。 |
| `connect_on(stream: UnixStream, flavor: &str, worker_id: Option<&str>) -> Result<ControlClient>` | 在已建立的流上完成 hello 握手并启动读写任务；用于嵌入方自备 socket 或 socketpair 测试。 |
| `call<Q, R>(&self, method: &str, request: &Q) -> Result<R>` | 发送一个 `manager.*` 请求并等待应答。`Q` 为请求消息，`R` 为应答消息；服务端回 `manager.error` 时转为 `anyhow::Error`。 |
| `call_raw(&self, method: &str, request: &[u8]) -> Result<Vec<u8>>` | 以已编码的 protobuf 载荷发送请求并返回原始应答载荷；供 protobuf 类型在本 crate 之外的嵌入方（如 C API）使用。 |
| `enqueue<S: AsRef<str>>(&self, names: &[S], job_type: &str, mode: &str, reload_if_possible: bool) -> Result<EnqueueJobResult>` | 通用 job 入队：对 `names` 中每个单元下发 `job_type`，作业模式为 `mode`。是下列固定动作方法的共同实现。 |
| `start<S: AsRef<str>>(&self, names: &[S]) -> Result<EnqueueJobResult>` | 入队 `start` job（模式 `replace`）。 |
| `stop<S: AsRef<str>>(&self, names: &[S]) -> Result<EnqueueJobResult>` | 入队 `stop` job（模式 `replace`）。 |
| `restart<S: AsRef<str>>(&self, names: &[S]) -> Result<EnqueueJobResult>` | 入队 `restart` job（模式 `replace`）。 |
| `reload<S: AsRef<str>>(&self, names: &[S]) -> Result<EnqueueJobResult>` | 入队 `reload` job（模式 `replace`）。 |
| `try_restart<S: AsRef<str>>(&self, names: &[S]) -> Result<EnqueueJobResult>` | 入队 `try-restart` job（模式 `replace`）；未运行的单元坍缩为 no-op。 |
| `reload_or_restart<S: AsRef<str>>(&self, names: &[S]) -> Result<EnqueueJobResult>` | 入队 `reload-or-restart` job（模式 `replace`，`reload_if_possible = true`）；优先 reload，失败再 restart。 |
| `events(&self) -> Arc<Mutex<ControlEventStream>>` | 取事件流。会话断开时接收端关闭（`recv()` 返回 `None`）。 |
| `flavor(&self) -> &str` | 握手时声明的桥身份。 |

## 使用示例

连接并完成握手：

```rust
use sysa::manager::ControlClient;

let socket_path = sysa::paths::instance().control_socket_path;
let client = ControlClient::connect(socket_path, "systemd", Some("system-w-1")).await?;
```

下发 start / stop / restart / reload：固定动作方法直接封装了 `manager.enqueue`，
无需再手工构造 `EnqueueJobRequest`。返回的 `job_id` 可通过 `job.*` 事件跟踪到终态。

```rust
use sysa::manager::ControlClient;

async fn start_units(client: &ControlClient) -> anyhow::Result<()> {
    let reply = client.start(&["nginx.service", "sshd.service"]).await?;
    if !reply.success {
        anyhow::bail!("start failed: {}", reply.message);
    }
    Ok(())
}
```

需要自定义作业模式或 `reload_if_possible` 时仍可用通用入口：

```rust
let reply = client
    .enqueue(&["nginx.service"], "stop", "fail", false)
    .await?;
```

消费事件：

```rust
let events = client.events();
let mut rx = events.lock().await;
while let Some(event) = rx.recv().await {
    match event.kind {
        sysa::manager::ControlEventKind::JobCompleted => { /* 更新 job 镜像 */ }
        sysa::manager::ControlEventKind::UnitChanged => { /* 更新单元镜像 */ }
        _ => {}
    }
}
```

## 注意事项

- `connect` 需要显式传入 socket 路径：路径注册表在 `sysa`，而 shared 不反向依赖它。
- 请求编号从 `1` 递增；`0` 保留给单向事件。
- 事件流是唯一的消费端，用 `Arc<Mutex<_>>` 共享是因为 `ControlClient` 本身会被多处持有。
- 同一个会话上的 `call` 可并发，应答按请求编号分发。

## 相关 API

- [`ControlEvent`](./ControlEvent.md)：事件模型与分类。
- [`calls`](./calls.md)：快照、job 列表等常用请求的封装。
- [`ipc`](../ipc/README.md)：分帧、版本协商与信封工具。
