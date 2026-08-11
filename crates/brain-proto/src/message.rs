//! Request and event types.
//!
//! Two invariants shape everything here, both learned from the spec's own
//! failure modes:
//!
//! 1. **Every event that belongs to a query carries its `id`.** A query the
//!    user abandoned will keep streaming tokens for a moment; without the id
//!    those tokens land in the *next* answer. The dock drops events whose id is
//!    not the one it is currently showing.
//!
//! 2. **Visibility flows daemon → dock.** `brainctl toggle` talks only to the
//!    daemon, which owns `visible` and relays `ShowDock`/`HideDock`. That keeps
//!    `brainctl` a stateless one-shot binary with no window knowledge.

use std::path::PathBuf;

use brain_core::{ActionId, SectionId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// client → server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientRequest {
    /// Ask a question. `id` is minted by the caller so it can cancel later.
    Query {
        id: Uuid,
        text: String,
        #[serde(default)]
        context: DesktopContext,
        /// Retrieval only — skip generation. Used by `brainctl ask --no-llm`
        /// and by the dock's as-you-type search.
        #[serde(default)]
        retrieval_only: bool,
    },
    /// Abandon a running query. Idempotent; unknown ids are ignored.
    Cancel { id: Uuid },

    /// Visibility. The daemon resolves `Toggle` against its own state.
    Toggle,
    Show,
    Hide,

    /// Sent once by `brain-dock` to register as *the* UI connection.
    Subscribe,

    Status,
    Reindex,
    PauseIndexing,
    ResumeIndexing,
}

// ---------------------------------------------------------------------------
// server → client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    QueryAccepted {
        id: Uuid,
    },
    RetrievalStarted {
        id: Uuid,
    },
    RetrievalComplete {
        id: Uuid,
        source_count: usize,
    },
    /// Sources are emitted *before* generation starts, not after.
    ///
    /// Retrieval finishes in ~100 ms; generation takes an order of magnitude
    /// longer. Painting the source path and its actions immediately, then
    /// filling in prose, is what makes the dock feel instant — and often the
    /// buttons were all the user needed. (Deviates from spec §25's ordering.)
    Sources {
        id: Uuid,
        items: Vec<SourceRef>,
    },
    Actions {
        id: Uuid,
        items: Vec<ActionView>,
    },
    GenerationStarted {
        id: Uuid,
    },
    Token {
        id: Uuid,
        text: String,
    },
    Complete {
        id: Uuid,
        timing: TimingInfo,
        cache: CacheStatus,
    },
    /// Retrieval found nothing confident enough to answer from (spec §45).
    /// A deliberate outcome, not an error — the model was never called.
    NoAnswer {
        id: Uuid,
        closest: Vec<SourceRef>,
    },
    Error {
        id: Option<Uuid>,
        message: String,
    },

    /// daemon → dock only.
    ShowDock {
        context: DesktopContext,
    },
    HideDock,

    Status(Box<StatusReport>),
}

