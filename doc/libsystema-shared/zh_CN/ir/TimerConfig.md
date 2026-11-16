# TimerConfig

所属模块：`libsystema_shared::ir`

## 简介

定时器单元（`.timer`）的配置，对应 `[Timer]` 段落。多个触发条件可以同时存在。

## 定义

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TimerConfig {
    pub on_active_sec: Option<u32>,
    pub on_boot_sec: Option<u32>,
    pub on_startup_sec: Option<u32>,
    pub on_unit_active_sec: Option<u32>,
    pub on_unit_inactive_sec: Option<u32>,
    pub on_calendar: Vec<String>,
    pub accuracy_sec: u32,
    pub randomized_delay_sec: u32,
    pub unit: String,
    pub persistent: bool,
}
```

## 字段

| 字段 | 指令 | 说明 |
| --- | --- | --- |
| `on_active_sec` | `OnActiveSec=` | 定时器激活后多久触发。 |
| `on_boot_sec` | `OnBootSec=` | 系统启动后多久触发。 |
| `on_startup_sec` | `OnStartupSec=` | 服务管理器启动后多久触发。 |
| `on_unit_active_sec` | `OnUnitActiveSec=` | 被触发单元上次激活后多久触发。 |
| `on_unit_inactive_sec` | `OnUnitInactiveSec=` | 被触发单元上次失活后多久触发。 |
| `on_calendar` | `OnCalendar=` | 日历表达式列表（systemd.time(7) 语法）。 |
| `accuracy_sec` | `AccuracySec=` | 触发精度窗口。 |
| `randomized_delay_sec` | `RandomizedDelaySec=` | 随机延迟上限。 |
| `unit` | `Unit=` | 要触发的单元；缺省时与定时器同名。 |
| `persistent` | `Persistent=` | 是否在停机期间错过触发时于启动后补触发。 |

## 使用示例

```rust
use libsystema_shared::ir::TimerConfig;

fn daily_backup_timer() -> TimerConfig {
    TimerConfig {
        on_calendar: vec!["*-*-* 03:30:00".to_string()],
        persistent: true,
        accuracy_sec: 60,
        unit: "backup.service".to_string(),
        ..Default::default()
    }
}

fn boot_timer() -> TimerConfig {
    TimerConfig {
        on_boot_sec: Some(15 * 60),
        randomized_delay_sec: 30,
        ..Default::default()
    }
}

fn main() {
    let timer = daily_backup_timer();
    assert_eq!(timer.on_calendar.len(), 1);
    assert!(timer.persistent);
    assert_eq!(boot_timer().on_boot_sec, Some(900));
}
```

## 相关 API

- [`UnitIR`](./UnitIR.md)：`timer` 字段。
- [`ServiceConfig`](./ServiceConfig.md)：通常被定时器触发的服务配置。
