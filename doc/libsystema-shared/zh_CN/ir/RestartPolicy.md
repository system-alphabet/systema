# RestartPolicy

所属模块：`libsystema_shared::ir`

## 简介

服务单元的重启策略，对应 `[Service] Restart=`。默认值为 `No`。

## 定义

```rust
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RestartPolicy {
    #[default]
    No,
    OnSuccess,
    OnFailure,
    OnAbnormal,
    OnWatchdog,
    OnAbort,
    Always,
}
```

## 变体

| 变体 | `as_str()` | 说明 |
| --- | --- | --- |
| `No` | `no` | 不自动重启（默认）。 |
| `OnSuccess` | `on-success` | 仅在正常退出时重启。 |
| `OnFailure` | `on-failure` | 在非零退出、信号终止、超时等失败情况重启。 |
| `OnAbnormal` | `on-abnormal` | 在信号终止、超时、看门狗触发时重启。 |
| `OnWatchdog` | `on-watchdog` | 仅在看门狗超时时重启。 |
| `OnAbort` | `on-abort` | 仅在收到未被捕获的致命信号时重启。 |
| `Always` | `always` | 无论退出状态如何都重启。 |

## 方法

| 签名 | 说明 |
| --- | --- |
| `pub fn as_str(&self) -> &str` | 返回 `Restart=` 的字符串形式。 |

## 使用示例

```rust
use libsystema_shared::ir::RestartPolicy;

fn policy_from(value: &str) -> RestartPolicy {
    match value {
        "always" => RestartPolicy::Always,
        "on-failure" => RestartPolicy::OnFailure,
        "on-success" => RestartPolicy::OnSuccess,
        "on-abnormal" => RestartPolicy::OnAbnormal,
        "on-watchdog" => RestartPolicy::OnWatchdog,
        "on-abort" => RestartPolicy::OnAbort,
        _ => RestartPolicy::No,
    }
}

fn main() {
    let policy = policy_from("on-failure");
    assert_eq!(policy.as_str(), "on-failure");
    assert_eq!(RestartPolicy::default(), RestartPolicy::No);
}
```

## 相关 API

- [`ServiceConfig`](./ServiceConfig.md)：`restart_policy` 字段。
