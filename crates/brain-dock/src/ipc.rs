//! The dock's half of the control connection.
//!
//! Slint owns the main thread and Tokio runs on its own, so the two talk
//! through exactly two channels:
//!
//! - UI → backend: an unbounded mpsc of `ClientRequest`, captured in Slint
//!   callbacks (which run on the UI thread).
//! - backend → UI: `slint::invoke_from_event_loop`, which is the only
//!   supported way to touch a `slint::Weak` from another thread.
//!
//! The dock reconnects on its own. i3 starts both processes from the same
//! session file with no ordering guarantee, so the dock routinely comes up
//! before the daemon and must not treat that as fatal.

use std::time::Duration;

use brain_proto::{ClientConnection, ClientRequest, ServerEvent, socket_path};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// Backoff bounds for reconnection. Fast enough that a daemon restart is
/// invisible, slow enough that a permanently absent daemon does not spin.
const RECONNECT_MIN: Duration = Duration::from_millis(200);
const RECONNECT_MAX: Duration = Duration::from_secs(5);

/// Run the client loop until the process exits.
///
/// `on_event` is invoked on the Tokio thread; it is responsible for hopping to
/// the UI thread itself.
pub async fn run<F>(mut outbox: UnboundedReceiver<ClientRequest>, on_event: F)
where
    F: Fn(ServerEvent) + Send + Clone + 'static,
{
    let mut backoff = RECONNECT_MIN;

    loop {
        match connect_and_serve(&mut outbox, on_event.clone()).await {
            Ok(()) => {
                tracing::info!("daemon closed the connection; reconnecting");
                backoff = RECONNECT_MIN;
            }
            Err(err) => {
                tracing::debug!(%err, "not connected to brain-daemon");
                backoff = (backoff * 2).min(RECONNECT_MAX);
            }
        }
        tokio::time::sleep(backoff).await;
    }
}

async fn connect_and_serve<F>(
    outbox: &mut UnboundedReceiver<ClientRequest>,
    on_event: F,
) -> anyhow::Result<()>
where
    F: Fn(ServerEvent) + Send + 'static,
{
    let path = socket_path()?;
    let connection = ClientConnection::connect(&path).await?;
    tracing::info!("connected to brain-daemon");

    let (mut sink, mut source) = connection.split();

    // Announce ourselves as *the* UI. The daemon replays the current
    // visibility state in response, so a dock that restarts mid-session comes
    // back in agreement with the daemon rather than guessing.
    sink.send(&ClientRequest::Subscribe).await?;

    loop {
        tokio::select! {
            incoming = source.recv() => {
                match incoming {
                    Some(Ok(event)) => on_event(event),
                    Some(Err(err)) => {
                        // One unparseable line is not worth a reconnect.
                        tracing::warn!(%err, "ignoring malformed event");
                    }
                    None => return Ok(()),
                }
            }
            request = outbox.recv() => {
                let Some(request) = request else {
                    // UI side dropped its sender: the process is shutting down.
                    return Ok(());
                };
                sink.send(&request).await?;
            }
        }
    }
}

/// Spawn the Tokio runtime on its own thread and return the UI-side sender.
pub fn spawn<F>(on_event: F) -> UnboundedSender<ClientRequest>
where
    F: Fn(ServerEvent) + Send + Clone + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

    std::thread::Builder::new()
        .name("brain-ipc".into())
        .spawn(move || {
            // A single-threaded runtime is plenty: this thread does nothing but
            // shuttle small JSON messages, and it keeps the dock's footprint
            // honest next to a daemon that is holding a model in VRAM.
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    tracing::error!(%err, "could not start the IPC runtime");
                    return;
                }
            };
            runtime.block_on(run(rx, on_event));
        })
        .expect("spawning the IPC thread");

    tx
}
