# finder 模块综述

## 在 systema 中的位置

systema 的职责划分很严格：**只有 Finder 解析单元文件，System A 只消费解析结果。**
Finder 不是 System A 的一部分，而是独立的单元类型提供者进程（如
`systema-sysf.systemd` 解析 systemd 单元文件）。

`finder` 模块是 Finder 与 System A 之间的提交接口，描述“发现单元 → 提交定义 →
提交生效”这一套协作。第三方想支持新的 init 格式（SysV、OpenRC、Runit……），
只需实现自己的 Finder 并调用这套接口，不必改动 System A。

## 协作流程

```text
Finder 进程
   │ 1. register_units：声明本次发现的单元集合（统一 IR）
   │ 2. commit_units：把这些单元合并进活动集合
   ▼
System A（staging 区 → 活动单元表）
   │ 3. ack：确认注册/提交结果
   ▼
Finder 结束本轮扫描
```

- **staging 区**按 `(UID, name)` 隔离：不同来源（不同 finder、System D 的设备注入、
  不同用户域）可以并发写入，互不覆盖。
- 提交前内容只存在于暂存区；提交后才进入 System A 的单元模型，参与依赖解析与调度。
  运维工具（见 [staging_admin](../staging_admin/README.md)）可以在两者之间观察和
  手动提交。

## 边界与触发

- Finder **只产出模型**，不做依赖解析、不做调度、不碰进程。
- System A **不读单元文件**，只处理 Finder 提交的 IR；IR 中的类型专属指令保持
  扁平字典形态（见 [ir](../ir/README.md)）。
- 触发方式：System I 在控制阶段请求 `daemon_reload`，System A 随即按需拉起
  Finder（Finder 不在 System I 的常驻监督集合中）。System D 等动态来源也通过
  同一 staging 通道注入单元。

## 与其它模块的关系

- [ir](../ir/README.md)：提交内容的数据模型。
- [worker_ipc](../worker_ipc/README.md)：Finder 复用同一套连接、分帧与信封。
- [staging_admin](../staging_admin/README.md)：暂存区的运维视图。
- [error_code](../error_code/README.md)：注册/提交失败的结构化错误。

## 本目录文档

- [`UnitFinder`](./UnitFinder.md)：注册、提交与查询暂存区的客户端。
