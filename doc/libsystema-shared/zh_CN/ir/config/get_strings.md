# get_strings

所属模块：`libsystema_shared::ir::config`

## 简介

读取一个可重复指令的全部值，按单元文件中的出现顺序返回。

- JSON 数组：逐个元素转成文本（字符串原样，数字/布尔转十进制文本）；
- 单值写法：返回只含一个元素的 `Vec`；
- 指令缺失：返回空 `Vec`。

因此消费方不需要区分“写了一次”和“写成一个数组”——遍历结果即可，与 systemd
对 `ExecStart=`、`ListenStream=`、`Condition*=` 等指令的累加语义一致。

## 定义

```rust
pub fn get_strings(map: &ConfigMap, key: &str) -> Vec<String>
```

## 参数与返回

| 项 | 说明 |
| --- | --- |
| `map` | 指令字典。 |
| `key` | 指令原名。 |
| 返回 `Vec<String>` | 值列表；缺失时为空。 |

## 使用示例

```rust
use libsystema_shared::ir::config;
use libsystema_shared::ir::ConfigMap;

fn exec_start_commands(map: &ConfigMap) -> Vec<String> {
    // `-`/`+`/`!` 前缀保留在字符串里，由进程启动方自行解析。
    config::get_strings(map, "ExecStart")
}

fn main() {
    let mut map = ConfigMap::new();
    map.insert(
        "ExecStart".to_string(),
        serde_json::json!(["-/usr/bin/a", "/usr/bin/b"]),
    );
    assert_eq!(
        exec_start_commands(&map),
        ["-/usr/bin/a", "/usr/bin/b"]
    );

    // 单值写法等价于一元素数组；缺失得到空列表。
    let mut single = ConfigMap::new();
    single.insert("ListenStream".to_string(), serde_json::json!("/run/a.sock"));
    assert_eq!(config::get_strings(&single, "ListenStream"), ["/run/a.sock"]);
    assert!(config::get_strings(&single, "Missing").is_empty());
}
```

## 相关 API

- [`get_string`](./get_string.md)：只取单值。
- [`UnitIR`](../UnitIR.md)：“重复指令 = 字符串数组”的编码约定。
