# recv_fd

所属模块：`libsystema_shared::ipc`

## 简介

通过 `SCM_RIGHTS` 从 Unix 流接收一个文件描述符。内部使用 `spawn_blocking`
运行同步 `recvmsg`；流可能是非阻塞的（tokio `UnixStream`），`EAGAIN` 会短暂休眠
后重试，直到数据到达或对端关闭。

**限制**：该流必须是专用的原始连接，不能是 [`EnvelopeFramed`](./EnvelopeFramed.md)。

## 定义

```rust
pub async fn recv_fd(stream: &UnixStream) -> Result<std::os::unix::io::RawFd>
```

## 错误码

| 情况 | `ErrorCode` |
| --- | --- |
| 阻塞任务 panic | `FdRecvTaskPanicked` |
| `recvmsg` 失败 | `RecvmsgFailed`（`e`） |
| 对端关闭（读到 0 字节） | `FdpassClosed` |
| 控制消息中没有 `SCM_RIGHTS` fd | `NoScmRightsFd` |

## 使用示例

```rust
use libsystema_shared::ipc::recv_fd;

async fn take_listener(stream: &tokio::net::UnixStream) -> anyhow::Result<i32> {
    let raw_fd = recv_fd(stream).await?;
    // 接收方拥有该 fd，使用完毕后需要 close。
    Ok(raw_fd)
}
```

## 相关 API

- [`send_fd`](./send_fd.md)：发送端。
- [`ErrorCode`](../error_code/ErrorCode.md)：fd 传递相关错误码。
