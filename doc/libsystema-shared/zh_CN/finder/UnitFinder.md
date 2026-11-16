# UnitFinder

所属模块：`libsystema_shared::finder`

## 简介

Finder 客户端。Finder（例如 `systema-sysf-systemd`）负责解析磁盘上的单元
定义，把 `UnitIR` 序列化成 JSON 提交给 System A 的 staging 区；System A
只消费解析后的模型，不亲自解析配置文件。

`UnitFinder` 的每个方法都会新建一条到 `allocator.sock` 的连接，先完成
`ipc.version` 版本协商，再发送请求并读取回复。

## 定义

```rust
pub struct UnitFinder {
    socket_path: String,
}

impl UnitFinder {
    pub fn with_socket(socket_path: impl Into<String>) -> Self;

    pub async fn register_units(
        &self,
        name: &str,
        units_json: Vec<u8>,
    ) -> Result<UnitRegistrationAck>;

    pub async fn commit_units(&self, name: &str) -> Result<UnitRegistrationAck>;

    pub async fn query_staging(&self, name: &str) -> Result<StagingQueryResult>;
}
```

## 方法

| 方法 | 说明 |
| --- | --- |
| `with_socket` | 构造客户端，指向 System A 的 IPC socket 路径。 |
| `register_units` | 把 JSON 序列化的 `unit_id → UnitIR` 映射注册到调用方的 staging 区，返回 `UnitRegistrationAck`。 |
| `commit_units` | 提交（合并进活动单元集）当前 UID 下名为 `name` 的 staging 区。 |
| `query_staging` | 查询当前 UID 下名为 `name` 的 staging 区，返回 `StagingQueryResult`。 |

错误处理：连接失败为 `ErrorCode::ConnectFailed`；对端提前关闭为
`AckConnectionClosed` / `QueryConnectionClosed`；回复方法名不符为
`ExpectedFinderAck` / `ExpectedStagingQueryResult`；载荷无法解码为
`AckDecodeFailed` / `StagingQueryDecodeFailed`。

## 使用示例

> 示例使用 `serde_json` 序列化 IR；`libsystema-shared` 本身只提供 IR 的
> `Serialize`/`Deserialize` 派生，调用方需自行依赖 `serde_json`。

```rust
use std::collections::BTreeMap;

use libsystema_shared::ir::{UnitIR, UnitType};
use libsystema_shared::finder::UnitFinder;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let finder = UnitFinder::with_socket("/run/systema/allocator.sock");

    // 构造一个最小的 UnitIR 并编码为 JSON 映射。
    let mut units: BTreeMap<String, UnitIR> = BTreeMap::new();
    units.insert(
        "example.service".to_string(),
        UnitIR {
            id: "example.service".to_string(),
            unit_type: Some(UnitType::Service),
            description: Some("Example service".to_string()),
            source_format: Some("systemd".to_string()),
            ..Default::default()
        },
    );
    let units_json = serde_json::to_vec(&units)?;

    let ack = finder.register_units("boot-scan", units_json).await?;
    println!("registered {} units: {}", ack.unit_count, ack.message);

    let ack = finder.commit_units("boot-scan").await?;
    println!("committed {} units", ack.unit_count);

    let staged = finder.query_staging("boot-scan").await?;
    println!("staging has {} units", staged.unit_count);

    Ok(())
}
```

## 相关 API

- [`UnitIR`](../ir/UnitIR.md)：提交给 staging 区的模型。
- [`StagingAdmin`](../staging_admin/StagingAdmin.md)：管理员侧的 staging 查询接口。
- [`SysError`](../error_code/SysError.md)：错误码与占位符。
