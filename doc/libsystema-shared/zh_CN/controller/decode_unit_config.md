# decode_unit_config

所属模块：`libsystema_shared::controller`

## 简介

把 `UnitController::start` / `restart` / `reload` 收到的裸字节解码成
protobuf 的 `UnitConfig`。解码失败时返回的 `anyhow::Error` 在底层携带
`SysError(ErrorCode::UnitConfigDecodeFailed)`，便于调用方按错误码分支处理。

## 定义

```rust
pub fn decode_unit_config(bytes: &[u8]) -> anyhow::Result<crate::proto::UnitConfig>
```

## 参数与返回

| 项 | 说明 |
| --- | --- |
| `bytes` | `MethodCall::args` 中的 protobuf 编码字节。 |
| 返回 `Ok(UnitConfig)` | 解码成功。 |
| 返回 `Err` | 字节不是合法的 `UnitConfig`，底层错误码为 `ErrorCode::UnitConfigDecodeFailed`。 |

## 使用示例

```rust
use anyhow::Result;
use libsystema_shared::controller::decode_unit_config;

fn read_unit_name(args: &[u8]) -> Result<String> {
    let config = decode_unit_config(args)?;
    Ok(config.unit_name)
}
```

在 `UnitController::start` 中使用：

```rust
use anyhow::Result;
use libsystema_shared::controller::decode_unit_config;
use libsystema_shared::ir::config;

fn prepare_start(config_bytes: &[u8]) -> Result<()> {
    let config = decode_unit_config(config_bytes)?;
    println!("starting {}", config.unit_name);
    let directives = config::parse_json(&config.config_json)?;
    for command in config::get_strings(&directives, "ExecStart") {
        println!("exec_start = {command}");
    }
    Ok(())
}
```

## 相关 API

- [`UnitController`](./UnitController.md)：`start` / `restart` / `reload` 的 `config` 参数。
- [`UnitIR`](../ir/UnitIR.md)：`config_json` 解码后的指令字典结构。
- [`SysError`](../error_code/SysError.md)：底层错误类型。
- [`ErrorCode`](../error_code/ErrorCode.md)：错误码 `UnitConfigDecodeFailed`。
