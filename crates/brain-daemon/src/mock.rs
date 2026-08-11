//! A canned query pipeline.
//!
//! Stands in for retrieval and generation until Stages 1 and 2 exist, and
//! stays afterwards behind `--mock`. It is how UI timing and animation get
//! tuned without loading a model, and how the dock's streaming path is tested
//! in CI where no GPU exists.
//!
//! The timings mirror what was actually measured on this machine
//! (`llama-bench`: pp2048 ≈ 6555 t/s, tg128 ≈ 168 t/s), so the mock feels like
//! the real thing rather than like an instant stub — a mock that returns
//! immediately would let genuinely bad streaming UX pass unnoticed.

use std::time::{Duration, Instant};

use brain_core::{ActionId, SectionId};
use brain_proto::{
    ActionKind, ActionView, CacheStatus, ServerEvent, SourceRef, TimingInfo,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Retrieval: fast, and the reason sources are painted before generation.
const RETRIEVAL: Duration = Duration::from_millis(90);
/// Prefill of a ~2000-token context pack at the measured 6555 t/s.
const TIME_TO_FIRST_TOKEN: Duration = Duration::from_millis(310);
/// 168 t/s ≈ 6 ms per token.
const PER_TOKEN: Duration = Duration::from_millis(6);

const ANSWER: &str = "You stopped the jitter by smoothing the crop target instead of moving \
it directly to every cursor position. Read the pointer, apply exponential smoothing to the \
rectangle's target, then update the OBS crop transform once per frame.";

pub async fn run(
    id: Uuid,
    query: String,
    retrieval_only: bool,
    events: UnboundedSender<ServerEvent>,
    cancel: CancellationToken,
) {
    let started = Instant::now();
    let _ = events.send(ServerEvent::QueryAccepted { id });

    let _ = events.send(ServerEvent::RetrievalStarted { id });
    if sleep_or_cancel(RETRIEVAL, &cancel).await {
        return;
    }
    let retrieval_ms = started.elapsed().as_millis() as u32;

    let sources = vec![SourceRef {
        section_id: SectionId(1),
        // A real uid from the vault, so the graph panel has something to seed on before
        // Stage 1 retrieval exists. Replaced by whatever the index returns.
        section_uid: format!("{}#root", topic(&query)),
        path: format!("~/brain/{}.md", topic(&query)).into(),
        heading_path: "OBS workflows > Follow cursor > Smoothing".into(),
        start_line: 12,
        end_line: 18,
        score: -8.42,
        snippet: "Apply exponential smoothing before updating the crop transform.".into(),
        explain: "matched heading · 1 hop to contradicts".into(),
    }];

    let _ = events.send(ServerEvent::RetrievalComplete {
        id,
        source_count: sources.len(),
    });
    let _ = events.send(ServerEvent::Sources {
        id,
        items: sources,
    });
    let _ = events.send(ServerEvent::Actions {
        id,
        items: mock_actions(),
    });

    if retrieval_only {
        let _ = events.send(ServerEvent::Complete {
            id,
            timing: TimingInfo {
                retrieval_ms,
                total_ms: started.elapsed().as_millis() as u32,
                ..Default::default()
            },
            cache: CacheStatus::default(),
        });
        return;
    }

    let _ = events.send(ServerEvent::GenerationStarted { id });
    if sleep_or_cancel(TIME_TO_FIRST_TOKEN, &cancel).await {
        return;
    }
    let ttft_ms = started.elapsed().as_millis() as u32;

    // Word by word with the space attached, which is roughly how a tokenizer
    // emits prose and keeps the dock's incremental append honest.
    let mut output_tokens = 0;
    for word in ANSWER.split_inclusive(' ') {
        if cancel.is_cancelled() {
            return;
        }
        let _ = events.send(ServerEvent::Token {
            id,
            text: word.to_string(),
        });
        output_tokens += 1;
        if sleep_or_cancel(PER_TOKEN, &cancel).await {
            return;
        }
    }

    let total_ms = started.elapsed().as_millis() as u32;
    let _ = events.send(ServerEvent::Complete {
        id,
        timing: TimingInfo {
            context_ms: 0,
            retrieval_ms,
            prompt_ms: 0,
            ttft_ms,
            generation_ms: total_ms.saturating_sub(ttft_ms),
            total_ms,
            output_tokens,
        },
        cache: CacheStatus::default(),
    });
}

fn mock_actions() -> Vec<ActionView> {
    vec![
        ActionView {
            id: ActionId(1),
            kind: ActionKind::OpenFile,
            label: "Note".into(),
            detail: "~/brain/obs.md:12".into(),
            enabled: true,
        },
        ActionView {
            id: ActionId(2),
            kind: ActionKind::OpenFile,
            label: "Code".into(),
            detail: "~/projects/obs-follow/src/smoothing.rs:42".into(),
            enabled: true,
        },
        ActionView {
            id: ActionId(3),
            kind: ActionKind::OpenVideo,
            label: "▶ 06:54".into(),
            detail: "https://example.com/watch?v=ABC".into(),
            enabled: true,
        },
        ActionView {
            id: ActionId(4),
            kind: ActionKind::LaunchDesktopApp,
            label: "OBS".into(),
            detail: "obs.desktop".into(),
            enabled: true,
        },
        // One deliberately broken target, so the disabled rendering is exercised
        // every time rather than only when a real note rots.
        ActionView {
            id: ActionId(5),
            kind: ActionKind::OpenFile,
            label: "Missing".into(),
            detail: "~/gone/nowhere.md (target does not exist)".into(),
            enabled: false,
        },
    ]
}

/// Returns true if the wait was cancelled.
async fn sleep_or_cancel(duration: Duration, cancel: &CancellationToken) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(duration) => false,
        _ = cancel.cancelled() => true,
    }
}

/// Pull a plausible filename out of the query so successive mock answers do not
/// all look identical while iterating on the UI.
fn topic(query: &str) -> String {
    query
        .split_whitespace()
        .rfind(|w| w.len() > 3)
        .unwrap_or("notes")
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
}
