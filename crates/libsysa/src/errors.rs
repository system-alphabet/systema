//! The wording of every IPC-kernel failure, and its localization.
//!
//! `sysa-ipc` reports failures as [`SysError`] — a stable [`ErrorCode`]
//! plus the values of its `{name}` placeholders — and carries no message
//! text at all.  This module owns the message table: each code maps to an
//! English template (the gettext msgid) that is translated and filled here.
//!
//! Keeping the table on this side means the po msgids are ordinary
//! `t_()` call sites again, extractable by xgettext, while the kernel
//! stays free of gettext and of every user-facing string.

use anyhow::Error;
use sysa_ipc::error_code::{ErrorCode, SysError};

use crate::l10n;

/// The English template (gettext msgid) for one code.
///
/// Placeholders are written `{name}`; [`ErrorCode::params`] lists them.
/// Never reword a template without updating the po files — an edited
/// msgid silently loses its translation.
fn template(code: ErrorCode) -> String {
    match code {
        ErrorCode::ConnectFailed => l10n::t_("Failed to connect to System A: {e}"),
        ErrorCode::ConnectPermissionDenied => l10n::t_(
            "Failed to connect to System A: {e}. System-domain operations need root: re-run with sudo, or pass --user to manage the per-user instance.",
        ),
        ErrorCode::EnvelopeEncodeFailed => l10n::t_("Failed to encode Envelope."),
        ErrorCode::EnvelopeSendFailed => l10n::t_("Failed to send Envelope frame."),
        ErrorCode::FrameReceiveFailed => l10n::t_("Frame receive error."),
        ErrorCode::EnvelopeDecodeFailed => l10n::t_("Failed to decode Envelope."),
        ErrorCode::PayloadEncodeFailed => l10n::t_("Failed to encode payload."),
        ErrorCode::VersionPeerClosed => {
            l10n::t_("peer closed the connection during the IPC version exchange")
        }
        ErrorCode::VersionUnexpectedReply => {
            l10n::t_("unexpected reply '{method}' during the IPC version exchange")
        }
        ErrorCode::VersionResultUnreadable => l10n::t_("unreadable ipc.version.result payload"),
        ErrorCode::IncompatibleVersion => l10n::t_(
            "incompatible IPC version: we speak {mine}, the peer announced {theirs}: {message}",
        ),
        ErrorCode::VersionMismatch => {
            l10n::t_("this build pins IPC version {have}; the peer announced {want}")
        }
        ErrorCode::FdSendTaskPanicked => l10n::t_("SCM_RIGHTS send task panicked."),
        ErrorCode::FdRecvTaskPanicked => l10n::t_("SCM_RIGHTS recv task panicked."),
        ErrorCode::CmsgHeaderMissing => l10n::t_("CMSG_FIRSTHDR returned null."),
        ErrorCode::SendmsgFailed => l10n::t_("sendmsg (SCM_RIGHTS) failed: {e}."),
        ErrorCode::RecvmsgFailed => l10n::t_("recvmsg (SCM_RIGHTS) failed: {e}."),
        ErrorCode::FdpassClosed => {
            l10n::t_("fdpass connection closed while waiting for a listener fd.")
        }
        ErrorCode::NoScmRightsFd => l10n::t_("recvmsg did not contain SCM_RIGHTS fd."),
        ErrorCode::AckConnectionClosed => l10n::t_("System A disconnected before sending ack."),
        ErrorCode::ExpectedFinderAck => l10n::t_("Expected 'finder.ack', got '{method}'"),
        ErrorCode::AckDecodeFailed => l10n::t_("Failed to decode UnitRegistrationAck"),
        ErrorCode::QueryConnectionClosed => {
            l10n::t_("System A disconnected before sending query result.")
        }
        ErrorCode::ExpectedStagingQueryResult => {
            l10n::t_("Expected 'staging.query_result', got '{method}'")
        }
        ErrorCode::StagingQueryDecodeFailed => l10n::t_("Failed to decode StagingQueryResult"),
        ErrorCode::AdminConnectionClosed => l10n::t_("System A disconnected before admin result."),
        ErrorCode::ExpectedAdminStagingResult => {
            l10n::t_("Expected 'admin.staging.result', got '{method}'")
        }
        ErrorCode::AdminStagingDecodeFailed => l10n::t_("Failed to decode AdminStagingResult"),
        ErrorCode::UnitStateConnectionClosed => {
            l10n::t_("System A disconnected before unit-state EOF.")
        }
        ErrorCode::UnitStateEntryDecodeFailed => l10n::t_("Failed to decode UnitStateEntry"),
        ErrorCode::UnitStateEofDecodeFailed => l10n::t_("Failed to decode UnitStateEof"),
        ErrorCode::ExpectedUnitStateFrame => {
            l10n::t_("Expected 'admin.unitstate.entry'/'admin.unitstate.eof', got '{other}'")
        }
        ErrorCode::UnitConfigDecodeFailed => l10n::t_("failed to decode UnitConfig"),
        ErrorCode::WorkerConnectFailed => l10n::t_("Cannot connect to {path}."),
        ErrorCode::ConnectionClosed => l10n::t_("connection to System A is closed"),
        ErrorCode::CgroupQueryTimeout => l10n::t_("unit.cgroup timed out after {timeout}"),
        ErrorCode::ClosedBeforeAck => l10n::t_("System A closed connection before ack."),
        ErrorCode::RegistrationRejected => l10n::t_("Registration rejected: {message}."),
        ErrorCode::SocketWriteFailed => l10n::t_("Write to System A socket."),
        ErrorCode::InboundAckDecodeFailed => l10n::t_("failed to decode ack"),
        ErrorCode::CgroupReplyDecodeFailed => l10n::t_("failed to decode the cgroup reply"),
        ErrorCode::RequestIdWraparound => l10n::t_("request id wraparound"),
        ErrorCode::UnknownMethod => l10n::t_("unknown method: {other}"),
    }
}

