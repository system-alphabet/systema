# ir::config 模块综述

## 在 systema 中的位置

[统一 IR](../README.md) 只对**公共字段**建模；`[Service]`、`[Socket]`、`[Timer]`
这些类型专属 section 用什么结构表达，是这套设计的核心取舍。

systema 的选择是**不建模**：把整个 section 原样摊平成一张“指令名 → 值”的字典
（`ConfigMap`）。本模块就是这张字典的编码规则与读取约定。

## 为什么扁平化

1. **协议宽度 = 单元文件宽度**：systemd 新增一个指令时，只要生产方把它放进字典、
   消费方按需读取即可，不需要改 protobuf、不需要升 IPC 版本。
2. **解释权归拥有者**：字典本身不解释 `Type=notify` 或 `ListenNetlink=` 的含义；
   那属于消费该单元类型的 worker（例如 System S 解析服务指令、System K 解析
   套接字指令）。这符合“一个组件只做一件事”的边界。
3. **生产方只搬运**：Finder 只负责把文件内容按原拼写放进字典，不做归一化，
   因此不会在多处重复实现指令语义。

## 编码约定

- 键是指令原名，保留大小写（`ExecStart`、`DirectoryMode`），**不写 section 名**；
- 所属 section 由单元的 `unit_type` 隐含（`Service` → `[Service]`，……）；
- 单值指令是 JSON 字符串，可重复指令是字符串数组；
- 值保留单元文件原样：`Exec*=` 的 `-`/`+`/`!` 前缀、`0755` 这类八进制写法都
  不会被归一化。

同一编码也用于 worker 收到的 `UnitConfig.config_json`：System A 把 IR 的类型专属
部分编码后随任务下发，worker 用相同规则解码。

## 读取约定，而非策略

本模块只提供“把文本变成值”的通用读取函数：按类型取值、缺省值、布尔写法、
八进制权限等。它不决定任何指令的默认语义——例如“未配置 `TimeoutStartSec=` 时
默认 90 秒”属于 System S 的策略，而不是字典层的规则。

区分“缺失”和“显式空值/零值”是这里的关键：`None` 经常表示“本次未报告”，
而显式 `0` 或空串可能是有意义配置，消费方必须能分辨。

## 与其它模块的关系

- [ir](../README.md)：字典挂在 IR 的 `config` 字段上。
- [controller](../../controller/README.md)：System A 把字典放进 `UnitConfig`
  派发给 worker。
- [finder](../../finder/README.md)：字典的主要生产方。

## 本目录文档

- [`parse_json`](./parse_json.md) / [`to_json_bytes`](./to_json_bytes.md)：
  字节与字典互转。
- [`get_string`](./get_string.md) / [`get_string_or`](./get_string_or.md) /
  [`get_strings`](./get_strings.md)：文本指令读取。
- [`get_bool`](./get_bool.md)：布尔指令。
- [`get_u32`](./get_u32.md) / [`get_u32_or`](./get_u32_or.md) /
  [`get_i32_or`](./get_i32_or.md) / [`get_mode`](./get_mode.md)：
  数值与权限指令。
