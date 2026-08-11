//! The wire contract between `brainctl`, `brain-daemon`, and `brain-dock`.
//!
//! Transport is a Unix domain socket carrying JSON Lines (spec §26). JSON is
//! chosen over MessagePack deliberately: it is inspectable with `nc` and `jq`,
//! and the payloads are tiny next to model inference. Revisit only if profiling
//! says otherwise.
//!
//! This crate is the *only* thing the three binaries share. Anything that leaks
//! between them belongs here or nowhere.

pub mod codec;
pub mod message;
pub mod socket;

pub use codec::{ClientConnection, Connection, ProtoError, Receiver, Sender, ServerConnection};
pub use message::{
    ActionKind, ActionView, CacheStatus, ClientRequest, DesktopContext, ServerEvent, SourceRef,
    StatusReport, TimingInfo,
};
// The ids appear in this crate's public types, so anyone holding a `SourceRef` or an
// `ActionView` needs to be able to name them without also depending on `brain-core`.
pub use brain_core::{ActionId, SectionId};
pub use socket::socket_path;
