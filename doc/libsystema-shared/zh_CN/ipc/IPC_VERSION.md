# IPC_VERSION

所属模块：`libsystema_shared::ipc`

## 简介

本构建中 envelope / 分帧线协议层的版本号。分配器目前只固定一个版本：宣告其它
版本的 peer 会收到 `ipc.version.result{compatible:false}`，并在任何会话语义
（hello、注册、admin 请求）开始前被断开。未来可扩展为 min/max 协商
（见 `doc/ipc-api-stability.md` §4.1）。

## 定义

```rust
pub const IPC_VERSION: u32 = 1;
```

## 使用示例

```rust
use libsystema_shared::ipc::{exchange_version, frame_stream, IPC_VERSION};

async fn handshake(path: &str) -> anyhow::Result<()> {
    let mut framed = frame_stream(tokio::net::UnixStream::connect(path).await?);
    let server = exchange_version(&mut framed, "client").await?;
    assert_eq!(server, IPC_VERSION);
    Ok(())
}
```

## 相关 API

- [`exchange_version`](./exchange_version.md)：使用本常量完成协商。
- [`METHOD_VERSION`](./METHOD_VERSION.md) / [`METHOD_VERSION_RESULT`](./METHOD_VERSION_RESULT.md)：协商方法名。
