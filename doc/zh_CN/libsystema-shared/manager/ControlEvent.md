# ControlEvent

所属模块：`libsystema_shared::manager`

## 简介

System A 推到控制会话的信封分两类：**请求应答**（`request_id != 0`，由
`ControlClient` 内部路由到对应的 `call`）与**单向事件**（`request_id == 0`）。
本模块把后者分类为 `ControlEvent`，交由上层维护单元与 job 镜像。

## 定义

```rust
#[derive(Debug)]
pub struct ControlEvent {
    pub kind: ControlEventKind,
    pub envelope: Envelope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlEventKind {
    UnitChanged,
    UnitNew,
    UnitRemoved,
    UnitMetrics,
    JobNew,
    JobCompleted,
}
```

## 事件分类

| `ControlEventKind` | 信封方法 | `envelope.payload` |
| --- | --- | --- |
| `UnitChanged` | `unit.changed` | `UnitSnapshot`：配置或活动状态变化 |
| `UnitNew` | `unit.new` | `UnitSnapshot`：单元载入内存 |
| `UnitRemoved` | `unit.removed` | `UnitRemovedEvent`：单元卸载 |
| `UnitMetrics` | `unit.metrics` | `UnitCgroupMetrics`：cgroup 指标 |
| `JobNew` | `job.new` | `JobEvent`：新 job 入队 |
| `JobCompleted` | `job.completed` | `JobEvent`：job 到达终态，`result` 已填 |

## 方法

| 签名 | 说明 |
| --- | --- |
| `kind.method() -> &'static str` | 该事件对应的信封方法名。 |
| `ControlEventKind::from_method(method: &str) -> Option<Self>` | 把方法名分类；不是控制事件时返回 `None`（例如请求应答）。 |
| `kind.is_job_completed() -> bool` | `JobCompleted` 的便捷判断。 |

## 使用示例

```rust
use sysa::manager::ControlEventKind;
use sysa::proto::JobEvent;
use prost::Message;

while let Some(event) = rx.recv().await {
    match event.kind {
        ControlEventKind::JobCompleted => {
            let job = JobEvent::decode(event.envelope.payload.as_slice())?;
            println!("job {} on {} -> {}", job.job_id, job.unit_name, job.result);
        }
        ControlEventKind::UnitRemoved => {
            // 从镜像移除该单元
        }
        _ => {}
    }
}
```

## 相关 API

- [`ControlClient`](./ControlClient.md)：`events()` 是事件流的来源。
- [`calls`](./calls.md)：初始镜像通常先用快照请求填充，再接事件流增量更新。
