# ExecCommand

所属模块：`libsystema_shared::ir`

## 简介

一条可执行指令（如 `ExecStart=`）及其修饰符。`ServiceConfig` 中的各类
`exec_*` 字段都是 `Vec<ExecCommand>`。

## 定义

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecCommand {
    pub raw: String,
    pub program: String,
    pub args: Vec<String>,
    pub ignore_failure: bool,
    pub privileged: bool,
}
```

## 字段

| 字段 | 说明 |
| --- | --- |
| `raw` | 完整的原始命令字符串（包含任何格式特定的前缀）。 |
| `program` | 解析后的可执行文件路径。 |
| `args` | 命令行参数。 |
| `ignore_failure` | 是否忽略非零退出码（对应 `-` 前缀）。 |
| `privileged` | 是否以提升后的权限运行（对应 `+` 前缀）。 |

## 使用示例

```rust
use libsystema_shared::ir::ExecCommand;

fn start_command() -> ExecCommand {
    ExecCommand {
        raw: "/usr/bin/nginx -g daemon off;".to_string(),
        program: "/usr/bin/nginx".to_string(),
        args: vec![
            "-g".to_string(),
            "daemon off;".to_string(),
        ],
        ignore_failure: false,
        privileged: true,
    }
}

fn main() {
    let cmd = start_command();
    assert_eq!(cmd.program, "/usr/bin/nginx");
    assert_eq!(cmd.args.len(), 2);
    assert!(cmd.privileged);
}
```

## 相关 API

- [`ServiceConfig`](./ServiceConfig.md)：`exec_start` 等字段。
- [`UnitIR`](./UnitIR.md)：服务配置的载体。
