# UnitFailure

所属模块：`libsystema_shared::worker_ipc`

## 简介

标记“单元自身的失败结果、而不是任务级故障”的哨兵错误。

当单元进程正常退出但状态码非零时（例如 `Type=oneshot` 的 `ExecStart` 返回 1），
worker 把本类型包在本地化消息外面返回。失败的 job 仍会上报（systemd 在这种情况下
同样会让启动 job 失败），但 System A **不应**记录任务级警告：单元状态更新本身已经
携带了结果。

## 定义

```rust
#[derive(Debug)]
pub struct UnitFailure;

impl std::fmt::Display for UnitFailure { /* ... */ }
impl std::error::Error for UnitFailure {}
```

## 行为

- 用 `anyhow::Error::new(UnitFailure).context(message)` 附加：顶层 `Display`
  仍是本地化消息。
- `run_method` 会把该标记转换成 `MethodResult::unit_failure = true`。
- 该字段向后兼容：旧 worker 不设置时默认为 `false`。

## 使用示例

```rust
use anyhow::{Context, Result};
use libsystema_shared::worker_ipc::UnitFailure;

fn oneshot_exited(status: i32) -> Result<()> {
    Err(anyhow::Error::new(UnitFailure))
        .with_context(|| format!("unit process exited with status {status}"))
}
```

在 `UnitController::start` 中使用：

```rust
use anyhow::{Context, Result};
use libsystema_shared::worker_ipc::UnitFailure;

async fn run_oneshot(exit_code: i32) -> Result<()> {
    if exit_code != 0 {
        return Err(anyhow::Error::new(UnitFailure))
            .with_context(|| format!("exec start exited with {exit_code}"));
    }
    Ok(())
}
```

## 相关 API

- [`UnitController`](../controller/UnitController.md)：通常在这里返回本标记。
- [`WorkerIpc`](./WorkerIpc.md)：把标记写进 `MethodResult::unit_failure`。
