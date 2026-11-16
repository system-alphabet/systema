# SocketConfig

所属模块：`libsystema_shared::ir`

## 简介

套接字单元（`.socket`）的配置，对应 `[Socket]` 段落。socket 单元可以激活一个
服务单元（`Accept=no` 时为被动套接字；`Accept=yes` 时为每条连接一个服务实例）。

## 定义

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SocketConfig {
    pub listen_stream: Vec<String>,
    pub listen_datagram: Vec<String>,
    pub listen_sequential_packet: Vec<String>,
    pub listen_fifo: Vec<String>,
    pub listen_netlink: Vec<String>,
    pub accept: bool,
    pub socket_mode: String,
    pub socket_user: String,
    pub socket_group: String,
    pub backlog: u32,
    pub service: String,
    pub directory_mode: String,
}
```

## 字段

| 字段 | 指令 | 说明 |
| --- | --- | --- |
| `listen_stream` | `ListenStream=` | 流式 socket 地址，如 `0.0.0.0:8080`、`/run/foo.sock`。 |
| `listen_datagram` | `ListenDatagram=` | 数据报 socket 地址。 |
| `listen_sequential_packet` | `ListenSequentialPacket=` | 顺序包 socket 地址。 |
| `listen_fifo` | `ListenFIFO=` | FIFO 路径。 |
| `listen_netlink` | `ListenNetlink=` | netlink 地址，如 `kobject-uevent 1`。 |
| `accept` | `Accept=` | `true` 为每连接一个服务实例；`false` 为被动套接字。 |
| `socket_mode` | `SocketMode=` | socket 文件权限（数字形式，如 `"0666"`）。 |
| `socket_user` / `socket_group` | `SocketUser=` / `SocketGroup=` | Unix socket 的属主。 |
| `backlog` | `Backlog=` | `listen()` backlog；`0` 表示内核默认。 |
| `service` | `Service=` | 关联的服务单元名。 |
| `directory_mode` | `DirectoryMode=` | 自动创建的 socket 目录权限。 |

## 使用示例

```rust
use libsystema_shared::ir::SocketConfig;

fn api_socket() -> SocketConfig {
    SocketConfig {
        listen_stream: vec!["/run/example/api.sock".to_string()],
        accept: false,
        socket_mode: "0660".to_string(),
        socket_user: "root".to_string(),
        socket_group: "example".to_string(),
        backlog: 128,
        service: "example-api.service".to_string(),
        directory_mode: "0755".to_string(),
        ..Default::default()
    }
}

fn main() {
    let socket = api_socket();
    assert_eq!(socket.listen_stream[0], "/run/example/api.sock");
    assert!(!socket.accept);
    assert_eq!(socket.backlog, 128);
}
```

## 相关 API

- [`UnitIR`](./UnitIR.md)：`socket` 字段。
- [`ServiceConfig`](./ServiceConfig.md)：被套接字激活的服务配置。
