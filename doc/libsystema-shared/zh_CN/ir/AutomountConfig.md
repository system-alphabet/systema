# AutomountConfig

所属模块：`libsystema_shared::ir`

## 简介

自动挂载单元（`.automount`）的配置，对应 `[Automount]` 段落。automount 单元
在访问挂载点时按需触发同名的 `.mount` 单元。

## 定义

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AutomountConfig {
    pub where_: String,
    pub extra_options: String,
    pub timeout_idle_sec: u32,
    pub directory_mode: String,
}
```

## 字段

| 字段 | 指令 | 说明 |
| --- | --- | --- |
| `where_` | `Where=` | 自动挂载点路径。JSON 字段名仍为 `where_`。 |
| `extra_options` | `ExtraOptions=` | 额外挂载选项。 |
| `timeout_idle_sec` | `TimeoutIdleSec=` | 空闲多少秒后自动卸载；`0` 表示不自动卸载。 |
| `directory_mode` | `DirectoryMode=` | 自动创建的挂载点目录权限模式。 |

## 使用示例

```rust
use libsystema_shared::ir::AutomountConfig;

fn automount_for_data() -> AutomountConfig {
    AutomountConfig {
        where_: "/data".to_string(),
        extra_options: String::new(),
        timeout_idle_sec: 60,
        directory_mode: "0755".to_string(),
    }
}

fn main() {
    let config = automount_for_data();
    assert_eq!(config.where_, "/data");
    assert_eq!(config.timeout_idle_sec, 60);
}
```

## 相关 API

- [`MountConfig`](./MountConfig.md)：被自动触发的挂载配置。
- [`UnitIR`](./UnitIR.md)：`automount` 字段。
