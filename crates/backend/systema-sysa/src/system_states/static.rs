//! System-state queries whose results do not normally change during one boot.

use tracing::debug;

pub(super) fn evaluate(kind: &str, value: &str) -> Option<bool> {
    match kind {
        "KernelCommandLine" => Some(kernel_command_line_matches(value)),
        "Security" => security_enabled(value),
        _ => None,
    }
}

/// `ConditionSecurity=`: whether the named security technology is enabled.
///
/// Only technologies observable on this system are implemented; unrecognised
/// values return `None` (treated as met).
fn security_enabled(value: &str) -> Option<bool> {
    let enabled = match value {
        "selinux" => std::path::Path::new("/sys/fs/selinux").exists(),
        "apparmor" => std::fs::read_to_string("/sys/module/apparmor/parameters/enabled")
            .map(|contents| contents.trim().eq_ignore_ascii_case("Y"))
            .unwrap_or(false),
        "tomoyo" => std::path::Path::new("/sys/kernel/security/tomoyo").exists(),
        "smack" => std::path::Path::new("/sys/fs/smackfs").exists(),
        "ima" => std::path::Path::new("/sys/kernel/security/ima").exists(),
        "uefi-secureboot" => secure_boot_enabled(),
        "tpm2" => tpm2_supported(),
        // A measured UKI implies the firmware measured the boot chain into a
        // TPM, so both are required.
        "measured-uki" => std::path::Path::new("/sys/firmware/efi").exists() && tpm2_supported(),
        other => {
            debug!("ConditionSecurity={other} is not evaluated; treated as met");
            return None;
        }
    };
    Some(enabled)
}

/// UEFI Secure Boot: the `SecureBoot` EFI variable holds `1` in byte 4.
fn secure_boot_enabled() -> bool {
    const SECURE_BOOT: &str =
        "/sys/firmware/efi/efivars/SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c";
    std::fs::read(SECURE_BOOT)
        .ok()
        .and_then(|bytes| bytes.get(4).copied())
        .map(|value| value == 1)
        .unwrap_or(false)
}

/// Whether a TPM2 device is visible to the kernel (ACPI table, event log, or
/// the tpm class device).
fn tpm2_supported() -> bool {
    std::path::Path::new("/sys/firmware/acpi/tables/TPM2").exists()
        || std::path::Path::new("/sys/kernel/security/tpm0/binary_bios_measurements").exists()
        || std::path::Path::new("/sys/class/tpm/tpm0").exists()
}

/// `ConditionKernelCommandLine=`: a word, or `word=value`, on `/proc/cmdline`.
fn kernel_command_line_matches(value: &str) -> bool {
    let Ok(cmdline) = std::fs::read_to_string("/proc/cmdline") else {
        return false;
    };
    if let Some((key, expected)) = value.split_once('=') {
        cmdline
            .split_whitespace()
            .any(|token| token == format!("{key}={expected}"))
    } else {
        cmdline
            .split_whitespace()
            .any(|token| token == value || token == format!("{value}="))
    }
}
