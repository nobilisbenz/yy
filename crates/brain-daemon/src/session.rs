//! One control connection.
//!
//! Two kinds of client arrive on the same socket: `brainctl`, which sends one
//! request and leaves, and `brain-dock`, which sends `Subscribe` and then stays
//! for the session. They are handled identically until the `Subscribe` arrives.
//!
//! The connection is split so that reading never waits on writing. That matters
//! during generation: a `Cancel` has to be readable while tokens are still
//! streaming out.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use brain_proto::{ClientRequest, ServerConnection, ServerEvent};
use tokio::net::UnixStream;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::state::{Daemon, Visibility};

pub async fn run(stream: UnixStream, daemon: Arc<Daemon>, mock: bool) -> Result<()> {
    let (mut sink, mut source) = ServerConnection::new(stream).split();
    let (events, mut outbox) = unbounded_channel::<ServerEvent>();

    // Writer task. Owns the write half for the life of the connection so that
    // any number of producers (this session, the daemon's UI broadcast) can
    // enqueue without contending for it.
    let writer = tokio::spawn(async move {
        while let Some(event) = outbox.recv().await {
            if let Err(err) = sink.send(&event).await {
                tracing::debug!(%err, "write failed; peer is gone");
                break;
            }
        }
    });

    let mut ui_token = None;
    let mut running: HashMap<Uuid, CancellationToken> = HashMap::new();

    while let Some(message) = source.recv().await {
        let request = match message {
            Ok(request) => request,
            Err(err) => {
                // One bad line is not worth dropping the connection over; the
                // next one may be fine. A peer that only ever sends garbage
                // will hang up on its own.
                tracing::warn!(%err, "ignoring malformed request");
                let _ = events.send(ServerEvent::Error {
                    id: None,
                    message: err.to_string(),
                });
                continue;
            }
        };

        handle(request, &daemon, &events, &mut ui_token, &mut running, mock);
    }

    // Nothing is listening any more, so stop paying for work nobody will see.
    for (_, cancel) in running.drain() {
        cancel.cancel();
    }

    if let Some(token) = ui_token {
        tracing::info!("dock disconnected");
        daemon.unregister_ui(token);
    }

    drop(events);
    let _ = writer.await;
    Ok(())
}

fn handle(
    request: ClientRequest,
    daemon: &Arc<Daemon>,
    events: &UnboundedSender<ServerEvent>,
    ui_token: &mut Option<u64>,
    running: &mut HashMap<Uuid, CancellationToken>,
    mock: bool,
) {
    match request {
        ClientRequest::Subscribe => {
            if ui_token.is_some() {
                tracing::debug!("duplicate Subscribe on the same connection; ignoring");
                return;
            }
            tracing::info!("dock connected");
            *ui_token = Some(daemon.register_ui(events.clone()));
        }

        ClientRequest::Toggle => log_visibility(daemon.toggle()),
        ClientRequest::Show => log_visibility(daemon.set_visible(true)),
        ClientRequest::Hide => log_visibility(daemon.set_visible(false)),

        ClientRequest::Status => {
            let _ = events.send(ServerEvent::Status(Box::new(daemon.status())));
        }

        ClientRequest::PauseIndexing => daemon.set_indexing_paused(true),
        ClientRequest::ResumeIndexing => daemon.set_indexing_paused(false),

        ClientRequest::Query {
            id,
            text,
            retrieval_only,
            ..
        } => {
            // A new query supersedes whatever this connection had running.
            // Without this, an abandoned query keeps streaming tokens that the
            // dock has to discard by id — correct, but wasteful, and on a real
            // model it holds a slot that the new query needs.
            for (_, cancel) in running.drain() {
                cancel.cancel();
            }

            if !mock {
                let _ = events.send(ServerEvent::Error {
                    id: Some(id),
                    message: "query pipeline not implemented yet (Stage 1) — \
                              run brain-daemon --mock to exercise the UI"
                        .into(),
                });
                return;
            }

            let cancel = CancellationToken::new();
            running.insert(id, cancel.clone());
            tokio::spawn(crate::mock::run(
                id,
                text,
                retrieval_only,
                events.clone(),
                cancel,
            ));
        }

        ClientRequest::Cancel { id } => {
            if let Some(cancel) = running.remove(&id) {
                tracing::debug!(%id, "cancelled");
                cancel.cancel();
            }
        }
        ClientRequest::Reindex => {
            let _ = events.send(ServerEvent::Error {
                id: None,
                message: "indexing not implemented yet (Stage 1)".into(),
            });
        }
    }
}

fn log_visibility(outcome: Visibility) {
    match outcome {
        Visibility::Shown => tracing::debug!("dock shown"),
        Visibility::Hidden => tracing::debug!("dock hidden"),
        Visibility::Unchanged => tracing::debug!("visibility unchanged"),
    }
}
