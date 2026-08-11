//! `llama-server` as a supervised child, and the streaming client that talks to it.
//!
//! Spec §22 option A: a child process over localhost HTTP rather than an FFI binding. At
//! this scale the binding buys nothing and costs build complexity, and an out-of-process
//! model means a crash in inference cannot take the daemon — and therefore the dock — with
//! it.
//!
//! The rule that shapes the supervision: **the model loads at daemon startup and stays
//! resident** (spec §37). Lazily loading on first query would put a multi-second model load
//! on the summon path, which is the one thing the whole split-process design exists to
//! prevent. Everything here is therefore built so that *the daemon serves lexical search
//! throughout* — while the model is loading, after it has crashed, and if it never starts.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use brain_core::config::Llm as LlmConfig;
use futures_util::StreamExt as _;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio_util::sync::CancellationToken;

/// How long to wait for the model to load before giving up on a start attempt.
///
/// A 1.5 GB Q5 model on this machine loads in a few seconds; a minute means something is
/// wrong that waiting will not fix.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// Generation that produces nothing for this long is abandoned.
const GENERATION_TIMEOUT: Duration = Duration::from_secs(15);

/// How often the supervisor asks whether the server is still there.
const HEALTH_INTERVAL: Duration = Duration::from_secs(5);

/// Consecutive start failures before the daemon stops trying.
const MAX_RESTARTS: u32 = 5;

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("the model is not loaded")]
    NotLoaded,
    #[error("llama-server did not become healthy within {}s", STARTUP_TIMEOUT.as_secs())]
    StartupTimeout,
    #[error("could not start llama-server: {0}")]
    Spawn(String),
    #[error("llama-server returned {status}: {body}")]
    Http { status: u16, body: String },
    #[error("generation stalled for {}s", GENERATION_TIMEOUT.as_secs())]
    Stalled,
    #[error(transparent)]
    Transport(#[from] reqwest::Error),
}

/// What `brainctl status` reports about the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ModelState {
    /// No model configured, or `backend = "none"`.
    Disabled = 0,
    /// Starting, or restarting after a crash. Lexical search works throughout.
    Loading = 1,
    Loaded = 2,
    /// Gave up after repeated failures. Permanently lexical-only until restarted.
    Failed = 3,
}

impl ModelState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "not configured",
            Self::Loading => "loading",
            Self::Loaded => "loaded",
            Self::Failed => "failed (lexical-only)",
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Loading,
            2 => Self::Loaded,
            3 => Self::Failed,
            _ => Self::Disabled,
        }
    }
}

/// One piece of a streamed answer.
#[derive(Debug, Clone)]
pub enum Chunk {
    Token(String),
    /// The model stopped. Carries how many tokens it produced.
    Done { output_tokens: u32 },
}

/// A running (or not-yet-running) llama-server.
pub struct Llm {
    config: Resolved,
    client: reqwest::Client,
    state: Arc<AtomicU8>,
    /// Kept so the child is killed when this is dropped. `None` for `backend = "external"`.
    ///
    /// Behind a mutex so `start` can take `&self`: the daemon wraps the backend in an `Arc`
    /// and shares it with the socket listener *before* the model has loaded, because it has
    /// to answer lexical queries throughout that window.
    child: std::sync::Mutex<Option<tokio::process::Child>>,
}

/// The subset of `[llm]` this module needs, resolved.
#[derive(Debug, Clone)]
struct Resolved {
    base_url: String,
    model: Option<PathBuf>,
    draft_model: Option<PathBuf>,
    managed: bool,
    context_tokens: usize,
    max_output_tokens: usize,
    temperature: f32,
    top_p: f32,
}

