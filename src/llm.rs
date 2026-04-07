//! LLM inference engine.
//!
//! Inference is handled by a single **dedicated worker thread** that owns the
//! `LlamaContext` for the lifetime of the daemon.  Requests are submitted via a
//! synchronous channel; the worker processes them one at a time and clears the
//! KV-cache between requests so the context is reused without re-allocation.
//!
//! `Engine::rewrite` and `Engine::rewrite_streaming` block the calling thread
//! until the job completes or the `inference_timeout_secs` deadline fires.
//! Both must be called from inside `tokio::task::spawn_blocking`.

use std::num::NonZeroU32;
use std::sync::{
    atomic::{AtomicU64, AtomicU8, Ordering},
    mpsc as stdmpsc, Arc, Mutex,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use encoding_rs::UTF_8;
use llama_cpp_2::{
    context::params::LlamaContextParams,
    llama_backend::LlamaBackend,
    llama_batch::LlamaBatch,
    model::{params::LlamaModelParams, AddBos, LlamaModel},
    sampling::LlamaSampler,
    send_logs_to_tracing, LogOptions,
};
use tokio::sync::{mpsc, OnceCell};
use tracing::info;

use crate::{config::Config, model, prompt::Style};

// ── Runtime status ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeStatus {
    Idle = 0,
    Downloading = 1,
    Loading = 2,
    Ready = 3,
    Failed = 4,
}

impl RuntimeStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Downloading => "downloading",
            Self::Loading => "loading",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Downloading,
            2 => Self::Loading,
            3 => Self::Ready,
            4 => Self::Failed,
            _ => Self::Idle,
        }
    }
}

// ── Inference worker ──────────────────────────────────────────────────────────

/// A job submitted to the inference worker thread.
enum InferenceJob {
    /// Full (non-streaming) rewrite; complete result returned via `reply_tx`.
    Full {
        text: String,
        style: Style,
        reply_tx: stdmpsc::SyncSender<Result<String>>,
    },
    /// Streaming rewrite; each token piece sent via `chunk_tx`, completion via `done_tx`.
    Streaming {
        text: String,
        style: Style,
        chunk_tx: mpsc::Sender<String>,
        done_tx: stdmpsc::SyncSender<Result<()>>,
    },
}

/// Handle to the single inference worker thread.
///
/// The thread owns the `LlamaContext` and processes jobs sequentially,
/// calling `ctx.clear_kv_cache()` between requests to reuse the allocated
/// KV-cache memory without deallocation/reallocation.
struct InferenceWorker {
    tx: stdmpsc::SyncSender<InferenceJob>,
}

