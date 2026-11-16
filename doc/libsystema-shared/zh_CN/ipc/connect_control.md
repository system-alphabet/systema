# connect_control

所属模块：`libsystema_shared::ipc`

## 简介

连接到控制端口总线 socket（`control.socket`）。

内核因权限拒绝而失败（非特权调用方访问**系统**实例的 socket）时，错误会带上
跨域提示：P1 严格分离系统域与用户域，系统域只能以提升的权限访问，而 `--user`
指向调用方自己的用户实例。其它失败保留原始消息，因此仅仅“实例不存在”仍然
报告 `ENOENT`。

## 定义

```rust
pub async fn connect_control(path: &str) -> Result<UnixStream>
```

## 错误码

| 情况 | `ErrorCode` |
| --- | --- |
| `PermissionDenied` | `ConnectPermissionDenied`（携带 `e` 占位符，服务端追加跨域提示） |
| 其它 IO 错误 | `ConnectFailed`（携带 `e` 占位符） |

## 使用示例

```rust
use libsystema_shared::ipc::{connect_control, finish_control_session, frame_stream};

async fn probe(path: &str) -> anyhow::Result<()> {
    let stream = connect_control(path).await?;
    let mut framed = frame_stream(stream);
    // … 发送 manager.* 请求并读取回复 …
    finish_control_session(&mut framed, std::time::Duration::from_millis(500)).await;
    Ok(())
}
```

调用方**不要**对错误消息做字符串匹配——消息是本地化的；应基于
[`ErrorCode`](../error_code/ErrorCode.md) 分支。

## 相关 API

- [`frame_stream`](./frame_stream.md)：包装连接。
- [`finish_control_session`](./finish_control_session.md)：一次性控制会话的优雅收尾。
- [`StagingAdmin`](../staging_admin/StagingAdmin.md) / [`UnitStateAdmin`](../unitstate_admin/UnitStateAdmin.md)：使用本函数的客户端。
