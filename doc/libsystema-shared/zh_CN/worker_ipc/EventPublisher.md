# EventPublisher

所属模块：`libsystema_shared::worker_ipc`

## 简介

在单元控制器内部发布状态更新与其它事件的句柄。它由
[`WorkerIpc::run`](./WorkerIpc.md) 的工厂闭包注入，通常保存在控制器里，这样
控制器随时都能发出 `unit.state_update` 等消息。

发布是异步的：`publish_unit_state_update` 序列化更新后启动后台任务发送，并等待
System A 的 `unit.state_update_ack`（5 秒超时、最多 3 次尝试，逐次记录失败，最后
放弃且不阻塞调用方）。因此它可以安全地在读取任务内部调用，而读取任务本身也负责
分发 ACK。

## 定义

```rust
#[derive(Clone)]
pub struct EventPublisher {
    // 私有字段
}

impl EventPublisher {
    pub fn new(
        tx: mpsc::UnboundedSender<bytes::Bytes>,
        worker_id: &str,
        pending_acks: Arc<Mutex<HashMap<u64, oneshot::Sender<UnitStateUpdateAck>>>>,
        pending_cgroups: Arc<Mutex<HashMap<u64, oneshot::Sender<UnitCgroupResult>>>>,
    ) -> Self;

    pub fn publish_unit_state_update(&self, units: Vec<UnitStatus>, full_snapshot: bool);

    pub async fn query_unit_cgroup(
        &self,
        unit_name: &str,
        timeout: Duration,
    ) -> Result<UnitCgroupResult, SysError>;

    pub fn send_envelope<P>(&self, method: &str, payload: P)
    where
        P: ProstMessage + Send + 'static;

    pub fn send_envelope_bytes(&self, method: &str, payload: Vec<u8>);

    pub fn send_reply<P>(&self, request_id: u64, method: &str, payload: P)
    where
        P: ProstMessage + Send + 'static;

    pub fn subscribe_units(&self, unit_names: &[String]);
    pub fn unsubscribe_units(&self, unit_names: &[String]);
}
```

## 方法

| 方法 | 说明 |
| --- | --- |
| `new` | 由 IPC 循环内部构造；一般不需要自己调用。 |
| `publish_unit_state_update` | 发布 `unit.state_update`。`full_snapshot=true` 表示这是本 worker 全部单元的完整快照（连接/重连时发送）。 |
| `query_unit_cgroup` | 向 System A 询问单元的 cgroup 位置（`unit.cgroup`）。`slice` 由 System A 从单元模型填充，调用方留空。超时或传输失败返回 `Err(SysError)`；System A 回答 `available=false` 属于 `Ok`。 |
| `send_envelope` | **即发即忘**地发送一个载荷经 prost 编码的 envelope（不等待 ACK），用于 `timer.fired` 这类单向通知。 |
| `send_envelope_bytes` | 发送载荷为**原始字节**（非 prost 编码）的 envelope，例如载荷是单元名的 `socket.request_fd`。用错方法会给载荷加长度前缀并破坏对端解析。 |
| `send_reply` | 回声传入的 `request_id` 发送回复，例如对 System A `unit.define` 请求的 `unit.define_result`。 |
| `subscribe_units` | 订阅指定单元的 `event.publish` 通知；空列表表示订阅全部。增量添加。 |
| `unsubscribe_units` | 取消订阅；空列表表示取消全部（包括“全部单元”订阅）。增量删除。 |

## 使用示例

```rust
use std::time::Duration;

use libsystema_shared::controller::UnitStatus;
use libsystema_shared::worker_ipc::EventPublisher;

fn mark_failed(publisher: &EventPublisher, name: &str) {
    publisher.publish_unit_state_update(
        vec![UnitStatus {
            unit_name: name.to_string(),
            active_state: "failed".to_string(),
            sub_state: "failed".to_string(),
            ..Default::default()
        }],
        false,
    );
}

async fn query_cgroup(publisher: &EventPublisher, name: &str) {
    match publisher
        .query_unit_cgroup(name, Duration::from_secs(2))
        .await
    {
        Ok(result) if result.available && result.found => {
            println!("cgroup = {}", result.cgroup_path);
        }
        Ok(_) => println!("resource worker has no cgroup for {name}"),
        Err(e) => println!("cgroup query failed: {e}"),
    }
}

fn subscribe_all(publisher: &EventPublisher) {
    publisher.subscribe_units(&[]);
}
```

## 相关 API

- [`WorkerIpc`](./WorkerIpc.md)：持有并注入本句柄。
- [`UnitStatus`](../controller/UnitStatus.md)：状态更新的载荷模型。
- [`ErrorCode`](../error_code/ErrorCode.md)：`CgroupQueryTimeout`、`RequestIdWraparound` 等。
