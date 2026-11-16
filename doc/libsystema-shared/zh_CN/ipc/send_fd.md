# send_fd

所属模块：`libsystema_shared::ipc`

## 简介

通过 `SCM_RIGHTS` 在 Unix 流上发送一个文件描述符。内部使用 `spawn_blocking`
运行同步 `sendmsg`，并在 `EAGAIN`（非阻塞 socket、发送缓冲满）时重试。

**限制**：该流必须是专用的原始连接，不能是 [`EnvelopeFramed`](./EnvelopeFramed.md)。
fd 传递与分帧协议不能混用。

## 定义

```rust
pub async fn send_fd(stream: &UnixStream, fd: std::os::unix::io::RawFd) -> Result<()>
```

## 错误码

| 情况 | `ErrorCode` |
| --- | --- |
| 阻塞任务 panic | `FdSendTaskPanicked` |
| `CMSG_FIRSTHDR` 返回空 | `CmsgHeaderMissing` |
| `sendmsg` 失败 | `SendmsgFailed`（`e`） |

实现细节：Linux 会完全忽略流式 socket 上的零长度 `sendmsg`（连同 `SCM_RIGHTS`
控制消息一起丢弃），因此始终附带 1 字节载荷发送描述符。

## 使用示例

```rust
use libsystema_shared::ipc::send_fd;

async fn pass_listener(stream: &tokio::net::UnixStream, listener_fd: i32) -> anyhow::Result<()> {
    send_fd(stream, listener_fd).await
}
```

发送方负责在传递后关闭自己的副本（如果不再需要）。

## 相关 API

- [`recv_fd`](./recv_fd.md)：接收端。
- [`EventPublisher`](../worker_ipc/EventPublisher.md)：fd 请求信封的发送入口。
