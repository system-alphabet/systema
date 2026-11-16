# Condition

所属模块：`libsystema_shared::ir`

## 简介

一条 condition / assert 指令。`UnitIR::conditions` 决定单元能否启动；
`UnitIR::asserts` 在不满足时造成硬失败。

systemd 语义：当至少存在一个触发条件（`trigger = true`）时，只要**任一**触发
条件满足**且**所有普通条件都满足，单元即启动；没有任何触发条件时，所有条件
必须同时满足（逻辑与）。

## 定义

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Condition {
    pub kind: String,
    pub value: String,
    pub negate: bool,
    pub trigger: bool,
}
```

## 字段

| 字段 | 说明 |
| --- | --- |
| `kind` | 条件类型，如 `PathExists`、`Virtualization`、`Host`。 |
| `value` | 待检查的值，可能带否定前缀。 |
| `negate` | 是否取反（原始值以 `!` 为前缀）。 |
| `trigger` | 是否为触发条件（原始值以竖线字符为前缀）。 |

## 方法

| 签名 | 说明 |
| --- | --- |
| `pub fn from_value(kind: &str, value: &str) -> Self` | 从原始指令值构造，按 systemd 顺序先处理竖线触发前缀，再处理 `!` 否定前缀。 |

## 使用示例

```rust
use libsystema_shared::ir::Condition;

fn main() {
    let cond = Condition::from_value("ConditionPathExists", "!/run/example.lock");
    assert_eq!(cond.kind, "ConditionPathExists");
    assert_eq!(cond.value, "/run/example.lock");
    assert!(cond.negate);
    assert!(!cond.trigger);

    // 触发条件：竖线在 ! 之前。
    let trigger = Condition::from_value("ConditionHost", "|!builder");
    assert!(trigger.trigger);
    assert!(trigger.negate);
    assert_eq!(trigger.value, "builder");

    // 手动构造。
    let manual = Condition {
        kind: "ConditionUser".to_string(),
        value: "root".to_string(),
        negate: false,
        trigger: false,
    };
    assert_eq!(manual.kind, "ConditionUser");
}
```

## 相关 API

- [`UnitIR`](./UnitIR.md)：`conditions` / `asserts` 字段。
- [`DependencySet`](./DependencySet.md)：决定单元与其它单元的关系，而非启动前提。
