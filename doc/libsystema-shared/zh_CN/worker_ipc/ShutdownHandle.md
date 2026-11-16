# ShutdownHandle

所属模块：`libsystema_shared::worker_ipc`

## 简介

请求正在运行的 [`WorkerIpc`](./WorkerIpc.md) 退出的可克隆句柄。循环运行时无法
同时把 `WorkerIpc` 借用给信号监听器，所以监听器改持这个句柄。

丢弃全部句柄**不会**停止 worker：只有显式调用 [`shutdown`](#方法) 才会。

## 定义

```rust
#[derive(Clone)]
pub struct ShutdownHandle {
    // 私有字段
}

impl ShutdownHandle {
    pub fn shutdown(&self, reason: impl Into<String>);
}
```

## 方法

| 方法 | 说明 |
| --- | --- |
| `shutdown` | 置位退出标记并唤醒循环，随后优雅退出：在仍打开的连接上执行清理、发送 `worker.exit`、等待 System A 关闭连接。`reason` 用于日志和最终 `worker.exit` envelope（如 `"SIGTERM"`、`"System Init shutdown"`）。重复调用无副作用。 |

## 使用示例

```rust
use libsystema_shared::worker_ipc::{ShutdownHandle, WorkerIpc};

fn install_signal_forwarder(ipc: &WorkerIpc) {
    let handle = ipc.shutdown_handle();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        handle.shutdown("SIGINT");
    });
}

// 也可以直接持有句柄传给别的组件。
fn stop_from_elsewhere(handle: ShutdownHandle) {
    handle.shutdown("System Init shutdown");
}
```

## 相关 API

- [`WorkerIpc`](./WorkerIpc.md)：`shutdown_handle()` / `shutdown()` 的来源。
- [`UnitFailure`](./UnitFailure.md)：与退出无关，但同属 worker 运行时模型。