impl Llm {
    /// Resolve config into a client, without starting anything.
    pub fn new(config: &LlmConfig) -> Self {
        let profile = config.profiles.get(&config.profile);
        let resolved = Resolved {
            base_url: format!("http://{}:{}", config.host, config.port),
            model: profile.map(|profile| profile.model.clone()),
            draft_model: profile.and_then(|profile| profile.draft_model.clone()),
            // "external" means a server is already running at this URL and is not ours to
            // manage. Invaluable when debugging prompts by hand against a server you
            // started yourself with different flags.
            managed: config.backend != "external" && config.backend != "none",
            context_tokens: config.context_tokens,
            max_output_tokens: config.max_output_tokens,
            temperature: config.temperature,
            top_p: config.top_p,
        };

        let state = if config.backend == "none" {
            ModelState::Disabled
        } else {
            ModelState::Loading
        };

        Self {
            config: resolved,
            client: reqwest::Client::builder()
                // Generation streams for seconds; a request timeout would cut it off. The
                // stall timeout in `generate` is the real bound.
                .timeout(Duration::from_secs(600))
                .build()
                .unwrap_or_default(),
            state: Arc::new(AtomicU8::new(state as u8)),
            child: std::sync::Mutex::new(None),
        }
    }

    pub fn state(&self) -> ModelState {
        ModelState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Is the server answering right now?
    pub async fn healthy(&self) -> bool {
        matches!(
            self.client
                .get(format!("{}/health", self.config.base_url))
                .timeout(Duration::from_secs(2))
                .send()
                .await,
            Ok(response) if response.status().is_success()
        )
    }

    /// Keep the model running for the life of the daemon.
    ///
    /// Starts it, then watches. A crashed server is restarted with exponential backoff, and
    /// after [`MAX_RESTARTS`] consecutive failures the daemon gives up and stays in
    /// lexical-only mode — a circuit breaker, so a model that cannot load does not spend the
    /// session respawning and holding VRAM.
    ///
    /// Without this, killing `llama-server` leaves `status` reporting `loaded` forever while
    /// every query silently skips generation, which is the most confusing possible failure:
    /// the tool quietly does less and says nothing.
    pub async fn supervise(&self) {
        if self.state() == ModelState::Disabled {
            return;
        }

        let mut failures = 0;
        loop {
            match self.start().await {
                Ok(()) => failures = 0,
                Err(error) => {
                    failures += 1;
                    tracing::warn!(%error, failures, "llama-server did not start");
                    if failures >= MAX_RESTARTS {
                        tracing::error!(
                            "giving up on the model after {failures} attempts; \
                             serving lexical search only"
                        );
                        self.state.store(ModelState::Failed as u8, Ordering::Release);
                        return;
                    }
                    // 2s, 4s, 8s … capped. Long enough not to thrash on a machine that is
                    // out of VRAM, short enough to recover from a transient failure.
                    let backoff = Duration::from_secs(2u64.pow(failures.min(5)));
                    tokio::time::sleep(backoff).await;
                    continue;
                }
            }

            // Loaded. Watch until it stops answering.
            while self.healthy().await {
                tokio::time::sleep(HEALTH_INTERVAL).await;
            }

            tracing::warn!("llama-server stopped answering; restarting");
            self.state.store(ModelState::Loading as u8, Ordering::Release);
        }
    }

    pub fn context_tokens(&self) -> usize {
        self.config.context_tokens
    }

    pub fn model_name(&self) -> Option<String> {
        self.config.model.as_ref().and_then(|path| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().to_string())
        })
    }

    /// Spawn the server (if managed) and wait for it to answer `/health`.
    ///
    /// The caller runs this in the background: the daemon must be accepting connections and
    /// answering lexical queries while this is happening, not blocked on it.
    pub async fn start(&self) -> Result<(), LlmError> {
        if self.state() == ModelState::Disabled {
            return Ok(());
        }
        self.state.store(ModelState::Loading as u8, Ordering::Release);

        if self.config.managed {
            let model = self
                .config
                .model
                .clone()
                .ok_or_else(|| LlmError::Spawn("no model configured for this profile".into()))?;

            if !model.exists() {
                self.state.store(ModelState::Failed as u8, Ordering::Release);
                return Err(LlmError::Spawn(format!(
                    "{} does not exist",
                    model.display()
                )));
            }

            let child = self.spawn(&model)?;
            *self
                .child
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(child);
        }

        match self.await_health().await {
            Ok(()) => {
                self.state.store(ModelState::Loaded as u8, Ordering::Release);
                // Fire one token through the whole path so the CUDA context, graphs, and
                // allocator are hot before the first real query rather than during it.
                // Costs a few hundred milliseconds at startup, once.
                self.warm_up().await;
                Ok(())
            }
            Err(error) => {
                self.state.store(ModelState::Failed as u8, Ordering::Release);
                Err(error)
            }
        }
    }

    fn spawn(&self, model: &std::path::Path) -> Result<tokio::process::Child, LlmError> {
        let mut command = tokio::process::Command::new("llama-server");
        command
            .arg("--model")
            .arg(model)
            .args(["--alias", "brain"])
            .args(["--host", "127.0.0.1"])
            .args(["--port", &self.port().to_string()])
            .args(["--ctx-size", &self.config.context_tokens.to_string()])
            // The model is 1.4 GiB against 5806 MiB, and X runs on the AMD iGPU, so
            // nothing else is using the 3060. Full offload is free here.
            .args(["--n-gpu-layers", "99"])
            .args(["--flash-attn", "on"])
            // The point of the stable prompt prefix: reuse the KV cache across queries
            // instead of reprocessing the system block every time.
            .args(["--cache-reuse", "256"])
            // Qwen3 is a hybrid reasoning model and emits <think>…</think> by default,
            // which is fatal for a sub-500 ms TTFT. Belt and braces with the request-body
            // `enable_thinking: false` and the stream-side strip in `generate`.
            .args(["--reasoning-budget", "0"])
            .args(["--parallel", "1"])
            .arg("--no-webui")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            // Dies with the daemon rather than orphaning and holding 1.4 GB of VRAM.
            .kill_on_drop(true);

        if let Some(draft) = &self.config.draft_model {
            // Speculative decoding. The prompt contract makes the model restate retrieved
            // text, which is exactly the predictable-output regime where drafting wins.
            command.arg("--model-draft").arg(draft);
        }

        let mut child = command
            .spawn()
            .map_err(|error| LlmError::Spawn(error.to_string()))?;

        // llama-server logs to stderr. Forwarding it at debug means a failure to load is
        // diagnosable from the daemon's own journal instead of vanishing.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "llama-server", "{line}");
                }
            });
        }

        Ok(child)
    }

    fn port(&self) -> u16 {
        self.config
            .base_url
            .rsplit(':')
            .next()
            .and_then(|port| port.parse().ok())
            .unwrap_or(8177)
    }

    async fn await_health(&self) -> Result<(), LlmError> {
        let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
        let url = format!("{}/health", self.config.base_url);

        loop {
            if let Ok(response) = self.client.get(&url).send().await
                && response.status().is_success()
            {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(LlmError::StartupTimeout);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// One throwaway token, to make the first real query fast.
    async fn warm_up(&self) {
        let started = std::time::Instant::now();
        let request = serde_json::json!({
            "messages": [
                { "role": "system", "content": crate::prompt::SYSTEM },
                { "role": "user", "content": "warm" },
            ],
            "max_tokens": 1,
            "cache_prompt": true,
            "chat_template_kwargs": { "enable_thinking": false },
        });

        match self
            .client
            .post(format!("{}/v1/chat/completions", self.config.base_url))
            .json(&request)
            .send()
            .await
        {
            // Sending the real system block matters: it leaves the stable prefix already
            // in the KV cache, so the first real query reuses it too.
            Ok(_) => tracing::info!(elapsed_ms = started.elapsed().as_millis() as u64, "model warm"),
            Err(error) => tracing::warn!(%error, "warm-up failed; the first query pays for it"),
        }
    }

    /// Count tokens with the server's own tokenizer.
    ///
    /// The `len/3.6` estimate is fine for deciding how many sources to pack, but not for
    /// deciding whether the prompt fits: an overflowing prompt is truncated **from the
    /// left** by the server, which removes the system block and produces behaviour that
    /// looks exactly like the model ignoring its instructions.
    pub async fn count_tokens(&self, text: &str) -> Result<usize, LlmError> {
        #[derive(serde::Deserialize)]
        struct Tokenized {
            tokens: Vec<i64>,
        }

        let response = self
            .client
            .post(format!("{}/tokenize", self.config.base_url))
            .json(&serde_json::json!({ "content": text }))
            .send()
            .await?;

        let tokenized: Tokenized = response.json().await?;
        Ok(tokenized.tokens.len())
    }

    /// Stream an answer, calling `on_chunk` for each piece.
    ///
    /// Cancellation drops the stream, which closes the connection and lets llama-server
    /// free the slot — a new query supersedes the old one rather than queueing behind it.
    pub async fn generate<F>(
        &self,
        prompt: &crate::prompt::Prompt,
        cancel: &CancellationToken,
        mut on_chunk: F,
    ) -> Result<(), LlmError>
    where
        F: FnMut(Chunk),
    {
        if self.state() != ModelState::Loaded {
            return Err(LlmError::NotLoaded);
        }

        let request = serde_json::json!({
            "messages": [
                { "role": "system", "content": prompt.system },
                { "role": "user", "content": prompt.user },
            ],
            "stream": true,
            "cache_prompt": true,
            "max_tokens": self.config.max_output_tokens,
            "temperature": self.config.temperature,
            "top_p": self.config.top_p,
            "chat_template_kwargs": { "enable_thinking": false },
        });

        let response = self
            .client
            .post(format!("{}/v1/chat/completions", self.config.base_url))
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(LlmError::Http { status, body });
        }

        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut thinking = ThinkingFilter::default();
        let mut output_tokens = 0;

        loop {
            let next = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    tracing::debug!("generation cancelled");
                    return Ok(());
                }
                next = tokio::time::timeout(GENERATION_TIMEOUT, stream.next()) => next,
            };

            let chunk = match next {
                Err(_) => return Err(LlmError::Stalled),
                Ok(None) => break,
                Ok(Some(chunk)) => chunk?,
            };

            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // SSE frames are separated by a blank line, and a single read can split one in
            // half — so only complete lines are parsed and the tail is kept.
            while let Some(newline) = buffer.find('\n') {
                let line = buffer[..newline].trim().to_string();
                buffer.drain(..=newline);

                let Some(payload) = line.strip_prefix("data: ") else {
                    continue;
                };
                if payload == "[DONE]" {
                    let tail = thinking.flush();
                    if !tail.is_empty() {
                        on_chunk(Chunk::Token(tail));
                    }
                    on_chunk(Chunk::Done { output_tokens });
                    return Ok(());
                }

                let Ok(event) = serde_json::from_str::<serde_json::Value>(payload) else {
                    continue;
                };
                let Some(text) = event["choices"][0]["delta"]["content"].as_str() else {
                    continue;
                };

                let visible = thinking.filter(text);
                if !visible.is_empty() {
                    output_tokens += 1;
                    on_chunk(Chunk::Token(visible));
                }
            }
        }

        // The stream ended without a `[DONE]` frame. Still flush: a truncated stream
        // should keep whatever arrived, not lose its tail as well.
        let tail = thinking.flush();
        if !tail.is_empty() {
            on_chunk(Chunk::Token(tail));
        }
        on_chunk(Chunk::Done { output_tokens });
        Ok(())
    }
}

