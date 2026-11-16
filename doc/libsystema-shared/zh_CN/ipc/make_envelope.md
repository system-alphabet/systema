# make_envelope

所属模块：`libsystema_shared::ipc`

## 简介

把任意 prost 消息编码成 payload，构造一个 `Envelope`（请求或事件）。

## 定义

```rust
pub fn make_envelope(
    request_id: u64,
    source: impl Into<String>,
    target: impl Into<String>,
    method: impl Into<String>,
    payload: impl Message,
) -> Result<Envelope>
```

## 参数与返回

| 参数 | 说明 |
| --- | --- |
| `request_id` | 请求 ID；单向事件用 `0`。 |
| `source` | 发送方标识，如 `system-a`、`system-s`。 |
| `target` | 接收方标识，如 `system-a`。 |
| `method` | 方法/事件名，如 `worker.register`、`method.call`。 |
| `payload` | 任意 prost 消息；编码失败返回 `PayloadEncodeFailed`。 |

## 使用示例

```rust
use libsystema_shared::ipc::make_envelope;

fn exit_envelope() -> anyhow::Result<libsystema_shared::proto::Envelope> {
    make_envelope(
        0,
        "system-s-1",
        "system-a",
        "worker.exit",
        libsystema_shared::proto::WorkerExit {
            reason: "SIGTERM".to_string(),
        },
    )
}
```

注意：若载荷在线上是**原始字节**而非 prost 消息（例如 `socket.request_fd` 的
载荷是单元名），不要用 `make_envelope`；请用
[`EventPublisher::send_envelope_bytes`](../worker_ipc/EventPublisher.md)。

## 相关 API

- [`send_envelope`](./send_envelope.md)：编码发送完整的 `Envelope`。
- [`exchange_version`](./exchange_version.md)：首次会话的版本协商。
- [`EnvelopeFramed`](./EnvelopeFramed.md)：承载 envelope 的流。
