# calls

所属模块：`libsystema_shared::manager::calls`

## 简介

常用 `manager.*` 请求的类型化封装。每个函数都通过
[`ControlClient::call`](./ControlClient.md#方法) 发送请求；应答 `success == false`
时把服务端 `message` 转成 `anyhow::Error`。

## 函数

| 签名 | 请求方法 | 返回 |
| --- | --- | --- |
| `list_snapshots(client) -> Result<Vec<UnitSnapshot>>` | `manager.list_snapshots` | 当前全部单元快照，用于初始化镜像 |
| `unit_snapshot(client, name) -> Result<UnitSnapshot>` | `manager.unit_snapshot` | 单个单元快照；服务端会把别名解析为规范名 |
| `list_jobs(client) -> Result<Vec<JobInfo>>` | `manager.list_jobs` | 当前在途 job |
| `load_unit_snapshot(client, name) -> Result<(LoadUnitResult, UnitSnapshot)>` | `manager.load_unit` + `manager.unit_snapshot` | 确认单元已注册并取快照 |
| `get_unit_by_invocation(client, invocation_id) -> Result<String>` | `manager.get_unit_by_invocation` | 规范单元名 |
| `get_unit_by_pid(client, pid) -> Result<String>` | `manager.get_unit_by_pid` | 规范单元名 |

## 使用示例

初始化镜像：

```rust
use sysa::manager::calls;

let units = calls::list_snapshots(&client).await?;
let jobs = calls::list_jobs(&client).await?;
mirror.seed(units, jobs);
```

按名字解析并确保单元已加载：

```rust
let (load, snapshot) = calls::load_unit_snapshot(&client, "nginx.service").await?;
println!("{} -> {}", load.name, snapshot.active_state);
```

## 说明

- start / stop / restart / reload 不单独封装：控制面统一走
  `call("manager.enqueue", &EnqueueJobRequest { .. })`，用 `job_type` 区分动作。
  完整示例见 [`ControlClient`](./ControlClient.md#使用示例)。
- 这些函数都是“一问一答”；推送事件请用
  [`ControlClient::events`](./ControlClient.md#方法) 配合
  [`ControlEvent`](./ControlEvent.md)。

## 相关 API

- [`ControlClient`](./ControlClient.md)：底层 `call` / `events`。
- [`UnitSnapshot`](../../../README.md)：控制面快照模型（生成的 protobuf 类型）。
