# exchange_version

所属模块：`libsystema_shared::ipc`

## 简介

客户端在会话最开始（`manager.hello` 或 `worker.register` 之前）执行的前置版本
协商：发送 [`IPC_VERSION`](./IPC_VERSION.md) 并等待服务端裁决。

只有在回复为 `ipc.version.result`、其 `compatible` 为真、且回显的服务端版本
是本端会说的版本（目前要求相等）时才算兼容。其它任何回复——`manager.error`、
旧服务端的未知方法拒绝、EOF——都视为不兼容，调用方必须丢弃该流。函数返回
服务端版本，便于调用方记录协商结果。

## 定义

```rust
pub async fn exchange_version(framed: &mut EnvelopeFramed, client: &str) -> Result<u32>
```

## 错误码

| 情况 | `ErrorCode` |
| --- | --- |
| 对端在回答前关闭 | `VersionPeerClosed` |
| 首个回复不是 `ipc.version.result` | `VersionUnexpectedReply`（`method`） |
| `ipc.version.result` 载荷无法解码 | `VersionResultUnreadable` |
| 不兼容（`compatible=false` 或版本不等） | `IncompatibleVersion`（`mine`、`theirs`、`message`） |

## 使用示例

```rust
use libsystema_shared::ipc::{exchange_version, frame_stream};

async fn handshake(path: &str) -> anyhow::Result<u32> {
    let stream = tokio::net::UnixStream::connect(path).await?;
    let mut framed = frame_stream(stream);
    let server_version = exchange_version(&mut framed, "staging-admin").await?;
    println!("negotiated IPC version {server_version}");
    Ok(server_version)
}
```

## 相关 API

- [`IPC_VERSION`](./IPC_VERSION.md)：本构建的线协议版本。
- [`METHOD_VERSION`](./METHOD_VERSION.md) / [`METHOD_VERSION_RESULT`](./METHOD_VERSION_RESULT.md)：方法名常量。
