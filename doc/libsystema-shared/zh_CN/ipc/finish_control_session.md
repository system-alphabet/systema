# finish_control_session

所属模块：`libsystema_shared::ipc`

## 简介

在不向对端触发 RST 的前提下结束一次性控制会话。

控制会话同时也是一个事件流：调用方拿到自己请求的 `{method}.result` 之后，服务端
仍会继续推送 `job.*` / `unit.*` envelope。如果接收队列非空时直接关闭 socket，
Linux 会让对端收到 `ECONNRESET`（`unix_release_sock()` 会把队列非空的 peer 标记为
出错）。正确做法是先半关闭写端——服务端读到 EOF 后停止推送并关闭——再在丢弃前
把已经在途的帧读空。

## 定义

```rust
pub async fn finish_control_session(framed: &mut EnvelopeFramed, drain_timeout: Duration)
```

## 参数与行为

| 参数 | 说明 |
| --- | --- |
| `framed` | 已完成请求/回复的控制会话。 |
| `drain_timeout` | 排空在途帧的上限；超过后放弃等待并返回（不报错）。 |

函数先 `shutdown()` 写端；失败只记录 debug 日志并直接返回。头端关闭后循环读取
直到 EOF 或读错误，整体受 `drain_timeout` 约束。

## 使用示例

```rust
use std::time::Duration;

use libsystema_shared::ipc::{
    connect_control, finish_control_session, frame_stream, make_envelope, recv_envelope,
    send_envelope, CONTROL_DRAIN_TIMEOUT,
};

async fn one_shot(path: &str) -> anyhow::Result<()> {
    let mut framed = frame_stream(connect_control(path).await?);

    let req = make_envelope(1, "", "system-a", "admin.staging", libsystema_shared::proto::AdminStagingOp {
        op: "list".to_string(),
        uid: 0,
        name: String::new(),
    })?;
    send_envelope(&mut framed, &req).await?;
    let _reply = recv_envelope(&mut framed).await?;

    finish_control_session(&mut framed, CONTROL_DRAIN_TIMEOUT).await;
    Ok(())
}
```

## 相关 API

- [`CONTROL_DRAIN_TIMEOUT`](./CONTROL_DRAIN_TIMEOUT.md)：默认排空上限（500ms）。
- [`connect_control`](./connect_control.md)：控制端口连接。
