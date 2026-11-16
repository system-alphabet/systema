# frame_stream

所属模块：`libsystema_shared::ipc`

## 简介

用一个长度分帧编解码器包装 `UnixStream`，得到 [`EnvelopeFramed`](./EnvelopeFramed.md)。
单帧最大 16 MiB。

## 定义

```rust
pub fn frame_stream(stream: UnixStream) -> EnvelopeFramed
```

## 参数与返回

| 项 | 说明 |
| --- | --- |
| `stream` | 已连接的 tokio Unix 流。 |
| 返回 | 带 `LengthDelimitedCodec`（`max_frame_length = 16 * 1024 * 1024`）的 `Framed`。 |

## 使用示例

```rust
use libsystema_shared::ipc::{frame_stream, recv_envelope, send_envelope};

async fn round_trip(stream: tokio::net::UnixStream) -> anyhow::Result<()> {
    let mut framed = frame_stream(stream);
    let request = libsystema_shared::ipc::make_envelope(
        1,
        "client",
        "system-a",
        "manager.hello",
        (),
    )?;
    send_envelope(&mut framed, &request).await?;

    if let Some(reply) = recv_envelope(&mut framed).await? {
        println!("reply method = {}", reply.method);
    }
    Ok(())
}
```

## 相关 API

- [`EnvelopeFramed`](./EnvelopeFramed.md)：返回类型。
- [`connect_control`](./connect_control.md)：常用的建连入口。
