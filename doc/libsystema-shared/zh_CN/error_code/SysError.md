# SysError

所属模块：`libsystema_shared::error_code`

## 简介

IPC 内核报告的失败：一个 [`ErrorCode`](./ErrorCode.md) 加上该错误码占位符
对应的值。它实现了 `std::error::Error`，可以直接放进 `anyhow::Error` 链中。

`Display` 输出的是**技术性渲染**（`Code(arg=value)`），用于 tracing 日志和
协议字段，**绝不能**直接展示给终端用户；面向用户的本地化消息由
`sysa::errors::localize` 根据错误码生成。

## 定义

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SysError {
    code: ErrorCode,
    args: Vec<(&'static str, String)>,
}
```

## 方法

| 签名 | 说明 |
| --- | --- |
| `pub fn new(code: ErrorCode) -> Self` | 构造一个不带占位符值的错误。 |
| `pub fn with(self, name: &'static str, value: impl Into<String>) -> Self` | 附加一个占位符值；`name` 必须是 `ErrorCode::params()` 中的名字（错误名字会触发 `debug_assert!`）。 |
| `pub fn code(&self) -> ErrorCode` | 返回失败码，供调用方分支处理。 |
| `pub fn args(&self) -> &[(&'static str, String)]` | 返回按附加顺序排列的占位符值。 |

此外实现了 `From<ErrorCode> for SysError`。

## 使用示例

```rust
use anyhow::Context;
use libsystema_shared::error_code::{ErrorCode, SysError};

fn connect_like() -> anyhow::Result<()> {
    Err(anyhow::Error::new(
        SysError::new(ErrorCode::WorkerConnectFailed)
            .with("path", "/run/systema/allocator.sock"),
    ))
}

fn main() {
    let err = connect_like().unwrap_err();
    // 技术性渲染，只适合日志。
    assert_eq!(
        err.to_string(),
        "WorkerConnectFailed(path=/run/systema/allocator.sock)"
    );

    let sys = err.downcast_ref::<SysError>().unwrap();
    assert_eq!(sys.code(), ErrorCode::WorkerConnectFailed);
    assert_eq!(sys.args()[0].0, "path");
}
```

在其它错误上追加上下文，同时保留可下转型：

```rust
use anyhow::Context;
use libsystema_shared::error_code::{ErrorCode, SysError};

fn decode(bytes: &[u8]) -> anyhow::Result<()> {
    let _ = bytes;
    Err(anyhow::anyhow!("bad payload"))
        .context(SysError::new(ErrorCode::UnitConfigDecodeFailed))
}
```

## 相关 API

- [`ErrorCode`](./ErrorCode.md)：错误码集合与占位符声明。
- [`decode_unit_config`](../controller/decode_unit_config.md)：成功路径之外返回本错误。
- [`connect_control`](../ipc/connect_control.md)：连接失败时返回本错误。
