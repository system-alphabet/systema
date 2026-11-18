# get_i32_or

所属模块：`libsystema_shared::ir::config`

## 简介

读取一个**有符号**整数指令，无法解析时返回 `default`。

用于取值可以为负的指令，例如 `Nice=`、`OOMScoreAdjust=`、`Environment` 中的数值
配置。JSON 数字与十进制字符串都接受。

## 定义

```rust
pub fn get_i32_or(map: &ConfigMap, key: &str, default: i32) -> i32
```

## 参数与返回

| 项 | 说明 |
| --- | --- |
| `map` | 指令字典。 |
| `key` | 指令原名。 |
| `default` | 缺失或不可解析时返回的值。 |
| 返回 `i32` | 解析结果或 `default`。 |

## 使用示例

```rust
use libsystema_shared::ir::config;
use libsystema_shared::ir::ConfigMap;

fn nice_level(map: &ConfigMap) -> i32 {
    config::get_i32_or(map, "Nice", 0)
}

fn main() {
    let mut map = ConfigMap::new();
    assert_eq!(nice_level(&map), 0);

    map.insert("Nice".to_string(), serde_json::json!(-10));
    assert_eq!(nice_level(&map), -10);

    map.insert("Nice".to_string(), serde_json::json!("-5"));
    assert_eq!(nice_level(&map), -5);
}
```

## 相关 API

- [`get_u32`](./get_u32.md)：无符号整数。
- [`get_u32_or`](./get_u32_or.md)：无符号整数的默认值版本。