/// Render one code as its localized message, with the placeholder values
/// substituted.
///
/// Placeholders are filled by name, mirroring [`l10n::fmt`], so a
/// translation may reorder them.
pub fn localize(error: &SysError) -> String {
    let args: Vec<(&str, &str)> = error
        .args()
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    l10n::fmt(template(error.code()), &args)
}

/// Render an error chain with every [`SysError`] link translated.
///
/// Links that are not `SysError` (an `io::Error`, a `.context()` string
/// written by an upper layer) keep their own text, so wrapping context is
/// preserved verbatim. Outermost link first, joined with `": "` —
/// the same order `anyhow`'s `Debug` output uses.
pub fn localize_chain(error: &Error) -> String {
    error
        .chain()
        .map(|link| match link.downcast_ref::<SysError>() {
            Some(sys) => localize(sys),
            None => link.to_string(),
        })
        .collect::<Vec<_>>()
        .join(": ")
}

/// Print an error chain to stderr in the user's locale.
///
/// User-facing CLIs call this from `main` instead of returning the error,
/// which would make Rust's `Termination` print the kernel's technical
/// `Debug`/`Display` form.
pub fn report(error: &Error) {
    eprintln!("{}", localize_chain(error));
}

/// The message for one code, for callers that keep only the code (no
/// placeholder values, e.g. a status field describing what went wrong).
pub fn message(code: ErrorCode) -> String {
    l10n::fmt(template(code), &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localize_substitutes_every_placeholder() {
        let err = SysError::new(ErrorCode::WorkerConnectFailed).with("path", "/run/a.sock");
        let text = localize(&err);
        assert!(text.contains("/run/a.sock"), "got {text:?}");
        assert!(!text.contains("{path}"), "got {text:?}");
    }

    #[test]
    fn localize_without_placeholders_keeps_the_template() {
        let err = SysError::new(ErrorCode::ConnectionClosed);
        assert_eq!(localize(&err), "connection to System A is closed");
    }

    #[test]
    fn chain_keeps_links_that_are_not_sys_errors() {
        let inner = anyhow::Error::new(SysError::new(ErrorCode::ConnectFailed).with("e", "EPERM"));
        let wrapped = inner.context("while registering units");
        let text = localize_chain(&wrapped);
        assert!(
            text.starts_with("while registering units: "),
            "got {text:?}"
        );
        assert!(text.contains("EPERM"), "got {text:?}");
    }

    #[test]
    fn message_renders_a_bare_code() {
        assert_eq!(
            message(ErrorCode::EnvelopeDecodeFailed),
            "Failed to decode Envelope."
        );
    }

    #[test]
    fn every_code_has_a_template() {
        // A code without a message would reach the user as its raw
        // variant name; this guard makes adding a variant a compile-and-test
        // failure until its wording is added here too.
        for code in ErrorCode::ALL {
            let text = template(*code);
            assert!(!text.is_empty(), "ErrorCode::{code:?} has no message");
        }
    }

    #[test]
    fn every_template_placeholder_is_a_declared_param() {
        for code in ErrorCode::ALL {
            let text = template(*code);
            let mut rest = text.as_str();
            while let Some(start) = rest.find('{') {
                let after = &rest[start + 1..];
                let Some(end) = after.find('}') else { break };
                let name = &after[..end];
                assert!(
                    code.params().contains(&name),
                    "ErrorCode::{code:?} message uses '{{{name}}}' but params() omits it"
                );
                rest = &after[end + 1..];
            }
        }
    }

    #[test]
    fn every_declared_param_is_a_template_placeholder() {
        for code in ErrorCode::ALL {
            let text = template(*code);
            for name in code.params() {
                assert!(
                    text.contains(&format!("{{{name}}}")),
                    "ErrorCode::{code:?} declares '{name}' but its message has no such placeholder"
                );
            }
        }
    }
}
