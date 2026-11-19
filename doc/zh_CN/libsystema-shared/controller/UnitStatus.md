# UnitStatus

所属模块：`libsystema_shared::controller`

## 简介

单元的标准运行时状态模型。它是 `status()` 方法的返回值，也是
`unit.state_update` 消息中原样承载的载荷，用于在 worker 与 System A 之间
同步单元状态。模型本身不依赖服务端 crate，worker 可以直接构造、序列化和
反序列化。

## 定义

```rust
#[derive(Debug, Clone, Default)]
pub struct UnitStatus {
    pub unit_name: String,
    pub active_state: String,
    pub sub_state: String,
    pub main_pid: u32,
    pub invocation_id: String,
    pub extensions: HashMap<String, String>,
}
```

## 字段

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `unit_name` | `String` | 单元名，例如 `nginx.service`。 |
| `active_state` | `String` | 活动状态：`active`、`inactive`、`activating`、`deactivating`、`failed`。 |
| `sub_state` | `String` | 子状态，worker 特定，例如 `running`、`mounted`、`listening`、`dead`。 |
| `main_pid` | `u32` | 主进程 PID；未运行或不适用时为 `0`。 |
| `invocation_id` | `String` | 当前激活的调用 ID（UUID v4），没有时为空串。 |
| `extensions` | `HashMap<String, String>` | 不属于公共模式的扩展属性，例如 `exec_main_code`、`n_accepted`。 |

## 方法

| 签名 | 说明 |
| --- | --- |
| `pub fn from_proto(proto: proto::UnitStatus) -> Self` | 从 protobuf `UnitStatus` 消息构造。 |
| `pub fn into_proto(self) -> proto::UnitStatus` | 转换为 protobuf `UnitStatus` 消息。 |
| `pub fn encode_to_vec(&self) -> Vec<u8>` | 序列化为 protobuf 字节；编码失败时返回空 `Vec`。 |
| `pub fn decode_from(bytes: &[u8]) -> Option<Self>` | 从 protobuf 字节反序列化；失败时返回 `None`。 |

## 使用示例

```rust
use libsystema_shared::controller::UnitStatus;

fn main() {
    let status = UnitStatus {
        unit_name: "nginx.service".to_string(),
        active_state: "active".to_string(),
        sub_state: "running".to_string(),
        main_pid: 4242,
        invocation_id: "6f1c1b2e-0b4e-4f1a-9c3d-2f5d8a1e7c90".to_string(),
        extensions: [("n_accepted".to_string(), "12".to_string())]
            .into_iter()
            .collect(),
    };

    // 编成字节，随 method.result / unit.state_update 发送。
    let bytes = status.encode_to_vec();
    let decoded = UnitStatus::decode_from(&bytes).expect("valid payload");

    assert_eq!(decoded.unit_name, "nginx.service");
    assert_eq!(decoded.main_pid, 4242);
}
```

worker 在 `UnitController::status` 中返回该类型，IPC 循环会自动
`encode_to_vec()` 后放进 `method.result`：

```rust
use anyhow::Result;
use libsystema_shared::controller::UnitStatus;

fn status_of(name: &str) -> Result<UnitStatus> {
    Ok(UnitStatus {
        unit_name: name.to_string(),
        active_state: "inactive".to_string(),
        sub_state: "dead".to_string(),
        ..Default::default()
    })
}
```

## 相关 API

- [`UnitController`](./UnitController.md)：产生 `UnitStatus` 的 trait。
- [`EventPublisher`](../worker_ipc/EventPublisher.md)：通过状态更新把 `UnitStatus` 推送给 System A。
