# send_envelope

所属模块：`libsystema_shared::ipc`

## 简介

把 `Envelope` 编码并写入分帧流。函数在返回前完成 flush，因此随后立即关闭
写端不会截断请求帧。

## 定义

```rust
pub async fn send_envelope(framed: &mut EnvelopeFramed, envelope: &Envelope) -> Result<()>
```

## 错误码

| 情况 | `ErrorCode` |
| --- | --- |
| protobuf 编码失败 | `EnvelopeEncodeFailed` |
| 写入分帧流失败 | `EnvelopeSendFailed` |

## 使用示例

```rust
use libsystema_shared::ipc::{frame_stream, make_envelope, send_envelope};

async fn register(stream: tokio::net::UnixStream) -> anyhow::Result<()> {
    let mut framed = frame_stream(stream);

    let env = make_envelope(
        0,
        "system-s-1",
        "system-a",
        "worker.register",
        libsystema_shared::proto::WorkerRegistration {
            worker_id: "system-s-1".to_string(),
            unit_types: vec!["service".to_string()],
            supports_unit_define: false,
            supports_resource_prepare: false,
        },
    )?;

    send_envelope(&mut framed, &env).await
}
```

## 相关 API

- [`recv_envelope`](./recv_envelope.md)：读取对端 envelope。
- [`make_envelope`](./make_envelope.md)：构造 envelope。
