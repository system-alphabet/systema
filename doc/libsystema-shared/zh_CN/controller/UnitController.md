# UnitController

所属模块：`libsystema_shared::controller`

## 简介

每个单元 worker 都要实现的操作表。System A 与 D-Bus 层只以泛型方式调用这些
方法，不知道具体单元类型；IPC 层负责把调用路由到管理目标单元的那个 worker。

`start` / `restart` / `reload` / `stop` 可能长时间阻塞（例如 `Type=notify`
要等 `READY=1`，最长到 `TimeoutStartSec`），`WorkerIpc` 会把它们放进独立任务
执行，因此实现中可以直接 `.await` 这些耗时操作。

## 定义

```rust
#[async_trait::async_trait]
pub trait UnitController: Send + Sync {
    async fn status(&self, unit_name: &str) -> anyhow::Result<UnitStatus>;

    async fn start(
        &self,
        unit_name: &str,
        config: &[u8],
        invocation_id: &str,
    ) -> anyhow::Result<()>;

    async fn stop(&self, unit_name: &str) -> anyhow::Result<()>;

    async fn restart(
        &self,
        unit_name: &str,
        config: &[u8],
        invocation_id: &str,
    ) -> anyhow::Result<()>;

    async fn reload(&self, unit_name: &str, config: &[u8]) -> anyhow::Result<()>;

    async fn sync_state(&self) -> Vec<UnitStatus> {
        Vec::new()
    }
}
```

## 方法

| 方法 | 说明 |
| --- | --- |
| `status` | 查询单元当前运行时状态。 |
| `start` | 用序列化的配置启动单元。`invocation_id` 是本次激活的 UUID v4，worker 在适用时应把它设为子进程的 `INVOCATION_ID`。 |
| `stop` | 停止单元。 |
| `restart` | 重启（先 stop 再 start）单元。 |
| `reload` | 不重启、只重新加载单元配置。 |
| `sync_state` | 返回本 worker 管理的全部单元状态，用于连接/重连后的状态同步；默认实现返回空列表。 |

## 使用示例

```rust
use anyhow::Result;
use async_trait::async_trait;
use libsystema_shared::controller::{decode_unit_config, UnitController, UnitStatus};

struct MyServiceController;

#[async_trait]
impl UnitController for MyServiceController {
    async fn status(&self, unit_name: &str) -> Result<UnitStatus> {
        Ok(UnitStatus {
            unit_name: unit_name.to_string(),
            active_state: "inactive".to_string(),
            sub_state: "dead".to_string(),
            ..Default::default()
        })
    }

    async fn start(
        &self,
        unit_name: &str,
        config: &[u8],
        invocation_id: &str,
    ) -> Result<()> {
        let config = decode_unit_config(config)?;
        // 真正的启动逻辑：spawn 进程、设置 INVOCATION_ID 等。
        let _ = (unit_name, config.unit_name, invocation_id);
        Ok(())
    }

    async fn stop(&self, _unit_name: &str) -> Result<()> {
        Ok(())
    }

    async fn restart(
        &self,
        _unit_name: &str,
        _config: &[u8],
        _invocation_id: &str,
    ) -> Result<()> {
        Ok(())
    }

    async fn reload(&self, _unit_name: &str, _config: &[u8]) -> Result<()> {
        Ok(())
    }

    async fn sync_state(&self) -> Vec<UnitStatus> {
        Vec::new()
    }
}
```

把实现交给 [`WorkerIpc`](../worker_ipc/WorkerIpc.md) 的工厂闭包即可接入 IPC 循环：

```rust
use libsystema_shared::worker_ipc::WorkerIpc;

async fn serve() -> anyhow::Result<()> {
    let ipc = WorkerIpc::new("system-s-1", &["service"])
        .with_socket_path("/run/systema/allocator.sock");

    ipc.run(
        |_publisher| MyServiceController,
        |_env, _publisher| Ok(false),
    )
    .await
}
```

## 相关 API

- [`UnitStatus`](./UnitStatus.md)：`status` 与 `sync_state` 的返回模型。
- [`decode_unit_config`](./decode_unit_config.md)：把 `start` 收到的字节解码成 `UnitConfig`。
- [`WorkerIpc`](../worker_ipc/WorkerIpc.md)：调用本 trait 并负责重连、注册和退出握手。
