# EnvelopeFramed

所属模块：`libsystema_shared::ipc`

## 简介

`EnvelopeFramed` 是“已套上长度分帧编解码器的 Unix 流”的类型别名。所有
`connect_control` / `frame_stream` 得到的连接都用它来承载 protobuf
`Envelope`。4 字节大端长度前缀由 `LengthDelimitedCodec` 负责。

worker 主循环内部使用带读写拆分的 `FramedRead` / `FramedWrite`；本别名用于
一次性的客户端会话（finder、staging admin、unitstate admin、控制总线）。

## 定义

```rust
pub type EnvelopeFramed = Framed<UnixStream, LengthDelimitedCodec>;
```

## 使用示例

```rust
use futures::SinkExt;

use libsystema_shared::ipc::{frame_stream, make_envelope};

async fn send_ping(path: &str) -> anyhow::Result<()> {
    let stream = tokio::net::UnixStream::connect(path).await?;
    let mut framed: libsystema_shared::ipc::EnvelopeFramed = frame_stream(stream);

    let env = make_envelope(1, "client", "system-a", "manager.ping", ())?;
    let mut buf = bytes::BytesMut::new();
    prost::Message::encode(&env, &mut buf)?;
    framed.send(buf.freeze()).await?;
    Ok(())
}
```

## 相关 API

- [`frame_stream`](./frame_stream.md)：构造本类型。
- [`send_envelope`](./send_envelope.md) / [`recv_envelope`](./recv_envelope.md)：读写 envelope。
- [`connect_control`](./connect_control.md)：连接控制端口。
