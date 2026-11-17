# UnitType

所属模块：`libsystema_shared::ir`

## 简介

跨各种 init 系统统一的单元类型枚举。除标准类型外，`Other(String)` 用于承载
外部 init 系统的自定义/未知类型。

## 定义

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UnitType {
    Service,
    Target,
    Mount,
    Automount,
    Timer,
    Socket,
    Slice,
    Scope,
    Swap,
    Path,
    Device,
    Power,
    Other(String),
}
```

## 方法

| 签名 | 说明 |
| --- | --- |
| `pub fn as_str(&self) -> &str` | 返回小写类型名（`service`、`target`、`mount` …），`Other(s)` 返回 `s`。 |

## 使用示例

```rust
use libsystema_shared::ir::UnitType;

fn describe(kind: &UnitType) -> String {
    format!("unit type = {}", kind.as_str())
}

fn main() {
    assert_eq!(describe(&UnitType::Service), "unit type = service");
    assert_eq!(describe(&UnitType::Socket), "unit type = socket");

    let custom = UnitType::Other("openrc-script".to_string());
    assert_eq!(custom.as_str(), "openrc-script");
}
```

## 相关 API

- [`UnitIR`](./UnitIR.md)：持有 `Option<UnitType>`；类型专属指令见 `UnitIR::config`。
