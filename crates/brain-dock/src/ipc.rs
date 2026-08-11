//! The dock's half of the control connection, as an iced `Subscription`.
//!
//! Under Slint this was a Tokio thread marshalling into the UI with
//! `invoke_from_event_loop`. iced owns the executor, so the connection is just
//! a stream: `update()` folds each yielded event into state and there are no
//! cross-thread UI handles at all.
//!
//! The dock reconnects on its own. i3 starts both processes from the same
//! session file with no ordering guarantee, so the dock routinely comes up
//! before the daemon and must not treat that as fatal.
//!
//! **Token batching happens here, in the stream** — not in the UI. At the
//! measured generation rate tokens arrive roughly every 6 ms; one `Message`
//! each would wake the runtime ~170 times a second and re-lay-out a growing
//! text block every time, which reads as stutter rather than speed. So tokens
//! accumulate and are yielded as `Tokens` batches on a ~30 Hz tick.

use std::time::Duration;

use brain_proto::{ClientConnection, ClientRequest, ServerEvent, socket_path};
// Via `iced::futures` rather than `futures_util` directly: `iced::stream::channel`
// hands back a sender from *its* futures, and the two have to be the same type.
use iced::Subscription;
use iced::futures::channel::mpsc;
use iced::futures::{SinkExt as _, StreamExt as _};
use uuid::Uuid;

use crate::tokens;

/// Backoff bounds for reconnection. Fast enough that a daemon restart is
/// invisible, slow enough that a permanently absent daemon does not spin.
const RECONNECT_MIN: Duration = Duration::from_millis(200);
const RECONNECT_MAX: Duration = Duration::from_secs(5);

/// How many events may queue up before the stream applies backpressure.
const CHANNEL_DEPTH: usize = 256;

#[derive(Debug, Clone)]
pub enum Event {
    /// Handed out once per connection. The UI keeps the newest and drops any
    /// older one — a stale sender belongs to a socket that is already closed.
    Connected(mpsc::Sender<ClientRequest>),
    Disconnected,
    /// Any event except `Token`, which is batched into `Tokens`.
    Server(Box<ServerEvent>),
    /// A run of tokens for `id`, already concatenated.
    Tokens {
        id: Uuid,
        text: String,
    },
}

pub fn connect() -> Subscription<Event> {
    Subscription::run(stream)
}

fn stream() -> impl iced::futures::Stream<Item = Event> {
    iced::stream::channel(CHANNEL_DEPTH, async move |mut output| {
        let mut backoff = RECONNECT_MIN;

        loop {
            let (requests_tx, requests_rx) = mpsc::channel(CHANNEL_DEPTH);

            match serve(&mut output, requests_tx, requests_rx).await {
                Ok(()) => {
                    tracing::info!("daemon closed the connection; reconnecting");
                    backoff = RECONNECT_MIN;
                }
                Err(err) => {
                    tracing::debug!(%err, "not connected to brain-daemon");
                    backoff = (backoff * 2).min(RECONNECT_MAX);
                }
            }

            let _ = output.send(Event::Disconnected).await;
            tokio::time::sleep(backoff).await;
        }
    })
}

async fn serve(
    output: &mut mpsc::Sender<Event>,
    requests_tx: mpsc::Sender<ClientRequest>,
    mut requests_rx: mpsc::Receiver<ClientRequest>,
) -> anyhow::Result<()> {
    let path = socket_path()?;
    let connection = ClientConnection::connect(&path).await?;
    tracing::info!("connected to brain-daemon");

    let (mut sink, mut source) = connection.split();

    // Announce ourselves as *the* UI. The daemon replays the current visibility
    // state in response, so a dock that restarts mid-session comes back in
    // agreement with the daemon rather than guessing.
    sink.send(&ClientRequest::Subscribe).await?;
    output
        .send(Event::Connected(requests_tx))
        .await
        .map_err(|_| anyhow::anyhow!("UI dropped the event channel"))?;

    let mut batch = Batch::default();
    let mut ticker = tokio::time::interval(Duration::from_millis(tokens::TOKEN_FLUSH_MS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            incoming = source.recv() => {
                match incoming {
                    Some(Ok(ServerEvent::Token { id, text })) => {
                        if let Some(ready) = batch.push(id, &text) {
                            output.send(ready).await?;
                        }
                    }
                    Some(Ok(event)) => {
                        // Anything that is not a token ends the run: flush first
                        // so `Complete` never overtakes the tail of its answer.
                        if let Some(ready) = batch.take() {
                            output.send(ready).await?;
                        }
                        output.send(Event::Server(Box::new(event))).await?;
                    }
                    Some(Err(err)) => {
                        // One unparseable line is not worth a reconnect.
                        tracing::warn!(%err, "ignoring malformed event");
                    }
                    None => return Ok(()),
                }
            }
            _ = ticker.tick() => {
                if let Some(ready) = batch.take() {
                    output.send(ready).await?;
                }
            }
            request = requests_rx.next() => {
                let Some(request) = request else {
                    // UI side dropped its sender: the process is shutting down.
                    return Ok(());
                };
                sink.send(&request).await?;
            }
        }
    }
}

/// Tokens waiting to be shown, and the query they belong to.
#[derive(Default)]
struct Batch {
    /// Tokens for any other query are dropped. An abandoned query keeps
    /// streaming for a moment; without this its tail lands in the next answer.
    query: Option<Uuid>,
    pending: String,
}

impl Batch {
    /// Returns a batch to emit immediately once it is big enough to be worth
    /// showing; otherwise it waits for the tick.
    fn push(&mut self, id: Uuid, text: &str) -> Option<Event> {
        if self.query != Some(id) {
            // A new query's first token supersedes whatever was buffered.
            self.query = Some(id);
            self.pending.clear();
        }
        self.pending.push_str(text);

        if self.pending.len() >= tokens::TOKEN_FLUSH_CHARS {
            self.take()
        } else {
            None
        }
    }

    fn take(&mut self) -> Option<Event> {
        if self.pending.is_empty() {
            return None;
        }
        Some(Event::Tokens {
            id: self.query?,
            text: std::mem::take(&mut self.pending),
        })
    }
}
