//! JSON Lines framing over a Unix socket.
//!
//! One JSON object per line. The `Connection` type is generic over which
//! direction it speaks so the compiler enforces that a daemon cannot
//! accidentally send a `ClientRequest`, and vice versa.

use std::marker::PhantomData;
use std::path::Path;

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LinesCodec, LinesCodecError};

use crate::message::{ClientRequest, ServerEvent};

/// Generous, but bounded. A single message is a query, a token, or a status
/// report; nothing legitimate approaches this. An unbounded codec would let a
/// confused peer drive the process out of memory one byte at a time.
const MAX_LINE_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("i/o error on the control socket: {0}")]
    Io(#[from] std::io::Error),

    #[error("malformed message on the control socket: {0}")]
    Decode(#[from] serde_json::Error),

    #[error("message exceeded {MAX_LINE_BYTES} bytes")]
    TooLong,

    #[error("the peer closed the connection")]
    Closed,
}

impl From<LinesCodecError> for ProtoError {
    fn from(err: LinesCodecError) -> Self {
        match err {
            LinesCodecError::MaxLineLengthExceeded => Self::TooLong,
            LinesCodecError::Io(io) => Self::Io(io),
        }
    }
}

/// A typed, framed connection. `Tx` is what this end sends, `Rx` what it
/// receives.
pub struct Connection<Tx, Rx> {
    inner: Framed<UnixStream, LinesCodec>,
    _marker: PhantomData<fn(Tx) -> Rx>,
}

/// The `brainctl` / `brain-dock` end.
pub type ClientConnection = Connection<ClientRequest, ServerEvent>;
/// The `brain-daemon` end.
pub type ServerConnection = Connection<ServerEvent, ClientRequest>;

impl<Tx, Rx> Connection<Tx, Rx>
where
    Tx: Serialize,
    Rx: DeserializeOwned,
{
    pub fn new(stream: UnixStream) -> Self {
        Self {
            inner: Framed::new(stream, LinesCodec::new_with_max_length(MAX_LINE_BYTES)),
            _marker: PhantomData,
        }
    }

    /// Connect to an existing socket.
    ///
    /// A `ConnectionRefused` or `NotFound` here is the normal "daemon is not
    /// running" signal; callers should report it as that rather than as an I/O
    /// failure.
    pub async fn connect(path: &Path) -> Result<Self, ProtoError> {
        Ok(Self::new(UnixStream::connect(path).await?))
    }

    pub async fn send(&mut self, message: &Tx) -> Result<(), ProtoError> {
        // serde_json never emits a bare newline outside a string literal, and
        // strings are escaped, so one value is always exactly one line.
        let line = serde_json::to_string(message)?;
        self.inner.send(line).await?;
        Ok(())
    }

    /// Next message, or `None` once the peer hangs up.
    ///
    /// A decode error is returned rather than swallowed, but it does not
    /// invalidate the connection — the caller may log it and keep reading. A
    /// single malformed line should not take down a session.
    pub async fn recv(&mut self) -> Option<Result<Rx, ProtoError>> {
        let line = match self.inner.next().await? {
            Ok(line) => line,
            Err(err) => return Some(Err(err.into())),
        };
        Some(serde_json::from_str(&line).map_err(ProtoError::from))
    }

    /// Like `recv`, but treats a clean hangup as an error. For call sites that
    /// are waiting on a specific reply and cannot proceed without it.
    pub async fn recv_expected(&mut self) -> Result<Rx, ProtoError> {
        self.recv().await.ok_or(ProtoError::Closed)?
    }

    /// Split into independently owned halves.
    ///
    /// The daemon needs this: it streams events to the dock while
    /// simultaneously reading the next request (a `Cancel` arrives *during*
    /// generation, so reading cannot wait for writing to finish).
    pub fn split(self) -> (Sender<Tx>, Receiver<Rx>) {
        let (sink, stream) = self.inner.split();
        (
            Sender {
                inner: sink,
                _marker: PhantomData,
            },
            Receiver {
                inner: stream,
                _marker: PhantomData,
            },
        )
    }
}

/// Write half of a split [`Connection`].
pub struct Sender<Tx> {
    inner: futures_util::stream::SplitSink<Framed<UnixStream, LinesCodec>, String>,
    _marker: PhantomData<fn(Tx)>,
}

impl<Tx: Serialize> Sender<Tx> {
    pub async fn send(&mut self, message: &Tx) -> Result<(), ProtoError> {
        let line = serde_json::to_string(message)?;
        self.inner.send(line).await?;
        Ok(())
    }
}

/// Read half of a split [`Connection`].
pub struct Receiver<Rx> {
    inner: futures_util::stream::SplitStream<Framed<UnixStream, LinesCodec>>,
    _marker: PhantomData<fn() -> Rx>,
}

impl<Rx: DeserializeOwned> Receiver<Rx> {
    pub async fn recv(&mut self) -> Option<Result<Rx, ProtoError>> {
        let line = match self.inner.next().await? {
            Ok(line) => line,
            Err(err) => return Some(Err(err.into())),
        };
        Some(serde_json::from_str(&line).map_err(ProtoError::from))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;
    use uuid::Uuid;

    async fn socket_pair() -> (ClientConnection, ServerConnection) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sock");
        let listener = UnixListener::bind(&path).unwrap();

        let accept = tokio::spawn(async move { listener.accept().await.unwrap().0 });
        let client = UnixStream::connect(&path).await.unwrap();
        let server = accept.await.unwrap();

        // `dir` drops here; the sockets stay valid because both ends are open.
        (Connection::new(client), Connection::new(server))
    }

    #[tokio::test]
    async fn request_and_event_cross_the_wire() {
        let (mut client, mut server) = socket_pair().await;
        let id = Uuid::new_v4();

        client
            .send(&ClientRequest::Query {
                id,
                text: "how do I mirror bones?".into(),
                context: Default::default(),
                retrieval_only: false,
            })
            .await
            .unwrap();

        match server.recv_expected().await.unwrap() {
            ClientRequest::Query { id: got, text, .. } => {
                assert_eq!(got, id);
                assert_eq!(text, "how do I mirror bones?");
            }
            other => panic!("unexpected: {other:?}"),
        }

        server
            .send(&ServerEvent::Token {
                id,
                text: "Use ".into(),
            })
            .await
            .unwrap();
        assert_eq!(
            client.recv_expected().await.unwrap(),
            ServerEvent::Token { id, text: "Use ".into() }
        );
    }

    /// A token stream is the real workload: thousands of tiny messages, fast.
    ///
    /// Producer and consumer must run concurrently. Writing the whole stream
    /// before reading any of it fills the socket buffer and deadlocks — which
    /// is exactly why the daemon gives each connection a dedicated writer task
    /// instead of sending inline from the request handler.
    #[tokio::test]
    async fn a_long_token_stream_arrives_in_order() {
        let (mut client, mut server) = socket_pair().await;
        let id = Uuid::new_v4();
        const COUNT: usize = 5_000;

        let producer = tokio::spawn(async move {
            for i in 0..COUNT {
                server
                    .send(&ServerEvent::Token {
                        id,
                        text: format!("{i} "),
                    })
                    .await
                    .unwrap();
            }
        });

        for i in 0..COUNT {
            match client.recv_expected().await.unwrap() {
                ServerEvent::Token { text, .. } => assert_eq!(text, format!("{i} ")),
                other => panic!("unexpected: {other:?}"),
            }
        }
        producer.await.unwrap();
    }

    #[tokio::test]
    async fn hangup_ends_the_stream() {
        let (mut client, server) = socket_pair().await;
        drop(server);
        assert!(client.recv().await.is_none());
        assert!(matches!(
            client.recv_expected().await,
            Err(ProtoError::Closed)
        ));
    }

    #[tokio::test]
    async fn a_malformed_line_does_not_kill_the_connection() {
        let (mut client, mut server) = socket_pair().await;

        // Speak nonsense, then speak sense.
        {
            let raw: &mut Framed<UnixStream, LinesCodec> = &mut server.inner;
            raw.send("{not json".to_string()).await.unwrap();
        }
        server.send(&ServerEvent::HideDock).await.unwrap();

        assert!(matches!(
            client.recv().await,
            Some(Err(ProtoError::Decode(_)))
        ));
        assert_eq!(client.recv_expected().await.unwrap(), ServerEvent::HideDock);
    }
}
