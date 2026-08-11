//! Incremental answer rendering.
//!
//! Tokens arrive roughly every 6 ms at the measured generation rate. Pushing
//! each one straight into a Slint property means waking the event loop ~170
//! times a second and re-laying-out a growing text block every time — which
//! reads as stutter, not speed.
//!
//! So tokens accumulate here and flush on a timer. The batching is built in
//! from the first commit rather than retrofitted, because it changes the
//! perceived timing of the show and expand animations, and those get tuned
//! against whatever behaviour is in place.

use std::cell::RefCell;
use std::time::Duration;

use uuid::Uuid;

/// ~30 Hz. Fast enough to read as continuous, slow enough that layout cost
/// stays irrelevant.
const FLUSH_EVERY: Duration = Duration::from_millis(33);

/// Flush early once a batch is big enough to be worth showing. Keeps the first
/// words appearing promptly rather than waiting out the full tick.
const FLUSH_AT_CHARS: usize = 24;

#[derive(Default)]
struct Buffer {
    /// The query these tokens belong to. Tokens for anything else are dropped:
    /// a query the user abandoned keeps streaming for a moment, and without
    /// this its tail lands in the next answer.
    query: Option<Uuid>,
    pending: String,
    committed: String,
    flush_scheduled: bool,
}

thread_local! {
    static BUFFER: RefCell<Buffer> = RefCell::new(Buffer::default());
}

/// Begin a new answer, discarding anything buffered for a previous one.
pub fn begin(query: Uuid) {
    BUFFER.with(|buffer| {
        let mut buffer = buffer.borrow_mut();
        buffer.query = Some(query);
        buffer.pending.clear();
        buffer.committed.clear();
    });
}

/// Queue a token. `apply` is called on the UI thread with the full answer text
/// whenever a flush happens.
pub fn push(query: Uuid, text: &str, apply: impl Fn(String) + Clone + 'static) {
    let should_flush_now = BUFFER.with(|buffer| {
        let mut buffer = buffer.borrow_mut();
        if buffer.query != Some(query) {
            return false;
        }
        buffer.pending.push_str(text);
        buffer.pending.len() >= FLUSH_AT_CHARS
    });

    if should_flush_now {
        flush(apply);
    } else {
        schedule_flush(apply);
    }
}

/// Force out whatever is buffered. Call on `Complete` so the last partial batch
/// is not left sitting in the buffer.
pub fn flush(apply: impl Fn(String) + 'static) {
    let text = BUFFER.with(|buffer| {
        let mut buffer = buffer.borrow_mut();
        if buffer.pending.is_empty() {
            return None;
        }
        let pending = std::mem::take(&mut buffer.pending);
        buffer.committed.push_str(&pending);
        Some(buffer.committed.clone())
    });

    if let Some(text) = text {
        apply(text);
    }
}

fn schedule_flush(apply: impl Fn(String) + Clone + 'static) {
    let already = BUFFER.with(|buffer| {
        let mut buffer = buffer.borrow_mut();
        let already = buffer.flush_scheduled;
        buffer.flush_scheduled = true;
        already
    });
    if already {
        return;
    }

    slint::Timer::single_shot(FLUSH_EVERY, move || {
        BUFFER.with(|buffer| buffer.borrow_mut().flush_scheduled = false);
        flush(apply);
    });
}

/// The answer accumulated so far, for `Ctrl+C`.
pub fn current() -> String {
    BUFFER.with(|buffer| {
        let buffer = buffer.borrow();
        let mut text = buffer.committed.clone();
        text.push_str(&buffer.pending);
        text
    })
}
