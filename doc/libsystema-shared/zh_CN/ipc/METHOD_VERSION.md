# METHOD_VERSION

所属模块：`libsystema_shared::ipc`

## 简介

任何新会话的第一个 envelope 的方法名——客户端 → 服务端，在 hello 之前。
载荷是 `proto::IpcVersionRequest { version, client }`。

## 定义

```rust
pub const METHOD_VERSION: &str = "ipc.version";
```

## 使用示例

```rust
use libsystema_shared::ipc::{make_envelope, METHOD_VERSION, IPC_VERSION};

fn version_request(client: &str) -> anyhow::Result<libsystema_shared::proto::Envelope> {
    make_envelope(
        0,
        client,
        "system-a",
        METHOD_VERSION,
        libsystema_shared::proto::IpcVersionRequest {
            version: IPC_VERSION,
            client: client.to_string(),
        },
    )
}
```

通常无需手工构造，直接调用 [`exchange_version`](./exchange_version.md) 即可。

## 相关 API

- [`METHOD_VERSION_RESULT`](./METHOD_VERSION_RESULT.md)：对应的裁决方法名。
- [`IPC_VERSION`](./IPC_VERSION.md)：请求中携带的版本。
