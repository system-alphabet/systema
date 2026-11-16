# PathConfig

所属模块：`libsystema_shared::ir`

## 简介

路径单元（`.path`）的配置，对应 `[Path]` 段落。路径单元监视文件系统变化，
并据此激活另一个单元。

## 定义

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PathConfig {
    pub path_exists: Vec<String>,
    pub path_exists_glob: Vec<String>,
    pub path_changed: Vec<String>,
    pub path_modified: Vec<String>,
    pub directory_not_empty: Vec<String>,
    pub unit: String,
    pub make_directory: bool,
    pub directory_mode: String,
    pub trigger_limit_interval_sec: u32,
    pub trigger_limit_burst: u32,
}
```

## 字段

| 字段 | 指令 | 说明 |
| --- | --- | --- |
| `path_exists` | `PathExists=` | 路径存在时触发。 |
| `path_exists_glob` | `PathExistsGlob=` | 匹配 glob 的路径存在时触发。 |
| `path_changed` | `PathChanged=` | 路径内容变化时触发。 |
| `path_modified` | `PathModified=` | 路径被修改（含元数据）时触发。 |
| `directory_not_empty` | `DirectoryNotEmpty=` | 目录非空时触发。 |
| `unit` | `Unit=` | 要激活的单元；缺省时与 path 单元同名。 |
| `make_directory` | `MakeDirectory=` | 是否自动创建被监视目录。 |
| `directory_mode` | `DirectoryMode=` | 自动创建目录的权限。 |
| `trigger_limit_interval_sec` | `TriggerLimitIntervalSec=` | 触发速率限制的统计窗口。 |
| `trigger_limit_burst` | `TriggerLimitBurst=` | 窗口内允许的最大触发次数。 |

## 使用示例

```rust
use libsystema_shared::ir::PathConfig;

fn watch_uploads() -> PathConfig {
    PathConfig {
        directory_not_empty: vec!["/var/spool/uploads".to_string()],
        unit: "process-uploads.service".to_string(),
        make_directory: true,
        directory_mode: "0755".to_string(),
        trigger_limit_interval_sec: 2,
        trigger_limit_burst: 200,
        ..Default::default()
    }
}

fn main() {
    let path = watch_uploads();
    assert_eq!(path.directory_not_empty[0], "/var/spool/uploads");
    assert!(path.make_directory);
}
```

## 相关 API

- [`UnitIR`](./UnitIR.md)：`path` 字段。
- [`ServiceConfig`](./ServiceConfig.md)：通常被路径单元激活的服务。
