# UnitStateResult

所属模块：`libsystema_shared::unitstate_admin`

## 简介

`admin.unitstate` 流结束时（收到 `admin.unitstate.eof` 哨兵）返回的结果，
携带服务端统计的条目总数和一条人类可读消息。

## 定义

```rust
#[derive(Debug, Clone)]
pub struct UnitStateResult {
    pub total: u32,
    pub message: String,
}
```

## 字段

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `total` | `u32` | 服务端流式发送的单元条目数；失败时为 `0`。 |
| `message` | `String` | 人类可读消息；成功时为空串。 |

## 使用示例

```rust
use libsystema_shared::unitstate_admin::{UnitStateAdmin, UnitStateResult};

async fn collect(admin: &UnitStateAdmin) -> anyhow::Result<UnitStateResult> {
    let mut names = Vec::new();
    let result = admin
        .list(|name, _json| {
            names.push(name.to_string());
            Ok(())
        })
        .await?;

    println!("client saw {} units, server reported {}", names.len(), result.total);
    Ok(result)
}
```

## 相关 API

- [`UnitStateAdmin`](./UnitStateAdmin.md)：产生本结果的客户端。
- [`ErrorCode`](../error_code/ErrorCode.md)：流失败时的错误码（如 `UnitStateConnectionClosed`）。
