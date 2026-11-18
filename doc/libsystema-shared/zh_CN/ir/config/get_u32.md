# get_u32

所属模块：`libsystema_shared::ir::config`

## 简介

读取一个无符号整数指令。

- JSON 数字：直接取整数值（超出 `u32` 范围视为不可解析）；
- 字符串：去首尾空白后按十进制解析；
- 指令缺失、格式非法或溢出：返回 `None`。

用 `Option` 而不是 `0` 作为缺省，是为了让调用方区分“没写”和“显式写了 0”——
例如 `TimeoutStartSec=0` 在 systemd 里有明确含义（关闭超时），不能与“未配置、
应回退默认值”混为一谈。

## 定义

```rust
pub fn get_u32(map: &ConfigMap, key: &str) -> Option<u32>
```

## 参数与返回

| 项 | 说明 |
| --- | --- |
| `map` | 指令字典。 |
| `key` | 指令原名。 |
| 返回 `Some(u32)` | 解析成功的值（可能是 0）。 |
| 返回 `None` | 缺失、非数字或超出范围。 |

## 使用示例

```rust
use libsystema_shared::ir::config;
use libsystema_shared::ir::ConfigMap;

fn start_timeout_secs(map: &ConfigMap) -> u64 {
    // systemd 未配置 TimeoutStartSec= 时默认 90 秒；显式 0 也在字典里存在。
    config::get_u32(map, "TimeoutStartSec")
        .map(u64::from)
        .unwrap_or(90)
}

fn main() {
    let mut map = ConfigMap::new();
    assert_eq!(start_timeout_secs(&map), 90);

    map.insert("TimeoutStartSec".to_string(), serde_json::json!(0));
    assert_eq!(start_timeout_secs(&map), 0);

    map.insert("TimeoutStartSec".to_string(), serde_json::json!("30"));
    assert_eq!(start_timeout_secs(&map), 30);
}
```

## 相关 API

- [`get_u32_or`](./get_u32_or.md)：直接回退到 `default`。
- [`get_i32_or`](./get_i32_or.md)：允许负数的指令（如 `Nice=`）。
- [`get_mode`](./get_mode.md)：八进制权限值。
