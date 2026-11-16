# MountConfig

所属模块：`libsystema_shared::ir`

## 简介

挂载单元（`.mount`）的配置，对应 `[Mount]` 段落。

## 定义

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MountConfig {
    pub what: String,
    pub where_: String,
    pub type_: String,
    pub options: String,
    pub timeout_sec: u32,
}
```

## 字段

| 字段 | 指令 | 说明 |
| --- | --- | --- |
| `what` | `What=` | 要挂载的设备或远端文件系统。 |
| `where_` | `Where=` | 挂载点路径。JSON 字段名仍为 `where_`。 |
| `type_` | `Type=` | 文件系统类型，如 `ext4`、`nfs`。JSON 字段名仍为 `type_`。 |
| `options` | `Options=` | 传给 mount 的选项。 |
| `timeout_sec` | `TimeoutSec=` | 挂载超时（秒）。 |

## 使用示例

```rust
use libsystema_shared::ir::MountConfig;

fn data_mount() -> MountConfig {
    MountConfig {
        what: "/dev/sda2".to_string(),
        where_: "/data".to_string(),
        type_: "ext4".to_string(),
        options: "defaults,noatime".to_string(),
        timeout_sec: 90,
    }
}

fn main() {
    let mount = data_mount();
    let json = serde_json::to_string(&mount).unwrap();
    assert!(json.contains("\"where_\""));
    assert!(json.contains("\"type_\""));
}
```

## 相关 API

- [`UnitIR`](./UnitIR.md)：`mount` 字段。
- [`AutomountConfig`](./AutomountConfig.md)：按需触发挂载的 automount 单元。
