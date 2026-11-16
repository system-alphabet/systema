# UnitIR

所属模块：`libsystema_shared::ir`

## 简介

单个单元的统一中间表示（IR）。所有 Finder（systemd、SysV、OpenRC、Runit 等）
都产出该类型；System Allocator 只消费 `UnitIR`，从不直接解析配置文件。

只有 `id` 是必填字段，其余字段都是可选的，因此一次提交可以是**部分更新**：
当单元已存在于 System A 的运行时缓存时，只覆盖本次提供的字段，其它字段保持不变；
单元尚不存在时则用提供的字段新建，缺少必需字段（如 `unit_type`）会向提交方返回错误。

IR 以 JSON 字节承载在 protobuf 消息的 `units_json` 字段中。

## 定义

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UnitIR {
    pub id: String,
    pub unit_type: Option<UnitType>,
    pub description: Option<String>,
    pub source_format: Option<String>,
    pub source_path: Option<String>,
    pub aliases: Vec<String>,
    pub alias_of: Option<String>,
    pub slice: Option<String>,
    pub dependencies: Option<DependencySet>,
    pub service: Option<ServiceConfig>,
    pub mount: Option<MountConfig>,
    pub automount: Option<AutomountConfig>,
    pub timer: Option<TimerConfig>,
    pub socket: Option<SocketConfig>,
    pub path: Option<PathConfig>,
    pub resource_control: Option<ResourceControl>,
    pub conditions: Option<Vec<Condition>>,
    pub asserts: Option<Vec<Condition>>,
    pub wanted_by: Option<Vec<String>>,
    pub required_by: Option<Vec<String>>,
    pub enabled: Option<bool>,
    pub unit_file_state: Option<String>,
    pub template: Option<TemplateInfo>,
}
```

## 字段

| 字段 | 说明 |
| --- | --- |
| `id` | 规范单元标识，例如 `nginx.service`、`network`、`sshd`。 |
| `unit_type` | 单元类型；新建单元时必需。 |
| `description` | 人类可读描述。 |
| `source_format` | 来源格式，例如 `systemd`、`sysv`、`openrc`、`runit`。 |
| `source_path` | 加载该单元的文件路径。 |
| `aliases` | 指向本单元的别名；systemd finder 已改为每个别名报告一条 `alias_of` 记录，此字段保留给只声明别名的生产者。 |
| `alias_of` | 非 `None` 时表示这是一条**别名记录**：`id` 是规范单元 `alias_of` 的另一个名字。消费方不得为别名记录创建单元；别名记录其它字段无意义。 |
| `slice` | `[Unit] Slice=` 的父 slice；缺省时消费方在应用阶段默认 `system.slice`。 |
| `dependencies` | 依赖集合；提供时整体替换旧值。 |
| `service` / `mount` / `automount` / `timer` / `socket` / `path` | 各单元类型的专属配置。 |
| `resource_control` | 从单元文件解析出的 cgroup 资源限制。 |
| `conditions` | 启动前必须满足的条件。 |
| `asserts` | 不满足即硬失败（assert）的条件。 |
| `wanted_by` | `[Install] WantedBy=`：哪些 target 想要本单元（只是 `systemctl enable` 的配方）。 |
| `required_by` | `[Install] RequiredBy=`。 |
| `enabled` | Finder 报告的“是否属于某个 target 的启动集”；动态单元或部分更新时为 `None`。 |
| `unit_file_state` | Finder 报告的完整 `UnitFileState`：`enabled`、`enabled-runtime`、`linked`、`linked-runtime`、`static`、`indirect`、`disabled` 之一；`None` 时由 System A 依据 `enabled` 与安装信息推导。 |
| `template` | 模板元数据；普通单元为 `None`。 |

## 方法

| 签名 | 说明 |
| --- | --- |
| `pub fn is_template(&self) -> bool` | 是否为模板定义（如 `getty@.service`），而非可运行单元。 |
| `pub fn is_template_instance(&self) -> bool` | 是否为模板实例。 |
| `pub fn template_of(&self) -> Option<&str>` | 实例所特化的模板名。 |
| `pub fn template_parameters(&self) -> &[String]` | 替换进模板的参数；非实例返回空切片。 |

## 使用示例

> 示例使用 `serde_json` 序列化 IR；`libsystema-shared` 本身只提供
> `Serialize`/`Deserialize` 派生，调用方需自行依赖 `serde_json`。

```rust
use libsystema_shared::ir::{ServiceConfig, UnitIR, UnitType};

fn new_service() -> UnitIR {
    UnitIR {
        id: "example.service".to_string(),
        unit_type: Some(UnitType::Service),
        description: Some("Example service".to_string()),
        source_format: Some("systemd".to_string()),
        service: Some(ServiceConfig {
            exec_start: vec![],
            working_directory: "/".to_string(),
            ..Default::default()
        }),
        enabled: Some(true),
        unit_file_state: Some("enabled".to_string()),
        ..Default::default()
    }
}

fn main() {
    let sr = new_service();
    assert!(!sr.is_template());
    assert_eq!(sr.unit_type.unwrap().as_str(), "service");

    // 序列化为 JSON，供 finder.register_units 提交。
    let json = serde_json::to_vec(&sr).unwrap();
    println!("{}", String::from_utf8_lossy(&json));
}
```

部分更新（只改描述）：

```rust
use libsystema_shared::ir::UnitIR;

let patch = UnitIR {
    id: "example.service".to_string(),
    description: Some("New description".to_string()),
    ..Default::default()
};
```

## 相关 API

- [`UnitType`](./UnitType.md)、[`DependencySet`](./DependencySet.md)、[`ServiceConfig`](./ServiceConfig.md) 等类型化配置。
- [`TemplateInfo`](./TemplateInfo.md)：模板与实例元数据。
- [`UnitFinder`](../finder/UnitFinder.md)：提交 IR 的客户端。
