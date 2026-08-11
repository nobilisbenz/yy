//! `brain-daemon` — the long-running backend.
//!
//! Everything expensive lives here and stays hot: the database, the file
//! watchers, and (from Stage 2) the language model. Showing the dock must never
//! require loading a model (spec §3.1), which is the entire reason this process
//! is separate from the UI.

mod listener;
mod mock;
mod session;
mod state;

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;

use crate::state::Daemon;

#[derive(Parser, Debug)]
#[command(name = "brain-daemon", version, about = "Brain Dock backend daemon")]
struct Args {
    /// Answer queries from a canned script instead of retrieving and
    /// generating. Keeps UI timing work possible with no model loaded.
    #[arg(long)]
    mock: bool,

    /// Log filter, e.g. `debug` or `brain_daemon=trace,info`.
    #[arg(long, env = "BRAIN_LOG", default_value = "info")]
    log: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    init_tracing(&args.log);

    let path = brain_proto::socket_path().context("resolving the control socket path")?;
    let listener = listener::bind(&path).await?;

    let daemon = Arc::new(Daemon::new());
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        mock = args.mock,
        "brain-daemon ready"
    );

    let result = serve(listener, Arc::clone(&daemon), args.mock).await;

    // A socket file outliving its daemon is exactly the stale-socket case the
    // listener has to reason about on next start. Clean up after ourselves so
    // it stays a rare path rather than the normal one.
    if let Err(err) = std::fs::remove_file(&path) {
        tracing::warn!(%err, path = %path.display(), "could not remove the control socket");
    }
    result
}

async fn serve(
    listener: tokio::net::UnixListener,
    daemon: Arc<Daemon>,
    mock: bool,
) -> Result<()> {
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("accepting a control connection")?;
                let daemon = Arc::clone(&daemon);
                tokio::spawn(async move {
                    if let Err(err) = session::run(stream, daemon, mock).await {
                        tracing::debug!(%err, "control connection ended");
                    }
                });
            }
            _ = shutdown_signal() => {
                tracing::info!("shutting down");
                return Ok(());
            }
        }
    }
}

/// Ctrl-C or a systemd `TERM`. Both need to reach the same cleanup path, or a
/// `systemctl restart` leaves a stale socket behind every time.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut term = match signal(SignalKind::terminate()) {
        Ok(sig) => sig,
        Err(err) => {
            tracing::error!(%err, "cannot listen for SIGTERM; Ctrl-C only");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

fn init_tracing(filter: &str) {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_timer(fmt::time::uptime())
        .init();
}
