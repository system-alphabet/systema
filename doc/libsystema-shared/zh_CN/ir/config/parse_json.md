# parse_json

所属模块：`libsystema_shared::ir::config`

## 简介

把 `UnitConfig.config_json`（或任何以 [`ConfigMap`](../ConfigMap.md) 编码的字节）
解码成扁平指令字典。

空字节表示“没有类型专属配置”，返回**空字典**而不是错误。字节不是合法 JSON 时，
返回的 `anyhow::Error` 底层携带 `SysError(ErrorCode::UnitConfigDecodeFailed)`，
便于调用方按错误码分支处理。

## 定义

```rust
pub fn parse_json(bytes: &[u8]) -> anyhow::Result<ConfigMap>
```

## 参数与返回

| 项 | 说明 |
| --- | --- |
| `bytes` | JSON 编码的指令字典；空切片表示无配置。 |
| 返回 `Ok(ConfigMap)` | 解码成功；空输入得到空字典。 |
| 返回 `Err` | 不是合法 JSON，底层错误码为 `ErrorCode::UnitConfigDecodeFailed`。 |

## 使用示例

```rust
use anyhow::Result;
use libsystema_shared::controller::decode_unit_config;
use libsystema_shared::ir::config;

fn service_type(config_bytes: &[u8]) -> Result<String> {
    let unit_config = decode_unit_config(config_bytes)?;
    let directives = config::parse_json(&unit_config.config_json)?;
    Ok(config::get_string_or(&directives, "Type", "simple"))
}
```

`UnitIR` 的 `config` 字段可以直接序列化进提交，不需要先转成字节：

```rust
use libsystema_shared::ir::{ConfigMap, UnitIR};

fn patch_from_json(bytes: &[u8]) -> anyhow::Result<UnitIR> {
    Ok(UnitIR {
        id: "example.service".to_string(),
        config: Some(libsystema_shared::ir::config::parse_json(bytes)?),
        ..Default::default()
    })
}

fn empty_patch() -> anyhow::Result<Option<ConfigMap>> {
    // 空输入 → 空字典（"已报告，section 为空"）。
    Ok(Some(libsystema_shared::ir::config::parse_json(&[])?))
}
```

## 相关 API

- [`ConfigMap`](../ConfigMap.md)：返回的字典类型。
- [`to_json_bytes`](./to_json_bytes.md)：反向编码。
- [`decode_unit_config`](../../controller/decode_unit_config.md)：得到 `UnitConfig`。
- [`ErrorCode`](../../error_code/ErrorCode.md)：错误码 `UnitConfigDecodeFailed`。
