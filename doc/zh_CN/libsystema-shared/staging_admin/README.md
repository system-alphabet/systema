# staging_admin 模块综述

## 在 systema 中的位置

单元定义进入 systema 分两步：先写进**暂存区（staging area）**，再**提交**到
System A 的活动单元集合。生产方（Finder、System D 等）负责写入，System A 负责
在提交时合并；而 `staging_admin` 是这条链路面向**运维人员**的观察与控制入口。

它在控制面（`control.socket`）上工作，供 `systema-stagingctl` 这类运维 CLI 使用，
不参与 worker 的任务执行。

## 为什么需要暂存区

- **收集与生效分离**：一次扫描发现的单元可能来自多个来源，先按 `(UID, name)`
  隔离存放，提交时才一起进入 System A，避免半成品参与依赖解析与调度。
- **可观察**：运维可以在提交前后查看暂存区内容，确认某个 finder 或设备来源
  究竟提交了什么。
- **可手动提交**：排查问题时可以不经完整重扫，直接提交某个来源的暂存内容。

## 它能做什么（概念层面）

- 列出当前存在的暂存区；
- 按 UID、名称或两者组合查询暂存内容，或一次性导出全部；
- 提交某个暂存区：把它合并进活动单元集合，并消费掉暂存内容。

所有操作都要求调用者是 root，或与 System A 相同的 UID；否则服务端直接拒绝。

## 与其它模块的关系

- [finder](../finder/README.md)：暂存区的主要生产方。
- [ir](../ir/README.md)：暂存区里保存的单元模型。
- [unitstate_admin](../unitstate_admin/README.md)：另一个运维视角——那个看的是
  “实际状态”，这里看的是“尚未生效的期望”。
- [error_code](../error_code/README.md)：权限/查询/提交失败的结构化错误。

## 本目录文档

- [`StagingAdmin`](./StagingAdmin.md)：暂存区查询与提交客户端。
