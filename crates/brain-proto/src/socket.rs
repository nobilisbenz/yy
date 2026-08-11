//! Socket location.
//!
//! `$XDG_RUNTIME_DIR/brain/brain.sock` (spec §26). `/tmp` is explicitly not a
//! fallback: it is world-writable and survives logout, so a stale or hostile
//! socket there could intercept queries. If `XDG_RUNTIME_DIR` is missing we
//! fail loudly with something the user can act on.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum SocketPathError {
    #[error(
        "XDG_RUNTIME_DIR is not set, so there is no private directory for the \
         control socket.\nThis is normally set by the login session. Start Brain Dock \
         from a graphical session, or export XDG_RUNTIME_DIR=/run/user/$(id -u) yourself."
    )]
    NoRuntimeDir,
}

/// Directory holding the socket. Callers that bind must create it (0700).
pub fn runtime_dir() -> Result<PathBuf, SocketPathError> {
    let base = std::env::var_os("XDG_RUNTIME_DIR").ok_or(SocketPathError::NoRuntimeDir)?;
    Ok(PathBuf::from(base).join("brain"))
}

/// Full path to the control socket.
pub fn socket_path() -> Result<PathBuf, SocketPathError> {
    Ok(runtime_dir()?.join("brain.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_lives_under_the_runtime_dir() {
        // SAFETY: single-threaded test, no other thread reads the environment.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000") };
        assert_eq!(
            socket_path().unwrap(),
            PathBuf::from("/run/user/1000/brain/brain.sock")
        );
    }
}
