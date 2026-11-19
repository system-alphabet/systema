# ir 模块综述

## 在 systema 中的位置

systema 要同时面对 systemd、SysV、OpenRC、Runit 等不同的 init 世界。为了让
System A 与各 worker 不必理解每种格式，所有解析结果都被归一成同一个模型：
**统一单元中间表示（IR）**。

- **生产方**：Finder（systemd 单元文件、挂载表、设备扫描……）把源格式解析成 IR；
- **消费方**：System A 用它做依赖解析、模板实例化与调度；
- **下游**：System A 把 IR 中“本类型专属”的部分转成 `UnitConfig` 交给对应 worker。

IR 只描述单元，不描述进程，也不解释任何指令的语义。

## 模型由哪几块组成

1. **身份与类型**：单元名、单元类型、描述、来源格式与来源路径；别名记录单独
   表达（“这个名字是另一个单元的名字”），不产生新单元。
2. **依赖关系**：`After/Before/Requires/Wants/Conflicts/BindsTo/…` 以及成功/失败
   动作等，整体替换式更新。
3. **条件与断言**：启动前检查；条件不满足是“静默跳过”，断言不满足是“硬失败”。
4. **安装事实**：Finder 观察文件系统后给出的启用状态与完整 `UnitFileState`；
   System A 只存不算。
5. **模板元数据**：模板定义与实例的区分，以及实例替换进来的参数。
6. **类型专属配置**：一个**扁平指令字典**（见 [config/](config/README.md)），
   键就是单元文件里的指令名，所属 section 由单元类型隐含。

## 部分更新语义

IR 里只有 `id` 是必填的，其余字段都是“可选报告”。当单元已存在时，一次提交只覆盖
本次提供的字段：例如只报告 `config` 的提交不会清掉已有的描述或依赖。这让
“增量扫描”“单独刷新某个 section”“设备热插拔注入”都能复用同一条提交路径。

`config` 字段的三态需要区分：`None` 表示本次未报告（保留旧值），空对象表示
“已报告且 section 为空”，非空对象表示该 section 的完整内容。

## 边界

- IR **不解析、不校验**具体指令；`Type=notify`、`ListenNetlink=` 的含义由消费
  该单元类型的组件解释。
- IR **不持有运行时状态**；实际状态由 worker 推送、System A 缓存。
- IR 以 JSON 字节承载在 IPC 消息中，因此新增一个 systemd 指令不需要改 protobuf。

## 与其它模块的关系

- [finder](../finder/README.md)：IR 的生产与提交入口。
- [config/](config/README.md)：类型专属指令字典的编码与读取约定。
- [controller](../controller/README.md)：System A 把 IR 的类型专属部分转成
  `UnitConfig` 后派发给 worker。
- [ipc](../ipc/README.md)：IR 依赖的传输层。

## 本目录文档

- [`UnitIR`](./UnitIR.md)：统一单元模型。
- [`UnitType`](./UnitType.md)：单元类型，以及它隐含的 section。
- [`DependencySet`](./DependencySet.md)：依赖集合。
- [`TemplateInfo`](./TemplateInfo.md)：模板与实例元数据。
- [`Condition`](./Condition.md)：条件/断言。
- [`ConfigMap`](./ConfigMap.md)：扁平指令字典。
