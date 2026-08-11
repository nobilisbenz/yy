//! `brain-daemon` — the long-running backend.
//!
//! Everything expensive lives here and stays hot: the database, the file
//! watchers, and (from Stage 2) the language model. Showing the dock must never
//! require loading a model (spec §3.1), which is the entire reason this process
//! is separate from the UI.

mod backend;
mod listener;
mod mock;
mod session;
mod state;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use brain_core::Config;
use clap::Parser;

use crate::backend::Backend;
use crate::session::Services;
use crate::state::Daemon;

#[derive(Parser, Debug)]
#[command(name = "brain-daemon", version, about = "Brain Dock backend daemon")]
struct Args {
    /// Answer queries from a canned script instead of retrieving and
    /// generating. Keeps UI timing work possible with no model loaded.
    #[arg(long)]
    mock: bool,

    /// Config file. Defaults to `$XDG_CONFIG_HOME/brain/config.toml`.
    #[arg(long, env = "BRAIN_CONFIG")]
    config: Option<PathBuf>,

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
    let backend = if args.mock {
        None
    } else {
        Some(Arc::new(open_backend(args.config.as_deref())?))
    };

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        mock = args.mock,
        "brain-daemon ready"
    );

    // Index in the background. Blocking startup on a vault walk would mean the dock cannot
    // be summoned until it finishes, which is the one thing the split process exists to
    // prevent (spec §3.1).
    if let Some(backend) = backend.clone() {
        let daemon = Arc::clone(&daemon);
        tokio::spawn(async move {
            match backend.reindex().await {
                Ok(stats) => {
                    daemon.record_counts(stats);
                    tracing::info!(
                        documents = stats.documents,
                        sections = stats.sections,
                        "initial index ready"
                    );
                }
                Err(error) => tracing::error!(%error, "the initial index failed"),
            }
        });
    }

    let services = Arc::new(Services {
        daemon: Arc::clone(&daemon),
        backend,
    });
    let result = serve(listener, services).await;

    // A socket file outliving its daemon is exactly the stale-socket case the
    // listener has to reason about on next start. Clean up after ourselves so
    // it stays a rare path rather than the normal one.
    if let Err(err) = std::fs::remove_file(&path) {
        tracing::warn!(%err, path = %path.display(), "could not remove the control socket");
    }
    result
}

/// Load the config and open the vault behind it.
///
/// A config problem is fatal here rather than degraded-to-defaults: defaults index nothing,
/// so a daemon that "started fine" would answer every question with silence.
fn open_backend(config_path: Option<&std::path::Path>) -> Result<Backend> {
    let config = match config_path {
        Some(path) => Config::load_from(path),
        None => Config::load(),
    }
    .context("loading the configuration")?;

    tracing::info!(
        sources = config.sources.len(),
        "configuration loaded"
    );
    Backend::open(config)
}

async fn serve(listener: tokio::net::UnixListener, services: Arc<Services>) -> Result<()> {
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("accepting a control connection")?;
                let services = Arc::clone(&services);
                tokio::spawn(async move {
                    if let Err(err) = session::run(stream, services).await {
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