impl InferenceWorker {
    fn spawn(
        model: Arc<LlamaModel>,
        backend: Arc<LlamaBackend>,
        config: Arc<Config>,
        inference_started_at: Arc<AtomicU64>,
    ) -> Self {
        // Buffer up to 4 pending jobs; callers block beyond that (natural backpressure).
        let (tx, rx) = stdmpsc::sync_channel::<InferenceJob>(4);
        let started_at = Arc::clone(&inference_started_at);

        std::thread::Builder::new()
            .name("inference".into())
            .spawn(move || {
                let ctx_params = make_ctx_params(&config);
                // Both model and backend are kept alive by their Arcs for the
                // full thread lifetime, so the context borrow is valid.
                let mut ctx = match model.new_context(&backend, ctx_params) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!("inference context init failed: {e}");
                        return;
                    }
                };
                info!(
                    context_size = config.context_size,
                    "inference worker ready (context reuse enabled)"
                );

                // Tracks the last cached KV-prefix: (style, prefix_token_len).
                // Allows partial KV-cache eviction between same-style requests,
                // skipping re-prefill of the invariant system + instruction prefix.
                let mut cached_prefix: Option<(Style, usize)> = None;

                while let Ok(job) = rx.recv() {
                    // KV-cache management is handled inside run_inference_with_ctx,
                    // which performs a full clear on style change and a partial
                    // eviction (keeping the prefix) on same-style requests.

                    // Record start time for hang detection.
                    started_at.store(current_unix_secs(), Ordering::Relaxed);

                    process_job(&model, &mut ctx, &config, job, &mut cached_prefix);

                    // Mark inference as idle.
                    started_at.store(0, Ordering::Relaxed);
                }
                info!("inference worker thread exiting");
            })
            .expect("failed to spawn inference thread");

        InferenceWorker { tx }
    }

    /// Submit a full rewrite job and block until completed or timed out.
    fn run_full(&self, text: String, style: Style, timeout: Duration) -> Result<String> {
        let (reply_tx, reply_rx) = stdmpsc::sync_channel(1);
        self.tx
            .send(InferenceJob::Full { text, style, reply_tx })
            .map_err(|_| anyhow::anyhow!("inference worker has stopped"))?;
        reply_rx
            .recv_timeout(timeout)
            .map_err(|e| anyhow::anyhow!("inference timed out or worker stopped: {e}"))?
    }

    /// Submit a streaming rewrite job and block until completed or timed out.
    fn run_streaming(
        &self,
        text: String,
        style: Style,
        chunk_tx: mpsc::Sender<String>,
        timeout: Duration,
    ) -> Result<()> {
        let (done_tx, done_rx) = stdmpsc::sync_channel(1);
        self.tx
            .send(InferenceJob::Streaming { text, style, chunk_tx, done_tx })
            .map_err(|_| anyhow::anyhow!("inference worker has stopped"))?;
        done_rx
            .recv_timeout(timeout)
            .map_err(|e| anyhow::anyhow!("inference timed out or worker stopped: {e}"))?
    }
}

/// Build `LlamaContextParams` from the active configuration.
fn make_ctx_params(config: &Config) -> LlamaContextParams {
    let mut params = LlamaContextParams::default()
        .with_n_ctx(Some(NonZeroU32::new(config.context_size).unwrap()));
    if let Some(threads) = config.n_threads {
        params = params
            .with_n_threads(threads)
            .with_n_threads_batch(threads);
    }
    params
}

/// Execute one `InferenceJob` and route the result back to the caller.
fn process_job(
    model: &LlamaModel,
    ctx: &mut llama_cpp_2::context::LlamaContext<'_>,
    config: &Config,
    job: InferenceJob,
    cached_prefix: &mut Option<(Style, usize)>,
) {
    match job {
        InferenceJob::Full { text, style, reply_tx } => {
            let result = (|| {
                let prompt = crate::prompt::build_prompt(model, &text, style)?;
                let mut output = String::new();
                run_inference_with_ctx(model, ctx, config, style, &prompt, cached_prefix, |piece| {
                    output.push_str(&piece);
                    true
                })?;
                Ok(output.trim().to_string())
            })();
            let _ = reply_tx.send(result);
        }
        InferenceJob::Streaming { text, style, chunk_tx, done_tx } => {
            let result = (|| {
                let prompt = crate::prompt::build_prompt(model, &text, style)?;
                run_inference_with_ctx(model, ctx, config, style, &prompt, cached_prefix, |piece| {
                    chunk_tx.blocking_send(piece).is_ok()
                })
            })();
            let _ = done_tx.send(result);
        }
    }
}

// ── Engine ────────────────────────────────────────────────────────────────────

pub struct Engine {
    worker: InferenceWorker,
    model_name: String,
    config: Arc<Config>,
    /// Unix timestamp (seconds) of the current inference start; 0 when idle.
    /// Exposed so `EngineManager` can detect hangs from the watchdog task.
    pub inference_started_at: Arc<AtomicU64>,
}

