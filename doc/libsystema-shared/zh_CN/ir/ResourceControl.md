# ResourceControl

所属模块：`libsystema_shared::ir`

## 简介

单元的 cgroup 资源控制限制，镜像 `systemd.resource-control(5)`。字符串值保留
单元文件中的原样（如 `"50%"`、`"1G"`、`"100ms"`），以便下游（System R、D-Bus）
按各自需要的精度解析。

## 定义

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceControl {
    pub cpu_quota: String,
    pub cpu_quota_period: String,
    pub cpu_weight: u32,
    pub startup_cpu_weight: u32,
    pub cpu_set_cpus: String,
    pub cpu_set_memory_nodes: String,
    pub memory_min: String,
    pub memory_low: String,
    pub memory_high: String,
    pub memory_max: String,
    pub memory_swap_max: String,
    pub io_weight: u32,
    pub startup_io_weight: u32,
    pub io_device_weight: Vec<String>,
    pub io_read_bandwidth_max: Vec<String>,
    pub io_write_bandwidth_max: Vec<String>,
    pub tasks_max: u32,
    pub allowed_cpus: String,
    pub allowed_memory_nodes: String,
    pub delegate: bool,
    pub delegate_controllers: Vec<String>,
    pub delegate_subgroup: String,
}
```

## 字段

| 字段 | 指令 | 说明 |
| --- | --- | --- |
| `cpu_quota` | `CPUQuota=` | CPU 配额，如单核的 `"50%"` 或绝对时间片 `"100ms"`。 |
| `cpu_quota_period` | `CPUQuotaPeriod=` | 配额生效周期。 |
| `cpu_weight` | `CPUWeight=` | 相对 CPU 权重（默认 100）。 |
| `startup_cpu_weight` | `StartupCPUWeight=` | 启动阶段使用的 CPU 权重。 |
| `cpu_set_cpus` | `CPUSetCPUs=` | 可绑定的 CPU 列表。 |
| `cpu_set_memory_nodes` | `CPUSetMemoryNodes=` | 可绑定的内存节点。 |
| `memory_min` / `memory_low` | `MemoryMin=` / `MemoryLow=` | 硬/软内存下限。 |
| `memory_high` / `memory_max` | `MemoryHigh=` / `MemoryMax=` | 软/硬内存上限。 |
| `memory_swap_max` | `MemorySwapMax=` | swap 使用上限。 |
| `io_weight` / `startup_io_weight` | `IOWeight=` / `StartupIOWeight=` | IO 权重。 |
| `io_device_weight` | `IODeviceWeight=` | 按设备 IO 权重，如 `"/dev/sda 100"`。 |
| `io_read_bandwidth_max` / `io_write_bandwidth_max` | `IOReadBandwidthMax=` / `IOWriteBandwidthMax=` | 按设备读写带宽上限。 |
| `tasks_max` | `TasksMax=` | cgroup 中任务/线程数上限。 |
| `allowed_cpus` / `allowed_memory_nodes` | `AllowedCPUs=` / `AllowedMemoryNodes=` | 允许的 CPU / 内存节点。 |
| `delegate` | `Delegate=` | 单元是否接管自己的 cgroup 子树（chown 给 `User=`/`Group=`）。 |
| `delegate_controllers` | `DelegateControllers=` | 委托时保留启用的控制器；空表示当前全部启用的控制器。 |
| `delegate_subgroup` | `DelegateSubgroup=` | 委托时在单元 cgroup 下创建的子目录，如 `init.scope`。 |

## 使用示例

```rust
use libsystema_shared::ir::ResourceControl;

fn web_limits() -> ResourceControl {
    ResourceControl {
        cpu_quota: "50%".to_string(),
        cpu_weight: 200,
        memory_max: "1G".to_string(),
        memory_high: "512M".to_string(),
        tasks_max: 512,
        io_device_weight: vec!["/dev/sda 100".to_string()],
        ..Default::default()
    }
}

fn delegated_slice() -> ResourceControl {
    ResourceControl {
        delegate: true,
        delegate_subgroup: "init.scope".to_string(),
        ..Default::default()
    }
}

fn main() {
    let limits = web_limits();
    assert_eq!(limits.memory_max, "1G");
    assert!(delegated_slice().delegate);
}
```

## 相关 API

- [`UnitIR`](./UnitIR.md)：`resource_control` 字段。
- [`WorkerIpc`](../worker_ipc/WorkerIpc.md)：`supports_resource_prepare()` 声明委托支持。
