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
| `events(&self) -> Arc<Mutex<ControlEventStream>>` | 取事件流。会话断开时接收端关闭（`recv()` 返回 `None`）。 |
| `flavor(&self) -> &str` | 握手时声明的桥身份。 |

## 使用示例

连接并完成握手：

```rust
use sysa::manager::ControlClient;

let socket_path = sysa::paths::instance().control_socket_path;
let client = ControlClient::connect(socket_path, "systemd", Some("system-w-1")).await?;
```

下发 start / stop / restart / reload：控制面统一走 `manager.enqueue`，由
`job_type` 区分动作。返回的 `job_id` 可通过 `job.*` 事件跟踪到终态。

```rust
use sysa::manager::ControlClient;
use sysa::proto::{EnqueueJobRequest, EnqueueJobResult};

async fn enqueue(
    client: &ControlClient,
    name: &str,
    job_type: &str,
) -> anyhow::Result<EnqueueJobResult> {
    let request = EnqueueJobRequest {
        name: name.to_string(),
        job_type: job_type.to_string(),
        mode: "replace".to_string(),
        reload_if_possible: false,
    };
    let reply: EnqueueJobResult = client.call("manager.enqueue", &request).await?;
    if !reply.success {
        anyhow::bail!("{}: {}", name, reply.message);
    }
    Ok(reply)
}
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
