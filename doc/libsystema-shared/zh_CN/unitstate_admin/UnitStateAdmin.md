# UnitStateAdmin

所属模块：`libsystema_shared::unitstate_admin`

## 简介

`admin.unitstate` 客户端。它从 System Allocator 的**缓存**中流式读取每个
单元的完整状态（单元定义、运行时状态、cgroup 指标、活动 job、期望状态、
所属 worker 等），整个过程不接触任何 worker。

管理接口位于控制端口（`control.socket`），要求调用方是 UID 0（root）或
System Allocator 运行所在的 UID，否则服务端以权限拒绝的 EOF 结束。

## 定义

```rust
pub struct UnitStateAdmin {
    socket_path: String,
}

impl UnitStateAdmin {
    pub fn with_socket(socket_path: impl Into<String>) -> Self;

    pub async fn list<F>(&self, on_unit: F) -> Result<UnitStateResult>
    where
        F: FnMut(&str, &[u8]) -> Result<()>;
}
```

## 方法

| 方法 | 说明 |
| --- | --- |
| `with_socket` | 构造客户端，指向 System A 的 `control.socket` 路径。 |
| `list` | 逐个接收 `admin.unitstate.entry`，对每个非空 JSON 载荷调用 `on_unit(name, json)`；收到 `admin.unitstate.eof` 后返回 [`UnitStateResult`](./UnitStateResult.md)。`on_unit` 返回错误会立即中断流。 |

## 使用示例

```rust
use libsystema_shared::unitstate_admin::UnitStateAdmin;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let admin = UnitStateAdmin::with_socket("/run/systema/control.socket");

    let mut count = 0usize;
    let result = admin
        .list(|name, json| {
            count += 1;
            println!("{name}: {} bytes of JSON", json.len());
            Ok(())
        })
        .await?;

    println!("streamed {} entries, total={}", count, result.total);
    if !result.message.is_empty() {
        eprintln!("server message: {}", result.message);
    }
    Ok(())
}
```

## 相关 API

- [`UnitStateResult`](./UnitStateResult.md)：流结束后的统计与消息。
- [`connect_control`](../ipc/connect_control.md)：控制端口连接与权限提示。
- [`finish_control_session`](../ipc/finish_control_session.md)：一次性控制会话的优雅收尾。
