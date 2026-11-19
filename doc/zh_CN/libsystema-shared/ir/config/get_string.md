# get_string

所属模块：`libsystema_shared::ir::config`

## 简介

读取一个**单值**指令，返回其文本形式。

- JSON 字符串原样返回；
- JSON 数字或布尔值转换成对应的十进制/`true`/`false` 文本，兼容生产方直接
  写入标量的情况；
- 可重复指令（JSON 数组）返回 `None`——请改用 [`get_strings`](./get_strings.md)；
- 指令缺失返回 `None`。

## 定义

```rust
pub fn get_string(map: &ConfigMap, key: &str) -> Option<String>
```

## 参数与返回

| 项 | 说明 |
| --- | --- |
| `map` | 指令字典。 |
| `key` | 指令原名，例如 `Type`、`Service`、`Where`。 |
| 返回 `Some(String)` | 指令存在且是字符串/数字/布尔。 |
| 返回 `None` | 指令缺失，或值是数组。 |

## 使用示例

```rust
use libsystema_shared::ir::config;
use libsystema_shared::ir::ConfigMap;

fn socket_service(map: &ConfigMap, unit_name: &str) -> String {
    // 显式 Service= 优先，否则按 "foo.socket" -> "foo.service" 推断。
    config::get_string(map, "Service")
        .unwrap_or_else(|| unit_name.replace(".socket", ".service"))
}

fn main() {
    let mut map = ConfigMap::new();
    map.insert("Service".to_string(), serde_json::json!("udevd.service"));
    assert_eq!(socket_service(&map, "udev.socket"), "udevd.service");
    assert_eq!(socket_service(&ConfigMap::new(), "dbus.socket"), "dbus.service");
}
```

标量值同样可读：

```rust
use libsystema_shared::ir::{config, ConfigMap};

let mut map = ConfigMap::new();
map.insert("TimeoutStartSec".to_string(), serde_json::json!(90));
assert_eq!(config::get_string(&map, "TimeoutStartSec").as_deref(), Some("90"));
assert_eq!(config::get_string(&map, "Missing"), None);
```

## 相关 API

- [`get_string_or`](./get_string_or.md)：带默认值。
- [`get_strings`](./get_strings.md)：读取可重复指令。
- [`get_u32`](./get_u32.md)：需要数值时避免文本转换。
