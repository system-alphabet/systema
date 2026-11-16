# CONTROL_DRAIN_TIMEOUT

所属模块：`libsystema_shared::ipc`

## 简介

一次性控制会话在收到终止回复后排空在途帧的上限。健康的服务端会在一个往返内
看到我们的 FIN、停止推送并关闭；该上限只用于防止对端卡死。

## 定义

```rust
pub const CONTROL_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);
```

## 使用示例

```rust
use libsystema_shared::ipc::{finish_control_session, CONTROL_DRAIN_TIMEOUT};

async fn close(framed: &mut libsystema_shared::ipc::EnvelopeFramed) {
    finish_control_session(framed, CONTROL_DRAIN_TIMEOUT).await;
}
```

## 相关 API

- [`finish_control_session`](./finish_control_session.md)：使用本上限。
