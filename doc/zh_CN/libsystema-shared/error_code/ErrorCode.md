# ErrorCode

所属模块：`libsystema_shared::error_code`

## 简介

IPC 内核中一个失败场景的稳定标识符。该枚举是公开 API：调用方可以匹配某个
错误码来定制处理，因此各变体的含义在版本之间保持不变。错误码本身**不携带任何
文案**——所有面向用户的措辞都在 `sysa::errors` 中按错误码映射成本地化消息。

## 定义

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    // 连接与分帧
    ConnectFailed,
    ConnectPermissionDenied,
    EnvelopeEncodeFailed,
    EnvelopeSendFailed,
    FrameReceiveFailed,
    EnvelopeDecodeFailed,
    PayloadEncodeFailed,
    // 协议版本协商
    VersionPeerClosed,
    VersionUnexpectedReply,
    VersionResultUnreadable,
    IncompatibleVersion,
    VersionMismatch,
    // SCM_RIGHTS fd 传递
    FdSendTaskPanicked,
    FdRecvTaskPanicked,
    CmsgHeaderMissing,
    SendmsgFailed,
    RecvmsgFailed,
    FdpassClosed,
    NoScmRightsFd,
    // Finder：注册、提交与 staging 查询
    AckConnectionClosed,
    ExpectedFinderAck,
    AckDecodeFailed,
    QueryConnectionClosed,
    ExpectedStagingQueryResult,
    StagingQueryDecodeFailed,
    // admin.staging
    AdminConnectionClosed,
    ExpectedAdminStagingResult,
    AdminStagingDecodeFailed,
    // admin.unitstate
    UnitStateConnectionClosed,
    UnitStateEntryDecodeFailed,
    UnitStateEofDecodeFailed,
    ExpectedUnitStateFrame,
    // controller
    UnitConfigDecodeFailed,
    // worker_ipc
    WorkerConnectFailed,
    ConnectionClosed,
    CgroupQueryTimeout,
    ClosedBeforeAck,
    RegistrationRejected,
    SocketWriteFailed,
    InboundAckDecodeFailed,
    CgroupReplyDecodeFailed,
    RequestIdWraparound,
    UnknownMethod,
}
```

## 常量与方法

| 项 | 签名 | 说明 |
| --- | --- | --- |
| `ALL` | `pub const ALL: &'static [ErrorCode]` | 枚举全部变体，便于测试与遍历，无需通配匹配。 |
| `params` | `pub const fn params(self) -> &'static [&'static str]` | 该错误码携带的占位符名字，按消息中的顺序排列；无占位符时返回空切片。 |

常见 `params` 映射：`ConnectFailed`/`ConnectPermissionDenied` → `["e"]`，
`VersionUnexpectedReply` → `["method"]`，
`IncompatibleVersion` → `["mine", "theirs", "message"]`，
`VersionMismatch` → `["have", "want"]`，
`WorkerConnectFailed` → `["path"]`，
`CgroupQueryTimeout` → `["timeout"]`，
`RegistrationRejected` → `["message"]`，
`UnknownMethod` → `["other"]`。

## 使用示例

```rust
use libsystema_shared::error_code::{ErrorCode, SysError};

fn classify(err: &anyhow::Error) -> &'static str {
    match err.downcast_ref::<SysError>().map(|e| e.code()) {
        Some(ErrorCode::ConnectPermissionDenied) => "需要 sudo，或改用 --user",
        Some(ErrorCode::WorkerConnectFailed) => "System A 不可达",
        Some(ErrorCode::IncompatibleVersion) => "协议版本不兼容",
        _ => "其他错误",
    }
}

fn all_codes() -> usize {
    ErrorCode::ALL.len()
}
```

## 相关 API

- [`SysError`](./SysError.md)：错误码 + 占位符值的实际错误类型。
- [`exchange_version`](../ipc/exchange_version.md)：产生版本协商类错误码。
- [`WorkerIpc`](../worker_ipc/WorkerIpc.md)：产生 worker 会话类错误码。
