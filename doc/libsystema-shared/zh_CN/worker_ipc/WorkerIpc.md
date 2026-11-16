# WorkerIpc

所属模块：`libsystema_shared::worker_ipc`

## 简介

封装好的 worker IPC 主循环：连接、注册、方法分发、状态同步与事件发布都由它
处理。worker 只需要实现 [`UnitController`](../controller/UnitController.md)，可选地
提供自定义 envelope 处理器。

循环**不监听任何进程信号**。需要 SIGTERM/SIGINT 语义的 worker 由调用方通过
[`ShutdownHandle`](./ShutdownHandle.md) 或 [`WorkerIpc::shutdown`](#方法) 请求退出，
循环会在边界处优雅收尾：先跑 `on_shutdown` 清理（连接仍保持），再发送
`worker.exit`，然后等待 System A 关闭连接。调用方**不要**自行拆掉 socket。

## 定义

```rust
pub struct WorkerIpc {
    // 私有字段
}

impl WorkerIpc {
    pub fn new(worker_id: &str, unit_types: &[&str]) -> Self;

    pub fn shutdown_handle(&self) -> ShutdownHandle;
    pub fn shutdown(&self, reason: impl Into<String>);

    pub fn with_socket_path(self, path: impl Into<String>) -> Self;

    pub fn on_shutdown<F, Fut>(self, cleanup: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static;

    pub fn on_reconnect<F, Fut>(self, hook: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static;

    pub fn on_method_result<F>(self, hook: F) -> Self
    where
        F: Fn(&MethodCall, &MethodResult, bool) + Send + Sync + 'static;

    pub fn supports_unit_define(self) -> Self;
    pub fn supports_resource_prepare(self) -> Self;

    pub async fn run<C, H>(
        &self,
        controller_factory: impl Fn(EventPublisher) -> C,
        custom_handler: H,
    ) -> Result<()>
    where
        C: UnitController + Send + Sync + 'static,
        H: Fn(&Envelope, &EventPublisher) -> Result<bool>;
}
```

## 方法

| 方法 | 说明 |
| --- | --- |
| `new` | 用唯一 `worker_id`（如 `"system-s-1"`）和管理的单元类型（如 `["service"]`）构造。 |
| `shutdown_handle` | 返回可克隆的 [`ShutdownHandle`](./ShutdownHandle.md)，供信号监听器等其它任务请求退出。 |
| `shutdown` | 等价于 `shutdown_handle().shutdown(reason)`。 |
| `with_socket_path` | 覆盖 System A 的 socket 路径；**必须在 `run` 之前调用**（本 crate 不持有编译期路径注册表）。 |
| `on_shutdown` | 注册退出前执行的本地清理，在 `worker.exit` 之前、连接仍打开时运行。 |
| `on_reconnect` | 注册每次成功（重）连接后、读取循环开始前的钩子，用于清理只对单条连接有效的缓存。 |
| `on_method_result` | 在尝试排队一条延迟方法结果后调用；第三个参数表示结果是否成功入队，钩子不得阻塞。 |
| `supports_unit_define` | 声明支持 `unit.define`（System A 可请求动态单元定义，如 System R 的 slice 父链）。 |
| `supports_resource_prepare` | 声明支持 `resource.prepare`（启动前确保 cgroup 层级并应用 `Delegate=` 归属）。 |
| `run` | 运行带自动重连的 IPC 循环。`controller_factory` 在**每次**连接尝试时用新的 [`EventPublisher`](./EventPublisher.md) 调用；`custom_handler` 处理既非 `method.call` 也非 `state.sync_request` 的 envelope，返回 `Ok(true)` 表示已处理，`Ok(false)` 表示交给循环记录警告。 |

## 使用示例

```rust
use anyhow::Result;
use async_trait::async_trait;
use libsystema_shared::controller::{UnitController, UnitStatus};
use libsystema_shared::worker_ipc::{EventPublisher, WorkerIpc};

struct ServiceController {
    publisher: EventPublisher,
}

#[async_trait]
impl UnitController for ServiceController {
    async fn status(&self, unit_name: &str) -> Result<UnitStatus> {
        Ok(UnitStatus {
            unit_name: unit_name.to_string(),
            active_state: "inactive".to_string(),
            sub_state: "dead".to_string(),
            ..Default::default()
        })
    }

    async fn start(&self, _unit_name: &str, _config: &[u8], _invocation_id: &str) -> Result<()> {
        // 发布状态更新（异步、带 ACK 重试）。
        // self.publisher.publish_unit_state_update(vec![...], false);
        Ok(())
    }

    async fn stop(&self, _unit_name: &str) -> Result<()> {
        Ok(())
    }

    async fn restart(&self, _unit_name: &str, _config: &[u8], _invocation_id: &str) -> Result<()> {
        Ok(())
    }

    async fn reload(&self, _unit_name: &str, _config: &[u8]) -> Result<()> {
        Ok(())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let ipc = WorkerIpc::new("system-s-1", &["service"])
        .with_socket_path("/run/systema/allocator.sock")
        .on_shutdown(|| async {
            // 在连接仍打开时停止本 worker 管理的服务。
        })
        .on_reconnect(|| async {
            // 清理仅对单条连接有效的缓存。
        });

    // 允许其它任务（如信号监听器）请求退出。
    let handle = ipc.shutdown_handle();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        handle.shutdown("SIGINT");
    });

    ipc.run(
        |publisher| ServiceController { publisher },
        |_env, _publisher| Ok(false),
    )
    .await
}
```

## 相关 API

- [`UnitController`](../controller/UnitController.md)：方法分发的目标 trait。
- [`EventPublisher`](./EventPublisher.md)：状态更新、cgroup 查询与事件发送。
- [`ShutdownHandle`](./ShutdownHandle.md)：跨任务的退出请求。
- [`UnitFailure`](./UnitFailure.md)：把“单元自身失败”与“任务级故障”区分开。