/// Drops `<think>…</think>` spans from the token stream.
///
/// Third line of defence, after `--reasoning-budget 0` and `enable_thinking: false`. It is
/// here because both of those are llama.cpp API surface that has moved between releases,
/// and a reasoning trace leaking into the dock is a visible, confusing failure rather than
/// a subtle one. Streaming means the markers can arrive split across chunks, so this is a
/// small state machine rather than a regex.
#[derive(Default)]
struct ThinkingFilter {
    inside: bool,
    pending: String,
}

/// `"<think>".len()` — the most that can be a partial opening marker.
const MARKER_HOLDBACK: usize = 7;
/// `"</think>".len()`, plus slack.
const CLOSE_HOLDBACK: usize = 16;

/// Largest index at or below `index` that is a char boundary.
///
/// The holdback is measured in bytes but the buffer holds UTF-8, and note bodies are full
/// of typographic quotes and accented words — slicing mid-character panics.
fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

impl ThinkingFilter {
    /// Release whatever was held back against a marker that never arrived.
    ///
    /// Must be called when the stream ends. Without it the last few characters of every
    /// answer are silently dropped — a sentence ending mid-word, which reads as the model
    /// being cut off rather than as a bug here.
    fn flush(&mut self) -> String {
        if self.inside {
            // An unterminated think block: everything left is reasoning, not answer.
            self.pending.clear();
            return String::new();
        }
        std::mem::take(&mut self.pending)
    }

