//! Binding the control socket.
//!
//! The subtle part is stale sockets. A socket file left behind by a crashed
//! daemon must be removed before we can bind, but unlinking unconditionally
//! would let a second daemon silently steal a *live* daemon's socket — the
//! first keeps running, holding the model in VRAM, while every client talks to
//! the second. So: probe first, unlink only what is provably dead.

use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result};
use tokio::net::{UnixListener, UnixStream};

/// Bind the control socket, clearing a dead predecessor if there is one.
///
/// Fails if another daemon is already listening — that is a user error worth
/// reporting, not a condition to paper over.
pub async fn bind(path: &Path) -> Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating runtime directory {}", parent.display()))?;
        restrict_to_owner(parent)?;
    }

    match path.try_exists() {
        Ok(true) => {
            // Something is there. Is it answering?
            match UnixStream::connect(path).await {
                Ok(_) => anyhow::bail!(
                    "another brain-daemon is already listening on {}\n\
                     Stop it first, or run `brainctl status` to see what it is doing.",
                    path.display()
                ),
                Err(err) if is_dead_socket(&err) => {
                    tracing::warn!(
                        path = %path.display(),
                        "removing stale socket left by a previous daemon"
                    );
                    std::fs::remove_file(path)
                        .with_context(|| format!("removing stale socket {}", path.display()))?;
                }
                Err(err) => {
                    return Err(anyhow::Error::new(err).context(format!(
                        "probing existing socket {} — refusing to remove it, since it \
                         may belong to a running daemon",
                        path.display()
                    )));
                }
            }
        }
        Ok(false) => {}
        Err(err) => {
            return Err(anyhow::Error::new(err)
                .context(format!("checking for an existing socket at {}", path.display())));
        }
    }

    let listener = UnixListener::bind(path)
        .with_context(|| format!("binding control socket {}", path.display()))?;
    restrict_to_owner(path)?;

    tracing::info!(path = %path.display(), "listening");
    Ok(listener)
}

/// Nobody has any business connecting to this socket but its owner. The
/// runtime dir is already 0700 on a normal system, but say so explicitly
/// rather than inheriting whatever the umask happened to be.
fn restrict_to_owner(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = if path.is_dir() { 0o700 } else { 0o600 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("restricting permissions on {}", path.display()))
}

/// Distinguishes "the file is there but nothing is listening" from every other
/// connect failure. Only the former justifies deleting it.
fn is_dead_socket(err: &std::io::Error) -> bool {
    matches!(err.kind(), ErrorKind::ConnectionRefused | ErrorKind::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn binds_a_fresh_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("brain.sock");
        let listener = bind(&path).await.unwrap();
        assert!(path.exists());
        drop(listener);
    }

    #[tokio::test]
    async fn clears_a_stale_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("brain.sock");

        // A socket file with nobody behind it: exactly what a crash leaves.
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists());

        let listener = bind(&path).await.unwrap();
        drop(listener);
    }

    #[tokio::test]
    async fn refuses_to_displace_a_live_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("brain.sock");

        let _incumbent = bind(&path).await.unwrap();
        let err = bind(&path).await.unwrap_err();
        assert!(
            err.to_string().contains("already listening"),
            "expected a clear conflict message, got: {err}"
        );
    }
}
