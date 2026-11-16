# StagingAdmin

所属模块：`libsystema_shared::staging_admin`

## 简介

staging 区管理工具。它通过控制端口（`control.socket`）上的 `admin.staging`
请求查询或提交 staging 区。所有操作都要求调用方是 UID 0（root）或
System Allocator 运行所在的 UID，否则服务端返回权限拒绝。

## 定义

```rust
pub struct StagingAdmin {
    socket_path: String,
}

impl StagingAdmin {
    pub fn with_socket(socket_path: impl Into<String>) -> Self;

    pub async fn list(&self) -> Result<Vec<(u32, String)>>;

    pub async fn query_by_uid(&self, uid: u32) -> Result<AdminStagingResult>;

    pub async fn query_by_name(&self, name: &str) -> Result<AdminStagingResult>;

    pub async fn query(&self, uid: u32, name: &str) -> Result<AdminStagingResult>;

    pub async fn query_all(&self) -> Result<AdminStagingResult>;

    pub async fn commit(&self, uid: u32, name: &str) -> Result<AdminStagingResult>;
}
```

## 方法

| 方法 | 说明 |
| --- | --- |
| `with_socket` | 构造客户端，指向 System A 的 `control.socket` 路径。 |
| `list` | 列出全部 staging 区，返回 `(uid, name)`；`success == false` 时直接返回错误。 |
| `query_by_uid` | 返回指定 UID 的全部 staging 区，`units_json` 为原始 JSON 字节。 |
| `query_by_name` | 返回指定名称的全部 staging 区。 |
| `query` | 返回 `(uid, name)` 精确匹配的 staging 区。 |
| `query_all` | 返回全部 staging 区的完整数据。 |
| `commit` | `name` 非空时只合并并消费 `(uid, name)`；为空时提交该 UID 名下的所有区。 |

## 使用示例

```rust
use libsystema_shared::staging_admin::StagingAdmin;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let admin = StagingAdmin::with_socket("/run/systema/control.socket");

    for (uid, name) in admin.list().await? {
        println!("staging area uid={uid} name={name}");
    }

    // 查询并打印某个区的单元数量。
    let result = admin.query(0, "boot-scan").await?;
    if !result.success {
        anyhow::bail!("{}", result.message);
    }
    for entry in &result.entries {
        println!(
            "uid={} name={} units={} json_bytes={}",
            entry.uid,
            entry.name,
            entry.unit_count,
            entry.units_json.len()
        );
    }

    // 把 root 名下所有 staging 区提交进活动单元集。
    let committed = admin.commit(0, "").await?;
    println!("success={} message={}", committed.success, committed.message);

    Ok(())
}
```

## 相关 API

- [`connect_control`](../ipc/connect_control.md)：控制端口连接与权限提示。
- [`UnitFinder`](../finder/UnitFinder.md)：worker 侧的 staging 注册/提交入口。
- [`SysError`](../error_code/SysError.md)：错误码与占位符。