    fn filter(&mut self, text: &str) -> String {
        self.pending.push_str(text);
        let mut out = String::new();

        loop {
            if self.inside {
                match self.pending.find("</think>") {
                    Some(end) => {
                        self.pending.drain(..end + "</think>".len());
                        self.inside = false;
                    }
                    None => {
                        // Inside a think block: discard everything except enough to
                        // recognise a closing marker split across chunks.
                        if self.pending.len() > CLOSE_HOLDBACK {
                            let keep = floor_char_boundary(
                                &self.pending,
                                self.pending.len() - CLOSE_HOLDBACK,
                            );
                            self.pending.drain(..keep);
                        }
                        return out;
                    }
                }
            } else {
                match self.pending.find("<think>") {
                    Some(start) => {
                        out.push_str(&self.pending[..start]);
                        self.pending.drain(..start + "<think>".len());
                        self.inside = true;
                    }
                    None => {
                        // A partial `<think>` at the end must not be emitted yet — but the
                        // held-back tail has to be released by `flush` when the stream
                        // ends, or every answer loses its last few characters.
                        let safe = floor_char_boundary(
                            &self.pending,
                            self.pending.len().saturating_sub(MARKER_HOLDBACK),
                        );
                        out.push_str(&self.pending[..safe]);
                        self.pending.drain(..safe);
                        return out;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thinking_span_never_reaches_the_dock() {
        let mut filter = ThinkingFilter::default();
        let mut out = String::new();
        for piece in ["<think>", "I should ", "consider", "</think>", "The answer."] {
            out.push_str(&filter.filter(piece));
        }
        out.push_str(&filter.flush());
        assert_eq!(out, "The answer.");
    }

    #[test]
    fn a_marker_split_across_chunks_is_still_recognised() {
        // The realistic case: SSE chunks do not respect token or marker boundaries.
        let mut filter = ThinkingFilter::default();
        let mut out = String::new();
        for piece in ["Before <thi", "nk>hidden</thi", "nk>after"] {
            out.push_str(&filter.filter(piece));
        }
        out.push_str(&filter.flush());
        assert_eq!(out, "Before after");
    }

    #[test]
    fn ordinary_text_passes_through_unchanged() {
        let mut filter = ThinkingFilter::default();
        let mut out = String::new();
        for piece in ["Apply exponential ", "smoothing to the crop ", "target."] {
            out.push_str(&filter.filter(piece));
        }
        out.push_str(&filter.flush());
        assert_eq!(out, "Apply exponential smoothing to the crop target.");
    }

    #[test]
    fn a_disabled_backend_reports_disabled_rather_than_loading() {
        // `backend = "none"` is how a user turns generation off entirely. It must not sit
        // in `Loading` forever waiting for a server nobody is going to start.
        let config = LlmConfig {
            backend: "none".into(),
            ..LlmConfig::default()
        };
        assert_eq!(Llm::new(&config).state(), ModelState::Disabled);
    }

    #[test]
    fn an_external_backend_is_not_spawned_but_is_still_used() {
        // "external" means a server is already running at this URL and is not ours to
        // manage — the shape that makes prompt debugging by hand possible.
        let config = LlmConfig {
            backend: "external".into(),
            ..LlmConfig::default()
        };
        let llm = Llm::new(&config);
        assert!(!llm.config.managed);
        assert_eq!(llm.state(), ModelState::Loading);
    }

    #[test]
    fn generating_before_the_model_is_loaded_is_an_error_not_a_hang() {
        let llm = Llm::new(&LlmConfig::default());
        let prompt = crate::prompt::build("q", &[], 100);
        let outcome = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(llm.generate(&prompt, &CancellationToken::new(), |_| {}));
        assert!(matches!(outcome, Err(LlmError::NotLoaded)));
    }
}
