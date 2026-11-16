# METHOD_VERSION_RESULT

所属模块：`libsystema_shared::ipc`

## 简介

对 [`METHOD_VERSION`](./METHOD_VERSION.md) 的裁决方法名——服务端 → 客户端，
回显请求的 `request_id`。载荷是 `proto::IpcVersionResult`，其中 `compatible`
表示是否兼容，`version` 是服务端版本，`message` 供日志参考。

## 定义

```rust
pub const METHOD_VERSION_RESULT: &str = "ipc.version.result";
```

## 使用示例

```rust
use libsystema_shared::ipc::{recv_envelope, METHOD_VERSION_RESULT};

async fn expect_verdict(framed: &mut libsystema_shared::ipc::EnvelopeFramed) -> anyhow::Result<bool> {
    let env = recv_envelope(framed)
        .await?
        .ok_or_else(|| anyhow::anyhow!("peer closed before verdict"))?;
    Ok(env.method == METHOD_VERSION_RESULT)
}
```

兼容性判断应交给 [`exchange_version`](./exchange_version.md)，它同时校验方法名、
`compatible` 标志和版本号。

## 相关 API

- [`METHOD_VERSION`](./METHOD_VERSION.md)：请求方法名。
- [`exchange_version`](./exchange_version.md)：完整的客户端协商。
