# get_u32_or

所属模块：`libsystema_shared::ir::config`

## 简介

[`get_u32`](./get_u32.md) 的带默认值版本：指令缺失或不可解析时返回 `default`。

注意它无法区分“缺失”与“显式 0”——两者都会得到 `0`（当 `default` 为 0 时）。
需要保留该区别时请使用 `get_u32`。

## 定义

```rust
pub fn get_u32_or(map: &ConfigMap, key: &str, default: u32) -> u32
```

## 参数与返回

| 项 | 说明 |
| --- | --- |
| `map` | 指令字典。 |
| `key` | 指令原名。 |
| `default` | 不可解析时返回的值。 |
| 返回 `u32` | 解析结果或 `default`。 |

## 使用示例

```rust
use libsystema_shared::ir::config;
use libsystema_shared::ir::ConfigMap;

fn backlog(map: &ConfigMap) -> u32 {
    // systemd 的 socket 默认 backlog 为 128。
    config::get_u32_or(map, "Backlog", 128)
}

fn main() {
    let mut map = ConfigMap::new();
    assert_eq!(backlog(&map), 128);

    map.insert("Backlog".to_string(), serde_json::json!(256));
    assert_eq!(backlog(&map), 256);

    map.insert("Backlog".to_string(), serde_json::json!("garbage"));
    assert_eq!(backlog(&map), 128);
}
```

## 相关 API

- [`get_u32`](./get_u32.md)：返回 `Option<u32>`。
- [`get_string_or`](./get_string_or.md)：字符串版本的默认值回退。
