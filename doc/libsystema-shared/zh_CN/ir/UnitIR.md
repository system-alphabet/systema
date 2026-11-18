# UnitIR

所属模块：`libsystema_shared::ir`

## 简介

单个单元的统一中间表示（IR）。所有 Finder（systemd、SysV、OpenRC、Runit 等）
都产出该类型；System Allocator 只消费 `UnitIR`，从不直接解析配置文件。

只有 `id` 是必填字段，其余字段都是可选的，因此一次提交可以是**部分更新**：
当单元已存在于 System A 的运行时缓存时，只覆盖本次提供的字段，其它字段保持不变；
单元尚不存在时则用提供的字段新建，缺少必需字段（如 `unit_type`）会向提交方返回错误。

IR 只对**公共字段**建模。单元类型专属的 section（`[Service]`、`[Mount]`、
`[Timer]`……）以扁平指令字典 [`config`](#类型专属指令-config) 承载，键就是
systemd 单元文件中的指令原名，section 由 `unit_type` 隐含。解析这些指令是
消费方（System A 或拥有该单元类型的 worker）的职责，IR 本身不解释它们。

IR 以 JSON 字节承载在 protobuf 消息的 `units_json` 字段中。

## 定义

```rust
pub type ConfigMap = serde_json::Map<String, serde_json::Value>;

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
    pub config: Option<ConfigMap>,
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
| `config` | 类型专属 section 的扁平指令字典；详见下文。 |
| `conditions` | 启动前必须满足的条件。 |
| `asserts` | 不满足即硬失败（assert）的条件。 |
| `wanted_by` | `[Install] WantedBy=`：哪些 target 想要本单元（只是 `systemctl enable` 的配方）。 |
| `required_by` | `[Install] RequiredBy=`。 |
| `enabled` | Finder 报告的“是否属于某个 target 的启动集”；动态单元或部分更新时为 `None`。 |
| `unit_file_state` | Finder 报告的完整 `UnitFileState`：`enabled`、`enabled-runtime`、`linked`、`linked-runtime`、`static`、`indirect`、`disabled` 之一；`None` 时由 System A 依据 `enabled` 与安装信息推导。 |
| `template` | 模板元数据；普通单元为 `None`。 |

## 类型专属指令 `config`

`config` 是一个 JSON 对象（[`ConfigMap`](./ConfigMap.md)），结构完全对应单元文件：

- 键是指令名，保留 systemd 的拼写与大小写，例如 `ExecStart`、`Type`、
  `ListenStream`、`Delegate`；不写 section 名。
- 单值指令映射为 JSON 字符串，可重复指令映射为字符串数组，例如
  `{"Type": "notify", "ExecStart": ["/usr/bin/daemon", "/usr/bin/other"]}`。
- 所属 section 由 `unit_type` 隐含：`Service` → `[Service]`，`Mount` →
  `[Mount]`，`Timer` → `[Timer]`，以此类推。
- 值保留单元文件中的原样（`Exec*=` 的 `-`/`+` 等前缀不会被剥离），消费方
  按解析单元文件的同一套规则解析。
- `None` 表示“本次未报告”，部分更新时保留旧值；空对象表示“已报告，section
  为空”。

`libsystema_shared::ir::config` 提供通用取值函数，避免各消费方手写 JSON 匹配：

```rust
pub fn parse_json(bytes: &[u8]) -> anyhow::Result<ConfigMap>;
pub fn to_json_bytes(map: &ConfigMap) -> Vec<u8>;
pub fn get_string(map: &ConfigMap, key: &str) -> Option<String>;
pub fn get_string_or(map: &ConfigMap, key: &str, default: &str) -> String;
pub fn get_strings(map: &ConfigMap, key: &str) -> Vec<String>;
pub fn get_bool(map: &ConfigMap, key: &str) -> bool;
pub fn get_u32(map: &ConfigMap, key: &str) -> Option<u32>;
pub fn get_u32_or(map: &ConfigMap, key: &str, default: u32) -> u32;
pub fn get_i32_or(map: &ConfigMap, key: &str, default: i32) -> i32;
pub fn get_mode(map: &ConfigMap, key: &str) -> Option<u32>;
```

`get_bool` 接受 `yes/no`、`true/false`、`1/0`、`on/off`；`get_mode` 把
`"0755"` 这类权限值按八进制解析。

## 方法

| 签名 | 说明 |
| --- | --- |
| `pub fn is_template(&self) -> bool` | 是否为模板定义（如 `getty@.service`），而非可运行单元。 |
| `pub fn is_template_instance(&self) -> bool` | 是否为模板实例。 |
| `pub fn template_of(&self) -> Option<&str>` | 实例所特化的模板名。 |
| `pub fn template_parameters(&self) -> &[String]` | 替换进模板的参数；非实例返回空切片。 |

## 使用示例

```rust
use libsystema_shared::ir::{ConfigMap, UnitIR, UnitType};

fn new_service() -> UnitIR {
    let mut config = ConfigMap::new();
    config.insert("Type".to_string(), serde_json::json!("simple"));
    config.insert(
        "ExecStart".to_string(),
        serde_json::json!(["/usr/bin/example"]),
    );
    config.insert("WorkingDirectory".to_string(), serde_json::json!("/"));

    UnitIR {
        id: "example.service".to_string(),
        unit_type: Some(UnitType::Service),
        description: Some("Example service".to_string()),
        source_format: Some("systemd".to_string()),
        config: Some(config),
        enabled: Some(true),
        unit_file_state: Some("enabled".to_string()),
        ..Default::default()
    }
}

fn main() {
    let ir = new_service();
    assert!(!ir.is_template());
    assert_eq!(ir.unit_type.unwrap().as_str(), "service");

    let config = ir.config.as_ref().unwrap();
    assert_eq!(
        libsystema_shared::ir::config::get_strings(config, "ExecStart"),
        ["/usr/bin/example"]
    );

    // 序列化为 JSON，供 finder.register_units 提交。
    let json = serde_json::to_vec(&ir).unwrap();
    println!("{}", String::from_utf8_lossy(&json));
}
```

部分更新（只改描述，保留已有 `config`）：

```rust
use libsystema_shared::ir::UnitIR;

let patch = UnitIR {
    id: "example.service".to_string(),
    description: Some("New description".to_string()),
    ..Default::default()
};
```

## 相关 API

- [`ConfigMap`](./ConfigMap.md)：`config` 的扁平指令字典及其编码约定。
- [`parse_json`](./config/parse_json.md) / [`to_json_bytes`](./config/to_json_bytes.md)：
  字节与字典互转。
- [`get_string`](./config/get_string.md)、[`get_string_or`](./config/get_string_or.md)、
  [`get_strings`](./config/get_strings.md)、[`get_bool`](./config/get_bool.md)、
  [`get_u32`](./config/get_u32.md)、[`get_u32_or`](./config/get_u32_or.md)、
  [`get_i32_or`](./config/get_i32_or.md)、[`get_mode`](./config/get_mode.md)：类型化取值。
- [`UnitType`](./UnitType.md)：`unit_type` 的取值，同时隐含 `config` 所属 section。
- [`DependencySet`](./DependencySet.md)：`dependencies` 字段。
- [`TemplateInfo`](./TemplateInfo.md)：模板与实例元数据。
- [`Condition`](./Condition.md)：`conditions` / `asserts` 字段。
- [`UnitFinder`](../finder/UnitFinder.md)：提交 IR 的客户端。
