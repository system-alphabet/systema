# unitstate_admin 模块综述

## 在 systema 中的位置

systema 把状态分成两份：

- **期望状态**由 System A 持有（这个单元应该处于什么状态、有哪些 job 在跑）；
- **实际状态**由各个 worker 产生，以事件推回 System A，System A 只做缓存。

`unitstate_admin` 是**实际状态缓存**的运维导出接口。它在控制面
（`control.socket`）上把这些缓存流式导出，供 `systema-unitstatectl` 等工具查看，
全程不需要任何 worker 参与。

## 它解决什么问题

- 排查“System A 认为单元是什么状态”与“worker 实际报告了什么”是否一致；
- 在没有 D-Bus、没有 SysW 的环境里快速拿到全量状态快照；
- 为自动化检查提供机器可读输出（YAML/JSON）。

导出是**流式**的：逐条发送单元条目，最后以结束标记收尾，因此大量单元也不会
一次性占用大块内存。

## 边界

- 它读的是 System A 的缓存，不是实时去问 worker；缓存的新鲜度取决于 worker
  最近一次推送。
- 与 [staging_admin](../staging_admin/README.md) 相对：那边看“尚未生效的定义”，
  这边看“已经上报的运行事实”。
- 权限与其它 admin 操作一致：调用者需为 root 或 System A 运行 UID。

## 与其它模块的关系

- [controller](../controller/README.md)：`UnitStatus` 是缓存的来源格式。
- [worker_ipc](../worker_ipc/README.md)：worker 通过它推送状态事件。
- [error_code](../error_code/README.md)：权限/连接/解码失败的结构化错误。

## 本目录文档

- [`UnitStateAdmin`](./UnitStateAdmin.md)：流式导出客户端。
- [`UnitStateResult`](./UnitStateResult.md)：导出结果摘要。
