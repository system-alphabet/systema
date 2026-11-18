# ConfigMap

所属模块：`libsystema_shared::ir`

## 简介

单元类型专属配置的**扁平指令字典**：键就是 systemd 单元文件里的指令原名，
不再有 `ServiceConfig` / `SocketConfig` 这类按类型定义的 Rust 结构。

- 所属 section 由 [`UnitIR::unit_type`](./UnitIR.md) 隐含：`Service` → `[Service]`，
  `Mount` → `[Mount]`，`Socket` → `[Socket]`……键中不写 section 名。
- 单值指令映射为 JSON 字符串，可重复指令映射为字符串数组。
- 值保留单元文件中的原样：`ExecStart=` 的 `-`/`+`/`!` 前缀、`0755` 这类八进制
  写法都不会被归一化，消费方按解析单元文件的同一套规则解析。
- 生产方只负责把文件内容搬进字典；**解释键的职责属于拥有该单元类型的组件**
  （System A 或具体 worker）。IR 因此与单元文件一样宽，新增指令不需要改协议。

同一个编码也用于 worker 的 `UnitConfig.config_json` 字段（见
[`decode_unit_config`](../controller/decode_unit_config.md)），由
[`parse_json`](./config/parse_json.md) / [`to_json_bytes`](./config/to_json_bytes.md)
在两者之间转换。

## 定义

```rust
pub type ConfigMap = serde_json::Map<String, serde_json::Value>;
```

## 取值约定

| 单元文件写法 | 字典中的形态 |
| --- | --- |
| `Type=notify` | `"Type": "notify"` |
| `ExecStart=/usr/bin/a`、`ExecStart=/usr/bin/b` | `"ExecStart": ["/usr/bin/a", "/usr/bin/b"]` |
| `TimeoutStartSec=90` | `"TimeoutStartSec": 90` |
| `RemainAfterExit=yes` | `"RemainAfterExit": "yes"` |
| `SocketMode=0660` | `"SocketMode": "0660"` |

`UnitIR::config` 使用 `Option<ConfigMap>`，三态语义与普通字段一致：

- `None`：本次未报告；部分更新时保留旧值。
- `Some(空)`：已报告，但该 section 为空。
- `Some(非空)`：该 section 的完整内容。

读取时不要手写 JSON 匹配，使用 [`ir::config`](./config/parse_json.md) 的取值函数，
它们同时接受字符串写法与 JSON 标量（数字、布尔）。

## 使用示例

```rust
use libsystema_shared::ir::{ConfigMap, UnitIR, UnitType};

fn socket_unit() -> UnitIR {
    let mut config = ConfigMap::new();
    config.insert(
        "ListenStream".to_string(),
        serde_json::json!(["/run/example.sock"]),
    );
    config.insert("SocketMode".to_string(), serde_json::json!("0660"));
    config.insert("FileDescriptorName".to_string(), serde_json::json!("varlink"));

    UnitIR {
        id: "example.socket".to_string(),
        unit_type: Some(UnitType::Socket),
        config: Some(config),
        ..Default::default()
    }
}
```

## 相关 API

- [`UnitIR`](./UnitIR.md)：持有 `config: Option<ConfigMap>`。
- [`UnitType`](./UnitType.md)：决定 `config` 所属 section。
- [`parse_json`](./config/parse_json.md)：字节 → `ConfigMap`。
- [`to_json_bytes`](./config/to_json_bytes.md)：`ConfigMap` → 字节。
- [`get_string`](./config/get_string.md)、[`get_strings`](./config/get_strings.md)、
  [`get_bool`](./config/get_bool.md)、[`get_u32`](./config/get_u32.md)、
  [`get_mode`](./config/get_mode.md)：类型化取值。
