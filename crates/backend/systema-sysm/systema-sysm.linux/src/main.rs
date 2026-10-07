//! System M — System Mount Worker for Linux.
//!
//! This worker is Linux-only.  On other platforms the whole implementation
//! is compiled out (gated behind `cfg(any(target_os = "linux", target_os = "android"))`) and the binary
//! becomes an inert stub, so building the workspace never fails and never
//! compiles Linux-only code.  The dependencies in Cargo.toml are gated the
//! same way — apart from `sysa`, which the stub needs for `l10n::t_()` — so on
//! non-Linux platforms this crate is a stub with a single dependency.

#[cfg(any(target_os = "linux", target_os = "android"))]
mod linux;

#[cfg(any(target_os = "linux", target_os = "android"))]
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    linux::run().await
}

/// Inert stub on non-Linux platforms: this worker is Linux-only.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn main() {
    eprintln!(
        "{}",
        sysa::l10n::t_("systema-sysm.linux is a Linux-only worker; nothing to do.")
    );
    std::process::exit(0);
}
