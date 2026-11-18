# get_mode

所属模块：`libsystema_shared::ir::config`

## 简介

读取一个文件权限指令，按**八进制**解析。

- 字符串按八进制解析：`"0755"`、`"0644"` 都符合单元文件写法；也接受
  `"0o755"` / `"0O755"` 前缀写法；
- 八进制解析失败时再尝试十进制，兼顾某些生产者直接写入的数字文本；
- JSON 数字视为**已经解析好的**权限值，原样返回；
- 指令缺失或不可解析：返回 `None`。

## 定义

```rust
pub fn get_mode(map: &ConfigMap, key: &str) -> Option<u32>
```

## 参数与返回

| 项 | 说明 |
| --- | --- |
| `map` | 指令字典。 |
| `key` | 指令原名，例如 `SocketMode`、`DirectoryMode`。 |
| 返回 `Some(u32)` | 权限位（如 `0o660`）。 |
| 返回 `None` | 缺失或不可解析。 |

## 使用示例

```rust
use libsystema_shared::ir::config;
use libsystema_shared::ir::ConfigMap;

fn socket_mode(map: &ConfigMap) -> u32 {
    // 与 systemd 一致：未配置 SocketMode= 时套接字文件为 0666。
    config::get_mode(map, "SocketMode").unwrap_or(0o666)
}

fn main() {
    let mut map = ConfigMap::new();
    assert_eq!(socket_mode(&map), 0o666);

    map.insert("SocketMode".to_string(), serde_json::json!("0660"));
    assert_eq!(socket_mode(&map), 0o660);

    // JSON 数字按已解析的权限值处理。
    map.insert("SocketMode".to_string(), serde_json::json!(0o600));
    assert_eq!(socket_mode(&map), 0o600);
}
```

## 相关 API

- [`get_u32`](./get_u32.md)：十进制数值指令。
- [`ConfigMap`](../ConfigMap.md)：`SocketMode`/`DirectoryMode` 等指令的存放形态。
