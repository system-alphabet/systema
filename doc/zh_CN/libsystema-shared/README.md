# libsystema-shared 文档综述

本目录收录 `libsystema-shared` 的简体中文文档。根文档给出 **System A（Allocator）**、
**System Worker** 与 **Controller** 的协作全貌；每个子目录的 `README.md` 综述该模块
在 systema 组件结构中的位置与职责；其余以 API 命名的文档只讲对应接口的用法。

## 协作结构

### 进程与连接视图

```text
        systemctl / busctl / 桌面组件
                    │  D-Bus  org.freedesktop.systemd1
                    ▼
        ┌───────────────────────────┐
        │ systema-sysw.systemd      │
        └─────────────┬─────────────┘
                      │ control.socket（控制面）
                      ▼
        ┌───────────────────────────┐        ┌────────────────────┐
        │ systema-sysa  (System A)  │◄───────│ systema-sysi       │
        │  期望状态 · 依赖 · 调度   │ notify │ 拉起 / 监督 / 退出 │
        └─────────────┬─────────────┘        └────────────────────┘
                      │ allocator.sock（工作负载面）
      ┌───────────────┼──────────────────────────────┐
      ▼             ▼                              ▼
 ┌──────────┐    ┌──────────┐                   ┌──────────┐
 │ System S │    │ System K │   ……              │ System F │
 │ service  │    │ socket   │                   │ (Finder) │
 └──────────┘    └──────────┘                   └──────────┘
   每个 worker 都实现 Controller 操作表
```

### 一次启动的协作流

```text
单元文件 ──解析──► Finder ──register/commit（UnitIR）──► System A
                                                          │
                                               依赖解析 + 事务调度
                                                          │
                                          method.call / job 事件
                                                          ▼
                                             Worker（Controller 操作表）
                                                          │
                                            实际状态推送（unit.state_update）
                                                          ▼
                                        System A 状态缓存（只缓存，不产生）
                                                          │
                                                control.socket 事件
                                                          ▼
                                                 SysW ──► systemctl
```

协作要点：

1. **System A 只持期望状态**；实际状态由 worker 以事件推回，System A 只做缓存。
2. **只有 Finder 解析单元文件**；System A 消费的是解析后的统一 IR。
3. **Controller 是 worker 对外的唯一操作面**；System A 按单元类型把任务路由给
   注册了该类型的 worker，不关心其实现。
4. **两个平面在 proto 层隔离**：工作负载面（`allocator.sock`）与控制/运维面
   （`control.socket`）的消息不能互相引用。fd 传递另有独立的 `fdpass.sock`。
5. **退出是握手的**：worker 不监听进程信号，由调用方请求退出；循环在边界处完成
   本地清理后发送 `worker.exit`，再等待 System A 关闭连接。

## 目录导览

| 目录 | 在 systema 中的作用 |
| --- | --- |
| [controller/](controller/README.md) | worker 的操作表与跨组件状态模型 |
| [worker_ipc/](worker_ipc/README.md) | worker 侧 IPC 运行时（注册、分发、状态、退出） |
| [finder/](finder/README.md) | 单元提供者接口：IR 的提交与 staging |
| [staging_admin/](staging_admin/README.md) | 运维视角查询/提交 staging 区 |
| [unitstate_admin/](unitstate_admin/README.md) | 运维视角导出实际状态快照 |
| [ir/](ir/README.md) | 统一单元中间表示 |
| [ir/config/](ir/config/README.md) | 扁平指令字典的读取约定 |
| [ipc/](ipc/README.md) | 线协议：分帧、信封、版本握手、fd 传递 |
| [error_code/](error_code/README.md) | 跨组件边界的结构化错误码 |

## 依赖方向

```text
 libsysa（package sysa）
   ├─ 重导出 ─► libsystema-shared
   └─ 重导出 ─► libsystema-sysa
                     │ 单向依赖
                     ▼
               libsystema-shared       契约 + worker 运行时（不依赖服务端）
```

- `libsystema-shared` 拥有：proto 契约、信封分帧、单元 IR、错误码、`UnitStatus`
  模型、worker 运行时，以及 finder / staging / unit-state 客户端。实际上就是对SysA IPC的封装
- `libsystema-sysa` 只追加服务端版本判定与会话握手辅助，不允许被 shared 反向依赖。
- `libsysa` 重导出两者，使 `sysa::ipc`、`sysa::proto`、`sysa::worker_ipc` 等
  下游路径保持不变。

## 阅读建议

先读本页了解全局，再按角色进入：

- 想新增 worker：读 [controller/](controller/README.md) 与
  [worker_ipc/](worker_ipc/README.md)；
- 想新增单元解析器：读 [ir/](ir/README.md)、[ir/config/](ir/config/README.md)
  与 [finder/](finder/README.md)；
- 想写运维/调试工具：读 [staging_admin/](staging_admin/README.md)、
  [unitstate_admin/](unitstate_admin/README.md) 与 [error_code/](error_code/README.md)；
- 想理解传输与握手：读 [ipc/](ipc/README.md)。