impl ServerEvent {
    /// The query this event belongs to, if any. The dock uses this to discard
    /// stragglers from a query it has already moved on from.
    pub fn query_id(&self) -> Option<Uuid> {
        match self {
            Self::QueryAccepted { id }
            | Self::RetrievalStarted { id }
            | Self::RetrievalComplete { id, .. }
            | Self::Sources { id, .. }
            | Self::Actions { id, .. }
            | Self::GenerationStarted { id }
            | Self::Token { id, .. }
            | Self::Complete { id, .. }
            | Self::NoAnswer { id, .. } => Some(*id),
            Self::Error { id, .. } => *id,
            Self::ShowDock { .. } | Self::HideDock | Self::Status(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// payloads
// ---------------------------------------------------------------------------

/// What the user was doing when they summoned the dock (spec §18).
///
/// Every field is optional and best-effort. Context *boosts* ranking; it never
/// filters. A failed X11 read must never fail a query.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DesktopContext {
    pub wm_class: Option<String>,
    pub window_title: Option<String>,
    pub pid: Option<u32>,
    pub process_name: Option<String>,
    pub cwd: Option<PathBuf>,
    pub workspace: Option<String>,
}

impl DesktopContext {
    /// Coarse cache key. Bucketing on the full struct would mean the retrieval
    /// cache never hits, since window titles change constantly.
    pub fn cache_bucket(&self) -> String {
        format!(
            "{}|{}",
            self.wm_class.as_deref().unwrap_or("-"),
            self.cwd.as_deref().and_then(|p| p.to_str()).unwrap_or("-"),
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SourceRef {
    pub section_id: SectionId,
    pub path: PathBuf,
    /// `OBS workflows > Follow cursor > Smoothing`
    pub heading_path: String,
    pub start_line: u32,
    pub end_line: u32,
    /// Internal score. The normal UI does not render this (spec §24); the
    /// debug view does.
    pub score: f32,
    /// First lines of the section, for the expanded source list.
    pub snippet: String,
}

/// A trusted jump target. Always built by application code from parsed
/// metadata — never from model output (spec §3.3, §48).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActionView {
    pub id: ActionId,
    pub kind: ActionKind,
    /// What the button says: `Note`, `Code`, `▶ 06:54`, `OBS`.
    pub label: String,
    /// Shown on hover / in the debug view, not on the button.
    pub detail: String,
    /// A resolved target that failed validation at index time (dead path,
    /// unknown desktop id). Rendered disabled rather than hidden — a broken
    /// link you can see beats one that silently vanished.
    pub enabled: bool,
}

/// Note the absence of a `RunShell` variant. This is structural, not an
/// oversight: if the type cannot represent an arbitrary command, no amount of
/// prompt injection or parser confusion can produce one (spec §11, §48).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    OpenFile,
    OpenUrl,
    OpenVideo,
    LaunchDesktopApp,
    RevealPath,
    CopyText,
    OpenProject,
    OpenTerminal,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimingInfo {
    pub context_ms: u32,
    pub retrieval_ms: u32,
    pub prompt_ms: u32,
    /// Time to first token. The number that decides whether this feels fast.
    pub ttft_ms: u32,
    pub generation_ms: u32,
    pub total_ms: u32,
    pub output_tokens: u32,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheStatus {
    pub retrieval_hit: bool,
    pub answer_hit: bool,
}

/// `brainctl status` (spec §38).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StatusReport {
    pub daemon_version: String,
    pub uptime_seconds: u64,
    pub llm_model: Option<String>,
    pub llm_backend: Option<String>,
    pub llm_state: String,
    pub llm_context_tokens: Option<u32>,
    pub indexed_documents: u64,
    pub indexed_sections: u64,
    pub embedding_queue: u64,
    pub index_generation: u64,
    pub indexing_paused: bool,
    pub ui_connected: bool,
    pub dock_visible: bool,
    pub last_query: Option<TimingInfo>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant must survive a round trip. A field that fails to
    /// deserialize shows up as a mystery hang at runtime, not a compile error.
    #[test]
    fn requests_round_trip() {
        let id = Uuid::new_v4();
        let cases = vec![
            ClientRequest::Query {
                id,
                text: "how did I make OBS follow the cursor?".into(),
                context: DesktopContext {
                    wm_class: Some("obs".into()),
                    cwd: Some(PathBuf::from("/home/nabi/projects/obs-tools")),
                    ..Default::default()
                },
                retrieval_only: false,
            },
            ClientRequest::Cancel { id },
            ClientRequest::Toggle,
            ClientRequest::Show,
            ClientRequest::Hide,
            ClientRequest::Subscribe,
            ClientRequest::Status,
            ClientRequest::Reindex,
            ClientRequest::PauseIndexing,
            ClientRequest::ResumeIndexing,
        ];

        for case in cases {
            let line = serde_json::to_string(&case).unwrap();
            assert!(!line.contains('\n'), "JSON Lines framing forbids newlines");
            assert_eq!(serde_json::from_str::<ClientRequest>(&line).unwrap(), case);
        }
    }

    #[test]
    fn events_round_trip() {
        let id = Uuid::new_v4();
        let source = SourceRef {
            section_id: SectionId(3),
            path: PathBuf::from("/home/nabi/brain/obs.md"),
            heading_path: "OBS > Follow cursor > Smoothing".into(),
            start_line: 12,
            end_line: 18,
            score: -8.42,
            snippet: "Apply exponential smoothing…".into(),
        };
        let action = ActionView {
            id: ActionId(1),
            kind: ActionKind::OpenVideo,
            label: "▶ 06:54".into(),
            detail: "https://example.com/watch?v=ABC".into(),
            enabled: true,
        };

        let cases = vec![
            ServerEvent::QueryAccepted { id },
            ServerEvent::RetrievalStarted { id },
            ServerEvent::RetrievalComplete {
                id,
                source_count: 3,
            },
            ServerEvent::Sources {
                id,
                items: vec![source.clone()],
            },
            ServerEvent::Actions {
                id,
                items: vec![action],
            },
            ServerEvent::GenerationStarted { id },
            ServerEvent::Token {
                id,
                text: " smoothing".into(),
            },
            ServerEvent::Complete {
                id,
                timing: TimingInfo::default(),
                cache: CacheStatus::default(),
            },
            ServerEvent::NoAnswer {
                id,
                closest: vec![source],
            },
            ServerEvent::Error {
                id: Some(id),
                message: "boom".into(),
            },
            ServerEvent::Error {
                id: None,
                message: "boom".into(),
            },
            ServerEvent::ShowDock {
                context: DesktopContext::default(),
            },
            ServerEvent::HideDock,
            ServerEvent::Status(Box::new(StatusReport::default())),
        ];

        for case in cases {
            let line = serde_json::to_string(&case).unwrap();
            assert!(!line.contains('\n'), "JSON Lines framing forbids newlines");
            assert_eq!(serde_json::from_str::<ServerEvent>(&line).unwrap(), case);
        }
    }

    /// Multi-line text must not break framing — a correction or an answer
    /// containing newlines is normal, and JSON escapes them as `\n`.
    #[test]
    fn embedded_newlines_stay_on_one_line() {
        let ev = ServerEvent::Token {
            id: Uuid::nil(),
            text: "line one\nline two".into(),
        };
        let line = serde_json::to_string(&ev).unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(serde_json::from_str::<ServerEvent>(&line).unwrap(), ev);
    }

    #[test]
    fn query_id_is_exposed_for_stale_event_filtering() {
        let id = Uuid::new_v4();
        assert_eq!(ServerEvent::Token { id, text: "x".into() }.query_id(), Some(id));
        assert_eq!(ServerEvent::HideDock.query_id(), None);
        assert_eq!(
            ServerEvent::Error { id: None, message: "x".into() }.query_id(),
            None
        );
    }

    #[test]
    fn context_bucket_ignores_volatile_fields() {
        let a = DesktopContext {
            wm_class: Some("obs".into()),
            window_title: Some("OBS 30.0 — Scene 1".into()),
            ..Default::default()
        };
        let b = DesktopContext {
            wm_class: Some("obs".into()),
            window_title: Some("OBS 30.0 — Scene 2 (recording)".into()),
            pid: Some(9999),
            ..Default::default()
        };
        assert_eq!(a.cache_bucket(), b.cache_bucket());
    }
}
