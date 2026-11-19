# DependencySet

所属模块：`libsystema_shared::ir`

## 简介

单元之间的依赖关系集合。它是 [`UnitIR::dependencies`](./UnitIR.md) 的值；
提供时整体替换旧的依赖集合。

## 定义

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DependencySet {
    pub after: HashSet<String>,
    pub before: HashSet<String>,
    pub requires: HashSet<String>,
    pub wants: HashSet<String>,
    pub conflicts: HashSet<String>,
    pub binds_to: HashSet<String>,
    pub requisite: HashSet<String>,
    pub part_of: HashSet<String>,
    pub upholds: HashSet<String>,
    pub on_success: HashSet<String>,
    pub on_failure: HashSet<String>,
    pub propagates_reload_to: HashSet<String>,
    pub success_action: String,
    pub default_dependencies: bool,
}
```

## 字段

| 字段 | 对应指令 | 说明 |
| --- | --- | --- |
| `after` | `After=` | 必须在本单元之前启动的单元。 |
| `before` | `Before=` | 应在启动本单元之前启动的单元。 |
| `requires` | `Requires=` | 硬依赖：这些单元必须处于 active。 |
| `wants` | `Wants=` | 软依赖：能启动就启动，失败可忽略。 |
| `conflicts` | `Conflicts=` | 启动本单元时停止这些单元。 |
| `binds_to` | `BindsTo=` | 生命周期绑定：被绑定单元停止则本单元停止。 |
| `requisite` | `Requisite=` | 类似 `Requires`，但要求依赖**已经**处于 active。 |
| `part_of` | `PartOf=` | 本单元所属的一组单元（停止会传播）。 |
| `upholds` | `Upholds=` | 需要持续保持激活的单元。 |
| `on_success` | `OnSuccess=` | 成功时触发的 target。 |
| `on_failure` | `OnFailure=` | 失败时触发的 target。 |
| `propagates_reload_to` | `PropagatesReloadTo=` | reload 传播目标。 |
| `success_action` | `SuccessAction=` | 成功后执行的动作（如 `poweroff-force`）。 |
| `default_dependencies` | `DefaultDependencies=` | 是否应用默认依赖。 |

## 方法

| 签名 | 说明 |
| --- | --- |
| `pub fn is_empty(&self) -> bool` | 所有集合字段均为空时返回 `true`（`success_action` 与 `default_dependencies` 不参与判断）。 |

## 使用示例

```rust
use libsystema_shared::ir::DependencySet;

fn deps_for_web() -> DependencySet {
    let mut deps = DependencySet::default();
    deps.after.insert("network.target".to_string());
    deps.requires.insert("network.target".to_string());
    deps.wants.insert("logging.target".to_string());
    deps
}

fn main() {
    let deps = deps_for_web();
    assert!(!deps.is_empty());
    assert!(deps.requires.contains("network.target"));

    let empty = DependencySet::default();
    assert!(empty.is_empty());
}
```

## 相关 API

- [`UnitIR`](./UnitIR.md)：`dependencies` 字段。
- [`Condition`](./Condition.md)：与依赖不同，条件决定单元能否启动。