impl Engine {
    /// Load the model and start the inference worker thread.
    /// **Must be called from a blocking context** (`spawn_blocking`).
    pub fn load(config: Arc<Config>) -> Result<Self> {
        // Redirect llama.cpp's C-level log output into our tracing subscriber.
        send_logs_to_tracing(LogOptions::default());

        let backend = Arc::new(LlamaBackend::init().context("initialising llama backend")?);
        let model_params = LlamaModelParams::default().with_n_gpu_layers(config.n_gpu_layers);

        info!(path = ?config.model_path, n_gpu_layers = config.n_gpu_layers, "loading model");

        let model = Arc::new(
            LlamaModel::load_from_file(&backend, &config.model_path, &model_params)
                .context("loading model from file")?,
        );

        info!(
            params = model.n_params(),
            ctx_train = model.n_ctx_train(),
            "model loaded"
        );

        let model_name = config
            .model_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_string());

        let inference_started_at = Arc::new(AtomicU64::new(0));
        let worker = InferenceWorker::spawn(
            model,
            backend,
            Arc::clone(&config),
            Arc::clone(&inference_started_at),
        );

        Ok(Self { worker, model_name, config, inference_started_at })
    }

    /// Fully blocking rewrite: returns the complete result string.
    ///
    /// Call this via `tokio::task::spawn_blocking`.
    pub fn rewrite(&self, text: &str, style: Style) -> Result<String> {
        let timeout = Duration::from_secs(self.config.inference_timeout_secs);
        self.worker.run_full(text.to_string(), style, timeout)
    }

    /// Streaming rewrite: sends each generated token piece via `tx`.
    ///
    /// Stops early if `tx` is closed (caller cancelled).
    /// Call this via `tokio::task::spawn_blocking`.
    pub fn rewrite_streaming(
        &self,
        text: &str,
        style: Style,
        tx: mpsc::Sender<String>,
    ) -> Result<()> {
        let timeout = Duration::from_secs(self.config.inference_timeout_secs);
        self.worker.run_streaming(text.to_string(), style, tx, timeout)
    }

    pub fn model_name(&self) -> String {
        self.model_name.clone()
    }
}

// ── EngineManager ─────────────────────────────────────────────────────────────

pub struct EngineManager {
    config: Arc<Config>,
    engine: OnceCell<Arc<Engine>>,
    status: AtomicU8,
    last_error: Mutex<Option<String>>,
    /// Unix timestamp (seconds) of the last completed request; 0 = never used.
    last_active: AtomicU64,
}

impl EngineManager {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            config,
            engine: OnceCell::new(),
            status: AtomicU8::new(RuntimeStatus::Idle as u8),
            last_error: Mutex::new(None),
            last_active: AtomicU64::new(0),
        }
    }

    pub async fn ensure_ready(&self) -> Result<Arc<Engine>> {
        let engine = self
            .engine
            .get_or_try_init(|| async {
                self.set_status(RuntimeStatus::Downloading, None);
                let model_path = match model::ensure_model(&self.config).await {
                    Ok(path) => path,
                    Err(err) => {
                        self.set_status(RuntimeStatus::Failed, Some(err.to_string()));
                        return Err(err);
                    }
                };

                let mut effective_config = (*self.config).clone();
                effective_config.model_path = model_path;
                let effective_config = Arc::new(effective_config);

                self.set_status(RuntimeStatus::Loading, None);
                let engine = match tokio::task::spawn_blocking({
                    let cfg = Arc::clone(&effective_config);
                    move || Engine::load(cfg)
                })
                .await
                .context("model-loading thread panicked")
                {
                    Ok(Ok(engine)) => engine,
                    Ok(Err(err)) => {
                        self.set_status(RuntimeStatus::Failed, Some(err.to_string()));
                        return Err(err);
                    }
                    Err(err) => {
                        self.set_status(RuntimeStatus::Failed, Some(err.to_string()));
                        return Err(err);
                    }
                };

                self.set_status(RuntimeStatus::Ready, None);
                Ok(Arc::new(engine))
            })
            .await?;

        Ok(Arc::clone(engine))
    }

    pub fn is_ready(&self) -> bool {
        self.engine.initialized()
    }

    pub fn status(&self) -> String {
        let status = RuntimeStatus::from_u8(self.status.load(Ordering::Relaxed));
        match self
            .last_error
            .lock()
            .expect("status mutex poisoned")
            .as_deref()
        {
            Some(err) if status == RuntimeStatus::Failed => format!("failed: {err}"),
            _ => status.as_str().to_string(),
        }
    }

    pub fn model_name(&self) -> String {
        if let Some(engine) = self.engine.get() {
            return engine.model_name();
        }
        self.config
            .model_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_string())
    }

    pub fn config(&self) -> &Arc<Config> {
        &self.config
    }

    /// Record the current time as the last active timestamp (call on each request).
    pub fn touch(&self) {
        self.last_active.store(current_unix_secs(), Ordering::Relaxed);
    }

    /// Seconds elapsed since the last request; 0 if no request has ever been made.
    pub fn seconds_idle(&self) -> u64 {
        let last = self.last_active.load(Ordering::Relaxed);
        if last == 0 {
            return 0;
        }
        current_unix_secs().saturating_sub(last)
    }

    /// Returns `true` when an inference has been running longer than
    /// `config.inference_timeout_secs`, indicating a likely hang.
    pub fn inference_hung(&self) -> bool {
        let Some(engine) = self.engine.get() else {
            return false;
        };
        let start = engine.inference_started_at.load(Ordering::Relaxed);
        if start == 0 {
            return false; // idle
        }
        current_unix_secs().saturating_sub(start) > self.config.inference_timeout_secs
    }

    fn set_status(&self, status: RuntimeStatus, error: Option<String>) {
        self.status.store(status as u8, Ordering::Relaxed);
        *self.last_error.lock().expect("status mutex poisoned") = error;
    }
}

fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ── Token-by-token inference (context-reuse variant) ─────────────────────────

/// Token-by-token inference loop using a pre-allocated, caller-managed context.
///
/// ## KV-cache prefix reuse
///
/// The user message is always formatted as `{instruction}\n\n---\n{user_text}`.
/// Everything before (and including) the `---\n` separator is invariant for a
/// given style.  On consecutive requests with the same style the function only
/// evicts the suffix tokens from the KV-cache and re-prefills those, leaving
/// the system-prompt and style-instruction prefix warm.
///
/// `cached_prefix` tracks `(style, prefix_token_len)` across calls.  Pass
/// the same `Option` for the lifetime of the inference worker thread.
///
/// ## Sampler choice
///
/// `Style::Grammar` uses greedy decoding (maximally deterministic, slightly
/// faster).  All other styles use temperature → top-k → stochastic dist.
fn run_inference_with_ctx<F>(
    model: &LlamaModel,
    ctx: &mut llama_cpp_2::context::LlamaContext<'_>,
    config: &Config,
    style: Style,
    prompt: &str,
    cached_prefix: &mut Option<(Style, usize)>,
    mut on_token: F,
) -> Result<()>
where
    F: FnMut(String) -> bool,
{
    let max_new = config.max_tokens as i32;

    // ── Prefix-cache analysis ──────────────────────────────────────────────
    // `build_prompt` always places the user text after `\n---\n`.
    // Tokenising everything up to and including that separator gives us a
    // stable prefix length we can keep in the KV-cache across same-style
    // requests.
    const SEP: &str = "\n---\n";
    let prefix_char_end = prompt.find(SEP).map(|i| i + SEP.len()).unwrap_or(0);

    let prefix_token_len: usize = if prefix_char_end > 0 {
        model
            .str_to_token(&prompt[..prefix_char_end], AddBos::Always)
            .map(|t| t.len())
            .unwrap_or(0)
    } else {
        0
    };

    let can_reuse = prefix_token_len > 0
        && cached_prefix
            .as_ref()
            .is_some_and(|&(cs, cl)| cs == style && cl == prefix_token_len);

    // ── Prefill ────────────────────────────────────────────────────────────
    // Returns (total_prompt_tokens, first_sample_batch_idx).
    // `first_sample_batch_idx` is the slot in the last decoded batch whose
    // logits should be consumed for the very first generated token.
    let (n_prompt, first_sample_idx): (i32, i32) = if can_reuse {
        // Evict only the suffix (previous user text + generated response).
        // The prefix tokens [0, prefix_token_len) stay warm in the KV-cache.
        ctx.clear_kv_cache_seq(Some(0), Some(prefix_token_len as u32), None)
            .unwrap_or(false);

        let suffix_tokens = model
            .str_to_token(&prompt[prefix_char_end..], AddBos::Never)
            .context("tokenising prompt suffix")?;

        let n_suffix = suffix_tokens.len() as i32;
        let n_total = prefix_token_len as i32 + n_suffix;
        let n_ctx = ctx.n_ctx() as i32;

        anyhow::ensure!(
            n_total + max_new <= n_ctx,
            "prompt ({n_total} tokens) + max_tokens ({max_new}) exceeds context size ({n_ctx}); \
             increase context_size in config or shorten the input"
        );

        let mut batch = LlamaBatch::new(suffix_tokens.len().max(1), 1);
        let last_idx = n_suffix - 1;
        for (i, token) in suffix_tokens.into_iter().enumerate() {
            let pos = prefix_token_len as i32 + i as i32;
            batch
                .add(token, pos, &[0], i as i32 == last_idx)
                .context("adding suffix token to batch")?;
        }
        ctx.decode(&mut batch).context("suffix prefill decode failed")?;

        (n_total, last_idx)
    } else {
        // Full prefill — either the first request or a different style.
        ctx.clear_kv_cache();

        let tokens = model
            .str_to_token(prompt, AddBos::Always)
            .context("tokenising prompt")?;

        let n_total = tokens.len() as i32;
        let n_ctx = ctx.n_ctx() as i32;

        anyhow::ensure!(
            n_total + max_new <= n_ctx,
            "prompt ({n_total} tokens) + max_tokens ({max_new}) exceeds context size ({n_ctx}); \
             increase context_size in config or shorten the input"
        );

        let mut batch = LlamaBatch::new(tokens.len().max(1), 1);
        let last_idx = n_total - 1;
        for (i, token) in (0i32..).zip(tokens.into_iter()) {
            batch
                .add(token, i, &[0], i == last_idx)
                .context("adding prompt token to batch")?;
        }
        ctx.decode(&mut batch).context("prefill decode failed")?;

        // Cache the prefix for future same-style requests.
        if prefix_token_len > 0 {
            *cached_prefix = Some((style, prefix_token_len));
        }

        (n_total, last_idx)
    };

    // ── Sampler ────────────────────────────────────────────────────────────
    // Grammar uses greedy decoding: maximally deterministic and slightly faster
    // since no softmax/sampling arithmetic is needed.
    // All other styles use temperature + top-k + stochastic dist sampling.
    let mut sampler = if style == Style::Grammar {
        LlamaSampler::greedy()
    } else {
        LlamaSampler::chain_simple([
            LlamaSampler::temp(config.temperature.max(0.01)),
            LlamaSampler::top_k(40),
            LlamaSampler::dist(config.seed),
        ])
    };

    // ── Generation loop ────────────────────────────────────────────────────
    let mut n_cur = n_prompt;
    let n_max = n_prompt + max_new;
    let mut decoder = UTF_8.new_decoder();
    let mut batch = LlamaBatch::new(1, 1);
    let mut sample_idx = first_sample_idx;

    while n_cur <= n_max {
        let token = sampler.sample(ctx, sample_idx);
        sampler.accept(token);

        if model.is_eog_token(token) {
            break;
        }

        let piece = model
            .token_to_piece(token, &mut decoder, /* special */ true, /* lstrip */ None)
            .context("decoding token to string")?;

        if !on_token(piece) {
            break; // streaming client disconnected
        }

        batch.clear();
        batch
            .add(token, n_cur, &[0], true)
            .context("adding generated token to batch")?;
        n_cur += 1;
        // After the first generated token the batch always has exactly one token at slot 0.
        sample_idx = 0;

        ctx.decode(&mut batch).context("generation decode failed")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    // Integration-level tests require a model file; skipped in CI.
    #[test]
    fn engine_module_exists() {
        // compile-time check that the module compiles
    }
}
