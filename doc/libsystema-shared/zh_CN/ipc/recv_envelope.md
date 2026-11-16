# recv_envelope

所属模块：`libsystema_shared::ipc`

## 简介

从分帧流读取下一个 `Envelope`。流关闭时返回 `Ok(None)`；帧读取或 protobuf
解码失败时返回 `Err`。

## 定义

```rust
pub async fn recv_envelope(framed: &mut EnvelopeFramed) -> Result<Option<Envelope>>
```

## 返回与错误码

| 情况 | 结果 |
| --- | --- |
| 正常读到一帧 | `Ok(Some(Envelope))` |
| 对端关闭流 | `Ok(None)` |
| 读取帧失败 | `Err`，`FrameReceiveFailed` |
| 帧不是合法 `Envelope` | `Err`，`EnvelopeDecodeFailed` |

## 使用示例

```rust
use libsystema_shared::ipc::{frame_stream, recv_envelope};

async fn wait_for(stream: tokio::net::UnixStream) -> anyhow::Result<()> {
    let mut framed = frame_stream(stream);
    match recv_envelope(&mut framed).await? {
        Some(env) => println!("method = {}, payload = {} bytes", env.method, env.payload.len()),
        None => println!("peer closed the connection"),
    }
    Ok(())
}
```

## 相关 API

- [`send_envelope`](./send_envelope.md)：发送 envelope。
- [`EnvelopeFramed`](./EnvelopeFramed.md)：承载流类型。
