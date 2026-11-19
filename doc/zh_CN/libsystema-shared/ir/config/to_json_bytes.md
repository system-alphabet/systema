# to_json_bytes

所属模块：`libsystema_shared::ir::config`

## 简介

把 [`ConfigMap`](../ConfigMap.md) 编码成 `UnitConfig.config_json` 使用的 JSON 字节。

`serde_json::Map` 总是可序列化，因此这里没有可预期的失败路径：万一出现内部序列化
错误，函数退化为返回空字节（消费方按“无类型专属配置”处理），不会 panic。

## 定义

```rust
pub fn to_json_bytes(map: &ConfigMap) -> Vec<u8>
```

## 参数与返回

| 项 | 说明 |
| --- | --- |
| `map` | 要编码的指令字典。 |
| 返回 `Vec<u8>` | JSON 字节；序列化异常时为空 `Vec`。 |

## 使用示例

```rust
use libsystema_shared::ir::config;
use libsystema_shared::ir::ConfigMap;

fn mount_directives(what: &str, where_: &str) -> Vec<u8> {
    let mut config = ConfigMap::new();
    config.insert("What".to_string(), serde_json::json!(what));
    config.insert("Where".to_string(), serde_json::json!(where_));
    config.insert("Type".to_string(), serde_json::json!("ext4"));
    config.insert(
        "Options".to_string(),
        serde_json::json!("defaults,noatime"),
    );
    config::to_json_bytes(&config)
}

fn main() {
    assert!(!mount_directives("/dev/sda2", "/data").is_empty());
}
```

往返转换是无损的：

```rust
use libsystema_shared::ir::{config, ConfigMap};

let mut map = ConfigMap::new();
map.insert("Type".to_string(), serde_json::json!("notify"));
let round_tripped = config::parse_json(&config::to_json_bytes(&map)).unwrap();
assert_eq!(round_tripped, map);
```

空字典会编码为 `{}`，而不是空字节；需要表达“未报告”时请使用 `Option<ConfigMap>`
的 `None`。

## 相关 API

- [`ConfigMap`](../ConfigMap.md)：被编码的字典类型。
- [`parse_json`](./parse_json.md)：反向解码。
- [`UnitIR`](../UnitIR.md)：`config` 字段直接参与 JSON 序列化，无需本函数。
