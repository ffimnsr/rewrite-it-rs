//! LLM inference engine.
//!
//! All llama.cpp calls are **synchronous** and must run inside
//! `tokio::task::spawn_blocking`.  `LlamaModel` is `Send + Sync` so the
//! `Arc<LlamaModel>` can be cloned into blocking closures safely.  `LlamaContext`
//! is `!Send`, but it is created, used, and dropped entirely within the closure.

use std::num::NonZeroU32;
use std::sync::Arc;

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
use tokio::sync::mpsc;
use tracing::info;

use crate::{config::Config, prompt::Style};

pub(crate) struct Engine {
    backend: Arc<LlamaBackend>,
    model: Arc<LlamaModel>,
    config: Arc<Config>,
}

impl Engine {
    /// Load the model from disk. **Must be called from a blocking context.**
    pub(crate) fn load(config: Arc<Config>) -> Result<Self> {
        // Redirect llama.cpp's C-level log output into our tracing subscriber.
        send_logs_to_tracing(LogOptions::default());

        let backend = LlamaBackend::init().context("initialising llama backend")?;

        let model_params = LlamaModelParams::default().with_n_gpu_layers(config.n_gpu_layers);

        info!(path = ?config.model_path, n_gpu_layers = config.n_gpu_layers, "loading model");

        // LlamaModel::load_from_file takes &LlamaModelParams (no pinning needed
        // unless we call append_kv_override which we don't).
        let model =
            LlamaModel::load_from_file(&backend, &config.model_path, &model_params)
                .context("loading model from file")?;

        info!(
            params = model.n_params(),
            ctx_train = model.n_ctx_train(),
            "model loaded"
        );

        Ok(Self {
            backend: Arc::new(backend),
            model: Arc::new(model),
            config,
        })
    }

    /// Fully blocking rewrite: returns the complete result string.
    ///
    /// Call this via `tokio::task::spawn_blocking`.
    pub(crate) fn rewrite(&self, text: &str, style: Style) -> Result<String> {
        let prompt = crate::prompt::build_prompt(&self.model, text, style)?;
        let mut output = String::new();
        run_inference(&self.model, &self.backend, &self.config, &prompt, |piece| {
            output.push_str(&piece);
            true // continue
        })?;
        // Strip leading/trailing whitespace the model may insert around the output.
        Ok(output.trim().to_string())
    }

    /// Streaming rewrite: sends each generated token piece via `tx`.
    ///
    /// Stops early if `tx` is closed (caller cancelled).
    /// Call this via `tokio::task::spawn_blocking`.
    pub(crate) fn rewrite_streaming(
        &self,
        text: &str,
        style: Style,
        tx: mpsc::Sender<String>,
    ) -> Result<()> {
        let prompt = crate::prompt::build_prompt(&self.model, text, style)?;
        run_inference(&self.model, &self.backend, &self.config, &prompt, |piece| {
            // blocking_send is safe inside spawn_blocking threads.
            tx.blocking_send(piece).is_ok()
        })
    }

    pub(crate) fn model_name(&self) -> String {
        self.config
            .model_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_string())
    }
}

/// Token-by-token inference loop.
///
/// `on_token` is called with each decoded piece; returning `false` stops
/// generation early (e.g. the streaming receiver was dropped).
fn run_inference<F>(
    model: &LlamaModel,
    backend: &LlamaBackend,
    config: &Config,
    prompt: &str,
    mut on_token: F,
) -> Result<()>
where
    F: FnMut(String) -> bool,
{
    let ctx_size = config.context_size;
    let max_new = config.max_tokens as i32;

    // ── Context ────────────────────────────────────────────────────────────
    let mut ctx_params = LlamaContextParams::default()
        .with_n_ctx(Some(NonZeroU32::new(ctx_size).unwrap()));

    if let Some(threads) = config.n_threads {
        ctx_params = ctx_params
            .with_n_threads(threads)
            .with_n_threads_batch(threads);
    }

    // new_context takes ownership of ctx_params.
    let mut ctx = model
        .new_context(backend, ctx_params)
        .context("creating llama context")?;

    // ── Tokenise prompt ────────────────────────────────────────────────────
    let tokens = model
        .str_to_token(prompt, AddBos::Always)
        .context("tokenising prompt")?;

    let n_prompt = tokens.len() as i32;
    let n_ctx = ctx.n_ctx() as i32;

    anyhow::ensure!(
        n_prompt + max_new <= n_ctx,
        "prompt ({n_prompt} tokens) + max_tokens ({max_new}) exceeds context size ({n_ctx}); \
         increase context_size in config or shorten the input"
    );

    // ── Batch: prefill ─────────────────────────────────────────────────────
    // Capacity = len of initial prompt; we re-use the batch with capacity 1
    // for the generation phase.
    let prefill_cap = tokens.len().max(1);
    let mut batch = LlamaBatch::new(prefill_cap, 1);

    let last_idx = n_prompt - 1;
    for (i, token) in (0i32..).zip(tokens.into_iter()) {
        batch
            .add(token, i, &[0], i == last_idx)
            .context("adding prompt token to batch")?;
    }

    ctx.decode(&mut batch).context("prefill decode failed")?;

    // ── Sampler ────────────────────────────────────────────────────────────
    // temperature → top-k (loose filter) → dist (probabilistic pick).
    // With low temperature (≈0.3) the output is near-deterministic.
    let mut sampler = LlamaSampler::chain_simple([
        LlamaSampler::temp(config.temperature.max(0.01)),
        LlamaSampler::top_k(40),
        LlamaSampler::dist(config.seed),
    ]);

    // ── Generation loop ────────────────────────────────────────────────────
    let mut n_cur = batch.n_tokens(); // i32
    let n_max = n_prompt + max_new;
    let mut decoder = UTF_8.new_decoder();

    while n_cur <= n_max {
        let token = sampler.sample(&ctx, batch.n_tokens() - 1);
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

        ctx.decode(&mut batch).context("generation decode failed")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    // Integration-level tests require a model file; skipped in CI.
    // Unit-level structural tests live here.

    #[test]
    fn engine_module_exists() {
        // compile-time check that the module compiles
    }
}
