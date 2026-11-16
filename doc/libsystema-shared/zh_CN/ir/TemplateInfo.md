# TemplateInfo

所属模块：`libsystema_shared::ir`

## 简介

模板定义或其某个实例的元数据，存放在 [`UnitIR::template`](./UnitIR.md) 中。

模板单元（`getty@.service`）是定义而非可运行单元：其取值保留 `%i`、`%I` 等实例
占位符不展开，以便消费方据此实例化任意实例。实例（`getty@tty3.service`）则记录
替换所用的参数以及它特化的模板。生产者报告实例时，已经把该实例自己的 drop-in
合并进去——这正是“报告的实例优先于模板实例化”的原因。

## 定义

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateInfo {
    #[serde(default)]
    pub definition: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub of: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<String>,
}
```

## 字段

| 字段 | 说明 |
| --- | --- |
| `definition` | `true` 表示该 IR **就是**模板定义（`getty@.service`），而不是其实例。 |
| `of` | 该 IR 特化的模板名（`getty@tty3.service` 对应 `getty@.service`）。 |
| `parameters` | 替换进模板的参数，按顺序排列。systemd 最多一个（`%i` 实例字符串）；向量是为多参数模板后端保留的形状。 |

## 使用示例

```rust
use libsystema_shared::ir::{TemplateInfo, UnitIR, UnitType};

fn template_definition() -> UnitIR {
    UnitIR {
        id: "getty@.service".to_string(),
        unit_type: Some(UnitType::Service),
        template: Some(TemplateInfo {
            definition: true,
            of: None,
            parameters: vec![],
        }),
        ..Default::default()
    }
}

fn instance() -> UnitIR {
    UnitIR {
        id: "getty@tty3.service".to_string(),
        unit_type: Some(UnitType::Service),
        template: Some(TemplateInfo {
            definition: false,
            of: Some("getty@.service".to_string()),
            parameters: vec!["tty3".to_string()],
        }),
        ..Default::default()
    }
}

fn main() {
    assert!(template_definition().is_template());
    let inst = instance();
    assert!(inst.is_template_instance());
    assert_eq!(inst.template_of(), Some("getty@.service"));
    assert_eq!(inst.template_parameters(), ["tty3".to_string()]);
}
```

## 相关 API

- [`UnitIR`](./UnitIR.md)：`template` 字段与 `is_template*` / `template_*` 方法。
- [`UnitType`](./UnitType.md)：模板单元通常也是服务单元。
