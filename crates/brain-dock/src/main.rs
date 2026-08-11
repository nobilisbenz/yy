//! `brain-dock` — the window.
//!
//! Deliberately thin. It renders what the daemon sends and forwards what the
//! user does; it holds no index, no model, and no opinions about retrieval.
//! Keeping it that way is what lets it stay resident and appear instantly.

mod platform;

use anyhow::{Context, Result};
use clap::Parser;

slint::include_modules!();

#[derive(Parser, Debug)]
#[command(name = "brain-dock", version, about = "Brain Dock window")]
struct Args {
    /// Start without mapping the window. This is how i3 launches it: the
    /// process is resident from login, and `brainctl toggle` maps it.
    #[arg(long)]
    hidden: bool,

    /// Override the UI scale factor. Slint's guess is wrong on some panels;
    /// 1.0 matches an unscaled 96 DPI X session.
    #[arg(long, default_value = "1.0")]
    scale: f32,

    #[arg(long, env = "BRAIN_LOG", default_value = "info")]
    log: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    init_tracing(&args.log);

    platform::install(args.scale)?;

    let dock = Dock::new().context("creating the dock window")?;

    // Slint tears down the event loop when the last window closes, which would
    // turn `Esc` into "quit the application". The dock is meant to outlive
    // every dismissal — C6 replaces show/hide entirely with an X11 map/unmap
    // on a window that is created once and never destroyed.
    dock.show().context("showing the dock window")?;
    dock.invoke_focus_query();

    dock.on_dismiss(|| {
        tracing::debug!("dismiss (window control lands in C6)");
    });

    dock.on_submit(|text| {
        tracing::info!(query = %text, "submit (no backend yet)");
    });

    slint::run_event_loop().context("running the Slint event loop")?;
    Ok(())
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
