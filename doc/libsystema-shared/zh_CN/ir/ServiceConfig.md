# ServiceConfig

所属模块：`libsystema_shared::ir`

## 简介

服务单元（`.service`）的统一配置形式，对应 systemd 的 `[Service]` 等段落。

`service_type` 必须原样保留定义中的 `Type=`，消费方不得根据 `ExecStart=`
重新推导：`notify` / `dbus` 类型决定启动 job 何时完成（`notify` 还会得到
`$NOTIFY_SOCKET`）。

## 定义

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub service_type: String,
    pub bus_name: String,
    pub exec_start: Vec<ExecCommand>,
    pub exec_stop: Vec<ExecCommand>,
    pub exec_reload: Vec<ExecCommand>,
    pub exec_start_pre: Vec<ExecCommand>,
    pub exec_start_post: Vec<ExecCommand>,
    pub exec_stop_post: Vec<ExecCommand>,
    pub working_directory: String,
    pub user: String,
    pub group: String,
    pub pam_name: String,
    pub environment: Vec<String>,
    pub environment_file: Vec<String>,
    pub state_directory: Vec<String>,
    pub state_directory_mode: u32,
    pub logs_directory: Vec<String>,
    pub logs_directory_mode: u32,
    pub runtime_directory: Vec<String>,
    pub runtime_directory_mode: u32,
    pub configuration_directory: Vec<String>,
    pub configuration_directory_mode: u32,
    pub restart_policy: RestartPolicy,
    pub restart_sec: u32,
    pub timeout_start_sec: u32,
    pub timeout_stop_sec: u32,
    pub remain_after_exit: bool,
    pub watchdog_sec: u32,
    pub kill_signal: String,
    pub kill_mode: String,
    pub standard_input: String,
    pub standard_output: String,
    pub standard_error: String,
    pub tty_path: String,
}
```

## 字段

| 字段 | 指令 | 说明 |
| --- | --- | --- |
| `service_type` | `Type=` | `simple`、`notify`、`notify-reload`、`oneshot`、`dbus`、`forking`、`idle`。 |
| `bus_name` | `BusName=` | `Type=dbus` 要求的总线名，用于判断服务就绪。 |
| `exec_start` / `exec_stop` / `exec_reload` | `ExecStart=` 等 | 主命令。 |
| `exec_start_pre` / `exec_start_post` / `exec_stop_post` | 对应指令 | 启动前、启动后、停止后的命令。 |
| `working_directory` | `WorkingDirectory=` | 工作目录。 |
| `user` / `group` | `User=` / `Group=` | 运行身份。 |
| `pam_name` | `PAMName=` | exec 前打开的 PAM 服务名。 |
| `environment` / `environment_file` | `Environment=` / `EnvironmentFile=` | 环境变量。 |
| `state_directory` / `_mode` | `StateDirectory=` | `/var/lib` 下的目录片段，worker 创建并导出 `$STATE_DIRECTORY`；模式 `0` 表示未设置（默认 `0755`）。 |
| `logs_directory` / `_mode` | `LogsDirectory=` | `/var/log` 下，导出 `$LOGS_DIRECTORY`。 |
| `runtime_directory` / `_mode` | `RuntimeDirectory=` | `/run` 下，导出 `$RUNTIME_DIRECTORY`。 |
| `configuration_directory` / `_mode` | `ConfigurationDirectory=` | `/etc` 下，导出 `$CONFIGURATION_DIRECTORY`。 |
| `restart_policy` / `restart_sec` | `Restart=` / `RestartSec=` | 重启策略与间隔。 |
| `timeout_start_sec` / `timeout_stop_sec` | `TimeoutStartSec=` / `TimeoutStopSec=` | 启停超时。 |
| `remain_after_exit` | `RemainAfterExit=` | 主进程成功退出后服务是否保持 active。 |
| `watchdog_sec` | `WatchdogSec=` | 看门狗超时。 |
| `kill_signal` / `kill_mode` | `KillSignal=` / `KillMode=` | 停止时使用的信号与模式。 |
| `standard_input` / `standard_output` / `standard_error` | 对应指令 | 标准 IO 配置。 |
| `tty_path` | `TTYPath=` | TTY 路径。 |

## 使用示例

```rust
use libsystema_shared::ir::{ExecCommand, RestartPolicy, ServiceConfig};

fn oneshot_service() -> ServiceConfig {
    ServiceConfig {
        service_type: "oneshot".to_string(),
        exec_start: vec![ExecCommand {
            raw: "/usr/bin/true".to_string(),
            program: "/usr/bin/true".to_string(),
            args: vec![],
            ignore_failure: false,
            privileged: false,
        }],
        remain_after_exit: true,
        timeout_start_sec: 90,
        restart_policy: RestartPolicy::No,
        standard_output: "journal".to_string(),
        ..Default::default()
    }
}

fn main() {
    let config = oneshot_service();
    assert_eq!(config.service_type, "oneshot");
    assert!(config.remain_after_exit);
}
```

## 相关 API

- [`ExecCommand`](./ExecCommand.md)：`exec_*` 字段的元素类型。
- [`RestartPolicy`](./RestartPolicy.md)：`restart_policy` 字段。
- [`UnitIR`](./UnitIR.md)：服务配置的载体。
