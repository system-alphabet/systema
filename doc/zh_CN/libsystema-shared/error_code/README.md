# error_code 模块综述

## 在 systema 中的位置

systema 的组件分散在多个进程里，一次失败往往跨越两三个边界：worker 解码不了
任务、Finder 提交被拒、运维工具没权限、socket 激活拿不到 fd……每个边界都需要
一种**机器可读**的方式说明“哪里错了、错误参数是什么”。

`error_code` 模块就是这套跨边界失败语言：稳定错误码 + 结构化参数。它同时被
worker 运行时、Finder/Admin 客户端和线协议层使用。

## 设计原则

1. **库里没有用户文案**：本模块不携带任何消息文本（连英文兜底都没有）。错误码和
   命名参数是唯一载荷；把“码 + 参数”渲染成人能看懂的文字，是二进制层
   （`libsysa` 的本地化表）的职责。这样共享库不依赖 gettext，也不会把某一种语言
   固化进协议。
2. **错误码是公开契约**：调用方可以按错误码分支处理（例如“权限不足时提示用户
   加 sudo 或切到 `--user`”），因此错误码的含义跨版本保持稳定。
3. **技术渲染不是用户界面**：错误类型自身的 `Display` 只用于日志与协议字段，
   形如 `Code(arg=value)`，绝不能直接展示给最终用户。

## 覆盖的失败面

错误码按系统边界分组，主要包括：

- 连接与分帧（连接失败、权限不足、信封编解码失败、帧读取失败）；
- 版本协商（对端提前关闭、应答类型不符、版本不兼容）；
- fd 传递（发送/接收失败、控制消息缺失、通道关闭）；
- Finder 的注册、提交与暂存区查询；
- 运维面的暂存区与单元状态导出；
- 任务配置解码等。

## 与其它模块的关系

- [ipc](../ipc/README.md)：线协议失败的出口。
- [worker_ipc](../worker_ipc/README.md)：任务失败与协议异常的表达。
- [finder](../finder/README.md)、[staging_admin](../staging_admin/README.md)、
  [unitstate_admin](../unitstate_admin/README.md)：客户端失败的结构化表达。
- [controller](../controller/README.md)：任务配置解码失败时使用对应错误码。

## 本目录文档

- [`ErrorCode`](./ErrorCode.md)：错误码清单与含义。
- [`SysError`](./SysError.md)：错误码 + 命名参数的载体。
