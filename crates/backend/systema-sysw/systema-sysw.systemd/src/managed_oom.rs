//! PID 1 compatibility endpoint for `systemd-oomd`.
//!
//! Native systemd binds `/run/systemd/io.systemd.ManagedOOM` directly from
//! PID 1.  It is not owned by a `.socket` unit: that socket carries
//! `io.systemd.ManagedOOM.SubscribeManagedOOMCGroups`, where systemd-oomd is
//! the subscriber and the service manager is the publisher.
//!
//! System A does not model ManagedOOM cgroup properties yet, so this
//! implementation starts oomd with a valid empty cgroup set.  The socket and
//! subscription stay alive, but no cgroup is selected for managed-OOM action.

use std::io::ErrorKind;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tracing::debug;

const SOCKET_NAME: &str = "io.systemd.ManagedOOM";
const MAX_MESSAGE_LEN: usize = 1024 * 1024;
const METHOD_SUBSCRIBE: &str = "io.systemd.ManagedOOM.SubscribeManagedOOMCGroups";

pub async fn serve(run_state_dir: &str) -> Result<()> {
    let socket_path = PathBuf::from(run_state_dir)
        .join("systemd")
        .join(SOCKET_NAME);
    prepare_socket_path(&socket_path)?;

    let listener = UnixListener::bind(&socket_path).with_context(|| {
        sysa::l10n::fmt(
            sysa::l10n::t_("Binding ManagedOOM varlink socket '{path}' failed: {error}."),
            &[
                ("path", &socket_path.display().to_string()),
                ("error", &std::io::Error::last_os_error().to_string()),
            ],
        )
    })?;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o666))?;
    debug!(
        "Listening on ManagedOOM varlink socket {}",
        socket_path.display()
    );

    let subscribed = Arc::new(AtomicBool::new(false));
    loop {
        let (stream, _) = listener.accept().await?;
        let subscribed = subscribed.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_connection(stream, subscribed).await {
                debug!("ManagedOOM varlink connection ended: {error}");
            }
        });
    }
}

fn prepare_socket_path(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context(sysa::l10n::t_("ManagedOOM socket has no parent directory."))?;
    std::fs::create_dir_all(parent)?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755))?;

    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                anyhow::bail!(
                    "{}",
                    sysa::l10n::fmt(
                        sysa::l10n::t_("ManagedOOM socket '{path}' is already in use."),
                        &[("path", &path.display().to_string())]
                    )
                );
            }
            std::fs::remove_file(path)
                .with_context(|| format!("removing stale ManagedOOM socket {}", path.display()))?;
        }
        Ok(_) => {
            anyhow::bail!(
                "{}",
                sysa::l10n::fmt(
                    sysa::l10n::t_(
                        "ManagedOOM socket path '{path}' exists and is not a Unix socket."
                    ),
                    &[("path", &path.display().to_string())]
                )
            );
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

async fn handle_connection(mut stream: UnixStream, subscribed: Arc<AtomicBool>) -> Result<()> {
    let mut buffer = Vec::new();
    let mut input = [0u8; 4096];
    let mut subscription = None;

    loop {
        let count = stream.read(&mut input).await?;
        if count == 0 {
            break;
        }
        buffer.extend_from_slice(&input[..count]);
        if buffer.len() > MAX_MESSAGE_LEN {
            anyhow::bail!(sysa::l10n::t_("ManagedOOM varlink message is too large."));
        }

        while let Some(end) = buffer.iter().position(|byte| *byte == 0) {
            let frame: Vec<u8> = buffer.drain(..=end).collect();
            let payload = &frame[..frame.len().saturating_sub(1)];
            let request: Value = serde_json::from_slice(payload)?;
            let method = request
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let streaming = request
                .get("more")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            let reply = match method {
                METHOD_SUBSCRIBE => {
                    if streaming {
                        if subscribed
                            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                            .is_err()
                        {
                            json!({
                                "error": "io.systemd.ManagedOOM.SubscriptionTaken",
                                "parameters": {}
                            })
                        } else {
                            subscription
                                .get_or_insert_with(|| SubscriptionGuard::new(subscribed.clone()));
                            json!({
                                "parameters": {"cgroups": []},
                                "continues": true
                            })
                        }
                    } else {
                        json!({
                            "parameters": {"cgroups": []},
                            "continues": false
                        })
                    }
                }
                "io.systemd.service.Ping" => json!({"parameters": {}}),
                "org.varlink.service.GetInfo" => json!({
                    "parameters": {
                        "vendor": "SystemA",
                        "product": "System Wrapper",
                        "version": env!("CARGO_PKG_VERSION"),
                        "url": "https://github.com/system-alphabet/systema",
                        "interfaces": ["io.systemd.ManagedOOM"]
                    }
                }),
                _ => json!({
                    "error": "org.varlink.service.MethodNotFound",
                    "parameters": {"method": method}
                }),
            };

            write_frame(&mut stream, &reply).await?;
        }
    }

    Ok(())
}

struct SubscriptionGuard {
    subscribed: Arc<AtomicBool>,
}

impl SubscriptionGuard {
    fn new(subscribed: Arc<AtomicBool>) -> Self {
        Self { subscribed }
    }
}

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        self.subscribed.store(false, Ordering::Release);
    }
}

async fn write_frame(stream: &mut UnixStream, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(0);
    stream.write_all(&bytes).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn subscription_streams_an_empty_cgroup_set() {
        let dir = std::env::temp_dir().join(format!(
            "systema-managed-oom-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("io.systemd.ManagedOOM");
        prepare_socket_path(&socket).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let subscribed = Arc::new(AtomicBool::new(false));

        let server = tokio::spawn({
            let subscribed = subscribed.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let _ = handle_connection(stream, subscribed).await;
            }
        });

        let mut client = UnixStream::connect(socket).await.unwrap();
        client
            .write_all(br#"{"method":"io.systemd.ManagedOOM.SubscribeManagedOOMCGroups","parameters":{},"more":true}"#)
            .await
            .unwrap();
        client.write_all(&[0]).await.unwrap();
        let mut reply = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            client.read_exact(&mut byte).await.unwrap();
            if byte[0] == 0 {
                break;
            }
            reply.push(byte[0]);
        }
        let reply: Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(reply["parameters"]["cgroups"], json!([]));
        assert_eq!(reply["continues"], true);
        assert!(subscribed.load(Ordering::Acquire));

        drop(client);
        tokio::time::timeout(std::time::Duration::from_secs(1), server)
            .await
            .expect("subscription task must observe EOF")
            .unwrap();
        assert!(!subscribed.load(Ordering::Acquire));
        let _ = std::fs::remove_dir_all(dir);
    }
}
