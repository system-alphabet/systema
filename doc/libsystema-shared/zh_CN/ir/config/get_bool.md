# get_bool

所属模块：`libsystema_shared::ir::config`

## 简介

读取一个布尔指令，兼容 systemd 的布尔写法。

- JSON `true` / `false`：直接采用；
- 字符串：去掉首尾空白、忽略大小写后，`1`、`yes`、`true`、`on` 为真，
  其余为假；
- 指令缺失或值不可解析：返回 `false`。

“缺失即 `false`”与 systemd 对这些指令的默认值一致（如 `RemainAfterExit=`、
`Accept=`），因此调用方通常不需要再区分“没写”和“写了 no”。

## 定义

```rust
pub fn get_bool(map: &ConfigMap, key: &str) -> bool
```

## 参数与返回

| 项 | 说明 |
| --- | --- |
| `map` | 指令字典。 |
| `key` | 指令原名。 |
| 返回 `bool` | 解析结果；缺失或不可解析时为 `false`。 |

## 使用示例

```rust
use libsystema_shared::ir::config;
use libsystema_shared::ir::ConfigMap;

fn is_remain_after_exit(map: &ConfigMap) -> bool {
    config::get_bool(map, "RemainAfterExit")
}

fn main() {
    let mut map = ConfigMap::new();
    assert!(!is_remain_after_exit(&map)); // 缺省 false

    map.insert("RemainAfterExit".to_string(), serde_json::json!("yes"));
    assert!(is_remain_after_exit(&map));

    map.insert("RemainAfterExit".to_string(), serde_json::json!(true));
    assert!(is_remain_after_exit(&map));

    map.insert("RemainAfterExit".to_string(), serde_json::json!("no"));
    assert!(!is_remain_after_exit(&map));
}
```

## 相关 API

- [`get_string`](./get_string.md)：需要保留原始文本时使用。
- [`ConfigMap`](../ConfigMap.md)：布尔值的存放形态。
