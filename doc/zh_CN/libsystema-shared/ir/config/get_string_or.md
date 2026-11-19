# get_string_or

所属模块：`libsystema_shared::ir::config`

## 简介

[`get_string`](./get_string.md) 的带默认值版本：指令缺失或值不是单值时返回
`default`。

systemd 的绝大多数字符串指令缺省为空串，因此 `get_string_or(map, key, "")` 是
常见写法；需要区分“缺失”和“显式空值”的指令请直接用 `get_string`。

## 定义

```rust
pub fn get_string_or(map: &ConfigMap, key: &str, default: &str) -> String
```

## 参数与返回

| 项 | 说明 |
| --- | --- |
| `map` | 指令字典。 |
| `key` | 指令原名。 |
| `default` | 指令不可读时返回的值。 |
| 返回 `String` | 指令文本，或 `default` 的副本。 |

## 使用示例

```rust
use libsystema_shared::ir::config;
use libsystema_shared::ir::ConfigMap;

fn service_type(map: &ConfigMap) -> String {
    // systemd 的默认服务类型是 simple。
    config::get_string_or(map, "Type", "simple")
}

fn main() {
    let mut map = ConfigMap::new();
    assert_eq!(service_type(&map), "simple");

    map.insert("Type".to_string(), serde_json::json!("notify"));
    assert_eq!(service_type(&map), "notify");
}
```

## 相关 API

- [`get_string`](./get_string.md)：返回 `Option<String>`。
- [`get_u32_or`](./get_u32_or.md)：数值指令的默认值版本。
