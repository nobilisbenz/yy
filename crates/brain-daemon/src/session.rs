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

use crate::backend::Backend;
use crate::state::{Daemon, Visibility};

/// What a session needs to answer with: the daemon's own state, and the index behind it.
///
/// `backend` is `None` only when the daemon was started with `--mock`, which is how UI
/// timing work stays possible with no vault and no model.
pub struct Services {
    pub daemon: Arc<Daemon>,
    pub backend: Option<Arc<Backend>>,
}

pub async fn run(stream: UnixStream, services: Arc<Services>) -> Result<()> {
    let daemon = Arc::clone(&services.daemon);
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

        handle(request, &services, &events, &mut ui_token, &mut running);
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
    services: &Arc<Services>,
    events: &UnboundedSender<ServerEvent>,
    ui_token: &mut Option<u64>,
    running: &mut HashMap<Uuid, CancellationToken>,
) {
    let daemon = &services.daemon;
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

        ClientRequest::ToggleGraph => {
            let visible = daemon.toggle_graph();
            tracing::debug!(visible, "graph panel toggled");
        }

        ClientRequest::Status => {
            let _ = events.send(ServerEvent::Status(Box::new(daemon.status())));
        }

        ClientRequest::PauseIndexing => {
            daemon.set_indexing_paused(true);
            if let Some(backend) = &services.backend {
                backend.index().set_paused(true);
            }
        }
        ClientRequest::ResumeIndexing => {
            daemon.set_indexing_paused(false);
            if let Some(backend) = &services.backend {
                backend.index().set_paused(false);
            }
        }

        ClientRequest::Query {
            id,
            text,
            retrieval_only,
            context,
        } => {
            // A new query supersedes whatever this connection had running.
            // Without this, an abandoned query keeps streaming tokens that the
            // dock has to discard by id — correct, but wasteful, and on a real
            // model it holds a slot that the new query needs.
            for (_, cancel) in running.drain() {
                cancel.cancel();
            }

            // A client with nothing to report — `brainctl ask` from a terminal — gets the
            // context captured at the last summon. `--no-context` says so explicitly and
            // is left alone.
            let context = if context.is_empty() {
                daemon.context()
            } else {
                context
            };

            let cancel = CancellationToken::new();
            running.insert(id, cancel.clone());

            match services.backend.clone() {
                Some(backend) => {
                    let events = events.clone();
                    let daemon = Arc::clone(daemon);
                    tokio::spawn(async move {
                        if let Some(timing) =
                            backend
                                .query(id, text, retrieval_only, context, events, cancel)
                                .await
                        {
                            // Keep `brainctl status` honest about the last query. A
                            // cancelled query reports nothing rather than a truncated time.
                            daemon.record_query(timing);
                        }
                        if let Ok(stats) = backend.stats().await {
                            daemon.record_counts(stats);
                        }
                    });
                }
                None => {
                    tokio::spawn(crate::mock::run(
                        id,
                        text,
                        retrieval_only,
                        events.clone(),
                        cancel,
                    ));
                }
            }
        }

        ClientRequest::ActivateAction { id, action } => {
            let Some(backend) = &services.backend else {
                tracing::debug!("action activated against a mock daemon; nothing to open");
                return;
            };
            // Hide first, then spawn. The other order leaves the dock on screen while the
            // editor maps, which reads as a stutter.
            daemon.set_visible(false);
            if let Err(error) = backend.activate(id, action) {
                tracing::warn!(%error, "could not activate the action");
                let _ = events.send(ServerEvent::Error {
                    id: Some(id),
                    message: error.to_string(),
                });
            }
        }

        ClientRequest::SaveCorrection { id, answer } => {
            let Some(backend) = services.backend.clone() else {
                return;
            };
            let events = events.clone();
            // Spawned because saving re-checks staleness against the index, which is a
            // read this connection should not wait on.
            tokio::spawn(async move {
                if let Err(error) = backend.correct(id, &answer) {
                    tracing::warn!(%error, "could not save the correction");
                    let _ = events.send(ServerEvent::Error {
                        id: Some(id),
                        message: error.to_string(),
                    });
                }
            });
        }

        ClientRequest::RateAnswer { id, good } => {
            let Some(backend) = &services.backend else {
                return;
            };
            let rating = if good {
                brain_engine::store::Rating::Good
            } else {
                brain_engine::store::Rating::Bad
            };
            if let Err(error) = backend.rate(id, rating) {
                tracing::warn!(%error, "could not record the rating");
            }
        }

        ClientRequest::Cancel { id } => {
            if let Some(cancel) = running.remove(&id) {
                tracing::debug!(%id, "cancelled");
                cancel.cancel();
            }
        }
        ClientRequest::Reindex => {
            let Some(backend) = services.backend.clone() else {
                let _ = events.send(ServerEvent::Error {
                    id: None,
                    message: "this daemon was started with --mock and has no index".into(),
                });
                return;
            };

            let events = events.clone();
            let daemon = Arc::clone(daemon);
            // Spawned rather than awaited: a reindex of a large vault takes seconds, and
            // this connection has to stay readable for a `Cancel` or a `Status` throughout.
            tokio::spawn(async move {
                match backend.reindex().await {
                    Ok(stats) => {
                        daemon.record_counts(stats);
                        let _ = events.send(ServerEvent::Status(Box::new(daemon.status())));
                    }
                    Err(error) => {
                        let _ = events.send(ServerEvent::Error {
                            id: None,
                            message: error.to_string(),
                        });
                    }
                }
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
