use std::path::PathBuf;

use anyhow::{Context, Result};
use tracing::info;

use crate::config::Config;

/// Ensure the GGUF model file exists at `config.model_path`.
///
/// If the file is absent the model is downloaded from HuggingFace Hub using the
/// repo/filename stored in `config`.  HF Hub caches the raw download in its own
/// cache directory (`~/.cache/huggingface/hub`); once fully downloaded, the file
/// is copied to `config.model_path` so every subsequent launch is instant and
/// independent of the HF cache.
pub async fn ensure_model(config: &Config) -> Result<PathBuf> {
    if config.model_path.exists() {
        info!(path = ?config.model_path, "model already present");
        return Ok(config.model_path.clone());
    }

    // Create the target directory before starting the download.
    if let Some(dir) = config.model_path.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("creating model directory {dir:?}"))?;
    }

    info!(
        repo     = %config.hf_repo,
        filename = %config.hf_filename,
        target   = ?config.model_path,
        "model not found — downloading from HuggingFace Hub"
    );
    eprintln!(
        "Downloading {} from {} …\n(this is a one-time download, ~2–4 GB depending on the model)",
        config.hf_filename, config.hf_repo
    );

    let api = hf_hub::api::tokio::ApiBuilder::new()
        .with_progress(true)
        .build()
        .context("building HuggingFace API client")?;

    let hf_cached = api
        .model(config.hf_repo.clone())
        .get(&config.hf_filename)
        .await
        .with_context(|| {
            format!(
                "downloading {}/{} from HuggingFace",
                config.hf_repo, config.hf_filename
            )
        })?;

    // Copy from the HF cache to our well-known location so future runs are
    // independent of the HF cache directory.
    tokio::fs::copy(&hf_cached, &config.model_path)
        .await
        .with_context(|| {
            format!(
                "copying model from {hf_cached:?} to {:?}",
                config.model_path
            )
        })?;

    info!(path = ?config.model_path, "model download complete");
    Ok(config.model_path.clone())
}
